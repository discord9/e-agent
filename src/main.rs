use std::backtrace::Backtrace;
use std::ffi::OsStr;
use std::fmt;
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use e_agent::agent::{Agent, AgentEvent, preview};
use e_agent::codex::CodexModel;
use e_agent::codex_auth::{CodexAuth, login, logout};
use e_agent::config::Config;
use e_agent::config::{AuthMode, ResolvedModel};
use e_agent::delegate::Delegate;
use e_agent::mcp;
use e_agent::model::{ConfiguredModel, OpenAiModel};
use e_agent::runner::{IdlePolicy, SessionHandle, SessionResult, SessionRunner, SessionStatus};
use e_agent::session::Session;
use e_agent::session_store::SessionStore;
use e_agent::tools::builtins;
use e_agent::tui;
use e_agent::workspace::Workspace;

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
        eprintln!("e-agent: {error:#}");
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
    let mut max_rounds = None;
    let mut repl_mode = false;
    let mut prompt = Vec::new();
    let mut arguments = raw_arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--base-url" => base_url = Some(next_value(&mut arguments, "--base-url")?),
            "--model" => model = Some(next_value(&mut arguments, "--model")?),
            "--profile" => profile = Some(next_value(&mut arguments, "--profile")?),
            "--workspace" => workspace = Some(next_value(&mut arguments, "--workspace")?),
            "--session" | "-s" => session = Some(next_value(&mut arguments, "--session")?),
            "--max-rounds" => {
                max_rounds = Some(
                    next_value(&mut arguments, "--max-rounds")?
                        .parse::<usize>()
                        .context("--max-rounds must be a positive integer")?,
                )
            }
            "--repl" => repl_mode = true,
            "--help" | "-h" => {
                println!(
                    "usage: e-agent --version|-V\n       e-agent login|logout\n       e-agent [--profile PROFILE] [--base-url URL] [--model MODEL] [--workspace PATH] [--session|-s ID] [--max-rounds N] [--repl] [PROMPT]\n\nwithout --session a fresh unique session id is created every launch;\npass --session <id> to resume it (ids print on startup)"
                );
                return Ok(());
            }
            value => prompt.push(value.to_owned()),
        }
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
    let workspace = Workspace::new(match workspace {
        Some(path) => path.into(),
        None => std::env::current_dir()?,
    })
    .map_err(anyhow::Error::msg)
    .context("cannot open workspace")?;
    let root = workspace.root().to_path_buf();
    let agents_instructions = read_agents(&root)?;
    let skills_instructions = read_skills_merged(&root)?;
    let config = Config::load()?;
    let backend = config
        .as_ref()
        .map(|c| c.session_backend())
        .unwrap_or_default();
    // Migrate pre-session-id files only when using the JSONL backend;
    // GreptimeDB has its own session namespace and does not need file
    // migration.
    if matches!(backend, e_agent::config::SessionBackend::Jsonl) {
        for (old, new) in e_agent::session::migrate_legacy(&root) {
            eprintln!("e-agent: migrated session {old} -> {new}");
        }
    }
    let session = match session {
        Some(name) => {
            e_agent::session::validate_session_name(&name)?;
            name
        }
        None => {
            let id = e_agent::session::new_id();
            // TUI mode shows the id in the input border instead (printing
            // here would pollute the screen before the alternate screen).
            if !tui_mode {
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
    // Web search reads EXA_API_KEY from the process env (tools.rs and
    // subagents pick it up there). When unset, fall back to the `[web_search]`
    // config section by injecting it into the env once at startup — this keeps
    // the key's single transport mechanism and avoids threading it through
    // every tools constructor. Startup is single-threaded, so set_var is safe.
    if std::env::var_os("EXA_API_KEY").is_none()
        && let Some(config) = &config
        && let Some(key) = config.web_search_key()?
    {
        unsafe { std::env::set_var("EXA_API_KEY", key) };
    }
    let store = SessionStore::connect(&backend, &root, &session).await?;
    let (main_resolved, role_resolved, all_roles) = match &config {
        Some(config) => (
            Some(config.resolve(profile.as_deref())?),
            config
                .resolve_role("subagent")
                .context("cannot resolve [roles] subagent profile")?,
            config
                .resolve_roles()
                .context("cannot resolve [roles] profiles")?,
        ),
        None => (None, None, std::collections::HashMap::new()),
    };
    let mut main_context_window = main_resolved.as_ref().and_then(|r| r.context_window);
    // When --model overrides the profile's wire model, the profile's
    // context window is no longer valid for the unknown model.
    let model_override = model.is_some();
    if matches!(
        main_resolved.as_ref().map(|value| value.auth),
        Some(AuthMode::ChatGpt)
    ) && base_url.is_some()
    {
        return Err(anyhow!(
            "--base-url cannot be used with a provider using auth = `chatgpt`"
        ));
    }
    let needs_chatgpt = main_resolved
        .as_ref()
        .is_some_and(|value| value.auth == AuthMode::ChatGpt)
        || all_roles
            .values()
            .any(|value| value.auth == AuthMode::ChatGpt);
    let auth = needs_chatgpt.then(CodexAuth::load).transpose()?;
    let model = match main_resolved {
        Some(configured) => configured_model(configured, auth.as_ref(), base_url, model)?,
        None => {
            if profile.is_some() {
                return Err(anyhow!(
                    "--profile requires a config file at $XDG_CONFIG_HOME/e-agent/config.toml or $HOME/.config/e-agent/config.toml"
                ));
            }
            ConfiguredModel::chat(OpenAiModel::from_env(base_url, model)?)
        }
    };
    if model_override {
        main_context_window = None;
    }
    let model_name = model.display_name().to_owned();
    let subagent_context_window = role_resolved.as_ref().and_then(|r| r.context_window);
    let subagent_model = role_resolved
        .map(|resolved| configured_model(resolved, auth.as_ref(), None, None))
        .transpose()?;
    let mut role_models = std::collections::HashMap::new();
    let mut role_context_windows = std::collections::HashMap::new();
    for (role, resolved) in all_roles {
        role_context_windows.insert(role.clone(), resolved.context_window);
        role_models.insert(role, configured_model(resolved, auth.as_ref(), None, None)?);
    }
    // The bash sandbox ([sandbox] in config) wraps every bash call — main
    // agent and subagents alike — in bwrap when enabled.
    let sandbox = config.as_ref().and_then(|config| config.sandbox(&root));
    if sandbox.is_some() {
        if !e_agent::tools::bwrap_available() {
            return Err(anyhow!(
                "[sandbox] enabled = true but bwrap is not available. \
                 Install bubblewrap or disable the sandbox."
            ));
        }
        if !tui_mode {
            eprintln!("e-agent: bash sandboxed with bwrap");
        }
    }
    let (mut tools, background) = builtins(workspace.clone(), sandbox.clone());
    let mcp_servers = config
        .as_ref()
        .map(|config| config.mcp.clone())
        .unwrap_or_default();
    let (mcp_tools, mcp_instructions) = mcp::connect_all(mcp_servers, &root).await;
    tools.extend(mcp_tools);
    let mut delegate = Delegate::new(model.clone(), workspace, background.clone())
        .persist_sessions(root.clone())
        .with_role_models(role_models)
        .with_role_context_windows(role_context_windows)
        .with_subagent_context_window(subagent_context_window)
        .with_roles_root(root.clone())
        .with_sandbox(sandbox)
        .record_background_tasks_in(root.clone(), &session)
        .with_persist_store(backend);
    if let Some(subagent_model) = subagent_model {
        let name = subagent_model.display_name().to_owned();
        delegate = delegate.with_subagent_model(subagent_model);
        if !tui_mode {
            eprintln!("e-agent: subagent model {name}");
        }
    }
    let subagent_sessions = delegate.sessions();
    tools.push(Box::new(delegate));
    let mut agent = Agent::new(Box::new(model), tools);
    let mut context = Vec::new();
    // The main agent's orchestrator template (.e-agent/agents/main.md) leads;
    // it tells the model to decompose work and delegate to the named roles.
    let role_name = match e_agent::roles::role_prompt(&root, e_agent::roles::MAIN_ROLE)? {
        Some(orchestrator) => {
            context.push(orchestrator);
            Some(e_agent::roles::MAIN_ROLE.to_owned())
        }
        None => None,
    };
    if let Some(instructions) = agents_instructions {
        context.push(format!("## AGENTS.md\n\n{instructions}"));
    }
    if let Some(skills) = skills_instructions {
        context.push(skills);
    }
    context.extend(mcp_instructions);
    if !context.is_empty() {
        agent.set_context_prefix(context.join("\n\n"));
    }
    if let Some(rounds) = max_rounds {
        agent = agent.max_tool_rounds(rounds);
    }
    let loaded = store.load(&root, &session).await?;
    let legacy = loaded.legacy;
    agent.restore_history(loaded.entries);
    agent.record_background_tasks_in(root.clone(), &session);
    let unfinished = Session::take_unfinished_background(&root, &session);
    if !unfinished.is_empty() {
        let notice = format!(
            "[e-agent exited with {} background task(s) still running; they were killed with the process. Re-run them if still needed:]\n{}",
            unfinished.len(),
            unfinished.join("\n")
        );
        let entry = e_agent::agent::SessionEntry::Notice {
            text: notice.clone(),
        };
        // Persist immediately so a crash-before-first-turn cannot inject
        // the same notice again on the next launch.
        store
            .append(&root, &session, std::slice::from_ref(&entry))
            .await?;
        // Append (NOT restore_history, which would wipe the resumed history).
        agent.push_entry(entry);
    }
    if legacy {
        store.rewrite(&root, &session, agent.history()).await?;
    }

    if let Some(window) = main_context_window {
        agent.set_context_window(window);
    }

    if tui_mode {
        let (runner, handle) = SessionRunner::new(
            agent,
            store,
            root.clone(),
            session.clone(),
            IdlePolicy::WaitForInput,
        );
        let task = runner.start(None);
        let result = tui::run(
            handle,
            task,
            root,
            session,
            background,
            subagent_sessions,
            model_name,
            role_name,
            main_context_window,
        )
        .await;
        _tui_report.success = result.is_ok();
        return result;
    }
    let policy = if repl_mode {
        IdlePolicy::WaitForInput
    } else {
        IdlePolicy::FinishWhenIdle
    };
    let (runner, handle) = SessionRunner::new(agent, store, root, session, policy);
    let task = runner.start((!repl_mode).then_some(prompt));
    if repl_mode {
        repl(handle, task).await
    } else {
        let (_, events, status) = handle.attach();
        let render = tokio::spawn(consume_stderr_events(events));
        task.join().await?;
        let result = status.borrow().clone();
        drop(handle);
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
            AgentEvent::BackgroundCompletionNotice {
                id: _,
                output,
                label: _,
                ..
            } => {
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
                session,
            } => eprintln!(
                "\x1b[2mctx {}, session {} in / {} out\x1b[0m",
                context_input, session.input_tokens, session.output_tokens
            ),
        }
    }
}

async fn repl(handle: SessionHandle, task: e_agent::runner::SessionTask) -> anyhow::Result<()> {
    let (_, events, mut status) = handle.attach();
    let render = tokio::spawn(consume_stderr_events(events));
    let stdin = std::io::stdin();
    loop {
        while !matches!(*status.borrow(), SessionStatus::Idle) {
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
        match line.trim() {
            "" => {}
            "/exit" | "/quit" => break,
            "/compact" => {
                handle.compact();
                status.changed().await?;
            }
            prompt => {
                handle.prompt(prompt);
                status.changed().await?;
            }
        }
    }
    drop(handle);
    drop(task);
    render.abort();
    Ok(())
}

fn read_agents(root: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(root.join("AGENTS.md")) {
        Ok(content) if content.trim().is_empty() => Ok(None),
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("cannot read workspace AGENTS.md"),
    }
}

/// Read all workspace skills from `.e-agent/skills/<name>/SKILL.md`.
///
/// Returns `None` (silently) when the directory is missing, empty, or contains
/// only non-directories / missing / empty SKILL.md files. Actual I/O or UTF-8
/// errors on a SKILL.md that should be readable are returned with a path
/// context.
///
/// Skills are sorted by `<name>` (dictionary order, stable) and joined as a
/// single block prefixed with `## Skill: <name>` per skill.
/// Scan a single skill directory and return (name, content) pairs.
///
/// Missing dir, non-directory entries, missing/empty SKILL.md are silently
/// skipped. I/O/UTF-8 errors on a readable SKILL.md bubble up with path
/// context.
fn read_skills_from(dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let dir_entries = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context(format!("cannot read {}", dir.display())),
    };

    let mut skills: Vec<(String, String)> = Vec::new();

    for entry in dir_entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Err(e).context(format!("cannot read {} entry", dir.display())),
        };

        // Only directories are candidate skill folders
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }

        let skill_name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };

        let skill_path = entry.path().join("SKILL.md");

        match std::fs::read_to_string(&skill_path) {
            Ok(content) => {
                if !content.trim().is_empty() {
                    skills.push((skill_name, content));
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).context(format!("cannot read {}", skill_path.display()));
            }
        }
    }

    Ok(skills)
}

