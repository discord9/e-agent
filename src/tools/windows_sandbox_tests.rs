use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;

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

/// Wrap a command body with the shell's strict-mode prologue so any failing
/// step aborts the whole command instead of continuing with a partial state.
fn strict(shell: &Shell, body: &str) -> String {
    if shell.executable.ends_with("bash.exe") {
        format!("set -e; {body}")
    } else {
        format!("$ErrorActionPreference='Stop'; {body}")
    }
}

fn write_command(shell: &Shell, path: &std::path::Path, value: &str) -> String {
    if shell.executable.ends_with("bash.exe") {
        format!("set -e; printf '%s' '{value}' > '{}'", path.display())
    } else {
        format!(
            "$ErrorActionPreference='Stop'; Set-Content -LiteralPath '{}' -Value '{}' -NoNewline",
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
async fn preexisting_file_can_be_deleted() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    let old_file = workspace_root.join("old.txt");
    std::fs::write(&old_file, "old").unwrap();
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "set -e; rm 'old.txt'"
    } else {
        "$ErrorActionPreference='Stop'; Remove-Item -LiteralPath 'old.txt'"
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert!(!old_file.exists(), "pre-existing file was not deleted");
}

#[tokio::test]
async fn rename_overwrite_replaces_existing_target() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    let target = workspace_root.join("target.txt");
    let replacement = workspace_root.join("target.tmp");
    std::fs::write(&target, "original").unwrap();
    std::fs::write(&replacement, "replacement").unwrap();
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "set -e; mv -f 'target.tmp' 'target.txt'"
    } else {
        "$ErrorActionPreference='Stop'; Move-Item -LiteralPath 'target.tmp' -Destination 'target.txt' -Force"
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "replacement",
        "rename-overwrite did not replace the target"
    );
    assert!(!replacement.exists());
}

#[tokio::test]
async fn newly_created_file_can_be_deleted() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    let file = workspace_root.join("new.txt");
    bash.execute(json!({"command": write_command(&bash.shell, &file, "new")}))
        .await
        .unwrap();
    assert!(file.exists());
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "set -e; rm 'new.txt'"
    } else {
        "$ErrorActionPreference='Stop'; Remove-Item -LiteralPath 'new.txt'"
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert!(!file.exists(), "newly created file was not deleted");
}

#[tokio::test]
async fn empty_directory_can_be_renamed_and_deleted() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    let old_dir = workspace_root.join("old-dir");
    let renamed_dir = workspace_root.join("renamed-dir");
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "set -e; mkdir 'old-dir'; mv 'old-dir' 'renamed-dir'; rmdir 'renamed-dir'"
    } else {
        "$ErrorActionPreference='Stop'; New-Item -ItemType Directory -Path 'old-dir' | Out-Null; Move-Item -LiteralPath 'old-dir' -Destination 'renamed-dir'; Remove-Item -LiteralPath 'renamed-dir'"
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert!(!old_dir.exists());
    assert!(!renamed_dir.exists());
}

#[tokio::test]
async fn nonempty_directory_can_be_recursively_deleted() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));

    let old_dir = workspace_root.join("old-dir");
    let child = old_dir.join("child.txt");
    std::fs::create_dir(&old_dir).unwrap();
    std::fs::write(&child, "child").unwrap();
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "set -e; rm -rf 'old-dir'"
    } else {
        "$ErrorActionPreference='Stop'; Remove-Item -LiteralPath 'old-dir' -Recurse"
    };
    bash.execute(json!({"command": command})).await.unwrap();
    assert!(!old_dir.exists(), "non-empty directory was not deleted");
    assert!(!child.exists());
}

#[tokio::test]
async fn git_operations_leave_no_index_lock() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    let git_dir = workspace_root.join(".git");
    let tracked = workspace_root.join("tracked.txt");

    // Each git step runs as its own sandboxed launch so the lock file can be
    // asserted gone after every step.
    bash.execute(json!({"command": strict(&bash.shell, "git init --quiet")}))
        .await
        .unwrap();
    assert!(
        !git_dir.join("index.lock").exists(),
        ".git/index.lock left after git init"
    );
    bash
        .execute(json!({"command": strict(&bash.shell, "git config user.email test@example.invalid; git config user.name test")}))
        .await
        .unwrap();
    bash.execute(json!({"command": write_command(&bash.shell, &tracked, "one")}))
        .await
        .unwrap();
    bash.execute(json!({"command": strict(&bash.shell, "git add tracked.txt")}))
        .await
        .unwrap();
    assert!(
        !git_dir.join("index.lock").exists(),
        ".git/index.lock left after first git add"
    );
    bash.execute(json!({"command": strict(&bash.shell, "git commit --quiet -m one")}))
        .await
        .unwrap();
    assert!(
        !git_dir.join("index.lock").exists(),
        ".git/index.lock left after first commit"
    );
    bash.execute(json!({"command": write_command(&bash.shell, &tracked, "two")}))
        .await
        .unwrap();
    bash.execute(json!({"command": strict(&bash.shell, "git add tracked.txt")}))
        .await
        .unwrap();
    bash.execute(json!({"command": strict(&bash.shell, "git commit --quiet -m two")}))
        .await
        .unwrap();
    assert!(
        !git_dir.join("index.lock").exists(),
        ".git/index.lock left after second commit"
    );
    bash.execute(
        json!({"command": strict(&bash.shell, "git checkout --quiet HEAD~ -- tracked.txt")}),
    )
    .await
    .unwrap();
    assert!(
        !git_dir.join("index.lock").exists(),
        ".git/index.lock left after checkout"
    );
    assert_eq!(
        std::fs::read_to_string(&tracked).unwrap(),
        "one",
        "checkout did not restore the first revision"
    );
}

