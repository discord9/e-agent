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

#[test]
fn alias_entries_resolve_by_configured_destination() {
    // The capability is opened from the canonical source, but lookup goes
    // by the configured logical destination: a path written at the alias
    // destination is served from the canonical source without the input
    // ever being canonicalized.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let alias = temp.path().join("alias");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "content").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![source.to_string_lossy().into_owned()],
        writable_mounts: vec![(
            source.to_string_lossy().into_owned(),
            alias.to_string_lossy().into_owned(),
        )],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // Read through the configured alias destination.
    assert_eq!(
        workspace
            .read(alias.join("file").to_str().unwrap())
            .unwrap(),
        b"content"
    );
    // Write through the alias destination lands in the canonical source.
    workspace
        .write(alias.join("new").to_str().unwrap(), "written")
        .unwrap();
    assert_eq!(fs::read(source.join("new")).unwrap(), b"written");
    // The canonical source stays addressable too (its self entry).
    assert_eq!(
        workspace
            .read(source.join("file").to_str().unwrap())
            .unwrap(),
        b"content"
    );
}

#[test]
fn canonical_rw_and_alias_ro_coexist_independently() {
    // The same canonical source exists as a canonical RW entry and an
    // independent alias RO entry: writes through the canonical path are
    // allowed while the alias destination stays read-only.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let alias = temp.path().join("alias");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "content").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![source.to_string_lossy().into_owned()],
        readable_mounts: vec![(
            source.to_string_lossy().into_owned(),
            alias.to_string_lossy().into_owned(),
        )],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // Canonical RW: write and read through the canonical source path.
    workspace
        .write(source.join("rw").to_str().unwrap(), "rw")
        .unwrap();
    assert_eq!(fs::read(source.join("rw")).unwrap(), b"rw");
    // Alias RO: reads allowed, writes rejected.
    assert_eq!(
        workspace
            .read(alias.join("file").to_str().unwrap())
            .unwrap(),
        b"content"
    );
    let error = workspace
        .write(alias.join("file").to_str().unwrap(), "no")
        .unwrap_err();
    assert!(error.contains("read-only external root"), "{error}");
    // The read-only alias never granted write authority to the source.
    assert_eq!(fs::read(source.join("file")).unwrap(), b"content");
}

