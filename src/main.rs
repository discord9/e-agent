use std::backtrace::Backtrace;
use std::ffi::OsStr;
use std::fmt;
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use e_agent::agent::{AgentEvent, ImagePart, preview};
use e_agent::codex_auth::{login, logout};
use e_agent::runner::{IdlePolicy, SessionHandle, SessionResult, SessionStatus, SessionTask};
use e_agent::session_factory::{SessionFactory, UnfinishedPolicy};
use e_agent::tui;

const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw_arguments: Vec<String> = std::env::args().skip(1).collect();
    match version_requested(&raw_arguments) {
        Ok(true) => {
            println!("{}", version_line());
            return Ok(());
        }
        Err(error) => {
            eprintln!("e-agent: {error:#}");
            std::process::exit(1);
        }
        Ok(false) => {}
    }
    install_panic_hook();
    notify_crash_if_exists();
    if let Err(error) = run(raw_arguments).await {
        let text = format!("{error:#}");
        if let Some(friendly) = friendly_failure(&text) {
            eprintln!("{friendly}");
        } else {
            eprintln!("e-agent: {text}");
        }
        std::process::exit(1);
    }
    Ok(())
}

fn version_line() -> String {
    format!("e-agent {BUILD_VERSION}")
}

fn version_requested(arguments: &[String]) -> anyhow::Result<bool> {
    let Some(flag) = arguments
        .iter()
        .find(|argument| matches!(argument.as_str(), "--version" | "-V"))
    else {
        return Ok(false);
    };
    if arguments.len() != 1 {
        return Err(anyhow!("e-agent {flag} does not accept arguments"));
    }
    Ok(true)
}

/// Whether the invocation requests a read-only session (`--read-only`): the
/// same policy as a read-only role — no write/edit tools, no MCP tools, and
/// a narrowed read-only bash sandbox (no bash at all without the sandbox).
fn read_only_requested(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| argument == "--read-only")
}

/// Recognize GreptimeDB concurrent-write conflicts (backend message carries
/// the fixed `concurrent write conflict` substring) and render a friendly
/// multi-line Chinese hint with the raw English detail kept below. Returns
/// `None` for any other text so callers print it unchanged.
fn friendly_failure(text: &str) -> Option<String> {
    if !text.contains("concurrent write conflict") {
        return None;
    }
    Some(
        [
            "会话被其他客户端占用，已停止写入以避免数据冲突。".to_owned(),
            "请关闭另一个 TUI / Web 窗口 / 导入工具后再试，或新开会话继续。".to_owned(),
            format!("详情: {text}"),
        ]
        .join("\n"),
    )
}

