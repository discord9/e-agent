//! Role templates: each role's system prompt lives in a Markdown file named
//! `<role>.md`. Roles come from two layers, merged with the workspace
//! overriding the global user config:
//!
//! - global:    `$XDG_CONFIG_HOME/e-agent/agents/` (or `~/.config/e-agent/agents/`)
//! - workspace: `<workspace>/agents/`
//!
//! A role only exists when its file does — there are no built-in role
//! prompts. The `delegate` tool lists the available roles and routes each
//! subagent onto the role's model (from `[roles]` in the config) and prompt.
//! `main.md` is special: it is the MAIN agent's orchestrator template, not a
//! delegable role.

use std::path::{Path, PathBuf};

/// Directory (relative to each layer root) holding role templates.
const AGENTS_DIR: &str = "agents";

/// The role whose template, when present, is injected into the MAIN agent as
/// its orchestrator instructions.
pub const MAIN_ROLE: &str = "main";

/// The global (user-level) agents directory: alongside `config.toml`.
fn global_agents_dir() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join(AGENTS_DIR))
}

/// The two layer roots, lowest priority first: global, then workspace.
fn layer_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(global) = global_agents_dir() {
        dirs.push(global);
    }
    dirs.push(workspace.join(AGENTS_DIR));
    dirs
}

/// Read the template for `role`: the workspace file wins over the global one.
/// Returns `None` when neither exists; an error only when a file exists but
/// cannot be read.
pub fn role_prompt(workspace: &Path, role: &str) -> std::io::Result<Option<String>> {
    if !valid_role(role) {
        return Ok(None);
    }
    let file = format!("{role}.md");
    // Later layers override earlier ones, so take the LAST readable hit.
    let mut found = None;
    for dir in layer_dirs(workspace) {
        match std::fs::read_to_string(dir.join(&file)) {
            Ok(content) if !content.trim().is_empty() => found = Some(content),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(found)
}

/// Role names available across both layers (union), sorted, excluding the
/// main-agent template (it is not a delegable role).
pub fn available_roles(workspace: &Path) -> Vec<String> {
    let mut roles = std::collections::BTreeSet::new();
    for dir in layer_dirs(workspace) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let Some(role) = name.strip_suffix(".md") else {
                    continue;
                };
                if valid_role(role) && role != MAIN_ROLE {
                    roles.insert(role.to_owned());
                }
            }
        }
    }
    roles.into_iter().collect()
}

/// Role names become file names, so keep them to a safe charset.
fn valid_role(role: &str) -> bool {
    !role.is_empty()
        && role
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Serializes tests that mutate the shared XDG_CONFIG_HOME env var.
/// Shared across modules (delegate tests use it too).
#[cfg(test)]
pub(crate) static XDG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_overrides_global_and_unions() {
        let _guard = XDG_TEST_LOCK.lock().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        // Point the global layer at our temp dir via XDG.
        // (config_dir honours XDG_CONFIG_HOME.)
        let xdg = global.path().join("xdg");
        std::fs::create_dir_all(xdg.join("e-agent/agents")).unwrap();
        std::fs::write(xdg.join("e-agent/agents/explorer.md"), "global explorer").unwrap();
        std::fs::write(xdg.join("e-agent/agents/fixer.md"), "global fixer").unwrap();
        // SAFETY: single-threaded test process; no other thread reads env here.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

        // Workspace provides its own fixer (overrides) plus a project role.
        let ws_agents = workspace.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&ws_agents).unwrap();
        std::fs::write(ws_agents.join("fixer.md"), "workspace fixer").unwrap();
        std::fs::write(ws_agents.join("reviewer.md"), "workspace reviewer").unwrap();

        assert_eq!(
            available_roles(workspace.path()),
            vec!["explorer", "fixer", "reviewer"]
        );
        assert_eq!(
            role_prompt(workspace.path(), "fixer").unwrap().as_deref(),
            Some("workspace fixer"),
            "workspace must override global"
        );
        assert_eq!(
            role_prompt(workspace.path(), "explorer")
                .unwrap()
                .as_deref(),
            Some("global explorer"),
            "global role still available when not overridden"
        );
        assert_eq!(role_prompt(workspace.path(), "oracle").unwrap(), None);

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn invalid_role_names_read_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(role_prompt(temp.path(), "../etc/passwd").unwrap(), None);
        assert_eq!(role_prompt(temp.path(), "").unwrap(), None);
    }
}
