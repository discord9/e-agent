use super::*;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, io::AsRawFd};
// `std::process::Command::pre_exec` (CommandExt) is only used by the
// test-only `plan_spawn`; tokio's `Command` has an inherent `pre_exec`.
#[cfg(all(unix, test))]
use std::os::unix::process::CommandExt;

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

/// A descriptor-pinned bwrap invocation plan.
///
/// The policy-parent protection is installed as a projection rebuilt from
/// fds pinned with `openat2(RESOLVE_NO_SYMLINKS)` + `fstat` (regular files
/// and directories only) at plan time. The `fds` must be held by the caller
/// until every spawn that needs them has happened — bash foreground and
/// background, and both run_rust stages — so the fd numbers in `args` stay
/// valid and the pinned inodes survive pathname swaps (no TOCTOU re-bind).
#[cfg(unix)]
pub(super) struct BwrapPlan {
    /// Full bwrap argument vector. `--bind-fd N dest` / `--ro-bind-fd N
    /// dest` entries carry the actual fd numbers of `fds`.
    pub(super) args: Vec<std::ffi::OsString>,
    /// OwnedFds backing the `--bind-fd` numbers. Never read — holding them
    /// is the point: they keep the fd numbers valid and the pinned inodes
    /// alive until the last spawn.
    #[allow(dead_code)]
    pub(super) fds: Vec<rustix::fd::OwnedFd>,
    /// Raw fd numbers the bwrap child must inherit. CLOEXEC is cleared on
    /// exactly these numbers, in the forked child only, via `pre_exec`.
    pub(super) numbers: Vec<i32>,
}

