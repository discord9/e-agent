//! Role templates: each role's system prompt lives in
//! `.e-agent/agents/<role>.md` inside the workspace. A role only exists when
//! its file does — there are no built-in role prompts. The `delegate` tool
//! lists the available roles and routes each subagent onto the role's model
//! (from `[roles]` in the config) and prompt.

use std::path::Path;

/// Directory (relative to the workspace root) holding role templates.
const AGENTS_DIR: &str = ".e-agent/agents";

/// The role whose template, when present, is injected into the MAIN agent as
/// its orchestrator instructions.
pub const MAIN_ROLE: &str = "main";

/// Read the template for `role`, or `None` when the file does not exist.
/// Returns an error only when the file exists but cannot be read.
pub fn role_prompt(root: &Path, role: &str) -> std::io::Result<Option<String>> {
    if !valid_role(role) {
        return Ok(None);
    }
    let path = root.join(AGENTS_DIR).join(format!("{role}.md"));
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Ok(Some(content)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Role names available in this workspace: the stems of `.e-agent/agents/*.md`,
/// sorted, excluding the main-agent template (it is not a delegable role).
pub fn available_roles(root: &Path) -> Vec<String> {
    let mut roles = Vec::new();
    let directory = root.join(AGENTS_DIR);
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(role) = name.strip_suffix(".md") else {
                continue;
            };
            if valid_role(role) && role != MAIN_ROLE {
                roles.push(role.to_owned());
            }
        }
    }
    roles.sort();
    roles
}

/// Role names become file names, so keep them to a safe charset.
fn valid_role(role: &str) -> bool {
    !role.is_empty()
        && role
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_come_from_files_only() {
        let temp = tempfile::tempdir().unwrap();
        // No agents directory: no roles, no prompts.
        assert!(available_roles(temp.path()).is_empty());
        assert_eq!(role_prompt(temp.path(), "explorer").unwrap(), None);

        let directory = temp.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("fixer.md"), "You fix things.").unwrap();
        std::fs::write(directory.join("explorer.md"), "You explore.").unwrap();
        // The main template is not a delegable role.
        std::fs::write(directory.join("main.md"), "You orchestrate.").unwrap();
        // Not a role file.
        std::fs::write(directory.join("notes.txt"), "nope").unwrap();

        assert_eq!(available_roles(temp.path()), vec!["explorer", "fixer"]);
        assert_eq!(
            role_prompt(temp.path(), "fixer").unwrap().as_deref(),
            Some("You fix things.")
        );
        assert_eq!(role_prompt(temp.path(), "oracle").unwrap(), None);
    }

    #[test]
    fn invalid_role_names_read_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(role_prompt(temp.path(), "../etc/passwd").unwrap(), None);
        assert_eq!(role_prompt(temp.path(), "").unwrap(), None);
    }
}