#[test]
fn exact_file_alias_rejects_siblings() {
    // An exact-file alias is addressable at its configured destination and
    // at its canonical path, but refuses sibling paths beneath it.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let exact = temp.path().join("exact");
    let alias = temp.path().join("exact-alias");
    fs::create_dir(&workspace_dir).unwrap();
    fs::write(&exact, "old").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![exact.to_string_lossy().into_owned()],
        writable_mounts: vec![(
            exact.to_string_lossy().into_owned(),
            alias.to_string_lossy().into_owned(),
        )],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // The exact file is addressable at its configured alias destination…
    workspace.write(alias.to_str().unwrap(), "changed").unwrap();
    assert_eq!(fs::read(&exact).unwrap(), b"changed");
    // …and at its canonical path.
    workspace.write(exact.to_str().unwrap(), "again").unwrap();
    assert_eq!(fs::read(&exact).unwrap(), b"again");
    // Sibling paths under the file alias are rejected, never routed to a
    // broader entry.
    let error = workspace
        .write(alias.join("sibling").to_str().unwrap(), "no")
        .unwrap_err();
    assert!(error.contains("sibling paths"), "{error}");
    assert!(
        workspace
            .read(alias.join("sibling").to_str().unwrap())
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn input_is_not_ambiently_canonicalized() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let unconfigured = temp.path().join("unconfigured");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "content").unwrap();
    symlink(&source, &unconfigured).unwrap();
    let policy = crate::config::Sandbox {
        readable_paths: vec![source.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // The canonical path works…
    assert_eq!(
        workspace
            .read(source.join("file").to_str().unwrap())
            .unwrap(),
        b"content"
    );
    // …but a symlink alias that was never configured is NOT canonicalized
    // into a match: lookup is lexical against the configured destinations.
    let error = workspace
        .read(unconfigured.join("file").to_str().unwrap())
        .unwrap_err();
    assert!(error.contains("authorized external root"), "{error}");
}

#[test]
fn reroot_ignores_alias_destinations() {
    // Aliases never extend reroot: only the canonical writable source
    // authorizes a custom workspace.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let alias = temp.path().join("alias");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&source).unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![source.to_string_lossy().into_owned()],
        writable_mounts: vec![(
            source.to_string_lossy().into_owned(),
            alias.to_string_lossy().into_owned(),
        )],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // Reroot to the canonical writable source is authorized.
    assert_eq!(workspace.reroot(&source).unwrap().root(), source);
    // The configured alias destination never extends reroot authority.
    assert!(workspace.reroot(&alias).is_err());
}

#[test]
fn sandbox_enabled_read_only_workspace_allows_reads_denies_writes_and_removes() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("existing"), "data").unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // Reads (read / try-read) stay allowed on the read-only workspace.
    assert_eq!(workspace.read("existing").unwrap(), b"data");
    assert_eq!(workspace.read_to_string("existing").unwrap(), "data");
    assert_eq!(
        workspace.try_read_to_string("existing").unwrap(),
        Some("data".to_owned())
    );
    assert_eq!(workspace.try_read_to_string("missing").unwrap(), None);
    // Writes, edits (write) and removes are denied, relative and absolute.
    for path in [
        "new".to_owned(),
        temp.path().join("new").to_str().unwrap().to_owned(),
    ] {
        let error = workspace.write(&path, "no").unwrap_err();
        assert!(error.contains("read-only"), "{error}");
        assert!(error.contains("workspace_writable"), "{error}");
        assert!(workspace.remove_file(&path).is_err());
    }
    assert!(workspace.remove_file("existing").is_err());
    assert_eq!(
        fs::read_to_string(temp.path().join("existing")).unwrap(),
        "data"
    );
}