/// Merge skills from `global_dir` (e.g. `Config::config_dir()/skills/`) and
/// `workspace_dir` (`.e-agent/skills/`).  Workspace entries override same-name
/// globals.  Returns `None` silently when both are missing/empty.
fn read_skills_merge(
    global_dir: Option<&Path>,
    workspace_dir: &Path,
) -> anyhow::Result<Option<String>> {
    let mut merged: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(global) = global_dir {
        for (name, content) in read_skills_from(global)? {
            merged.insert(name, content);
        }
    }
    for (name, content) in read_skills_from(workspace_dir)? {
        merged.insert(name, content);
    }
    if merged.is_empty() {
        return Ok(None);
    }
    let mut skills: Vec<_> = merged.into_iter().collect();
    skills.sort_by(|a, b| a.0.cmp(&b.0));
    let combined = skills
        .into_iter()
        .map(|(name, content)| format!("## Skill: {name}\n\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Some(combined))
}

/// Production entry: global from `Config::config_dir()/skills/`, workspace
/// from `<root>/.e-agent/skills/`.
fn read_skills_merged(root: &Path) -> anyhow::Result<Option<String>> {
    let global = e_agent::config::config_dir().map(|d| d.join("skills"));
    let workspace = root.join(".e-agent").join("skills");
    read_skills_merge(global.as_deref(), &workspace)
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
        std::env::var_os("HOME").as_deref(),
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

fn configured_model(
    resolved: ResolvedModel,
    auth: Option<&CodexAuth>,
    base_url: Option<String>,
    model: Option<String>,
) -> anyhow::Result<ConfiguredModel> {
    let display = Some(resolved.display);
    match resolved.auth {
        AuthMode::ApiKey => {
            let mut cm = ConfiguredModel::chat(OpenAiModel::new(
                base_url.unwrap_or(resolved.base_url),
                resolved.api_key,
                model.unwrap_or(resolved.model),
                resolved.reasoning_effort,
            )?);
            cm.display = display;
            Ok(cm)
        }
        AuthMode::ChatGpt => {
            let mut cm = ConfiguredModel::codex(CodexModel::new(
                auth.cloned()
                    .ok_or_else(|| anyhow!("ChatGPT auth was not initialized"))?,
                model.unwrap_or(resolved.model),
                resolved.reasoning_effort,
            )?);
            cm.display = display;
            Ok(cm)
        }
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
