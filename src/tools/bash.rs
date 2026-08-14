use super::*;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::agent::AgentEvent;

use super::background::{BackgroundTasks, ExitSlot, OutputSlot, TaskExit, TaskSpool, slot_append};

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
    self_session_id: Option<String>,
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
        // 发起者会话：subagent 传自己的 session id（None = 主会话/未知）。
        // 后台 bash 任务记到共享 registry 时带它，任务面板才能显示真正的
        // 发起者而非 registry 所属会话。
        owner_session: self_session_id,
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
    /// 发起者会话 id（subagent 为它自己的 session id，主会话为 None）：
    /// 后台 bash 任务在共享 registry 里用它标注发起者。
    pub(super) owner_session: Option<String>,
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

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
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
                return self
                    .background
                    .start_detached(
                        self.workspace.clone(),
                        command.to_owned(),
                        self.protect_git,
                        self.sandbox.clone(),
                        self.owner_session.clone(),
                    )
                    .map(ToolOutput::text);
            }
            // Background commands run under THIS facade's sandbox (possibly a
            // read-only role's narrowed policy), never the shared registry's
            // wider one.
            return self
                .background
                .start_with_sender(
                    self.workspace.clone(),
                    command.to_owned(),
                    self.protect_git,
                    self.sender.clone(),
                    self.sandbox.clone(),
                    self.owner_session.clone(),
                )
                .map(ToolOutput::text);
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
            None,
        )
        .await
        .map(ToolOutput::text)
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

