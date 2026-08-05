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
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

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
/// The caller decides based on what it knows about the previous owner:
///
/// - [`UnfinishedPolicy::Consume`]: process-level startup (CLI/TUI/REPL),
///   and server lazy attach after the owner-liveness probe
///   ([`SessionStore::unfinished_owner_all_dead`]) proved every previous
///   owner dead. The old process is known dead, so its tasks really were
///   killed with it: take the running-task records and inject a "tasks
///   were killed" notice into the resumed history.
/// - [`UnfinishedPolicy::Preserve`]: a server lazily attaching to a session
///   that may still be alive in another process (a live owner, an old
///   record without an owner, or an unjudgeable identity). Do not read
///   the records and do not inject any notice; the owning process clears
///   them itself via `ack_background_entry` → `clear_background_task`
///   when it finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnfinishedPolicy {
    /// The previous owner is dead; consume the records and announce that
    /// its tasks were killed.
    Consume,
    /// The session may still be live in another process: leave the
    /// records untouched for the owning process to acknowledge.
    Preserve,
}

/// Everything resolved once at startup and shared by every built session.
pub struct SessionFactory {
    workspace: Workspace,
    root: PathBuf,
    /// Runtime-reloadable state: the unified TOML config plus everything
    /// derived from it that session builds consume. Swapped atomically by
    /// the config watcher ([`SessionFactory::spawn_config_watcher`]); every
    /// runtime read takes a short lock and clones what it needs.
    reloadable: Arc<RwLock<ReloadableState>>,
    backend: crate::config::SessionBackend,
    main_model: ConfiguredModel,
    main_context_window: Option<u64>,
    subagent_model: Option<ConfiguredModel>,
    subagent_context_window: Option<u64>,
    role_models: HashMap<String, ConfiguredModel>,
    role_context_windows: HashMap<String, Option<u64>>,
    /// `--profile` override, kept so hot reloads and `build()` re-resolve
    /// the same main profile the process started with.
    profile: Option<String>,
    /// `--base-url` override, honored by runtime `/model` switches exactly
    /// like it is for the main model at startup.
    base_url: Option<String>,
    /// `--model` override, honored by runtime `/model` switches exactly like
    /// it is for the main model at startup (the wire model replaces the
    /// profile's).
    model: Option<String>,
    /// Already-resolved bwrap policy (`None` = sandbox disabled). Fixed at
    /// startup: `[sandbox]` changes require a restart (workspace roots and
    /// file capabilities are wired at construction).
    sandbox: Option<Sandbox>,
    read_only: bool,
    agents_instructions: Option<String>,
    skills_instructions: Option<String>,
    /// Print startup/session announcements to stderr (`false` for the TUI,
    /// whose alternate screen must stay clean).
    announce: bool,
}

/// The subset of factory state a config hot reload can replace. Stored
/// behind an `Arc<RwLock<..>>` so the reload watcher can swap it while the
/// server/TUI keep resolving models from a consistent snapshot.
struct ReloadableState {
    /// The effective config (global + project merge). `None` = no config.
    config: Option<Config>,
    /// ChatGPT auth, loaded when any resolved profile uses
    /// `auth = "chatgpt"`; re-loaded on reload when the new config needs
    /// it. Kept here (not re-read per build) so chatgpt profiles keep
    /// working exactly like they do at startup.
    auth: Option<CodexAuth>,
    /// Models re-resolved from `config` (main / subagent role / all routed
    /// roles). Pre-resolved at swap time so `build()` stays infallible on
    /// config resolution and a bad config edit is rejected once, at reload,
    /// instead of failing every new session build. `None` when there is no
    /// config (builds then fall back to the startup-resolved fields).
    models: Option<RuntimeModels>,
}

/// Every switchable model of the effective config, resolved to wire models.
#[derive(Clone)]
struct RuntimeModels {
    main: ConfiguredModel,
    main_context_window: Option<u64>,
    subagent: Option<ConfiguredModel>,
    subagent_context_window: Option<u64>,
    roles: HashMap<String, ConfiguredModel>,
    role_context_windows: HashMap<String, Option<u64>>,
}

