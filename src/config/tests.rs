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
fn thinking_defaults_to_false_and_reads_from_model_profile() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "deepseek/v3"
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_file = "key"
[models."deepseek/v3"]
model = "deepseek-chat"
reasoning_effort = "max"
thinking = true
"#,
    );
    let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
    assert!(resolved.thinking);

    // Absent `thinking` means false (deepseek-style default).
    let path = write_config(
        temp.path(),
        r#"
default = "deepseek/v3"
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_file = "key"
[models."deepseek/v3"]
model = "deepseek-chat"
reasoning_effort = "high"
"#,
    );
    let resolved = Config::from_path(&path).unwrap().resolve(None).unwrap();
    assert!(!resolved.thinking);
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
        error.contains("not within any globally authorized writable root"),
        "{error}"
    );
    assert!(error.contains("project-local"), "{error}");
    assert!(error.contains("user-level config"), "{error}");
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
fn project_sandbox_rejects_unknown_fields() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
    let path = write_config(temp.path(), "");
    let config = Config::from_path(&path).unwrap();
    for field in [
        "readible_paths = []",
        "writable = []",
        "future_policy = true",
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
fn project_sandbox_policy_switch_fields_are_per_key_overrides() {
    // The policy-switch scalars (enabled/network/workspace_writable) are
    // project overrides, not rejected fields: each key present in the
    // project config replaces the global value, absent keys keep it.
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
    let path = write_config(
        temp.path(),
        "[sandbox]\nenabled = true\nnetwork = true\nworkspace_writable = true\n",
    );
    let config = Config::from_path(&path).unwrap();
    std::fs::write(
        workspace.join(".e-agent/config.toml"),
        "[sandbox]\nenabled = false\nnetwork = false\n",
    )
    .unwrap();
    let sandbox = config.sandbox(&workspace).unwrap();
    assert!(!sandbox.enabled);
    assert!(!sandbox.network);
    assert!(sandbox.workspace_writable);
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
            .contains("globally authorized writable root")
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
            .contains("globally authorized readable or writable root")
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

#[test]
fn resolve_background_timeout_defaults_and_global() {
    let temp = tempfile::tempdir().unwrap();
    // No config at all → default 1800s.
    let t = resolve_background_timeout(None, temp.path()).unwrap();
    assert_eq!(t, Some(Duration::from_secs(1800)));

    // Global config sets a value.
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[background]
timeout_secs = 120
"#,
    );
    let config = Config::from_path(&path).unwrap();
    let t = resolve_background_timeout(Some(&config), temp.path()).unwrap();
    assert_eq!(t, Some(Duration::from_secs(120)));
}

#[test]
fn resolve_background_timeout_workspace_override_and_zero() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[background]
timeout_secs = 120
"#,
    );
    let config = Config::from_path(&path).unwrap();

    // Workspace .e-agent/config.toml overrides global.
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(ws.join(".e-agent")).unwrap();
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[background]\ntimeout_secs = 5\n",
    )
    .unwrap();
    let t = resolve_background_timeout(Some(&config), &ws).unwrap();
    assert_eq!(t, Some(Duration::from_secs(5)));

    // timeout_secs = 0 → None (no timeout).
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[background]\ntimeout_secs = 0\n",
    )
    .unwrap();
    let t = resolve_background_timeout(Some(&config), &ws).unwrap();
    assert_eq!(t, None);

    // Workspace without [background] falls back to global.
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[sandbox]\nenabled = true\n",
    )
    .unwrap();
    let t = resolve_background_timeout(Some(&config), &ws).unwrap();
    assert_eq!(t, Some(Duration::from_secs(120)));
}

#[test]
fn resolve_bash_timeout_defaults_and_global() {
    let temp = tempfile::tempdir().unwrap();
    // No config at all → default 30s.
    let t = resolve_bash_timeout(None, temp.path()).unwrap();
    assert_eq!(t, Some(Duration::from_secs(30)));

    // Global config sets a value.
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[bash]
timeout_secs = 120
"#,
    );
    let config = Config::from_path(&path).unwrap();
    let t = resolve_bash_timeout(Some(&config), temp.path()).unwrap();
    assert_eq!(t, Some(Duration::from_secs(120)));
}

