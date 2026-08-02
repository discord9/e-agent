use super::*;

fn windows_policy(workspace_writable: bool, writable_paths: Vec<String>) -> crate::config::Sandbox {
    crate::config::Sandbox {
        enabled: true,
        network: true,
        workspace_writable,
        writable_paths,
        readable_paths: Vec::new(),
    }
}

fn test_bash(workspace: Workspace, policy: crate::config::Sandbox) -> Bash {
    Bash {
        workspace,
        timeout: Some(Duration::from_secs(15)),
        sender: None,
        background: BackgroundTasks::new(Some(Duration::from_secs(15)), Some(policy.clone())),
        sandbox: Some(policy),
        protect_git: false,
        shell: Shell::detect().unwrap(),
    }
}

fn write_command(shell: &Shell, path: &std::path::Path, value: &str) -> String {
    if shell.executable.ends_with("bash.exe") {
        format!("printf '%s' '{value}' > '{}'", path.display())
    } else {
        format!(
            "Set-Content -LiteralPath '{}' -Value '{}' -NoNewline",
            path.display(),
            value
        )
    }
}

#[tokio::test]
async fn restricted_token_enforces_configured_write_roots() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    let extra = parent.path().join("extra");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir_all(&workspace_root).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();

    let writable = test_bash(
        workspace.clone(),
        windows_policy(
            true,
            vec![
                crate::canonicalize_path(&extra)
                    .unwrap()
                    .into_os_string()
                    .into_string()
                    .unwrap(),
            ],
        ),
    );
    let workspace_file = workspace_root.join("workspace.txt");
    writable
        .execute(json!({"command": write_command(&writable.shell, &workspace_file, "workspace")}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&workspace_file).unwrap(),
        "workspace"
    );

    let extra_file = extra.join("extra.txt");
    writable
        .execute(json!({"command": write_command(&writable.shell, &extra_file, "extra")}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&extra_file).unwrap(), "extra");

    let sibling_file = sibling.join("denied.txt");
    writable
        .execute(json!({"command": write_command(&writable.shell, &sibling_file, "escape")}))
        .await
        .unwrap_err();
    assert!(!sibling_file.exists());

    let read_only = test_bash(workspace, windows_policy(false, Vec::new()));
    let denied_workspace_file = workspace_root.join("denied.txt");
    read_only
        .execute(
            json!({"command": write_command(&read_only.shell, &denied_workspace_file, "escape")}),
        )
        .await
        .unwrap_err();
    assert!(!denied_workspace_file.exists());
}

#[tokio::test]
async fn capability_supports_rename_delete_and_atomic_replace_but_not_outside() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&workspace_root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    let old_file = workspace_root.join("old.txt");
    let renamed_file = workspace_root.join("renamed.txt");
    let old_dir = workspace_root.join("old-dir");
    let renamed_dir = workspace_root.join("renamed-dir");
    let target = workspace_root.join("target.txt");
    let replacement = workspace_root.join("target.tmp");
    std::fs::write(&old_file, "old").unwrap();
    std::fs::create_dir(&old_dir).unwrap();
    std::fs::write(old_dir.join("child.txt"), "child").unwrap();
    std::fs::write(&target, "original").unwrap();
    std::fs::write(&replacement, "replacement").unwrap();

    let command = if bash.shell.executable.ends_with("bash.exe") {
        format!(
            "mv '{}' '{}'; rm '{}'; mv '{}' '{}'; rm -rf '{}'; mv -f '{}' '{}'",
            old_file.display(),
            renamed_file.display(),
            renamed_file.display(),
            old_dir.display(),
            renamed_dir.display(),
            renamed_dir.display(),
            replacement.display(),
            target.display(),
        )
    } else {
        format!(
            "Move-Item -LiteralPath '{}' -Destination '{}'; Remove-Item -LiteralPath '{}'; Move-Item -LiteralPath '{}' -Destination '{}'; Remove-Item -LiteralPath '{}' -Recurse; Move-Item -LiteralPath '{}' -Destination '{}' -Force",
            old_file.display(),
            renamed_file.display(),
            renamed_file.display(),
            old_dir.display(),
            renamed_dir.display(),
            renamed_dir.display(),
            replacement.display(),
            target.display(),
        )
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert!(!old_file.exists());
    assert!(!renamed_file.exists());
    assert!(!old_dir.exists());
    assert!(!renamed_dir.exists());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "replacement");

    // Exercise Git's lock-file/rename based updates and checkout replacement.
    let tracked = workspace_root.join("tracked.txt");
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "git init --quiet; git config user.email test@example.invalid; git config user.name test; printf one > tracked.txt; git add tracked.txt; git commit --quiet -m one; printf two > tracked.txt; git add tracked.txt; git commit --quiet -m two; git checkout --quiet HEAD~ -- tracked.txt"
    } else {
        "git init --quiet; git config user.email test@example.invalid; git config user.name test; Set-Content -LiteralPath tracked.txt -Value one -NoNewline; git add tracked.txt; git commit --quiet -m one; Set-Content -LiteralPath tracked.txt -Value two -NoNewline; git add tracked.txt; git commit --quiet -m two; git checkout --quiet HEAD~ -- tracked.txt"
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "one");

    let outside_file = outside.join("denied.txt");
    std::fs::write(&outside_file, "outside").unwrap();
    let outside_renamed = outside.join("renamed.txt");
    let command = if bash.shell.executable.ends_with("bash.exe") {
        format!(
            "mv '{}' '{}'",
            outside_file.display(),
            outside_renamed.display()
        )
    } else {
        format!(
            "Move-Item -LiteralPath '{}' -Destination '{}'",
            outside_file.display(),
            outside_renamed.display()
        )
    };
    bash.execute(json!({"command": command})).await.unwrap_err();
    assert!(outside_file.exists());
    assert!(!outside_renamed.exists());
}

