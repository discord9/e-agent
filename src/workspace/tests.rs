use super::*;
use std::fs;

#[test]
fn rejects_parent_absolute_and_current_directory_paths() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    assert!(workspace.write("../outside", "no").is_err());
    assert!(workspace.write("/tmp/outside", "no").is_err());
    assert!(workspace.write("./file", "no").is_err());
}

#[test]
fn absolute_paths_inside_workspace_are_treated_as_relative() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let file = temp.path().join("nested/file");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "content").unwrap();

    // Read via absolute path: succeeds with the correct content.
    assert_eq!(workspace.read(file.to_str().unwrap()).unwrap(), b"content");
    assert_eq!(
        workspace.read_to_string(file.to_str().unwrap()).unwrap(),
        "content"
    );
    assert_eq!(
        workspace
            .try_read_to_string(file.to_str().unwrap())
            .unwrap(),
        Some("content".to_owned())
    );
    // A missing absolute path inside the workspace maps to `None`, not an
    // external-root error (write_file's undo snapshot relies on this).
    assert_eq!(
        workspace
            .try_read_to_string(temp.path().join("missing").to_str().unwrap())
            .unwrap(),
        None
    );

    // Write via absolute path inside the workspace: succeeds.
    workspace
        .write(temp.path().join("new/file").to_str().unwrap(), "written")
        .unwrap();
    assert_eq!(fs::read(temp.path().join("new/file")).unwrap(), b"written");
    // And remove_file via absolute path inside the workspace works too.
    workspace
        .remove_file(temp.path().join("new/file").to_str().unwrap())
        .unwrap();
    assert!(!temp.path().join("new/file").exists());
}

#[test]
fn absolute_paths_escaping_or_outside_workspace_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    let workspace = Workspace::new(&workspace_dir).unwrap();

    // `..` escaping out of the root via an absolute path is rejected
    // (component-validation or external-root error, never a panic).
    let escape = workspace_dir.join("sub/../../outside");
    assert!(workspace.read(escape.to_str().unwrap()).is_err());
    assert!(workspace.write(escape.to_str().unwrap(), "no").is_err());
    // Lexical `..` that stays inside the root is rejected as well: the
    // remainder must contain only normal components.
    let inside = workspace_dir.join("sub/../file");
    assert!(workspace.read(inside.to_str().unwrap()).is_err());

    // Absolute paths outside the workspace are still rejected with the
    // external-root error message.
    let outside = temp.path().join("outside");
    let error = workspace.read(outside.to_str().unwrap()).unwrap_err();
    assert!(error.contains("authorized external root"));
}

#[test]
fn external_directories_and_exact_files_enforce_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let readable = temp.path().join("readable");
    let writable = temp.path().join("writable");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir_all(&readable).unwrap();
    fs::create_dir_all(&writable).unwrap();
    fs::write(readable.join("input"), "read").unwrap();
    let exact = temp.path().join("exact");
    fs::write(&exact, "old").unwrap();
    let policy = crate::config::Sandbox {
        readable_paths: vec![readable.to_string_lossy().into_owned()],
        writable_paths: vec![
            writable.to_string_lossy().into_owned(),
            exact.to_string_lossy().into_owned(),
        ],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    assert_eq!(
        workspace
            .read(readable.join("input").to_str().unwrap())
            .unwrap(),
        b"read"
    );
    assert!(
        workspace
            .write(readable.join("input").to_str().unwrap(), "no")
            .is_err()
    );
    workspace
        .write(writable.join("new/leaf").to_str().unwrap(), "new")
        .unwrap();
    assert_eq!(fs::read(writable.join("new/leaf")).unwrap(), b"new");
    workspace.write(exact.to_str().unwrap(), "changed").unwrap();
    assert_eq!(fs::read(&exact).unwrap(), b"changed");
    assert!(
        workspace
            .read(temp.path().join("sibling").to_str().unwrap())
            .is_err()
    );
}

#[test]
fn policy_file_is_readable_but_not_writable() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join(".e-agent")).unwrap();
    fs::write(temp.path().join(".e-agent/config.toml"), "[sandbox]\n").unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    assert_eq!(
        workspace.read_to_string(".e-agent/config.toml").unwrap(),
        "[sandbox]\n"
    );
    let error = workspace.write(".e-agent/config.toml", "no").unwrap_err();
    assert!(error.contains("controls sandbox and file capabilities"));
}

