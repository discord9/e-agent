use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::workspace::Workspace;

mod background;
mod bash;
mod file;
pub mod output;
#[cfg(target_os = "linux")]
mod rust;
mod tasks;
mod web;
#[cfg(windows)]
mod windows_sandbox;

use file::*;
use output::*;
use tasks::*;
use web::*;

pub use background::{
    BackgroundTaskInfo, BackgroundTasks, ExitSlot, OutputSlot, SpoolWindow, TaskDisplayMeta,
    TaskExit, TaskSpool, new_exit_slot,
};
pub use bash::bash_tool;
pub use file::{file_tools, undo_file_op};
#[cfg(target_os = "linux")]
pub(crate) use rust::resolve_rustc;
#[cfg(target_os = "linux")]
pub use rust::run_rust_tool;

// The `#[cfg(test)] mod tests` child re-imports these names via `use super::*`.
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use crate::agent::AgentEvent;

#[cfg(test)]
use background::*;

#[cfg(test)]
use bash::*;

/// Serializes tests that record/undo file operations: the undo stack is
/// process-global, so parallel tests would steal each other's snapshots.
/// Async-aware because the tests hold it across tool-execution awaits.
#[cfg(test)]
pub(crate) static UNDO_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Empty the undo stack. Test-only: undo tests outside the tools module
/// (server, TUI) use this to start from a known state.
#[cfg(test)]
pub(crate) fn clear_undo_stack() {
    file::clear_undo_stack();
}

const READ_LIMIT: usize = 64 * 1024;
const DEFAULT_READ_LINES: usize = 2000;
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Check whether bwrap is installed and user namespaces work.
/// Used at startup and by sandbox tests to skip when unavailable.
pub fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "/bin/true"])
        .status()
        .ok()
        .is_some_and(|status| status.success())
}

/// Built-in tools plus the shared background-task registry, exposed so that
/// other tools (e.g. delegate) can schedule background work too.
///
/// `read_only` (the main session's `--read-only` policy): file tools shrink
/// to `read_file`, bash runs in a narrowed read-only sandbox with network
/// disabled, and — fail closed — there is no bash at all when the sandbox is
/// not enabled.
///
/// `background_timeout` is the background bash timeout
/// (`Some(Duration)` = timeout, `None` = no timeout / run forever).
///
/// The foreground bash timeout is resolved from the workspace's
/// `<workspace>/.e-agent/config.toml` `[bash]` override plus the 30s default
/// (no global config is available here). The main session factory passes the
/// full global + workspace policy via [`builtins_with_bash_timeout`].
pub fn builtins(
    workspace: Workspace,
    sandbox: Option<crate::config::Sandbox>,
    read_only: bool,
    background_timeout: Option<Duration>,
) -> (Vec<Box<dyn Tool>>, BackgroundTasks) {
    let bash_timeout = crate::config::resolve_bash_timeout(None, workspace.root()).unwrap_or(None);
    builtins_with_bash_timeout(
        workspace,
        sandbox,
        read_only,
        background_timeout,
        bash_timeout,
    )
}

/// Like [`builtins`], with the foreground bash timeout passed explicitly.
/// The main session resolves it from the full config (`[bash]` global +
/// workspace override) via [`crate::config::resolve_bash_timeout`].
pub fn builtins_with_bash_timeout(
    workspace: Workspace,
    sandbox: Option<crate::config::Sandbox>,
    read_only: bool,
    background_timeout: Option<Duration>,
    bash_timeout: Option<Duration>,
) -> (Vec<Box<dyn Tool>>, BackgroundTasks) {
    builtins_with_exa_key(
        workspace,
        std::env::var("EXA_API_KEY").ok(),
        sandbox,
        read_only,
        background_timeout,
        bash_timeout,
    )
}

