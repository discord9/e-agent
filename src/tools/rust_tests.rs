//! run_rust tests: registration gating, fail-closed preflight, scratch
//! authority under hostile swaps, the socketpair status protocol, cancellation
//! and cleanup, and the sandbox policy surface.
use super::*;
#[rustfmt::skip]
use super::rust::{FAULT_CONFIRM, FAULT_KILL, KillGuard, RunRust, SOURCE_MAX_BYTES, STAGE_TIMEOUT, ScratchGuard, base_dir, create_scratch, create_scratch_once, group_absent, kill_group, reap_confirmed, remove_scratch, resolve_rustc, run_rust_policy, validate_source};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
#[rustfmt::skip]
fn env_ok() -> bool { bwrap_available() && resolve_rustc(std::env::var_os("PATH").as_deref()).is_some() }
#[rustfmt::skip]
fn sb() -> crate::config::Sandbox { crate::config::Sandbox { enabled: true, ..Default::default() } }
fn policy(w: bool, wpaths: &[&str], rpaths: &[&str]) -> crate::config::Sandbox {
    let mut s = run_rust_policy(None);
    s.workspace_writable = w;
    s.writable_paths = wpaths.iter().map(ToString::to_string).collect();
    s.readable_paths = rpaths.iter().map(ToString::to_string).collect();
    s
}
#[rustfmt::skip]
struct Harness { tool: RunRust, base: tempfile::TempDir }
#[rustfmt::skip]
fn harness(ws: &Workspace, policy: crate::config::Sandbox) -> Option<Harness> {
    if !env_ok() { return None; }
    let base = tempfile::tempdir().unwrap();
    Some(Harness { tool: RunRust { workspace: ws.clone(), policy, compile_timeout: STAGE_TIMEOUT, run_timeout: Duration::from_secs(30), scratch: Some(base.path().to_path_buf()) }, base })
}
#[rustfmt::skip]
fn hdir(policy: crate::config::Sandbox) -> Option<(Harness, tempfile::TempDir)> {
    let temp = tempfile::tempdir().unwrap();
    harness(&Workspace::new(temp.path()).unwrap(), policy).map(|h| (h, temp))
}
#[rustfmt::skip]
impl Harness {
    // An extra `reasoning` field must be irrelevant: only `source` is read.
    async fn run(&self, src: &str) -> Result<String, String> { self.tool.execute(json!({"source": src, "reasoning": ""})).await.map(|o| o.content) }
    async fn run_ok(&self, src: &str, want: &str) { let r = self.run(src).await.unwrap_or_else(|e| panic!("{e}")); assert!(r.contains(want), "missing {want}: {r}"); }
    async fn run_err(&self, src: &str, want: &str) { let e = self.run(src).await.unwrap_err(); assert!(e.contains(want), "missing {want}: {e}"); }
    fn scratch_empty(&self) -> bool { std::fs::read_dir(self.base.path()).unwrap().count() == 0 }
}
// `Some(marker)`: wait for a scratch containing it; `None`: all scratches gone.
#[rustfmt::skip]
async fn wait_scratch(base: &Path, marker: Option<&str>) {
    for _ in 0..500 { let done = match marker { Some(m) => std::fs::read_dir(base).into_iter().flatten().flatten().any(|e| e.path().join(m).exists()), None => std::fs::read_dir(base).map(|mut it| it.next().is_none()).unwrap_or(true) }; if done { return; } tokio::time::sleep(Duration::from_millis(10)).await; }
    panic!("scratch wait failed in {}", base.display());
}
#[rustfmt::skip]
#[test]
fn registration_and_preflight_gate_run_rust() {
    let temp = tempfile::tempdir().unwrap();
    let ws = Workspace::new(temp.path()).unwrap();
    let reg = |code: bool, ro: bool, sb: Option<crate::config::Sandbox>| { let mut tools: Vec<Box<dyn Tool>> = Vec::new(); crate::session_factory::register_run_rust(&mut tools, &ws, code, ro, sb); tools.iter().any(|t| t.spec().name == "run_rust") };
    // default off; read-only without any sandbox fails closed; else registered
    assert!(!reg(false, false, None) && !reg(true, true, None));
    assert!(reg(true, true, Some(sb())) && reg(true, false, None));
    let sub = builtins_with_background(ws.clone(), BackgroundTasks::new(None, None), Some(sb()), false, true, None);
    assert!(!sub.iter().any(|t| t.spec().name == "run_rust"), "subagents never inherit run_rust");
    let path = std::env::var_os("PATH");
    let empty = tempfile::tempdir().unwrap();
    let no_rustc = std::env::join_paths([empty.path()]).unwrap();
    for (bwrap_ok, path, want) in [(false, path.as_deref(), "bwrap"), (true, Some(no_rustc.as_os_str()), "rustc")] { let err = crate::session_factory::preflight_code_mode_impl(|| bwrap_ok, path, false).unwrap_err(); assert!(format!("{err:#}").contains(want)); }
    // source boundary: exactly SOURCE_MAX_BYTES passes validation; +1 rejected with its byte count
    assert!(validate_source(&"x".repeat(SOURCE_MAX_BYTES)).is_ok() && validate_source(&"x".repeat(SOURCE_MAX_BYTES + 1)).unwrap_err().contains("too large") && validate_source(&"x".repeat(SOURCE_MAX_BYTES + 1)).unwrap_err().contains(&(SOURCE_MAX_BYTES + 1).to_string()));
    if env_ok() { crate::session_factory::preflight_code_mode_impl(|| true, path.as_deref(), false).unwrap(); let spec = crate::tools::run_rust_tool(&ws, None).spec(); assert_eq!(spec.parameters["required"], json!(["source"])); assert!(spec.description.contains("bubblewrap")); }
}
#[rustfmt::skip]
#[tokio::test]
async fn statuses_stdin_compile_and_hash_reporting() {
    let Some((mut h, _t)) = hdir(run_rust_policy(None)) else { return; };
    let result = h.run("fn main(){println!(\"Hello from run_rust\")}").await.unwrap();
    assert!(result.contains("source: sha256:") && result.contains("compile command: rustc --edition 2021 /tmp/main.rs -o /tmp/main") && result.contains("run command: /tmp/main") && result.contains("Hello from run_rust") && !result.contains("fn main"));
    // exit 42/134/200 and SIGABRT are authoritative: 134 is never signal 6
    for (src, want) in [("fn main(){std::process::exit(42)}", "run: exit code 42"), ("fn main(){std::process::exit(134)}", "run: exit code 134"), ("fn main(){std::process::exit(200)}", "run: exit code 200"), ("fn main(){std::process::abort()}", "run: signal 6")] {
        h.run_err(src, want).await;
    }
    let error = h.run("fn main(){let x:u32=\"nope\";}").await.unwrap_err();
    assert!(error.contains("error[E0308]") && error.contains("run: not run (compile failed)"));
    h.tool.run_timeout = Duration::from_secs(5);
    h.run_ok(r#"use std::io::Read;fn main(){let mut s=String::new();let n=std::io::stdin().read_to_string(&mut s).unwrap();println!("READ:{n}")}"#, "READ:0").await;
    h.run_ok(r#"fn main(){print!("{}","a".repeat(100*1024))}"#, "[truncated: 69632 bytes omitted]").await;
    // run stage has no Cargo/toolchain: cargo/rustc spawn fails, RUSTUP_HOME absent
    let run_env = r#"use std::process::Command;fn main(){let c=Command::new("cargo").arg("--version").output();let r=Command::new("rustc").arg("--version").output();println!("CARGO:{} RUSTC:{} RHOME:{}",c.is_err(),r.is_err(),std::env::var("RUSTUP_HOME").is_err())}"#;
    assert!(h.run(run_env).await.unwrap().contains("CARGO:true RUSTC:true RHOME:true"));
    assert!(h.scratch_empty(), "scratch removed after a normal run");
    // pre-creation proof: a regular-file scratch base would fail create_scratch, yet execute reports only the size error (131073 = SOURCE_MAX_BYTES + 1 bytes)
    let scratch_base = h.base.path().join("blocker"); std::fs::write(&scratch_base, b"x").unwrap();
    let blocked = RunRust { workspace: h.tool.workspace.clone(), policy: run_rust_policy(None), compile_timeout: STAGE_TIMEOUT, run_timeout: Duration::from_secs(30), scratch: Some(scratch_base) };
    let error = blocked.execute(json!({"source": "x".repeat(SOURCE_MAX_BYTES + 1)})).await.unwrap_err();
    assert!(error.contains("run_rust source too large: 131073 bytes") && !error.contains("scratch base") && validate_source(&"x".repeat(SOURCE_MAX_BYTES)).is_ok(), "{error}");
}
#[rustfmt::skip]
#[tokio::test]
async fn hostile_fd_forgery_and_wrapper_kill_fail_closed() {
    let Some((h, _t)) = hdir(run_rust_policy(None)) else { return; };
    // The payload writes the exact valid packet to every non-stdio fd (the status
    // socket is CLOEXEC'd and invisible), kills the wrapper, exits.
    let error = h.run(r#"use std::io::Write;use std::process::Command;fn main(){let p=[b'e',b'a',b'g',b'e',b'n',b't',b'-',b'r',b'u',b'n',b'-',b'r',b'u',b's',b't',0,1,0,0,0,0];for e in std::fs::read_dir("/proc/self/fd").unwrap(){if let Ok(e)=e{let f=e.path();let n=f.file_name().unwrap().to_str().unwrap().parse::<i32>().unwrap();if n>2{let _=std::fs::OpenOptions::new().write(true).open(&f).map(|mut x|x.write_all(&p));}}}for e in std::fs::read_dir("/proc").unwrap(){if let Ok(e)=e{let n=e.file_name();if let Some(p)=n.to_str().and_then(|s|s.parse::<u32>().ok()).filter(|&p|p>1&&p!=std::process::id()){let _=Command::new("/bin/kill").args(["-9",&p.to_string()]).status();}}}std::process::exit(0)}"#).await.unwrap_err();
    assert!(!error.contains("run: exit 0") && error.contains("run: failed"), "{error}");
    assert!(h.scratch_empty(), "scratch removed after the failed run");
}
#[rustfmt::skip]
#[tokio::test]
async fn timeouts_kill_children_and_bound_the_drain() {
    let Some((mut h, _t)) = hdir(run_rust_policy(None)) else { return; };
    h.tool.run_timeout = Duration::from_millis(800);
    let error = h.run(r#"fn main(){use std::io::Write;print!("before-hang");std::io::stdout().flush().unwrap();loop{}}"#).await.unwrap_err();
    assert!(error.contains("run: timed out after") && error.contains("before-hang"));
    assert!(h.scratch_empty(), "scratch removed after a timeout");
    h.tool.run_timeout = Duration::from_secs(30);
    // residual sleeper and a pipe-holding descendant are killed, never hang
    let gone = |marker: &str| std::process::Command::new("pgrep").args(["-f", marker]).output().map(|o| o.stdout.is_empty()).unwrap_or(true);
    h.run_ok(r#"use std::process::Command;fn main(){let _c=Command::new("sleep").arg("98765").spawn().unwrap();println!("spawned")}"#, "spawned").await;
    h.run_ok(r#"use std::process::Command;fn main(){let _c=Command::new("sh").arg("-c").arg("sleep 98766 &").spawn().unwrap();println!("pipe-held")}"#, "pipe-held").await;
    assert!(gone("sleep 98765") && gone("sleep 98766"));
    h.tool.compile_timeout = Duration::from_millis(1); // compile shares the machinery
    assert!(h.run("fn main(){}").await.unwrap_err().contains("compile: timed out after"));
}
#[rustfmt::skip]
#[tokio::test]
async fn scratch_authority_and_hostile_trees() {
    let base = tempfile::tempdir().unwrap();
    // 0700 creation, atomic collision, unusable base, relative basename -> '.'
    let path = base.path().join("e-agent-run-rust-collision-test");
    create_scratch_once(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
    std::fs::write(path.join("marker"), b"keep").unwrap();
    assert!(create_scratch_once(&path).is_err() && std::fs::read(path.join("marker")).unwrap() == b"keep");
    let blocker = base.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    assert!(create_scratch(&blocker).is_err() && base_dir(Path::new("tree")) == Path::new("."));
    // chmod-000 dirs, a symlink escape and a hardlink to the sentinel are
    // removed without ever chmodding or following outside the scratch
    let tree = base.path().join("tree");
    std::fs::create_dir_all(tree.join("l/d")).unwrap();
    std::fs::write(tree.join("l/f"), b"x").unwrap();
    let sentinel = base.path().join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::hard_link(&sentinel, tree.join("l/h")).unwrap();
    for dir in [tree.join("l/d"), tree.join("l")] { std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap(); }
    std::os::unix::fs::symlink(&sentinel, tree.join("escape")).unwrap();
    remove_scratch(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    assert_eq!(std::fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777, 0o640);
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(base.path()).unwrap();
    std::fs::create_dir_all("rel-tree").unwrap(); std::fs::write("rel-tree/f", b"x").unwrap();
    remove_scratch(Path::new("rel-tree")).unwrap();
    std::env::set_current_dir(&old).unwrap();
    let Some((h, _t)) = hdir(run_rust_policy(None)) else { return; };
    h.run_err(r#"use std::os::unix::fs::PermissionsExt;fn main(){std::fs::create_dir("/tmp/hostile").unwrap();std::fs::set_permissions("/tmp/hostile",std::fs::Permissions::from_mode(0o000)).unwrap();std::fs::set_permissions("/tmp",std::fs::Permissions::from_mode(0o000)).unwrap();std::process::exit(200)}"#, "run: exit code 200").await;
    assert!(h.scratch_empty(), "hostile scratch must still be removed");
}
#[rustfmt::skip]
#[tokio::test]
async fn root_rename_or_replacement_fails_closed() {
    let base = tempfile::tempdir().unwrap();
    for (sub, file) in [("moved", ""), ("moved2", "f")] {
        let mut scratch = create_scratch(base.path()).unwrap();
        let path = scratch.path.clone();
        let moved = base.path().join(sub);
        if !file.is_empty() { std::fs::write(path.join(file), b"x").unwrap(); }
        std::fs::rename(&path, &moved).unwrap(); std::fs::create_dir(&path).unwrap();
        if !file.is_empty() { std::fs::write(path.join("g"), b"y").unwrap(); }
        let error = scratch.remove().unwrap_err();
        assert!(error.contains("replaced") && moved.exists() && (file.is_empty() || path.join("g").exists()));
    }
}
#[rustfmt::skip]
#[test]
fn concurrent_swaps_never_touch_the_sentinel() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let base = tempfile::tempdir().unwrap();
    let tree = base.path().join("tree");
    let sentinel = base.path().join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::create_dir_all(tree.join("l/d")).unwrap(); std::fs::write(tree.join("l/f"), b"x").unwrap();
    std::fs::set_permissions(tree.join("l/d"), std::fs::Permissions::from_mode(0o000)).unwrap();
    std::os::unix::fs::symlink(&sentinel, tree.join("escape")).unwrap();
    // a hostile thread swaps `l` (dir <-> symlink) while removal retries
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn({ let tree = tree.clone(); let sentinel = sentinel.clone(); let stop = stop.clone(); move || { let _ = tx.send(()); let mut i = 0; while !stop.load(Ordering::Relaxed) { i += 1; let l = tree.join("l"); if i % 2 == 0 { let _ = std::fs::remove_file(&l); let _ = std::fs::create_dir_all(l.join("d")); let _ = std::fs::write(l.join("f"), b"y"); } else { let _ = std::fs::remove_dir_all(&l); let _ = std::os::unix::fs::symlink(&sentinel, &l); } } } });
    rx.recv().unwrap();
    let result = remove_scratch(&tree);
    stop.store(true, Ordering::Relaxed);
    t.join().unwrap();
    let _ = remove_scratch(&tree);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    assert_eq!(std::fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777, 0o640);
    if result.is_ok() { assert!(!tree.exists(), "clean removal must have deleted the tree"); }
}
#[rustfmt::skip]
#[tokio::test]
async fn aborted_execute_kills_the_group_and_cleans_the_scratch() {
    let Some((h, _t)) = hdir(run_rust_policy(None)) else { return; };
    let src = r#"use std::process::Command;fn main(){std::fs::write("/tmp/ready","1").unwrap();let _c=Command::new("sleep").arg("987643").spawn().unwrap();loop{}}"#;
    let base = h.base.path().to_path_buf();
    let handle = tokio::spawn(async move { h.run(src).await });
    wait_scratch(&base, Some("ready")).await;
    handle.abort();
    let _ = handle.await; // JoinError::cancelled
    wait_scratch(&base, None).await; // bounded check: scratch gone
    if let Ok(output) = std::process::Command::new("pgrep").args(["-f", "sleep 987643"]).output() { assert!(output.stdout.is_empty(), "grandchild survived the abort"); }
}
#[rustfmt::skip]
#[tokio::test]
async fn cleanup_failure_preserves_primary_and_cleanup_errors() {
    let Some((h, _t)) = hdir(run_rust_policy(None)) else { return; };
    let src = r#"fn main(){std::fs::write("/tmp/ready","1").unwrap();std::thread::sleep(std::time::Duration::from_millis(300));std::process::exit(200)}"#;
    let base = h.base.path().to_path_buf();
    let lock = |mode: u32| std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap();
    let mut h = h;
    for (code, want) in [("200", "run: exit code 200"), ("0", "run: exit code 0")] {
        let src = src.replace("exit(200)", &format!("exit({code})"));
        let handle = tokio::spawn(async move { let r = h.run(&src).await; (r, h) });
        wait_scratch(&base, Some("ready")).await;
        lock(0o500);
        let (error, next) = handle.await.unwrap(); let error = error.unwrap_err();
        assert!(error.contains(want) && error.contains("[run_rust cleanup error"), "{error}");
        h = next;
        lock(0o700);
    }
}
#[rustfmt::skip]
#[test]
fn teardown_uncertainty_retains_scratch_confirmed_removes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let base = tempfile::tempdir().unwrap();
    let sleeper = || { let u = std::process::Command::new("setsid").arg("sh").arg("-c").arg("sleep 98767").spawn().unwrap().id(); let p = rustix::process::Pid::from_raw(u as i32).unwrap(); for _ in 0..500 { if matches!(rustix::process::test_kill_process_group(p), Ok(())) { break; } std::thread::sleep(Duration::from_millis(2)); } u };
    let pid = |u: u32| rustix::process::Pid::from_raw(u as i32).unwrap();
    let guard = |latch: &Arc<AtomicBool>| ScratchGuard(Some(create_scratch(base.path()).unwrap()), latch.clone());
    // pure bounded confirmation expires without confirming (no process needed)
    assert!(!reap_confirmed(pid(1), 0) && !group_absent(pid(1), 0));
    // confirmed teardown: SIGKILL + reap + group absence ⇒ latch safe, cleanup removes
    let latch = Arc::new(AtomicBool::new(false)); let mut confirmed_guard = guard(&latch); // post-spawn: cleanup unsafe until confirmed
    { let _kill = KillGuard::armed(pid(sleeper()), latch.clone()); }
    assert!(latch.load(Ordering::Acquire) && confirmed_guard.cleanup().is_ok());
    assert!(std::fs::read_dir(base.path()).unwrap().next().is_none());
    // injected kill failure ⇒ latch stays unsafe; scratch retained at its exact path
    let latch = Arc::new(AtomicBool::new(false)); let mut kill_guard = guard(&latch);
    let killed = pid(sleeper()); let retained = kill_guard.0.as_ref().unwrap().path.clone();
    FAULT_KILL.store(true, Ordering::Relaxed);
    { let _kill = KillGuard::armed(killed, latch.clone()); }
    FAULT_KILL.store(false, Ordering::Relaxed); // reset immediately after guarded Drop
    let error = kill_guard.cleanup().unwrap_err();
    assert!(!latch.load(Ordering::Acquire) && error.contains("retained") && error.contains("e-agent-run-rust") && retained.exists(), "kill-failure scratch retained at {}", retained.display());
    kill_group(killed).unwrap(); assert!(reap_confirmed(killed, 100) && group_absent(killed, 100), "bounded recovery confirms the group is gone");
    remove_scratch(&retained).unwrap();
    // injected confirmation expiry ⇒ kill ok but latch stays unsafe; scratch retained
    let latch = Arc::new(AtomicBool::new(false)); let mut expired_guard = guard(&latch);
    let killed = pid(sleeper()); let retained = expired_guard.0.as_ref().unwrap().path.clone();
    FAULT_CONFIRM.store(true, Ordering::Relaxed);
    { let _kill = KillGuard::armed(killed, latch.clone()); }
    FAULT_CONFIRM.store(false, Ordering::Relaxed); // reset immediately after guarded Drop
    let error = expired_guard.cleanup().unwrap_err();
    assert!(!latch.load(Ordering::Acquire) && error.contains("retained") && retained.exists(), "confirmation-expiry scratch retained at {}", retained.display());
    assert!(reap_confirmed(killed, 100) && group_absent(killed, 100), "direct child reaped and group absent after disabling the fault");
    remove_scratch(&retained).unwrap();
    assert!(std::fs::read_dir(base.path()).unwrap().next().is_none(), "base empty after explicit removal");
}
#[rustfmt::skip]
fn probe(label: &str, path: &str) -> String {
    format!(r##"let {label}=r#"{path}"#;let r=std::fs::read_to_string(format!("{{{label}}}/probe.txt")).map(|s|format!("R:OK({{}})",s.trim())).unwrap_or_else(|_|"R:ERR".into());let w=if std::fs::write(format!("{{{label}}}/newfile.txt"),b"x").is_ok(){{"W:OK"}}else{{"W:ERR"}};println!("{label}: {{r}} {{w}}");"##)
}
#[rustfmt::skip]
#[tokio::test]
async fn policy_surface_workspace_network_and_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::create_dir_all(ws.join("out")).unwrap();
    std::fs::write(ws.join("probe.txt"), "hello workspace").unwrap();
    let writable = temp.path().join("ext-w");
    let readable = temp.path().join("ext-ro");
    for (dir, content) in [(&writable, "writable"), (&readable, "readable")] { std::fs::create_dir_all(dir).unwrap(); std::fs::write(dir.join("probe.txt"), content).unwrap(); }
    let workspace = Workspace::new(&ws).unwrap();
    let ws_path = ws.to_string_lossy().into_owned();
    let out = ws.join("out").to_string_lossy().into_owned();
    let w = writable.to_string_lossy().into_owned();
    let ro = readable.to_string_lossy().into_owned();
    let source = |extra: &str| format!(r##"fn main(){{{0}if std::fs::write(format!("{{ws}}/evil.txt"),b"x").is_ok(){{println!("WS_WRITE_OK")}}else{{println!("WS_WRITE_ERR")}}if std::fs::write(format!("{{ws}}/.git/evil"),b"x").is_ok(){{println!("GIT_WRITE_OK")}}else{{println!("GIT_WRITE_ERR")}}{1}}}"##, probe("ws", &ws_path), extra);
    // workspace_writable = true (default): writes land, .git stays ro
    let Some(h) = harness(&workspace, run_rust_policy(None)) else { return; };
    let result = h.run(&source("")).await.unwrap();
    assert!(result.contains("ws: R:OK(hello workspace) W:OK") && result.contains("WS_WRITE_OK") && result.contains("GIT_WRITE_ERR"));
    assert!(ws.join("evil.txt").exists() && !ws.join(".git/evil").exists());
    // workspace_writable = false: ws ro, configured writable child stays rw
    let Some(h) = harness(&workspace, policy(false, &[&out], &[])) else { return; };
    let result = h.run(&source(&probe("out", &out))).await.unwrap();
    assert!(result.contains("ws: R:OK(hello workspace) W:ERR") && result.contains("GIT_WRITE_ERR") && result.contains("out: R:ERR W:OK"));
    assert!(!ws.join(".git/evil").exists() && ws.join("out/newfile.txt").exists());
    // configured paths and mounts keep their policy, aliases included
    let mut sandbox = policy(false, &[&w], &[&ro]);
    sandbox.writable_mounts = vec![(w.clone(), "/home/alias-w".into())]; sandbox.readable_mounts = vec![(ro.clone(), "/home/alias-ro".into())];
    let Some(h) = harness(&workspace, sandbox) else { return; };
    let source = format!(r##"fn main(){{ {0}{1}{2}{3} }}"##, probe("writable", &w), probe("alias_w", "/home/alias-w"), probe("readable", &ro), probe("alias_ro", "/home/alias-ro"));
    let result = h.run(&source).await.unwrap();
    assert!(result.contains("writable: R:OK(writable) W:OK") && result.contains("alias_w: R:OK(writable) W:OK") && result.contains("readable: R:OK(readable) W:ERR") && result.contains("alias_ro: R:OK(readable) W:ERR"));
    assert!(!readable.join("newfile.txt").exists() && writable.join("newfile.txt").exists());
    // network denied; credentials and other host env never reach the sandbox
    let Some((h, _t)) = hdir(run_rust_policy(None)) else { return; };
    h.run_ok(r#"use std::net::TcpStream;fn main(){match TcpStream::connect("1.1.1.1:80"){Ok(_)=>println!("NET_OK"),Err(_)=>println!("NET_ERR")}}"#, "NET_ERR").await;
    let src = r#"fn main(){for var in ["HOME","EXA_API_KEY","OPENAI_API_KEY","ANTHROPIC_API_KEY","DEEPSEEK_API_KEY","MOONSHOT_API_KEY","KIMI_API_KEY","GIT_CONFIG_COUNT"]{println!("{}:{}",var,if std::env::var(var).is_ok(){"SET"}else{"ABSENT"})}}"#;
    let result = h.run(src).await.unwrap();
    assert!(result.contains("HOME:ABSENT") && !result.contains(":SET"));
}
