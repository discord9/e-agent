use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use crossterm::event::KeyModifiers;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
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
    /// Optional `[delegate]` policy (FinishWhenIdle finalize wait).
    #[serde(default)]
    delegate: Option<DelegateConfig>,
    /// Optional `[tui]` submit/newline key mapping. A project-level
    /// `[tui]` section replaces this one wholesale (fields the project
    /// omits fall back to the built-in defaults, not to these values); no
    /// project `[tui]` keeps this global section (see
    /// `merged_with_project`).
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

/// Delegate subagent policy, from `[delegate]`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct DelegateConfig {
    /// Upper bound, in seconds, that a `FinishWhenIdle` subagent waits for
    /// its blocking background tasks to finish before finalizing anyway.
    /// `0` disables the wait (wait forever). Absent defaults to
    /// [`DEFAULT_FINALIZE_WAIT_SECS`].
    #[serde(default)]
    pub finalize_wait_secs: Option<u64>,
}

/// Default FinishWhenIdle finalize wait: 10 minutes. The default background
/// task timeout is 30 minutes, but a subagent that already finished its
/// work should not wait that long — 10 minutes covers most background
/// tasks, and on expiry the subagent finalizes while the tasks keep running
/// in the shared registry (the parent agent can still read or cancel them).
pub const DEFAULT_FINALIZE_WAIT_SECS: u64 = 10 * 60;

/// The `[tui]` section: submit/newline key mapping. A project-level
/// `[tui]` section replaces the global one wholesale (see
/// `merged_with_project`): fields the project omits fall back to the
/// built-in defaults, not to the global values. No project `[tui]` keeps
/// the global section.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct TuiConfig {
    /// Key string that submits the prompt (default `"enter"`).
    #[serde(default)]
    pub submit: Option<String>,
    /// Key string that inserts a newline (default `"alt+enter"`).
    #[serde(default)]
    pub newline: Option<String>,
}

/// Every `[tui]` key string `parse_enter_key` accepts, listed in the
/// unsupported-key error so the user sees the full vocabulary at once.
pub const SUPPORTED_ENTER_KEYS: &str = "enter, alt+enter (option+enter), ctrl+enter, shift+enter";

/// Resolved TUI submit/newline key mapping (from `Config::tui_keys`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputKeys {
    /// Modifiers that submit the prompt. `NONE` = bare Enter.
    pub submit_modifiers: KeyModifiers,
    /// Modifiers that insert a newline. `NONE` = bare Enter.
    pub newline_modifiers: KeyModifiers,
}

impl Default for InputKeys {
    /// Bare Enter submits, Alt+Enter inserts a newline (historical default).
    fn default() -> Self {
        InputKeys {
            submit_modifiers: KeyModifiers::NONE,
            newline_modifiers: KeyModifiers::ALT,
        }
    }
}

impl InputKeys {
    /// Human-readable labels for the TUI hint bar: `(submit, newline)`.
    pub fn describe(&self) -> (String, String) {
        (
            describe_enter_key(self.submit_modifiers),
            describe_enter_key(self.newline_modifiers),
        )
    }
}

fn describe_enter_key(modifiers: KeyModifiers) -> String {
    if modifiers == KeyModifiers::ALT {
        "Alt+Enter".to_owned()
    } else if modifiers == KeyModifiers::CONTROL {
        "Ctrl+Enter".to_owned()
    } else if modifiers == KeyModifiers::SHIFT {
        "Shift+Enter".to_owned()
    } else {
        "Enter".to_owned()
    }
}

/// Parse a `[tui]` key string (matched literally) into the modifier set for
/// a bare Enter. `option+enter` is the macOS alias for `alt+enter`.
fn parse_enter_key(key: &str) -> Option<KeyModifiers> {
    match key {
        "enter" => Some(KeyModifiers::NONE),
        "alt+enter" => Some(KeyModifiers::ALT),
        "option+enter" => Some(KeyModifiers::ALT),
        "ctrl+enter" => Some(KeyModifiers::CONTROL),
        "shift+enter" => Some(KeyModifiers::SHIFT),
        _ => None,
    }
}

