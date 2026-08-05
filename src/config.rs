use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use crossterm::event::KeyModifiers;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct Config {
    default: Option<String>,
    #[serde(default)]
    providers: HashMap<String, Provider>,
    #[serde(default)]
    models: HashMap<String, ModelProfile>,
    /// MCP server definitions (`[mcp.<name>]`), shared with mcp.rs.
    #[serde(default)]
    pub mcp: HashMap<String, crate::mcp::McpServerConfig>,
    /// Built-in role -> model profile routing (`[roles]`). Roles fall back
    /// to the main profile when not routed. Currently only `subagent`
    /// exists; new roles are added where they are spawned.
    #[serde(default)]
    roles: HashMap<String, String>,
    /// Optional `[web_search]` credentials for the Exa `web_search` tool.
    #[serde(default)]
    web_search: Option<WebSearch>,
    /// Optional `[sandbox]` policy shared by bash mounts and file tools.
    #[serde(default)]
    sandbox: Option<Sandbox>,
    /// Optional `[session]` backend configuration.
    #[serde(default)]
    session: Option<SessionConfig>,
    /// Optional `[background]` policy (background-task timeout).
    #[serde(default)]
    background: Option<BackgroundConfig>,
    /// Optional `[bash]` policy (foreground bash timeout).
    #[serde(default)]
    bash: Option<BashConfig>,
    /// Optional `[tui]` submit/newline key mapping. Global-only: key
    /// bindings are personal preference and are never merged from project
    /// configs (see `merged_with_project`).
    #[serde(default)]
    tui: Option<TuiConfig>,
    #[serde(skip)]
    path: PathBuf,
}

/// Background-task policy, from `[background]`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct BackgroundConfig {
    /// Background bash timeout in seconds. `0` disables the timeout
    /// (runs until the task finishes or is cancelled). Absent defaults
    /// to [`DEFAULT_BACKGROUND_TIMEOUT_SECS`].
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Default background-task timeout: 30 minutes.
pub const DEFAULT_BACKGROUND_TIMEOUT_SECS: u64 = 30 * 60;

/// Foreground bash tool policy, from `[bash]`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct BashConfig {
    /// Foreground bash timeout in seconds. `0` disables the timeout
    /// (runs until the command finishes). Absent defaults to
    /// [`DEFAULT_BASH_TIMEOUT_SECS`].
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Default foreground bash timeout: 30 seconds.
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 30;

/// The `[tui]` config section: submit/newline key mapping for the TUI
/// input boxes. Only Enter variants are supported; each field accepts
/// `enter`, `alt+enter` (or its macOS alias `option+enter`), `ctrl+enter`
/// or `shift+enter`, matched exactly.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct TuiConfig {
    /// Key that submits the input box. Defaults to `enter`.
    #[serde(default)]
    pub submit: Option<String>,
    /// Key that inserts a newline in the input box. Defaults to `alt+enter`.
    #[serde(default)]
    pub newline: Option<String>,
}

/// The resolved TUI input-box key mapping: which Enter variant submits and
/// which inserts a newline. The key code is always Enter; only the
/// modifiers vary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputKeys {
    pub submit_modifiers: KeyModifiers,
    pub newline_modifiers: KeyModifiers,
}

impl Default for InputKeys {
    /// Absent `[tui]` section: bare Enter submits, Alt+Enter inserts a
    /// newline (the historical behavior).
    fn default() -> Self {
        InputKeys {
            submit_modifiers: KeyModifiers::NONE,
            newline_modifiers: KeyModifiers::ALT,
        }
    }
}

/// The whitelist text shared by every unsupported-key error.
const SUPPORTED_ENTER_KEYS: &str = "enter, alt+enter (option+enter), ctrl+enter, shift+enter";

/// Parse one `[tui]` key string into the Enter-variant modifiers. Exact
/// match only — no case or whitespace leniency. `option+enter` is the
/// macOS alias for `alt+enter`.
fn parse_enter_key(s: &str) -> Option<KeyModifiers> {
    match s {
        "enter" => Some(KeyModifiers::NONE),
        "alt+enter" | "option+enter" => Some(KeyModifiers::ALT),
        "ctrl+enter" => Some(KeyModifiers::CONTROL),
        "shift+enter" => Some(KeyModifiers::SHIFT),
        _ => None,
    }
}

