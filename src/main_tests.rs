use super::*;
use std::fs;

#[test]
fn test_version_requested() {
    assert!(version_requested(&["--version".into()]).unwrap());
    assert!(version_requested(&["-V".into()]).unwrap());
    assert!(!version_requested(&[]).unwrap());
    assert!(!version_requested(&["--help".into()]).unwrap());
}

#[test]
fn test_version_rejects_additional_arguments() {
    for arguments in [
        vec!["--version".into(), "extra".into()],
        vec!["extra".into(), "-V".into()],
    ] {
        let error = version_requested(&arguments).unwrap_err().to_string();
        assert!(error.contains("does not accept arguments"), "{error}");
    }
}

#[test]
fn test_build_version_is_package_version() {
    assert_eq!(BUILD_VERSION, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        version_line(),
        format!("e-agent {}", env!("CARGO_PKG_VERSION"))
    );
}

fn write_skill(base: &std::path::Path, name: &str, content: &str) {
    let d = base.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("SKILL.md"), content).unwrap();
}

/// scan skip + error: missing dir, non-dir, missing/empty SKILL.md, UTF-8 error
#[test]
fn test_skills_scan_skip_and_error() {
    let tmp = tempfile::tempdir().unwrap();
    let gl = tmp.path().join("gl");
    // missing dir → empty
    assert!(read_skills_from(&gl).unwrap().is_empty());
    fs::create_dir_all(&gl).unwrap();
    // non-directory entry → skipped
    fs::write(gl.join("not_a_dir"), "content").unwrap();
    let d = gl.join("no-skill-md");
    fs::create_dir_all(&d).unwrap();
    let d2 = gl.join("empty-skill");
    fs::create_dir_all(&d2).unwrap();
    fs::write(d2.join("SKILL.md"), "").unwrap();
    let d3 = gl.join("ws-only");
    fs::create_dir_all(&d3).unwrap();
    fs::write(d3.join("SKILL.md"), "   \n  \t  ").unwrap();
    // only the real skill survives
    write_skill(&gl, "real", "content");
    let loaded = read_skills_from(&gl).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].0, "real");
    // UTF-8 error
    let bad = gl.join("bad-utf8");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("SKILL.md"), [0xff, 0xfe, 0x00]).unwrap();
    let err = read_skills_from(&gl).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("bad-utf8/SKILL.md"), "{msg}");
    assert!(msg.contains("UTF-8"), "{msg}");
}

/// merge: sort across dirs, workspace override, content intact
#[test]
fn test_skills_merge_sort_override_and_content() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let gl = tmp.path().join("gl");
    write_skill(&gl, "z-global", "zzz");
    write_skill(&ws, "a-ws", "aaa");
    write_skill(&ws, "common", "ws wins");
    write_skill(&gl, "common", "global lose");
    write_skill(&ws, "m-ws", "line 1\nline 2\n\nline 4");
    let result = read_skills_merge(Some(&gl), &ws).unwrap().unwrap();
    assert_eq!(
        result,
        "## Skill: a-ws\n\naaa\n\n\
         ## Skill: common\n\nws wins\n\n\
         ## Skill: m-ws\n\nline 1\nline 2\n\nline 4\n\n\
         ## Skill: z-global\n\nzzz"
    );
    assert!(!result.contains("global lose"), "override failed");
}

/// merge: global_dir=None and both dirs missing
#[test]
fn test_skills_merge_global_none_and_missing() {
    let tmp = tempfile::tempdir().unwrap();
    // global=None, workspace exists
    let ws = tmp.path().join("ws");
    write_skill(&ws, "only-ws", "content");
    assert_eq!(
        read_skills_merge(None, &ws).unwrap().unwrap(),
        "## Skill: only-ws\n\ncontent"
    );
    // both missing → None
    let missing = tmp.path().join("missing");
    assert_eq!(read_skills_merge(Some(&missing), &missing).unwrap(), None);
}