/// How often the config hot-reload watcher polls the config files for mtime
/// changes (server and TUI). A poll is a stat on one or two small files, so
/// 2s keeps edits feel near-instant without any load.
pub const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
    /// Mount points as configured: (canonical source, configured dest).
    /// Kept separate from `readable_paths`/`writable_paths` (which stay
    /// canonical) so a configured path that is a symlink alias of a canonical
    /// root still appears inside the sandbox at its configured location.
    /// Filled by `resolve_sandbox`; never read from config.
    #[serde(default, skip)]
    pub readable_mounts: Vec<(String, String)>,
    #[serde(default, skip)]
    pub writable_mounts: Vec<(String, String)>,
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
            readable_mounts: Vec::new(),
            writable_mounts: Vec::new(),
        }
    }
}

/// Backend selection for session persistence.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum SessionBackend {
    /// Local SQLite/turso database file backend (default). `path` is a path
    /// to a SQLite-compatible database file (e.g. "/data/sessions.db";
    /// ":memory:" works for tests; `~` is NOT expanded). `None` resolves
    /// to `<workspace>/.e-agent/sessions.db`. Requires the `sqlite` feature.
    Sqlite { path: Option<String> },
    /// JSONL file backend (legacy default, still selectable).
    Jsonl,
    /// GreptimeDB backend. `conn` is a tokio-postgres connection string
    /// (e.g. "host=127.0.0.1 port=4002 dbname=public"). Requires the `greptime` feature.
    Greptime { conn: String },
}

impl Default for SessionBackend {
    fn default() -> Self {
        SessionBackend::Sqlite { path: None }
    }
}

/// The `[session]` config section.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SessionConfig {
    /// Backend type: `"sqlite"` (default), `"jsonl"` or `"greptime"`.
    #[serde(flatten)]
    pub backend: SessionBackend,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