/// Human-readable label of an Enter-variant modifier set, for footer hints
/// ("Enter", "Alt+Enter", "Ctrl+Enter", "Shift+Enter").
fn enter_key_label(modifiers: KeyModifiers) -> String {
    if modifiers == KeyModifiers::NONE {
        "Enter".to_owned()
    } else if modifiers == KeyModifiers::ALT {
        "Alt+Enter".to_owned()
    } else if modifiers == KeyModifiers::CONTROL {
        "Ctrl+Enter".to_owned()
    } else if modifiers == KeyModifiers::SHIFT {
        "Shift+Enter".to_owned()
    } else {
        "Enter".to_owned()
    }
}

impl InputKeys {
    /// Human-readable labels of the submit and newline keys, e.g.
    /// ("Enter", "Alt+Enter") for the defaults. The TUI footer uses the
    /// submit label so a reconfigured mapping is shown accurately.
    pub fn describe(&self) -> (String, String) {
        (
            enter_key_label(self.submit_modifiers),
            enter_key_label(self.newline_modifiers),
        )
    }
}

/// Runtime sandbox configuration for the bash tool, from `[sandbox]`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Sandbox {
    /// Master switch. Absent section or `enabled = false` means no sandbox.
    #[serde(default)]
    pub enabled: bool,
    /// Allow network access inside the sandbox (default true).
    #[serde(default = "default_true")]
    pub network: bool,
    /// Mount the workspace read-write inside the sandbox (default true).
    #[serde(default = "default_true")]
    pub workspace_writable: bool,
    /// Extra writable roots shared by bash mounts and file tools.
    #[serde(default)]
    pub writable_paths: Vec<String>,
    /// Extra readable roots shared by bash mounts and file tools.
    #[serde(default)]
    pub readable_paths: Vec<String>,
}

impl Default for Sandbox {
    /// An absent `[sandbox]` section: disabled, but with the documented
    /// defaults (`network` and `workspace_writable` true) materialized so a
    /// project `[sandbox] enabled = true` starts from the same baseline as
    /// a parsed section.
    fn default() -> Self {
        Sandbox {
            enabled: false,
            network: true,
            workspace_writable: true,
            writable_paths: Vec::new(),
            readable_paths: Vec::new(),
        }
    }
}

/// Backend selection for session persistence.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum SessionBackend {
    /// JSONL file backend (default).
    #[default]
    Jsonl,
    /// GreptimeDB backend. `conn` is a tokio-postgres connection string
    /// (e.g. "host=127.0.0.1 port=4002 dbname=public"). Requires the `greptime` feature.
    Greptime { conn: String },
    /// Local SQLite/turso database file backend. `path` is a path to a
    /// SQLite-compatible database file (e.g. "~/.local/share/e-agent/sessions.db";
    /// ":memory:" works for tests). Requires the `sqlite` feature.
    Sqlite { path: String },
}

/// The `[session]` config section.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SessionConfig {
    /// Backend type: `"jsonl"` (default), `"greptime"` or `"sqlite"`.
    #[serde(flatten)]
    pub backend: SessionBackend,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Deserialize)]
struct WebSearch {
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Provider {
    auth: Option<String>,
    base_url: Option<String>,
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
}

#[derive(Clone, Deserialize)]
struct ModelProfile {
    model: Option<String>,
    reasoning_effort: Option<String>,
    /// Force the OpenAI-compatible `thinking` switch (`thinking:
    /// {"type": "enabled"}`) on chat requests. DeepSeek V4's `max`
    /// reasoning effort needs the explicit switch (high enables thinking
    /// by default); most other providers ignore unknown top-level fields.
    /// Defaults to false; set `thinking = true` to enable.
    #[serde(default)]
    thinking: Option<bool>,
    /// Maximum context window in tokens (e.g. 131072). When set, the agent
    /// auto-compacts when usage exceeds 80% of this value, and the TUI
    /// shows a percentage alongside the token count.
    context_window: Option<u64>,
    /// Whether the model accepts image input. Defaults to false: user
    /// messages with attached images fail the vision gate with a clear
    /// error on non-vision models. kimi/k3 profiles should set
    /// `vision = true`; deepseek defaults to false.
    #[serde(default)]
    vision: Option<bool>,
}

#[derive(Debug)]
pub struct ResolvedModel {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Force the OpenAI-compatible `thinking` switch (`thinking:
    /// {"type": "enabled"}`) on chat requests; DeepSeek V4 `max` needs it.
    pub thinking: bool,
    pub auth: AuthMode,
    pub display: String,
    pub context_window: Option<u64>,
    pub vision: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    ChatGpt,
}

impl Config {
    pub fn load() -> anyhow::Result<Option<Self>> {
        for path in config_paths() {
            if path.is_file() {
                return Self::from_path(&path).map(Some);
            }
        }
        Ok(None)
    }

