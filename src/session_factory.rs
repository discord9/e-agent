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

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use anyhow::{Context, anyhow};

use crate::agent::{Agent, GoalSnapshot, SessionEntry};
use crate::codex::CodexModel;
use crate::codex_auth::CodexAuth;
use crate::config::{
    AuthMode, Config, ProjectSourceObservation, ResolvedModel, Sandbox, SandboxPolicy,
    WebFetchProvider, WebSearchProvider,
};
use crate::delegate::{Delegate, Sessions};
use crate::mcp;
use crate::model::{ConfiguredModel, OpenAiModel};
use crate::runner::{IdlePolicy, SessionBootstrap, SessionHandle, SessionRunner};
use crate::session_store::SessionStore;
use crate::tools::{
    AuthResolver, BackgroundTasks, DEEPSEEK_API_KEY_ENV, SEARXNG_BASE_URL_ENV,
    WEB_FETCH_PROVIDER_ENV, WEB_SEARCH_PROVIDER_ENV, add_root_user_input_tool,
    builtins_with_bash_timeout, generate_image_tool,
};
use crate::workspace::Workspace;

/// Everything before a main session's history, participating in prefix-cache matching.
#[derive(Clone)]
pub(crate) struct RequestHead {
    pub model: ConfiguredModel,
    pub context_window: Option<u64>,
    pub context_prefix: Option<String>,
    pub compaction_reminder: Option<String>,
    /// The factory's STARTUP `[sandbox]` policy: the fork's `run_rust`
    /// placeholder (`code_mode` gate + forced policy), never the live shell
    /// policy.
    pub rust_sandbox: Option<crate::config::Sandbox>,
    pub read_only: bool,
    pub protect_git: bool,
    pub code_mode: bool,
    pub mcp_servers: HashMap<String, crate::mcp::McpServerConfig>,
    /// The parent's goal projection at build time (folded from the restored
    /// history). A fork applies it so its request carries the identical
    /// `[session goal]` system message; `goal_cleared` distinguishes an
    /// explicit clear tombstone from "never set".
    pub goal: Option<GoalSnapshot>,
    pub goal_cleared: bool,
}

/// What to do with background-task records left behind by a previous run
/// when building a session.
///
/// The caller decides whether to consume the session's own unfinished
/// background-task records:
///
/// - [`UnfinishedPolicy::Consume`]: take the running-task records and inject
///   a "tasks were killed" notice into the resumed history.
/// - [`UnfinishedPolicy::Preserve`]: leave the records untouched and do not
///   inject a notice; the owning process may acknowledge them itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnfinishedPolicy {
    /// Consume this session's records and announce that its tasks were killed.
    Consume,
    /// Leave this session's records untouched and inject no notice.
    Preserve,
}

/// Process-local admission key: the same session id in different workspaces or
/// backend instances is a different session.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct LocalSessionKey {
    pub workspace: String,
    pub backend: String,
    pub session: String,
}

/// Shared process-local admission registry. Persisted rows are deliberately
/// not consulted; a permit only represents a live local runner.
#[derive(Clone, Default)]
pub struct LocalSessionRegistry {
    live: Arc<std::sync::Mutex<HashSet<LocalSessionKey>>>,
}

pub struct LocalSessionPermit {
    registry: LocalSessionRegistry,
    key: Option<LocalSessionKey>,
}

impl LocalSessionRegistry {
    #[allow(clippy::result_unit_err)]
    pub fn reserve(&self, key: LocalSessionKey) -> Result<LocalSessionPermit, ()> {
        let mut live = self.live.lock().unwrap();
        if !live.insert(key.clone()) {
            return Err(());
        }
        Ok(LocalSessionPermit {
            registry: self.clone(),
            key: Some(key),
        })
    }

    pub fn key(
        workspace_root: &Path,
        storage_root: &Path,
        backend: &crate::config::SessionBackend,
        session: &str,
    ) -> LocalSessionKey {
        let workspace = crate::session_store::derive_workspace_id(workspace_root);
        let (kind, instance) = match backend {
            crate::config::SessionBackend::Jsonl => (
                "jsonl",
                crate::session_store::derive_workspace_id(storage_root),
            ),
            crate::config::SessionBackend::Sqlite { path } => (
                "sqlite",
                crate::session_store::normalize_db_path(
                    &crate::session_store::resolve_sqlite_path(storage_root, path.as_deref()),
                ),
            ),
            crate::config::SessionBackend::Greptime { conn } => ("greptime", conn.clone()),
        };
        LocalSessionKey {
            workspace,
            backend: crate::session_store::backend_instance_fingerprint(kind, &instance),
            session: session.to_owned(),
        }
    }
}

impl Drop for LocalSessionPermit {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.registry.live.lock().unwrap().remove(&key);
        }
    }
}

/// Test-only interleaving point inside ONE build: a hook armed for one
/// session id runs after that build's persisted-provenance read and before
/// its optional metadata backfill, so a test can place a concurrent
/// writer's damage in exactly the window the locked operation must detect.
/// Scoped to the session id so builds in parallel tests never fire it.
#[cfg(test)]
pub(crate) mod build_probe {
    use std::sync::Mutex;

    type Hook = Box<dyn FnOnce() + Send>;
    static AFTER_PROVENANCE: Mutex<Option<(String, Hook)>> = Mutex::new(None);

    pub(crate) fn arm_after_provenance(session: &str, hook: impl FnOnce() + Send + 'static) {
        *AFTER_PROVENANCE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((session.to_owned(), Box::new(hook)));
    }

    pub(crate) fn after_provenance_read(session: &str) {
        let mut slot = AFTER_PROVENANCE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.as_ref().is_some_and(|(armed, _)| armed == session) {
            let (_, hook) = slot.take().expect("checked above");
            drop(slot);
            hook();
        }
    }
}

/// Everything resolved once at startup and shared by every built session.
#[derive(Clone)]
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
    local_sessions: LocalSessionRegistry,
    /// `[sandbox]` cell shared with every session's bash / background tools
    /// (hot-reloaded on a `[sandbox]` edit).
    sandbox: SandboxPolicy,
    /// The STARTUP `[sandbox]` policy (`None` = disabled): `run_rust` and its
    /// fork placeholder keep using it, so a reload never changes those mounts.
    rust_sandbox: Option<Sandbox>,
    /// User-level `[code_mode]` flag: registers the experimental run_rust tool.
    code_mode: bool,
    read_only: bool,
    agents_instructions: Option<String>,
    /// Startup snapshot of the skill disclosure index; never hot-reloaded.
    skills_instructions: Option<String>,
    /// The workspace plus one extra read-only capability rooted at the
    /// global skills directory, for the MAIN session's file tools only
    /// (`None` when there is no usable global skills root). The plain
    /// [`SessionFactory::workspace`] keeps no such capability, so delegated
    /// subagents and btw forks never see global skills.
    skills_workspace: Option<Workspace>,
    /// The canonical global skills directory (`None` when there is no usable
    /// global skills root). [`register_generate_image`] refuses to register
    /// when the workspace root is at or below it: there an ordinary output
    /// path resolves as the writable workspace entry, not as the read-only
    /// skills entry (which only wins on a strict child).
    skills_root: Option<PathBuf>,
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
    /// it. Kept here (not re-read per request) so chatgpt profiles keep
    /// working exactly like they do at startup. A runtime resolution that
    /// needs chatgpt auth while this is `None` (the process started before
    /// `e-agent login` wrote the auth file) loads it from disk once and
    /// caches it here — see [`lazy_chatgpt_auth`].
    auth: Option<CodexAuth>,
    /// Models re-resolved from `config` (main / subagent role / all routed
    /// roles). Pre-resolved at swap time so `build()` stays infallible on
    /// config resolution and a bad config edit is rejected once, at reload,
    /// instead of failing every new session build. `None` when there is no
    /// config (builds then fall back to the startup-resolved fields).
    models: Option<RuntimeModels>,
    /// Sanitized source snapshots are independent of effective config presence.
    source_global: Option<toml::Value>,
    source_project: Option<toml::Value>,
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
    /// The new config parsed, validated and was swapped in, with source edits.
    Reloaded,
    /// The validated state was refreshed, but sanitized source leaves did not change.
    ReloadedNoSource,
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
    pub local_permit: LocalSessionPermit,
}

