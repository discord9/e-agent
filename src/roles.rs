//! Role templates: each role's system prompt lives in a Markdown file named
//! `<role>.md`, optionally led by a TOML frontmatter block
//! (`---` … `---`) declaring role attributes such as `read_only = true` or
//! `protect_git = false`.
//! Roles come from three layers, merged from lowest to highest priority:
//!
//! - global:    `$XDG_CONFIG_HOME/e-agent/agents/` (or `~/.config/e-agent/agents/`)
//! - legacy project: `<workspace>/agents/`
//! - canonical project: `<workspace>/.e-agent/agents/`
//!
//! A role only exists when its file does — there are no built-in role
//! prompts. The `delegate` tool lists the available roles and routes each
//! subagent onto the role's model (from `[roles]` in the config) and prompt.
//! `main.md` is special: it is the MAIN agent's orchestrator template, not a
//! delegable role. A template may also contain a `## Core directive` section;
//! its non-empty body lines through the next Markdown heading form the compacted
//! conversation reminder for agents installed from that template.

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

/// The three layer directories, lowest priority first.
fn layer_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(global) = global_agents_dir() {
        dirs.push(global);
    }
    dirs.push(workspace.join(AGENTS_DIR));
    dirs.push(workspace.join(".e-agent").join(AGENTS_DIR));
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
    /// Optional short description disclosed by the delegate tool.
    pub description: Option<String>,
    /// `protect_git` declaration from the frontmatter (default true). When
    /// true, subagent bash binds `<workspace>/.git` read-only (non-Windows)
    /// — which the Windows write-sandbox MVP cannot enforce, so on Windows a
    /// protect_git subagent's bash fails closed before execution. Set
    /// `protect_git = false` in the role frontmatter to lift that (e.g. to
    /// let a fixer run under the Windows sandbox).
    pub protect_git: bool,
}

/// Frontmatter metadata parsed from the leading TOML block of a role file.
/// Unknown keys are ignored (serde default); `read_only` defaults to false,
/// `protect_git` to true.
#[derive(serde::Deserialize)]
struct RoleMeta {
    read_only: Option<bool>,
    protect_git: Option<bool>,
    description: Option<String>,
}

/// Read the template for `role`, including frontmatter attributes: the
/// canonical project file wins over the legacy project and global ones.
/// Returns `None` when neither exists; an error when a file exists but cannot
/// be read, or when its frontmatter is malformed (first line `---` without a
/// closing delimiter, or a TOML block that fails to parse — fail closed, the
/// role is rejected).
///
/// Both the prompt and all attributes always come from the SAME (winning)
/// file.
pub fn role_template(workspace: &Path, role: &str) -> std::io::Result<Option<RoleTemplate>> {
    if !valid_role(role) {
        return Ok(None);
    }
    let file = format!("{role}.md");
    // Later layers override earlier ones, so take the LAST readable hit.
    // Empty files are skipped, matching role_prompt's historical behaviour.
    // Parse only the winning file: a lower-priority malformed file must not
    // affect a valid higher-priority override, while a malformed winner fails
    // closed instead of falling back.
    let mut found = None;
    for dir in layer_dirs(workspace) {
        match std::fs::read_to_string(dir.join(&file)) {
            Ok(content) if !content.trim().is_empty() => found = Some(content),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    found.map(parse_role_template).transpose()
}

/// Parse one role file. Frontmatter mode starts only when the file's FIRST
/// line is exactly `---`; the block runs until the next `---` line and must
/// parse as TOML. Without a leading `---` the whole file is the prompt,
/// `read_only` is false, and `protect_git` stays true (the subagent
/// default). Anything else is fail-closed: an unclosed opener or invalid
/// TOML is an error (the role is rejected rather than partially applied).
fn parse_role_template(content: String) -> std::io::Result<RoleTemplate> {
    // Frontmatter mode starts only when the file's FIRST line is exactly
    // `---`.
    let first_line_end = content.find('\n').map(|i| i + 1).unwrap_or(content.len());
    let first_line = content[..first_line_end].trim_end_matches(['\n', '\r']);
    if first_line != "---" {
        return Ok(RoleTemplate {
            prompt: content,
            description: None,
            read_only: false,
            protect_git: true,
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
    if meta
        .description
        .as_deref()
        .is_some_and(|description| description.contains(['\n', '\r']))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "role template description must not contain newlines",
        ));
    }
    let description = meta
        .description
        .map(|description| description.trim().to_owned())
        .filter(|description| !description.is_empty());
    Ok(RoleTemplate {
        prompt: after.to_owned(),
        description,
        read_only: meta.read_only.unwrap_or(false),
        protect_git: meta.protect_git.unwrap_or(true),
    })
}

/// Read the prompt for `role`, ignoring frontmatter attributes. The canonical
/// project file wins over the legacy project and global ones. Returns `None`
/// when neither exists; an error when a file exists but cannot be read or has
/// malformed frontmatter.
pub fn role_prompt(workspace: &Path, role: &str) -> std::io::Result<Option<String>> {
    Ok(role_template(workspace, role)?.map(|template| template.prompt))
}

/// Extract the optional compacted-conversation reminder from a role template.
/// The body of a `## Core directive` section is trimmed line-by-line, empty
/// lines are omitted, and the remaining lines are joined with newlines.
pub fn core_directive(template: &str) -> Option<String> {
    let mut lines = template.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "## Core directive" {
            let directive = lines
                .take_while(|line| !markdown_heading(line))
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            return (!directive.is_empty()).then_some(directive);
        }
    }
    None
}

/// Whether `line` begins with a Markdown heading as defined by `^#{1,6} `.
fn markdown_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ')
}

/// Role names available across all layers (union), sorted, excluding the
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
mod tests;