    /// Load the effective config for a workspace: the global config
    /// (`$XDG_CONFIG_HOME`/`~/.config/e-agent/config.toml`) with the
    /// project-level overrides from `<workspace>/.e-agent/config.toml`
    /// applied on top ([`Self::merged_with_project`]). Returns `None` when
    /// there is no global config — the project file is an override layer on
    /// top of the global config, not a standalone config.
    pub fn load_for_workspace(workspace: &Path) -> anyhow::Result<Option<Self>> {
        Self::load()?
            .map(|config| config.merged_with_project(workspace))
            .transpose()
    }

    /// Overlay the project-level overrides from
    /// `<workspace>/.e-agent/config.toml` over this (global) config and
    /// return the effective config for the workspace. When the project
    /// file is absent the global config is returned unchanged.
    ///
    /// Merged sections:
    /// - `[models."<name>"]`: merged **by model name** — a project model
    ///   replaces the same-named global model; models the project does not
    ///   define keep their global definitions. An absent or empty `[models]`
    ///   table keeps every global model.
    /// - `[roles]`: merged per key — a project role replaces the same-named
    ///   global role; other roles survive.
    ///
    /// `[sandbox]`, `[background]` and `[bash]` are not merged here: they
    /// keep their workspace-aware resolvers (`resolve_sandbox`,
    /// `resolve_background_timeout`, `resolve_bash_timeout`), which read
    /// the project file themselves and apply per-key overrides. All other
    /// sections (`default`, `providers`, `mcp`, `web_search`, `session`)
    /// stay global-only.
    ///
    /// The merged config keeps the global config's file path, so relative
    /// `api_key_file` paths keep resolving against the global config
    /// directory.
    pub fn merged_with_project(&self, workspace: &Path) -> anyhow::Result<Self> {
        let Some(project) = project_config(workspace)? else {
            return Ok(self.clone());
        };
        let mut merged = self.clone();
        if let Some(models) = project.models {
            merged.models.extend(models);
        }
        if let Some(roles) = project.roles {
            merged.roles.extend(roles);
        }
        Ok(merged)
    }

    fn from_path(path: &Path) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        let mut config: Self = toml::from_str(&source)
            .with_context(|| format!("cannot parse config file {}", path.display()))?;
        config.path = path.to_path_buf();
        Ok(config)
    }

    pub fn resolve(&self, requested_profile: Option<&str>) -> anyhow::Result<ResolvedModel> {
        let profile = requested_profile
            .or(self.default.as_deref())
            .or_else(|| self.roles.get("main").map(String::as_str))
            .ok_or_else(|| {
                anyhow!("config requires `default`, `[roles] main`, or --profile PROFILE")
            })?;
        self.resolve_profile(profile)
    }

    /// Resolve the profile routed to a built-in role. Returns None when the
    /// role is not routed (callers then fall back to the main profile).
    pub fn resolve_role(&self, role: &str) -> anyhow::Result<Option<ResolvedModel>> {
        match self.roles.get(role) {
            Some(profile) => Ok(Some(self.resolve_profile(profile)?)),
            None => Ok(None),
        }
    }

    /// Resolve every routed role to its model (role name -> resolved model).
    pub fn resolve_roles(&self) -> anyhow::Result<HashMap<String, ResolvedModel>> {
        let mut resolved = HashMap::new();
        for role in self.roles.keys() {
            if let Some(model) = self.resolve_role(role)? {
                resolved.insert(role.clone(), model);
            }
        }
        Ok(resolved)
    }