/// Builtin tools bound to an existing background-task registry.
///
/// Subagents use this path to share global task visibility while their bash
/// facade is independently bound to the subagent Agent's completion sink.
/// `read_only` applies the read-only role policy (see [`builtins`]).
/// `protect_git` comes from the role's frontmatter (default true): when true,
/// bash binds `<workspace>/.git` read-only on non-Windows; the Windows
/// write-sandbox MVP rejects the mode before execution, so a role with
/// `protect_git = false` is how a fixer/subagent gets a working shell under
/// the Windows sandbox.
///
/// `self_session_id` is the calling session's own id (`Some` for subagents,
/// `None` otherwise): it lets `get_background_tasks` annotate the delegate
/// entry that represents the caller itself.
pub fn builtins_with_background(
    workspace: Workspace,
    background: BackgroundTasks,
    sandbox: Option<crate::config::Sandbox>,
    read_only: bool,
    protect_git: bool,
    self_session_id: Option<String>,
) -> Vec<Box<dyn Tool>> {
    // Subagents have no loaded global Config; apply the workspace `[bash]`
    // override plus the default (mirrors `builtins`).
    let bash_timeout = crate::config::resolve_bash_timeout(None, workspace.root()).unwrap_or(None);
    // Subagent / fixer: protect_git defaults to true so .git is read-only;
    // a role frontmatter `protect_git = false` opts out (needed under the
    // Windows sandbox, whose MVP cannot enforce the protection).
    tools_with_background_and_exa_key(
        workspace,
        background,
        std::env::var("EXA_API_KEY").ok(),
        sandbox,
        protect_git,
        read_only,
        bash_timeout,
        self_session_id,
        true,
        // Subagents: enable the unchanged-snapshot poll guard on
        // get_background_tasks with a 3rd-poll termination threshold (each
        // subagent instance owns its tools, so guards are independent per
        // instance; the main agent gets its softer 5th-poll threshold via
        // `builtins`).
        Some(SUBAGENT_POLL_GUARD_THRESHOLD),
    )
}

fn builtins_with_exa_key(
    workspace: Workspace,
    exa_api_key: Option<String>,
    sandbox: Option<crate::config::Sandbox>,
    read_only: bool,
    background_timeout: Option<Duration>,
    bash_timeout: Option<Duration>,
) -> (Vec<Box<dyn Tool>>, BackgroundTasks) {
    let background = BackgroundTasks::new(background_timeout, sandbox.clone());
    // Main agent: protect_git = false so git worktree/add/commit work.
    let tools = tools_with_background_and_exa_key(
        workspace,
        background.clone(),
        exa_api_key,
        sandbox,
        false,
        read_only,
        bash_timeout,
        // The main agent has no own session id to annotate.
        None,
        false,
        // Main builtins carry the unchanged-snapshot poll guard with a
        // 5th-poll termination threshold: the 3rd and 4th consecutive
        // unchanged polls return the reminder, the 5th ends the turn
        // (softer than the subagent 3rd-poll threshold, but repeated
        // get_background_tasks calls never poll forever).
        Some(MAIN_POLL_GUARD_THRESHOLD),
    );
    (tools, background)
}

