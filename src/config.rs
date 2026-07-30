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
    /// Optional `[sandbox]` bwrap wrapper for the bash tool.
    #[serde(default)]
    sandbox: Option<Sandbox>,
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
    /// Extra paths mounted read-write inside the sandbox, beyond the
    /// workspace. Global and project configs merge (union). `~` expands to
    /// the home directory; relative paths resolve against the workspace root.
    #[serde(default)]
    pub writable_paths: Vec<String>,
    /// Extra paths mounted read-only inside the sandbox. Same merge and
    /// resolution rules as `writable_paths`.
    #[serde(default)]
    pub readable_paths: Vec<String>,
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

    /// The bash sandbox, only when `[sandbox] enabled = true`. Otherwise None.
    ///
    /// Merges the project-local `<workspace>/.e-agent/config.toml` on top of
    /// this (global) config: `writable_paths` / `readable_paths` are unioned,
    /// while `enabled` / `network` / `workspace_writable` come from the global
    /// config only (a project file can add paths but never weaken the policy).
    /// Paths are expanded: `~` -> home, relative -> the workspace root.
    pub fn sandbox(&self, workspace: &Path) -> Option<Sandbox> {
        let mut sandbox = self.sandbox.clone().filter(|sandbox| sandbox.enabled)?;
        if let Some(project) = project_sandbox(workspace) {
            sandbox.writable_paths.extend(project.writable_paths);
            sandbox.readable_paths.extend(project.readable_paths);
        }
        for paths in [&mut sandbox.writable_paths, &mut sandbox.readable_paths] {
            for path in paths.iter_mut() {
                *path = expand_sandbox_path(path, workspace);
            }
            paths.sort();
            paths.dedup();
        }
        Some(sandbox)
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

/// The `[sandbox]` section of the project-local config at
/// `<workspace>/.e-agent/config.toml`, if present and parseable. Only the
/// sandbox is read; a malformed file is ignored (the sandbox must not fail
/// closed/open because of a typo in an optional local file).
fn project_sandbox(workspace: &Path) -> Option<Sandbox> {
    #[derive(Deserialize)]
    struct ProjectConfig {
        sandbox: Option<Sandbox>,
    }
    let path = workspace.join(".e-agent/config.toml");
    let source = std::fs::read_to_string(path).ok()?;
    toml::from_str::<ProjectConfig>(&source).ok()?.sandbox
}

/// Expand a sandbox path: a leading `~` becomes the home directory, and a
/// relative path is resolved against the workspace root (then normalized
/// lexically, not symlink-resolved — the sandbox binds the literal path).
fn expand_sandbox_path(path: &str, workspace: &Path) -> String {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            candidate
        } else {
            workspace.join(candidate)
        }
    };
    expanded.to_string_lossy().into_owned()
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
    fn sandbox_merges_project_paths_and_expands_them() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
        // Global config: enabled, one writable path with ~, one absolute.
        let path = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://x"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[sandbox]
enabled = true
network = false
writable_paths = ["~/.cargo/registry", "/mnt/nvme_rust/sccache"]
readable_paths = ["~/.gitconfig"]
"#,
        );
        std::fs::write(temp.path().join("key"), "key").unwrap();
        // Project-local: adds paths (cannot flip `network` back on).
        std::fs::write(
            workspace.join(".e-agent/config.toml"),
            r#"
[sandbox]
enabled = false
network = true
writable_paths = ["target", "~/.cargo/registry"]
readable_paths = ["/opt/shared"]
"#,
        )
        .unwrap();

        let home = std::env::var("HOME").unwrap();
        let sandbox = Config::from_path(&path)
            .unwrap()
            .sandbox(&workspace)
            .expect("sandbox enabled globally");

        // Policy switches come from the global config only.
        assert!(!sandbox.network, "project file must not weaken policy");
        // Paths merged (union) and expanded; project duplicate deduped.
        let cargo = format!("{home}/.cargo/registry");
        assert!(sandbox.writable_paths.contains(&cargo));
        assert!(
            sandbox
                .writable_paths
                .contains(&"/mnt/nvme_rust/sccache".to_owned())
        );
        assert!(
            sandbox
                .writable_paths
                .contains(&workspace.join("target").to_string_lossy().into_owned())
        );
        assert_eq!(
            sandbox
                .writable_paths
                .iter()
                .filter(|p| **p == cargo)
                .count(),
            1,
            "global + project duplicate must be deduped"
        );
        assert!(
            sandbox
                .readable_paths
                .contains(&format!("{home}/.gitconfig"))
        );
        assert!(sandbox.readable_paths.contains(&"/opt/shared".to_owned()));
    }

    #[test]
    fn sandbox_absent_or_disabled_is_none() {
        let temp = tempfile::tempdir().unwrap();
        let bare = write_config(
            temp.path(),
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://x"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
"#,
        );
        std::fs::write(temp.path().join("key"), "key").unwrap();
        assert!(
            Config::from_path(&bare)
                .unwrap()
                .sandbox(temp.path())
                .is_none()
        );
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
