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
