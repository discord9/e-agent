//! Role templates: each role's system prompt lives in a Markdown file named
//! `<role>.md`, optionally led by a TOML frontmatter block
//! (`---` … `---`) declaring role attributes such as `read_only = true`.
//! Roles come from two layers, merged with the workspace overriding the
//! global user config:
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

/// A role's resolved template: the system prompt plus role attributes parsed
/// from an optional TOML frontmatter block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTemplate {
    /// The role's system prompt (frontmatter stripped when present).
    pub prompt: String,
    /// `read_only` capability declaration from the frontmatter (default
    /// false). A read-only role gets no write/edit tools and its bash (when
    /// sandboxed) runs in a narrowed read-only sandbox with network disabled.
    pub read_only: bool,
}

/// Frontmatter metadata parsed from the leading TOML block of a role file.
/// Unknown keys are ignored (serde default); `read_only` defaults to false.
#[derive(serde::Deserialize)]
struct RoleMeta {
    read_only: Option<bool>,
}

/// Read the template for `role`, including frontmatter attributes: the
/// workspace file wins over the global one. Returns `None` when neither
/// exists; an error when a file exists but cannot be read, or when its
/// frontmatter is malformed (first line `---` without a closing delimiter,
/// or a TOML block that fails to parse — fail closed, the role is rejected).
///
/// Both the prompt and the `read_only` flag always come from the SAME
/// (winning) file: the workspace layer carries its own frontmatter, exactly
/// like the global layer.
pub fn role_template(workspace: &Path, role: &str) -> std::io::Result<Option<RoleTemplate>> {
    if !valid_role(role) {
        return Ok(None);
    }
    let file = format!("{role}.md");
    // Later layers override earlier ones, so take the LAST readable hit.
    // Empty files are skipped, matching role_prompt's historical behaviour.
    let mut found = None;
    for dir in layer_dirs(workspace) {
        match std::fs::read_to_string(dir.join(&file)) {
            Ok(content) if !content.trim().is_empty() => {
                found = Some(parse_role_template(content)?);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(found)
}

/// Parse one role file. Frontmatter mode starts only when the file's FIRST
/// line is exactly `---`; the block runs until the next `---` line and must
/// parse as TOML. Without a leading `---` the whole file is the prompt and
/// `read_only` is false. Anything else is fail-closed: an unclosed opener or
/// invalid TOML is an error (the role is rejected rather than partially
/// applied).
fn parse_role_template(content: String) -> std::io::Result<RoleTemplate> {
    // Frontmatter mode starts only when the file's FIRST line is exactly
    // `---`.
    let first_line_end = content.find('\n').map(|i| i + 1).unwrap_or(content.len());
    let first_line = content[..first_line_end].trim_end_matches(['\n', '\r']);
    if first_line != "---" {
        return Ok(RoleTemplate {
            prompt: content,
            read_only: false,
        });
    }
    // The block between the two `---` lines is the frontmatter.
    let mut frontmatter = String::new();
    let mut rest = &content[first_line_end..];
    let after = loop {
        let line_end = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let line = &rest[..line_end];
        if line.trim_end_matches(['\n', '\r']) == "---" {
            break &rest[line_end..];
        }
        frontmatter.push_str(line);
        rest = &rest[line_end..];
        if rest.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "role template starts with `---` but has no closing `---` delimiter",
            ));
        }
    };
    let meta: RoleMeta = toml::from_str(&frontmatter).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("role template frontmatter is not valid TOML: {error}"),
        )
    })?;
    Ok(RoleTemplate {
        prompt: after.to_owned(),
        read_only: meta.read_only.unwrap_or(false),
    })
}

