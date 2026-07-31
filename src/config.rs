use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

#[derive(Deserialize)]
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
    #[serde(skip)]
    path: PathBuf,
}

/// Runtime sandbox configuration for the bash tool, from `[sandbox]`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
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
}

/// The `[session]` config section.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SessionConfig {
    /// Backend type: `"jsonl"` (default) or `"greptime"`.
    #[serde(flatten)]
    pub backend: SessionBackend,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct WebSearch {
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
}

#[derive(Deserialize)]
struct Provider {
    auth: Option<String>,
    base_url: Option<String>,
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,
}

#[derive(Deserialize)]
struct ModelProfile {
    model: Option<String>,
    reasoning_effort: Option<String>,
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
            (Some(file), None) => self.read_key_file("web_search", file)?,
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

    fn resolve_profile(&self, profile: &str) -> anyhow::Result<ResolvedModel> {
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
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/e-agent"))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSandbox {
    writable_paths: Option<Vec<String>>,
    readable_paths: Option<Vec<String>>,
}

/// Resolve global roots and the optional project selection, merged with
/// narrowing semantics: project roots that are strict subpaths of a global
/// root replace that global root (least privilege), while unrelated project
/// roots accumulate alongside the global ones. Missing global roots are
/// ignored for compatibility; all returned paths are canonical.
pub fn resolve_sandbox(config: Option<&Config>, workspace: &Path) -> anyhow::Result<Sandbox> {
    let mut result = config.and_then(|c| c.sandbox.clone()).unwrap_or_default();
    let global_writable = canonical_roots(&result.writable_paths, workspace, true)?;
    let global_readable = canonical_roots(&result.readable_paths, workspace, true)?;

    let local = project_sandbox(workspace)?;
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
                    "project writable path {} is not within a global writable root",
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
                    "project readable path {} is not within a global readable or writable root",
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
    let (writable, readable) = normalize_roots(writable, readable)?;
    result.writable_paths = utf8_roots(writable)?;
    result.readable_paths = utf8_roots(readable)?;
    Ok(result)
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
        let path = match std::fs::canonicalize(&expanded) {
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
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("cannot expand `{path}`: HOME is not set"))?;
        return Ok(PathBuf::from(home).join(path.strip_prefix("~/").unwrap_or("")));
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
    if let Some(home) = std::env::var_os("HOME") {
        let fallback = PathBuf::from(home).join(".config/e-agent/config.toml");
        if !paths.contains(&fallback) {
            paths.push(fallback);
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn parses_session_backend_config() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();

        // Default (no [session] section) -> Jsonl
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
"#,
        );
        let config = Config::from_path(&path).unwrap();
        assert!(matches!(config.session_backend(), SessionBackend::Jsonl));

        // Explicit jsonl
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[session]
backend = "jsonl"
"#,
        );
        let config = Config::from_path(&path).unwrap();
        assert!(matches!(config.session_backend(), SessionBackend::Jsonl));

        // Greptime with connection string
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=4002 dbname=public"
"#,
        );
        let config = Config::from_path(&path).unwrap();
        match config.session_backend() {
            SessionBackend::Greptime { conn } => {
                assert_eq!(conn, "host=127.0.0.1 port=4002 dbname=public");
            }
            _ => panic!("expected Greptime backend"),
        }
    }

    #[test]
    fn resolves_toml_profile() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
reasoning_effort = "max"
"#,
        );
        let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
        assert_eq!(resolved.base_url, "https://api.kimi.com/coding/v1");
        assert_eq!(resolved.model, "k3");
        assert_eq!(resolved.display, "kimi/k3");
        assert_eq!(resolved.api_key, "key");
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("max"));
        assert!(resolved.context_window.is_none());
    }

    #[test]
    fn resolves_context_window_from_model_profile() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
context_window = 131072
"#,
        );
        let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
        assert_eq!(resolved.context_window, Some(131072));
    }

    #[test]
    fn vision_defaults_to_false_and_reads_from_model_profile() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
