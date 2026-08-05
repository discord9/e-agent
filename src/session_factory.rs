//! `SessionFactory`: reusable per-session bootstrap pipeline.
//!
//! The single-session bootstrap that used to live in `main.rs::run()` is
//! factored into a factory so a headless server can build many main sessions
//! from one set of startup-resolved configuration. Construction
//! ([`SessionFactory::new`]) resolves everything that is process-global
//! (workspace, config, models, sandbox); [`SessionFactory::build`] runs the
//! per-session pipeline (store connect, fork, tools, MCP, delegate, agent,
//! runner). Behavior is identical to the old `main.rs` flow — build() is a
//! mechanical move of the per-session block, in the same order.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

use crate::agent::{Agent, SessionEntry};
use crate::codex::CodexModel;
use crate::codex_auth::CodexAuth;
use crate::config::{AuthMode, Config, ResolvedModel, Sandbox, resolve_sandbox};
use crate::delegate::{Delegate, Sessions};
use crate::mcp;
use crate::model::{ConfiguredModel, OpenAiModel};
use crate::runner::{IdlePolicy, SessionHandle, SessionRunner};
use crate::session_store::SessionStore;
use crate::tools::{BackgroundTasks, builtins_with_bash_timeout};
use crate::workspace::Workspace;

/// What to do with background-task records left behind by a previous run
/// when building a session.
///
/// The store cannot tell whether the process that owned those tasks is
/// still alive, so the *caller* decides:
///
/// - [`UnfinishedPolicy::Consume`]: process-level startup (CLI/TUI/REPL).
///   The old process is known dead, so its tasks really were killed with
///   it: take the running-task records and inject a "tasks were killed"
///   notice into the resumed history.
/// - [`UnfinishedPolicy::Preserve`]: a server lazily attaching to a session
///   that may still be alive in another process. Do not read the records
///   and do not inject any notice; the owning process clears them itself
///   via `ack_background_entry` → `clear_background_task` when it finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnfinishedPolicy {
    /// Process-level startup: the previous owner is dead; consume the
    /// records and announce that its tasks were killed.
    Consume,
    /// Server-side lazy attach to a possibly-live session: leave the
    /// records untouched for the owning process to acknowledge.
    Preserve,
}

/// Everything resolved once at startup and shared by every built session.
pub struct SessionFactory {
    workspace: Workspace,
    root: PathBuf,
    /// Unified TOML config. Kept so `build()` can read the MCP server
    /// definitions; a fresh set of MCP servers is spawned per build exactly
    /// like `main.rs` did (sharing the spawned connections is Phase 2).
    config: Option<Config>,
    backend: crate::config::SessionBackend,
    main_model: ConfiguredModel,
    main_context_window: Option<u64>,
    subagent_model: Option<ConfiguredModel>,
    subagent_context_window: Option<u64>,
    role_models: HashMap<String, ConfiguredModel>,
    role_context_windows: HashMap<String, Option<u64>>,
    /// ChatGPT auth resolved at startup; runtime `/model` switches reuse it
    /// so `chatgpt` profiles keep working after startup.
    auth: Option<CodexAuth>,
    /// `--base-url` override, honored by runtime `/model` switches exactly
    /// like it is for the main model at startup.
    base_url: Option<String>,
    /// `--model` override, honored by runtime `/model` switches exactly like
    /// it is for the main model at startup (the wire model replaces the
    /// profile's).
    model: Option<String>,
    /// Already-resolved bwrap policy (`None` = sandbox disabled).
    sandbox: Option<Sandbox>,
    read_only: bool,
    agents_instructions: Option<String>,
    skills_instructions: Option<String>,
    /// Print startup/session announcements to stderr (`false` for the TUI,
    /// whose alternate screen must stay clean).
    announce: bool,
}