/// Profiles resolved without building wire models: the shared input of
/// startup, reload validation and [`build_runtime_models`].
struct ProfilesResolved {
    main: ResolvedModel,
    subagent: Option<ResolvedModel>,
    roles: HashMap<String, ResolvedModel>,
}

/// Outcome of one config reload attempt.
#[derive(Debug)]
pub enum ReloadResult {
    /// Nothing changed (watcher tick with no mtime change).
    NoChange,
    /// The new config parsed, validated and was swapped in.
    Reloaded,
    /// The new config failed to parse or resolve; the previous config is
    /// kept. Carries the reason for logging.
    Rejected(String),
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
        let (runtime, auth) = match &config {
            Some(config) => {
                let resolved = resolved_profiles(config, profile)?;
                // `--base-url` cannot combine with a chatgpt-routed main
                // profile; checked before auth load so this message wins
                // over an auth error. Also enforced by reload validation
                // (`apply_reloaded_config`).
                if matches!(resolved.main.auth, AuthMode::ChatGpt) && base_url.is_some() {
                    return Err(anyhow!(
                        "--base-url cannot be used with a provider using auth = `chatgpt`"
                    ));
                }
                let needs_chatgpt = resolved.main.auth == AuthMode::ChatGpt
                    || resolved
                        .roles
                        .values()
                        .any(|value| value.auth == AuthMode::ChatGpt);
                let auth = needs_chatgpt.then(CodexAuth::load).transpose()?;
                let models = build_runtime_models(resolved, auth.as_ref(), &base_url, &model)?;
                (Some(models), auth)
            }
            None => (None, None),
        };
        // The startup overrides stay stored so runtime `/model` switches and
        // hot reloads honor the same flags the main model was built with.
        let base_url_override = base_url.clone();
        let model_override_flag = model.clone();
        let (
            main_model,
            main_context_window,
            subagent_model,
            subagent_context_window,
            role_models,
            role_context_windows,
        ) = match &runtime {
            Some(models) => (
                models.main.clone(),
                models.main_context_window,
                models.subagent.clone(),
                models.subagent_context_window,
                models.roles.clone(),
                models.role_context_windows.clone(),
            ),
            None => {
                if profile.is_some() {
                    return Err(anyhow!(
                        "--profile requires a config file at $XDG_CONFIG_HOME/e-agent/config.toml or $HOME/.config/e-agent/config.toml"
                    ));
                }
                let main_model = ConfiguredModel::chat(OpenAiModel::from_env(base_url, model)?);
                (main_model, None, None, None, HashMap::new(), HashMap::new())
            }
        };
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
            reloadable: Arc::new(RwLock::new(ReloadableState {
                config,
                auth,
                models: runtime,
            })),
            backend,
            main_model,
            main_context_window,
            subagent_model,
            subagent_context_window,
            role_models,
            role_context_windows,
            profile: profile.map(str::to_owned),
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
    /// Live: resolves against the CURRENT (possibly hot-reloaded) config,
    /// like `main_model()` and the other runtime reads.
    pub fn tui_keys(&self) -> anyhow::Result<crate::config::InputKeys> {
        let state = self.reloadable.read().unwrap();
        match &state.config {
            Some(config) => config.tui_keys(),
            None => Ok(crate::config::InputKeys::default()),
        }
    }

    /// The main model's context window (already cleared when `--model`
    /// overrode the profile's wire model).
    pub fn main_context_window(&self) -> Option<u64> {
        let state = self.reloadable.read().unwrap();
        state
            .models
            .as_ref()
            .map(|models| models.main_context_window)
            .unwrap_or(self.main_context_window)
    }

    /// The main model, shared by every built session. The btw fork entry
    /// (`delegate::spawn_btw_subagent`) runs the subagent on the source
    /// session's own model, so the server passes this through. Live: after a
    /// config hot reload swapped in a new default model this returns the
    /// reloaded model (falling back to the startup snapshot with no config).
    pub fn main_model(&self) -> ConfiguredModel {
        let state = self.reloadable.read().unwrap();
        state
            .models
            .as_ref()
            .map(|models| models.main.clone())
            .unwrap_or_else(|| self.main_model.clone())
    }