/// True when `path` (or one of its ancestors) is itself a sandbox mount
/// root — i.e. the file is reachable at that path inside the sandbox.
pub(super) fn path_visible_in_sandbox(path: &Path, mounts: &[PathBuf]) -> bool {
    let mut current = Some(path);
    while let Some(dir) = current {
        if mounts.iter().any(|mount| mount.as_path() == dir) {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// Mount roots visible inside the sandbox, mirroring the bind set
/// `run_bash` builds for bwrap: system read-only binds, configured
/// readable/writable mounts and paths, and the workspace root.
pub(super) fn sandbox_mount_roots(
    sandbox: &crate::config::Sandbox,
    workspace_root: &Path,
) -> Vec<PathBuf> {
    let mut mounts = vec![
        PathBuf::from("/dev"),
        PathBuf::from("/proc"),
        PathBuf::from("/usr"),
        PathBuf::from("/bin"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
        PathBuf::from("/etc"),
    ];
    for (source, _dest) in &sandbox.readable_mounts {
        mounts.push(PathBuf::from(source));
    }
    for (source, _dest) in &sandbox.writable_mounts {
        mounts.push(PathBuf::from(source));
    }
    mounts.extend(sandbox.readable_paths.iter().map(PathBuf::from));
    mounts.extend(sandbox.writable_paths.iter().map(PathBuf::from));
    mounts.push(workspace_root.to_path_buf());
    mounts
}

/// The sandbox default git config is injected only when the host has a
/// `gh` executable whose path is visible inside the sandbox. `None`
/// (no host gh) or a path outside every mount root → `false`.
pub(super) fn gh_visible_in_sandbox(host_gh: Option<&Path>, mounts: &[PathBuf]) -> bool {
    host_gh.is_some_and(|gh| path_visible_in_sandbox(gh, mounts))
}

/// True when `path` is a regular file with at least one execute bit.
#[cfg(unix)]
pub(super) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// Non-Unix fallback: `is_file` only (Windows `where`-style semantics).
#[cfg(not(unix))]
pub(super) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Resolve `gh` through a PATH-style variable (`which` semantics),
/// honoring execute bits on Unix. `path_env` is injectable for tests.
pub(super) fn gh_in_path(path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "gh.exe" } else { "gh" };
    std::env::split_paths(path_env?)
        .map(|dir| dir.join(exe))
        .find(|candidate| is_executable_file(candidate))
}

/// Host-side absolute path to the `gh` CLI via the process PATH.
/// `None` when the host has no executable `gh`.
pub(super) fn gh_host_path() -> Option<PathBuf> {
    gh_in_path(std::env::var_os("PATH").as_deref())
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
    exit_slot: Option<ExitSlot>,
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
            exit_slot,
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
            // Configured mounts first: (canonical source, configured dest),
            // so a symlink alias appears inside the sandbox at the path the
            // user configured. The canonical path self-mounts below keep
            // manually-constructed sandboxes (and the canonical locations
            // themselves) working exactly as before.
            for (source, dest) in &sandbox.readable_mounts {
                mounts.push((source.as_str(), dest.as_str(), "--ro-bind-try", false));
            }
            for (source, dest) in &sandbox.writable_mounts {
                mounts.push((source.as_str(), dest.as_str(), "--bind-try", false));
            }
            for path in &sandbox.readable_paths {
                mounts.push((path.as_str(), path.as_str(), "--ro-bind-try", false));
            }
            for path in &sandbox.writable_paths {
                mounts.push((path.as_str(), path.as_str(), "--bind-try", false));
            }
            mounts.push((root_str.as_str(), root_str.as_str(), workspace_bind, true));
            mounts.sort_by_key(|(_, dest, _, workspace)| {
                (std::path::Path::new(dest).components().count(), *workspace)
            });
            for (source, dest, bind, _) in mounts {
                args.push(bind.into());
                args.push(source.into());
                args.push(dest.into());
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
            // Only inject the default git config when the gh CLI is
            // actually reachable inside the sandbox: a host-side `gh`
            // whose executable path falls under a sandbox-mounted
            // directory. A host without gh (or with gh outside every
            // mount root) gets no injection at all — zero impact.
            let gh_visible = gh_visible_in_sandbox(
                gh_host_path().as_deref(),
                &sandbox_mount_roots(sandbox, root),
            );
            if sandbox.network && gh_visible {
                // Sandbox default git config, injected via env (equivalent
                // to a minimal ~/.gitconfig, no file needed): let `git
                // push` work out of the box by delegating credentials to
                // the gh CLI (whose config dir is mounted read-only) and
                // rewriting SSH github.com URLs to HTTPS. GIT_CONFIG_*
                // requires git >= 2.31; older git silently ignores them,
                // which is fine (no config is still the current behavior).
                cmd.env("GIT_CONFIG_COUNT", "2");
                cmd.env("GIT_CONFIG_KEY_0", "credential.helper");
                cmd.env("GIT_CONFIG_VALUE_0", "!gh auth git-credential");
                cmd.env("GIT_CONFIG_KEY_1", "url.https://github.com/.insteadOf");
                cmd.env("GIT_CONFIG_VALUE_1", "git@github.com:");
            }
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
                if let Some(slot) = &exit_slot {
                    *slot.lock().unwrap() = TaskExit {
                        exit_code: None,
                        signal: Some("SIGKILL".into()),
                        status: Some("killed".into()),
                    };
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
    // Structured exit metadata via the out-slot (background tasks only):
    // the human-readable text below stays the model-visible wire format,
    // and the structured fields ride along as durable trace metadata.
    if let Some(slot) = &exit_slot {
        *slot.lock().unwrap() = TaskExit {
            exit_code: status.code(),
            signal: exit_signal_name(&status),
            status: Some(exit_status_label(&status).to_owned()),
        };
    }
    let text = format_output(status.code(), &stdout, &stderr);
    if status.success() {
        Ok(text)
    } else {
        let mut text = text;
        // rtk-style "tee on failure": a failed command whose visible output
        // was truncated gets its full output written to a log file so the
        // model can read_file the whole log instead of guessing from the
        // surviving head+tail. Successful long output is not persisted.
        if (stdout.truncated || stderr.truncated)
            && let Some(path) = persist_full_output(workspace.root(), command, &stdout, &stderr)
        {
            text.push_str(&format!("\n[full output: {}]", path.display()));
        }
        Err(text)
    }
}

/// Per-stream output budget shown to the model (unchanged semantics: with
/// stdout and stderr the worst case is still ~2 × OUTPUT_LIMIT bytes).
/// The budget is split head + tail so the *end* of a stream — where build
/// and test errors almost always land — survives truncation.
/// `HEAD_LIMIT` (48 KiB) keeps the beginning (command echo, early progress);
/// `TAIL_LIMIT` (16 KiB) keeps the final lines, which are the useful ones
/// for a failed command.
pub(super) const HEAD_LIMIT: usize = 48 * 1024;
pub(super) const TAIL_LIMIT: usize = 16 * 1024;
/// Upper bound on the full output retained in memory per stream for failure
/// persistence (see [`persist_full_output`]). Always retaining up to
/// 2 × `FULL_LIMIT` bytes is cheap for typical command output and avoids a
/// spool-file round trip; a stream longer than this only gets its first
/// `FULL_LIMIT` bytes written to the log.
pub(super) const FULL_LIMIT: usize = 16 * 1024 * 1024;

pub(super) struct Captured {
    /// Head segment: the first `HEAD_LIMIT` bytes of the stream (the whole
    /// stream when `total ≤ HEAD_LIMIT`). When `truncated`, its end is
    /// trimmed to a UTF-8 char boundary so the seam never renders a half
    /// character; when not truncated the bytes are kept raw so the exact
    /// stream can be glued back together with the tail window.
    pub(super) bytes: Vec<u8>,
    /// Tail segment: the last `TAIL_LIMIT` bytes, rendered after the
    /// truncation marker; only meaningful when `truncated`.
    pub(super) tail: Vec<u8>,
    /// Total bytes read from the stream (before head/tail alignment).
    pub(super) total: usize,
    pub(super) truncated: bool,
    /// Complete stream up to `FULL_LIMIT` bytes, retained so a failed and
    /// truncated command's full output can be persisted to a log file.
    pub(super) full: Vec<u8>,
}

pub(super) async fn capture(
    mut reader: impl AsyncRead + Unpin,
    slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
) -> std::io::Result<Captured> {
    let mut captured = Captured {
        bytes: Vec::new(),
        tail: Vec::new(),
        total: 0,
        truncated: false,
        full: Vec::new(),
    };
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let data = &buffer[..count];
        if let Some(slot) = &slot {
            slot_append(slot, data);
        }
        if let Some(spool) = &spool {
            spool.append(data);
        }
        captured.total += count;
        // Full output for potential failure persistence, capped at FULL_LIMIT.
        if captured.full.len() < FULL_LIMIT {
            let room = FULL_LIMIT - captured.full.len();
            captured.full.extend_from_slice(&data[..count.min(room)]);
        }
        // Head: first HEAD_LIMIT bytes.
        if captured.bytes.len() < HEAD_LIMIT {
            let room = HEAD_LIMIT - captured.bytes.len();
            captured.bytes.extend_from_slice(&data[..count.min(room)]);
        }
        // Tail: rolling window of the last TAIL_LIMIT bytes.
        captured.tail.extend_from_slice(data);
        if captured.tail.len() > TAIL_LIMIT {
            let excess = captured.tail.len() - TAIL_LIMIT;
            captured.tail.drain(..excess);
        }
        captured.truncated |= captured.total > OUTPUT_LIMIT;
    }
    if captured.truncated {
        // Align both seams to UTF-8 char boundaries so the head/tail never
        // split a multibyte character (which would render extra U+FFFD).
        let keep = utf8_back_boundary(&captured.bytes, captured.bytes.len());
        captured.bytes.truncate(keep);
        let skip = utf8_front_boundary(&captured.tail, 0);
        if skip > 0 {
            captured.tail.drain(..skip);
        }
    }
    Ok(captured)
}

pub(super) fn format_output(code: Option<i32>, stdout: &Captured, stderr: &Captured) -> String {
    format!(
        "exit code: {}\nstdout:\n{}\nstderr:\n{}",
        code.map_or_else(|| "signal".into(), |code| code.to_string()),
        render_stream(stdout),
        render_stream(stderr)
    )
}

/// Structured status label for the exit out-slot: exit 0 → "completed", a
/// non-zero exit code → "failed", a signal death → "killed". Mirrors the
/// "exit code: N" / "exit code: signal" text in [`format_output`] without
/// changing that model-visible wire text.
pub(super) fn exit_status_label(status: &std::process::ExitStatus) -> &'static str {
    if status.success() {
        "completed"
    } else if status.code().is_some() {
        "failed"
    } else {
        "killed"
    }
}

/// Human-readable signal name for a signal-killed exit status; `None` for
/// normal (exit-code) completions. Linux signal numbers (this project's
/// primary target); unknown numbers fall back to "signal N".
#[cfg(unix)]
pub(super) fn exit_signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(signal_name)
}

#[cfg(not(unix))]
pub(super) fn exit_signal_name(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// Map a raw signal number to its conventional name (Linux table).
#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    const NAMES: &[(i32, &str)] = &[
        (1, "SIGHUP"),
        (2, "SIGINT"),
        (3, "SIGQUIT"),
        (4, "SIGILL"),
        (5, "SIGTRAP"),
        (6, "SIGABRT"),
        (7, "SIGBUS"),
        (8, "SIGFPE"),
        (9, "SIGKILL"),
        (10, "SIGUSR1"),
        (11, "SIGSEGV"),
        (12, "SIGUSR2"),
        (13, "SIGPIPE"),
        (14, "SIGALRM"),
        (15, "SIGTERM"),
        (17, "SIGCHLD"),
        (18, "SIGCONT"),
        (19, "SIGSTOP"),
        (20, "SIGTSTP"),
        (23, "SIGURG"),
        (24, "SIGXCPU"),
        (25, "SIGXFSZ"),
        (26, "SIGVTALRM"),
        (27, "SIGPROF"),
        (28, "SIGWINCH"),
        (31, "SIGSYS"),
    ];
    NAMES
        .iter()
        .find(|(number, _)| *number == signal)
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| format!("signal {signal}"))
}