/// Read the prompt for `role`, ignoring frontmatter attributes. The workspace
/// file wins over the global one. Returns `None` when neither exists; an
/// error when a file exists but cannot be read or has malformed frontmatter.
pub fn role_prompt(workspace: &Path, role: &str) -> std::io::Result<Option<String>> {
    Ok(role_template(workspace, role)?.map(|template| template.prompt))
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
        std::fs::write(xdg.join("e-agent/agents/oracle.md"), "global oracle").unwrap();
        // SAFETY: single-threaded test process; no other thread reads env here.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

        // Workspace provides its own fixer (overrides) plus a project role.
        let ws_agents = workspace.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&ws_agents).unwrap();
        std::fs::write(ws_agents.join("fixer.md"), "workspace fixer").unwrap();
        std::fs::write(ws_agents.join("reviewer.md"), "workspace reviewer").unwrap();

        assert_eq!(
            available_roles(workspace.path()),
            vec!["explorer", "fixer", "oracle", "reviewer"]
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
        assert_eq!(
            role_prompt(workspace.path(), "oracle").unwrap().as_deref(),
            Some("global oracle"),
            "oracle resolves from the global layer like any role"
        );
        assert_eq!(role_prompt(workspace.path(), "nonexistent").unwrap(), None);

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn invalid_role_names_read_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(role_prompt(temp.path(), "../etc/passwd").unwrap(), None);
        assert_eq!(role_prompt(temp.path(), "").unwrap(), None);
    }

    /// Isolated global layer + workspace agents dir helper, so tests never
    /// read the developer's real `~/.config/e-agent/agents`.
    fn isolated_layers(workspace: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let xdg = workspace.path().join("xdg");
        let global = xdg.join("e-agent/agents");
        std::fs::create_dir_all(&global).unwrap();
        // SAFETY: serialized by XDG_TEST_LOCK; no other thread reads env.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let ws_agents = workspace.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&ws_agents).unwrap();
        (global, ws_agents)
    }

    #[test]
    fn frontmatter_read_only_flag_and_prompt_stripping() {
        let _guard = XDG_TEST_LOCK.lock().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (_, ws_agents) = isolated_layers(&workspace);

        // No frontmatter: the whole file is the prompt, flag defaults false.
        std::fs::write(ws_agents.join("plain.md"), "You fix things.").unwrap();
        let template = role_template(workspace.path(), "plain").unwrap().unwrap();
        assert_eq!(template.prompt, "You fix things.");
        assert!(!template.read_only);

        // Frontmatter: the block is stripped and the flag is parsed.
        std::fs::write(
            ws_agents.join("auditor.md"),
            "---\nread_only = true\n---\nAudit only.\n",
        )
        .unwrap();
        let template = role_template(workspace.path(), "auditor").unwrap().unwrap();
        assert_eq!(template.prompt, "Audit only.\n");
        assert!(template.read_only, "read_only = true must set the flag");

        // read_only = false stays false; role_prompt returns the stripped prompt.
        std::fs::write(
            ws_agents.join("helper.md"),
            "---\nread_only = false\n---\nHelp only.",
        )
        .unwrap();
        let template = role_template(workspace.path(), "helper").unwrap().unwrap();
        assert!(!template.read_only);
        assert_eq!(
            role_prompt(workspace.path(), "helper").unwrap().as_deref(),
            Some("Help only."),
            "role_prompt must agree with role_template on the winning prompt"
        );

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn frontmatter_ignores_unknown_keys() {
        let _guard = XDG_TEST_LOCK.lock().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (_, ws_agents) = isolated_layers(&workspace);

        std::fs::write(
            ws_agents.join("future.md"),
            "---\nread_only = true\nmodel = \"k2\"\nbudget = 100\n---\nPrompt text.",
        )
        .unwrap();
        let template = role_template(workspace.path(), "future").unwrap().unwrap();
        assert!(template.read_only);
        assert_eq!(template.prompt, "Prompt text.");
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn frontmatter_fails_closed_on_unclosed_or_invalid_toml() {
        let _guard = XDG_TEST_LOCK.lock().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (_, ws_agents) = isolated_layers(&workspace);

        // First line `---` without a closing delimiter: error, not silence.
        std::fs::write(
            ws_agents.join("unclosed.md"),
            "---\nread_only = true\nno closing line",
        )
        .unwrap();
        let error = role_template(workspace.path(), "unclosed").unwrap_err();
        assert!(
            error.to_string().contains("no closing `---` delimiter"),
            "{error}"
        );

        // TOML parse failure: error.
        std::fs::write(
            ws_agents.join("bad.md"),
            "---\nread_only = maybe\n---\nPrompt",
        )
        .unwrap();
        let error = role_template(workspace.path(), "bad").unwrap_err();
        assert!(error.to_string().contains("not valid TOML"), "{error}");
        assert!(
            role_prompt(workspace.path(), "bad").is_err(),
            "role_prompt inherits the fail-closed frontmatter error"
        );

        // A `---` later in the file (not the first line) is ordinary text.
        std::fs::write(
            ws_agents.join("table.md"),
            "Intro\n---\nread_only = true\n---\nmore",
        )
        .unwrap();
        let template = role_template(workspace.path(), "table").unwrap().unwrap();
        assert!(!template.read_only);
        assert_eq!(template.prompt, "Intro\n---\nread_only = true\n---\nmore");

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn frontmatter_flag_comes_from_the_winning_workspace_layer() {
        let _guard = XDG_TEST_LOCK.lock().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (global, ws_agents) = isolated_layers(&workspace);

        // Global declares read_only; the workspace override does not.
        std::fs::write(
            global.join("explorer.md"),
            "---\nread_only = true\n---\nglobal explorer",
        )
        .unwrap();
        std::fs::write(ws_agents.join("explorer.md"), "workspace explorer").unwrap();
        let template = role_template(workspace.path(), "explorer")
            .unwrap()
            .unwrap();
        assert_eq!(template.prompt, "workspace explorer");
        assert!(
            !template.read_only,
            "the flag must come from the winning workspace file, not the global one"
        );

        // And the reverse: only the workspace file carries the flag.
        std::fs::write(global.join("reviewer.md"), "global reviewer").unwrap();
        std::fs::write(
            ws_agents.join("reviewer.md"),
            "---\nread_only = true\n---\nworkspace reviewer",
        )
        .unwrap();
        let template = role_template(workspace.path(), "reviewer")
            .unwrap()
            .unwrap();
        assert_eq!(template.prompt, "workspace reviewer");
        assert!(template.read_only);

        // Global-only role still resolves its own frontmatter.
        std::fs::write(
            global.join("archiver.md"),
            "---\nread_only = true\n---\nglobal archiver",
        )
        .unwrap();
        let template = role_template(workspace.path(), "archiver")
            .unwrap()
            .unwrap();
        assert_eq!(template.prompt, "global archiver");
        assert!(template.read_only);

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