/// Shared body of [`SessionFactory::resolve_profile`] and the live fork-model
/// resolver: resolve one profile against the CURRENT config, honoring the
/// startup `--base-url`/`--model` overrides. Split out of the method so a
/// `'static` closure can resolve a profile without capturing the factory.
fn resolve_profile_from(
    reloadable: &Arc<RwLock<ReloadableState>>,
    base_url: Option<String>,
    model_override: Option<String>,
    profile: &str,
    effort: Option<EffortOverride>,
) -> anyhow::Result<(ConfiguredModel, Option<u64>)> {
    // Clone the config out of the state lock before building the model:
    // the chatgpt branch may take the write half of the same lock to
    // load (and cache) the login that arrived after start.
    let config = reloadable
        .read()
        .unwrap()
        .config
        .clone()
        .ok_or_else(|| anyhow!("no config file; cannot resolve model profile `{profile}`"))?;
    let mut resolved = config.resolve_profile(profile)?;
    if let Some(effort) = effort {
        resolved.reasoning_effort = match effort {
            EffortOverride::Default => resolved.reasoning_effort,
            EffortOverride::Omit => None,
            EffortOverride::Literal(value) => Some(value),
        };
    }
    // A `--model` override replaces the profile's wire model, so its
    // context-window declaration cannot describe the active model.
    let context_window = model_override
        .as_ref()
        .map(|_| None)
        .unwrap_or(resolved.context_window);
    let model = configured_model(
        resolved,
        AuthSource::Lazy(reloadable),
        base_url,
        model_override,
    )?;
    Ok((model, context_window))
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
        // Startup snapshot of the local skill index: global
        // `<config_dir>/skills/*/SKILL.md` plus workspace
        // `.e-agent/skills/*/SKILL.md` (workspace directory name fully
        // replaces a same-name global). The global root must be a real
        // directory, not a symlink, or global skills are skipped entirely.
        let global_skills_dir = crate::config::config_dir()
            .map(|dir| dir.join("skills"))
            .filter(|dir| skills_root_is_dir(dir));
        let skills_instructions = read_skills_index(&root, global_skills_dir.as_deref())?;
        let (config, project_source) = Config::load_for_workspace_with_sources(&root)?;
        let backend = config
            .as_ref()
            .map(|c| c.session_backend())
            .unwrap_or_default();
        // Web search/fetch read their backends from the process env (tools.rs
        // and subagents pick it up there): EXA_API_KEY for the default Exa
        // provider, the internal `E_AGENT_SEARXNG_BASE_URL` for
        // `[web_search] provider = "searxng"`, and the internal
        // `E_AGENT_DEEPSEEK_API_KEY` for `provider = "deepseek"` or
        // `fetch_provider = "deepseek"`. The provider choices travel beside
        // the credentials in `E_AGENT_WEB_SEARCH` / `E_AGENT_WEB_FETCH`, so a
        // SearXNG search can carry a DeepSeek key for the fetch without the
        // key hijacking the search backend. The section is validated once
        // here, which keeps the single transport mechanism and avoids
        // threading the setting through every tools constructor. Startup is
        // single-threaded, so set_var is safe.
        if let Some(config) = &config {
            let search = config.web_search_provider()?;
            let deepseek_fetch = config.web_fetch_provider()? == Some(WebFetchProvider::Deepseek);
            if let Some(search) = &search {
                let selector = match search {
                    WebSearchProvider::Exa => "exa",
                    WebSearchProvider::Searxng { .. } => "searxng",
                    WebSearchProvider::Deepseek => "deepseek",
                };
                if std::env::var_os(WEB_SEARCH_PROVIDER_ENV).is_none() {
                    unsafe { std::env::set_var(WEB_SEARCH_PROVIDER_ENV, selector) };
                }
            }
            if deepseek_fetch && std::env::var_os(WEB_FETCH_PROVIDER_ENV).is_none() {
                unsafe { std::env::set_var(WEB_FETCH_PROVIDER_ENV, "deepseek") };
            }
            let search_is_deepseek = matches!(search, Some(WebSearchProvider::Deepseek));
            match search {
                Some(WebSearchProvider::Searxng { base_url }) => {
                    if std::env::var_os(SEARXNG_BASE_URL_ENV).is_none() {
                        unsafe { std::env::set_var(SEARXNG_BASE_URL_ENV, base_url) };
                    }
                }
                // Process env still wins over a configured key, and an
                // absent `[web_search]` section resolves to no key at all.
                Some(WebSearchProvider::Exa) | None => {
                    if std::env::var_os("EXA_API_KEY").is_none()
                        && let Some(key) = config.web_search_key()?
                    {
                        unsafe { std::env::set_var("EXA_API_KEY", key) };
                    }
                }
                Some(WebSearchProvider::Deepseek) => {
                    if std::env::var_os(DEEPSEEK_API_KEY_ENV).is_none()
                        && let Some(key) = config.web_search_key()?
                    {
                        unsafe { std::env::set_var(DEEPSEEK_API_KEY_ENV, key) };
                    }
                }
            }
            // A SearXNG (or Exa) search with a DeepSeek fetch still needs the
            // credential, because it now belongs to the fetch.
            if deepseek_fetch
                && !search_is_deepseek
                && std::env::var_os(DEEPSEEK_API_KEY_ENV).is_none()
                && let Some(key) = config.web_search_key()?
            {
                unsafe { std::env::set_var(DEEPSEEK_API_KEY_ENV, key) };
            }
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
                let models = build_runtime_models(
                    resolved,
                    AuthSource::Fixed(auth.as_ref()),
                    &base_url,
                    &model,
                )?;
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
        // Resolve one shared canonical policy. `enabled` controls both the
        // bwrap sandbox and the workspace's logical file-tool entry: with
        // the sandbox enabled, `workspace_writable = false` denies
        // file-tool writes inside the workspace too; with it disabled the
        // file tools keep the historical always-writable workspace.
        // External roots stay active independently either way. The project
        // `[sandbox]` overlay comes from the observation this load produced,
        // never from a second read of the project file.
        let project_overlay = startup_project_overlay(&root, &project_source)?;
        let resolved_policy = crate::config::resolve_sandbox_with_project(
            config.as_ref(),
            &root,
            project_overlay.as_ref(),
        )?;
        workspace = workspace
            .with_external_roots(&resolved_policy)
            .map_err(anyhow::Error::msg)?;
        // Main-session file tools only: the plain workspace plus a read-only
        // capability for the global skills directory (see
        // [`main_tools_workspace`]). The stored plain `workspace` (used by
        // the delegate and btw forks) never gains this root, and bash keeps
        // the original policy.
        let skills_workspace = main_tools_workspace(
            workspace.clone(),
            &resolved_policy,
            global_skills_dir.as_deref(),
        )?;
        // Canonical global-skills path for `generate_image`'s registration
        // guard. Fails closed like the capability: a workspace construction
        // that cannot canonicalize the skills root must not silently keep a
        // tool that could publish into it.
        let skills_root = match global_skills_dir.as_deref() {
            Some(dir) => Some(crate::canonicalize_path(dir).with_context(|| {
                format!(
                    "cannot canonicalize the global skills directory {}",
                    dir.display()
                )
            })?),
            None => None,
        };
        let startup_sandbox = resolved_policy.enabled.then_some(resolved_policy);
        if let Some(policy) = &startup_sandbox {
            preflight_sandbox(policy, announce)?;
        }
        let sandbox = SandboxPolicy::fixed(startup_sandbox.clone());
        // run_rust is opt-in, user-level, and preflighted fail-closed.
        #[rustfmt::skip]
        let code_mode = config.as_ref().map(Config::code_mode_enabled).unwrap_or(false);
        if code_mode {
            preflight_code_mode(announce)?;
        }
        let source_global = config
            .as_ref()
            .and_then(|c| c.diagnostic_sources().0.cloned());
        let source_project = match project_source {
            ProjectSourceObservation::Observed(source) => Some(source),
            ProjectSourceObservation::Absent | ProjectSourceObservation::Unavailable => None,
        };
        Ok(Self {
            workspace,
            root,
            reloadable: Arc::new(RwLock::new(ReloadableState {
                config,
                auth,
                models: runtime,
                source_global,
                source_project,
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
            local_sessions: LocalSessionRegistry::default(),
            sandbox,
            rust_sandbox: startup_sandbox,
            code_mode,
            read_only,
            agents_instructions,
            skills_instructions,
            skills_workspace,
            skills_root,
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

    pub fn local_sessions(&self) -> LocalSessionRegistry {
        self.local_sessions.clone()
    }

    pub fn local_session_key(&self, session: &str) -> LocalSessionKey {
        LocalSessionRegistry::key(&self.root, &self.root, &self.backend, session)
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
    /// that into a 400. A chatgpt-routed profile loads the login credentials
    /// lazily on first use ([`AuthSource::Lazy`]), so a login that happened
    /// after start works without a restart. The context window accompanies
    /// the concrete model so a runtime switch updates both runner
    /// observations together.
    pub fn resolve_profile(
        &self,
        profile: &str,
        effort: Option<EffortOverride>,
    ) -> anyhow::Result<(ConfiguredModel, Option<u64>)> {
        resolve_profile_from(
            &self.reloadable,
            self.base_url.clone(),
            self.model.clone(),
            profile,
            effort,
        )
    }

    /// Resolve a model profile by name at FORK time — the `fork` delegate's
    /// child runs on the parent's current model, so a parent `/model <profile>`
    /// switch (which only updates the live agent) must not leave the child on
    /// the build-time model. `None` when the profile cannot be resolved (no
    /// config, unknown profile); the fork then fails closed instead of
    /// silently using the stale model. The effort level is the profile's own
    /// configured default: the live identity carries the profile name only.
    pub fn fork_model_resolver(&self) -> crate::delegate::ForkModelSource {
        let reloadable = self.reloadable.clone();
        let base_url = self.base_url.clone();
        let model = self.model.clone();
        Arc::new(move |profile: &str| {
            resolve_profile_from(&reloadable, base_url.clone(), model.clone(), profile, None).ok()
        })
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

    /// The cached ChatGPT credentials, loading them from disk on first use
    /// (a login after start is picked up without a restart). Read-only for
    /// the account/quota overview: no network on the factory side.
    pub(crate) fn chatgpt_auth(&self) -> anyhow::Result<CodexAuth> {
        lazy_chatgpt_auth(&self.reloadable)
    }

    /// Resolve a role's model live from the reloadable state, for a Delegate
    /// that outlives individual session builds (the resident delegate tool).
    /// Returns `(model, context window)` for the role, or `None` when the
    /// role is not routed — the caller then falls back to its subagent
    /// model. `'static`: the closure owns an `Arc` clone of the reloadable
    /// cell plus the startup snapshot, so it survives the factory and sees
    /// config hot reloads — a `[roles]` edit is picked up by newly spawned
    /// subagents without a restart. When there is no config at all
    /// (`models` is `None`), the startup snapshot (this factory's
    /// `role_models` / `role_context_windows`) is used, exactly like
    /// [`SessionFactory::build`]'s fallback.
    pub fn role_models_resolver(&self) -> crate::delegate::RoleModelSource {
        let reloadable = self.reloadable.clone();
        let snapshot_roles = self.role_models.clone();
        let snapshot_windows = self.role_context_windows.clone();
        Arc::new(move |role: &str| {
            let state = reloadable.read().unwrap();
            match &state.models {
                Some(models) => models.roles.get(role).map(|model| {
                    (
                        model.clone(),
                        models.role_context_windows.get(role).copied().flatten(),
                    )
                }),
                None => snapshot_roles
                    .get(role)
                    .map(|model| (model.clone(), snapshot_windows.get(role).copied().flatten())),
            }
        })
    }

    /// Resolve the default subagent model live from the reloadable state —
    /// the fallback for delegated tasks with no routed role (no `role`
    /// argument, or a role the live `[roles]` config does not route).
    /// Returns `(model, context window)`: the routed `[roles] subagent`
    /// model when one exists, else the live main model — the same fallback
    /// [`SessionFactory::build`] uses, so a resident Delegate and a newly
    /// built session never disagree about what "no subagent route" means.
    /// `None` only when the live state has no config at all; the caller
    /// then falls back to its startup snapshot as a last resort. `'static`:
    /// the closure owns an `Arc` clone of the reloadable cell plus the
    /// startup snapshot, so it survives the factory and sees config hot
    /// reloads — exactly like [`SessionFactory::role_models_resolver`],
    /// keeping the routed-role path and the fallback path equally hot.
    pub fn subagent_model_resolver(
        &self,
    ) -> Arc<dyn Fn() -> Option<(ConfiguredModel, Option<u64>)> + Send + Sync> {
        let reloadable = self.reloadable.clone();
        let snapshot_model = self.subagent_model.clone();
        let snapshot_window = self.subagent_context_window;
        Arc::new(move || {
            let state = reloadable.read().unwrap();
            match &state.models {
                // Model and context window always come from the same
                // profile: main↔main window, subagent↔subagent window.
                Some(models) => Some(
                    models
                        .subagent
                        .clone()
                        .map(|model| (model, models.subagent_context_window))
                        .unwrap_or_else(|| (models.main.clone(), models.main_context_window)),
                ),
                None => snapshot_model
                    .as_ref()
                    .map(|model| (model.clone(), snapshot_window)),
            }
        })
    }

    /// Re-read the config files for this workspace and, when they changed
    /// and the new config parses + resolves, atomically swap it in. Returns
    /// the outcome for logging. Never fails the process: a bad edit keeps
    /// the previous config and is reported via [`ReloadResult::Rejected`].
    pub fn reload_config(&self) -> ReloadResult {
        reload_config_at(
            &self.reloadable,
            &self.root,
            &self.sandbox,
            self.profile.as_deref(),
            &self.base_url,
            &self.model,
            self.announce,
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
        let sandbox = self.sandbox.clone();
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
                match reload_config_at(
                    &reloadable,
                    &root,
                    &sandbox,
                    profile.as_deref(),
                    &base_url,
                    &model,
                    announce,
                ) {
                    ReloadResult::Reloaded => {
                        if announce {
                            tracing::info!(
                                "e-agent: config reloaded: new sessions, `/model` switches and the next bash/background dispatch use the updated config; \
                                 the [session] backend and web-search key changes still need a restart"
                            );
                        }
                    }
                    ReloadResult::ReloadedNoSource => {}
                    ReloadResult::Rejected(reason) => {
                        if announce {
                            tracing::warn!(
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

    /// The `[sandbox]` cell itself, for tools and for `/btw` forks: sharing
    /// the cell (never a snapshot) is what keeps a fork live.
    pub fn sandbox_policy(&self) -> SandboxPolicy {
        self.sandbox.clone()
    }

    /// The read-only policy every session builds with; btw fork subagents
    /// inherit it (a read-only parent forks read-only subagents).
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The shared lazy ChatGPT-auth resolver `generate_image` uses: the same
    /// reloadable cache chatgpt-routed profiles resolve through, so a login
    /// that arrives after startup is picked up on first use without a
    /// restart, and every session shares one credential handle.
    fn generate_image_auth(&self) -> AuthResolver {
        let reloadable = self.reloadable.clone();
        Arc::new(move || lazy_chatgpt_auth(&reloadable))
    }

    /// Build one session end-to-end, in the exact order `main.rs::run()`
    /// used: store connect → fork → tools → MCP → delegate → agent →
    /// context prefix → history restore → unfinished-background handling →
    /// context window → runner.
    ///
    /// `id` is the requested session id; `fork_from` is `(source session,
    /// optional 1-based entry index)` and replaces `id` with a fresh
    /// `fork-…` id. `unfinished` selects how leftover background-task
    /// records are handled: `Consume` takes this session's own records and
    /// injects a "killed with the process" notice; `Preserve` leaves them
    /// untouched. The caller still chooses when to
    /// `runner.start(...)` with its initial prompt.
    pub async fn build(
        &self,
        id: &str,
        fork_from: Option<(String, Option<usize>)>,
        max_rounds: Option<usize>,
        policy: IdlePolicy,
        unfinished: UnfinishedPolicy,
    ) -> anyhow::Result<SessionBuild> {
        self.build_internal(id, fork_from, max_rounds, policy, unfinished, None)
            .await
    }

    /// Build a server fork using the handler-reserved final session id.
    /// Unlike the legacy CLI/TUI fork path, this never generates or replaces
    /// the target id internally.
    pub async fn build_fork(
        &self,
        target_id: &str,
        source: String,
        at: Option<usize>,
        policy: IdlePolicy,
        unfinished: UnfinishedPolicy,
    ) -> anyhow::Result<SessionBuild> {
        self.build_internal(
            target_id,
            Some((source, at)),
            None,
            policy,
            unfinished,
            Some(target_id),
        )
        .await
    }

    async fn build_internal(
        &self,
        id: &str,
        fork_from: Option<(String, Option<usize>)>,
        max_rounds: Option<usize>,
        policy: IdlePolicy,
        unfinished: UnfinishedPolicy,
        fork_target: Option<&str>,
    ) -> anyhow::Result<SessionBuild> {
        let mut session = id.to_owned();
        let mut local_permit = if fork_target.is_some() || fork_from.is_none() {
            Some(
                self.local_sessions
                    .reserve(self.local_session_key(&session))
                    .map_err(|_| anyhow!("session {session} is already live locally"))?,
            )
        } else {
            None
        };
        let mut store = SessionStore::connect(&self.backend, &self.root, &session).await?;
        // Whether this build CONTINUES another session's transcript (CLI/TUI
        // `--fork`, web fork) — captured before `fork_from` is consumed.
        let mut forked = fork_from.is_some();
        if let Some((source, at)) = fork_from {
            // Zero-copy fork: the child's store receives ONLY its own frozen
            // goal plus the marker. The source prefix is read back read-only by
            // the marker-aware loader at startup/resume, so the parent rows are
            // never copied and later parent writes stay invisible. The source
            // is read here exactly once, to validate the cut and to pin the
            // physical row it lands on.
            let source_store = SessionStore::connect(&self.backend, &self.root, &source).await?;
            // The DEFAULT cut is selected from the source's physical located
            // HEAD (last compaction + after) and its owner/key is carried
            // forward: no resolved ordinal, and no second full load. An
            // explicit `--at`/server `at` stays a 1-based index into the
            // RESOLVED transcript, which may page/load older entries.
            let (owner, key) = match at {
                None => {
                    let cut = source_store
                        .default_fork_cut(&self.root, &source)
                        .await
                        .with_context(|| format!("cannot fork session {source}"))?;
                    (cut.owner, cut.key)
                }
                Some(at) => {
                    let source_history = source_store
                        .load_fork_located(&self.root, &source)
                        .await
                        .with_context(|| format!("cannot load source session {source} for fork"))?;
                    let at = crate::agent::fork_prefix(&source_history.entries, Some(at))
                        .map_err(anyhow::Error::msg)
                        .with_context(|| format!("cannot fork session {source}"))?
                        .len();
                    // The cut row may belong to an ANCESTOR (the source can
                    // itself be a zero-copy child): pin the row that really
                    // stores it.
                    source_store
                        .fork_cut_origin(&self.root, &source, at)
                        .await
                        .with_context(|| format!("cannot fork session {source}"))?
                }
            };
            // `GoalUpdated` is not embedded in a `Compaction` and the child's
            // context starts at the last compaction at or before the cut, so
            // freeze the goal that applies AT the cut — resolved through the
            // owning history, never from the materialized prefix alone (a
            // fork of a fork keeps a goal that sits before the compaction).
            let mut own_entries = Vec::with_capacity(2);
            if let Some(goal) = source_store
                .fork_goal_at_cut(&self.root, &owner, &key)
                .await
                .with_context(|| format!("cannot fork session {source}"))?
            {
                own_entries.push(SessionEntry::GoalUpdated { goal });
            }
            own_entries.push(SessionEntry::ForkedFrom {
                source: owner,
                at: None,
                event_time: key.event_time_us(),
                seq: Some(key.seq()),
            });
            let new_id = fork_target
                .map(str::to_owned)
                .unwrap_or_else(|| crate::session::new_id_prefixed("fork-"));
            store = SessionStore::connect(&self.backend, &self.root, &new_id).await?;
            store
                .append(&self.root, &new_id, &own_entries)
                .await
                .with_context(|| format!("cannot write the fork marker of session {new_id}"))?;
            eprintln!("e-agent: forked session: {new_id}");
            session = new_id;
            if local_permit.is_none() {
                local_permit = Some(
                    self.local_sessions
                        .reserve(self.local_session_key(&session))
                        .map_err(|_| anyhow!("session {session} is already live locally"))?,
                );
            }
        }
        // Persisted provenance, not just this build's request: a fork or a
        // child reopened by id (web resume, `--session`) passes
        // `fork_from = None`, but its metadata row still records its parent
        // and/or its stored transcript still carries the `ForkedFrom` marker
        // (at whatever depth the layout put it). `link_ref` must stay off for
        // it exactly as it does for a fresh fork.
        //
        // Head-oriented marker-aware load: the model context this session
        // initializes from — its own head, or a zero-copy child's resolved
        // inherited context — never the whole transcript. Older rows stay
        // readable on demand through the history pager. On JSONL this load
        // necessarily decodes every entry before slicing at the last
        // compaction, so it ALSO answers the transcript-marker question: a
        // second provenance scan would be a second full decode of a long
        // transcript on every reopen. SQL keeps its bounded marker query.
        let (loaded, persisted_fork) = store
            .load_fork_head_and_provenance(&self.root, &session)
            .await
            .with_context(|| format!("cannot reopen session {session}"))?;
        if !forked {
            forked = persisted_fork;
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
        // Sessions metadata: write the creation snapshot for a brand-new
        // session (model/role from the factory configuration; parent links
        // are None for main sessions). A resumed session (`--session`,
        // server resume, fork-source reconnect) already has its row —
        // creation is idempotent and appends nothing — and the runner's
        // turn-boundary touch keeps it fresh.
        //
        // On an EXISTING row that call is only the OPTIONAL backfill of a
        // model/role the row was created without (a bootstrap-backfilled
        // session has `model: None`), and a damaged sidecar must not take
        // that append so unknown provenance is never rewritten into a clean
        // one. Refusing to open would make an interrupted write fatal to a
        // resume whose transcript is intact, so the optional entry point
        // skips the damaged write and REPORTS that it observed damage. The
        // tool set below is finalized only after that answer: a damage that
        // lands after the provenance read (a concurrent title edit) must
        // still keep `link_ref` off, because its links belong to the
        // metadata row the damage prevents us from trusting. There is
        // deliberately no `meta_is_damaged` pre-check here — it would race a
        // concurrent writer that damages the sidecar between the check and
        // this call; the optional entry point makes the decision in the same
        // locked read that appends. A fresh session has no sidecar, so it is
        // never damaged and its creation snapshot is still written.
        let model_name = main_model.profile_key();
        let main_role = crate::roles::role_prompt(&self.root, crate::roles::MAIN_ROLE)?;
        let role_name = main_role
            .as_ref()
            .map(|_| crate::roles::MAIN_ROLE.to_owned());
        #[cfg(test)]
        build_probe::after_provenance_read(&session);
        let metadata_damaged = store
            .create_meta_optional(
                &self.root,
                &session,
                Some(&model_name),
                role_name.as_deref(),
                None,
                None,
                None, // main sessions keep manual naming; title is a subagent-spawn concern
            )
            .await?;
        #[rustfmt::skip]
        let tools_workspace = self.skills_workspace.clone().unwrap_or_else(|| self.workspace.clone());
        let (mut tools, background) = builtins_with_bash_timeout(
            tools_workspace.clone(),
            self.sandbox.clone(),
            self.read_only,
            background_timeout,
            bash_timeout,
        );
        // run_rust is main-agent only; the read-only sandbox is narrowed like bash.
        #[cfg(target_os = "linux")]
        #[rustfmt::skip]
        register_run_rust(&mut tools, &tools_workspace, self.code_mode, self.read_only, self.rust_sandbox.clone());
        // Only the root/main session can pause for Web human input.
        add_root_user_input_tool(&mut tools);
        // `link_ref` records PR/issue URLs on THIS session's metadata row
        // through the store already open above (no second connection), so it
        // belongs only to a session the human started from scratch: a
        // user-created top-level session with no source transcript. A FORK is
        // a top-level main session too (the user talks to it), but it
        // continues another session's transcript — its links belong to the
        // source it continues, so a fork must never rewrite them. Children
        // (delegate/btw forks) build the common builtins and never see it
        // either. Metadata damage observed by the optional backfill above
        // keeps it off too: a torn row is not a trustworthy place to record
        // links, even when the provenance read happened before the damage.
        if !forked && !metadata_damaged {
            tools.push(crate::tools::link_ref_tool(
                store.clone(),
                self.root.clone(),
                session.clone(),
            ));
        }
        // Main-agent only: `generate_image` (direct Codex image HTTP).
        // Read-only sessions skip it (it creates files), and so does a
        // workspace rooted at or below the canonical global skills
        // directory (`skills_root`: there the workspace entry is writable
        // and would publish into global skills). Otherwise it gets the main
        // session's tools workspace — the plain workspace plus the
        // read-only global-skills capability — so a path under that
        // read-only entry is refused before any request. Children/btw
        // forks use the common builtins, so they never see it; a
        // same-workspace fork therefore lacks this one tool even when its
        // main-session parent had it (the documented fork-prefix
        // exception, covered by `image_generation::tests::
        // generate_image_is_not_in_common_builtins`).
        register_generate_image(
            &mut tools,
            &tools_workspace,
            self.generate_image_auth(),
            self.read_only,
            self.skills_root.as_deref(),
        );
        // Main-agent only: `acp_prompt` (one external ACP stdio agent per
        // call). Children/btw forks build the common builtins, so they never
        // see it — like `generate_image`, a same-workspace fork of a main
        // session that exposed it has a different tool array.
        // The store + root + session id are the ACP session-reuse wiring: the
        // tool reads/writes `sessions.acp_session_id` through the store.
        register_acp(
            &mut tools,
            &tools_workspace,
            config.as_ref(),
            self.read_only,
            &store,
            &self.root,
            &session,
        );
        // Read-only sessions skip MCP entirely: MCP tools carry no read-only
        // marker, so exposing them would defeat the policy. Delegation stays —
        // spawning a subagent does not mutate this session's host state, and
        // each subagent resolves its own role template (read-only or not).
        let mcp_servers_for_fork = config.as_ref().map(|c| c.mcp.clone()).unwrap_or_default();
        let (mut mcp_tools, mcp_instructions) = if self.read_only {
            (Vec::new(), Vec::new())
        } else {
            mcp::connect_all(mcp_servers_for_fork.clone(), &self.root).await
        };
        // Deterministic order is required for prefix-cache parity.
        mcp_tools.sort_by_key(|tool| tool.spec().name);
        tools.extend(mcp_tools);
        let mut delegate = Delegate::new(
            main_model.clone(),
            self.workspace.clone(),
            background.clone(),
        )
        .persist_sessions(self.root.clone())
        .with_role_models(role_models.clone())
        .with_role_model_source(self.role_models_resolver())
        .with_role_context_windows(role_context_windows.clone())
        .with_subagent_context_window(subagent_context_window)
        .with_roles_root(self.root.clone())
        .with_sandbox(self.sandbox.clone())
        .with_local_sessions(self.local_sessions.clone())
        .record_background_tasks_in(self.root.clone(), &session, store.clone())
        .with_persist_store(self.backend.clone());
        if let Some(subagent_model) = &subagent_model {
            let name = subagent_model.profile_key();
            delegate = delegate.with_subagent_model(subagent_model.clone());
            if self.announce {
                tracing::info!("e-agent: subagent model {name}");
            }
        }
        // Live default-subagent-model resolver: fallback delegations (no
        // routed role) read `[roles] subagent` from the reloadable config at
        // spawn time, exactly like the routed-role path above reads `[roles]
        // <role>` — both survive config hot reloads without a restart.
        delegate = delegate.with_subagent_model_source(self.subagent_model_resolver());
        let head_cell = Arc::new(std::sync::Mutex::new(None::<RequestHead>));
        let cell = head_cell.clone();
        delegate = delegate.with_fork_head(Arc::new(move || {
            cell.lock()
                .unwrap()
                .as_ref()
                .expect("main request head initialized")
                .clone()
        }));
        // A forked child runs on the parent's CURRENT model: after a runtime
        // `/model` switch the request head above is stale, so the fork asks
        // the runner for its live identity and re-resolves that profile here.
        delegate = delegate.with_fork_model_source(self.fork_model_resolver());
        let subagent_sessions = delegate.sessions();
        tools.push(Box::new(crate::delegate::SendMessage::parent(
            subagent_sessions.clone(),
        )));
        tools.push(Box::new(delegate));
        let mut agent = Agent::new(Box::new(main_model.clone()), tools);
        let mut context = Vec::new();
        // The main agent's orchestrator template resolves through the same role
        // layers as delegated roles; the highest-priority available template
        // leads and tells the model to decompose work and delegate.
        if let Some(orchestrator) = main_role {
            if let Some(reminder) = crate::roles::core_directive(&orchestrator) {
                agent.set_compaction_reminder(reminder);
            }
            context.push(orchestrator);
        }
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
        let legacy = loaded.legacy;
        let inherited = loaded.inherited;
        let older_route = loaded.next_older;
        agent.restore_located(loaded.entries, loaded.locations);
        agent.set_inherited_prefix(inherited);
        agent.set_older_route(older_route);
        // The head excludes the rows a compaction summarized, and a
        // `GoalUpdated` is never inside the summary: restore the newest goal in
        // the ancestry so a resumed goal (or its clear tombstone) survives.
        agent.restore_goal_projection(store.latest_goal(&self.root, &session).await?);
        // Capture the request head AFTER the history restore: its goal fields
        // must carry the session's live folded goal (not the pre-restore
        // "never set" state) so a fork's first request projects the exact
        // same `[session goal]` system message as its parent.
        let head_prefix = agent.context_prefix().map(str::to_owned);
        let head_reminder = agent.compaction_reminder_text().map(str::to_owned);
        let head_goal = agent.goal();
        let head_goal_cleared = agent.goal_cleared();
        let head_model = main_model.clone();
        let head_value = RequestHead {
            model: head_model,
            context_window: main_context_window,
            context_prefix: head_prefix,
            compaction_reminder: head_reminder,
            rust_sandbox: self.rust_sandbox.clone(),
            read_only: self.read_only,
            protect_git: false,
            code_mode: self.code_mode,
            mcp_servers: mcp_servers_for_fork.clone(),
            goal: head_goal,
            goal_cleared: head_goal_cleared,
        };
        *head_cell.lock().unwrap() = Some(head_value);
        let recovery_labels = if matches!(unfinished, UnfinishedPolicy::Consume) {
            store
                .peek_unfinished_background(&self.root, &session)
                .await?
        } else {
            Vec::new()
        };
        let bootstrap = SessionBootstrap {
            recovery_tasks: if matches!(unfinished, UnfinishedPolicy::Consume) {
                recovery_labels
            } else {
                Vec::new()
            },
            legacy,
            // Zero-copy forks write their marker above; nothing else is
            // bootstrapped, so a fork never performs O(N) durable writes.
            initial_entries: Vec::new(),
        };

        if let Some(window) = main_context_window {
            agent.set_context_window(window);
        }

        let (runner, handle) = SessionRunner::new_with_bootstrap(
            agent,
            store.clone(),
            self.root.clone(),
            session.clone(),
            policy,
            bootstrap,
        )
        .await?;
        // The root/main session is the only one with a user-facing
        // `WaitingInput` frontend (Web/TUI), so it is the only session
        // allowed to suspend on `request_user_input` for an answer.
        let runner = runner
            .with_background_tasks(background.clone())
            .with_user_input_allowed();
        Ok(SessionBuild {
            runner,
            handle,
            store,
            background,
            sessions: subagent_sessions,
            session,
            model_name,
            role_name,
            local_permit: local_permit.expect("session build must reserve its local id"),
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

    /// Test-only factory with an explicitly selected persistence backend.
    #[cfg(test)]
    pub(crate) fn test_factory_with_backend(
        root: PathBuf,
        backend: crate::config::SessionBackend,
    ) -> Self {
        let mut factory = Self::test_factory(root);
        factory.backend = backend;
        factory
    }

    /// Test-only factory carrying a config, so runtime `/model` endpoint
    /// tests can exercise real profile resolution (`resolve_profile`)
    /// without touching the user's global config.
    #[cfg(test)]
    pub(crate) fn test_factory_with_config(root: PathBuf, config: Option<Config>) -> Self {
        let source_global = config
            .as_ref()
            .and_then(|c| c.diagnostic_sources().0.cloned());
        let source_project = config
            .as_ref()
            .and_then(|c| c.diagnostic_sources().1.cloned());
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
                source_global,
                source_project,
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
            local_sessions: LocalSessionRegistry::default(),
            sandbox: SandboxPolicy::fixed(None),
            rust_sandbox: None,
            code_mode: false,
            read_only: false,
            agents_instructions: None,
            skills_instructions: None,
            skills_workspace: None,
            skills_root: None,
            announce: false,
        }
    }
}

/// Fail-closed `[code_mode]` startup check (Linux + bwrap + rustc on PATH).
#[rustfmt::skip]
pub(crate) fn preflight_code_mode(announce: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    { preflight_code_mode_impl(crate::tools::bwrap_available, std::env::var_os("PATH").as_deref(), announce) }
    #[cfg(not(target_os = "linux"))]
    { let _ = announce; Err(anyhow!("[code_mode] requires Linux with bubblewrap; run_rust unavailable")) }
}
#[cfg(target_os = "linux")]
#[rustfmt::skip]
pub(crate) fn preflight_code_mode_impl(bwrap_ok: impl Fn() -> bool, path_env: Option<&std::ffi::OsStr>, announce: bool) -> anyhow::Result<()> {
    if !bwrap_ok() { return Err(anyhow!("[code_mode] enabled = true but bwrap is unavailable")); }
    if crate::tools::resolve_rustc(path_env).is_none() { return Err(anyhow!("[code_mode] enabled = true but rustc is unavailable on PATH")); }
    if announce { tracing::info!("e-agent: run_rust tool enabled ([code_mode])"); }
    Ok(())
}
/// Register the experimental run_rust tool (main agent only, opt-in): the
/// read-only sandbox is narrowed exactly like bash's (no policy fails closed).
#[cfg(target_os = "linux")]
#[rustfmt::skip]
pub(crate) fn register_run_rust(tools: &mut Vec<Box<dyn crate::agent::Tool>>, workspace: &Workspace, code_mode: bool, read_only: bool, sandbox: Option<Sandbox>) {
    if code_mode && (!read_only || sandbox.is_some()) {
        let narrowed = if read_only { sandbox.as_ref().map(crate::tools::read_only_sandbox) } else { sandbox };
        tools.push(crate::tools::run_rust_tool(workspace, narrowed.as_ref()));
    }
}

/// Build the MAIN session's tools workspace: the plain workspace plus one
/// extra read-only capability at the global skills directory, so the model
/// can `read_file` the SKILL.md paths the index advertises.
///
/// The configured logical path AND its canonical form are both installed as
/// read-only entries. [`skills_root_is_dir`] only rules out a symlinked
/// `skills` directory itself, not a symlinked PARENT component: the index
/// advertises the logical path, while the file tools match entry
/// destinations lexically, so a parent-symlink alias leaves the real
/// directory (which can sit inside the workspace, where the workspace entry
/// is writable) unprotected. A canonicalization failure is a construction
/// error rather than a silently dropped protection.
///
/// [`crate::canonicalize_path`] (not raw `std::fs::canonicalize`) is used so
/// the Windows `\\?\` verbatim prefix is stripped and the destination
/// compares equal to `Workspace` roots and inputs, which are stripped the
/// same way.
fn main_tools_workspace(
    workspace: Workspace,
    policy: &Sandbox,
    global_skills_dir: Option<&Path>,
) -> anyhow::Result<Option<Workspace>> {
    let Some(dir) = global_skills_dir else {
        return Ok(None);
    };
    let canonical = crate::canonicalize_path(dir).with_context(|| {
        format!(
            "cannot canonicalize the global skills directory {}",
            dir.display()
        )
    })?;
    let mut with_skills = policy.clone();
    with_skills.readable_paths.push(dir.display().to_string());
    if canonical.as_path() != dir {
        with_skills
            .readable_paths
            .push(canonical.display().to_string());
    }
    Ok(Some(
        workspace
            .with_external_roots(&with_skills)
            .map_err(anyhow::Error::msg)?,
    ))
}

/// Register the main-agent-only `generate_image` tool. Read-only sessions
/// skip it entirely (it creates files in the workspace), and children/btw
/// forks build the common builtins without it. A client-construction
/// failure skips the tool (mirroring `bash_tool`) instead of failing the
/// whole session build.
///
/// `protected_skills` is the canonical global skills directory (if any). A
/// workspace rooted AT or BELOW it registers nothing: the read-only skills
/// entry only wins for a strict child, so an ordinary output path would
/// resolve as the writable workspace entry and publish into global skills.
pub(crate) fn register_generate_image(
    tools: &mut Vec<Box<dyn crate::agent::Tool>>,
    workspace: &Workspace,
    auth: AuthResolver,
    read_only: bool,
    protected_skills: Option<&Path>,
) {
    if protected_skills.is_some_and(|skills| workspace.root().starts_with(skills)) {
        return;
    }
    if !read_only && let Ok(tool) = generate_image_tool(workspace.clone(), auth) {
        tools.push(tool);
    }
}

/// Register the main-agent-only `acp_prompt` tool. The `[acp]` section is the
/// opt-in: without it nothing is registered (the same boundary as a
/// `[web_search]` without a usable credential). Read-only sessions skip it —
/// the external agent carries no read-only marker and would defeat the policy
/// — and children/btw forks build the common builtins without it.
///
/// This is the wiring point for ACP session reuse: the tool gets the session
/// store (`sessions.acp_session_id`) plus the store's workspace root and the
/// session id it belongs to, so it never opens a database itself.
pub(crate) fn register_acp(
    tools: &mut Vec<Box<dyn crate::agent::Tool>>,
    workspace: &Workspace,
    config: Option<&crate::config::Config>,
    read_only: bool,
    store: &crate::session_store::SessionStore,
    root: &std::path::Path,
    session: &str,
) {
    if read_only {
        return;
    }
    let Some(acp) = config.and_then(crate::config::Config::acp) else {
        return;
    };
    // The session id is bound up front: the ACP id belongs to THIS session's
    // metadata row from the first call.
    tools.push(crate::tools::acp_tool(
        workspace.clone(),
        acp,
        store.clone(),
        root.to_path_buf(),
        session,
    ));
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
            tracing::info!("e-agent: shell write-restricted with a Windows restricted token");
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
            tracing::info!("e-agent: bash sandboxed with bwrap");
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
                gpu: false,
                writable_paths: Vec::new(),
                readable_paths: Vec::new(),
                readable_mounts: Vec::new(),
                writable_mounts: Vec::new(),
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
                gpu: false,
                writable_paths: Vec::new(),
                readable_paths: Vec::new(),
                readable_mounts: Vec::new(),
                writable_mounts: Vec::new(),
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

/// Fixed fallback description when a skill's frontmatter has none.
pub const SKILL_DEFAULT_DESCRIPTION: &str = "Local e-agent skill";

/// One skill's disclosure entry: frontmatter `name` (fallback: the skill
/// directory's name), frontmatter `description` (fallback: the fixed short
/// phrase), the absolute `SKILL.md` path, and the directory name used as
/// the same-name override key.
pub struct SkillIndexEntry {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub(crate) dir_name: String,
}

/// The global skills root must be a real directory, never a symlink: the
/// read-only capability handed to the main `read_file` must not reach a
/// config sibling through a link. Missing paths count as "not a root".
pub fn skills_root_is_dir(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

/// Parse the leading `---`-fenced frontmatter for single-line `name` and
/// `description`. Missing or unterminated frontmatter and missing/empty
/// fields fall back: name → the skill directory's name, description → the
/// fixed short phrase. Skills are trusted local config — no sanitizer or
/// caps apply.
fn skill_meta(content: &str, dir_name: &str) -> (String, String) {
    let mut name = None;
    let mut description = None;
    if let Some(after_fence) = content.strip_prefix("---")
        && let Some((frontmatter, _)) = after_fence.split_once("\n---")
    {
        for line in frontmatter.lines() {
            if let Some(value) = line.strip_prefix("name:") {
                name = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("description:") {
                description = Some(value.trim().to_owned());
            }
        }
    }
    let name = name
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| dir_name.to_owned());
    let description = description
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| SKILL_DEFAULT_DESCRIPTION.to_owned());
    (name, description)
}

/// Scan one skills root `<dir>/<name>/SKILL.md` for disclosure entries.
/// Missing directories are empty; non-directory or symlinked skill
/// directories and missing or symlinked `SKILL.md` files are skipped (static
/// boundary only — no TOCTOU/hostile-dir model). I/O errors on a `SKILL.md`
/// that should be readable bubble up with path context.
pub fn scan_skills_dir(dir: &Path) -> anyhow::Result<Vec<SkillIndexEntry>> {
    let dir_entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context(format!("cannot read {}", dir.display())),
    };
    let mut skills = Vec::new();
    for entry in dir_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Err(error).context(format!("cannot read {} entry", dir.display()));
            }
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Ok(dir_name) = entry.file_name().into_string() else {
            continue;
        };
        let skill_path = entry.path().join("SKILL.md");
        let Ok(metadata) = std::fs::symlink_metadata(&skill_path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let content = match std::fs::read_to_string(&skill_path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).context(format!("cannot read {}", skill_path.display()));
            }
        };
        let (name, description) = skill_meta(&content, &dir_name);
        skills.push(SkillIndexEntry {
            name,
            description,
            path: skill_path,
            dir_name,
        });
    }
    Ok(skills)
}

/// Build the startup skill index: global entries first, then workspace
/// entries (`.e-agent/skills/`), a workspace entry with the same directory
/// name fully replacing the global one. Sorted by display name (directory
/// name tie-break, so a HashMap source stays deterministic) and formatted as
/// a `name + description + absolute path` index — never the bodies.
/// `None` when both roots are missing/empty.
pub fn read_skills_index(root: &Path, global_dir: Option<&Path>) -> anyhow::Result<Option<String>> {
    let mut by_dir_name: HashMap<String, SkillIndexEntry> = HashMap::new();
    // The global root must be a real directory, not a symlink, or global
    // skills are skipped entirely (the factory's capability decision uses
    // the same gate).
    if let Some(global) = global_dir.filter(|dir| skills_root_is_dir(dir)) {
        for entry in scan_skills_dir(global)? {
            by_dir_name.insert(entry.dir_name.clone(), entry);
        }
    }
    // The workspace root must be a real directory, not a symlink, or the
    // whole workspace layer is skipped (same gate as the global root).
    let workspace_skills = root.join(".e-agent").join("skills");
    if skills_root_is_dir(&workspace_skills) {
        for entry in scan_skills_dir(&workspace_skills)? {
            by_dir_name.insert(entry.dir_name.clone(), entry);
        }
    }
    if by_dir_name.is_empty() {
        return Ok(None);
    }
    let mut entries: Vec<SkillIndexEntry> = by_dir_name.into_values().collect();
    entries.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.dir_name.cmp(&b.dir_name))
    });
    let lines = entries
        .iter()
        .map(|entry| {
            format!(
                "- **{}**: {} — {}",
                entry.name,
                entry.description,
                entry.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(format!("## Skills\n\n{lines}")))
}

/// In-memory reasoning-effort choice for a runtime profile switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffortOverride {
    Default,
    Omit,
    Literal(String),
}

/// Where the ChatGPT credentials for a chatgpt-routed profile come from.
#[derive(Clone, Copy)]
enum AuthSource<'a> {
    /// Startup and reload validation: the auth captured when the config was
    /// resolved, all-or-nothing — a chatgpt profile without it is an error.
    Fixed(Option<&'a CodexAuth>),
    /// Runtime resolution (`/model` profile switches): the credentials
    /// currently cached in the reloadable state, or a lazy
    /// first-use load from disk, so a login that happened after start is
    /// picked up without a restart or a config reload.
    Lazy(&'a RwLock<ReloadableState>),
}

/// The ChatGPT credentials cached in the reloadable state, loading them from
/// disk on first use when the process started without a login. `CodexAuth::load`
/// is a pure disk read (no network, no rewrite), so a failure leaves the cache
/// empty and the next resolution retries. Loaded once: the write lock is only
/// taken while the cache is still empty, and re-checked under it so concurrent
/// resolutions share one handle.
fn lazy_chatgpt_auth(reloadable: &RwLock<ReloadableState>) -> anyhow::Result<CodexAuth> {
    if let Some(auth) = reloadable.read().unwrap().auth.clone() {
        return Ok(auth);
    }
    let mut state = reloadable.write().unwrap();
    if let Some(auth) = state.auth.clone() {
        return Ok(auth);
    }
    let auth = CodexAuth::load().map_err(|error| {
        anyhow!("ChatGPT auth is not initialized ({error:#}); run `e-agent login`")
    })?;
    state.auth = Some(auth.clone());
    Ok(auth)
}

/// Turn a resolved profile into a wire model, honoring `--base-url`/`--model`
/// overrides for the main model.
fn configured_model(
    resolved: ResolvedModel,
    auth: AuthSource<'_>,
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
                .with_thinking(resolved.thinking)
                .with_deepseek_compat(resolved.deepseek_compat),
            );
            cm.display = display;
            Ok(cm)
        }
        AuthMode::ChatGpt => {
            let auth = match auth {
                AuthSource::Fixed(auth) => auth
                    .cloned()
                    .ok_or_else(|| anyhow!("ChatGPT auth was not initialized"))?,
                AuthSource::Lazy(reloadable) => lazy_chatgpt_auth(reloadable)?,
            };
            let mut cm = ConfiguredModel::codex(
                CodexModel::new(
                    auth,
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
    auth: AuthSource<'_>,
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
/// state. A config that fails to parse or resolve — a bad edit, a missing key
/// file, a chatgpt-routed profile with no login — is rejected and the previous
/// config stays, so a running server never breaks on a config edit.
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
    // Mirror the startup web-search gate: provider/base_url validation
    // always applies, and a provider key section that is malformed is a
    // config error when the matching internal/process env variable does not
    // shadow it (EXA_API_KEY for Exa, the internal DeepSeek key for
    // DeepSeek, no key at all for SearXNG). The resolved values are NOT
    // re-injected at runtime — `std::env::set_var` is only safe at startup
    // (single-threaded), so web-search backend changes still need a restart.
    let web_search_provider = match config.web_search_provider() {
        Ok(provider) => provider,
        Err(error) => return ReloadResult::Rejected(format!("{error:#}")),
    };
    let web_fetch_provider = match config.web_fetch_provider() {
        Ok(provider) => provider,
        Err(error) => return ReloadResult::Rejected(format!("{error:#}")),
    };
    let deepseek_fetch = web_fetch_provider == Some(WebFetchProvider::Deepseek);
    let key_env = match &web_search_provider {
        // A SearXNG search carries no credential unless the fetch is
        // DeepSeek's, which then needs one.
        Some(WebSearchProvider::Searxng { .. }) if !deepseek_fetch => None,
        Some(WebSearchProvider::Searxng { .. }) | Some(WebSearchProvider::Deepseek) => {
            Some(DEEPSEEK_API_KEY_ENV)
        }
        Some(WebSearchProvider::Exa) | None => Some("EXA_API_KEY"),
    };
    if let Some(variable) = key_env
        && std::env::var_os(variable).is_none()
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
    match build_runtime_models(resolved, AuthSource::Fixed(auth.as_ref()), base_url, model) {
        Ok(models) => {
            state.config = Some(config);
            state.auth = auth;
            state.models = Some(models);
            ReloadResult::Reloaded
        }
        Err(error) => ReloadResult::Rejected(format!("{error:#}")),
    }
}

/// The STARTUP project `[sandbox]` overlay, taken from the observation the
/// load produced. An unavailable source is an error, not "no overlay": with no
/// global config it is the only place a policy can come from, and silently
/// dropping it would start an unsandboxed session where the file-reading
/// resolver used to fail.
fn startup_project_overlay(
    root: &Path,
    observation: &ProjectSourceObservation,
) -> anyhow::Result<Option<crate::config::ProjectSandbox>> {
    match observation {
        ProjectSourceObservation::Observed(source) => {
            crate::config::project_sandbox_from_observation(Some(source))
        }
        ProjectSourceObservation::Absent => Ok(None),
        ProjectSourceObservation::Unavailable => Err(anyhow!(
            "cannot use the project config {}: it cannot be read or parsed",
            root.join(".e-agent/config.toml").display()
        )),
    }
}

/// The RECOGNIZED `[sandbox]` inputs of one source table. Only keys the
/// resolver consumes take part in the comparison, so an ignored unknown key
/// (e.g. a TOML `nan`, which never compares equal to itself) cannot make an
/// unchanged source look edited.
fn sandbox_table(source: Option<&toml::Value>) -> Option<toml::Value> {
    let table = source?.get("sandbox")?.as_table()?;
    let mut kept = toml::map::Map::new();
    for key in [
        "enabled",
        "network",
        "workspace_writable",
        "gpu",
        "writable_paths",
        "readable_paths",
    ] {
        if let Some(value) = table.get(key) {
            kept.insert(key.to_owned(), value.clone());
        }
    }
    Some(toml::Value::Table(kept))
}

/// The parsed `[sandbox]` inputs of one config source pair: any change here is
/// what may re-resolve the installed policy.
fn sandbox_inputs(
    global: Option<&toml::Value>,
    project: Option<&toml::Value>,
) -> (Option<toml::Value>, Option<toml::Value>) {
    (sandbox_table(global), sandbox_table(project))
}

/// Load the effective config for `root` and apply it through
/// [`apply_reloaded_config`], then swap the `[sandbox]` policy when (and only
/// when) its parsed inputs changed. The shared core of
/// [`SessionFactory::reload_config`] and the watcher task (which holds the
/// reloadable state and the `[sandbox]` cell, not the factory).
fn reload_config_at(
    reloadable: &RwLock<ReloadableState>,
    root: &Path,
    sandbox: &SandboxPolicy,
    profile: Option<&str>,
    base_url: &Option<String>,
    model: &Option<String>,
    announce: bool,
) -> ReloadResult {
    let (loaded, project_source) = match Config::load_for_workspace_with_sources(root) {
        Ok(loaded) => loaded,
        Err(error) => return ReloadResult::Rejected(format!("{error:#}")),
    };
    // A project override that cannot be read/parsed leaves both the config
    // overlay and the `[sandbox]` inputs unknown. Reject the reload and keep
    // the previous config AND policy rather than accepting a transition that
    // was never validated from one observed source.
    if matches!(project_source, ProjectSourceObservation::Unavailable) {
        return ReloadResult::Rejected(format!(
            "cannot use the project config {}: it cannot be read or parsed",
            root.join(".e-agent/config.toml").display()
        ));
    }
    let mut state = reloadable.write().unwrap();
    let old_global = state.source_global.clone();
    let old_project = state.source_project.clone();
    let new_global = loaded
        .as_ref()
        .and_then(|config| config.diagnostic_sources().0.cloned());
    let new_project = match &project_source {
        ProjectSourceObservation::Observed(source) => Some(source.clone()),
        ProjectSourceObservation::Absent => None,
        ProjectSourceObservation::Unavailable => unreachable!("rejected above"),
    };
    // Parse the observed project `[sandbox]` overlay STRICTLY before the change
    // gate: the comparison only looks at recognized keys, so an invalid scalar
    // or an unknown key inside an otherwise unchanged table would otherwise be
    // accepted without ever being validated against the project schema.
    let project_overlay =
        match crate::config::project_sandbox_from_observation(new_project.as_ref()) {
            Ok(overlay) => overlay,
            Err(error) => return ReloadResult::Rejected(format!("{error:#}")),
        };
    // The Windows write sandbox has no policy-file carve-out, so a project
    // `[sandbox]` edit could be authored by the very shell it governs. Keep
    // the last good config AND policy: project sandbox edits need a restart
    // there (global ones still reload).
    #[cfg(windows)]
    if sandbox_table(old_project.as_ref()) != sandbox_table(new_project.as_ref()) {
        return ReloadResult::Rejected(format!(
            "a project [sandbox] edit cannot be hot-reloaded on Windows: the write sandbox cannot keep {} read-only, so the reloaded policy could be authored by a sandboxed command; restart to apply it",
            root.join(".e-agent/config.toml").display()
        ));
    }
    // Re-resolve ONLY on a `[sandbox]` input change: an unrelated accepted
    // reload keeps the installed policy byte-identical, so a directory that
    // appeared in the meantime never becomes a grant through an edit that had
    // nothing to do with the sandbox. Validation happens here, before any
    // swap, and installation after it, and it uses the overlay parsed from
    // THIS observation (never a second read of the project file).
    let policy = if sandbox_inputs(old_global.as_ref(), old_project.as_ref())
        != sandbox_inputs(new_global.as_ref(), new_project.as_ref())
    {
        match crate::config::resolve_sandbox_with_project(
            loaded.as_ref(),
            root,
            project_overlay.as_ref(),
        ) {
            Ok(policy) => Some(policy.enabled.then_some(policy)),
            Err(error) => return ReloadResult::Rejected(format!("{error:#}")),
        }
    } else {
        None
    };
    let result = apply_reloaded_config(&mut state, loaded, profile, base_url, model);
    if matches!(result, ReloadResult::Reloaded) {
        if let Some(policy) = policy {
            sandbox.replace(policy);
        }
        state.source_global = new_global.clone();
        state.source_project = new_project.clone();
        drop(state);
        if !announce {
            return result;
        }
        let had_edits = crate::config::trace_reload_source_edits(
            old_global.as_ref(),
            old_project.as_ref(),
            new_global.as_ref(),
            new_project.as_ref(),
            profile,
        );
        if !had_edits {
            return ReloadResult::ReloadedNoSource;
        }
    }
    result
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

    fn reasoning(model: &ConfiguredModel) -> Option<&str> {
        match &model.kind {
            crate::model::ConfiguredModelKind::Chat(model) => model.reasoning_effort.as_deref(),
            crate::model::ConfiguredModelKind::Codex(model) => model.reasoning_effort.as_deref(),
        }
    }

    #[test]
    fn resolve_profile_effort_override_table_and_preserves_profile_fields() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(test_config(
                "p1/m1",
                r#"reasoning_effort = "max"
context_window = 32768
vision = true
thinking = true
deepseek_compat = true
"#,
            )),
        );
        let cases = [
            (None, Some("max")),
            (Some(EffortOverride::Default), Some("max")),
            (Some(EffortOverride::Omit), None),
            (
                Some(EffortOverride::Literal("medium".into())),
                Some("medium"),
            ),
            (Some(EffortOverride::Literal("none".into())), Some("none")),
        ];
        let (base, base_window) = factory.resolve_profile("p1/m1", None).unwrap();
        assert_eq!(reasoning(&base), Some("max"));
        let (vision, compat, name) = match &base.kind {
            crate::model::ConfiguredModelKind::Chat(model) => {
                (model.vision, model.deepseek_compat, model.name().to_owned())
            }
            _ => panic!("expected Chat profile"),
        };
        for (override_value, expected_effort) in cases {
            let (model, window) = factory.resolve_profile("p1/m1", override_value).unwrap();
            assert_eq!(reasoning(&model), expected_effort);
            assert_eq!(window, base_window);
            match &model.kind {
                crate::model::ConfiguredModelKind::Chat(model) => {
                    assert_eq!(model.vision, vision);
                    assert_eq!(model.deepseek_compat, compat);
                    assert_eq!(model.name(), name);
                }
                _ => panic!("expected Chat profile"),
            }
        }
    }

    #[derive(Clone, Default)]
    struct ReloadCapture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for ReloadCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ReloadCapture {
        type Writer = ReloadCapture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_reload<F>(capture: &ReloadCapture, action: F)
    where
        F: FnOnce(),
    {
        use tracing_subscriber::layer::SubscriberExt;
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(capture.clone());
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), action);
    }

    fn write_reload_fixture(global: &std::path::Path, project: &std::path::Path) {
        std::fs::write(
            global,
            r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
"#,
        )
        .unwrap();
        std::fs::write(
            project,
            r#"[models."p1/m1"]
model = "project"
"#,
        )
        .unwrap();
    }

    #[test]
    fn reload_global_delete_keeps_unchanged_project_source_silent() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        write_reload_fixture(&global, &project);
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        }

        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        let capture = ReloadCapture::default();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        capture.0.lock().unwrap().clear();

        std::fs::remove_file(&global).unwrap();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("origin=global"), "{output}");
        assert!(!output.contains("origin=project"), "{output}");
        assert!(factory.current_config().is_none());
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    #[test]
    fn reload_global_recreate_same_project_source_silent() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        write_reload_fixture(&global, &project);
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        };

        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        let capture = ReloadCapture::default();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        std::fs::remove_file(&global).unwrap();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        capture.0.lock().unwrap().clear();

        std::fs::write(
            &global,
            r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
"#,
        )
        .unwrap();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("origin=global"), "{output}");
        assert!(!output.contains("origin=project"), "{output}");
        assert!(factory.current_config().is_some());
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    /// A7: with the global config gone and the project file malformed, the
    /// reload has no single observed project source to validate the transition
    /// against. Reject it and keep BOTH the previous config and the previous
    /// policy (the smaller of the two allowed outcomes).
    #[test]
    fn reload_malformed_project_without_global_is_rejected_and_keeps_both() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        write_reload_fixture(&global, &project);
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        };

        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        let capture = ReloadCapture::default();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        let before = factory.reloadable.read().unwrap().source_project.clone();
        let before_policy = factory.sandbox_policy().snapshot();
        std::fs::remove_file(&global).unwrap();
        std::fs::write(&project, "[models.\"p1/m1\"\n").unwrap();
        capture.0.lock().unwrap().clear();

        capture_reload(&capture, || match factory.reload_config() {
            ReloadResult::Rejected(reason) => assert!(
                reason.contains("project config") && reason.contains("cannot be read or parsed"),
                "{reason}"
            ),
            other => panic!("expected rejection, got {other:?}"),
        });
        // No success diff is emitted, and neither the config nor the policy
        // moved: the transition was never validated.
        assert!(capture.0.lock().unwrap().is_empty());
        assert!(factory.current_config().is_some());
        assert_eq!(factory.sandbox_policy().snapshot(), before_policy);
        assert_eq!(factory.reloadable.read().unwrap().source_project, before);
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
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
            factory
                .resolve_profile("p2/m2", None)
                .unwrap()
                .0
                .display_name(),
            "m2"
        );
        assert_eq!(
            factory.resolve_profile("p2/m2", None).unwrap().1,
            None,
            "profiles without context_window carry None to runtime switches"
        );
        assert_eq!(factory.main_model().display_name(), "m1");
        assert_eq!(
            factory.current_config().unwrap().model_profiles(),
            vec!["p1/m1", "p2/m2"]
        );
    }

    #[test]
    fn resolve_profile_returns_profile_context_window() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(test_config(
                "p1/m1",
                r#"[models."p2/m2"]
model = "m2"
context_window = 32768
[providers.p2]
base_url = "http://two"
api_key_env = "PATH"
"#,
            )),
        );

        assert_eq!(
            factory.resolve_profile("p2/m2", None).unwrap().1,
            Some(32_768)
        );
        assert_eq!(factory.resolve_profile("p1/m1", None).unwrap().1, None);
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
        assert!(factory.resolve_profile("p1/m1", None).is_ok());
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
        assert!(factory.resolve_profile("p1/m1", None).is_err());
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
            factory
                .resolve_profile("p2/m2", None)
                .unwrap()
                .0
                .display_name(),
            "m2"
        );
    }

    #[test]
    fn reload_path_emits_safe_source_diagnostics_and_suppresses_noops() {
        use std::io::Write;

        #[derive(Clone, Default)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Capture;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        fn run_with_capture<F>(capture: &Capture, action: F)
        where
            F: FnOnce(),
        {
            use tracing_subscriber::layer::SubscriberExt;
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(capture.clone());
            tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), action);
        }

        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        let global_text = |enabled: bool| {
            format!(
                r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
[mcp.server]
command = ["server"]
enabled = {enabled}
"#
            )
        };
        std::fs::write(&global, global_text(false)).unwrap();
        std::fs::write(
            &project,
            r#"[mcp.server]
command = ["project-server"]
[mcp.server.env]
TOKEN = "old-secret"
"#,
        )
        .unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        let capture = Capture::default();
        run_with_capture(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        capture.0.lock().unwrap().clear();

        // A project edit is observed through the actual reload entry point and
        // is classified against the global kill switch.
        std::fs::write(
            &project,
            r#"[mcp.server]
command = ["project-server-2"]
[mcp.server.env]
TOKEN = "new-secret"
[pet]
spritesheet = "pet.png"
[sandbox]
network = false
"#,
        )
        .unwrap();
        // An accepted project edit is inspected below. On Windows the project
        // `[sandbox]` leaf makes the whole edit restart-only, so the
        // platform-appropriate expectation there is a rejection that keeps
        // both states (checked in the `cfg(windows)` block at the end).
        #[cfg(not(windows))]
        {
            run_with_capture(&capture, || {
                assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
            });
            let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert!(output.contains(" WARN"), "{output}");
            assert!(output.contains("origin=project"), "{output}");
            assert!(output.contains("path=mcp.server.command"), "{output}");
            assert!(
                output.contains("before=[\"project-server\"] after=[\"project-server-2\"]"),
                "{output}"
            );
            assert!(output.contains("status=shadowed"), "{output}");
            let pet_line = output
                .lines()
                .find(|line| line.contains("path=pet.spritesheet"))
                .expect("pet diagnostic");
            assert!(pet_line.contains("INFO"), "{pet_line}");
            assert!(pet_line.contains("status=effective"), "{pet_line}");
            // `[sandbox]` leaves are effective now (live shell policy), and a
            // project path that the resolver would reject is a rejected reload —
            // this fixture therefore uses a scalar, which resolves on its own.
            assert!(output.contains("path=sandbox.network"), "{output}");
            assert!(!output.contains("old-secret"), "{output}");
            assert!(!output.contains("new-secret"), "{output}");
            // Secret-only env value changes collapse after sanitization, so no
            // env edit is emitted; the key-preserving shape is tested directly.
            assert!(!output.contains("mcp.server.env.TOKEN"), "{output}");
            assert!(output.lines().count() >= 3, "{output}");
            capture.0.lock().unwrap().clear();

            // The same validated state is refreshed without either success form.
            run_with_capture(&capture, || {
                assert!(matches!(
                    factory.reload_config(),
                    ReloadResult::ReloadedNoSource
                ));
            });
            assert!(capture.0.lock().unwrap().is_empty());

            // Removing the global kill switch is an effective global source edit,
            // not a falsely shadowed project removal.
            std::fs::write(&global, global_text(true)).unwrap();
            run_with_capture(&capture, || {
                assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
            });
            let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert!(output.contains("origin=global"), "{output}");
            assert!(output.contains("mcp.server.enabled"), "{output}");
            assert!(output.contains("status=effective"), "{output}");
            capture.0.lock().unwrap().clear();

            // A rejected file does not emit source-success records or replace the
            // prior validated runtime/snapshot.
            std::fs::write(&global, "default = \"missing/profile\"\n").unwrap();
            run_with_capture(&capture, || {
                assert!(matches!(factory.reload_config(), ReloadResult::Rejected(_)));
            });
            assert!(capture.0.lock().unwrap().is_empty());
            assert_eq!(factory.model_profiles(), vec!["p1/m1"]);
            let state = factory.reloadable.read().unwrap();
            assert!(state.source_global.is_some());
            drop(state);

            // TUI mode suppresses diagnostics while still accepting the edit.
            let mut quiet = SessionFactory::test_factory(temp.path().to_path_buf());
            quiet.announce = false;
            std::fs::write(&global, global_text(true)).unwrap();
            capture.0.lock().unwrap().clear();
            run_with_capture(&capture, || {
                assert!(matches!(quiet.reload_config(), ReloadResult::Reloaded));
            });
            assert!(capture.0.lock().unwrap().is_empty());
        }
        // Windows: the same project edit (it carries `[sandbox] network =
        // false`) is rejected and keeps BOTH the config and the policy; a
        // global-only edit still reloads.
        #[cfg(windows)]
        {
            let before_cfg = factory.model_profiles();
            let before_policy = factory.sandbox_policy().snapshot();
            run_with_capture(&capture, || match factory.reload_config() {
                ReloadResult::Rejected(reason) => {
                    assert!(reason.contains("project [sandbox]"), "{reason}")
                }
                other => panic!("expected rejection, got {other:?}"),
            });
            assert!(capture.0.lock().unwrap().is_empty());
            assert_eq!(factory.model_profiles(), before_cfg);
            assert_eq!(factory.sandbox_policy().snapshot(), before_policy);
            // Global-only edit: still reloads (the project sandbox input did
            // not change).
            std::fs::write(
                &project,
                r#"[mcp.server]
command = ["project-server"]
[mcp.server.env]
TOKEN = "old-secret"
"#,
            )
            .unwrap();
            run_with_capture(&capture, || {
                assert!(matches!(
                    factory.reload_config(),
                    ReloadResult::Reloaded | ReloadResult::ReloadedNoSource
                ));
            });
            std::fs::write(&global, global_text(true)).unwrap();
            run_with_capture(&capture, || {
                assert!(matches!(
                    factory.reload_config(),
                    ReloadResult::Reloaded | ReloadResult::ReloadedNoSource
                ));
            });
        }
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
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
        assert!(factory.resolve_profile("p2/m2", None).is_ok());

        // A broken edit is rejected and the previous config stays.
        write_config("default = \"nope/nope\"\n");
        assert!(matches!(factory.reload_config(), ReloadResult::Rejected(_)));
        assert_eq!(factory.model_profiles(), vec!["p1/m1", "p2/m2"]);

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn role_models_resolver_reads_hot_reloaded_roles() {
        let temp = tempfile::tempdir().unwrap();
        // The test factory starts with an empty reloadable cell
        // (models = None); apply configs through the real reload path so
        // runtime models exist, exactly like the watcher does.
        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        let with_role = |role_profile: &str| -> Config {
            let source = format!(
                r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"

[models."p1/m1"]
model = "m1"

[providers.p2]
base_url = "http://two"
api_key_env = "PATH"

[models."p2/m2"]
model = "m2"

[roles]
fixer = "{role_profile}"
"#
            );
            toml::from_str(&source).expect("test config parses")
        };
        {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, Some(with_role("p1/m1")), None, &None, &None);
        }
        let resolver = factory.role_models_resolver();
        // Startup config routes fixer to p1/m1 (display name "m1").
        let (model, context_window) = resolver("fixer").expect("fixer routed");
        assert_eq!(model.display_name(), "m1");
        assert_eq!(context_window, None);

        // Hot reload re-routes fixer to p2/m2: the SAME resolver sees it,
        // so a resident Delegate picks the new model up at spawn.
        {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, Some(with_role("p2/m2")), None, &None, &None);
        }
        let (model, _) = resolver("fixer").expect("fixer still routed");
        assert_eq!(model.display_name(), "m2");

        // An unrouted role resolves to None; the caller falls back to its
        // subagent model.
        assert!(resolver("nobody").is_none());

        // No config at all (models = None): the closure falls back to the
        // startup snapshot, exactly like build() does.
        let mut plain = SessionFactory::test_factory(temp.path().to_path_buf());
        plain.role_models.insert(
            "fixer".to_owned(),
            ConfiguredModel::chat(
                OpenAiModel::new(
                    "http://localhost".into(),
                    "test-key".into(),
                    "snapshot-model".into(),
                    None,
                )
                .expect("test model"),
            ),
        );
        let snapshot_resolver = plain.role_models_resolver();
        let (model, _) = snapshot_resolver("fixer").expect("snapshot fallback");
        assert_eq!(model.display_name(), "snapshot-model");
    }

    /// A chatgpt-routed config: one profile on the ChatGPT login plus extra
    /// TOML (e.g. a `[roles]` block).
    fn chatgpt_config(extra: &str) -> Config {
        let source = format!(
            r#"default = "cg/m1"
[providers.cg]
auth = "chatgpt"

[models."cg/m1"]
model = "m1"
{extra}
"#
        );
        toml::from_str(&source).expect("test config parses")
    }

    /// Write a decodable ChatGPT login exactly where `CodexAuth::load` looks
    /// for it (`$XDG_CONFIG_HOME/e-agent/auth.json`).
    fn write_auth_file(xdg: &std::path::Path) -> std::path::PathBuf {
        let dir = xdg.join("e-agent");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"tokens":{"id_token":"id","access_token":"access","refresh_token":"refresh","account_id":"account"},"last_refresh":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        path
    }

    /// Point the process-wide config/home lookup at a temp dir for the
    /// duration of one test (serialized by `XDG_TEST_LOCK`), restoring both
    /// previous values on drop.
    struct XdgEnv {
        xdg: std::path::PathBuf,
        previous_xdg: Option<std::ffi::OsString>,
        previous_home: Option<std::ffi::OsString>,
    }

    impl XdgEnv {
        fn new(root: &std::path::Path) -> Self {
            let xdg = root.join("xdg");
            let home = root.join("home");
            std::fs::create_dir_all(&xdg).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
            let previous_home = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", &xdg);
                std::env::set_var("HOME", &home);
            }
            Self {
                xdg,
                previous_xdg,
                previous_home,
            }
        }
    }

    impl Drop for XdgEnv {
        fn drop(&mut self) {
            restore_env("XDG_CONFIG_HOME", self.previous_xdg.take());
            restore_env("HOME", self.previous_home.take());
        }
    }

    fn restore_env(key: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    /// The `{:#}` error chain of a failing `resolve_profile`.
    fn resolve_error(factory: &SessionFactory, profile: &str) -> String {
        match factory.resolve_profile(profile, None) {
            Ok(_) => panic!("expected `{profile}` to fail"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn lazy_load_after_login() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let env = XdgEnv::new(temp.path());
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(chatgpt_config("")),
        );

        // No login yet: the runtime path reports it and points at the fix.
        let error = resolve_error(&factory, "cg/m1");
        assert!(error.contains("e-agent login"), "{error}");

        // `e-agent login` in another process writes the file; the next
        // resolution picks it up without a restart or a config reload.
        write_auth_file(&env.xdg);
        let (model, context_window) = factory.resolve_profile("cg/m1", None).unwrap();
        assert_eq!(model.display_name(), "m1");
        assert_eq!(context_window, None);
        assert!(factory.reloadable.read().unwrap().auth.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    // Test-only env isolation: the std Mutex guard is held across the awaits
    // that join the resolution tasks (same pattern as delegate_tests).
    #[allow(clippy::await_holding_lock)]
    async fn lazy_load_concurrent() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let env = XdgEnv::new(temp.path());
        write_auth_file(&env.xdg);
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(chatgpt_config("")),
        );

        // Concurrent first-use resolutions must share one load without
        // panicking or deadlocking on the reloadable lock.
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let factory = factory.clone();
            tasks.push(tokio::spawn(async move {
                factory
                    .resolve_profile("cg/m1", None)
                    .map(|(model, _)| model)
            }));
        }
        for task in tasks {
            let model = task
                .await
                .expect("resolution task must not panic")
                .expect("resolution must succeed after login");
            assert_eq!(model.display_name(), "m1");
        }
        assert!(factory.reloadable.read().unwrap().auth.is_some());
    }

    #[test]
    fn lazy_load_preserves_failed_error() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let env = XdgEnv::new(temp.path());
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(chatgpt_config("")),
        );

        // No login: the actionable message names the fix.
        let error = resolve_error(&factory, "cg/m1");
        assert!(error.contains("ChatGPT auth is not initialized"), "{error}");
        assert!(error.contains("e-agent login"), "{error}");

        // A stored but undecodable login carries the underlying cause and is
        // not cached: the next resolution retries the load (here it picks up
        // the repaired file) without a restart.
        let auth_file = write_auth_file(&env.xdg);
        std::fs::write(&auth_file, "{not json").unwrap();
        let error = resolve_error(&factory, "cg/m1");
        assert!(error.contains("cannot decode"), "{error}");
        assert!(factory.reloadable.read().unwrap().auth.is_none());

        write_auth_file(&env.xdg);
        let (model, _) = factory.resolve_profile("cg/m1", None).unwrap();
        assert_eq!(model.display_name(), "m1");
    }

    #[test]
    fn lazy_auth_is_cached_without_rereading_disk() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let env = XdgEnv::new(temp.path());
        let auth_file = write_auth_file(&env.xdg);
        let factory = SessionFactory::test_factory_with_config(
            temp.path().to_path_buf(),
            Some(chatgpt_config("")),
        );
        assert!(factory.resolve_profile("cg/m1", None).is_ok());

        // The credentials are cached: later resolutions neither re-read the
        // file (deleted here) nor re-decode it (corrupt here).
        std::fs::remove_file(&auth_file).unwrap();
        assert!(factory.resolve_profile("cg/m1", None).is_ok());
        std::fs::write(&auth_file, "{not json").unwrap();
        assert!(factory.resolve_profile("cg/m1", None).is_ok());
    }

    #[test]
    fn subagent_model_resolver_reads_hot_reloaded_state() {
        let temp = tempfile::tempdir().unwrap();
        // The test factory starts with an empty reloadable cell
        // (models = None); apply configs through the real reload path so
        // runtime models exist, exactly like the watcher does.
        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        let with_subagent = |subagent_profile: Option<&str>| -> Config {
            let routing = match subagent_profile {
                Some(profile) => format!("subagent = \"{profile}\"\n"),
                None => String::new(),
            };
            let source = format!(
                r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"

[models."p1/m1"]
model = "m1"

[providers.p2]
base_url = "http://two"
api_key_env = "PATH"

[models."p2/m2"]
model = "m2"

[roles]
{routing}"#
            );
            toml::from_str(&source).expect("test config parses")
        };
        {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(
                &mut state,
                Some(with_subagent(Some("p1/m1"))),
                None,
                &None,
                &None,
            );
        }
        let resolver = factory.subagent_model_resolver();
        // Startup config routes the default subagent to p1/m1 (wire name
        // "m1").
        let (model, context_window) = resolver().expect("subagent routed");
        assert_eq!(model.display_name(), "m1");
        assert_eq!(context_window, None);

        // Hot reload re-routes the default subagent to p2/m2: the SAME
        // resolver sees it, so a resident Delegate's fallback delegations
        // (no routed role) pick the new model up at spawn.
        {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(
                &mut state,
                Some(with_subagent(Some("p2/m2"))),
                None,
                &None,
                &None,
            );
        }
        let (model, _) = resolver().expect("subagent still routed");
        assert_eq!(model.display_name(), "m2");

        // No `[roles] subagent` routing at all: the resolver falls back to
        // the live main model — the same fallback a newly built session
        // uses, so a resident Delegate and a fresh session agree.
        {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, Some(with_subagent(None)), None, &None, &None);
        }
        let (model, context_window) = resolver().expect("main fallback");
        assert_eq!(model.display_name(), "m1");
        assert_eq!(context_window, None);

        // No config at all (models = None): the closure falls back to the
        // startup snapshot, exactly like build() does.
        let mut plain = SessionFactory::test_factory(temp.path().to_path_buf());
        plain.subagent_model = Some(ConfiguredModel::chat(
            OpenAiModel::new(
                "http://localhost".into(),
                "test-key".into(),
                "snapshot-model".into(),
                None,
            )
            .expect("test model"),
        ));
        let snapshot_resolver = plain.subagent_model_resolver();
        let (model, _) = snapshot_resolver().expect("snapshot fallback");
        assert_eq!(model.display_name(), "snapshot-model");
    }

    /// The erased-route lifecycle a resident Delegate sees: main=M, the
    /// subagent route hot-reloads A → B, then the route is deleted — the
    /// resolver must end on the live main model M, not on the stale A/B
    /// route snapshot. Distinct context windows per profile pin the
    /// model↔window pairing (main keeps main's window).
    #[test]
    fn subagent_model_lifecycle_falls_back_to_live_main() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        let with_subagent = |profile: Option<&str>| -> Config {
            let routing = match profile {
                Some(profile) => format!("subagent = \"{profile}\"\n"),
                None => String::new(),
            };
            let source = format!(
                r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"

[models."p1/m1"]
model = "m1"
context_window = 1000

[providers.p2]
base_url = "http://two"
api_key_env = "PATH"

[models."p2/m2"]
model = "m2"
context_window = 2000

[providers.p3]
base_url = "http://three"
api_key_env = "PATH"

[models."p3/m3"]
model = "m3"
context_window = 3000

[roles]
{routing}"#
            );
            toml::from_str(&source).expect("test config parses")
        };
        let apply = |config: Config| {
            let mut state = factory.reloadable.write().unwrap();
            apply_reloaded_config(&mut state, Some(config), None, &None, &None);
        };
        let resolver = factory.subagent_model_resolver();

        // subagent = A.
        apply(with_subagent(Some("p2/m2")));
        let (model, context_window) = resolver().expect("subagent A routed");
        assert_eq!(model.display_name(), "m2");
        assert_eq!(context_window, Some(2000));

        // Hot reload: subagent = B.
        apply(with_subagent(Some("p3/m3")));
        let (model, context_window) = resolver().expect("subagent B routed");
        assert_eq!(model.display_name(), "m3");
        assert_eq!(context_window, Some(3000));

        // Hot reload: the route is deleted — main M and main's own window.
        apply(with_subagent(None));
        let (model, context_window) = resolver().expect("main fallback");
        assert_eq!(model.display_name(), "m1");
        assert_eq!(context_window, Some(1000));
    }

    /// Sandbox hot reload, factory level: the winning `[sandbox]` edit is
    /// re-resolved with the SAME workspace-aware resolver startup uses (global
    /// section + project overlay) and swapped into the live policy cell, so an
    /// already-built session's next bash dispatch sees it. A rejected edit
    /// keeps the last good policy, exactly like the rest of the reload.
    #[test]
    fn reload_swaps_the_live_sandbox_policy_and_keeps_it_on_rejection() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        std::fs::write(
            &global,
            r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
[sandbox]
enabled = true
network = true
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        };

        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        // A test factory starts with the live cell empty (no sandbox).
        assert!(factory.sandbox_policy().snapshot().is_none());

        // Policy A: the global `[sandbox]` section, enabled with networking.
        let capture = ReloadCapture::default();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        let policy_a = factory.sandbox_policy().snapshot().expect("policy A");
        assert!(policy_a.enabled && policy_a.network && policy_a.workspace_writable);
        // The per-leaf source edit is reported as effective, not as a restart.
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("path=sandbox.enabled") && output.contains("status=effective"),
            "{output}"
        );
        assert!(!output.contains("require restart"), "{output}");

        // Policy B: a project `[sandbox]` scalar overlay wins through the same
        // resolver startup uses — on Linux. Windows has no policy-file
        // carve-out, so the same edit is rejected and both states are kept.
        std::fs::write(&project, "[sandbox]\nnetwork = false\n").unwrap();
        capture.0.lock().unwrap().clear();
        let last_good = {
            #[cfg(not(windows))]
            {
                capture_reload(&capture, || {
                    assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
                });
                let policy_b = factory.sandbox_policy().snapshot().expect("policy B");
                assert!(policy_b.enabled, "the global switch still applies");
                assert!(!policy_b.network, "the project overlay wins");
                let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
                assert!(
                    output.contains("path=sandbox.network") && output.contains("status=effective"),
                    "{output}"
                );
                policy_b
            }
            #[cfg(windows)]
            {
                capture_reload(&capture, || match factory.reload_config() {
                    ReloadResult::Rejected(reason) => {
                        assert!(reason.contains("project [sandbox]"), "{reason}")
                    }
                    other => panic!("expected rejection, got {other:?}"),
                });
                assert!(capture.0.lock().unwrap().is_empty());
                let kept = factory.sandbox_policy().snapshot().expect("policy A kept");
                assert_eq!(kept, policy_a, "the last good policy is preserved");
                assert_eq!(
                    factory
                        .current_config()
                        .map(|config| config.model_profiles()),
                    Some(vec!["p1/m1".to_owned()]),
                    "the last good config is preserved"
                );
                kept
            }
        };

        // A project path with no globally authorized ancestor is a rejected
        // reload: the policy stays at the last good value.
        let outside = temp.path().join("project-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            &project,
            "[sandbox]\nwritable_paths = [\"project-outside\"]\n",
        )
        .unwrap();
        match factory.reload_config() {
            ReloadResult::Rejected(reason) => {
                #[cfg(not(windows))]
                assert!(
                    reason.contains("not within any globally authorized writable root"),
                    "{reason}"
                );
                #[cfg(windows)]
                assert!(reason.contains("project [sandbox]"), "{reason}");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
        assert_eq!(factory.sandbox_policy().snapshot(), Some(last_good));
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    /// D1: the STARTUP mapping distinguishes an absent project source (no
    /// overlay) from an unavailable one (an error, not a silently unsandboxed
    /// session) and parses the observed overlay with the strict project schema.
    #[test]
    fn startup_project_overlay_is_strict_and_rejects_unavailable_sources() {
        let root = Path::new("/workspace");
        let observed = |value: toml::Value| ProjectSourceObservation::Observed(value);
        let project = |text: &str| toml::from_str(text).unwrap();
        // Valid overlay: parsed, not ignored.
        let overlay =
            startup_project_overlay(root, &observed(project("[sandbox]\nnetwork = false\n")))
                .unwrap()
                .expect("overlay");
        assert_eq!(overlay.network, Some(false));
        // Absent source: no overlay at all.
        assert!(
            startup_project_overlay(root, &ProjectSourceObservation::Absent)
                .unwrap()
                .is_none()
        );
        // Unavailable source: an error (the old file-reading resolver used to
        // fail startup here; it must not become "unsandboxed").
        let error = startup_project_overlay(root, &ProjectSourceObservation::Unavailable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("project config"), "{error}");
        // The strict project schema applies to the observation: an unknown key,
        // a non-table section, or a POSITIONAL array (which serde would accept
        // for the struct fields while the input comparison reads it as absent)
        // is an error, not a silent ignore.
        for bad in [
            "[sandbox]\nbogus = 1\n",
            "sandbox = 5\n",
            "sandbox = [true, false, true, false, [], []]\n",
        ] {
            assert!(
                startup_project_overlay(root, &observed(project(bad))).is_err(),
                "{bad}"
            );
        }
    }

    /// D1 (reload half): the observed project `[sandbox]` overlay is validated
    /// strictly BEFORE the change gate, so an invalid overlay whose recognized
    /// inputs look unchanged is still rejected.
    ///
    /// Platform-neutral by construction: the baseline has NO project `[sandbox]`
    /// table, and the malformed value is a POSITIONAL array — the recognized
    /// comparison reads it as absent (unchanged, so no re-resolution and no
    /// Windows project-sandbox guard), which leaves strict validation as the
    /// only thing that can reject it.
    #[test]
    fn reload_validates_the_project_sandbox_overlay_before_the_change_gate() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::fs::create_dir_all(xdg.join("e-agent")).unwrap();
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        // No global config at all: the project file is the only source, and it
        // starts without a `[sandbox]` table.
        std::fs::write(&project, "[pet]\nspritesheet = \"pet.png\"\n").unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        };
        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        let capture = ReloadCapture::default();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        // A positional array: serde would accept it as the five struct fields,
        // while the recognized comparison (`as_table`) sees no table at all.
        std::fs::write(&project, "sandbox = [true, false, true, false, [], []]\n").unwrap();
        capture.0.lock().unwrap().clear();
        capture_reload(&capture, || match factory.reload_config() {
            ReloadResult::Rejected(reason) => assert!(
                reason.contains("[sandbox] overlay must be a table"),
                "{reason}"
            ),
            other => panic!("expected rejection, got {other:?}"),
        });
        assert!(capture.0.lock().unwrap().is_empty());
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    /// A3: only a CONFIRMED absence of every global config candidate may take
    /// the no-global transition. An unreadable/uninspectable candidate is a
    /// rejected reload that keeps the last good config AND policy.
    #[test]
    fn reload_uninspectable_global_config_is_rejected_and_keeps_state() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        // An ENABLED policy, so the test proves preservation rather than only
        // rejection.
        std::fs::write(
            &global,
            r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
[sandbox]
enabled = true
"#,
        )
        .unwrap();
        std::fs::write(&project, "").unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        };

        let mut factory = SessionFactory::test_factory(temp.path().to_path_buf());
        factory.announce = true;
        let capture = ReloadCapture::default();
        capture_reload(&capture, || {
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        });
        let profiles = factory.model_profiles();
        let policy = factory.sandbox_policy().snapshot();
        assert!(
            policy.as_ref().is_some_and(|policy| policy.enabled),
            "the fixture must install an enabled policy: {policy:?}"
        );

        // Replace the global config DIRECTORY with a regular file: looking up
        // `<xdg>/e-agent/config.toml` now fails with ENOTDIR, which is not a
        // confirmed absence.
        std::fs::remove_dir_all(&global_dir).unwrap();
        std::fs::write(&global_dir, "not a directory").unwrap();
        capture.0.lock().unwrap().clear();
        capture_reload(&capture, || match factory.reload_config() {
            ReloadResult::Rejected(reason) => {
                assert!(reason.contains("config.toml"), "{reason}")
            }
            other => panic!("expected rejection, got {other:?}"),
        });
        assert!(capture.0.lock().unwrap().is_empty());
        assert_eq!(factory.model_profiles(), profiles);
        assert_eq!(factory.sandbox_policy().snapshot(), policy);

        // A DIRECTORY at the candidate path is equally not a confirmed
        // absence.
        std::fs::remove_file(&global_dir).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        capture_reload(&capture, || match factory.reload_config() {
            ReloadResult::Rejected(reason) => assert!(
                reason.contains("not a regular file") && reason.contains("config.toml"),
                "{reason}"
            ),
            other => panic!("expected rejection, got {other:?}"),
        });
        assert_eq!(factory.model_profiles(), profiles);
        assert_eq!(factory.sandbox_policy().snapshot(), policy);

        // A DANGLING symlink at the candidate path is an error too (metadata
        // would report NotFound, which must not read as "absent").
        #[cfg(unix)]
        {
            std::fs::remove_dir_all(&global).unwrap();
            std::os::unix::fs::symlink(temp.path().join("missing-config"), &global).unwrap();
            capture_reload(&capture, || match factory.reload_config() {
                ReloadResult::Rejected(reason) => assert!(
                    reason.contains("dangling symlink") && reason.contains("config.toml"),
                    "{reason}"
                ),
                other => panic!("expected rejection, got {other:?}"),
            });
            assert_eq!(factory.model_profiles(), profiles);
            assert_eq!(factory.sandbox_policy().snapshot(), policy);
            // A symlink to a REGULAR file still loads.
            std::fs::remove_file(&global).unwrap();
            let target = temp.path().join("real-config.toml");
            std::fs::write(
                &target,
                r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
[sandbox]
enabled = true
network = false
"#,
            )
            .unwrap();
            std::os::unix::fs::symlink(&target, &global).unwrap();
            assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
            assert!(
                factory
                    .sandbox_policy()
                    .snapshot()
                    .is_some_and(|policy| !policy.network),
                "a symlinked global config is loaded normally"
            );
        }
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    /// A6: the installed policy is re-resolved only when the parsed `[sandbox]`
    /// inputs change. A directory that appears between reloads therefore never
    /// becomes a grant through an unrelated edit, while a real sandbox edit
    /// still re-resolves (and only then picks the path up).
    #[test]
    fn reload_reresolves_the_policy_only_on_a_sandbox_input_change() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        let global_dir = xdg.join("e-agent");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global = global_dir.join("config.toml");
        let project_dir = temp.path().join(".e-agent");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = project_dir.join("config.toml");
        let only_models = r#"default = "p1/m1"