/// Render one captured stream: the full text when it fit within the budget,
/// otherwise `head … [truncated: N bytes omitted] … tail`.
fn render_stream(captured: &Captured) -> String {
    if !captured.truncated {
        // The whole stream fits: `bytes` holds the first min(total,
        // HEAD_LIMIT) bytes and anything past HEAD_LIMIT is a suffix of the
        // rolling tail window, so gluing them back reproduces the exact
        // stream (no lossy conversion in between).
        let mut out = captured.bytes.clone();
        if captured.total > out.len() {
            let rest = captured.total - out.len();
            let start = captured.tail.len().saturating_sub(rest);
            out.extend_from_slice(&captured.tail[start..]);
        }
        return String::from_utf8_lossy(&out).into_owned();
    }
    let omitted = captured
        .total
        .saturating_sub(captured.bytes.len() + captured.tail.len());
    format!(
        "{}\n[truncated: {} bytes omitted]\n{}",
        String::from_utf8_lossy(&captured.bytes),
        omitted,
        String::from_utf8_lossy(&captured.tail)
    )
}

/// Largest index ≤ `len` on a UTF-8 char boundary: walks back over the
/// continuation bytes of a multibyte char split by the cut and drops the
/// char's leading byte too when the char is incomplete. Walks at most 3
/// bytes, so it is O(1).
pub(super) fn utf8_back_boundary(bytes: &[u8], len: usize) -> usize {
    let mut pos = len;
    while pos > 0 && (bytes[pos - 1] & 0xC0) == 0x80 {
        pos -= 1;
    }
    if pos == 0 {
        return 0;
    }
    let lead = bytes[pos - 1];
    let conts = len - pos;
    let needed = match lead {
        0xC0..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF7 => 3,
        _ => 0,
    };
    if conts < needed { pos - 1 } else { len }
}