#[test]
fn resolve_bash_timeout_workspace_override_and_zero() {
    let temp = tempfile::tempdir().unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[bash]
timeout_secs = 120
"#,
    );
    let config = Config::from_path(&path).unwrap();

    // Workspace .e-agent/config.toml overrides global.
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(ws.join(".e-agent")).unwrap();
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[bash]\ntimeout_secs = 5\n",
    )
    .unwrap();
    let t = resolve_bash_timeout(Some(&config), &ws).unwrap();
    assert_eq!(t, Some(Duration::from_secs(5)));

    // timeout_secs = 0 → None (no timeout).
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[bash]\ntimeout_secs = 0\n",
    )
    .unwrap();
    let t = resolve_bash_timeout(Some(&config), &ws).unwrap();
    assert_eq!(t, None);

    // Workspace without [bash] falls back to global.
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[sandbox]\nenabled = true\n",
    )
    .unwrap();
    let t = resolve_bash_timeout(Some(&config), &ws).unwrap();
    assert_eq!(t, Some(Duration::from_secs(120)));
}

#[test]
fn linked_worktree_main_repo_resolves_gitdir_pointer() {
    let temp = tempfile::tempdir().unwrap();
    let main_repo = temp.path().join("main-repo");
    let git_dir = main_repo.join(".git");
    std::fs::create_dir_all(git_dir.join("worktrees/feature")).unwrap();
    let worktree = temp.path().join("worktrees/feature");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git_dir.join("worktrees/feature").display()),
    )
    .unwrap();

    let resolved = linked_worktree_main_repo(&worktree).unwrap();
    assert_eq!(resolved, Some(main_repo), "must resolve the main repo root");

    // A normal repo (.git is a directory) → None.
    let normal = temp.path().join("normal");
    std::fs::create_dir_all(normal.join(".git")).unwrap();
    assert_eq!(linked_worktree_main_repo(&normal).unwrap(), None);

    // No .git at all → None.
    let bare = temp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(linked_worktree_main_repo(&bare).unwrap(), None);
}

#[test]
fn resolve_sandbox_adds_linked_worktree_main_repo_readonly() {
    let temp = tempfile::tempdir().unwrap();
    let main_repo = temp.path().join("main-repo");
    std::fs::create_dir_all(main_repo.join(".git/worktrees/feature")).unwrap();
    let worktree = temp.path().join("worktrees/feature");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!(
            "gitdir: {}\n",
            main_repo.join(".git/worktrees/feature").display()
        ),
    )
    .unwrap();

    let sandbox = resolve_sandbox(None, &worktree).unwrap();
    assert!(
        sandbox
            .readable_paths
            .iter()
            .any(|p| Path::new(p) == main_repo),
        "main repo must be in readable_paths, got: {:?}",
        sandbox.readable_paths
    );
    assert!(
        sandbox
            .writable_paths
            .iter()
            .all(|p| Path::new(p) != main_repo),
        "main repo must NOT be writable, got: {:?}",
        sandbox.writable_paths
    );

    // A normal workspace gets no extra readable root.
    let normal = temp.path().join("normal");
    std::fs::create_dir_all(normal.join(".git")).unwrap();
    let sandbox = resolve_sandbox(None, &normal).unwrap();
    assert!(
        sandbox.readable_paths.is_empty(),
        "normal repo must have no extra roots, got: {:?}",
        sandbox.readable_paths
    );
}

