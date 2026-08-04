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
/// `protect_git`: on non-Windows, `<workspace>/.git` is bound read-only for
/// subagents/fixers. It comes from the role's frontmatter (default true; a
/// role declares `protect_git = false` to opt out). The Windows write-sandbox
/// MVP rejects this mode.
/// `timeout`: foreground command timeout; `None` means no timeout.
/// Callers resolve it from config (`[bash]`), defaulting to 30s.
pub fn bash_tool(
    workspace: Workspace,
    background: BackgroundTasks,
    sandbox: Option<crate::config::Sandbox>,
    protect_git: bool,
    timeout: Option<Duration>,
) -> Result<Box<dyn Tool>, String> {
    let shell = Shell::detect()?;
    Ok(Box::new(Bash {
        workspace,
        timeout,
        background,
        sender: None,
        sandbox,
        protect_git,
        shell,
    }))
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
    /// On non-Windows, bind `<workspace>/.git` read-only when sandboxed.
    /// Resolved from the role's frontmatter `protect_git` (default true).
    /// The Windows write-sandbox MVP rejects this mode before side effects.
    pub(super) protect_git: bool,
    /// Resolved platform shell (pwsh on Windows, bash elsewhere).
    pub(super) shell: Shell,
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        #[cfg(windows)]
        let mut description = "Run a shell command with the workspace as its current directory. Without the Windows write sandbox, the command retains ambient host filesystem access; file-tool capabilities are an independent boundary.".to_owned();
        #[cfg(not(windows))]
        let mut description = "Run a shell command with the workspace as its current directory. Without bubblewrap, the command retains ambient host filesystem access; file-tool capabilities are an independent boundary.".to_owned();
        // Tell the model which shell syntax to use (pwsh on Windows, bash
        // elsewhere).
        description.push_str(self.shell.syntax_hint);
        if let Some(sandbox) = &self.sandbox {
            #[cfg(windows)]
            {
                let ws_mode = if sandbox.workspace_writable {
                    "an allowed write root"
                } else {
                    "not an allowed write root"
                };
                description.push_str(&format!(
                    " The command runs with a Windows restricted primary token. The workspace is {ws_mode}. This is write restriction, not read isolation, and it provides no network isolation; paths already writable through Everyone or the current logon SID, including some public locations, may remain writable. TEMP/TMP, HOME, and toolchain or engine caches are not allowed by default: add every required location to writable_paths explicitly."
                ));
                if !sandbox.writable_paths.is_empty() {
                    description.push_str(&format!(
                        " Explicit extra writable paths: {}.",
                        sandbox.writable_paths.join(", ")
                    ));
                }
                if self.protect_git {
                    description.push_str(" Windows write-sandbox MVP cannot enforce .git protection: shell commands fail closed with guidance to set `protect_git = false` in the role frontmatter (or disable the sandbox).");
                }
            }
            #[cfg(not(windows))]
            {
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
        }
        ToolSpec {
            name: self.shell.tool_name.into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "shell command"},
                    "background": {"type": "boolean", "description": "run without blocking; completion is delivered as an event and injected into the next model turn"},
                    "detached": {"type": "boolean", "description": "only with background:true — run a daemon, long-lived service, or watcher that must NOT block this session from finishing and whose completion is never delivered. Use the default non-detached background for ordinary async builds/tests/downloads whose completion you want to react to"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let command = required_string(&arguments, "command")?;
        let background = optional_bool(&arguments, "background")?;
        let detached = optional_bool(&arguments, "detached")?;
        if detached && !background {
            return Err(
                "`detached` requires `background: true`: a detached command runs without \
                 blocking and never delivers a completion"
                    .into(),
            );
        }
        if background {
            if detached {
                // Daemon / long-lived service / watcher: runs in the shared
                // registry (task panel stays visible) but never delivers a
                // completion and never blocks this session from finishing.
                // Runs under THIS facade's sandbox, like any background
                // command.
                return self.background.start_detached(
                    self.workspace.clone(),
                    command.to_owned(),
                    self.protect_git,
                    self.sandbox.clone(),
                );
            }
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
            &self.shell,
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
pub(super) struct ProcessGroupGuard {
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
    pub(super) fn armed(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE};
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        Self {
            handle: (!handle.is_null()).then_some(handle),
        }
    }

    pub(super) fn disarm(&mut self) {
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

/// The shell backend: everything platform-specific about the shell tool is
/// concentrated here — the wire name the model sees, the executable to
/// spawn, the invocation arguments, and the syntax hint in the tool
/// description. Platform differences live in exactly one place.
#[derive(Debug, Clone)]
pub(super) struct Shell {
    /// Tool name exposed to the model: `pwsh` on Windows so the model picks
    /// PowerShell syntax, `bash` everywhere else.
    pub(super) tool_name: &'static str,
    pub(super) executable: String,
    invoke_args: Vec<String>,
    /// Sentence appended to the tool description telling the model which
    /// shell syntax to use.
    syntax_hint: &'static str,
}

impl Shell {
    #[cfg(windows)]
    pub(super) fn detect() -> Result<Self, String> {
        // Prefer PowerShell (native, no Git install needed), fall back to
        // Windows PowerShell, then Git Bash.
        let executable = ["pwsh.exe", "powershell.exe"]
            .into_iter()
            .find_map(which_on_path)
            .or_else(|| which_on_path("bash.exe"))
            .or_else(|| {
                [r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files\Git\usr\bin\bash.exe"]
                    .into_iter()
                    .find(|p| std::path::Path::new(p).is_file())
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                "no shell found: install PowerShell 7 (pwsh) or Git Bash — put pwsh.exe/powershell.exe/bash.exe on PATH".to_owned()
            })?;
        let (tool_name, invoke_args, syntax_hint) = if executable.ends_with("bash.exe") {
            (
                "pwsh", // keep the wire name stable; bash is the fallback shell
                vec!["-lc".to_owned()],
                " The current shell is bash (Git Bash fallback): use POSIX/bash syntax.",
            )
        } else {
            (
                "pwsh",
                vec!["-NoProfile".to_owned(), "-Command".to_owned()],
                " The current shell is PowerShell (pwsh): use PowerShell syntax (Get-ChildItem, Get-Content, $env:VAR, etc.), not bash syntax.",
            )
        };
        Ok(Self {
            tool_name,
            executable,
            invoke_args,
            syntax_hint,
        })
    }

    #[cfg(not(windows))]
    pub(super) fn detect() -> Result<Self, String> {
        Ok(Self {
            tool_name: "bash",
            executable: "/bin/bash".into(),
            invoke_args: vec!["-lc".into()],
            syntax_hint: " The current shell is bash: use POSIX/bash syntax.",
        })
    }

    /// Full command-line arguments for a user command: invocation args +
    /// the command itself.
    pub(super) fn command_args(&self, command: &str) -> Vec<String> {
        let mut args = self.invoke_args.clone();
        args.push(command.to_owned());
        args
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
    shell: &Shell,
    workspace: &Workspace,
    command: &str,
    timeout: Option<Duration>,
    protect_git: bool,
    process_group_slot: Option<Arc<AtomicI32>>,
    output_slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
    sandbox: Option<&crate::config::Sandbox>,
) -> Result<String, String> {
    #[cfg(windows)]
    if let Some(policy) = sandbox {
        return super::windows_sandbox::run(
            shell,
            workspace,
            command,
            timeout,
            protect_git,
            process_group_slot,
            output_slot,
            spool,
            policy,
        )
        .await;
    }

    // Build the command: bare bash, or wrapped in bwrap when sandboxed.
    // bwrap is a *construction tool*, so we spell out the policy explicitly:
    // system dirs read-only, workspace writable (per config), /tmp scratch,
    // no new privileges, TIOCSTI blocked, die with the parent. Network is
    // shared by default (agents often need to fetch); config can disable it.
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
            args.push(shell.executable.clone());
            args.extend(shell.command_args(command));
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
            let mut cmd = Command::new(&shell.executable);
            cmd.args(shell.command_args(command));
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