    /// Every switchable model profile name: `[models]` keys plus `[roles]`
    /// values (roles route to profiles, so they are switchable too),
    /// deduplicated and sorted for a stable display. Feeds the web `/model`
    /// autocomplete (`GET /api/models`).
    pub fn model_profiles(&self) -> Vec<String> {
        let mut profiles: Vec<String> = self.models.keys().cloned().collect();
        for role_profile in self.roles.values() {
            if !profiles.iter().any(|p| p == role_profile) {
                profiles.push(role_profile.clone());
            }
        }
        profiles.sort();
        profiles
    }

    /// The Exa web-search API key from `[web_search]`, or None when the
    /// section is absent. Process env `EXA_API_KEY` always wins over this
    /// (callers check it first). Exactly one of `api_key_file` / `api_key_env`
    /// must be set when the section is present.
    pub fn web_search_key(&self) -> anyhow::Result<Option<String>> {
        let Some(web_search) = &self.web_search else {
            return Ok(None);
        };
        let key = match (&web_search.api_key_file, &web_search.api_key_env) {
            (Some(_), Some(_)) => {
                bail!("[web_search] must set exactly one of `api_key_file` or `api_key_env`")
            }
            (None, None) => {
                bail!("[web_search] requires exactly one of `api_key_file` or `api_key_env`")
            }
            (Some(file), None) => match self.read_key_file("web_search", file) {
                // Web search is optional: a missing or unreadable key file
                // disables the tool instead of failing startup (unlike
                // provider credentials, which are required).
                Ok(key) => key,
                Err(_) => return Ok(None),
            },
            (None, Some(variable)) => std::env::var(variable)
                .with_context(|| format!("api_key_env `{variable}` for [web_search] is not set"))?
                .trim()
                .to_owned(),
        };
        if key.is_empty() {
            bail!("credential for [web_search] is empty");
        }
        Ok(Some(key))
    }

    /// Resolve the shared bash/file path policy for a workspace.
    pub fn sandbox(&self, workspace: &Path) -> anyhow::Result<Sandbox> {
        resolve_sandbox(Some(self), workspace)
    }

    /// The session backend from `[session] backend`, defaulting to Jsonl.
    pub fn session_backend(&self) -> SessionBackend {
        match &self.session {
            Some(session) => session.backend.clone(),
            None => SessionBackend::default(),
        }
    }

    /// The TUI submit/newline key mapping from `[tui]`. An absent section
    /// (or field) falls back to the defaults: submit=Enter, newline=Alt+Enter.
    /// Unsupported key strings and submit == newline are configuration
    /// errors — startup refuses rather than silently falling back.
    pub fn tui_keys(&self) -> anyhow::Result<InputKeys> {
        let Some(tui) = &self.tui else {
            return Ok(InputKeys::default());
        };
        let submit = tui.submit.as_deref().unwrap_or("enter");
        let newline = tui.newline.as_deref().unwrap_or("alt+enter");
        let submit_modifiers = parse_enter_key(submit).ok_or_else(|| {
            anyhow!(
                "[tui] submit = \"{submit}\" is not a supported key; expected one of: {SUPPORTED_ENTER_KEYS}"
            )
        })?;
        let newline_modifiers = parse_enter_key(newline).ok_or_else(|| {
            anyhow!(
                "[tui] newline = \"{newline}\" is not a supported key; expected one of: {SUPPORTED_ENTER_KEYS}"
            )
        })?;
        if submit_modifiers == newline_modifiers {
            bail!(
                "[tui] submit and newline must be different keys; both are configured as \"{submit}\""
            );
        }
        Ok(InputKeys {
            submit_modifiers,
            newline_modifiers,
        })
    }

