use super::*;

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::agent::AgentEvent;

use super::background::{BackgroundTasks, OutputSlot, TaskSpool, slot_append};

/// A bash tool bound to a shared background-task registry.
/// `protect_git`: when true, `<workspace>/.git` is bound read-only
/// (subagent / fixer); main agent passes false so orchestration works.
/// `timeout`: foreground command timeout; `None` means no timeout.
/// Callers resolve it from config (`[bash]`), defaulting to 30s.
pub fn bash_tool(
    workspace: Workspace,
    background: BackgroundTasks,
    sandbox: Option<crate::config::Sandbox>,
    protect_git: bool,
    timeout: Option<Duration>,
) -> Box<dyn Tool> {
    Box::new(Bash {
        workspace,
        timeout,
        background,
        sender: None,
        sandbox,
        protect_git,
    })
}

pub(super) struct Bash {
    pub(super) workspace: Workspace,
    /// Foreground command timeout; `None` = no timeout (runs until done).
    pub(super) timeout: Option<Duration>,
    pub(super) background: BackgroundTasks,
    /// Completion delivery belongs to this bash facade's Agent, not the
    /// shared registry. Spawned tasks retain this origin sender.
    pub(super) sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    pub(super) sandbox: Option<crate::config::Sandbox>,
    /// When true and sandbox is enabled, bind `<workspace>/.git` read-only
    /// so subagents / fixers cannot corrupt the repository metadata.
    pub(super) protect_git: bool,
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        let mut description = "Run a shell command with the workspace as its current directory. Without bubblewrap, the command retains ambient host filesystem access; file-tool capabilities are an independent boundary.".to_owned();
        // Tell the model which shell syntax to use: PowerShell on Windows,
        // bash elsewhere.
        #[cfg(windows)]
        description.push_str(" The current shell is PowerShell (pwsh): use PowerShell syntax (Get-ChildItem, Get-Content, $env:VAR, etc.), not bash syntax.");
        #[cfg(not(windows))]
        description.push_str(" The current shell is bash: use POSIX/bash syntax.");
        if let Some(sandbox) = &self.sandbox {
            let ws_mode = if sandbox.workspace_writable {
                "writable"
            } else {
                "read-only"
            };
            description.push_str(&format!(
                " The command runs inside a bubblewrap sandbox: the workspace is {ws_mode}, \
                 system dirs (/usr, /bin, /lib, /etc) are read-only, and most of $HOME is \
                 hidden behind a fresh tmpfs. Files that are not mounted — e.g. ~/.ssh or \
                 ~/.gitconfig — fail with \"No such file or directory\"; that means they are \
                 OUTSIDE the sandbox, not that they don't exist on the host. Do not try to \
                 create such files; if a needed path errors this way, tell the user it is \
                 outside the sandbox.",
            ));
            if !sandbox.network {
                description.push_str(" The network is disabled (no DNS, no connections).");
            }
            if !sandbox.writable_paths.is_empty() {
                description.push_str(&format!(
                    " Extra writable paths: {}.",
                    sandbox.writable_paths.join(", ")
                ));
            }
            if !sandbox.readable_paths.is_empty() {
                description.push_str(&format!(
                    " Extra read-only paths: {}.",
                    sandbox.readable_paths.join(", ")
                ));
            }
            description.push_str(
                " Bash mounts and read_file/write_file/edit_file capabilities are independent boundaries sharing this resolved path policy.",
            );
            description.push_str(
                " In a linked git worktree the main repository is mounted read-only (absolute path); bash can also reach it via relative traversal, but the file tools cannot — use the absolute path with them.",
            );
            if self.protect_git {
                description.push_str(
                    " The workspace `.git` metadata (directory or linked-worktree pointer) \
                     is bound read-only to prevent accidental corruption by fixer subagents.",
                );
            }
        }
        ToolSpec {
            name: shell_tool_name().into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "shell command"},
                    "background": {"type": "boolean", "description": "run without blocking; completion is delivered as an event and injected into the next model turn"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let command = required_string(&arguments, "command")?;
        if optional_bool(&arguments, "background")? {
            // Background commands run under THIS facade's sandbox (possibly a
            // read-only role's narrowed policy), never the shared registry's
            // wider one.
            return self.background.start_with_sender(
                self.workspace.clone(),
                command.to_owned(),
                self.protect_git,
                self.sender.clone(),
                self.sandbox.clone(),
            );
        }
        run_bash(
            &self.workspace,
            command,
            self.timeout,
            self.protect_git,
            None,
            None,
            None,
            self.sandbox.as_ref(),
        )
        .await
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.sender = Some(sender);
    }

    fn has_event_sender(&self) -> bool {
        self.sender.is_some()
    }
}