#[test]
fn exact_file_handle_is_serialized_across_clones() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    fs::create_dir(&workspace_dir).unwrap();
    let exact = temp.path().join("exact");
    let initial = vec![b'x'; 32 * 1024];
    fs::write(&exact, &initial).unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![exact.to_str().unwrap().to_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let mut readers = Vec::new();
    for _ in 0..8 {
        let workspace = workspace.clone();
        let path = exact.to_str().unwrap().to_owned();
        let expected = initial.clone();
        readers.push(std::thread::spawn(move || {
            for _ in 0..50 {
                assert_eq!(workspace.read(&path).unwrap(), expected);
            }
        }));
    }
    for reader in readers {
        reader.join().unwrap();
    }

    let one = vec![b'a'; 20_001];
    let two = vec![b'b'; 30_003];
    let first = workspace.clone();
    let first_path = exact.to_str().unwrap().to_owned();
    let first_payload = one.clone();
    let t1 = std::thread::spawn(move || first.write(&first_path, first_payload).unwrap());
    let second = workspace.clone();
    let second_path = exact.to_str().unwrap().to_owned();
    let second_payload = two.clone();
    let t2 = std::thread::spawn(move || second.write(&second_path, second_payload).unwrap());
    t1.join().unwrap();
    t2.join().unwrap();
    let final_content = fs::read(&exact).unwrap();
    assert!(final_content == one || final_content == two);
}

#[test]
fn reroot_preserves_external_roots_and_ignores_local_policy() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let custom = parent.join("custom");
    let external = temp.path().join("external");
    fs::create_dir_all(custom.join(".e-agent")).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(external.join("visible"), "yes").unwrap();
    fs::write(
        custom.join(".e-agent/config.toml"),
        "[sandbox]\nreadable_paths = [\"/unauthorized\"]\n",
    )
    .unwrap();
    let policy = crate::config::Sandbox {
        readable_paths: vec![external.to_str().unwrap().to_owned()],
        ..Default::default()
    };
    let parent = Workspace::new(&parent)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let custom = parent.reroot(&custom).unwrap();
    assert_eq!(
        custom
            .read(external.join("visible").to_str().unwrap())
            .unwrap(),
        b"yes"
    );
    assert!(custom.read("/unauthorized").is_err());
}

#[test]
fn reroot_is_limited_to_existing_writable_authority() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let child = workspace_dir.join("child");
    let writable = temp.path().join("writable");
    let readable = temp.path().join("readable");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir(&writable).unwrap();
    fs::create_dir(&readable).unwrap();
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&crate::config::Sandbox {
            writable_paths: vec![writable.to_string_lossy().into_owned()],
            readable_paths: vec![readable.to_string_lossy().into_owned()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(workspace.reroot(&child).unwrap().root(), child);
    assert_eq!(workspace.reroot(&writable).unwrap().root(), writable);
    assert!(workspace.reroot(&readable).is_err());
    assert!(workspace.reroot(temp.path()).is_err());
    assert!(workspace.reroot("/").is_err());
}

#[test]
fn reroot_keeps_startup_policy_anchor_protected() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let child = workspace_dir.join("child");
    let policy = workspace_dir.join(".e-agent/config.toml");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir_all(policy.parent().unwrap()).unwrap();
    fs::write(&policy, "[sandbox]\n").unwrap();
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&crate::config::Sandbox {
            writable_paths: vec![workspace_dir.to_string_lossy().into_owned()],
            ..Default::default()
        })
        .unwrap()
        .reroot(&child)
        .unwrap();
    assert_eq!(workspace.policy_anchor(), policy);
    assert!(workspace.write(policy.to_str().unwrap(), "no").is_err());
    assert_eq!(fs::read_to_string(policy).unwrap(), "[sandbox]\n");
}

#[cfg(unix)]
#[test]
fn policy_write_rejects_dangling_and_parent_symlink_aliases() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let policy_dir = workspace.join(".e-agent");
    fs::create_dir_all(&policy_dir).unwrap();
    let policy = policy_dir.join("config.toml");
    let dangling = workspace.join("dangling");
    symlink(".e-agent/config.toml", &dangling).unwrap();
    let parent_alias = workspace.join("policy-dir-alias");
    symlink(".e-agent", &parent_alias).unwrap();
    let workspace = Workspace::new(&workspace).unwrap();
    assert!(workspace.write("dangling", "no").is_err());
    assert!(
        workspace
            .write("policy-dir-alias/config.toml", "no")
            .is_err()
    );
    assert!(!policy.exists());
}

#[cfg(unix)]
#[test]
fn external_directory_allows_internal_symlink_but_rejects_escape() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let external = temp.path().join("external");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir_all(external.join("inside")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(external.join("inside/file"), "yes").unwrap();
    fs::write(outside.join("secret"), "no").unwrap();
    symlink("inside", external.join("link-in")).unwrap();
    symlink(&outside, external.join("link-out")).unwrap();
    let policy = crate::config::Sandbox {
        readable_paths: vec![external.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    assert_eq!(
        workspace
            .read(external.join("link-in/file").to_str().unwrap())
            .unwrap(),
        b"yes"
    );
    assert!(
        workspace
            .read(external.join("link-out/secret").to_str().unwrap())
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escapes_including_dangling_final_write() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "no").unwrap();
    symlink(outside.path(), temp.path().join("escape")).unwrap();
    let dangling_target = outside.path().join("must-not-exist");
    symlink(&dangling_target, temp.path().join("dangling")).unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    assert!(workspace.read("escape/secret").is_err());
    assert!(workspace.write("escape/new", "no").is_err());
    assert!(workspace.write("dangling", "no").is_err());
    assert!(!dangling_target.exists());
}