#[tokio::test]
async fn hard_linked_descendant_is_rejected_with_all_names() {
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

    assert!(
        error.contains("has a name outside the configured writable roots"),
        "{error}"
    );
    assert!(error.contains("Scanned path:"), "{error}");
    assert!(error.contains(&linked.display().to_string()), "{error}");
    assert!(error.contains("All hard-link names:"), "{error}");
    assert!(error.contains(&outside.display().to_string()), "{error}");
    assert!(
        error.contains(
            "Hard links whose every name is inside the configured writable roots are supported."
        ),
        "{error}"
    );
    assert!(
        error.contains(
            "Tools such as Cargo may create hard links as part of link-or-copy operations."
        ),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
}

#[tokio::test]
async fn hard_links_entirely_inside_single_root_install_succeeds() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let original = workspace_root.join("original.txt");
    let first = workspace_root.join("first-link.txt");
    let second = workspace_root.join("second-link.txt");
    std::fs::write(&original, "content").unwrap();
    std::fs::hard_link(&original, &first).unwrap();
    std::fs::hard_link(&original, &second).unwrap();

    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    bash.execute(json!({"command": "exit 0"})).await.unwrap();

    // A file created after installation inherits the capability ACE, proving
    // the install completed despite the pre-existing hard links.
    let fresh = workspace_root.join("fresh.txt");
    bash.execute(json!({"command": write_command(&bash.shell, &fresh, "fresh")}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "fresh");
}

#[tokio::test]
async fn hard_links_across_two_configured_roots_install_succeeds() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    let extra = parent.path().join("extra");
    std::fs::create_dir_all(&workspace_root).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    let original = workspace_root.join("original.txt");
    let linked = extra.join("linked.txt");
    std::fs::write(&original, "content").unwrap();
    std::fs::hard_link(&original, &linked).unwrap();

    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(
        workspace,
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
    bash.execute(json!({"command": "exit 0"})).await.unwrap();

    let fresh = workspace_root.join("fresh.txt");
    bash.execute(json!({"command": write_command(&bash.shell, &fresh, "fresh")}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "fresh");
    let fresh_extra = extra.join("fresh-extra.txt");
    bash.execute(json!({"command": write_command(&bash.shell, &fresh_extra, "extra")}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&fresh_extra).unwrap(), "extra");
}

#[tokio::test]
async fn hard_link_with_three_names_one_outside_is_rejected_with_all_names() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    let outside_dir = parent.path().join("outside-dir");
    std::fs::create_dir(&workspace_root).unwrap();
    std::fs::create_dir(&outside_dir).unwrap();
    let original = workspace_root.join("original.txt");
    let first = workspace_root.join("first-link.txt");
    let outside = outside_dir.join("outside-link.txt");
    std::fs::write(&original, "content").unwrap();
    std::fs::hard_link(&original, &first).unwrap();
    std::fs::hard_link(&original, &outside).unwrap();

    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    let error = bash
        .execute(json!({"command": "exit 0"}))
        .await
        .unwrap_err();

    assert!(
        error.contains("has a name outside the configured writable roots"),
        "{error}"
    );
    assert!(error.contains(&original.display().to_string()), "{error}");
    assert!(error.contains(&first.display().to_string()), "{error}");
    assert!(error.contains(&outside.display().to_string()), "{error}");
}

#[tokio::test]
async fn hard_link_created_with_different_case_inside_root_installs_succeeds() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let original = workspace_root.join("original.txt");
    let linked = workspace_root.join("ORIGINAL.TXT");
    std::fs::write(&original, "content").unwrap();
    std::fs::hard_link(&original, &linked).unwrap();

    // The second alias name differs in case from the first. NTFS preserves
    // the stored case, so membership must be decided with a Windows
    // case-insensitive component comparison, not a case-sensitive prefix or
    // a lossy to_lowercase.
    let workspace = Workspace::new(&workspace_root).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    bash.execute(json!({"command": "exit 0"})).await.unwrap();

    let fresh = workspace_root.join("fresh.txt");
    bash.execute(json!({"command": write_command(&bash.shell, &fresh, "fresh")}))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "fresh");
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
    // The file itself carries no capability ACE (it was created outside the
    // root), so the restricted-token write is still denied.
    let error = bash
        .execute(json!({"command": write_command(&bash.shell, &linked, "changed")}))
        .await
        .unwrap_err();
    assert!(
        !error.contains("has a name outside the configured writable roots"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");

    let error = bash
        .execute(json!({"command": write_command(&bash.shell, &outside, "changed")}))
        .await
        .unwrap_err();
    assert!(
        !error.contains("has a name outside the configured writable roots"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
}

#[tokio::test]
async fn restricted_token_preserves_output_and_nonzero_exit() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let bash = test_bash(workspace, windows_policy(true, Vec::new()));
    let command = if bash.shell.executable.ends_with("bash.exe") {
        "set -e; printf out; printf err >&2; exit 7"
    } else {
        // [Console]::Error.WriteLine() is a method invocation — forbidden in
        // ConstrainedLanguage mode under the restricted token. Write-Error is
        // a native cmdlet (no method call) and still writes to stderr.
        "$ErrorActionPreference='Stop'; Write-Output 'out'; Write-Error 'err'; exit 7"
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

/// True when the root DACL carries exactly the two explicit v4 ACEs for the
/// workspace capability SID and nothing else for that SID. Uses the same
/// structural check and SID derivation as the sandbox itself.
fn root_acl_has_exact_v4_layout(root: &std::path::Path) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let canonical = crate::canonicalize_path(root).expect("workspace root canonicalizes");
    let sid_text = super::windows_sandbox::stable_sid(&canonical, "workspace");
    let mut sid_w: Vec<u16> = sid_text.encode_utf16().chain(Some(0)).collect();
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(sid_w.as_mut_ptr(), &mut sid) } == 0 {
        return false;
    }
    let mut path_w: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut acl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS || acl.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor) };
        }
        return false;
    }
    let ok = unsafe { super::windows_sandbox::v4_ace_layout_matches(acl, sid) };
    unsafe { LocalFree(sid) };
    unsafe { LocalFree(descriptor) };
    ok
}