#[cfg(unix)]
struct ProcessGroupGuard {
    process_group: Option<rustix::process::Pid>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn armed(process_group: rustix::process::Pid) -> Self {
        Self {
            process_group: Some(process_group),
        }
    }

    fn disarm(&mut self) {
        self.process_group = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group {
            let _ =
                rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        }
    }
}

/// Windows degradation for process-tree kill: Windows has no POSIX process
/// groups, so this guard terminates the top-level shell process only. Child
/// processes spawned by the shell are NOT killed — a full Job Object based
/// tree-kill is a later milestone. When the top-level bash dies, its
/// children typically observe EOF on the shared stdout/stderr pipes, which
/// is usually enough for interactive shells to exit.
#[cfg(windows)]
struct ProcessGroupGuard {
    handle: Option<windows_sys::Win32::Foundation::HANDLE>,
}

// Windows HANDLEs are plain values: CloseHandle/TerminateProcess are
// documented as callable from any thread, so the guard is Send even though
// HANDLE is `*mut c_void` under the hood. Required because the guard lives
// across awaits inside a `Send + 'static` background future.
#[cfg(windows)]
unsafe impl Send for ProcessGroupGuard {}

#[cfg(windows)]
impl ProcessGroupGuard {
    fn armed(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE};
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        Self {
            handle: (!handle.is_null()).then_some(handle),
        }
    }

    fn disarm(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::TerminateProcess;
        if let Some(handle) = self.handle.take() {
            unsafe {
                let _ = TerminateProcess(handle, 1);
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

/// Shell invocation arguments for `command`: PowerShell uses
/// `-NoProfile -Command`, bash uses `-lc`.
fn shell_invoke_args(command: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["-NoProfile".into(), "-Command".into(), command.to_owned()]
    }
    #[cfg(not(windows))]
    {
        vec!["-lc".into(), command.to_owned()]
    }
}

/// Tool name exposed to the model: `pwsh` on Windows (PowerShell is the
/// native shell there and the model sees the right name to pick the right
/// syntax), `bash` everywhere else. The internal `run_bash`/`kind="bash"`
/// plumbing is unchanged — only the wire name differs.
fn shell_tool_name() -> &'static str {
    #[cfg(windows)]
    {
        "pwsh"
    }
    #[cfg(not(windows))]
    {
        "bash"
    }
}

/// Resolve the shell executable. Non-Windows: `/bin/bash` (unchanged).
/// Windows: prefer PowerShell (`pwsh`, falling back to Windows PowerShell
/// `powershell`) since it is native and needs no Git installation; if
/// neither is found, fall back to Git Bash (`bash.exe` on PATH or the
/// common install locations). Returns a clear error when no shell exists.
fn bash_executable() -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = ();
        Ok("/bin/bash".into())
    }
    #[cfg(windows)]
    {
        // 1. pwsh (PowerShell 7+) / powershell (Windows PowerShell 5.1)
        for name in ["pwsh.exe", "powershell.exe"] {
            if let Some(path) = which_on_path(name) {
                return Ok(path);
            }
        }
        // 2. Git Bash fallback (PATH, then common install locations)
        if let Some(path) = which_on_path("bash.exe") {
            return Ok(path);
        }
        for candidate in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
        ] {
            let p = std::path::PathBuf::from(candidate);
            if p.is_file() {
                return Ok(candidate.to_string());
            }
        }
        Err("no shell found: install PowerShell 7 (pwsh) or Git Bash — put pwsh.exe/powershell.exe/bash.exe on PATH".into())
    }
}