    /// Resolve a config profile (`provider/model`) to a configured model at
    /// runtime, for the web/TUI `/model <profile>` switch. Honors the same
    /// `--base-url`/`--model` overrides the main model was built with and
    /// resolves against the CURRENT (possibly hot-reloaded) config. Errors
    /// when there is no config or the profile is unknown — the caller turns
    /// that into a 400. Note: a runtime switch does not touch the agent's
    /// context window (that stays the build-time `main_context_window`).
    pub fn resolve_profile(&self, profile: &str) -> anyhow::Result<ConfiguredModel> {
        let state = self.reloadable.read().unwrap();
        let config = state
            .config
            .as_ref()
            .ok_or_else(|| anyhow!("no config file; cannot resolve model profile `{profile}`"))?;
        let resolved = config.resolve_profile(profile)?;
        configured_model(
            resolved,
            state.auth.as_ref(),
            self.base_url.clone(),
            self.model.clone(),
        )
    }

    /// Every switchable model profile name (`[models]` keys + `[roles]`
    /// values, deduplicated and sorted), for the web `/model` autocomplete
    /// (`GET /api/models`). Empty when there is no config. Live: reflects
    /// hot-reloaded `[models]` / `[roles]`.
    pub fn model_profiles(&self) -> Vec<String> {
        self.reloadable
            .read()
            .unwrap()
            .config
            .as_ref()
            .map(crate::config::Config::model_profiles)
            .unwrap_or_default()
    }

    /// The summarizer model for cheap per-turn session summaries (desktop
    /// pet). Routed via `[roles] summarizer` (e.g. deepseek/flash with
    /// thinking off); falls back to the main model when not configured.
    /// Live: uses the current (possibly hot-reloaded) role routing.
    pub fn summarizer_model(&self) -> ConfiguredModel {
        let state = self.reloadable.read().unwrap();
        match &state.models {
            Some(models) => models
                .roles
                .get("summarizer")
                .cloned()
                .unwrap_or_else(|| models.main.clone()),
            None => self
                .role_models
                .get("summarizer")
                .cloned()
                .unwrap_or_else(|| self.main_model.clone()),
        }
    }

    /// Snapshot of the current (possibly hot-reloaded) config. Runtime
    /// readers that render config-driven UI (e.g. the TUI's key bindings)
    /// re-read this when they want live config instead of the startup copy.
    pub fn current_config(&self) -> Option<Config> {
        self.reloadable.read().unwrap().config.clone()
    }

    /// Re-read the config files for this workspace and, when they changed
    /// and the new config parses + resolves, atomically swap it in. Returns
    /// the outcome for logging. Never fails the process: a bad edit keeps
    /// the previous config and is reported via [`ReloadResult::Rejected`].
    pub fn reload_config(&self) -> ReloadResult {
        reload_config_at(
            &self.reloadable,
            &self.root,
            self.profile.as_deref(),
            &self.base_url,
            &self.model,
        )
    }