#[tokio::test]
async fn concurrent_first_installs_serialize_and_leave_exact_v4_layout() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = Workspace::new(&workspace_root).unwrap();
    let policy = windows_policy(true, Vec::new());
    let bash = test_bash(workspace.clone(), policy.clone());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::agent::AgentEvent>();

    for round in 0..3 {
        let target_dir = workspace_root.join(format!("target-{round}"));
        let background_file = target_dir.join("background.txt");
        let foreground_file = target_dir.join("foreground.txt");
        let mkdir_and_write = |shell: &Shell, file: &std::path::Path, value: &str| {
            if shell.executable.ends_with("bash.exe") {
                format!(
                    "set -e; mkdir -p '{}'; printf '%s' '{value}' > '{}'",
                    target_dir.display(),
                    file.display()
                )
            } else {
                format!(
                    "$ErrorActionPreference='Stop'; New-Item -ItemType Directory -Force -Path '{}' | Out-Null; Set-Content -LiteralPath '{}' -Value '{value}' -NoNewline",
                    target_dir.display(),
                    file.display()
                )
            }
        };

        // Background and foreground race through the same fresh write root:
        // both must pass preflight + scan + ACE install + command execution.
        let started = bash
            .background
            .start_with_sender(
                workspace.clone(),
                mkdir_and_write(&bash.shell, &background_file, "bg"),
                false,
                Some(tx.clone()),
                Some(policy.clone()),
            )
            .unwrap();
        assert!(started.contains("started background task"), "{started}");
        bash.execute(json!({"command": mkdir_and_write(&bash.shell, &foreground_file, "fg")}))
            .await
            .unwrap();

        let completion = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                match rx.recv().await {
                    Some(crate::agent::AgentEvent::BackgroundCompleted { .. }) => break true,
                    Some(_) => continue,
                    None => break false,
                }
            }
        })
        .await;
        assert!(
            matches!(completion, Ok(true)),
            "background task did not complete in time during round {round}"
        );

        assert!(
            background_file.exists(),
            "background file missing after round {round}"
        );
        assert!(
            foreground_file.exists(),
            "foreground file missing after round {round}"
        );
        assert_eq!(
            std::fs::read_to_string(&background_file).unwrap(),
            "bg",
            "background write wrong after round {round}"
        );
        assert_eq!(
            std::fs::read_to_string(&foreground_file).unwrap(),
            "fg",
            "foreground write wrong after round {round}"
        );
    }

    assert!(
        root_acl_has_exact_v4_layout(&workspace_root),
        "root ACL does not have the exact two-ACE v4 layout after concurrent installs"
    );
}