/// Build the bwrap argument vector from the resolved `[sandbox]` policy,
/// shared by bash and the experimental `run_rust` tool. Caller appends the
/// program.
///
/// Security invariants:
/// - The policy parent subtree is excluded from the generic pathname mount
///   loop and instead rebuilt by the projection: `--tmpfs P`, restored
///   top-level ordinary entries (regular files/dirs only, single-component
///   names from the pinned parent fd, byte-exact even for non-UTF-8) and
///   explicit descendants via `--bind-fd`/`--ro-bind-fd`, then
///   `--remount-ro P`. Symlinks, FIFOs, sockets and devices are never
///   projected. The policy file itself is always read-only; a missing one
///   keeps its mount point absent (ENOENT) and the host gains nothing.
/// - No pathname re-bind and no symlink fallback touch the policy parent:
///   every bind at/under it is descriptor-pinned.
/// - Construction failures fail closed: an Err is returned and nothing
///   spawns.
#[cfg(unix)]
pub(super) fn build_bwrap_plan(
    workspace: &Workspace,
    sandbox: &crate::config::Sandbox,
    protect_git: bool,
    network: bool,
    chdir: &str,
    scratch_bind: Option<&str>,
) -> Result<BwrapPlan, String> {
    let root = workspace.root();
    let workspace_bind = if sandbox.workspace_writable {
        "--bind"
    } else {
        "--ro-bind"
    };
    let root_str = root.to_string_lossy().into_owned();
    // Extra /home paths must be mounted AFTER the /home tmpfs.
    let mut args: Vec<std::ffi::OsString> = vec![
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
    // systemd-resolved stub so symlinked /etc/resolv.conf works.
    if std::path::Path::new("/run/systemd/resolve").exists() {
        args.push("--dir".into());
        args.push("/run/systemd".into());
        args.push("--ro-bind".into());
        args.push("/run/systemd/resolve".into());
        args.push("/run/systemd/resolve".into());
    }
    if let Some(scratch) = scratch_bind {
        // run_rust: the private scratch is the sandbox /tmp, deleted afterwards.
        args.extend(["--bind".into(), scratch.into(), "/tmp".into()]);
    } else {
        args.extend(["--tmpfs".into(), "/tmp".into()]);
    }
    args.extend(["--tmpfs".into(), "/home".into()]);
    // Ancestors before descendants (workspace last among equals stays
    // authoritative). Any destination at or under the policy parent is
    // excluded here: that subtree is owned by the fd-pinned projection
    // below, so no pathname bind can project over the policy file.
    let policy_parent = workspace.policy_anchor().parent().map(Path::to_path_buf);
    let mut mounts = Vec::new();
    // Any configured destination at or under the policy parent is excluded
    // from the generic pathname mount loop: that subtree is owned by the
    // fd-pinned projection below, so no pathname bind can project over the
    // policy file. Every filtered destination MUST be restored by the
    // projection; if the projection cannot run, the plan fails closed
    // instead of silently hiding the filtered destinations.
    let mut filtered_policy_dests = false;
    for (source, dest) in &sandbox.readable_mounts {
        if dest_is_policy_subtree(dest, policy_parent.as_deref()) {
            filtered_policy_dests = true;
        } else {
            mounts.push((source.as_str(), dest.as_str(), "--ro-bind-try", false));
        }
    }
    for (source, dest) in &sandbox.writable_mounts {
        if dest_is_policy_subtree(dest, policy_parent.as_deref()) {
            filtered_policy_dests = true;
        } else {
            mounts.push((source.as_str(), dest.as_str(), "--bind-try", false));
        }
    }
    for path in &sandbox.readable_paths {
        if dest_is_policy_subtree(path, policy_parent.as_deref()) {
            filtered_policy_dests = true;
        } else {
            mounts.push((path.as_str(), path.as_str(), "--ro-bind-try", false));
        }
    }
    for path in &sandbox.writable_paths {
        if dest_is_policy_subtree(path, policy_parent.as_deref()) {
            filtered_policy_dests = true;
        } else {
            mounts.push((path.as_str(), path.as_str(), "--bind-try", false));
        }
    }
    mounts.push((root_str.as_str(), root_str.as_str(), workspace_bind, true));
    mounts.sort_by_key(|(_, dest, _, workspace)| {
        (std::path::Path::new(dest).components().count(), *workspace)
    });
    // Close the ancestor-directory escape: `/home` (and any other empty
    // mount above the workspace) is a WRITABLE tmpfs, so without an
    // explicit ancestor bind the sandbox could create siblings of the
    // workspace bind (e.g. `parent/EVIL`) that never resolve to the
    // workspace bind — a silent host-looking hole when the same paths
    // exist on the host, and real pollution where a writable bind
    // legitimately covers them.
    //
    // Guard strategy — HIDE, then rebuild exactly what is authorized:
    // the nearest EXISTING host ancestor of the workspace is shadowed by
    // an EMPTY writable tmpfs (`--tmpfs G`), NOT bound from the host.
    // Mounting any host filesystem — read-only or not — would import its
    // PRE-EXISTING nested host submounts wholesale, and `--remount-ro`
    // locks only the named mount point, never those nested mounts: a
    // writable host submount beneath the guard would stay writable and be
    // an escape. A tmpfs carries no host mounts at all, so there is
    // nothing uncontrolled to lock. Only the mounts the plan installs
    // below the tmpfs ever exist under it — every one of them an explicit
    // authorization or the exact workspace.
    //
    // Visibility trade-off (intentional): unconfigured sibling entries of
    // the workspace are HIDDEN, not merely read-only — the previous
    // design exposed them by binding the host ancestor, but doing so is
    // exactly what imported the nested-submount escape, and no bind-free
    // bwrap mechanism can show a host directory without its submounts.
    // Mount order below a guard:
    //   1. `--tmpfs G` (writable, so bwrap can still create every
    //      configured mount point below it in depth order);
    //   2. every configured mount under the guard in ascending depth,
    //      INCLUDING the exact workspace bind — the empty tmpfs carries
    //      no pre-existing workspace directory, so bwrap must be able to
    //      mkdir the mount point while the guard is still writable;
    //   3. `--remount-ro G` — seals the guard tmpfs itself (per-mount,
    //      never recursive): no NEW unauthorized mount point can be
    //      created under it, while every submount installed in step 2
    //      keeps its own configured mode — the workspace per
    //      `workspace_writable`, explicit writable grants writable
    //      (intentional capability grants, never silently narrowed),
    //      `--ro-bind` mounts read-only (bwrap remounts those itself).
    //      Mounts under an explicitly configured WRITABLE ancestor keep
    //      their authority because the guard then sits above that
    //      ancestor.
    // A workspace at `/` or a guard that could only be `/` fails closed.
    // Guard destinations are canonicalized so the tmpfs lands on the real
    // directory a symlinked ancestor component would resolve to.
    let mut configured: Vec<(PathBuf, bool)> = Vec::new();
    for (writable, paths) in [
        (true, &sandbox.writable_paths),
        (false, &sandbox.readable_paths),
    ] {
        for path in paths {
            configured.push((PathBuf::from(path), writable));
        }
    }
    for (writable, mounts) in [
        (true, &sandbox.writable_mounts),
        (false, &sandbox.readable_mounts),
    ] {
        for (_, dest) in mounts {
            configured.push((PathBuf::from(dest), writable));
        }
    }
    let ancestor_guards = ancestor_guards(root, &configured)?;
    let under_workspace = |dest: &str| {
        let dest = Path::new(dest);
        dest != root && dest.starts_with(root)
    };
    // 1+2: guard tmpfs first (bwrap needs it mounted before it can create
    // mount points below it), then every configured mount except the
    // workspace subtree in ascending depth. The tmpfs imports NO host
    // content and NO pre-existing nested host submounts — only the
    // explicit mounts installed here exist under it.
    for (source, dest) in &ancestor_guards {
        debug_assert_eq!(source, dest, "guard tmpfs sources carry no host path");
        args.push("--tmpfs".into());
        args.push(dest.as_os_str().into());
    }
    for (source, dest, bind, workspace_flag) in &mounts {
        if !workspace_flag && !under_workspace(dest) {
            args.push((*bind).into());
            args.push((*source).into());
            args.push((*dest).into());
        }
    }
    // 3: the exact workspace bind, then mounts strictly under it. These
    // must also precede the guard lock: bwrap creates the mount points by
    // mkdir inside the guard tmpfs, which the lock would make read-only
    // (the guard tmpfs starts EMPTY — unlike a host bind it carries no
    // pre-existing workspace directory to mount over).
    args.push(workspace_bind.into());
    args.push(root_str.as_str().into());
    args.push(root_str.as_str().into());
    for (source, dest, bind, workspace_flag) in &mounts {
        if !workspace_flag && under_workspace(dest) {
            args.push((*bind).into());
            args.push((*source).into());
            args.push((*dest).into());
        }
    }
    // 4: lock the guard tmpfs read-only. `--remount-ro` is per-mount
    // (never recursive), which is exactly right here: the lock seals the
    // guard tmpfs so no NEW unauthorized mount point can be created under
    // it, while every submount installed above — the workspace and the
    // explicit grants — keeps its own configured mode. `--ro-bind`
    // submounts were already remounted read-only by bwrap itself.
    for (_, dest) in &ancestor_guards {
        args.push("--remount-ro".into());
        args.push(dest.as_os_str().into());
    }

    // Protect the startup policy anchor: a descriptor-pinned projection
    // applied after every other mount so no bind (workspace, alias or
    // explicit descendant) can shadow it. Every destination filtered from
    // the generic loop above must be restored here; a projection that
    // cannot run fails the plan closed instead of hiding them silently.
    let mut fds: Vec<rustix::fd::OwnedFd> = Vec::new();
    if workspace.policy_anchor_is_visible() {
        project_policy_parent(
            workspace,
            sandbox,
            &mut args,
            &mut fds,
            filtered_policy_dests,
        )?;
    } else if filtered_policy_dests {
        return Err(format!(
            "fail closed: sandbox policy destinations under {} are excluded from the generic mount loop, but the policy-anchor projection cannot run from workspace {} (policy anchor {} is not visible), so those destinations would be silently hidden",
            policy_parent
                .as_deref()
                .map_or_else(|| "<policy parent>".into(), |p| p.display().to_string()),
            root_str,
            workspace.policy_anchor().display()
        ));
    }

    // .git read-only over itself (subagents; run_rust always). Installed
    // AFTER the policy projection: a workspace whose `.git` lives under the
    // policy parent (e.g. rerooted into `.e-agent/worktrees/<name>`) would
    // otherwise have its ro-bind shadowed by the projection's `--tmpfs
    // parent` plus the top-level writable bind.
    if protect_git {
        let git_path = format!("{root_str}/.git");
        if std::path::Path::new(&git_path).exists() {
            args.push("--ro-bind".into());
            args.push(git_path.clone().into());
            args.push(git_path.into());
        }
    }

    args.extend([
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--new-session".into(),
        "--die-with-parent".into(),
        "--chdir".into(),
        chdir.into(),
    ]);
    if !network {
        args.push("--unshare-net".into());
    }
    let numbers = fds.iter().map(|fd| fd.as_raw_fd()).collect();
    Ok(BwrapPlan { args, fds, numbers })
}

/// Compute the ancestor guards closing the ancestor escape (see
/// `build_bwrap_plan`): for the workspace parent, walk up while an
/// explicitly configured WRITABLE destination covers the current directory
/// (that coverage is an intentional capability grant, so the guard moves
/// to ITS parent instead of silently narrowing it), then pin the nearest
/// existing host ancestor to be shadowed by an EMPTY writable tmpfs
/// (`--tmpfs`) — hiding it instead of importing it. Importing the host
/// directory (even read-only) would also import its pre-existing nested
/// host submounts, and `--remount-ro` locks only the named mount point,
/// never nested mounts: a writable nested submount would be an escape.
/// An explicit READ-ONLY configured mount already covers the directory —
/// no guard needed there.
///
/// Fail-closed rules:
/// - workspace root `/` (no parent): Err — the escape cannot be bounded.
/// - no existing ancestor below `/`: Err — protection cannot be established.
/// - the guard would have to be `/` itself: Err — shadowing `/` is the
///   exact overreach this fix must never perform.
///
/// Returns `(canonical, dest)` pairs ordered ancestor-first; the canonical
/// path is audit-only (the tmpfs mounts no source) and proves the guard
/// destination resolves to the intended real directory when an ancestor
/// component is a symlink. The exact workspace bind stays last and keeps
/// its configured mode.
#[cfg(unix)]
pub(super) fn ancestor_guards(
    root: &Path,
    configured: &[(PathBuf, bool)],
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut dir = root.parent().ok_or_else(|| {
        format!(
            "fail closed: workspace root {} has no parent; the sandbox ancestor protection cannot be established",
            root.display()
        )
    })?;
    let mut guards = Vec::new();
    // True once the walk moved up through explicitly-writable coverage: the
    // remaining path to / is then covered by that writable bind over the
    // private root tmpfs, which maps to nothing on the host.
    let mut covered_by_grant = false;
    loop {
        // Explicitly configured WRITABLE coverage is an intentional grant:
        // keep its authority and protect ITS parent instead.
        if configured
            .iter()
            .any(|(dest, writable)| *writable && dir.starts_with(dest))
        {
            covered_by_grant = true;
            dir = dir.parent().ok_or_else(|| {
                format!(
                    "fail closed: an explicitly writable path covers {} up to /; the ancestor protection cannot be established without binding /",
                    dir.display()
                )
            })?;
            continue;
        }
        // Explicitly configured READ-ONLY coverage is already safe as-is.
        if configured
            .iter()
            .any(|(dest, writable)| !*writable && dir.starts_with(dest))
        {
            break;
        }
        if dir == Path::new("/") {
            // Reached the private sandbox root tmpfs through an explicit
            // writable grant: writes there cannot resolve to host paths.
            if covered_by_grant {
                break;
            }
            return Err(format!(
                "fail closed: protecting the workspace ancestor of {} would require binding / read-only, which the sandbox must never do",
                root.display()
            ));
        }
        // /tmp is a private tmpfs (or the run_rust private scratch bind)
        // that never maps to host paths: nothing under it can escape to the
        // host, so no guard is needed for scratch workspaces.
        if dir == Path::new("/tmp") {
            break;
        }
        // /home is always the sandbox's own writable tmpfs (mounted before
        // every bind): a workspace under it REQUIRES a guard — this is the
        // exact incident shape (`/home` tmpfs + exact child bind left the
        // writable parent tmpfs covering real host siblings).
        if !dir.is_dir() {
            // A missing ancestor cannot host a write that escapes: no
            // pathname resolution can pass it, and no later mount binds
            // over it (guards are the only mounts ever installed at or
            // above the workspace). Walk up to the nearest existing one.
            dir = dir.parent().ok_or_else(|| {
                format!(
                    "fail closed: no existing ancestor found while protecting the workspace ancestor of {}",
                    root.display()
                )
            })?;
            continue;
        }
        let canonical = std::fs::canonicalize(dir).map_err(|error| {
            format!(
                "fail closed: cannot canonicalize the workspace ancestor {}: {error}",
                dir.display()
            )
        })?;
        guards.push((canonical, dir.to_path_buf()));
        break;
    }
    Ok(guards)
}

/// True when a configured destination is the policy parent itself or lies
/// under it (component-wise), i.e. the generic pathname mount loop must not
/// bind it — the fd-pinned projection owns that subtree.
#[cfg(unix)]
fn dest_is_policy_subtree(dest: &str, policy_parent: Option<&Path>) -> bool {
    let Some(parent) = policy_parent else {
        return false;
    };
    let dest = Path::new(dest);
    dest == parent || dest.starts_with(parent)
}

/// One resolved policy entry whose destination is the policy parent or a
/// descendant of it: `dest` is the sandbox path, `writable` the final
/// policy mode, `source` the real host path pinned for restoration
/// (canonical for configured paths; the configured canonical source for
/// mount aliases).
#[cfg(unix)]
struct PolicyEntry {
    dest: PathBuf,
    writable: bool,
    source: PathBuf,
}

/// Configured entries (paths and mount aliases) whose destination lies at
/// or under the policy parent.
#[cfg(unix)]
fn policy_entries_under(
    sandbox: &crate::config::Sandbox,
    policy_parent: &Path,
) -> Vec<PolicyEntry> {
    let mut out = Vec::new();
    for (writable, paths) in [
        (true, &sandbox.writable_paths),
        (false, &sandbox.readable_paths),
    ] {
        for path in paths {
            let dest = PathBuf::from(path);
            if dest == policy_parent || dest.starts_with(policy_parent) {
                out.push(PolicyEntry {
                    dest: dest.clone(),
                    writable,
                    source: dest,
                });
            }
        }
    }
    for (writable, mounts) in [
        (true, &sandbox.writable_mounts),
        (false, &sandbox.readable_mounts),
    ] {
        for (source, dest) in mounts {
            let dest = PathBuf::from(dest);
            if dest == policy_parent || dest.starts_with(policy_parent) {
                out.push(PolicyEntry {
                    dest,
                    writable,
                    source: PathBuf::from(source),
                });
            }
        }
    }
    out
}

/// Winner-by-depth writability for a top-level child of the policy parent:
/// the most specific policy entry covering the child decides, falling back
/// to the workspace writability (the generic mount loop's semantics).
#[cfg(unix)]
fn child_writable(entries: &[PolicyEntry], dest: &Path, workspace_writable: bool) -> bool {
    let mut best: Option<(usize, bool)> = None;
    for entry in entries {
        if dest.starts_with(&entry.dest) {
            let depth = entry.dest.components().count();
            if best.is_none_or(|(best_depth, _)| depth > best_depth) {
                best = Some((depth, entry.writable));
            }
        }
    }
    best.map_or(workspace_writable, |(_, writable)| writable)
}

/// Append one descriptor-pinned bind (`--bind-fd`/`--ro-bind-fd`). The fd
/// number is embedded in `args` and the OwnedFd is recorded so the number
/// stays valid until the last spawn.
#[cfg(unix)]
fn push_bind(
    args: &mut Vec<std::ffi::OsString>,
    fds: &mut Vec<rustix::fd::OwnedFd>,
    fd: rustix::fd::OwnedFd,
    dest: PathBuf,
    writable: bool,
) {
    args.push(
        (if writable {
            "--bind-fd"
        } else {
            "--ro-bind-fd"
        })
        .into(),
    );
    args.push(fd.as_raw_fd().to_string().into());
    args.push(dest.into_os_string());
    fds.push(fd);
}

/// Pin a single-component (or relative chain) path beneath a pinned parent,
/// rejecting every symlink component via `openat2(RESOLVE_NO_SYMLINKS)`.
#[cfg(unix)]
fn pin_relative(
    parent: &rustix::fd::OwnedFd,
    relative: &Path,
) -> rustix::io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    let mut fd = rustix::io::dup(parent)?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(rustix::io::Errno::INVAL);
        };
        fd = openat2(
            &fd,
            name,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )?;
    }
    Ok(fd)
}