    /// Resolve a single named profile (`provider/model`) to its wire
    /// settings. Public so runtime model switches (web/TUI `/model`) reuse
    /// the exact same resolution the startup path uses.
    pub fn resolve_profile(&self, profile: &str) -> anyhow::Result<ResolvedModel> {
        let provider_name = profile
            .split_once('/')
            .map(|(provider, _)| provider)
            .filter(|provider| !provider.is_empty())
            .ok_or_else(|| anyhow!("profile `{profile}` must be named provider/model"))?;
        let model_profile = self
            .models
            .get(profile)
            .ok_or_else(|| anyhow!("model profile `{profile}` is not defined"))?;
        let model = model_profile
            .model
            .as_deref()
            .filter(|model| !model.is_empty())
            .ok_or_else(|| anyhow!("model profile `{profile}` requires `model`"))?
            .to_owned();
        let reasoning_effort = model_profile.reasoning_effort.clone();
        let thinking = model_profile.thinking.unwrap_or(false);
        let provider = self.providers.get(provider_name).ok_or_else(|| {
            anyhow!("provider `{provider_name}` for profile `{profile}` is not defined")
        })?;
        if let Some(auth) = provider.auth.as_deref() {
            if auth != "chatgpt" {
                bail!("provider `{provider_name}` has unsupported auth `{auth}`");
            }
            if provider.base_url.is_some()
                || provider.api_key_file.is_some()
                || provider.api_key_env.is_some()
            {
                bail!(
                    "provider `{provider_name}` with auth = `chatgpt` cannot set `base_url`, `api_key_file`, or `api_key_env`"
                );
            }
            return Ok(ResolvedModel {
                base_url: String::new(),
                api_key: String::new(),
                model,
                reasoning_effort,
                thinking,
                auth: AuthMode::ChatGpt,
                display: profile.to_owned(),
                context_window: model_profile.context_window,
                vision: model_profile.vision.unwrap_or(false),
            });
        }
        let base_url = provider
            .base_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .ok_or_else(|| anyhow!("provider `{provider_name}` requires `base_url`"))?
            .to_owned();
        let api_key = match (&provider.api_key_file, &provider.api_key_env) {
            (Some(_), Some(_)) => bail!(
                "provider `{provider_name}` must set exactly one of `api_key_file` or `api_key_env`"
            ),
            (None, None) => bail!(
                "provider `{provider_name}` requires exactly one of `api_key_file` or `api_key_env`"
            ),
            (Some(file), None) => self.read_key_file(provider_name, file)?,
            (None, Some(variable)) => std::env::var(variable)
                .with_context(|| {
                    format!("api_key_env `{variable}` for provider `{provider_name}` is not set")
                })?
                .trim()
                .to_owned(),
        };
        if api_key.is_empty() {
            bail!("credential for provider `{provider_name}` is empty");
        }
        Ok(ResolvedModel {
            base_url,
            api_key,
            model,
            reasoning_effort,
            thinking,
            auth: AuthMode::ApiKey,
            display: profile.to_owned(),
            context_window: model_profile.context_window,
            vision: model_profile.vision.unwrap_or(false),
        })
    }

    fn read_key_file(&self, provider_name: &str, file: &Path) -> anyhow::Result<String> {
        let path = if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(file)
        };
        let key = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read api_key_file for provider `{provider_name}` at {}",
                path.display()
            )
        })?;
        Ok(key.trim().to_owned())
    }
}

/// The user-level config directory (`$XDG_CONFIG_HOME/e-agent` or
/// `~/.config/e-agent`), without requiring a `config.toml` to exist. Used by
/// roles.rs to locate the global agents directory.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("e-agent"));
    }
    crate::home_dir().map(|home| home.join(".config/e-agent"))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSandbox {
    /// Per-key scalar overrides: a project key replaces the global key;
    /// absent project keys keep the global value. Paths below keep the
    /// narrowing merge (project subpaths of global roots replace them).
    enabled: Option<bool>,
    network: Option<bool>,
    workspace_writable: Option<bool>,
    writable_paths: Option<Vec<String>>,
    readable_paths: Option<Vec<String>>,
}

