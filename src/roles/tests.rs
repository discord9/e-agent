use super::*;

#[test]
fn workspace_overrides_global_and_unions() {
    let _guard = XDG_TEST_LOCK.lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let xdg = global.path().join("xdg");
    std::fs::create_dir_all(xdg.join("e-agent/agents")).unwrap();
    std::fs::write(xdg.join("e-agent/agents/explorer.md"), "global explorer").unwrap();
    std::fs::write(xdg.join("e-agent/agents/fixer.md"), "global fixer").unwrap();
    std::fs::write(xdg.join("e-agent/agents/oracle.md"), "global oracle").unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    let ws_agents = workspace.path().join(AGENTS_DIR);
    std::fs::create_dir_all(&ws_agents).unwrap();
    std::fs::write(
        ws_agents.join("fixer.md"),
        "---\ndescription = \"  workspace description  \"\n---\nworkspace fixer",
    )
    .unwrap();
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
        role_template(workspace.path(), "fixer")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("workspace description")
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
        "global role resolves like any role"
    );
    assert_eq!(role_prompt(workspace.path(), "nonexistent").unwrap(), None);
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[test]
fn all_layers_are_scanned_with_canonical_precedence_and_atomic_attributes() {
    let _guard = XDG_TEST_LOCK.lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (global, legacy, canonical) = isolated_layers(&workspace);

    std::fs::write(
        global.join("shared.md"),
        "---\ndescription = \"global\"\nread_only = false\nprotect_git = true\n---\nglobal prompt",
    )
    .unwrap();
    std::fs::write(
        legacy.join("shared.md"),
        "---\ndescription = \"legacy\"\nread_only = true\nprotect_git = false\n---\nlegacy prompt",
    )
    .unwrap();
    std::fs::write(
        canonical.join("shared.md"),
        "---\ndescription = \"canonical\"\nread_only = false\nprotect_git = false\n---\ncanonical prompt",
    )
    .unwrap();
    std::fs::write(legacy.join("legacy-only.md"), "legacy-only prompt").unwrap();
    std::fs::write(canonical.join("canonical-only.md"), "canonical-only prompt").unwrap();
    std::fs::write(canonical.join("main.md"), "canonical main prompt").unwrap();

    assert_eq!(
        available_roles(workspace.path()),
        vec!["canonical-only", "legacy-only", "shared"]
    );
    let shared = role_template(workspace.path(), "shared").unwrap().unwrap();
    assert_eq!(shared.prompt, "canonical prompt");
    assert_eq!(shared.description.as_deref(), Some("canonical"));
    assert!(!shared.read_only);
    assert!(!shared.protect_git);
    assert_eq!(
        role_prompt(workspace.path(), "legacy-only")
            .unwrap()
            .as_deref(),
        Some("legacy-only prompt")
    );
    assert_eq!(
        role_prompt(workspace.path(), "canonical-only")
            .unwrap()
            .as_deref(),
        Some("canonical-only prompt")
    );
    assert_eq!(
        role_prompt(workspace.path(), MAIN_ROLE).unwrap().as_deref(),
        Some("canonical main prompt")
    );

    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[test]
fn malformed_canonical_role_fails_closed_without_global_fallback() {
    let _guard = XDG_TEST_LOCK.lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (global, _, canonical) = isolated_layers(&workspace);
    std::fs::write(global.join("broken.md"), "global fallback").unwrap();
    std::fs::write(
        canonical.join("broken.md"),
        "---\ndescription = 7\n---\ncanonical prompt",
    )
    .unwrap();

    assert_eq!(
        role_template(workspace.path(), "broken")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[test]
fn invalid_role_names_read_nothing() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(role_prompt(temp.path(), "../etc/passwd").unwrap(), None);
    assert_eq!(role_prompt(temp.path(), "").unwrap(), None);
}

fn isolated_layers(
    workspace: &tempfile::TempDir,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let xdg = workspace.path().join("xdg");
    let global = xdg.join("e-agent/agents");
    std::fs::create_dir_all(&global).unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
    let legacy = workspace.path().join(AGENTS_DIR);
    std::fs::create_dir_all(&legacy).unwrap();
    let canonical = workspace.path().join(".e-agent").join(AGENTS_DIR);
    std::fs::create_dir_all(&canonical).unwrap();
    (global, legacy, canonical)
}

#[test]
fn frontmatter_read_only_flag_and_prompt_stripping() {
    let _guard = XDG_TEST_LOCK.lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_, ws_agents, _) = isolated_layers(&workspace);

    std::fs::write(ws_agents.join("plain.md"), "You fix things.").unwrap();
    let template = role_template(workspace.path(), "plain").unwrap().unwrap();
    assert_eq!(template.prompt, "You fix things.");
    assert_eq!(template.description, None);
    assert!(!template.read_only);
    assert!(template.protect_git);

    std::fs::write(
        ws_agents.join("reviewer.md"),
        "---\nread_only = true\n---\nReview things.\n",
    )
    .unwrap();
    let template = role_template(workspace.path(), "reviewer")
        .unwrap()
        .unwrap();
    assert!(template.read_only);
    assert_eq!(template.prompt, "Review things.\n");

    std::fs::write(
        ws_agents.join("winfixer.md"),
        "---\nprotect_git = false\n---\nFix things on Windows.\n",
    )
    .unwrap();
    let template = role_template(workspace.path(), "winfixer")
        .unwrap()
        .unwrap();
    assert!(!template.protect_git);
    assert_eq!(template.prompt, "Fix things on Windows.\n");

    std::fs::write(
        ws_agents.join("strict.md"),
        "---\nprotect_git = true\n---\nStrict fixer.",
    )
    .unwrap();
    assert!(
        role_template(workspace.path(), "strict")
            .unwrap()
            .unwrap()
            .protect_git
    );

    std::fs::write(
        ws_agents.join("bogus.md"),
        "---\nprotect_git = \"no\"\n---\nPrompt",
    )
    .unwrap();
    let error = role_template(workspace.path(), "bogus").unwrap_err();
    assert!(error.to_string().contains("not valid TOML"));
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[test]
fn description_is_trimmed_missing_blank_and_rejects_newlines() {
    let _guard = XDG_TEST_LOCK.lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_, ws_agents, _) = isolated_layers(&workspace);

    std::fs::write(
        ws_agents.join("described.md"),
        "---\ndescription = \"  A useful role  \"\n---\nPrompt",
    )
    .unwrap();
    assert_eq!(
        role_template(workspace.path(), "described")
            .unwrap()
            .unwrap()
            .description
            .as_deref(),
        Some("A useful role")
    );
    for (name, value) in [("missing", ""), ("blank", "description = \"   \"")] {
        let content = if value.is_empty() {
            "---\n---\nPrompt".to_owned()
        } else {
            format!("---\n{value}\n---\nPrompt")
        };
        std::fs::write(ws_agents.join(format!("{name}.md")), content).unwrap();
        assert_eq!(
            role_template(workspace.path(), name)
                .unwrap()
                .unwrap()
                .description,
            None
        );
    }
    std::fs::write(
        ws_agents.join("newline.md"),
        "---\ndescription = \"line\\nnext\"\n---\nPrompt",
    )
    .unwrap();
    assert_eq!(
        role_template(workspace.path(), "newline")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[test]
fn frontmatter_flag_comes_from_the_winning_workspace_layer() {
    let _guard = XDG_TEST_LOCK.lock().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (global, ws_agents, _) = isolated_layers(&workspace);
    std::fs::write(
        global.join("explorer.md"),
        "---\nread_only = true\n---\nglobal explorer",
    )
    .unwrap();
    std::fs::write(ws_agents.join("explorer.md"), "workspace explorer").unwrap();
    let template = role_template(workspace.path(), "explorer")
        .unwrap()
        .unwrap();
    assert!(!template.read_only);
    std::fs::write(global.join("reviewer.md"), "global reviewer").unwrap();
    std::fs::write(
        ws_agents.join("reviewer.md"),
        "---\nread_only = true\n---\nworkspace reviewer",
    )
    .unwrap();
    assert!(
        role_template(workspace.path(), "reviewer")
            .unwrap()
            .unwrap()
            .read_only
    );
    std::fs::write(
        global.join("archiver.md"),
        "---\nread_only = true\n---\nglobal archiver",
    )
    .unwrap();
    assert!(
        role_template(workspace.path(), "archiver")
            .unwrap()
            .unwrap()
            .read_only
    );
    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[test]
fn core_directive_extracts_trimmed_body() {
    let template =
        "# Role\n\n## Core directive\n\n  Keep the task scoped.  \n\n  Verify results.\n";
    assert_eq!(
        core_directive(template).as_deref(),
        Some("Keep the task scoped.\nVerify results.")
    );
}

#[test]
fn core_directive_returns_none_when_absent_or_empty() {
    assert_eq!(core_directive("# Role\n\nNo directive here."), None);
    assert_eq!(
        core_directive("## Core directive\n\n   \n\n### Next section\ntext"),
        None
    );
}

#[test]
fn core_directive_stops_at_next_heading() {
    let template = "## Core directive\nFirst line\n\n## Details\nSecond line";
    assert_eq!(core_directive(template).as_deref(), Some("First line"));
}