    /// Spawn a detached task that polls the config files
    /// ([`crate::config::config_watch_paths`]) every
    /// [`crate::config::CONFIG_POLL_INTERVAL`] and hot-reloads on change.
    /// One per long-running frontend (headless server, TUI). Reload
    /// announcements go to stderr only when `announce` is set (the server);
    /// the TUI keeps its alternate screen clean and surfaces nothing.
    pub fn spawn_config_watcher(&self) {
        let announce = self.announce;
        let root = self.root.clone();
        let reloadable = self.reloadable.clone();
        let profile = self.profile.clone();
        let base_url = self.base_url.clone();
        let model = self.model.clone();
        tokio::spawn(async move {
            let mut mtimes: HashMap<PathBuf, Option<SystemTime>> =
                crate::config::config_watch_paths(&root)
                    .into_iter()
                    .map(|path| {
                        let mtime = config_file_mtime(&path);
                        (path, mtime)
                    })
                    .collect();
            let mut ticker = tokio::time::interval(crate::config::CONFIG_POLL_INTERVAL);
            // The first tick fires immediately; startup already loaded the
            // config, so consume it without checking.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if !watch_paths_changed(&mut mtimes) {
                    continue;
                }
                match reload_config_at(&reloadable, &root, profile.as_deref(), &base_url, &model) {
                    ReloadResult::Reloaded => {
                        if announce {
                            eprintln!(
                                "e-agent: config reloaded: new sessions and `/model` switches use the updated config; \
                                 [sandbox] / [session] backend / web-search key changes still need a restart"
                            );
                        }
                    }
                    ReloadResult::Rejected(reason) => {
                        if announce {
                            eprintln!(
                                "e-agent: config reload rejected (keeping the previous config): {reason}"
                            );
                        }
                    }
                    ReloadResult::NoChange => {}
                }
            }
        });
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
    /// records are handled: `Consume` (process-level startup, or server
    /// attach after the owner-liveness probe; see [`UnfinishedPolicy`])
    /// takes them and injects a "killed with the process" notice,
    /// `Preserve` (server lazy attach to a possibly-live session, or a
    /// fork, which has no records of its own) leaves them for
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
        // Snapshot the reloadable state once: the config watcher may swap it
        // between awaits, so everything below uses one consistent view. The
        // models are pre-resolved at construction/reload (a bad edit never
        // reaches here), so this block is infallible.
        let (config, models) = {
            let state = self.reloadable.read().unwrap();
            (state.config.clone(), state.models.clone())
        };
        let (
            main_model,
            subagent_model,
            role_models,
            role_context_windows,
            subagent_context_window,
            main_context_window,
        ) = match &models {
            Some(models) => (
                models.main.clone(),
                models.subagent.clone(),
                models.roles.clone(),
                models.role_context_windows.clone(),
                models.subagent_context_window,
                models.main_context_window,
            ),
            None => (
                self.main_model.clone(),
                self.subagent_model.clone(),
                self.role_models.clone(),
                self.role_context_windows.clone(),
                self.subagent_context_window,
                self.main_context_window,
            ),
        };
        let background_timeout =
            crate::config::resolve_background_timeout(config.as_ref(), &self.root).unwrap_or(None);
        let bash_timeout =
            crate::config::resolve_bash_timeout(config.as_ref(), &self.root).unwrap_or(None);
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
            let mcp_servers = config.as_ref().map(|c| c.mcp.clone()).unwrap_or_default();
            mcp::connect_all(mcp_servers, &self.root).await
        };
        tools.extend(mcp_tools);
        let mut delegate = Delegate::new(
            main_model.clone(),
            self.workspace.clone(),
            background.clone(),
        )
        .persist_sessions(self.root.clone())
        .with_role_models(role_models.clone())
        .with_role_context_windows(role_context_windows.clone())
        .with_subagent_context_window(subagent_context_window)
        .with_roles_root(self.root.clone())
        .with_sandbox(self.sandbox.clone())
        .record_background_tasks_in(self.root.clone(), &session, store.clone())
        .with_persist_store(self.backend.clone());
        if let Some(subagent_model) = &subagent_model {
            let name = subagent_model.display_name().to_owned();
            delegate = delegate.with_subagent_model(subagent_model.clone());
            if self.announce {
                eprintln!("e-agent: subagent model {name}");
            }
        }
        let subagent_sessions = delegate.sessions();
        tools.push(Box::new(delegate));
        let mut agent = Agent::new(Box::new(main_model.clone()), tools);
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

        if let Some(window) = main_context_window {
            agent.set_context_window(window);
        }

        let model_name = main_model.display_name().to_owned();
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
            reloadable: Arc::new(RwLock::new(ReloadableState {
                config,
                auth: None,
                models: None,
            })),
            backend: crate::config::SessionBackend::Jsonl,
            main_model,
            main_context_window: None,
            subagent_model: None,
            subagent_context_window: None,
            role_models: HashMap::new(),
            role_context_windows: HashMap::new(),
            profile: None,
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