/// Narrow a resolved sandbox policy for read-only roles: the workspace is
/// mounted read-only and extra writable roots are dropped, while the network
/// follows the main `[sandbox]` configuration (default true) so read-only
/// network operations (curl GET, git fetch) stay possible. Readable roots are
/// kept — reads stay possible. To disable networking for read-only roles too,
/// set `[sandbox] network = false`.
pub(crate) fn read_only_sandbox(sandbox: &crate::config::Sandbox) -> crate::config::Sandbox {
    crate::config::Sandbox {
        enabled: true,
        network: sandbox.network,
        workspace_writable: false,
        writable_paths: Vec::new(),
        readable_paths: sandbox.readable_paths.clone(),
        writable_mounts: Vec::new(),
        readable_mounts: sandbox.readable_mounts.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn tools_with_background_and_exa_key(
    workspace: Workspace,
    background: BackgroundTasks,
    exa_api_key: Option<String>,
    sandbox: Option<crate::config::Sandbox>,
    protect_git: bool,
    read_only: bool,
    bash_timeout: Option<Duration>,
    self_session_id: Option<String>,
    tmp_read_only: bool,
    poll_guard: Option<u8>,
) -> Vec<Box<dyn Tool>> {
    let mut tools = if read_only {
        // Read-only roles get no write/edit tools at all.
        vec![Box::new(ReadFile {
            workspace: workspace.clone(),
        }) as Box<dyn Tool>]
    } else {
        file_tools(&workspace)
    };
    tools.push(Box::new(GetBackgroundTasks::new(
        background.clone(),
        self_session_id.clone(),
        poll_guard,
    )));
    tools.push(Box::new(CancelBackgroundTask {
        background: background.clone(),
    }));
    // Bash for read-only roles only exists inside a narrowed bwrap policy;
    // without one (sandbox disabled / bwrap unavailable) there is no bash —
    // fail closed rather than an unsandboxed shell.
    let bash_sandbox = if read_only {
        sandbox.as_ref().map(read_only_sandbox)
    } else {
        sandbox
    };
    if (!read_only || bash_sandbox.is_some())
        && let Ok(tool) = bash_tool(
            workspace,
            background,
            bash_sandbox,
            protect_git,
            bash_timeout,
            // 发起者会话：subagent 传自己的 session id（None = 主会话/未知），
            // bash 后台任务在共享 registry 里标注真正的发起者。GetBackgroundTasks
            // 后面还要用 self_session_id，这里 clone。
            self_session_id.clone(),
            tmp_read_only,
        )
    {
        tools.push(tool);
    }
    if let Some(key) = exa_api_key
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
    {
        tools.push(Box::new(WebSearch::new(key)));
    }
    // Goal tools ride on every session (main + subagents): each session's
    // runner executes them against ITS OWN goal state, so a subagent can
    // never touch its parent's goal.
    tools.extend(goal_tools());
    // The always-on, read-only `read_output` tool (every session: main,
    // read-only main, ordinary/read-only subagents, btw forks). The runner
    // intercepts it by name with the session's store.
    tools.push(Box::new(ReadOutput));
    tools
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type": "object", "properties": properties, "required": required}),
    }
}

/// Goal-only spec builder: the Goal schemas are CLOSED
/// (`additionalProperties: false`) so misspelled/unknown fields are
/// rejected by the provider's schema validation AND by the runner at
/// runtime. Generic tool schemas stay open.
fn goal_spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{name}` must be a string"))
}

fn optional_usize(arguments: &Value, name: &str) -> Result<Option<usize>, String> {
    arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .get(name)
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| format!("`{name}` must be a non-negative integer"))
        })
        .transpose()
}

fn optional_bool(arguments: &Value, name: &str) -> Result<bool, String> {
    arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("`{name}` must be a boolean"))
        })
        .transpose()
        .map(|value| value.unwrap_or(false))
}

/// `get_goal` / `update_goal`: the RUNNER intercepts their execution by
/// name (the session's goal state + durable commit live in the runner);
/// the fallback `execute` only fires on direct `Agent::run` paths (tests).
pub struct GetGoal;
pub struct UpdateGoal;

pub fn goal_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(GetGoal), Box::new(UpdateGoal)]
}

#[async_trait::async_trait]
impl Tool for GetGoal {
    fn spec(&self) -> ToolSpec {
        goal_spec(
            "get_goal",
            "Read the session's current goal as a FULL JSON snapshot: id, revision, objective, success_criteria, status, progress, evidence, blocked_reason. Goals are created by the user (/goal set …), never by the model.",
            json!({}),
            &[],
        )
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("get_goal is executed by the session runner".into())
    }
}

#[async_trait::async_trait]
impl Tool for UpdateGoal {
    fn spec(&self) -> ToolSpec {
        goal_spec(
            "update_goal",
            "Update the session's goal under an id + revision CAS (revision from get_goal). Actions: progress (set `progress`), pause, resume, block (set `blocked_reason`), complete (requires non-empty `evidence` in the SAME call; pure analysis may use an explicit `unverified: <analysis>` string), clear. `evidence` (string array) appends evidence. Never creates a goal.",
            json!({
                "id": {"type": "string", "description": "goal id from get_goal"},
                "revision": {"type": "integer", "description": "current revision from get_goal (CAS)"},
                "action": {"type": "string", "enum": ["progress", "pause", "resume", "block", "complete", "clear"]},
                "progress": {"type": "string", "description": "required for action=progress"},
                "blocked_reason": {"type": "string", "description": "required for action=block"},
                "success_criteria": {"type": "array", "items": {"type": "string"}, "description": "replaces the goal's success criteria"},
                "evidence": {"type": "array", "items": {"type": "string"}, "description": "appends evidence (required non-empty for complete)"}
            }),
            &["id", "revision", "action"],
        )
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("update_goal is executed by the session runner".into())
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;

/// Windows restricted-token sandbox tests: `#[cfg(all(test, windows))]`
/// (compiled only when running the test suite on Windows, where the
/// write-restriction code paths are real).
#[cfg(all(test, target_os = "linux"))]
#[path = "tools/rust_tests.rs"]
mod rust_tests;

#[cfg(all(test, windows))]
#[path = "tools/windows_sandbox_tests.rs"]
mod windows_sandbox_tests;