/// Pin the source of an explicit descendant: relative descent from the
/// pinned policy parent when the source lies under it, else an absolute
/// no-symlink open of the canonical source.
#[cfg(unix)]
fn pin_source(
    policy_parent_fd: &rustix::fd::OwnedFd,
    policy_parent: &Path,
    source: &Path,
) -> rustix::io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    if let Ok(relative) = source.strip_prefix(policy_parent) {
        pin_relative(policy_parent_fd, relative)
    } else {
        openat2(
            rustix::fs::CWD,
            source,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
    }
}

/// Install the policy-parent protection: `--tmpfs P`, restore the allowed
/// existing top-level ordinary entries (regular files/dirs only) and the
/// explicit descendants via descriptor-pinned binds, then `--remount-ro P`
/// so the policy parent is read-only and a missing config file stays ENOENT
/// (never created; the host gains nothing). Failures fail closed.
#[cfg(unix)]
fn project_policy_parent(
    workspace: &Workspace,
    sandbox: &crate::config::Sandbox,
    args: &mut Vec<std::ffi::OsString>,
    fds: &mut Vec<rustix::fd::OwnedFd>,
    filtered_policy_dests: bool,
) -> Result<(), String> {
    use rustix::fs::{FileType, Mode, OFlags, RawDir, ResolveFlags, fstat, openat2};
    let policy = workspace.policy_anchor();
    let parent = policy.parent().expect("policy anchor has a parent");
    let parent_depth = parent.components().count();
    // The parent must be opened O_RDONLY|O_DIRECTORY (a real fd): children
    // are enumerated from it with getdents, which EBADFs on O_PATH fds.
    // RESOLVE_NO_SYMLINKS rejects a symlinked `.e-agent` outright (fail
    // closed — no symlink fallback is ever used).
    let parent_fd = match openat2(
        rustix::fs::CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    ) {
        Ok(fd) => fd,
        // A missing policy parent (or a non-directory at that path) cannot
        // be projected; the config stays ENOENT and the host gains nothing.
        // If the generic loop excluded configured destinations under it,
        // those would be silently hidden — fail closed instead.
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => {
            if filtered_policy_dests {
                return Err(format!(
                    "fail closed: policy destinations under {} are excluded from the generic mount loop, but the policy parent {} does not exist, so the projection cannot restore them",
                    parent.display(),
                    parent.display()
                ));
            }
            return Ok(());
        }
        // An unreadable (execute-only) parent must NOT be skipped: the
        // sandbox — same user — could still open `config.toml` by known
        // filename through the writable workspace bind, bypassing the
        // protection. Plan construction fails closed instead.
        Err(rustix::io::Errno::ACCESS) => {
            return Err(format!(
                "fail closed: cannot open policy parent {} read-only (execute-only?): the projection cannot enumerate and protect the policy file",
                parent.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot pin policy parent {}: {error}",
                parent.display()
            ));
        }
    };
    let entries = policy_entries_under(sandbox, parent);
    args.push("--tmpfs".into());
    args.push(parent.as_os_str().into());
    // Top-level ordinary children of the policy parent, enumerated from the
    // pinned fd with single-component names (never followed, never lossy).
    let mut children: Vec<(std::ffi::CString, rustix::fd::OwnedFd)> = Vec::new();
    {
        let mut buffer = Vec::with_capacity(8192);
        let mut raw = RawDir::new(
            rustix::io::dup(&parent_fd).map_err(|e| format!("cannot dup policy parent fd: {e}"))?,
            buffer.spare_capacity_mut(),
        );
        while let Some(entry) = raw.next() {
            let entry = entry
                .map_err(|e| format!("cannot enumerate policy parent {}: {e}", parent.display()))?;
            let name = entry.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let child = match openat2(
                &parent_fd,
                name,
                OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::NO_SYMLINKS,
            ) {
                Ok(fd) => fd,
                // A symlink (or an entry that vanished mid-enumeration):
                // hidden, never exposed, never followed.
                Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOENT) => continue,
                Err(error) => {
                    return Err(format!("cannot pin policy parent child {bytes:?}: {error}"));
                }
            };
            let stat =
                fstat(&child).map_err(|e| format!("cannot stat policy parent child: {e}"))?;
            let file_type = FileType::from_raw_mode(stat.st_mode);
            if !file_type.is_file() && !file_type.is_dir() {
                // FIFOs, sockets, devices: not projected.
                continue;
            }
            children.push((name.to_owned(), child));
        }
    }
    for (name, child) in children {
        let bytes = name.to_bytes();
        let dest = parent.join(std::ffi::OsStr::from_bytes(bytes));
        // The policy file itself is always read-only (an existing config
        // stays readable; a missing one is never created).
        let writable = if bytes == b"config.toml" {
            false
        } else {
            child_writable(&entries, &dest, sandbox.workspace_writable)
        };
        push_bind(args, fds, child, dest, writable);
    }
    // Explicit descendants strictly below the top level, applied after the
    // top-level binds so deeper mounts keep the winner/depth semantics.
    // Ancestor-first, descendant-last: a deeper bind must be installed after
    // its shallower ancestors, otherwise a shallow RO parent bound later
    // stacks over (shadows) a deep RW child. Same destination: the config
    // resolver picks the first logical entry in policy order (paths before
    // mounts, writable before readable), so later duplicates are dropped —
    // stacking them would let the later mode win.
    let mut descendants: Vec<&PolicyEntry> = entries
        .iter()
        .filter(|entry| entry.dest.components().count() > parent_depth + 1)
        .collect();
    descendants.sort_by_key(|entry| entry.dest.components().count());
    descendants.dedup_by(|a, b| a.dest == b.dest);
    for entry in descendants {
        let source_fd = match pin_source(&parent_fd, parent, &entry.source) {
            Ok(fd) => fd,
            // Missing configured sources keep the old --bind-try semantics.
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => {
                return Err(format!(
                    "cannot pin policy descendant {}: {error}",
                    entry.dest.display()
                ));
            }
        };
        // Every pinned source must be a regular file or directory: O_PATH
        // also opens FIFOs/sockets/devices, and projecting one would leak a
        // descriptor-bound special file into the sandbox. Anything else
        // fails closed.
        let stat = fstat(&source_fd).map_err(|e| {
            format!(
                "cannot stat policy descendant {}: {e}",
                entry.dest.display()
            )
        })?;
        let file_type = FileType::from_raw_mode(stat.st_mode);
        if !file_type.is_file() && !file_type.is_dir() {
            return Err(format!(
                "fail closed: policy descendant {} source {} is not a regular file or directory",
                entry.dest.display(),
                entry.source.display()
            ));
        }
        push_bind(args, fds, source_fd, entry.dest.clone(), entry.writable);
    }
    args.push("--remount-ro".into());
    args.push(parent.as_os_str().into());
    Ok(())
}

