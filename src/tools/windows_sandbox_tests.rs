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