/// Resolve global roots and the optional project selection, merged with
/// narrowing semantics: project roots that are strict subpaths of a global
/// root replace that global root (least privilege), while unrelated project
/// roots accumulate alongside the global ones. Missing global roots are
/// ignored for compatibility; all returned paths are canonical. The
/// project `[sandbox]` scalars (`enabled`, `network`, `workspace_writable`)
/// override the global keys per-key; absent project keys keep the global
/// values.
pub fn resolve_sandbox(config: Option<&Config>, workspace: &Path) -> anyhow::Result<Sandbox> {
    let mut result = config.and_then(|c| c.sandbox.clone()).unwrap_or_default();
    let global_writable = canonical_roots(&result.writable_paths, workspace, true)?;
    let global_readable = canonical_roots(&result.readable_paths, workspace, true)?;

    let local = project_sandbox(workspace)?;
    if let Some(local) = &local {
        if let Some(enabled) = local.enabled {
            result.enabled = enabled;
        }
        if let Some(network) = local.network {
            result.network = network;
        }
        if let Some(workspace_writable) = local.workspace_writable {
            result.workspace_writable = workspace_writable;
        }
    }
    let selecting = local
        .as_ref()
        .is_some_and(|s| s.writable_paths.is_some() || s.readable_paths.is_some());
    let (writable, readable) = if selecting {
        let local = local.expect("selection has a sandbox");
        let local_writable = canonical_roots(
            local.writable_paths.as_deref().unwrap_or_default(),
            workspace,
            false,
        )?;
        let local_readable = canonical_roots(
            local.readable_paths.as_deref().unwrap_or_default(),
            workspace,
            false,
        )?;
        for path in &local_writable {
            if !global_writable.iter().any(|root| path.starts_with(root)) {
                bail!(
                    "project writable path {} is not within any globally authorized writable root; \
                     add this path or an ancestor to [sandbox].writable_paths in the user-level \
                     config, or remove/narrow the project-local writable_paths entry",
                    path.display()
                );
            }
        }
        for path in &local_readable {
            if !global_readable
                .iter()
                .chain(&global_writable)
                .any(|root| path.starts_with(root))
            {
                bail!(
                    "project readable path {} is not within any globally authorized readable or \
                     writable root; add this path or an ancestor to [sandbox].readable_paths in \
                     the user-level config (or to writable_paths if writes are intended), or \
                     remove/narrow the project-local readable_paths entry",
                    path.display()
                );
            }
        }
        (
            merge_roots(global_writable, local_writable),
            merge_roots(global_readable, local_readable),
        )
    } else {
        (global_writable, global_readable)
    };
    let (writable, mut readable) = normalize_roots(writable, readable)?;

    // Linked-worktree support: when the workspace is a `git worktree`, its
    // `.git` is a pointer file (`gitdir: <main-repo>/.git/worktrees/<name>`).
    // The main repository (with its full object store and other branches)
    // lives OUTSIDE the workspace, so a subagent (oracle / fixer) running in
    // the worktree cannot read it — its reads and bash mounts only cover the
    // workspace. Bind the main repo read-only into the readable roots so
    // review/comparison against the main branch keeps working.
    if let Some(main_repo) = linked_worktree_main_repo(workspace)?
        && !writable.iter().any(|root| root == &main_repo)
        && !readable.iter().any(|root| root == &main_repo)
    {
        readable.push(main_repo);
    }

    result.writable_paths = utf8_roots(writable)?;
    result.readable_paths = utf8_roots(readable)?;
    Ok(result)
}

/// Resolve the main repository of a linked worktree. When `workspace/.git`
/// is a regular file starting with `gitdir: `, it points at the main repo's
/// gitdir (e.g. `<main>/.git/worktrees/<name>`); the main repository root is
/// that path's grandparent's parent (strip `.git/worktrees/<name>`). Returns
/// `Ok(None)` for a normal repository (`.git` is a directory or absent).
fn linked_worktree_main_repo(workspace: &Path) -> anyhow::Result<Option<PathBuf>> {
    let git_path = workspace.join(".git");
    let metadata = match std::fs::metadata(&git_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", git_path.display()));
        }
    };
    if metadata.is_dir() {
        return Ok(None);
    }
    let pointer = std::fs::read_to_string(&git_path)
        .with_context(|| format!("cannot read {}", git_path.display()))?;
    let Some(target) = pointer.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = PathBuf::from(target.trim());
    // Structural validation (HIGH-1): the pointer must be an absolute,
    // component-clean path of the shape <main>/.git/worktrees/<name>. A
    // malicious project archive could otherwise point gitdir: at ~/.ssh or
    // / and get that path auto-added as a read-only external root.
    if !target.is_absolute()
        || target
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Ok(None);
    }
    // <main>/.git/worktrees/<name>  →  main repo root = <main>
    let Some(git_dir) = target.parent().and_then(|p| p.parent()) else {
        return Ok(None);
    };
    let Some(main_repo) = git_dir.parent() else {
        return Ok(None);
    };
    // Reject the filesystem root and require <main>/.git to be a real
    // directory (the gitdir of the main repository).
    if main_repo.as_os_str().is_empty() || main_repo == Path::new("/") {
        return Ok(None);
    }
    if !git_dir.is_dir() {
        return Ok(None);
    }
    let main_repo = main_repo.to_path_buf();
    // Canonicalize to defeat symlink tricks and normalize the root (the
    // Windows `\\?\` verbatim prefix is stripped so the result compares
    // equal to the roots produced by `canonical_roots`).
    match crate::canonicalize_path(&main_repo) {
        Ok(canonical) if canonical.is_dir() => Ok(Some(canonical)),
        _ => Ok(None),
    }
}