/// `pre_exec` closure that clears CLOEXEC on exactly the given fd numbers in
/// the forked child, so bwrap can bind the descriptor-pinned sources. Every
/// other fd keeps CLOEXEC and is never inherited. Only async-signal-safe
/// fcntl syscalls are used.
#[cfg(unix)]
pub(super) fn clear_cloexec_pre_exec(
    numbers: Vec<i32>,
) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
    move || {
        for raw in &numbers {
            // SAFETY: the raw numbers were recorded from live OwnedFds held
            // by the plan for the whole spawn, so they are valid fds of the
            // forked child; only fcntl runs here.
            let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(*raw) };
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
        }
        Ok(())
    }
}

/// Spawn bwrap with a built plan (test hook and TOCTOU verification):
/// the plan fds stay pinned while the pathnames underneath may be swapped;
/// the sandbox binds the pinned inodes regardless.
#[cfg(all(unix, test))]
pub(super) fn plan_spawn(
    plan: &BwrapPlan,
    program: &[std::ffi::OsString],
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new("bwrap");
    cmd.args(&plan.args);
    cmd.args(program);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let numbers = plan.numbers.clone();
    if !numbers.is_empty() {
        // SAFETY: pre_exec runs in the forked child before exec and only
        // clears CLOEXEC on the plan fds (async-signal-safe fcntl).
        unsafe { cmd.pre_exec(clear_cloexec_pre_exec(numbers)) };
    }
    cmd.spawn()
}

