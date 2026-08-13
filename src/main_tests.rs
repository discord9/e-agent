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
fn test_read_only_requested() {
    assert!(read_only_requested(&["--read-only".into()]));
    assert!(read_only_requested(&[
        "--workspace".into(),
        "/tmp".into(),
        "--read-only".into(),
        "audit".into()
    ]));
    assert!(!read_only_requested(&[]));
    assert!(!read_only_requested(&["--repl".into(), "prompt".into()]));
    assert!(!read_only_requested(&["--read".into(), "-only".into()]));
}

#[test]
fn test_build_version_is_package_version() {
    assert_eq!(BUILD_VERSION, env!("CARGO_PKG_VERSION"));
    let commit = env!("E_AGENT_COMMIT");
    let expected = if commit == "unknown" {
        format!("e-agent {BUILD_VERSION}")
    } else {
        format!("e-agent {BUILD_VERSION} ({commit})")
    };
    assert_eq!(version_line(), expected);
}

// Progressive skill disclosure tests

use e_agent::{
    config::Sandbox,
    session_factory::{
        SKILL_DEFAULT_DESCRIPTION, read_skills_index, scan_skills_dir, skills_root_is_dir,
    },
    workspace::Workspace,
};

fn write_skill(base: &std::path::Path, name: &str, content: &str) {
    let d = base.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("SKILL.md"), content).unwrap();
}

fn fm(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{name} secret body\n")
}

#[test]
fn test_skills_index_content_override_sort() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let gl = tmp.path().join("gl");
    let ws_skills = ws.join(".e-agent/skills");
    fs::create_dir_all(&ws_skills).unwrap();
    write_skill(&gl, "z-global", &fm("z-global", "global z"));
    write_skill(&gl, "common", &fm("common", "global common"));
    write_skill(&ws_skills, "a-ws", &fm("a-ws", "ws a"));
    write_skill(&ws_skills, "common", &fm("common", "ws wins"));
    // Same display name from different directories: the index sorts by
    // display name and tie-breaks deterministically on directory name.
    write_skill(&ws_skills, "dup-aaa", &fm("same-name", "dup one"));
    write_skill(&ws_skills, "dup-zzz", &fm("same-name", "dup two"));
    let index = read_skills_index(&ws, Some(&gl)).unwrap().unwrap();
    assert!(index.starts_with("## Skills\n\n- **a-ws**: ws a — "));
    assert!(index.contains("ws wins") && !index.contains("global common"));
    assert!(index.contains("z-global/SKILL.md") && !index.contains("secret body"));
    let pos = |s: &str| index.find(s).unwrap();
    assert!(pos("**a-ws**") < pos("**common**") && pos("**common**") < pos("**z-global**"));
    assert!(pos("dup-aaa/SKILL.md") < pos("dup-zzz/SKILL.md"));
}

#[test]
fn test_skills_index_frontmatter_fallbacks() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let ws_skills = ws.join(".e-agent/skills");
    fs::create_dir_all(&ws_skills).unwrap();
    write_skill(&ws_skills, "plain", "just a body");
    write_skill(&ws_skills, "unclosed", "---\nname: unclosed\n");
    write_skill(
        &ws_skills,
        "noname",
        "---\ndescription: only desc\n---\nbody",
    );
    write_skill(&ws_skills, "nodesc", "---\nname: nodesc\n---\nbody");
    let index = read_skills_index(&ws, None).unwrap().unwrap();
    assert!(index.contains("**plain**") && index.contains("**unclosed**"));
    assert!(index.contains("**noname**") && index.contains("only desc"));
    assert!(index.contains("**nodesc**"));
    assert_eq!(index.matches(SKILL_DEFAULT_DESCRIPTION).count(), 3);
    let missing = read_skills_index(&tmp.path().join("missing"), None).unwrap();
    assert!(missing.is_none());
}

#[test]
fn test_skills_read_capability_scoped_to_skills_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("ws");
    let skills = tmp.path().join("config/e-agent/skills");
    fs::create_dir_all(&ws_root).unwrap();
    write_skill(&skills, "alpha", &fm("alpha", "d"));
    let sibling = tmp.path().join("config/e-agent/config.toml");
    fs::write(&sibling, "sibling secret").unwrap();
    let policy = Sandbox {
        readable_paths: vec![skills.to_string_lossy().into_owned()],
        ..Sandbox::default()
    };
    let ws = Workspace::new(&ws_root).unwrap();
    let main_ws = ws.with_external_roots(&policy).unwrap();
    let skill_path = skills.join("alpha/SKILL.md").to_string_lossy().into_owned();
    let content = main_ws.read_to_string(&skill_path).unwrap();
    assert!(content.contains("secret body"));
    let err = main_ws
        .read_to_string(&sibling.to_string_lossy())
        .unwrap_err();
    assert!(err.contains("authorized external root"));
    let plain_ws = Workspace::new(&ws_root).unwrap();
    assert!(plain_ws.read_to_string(&skill_path).is_err());
}

#[cfg(unix)]
#[test]
fn test_skills_symlink_boundaries() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let ws_skills = ws.join(".e-agent/skills");
    // A workspace `.e-agent/skills` root that is itself a symlink skips
    // the whole workspace layer, exactly like a symlinked global root.
    let sym_ws = tmp.path().join("ws-sym");
    let real_ws_root = tmp.path().join("real-ws-skills");
    write_skill(&real_ws_root, "ws-skill", &fm("ws-skill", "d"));
    fs::create_dir_all(sym_ws.join(".e-agent")).unwrap();
    symlink(&real_ws_root, sym_ws.join(".e-agent/skills")).unwrap();
    let ws_index = read_skills_index(&sym_ws, None).unwrap();
    assert!(ws_index.is_none());
    fs::create_dir_all(&ws_skills).unwrap();
    let real_root = tmp.path().join("real-root");
    write_skill(&real_root, "global-skill", &fm("global-skill", "d"));
    let sym_root = tmp.path().join("sym-root");
    symlink(&real_root, &sym_root).unwrap();
    assert!(!skills_root_is_dir(&sym_root));
    let global = read_skills_index(&ws, Some(&sym_root)).unwrap();
    assert!(global.is_none());
    symlink(tmp.path().join("nowhere"), ws_skills.join("sym-dir")).unwrap();
    let sym_file_dir = ws_skills.join("sym-file");
    fs::create_dir_all(&sym_file_dir).unwrap();
    symlink(tmp.path().join("nowhere.md"), sym_file_dir.join("SKILL.md")).unwrap();
    write_skill(&ws_skills, "real", &fm("real", "real desc"));
    let entries = scan_skills_dir(&ws_skills).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "real");
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

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Concurrent-write conflict hint tests
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[test]
fn test_friendly_failure_with_conflict_marker() {
    let raw = "session failed: concurrent write conflict on table foo (status 409)";
    let friendly = friendly_failure(raw).expect("conflict marker must be recognized");
    assert!(
        friendly.contains("会话被其他客户端占用，已停止写入以避免数据冲突。"),
        "{friendly}"
    );
    assert!(
        friendly.contains("请关闭另一个 TUI / Web 窗口 / 导入工具后再试，或新开会话继续。"),
        "{friendly}"
    );
    assert!(friendly.contains("详情: "), "{friendly}");
    assert!(friendly.contains(raw), "{friendly}");
}

#[test]
fn test_friendly_failure_without_conflict_marker() {
    assert_eq!(friendly_failure("some other error"), None);
    assert_eq!(friendly_failure(""), None);
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