[providers.p1]
base_url = "http://one"
api_key_env = "PATH"
[models."p1/m1"]
model = "m1"
"#;
        // The readable path does not exist yet, so it resolves to nothing.
        std::fs::write(
            &global,
            format!("{only_models}[sandbox]\nenabled = true\nreadable_paths = [\"later\"]\n"),
        )
        .unwrap();
        std::fs::write(&project, "").unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
            std::env::set_var("HOME", &home);
        };

        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        let installed = factory.sandbox_policy().snapshot().expect("sandbox on");
        assert!(installed.readable_paths.is_empty(), "{installed:?}");

        // The grant appears on disk, then an UNRELATED edit is accepted.
        let later = temp.path().join("later");
        std::fs::create_dir_all(&later).unwrap();
        std::fs::write(
            &global,
            format!(
                "{only_models}[models.\"p2/m2\"]\nmodel = \"m2\"\n[sandbox]\nenabled = true\nreadable_paths = [\"later\"]\n"
            ),
        )
        .unwrap();
        assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        assert!(factory.model_profiles().contains(&"p2/m2".to_owned()));
        let unchanged = factory
            .sandbox_policy()
            .snapshot()
            .expect("sandbox still on");
        assert_eq!(
            unchanged, installed,
            "an unrelated edit must not install the newly resolvable path"
        );

        // Touching the sandbox input itself re-resolves, so the path now
        // appears (a deliberate sandbox edit, not a silent side effect).
        std::fs::write(
            &global,
            format!(
                "{only_models}[models.\"p2/m2\"]\nmodel = \"m2\"\n[sandbox]\nenabled = true\nnetwork = false\nreadable_paths = [\"later\"]\n"
            ),
        )
        .unwrap();
        assert!(matches!(factory.reload_config(), ReloadResult::Reloaded));
        let resolved = factory.sandbox_policy().snapshot().expect("sandbox on");
        assert!(!resolved.network);
        assert_eq!(
            resolved.readable_paths,
            vec![
                crate::canonicalize_path(&later)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            if let Some(home) = old_home {
                std::env::set_var("HOME", home);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

#[cfg(test)]
mod fork_marker_tests {
    use super::*;
    use crate::agent::SessionEntry;
    use crate::session::Session;

    fn user(text: &str) -> SessionEntry {
        crate::agent::Message::User {
            content: text.to_owned(),
            images: vec![],
        }
        .into()
    }

    fn assistant(text: &str) -> SessionEntry {
        crate::agent::Message::Assistant(crate::agent::AssistantMessage {
            content: Some(text.to_owned()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into()
    }

    fn compaction(summary: &str) -> SessionEntry {
        SessionEntry::Compaction {
            summary: summary.to_owned(),
            retained: vec![],
            current_prompt_at: None,
            no_current_prompt: false,
        }
    }

    /// A main/server fork writes a PINNED marker (owner + row key, no
    /// positional `at`) and freezes the newest goal at or before the cut, so a
    /// context that starts at the last compaction cannot drop it.
    #[tokio::test]
    async fn main_fork_pins_the_cut_and_freezes_the_goal_at_the_cut() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let source = "main-fork-src";
        let goal = crate::agent::create_goal(
            None,
            "main fork goal".into(),
            vec!["survives the head-only load".into()],
        )
        .unwrap();
        // JSONL ordinals: 0 q0, 1 a0, 2 goal, 3 compaction, 4 q1, 5 a1.
        Session::append(
            &root,
            source,
            &[
                user("q0"),
                assistant("a0"),
                SessionEntry::GoalUpdated {
                    goal: Some(goal.clone()),
                },
                compaction("main summary"),
                user("q1"),
                assistant("a1"),
            ],
        )
        .unwrap();
        let factory = SessionFactory::test_factory(root.clone());
        let build = factory
            .build_fork(
                "main-fork-target",
                source.to_owned(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the main fork builds");
        assert_eq!(build.session, "main-fork-target");
        drop(build);

        let stored = Session::load(&root, "main-fork-target").unwrap().entries;
        assert_eq!(stored.len(), 2, "goal freeze + marker only: {stored:?}");
        assert!(matches!(
            stored[0],
            SessionEntry::GoalUpdated { goal: Some(_) }
        ));
        assert!(
            matches!(
                stored[1],
                SessionEntry::ForkedFrom {
                    at: None,
                    seq: Some(5),
                    ..
                }
            ),
            "the cut is pinned by row, not by position: {stored:?}"
        );
        let raw =
            std::fs::read_to_string(root.join(".e-agent/sessions/main-fork-target.jsonl")).unwrap();
        assert!(!raw.contains(r#""at":"#), "no positional `at`: {raw}");

        // The inherited context starts at the compaction, so the pre-compaction
        // goal row is only available through the freeze — and it is.
        let loaded = SessionStore::Jsonl
            .load_fork_located(&root, "main-fork-target")
            .await
            .unwrap();
        assert_eq!(loaded.inherited, 3, "compaction + q1 + a1");
        assert_eq!(loaded.entries[0], compaction("main summary"));
        let (folded, cleared) = Agent::fold_goal(&loaded.entries);
        assert!(!cleared);
        assert_eq!(
            folded.map(|goal| goal.objective),
            Some(goal.objective.clone()),
            "the frozen goal must survive the head-only context"
        );
    }

    /// A fork of a ZERO-COPY child: a cut inside the inherited prefix is
    /// pinned to the ANCESTOR that physically stores that row, a cut inside
    /// the child's own rows stays the child's. `--at` stays a 1-based index
    /// into the RESOLVED transcript, never a physical `seq`.
    #[tokio::test]
    async fn main_fork_of_a_fork_resolves_the_cut_to_its_physical_owner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(
            &root,
            "chain-grand",
            &[user("q0"), assistant("a0"), user("q1"), assistant("a1")],
        )
        .unwrap();
        // middle: a LEGACY positional marker (grand ordinals 0..4) plus two own
        // rows at its own ordinals 1 and 2.
        Session::append(
            &root,
            "chain-middle",
            &[
                SessionEntry::ForkedFrom {
                    source: "chain-grand".into(),
                    at: Some(4),
                    event_time: None,
                    seq: Some(3),
                },
                user("q-m"),
                assistant("a-m"),
            ],
        )
        .unwrap();
        let factory = SessionFactory::test_factory(root.clone());

        // 1-based resolved index 2 = grand's "a0" (grand ordinal 1).
        let inherited = factory
            .build_fork(
                "chain-leaf-inherited",
                "chain-middle".into(),
                Some(2),
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the inherited-cut fork builds");
        drop(inherited);
        let stored = Session::load(&root, "chain-leaf-inherited")
            .unwrap()
            .entries;
        assert_eq!(
            stored,
            vec![SessionEntry::ForkedFrom {
                source: "chain-grand".into(),
                at: None,
                event_time: None,
                seq: Some(1),
            }],
            "an inherited cut belongs to the ancestor that stores the row"
        );
        let resolved = SessionStore::Jsonl
            .load_fork_located(&root, "chain-leaf-inherited")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 2);
        assert_eq!(resolved.entries[..2], [user("q0"), assistant("a0")]);

        // The default cut is middle's own last completed turn (resolved index
        // 7 = middle's own ordinal 2).
        let own = factory
            .build_fork(
                "chain-leaf-own",
                "chain-middle".into(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the own-cut fork builds");
        drop(own);
        let stored = Session::load(&root, "chain-leaf-own").unwrap().entries;
        assert_eq!(
            stored,
            vec![SessionEntry::ForkedFrom {
                source: "chain-middle".into(),
                at: None,
                event_time: None,
                seq: Some(2),
            }],
            "a cut in the child's own rows stays the child's"
        );
        let resolved = SessionStore::Jsonl
            .load_fork_located(&root, "chain-leaf-own")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 7);
        assert_eq!(resolved.entries[6], assistant("a-m"));
    }

    /// A fork OF a fork keeps a goal that sits BEFORE the compaction: the
    /// child's materialized context starts at the compaction, so folding the
    /// visible prefix would find nothing. The goal is resolved through the
    /// owning history at the cut (two generations: `A` compacted, `B` a fork
    /// of `A` whose own rows carry only the frozen goal).
    ///
    /// `A=[Goal(G), Compaction(C), Assistant(a)]`, `B=[Goal(G), pinned(A,a)]`;
    /// forking `B` at its inherited cut must still freeze `G`.
    async fn assert_two_generation_goal(goal: Option<crate::agent::GoalSnapshot>, cleared: bool) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let goal_row = SessionEntry::GoalUpdated { goal: goal.clone() };
        Session::append(
            &root,
            "gen-a",
            &[
                goal_row.clone(),
                compaction("gen summary"),
                assistant("gen a"),
            ],
        )
        .unwrap();
        let factory = SessionFactory::test_factory(root.clone());
        factory
            .build_fork(
                "gen-b",
                "gen-a".into(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the first generation builds");
        let b = Session::load(&root, "gen-b").unwrap().entries;
        assert_eq!(b[0], goal_row, "generation B froze the goal: {b:?}");
        assert!(matches!(
            b[1],
            SessionEntry::ForkedFrom {
                at: None,
                seq: Some(_),
                ..
            }
        ));

        // B resolves to [C, a, goal, marker]: the goal is AFTER the cut.
        let resolved_b = SessionStore::Jsonl
            .load_fork_located(&root, "gen-b")
            .await
            .unwrap();
        assert_eq!(resolved_b.inherited, 2, "the compaction and its turn");
        assert_eq!(resolved_b.entries[0], compaction("gen summary"));

        // Generation C: default cut = B's inherited cut.
        factory
            .build_fork(
                "gen-c",
                "gen-b".into(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the second generation builds");
        let c = Session::load(&root, "gen-c").unwrap().entries;
        assert_eq!(
            c[0], goal_row,
            "the two-generation fork must not lose the pre-compaction goal: {c:?}"
        );
        let (folded, folded_cleared) = Agent::fold_goal(
            &SessionStore::Jsonl
                .load_fork_located(&root, "gen-c")
                .await
                .unwrap()
                .entries,
        );
        assert_eq!(folded, goal);
        assert_eq!(folded_cleared, cleared);

        // An EXPLICIT inherited cut resolves the same way.
        factory
            .build_fork(
                "gen-d",
                "gen-b".into(),
                Some(2),
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the explicit inherited cut builds");
        let d = Session::load(&root, "gen-d").unwrap().entries;
        assert_eq!(
            d[0], goal_row,
            "an explicit inherited cut keeps the goal too: {d:?}"
        );
    }

    #[tokio::test]
    async fn two_generation_fork_keeps_a_pre_compaction_goal() {
        let goal = crate::agent::create_goal(
            None,
            "pre-compaction goal".into(),
            vec!["survives two generations".into()],
        )
        .unwrap();
        assert_two_generation_goal(Some(goal), false).await;
    }

    /// The cleared variant: the tombstone is preserved, never mistaken for
    /// "no goal at all".
    #[tokio::test]
    async fn two_generation_fork_keeps_a_pre_compaction_cleared_tombstone() {
        assert_two_generation_goal(None, true).await;
    }

    /// A goal the OWNER updated AFTER the cut is never inherited: the lookup is
    /// bounded by the cut row, not by the session's newest goal.
    #[tokio::test]
    async fn fork_goal_at_cut_ignores_a_later_goal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let early = crate::agent::create_goal(None, "early goal".into(), vec![]).unwrap();
        // A newer snapshot of the same goal (no lifecycle transition needed).
        let mut late = early.clone();
        late.revision += 1;
        late.objective = "late goal".into();
        // JSONL ordinals: 0 q0, 1 a0, 2 early goal, 3 q1, 4 a1, 5 late goal.
        Session::append(
            &root,
            "late-goal-src",
            &[
                user("q0"),
                assistant("a0"),
                SessionEntry::GoalUpdated {
                    goal: Some(early.clone()),
                },
                user("q1"),
                assistant("a1"),
                SessionEntry::GoalUpdated {
                    goal: Some(late.clone()),
                },
            ],
        )
        .unwrap();
        let factory = SessionFactory::test_factory(root.clone());
        factory
            .build_fork(
                "late-goal-fork",
                "late-goal-src".into(),
                Some(5),
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the explicit-cut fork builds");
        let stored = Session::load(&root, "late-goal-fork").unwrap().entries;
        assert_eq!(
            stored[0],
            SessionEntry::GoalUpdated { goal: Some(early) },
            "the goal at the cut wins over a later update: {stored:?}"
        );
    }

    /// Default creation on a COMPACTED SQL source reads only the physical
    /// head: the child's materialized context starts at the last compaction,
    /// never at the session start, and no pre-compaction row is loaded.
    #[tokio::test]
    async fn main_fork_on_a_compacted_sql_source_materializes_only_the_head() {
        use crate::session_store::SessionStore;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let backend = crate::config::SessionBackend::Sqlite {
            path: Some(
                temp.path()
                    .join("sessions.db")
                    .to_string_lossy()
                    .into_owned(),
            ),
        };
        let source = SessionStore::connect(&backend, &root, "sql-compacted-src")
            .await
            .unwrap();
        // 120 pre-compaction rows: a full load would return all of them.
        let mut rows: Vec<SessionEntry> = (0..60)
            .flat_map(|i| [user(&format!("old q{i}")), assistant(&format!("old a{i}"))])
            .collect();
        rows.push(compaction("sql summary"));
        rows.push(user("new q"));
        rows.push(assistant("new a"));
        source
            .append(&root, "sql-compacted-src", &rows)
            .await
            .unwrap();

        let factory = SessionFactory::test_factory_with_backend(root.clone(), backend.clone());
        let build = factory
            .build_fork(
                "sql-compacted-fork",
                "sql-compacted-src".into(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the compacted SQL fork builds");
        drop(build);

        let child = SessionStore::connect(&backend, &root, "sql-compacted-fork")
            .await
            .unwrap();
        let stored = child
            .load(&root, "sql-compacted-fork")
            .await
            .unwrap()
            .entries;
        assert_eq!(stored.len(), 1, "marker only: {stored:?}");
        assert!(matches!(
            stored[0],
            SessionEntry::ForkedFrom {
                at: None,
                seq: Some(_),
                ..
            }
        ));
        // The child's own load is head-oriented too: exactly the compaction
        // and the two rows after it.
        let loaded = child
            .load_fork_head_located(&root, "sql-compacted-fork")
            .await
            .unwrap();
        assert_eq!(loaded.inherited, 3, "compaction + new q + new a");
        assert_eq!(loaded.entries[0], compaction("sql summary"));
        assert_eq!(loaded.entries.len(), 4, "context + the child's own marker");
        assert!(
            !loaded
                .entries
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Message {
                    message: crate::agent::Message::User { content, .. }
                } if content.starts_with("old q")))
        );
        // Older audit rows remain reachable on demand.
        let (owner, cursor) = loaded.next_older.clone().expect("an older route");
        assert_eq!(owner, "sql-compacted-src");
        let older = child
            .load_fork_older_located(&root, &owner, cursor, Some(4))
            .await
            .unwrap();
        assert_eq!(older.entries.len(), 4, "the newest pre-compaction rows");
        assert!(matches!(
            &older.entries[3],
            SessionEntry::Message {
                message: crate::agent::Message::Assistant(_)
            }
        ));
    }

    /// Forking an UNCOMPACTED zero-copy child that has its own completed turn
    /// materializes BOTH its inherited context and its own rows: the fresh seed
    /// a producer computes and the marker-based reload must be identical.
    #[tokio::test]
    async fn fork_of_an_uncompacted_fork_matches_its_reload() {
        use crate::session_store::SessionStore;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(&root, "seed-grand", &[user("g q"), assistant("g a")]).unwrap();
        // B: marker-first, uncompacted, with its own completed turn.
        Session::append(
            &root,
            "seed-b",
            &[
                SessionEntry::ForkedFrom {
                    source: "seed-grand".into(),
                    at: None,
                    event_time: None,
                    seq: Some(1),
                },
                user("b q"),
                assistant("b a"),
            ],
        )
        .unwrap();

        let cut = SessionStore::Jsonl
            .default_fork_cut(&root, "seed-b")
            .await
            .expect("the uncompacted fork has a cut");
        // The cut stays B's own boundary…
        assert_eq!(cut.owner, "seed-b");
        assert_eq!(cut.key.seq(), 2);
        // …but the context it materializes includes grand's prefix.
        assert_eq!(cut.inherited, Some(2));
        assert_eq!(
            cut.context,
            vec![
                user("g q"),
                assistant("g a"),
                SessionEntry::ForkedFrom {
                    source: "seed-grand".into(),
                    at: None,
                    event_time: None,
                    seq: Some(1),
                },
                user("b q"),
                assistant("b a"),
            ],
            "the fresh seed must carry the inherited context"
        );

        let factory = SessionFactory::test_factory(root.clone());
        factory
            .build_fork(
                "seed-c",
                "seed-b".into(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the fork of an uncompacted fork builds");
        let reloaded = SessionStore::Jsonl
            .load_fork_located(&root, "seed-c")
            .await
            .unwrap();
        assert_eq!(reloaded.inherited, cut.context.len());
        assert_eq!(
            reloaded.entries[..reloaded.inherited],
            cut.context[..],
            "fresh seed and reload must agree"
        );
    }

    /// Head-only RESUME must not lose a goal (or its clear tombstone) that
    /// precedes the newest compaction: the projection is restored from the
    /// ancestry, without fabricating a history entry. Ordinary and fork
    /// sessions both.
    async fn assert_resume_keeps_goal(goal: Option<crate::agent::GoalSnapshot>, cleared: bool) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let goal_row = SessionEntry::GoalUpdated { goal: goal.clone() };
        // Ordinary session: the goal (or tombstone) sits BEFORE the compaction.
        Session::append(
            &root,
            "resume-goal-src",
            &[
                goal_row.clone(),
                compaction("resume summary"),
                user("q0"),
                assistant("a0"),
            ],
        )
        .unwrap();
        let factory = SessionFactory::test_factory(root.clone());
        let build = factory
            .build(
                "resume-goal-src",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the ordinary resume builds");
        let (resumed_goal, resumed_cleared) = build.runner.goal_projection_for_test();
        assert_eq!(resumed_goal, goal, "ordinary resume keeps the goal");
        assert_eq!(resumed_cleared, cleared);
        drop(build);

        // The same goal in a FORK child that later compacted on its own: the
        // frozen goal leads the child's own rows, and the child's own
        // compaction hides it from a head-only load.
        Session::append(&root, "resume-grand", &[user("g q"), assistant("g a")]).unwrap();
        Session::append(
            &root,
            "resume-fork",
            &[
                goal_row.clone(),
                SessionEntry::ForkedFrom {
                    source: "resume-grand".into(),
                    at: None,
                    event_time: None,
                    seq: Some(1),
                },
                user("c q"),
                assistant("c a"),
                compaction("child summary"),
                user("c q2"),
                assistant("c a2"),
            ],
        )
        .unwrap();
        let build = factory
            .build(
                "resume-fork",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the fork resume builds");
        let (resumed_goal, resumed_cleared) = build.runner.goal_projection_for_test();
        assert_eq!(resumed_goal, goal, "fork resume keeps the goal");
        assert_eq!(resumed_cleared, cleared);
        // The persisted history is untouched: only the projection was restored.
        let stored = Session::load(&root, "resume-fork").unwrap().entries;
        assert_eq!(stored[0], goal_row);
        assert_eq!(stored.len(), 7, "no entry fabricated: {stored:?}");
    }

    #[tokio::test]
    async fn head_only_resume_keeps_a_pre_compaction_goal() {
        let goal =
            crate::agent::create_goal(None, "resumed goal".into(), vec!["kept".into()]).unwrap();
        assert_resume_keeps_goal(Some(goal), false).await;
    }

    #[tokio::test]
    async fn head_only_resume_keeps_a_pre_compaction_cleared_tombstone() {
        assert_resume_keeps_goal(None, true).await;
    }

    /// A source with no goal at the cut writes no `GoalUpdated`: the child's
    /// store is the marker alone.
    #[tokio::test]
    async fn main_fork_without_a_goal_writes_only_the_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(
            &root,
            "plain-src",
            &[user("q0"), assistant("a0"), user("q1"), assistant("a1")],
        )
        .unwrap();
        let factory = SessionFactory::test_factory(root.clone());
        let build = factory
            .build_fork(
                "plain-fork",
                "plain-src".into(),
                Some(2),
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the main fork builds");
        drop(build);
        let stored = Session::load(&root, "plain-fork").unwrap().entries;
        assert_eq!(
            stored,
            vec![SessionEntry::ForkedFrom {
                source: "plain-src".into(),
                at: None,
                event_time: None,
                seq: Some(1),
            }],
            "an explicit 1-based `at` is resolved, then pinned by row"
        );
    }
}

#[cfg(test)]
mod user_input_gate_tests {
    use super::*;

    /// Only the root/main session may suspend on `request_user_input`: the
    /// factory installs the gate for `build`, while the delegate/btw/fork spawn
    /// paths construct their runner without it, so their model gets a tool
    /// error instead of parking a turn nobody can answer.
    #[tokio::test]
    async fn only_the_root_session_may_park_for_user_input() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        let build = factory
            .build(
                "web-main",
                None,
                None,
                IdlePolicy::WaitForInput,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the root session builds");
        assert!(
            build.runner.user_input_allowed_for_test(),
            "a root session built by the factory must answer request_user_input"
        );

        // The child shape: `Delegate::spawn`/`spawn_fork` and
        // `spawn_btw_subagent` build exactly this runner (same constructor,
        // same `FinishWhenIdle` policy) and never enable the gate.
        let model = ConfiguredModel::chat(
            OpenAiModel::new(
                "http://localhost".into(),
                "test-key".into(),
                "child-model".into(),
                None,
            )
            .expect("test child model"),
        );
        let store = SessionStore::connect(
            &crate::config::SessionBackend::Jsonl,
            temp.path(),
            "sub-test",
        )
        .await
        .expect("the child store connects");
        let (child, child_handle) = SessionRunner::new_with_bootstrap(
            Agent::new(Box::new(model), Vec::new()),
            store,
            temp.path().to_path_buf(),
            "sub-test".to_owned(),
            IdlePolicy::FinishWhenIdle,
            SessionBootstrap {
                recovery_tasks: Vec::new(),
                legacy: false,
                initial_entries: Vec::new(),
            },
        )
        .await
        .expect("the child session builds");
        assert!(
            !child.user_input_allowed_for_test(),
            "a delegate/btw/fork runner must never park for user input"
        );
        drop(child_handle);
    }
}

/// Main-agent-only `generate_image` registration: the decision function the
/// session build calls, plus the lazy shared ChatGPT-auth handle.
#[cfg(test)]
mod generate_image_registration_tests {
    use super::*;

    fn tool_names(tools: &[Box<dyn crate::agent::Tool>]) -> Vec<String> {
        tools.iter().map(|tool| tool.spec().name).collect()
    }

    #[test]
    fn main_session_registers_generate_image_read_only_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let auth: AuthResolver = Arc::new(|| unreachable!("registration resolves no auth"));
        let mut main = Vec::new();
        register_generate_image(&mut main, &workspace, auth.clone(), false, None);
        assert_eq!(tool_names(&main), vec!["generate_image"]);
        assert_eq!(main[0].spec().name, "generate_image");
        let mut read_only = Vec::new();
        register_generate_image(&mut read_only, &workspace, auth, true, None);
        assert!(read_only.is_empty(), "read-only sessions must not see it");
    }

    /// A workspace rooted AT or BELOW the canonical global-skills directory
    /// must register NO `generate_image`: the read-only skills entry only
    /// wins for a strict child, so there an ordinary output path resolves as
    /// the writable workspace entry and would publish into global skills. A
    /// sibling workspace (and a build with no global skills root) keeps the
    /// tool. The auth resolver is `unreachable!`: registration must not
    /// resolve it on any of these paths.
    #[test]
    fn workspace_at_or_below_the_global_skills_dir_registers_no_generate_image() {
        let temp = tempfile::tempdir().unwrap();
        let skills = temp.path().join("skills");
        std::fs::create_dir(&skills).unwrap();
        let below = skills.join("nested");
        std::fs::create_dir(&below).unwrap();
        let sibling = temp.path().join("sibling");
        std::fs::create_dir(&sibling).unwrap();
        let protected = crate::canonicalize_path(&skills).unwrap();
        let auth =
            || -> AuthResolver { Arc::new(|| unreachable!("registration must not resolve auth")) };

        for root in [&skills, &below] {
            let mut tools = Vec::new();
            register_generate_image(
                &mut tools,
                &Workspace::new(root).unwrap(),
                auth(),
                false,
                Some(&protected),
            );
            assert!(
                tools.is_empty(),
                "{} must not expose generate_image",
                root.display()
            );
        }

        // A sibling workspace is outside the protected root: still registered.
        let mut sibling_tools = Vec::new();
        register_generate_image(
            &mut sibling_tools,
            &Workspace::new(&sibling).unwrap(),
            auth(),
            false,
            Some(&protected),
        );
        assert_eq!(tool_names(&sibling_tools), vec!["generate_image"]);

        // No global skills root (or a build without one) is unchanged.
        let mut unprotected = Vec::new();
        register_generate_image(
            &mut unprotected,
            &Workspace::new(&skills).unwrap(),
            auth(),
            false,
            None,
        );
        assert_eq!(tool_names(&unprotected), vec!["generate_image"]);
    }

    #[tokio::test]
    async fn generate_image_auth_shares_the_reloadable_login_cache() {
        let temp = tempfile::tempdir().unwrap();
        let factory = SessionFactory::test_factory(temp.path().to_path_buf());
        let fixture = CodexAuth::test_auth(temp.path().join("auth.json"));
        factory.reloadable.write().unwrap().auth = Some(fixture);
        let resolver = factory.generate_image_auth();
        let auth = resolver().expect("cached login");
        // The fixture handle: no disk read, no network, no refresh.
        assert_eq!(
            auth.current_access_token_and_account().await.unwrap(),
            ("access".to_owned(), "account".to_owned())
        );
    }

    /// Red-before-fix regression: [`skills_root_is_dir`] only rules out a
    /// symlinked `skills` directory itself, never a symlinked PARENT. With a
    /// parent-symlink alias the real skills directory can sit inside the
    /// workspace, whose entry is writable, and only the configured logical
    /// path (an alias the file tools match lexically) would be read-only. The
    /// canonical path must be installed as its own read-only entry, so the
    /// factory-registered `generate_image` refuses the real directory before
    /// auth; the shared resolver here is `unreachable!`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn main_tools_workspace_protects_the_canonical_global_skills_path() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_skills = temp.path().join("real/skills/demo");
        std::fs::create_dir_all(&real_skills).unwrap();
        std::fs::write(real_skills.join("SKILL.md"), "# demo").unwrap();
        // A parent-component alias: the configured logical skills path is
        // `<ws>/alias/skills`, the real directory `<ws>/real/skills`.
        symlink(temp.path().join("real"), temp.path().join("alias")).unwrap();
        let logical = temp.path().join("alias/skills");
        assert!(
            skills_root_is_dir(&logical),
            "the alias itself is a real directory"
        );

        let tools_workspace = main_tools_workspace(
            Workspace::new(temp.path()).unwrap(),
            &Sandbox::default(),
            Some(&logical),
        )
        .expect("the skills workspace builds")
        .expect("a skills root is installed");
        let mut tools = Vec::new();
        register_generate_image(
            &mut tools,
            &tools_workspace,
            Arc::new(|| unreachable!("the canonical skills path must be refused before auth")),
            false,
            Some(&crate::canonicalize_path(&logical).unwrap()),
        );
        let tool = tools.first().expect("generate_image is registered");

        // The canonical path names the real directory with no symlinked
        // parent, so only the canonical read-only entry can refuse it.
        let error = tool
            .execute(serde_json::json!({"prompt": "x", "path": "real/skills/out.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("inside the workspace"), "{error}");
        assert!(!temp.path().join("real/skills/out.png").exists());
        // The logical alias path stays refused through its read-only entry
        // (or the symlinked-parent gate) and never publishes.
        assert!(
            tool.execute(serde_json::json!({"prompt": "x", "path": "alias/skills/out.png"}))
                .await
                .is_err()
        );
        assert!(!temp.path().join("real/skills/out.png").exists());

        // A root that canonicalizes to the configured path keeps exactly the
        // same read-only protection (the deduplicated branch).
        let plain = temp.path().join("plain-skills");
        std::fs::create_dir_all(&plain).unwrap();
        let plain_workspace = main_tools_workspace(
            Workspace::new(temp.path()).unwrap(),
            &Sandbox::default(),
            Some(&plain),
        )
        .expect("the skills workspace builds")
        .expect("a skills root is installed");
        let mut plain_tools = Vec::new();
        register_generate_image(
            &mut plain_tools,
            &plain_workspace,
            Arc::new(|| unreachable!("a read-only skills dir must be refused before auth")),
            false,
            Some(&crate::canonicalize_path(&plain).unwrap()),
        );
        let error = plain_tools[0]
            .execute(serde_json::json!({"prompt": "x", "path": "plain-skills/out.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("inside the workspace"), "{error}");
        assert!(!plain.join("out.png").exists());
    }

    /// Fail closed: if the global skills directory cannot be canonicalized
    /// (e.g. it vanished after the [`skills_root_is_dir`] filter), the
    /// workspace construction errors instead of silently dropping the
    /// read-only entry.
    #[test]
    fn main_tools_workspace_fails_closed_when_canonicalization_fails() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-skills");
        assert!(
            main_tools_workspace(
                Workspace::new(temp.path()).unwrap(),
                &Sandbox::default(),
                Some(&missing),
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod link_ref_registration_tests {
    use super::*;
    use crate::agent::SessionEntry;
    use crate::session::Session;

    fn user(text: &str) -> SessionEntry {
        crate::agent::Message::User {
            content: text.to_owned(),
            images: vec![],
        }
        .into()
    }

    fn assistant(text: &str) -> SessionEntry {
        crate::agent::Message::Assistant(crate::agent::AssistantMessage {
            content: Some(text.to_owned()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into()
    }

    fn fork_marker(source: &str, at: usize, seq: i64) -> SessionEntry {
        SessionEntry::ForkedFrom {
            source: source.to_owned(),
            at: Some(at),
            event_time: None,
            seq: Some(seq),
        }
    }

    /// The main role template resolves through the process-global
    /// `XDG_CONFIG_HOME`/`HOME` layers, and another test parks a regular file
    /// at `$XDG_CONFIG_HOME/e-agent` to prove a rejected reload. A reopen
    /// that builds more than once must hold the same env lock the mutators
    /// use, or that transient state fails an unrelated build.
    fn lock_global_config_env() -> std::sync::MutexGuard<'static, ()> {
        crate::roles::XDG_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Provenance must cover every fork layout that exists on disk, not just
    /// the zero-copy leading marker: zero-copy children store the marker
    /// first (optionally after the frozen goal), while the older copy-based
    /// layout stores the copied prefix BEFORE the marker. Reopening each by
    /// id (no `fork_from` in the build request) must still keep `link_ref`
    /// off; a session the human started from scratch keeps it.
    #[tokio::test]
    async fn every_persisted_fork_layout_reopens_without_link_ref() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let source = [user("q0"), assistant("a0"), user("q1"), assistant("a1")];
        Session::append(&root, "layout-src", &source).unwrap();
        // Zero-copy: marker alone, and marker after the child's frozen goal.
        Session::append(
            &root,
            "layout-zero",
            std::slice::from_ref(&fork_marker("layout-src", 4, 3)),
        )
        .unwrap();
        Session::append(
            &root,
            "layout-goal",
            &[
                SessionEntry::GoalUpdated { goal: None },
                fork_marker("layout-src", 4, 3),
            ],
        )
        .unwrap();
        // Older copy-based layout: two and four stored prefix entries before
        // the marker (the four-entry one is the layout this repo actually
        // still carries), plus the four-ordinary-turn case the reviewer
        // constructed — the marker sits NINTH, behind the whole selected
        // prefix, so a fixed head can never be the rule.
        Session::append(
            &root,
            "layout-copy2",
            &[
                source[0].clone(),
                source[1].clone(),
                fork_marker("layout-src", 2, 1),
            ],
        )
        .unwrap();
        Session::append(
            &root,
            "layout-copy4",
            &[
                source[0].clone(),
                source[1].clone(),
                source[2].clone(),
                source[3].clone(),
                fork_marker("layout-src", 4, 3),
            ],
        )
        .unwrap();
        let source4 = [
            user("q0"),
            assistant("a0"),
            user("q1"),
            assistant("a1"),
            user("q2"),
            assistant("a2"),
            user("q3"),
            assistant("a3"),
        ];
        Session::append(&root, "layout-src4", &source4).unwrap();
        let mut copy8 = source4.to_vec();
        copy8.push(fork_marker("layout-src4", source4.len(), 7));
        Session::append(&root, "layout-copy8", &copy8).unwrap();

        let factory = SessionFactory::test_factory(root.clone());
        for id in [
            "layout-zero",
            "layout-goal",
            "layout-copy2",
            "layout-copy4",
            "layout-copy8",
        ] {
            let reopened = factory
                .build(
                    id,
                    None,
                    None,
                    IdlePolicy::FinishWhenIdle,
                    UnfinishedPolicy::Preserve,
                )
                .await
                .unwrap_or_else(|error| panic!("{id} reopens: {error:#}"));
            let names = reopened.runner.tool_names_for_test();
            assert!(
                !names.iter().any(|name| name == "link_ref"),
                "{id} must stay recognizable as a fork: {names:?}"
            );
        }

        Session::append(&root, "layout-plain", &[user("q0"), assistant("a0")]).unwrap();
        let plain = factory
            .build(
                "layout-plain",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("normal session builds");
        let names = plain.runner.tool_names_for_test();
        assert!(
            names.iter().any(|name| name == "link_ref"),
            "a user-created top-level session still gets link_ref: {names:?}"
        );

        // A LONG ordinary session is not a fork either: provenance must not
        // mistake depth for a marker. Its build still registers link_ref.
        let long: Vec<SessionEntry> = (0..500)
            .flat_map(|i| [user(&format!("q{i}")), assistant(&format!("a{i}"))])
            .collect();
        Session::append(&root, "layout-long", &long).unwrap();
        let long = factory
            .build(
                "layout-long",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("a long ordinary session builds");
        let names = long.runner.tool_names_for_test();
        assert!(
            names.iter().any(|name| name == "link_ref"),
            "a long ordinary session still gets link_ref: {names:?}"
        );
    }

    /// A resumed CHILD session (delegate/btw) records its parent in the
    /// metadata row. Provenance takes that answer without reading the
    /// transcript, so a child keeps building without `link_ref` even though
    /// its own log carries no `ForkedFrom` marker.
    #[tokio::test]
    async fn a_resumed_child_session_takes_its_metadata_parent_as_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(&root, "child-session", &[user("q0"), assistant("a0")]).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "child-session",
                Some("m"),
                Some("subagent"),
                Some("parent-session"),
                Some(7),
                None,
            )
            .await
            .unwrap();
        let factory = SessionFactory::test_factory(root.clone());
        let reopened = factory
            .build(
                "child-session",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the child session builds");
        let names = reopened.runner.tool_names_for_test();
        assert!(
            !names.iter().any(|name| name == "link_ref"),
            "a persisted parent link is provenance: {names:?}"
        );
    }

    /// Delete the closing brace of the sidecar's LAST line: a crash artifact
    /// that still reads, but no longer decodes.
    fn tear_last_sidecar_line(root: &Path, session: &str) {
        let path = root
            .join(".e-agent/sessions")
            .join(format!("{session}.meta.jsonl"));
        let sidecar = std::fs::read_to_string(&path).unwrap();
        let torn = sidecar
            .strip_suffix("}\n")
            .unwrap_or_else(|| panic!("{session} sidecar has no complete last line: {sidecar:?}"));
        std::fs::write(&path, format!("{torn}\n")).unwrap();
    }

    /// A TORN metadata tail must not stop a session from opening — the last
    /// good snapshot is enough for display — but it is not evidence of "no
    /// parent". Provenance is unknown, so a child whose parent-bearing
    /// snapshot was truncated must not regain `link_ref`, and an older
    /// parentless line followed by a torn parent update is just as unknown.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_torn_sidecar_tail_never_proves_parentlessness() {
        let _env = lock_global_config_env();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let history = [user("q0"), assistant("a0")];

        Session::append(&root, "torn-child", &history).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "torn-child",
                Some("m"),
                Some("subagent"),
                Some("parent-session"),
                Some(7),
                None,
            )
            .await
            .unwrap();
        tear_last_sidecar_line(&root, "torn-child");

        Session::append(&root, "torn-update-child", &history).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "torn-update-child",
                Some("m"),
                Some("subagent"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "torn-update-child",
                Some("m"),
                Some("subagent"),
                Some("parent-session"),
                Some(7),
                None,
            )
            .await
            .unwrap();
        tear_last_sidecar_line(&root, "torn-update-child");

        let factory = SessionFactory::test_factory(root.clone());
        for id in ["torn-child", "torn-update-child"] {
            let path = root
                .join(".e-agent/sessions")
                .join(format!("{id}.meta.jsonl"));
            let torn_bytes = std::fs::read(&path).unwrap();
            for build in 1..=2 {
                let reopened = factory
                    .build(
                        id,
                        None,
                        None,
                        IdlePolicy::FinishWhenIdle,
                        UnfinishedPolicy::Preserve,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{id} build {build} still opens: {error:#}"));
                assert_eq!(
                    Session::load(&root, id).unwrap().entries,
                    history,
                    "{id} build {build} reopens with its history intact"
                );
                let names = reopened.runner.tool_names_for_test();
                assert!(
                    !names.iter().any(|name| name == "link_ref"),
                    "{id} build {build} has unknown provenance, so link_ref must stay off: {names:?}"
                );
                drop(reopened);
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    torn_bytes,
                    "{id} build {build} never rewrites the torn sidecar"
                );
            }
        }
    }

    /// A torn tail is not special-cased into "fork": an ordinary session
    /// (no parent anywhere) still opens on its last good snapshot and keeps
    /// `link_ref` off, because surviving the crash does not prove it is
    /// parentless.
    #[tokio::test]
    async fn a_torn_sidecar_tail_on_an_ordinary_session_still_opens() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let history = [user("q0"), assistant("a0")];
        Session::append(&root, "torn-ordinary", &history).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "torn-ordinary",
                Some("m"),
                Some("main"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        tear_last_sidecar_line(&root, "torn-ordinary");

        let factory = SessionFactory::test_factory(root.clone());
        let reopened = factory
            .build(
                "torn-ordinary",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("an ordinary session still opens after a torn sidecar tail");
        assert_eq!(
            Session::load(&root, "torn-ordinary").unwrap().entries,
            history,
            "the torn sidecar never rewrites the transcript"
        );
        let names = reopened.runner.tool_names_for_test();
        assert!(
            !names.iter().any(|name| name == "link_ref"),
            "unknown provenance fails closed: {names:?}"
        );
    }

    /// The writer creates the sidecar file BEFORE appending its first
    /// snapshot, so an interruption between those two operations leaves a
    /// zero-byte sidecar. That is an interrupted first write, not evidence
    /// of "no parent": the session must still open with its history, and a
    /// markerless child must stay without `link_ref`. Every reopen must also
    /// leave the sidecar untouched — a first record written over the crash
    /// would turn unknown provenance into a parentless one on the next read.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_zero_byte_sidecar_is_unknown_not_parentless() {
        let _env = lock_global_config_env();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let history = [user("q0"), assistant("a0")];
        Session::append(&root, "empty-sidecar-child", &history).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "empty-sidecar-child",
                Some("m"),
                Some("subagent"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        // The crash: the file exists (created before the append) but its
        // first snapshot never landed.
        let path = root.join(".e-agent/sessions/empty-sidecar-child.meta.jsonl");
        std::fs::write(&path, "").unwrap();

        let factory = SessionFactory::test_factory(root.clone());
        for build in 1..=3 {
            let reopened = factory
                .build(
                    "empty-sidecar-child",
                    None,
                    None,
                    IdlePolicy::FinishWhenIdle,
                    UnfinishedPolicy::Preserve,
                )
                .await
                .unwrap_or_else(|error| panic!("build {build} must open: {error:#}"));
            assert_eq!(
                Session::load(&root, "empty-sidecar-child").unwrap().entries,
                history,
                "build {build}: the transcript still loads"
            );
            let names = reopened.runner.tool_names_for_test();
            assert!(
                !names.iter().any(|name| name == "link_ref"),
                "build {build}: an existing but empty sidecar is unknown provenance: {names:?}"
            );
            drop(reopened);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"",
                "build {build}: the unknown sidecar is never rewritten into a parentless record"
            );
        }
    }

    /// Truncate the sidecar inside a multi-byte character of the title. A
    /// `lines()` reader would fail the whole reopen with `InvalidData`; the
    /// crash only damaged the metadata tail, so the session must still open
    /// with its history, and the undecodable record is unknown provenance
    /// (no `link_ref` for a markerless child).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_sidecar_torn_inside_a_multibyte_character_still_opens() {
        let _env = lock_global_config_env();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let history = [user("q0"), assistant("a0")];
        Session::append(&root, "torn-multibyte-child", &history).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "torn-multibyte-child",
                Some("m"),
                Some("subagent"),
                None,
                None,
                Some("crash 🧵 tail"),
            )
            .await
            .unwrap();

        let path = root.join(".e-agent/sessions/torn-multibyte-child.meta.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let scissor = "🧵".as_bytes();
        let at = bytes
            .windows(scissor.len())
            .position(|window| window == scissor)
            .expect("the snapshot title carries the multi-byte character");
        // Keep ONE byte of the four-byte character: the file ends mid-char,
        // so it is not valid UTF-8 and no JSON value decodes.
        let torn = &bytes[..at + 1];
        assert!(
            std::str::from_utf8(torn).is_err(),
            "the test must actually tear a multi-byte character"
        );
        std::fs::write(&path, torn).unwrap();
        let torn_bytes = std::fs::read(&path).unwrap();

        let factory = SessionFactory::test_factory(root.clone());
        for build in 1..=2 {
            let reopened = factory
                .build(
                    "torn-multibyte-child",
                    None,
                    None,
                    IdlePolicy::FinishWhenIdle,
                    UnfinishedPolicy::Preserve,
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("build {build} of a mid-character tear must open: {error:#}")
                });
            assert_eq!(
                Session::load(&root, "torn-multibyte-child")
                    .unwrap()
                    .entries,
                history,
                "build {build}: the intact transcript opens despite the torn metadata"
            );
            let names = reopened.runner.tool_names_for_test();
            assert!(
                !names.iter().any(|name| name == "link_ref"),
                "build {build}: an undecodable record is unknown provenance: {names:?}"
            );
            drop(reopened);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                torn_bytes,
                "build {build}: the torn sidecar is never rewritten"
            );
        }
    }

    /// The positive case: a sidecar whose lines ALL decode to "no parent" is
    /// evidence, so an ordinary session keeps `link_ref`. A transcript
    /// marker still overrides that answer.
    #[tokio::test]
    async fn a_fully_decodable_parentless_session_keeps_link_ref() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(&root, "decoded-plain", &[user("q0"), assistant("a0")]).unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "decoded-plain",
                Some("m"),
                Some("main"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        // The copy-based fork layout: the marker sits behind stored prefix
        // entries, and a parentless sidecar row is all the metadata says.
        Session::append(
            &root,
            "decoded-marker",
            &[
                user("q0"),
                assistant("a0"),
                fork_marker("decoded-source", 2, 1),
            ],
        )
        .unwrap();
        SessionStore::Jsonl
            .create_meta(
                &root,
                "decoded-marker",
                Some("m"),
                Some("main"),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let factory = SessionFactory::test_factory(root.clone());
        let plain = factory
            .build(
                "decoded-plain",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the decoded parentless session builds");
        let names = plain.runner.tool_names_for_test();
        assert!(
            names.iter().any(|name| name == "link_ref"),
            "a fully decoded parentless session keeps link_ref: {names:?}"
        );

        let fork = factory
            .build(
                "decoded-marker",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the marker-bearing session builds");
        let names = fork.runner.tool_names_for_test();
        assert!(
            !names.iter().any(|name| name == "link_ref"),
            "a transcript marker still counts as a fork: {names:?}"
        );
    }

    /// `link_ref` writes to THIS session's metadata row, so only a session the
    /// human started from scratch gets it. A web fork goes through the same
    /// factory path as a CLI fork (`build_fork` → `build_internal`) and IS a
    /// new top-level main session, but it continues the SOURCE's transcript:
    /// its links belong to the source, so it must not be able to relink them.
    #[tokio::test]
    async fn forked_sessions_are_built_without_link_ref() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(&root, "refs-src", &[user("q0"), assistant("a0")]).unwrap();
        let factory = SessionFactory::test_factory(root.clone());

        let forked = factory
            .build_fork(
                "refs-fork",
                "refs-src".to_owned(),
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the web fork builds");
        let names = forked.runner.tool_names_for_test();
        assert!(
            !names.iter().any(|name| name == "link_ref"),
            "a fork must not relink the source it continues: {names:?}"
        );
        drop(forked);

        // Reopen the SAME fork by id, the way a web resume/attach does: no
        // `fork_from` is available any more, so only the persisted
        // `ForkedFrom` marker can keep `link_ref` off.
        let reopened = factory
            .build(
                "refs-fork",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the persisted fork resumes");
        let names = reopened.runner.tool_names_for_test();
        assert!(
            !names.iter().any(|name| name == "link_ref"),
            "a resumed fork must not regain link_ref: {names:?}"
        );
        drop(reopened);

        let fresh = factory
            .build(
                "refs-main",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
            .expect("the plain top-level session builds");
        let names = fresh.runner.tool_names_for_test();
        assert!(
            names.iter().any(|name| name == "link_ref"),
            "a user-created top-level session keeps link_ref: {names:?}"
        );
    }

    /// A reopen must answer fork provenance from the SAME JSONL decode that
    /// builds the history head. The ninth-entry fix first added a separate
    /// full scan here, so a long transcript was decoded twice on every
    /// reopen. The probe counts both entry points for sessions named
    /// `once-…` (this test only): the head decode runs once and the separate
    /// marker scan never runs, while the fork/ordinary answer stays right.
    #[tokio::test]
    async fn a_reopen_decodes_a_large_transcript_once_for_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let turns: Vec<SessionEntry> = (0..500)
            .flat_map(|i| [user(&format!("q{i}")), assistant(&format!("a{i}"))])
            .collect();
        Session::append(&root, "once-ordinary", &turns).unwrap();
        // The legacy copy layout: the marker sits behind the whole copied
        // prefix, so only a whole-log answer can recognize it.
        let prefix = turns[..8].to_vec();
        Session::append(&root, "once-source", &prefix).unwrap();
        let mut copy = prefix.clone();
        copy.push(fork_marker("once-source", prefix.len(), 7));
        Session::append(&root, "once-fork", &copy).unwrap();

        let factory = SessionFactory::test_factory(root.clone());
        for (id, expected_link_ref) in [("once-ordinary", true), ("once-fork", false)] {
            crate::session::reopen_decode_probe::reset();
            let built = factory
                .build(
                    id,
                    None,
                    None,
                    IdlePolicy::FinishWhenIdle,
                    UnfinishedPolicy::Preserve,
                )
                .await
                .unwrap_or_else(|error| panic!("{id} reopens: {error:#}"));
            let (loads, scans) = crate::session::reopen_decode_probe::counts();
            assert_eq!(
                loads, 1,
                "{id} must decode the transcript once for history + provenance"
            );
            assert_eq!(
                scans, 0,
                "{id} must not run a second provenance scan over the transcript"
            );
            let names = built.runner.tool_names_for_test();
            assert_eq!(
                names.iter().any(|name| name == "link_ref"),
                expected_link_ref,
                "{id}: {names:?}"
            );
            drop(built);
        }
    }

    /// An unreadable parent sidecar must never be read as "no parent": that
    /// fall-through is what re-enabled `link_ref` on a child. The reopen
    /// surfaces the sidecar error instead (so no session/tool is built).
    #[tokio::test]
    async fn an_unreadable_sidecar_fails_the_reopen_instead_of_enabling_link_ref() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        Session::append(&root, "unreadable-child", &[user("q0"), assistant("a0")]).unwrap();
        // A directory at the sidecar path is openable but not readable on
        // Unix, and refused at open on Windows: both are non-NotFound.
        std::fs::create_dir(root.join(".e-agent/sessions/unreadable-child.meta.jsonl")).unwrap();

        let factory = SessionFactory::test_factory(root.clone());
        let error = match factory
            .build(
                "unreadable-child",
                None,
                None,
                IdlePolicy::FinishWhenIdle,
                UnfinishedPolicy::Preserve,
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("an unreadable sidecar is not an ordinary session"),
        };
        let text = format!("{error:#}");
        assert!(
            text.contains("unreadable-child.meta.jsonl"),
            "the sidecar read error is surfaced by name: {text}"
        );
    }
}
