//! Experimental `run_rust` (main agent only, `[code_mode] enabled`): compile
//! and run one std-only Rust source (≤ 128 KiB) in a forced Linux bubblewrap
//! sandbox inheriting the resolved `[sandbox]` policy.
use super::bash::{RUST_CAPTURE, bwrap_args, capture_with, render_stream};
use super::*;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::os::unix::{io::AsRawFd as _, process::ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Command;
pub(super) const SOURCE_MAX_BYTES: usize = 128 * 1024;
pub(super) const STAGE_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_BOUND: Duration = Duration::from_millis(500);
const GROUP_BOUND: u32 = 100;
#[rustfmt::skip]
pub(super) struct RunRust { pub(super) workspace: Workspace, pub(super) policy: crate::config::Sandbox, pub(super) compile_timeout: Duration, pub(super) run_timeout: Duration, #[cfg(test)] pub(super) scratch: Option<PathBuf> }
pub(super) type RustToolchain = (Vec<PathBuf>, String, Option<PathBuf>);
#[rustfmt::skip]
pub fn run_rust_tool(workspace: &Workspace, sandbox: Option<&crate::config::Sandbox>) -> Box<dyn Tool> {
    Box::new(RunRust { workspace: workspace.clone(), policy: run_rust_policy(sandbox), compile_timeout: STAGE_TIMEOUT, run_timeout: STAGE_TIMEOUT, #[cfg(test)] scratch: None })
}
#[rustfmt::skip]
pub(crate) fn run_rust_policy(sandbox: Option<&crate::config::Sandbox>) -> crate::config::Sandbox {
    let mut policy = sandbox.cloned().unwrap_or_default();
    policy.enabled = true; policy.network = false; policy
}
#[rustfmt::skip]
pub(crate) fn resolve_rustc(path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_env?).map(|dir| dir.join("rustc")).find(|candidate| super::bash::is_executable_file(candidate))
}
#[rustfmt::skip]
pub(super) fn validate_source(source: &str) -> Result<(), String> { if source.len() > SOURCE_MAX_BYTES { Err(format!("run_rust source too large: {} bytes", source.len())) } else { Ok(()) } }
#[async_trait]
impl Tool for RunRust {
    #[rustfmt::skip]
    fn spec(&self) -> ToolSpec {
        let description = "Compile and run a single Rust source (edition 2021, std only, max 128 KiB) in a forced Linux bubblewrap sandbox, main agent only. The source is staged to a private 0700 scratch (<tmp>/e-agent-run-rust-*, unpredictable name) mounted as /tmp and removed afterwards only after confirmed teardown (bwrap reaped + process group gone); on any teardown uncertainty the scratch is retained/quarantined and reported, never force-deleted. Removal is descriptor-relative with O_NOFOLLOW everywhere (chmod only ever targets pinned directory fds; the final unlink verifies the created inode identity), so hostile symlink/hardlink swaps or chmod-000 trees can make cleanup fail or retry but never follow or mutate anything outside the scratch. The sandbox inherits the resolved [sandbox] policy: workspace per workspace_writable, configured paths/mounts keep their policy, .git always read-only, network always disabled, credentials never set. The compile stage mounts the host toolchain and runs with its PATH/RUSTUP_HOME; the run stage gets PATH /bin:/usr/bin and no toolchain mounts, so Cargo/toolchain is not supplied on the run-stage PATH/mounts (the prebuilt /tmp/runner and /tmp/main still run on system runtime libraries). No Cargo/crates, no stdin, no background or daemon execution, no custom env/mount/cwd/timeout; compile and run each have a 30-second timeout, killed as a process group on timeout or cancellation. The run status comes from an in-sandbox wrapper over a private inherited socketpair: it drops its dumpable flag and CLOEXECs the fd before exec'ing the payload (no descendant can forge), waits, kills survivors, then sends exactly one magic/tag/code packet gated on the outer bwrap exit 0, so an explicit exit(134) is never misreported as a signal and killing the wrapper can only deny service. Each stage's output is capped at 24 KiB head + 8 KiB tail. The result reports the source SHA-256, the stable sandbox-visible commands, both stage statuses and captured output; the source text is not repeated. Linux-only and unavailable to subagents.";
        super::spec("run_rust", description, json!({"type": "object", "properties": {"source": {"type": "string", "description": "complete Rust source code (edition 2021, std only), at most 128 KiB"}}, "required": ["source"]}), &["source"])
    }
    #[rustfmt::skip]
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let source = required_string(&arguments, "source")?;
        validate_source(source)?;
        self.execute_sandboxed(source).await
    }
}
const COMPILE_CMD: &str = "rustc --edition 2021 /tmp/main.rs -o /tmp/main";
const RUN_CMD: &str = "/tmp/main";
#[rustfmt::skip]
const COMPILE_PROGRAM: &[&str] = &["/bin/sh", "-c", "rustc --edition 2021 /tmp/runner.rs -o /tmp/runner && rustc --edition 2021 /tmp/main.rs -o /tmp/main"];
const RUN_PROGRAM: &[&str] = &["/tmp/runner", "/tmp/main"];
// Status packet: 16-byte magic + tag + i32 LE code, exactly one packet then EOF.
const STATUS_MAGIC: [u8; 16] = *b"eagent-run-rust\0";
const STATUS_PACKET_LEN: usize = 21;
// The runner drops dumpable + CLOEXECs the status fd before exec, waits, kills survivors, sends one packet + EOF.
#[rustfmt::skip]
const RUNNER_SOURCE: &str = "use std::os::unix::io::FromRawFd as _; use std::os::unix::process::ExitStatusExt as _; use std::process::Command; extern \"C\" { fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32; fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32; } fn main() { let mut args = std::env::args_os().skip(1); let fd: i32 = match args.next().and_then(|a| a.into_string().ok()).and_then(|s| s.parse().ok()) { Some(fd) => fd, None => std::process::exit(1) }; let Some(payload) = args.next() else { std::process::exit(1) }; let payload_args: Vec<std::ffi::OsString> = args.collect(); unsafe { if prctl(4, 0, 0, 0, 0) != 0 || fcntl(fd, 2, 1) != 0 { std::process::exit(2); } } let (tag, code) = match Command::new(&payload).args(&payload_args).status() { Ok(s) => match (s.code(), s.signal()) { (Some(code), _) => (1, code), (None, Some(sig)) => (2, sig), _ => (1, 127) }, Err(_) => (1, 127) }; if let Ok(entries) = std::fs::read_dir(\"/proc\") { for entry in entries.flatten() { let name = entry.file_name(); if let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()).filter(|&p| p > 1 && p != std::process::id()) { let _ = Command::new(\"/bin/kill\").args([\"-9\", &pid.to_string()]).status(); } } } let mut packet = Vec::with_capacity(21); packet.extend_from_slice(b\"eagent-run-rust\\0\"); packet.push(tag); packet.extend_from_slice(&code.to_le_bytes()); let mut file = unsafe { std::fs::File::from_raw_fd(fd) }; std::process::exit(if std::io::Write::write_all(&mut file, &packet).is_ok() { 0 } else { 1 }); }";
impl RunRust {
    #[rustfmt::skip]
    async fn execute_sandboxed(&self, source: &str) -> Result<ToolOutput, String> {
        let toolchain = resolve_toolchain()?;
        let hash = sha256_hex(source.as_bytes());
        #[cfg(test)] let scratch_base = self.scratch.clone().unwrap_or_else(std::env::temp_dir);
        #[cfg(not(test))] let scratch_base = std::env::temp_dir();
        // Cleanup is unsafe from the first spawn until confirmed reap + group absence.
        let latch = Arc::new(AtomicBool::new(true));
        let mut guard = ScratchGuard(Some(create_scratch(&scratch_base)?), latch.clone());
        let primary = self.run_stages(&toolchain, source, &hash, guard.0.as_ref().unwrap(), &latch).await;
        match (primary, guard.cleanup()) { (Ok(text), Ok(())) => Ok(ToolOutput::text(text)), (Ok(text), Err(cleanup)) => Err(format!("{text}\n[run_rust cleanup error: {cleanup}]")), (Err(primary), Err(cleanup)) => Err(format!("{primary}\n[run_rust cleanup error: {cleanup}]")), (Err(primary), Ok(())) => Err(primary) }
    }
    #[rustfmt::skip]
    async fn run_stages(&self, toolchain: &RustToolchain, source: &str, hash: &str, scratch: &Scratch, latch: &Arc<AtomicBool>) -> Result<String, String> {
        std::fs::write(scratch.path.join("main.rs"), source).map_err(|e| format!("cannot stage main.rs: {e}"))?;
        std::fs::write(scratch.path.join("runner.rs"), RUNNER_SOURCE).map_err(|e| format!("cannot stage runner.rs: {e}"))?;
        // Base args carry no toolchain mounts; only the compile stage extends them.
        let base = bwrap_args(&self.workspace, &self.policy, true, false, "/tmp", Some(scratch.path.to_string_lossy().as_ref()));
        let compile_bwrap = { let mut args = base.clone(); toolchain_mount_args(toolchain, &mut args); args };
        let compile_program: Vec<String> = COMPILE_PROGRAM.iter().map(|s| s.to_string()).collect();
        let compile = run_stage(&compile_bwrap, &compile_program, &StageEnv::compile(toolchain), self.compile_timeout, None, latch).await;
        let run = if matches!(compile.0, StageStatus::Exit(0)) {
            match status_program() {
                Ok((pipe, program)) => Some(run_stage(&base, &program, &StageEnv::run(), self.run_timeout, Some(pipe), latch).await),
                Err(error) => Some(Stage::failed(error)),
            }
        } else { None };
        let succeeded = matches!(run.as_ref(), Some(s) if matches!(s.0, StageStatus::Exit(0)));
        let text = format_result(hash, &compile, run.as_ref());
        if succeeded { Ok(text) } else { Err(text) }
    }
}
#[rustfmt::skip]
fn status_program() -> Result<(StatusPipe, Vec<String>), String> {
    let (reader, writer) = std::os::unix::net::UnixStream::pair().map_err(|e| format!("cannot create status pipe: {e}"))?;
    for fd in [&reader, &writer] { rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).map_err(|e| format!("cannot seal status pipe: {e}"))?; }
    let fd = writer.as_raw_fd();
    reader.set_nonblocking(true).map_err(|e| format!("cannot make status reader nonblocking: {e}"))?;
    let reader = tokio::net::UnixStream::from_std(reader).map_err(|e| format!("cannot adopt status reader: {e}"))?;
    let mut program: Vec<String> = RUN_PROGRAM.iter().map(|s| s.to_string()).collect(); program.insert(1, fd.to_string());
    Ok((StatusPipe { reader, writer: Some(writer) }, program))
}
#[rustfmt::skip]
struct StatusPipe { reader: tokio::net::UnixStream, writer: Option<std::os::unix::net::UnixStream> }
// Read until EOF, require exactly one valid packet; anything else fails closed.
#[rustfmt::skip]
async fn read_status_packet(reader: &mut tokio::net::UnixStream) -> Result<StageStatus, String> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await.map_err(|e| format!("status channel read failed: {e}"))?;
    if buf.len() != STATUS_PACKET_LEN { return Err(format!("status channel carried {} bytes", buf.len())); }
    if buf[..16] != STATUS_MAGIC { return Err("invalid status packet magic".into()); }
    let code = i32::from_le_bytes(buf[17..].try_into().unwrap());
    match buf[16] { 1 => Ok(StageStatus::Exit(code)), 2 => Ok(StageStatus::Signal(code)), tag => Err(format!("invalid status packet tag {tag}")) }
}
#[rustfmt::skip]
#[cfg(test)] pub(super) fn create_scratch_once(path: &Path) -> std::io::Result<()> { use std::os::unix::fs::DirBuilderExt as _; std::fs::DirBuilder::new().mode(0o700).create(path) }
const SCRATCH_RETRIES: usize = 8;
// Creation authority: pinned parent/root fds + root identity; the final unlink re-verifies identity.
#[rustfmt::skip]
pub(super) struct Scratch { pub(super) path: PathBuf, parent: rustix::fd::OwnedFd, root: rustix::fd::OwnedFd, name: String, dev: u64, ino: u64 }
#[rustfmt::skip]
pub(super) fn create_scratch(base: &Path) -> Result<Scratch, String> {
    let base = if base.as_os_str().is_empty() { Path::new(".") } else { base };
    let parent = open_dir(rustix::fs::CWD, base).map_err(|e| format!("cannot open scratch base {}: {e}", base.display()))?;
    for _ in 0..SCRATCH_RETRIES {
        let name = format!("e-agent-run-rust-{}-{}", std::process::id(), scratch_suffix()?);
        match rustix::fs::mkdirat(&parent, &name, rustix::fs::Mode::from_bits(0o700).unwrap()) { Ok(()) => { let root = open_dir(&parent, &name).map_err(|e| format!("cannot open run_rust scratch: {e}"))?; let stat = rustix::fs::fstat(&root).map_err(|e| format!("cannot stat run_rust scratch: {e}"))?; return Ok(Scratch { path: base.join(&name), parent, root, name, dev: stat.st_dev, ino: stat.st_ino }); } Err(rustix::io::Errno::EXIST) => continue, Err(error) => return Err(format!("cannot create run_rust scratch: {error}")) }
    }
    Err(format!("cannot allocate run_rust scratch: {SCRATCH_RETRIES} collisions"))
}
#[rustfmt::skip]
fn scratch_suffix() -> Result<String, String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 8];
    let mut file = std::fs::File::open("/dev/urandom").map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    file.read_exact(&mut bytes).map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
