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