/// One fully constructed session, ready for the caller to
/// `runner.start(initial_prompt)` and dispatch to its frontend.
pub struct SessionBuild {
    pub runner: SessionRunner,
    pub handle: SessionHandle,
    pub store: SessionStore,
    pub background: BackgroundTasks,
    /// Live subagent session registry (background delegates).
    pub sessions: Sessions,
    /// The effective session id (the fresh `fork-…` id when forking).
    pub session: String,
    pub model_name: String,
    pub role_name: Option<String>,
}

impl SessionFactory {
    /// Resolve all process-global startup state: workspace + external roots,
    /// config, models (main/subagent/roles), sandbox policy, workspace
    /// instructions. `announce` gates the stderr startup messages the TUI
    /// suppresses.
    pub fn new(
        workspace_root: PathBuf,
        profile: Option<&str>,
        base_url: Option<String>,
        model: Option<String>,
        read_only: bool,
        announce: bool,
    ) -> anyhow::Result<Self> {
        let mut workspace = Workspace::new(workspace_root)
            .map_err(anyhow::Error::msg)
            .context("cannot open workspace")?;
        let root = workspace.root().to_path_buf();
        let agents_instructions = read_agents(&root)?;
        let skills_instructions = read_skills_merged(&root)?;
        let config = Config::load_for_workspace(&root)?;
        let backend = config
            .as_ref()
            .map(|c| c.session_backend())
            .unwrap_or_default();
        // Migrate pre-session-id files only when using the JSONL backend;
        // GreptimeDB has its own session namespace and does not need file
        // migration.
        if matches!(backend, crate::config::SessionBackend::Jsonl) {
            for (old, new) in crate::session::migrate_legacy(&root) {
                eprintln!("e-agent: migrated session {old} -> {new}");
            }
        }
        // Web search reads EXA_API_KEY from the process env (tools.rs and
        // subagents pick it up there). When unset, fall back to the
        // `[web_search]` config section by injecting it into the env once at
        // startup — this keeps the key's single transport mechanism and
        // avoids threading it through every tools constructor. Startup is
        // single-threaded, so set_var is safe.
        if std::env::var_os("EXA_API_KEY").is_none()
            && let Some(config) = &config
            && let Some(key) = config.web_search_key()?
        {
            unsafe { std::env::set_var("EXA_API_KEY", key) };
        }
        let (main_resolved, role_resolved, all_roles) = match &config {
            Some(config) => (
                Some(config.resolve(profile)?),
                config
                    .resolve_role("subagent")
                    .context("cannot resolve [roles] subagent profile")?,
                config
                    .resolve_roles()
                    .context("cannot resolve [roles] profiles")?,
            ),
            None => (None, None, HashMap::new()),
        };
        let mut main_context_window = main_resolved.as_ref().and_then(|r| r.context_window);
        // When --model overrides the profile's wire model, the profile's
        // context window is no longer valid for the unknown model.
        let model_override = model.is_some();
        if matches!(
            main_resolved.as_ref().map(|value| value.auth),
            Some(AuthMode::ChatGpt)
        ) && base_url.is_some()
        {
            return Err(anyhow!(
                "--base-url cannot be used with a provider using auth = `chatgpt`"
            ));
        }
        let needs_chatgpt = main_resolved
            .as_ref()
            .is_some_and(|value| value.auth == AuthMode::ChatGpt)
            || all_roles
                .values()
                .any(|value| value.auth == AuthMode::ChatGpt);
        let auth = needs_chatgpt.then(CodexAuth::load).transpose()?;
        // The startup overrides are consumed by `configured_model` below;
        // keep clones so runtime `/model` switches honor the same flags.
        let base_url_override = base_url.clone();
        let model_override_flag = model.clone();
        let main_model = match main_resolved {
            Some(configured) => configured_model(configured, auth.as_ref(), base_url, model)?,
            None => {
                if profile.is_some() {
                    return Err(anyhow!(
                        "--profile requires a config file at $XDG_CONFIG_HOME/e-agent/config.toml or $HOME/.config/e-agent/config.toml"
                    ));
                }
                ConfiguredModel::chat(OpenAiModel::from_env(base_url, model)?)
            }
        };
        if model_override {
            main_context_window = None;
        }
        let subagent_context_window = role_resolved.as_ref().and_then(|r| r.context_window);
        let subagent_model = role_resolved
            .map(|resolved| configured_model(resolved, auth.as_ref(), None, None))
            .transpose()?;
        let mut role_models = HashMap::new();
        let mut role_context_windows = HashMap::new();
        for (role, resolved) in all_roles {
            role_context_windows.insert(role.clone(), resolved.context_window);
            role_models.insert(role, configured_model(resolved, auth.as_ref(), None, None)?);
        }
        // Resolve one shared canonical policy. `enabled` controls only bwrap;
        // file capabilities remain active independently.
        let resolved_policy = resolve_sandbox(config.as_ref(), &root)?;
        workspace = workspace
            .with_external_roots(&resolved_policy)
            .map_err(anyhow::Error::msg)?;
        let sandbox = resolved_policy.enabled.then_some(resolved_policy.clone());
        if let Some(policy) = &sandbox {
            preflight_sandbox(policy, announce)?;
        }
        Ok(Self {
            workspace,
            root,
            config,
            backend,
            main_model,
            main_context_window,
            subagent_model,
            subagent_context_window,
            role_models,
            role_context_windows,
            auth,
            base_url: base_url_override,
            model: model_override_flag,
            sandbox,
            read_only,
            agents_instructions,
            skills_instructions,
            announce,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The session storage backend this factory builds stores for.
    pub fn backend(&self) -> &crate::config::SessionBackend {
        &self.backend
    }

    /// The TUI submit/newline key mapping from `[tui]` (default
    /// Enter/Alt+Enter when the config or section is absent). Errors on
    /// unsupported keys or submit == newline; the caller refuses startup.
    pub fn tui_keys(&self) -> anyhow::Result<crate::config::InputKeys> {
        match &self.config {
            Some(config) => config.tui_keys(),
            None => Ok(crate::config::InputKeys::default()),
        }
    }

    /// The main model's context window (already cleared when `--model`
    /// overrode the profile's wire model).
    pub fn main_context_window(&self) -> Option<u64> {
        self.main_context_window
    }

    /// The main model, shared by every built session. The btw fork entry
    /// (`delegate::spawn_btw_subagent`) runs the subagent on the source
    /// session's own model, so the server passes this through.
    pub fn main_model(&self) -> &ConfiguredModel {
        &self.main_model
    }

    /// Resolve a config profile (`provider/model`) to a configured model at
    /// runtime, for the web/TUI `/model <profile>` switch. Honors the same
    /// `--base-url`/`--model` overrides the main model was built with. Errors
    /// when there is no config or the profile is unknown — the caller turns
    /// that into a 400. Note: a runtime switch does not touch the agent's
    /// context window (that stays the startup `main_context_window`).
    pub fn resolve_profile(&self, profile: &str) -> anyhow::Result<ConfiguredModel> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow!("no config file; cannot resolve model profile `{profile}`"))?;
        let resolved = config.resolve_profile(profile)?;
        configured_model(
            resolved,
            self.auth.as_ref(),
            self.base_url.clone(),
            self.model.clone(),
        )
    }

    /// Every switchable model profile name (`[models]` keys + `[roles]`
    /// values, deduplicated and sorted), for the web `/model` autocomplete
    /// (`GET /api/models`). Empty when there is no config.
    pub fn model_profiles(&self) -> Vec<String> {
        self.config
            .as_ref()
            .map(crate::config::Config::model_profiles)
            .unwrap_or_default()
    }

    /// The summarizer model for cheap per-turn session summaries (desktop
    /// pet). Routed via `[roles] summarizer` (e.g. deepseek/flash with
    /// thinking off); falls back to the main model when not configured.
    pub fn summarizer_model(&self) -> ConfiguredModel {
        self.role_models
            .get("summarizer")
            .cloned()
            .unwrap_or_else(|| self.main_model.clone())
    }

    /// The workspace every session works in; btw fork subagents inherit it.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// The resolved bwrap policy (`None` = sandbox disabled); btw fork
    /// subagents inherit it.
    pub fn sandbox(&self) -> Option<&Sandbox> {
        self.sandbox.as_ref()
    }

    /// The read-only policy every session builds with; btw fork subagents
    /// inherit it (a read-only parent forks read-only subagents).
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Build one session end-to-end, in the exact order `main.rs::run()`
    /// used: store connect → fork → tools → MCP → delegate → agent →
    /// context prefix → history restore → unfinished-background handling →
    /// context window → runner.
    ///
    /// `id` is the requested session id; `fork_from` is `(source session,
    /// optional 1-based entry index)` and replaces `id` with a fresh
    /// `fork-…` id. `unfinished` selects how leftover background-task
    /// records are handled: `Consume` (process-level startup; see
    /// [`UnfinishedPolicy`]) takes them and injects a "killed with the
    /// process" notice, `Preserve` (server lazy attach) leaves them for
    /// the owning process. The caller still chooses when to
    /// `runner.start(...)` with its initial prompt.
    pub async fn build(
        &self,
        id: &str,
        fork_from: Option<(String, Option<usize>)>,
        max_rounds: Option<usize>,
        policy: IdlePolicy,
        unfinished: UnfinishedPolicy,
    ) -> anyhow::Result<SessionBuild> {
        let mut session = id.to_owned();
        let mut store = SessionStore::connect(&self.backend, &self.root, &session).await?;
        if let Some((source, at)) = fork_from {
            // Fork: copy the source session's history up to a completed-turn
            // boundary into a brand-new session id. The source is only read;
            // it never changes.
            let source_store = SessionStore::connect(&self.backend, &self.root, &source).await?;
            let with_seq = source_store
                .load_with_seq(&self.root, &source)
                .await
                .with_context(|| format!("cannot load source session {source} for fork"))?;
            let source_entries: Vec<SessionEntry> =
                with_seq.iter().map(|(_, entry)| entry.clone()).collect();
            let prefix = crate::agent::fork_prefix(&source_entries, at)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("cannot fork session {source}"))?;
            // `at` (1-based, inclusive) is the index of the last kept entry.
            let at = prefix.len();
            let seq = with_seq.get(at - 1).map(|(seq, _)| *seq);
            let marker = SessionEntry::ForkedFrom {
                source: source.clone(),
                at,
                // JSONL has no event_time column and SessionStore does not
                // expose one; kept as an optional provenance slot.
                event_time: None,
                seq,
            };
            // The marker sits at the fork point: source prefix first, then
            // the marker, then the session's own new messages — so scrolling
            // the forked session reads as "source history … forked from …
            // new work".
            let mut fork_entries = Vec::with_capacity(prefix.len() + 1);
            fork_entries.extend(prefix);
            fork_entries.push(marker);
            let new_id = crate::session::new_id_prefixed("fork-");
            store = SessionStore::connect(&self.backend, &self.root, &new_id).await?;
            match self.backend {
                // Atomic create-or-replace for a brand-new JSONL session file.
                crate::config::SessionBackend::Jsonl => {
                    store.rewrite(&self.root, &new_id, &fork_entries).await?
                }
                // Greptime: fresh session (no rows, next_seq = 0); append
                // writes contiguous seqs with fresh timestamps. The marker's
                // provenance fields are payload-only.
                crate::config::SessionBackend::Greptime { .. } => {
                    store.append(&self.root, &new_id, &fork_entries).await?
                }
                // SQLite: same append-only semantics as Greptime.
                crate::config::SessionBackend::Sqlite { .. } => {
                    store.append(&self.root, &new_id, &fork_entries).await?
                }
            }
            eprintln!("e-agent: forked session: {new_id}");
            session = new_id;
        }
        let background_timeout =
            crate::config::resolve_background_timeout(self.config.as_ref(), &self.root)
                .unwrap_or(None);
        let bash_timeout =
            crate::config::resolve_bash_timeout(self.config.as_ref(), &self.root).unwrap_or(None);
        let (mut tools, background) = builtins_with_bash_timeout(
            self.workspace.clone(),
            self.sandbox.clone(),
            self.read_only,
            background_timeout,
            bash_timeout,
        );
        // Read-only sessions skip MCP entirely: MCP tools carry no read-only
        // marker, so exposing them would defeat the policy. Delegation stays —
        // spawning a subagent does not mutate this session's host state, and
        // each subagent resolves its own role template (read-only or not).
        let (mcp_tools, mcp_instructions) = if self.read_only {
            (Vec::new(), Vec::new())
        } else {
            let mcp_servers = self
                .config
                .as_ref()
                .map(|c| c.mcp.clone())
                .unwrap_or_default();
            mcp::connect_all(mcp_servers, &self.root).await
        };
        tools.extend(mcp_tools);
        let mut delegate = Delegate::new(
            self.main_model.clone(),
            self.workspace.clone(),
            background.clone(),
        )
        .persist_sessions(self.root.clone())
        .with_role_models(self.role_models.clone())
        .with_role_context_windows(self.role_context_windows.clone())
        .with_subagent_context_window(self.subagent_context_window)
        .with_roles_root(self.root.clone())
        .with_sandbox(self.sandbox.clone())
        .record_background_tasks_in(self.root.clone(), &session, store.clone())
        .with_persist_store(self.backend.clone());
        if let Some(subagent_model) = &self.subagent_model {
            let name = subagent_model.display_name().to_owned();
            delegate = delegate.with_subagent_model(subagent_model.clone());
            if self.announce {
                eprintln!("e-agent: subagent model {name}");
            }
        }
        let subagent_sessions = delegate.sessions();
        tools.push(Box::new(delegate));
        let mut agent = Agent::new(Box::new(self.main_model.clone()), tools);
        let mut context = Vec::new();
        // The main agent's orchestrator template (.e-agent/agents/main.md)
        // leads; it tells the model to decompose work and delegate to the
        // named roles.
        let role_name = match crate::roles::role_prompt(&self.root, crate::roles::MAIN_ROLE)? {
            Some(orchestrator) => {
                context.push(orchestrator);
                Some(crate::roles::MAIN_ROLE.to_owned())
            }
            None => None,
        };
        if let Some(instructions) = &self.agents_instructions {
            context.push(format!("## AGENTS.md\n\n{instructions}"));
        }
        if let Some(skills) = &self.skills_instructions {
            context.push(skills.clone());
        }
        context.extend(mcp_instructions);
        if !context.is_empty() {
            agent.set_context_prefix(context.join("\n\n"));
        }
        if let Some(rounds) = max_rounds {
            agent = agent.max_tool_rounds(rounds);
        }
        let loaded = store.load(&self.root, &session).await?;
        let legacy = loaded.legacy;
        agent.restore_history(loaded.entries);
        agent.record_background_tasks_in(self.root.clone(), &session, store.clone());
        if matches!(unfinished, UnfinishedPolicy::Consume) {
            // Process-level startup only: the previous owner is dead, so its
            // tasks really were killed with the process. Preserve skips this
            // entirely — the server may be attaching to a session that is
            // still live in another process, and that owner clears its own
            // records via ack_background_entry → clear_background_task.
            let unfinished = store
                .take_unfinished_background(&self.root, &session)
                .await?;
            if !unfinished.is_empty() {
                let notice = format!(
                    "[e-agent exited with {} background task(s) still running; they were killed with the process. Re-run them if still needed:]\n{}",
                    unfinished.len(),
                    unfinished.join("\n")
                );
                let entry = crate::agent::SessionEntry::Notice {
                    text: notice.clone(),
                };
                // Persist immediately so a crash-before-first-turn cannot inject
                // the same notice again on the next launch.
                store
                    .append(&self.root, &session, std::slice::from_ref(&entry))
                    .await?;
                // Append (NOT restore_history, which would wipe the resumed history).
                agent.push_entry(entry);
            }
        }
        if legacy {
            store.rewrite(&self.root, &session, agent.history()).await?;
        }

        if let Some(window) = self.main_context_window {
            agent.set_context_window(window);
        }

        let model_name = self.main_model.display_name().to_owned();
        // Sessions metadata: write the creation snapshot for a brand-new
        // session (model/role from the factory configuration; parent links
        // are None for main sessions). A resumed session (`--session`,
        // server resume, fork-source reconnect) already has its row —
        // create_meta is idempotent and appends nothing — and the runner's
        // turn-boundary touch keeps it fresh.
        store
            .create_meta(
                &self.root,
                &session,
                Some(&model_name),
                role_name.as_deref(),
                None,
                None,
                None, // main sessions keep manual naming; title is a subagent-spawn concern
            )
            .await?;
        let (runner, handle) = SessionRunner::new(
            agent,
            store.clone(),
            self.root.clone(),
            session.clone(),
            policy,
        );
        Ok(SessionBuild {
            runner,
            handle,
            store,
            background,
            sessions: subagent_sessions,
            session,
            model_name,
            role_name,
        })
    }