// An empty relative parent (a bare basename like "tree") maps to '.'.
#[rustfmt::skip]
#[cfg(test)] pub(super) fn base_dir(root: &Path) -> &Path { root.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")) }
impl Scratch {
    // Descent is descriptor-relative DIRECTORY|NOFOLLOW; files are unlinked by basename, never chmodded.
    #[rustfmt::skip]
    pub(super) fn remove(&mut self) -> Result<(), String> {
        use rustix::fs::{AtFlags, unlinkat};
        remove_dir_fd(&self.root).map_err(|e| format!("cannot remove run_rust scratch: {e}"))?;
        match rustix::fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if stat.st_dev == self.dev && stat.st_ino == self.ino => {}
            Ok(_) => return Err(format!("scratch {} replaced before removal", self.name)),
            Err(rustix::io::Errno::NOENT) => return Err(format!("scratch {} vanished before removal", self.name)),
            Err(error) => return Err(format!("cannot verify run_rust scratch: {error}")),
        }
        unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR).map_err(|e| format!("cannot remove run_rust scratch: {e}"))
    }
}
#[rustfmt::skip]
fn remove_dir_fd(fd: &rustix::fd::OwnedFd) -> rustix::io::Result<()> {
    use rustix::fs::{AtFlags, Mode, RawDir, unlinkat};
    rustix::fs::fchmod(fd, Mode::RWXU)?;
    let mut buffer = Vec::with_capacity(8192);
    let mut names = Vec::new();
    { let mut raw = RawDir::new(rustix::io::dup(fd)?, buffer.spare_capacity_mut()); while let Some(entry) = raw.next() { let entry = entry?; let name = entry.file_name().to_bytes(); if name != b"." && name != b".." { names.push(name.to_vec()); } } }
    for name in names {
        let opened = match open_dir(fd, &name) { Err(rustix::io::Errno::ACCESS) => { chmod_pinned(fd, &name)?; open_dir(fd, &name) } result => result };
        match opened { Ok(child) => { remove_dir_fd(&child)?; unlinkat(fd, &name, AtFlags::REMOVEDIR)?; } Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => unlinkat(fd, &name, AtFlags::empty())?, Err(rustix::io::Errno::NOENT) => {} Err(error) => return Err(error) }
    }
    Ok(())
}
#[rustfmt::skip]
fn open_dir<F: rustix::fd::AsFd, P: rustix::path::Arg>(parent: F, name: P) -> rustix::io::Result<rustix::fd::OwnedFd> {
    rustix::fs::openat(parent, name, rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty())
}
// Permission restoration pins directories only (O_PATH|O_DIRECTORY|NOFOLLOW) via /proc/self/fd.
#[rustfmt::skip]
fn chmod_pinned<P: rustix::path::Arg>(parent: &rustix::fd::OwnedFd, name: P) -> rustix::io::Result<()> {
    let pinned = rustix::fs::openat(parent, name, rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty())?;
    let magic = format!("/proc/self/fd/{}", pinned.as_raw_fd());
    rustix::fs::chmodat(rustix::fs::CWD, magic, rustix::fs::Mode::RWXU, rustix::fs::AtFlags::empty())
}
// Path-based entry (tests): resolve parent/root fds + root identity, bounded retries; a missing root reads as success.
#[rustfmt::skip]
#[cfg(test)] pub(super) fn remove_scratch(root: &Path) -> Result<(), String> {
    let parent = open_dir(rustix::fs::CWD, base_dir(root)).map_err(|e| format!("cannot open scratch base {}: {e}", base_dir(root).display()))?;
    let name = root.file_name().ok_or_else(|| format!("scratch {} has no file name", root.display()))?;
    let root_fd = match open_dir(&parent, name) { Ok(fd) => fd, Err(rustix::io::Errno::NOENT) => return Ok(()), Err(rustix::io::Errno::ACCESS) => { chmod_pinned(&parent, name).map_err(|e| format!("cannot restore scratch permissions: {e}"))?; open_dir(&parent, name).map_err(|e| format!("cannot open run_rust scratch: {e}"))? } Err(error) => return Err(format!("cannot open run_rust scratch: {error}")) };
    let stat = rustix::fs::fstat(&root_fd).map_err(|e| format!("cannot stat run_rust scratch: {e}"))?;
    let mut scratch = Scratch { path: root.to_path_buf(), parent, root: root_fd, name: name.to_string_lossy().into_owned(), dev: stat.st_dev, ino: stat.st_ino };
    let mut last = String::new();
    for attempt in 0..5 { match scratch.remove() { Ok(()) => return Ok(()), Err(error) if attempt < 4 => { last = error; std::thread::sleep(Duration::from_millis(10)); }, Err(error) => last = error } }
    Err(last)
}
// Cleanup is safe only after confirmed reap + group absence; else the scratch is retained.
#[rustfmt::skip]
pub(super) struct ScratchGuard(pub(super) Option<Scratch>, pub(super) Arc<AtomicBool>);
impl ScratchGuard {
    #[rustfmt::skip]
    pub(super) fn cleanup(&mut self) -> Result<(), String> {
        let Some(mut scratch) = self.0.take() else { return Ok(()) };
        if self.1.load(Ordering::Acquire) { scratch.remove() } else { Err(format!("teardown unconfirmed; scratch retained at {}", scratch.path.display())) }
    }
}
#[rustfmt::skip]
impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let Some(mut scratch) = self.0.take() else { return };
        if self.1.load(Ordering::Acquire) { if let Err(error) = scratch.remove() { eprintln!("run_rust: scratch cleanup failed: {error}"); } }
        else { eprintln!("run_rust: teardown unconfirmed; retaining scratch at {}", scratch.path.display()); }
    }
}
// Per-stage env: compile carries the host toolchain; run gets plain /bin:/usr/bin.
struct StageEnv(String, Option<PathBuf>);
impl StageEnv {
    #[rustfmt::skip]
    fn compile(toolchain: &RustToolchain) -> Self { Self(toolchain.1.clone(), toolchain.2.clone()) }
    #[rustfmt::skip]
    fn run() -> Self { Self("/bin:/usr/bin".into(), None) }
}
#[rustfmt::skip]
fn toolchain_mount_args(toolchain: &RustToolchain, args: &mut Vec<String>) {
    let mut mounts: Vec<String> = toolchain.0.iter().map(|m| m.to_string_lossy().into_owned()).collect();
    mounts.sort_by_key(|mount| Path::new(mount).components().count());
    for mount in mounts { args.extend(["--ro-bind".into(), mount.clone(), mount]); }
}
pub(super) struct Stage(pub(super) StageStatus, pub(super) String, pub(super) String);
#[rustfmt::skip]
impl Stage { fn failed(reason: String) -> Stage { Stage(StageStatus::Failed(reason), String::new(), String::new()) } }
#[rustfmt::skip]
pub(super) enum StageStatus { Exit(i32), Signal(i32), TimedOut(Duration), Failed(String) }
#[rustfmt::skip]
impl StageStatus { fn line(&self) -> String { match self { StageStatus::Exit(code) => format!("exit code {code}"), StageStatus::Signal(n) => format!("signal {n}"), StageStatus::TimedOut(after) => format!("timed out after {:.0}s", after.as_secs_f64()), StageStatus::Failed(reason) => format!("failed: {reason}") } } }
// One checked kill → reap → bounded drain; KillGuard confirms on abort.
#[rustfmt::skip]
async fn run_stage(bwrap: &[String], program: &[String], env: &StageEnv, timeout: Duration, status: Option<StatusPipe>, latch: &Arc<AtomicBool>) -> Stage {
    let mut cmd = Command::new("bwrap");
    cmd.args(bwrap).args(program);
    cmd.env_clear().env("PATH", &env.0).env("LC_ALL", "C.UTF-8").env("LANG", "C.UTF-8");
    if let Some(home) = &env.1 { cmd.env("RUSTUP_HOME", home); }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut status_pipe = status;
    if let Some(pipe) = &status_pipe {
        let fd = pipe.writer.as_ref().unwrap().as_raw_fd();
        // SAFETY: in the forked child, before exec, clears CLOEXEC on the inherited writer only.
        unsafe { cmd.pre_exec(move || { let borrowed = rustix::fd::BorrowedFd::borrow_raw(fd); rustix::io::fcntl_setfd(borrowed, rustix::io::FdFlags::empty()).map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error())) }); }
    }
    let mut child = match cmd.spawn() { Ok(child) => child, Err(error) => return Stage::failed(format!("cannot start run_rust stage: {error}")) };
    // Spawned: cleanup is unsafe until confirmed reap + group absence.
    latch.store(false, Ordering::Release);
    if let Some(pipe) = &mut status_pipe { pipe.writer = None; }
    let Some(process_group) = child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32)) else { let _ = child.start_kill(); return Stage::failed("run_rust stage exited too early".into()); };
    let mut cancel = KillGuard::armed(process_group, latch.clone());
    let Some(stdout) = child.stdout.take() else { return Stage::failed("cannot capture run_rust stdout".into()); };
    let Some(stderr) = child.stderr.take() else { return Stage::failed("cannot capture run_rust stderr".into()); };
    let stdout_task = tokio::spawn(capture_with(stdout, None, None, RUST_CAPTURE));
    let stderr_task = tokio::spawn(capture_with(stderr, None, None, RUST_CAPTURE));
    let wait = tokio::time::timeout(timeout, child.wait()).await;
    let timed_out = wait.is_err();
    if timed_out && let Err(error) = kill_group(process_group) { return Stage::failed(error); }
    let wait_status = if timed_out { match tokio::time::timeout(DRAIN_BOUND, child.wait()).await { Ok(Ok(status)) => status, Ok(Err(_)) => { return Stage::failed("run_rust stage I/O failed".into()); }, Err(_) => { return Stage::failed("run_rust stage did not die after kill".into()); } } } else { match wait { Ok(Ok(status)) => status, Ok(Err(_)) => { return Stage::failed("run_rust stage I/O failed".into()); }, Err(_) => unreachable!() } };
    if let Err(error) = kill_group(process_group) { return Stage::failed(error); }
    let (stdout, stderr) = match tokio::time::timeout(DRAIN_BOUND, async { tokio::join!(stdout_task, stderr_task) }).await { Ok((Ok(Ok(o)), Ok(Ok(e)))) => (render_stream(&o), render_stream(&e)), _ => { return Stage::failed("run_rust stage I/O failed".into()); } };
    // Confirmed teardown: child reaped; cleanup is safe only after group absence.
    if !cancel.disarm() { return Stage::failed("run_rust stage process group still present".into()); }
    if timed_out { return Stage(StageStatus::TimedOut(timeout), stdout, stderr); }
    let stage_status = match (status_pipe.as_mut().map(|p| &mut p.reader), wait_status.code()) {
        (Some(reader), Some(0)) => read_status_packet(reader).await.unwrap_or_else(StageStatus::Failed),
        (Some(_), _) => StageStatus::Failed(format!("wrapper {}; status not confirmed", outer_status(&wait_status))),
        (None, Some(code)) => StageStatus::Exit(code),
        (None, None) => StageStatus::Signal(wait_status.signal().unwrap_or(0)),
    };
    Stage(stage_status, stdout, stderr)
}
// SIGKILL accepts ESRCH only; other failures are preserved (test fault injection).
#[rustfmt::skip]
pub(super) fn kill_group(process_group: rustix::process::Pid) -> Result<(), String> {
    #[cfg(test)] if FAULT_KILL.load(Ordering::Relaxed) { return Err("injected kill failure".into()); }
    match rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(format!("cannot kill run_rust stage process group: {error}")),
    }
}
#[cfg(test)]
pub(super) static FAULT_KILL: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
pub(super) static FAULT_CONFIRM: AtomicBool = AtomicBool::new(false);
// Bounded confirmations: the direct child is reaped, then the group is gone.
#[rustfmt::skip]
pub(super) fn reap_confirmed(process_group: rustix::process::Pid, attempts: u32) -> bool {
    for _ in 0..attempts { match rustix::process::waitpid(Some(process_group), rustix::process::WaitOptions::NOHANG) { Ok(Some(_)) | Err(rustix::io::Errno::CHILD) => return true, Err(rustix::io::Errno::INTR) => continue, _ => std::thread::sleep(Duration::from_millis(1)) } }
    false
}
#[rustfmt::skip]
pub(super) fn group_absent(process_group: rustix::process::Pid, attempts: u32) -> bool {
    #[cfg(test)] if FAULT_CONFIRM.load(Ordering::Relaxed) { return false; }
    for _ in 0..attempts { if matches!(rustix::process::test_kill_process_group(process_group), Err(rustix::io::Errno::SRCH)) { return true; } std::thread::sleep(Duration::from_millis(1)); }
    false
}
// Abort fallback: SIGKILL, bounded reap, bounded group-absence confirmation.
#[rustfmt::skip]
pub(super) struct KillGuard(Option<rustix::process::Pid>, Arc<AtomicBool>);
impl KillGuard {
    #[rustfmt::skip]
    pub(super) fn armed(process_group: rustix::process::Pid, latch: Arc<AtomicBool>) -> Self { Self(Some(process_group), latch) }
    #[rustfmt::skip]
    fn disarm(&mut self) -> bool {
        let Some(pid) = self.0 else { return true };
        if group_absent(pid, GROUP_BOUND) { self.0 = None; self.1.store(true, Ordering::Release); true } else { false }
    }
}
#[rustfmt::skip]
impl Drop for KillGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        if kill_group(pid).is_ok() && reap_confirmed(pid, GROUP_BOUND) && group_absent(pid, GROUP_BOUND) { self.1.store(true, Ordering::Release); }
    }
}
#[rustfmt::skip]
fn outer_status(status: &std::process::ExitStatus) -> String {
    match (status.code(), status.signal()) { (Some(code), _) => format!("exit code {code}"), (None, Some(sig)) => format!("signal {sig}"), _ => "unknown status".to_owned() }
}
#[rustfmt::skip]
pub(super) fn resolve_toolchain() -> Result<RustToolchain, String> {
    let rustc = resolve_rustc(std::env::var_os("PATH").as_deref()).ok_or_else(|| "rustc not found on PATH".to_owned())?;
    let real = std::fs::canonicalize(&rustc).map_err(|e| format!("cannot resolve rustc: {e}"))?;
    let real_dir = real.parent().ok_or("rustc has no parent directory")?.to_path_buf();
    let rustup = rustc.parent().unwrap_or(&real_dir).join("rustup");
    let rustup_home = if super::bash::is_executable_file(&rustup) || super::bash::is_executable_file(&real_dir.join("rustup")) { std::env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| crate::home_dir().map(|home| home.join(".rustup"))).filter(|dir| dir.join("settings.toml").exists() || dir.join("toolchains").is_dir()) } else { None };
    let mut mounts = vec![real_dir.clone()];
    if let Some(rustup_home) = &rustup_home && let Ok(canonical) = std::fs::canonicalize(rustup_home) && canonical != real_dir { mounts.push(canonical); }
    let mut path_dirs = vec![rustc.parent().unwrap_or(&real_dir).to_path_buf(), real_dir];
    if let Some(rustup_home) = &rustup_home { path_dirs.push(rustup_home.join("bin")); }
    path_dirs.extend([PathBuf::from("/bin"), PathBuf::from("/usr/bin")]);
    let path = path_dirs.into_iter().map(|dir| dir.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
    Ok((mounts, path, rustup_home))
}
#[rustfmt::skip]
fn format_result(hash: &str, compile: &Stage, run: Option<&Stage>) -> String {
    let run_line = run.map_or_else(|| "not run (compile failed)".to_owned(), |s| s.0.line());
    let mut text = format!("source: sha256:{hash}\ncompile command: {COMPILE_CMD}\nrun command: {RUN_CMD}\ncompile: {}\nrun: {run_line}\n", compile.0.line());
    for (label, stage) in [("compile", Some(compile)), ("run", run)] { let (o, e) = stage.map_or((String::new(), String::new()), |s| (s.1.clone(), s.2.clone())); text.push_str(&format!("{label} stdout:\n{o}\n\n{label} stderr:\n{e}\n\n")); }
    text
}
#[rustfmt::skip]
fn sha256_hex(bytes: &[u8]) -> String { Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect() }