/// Resolve every profile the factory routes on, without building wire
/// models: the main profile (honoring the startup `--profile`), the
/// `subagent` role and every routed role. Shared by the constructor, reload
/// validation and `build_runtime_models`.
fn resolved_profiles(config: &Config, profile: Option<&str>) -> anyhow::Result<ProfilesResolved> {
    let main = config.resolve(profile)?;
    let subagent = config
        .resolve_role("subagent")
        .context("cannot resolve [roles] subagent profile")?;
    let roles = config
        .resolve_roles()
        .context("cannot resolve [roles] profiles")?;
    Ok(ProfilesResolved {
        main,
        subagent,
        roles,
    })
}

/// Turn resolved profiles into wire models, mirroring the constructor's
/// startup resolution exactly: `--base-url` cannot combine with a
/// chatgpt-routed main profile, and a `--model` override invalidates the
/// profile's context window. Shared by the constructor, reload validation
/// and the reloaded state (so `build()` and the runtime resolution helpers
/// always see the same resolution rules).
fn build_runtime_models(
    resolved: ProfilesResolved,
    auth: Option<&CodexAuth>,
    base_url: &Option<String>,
    model: &Option<String>,
) -> anyhow::Result<RuntimeModels> {
    if matches!(resolved.main.auth, AuthMode::ChatGpt) && base_url.is_some() {
        return Err(anyhow!(
            "--base-url cannot be used with a provider using auth = `chatgpt`"
        ));
    }
    // A `--model` override replaces the wire model, so the profile's
    // context window is no longer valid for the unknown model.
    let main_context_window = if model.is_some() {
        None
    } else {
        resolved.main.context_window
    };
    let main = configured_model(resolved.main, auth, base_url.clone(), model.clone())?;
    let subagent_context_window = resolved.subagent.as_ref().and_then(|r| r.context_window);
    let subagent = resolved
        .subagent
        .map(|resolved| configured_model(resolved, auth, None, None))
        .transpose()?;
    let mut roles = HashMap::new();
    let mut role_context_windows = HashMap::new();
    for (role, resolved) in resolved.roles {
        role_context_windows.insert(role.clone(), resolved.context_window);
        roles.insert(role, configured_model(resolved, auth, None, None)?);
    }
    Ok(RuntimeModels {
        main,
        main_context_window,
        subagent,
        subagent_context_window,
        roles,
        role_context_windows,
    })
}

/// Validate a freshly loaded config and, when it is good, atomically swap it
/// (plus its re-resolved models and needed ChatGPT auth) into the reloadable
/// state. A config that fails to parse or resolve — a bad edit, a missing
/// key file, a chatgpt-routed profile with no login — is rejected and the
/// previous config stays, so a running server never breaks on a config edit.
fn apply_reloaded_config(
    state: &mut ReloadableState,
    loaded: Option<Config>,
    profile: Option<&str>,
    base_url: &Option<String>,
    model: &Option<String>,
) -> ReloadResult {
    let Some(config) = loaded else {
        // The global config was deleted (or never existed): fall back to the
        // no-config behavior exactly like startup with no config file.
        state.config = None;
        state.auth = None;
        state.models = None;
        return ReloadResult::Reloaded;
    };
    // Mirror the startup web-search gate: when EXA_API_KEY is not set, a
    // malformed `[web_search]` section is a config error. The key value
    // itself is NOT re-injected at runtime — `std::env::set_var` is only
    // safe at startup (single-threaded), so web-search key changes still
    // need a restart.
    if std::env::var_os("EXA_API_KEY").is_none()
        && let Err(error) = config.web_search_key()
    {
        return ReloadResult::Rejected(format!("{error:#}"));
    }
    let resolved = match resolved_profiles(&config, profile) {
        Ok(resolved) => resolved,
        Err(error) => return ReloadResult::Rejected(format!("{error:#}")),
    };
    let needs_chatgpt = resolved.main.auth == AuthMode::ChatGpt
        || resolved
            .roles
            .values()
            .any(|value| value.auth == AuthMode::ChatGpt);
    let auth = if needs_chatgpt {
        match &state.auth {
            Some(auth) => Some(auth.clone()),
            None => match CodexAuth::load() {
                Ok(auth) => Some(auth),
                Err(error) => {
                    return ReloadResult::Rejected(format!(
                        "the new config routes a profile to auth = `chatgpt` but ChatGPT login is not initialized: {error:#}"
                    ));
                }
            },
        }
    } else {
        None
    };
    match build_runtime_models(resolved, auth.as_ref(), base_url, model) {
        Ok(models) => {
            state.config = Some(config);
            state.auth = auth;
            state.models = Some(models);
            ReloadResult::Reloaded
        }
        Err(error) => ReloadResult::Rejected(format!("{error:#}")),
    }
}