    /// Test-only factory: the real constructor resolves config, model env
    /// keys and sandbox policy, which the server tests must not depend on.
    /// The server's test handlers only ever call `root()` (registry-miss
    /// paths short-circuit before any store I/O), never `build()`.
    #[cfg(test)]
    pub(crate) fn test_factory(root: PathBuf) -> Self {
        Self::test_factory_with_config(root, None)
    }

    /// Test-only factory carrying a config, so runtime `/model` endpoint
    /// tests can exercise real profile resolution (`resolve_profile`)
    /// without touching the user's global config.
    #[cfg(test)]
    pub(crate) fn test_factory_with_config(root: PathBuf, config: Option<Config>) -> Self {
        let workspace = Workspace::new(root.clone()).expect("temp workspace");
        let main_model = ConfiguredModel::chat(
            OpenAiModel::new(
                "http://localhost".into(),
                "test-key".into(),
                "test-model".into(),
                None,
            )
            .expect("test model"),
        );
        Self {
            workspace,
            root,
            config,
            backend: crate::config::SessionBackend::Jsonl,
            main_model,
            main_context_window: None,
            subagent_model: None,
            subagent_context_window: None,
            role_models: HashMap::new(),
            role_context_windows: HashMap::new(),
            auth: None,
            base_url: None,
            model: None,
            sandbox: None,
            read_only: false,
            agents_instructions: None,
            skills_instructions: None,
            announce: false,
        }
    }
}