#[test]
fn linked_worktree_main_repo_rejects_malicious_pointers() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("wt");
    std::fs::create_dir_all(&worktree).unwrap();

    // gitdir: pointing at ~/.ssh (or any non-.git/worktrees shape) → None.
    std::fs::write(
        worktree.join(".git"),
        "gitdir: /home/victim/.ssh
",
    )
    .unwrap();
    assert_eq!(
        linked_worktree_main_repo(&worktree).unwrap(),
        None,
        "non-worktree gitdir shape must be rejected"
    );

    // Relative pointer → None.
    std::fs::write(
        worktree.join(".git"),
        "gitdir: ../.git/worktrees/x
",
    )
    .unwrap();
    assert_eq!(linked_worktree_main_repo(&worktree).unwrap(), None);

    // Pointer with .. inside → None.
    std::fs::write(
        worktree.join(".git"),
        "gitdir: /home/victim/.git/worktrees/../../.ssh
",
    )
    .unwrap();
    assert_eq!(linked_worktree_main_repo(&worktree).unwrap(), None);

    // Pointer to the filesystem root → None.
    std::fs::write(
        worktree.join(".git"),
        "gitdir: /.git/worktrees/x
",
    )
    .unwrap();
    assert_eq!(linked_worktree_main_repo(&worktree).unwrap(), None);

    // Valid-shaped pointer but <main>/.git is not a directory → None.
    let main_repo = temp.path().join("main-repo");
    std::fs::create_dir_all(&main_repo).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!(
            "gitdir: {}/.git/worktrees/feature
",
            main_repo.display()
        ),
    )
    .unwrap();
    assert_eq!(
        linked_worktree_main_repo(&worktree).unwrap(),
        None,
        "missing <main>/.git must be rejected"
    );

    // Valid worktree → Some(main_repo).
    std::fs::create_dir_all(main_repo.join(".git/worktrees/feature")).unwrap();
    let resolved = linked_worktree_main_repo(&worktree).unwrap();
    assert_eq!(resolved, Some(main_repo.canonicalize().unwrap()));
}

#[test]
fn merged_with_project_models_merge_by_name_and_roles_by_key() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let global = write_config(
        temp.path(),
        r#"
default = "global/a"
[providers.global]
base_url = "https://global.test/v1"
api_key_file = "key"
[providers.project]
base_url = "https://project.test/v1"
api_key_file = "key"
[providers.shared]
base_url = "https://shared.test/v1"
api_key_file = "key"
[models."global/a"]
model = "a-global"
[models."shared/b"]
model = "b-global"
[roles]
main = "global/a"
subagent = "shared/b"
"#,
    );
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(ws.join(".e-agent")).unwrap();
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        r#"
[models."shared/b"]
model = "b-project"
reasoning_effort = "max"
[models."project/c"]
model = "c-project"
[roles]
subagent = "project/c"
"#,
    )
    .unwrap();

    let merged = Config::from_path(&global)
        .unwrap()
        .merged_with_project(&ws)
        .unwrap();

    // Same-name global model replaced by the project definition.
    let shared = merged.resolve(Some("shared/b")).unwrap();
    assert_eq!(shared.model, "b-project");
    assert_eq!(shared.reasoning_effort.as_deref(), Some("max"));

    // Global-only model survives, still served by the global provider.
    let global_a = merged.resolve(Some("global/a")).unwrap();
    assert_eq!(global_a.model, "a-global");
    assert_eq!(global_a.base_url, "https://global.test/v1");

    // Project-only model is defined, served by the global provider block.
    let project_c = merged.resolve(Some("project/c")).unwrap();
    assert_eq!(project_c.model, "c-project");
    assert_eq!(project_c.base_url, "https://project.test/v1");

    // Global default is untouched: resolve(None) still picks global/a.
    assert_eq!(merged.resolve(None).unwrap().display, "global/a");

    // Roles merge per key: project subagent wins, global main survives.
    assert_eq!(
        merged.resolve_role("subagent").unwrap().unwrap().display,
        "project/c"
    );
    assert_eq!(
        merged.resolve_role("main").unwrap().unwrap().display,
        "global/a"
    );
}

#[test]
fn merged_with_project_without_project_config_is_identical() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let global = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[roles]
subagent = "kimi/k3"
"#,
    );
    // Workspace exists but has no .e-agent/config.toml.
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let merged = Config::from_path(&global)
        .unwrap()
        .merged_with_project(&ws)
        .unwrap();

    assert_eq!(merged.resolve(None).unwrap().model, "k3");
    assert_eq!(
        merged.resolve_role("subagent").unwrap().unwrap().display,
        "kimi/k3"
    );
    // Sandbox stays pure global.
    let sandbox = merged.sandbox(&ws).unwrap();
    assert!(!sandbox.enabled);
    assert!(sandbox.network);
    assert!(sandbox.workspace_writable);
    assert!(sandbox.writable_paths.is_empty());
}