#[test]
fn ro_workspace_explicit_rw_child_wins_by_specificity() {
    // A read-only workspace plus an explicit writable child: the most
    // specific entry (the child) allows the write, everything else inside
    // the workspace stays read-only.
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("child");
    fs::create_dir(&child).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        writable_paths: vec![child.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // The explicit RW child is the most specific entry: writes land even
    // though the workspace itself is read-only (absolute and relative).
    workspace
        .write(child.join("absolute").to_str().unwrap(), "abs")
        .unwrap();
    workspace.write("child/relative", "rel").unwrap();
    assert_eq!(fs::read(child.join("absolute")).unwrap(), b"abs");
    assert_eq!(fs::read(child.join("relative")).unwrap(), b"rel");
    // Reads inside the child work through either path.
    assert_eq!(
        workspace
            .read(child.join("absolute").to_str().unwrap())
            .unwrap(),
        b"abs"
    );
    // Other workspace paths stay read-only.
    assert!(workspace.write("denied", "no").is_err());
    assert!(
        workspace
            .write(temp.path().join("denied-abs").to_str().unwrap(), "no")
            .is_err()
    );
}

#[test]
fn workspace_entry_wins_over_external_ancestor() {
    // An external writable ancestor must not re-open the read-only
    // workspace: the workspace entry is the more specific logical entry.
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let workspace_dir = parent.join("ws");
    fs::create_dir_all(&workspace_dir).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        writable_paths: vec![parent.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    assert!(
        workspace
            .write(workspace_dir.join("file").to_str().unwrap(), "no")
            .is_err()
    );
    assert!(workspace.write("file", "no").is_err());
    // Paths outside the workspace under the external ancestor still write.
    workspace
        .write(parent.join("outside").to_str().unwrap(), "yes")
        .unwrap();
    assert_eq!(fs::read(parent.join("outside")).unwrap(), b"yes");
}

#[test]
fn workspace_entry_wins_over_equal_external_destination() {
    // An external writable entry whose destination equals the workspace
    // root loses the tie: the workspace entry governs.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    fs::create_dir_all(&workspace_dir).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        writable_paths: vec![workspace_dir.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let error = workspace
        .write(workspace_dir.join("file").to_str().unwrap(), "no")
        .unwrap_err();
    assert!(error.contains("read-only"), "{error}");
}

#[test]
fn sandbox_disabled_keeps_historical_workspace_writable() {
    // With the sandbox disabled the workspace is NOT a logical policy
    // entry: the file tools keep the historical always-writable workspace
    // even when `workspace_writable` happens to be false in the config.
    let temp = tempfile::tempdir().unwrap();
    let policy = crate::config::Sandbox {
        enabled: false,
        workspace_writable: false,
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    workspace.write("relative", "yes").unwrap();
    workspace
        .write(temp.path().join("absolute").to_str().unwrap(), "yes")
        .unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("relative")).unwrap(),
        "yes"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("absolute")).unwrap(),
        "yes"
    );
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Winner-before-mode (final review High 1)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[test]
fn rw_workspace_ro_child_dir_denies_writes_without_fallback() {
    // A read-only child dir under a writable (workspace) parent: the
    // most-specific RO winner is selected first, so writes to the child are
    // rejected outright (relative and absolute) instead of falling back to
    // the broader writable workspace. Reads select the same winner.
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("child");
    fs::create_dir(&child).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: true,
        readable_paths: vec![child.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    for path in [
        "child/file".to_owned(),
        child.join("file").to_str().unwrap().to_owned(),
    ] {
        let error = workspace.write(&path, "no").unwrap_err();
        assert!(error.contains("read-only external root"), "{error}");
        assert!(workspace.remove_file(&path).is_err());
    }
    assert!(!child.join("file").exists());
    // Reads and try-reads resolve through the same RO winner.
    fs::write(child.join("readable"), "data").unwrap();
    assert_eq!(workspace.read("child/readable").unwrap(), b"data");
    assert_eq!(
        workspace
            .read(child.join("readable").to_str().unwrap())
            .unwrap(),
        b"data"
    );
    assert_eq!(
        workspace.try_read_to_string("child/readable").unwrap(),
        Some("data".to_owned())
    );
    // The writable workspace parent still accepts writes elsewhere.
    workspace.write("other", "yes").unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("other")).unwrap(),
        "yes"
    );
}