/// Wrap a shell invocation in bwrap per the resolved policy, returning the
/// command and the descriptor-pinned plan. The caller must keep the plan
/// alive until the spawn.
#[cfg(unix)]
pub(super) fn wrap_bash_command(
    shell: &Shell,
    workspace: &Workspace,
    command: &str,
    protect_git: bool,
    sandbox: &crate::config::Sandbox,
) -> Result<(Command, Option<BwrapPlan>), String> {
    let root = workspace.root();
    let root_str = root.to_string_lossy().into_owned();
    let plan = build_bwrap_plan(
        workspace,
        sandbox,
        protect_git,
        sandbox.network,
        &root_str,
        None,
    )?;
    let mut cmd = Command::new("bwrap");
    cmd.args(&plan.args);
    cmd.arg(shell.executable.clone());
    cmd.args(shell.command_args(command));
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
    // The plan fds are CLOEXEC in the parent; clear CLOEXEC on exactly
    // these numbers in the forked child so bwrap can bind them. Nothing
    // else ever inherits them.
    let numbers = plan.numbers.clone();
    if !numbers.is_empty() {
        // SAFETY: pre_exec runs in the forked child before exec; the
        // closure only touches the plan fds with async-signal-safe fcntl.
        unsafe { cmd.pre_exec(clear_cloexec_pre_exec(numbers)) };
    }
    Ok((cmd, Some(plan)))
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

    // Build the command: bare bash, or wrapped in bwrap when sandboxed (the
    // policy lives in `build_bwrap_plan`, shared with run_rust; network
    // defaults on). The plan's descriptor pins must stay alive until the
    // spawn below, so the plan is held here (never dropped with the match
    // arm) — for foreground and background calls alike.
    #[cfg(unix)]
    let (mut process, _plan) = match sandbox {
        Some(sandbox) => wrap_bash_command(shell, workspace, command, protect_git, sandbox)?,
        None => {
            let mut cmd = Command::new(&shell.executable);
            cmd.args(shell.command_args(command));
            (cmd, None)
        }
    };
    #[cfg(not(unix))]
    let mut process = {
        let mut cmd = Command::new(&shell.executable);
        cmd.args(shell.command_args(command));
        cmd
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

/// Per-stream capture budget `(head, tail, output cap, full cap)`.
pub(super) type CaptureLimits = (usize, usize, usize, usize);

pub(super) const BASH_CAPTURE: CaptureLimits = (HEAD_LIMIT, TAIL_LIMIT, OUTPUT_LIMIT, FULL_LIMIT);

/// run_rust per-stage capture: first 24 KiB and last 8 KiB per stream kept.
pub(super) const RUST_CAPTURE: CaptureLimits = (24 * 1024, 8 * 1024, 32 * 1024, FULL_LIMIT);

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
    reader: impl AsyncRead + Unpin,
    slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
) -> std::io::Result<Captured> {
    capture_with(reader, slot, spool, BASH_CAPTURE).await
}

pub(super) async fn capture_with(
    mut reader: impl AsyncRead + Unpin,
    slot: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
    limits: CaptureLimits,
) -> std::io::Result<Captured> {
    let (head_limit, tail_limit, output_limit, full_limit) = limits;
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
        // Full output for potential failure persistence, capped at full_limit.
        if captured.full.len() < full_limit {
            let room = full_limit - captured.full.len();
            captured.full.extend_from_slice(&data[..count.min(room)]);
        }
        if captured.bytes.len() < head_limit {
            let room = head_limit - captured.bytes.len();
            captured.bytes.extend_from_slice(&data[..count.min(room)]);
        }
        // Tail: rolling window of the last tail_limit bytes.
        captured.tail.extend_from_slice(data);
        if captured.tail.len() > tail_limit {
            let excess = captured.tail.len() - tail_limit;
            captured.tail.drain(..excess);
        }
        captured.truncated |= captured.total > output_limit;
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
pub(super) fn render_stream(captured: &Captured) -> String {
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