#[test]
fn test_tui_session_report_ok() {
    let lines = tui_report_lines("abc-123", true);
    assert_eq!(lines, ["e-agent: resume with: e-agent --session abc-123"]);
}

#[test]
fn test_tui_session_report_err() {
    let lines = tui_report_lines("xyz-789", false);
    assert_eq!(
        lines,
        [
            "e-agent: session xyz-789 (the failed turn may not have been persisted)",
            "e-agent: resume with: e-agent --session xyz-789",
        ]
    );
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Crash diagnostic tests
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[test]
fn test_format_crash_report() {
    let r = format_crash_report(1700000000, "main", Some("src/main.rs:42:5"), "  bt line\n");
    assert!(r.contains("timestamp: 1700000000\n"), "{r}");
    assert!(r.contains("thread: main\n"));
    assert!(r.contains("location: src/main.rs:42:5\n"));
    assert!(r.contains("panic payload omitted\n"));
    assert!(r.contains("  bt line\n"));
    assert!(r.ends_with('\n'));
}

#[test]
fn test_format_crash_report_no_location() {
    let r = format_crash_report(0, "t", None, "bt\n");
    assert!(!r.contains("location:"));
    assert!(r.contains("panic payload omitted"));
}

#[test]
fn test_crash_dir_inner() {
    // XDG takes precedence
    let d = crash_dir_inner(Some(OsStr::new("/xdg/state")), Some(OsStr::new("/home/u")));
    assert_eq!(d, Some(PathBuf::from("/xdg/state/e-agent/crash")));
    // Empty XDG -> fallback to HOME
    let d = crash_dir_inner(Some(OsStr::new("")), Some(OsStr::new("/home/u")));
    assert_eq!(d, Some(PathBuf::from("/home/u/.config/e-agent/crash")));
    // No XDG, no HOME -> None
    let d = crash_dir_inner(None, None);
    assert_eq!(d, None);
}

#[test]
fn test_write_crash_report_replaces_latest_and_is_private() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("config/e-agent/crash");
    let path = write_crash_report(&dir, "first report\n").unwrap();
    assert_eq!(path, dir.join("latest.log"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "first report\n");
    assert!(!dir.join("latest.tmp").exists());

    write_crash_report(&dir, "second report\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "second report\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

// This is invoked only by the integration-style test below in a fresh test
// process. It is not a command-line feature.
#[test]
fn panic_hook_subprocess() {
    if std::env::var_os("E_AGENT_PANIC_HOOK_TEST").is_some() {
        install_panic_hook();
        panic!("test payload must stay private");
    }
}

#[test]
fn test_panic_hook_forces_stack_and_writes_report_without_rust_backtrace() {
    let state = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::panic_hook_subprocess", "--nocapture"])
        .env("E_AGENT_PANIC_HOOK_TEST", "1")
        .env("XDG_STATE_HOME", state.path())
        .env_remove("RUST_BACKTRACE")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("e-agent: Rust panic on thread"), "{stderr}");
    assert!(stderr.contains("backtrace:"), "{stderr}");
    assert!(stderr.contains("crash report:"), "{stderr}");
    assert!(
        !stderr.contains("test payload must stay private"),
        "{stderr}"
    );

    let report = fs::read_to_string(state.path().join("e-agent/crash/latest.log")).unwrap();
    assert!(report.contains("backtrace:"), "{report}");
    assert!(report.contains("panic payload omitted"), "{report}");
}

#[test]
fn test_acknowledge_crash_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let latest = tmp.path().join("latest.log");
    let previous = tmp.path().join("previous.log");

    // Write a fake report
    let report = "timestamp: 1\nthread: main\n\
                   location: foo.rs:1:1\n\
                   panic payload omitted\nbacktrace:\n  bt\n";
    std::fs::write(&latest, report).unwrap();

    assert!(acknowledge_crash(&latest, &previous));
    assert!(!latest.exists());
    assert!(previous.exists());

    // No latest file -> false
    assert!(!acknowledge_crash(&latest, &previous));
}