/// Smallest index ≥ `offset` on a UTF-8 char boundary: skips at most 3
/// leading continuation bytes when the tail window starts mid-character.
pub(super) fn utf8_front_boundary(bytes: &[u8], offset: usize) -> usize {
    let mut pos = offset;
    let max = (offset + 3).min(bytes.len());
    while pos < max && (bytes[pos] & 0xC0) == 0x80 {
        pos += 1;
    }
    pos
}

/// Persist the full untruncated output of a failed command whose displayed
/// text was truncated, to `<workspace>/.e-agent/logs/bash-{timestamp}-{slug}.log`,
/// and return the path so the result can hint `[full output: …]` and the
/// model can `read_file` the log for the whole text. Returns `None` when
/// nothing could be written (I/O error — e.g. a read-only workspace).
/// Memory trade-off: `capture` always retains up to `FULL_LIMIT` bytes per
/// stream so the full text is available here without re-running the command;
/// streams longer than the cap are truncated in the log with a note.
pub(super) fn persist_full_output(
    workspace_root: &std::path::Path,
    command: &str,
    stdout: &Captured,
    stderr: &Captured,
) -> Option<std::path::PathBuf> {
    let logs_dir = workspace_root.join(".e-agent").join("logs");
    std::fs::create_dir_all(&logs_dir).ok()?;
    let path = logs_dir.join(format!(
        "bash-{}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f"),
        command_slug(command)
    ));
    let mut content = format!("$ {command}\n--- stdout ---\n");
    content.push_str(&String::from_utf8_lossy(&stdout.full));
    if stdout.total > stdout.full.len() {
        content.push_str(&format!(
            "\n[stdout log capped at {} bytes]\n",
            stdout.full.len()
        ));
    }
    content.push_str("\n--- stderr ---\n");
    content.push_str(&String::from_utf8_lossy(&stderr.full));
    if stderr.total > stderr.full.len() {
        content.push_str(&format!(
            "\n[stderr log capped at {} bytes]\n",
            stderr.full.len()
        ));
    }
    content.push('\n');
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// Filesystem-safe short slug for log filenames derived from the command
/// (e.g. `cargo test -- --nocapture` → `cargo_test_nocapture`; falls back to
/// `bash` for commands without any alphanumerics).
fn command_slug(command: &str) -> String {
    let mut slug = String::new();
    let mut prev_sep = true;
    for c in command.chars().take(64) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            prev_sep = false;
        } else if !prev_sep {
            slug.push('_');
            prev_sep = true;
        }
    }
    let slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        "bash".to_string()
    } else {
        slug
    }
}