vision = true
"#,
        );
        let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
        assert!(resolved.vision);

        // Absent `vision` means false (deepseek-style default).
        let path = write_config(
            temp.path(),
            r#"
default = "deepseek/v3"
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_file = "key"
[models."deepseek/v3"]
model = "deepseek-chat"
"#,
        );
        let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
        assert!(!resolved.vision);
    }

    #[test]
    fn roles_main_falls_back_when_no_default() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[roles]
main = "kimi/k3"
subagent = "kimi/k3"
"#,
        );
        let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
        assert_eq!(resolved.model, "k3");
        assert_eq!(resolved.display, "kimi/k3");
    }

    #[test]
    fn explicit_default_wins_over_roles_main() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k2"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k2"]
model = "k2"
[models."kimi/k3"]
model = "k3"
[roles]
main = "kimi/k3"
"#,
        );
        let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
        assert_eq!(
            resolved.model, "k2",
            "explicit default wins over [roles] main"
        );
    }

    #[test]
    fn web_search_key_from_file_env_or_absent() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("exa-key"), "  exa-secret\n").unwrap();

        // Absent section: no key.
        let bare = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://x"
api_key_file = "exa-key"
[models."kimi/k3"]
model = "k3"
"#,
        );
        assert_eq!(
            Config::from_path(&bare).unwrap().web_search_key().unwrap(),
            None
        );

        // api_key_file (relative to the config file's directory).
        let with_file = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://x"
api_key_file = "exa-key"
[models."kimi/k3"]
model = "k3"
[web_search]
api_key_file = "exa-key"
"#,
        );
        assert_eq!(
            Config::from_path(&with_file)
                .unwrap()
                .web_search_key()
                .unwrap(),
            Some("exa-secret".to_owned())
        );

        // Both set: rejected.
        let both = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://x"