/// Search `PATH` for an executable by name; returns its full path.
#[cfg(windows)]
fn which_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bash(
    workspace: &Workspace,
    command: &str,
    timeout: Option<Duration>,
    protect_git: bool,
    process_group_slot: Option<Arc<AtomicI32>>,
    output_slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
    sandbox: Option<&crate::config::Sandbox>,
) -> Result<String, String> {
    // Build the command: bare bash, or wrapped in bwrap when sandboxed.
    // bwrap is a *construction tool*, so we spell out the policy explicitly:
    // system dirs read-only, workspace writable (per config), /tmp scratch,
    // no new privileges, TIOCSTI blocked, die with the parent. Network is
    // shared by default (agents often need to fetch); config can disable it.
    let bash = bash_executable()?;
    let mut process = match sandbox {
        Some(sandbox) => {
            let root = workspace.root();
            let workspace_bind = if sandbox.workspace_writable {
                "--bind"
            } else {
                "--ro-bind"
            };
            let root_str = root.to_string_lossy().into_owned();
            // Order matters: extra paths under /home (e.g. ~/.cargo) must be
            // mounted AFTER the /home tmpfs, or the tmpfs would shadow them.
            let mut args: Vec<String> = vec![
                "--dev".into(),
                "/dev".into(),
                "--proc".into(),
                "/proc".into(),
                "--ro-bind".into(),
                "/usr".into(),
                "/usr".into(),
                "--ro-bind".into(),
                "/bin".into(),
                "/bin".into(),
                "--ro-bind".into(),
                "/lib".into(),
                "/lib".into(),
                "--ro-bind".into(),
                "/lib64".into(),
                "/lib64".into(),
                "--ro-bind-try".into(),
                "/etc".into(),
                "/etc".into(),
            ];
            // If systemd-resolved is running on the host, mount its stub
            // resolver so that symlinked /etc/resolv.conf works inside the
            // sandbox. Only mount the resolver directory, not whole /run.
            if std::path::Path::new("/run/systemd/resolve").exists() {
                args.push("--dir".into());
                args.push("/run/systemd".into());
                args.push("--ro-bind".into());
                args.push("/run/systemd/resolve".into());
                args.push("/run/systemd/resolve".into());
            }
            args.extend([
                "--tmpfs".into(),
                "/tmp".into(),
                "--tmpfs".into(),
                "/home".into(),
            ]);
            // Bind ancestors before descendants so the most specific policy
            // wins. In particular, an external ancestor cannot override the
            // workspace mode, while an explicit external child still can.
            // Put the workspace last among equal paths so workspace_writable
            // remains authoritative for the workspace root itself.
            let mut mounts = Vec::new();
            for path in &sandbox.readable_paths {
                mounts.push((path.as_str(), "--ro-bind-try", false));
            }
            for path in &sandbox.writable_paths {
                mounts.push((path.as_str(), "--bind-try", false));
            }
            mounts.push((root_str.as_str(), workspace_bind, true));
            mounts.sort_by_key(|(path, _, workspace)| {
                (std::path::Path::new(path).components().count(), *workspace)
            });
            for (path, bind, _) in mounts {
                args.push(bind.into());
                args.push(path.into());
                args.push(path.into());
            }
            // Protect the startup policy anchor, not a custom delegated
            // workspace's unrelated config. It must come after every bind.
            if workspace.policy_anchor_is_visible() {
                let policy = workspace.policy_anchor().to_string_lossy().into_owned();
                let policy_dir = workspace
                    .policy_anchor()
                    .parent()
                    .expect("policy anchor has a parent")
                    .to_string_lossy()
                    .into_owned();
                if std::path::Path::new(&policy).exists() {
                    args.push("--ro-bind".into());
                    args.push("/dev/null".into());
                    args.push(policy);
                } else if std::path::Path::new(&policy_dir).is_dir() {
                    // No file mountpoint exists. Freeze the existing parent
                    // so a writable child cannot create this run's policy.
                    args.push("--ro-bind".into());
                    args.push(policy_dir.clone());
                    args.push(policy_dir);
                }
            }

            // Protect the workspace .git metadata (directory or
            // linked-worktree pointer file) by binding it read-only over
            // itself. This prevents the fixer / subagent from deleting or
            // corrupting the pointer, running `git init`, or writing any
            // commit metadata. It comes after all writable binds and the
            // .e-agent/config.toml protection so it cannot be shadowed.
            // Only enabled for subagents (protect_git=true); the main agent
            // needs writable .git for orchestration (git add/commit/etc).
            if protect_git {
                let git_path = format!("{root_str}/.git");
                if std::path::Path::new(&git_path).exists() {
                    args.push("--ro-bind".into());
                    args.push(git_path.clone());
                    args.push(git_path);
                }
            }
            args.extend([
                "--unshare-pid".into(),
                "--unshare-ipc".into(),
                "--unshare-uts".into(),
                "--new-session".into(),
                "--die-with-parent".into(),
                "--chdir".into(),
                root_str,
            ]);
            if !sandbox.network {
                args.push("--unshare-net".into());
            }
            args.push(bash.clone());
            args.extend(shell_invoke_args(command));
            let mut cmd = Command::new("bwrap");
            cmd.args(args);
            // Strip credential env vars so they are not inherited by the
            // sandboxed command. The agent reads these from the parent
            // process env directly, not via bash, so removing them here
            // does not break anything.
            cmd.env_remove("EXA_API_KEY");
            cmd.env_remove("OPENAI_API_KEY");
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("DEEPSEEK_API_KEY");
            cmd.env_remove("MOONSHOT_API_KEY");
            cmd.env_remove("KIMI_API_KEY");
            cmd
        }
        None => {
            let mut cmd = Command::new(bash);
            cmd.args(shell_invoke_args(command));
            cmd
        }
    };
    process
        .current_dir(workspace.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Force UTF-8 output from the shell and its children. On Linux this is
    // already the norm; on Windows (Git Bash) the default console codepage
    // is GBK/cp936, so without these the byte stream would be mis-decoded
    // by from_utf8_lossy downstream (mojibake on every command).
    process.env("LC_ALL", "C.UTF-8").env("LANG", "C.UTF-8");
    #[cfg(unix)]
    process.process_group(0);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) if sandbox.is_some() && error.kind() == std::io::ErrorKind::NotFound => {
            // Fail closed: if bwrap is configured but missing, return the
            // error so the model (or startup check) sees it.
            return Err(format!(
                "bwrap not found: sandbox requires bubblewrap (bwrap) to be installed: {error}"
            ));
        }
        Err(error) => return Err(format!("failed to start shell: {error}")),
    };
    #[cfg(unix)]
    let process_group = rustix::process::Pid::from_raw(
        child
            .id()
            .ok_or("bash exited before its process group was recorded")? as i32,
    )
    .ok_or("bash returned an invalid process id")?;
    #[cfg(windows)]
    let child_pid = child
        .id()
        .ok_or("bash exited before its pid was recorded")?;
    #[cfg(unix)]
    if let Some(slot) = &process_group_slot {
        slot.store(process_group.as_raw_nonzero().get(), Ordering::Release);
    }
    #[cfg(windows)]
    if let Some(slot) = &process_group_slot {
        // The registry's kill-on-drop reads this as a u32 pid on Windows.
        slot.store(child_pid as i32, Ordering::Release);
    }
    // Kills the process group (Unix) or the top-level process (Windows,
    // degraded) if this future is dropped mid-execution (e.g. the user
    // cancelled the turn).
    #[cfg(unix)]
    let mut cancel_guard = ProcessGroupGuard::armed(process_group);
    #[cfg(windows)]
    let mut cancel_guard = ProcessGroupGuard::armed(child_pid);
    let stdout = child.stdout.take().ok_or("failed to capture stdout")?;
    let stderr = child.stderr.take().ok_or("failed to capture stderr")?;
    let run = async {
        let (stdout, stderr, status) = tokio::join!(
            capture(stdout, output_slot.clone(), spool.clone()),
            capture(stderr, output_slot, spool),
            child.wait()
        );
        Ok::<_, std::io::Error>((stdout?, stderr?, status?))
    };
    let result = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => {
                #[cfg(unix)]
                let kill_error = match rustix::process::kill_process_group(
                    process_group,
                    rustix::process::Signal::KILL,
                ) {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => None,
                    Err(error) => Some(error),
                };
                #[cfg(unix)]
                let _ = child.wait().await;
                #[cfg(not(unix))]
                let _ = child.kill().await;
                #[cfg(not(unix))]
                let _ = child.wait().await;
                if let Some(slot) = &process_group_slot {
                    slot.store(0, Ordering::Release);
                }
                #[cfg(unix)]
                if let Some(error) = kill_error {
                    return Err(format!("failed to kill bash process group: {error}"));
                }
                return Err(format!(
                    "exit code: signal\nstdout:\n\nstderr:\n\n[command timed out after {} seconds]",
                    timeout.as_secs_f64()
                ));
            }
        },
        None => run.await,
    };
    let (stdout, stderr, status) = result.map_err(|error| format!("shell I/O failed: {error}"))?;
    #[cfg(any(unix, windows))]
    cancel_guard.disarm();
    if let Some(slot) = &process_group_slot {
        slot.store(0, Ordering::Release);
    }
    let text = format_output(status.code(), &stdout, &stderr);
    if status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

pub(super) struct Captured {
    pub(super) bytes: Vec<u8>,
    pub(super) truncated: bool,
}

pub(super) async fn capture(
    mut reader: impl AsyncRead + Unpin,
    slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(Captured { bytes, truncated });
        }
        if let Some(slot) = &slot {
            slot_append(slot, &buffer[..count]);
        }
        if let Some(spool) = &spool {
            spool.append(&buffer[..count]);
        }
        let room = OUTPUT_LIMIT.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(room)]);
        truncated |= count > room;
    }
}

pub(super) fn format_output(code: Option<i32>, stdout: &Captured, stderr: &Captured) -> String {
    let mut output = format!(
        "exit code: {}\nstdout:\n{}\nstderr:\n{}",
        code.map_or_else(|| "signal".into(), |code| code.to_string()),
        String::from_utf8_lossy(&stdout.bytes),
        String::from_utf8_lossy(&stderr.bytes)
    );
    if stdout.truncated {
        output.push_str("\nstdout: [truncated]");
    }
    if stderr.truncated {
        output.push_str("\nstderr: [truncated]");
    }
    output
}