struct WebSearch {
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Provider {
    auth: Option<String>,
    base_url: Option<String>,
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// DeepSeek Chat wire compatibility (default false). Solely for
    /// DeepSeek Chat profiles in thinking mode: when `thinking = true`,
    /// assistant turns that carry `tool_calls` echo their original
    /// `reasoning_content` back to the API on the next request (DeepSeek
    /// 400s without it), and content-less assistant turns wire as
    /// `content: ""` instead of absent/null. Other providers keep the
    /// default wire and are unaffected.
    #[serde(default)]
    deepseek_compat: Option<bool>,
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
    /// DeepSeek Chat wire compatibility (`deepseek_compat = true`):
    /// thinking-mode tool-call turns replay `reasoning_content` and
    /// content-less assistant turns wire as `content: ""`. False for every
    /// other provider.
    pub deepseek_compat: bool,
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
    /// - `default`: scalar override — a project `default` replaces the
    ///   global default profile; no project `default` keeps the global one.
    /// - `[models."<name>"]`: merged **by model name** — a project model
    ///   replaces the same-named global model; models the project does not
    ///   define keep their global definitions. An absent or empty `[models]`
    ///   table keeps every global model.
    /// - `[mcp."<name>"]`: merged **by server name**, same as `[models]` —
    ///   a project server replaces the same-named global server; servers
    ///   the project does not define keep their global definitions. A
    ///   global server with `enabled = false` is a kill switch: a
    ///   same-named project server does NOT re-enable it — the disabled
    ///   global entry is kept, because project MCP servers spawn commands
    ///   and the global `enabled = false` is the trust boundary against
    ///   untrusted project files.
    /// - `[roles]`: merged per key — a project role replaces the same-named
    ///   global role; other roles survive.
    /// - `[tui]`: replaced **wholesale** — a project `[tui]` section
    ///   replaces the global section entirely; fields the project omits
    ///   fall back to the built-in defaults, not the global values. No
    ///   project `[tui]` keeps the global section.
    ///
    /// `[sandbox]`, `[background]` and `[bash]` are not merged here: they
    /// keep their workspace-aware resolvers (`resolve_sandbox`,
    /// `resolve_background_timeout`, `resolve_bash_timeout`), which read
    /// the project file themselves and apply per-key overrides. The
    /// remaining sections (`providers`, `web_search`, `session`) stay
    /// global-only — a project file that carries them is accepted for
    /// compatibility but the sections are ignored.
    ///
    /// The project file is parsed with `deny_unknown_fields` (see
    /// [`ProjectConfig`]): an unknown section is a parse error, refusing
    /// startup rather than being silently dropped.
    ///
    /// The merged config keeps the global config's file path, so relative
    /// `api_key_file` paths keep resolving against the global config
    /// directory.
    pub fn merged_with_project(&self, workspace: &Path) -> anyhow::Result<Self> {
        let Some(project) = project_config(workspace)? else {
            return Ok(self.clone());
        };
        let mut merged = self.clone();
        if let Some(default) = project.default {
            merged.default = Some(default);
        }
        if let Some(models) = project.models {
            merged.models.extend(models);
        }
        if let Some(project_mcp) = project.mcp {
            for (name, server) in project_mcp {
                // Global `enabled = false` is a kill switch: a same-named
                // project server must not re-enable a server the user
                // disabled globally (project MCP servers spawn commands, so
                // this is the trust boundary against untrusted project
                // files). Otherwise the project server replaces the
                // same-named global one, and new names are added.
                let globally_disabled = merged
                    .mcp
                    .get(&name)
                    .is_some_and(|existing| !existing.enabled);
                if !globally_disabled {
                    merged.mcp.insert(name, server);
                }
            }
        }
        if let Some(roles) = project.roles {
            merged.roles.extend(roles);
        }
        if let Some(tui) = project.tui {
            merged.tui = Some(tui);
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

    /// The session backend from `[session] backend`, defaulting to Sqlite
    /// (`<workspace>/.e-agent/sessions.db`).
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
                deepseek_compat: model_profile.deepseek_compat.unwrap_or(false),
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
            deepseek_compat: model_profile.deepseek_compat.unwrap_or(false),
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
/// values. In addition, every configured root is surfaced as a
/// (canonical source, configured dest) pair in `readable_mounts` /
/// `writable_mounts` so a configured alias (e.g. a `~/.cargo` symlink onto a
/// canonical writable root) stays visible inside the sandbox at the
/// configured location even when the readable root itself is shadowed by a
/// writable root during normalization. Two configured aliases of the same
/// canonical source with different destinations are BOTH preserved as
/// separate mount entries (dedup only collapses identical source+dest
/// pairs); the canonical authority path vectors still deduplicate by source,
/// so aliases never multiply authority. After the merge/narrowing, the
/// configured mounts are filtered by the final canonical roots: a stale
/// mount whose source is no longer inside any final writable root (a
/// narrowed-away global writable parent) is dropped so bash cannot regain
/// the removed authority, while unrelated global mounts and independent RO
/// alias mounts whose source lies inside a final writable root survive.
pub fn resolve_sandbox(config: Option<&Config>, workspace: &Path) -> anyhow::Result<Sandbox> {
    let mut result = config.and_then(|c| c.sandbox.clone()).unwrap_or_default();
    // Collect configured (canonical source, configured dest) mount pairs
    // from the RAW canonical_roots output, before merge/normalize narrowing.
    // normalize_roots drops a readable root shadowed by a writable root, but
    // the configured alias (e.g. `~/.cargo` symlinked onto a canonical
    // writable root) must still appear inside the sandbox at its configured
    // location — that is exactly the scenario this feature fixes.
    let global_writable = canonical_roots(&result.writable_paths, workspace, true)?;
    let global_readable = canonical_roots(&result.readable_paths, workspace, true)?;
    let mut readable_mounts: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut writable_mounts: Vec<(PathBuf, PathBuf)> = Vec::new();
    writable_mounts.extend(global_writable.iter().cloned());
    readable_mounts.extend(global_readable.iter().cloned());

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
        writable_mounts.extend(local_writable.iter().cloned());
        readable_mounts.extend(local_readable.iter().cloned());
        for (path, _) in &local_writable {
            if !global_writable
                .iter()
                .any(|(root, _)| path.starts_with(root))
            {
                bail!(
                    "project writable path {} is not within any globally authorized writable root; \
                     add this path or an ancestor to [sandbox].writable_paths in the user-level \
                     config, or remove/narrow the project-local writable_paths entry",
                    path.display()
                );
            }
        }
        for (path, _) in &local_readable {
            if !global_readable
                .iter()
                .chain(&global_writable)
                .any(|(root, _)| path.starts_with(root))
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
            merge_roots(
                canonical_only(global_writable),
                canonical_only(local_writable),
            ),
            merge_roots(
                canonical_only(global_readable),
                canonical_only(local_readable),
            ),
        )
    } else {
        (
            canonical_only(global_writable),
            canonical_only(global_readable),
        )
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
        readable.push(main_repo.clone());
        // The main repo was never user-configured under an alias, so its
        // mount is the canonical self-mount (source == dest == canonical).
        readable_mounts.push((main_repo.clone(), main_repo));
    }

    // Filter the configured mounts by the FINAL canonical roots. The mounts
    // were collected from the RAW roots before merge/narrowing, so they
    // still carry narrowed-away global ancestors: a writable mount whose
    // source is no longer inside any final writable root is stale (e.g. a
    // global writable parent replaced by a project subpath) and is dropped,
    // so bash cannot regain the narrowed-away authority through the stale
    // bind. Unrelated global mounts keep their sources inside final roots
    // and survive; an independent RO alias mount whose source lies inside a
    // final writable root survives too (its dest differs from the writable
    // self-mount, so it stays an independent mount point).
    writable_mounts.retain(|(source, _)| {
        writable
            .iter()
            .any(|root| Path::new(source).starts_with(root))
    });
    readable_mounts.retain(|(source, _)| {
        readable
            .iter()
            .any(|root| Path::new(source).starts_with(root))
            || writable
                .iter()
                .any(|root| Path::new(source).starts_with(root))
    });

    result.writable_paths = utf8_roots(writable)?;
    result.readable_paths = utf8_roots(readable)?;
    // Deduplicate the configured mounts: identical (source, dest) pairs
    // collapse, and a readable dest shadowed by a writable mount at the same
    // dest is dropped (the writable bind wins at that mount point).
    let mut writable_mounts = utf8_mounts(writable_mounts)?;
    writable_mounts.sort();
    writable_mounts.dedup();
    let mut readable_mounts = utf8_mounts(readable_mounts)?;
    readable_mounts.sort();
    readable_mounts.dedup();
    readable_mounts.retain(|(_, dest)| !writable_mounts.iter().any(|(_, wdest)| wdest == dest));
    result.writable_mounts = writable_mounts;
    result.readable_mounts = readable_mounts;
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

/// Project-level overrides parsed from `<workspace>/.e-agent/config.toml`
/// (see [`project_config`]).
///
/// `#[serde(deny_unknown_fields)]` makes an unknown section (a typo, or a
/// section the project file cannot carry) a hard parse error instead of a
/// silent ignore — the file is an override layer, so a misspelled section
/// would otherwise be silently dropped. Every legal section is declared
/// below: the merged ones (`default`, `[models]`, `[mcp]`, `[roles]`,
/// `[tui]`) are applied by `merged_with_project`; `[sandbox]`, `[bash]`,
/// `[background]` and `[delegate]` are consumed by their own
/// workspace-aware resolvers; and `[providers]`, `[web_search]`,
/// `[session]` are white-listed for compatibility — they parse but stay
/// global-only and are silently ignored by the project layer (a project
/// file that carried them before this strictness was added keeps starting).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    /// Default model profile override; replaces the global `default` when
    /// present (scalar override, absent keeps the global value). It
    /// participates in the normal resolve priority chain — an explicit
    /// `--profile` wins over it, and it wins over `[roles] main`.
    default: Option<String>,
    models: Option<HashMap<String, ModelProfile>>,
    /// Project `[mcp."<name>"]` servers, merged by name (see
    /// `merged_with_project`). Project MCP servers start the commands they
    /// configure, so `<workspace>/.e-agent/config.toml` is trusted input
    /// (documented in the README).
    mcp: Option<HashMap<String, crate::mcp::McpServerConfig>>,
    roles: Option<HashMap<String, String>>,
    /// `[sandbox]` per-key overrides, consumed by `resolve_sandbox`.
    sandbox: Option<ProjectSandbox>,
    /// `[bash]` override, consumed by `resolve_bash_timeout`.
    bash: Option<BashConfig>,
    /// `[background]` override, consumed by `resolve_background_timeout`.
    background: Option<BackgroundConfig>,
    /// `[delegate]` override, consumed by `resolve_finalize_wait`.
    delegate: Option<DelegateConfig>,
    /// Whole-section `[tui]` replacement; absent keeps the global section.
    tui: Option<TuiConfig>,
    /// Global-only sections white-listed for compatibility: parsed so the
    /// project file does not fail `deny_unknown_fields`, but never merged
    /// into the effective config (see the struct doc).
    #[allow(dead_code)]
    providers: Option<HashMap<String, Provider>>,
    #[allow(dead_code)]
    web_search: Option<WebSearch>,
    #[allow(dead_code)]
    session: Option<SessionConfig>,
}

/// Read the project-level overrides from
/// `<workspace>/.e-agent/config.toml` (same pattern as `project_sandbox`);
/// `None` when the file is absent. Unknown sections are a parse error
/// (`deny_unknown_fields`, see [`ProjectConfig`]); the merged sections are
/// applied by `merged_with_project`, and `[sandbox]` / `[bash]` /
/// `[background]` are picked up by their own resolvers.
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

/// Resolve the FinishWhenIdle finalize-wait policy for a workspace:
/// workspace `<workspace>/.e-agent/config.toml` `[delegate]` overrides the
/// global config; `finalize_wait_secs = 0` disables the wait (`None` — the
/// subagent waits indefinitely); absent everywhere returns the default
/// 600s. Mirrors `resolve_background_timeout`/`project_background`.
pub fn resolve_finalize_wait(
    config: Option<&Config>,
    workspace: &Path,
) -> anyhow::Result<Option<Duration>> {
    let global = config
        .and_then(|c| c.delegate.as_ref())
        .and_then(|d| d.finalize_wait_secs);
    let local = project_delegate(workspace)?.and_then(|d| d.finalize_wait_secs);
    match local.or(global) {
        Some(0) => Ok(None),
        Some(secs) => Ok(Some(Duration::from_secs(secs))),
        None => Ok(Some(Duration::from_secs(DEFAULT_FINALIZE_WAIT_SECS))),
    }
}

/// Read `[delegate]` from `<workspace>/.e-agent/config.toml` (same pattern
/// as `project_background`); `None` when absent.
fn project_delegate(workspace: &Path) -> anyhow::Result<Option<DelegateConfig>> {
    let path = workspace.join(".e-agent/config.toml");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let parsed: ProjectConfig = toml::from_str(&source)
        .with_context(|| format!("cannot parse project config {}", path.display()))?;
    Ok(parsed.delegate)
}

fn project_sandbox(workspace: &Path) -> anyhow::Result<Option<ProjectSandbox>> {
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

/// Resolve configured sandbox roots into (canonical path, configured path)
/// pairs. The canonical path is what file tools and merge/normalize use; the
/// configured path is the `~`-expanded, workspace-joined form the user wrote
/// (before canonicalization), so a symlink alias still mounts inside the
/// sandbox at the user's configured location while the security boundary
/// stays canonical. `skip_missing` silently ignores NotFound (compatibility
/// for global roots); everything else still fails loudly. Deduplication
/// collapses only identical (canonical source, configured dest) pairs: two
/// configured aliases of the same canonical source with DIFFERENT
/// destinations are both preserved as separate mount entries (each appears
/// inside the sandbox and is addressable by the file tools at its own
/// configured location). The canonical authority path vectors derived from
/// these pairs are still deduplicated by source in `normalize_roots`, so
/// multiple aliases never multiply authority.
fn canonical_roots(
    paths: &[String],
    workspace: &Path,
    skip_missing: bool,
) -> anyhow::Result<Vec<(PathBuf, PathBuf)>> {
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
        if !roots
            .iter()
            .any(|(canonical, dest)| canonical == &path && dest == &expanded)
        {
            roots.push((path, expanded));
        }
    }
    Ok(roots)
}

/// Strip the configured dest from canonical_roots pairs: merge/normalize
/// operate only on the canonical roots.
fn canonical_only(pairs: Vec<(PathBuf, PathBuf)>) -> Vec<PathBuf> {
    pairs.into_iter().map(|(canonical, _)| canonical).collect()
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

fn utf8_mounts(mounts: Vec<(PathBuf, PathBuf)>) -> anyhow::Result<Vec<(String, String)>> {
    mounts
        .into_iter()
        .map(|(source, dest)| {
            let source = source.into_os_string().into_string().map_err(|path| {
                anyhow!(
                    "canonical external path {} is not valid UTF-8",
                    PathBuf::from(path).display()
                )
            })?;
            let dest = dest.into_os_string().into_string().map_err(|path| {
                anyhow!(
                    "configured external path {} is not valid UTF-8",
                    PathBuf::from(path).display()
                )
            })?;
            Ok((source, dest))
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

/// Every file that feeds the effective config of a workspace: the global
/// candidates ([`config_paths`], the first existing one wins) plus the
/// project override `<workspace>/.e-agent/config.toml`. The hot-reload
/// watcher polls these for mtime changes; a path may not exist yet (a
/// config created while the server runs is picked up too).
pub fn config_watch_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut paths = config_paths();
    let project = workspace.join(".e-agent/config.toml");
    if !paths.contains(&project) {
        paths.push(project);
    }
    paths
}

#[cfg(test)]
mod tests;
