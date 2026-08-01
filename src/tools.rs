use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::{Tool, ToolSpec};
use crate::workspace::Workspace;

mod background;
mod bash;
mod file;
mod tasks;
mod web;

use file::*;
use tasks::*;
use web::*;

pub use background::{
    BackgroundTaskInfo, BackgroundTasks, OutputSlot, SpoolWindow, TaskDisplayMeta, TaskSpool,
};
pub use bash::bash_tool;
pub use file::file_tools;

// The `#[cfg(test)] mod tests` child re-imports these names via `use super::*`.
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use crate::agent::AgentEvent;

#[cfg(test)]
use background::*;

#[cfg(test)]
use bash::*;

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
pub fn builtins_with_background(
    workspace: Workspace,
    background: BackgroundTasks,
    sandbox: Option<crate::config::Sandbox>,
    read_only: bool,
) -> Vec<Box<dyn Tool>> {
    // Subagents have no loaded global Config; apply the workspace `[bash]`
    // override plus the default (mirrors `builtins`).
    let bash_timeout = crate::config::resolve_bash_timeout(None, workspace.root()).unwrap_or(None);
    // Subagent / fixer: protect_git = true so .git is read-only.
    tools_with_background_and_exa_key(
        workspace,
        background,
        std::env::var("EXA_API_KEY").ok(),
        sandbox,
        true,
        read_only,
        bash_timeout,
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
    );
    (tools, background)
}

/// Narrow a resolved sandbox policy for read-only roles: the workspace is
/// mounted read-only, extra writable roots are dropped, and the network is
/// disabled. Readable roots are kept — reads stay possible.
pub(crate) fn read_only_sandbox(sandbox: &crate::config::Sandbox) -> crate::config::Sandbox {
    crate::config::Sandbox {
        enabled: true,
        network: false,
        workspace_writable: false,
        writable_paths: Vec::new(),
        readable_paths: sandbox.readable_paths.clone(),
    }
}

fn tools_with_background_and_exa_key(
    workspace: Workspace,
    background: BackgroundTasks,
    exa_api_key: Option<String>,
    sandbox: Option<crate::config::Sandbox>,
    protect_git: bool,
    read_only: bool,
    bash_timeout: Option<Duration>,
) -> Vec<Box<dyn Tool>> {
    let mut tools = if read_only {
        // Read-only roles get no write/edit tools at all.
        vec![Box::new(ReadFile {
            workspace: workspace.clone(),
        }) as Box<dyn Tool>]
    } else {
        file_tools(&workspace)
    };
    tools.push(Box::new(GetBackgroundTasks {
        background: background.clone(),
    }));
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
    tools
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type": "object", "properties": properties, "required": required}),
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

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