#[tokio::test]
async fn hard_linked_descendant_is_rejected_before_external_file_can_change() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let outside = parent.path().join("outside.txt");
    std::fs::write(&outside, "unchanged").unwrap();
    let linked = workspace_root.join("linked.txt");
    std::fs::hard_link(&outside, &linked).unwrap();

    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    let error = bash
        .execute(json!({"command": write_command(&bash.shell, &linked, "changed")}))
        .await
        .unwrap_err();

    assert!(error.contains("hard-linked descendants"), "{error}");
    assert!(error.contains(&linked.display().to_string()), "{error}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
}

#[tokio::test]
async fn installed_root_skips_rescan_but_hard_link_cannot_modify_outside() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    // First launch installs and propagates the complete versioned ACE.
    bash.execute(json!({"command": "exit 0"})).await.unwrap();

    let outside = parent.path().join("outside.txt");
    std::fs::write(&outside, "unchanged").unwrap();
    let linked = workspace_root.join("linked.txt");
    std::fs::hard_link(&outside, &linked).unwrap();

    // The exact ACE makes this launch a no-op, so it does not rescan/reject.
    let error = bash
        .execute(json!({"command": write_command(&bash.shell, &linked, "changed")}))
        .await
        .unwrap_err();
    assert!(!error.contains("hard-linked descendants"), "{error}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");

    let error = bash
        .execute(json!({"command": write_command(&bash.shell, &outside, "changed")}))
        .await
        .unwrap_err();
    assert!(!error.contains("hard-linked descendants"), "{error}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
}

#[tokio::test]
async fn restricted_token_preserves_output_and_nonzero_exit() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "printf out; printf err >&2; exit 7"
    } else {
        "Write-Output 'out'; Write-Error 'err'; exit 7"
    };
    let error = bash.execute(json!({"command": command})).await.unwrap_err();
    assert!(error.contains("exit code: 7"), "{error}");
    assert!(error.contains("stdout:\nout"), "{error}");
    assert!(error.contains("stderr:"), "{error}");
    assert!(error.contains("err"), "{error}");
}

#[tokio::test]
async fn restricted_token_rejects_network_false_at_execution() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    let mut policy = bash.sandbox.clone().unwrap();
    policy.network = false;
    let bash = test_bash(bash.workspace, policy);
    let error = bash
        .execute(json!({"command": "exit 0"}))
        .await
        .unwrap_err();
    assert!(
        error.contains("does not implement network isolation"),
        "{error}"
    );
}

#[tokio::test]
async fn protected_git_is_rejected_before_acl_preflight_or_process_start() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let mut policy = windows_policy(true, Vec::new());
    policy.writable_paths.push(
        temp.path()
            .join("missing-write-root")
            .to_string_lossy()
            .into_owned(),
    );
    let mut bash = test_bash(workspace, policy);
    bash.protect_git = true;
    let marker = temp.path().join("must-not-exist.txt");
    let command = write_command(&bash.shell, &marker, "started");
    let error = bash.execute(json!({"command": command})).await.unwrap_err();
    assert_eq!(
        error,
        "Windows write-sandbox MVP does not support protected-git shell execution"
    );
    assert!(!marker.exists());
}