#[test]
fn merged_with_project_empty_models_table_keeps_all_global() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let global = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[providers.deepseek]
base_url = "https://deepseek.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[models."deepseek/d1"]
model = "d1"
"#,
    );
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(ws.join(".e-agent")).unwrap();
    // Empty [models] table (and no [roles]) must not drop any global model.
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        "[models]\n[sandbox]\nenabled = true\n",
    )
    .unwrap();
    let merged = Config::from_path(&global)
        .unwrap()
        .merged_with_project(&ws)
        .unwrap();

    assert_eq!(merged.resolve(Some("kimi/k3")).unwrap().model, "k3");
    assert_eq!(merged.resolve(Some("deepseek/d1")).unwrap().model, "d1");
    // The [sandbox] in the project file is ignored by the model merge and
    // handled by resolve_sandbox instead.
    assert_eq!(merged.resolve(None).unwrap().display, "kimi/k3");
}

#[test]
fn merged_with_project_resolves_key_file_relative_to_global_config() {
    let temp = tempfile::tempdir().unwrap();
    // Key file lives ONLY next to the global config, not in the workspace.
    std::fs::write(temp.path().join("key"), "global-key").unwrap();
    let global = write_config(
        temp.path(),
        r#"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
"#,
    );
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(ws.join(".e-agent")).unwrap();
    std::fs::write(
        ws.join(".e-agent/config.toml"),
        r#"
[models."kimi/k3"]
model = "k3-project"
"#,
    )
    .unwrap();
    let merged = Config::from_path(&global)
        .unwrap()
        .merged_with_project(&ws)
        .unwrap();
    let resolved = merged.resolve(Some("kimi/k3")).unwrap();
    assert_eq!(resolved.model, "k3-project");
    assert_eq!(
        resolved.api_key, "global-key",
        "relative api_key_file still resolves against the global config dir"
    );
}

#[test]
fn sandbox_project_scalar_fields_override_global() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    let external = temp.path().join("external");
    std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
    std::fs::create_dir_all(&external).unwrap();
    let path = write_config(
        temp.path(),
        &format!(
            "[sandbox]\nenabled = true\nnetwork = true\nworkspace_writable = true\nwritable_paths = [\"{}\"]\n",
            external.display()
        ),
    );
    // Project overrides two scalars, leaves workspace_writable and paths
    // alone: those must keep the global values.
    std::fs::write(
        workspace.join(".e-agent/config.toml"),
        "[sandbox]\nenabled = false\nnetwork = false\n",
    )
    .unwrap();
    let sandbox = Config::from_path(&path)
        .unwrap()
        .sandbox(&workspace)
        .unwrap();
    assert!(!sandbox.enabled, "project enabled overrides global");
    assert!(!sandbox.network, "project network overrides global");
    assert!(
        sandbox.workspace_writable,
        "absent project key keeps the global value"
    );
    assert_eq!(
        sandbox.writable_paths,
        vec![external.to_str().unwrap()],
        "absent project paths keep the global roots"
    );
}

#[test]
fn sandbox_project_scalars_can_enable_without_global_sandbox() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
    let path = write_config(temp.path(), ""); // no [sandbox] at all
    std::fs::write(
        workspace.join(".e-agent/config.toml"),
        "[sandbox]\nenabled = true\n",
    )
    .unwrap();
    let sandbox = Config::from_path(&path)
        .unwrap()
        .sandbox(&workspace)
        .unwrap();
    assert!(sandbox.enabled, "project can turn the sandbox on");
    assert!(
        sandbox.network && sandbox.workspace_writable,
        "unset scalars keep Sandbox defaults"
    );
    assert!(sandbox.writable_paths.is_empty());
}