async fn run(raw_arguments: Vec<String>) -> anyhow::Result<()> {
    if let Some(command) = raw_arguments
        .first()
        .filter(|value| *value == "login" || *value == "logout")
    {
        if raw_arguments.len() != 1 {
            return Err(anyhow!("e-agent {command} does not accept arguments"));
        }
        if command == "login" {
            login().await?;
            println!("ChatGPT login saved.");
        } else {
            logout()?;
            println!("ChatGPT login removed.");
        }
        return Ok(());
    }
    let mut base_url = None;
    let mut model = None;
    let mut profile = None;
    let mut workspace = None;
    let mut session: Option<String> = None;
    let mut fork: Option<String> = None;
    let mut at: Option<usize> = None;
    let mut max_rounds = None;
    let mut repl_mode = false;
    let mut serve_mode = false;
    let mut host = None;
    let mut port = None;
    let mut prompt = Vec::new();
    let read_only = read_only_requested(&raw_arguments);
    let mut raw_arguments = raw_arguments;
    // `e-agent web` subcommand: a first non-empty positional "web" selects
    // headless serve mode, equivalent to `--serve`.
    if let Some(position) = raw_arguments
        .iter()
        .position(|argument| !argument.is_empty())
        && raw_arguments[position] == "web"
    {
        raw_arguments.remove(position);
        serve_mode = true;
    }
    let mut arguments = raw_arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--base-url" => base_url = Some(next_value(&mut arguments, "--base-url")?),
            "--model" => model = Some(next_value(&mut arguments, "--model")?),
            "--profile" => profile = Some(next_value(&mut arguments, "--profile")?),
            "--workspace" => workspace = Some(next_value(&mut arguments, "--workspace")?),
            "--session" | "-s" => session = Some(next_value(&mut arguments, "--session")?),
            "--fork" => fork = Some(next_value(&mut arguments, "--fork")?),
            "--at" => {
                let value = next_value(&mut arguments, "--at")?;
                let parsed = value.parse::<usize>().ok().filter(|n| *n > 0);
                at = Some(parsed.with_context(|| {
                    format!("--at must be a 1-based entry index, got {value:?}")
                })?);
            }
            "--max-rounds" => {
                max_rounds = Some(
                    next_value(&mut arguments, "--max-rounds")?
                        .parse::<usize>()
                        .context("--max-rounds must be a positive integer")?,
                )
            }
            "--repl" => repl_mode = true,
            "--serve" => serve_mode = true,
            "--host" => host = Some(next_value(&mut arguments, "--host")?),
            "--port" | "-p" => {
                port = Some(
                    next_value(&mut arguments, &argument)?
                        .parse::<u16>()
                        .context("--port must be a number between 0 and 65535")?,
                )
            }
            "--read-only" => {} // consumed via read_only_requested above
            "--help" | "-h" => {
                println!(
                    "usage: e-agent --version|-V\n       e-agent login|logout\n       e-agent --serve [--host ADDR] [--port PORT] [--profile PROFILE] [--base-url URL] [--model MODEL] [--workspace PATH] [--read-only]\n       e-agent web [-p PORT] [--host ADDR] [--profile PROFILE] [--base-url URL] [--model MODEL] [--workspace PATH] [--read-only]\n       e-agent [--profile PROFILE] [--base-url URL] [--model MODEL] [--workspace PATH] [--session|-s ID] [--fork SESSION] [--at N] [--max-rounds N] [--read-only] [--repl] [PROMPT]\n\nwithout --session a fresh unique session id is created every launch;\npass --session <id> to resume it (ids print on startup);\npass --fork <id> to start a new session from a completed turn of an existing one\n(--at N forks at the N-th entry, 1-based and inclusive, and must be a turn boundary);\n--read-only applies the read-only role policy to the main session only (no write/edit tools, no MCP tools, narrowed bash sandbox); delegated subagents keep their full default toolset and can write — give their role template read_only = true to make them read-only too;\nthe web subcommand starts a headless HTTP server (default http://127.0.0.1:8766);\n--serve runs a headless HTTP server (default http://127.0.0.1:8766) with a token-authenticated /api and a web UI"
                );
                return Ok(());
            }
            value => prompt.push(value.to_owned()),
        }
    }
    if at.is_some() && fork.is_none() {
        return Err(anyhow!("--at requires --fork <session-id>"));
    }
    if fork.is_some() && session.is_some() {
        return Err(anyhow!(
            "--fork cannot be combined with --session (the forked session gets a new id)"
        ));
    }
    // Headless mode: build the process-global factory once and hand it to
    // the HTTP server, which builds sessions on demand. Everything below
    // (TUI/REPL/stdin prompt handling) is skipped.
    if serve_mode {
        let factory = SessionFactory::new(
            match workspace {
                Some(path) => path.into(),
                None => std::env::current_dir()?,
            },
            profile.as_deref(),
            base_url,
            model,
            read_only,
            true,
        )?;
        return e_agent::server::run(
            factory,
            host.as_deref().unwrap_or("127.0.0.1"),
            port.unwrap_or(8766),
        )
        .await;
    }
    let tui_mode = prompt.is_empty() && std::io::stdout().is_terminal() && !repl_mode;
    let prompt = if prompt.is_empty() && !tui_mode && !repl_mode {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        input
    } else {
        prompt.join(" ")
    };
    if !tui_mode && !repl_mode && prompt.trim().is_empty() {
        return Err(anyhow!("a prompt argument or stdin content is required"));
    }
    // Resolve all process-global startup state once: workspace, config,
    // models, sandbox. Per-session construction happens in
    // SessionFactory::build.
    let factory = SessionFactory::new(
        match workspace {
            Some(path) => path.into(),
            None => std::env::current_dir()?,
        },
        profile.as_deref(),
        base_url,
        model,
        read_only,
        !tui_mode,
    )?;
    let session = match session {
        Some(name) => {
            e_agent::session::validate_session_name(&name)?;
            name
        }
        None => {
            let id = e_agent::session::new_id();
            // TUI mode shows the id in the input border instead (printing
            // here would pollute the screen before the alternate screen).
            // A --fork run replaces this placeholder with the forked id, so
            // do not announce it.
            if !tui_mode && fork.is_none() {
                eprintln!("e-agent: session {id}");
            }
            id
        }
    };
    // TUI-mode session-report guard: prints session info to stderr when
    // the function returns (regardless of Ok/Err).  Non-TUI sessions set
    // session=None so the guard is a no-op.
    let mut _tui_report = TuiSessionReport {
        session: if tui_mode {
            Some(session.clone())
        } else {
            None
        },
        success: false,
    };
    let fork_from = fork.map(|source| (source, at));
    let policy = if tui_mode || repl_mode {
        IdlePolicy::WaitForInput
    } else {
        IdlePolicy::FinishWhenIdle
    };
    let built = factory
        .build(
            &session,
            fork_from,
            max_rounds,
            policy,
            UnfinishedPolicy::Consume,
        )
        .await?;
    if tui_mode {
        // The fork (if any) happened inside build; report the effective id.
        _tui_report.session = Some(built.session.clone());
        let task = built.runner.start(None);
        // The btw fork record mirrors the server's /btw endpoint: the
        // parent session's background record (workspace root + effective
        // session id + its store) so btw tasks are reported as killed on
        // exit and carry the parent_session_id metadata link. Cloned
        // before the moved `built.session` / `built.store` args below.
        let record_in = Some(e_agent::session_store::BackgroundRecord {
            root: factory.root().to_path_buf(),
            session: built.session.clone(),
            store: built.store.clone(),
        });
        // `/model <profile>` resolves profiles at runtime through the same
        // factory; shadow the factory with an Arc so the TUI can hold it.
        let factory = std::sync::Arc::new(factory);
        let result = tui::run(
            built.handle,
            task,
            factory.root().to_path_buf(),
            built.session,
            built.background,
            built.sessions,
            built.model_name,
            built.role_name,
            factory.main_context_window(),
            built.store,
            factory.main_model().clone(),
            factory.workspace().clone(),
            factory.sandbox().cloned(),
            factory.backend().clone(),
            read_only,
            record_in,
            factory.clone(),
        )
        .await;
        _tui_report.success = result.is_ok();
        return result;
    }
    let task = built.runner.start((!repl_mode).then_some(prompt));
    if repl_mode {
        repl(built.handle, task).await
    } else {
        let (_, events, status) = built.handle.attach();
        let render = tokio::spawn(consume_stderr_events(events));
        task.join().await?;
        let result = status.borrow().clone();
        drop(built.handle);
        let _ = render.await;
        if let SessionStatus::Finished(SessionResult::Completed(Some(answer))) = result {
            println!("{answer}");
        }
        Ok(())
    }
}