/// Resolve the background-task timeout policy for a workspace: workspace
/// `<workspace>/.e-agent/config.toml` `[background]` overrides the global
/// config; `timeout_secs = 0` means no timeout (`None`); absent everywhere
/// returns the default 1800s. Mirrors `resolve_sandbox`/`project_sandbox`.
pub fn resolve_background_timeout(
    config: Option<&Config>,
    workspace: &Path,
) -> anyhow::Result<Option<Duration>> {
    let global = config
        .and_then(|c| c.background.as_ref())
        .and_then(|b| b.timeout_secs);
    let local = project_background(workspace)?.and_then(|b| b.timeout_secs);
    match local.or(global) {
        Some(0) => Ok(None),
        Some(secs) => Ok(Some(Duration::from_secs(secs))),
        None => Ok(Some(Duration::from_secs(DEFAULT_BACKGROUND_TIMEOUT_SECS))),
    }
}

/// Project-level `[models]` / `[roles]` overrides parsed from
/// `<workspace>/.e-agent/config.toml` (see [`project_config`]).
#[derive(Deserialize)]
struct ProjectConfig {
    models: Option<HashMap<String, ModelProfile>>,
    roles: Option<HashMap<String, String>>,
}

/// Read the project-level `[models]` / `[roles]` overrides from
/// `<workspace>/.e-agent/config.toml` (same pattern as `project_sandbox`);
/// `None` when the file is absent. Unknown sections (`[background]`,
/// `[bash]`, `[sandbox]`, `[providers]`, …) are ignored — each resolver
/// picks up the sections it owns.
fn project_config(workspace: &Path) -> anyhow::Result<Option<ProjectConfig>> {
    let path = workspace.join(".e-agent/config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let parsed: ProjectConfig = toml::from_str(&source)
        .with_context(|| format!("cannot parse project config {}", path.display()))?;
    Ok(Some(parsed))
}

/// Read `[background]` from `<workspace>/.e-agent/config.toml` (same
/// pattern as `project_sandbox`); `None` when absent or unparseable-free.
fn project_background(workspace: &Path) -> anyhow::Result<Option<BackgroundConfig>> {
    #[derive(Deserialize)]
    struct ProjectConfig {
        background: Option<BackgroundConfig>,
    }
    let path = workspace.join(".e-agent/config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let parsed: ProjectConfig = toml::from_str(&source)
        .with_context(|| format!("cannot parse project config {}", path.display()))?;
    Ok(parsed.background)
}

/// Resolve the foreground bash timeout policy for a workspace: workspace
/// `<workspace>/.e-agent/config.toml` `[bash]` overrides the global config;
/// `timeout_secs = 0` means no timeout (`None`); absent everywhere returns
/// the default 30s. Mirrors `resolve_background_timeout`/`project_background`.
pub fn resolve_bash_timeout(
    config: Option<&Config>,
    workspace: &Path,
) -> anyhow::Result<Option<Duration>> {
    let global = config
        .and_then(|c| c.bash.as_ref())
        .and_then(|b| b.timeout_secs);
    let local = project_bash(workspace)?.and_then(|b| b.timeout_secs);
    match local.or(global) {
        Some(0) => Ok(None),
        Some(secs) => Ok(Some(Duration::from_secs(secs))),
        None => Ok(Some(Duration::from_secs(DEFAULT_BASH_TIMEOUT_SECS))),
    }
}

/// Read `[bash]` from `<workspace>/.e-agent/config.toml` (same pattern as
/// `project_background`); `None` when absent.
fn project_bash(workspace: &Path) -> anyhow::Result<Option<BashConfig>> {
    #[derive(Deserialize)]
    struct ProjectConfig {
        bash: Option<BashConfig>,
    }
    let path = workspace.join(".e-agent/config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let parsed: ProjectConfig = toml::from_str(&source)
        .with_context(|| format!("cannot parse project config {}", path.display()))?;
    Ok(parsed.bash)
}

fn project_sandbox(workspace: &Path) -> anyhow::Result<Option<ProjectSandbox>> {
    #[derive(Deserialize)]
    struct ProjectConfig {
        sandbox: Option<ProjectSandbox>,
    }
    let path = workspace.join(".e-agent/config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let parsed: ProjectConfig = toml::from_str(&source)
        .with_context(|| format!("cannot parse project config {}", path.display()))?;
    Ok(parsed.sandbox)
}

fn canonical_roots(
    paths: &[String],
    workspace: &Path,
    skip_missing: bool,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for configured in paths {
        let expanded = expand_sandbox_path(configured, workspace)?;
        let path = match crate::canonicalize_path(&expanded) {
            Ok(path) => path,
            Err(error) if skip_missing && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot canonicalize external path {}", expanded.display())
                });
            }
        };
        let kind = std::fs::metadata(&path)
            .with_context(|| format!("cannot inspect external path {}", path.display()))?
            .file_type();
        if !kind.is_dir() && !kind.is_file() {
            bail!(
                "external path {} is not a regular file or directory",
                path.display()
            );
        }
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    Ok(roots)
}

/// Merge project-selected roots into global roots with narrowing semantics:
///
/// - A project root that is a strict subpath of a global root (`W != G` and
///   `W.starts_with(G)`) replaces that global root: the global ancestor is
///   dropped and the narrower project roots are kept (least privilege).
///   Multiple project subpaths of the same global root all survive as
///   separate narrowing points.
/// - A project root equal to a global root is a no-op: the global root stays
///   and the duplicate is folded away by `normalize_roots`.
/// - A project root with no ancestor relationship to any global root is
///   simply accumulated alongside the global roots.
///
/// The subset validation in `resolve_sandbox` has already guaranteed every
/// project root is inside some global root, so no widening is possible here.
fn merge_roots(global: Vec<PathBuf>, local: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut merged: Vec<PathBuf> = global
        .into_iter()
        .filter(|root| {
            !local
                .iter()
                .any(|candidate| *candidate != *root && candidate.starts_with(root))
        })
        .collect();
    merged.extend(local);
    merged
}

fn normalize_roots(
    mut writable: Vec<PathBuf>,
    mut readable: Vec<PathBuf>,
) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    for read in &readable {
        if let Some(write) = writable
            .iter()
            .find(|write| read != *write && read.starts_with(write))
        {
            bail!(
                "read-only child {} under writable root {} is unsupported; select a narrower writable root or downgrade the whole selected root",
                read.display(),
                write.display()
            );
        }
    }
    writable.sort_by_key(|p| p.components().count());
    let mut compact_writable: Vec<PathBuf> = Vec::new();
    for path in writable {
        if !compact_writable.iter().any(|root| path.starts_with(root)) {
            compact_writable.push(path);
        }
    }
    writable = compact_writable;
    readable.sort_by_key(|p| p.components().count());
    let mut compact_readable: Vec<PathBuf> = Vec::new();
    for path in readable {
        if !compact_readable.iter().any(|root| path.starts_with(root))
            && !writable.iter().any(|root| path.starts_with(root))
        {
            compact_readable.push(path);
        }
    }
    readable = compact_readable;
    Ok((writable, readable))
}

fn utf8_roots(roots: Vec<PathBuf>) -> anyhow::Result<Vec<String>> {
    roots
        .into_iter()
        .map(|path| {
            path.into_os_string().into_string().map_err(|path| {
                anyhow!(
                    "canonical external path {} is not valid UTF-8",
                    PathBuf::from(path).display()
                )
            })
        })
        .collect()
}

fn expand_sandbox_path(path: &str, workspace: &Path) -> anyhow::Result<PathBuf> {
    if path == "~" || path.starts_with("~/") {
        let home =
            crate::home_dir().ok_or_else(|| anyhow!("cannot expand `{path}`: HOME is not set"))?;
        return Ok(home.join(path.strip_prefix("~/").unwrap_or("")));
    }
    let candidate = PathBuf::from(path);
    Ok(if candidate.is_absolute() {
        candidate
    } else {
        workspace.join(candidate)
    })
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(xdg).join("e-agent/config.toml"));
    }
    if let Some(home) = crate::home_dir() {
        let fallback = home.join(".config/e-agent/config.toml");
        if !paths.contains(&fallback) {
            paths.push(fallback);
        }
    }
    paths
}

#[cfg(test)]
mod tests;