#[test]
fn external_rw_parent_ro_child_winner_rejects_without_fallback() {
    // Adversarial: an RW external parent with an RO child (constructed
    // directly — config's normalize_roots rejects this combination, but the
    // resolver must still pick the child as the winner and never fall back
    // to the broader RW parent).
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    let parent = temp.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir_all(&child).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: true,
        writable_paths: vec![parent.to_string_lossy().into_owned()],
        readable_paths: vec![child.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let error = workspace
        .write(child.join("file").to_str().unwrap(), "no")
        .unwrap_err();
    assert!(error.contains("read-only external root"), "{error}");
    assert!(!child.join("file").exists());
    assert!(
        workspace
            .remove_file(child.join("file").to_str().unwrap())
            .is_err()
    );
    // Siblings under the RW parent keep the parent's own policy.
    workspace
        .write(parent.join("other").to_str().unwrap(), "yes")
        .unwrap();
    assert_eq!(fs::read(parent.join("other")).unwrap(), b"yes");
    // Reads of the RO child work through the same winner.
    fs::write(child.join("readable"), "data").unwrap();
    assert_eq!(
        workspace
            .read(child.join("readable").to_str().unwrap())
            .unwrap(),
        b"data"
    );
}

#[test]
fn rw_workspace_ro_exact_file_denies_only_the_file() {
    // An exact RO file under a writable (workspace) parent: the file itself
    // is write/edit/remove-denied, while siblings follow the parent's own
    // policy — the exact file is never expanded into a whole-directory deny.
    let temp = tempfile::tempdir().unwrap();
    let exact = temp.path().join("secret.txt");
    fs::write(&exact, "data").unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: true,
        readable_paths: vec![exact.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(temp.path())
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    for path in ["secret.txt".to_owned(), exact.to_str().unwrap().to_owned()] {
        let error = workspace.write(&path, "no").unwrap_err();
        assert!(error.contains("read-only external root"), "{error}");
        assert!(workspace.remove_file(&path).is_err());
    }
    assert_eq!(fs::read_to_string(&exact).unwrap(), "data");
    // Reads of the exact file work (read and try-read).
    assert_eq!(workspace.read("secret.txt").unwrap(), b"data");
    assert_eq!(
        workspace.try_read_to_string("secret.txt").unwrap(),
        Some("data".to_owned())
    );
    // Siblings are handled by the parent (workspace) policy: allowed.
    workspace.write("other.txt", "yes").unwrap();
    assert_eq!(
        fs::read_to_string(temp.path().join("other.txt")).unwrap(),
        "yes"
    );
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Same-source aliases with different destinations (final review High 2)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[test]
fn same_source_two_rw_aliases_both_resolve_and_write() {
    // Two RW aliases of the same canonical source are independent logical
    // entries: each is addressable at its own configured destination and
    // writes through to the canonical source.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let alias1 = temp.path().join("alias1");
    let alias2 = temp.path().join("alias2");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&source).unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![source.to_string_lossy().into_owned()],
        writable_mounts: vec![
            (
                source.to_string_lossy().into_owned(),
                alias1.to_string_lossy().into_owned(),
            ),
            (
                source.to_string_lossy().into_owned(),
                alias2.to_string_lossy().into_owned(),
            ),
        ],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    workspace
        .write(alias1.join("a").to_str().unwrap(), "one")
        .unwrap();
    workspace
        .write(alias2.join("b").to_str().unwrap(), "two")
        .unwrap();
    assert_eq!(fs::read(source.join("a")).unwrap(), b"one");
    assert_eq!(fs::read(source.join("b")).unwrap(), b"two");
    assert_eq!(
        workspace.read(alias1.join("a").to_str().unwrap()).unwrap(),
        b"one"
    );
    assert_eq!(
        workspace.read(alias2.join("b").to_str().unwrap()).unwrap(),
        b"two"
    );
}

#[test]
fn same_source_two_ro_aliases_both_resolve_read_only() {
    // Two RO aliases of the same canonical source: both are readable at
    // their own destinations and both reject writes with the RO winner
    // error (never falling back anywhere).
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let source = temp.path().join("source");
    let alias1 = temp.path().join("alias1");
    let alias2 = temp.path().join("alias2");
    fs::create_dir(&workspace_dir).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), "data").unwrap();
    let policy = crate::config::Sandbox {
        readable_paths: vec![source.to_string_lossy().into_owned()],
        readable_mounts: vec![
            (
                source.to_string_lossy().into_owned(),
                alias1.to_string_lossy().into_owned(),
            ),
            (
                source.to_string_lossy().into_owned(),
                alias2.to_string_lossy().into_owned(),
            ),
        ],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    for alias in [&alias1, &alias2] {
        assert_eq!(
            workspace
                .read(alias.join("file").to_str().unwrap())
                .unwrap(),
            b"data"
        );
        let error = workspace
            .write(alias.join("new").to_str().unwrap(), "no")
            .unwrap_err();
        assert!(error.contains("read-only external root"), "{error}");
        assert!(!source.join("new").exists());
    }
    // The canonical source itself stays readable too.
    assert_eq!(
        workspace
            .read(source.join("file").to_str().unwrap())
            .unwrap(),
        b"data"
    );
}

#[test]
fn same_source_two_exact_aliases_both_resolve() {
    // Two exact-file aliases of the same canonical source: both address the
    // same file at their own destinations and both reject sibling paths.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let exact = temp.path().join("exact");
    let alias1 = temp.path().join("exact1");
    let alias2 = temp.path().join("exact2");
    fs::create_dir(&workspace_dir).unwrap();
    fs::write(&exact, "old").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![exact.to_string_lossy().into_owned()],
        writable_mounts: vec![
            (
                exact.to_string_lossy().into_owned(),
                alias1.to_string_lossy().into_owned(),
            ),
            (
                exact.to_string_lossy().into_owned(),
                alias2.to_string_lossy().into_owned(),
            ),
        ],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    workspace.write(alias1.to_str().unwrap(), "v1").unwrap();
    assert_eq!(fs::read(&exact).unwrap(), b"v1");
    workspace.write(alias2.to_str().unwrap(), "v2").unwrap();
    assert_eq!(fs::read(&exact).unwrap(), b"v2");
    assert_eq!(workspace.read(alias1.to_str().unwrap()).unwrap(), b"v2");
    assert_eq!(workspace.read(alias2.to_str().unwrap()).unwrap(), b"v2");
    // Sibling paths under each exact alias stay rejected.
    for alias in [&alias1, &alias2] {
        let error = workspace
            .write(alias.join("sibling").to_str().unwrap(), "no")
            .unwrap_err();
        assert!(error.contains("sibling paths"), "{error}");
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Reroot provenance (final review: reroot adjacency)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[test]
fn reroot_ro_workspace_derives_only_from_rw_canonical_child() {
    // Sandbox enabled with a read-only workspace: the workspace itself is
    // not writable provenance, so reroot to the workspace root or to an
    // unlisted subdir is rejected — but an explicit writable canonical
    // child reroots through its RW external capability and succeeds, with
    // the rerooted workspace writable.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let child = workspace_dir.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir(workspace_dir.join("other")).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        writable_paths: vec![child.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // The RO workspace root and an unlisted subdir are not provenance.
    assert!(workspace.reroot(&workspace_dir).is_err());
    assert!(workspace.reroot(workspace_dir.join("other")).is_err());
    // The explicit RW canonical child derives from its RW capability.
    let rerooted = workspace.reroot(&child).unwrap();
    assert_eq!(rerooted.root(), child);
    rerooted.write("file", "yes").unwrap();
    assert_eq!(fs::read(child.join("file")).unwrap(), b"yes");
}

#[test]
fn reroot_rw_workspace_keeps_workspace_provenance_when_sandbox_enabled() {
    // A writable workspace (sandbox enabled, workspace_writable = true)
    // stays writable provenance: reroot to a subdir succeeds and the
    // rerooted workspace is writable.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("workspace");
    let child = workspace_dir.join("child");
    fs::create_dir_all(&child).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: true,
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let rerooted = workspace.reroot(&child).unwrap();
    assert_eq!(rerooted.root(), child);
    rerooted.write("file", "yes").unwrap();
    assert_eq!(fs::read(child.join("file")).unwrap(), b"yes");
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Reroot authority winner (final review: authority bypass)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[test]
fn reroot_ro_workspace_external_rw_ancestor_does_not_override() {
    // Sandbox enabled + RO workspace /parent/ws + external RW canonical
    // ancestor /parent: the requested path lies inside the RO workspace, so
    // the ancestor (less specific) must never override it — reroot rejects,
    // and no write access to the workspace can be derived through it.
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let workspace_dir = parent.join("ws");
    let child = workspace_dir.join("child");
    fs::create_dir_all(&child).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        writable_paths: vec![parent.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    // The RO workspace wins over the RW ancestor: reroot inside the
    // workspace is rejected.
    assert!(workspace.reroot(&child).is_err());
    // …and no write access to the workspace can be obtained.
    assert!(workspace.write("file", "no").is_err());
    assert!(!child.join("file").exists());
    // Paths OUTSIDE the workspace under the ancestor still reroot.
    let outside = parent.join("outside");
    fs::create_dir(&outside).unwrap();
    assert_eq!(workspace.reroot(&outside).unwrap().root(), outside);
}

#[test]
fn reroot_ro_workspace_external_rw_equal_does_not_override() {
    // Sandbox enabled + RO workspace whose root is ALSO configured as an
    // external RW root: equal canonical sources resolve to the workspace,
    // so the external equal cannot upgrade the RO workspace.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    let child = workspace_dir.join("child");
    fs::create_dir_all(&child).unwrap();
    let policy = crate::config::Sandbox {
        enabled: true,
        workspace_writable: false,
        writable_paths: vec![workspace_dir.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    assert!(workspace.reroot(&child).is_err());
    assert!(workspace.reroot(&workspace_dir).is_err());
    assert!(workspace.write("file", "no").is_err());
}

#[test]
fn reroot_external_rw_parent_ro_child_rejects_without_fallback() {
    // External RW parent + more-specific external RO child: the RO child is
    // the most specific source, so reroot under it is rejected outright —
    // never a fallback to the broader RW parent (constructed directly; the
    // config layer rejects this combination, but the resolver must be safe).
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    let parent = temp.path().join("data");
    let child = parent.join("sub");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir_all(&child).unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![parent.to_string_lossy().into_owned()],
        readable_paths: vec![child.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    assert!(workspace.reroot(&child).is_err());
    assert!(workspace.reroot(child.join("leaf")).is_err());
    // Siblings outside the RO child still reroot through the RW parent.
    let sibling = parent.join("other");
    fs::create_dir(&sibling).unwrap();
    assert_eq!(workspace.reroot(&sibling).unwrap().root(), sibling);
}

#[test]
fn reroot_external_rw_parent_more_specific_rw_child_wins() {
    // External RW parent + more-specific external RW child: the child
    // source is the authority winner, so reroot under it succeeds (pinned
    // positively; the no-fallback direction is covered by the RO-child
    // test above).
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    let parent = temp.path().join("data");
    let child = parent.join("sub");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir_all(&child).unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![
            parent.to_string_lossy().into_owned(),
            child.to_string_lossy().into_owned(),
        ],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    let rerooted = workspace.reroot(&child).unwrap();
    assert_eq!(rerooted.root(), child);
    rerooted.write("file", "yes").unwrap();
    assert_eq!(fs::read(child.join("file")).unwrap(), b"yes");
    // A deeper path under the child reroots through the child capability.
    let deep = child.join("deep");
    fs::create_dir(&deep).unwrap();
    let rerooted = workspace.reroot(&deep).unwrap();
    assert_eq!(rerooted.root(), deep);
}

#[test]
fn reroot_exact_file_source_rejects_without_fallback_to_broader_rw() {
    // A most-specific exact-file source is not a directory capability:
    // reroot to it is rejected even when a broader RW dir also matches.
    let temp = tempfile::tempdir().unwrap();
    let workspace_dir = temp.path().join("ws");
    let parent = temp.path().join("data");
    let exact = parent.join("exact");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(&exact, "x").unwrap();
    let policy = crate::config::Sandbox {
        writable_paths: vec![
            parent.to_string_lossy().into_owned(),
            exact.to_string_lossy().into_owned(),
        ],
        ..Default::default()
    };
    let workspace = Workspace::new(&workspace_dir)
        .unwrap()
        .with_external_roots(&policy)
        .unwrap();
    assert!(workspace.reroot(&exact).is_err());
    // A real dir under the parent still reroots.
    let sub = parent.join("sub");
    fs::create_dir(&sub).unwrap();
    assert_eq!(workspace.reroot(&sub).unwrap().root(), sub);
}