#[test]
fn sandbox_project_writable_path_without_global_sandbox_guides_user() {
    // No global [sandbox] at all + a project writable_paths entry must fail
    // with the offending path and actionable remediation, not a bare subset
    // rejection.
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let path = write_config(temp.path(), ""); // no [sandbox] section
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
    assert!(error.contains(&outside.display().to_string()), "{error}");
    assert!(error.contains("user-level config"), "{error}");
    assert!(error.contains("[sandbox].writable_paths"), "{error}");
    assert!(error.contains("project-local"), "{error}");
}

#[test]
fn sandbox_project_readable_path_without_global_sandbox_guides_user() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(workspace.join(".e-agent")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let path = write_config(temp.path(), ""); // no [sandbox] section
    std::fs::write(
        workspace.join(".e-agent/config.toml"),
        format!("[sandbox]\nreadable_paths = [\"{}\"]\n", outside.display()),
    )
    .unwrap();
    let error = Config::from_path(&path)
        .unwrap()
        .sandbox(&workspace)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&outside.display().to_string()), "{error}");
    assert!(error.contains("user-level config"), "{error}");
    assert!(error.contains("[sandbox].readable_paths"), "{error}");
    assert!(error.contains("project-local"), "{error}");
}

#[test]
fn tui_keys_default_to_enter_submit_and_alt_enter_newline() {
    // No [tui] section: bare Enter submits, Alt+Enter inserts a newline
    // (historical behavior).
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
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
    let keys = config.tui_keys().unwrap();
    assert_eq!(keys.submit_modifiers, KeyModifiers::NONE);
    assert_eq!(keys.newline_modifiers, KeyModifiers::ALT);
    assert_eq!(
        keys.describe(),
        ("Enter".to_owned(), "Alt+Enter".to_owned())
    );
}

#[test]
fn tui_keys_parse_swapped_submit_and_newline() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[tui]
submit = "alt+enter"
newline = "enter"
"#,
    );
    let config = Config::from_path(&path).unwrap();
    let keys = config.tui_keys().unwrap();
    assert_eq!(keys.submit_modifiers, KeyModifiers::ALT);
    assert_eq!(keys.newline_modifiers, KeyModifiers::NONE);
    assert_eq!(
        keys.describe(),
        ("Alt+Enter".to_owned(), "Enter".to_owned())
    );
}

#[test]
fn tui_keys_option_enter_is_an_alias_for_alt_enter() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[tui]
submit = "option+enter"
newline = "shift+enter"
"#,
    );
    let config = Config::from_path(&path).unwrap();
    let keys = config.tui_keys().unwrap();
    assert_eq!(keys.submit_modifiers, KeyModifiers::ALT);
    assert_eq!(keys.newline_modifiers, KeyModifiers::SHIFT);
}

#[test]
fn tui_keys_reject_unsupported_key() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[tui]
submit = "foo"
newline = "enter"
"#,
    );
    let error = Config::from_path(&path)
        .unwrap()
        .tui_keys()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("[tui] submit = \"foo\" is not a supported key"),
        "{error}"
    );
    assert!(
        error.contains("expected one of: enter, alt+enter (option+enter), ctrl+enter, shift+enter"),
        "{error}"
    );
}

#[test]
fn tui_keys_reject_submit_equal_newline() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("key"), "key").unwrap();
    let path = write_config(
        temp.path(),
        r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "key"
[models."kimi/k3"]
model = "k3"
[tui]
submit = "enter"
newline = "enter"
"#,
    );
    let error = Config::from_path(&path)
        .unwrap()
        .tui_keys()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("[tui] submit and newline must be different keys"),
        "{error}"
    );
    assert!(error.contains("\"enter\""), "{error}");
}

#[test]
fn config_watch_paths_covers_global_candidates_and_project_override() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    let paths = config_watch_paths(&workspace);
    // The project override is always watched (it may not exist yet).
    let project = workspace.join(".e-agent/config.toml");
    assert!(paths.contains(&project));
    assert_eq!(
        paths.iter().filter(|p| **p == project).count(),
        1,
        "project path appears exactly once"
    );
    // At least one global candidate (XDG and/or ~/.config, env-dependent).
    assert!(paths.iter().any(|p| p.ends_with("e-agent/config.toml")));
    assert!(paths.windows(2).all(|w| w[0] != w[1]), "no duplicates");
}