/// Lines printed by TuiSessionReport Drop (pure for testability).
fn tui_report_lines(session: &str, success: bool) -> Vec<String> {
    let resume = format!("e-agent: resume with: e-agent --session {session}");
    if success {
        vec![resume]
    } else {
        vec![
            format!("e-agent: session {session} (the failed turn may not have been persisted)"),
            resume,
        ]
    }
}

/// TUI-mode guard: prints session info to stderr on drop (terminal restored).
/// Non-TUI sessions set session=None and the guard is a no-op.
struct TuiSessionReport {
    session: Option<String>,
    success: bool,
}

impl Drop for TuiSessionReport {
    fn drop(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        for line in tui_report_lines(session, self.success) {
            eprintln!("{line}");
        }
    }
}

async fn consume_stderr_events(mut events: tokio::sync::broadcast::Receiver<AgentEvent>) {
    let mut streaming = false;
    let mut reasoning = false;
    while let Ok(event) = events.recv().await {
        match event {
            AgentEvent::PromptQueued(_) | AgentEvent::PromptConsumed => {}
            AgentEvent::UserPrompt(_) => {}
            AgentEvent::Notice(text) => {
                eprintln!("{}", text);
            }
            AgentEvent::Error(text) => eprintln!("error: {text}"),
            AgentEvent::AssistantText(text) => eprintln!("assistant: {}", preview(&text, 500)),
            AgentEvent::AssistantDelta(text) => {
                if reasoning {
                    eprintln!();
                    reasoning = false;
                }
                eprint!("{text}");
                streaming = true;
            }
            AgentEvent::ReasoningDelta(text) => {
                eprint!("\x1b[2m{text}\x1b[0m");
                reasoning = true;
            }
            AgentEvent::ToolCall { name, arguments } => {
                if streaming || reasoning {
                    eprintln!();
                    streaming = false;
                    reasoning = false;
                }
                eprintln!("tool: {name} {}", preview(&arguments, 200))
            }
            AgentEvent::ToolResult { is_error, content } => eprintln!(
                "  {}: {}",
                if is_error { "error" } else { "ok" },
                preview(&content, 500)
            ),
            AgentEvent::BackgroundCompleted {
                id, output, label, ..
            } => {
                let title = label
                    .as_deref()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default();
                eprintln!(
                    "background task {id} finished{title}: {}",
                    preview(&output, 500)
                )
            }
            AgentEvent::BackgroundCompletionNotice { output, .. } => {
                // REPL/stderr: show a finite preview with middle ellipsis
                let lines: Vec<&str> = output.lines().collect();
                if lines.len() <= 8 && output.len() <= 500 {
                    for line in &lines {
                        eprintln!("  {line}");
                    }
                } else {
                    let head: Vec<&str> = lines.iter().take(5).copied().collect();
                    let tail: Vec<&str> = lines.iter().rev().take(3).rev().copied().collect();
                    let omitted = lines.len() - head.len() - tail.len();
                    for line in &head {
                        eprintln!("  {line}");
                    }
                    eprintln!(
                        "  … ({omitted} lines, {} chars omitted)",
                        output.len().saturating_sub(
                            head.iter().map(|l| l.len()).sum::<usize>()
                                + tail.iter().map(|l| l.len()).sum::<usize>()
                        )
                    );
                    for line in &tail {
                        eprintln!("  {line}");
                    }
                }
            }
            AgentEvent::Usage {
                context_input,
                context_window: _,
                session,
            } => eprintln!(
                "\x1b[2mctx {}, session {} in / {} out\x1b[0m",
                context_input, session.input_tokens, session.output_tokens
            ),
        }
    }
}