fn preflight_sandbox(policy: &Sandbox, announce: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        if !policy.network {
            return Err(anyhow!(
                "Windows write-sandbox MVP does not implement network isolation"
            ));
        }
        if announce {
            eprintln!("e-agent: shell write-restricted with a Windows restricted token");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = policy;
        if !crate::tools::bwrap_available() {
            return Err(anyhow!(
                "[sandbox] enabled = true but bwrap is not available. \
                 Install bubblewrap or disable the sandbox."
            ));
        }
        if announce {
            eprintln!("e-agent: bash sandboxed with bwrap");
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn enabled_preflight_does_not_require_bwrap() {
        preflight_sandbox(
            &Sandbox {
                enabled: true,
                network: true,
                workspace_writable: true,
                writable_paths: Vec::new(),
                readable_paths: Vec::new(),
            },
            false,
        )
        .unwrap();
    }

    #[test]
    fn network_isolation_request_is_rejected() {
        let error = preflight_sandbox(
            &Sandbox {
                enabled: true,
                network: false,
                workspace_writable: true,
                writable_paths: Vec::new(),
                readable_paths: Vec::new(),
            },
            false,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not implement network isolation")
        );
    }
}

fn read_agents(root: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(root.join("AGENTS.md")) {
        Ok(content) if content.trim().is_empty() => Ok(None),
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("cannot read workspace AGENTS.md"),
    }
}

/// Read all workspace skills from `.e-agent/skills/<name>/SKILL.md`.
///
/// Returns `None` (silently) when the directory is missing, empty, or contains
/// only non-directories / missing / empty SKILL.md files. Actual I/O or UTF-8
/// errors on a SKILL.md that should be readable are returned with a path
/// context.
///
/// Skills are sorted by `<name>` (dictionary order, stable) and joined as a
/// single block prefixed with `## Skill: <name>` per skill.
/// Scan a single skill directory and return (name, content) pairs.
///
/// Missing dir, non-directory entries, missing/empty SKILL.md are silently
/// skipped. I/O/UTF-8 errors on a readable SKILL.md bubble up with path
/// context.
pub fn read_skills_from(dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let dir_entries = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context(format!("cannot read {}", dir.display())),
    };

    let mut skills: Vec<(String, String)> = Vec::new();

    for entry in dir_entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Err(e).context(format!("cannot read {} entry", dir.display())),
        };

        // Only directories are candidate skill folders
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }

        let skill_name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };

        let skill_path = entry.path().join("SKILL.md");

        match std::fs::read_to_string(&skill_path) {
            Ok(content) => {
                if !content.trim().is_empty() {
                    skills.push((skill_name, content));
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).context(format!("cannot read {}", skill_path.display()));
            }
        }
    }

    Ok(skills)
}

