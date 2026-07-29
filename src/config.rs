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
    #[serde(skip)]
    path: PathBuf,
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
}

#[derive(Debug)]
pub struct ResolvedModel {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub auth: AuthMode,
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
            .ok_or_else(|| anyhow!("config requires `default` or --profile PROFILE"))?;
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
        assert_eq!(resolved.api_key, "key");
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("max"));
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