async fn repl(handle: SessionHandle, task: SessionTask) -> anyhow::Result<()> {
    let (_, events, mut status) = handle.attach();
    let render = tokio::spawn(consume_stderr_events(events));
    let stdin = std::io::stdin();
    // Image attached via `/image <path>`; rides along with the next prompt.
    let mut pending_image: Option<(String, ImagePart)> = None;
    loop {
        // Wait for the runner to become idle again. Finished is a terminal
        // state: the watch channel gets no further values, so waiting for
        // Idle would hang forever. Report a failure and end the REPL.
        while !matches!(*status.borrow(), SessionStatus::Idle) {
            if let SessionStatus::Finished(result) = &*status.borrow() {
                if let SessionResult::Failed(text) = result {
                    match friendly_failure(text) {
                        Some(friendly) => eprintln!("{friendly}"),
                        None => eprintln!("e-agent: session failed: {text}"),
                    }
                }
                drop(handle);
                drop(task);
                render.abort();
                return Ok(());
            }
            status.changed().await?;
        }
        print!("e-agent> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                eprintln!("e-agent: ignored invalid UTF-8 input");
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        let trimmed = line.trim();
        match trimmed {
            "" => {}
            "/exit" | "/quit" => break,
            "/compact" => {
                handle.compact();
                status.changed().await?;
            }
            command if command.starts_with("/image ") => {
                // Entrance A (human attaches an image): store it in the
                // global content-addressed store and attach it to the next
                // prompt. The placeholder line also reaches the model text.
                let path = command.trim_start_matches("/image").trim();
                match e_agent::agent::attach_image_from_path(path) {
                    Ok(part) => {
                        pending_image = Some((path.to_owned(), part));
                        eprintln!("[image attached: {path}]");
                    }
                    Err(error) => eprintln!("e-agent: {error}"),
                }
            }
            prompt => {
                if let Some((path, part)) = pending_image.take() {
                    handle.prompt_with_image(format!("{prompt}\n[image attached: {path}]"), part);
                } else {
                    handle.prompt(prompt);
                }
                status.changed().await?;
            }
        }
    }
    drop(handle);
    drop(task);
    render.abort();
    Ok(())
}

fn next_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    arguments
        .next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Panic crash diagnostics
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Resolve crash directory from env, pure for testability.
fn crash_dir_inner(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(x) = xdg
        && !x.is_empty()
    {
        Some(PathBuf::from(x).join("e-agent/crash"))
    } else {
        home.map(|h| PathBuf::from(h).join(".config/e-agent/crash"))
    }
}
fn crash_dir() -> Option<PathBuf> {
    crash_dir_inner(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        e_agent::home_dir()
            .as_deref()
            .map(std::path::Path::as_os_str),
    )
}

/// Build crash report string (pure, for testability).
fn format_crash_report(ts: u64, thread: &str, location: Option<&str>, bt: &str) -> String {
    let mut r = format!("timestamp: {ts}\nthread: {thread}\n");
    if let Some(loc) = location {
        r.push_str(&format!("location: {loc}\n"));
    }
    r.push_str("panic payload omitted\nbacktrace:\n");
    r.push_str(bt);
    if !bt.ends_with('\n') {
        r.push('\n');
    }
    r
}

/// Create the crash directory without changing permissions on its config parent.
///
/// The directory may already exist from an older version; in that case only the
/// `crash` directory itself is made private.
fn create_private_crash_dir(dir: &Path) -> Result<(), String> {
    let parent = dir
        .parent()
        .ok_or_else(|| format!("invalid crash directory: {}", dir.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create {}: {error}", dir.display())),
        }
        if !dir.is_dir() {
            return Err(format!("create {}: not a directory", dir.display()));
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("set permissions on {}: {error}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(dir)
            .or_else(|error| {
                if error.kind() == ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
        if !dir.is_dir() {
            return Err(format!("create {}: not a directory", dir.display()));
        }
    }
    Ok(())
}

fn open_private_crash_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn make_crash_file_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Atomically replace `latest.log` with a private crash report.
///
/// Errors are returned to the panic hook, which is deliberately responsible
/// for printing them without invoking any further error-handling machinery.
fn write_crash_report(dir: &Path, report: &str) -> Result<PathBuf, String> {
    create_private_crash_dir(dir)?;
    let latest = dir.join("latest.log");
    let tmp = latest.with_extension("tmp");
    let mut file = open_private_crash_file(&tmp)
        .map_err(|error| format!("create {}: {error}", tmp.display()))?;
    make_crash_file_private(&tmp)
        .map_err(|error| format!("set permissions on {}: {error}", tmp.display()))?;
    file.write_all(report.as_bytes())
        .map_err(|error| format!("write {}: {error}", tmp.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, &latest)
        .map_err(|error| format!("rename {} to {}: {error}", tmp.display(), latest.display()))?;
    Ok(latest)
}

/// Write a panic diagnostic without panicking again if stderr is unavailable.
fn panic_stderr(args: fmt::Arguments<'_>) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_fmt(args);
}

/// Install the global hook. Rust calls it even for a Tokio task panic that is
/// later observed as a `JoinError`, so this hook only restores the TUI on the
/// main thread. It reports all panics but does not decide process fatality.
fn install_panic_hook() {
    // Do not call the previous hook: it conditionally prints a backtrace based
    // on RUST_BACKTRACE, and would duplicate the forced stack below. The panic
    // payload remains deliberately omitted because it can contain secrets.
    let _previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned();
        if thread == "main" {
            // A main-thread panic is normally fatal. Restore before writing a
            // potentially long stack so it is visible outside the TUI.
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stderr(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::cursor::Show,
            );
        }

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let bt = Backtrace::force_capture();
        let backtrace = format!("{bt:#}");
        let report = format_crash_report(ts, &thread, location.as_deref(), &backtrace);
        let write_result = crash_dir().map(|dir| write_crash_report(&dir, &report));

        // Keep this self-contained and non-recursive: a hook can run while
        // unwinding, and a Tokio worker panic may be caught by its JoinHandle.
        let mut diagnostic =
            format!("e-agent: Rust panic on thread {thread} (panic payload omitted)\n");
        if let Some(location) = location {
            diagnostic.push_str(&format!("  location: {location}\n"));
        }
        if thread != "main" {
            diagnostic.push_str(
                "  note: a non-main Tokio task panic may be caught as a JoinError; this diagnostic alone is not a fatal application status.\n",
            );
        }
        diagnostic.push_str(&format!("  backtrace:\n{backtrace}"));
        if !diagnostic.ends_with('\n') {
            diagnostic.push('\n');
        }
        match write_result {
            Some(Ok(path)) => {
                diagnostic.push_str(&format!("e-agent: crash report: {}\n", path.display()))
            }
            Some(Err(error)) => {
                diagnostic.push_str(&format!("e-agent: crash report write failed: {error}\n"))
            }
            None => diagnostic
                .push_str("e-agent: crash report write failed: no XDG_STATE_HOME or HOME\n"),
        }
        panic_stderr(format_args!("{diagnostic}"));
    }));
}

/// Acknowledge a previous crash: print brief notice and rename to
/// `previous.log` so the notice is one-shot. Returns `true` if a crash
/// file was found and acknowledged.
fn acknowledge_crash(latest: &Path, previous: &Path) -> bool {
    let content = match std::fs::read_to_string(latest) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return false,
        Err(error) => {
            eprintln!(
                "e-agent: crash report read failed ({}): {error}",
                latest.display()
            );
            return false;
        }
    };
    let location = content
        .lines()
        .find_map(|line| line.strip_prefix("location: "))
        .unwrap_or("<unknown>");
    eprintln!("e-agent: previous crash at {location}");
    let report_path = match std::fs::rename(latest, previous) {
        Ok(()) => previous,
        Err(error) => {
            eprintln!(
                "e-agent: crash report rename failed ({} to {}): {error}",
                latest.display(),
                previous.display()
            );
            latest
        }
    };
    eprintln!("  crash report: {}", report_path.display());
    true
}

fn notify_crash_if_exists() {
    let Some(dir) = crash_dir() else { return };
    acknowledge_crash(&dir.join("latest.log"), &dir.join("previous.log"));
}

// The skill-scanning helpers moved into session_factory.rs (the factory's
// constructor owns workspace-content reading); re-exported here so the
// existing main_tests.rs tests keep exercising them unchanged.
#[cfg(test)]
pub use e_agent::session_factory::{read_skills_from, read_skills_merge};

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