api_key_file = "exa-key"
[models."kimi/k3"]
model = "k3"
[web_search]
api_key_file = "exa-key"
api_key_env = "SOME_VAR"
"#,
        );
        assert!(
            Config::from_path(&both)
                .unwrap()
                .web_search_key()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn resolves_role_routing_and_falls_back_when_unrouted() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[models."kimi/k2"]
model = "k2"
[roles]
subagent = "kimi/k2"
"#,
        );
        let config = Config::from_path(&path).unwrap();
        let subagent = config.resolve_role("subagent").unwrap().unwrap();
        assert_eq!(subagent.model, "k2");
        assert_eq!(subagent.display, "kimi/k2");
        assert!(config.resolve_role("reviewer").unwrap().is_none());
    }

    #[test]
    fn reports_missing_role_profile() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("key"), "key").unwrap();
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[roles]
subagent = "kimi/nope"
"#,
        );
        assert!(
            Config::from_path(&path)
                .unwrap()
                .resolve_role("subagent")
                .unwrap_err()
                .to_string()
                .contains("model profile `kimi/nope` is not defined")
        );
    }

    #[test]
    fn sandbox_project_selects_and_narrows_global_writable() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let external = temp.path().join("external");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(external.join("child")).unwrap();
        let path = write_config(
            temp.path(),
            &format!(
                r#"
[sandbox]
enabled = false
writable_paths = ["{}"]
"#,
                external.display()
            ),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!(
                r#"
[sandbox]
writable_paths = ["{}"]
"#,
                external.join("child").display()
            ),
        )
        .unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert!(!sandbox.enabled);
        assert_eq!(
            sandbox.writable_paths,
            vec![external.join("child").to_str().unwrap()],
            "global writable root is narrowed to the project subpath"
        );
        assert!(sandbox.readable_paths.is_empty());
    }

    #[test]
    fn sandbox_project_readable_child_of_global_writable_is_rejected() {
        // Merging keeps the global writable root, so a project read-only
        // child of it would be a read-only child under a writable root —
        // rejected by normalize_roots instead of silently re-adding write
        // authority.
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let external = temp.path().join("external");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(external.join("child")).unwrap();
        let path = write_config(
            temp.path(),
            &format!("[sandbox]\nwritable_paths = [\"{}\"]\n", external.display()),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!(
                "[sandbox]\nreadable_paths = [\"{}\"]\n",
                external.join("child").display()
            ),
        )
        .unwrap();
        let error = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("read-only child") && error.contains("unsupported"),
            "{error}"
        );
    }

    #[test]
    fn sandbox_project_accumulates_unrelated_roots_and_narrows() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let keep = temp.path().join("keep");
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        let path = write_config(
            temp.path(),
            &format!(
                "[sandbox]\nwritable_paths = [\"{}\", \"{}\"]\n",
                keep.display(),
                parent.display()
            ),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!("[sandbox]\nwritable_paths = [\"{}\"]\n", child.display()),
        )
        .unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(
            sandbox.writable_paths,
            vec![keep.to_str().unwrap(), child.to_str().unwrap()],
            "unrelated global root accumulated, ancestor replaced by subpath"
        );
        assert!(sandbox.readable_paths.is_empty());
    }

    #[test]
    fn sandbox_project_narrows_global_writable_root() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let external = temp.path().join("external");
        let child = external.join("child");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        let path = write_config(
            temp.path(),
            &format!("[sandbox]\nwritable_paths = [\"{}\"]\n", external.display()),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!("[sandbox]\nwritable_paths = [\"{}\"]\n", child.display()),
        )
        .unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(
            sandbox.writable_paths,
            vec![child.to_str().unwrap()],
            "global root replaced by the narrower project subpath"
        );
        assert!(
            !sandbox
                .writable_paths
                .contains(&external.to_str().unwrap().to_owned())
        );
        assert!(sandbox.readable_paths.is_empty());
    }

    #[test]
    fn sandbox_project_multiple_narrowing_subpaths_all_survive() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let external = temp.path().join("external");
        let x = external.join("x");
        let z = external.join("z");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&x).unwrap();
        std::fs::create_dir_all(&z).unwrap();
        let path = write_config(
            temp.path(),
            &format!("[sandbox]\nwritable_paths = [\"{}\"]\n", external.display()),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!(
                "[sandbox]\nwritable_paths = [\"{}\", \"{}\"]\n",
                x.display(),
                z.display()
            ),
        )
        .unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(
            sandbox.writable_paths,
            vec![x.to_str().unwrap(), z.to_str().unwrap()],
            "both narrowing subpaths kept, ancestor dropped"
        );
    }

    #[test]
    fn sandbox_project_equal_to_global_is_noop() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let external = temp.path().join("external");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let path = write_config(
            temp.path(),
            &format!("[sandbox]\nwritable_paths = [\"{}\"]\n", external.display()),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!("[sandbox]\nwritable_paths = [\"{}\"]\n", external.display()),
        )
        .unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(
            sandbox.writable_paths,
            vec![external.to_str().unwrap()],
            "project root equal to the global root changes nothing"
        );
    }

    #[test]
    fn sandbox_without_project_config_is_pure_global() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let writable = temp.path().join("writable");
        let readable = temp.path().join("readable");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&writable).unwrap();
        std::fs::create_dir_all(&readable).unwrap();
        let path = write_config(
            temp.path(),
            &format!(
                "[sandbox]\nwritable_paths = [\"{}\"]\nreadable_paths = [\"{}\"]\n",
                writable.display(),
                readable.display()
            ),
        );
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(sandbox.writable_paths, vec![writable.to_str().unwrap()]);
        assert_eq!(sandbox.readable_paths, vec![readable.to_str().unwrap()]);
    }

    #[test]
    fn sandbox_project_subset_validation_still_applies() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let external = temp.path().join("external");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let path = write_config(
            temp.path(),
            &format!("[sandbox]\nwritable_paths = [\"{}\"]\n", external.display()),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!("[sandbox]\nwritable_paths = [\"{}\"]\n", outside.display()),
        )
        .unwrap();
        let error = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not within a global writable root"),
            "{error}"
        );
    }

    #[test]
    fn sandbox_normalize_does_not_undo_narrowing() {
        // The narrowed child must survive normalize_roots (which folds
        // children into parents): the global ancestor is gone, so nothing
        // re-expands it.
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let keep = temp.path().join("keep");
        let external = temp.path().join("external");
        let child = external.join("child");
        let readable = temp.path().join("readable");
        let readable_child = readable.join("rc");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        for dir in [&keep, &child, &readable_child] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let path = write_config(
            temp.path(),
            &format!(
                "[sandbox]\nwritable_paths = [\"{}\", \"{}\"]\nreadable_paths = [\"{}\"]\n",
                keep.display(),
                external.display(),
                readable.display()
            ),
        );
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!(
                "[sandbox]\nwritable_paths = [\"{}\"]\nreadable_paths = [\"{}\"]\n",
                child.display(),
                readable_child.display()
            ),
        )
        .unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(
            sandbox.writable_paths,
            vec![keep.to_str().unwrap(), child.to_str().unwrap()],
            "writable narrowing survives normalize_roots; unrelated root kept"
        );
        assert_eq!(
            sandbox.readable_paths,
            vec![readable_child.to_str().unwrap()],
            "readable narrowing survives normalize_roots"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_rejects_read_only_child_under_writable_parent_after_canonicalization() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let writable = temp.path().join("writable");
        let child = writable.join("child");
        let alias = temp.path().join("alias");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        symlink(&child, &alias).unwrap();
        let path = write_config(
            temp.path(),
            &format!("[sandbox]\nwritable_paths = [\"{}\"]\n", writable.display()),
        );
        let config = Config::from_path(&path).unwrap();
        for selected in [&child, &alias] {
            std::fs::write(
                workspace.join(".e-agent/config.toml"),
                format!(
                    "[sandbox]\nwritable_paths = [\"{}\"]\nreadable_paths = [\"{}\"]\n",
                    writable.display(),
                    selected.display()
                ),
            )
            .unwrap();
            let error = config.sandbox(&workspace).unwrap_err().to_string();
            assert!(
                error.contains("read-only child") && error.contains("unsupported"),
                "{error}"
            );
        }
    }

    #[test]
    fn sandbox_allows_read_only_parent_with_writable_child() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        let path = write_config(
            temp.path(),
            &format!(
                "[sandbox]\nreadable_paths = [\"{}\"]\nwritable_paths = [\"{}\"]\n",
                parent.display(),
                child.display()
            ),
        );
        let policy = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .unwrap();
        assert_eq!(policy.readable_paths, vec![parent.to_str().unwrap()]);
        assert_eq!(policy.writable_paths, vec![child.to_str().unwrap()]);
    }

    #[test]
    fn project_sandbox_rejects_unknown_and_policy_switch_fields() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        let path = write_config(temp.path(), "");
        let config = Config::from_path(&path).unwrap();
        for field in [
            "readible_paths = []",
            "enabled = true",
            "network = false",
            "workspace_writable = false",
        ] {
            std::fs::write(
                workspace.join(".e-agent/config.toml"),
                format!("[other]\nfuture = true\n[sandbox]\n{field}\n"),
            )
            .unwrap();
            let error = config.sandbox(&workspace).unwrap_err().to_string();
            assert!(error.contains("cannot parse project config"), "{error}");
        }
    }

    #[test]
    fn sandbox_project_empty_rejections_aliases_and_malformed() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let writable = temp.path().join("writable");
        let readable = temp.path().join("readable");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        std::fs::create_dir_all(writable.join("child")).unwrap();
        std::fs::create_dir_all(&readable).unwrap();
        let path = write_config(
            temp.path(),
            &format!(
                r#"
[sandbox]
writable_paths = ["{}", "{}"]
readable_paths = ["{}"]
"#,
                writable.display(),
                writable.join(".").display(),
                readable.display()
            ),
        );
        let config = Config::from_path(&path).unwrap();

        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            "[sandbox]\nwritable_paths = []\nreadable_paths = []\n",
        )
        .unwrap();
        // Empty project arrays no longer clear the global policy: with
        // nothing to narrow or accumulate, the merged policy is pure global.
        let empty = config.sandbox(&workspace).unwrap();
        assert_eq!(empty.writable_paths, vec![writable.to_str().unwrap()]);
        assert_eq!(empty.readable_paths, vec![readable.to_str().unwrap()]);

        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!("[sandbox]\nwritable_paths = [\"{}\"]\n", readable.display()),
        )
        .unwrap();
        assert!(
            config
                .sandbox(&workspace)
                .unwrap_err()
                .to_string()
                .contains("global writable")
        );

        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            format!(
                "[sandbox]\nreadable_paths = [\"{}\"]\n",
                temp.path().display()
            ),
        )
        .unwrap();
        assert!(
            config
                .sandbox(&workspace)
                .unwrap_err()
                .to_string()
                .contains("global readable or writable")
        );

        std::fs::write(workspace.join(".e-agent/config.toml"), "[sandbox\n").unwrap();
        assert!(
            config
                .sandbox(&workspace)
                .unwrap_err()
                .to_string()
                .contains("cannot parse project config")
        );

        std::fs::remove_file(workspace.join(".e-agent/config.toml")).unwrap();
        let inherited = config.sandbox(&workspace).unwrap();
        assert_eq!(
            inherited.writable_paths.len(),
            1,
            "canonical aliases deduplicate"
        );
    }

    #[test]
    fn sandbox_absent_is_empty_policy() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_config(temp.path(), "");
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(temp.path())
            .unwrap();
        assert!(!sandbox.enabled);
        assert!(sandbox.writable_paths.is_empty());
    }

    #[test]
    fn resolves_relative_key_file_and_trims_it() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("nested");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::write(config_dir.join("key"), " \n secret-key \t\n").unwrap();
        let path = write_config(
            &config_dir,
            r#"
[providers.kimi]
base_url = "https://example.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
"#,
        );
        assert_eq!(
            Config::from_path(&path)
                .unwrap()
                .resolve(Some("kimi/k3"))
                .unwrap()
                .api_key,
            "secret-key"
        );
    }

    #[test]
    fn reports_missing_model_provider_and_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let missing_model = write_config(temp.path(), "default = \"kimi/k3\"");
        assert!(
            Config::from_path(&missing_model)
                .unwrap()
                .resolve(None)
                .unwrap_err()
                .to_string()
                .contains("model profile `kimi/k3` is not defined")
        );

        let missing_provider = write_config(temp.path(), "[models.\"kimi/k3\"]\nmodel = \"k3\"");
        assert!(
            Config::from_path(&missing_provider)
                .unwrap()
                .resolve(Some("kimi/k3"))
                .unwrap_err()
                .to_string()
                .contains("provider `kimi` for profile `kimi/k3` is not defined")
        );

        let missing_credentials = write_config(
            temp.path(),
            "[providers.kimi]\nbase_url = \"https://example.test/v1\"\n[models.\"kimi/k3\"]\nmodel = \"k3\"",
        );
        assert!(
            Config::from_path(&missing_credentials)
                .unwrap()
                .resolve(Some("kimi/k3"))
                .unwrap_err()
                .to_string()
                .contains("requires exactly one of `api_key_file` or `api_key_env`")
        );
    }

    #[test]
    fn resolves_chatgpt_and_rejects_mixed_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let good = write_config(
            temp.path(),
            r#"
[providers.chatgpt]
auth = "chatgpt"
[models."chatgpt/codex"]
model = "gpt-5.6-sol"
"#,
        );
        let resolved = Config::from_path(&good)
            .unwrap()
            .resolve(Some("chatgpt/codex"))
            .unwrap();
        assert_eq!(resolved.auth, AuthMode::ChatGpt);
        assert_eq!(resolved.display, "chatgpt/codex");
        for field in [
            "base_url = \"https://example.test\"",
            "api_key_file = \"key\"",
            "api_key_env = \"KEY\"",
        ] {
            let path = write_config(
                temp.path(),
                &format!(
                    r#"
[providers.chatgpt]
auth = "chatgpt"
{field}
[models."chatgpt/codex"]
model = "codex"
"#
                ),
            );
            assert!(
                Config::from_path(&path)
                    .unwrap()
                    .resolve(Some("chatgpt/codex"))
                    .unwrap_err()
                    .to_string()
                    .contains("cannot set")
            );
        }
    }
}