/// Merge skills from `global_dir` (e.g. `Config::config_dir()/skills/`) and
/// `workspace_dir` (`.e-agent/skills/`).  Workspace entries override same-name
/// globals.  Returns `None` silently when both are missing/empty.
pub fn read_skills_merge(
    global_dir: Option<&Path>,
    workspace_dir: &Path,
) -> anyhow::Result<Option<String>> {
    let mut merged: HashMap<String, String> = HashMap::new();
    if let Some(global) = global_dir {
        for (name, content) in read_skills_from(global)? {
            merged.insert(name, content);
        }
    }
    for (name, content) in read_skills_from(workspace_dir)? {
        merged.insert(name, content);
    }
    if merged.is_empty() {
        return Ok(None);
    }
    let mut skills: Vec<_> = merged.into_iter().collect();
    skills.sort_by(|a, b| a.0.cmp(&b.0));
    let combined = skills
        .into_iter()
        .map(|(name, content)| format!("## Skill: {name}\n\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Some(combined))
}

/// Production entry: global from `Config::config_dir()/skills/`, workspace
/// from `<root>/.e-agent/skills/`.
fn read_skills_merged(root: &Path) -> anyhow::Result<Option<String>> {
    let global = crate::config::config_dir().map(|d| d.join("skills"));
    let workspace = root.join(".e-agent").join("skills");
    read_skills_merge(global.as_deref(), &workspace)
}

/// Turn a resolved profile into a wire model, honoring `--base-url`/`--model`
/// overrides for the main model.
fn configured_model(
    resolved: ResolvedModel,
    auth: Option<&CodexAuth>,
    base_url: Option<String>,
    model: Option<String>,
) -> anyhow::Result<ConfiguredModel> {
    let display = Some(resolved.display);
    match resolved.auth {
        AuthMode::ApiKey => {
            let mut cm = ConfiguredModel::chat(
                OpenAiModel::new(
                    base_url.unwrap_or(resolved.base_url),
                    resolved.api_key,
                    model.unwrap_or(resolved.model),
                    resolved.reasoning_effort,
                )?
                .with_vision(resolved.vision)
                .with_thinking(resolved.thinking),
            );
            cm.display = display;
            Ok(cm)
        }
        AuthMode::ChatGpt => {
            let mut cm = ConfiguredModel::codex(
                CodexModel::new(
                    auth.cloned()
                        .ok_or_else(|| anyhow!("ChatGPT auth was not initialized"))?,
                    model.unwrap_or(resolved.model),
                    resolved.reasoning_effort,
                )?
                .with_vision(resolved.vision),
            );
            cm.display = display;
            Ok(cm)
        }
    }
}