/// Load the effective config for `root` and apply it through
/// [`apply_reloaded_config`]. The shared core of [`SessionFactory::reload_config`]
/// and the watcher task (which holds the reloadable cell, not the factory).
fn reload_config_at(
    reloadable: &RwLock<ReloadableState>,
    root: &Path,
    profile: Option<&str>,
    base_url: &Option<String>,
    model: &Option<String>,
) -> ReloadResult {
    match Config::load_for_workspace(root) {
        Ok(loaded) => apply_reloaded_config(
            &mut reloadable.write().unwrap(),
            loaded,
            profile,
            base_url,
            model,
        ),
        Err(error) => ReloadResult::Rejected(format!("{error:#}")),
    }
}

/// The config file's mtime, or `None` when the file does not exist (yet).
/// A missing file and a `modified()` failure both read as `None`-stable, so
/// a stat error never turns into a reload every tick.
fn config_file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))
}

/// Whether any watched config file appeared, changed or disappeared since
/// the last check. Updates the seen mtimes in place, so a rejection (bad
/// edit) is not retried until the file changes again.
fn watch_paths_changed(mtimes: &mut HashMap<PathBuf, Option<SystemTime>>) -> bool {
    let mut changed = false;
    for (path, seen) in mtimes.iter_mut() {
        let now = config_file_mtime(path);
        if now != *seen {
            *seen = now;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod hot_reload_tests {
    use super::*;

    /// A minimal valid config: one ApiKey profile resolving via the `PATH`
    /// env var (always set), so validation needs no key files.
    fn test_config(default: &str, extra_models: &str) -> Config {
        let source = format!(
            r#"default = "{default}"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"

[models."p1/m1"]
model = "m1"
{extra_models}
"#
        );
        toml::from_str(&source).expect("test config parses")
    }

    #[test]
    fn reload_swaps_config_and_updates_runtime_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(test_config("p1/m1", "")),
        );
        assert_eq!(factory.model_profiles(), vec!["p1/m1"]);
        assert_eq!(factory.main_model().display_name(), "test-model"); // models not pre-resolved by the test factory

        let second = test_config(
            "p1/m1",
            r#"[models."p2/m2"]
model = "m2"
[providers.p2]
base_url = "http://two"
api_key_env = "PATH"
"#,
        );
        let result = {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, Some(second), None, &None, &None)
        };
        assert!(matches!(result, ReloadResult::Reloaded), "{result:?}");

        // The swapped config feeds every runtime resolution path.
        assert_eq!(factory.model_profiles(), vec!["p1/m1", "p2/m2"]);
        // `resolve_profile` proves the new profile is now resolvable; the
        // display name is the short form (after the last '/'), per
        // `ConfiguredModel::display_name`.
        assert_eq!(
            factory.resolve_profile("p2/m2").unwrap().display_name(),
            "m2"
        );
        assert_eq!(factory.main_model().display_name(), "m1");
        assert_eq!(
            factory.current_config().unwrap().model_profiles(),
            vec!["p1/m1", "p2/m2"]
        );
    }

    #[test]
    fn reload_rejects_bad_config_and_keeps_previous() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(test_config("p1/m1", "")),
        );
        // The default points at a profile that does not exist.
        let broken = test_config("nope/nope", "");
        let result = {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, Some(broken), None, &None, &None)
        };
        assert!(
            matches!(result, ReloadResult::Rejected(_)),
            "expected rejection, got {result:?}"
        );
        // Previous config stays untouched.
        assert_eq!(factory.model_profiles(), vec!["p1/m1"]);
        assert!(factory.resolve_profile("p1/m1").is_ok());
    }

    #[test]
    fn reload_without_config_resets_to_no_config_behavior() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(test_config("p1/m1", "")),
        );
        let result = {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, None, None, &None, &None)
        };
        assert!(matches!(result, ReloadResult::Reloaded));
        assert!(factory.current_config().is_none());
        assert!(factory.model_profiles().is_empty());
        // No config → resolve_profile errors instead of panicking.
        assert!(factory.resolve_profile("p1/m1").is_err());
    }

    #[test]
    fn watch_paths_changed_detects_create_modify_and_remove() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let mut mtimes = HashMap::from([(path.clone(), config_file_mtime(&path))]);
        assert!(!watch_paths_changed(&mut mtimes), "absent → absent");
        std::fs::write(&path, "default = \"p1/m1\"").unwrap();
        assert!(watch_paths_changed(&mut mtimes), "file appeared");
        assert!(!watch_paths_changed(&mut mtimes), "stable mtime");
        // Writes within the same mtime-granularity tick would be invisible
        // to pure mtime polling; pace the test writes like real edits.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "default = \"p2/m2\"").unwrap();
        assert!(watch_paths_changed(&mut mtimes), "file modified");
        assert!(!watch_paths_changed(&mut mtimes));
        std::fs::remove_file(&path).unwrap();
        assert!(watch_paths_changed(&mut mtimes), "file removed");
    }

    #[test]
    fn profile_override_is_preserved_through_reload() {
        let temp = tempfile::tempdir().unwrap();
        let mut factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(test_config("p1/m1", "")),
        );
        factory.profile = Some("p1/m1".to_owned());
        let second = test_config(
            "p1/m1",
            r#"[models."p2/m2"]
model = "m2"
[providers.p2]
base_url = "http://two"
api_key_env = "PATH"
"#,
        );
        let result = {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(
                &mut state,
                Some(second),
                factory.profile.as_deref(),
                &None,
                &None,
            )
        };
        assert!(matches!(result, ReloadResult::Reloaded), "{result:?}");
        assert_eq!(
            factory.resolve_profile("p2/m2").unwrap().display_name(),
            "m2"
        );
    }

    #[test]
    fn reload_config_reloads_from_edited_file_end_to_end() {
        // Env isolation: serialize with the roles/delegate tests that mutate
        // the shared XDG_CONFIG_HOME env var (same lock they use).
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let config_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_file = config_dir.join("config.toml");
        let write_config = |contents: &str| {
            std::fs::write(&config_file, contents).unwrap();
            // Pace writes like real edits: two writes within the same
            // mtime-granularity tick would be invisible to mtime polling.
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        write_config(
            r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"

[models."p1/m1"]
model = "m1"
"#,
        );
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

        // A factory whose reloadable cell starts empty: reload_config() must
        // pick the file up from disk, exactly like the watcher does.
        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        assert!(factory.current_config().is_none());
        assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        assert_eq!(factory.model_profiles(), vec!["p1/m1"]);
        assert_eq!(factory.main_model().display_name(), "m1");

        // Edit the file: the reload picks the new profile up.
        write_config(
            r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"

[providers.p2]
base_url = "http://two"
api_key_env = "PATH"

[models."p1/m1"]
model = "m1"

[models."p2/m2"]
model = "m2"
"#,
        );
        assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        assert_eq!(factory.model_profiles(), vec!["p1/m1", "p2/m2"]);
        assert!(factory.resolve_profile("p2/m2").is_ok());

        // A broken edit is rejected and the previous config stays.
        write_config("default = \"nope/nope\"\n");
        assert!(matches!(factory.reload_config(), ReloadResult::Rejected(_)));
        assert_eq!(factory.model_profiles(), vec!["p1/m1", "p2/m2"]);

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
