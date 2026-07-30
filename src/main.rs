use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::path::Path;

use anyhow::{Context, anyhow};
use e_agent::agent::{Agent, AgentEvent, preview};
use e_agent::codex::CodexModel;
use e_agent::codex_auth::{CodexAuth, login, logout};
use e_agent::config::Config;
use e_agent::config::{AuthMode, ResolvedModel};
use e_agent::delegate::Delegate;
use e_agent::mcp;
use e_agent::model::{ConfiguredModel, OpenAiModel};
use e_agent::session::Session;
use e_agent::session_store::SessionStore;
use e_agent::tools::builtins;
use e_agent::tui;
use e_agent::workspace::Workspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(error) = run().await {
        eprintln!("e-agent: {error:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let raw_arguments: Vec<String> = std::env::args().skip(1).collect();
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
                    "usage: e-agent login|logout\n       e-agent [--profile PROFILE] [--base-url URL] [--model MODEL] [--workspace PATH] [--session|-s ID] [--max-rounds N] [--repl] [PROMPT]\n\nwithout --session a fresh unique session id is created every launch;\npass --session <id> to resume it (ids print on startup)"
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
    let skills_instructions = read_skills(&root)?;
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
    let mut persisted = agent.history().len();
    if legacy {
        store.rewrite(&root, &session, agent.history()).await?;
    }

    if let Some(window) = main_context_window {
        agent.set_context_window(window);
    }

    if tui_mode {
        return tui::run(
            agent,
            root,
            session,
            store,
            persisted,
            background,
            subagent_sessions,
            model_name,
            role_name,
            main_context_window,
        )
        .await;
    }
    set_stderr_events(&mut agent);
    if repl_mode {
        return repl(agent, root, session, store, persisted).await;
    }
    let answer = run_and_save(&mut agent, &store, &root, &session, &mut persisted, prompt).await?;
    println!("{answer}");
    if !agent.background_task_ids().is_empty() {
        eprintln!(
            "background tasks {:?} still running; results will be delivered in the next session's first turn",
            agent.background_task_ids()
        );
    }
    Ok(())
}

fn set_stderr_events(agent: &mut Agent) {
    let mut streaming = false;
    let mut reasoning = false;
    agent.set_event_handler(Box::new(move |event| match event {
        // Steering prompts are recorded by the session handle; the stderr
        // frontend never sends any, so nothing to print.
        AgentEvent::UserPrompt(_) => {}
        AgentEvent::Notice(text) => {
            eprintln!("{}", text);
        }
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
        AgentEvent::BackgroundCompleted { id, output } => {
            eprintln!("background task {id} finished: {}", preview(&output, 500))
        }
        AgentEvent::BackgroundCompletionNotice { id: _, output } => {
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
    }));
}

async fn run_and_save(
    agent: &mut Agent,
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    persisted: &mut usize,
    prompt: String,
) -> anyhow::Result<String> {
    let result = agent.run(prompt).await;
    store
        .append(root, session, &agent.history()[*persisted..])
        .await?;
    *persisted = agent.history().len();
    result
}

async fn repl(
    mut agent: Agent,
    root: std::path::PathBuf,
    session: String,
    store: SessionStore,
    mut persisted: usize,
) -> anyhow::Result<()> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let stdin = std::io::stdin();
    loop {
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
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            break;
        }
        if line == "/compact" {
            let result = agent.compact().await;
            store
                .append(&root, &session, &agent.history()[persisted..])
                .await?;
            persisted = agent.history().len();
            match result {
                Ok(summary) => println!("compacted: {summary}"),
                Err(error) => eprintln!("e-agent: {error:#}"),
            }
            continue;
        }
        agent.subscribe(sender.clone());
        match run_and_save(
            &mut agent,
            &store,
            &root,
            &session,
            &mut persisted,
            line.to_owned(),
        )
        .await
        {
            Ok(answer) => println!("{answer}"),
            Err(error) => eprintln!("e-agent: {error:#}"),
        }
        while let Ok(AgentEvent::BackgroundCompleted { id, output }) = receiver.try_recv() {
            eprintln!("background task {id} finished: {}", preview(&output, 500));
        }
    }
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
fn read_skills(root: &Path) -> anyhow::Result<Option<String>> {
    let skills_dir = root.join(".e-agent").join("skills");

    let dir = match std::fs::read_dir(&skills_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("cannot read .e-agent/skills"),
    };

    let mut skills: Vec<(String, String)> = Vec::new();

    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Err(e).context("cannot read .e-agent/skills entry"),
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
                return Err(e)
                    .context(format!("cannot read .e-agent/skills/{skill_name}/SKILL.md"));
            }
        }
    }

    if skills.is_empty() {
        return Ok(None);
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));

    let combined = skills
        .into_iter()
        .map(|(name, content)| format!("## Skill: {name}\n\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(Some(combined))
}

fn next_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    arguments
        .next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
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
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn test_read_skills_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_skills(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_read_skills_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".e-agent/skills")).unwrap();
        assert_eq!(read_skills(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_read_skills_ignores_non_directories() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".e-agent/skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // A regular file (not a dir) inside skills — must be skipped
        fs::write(skills_dir.join("not_a_dir"), "content").unwrap();
        assert_eq!(read_skills(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_read_skills_missing_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".e-agent/skills/my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        // No SKILL.md inside — silently skipped
        assert_eq!(read_skills(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_read_skills_empty_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".e-agent/skills/my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "").unwrap();
        assert_eq!(read_skills(dir.path()).unwrap(), None);

        // Whitespace-only is also empty
        let skill_dir2 = dir.path().join(".e-agent/skills/other-skill");
        fs::create_dir_all(&skill_dir2).unwrap();
        fs::write(skill_dir2.join("SKILL.md"), "   \n  \t  ").unwrap();
        assert_eq!(read_skills(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_read_skills_multiple_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".e-agent/skills");

        for (name, content) in [
            ("z-skill", "last skill content"),
            ("a-skill", "first skill content"),
            ("m-skill", "middle skill content"),
        ] {
            let d = skills_dir.join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("SKILL.md"), content).unwrap();
        }

        let result = read_skills(dir.path()).unwrap().unwrap();
        // a-skill first, then m-skill, then z-skill
        assert_eq!(
            result,
            "## Skill: a-skill\n\nfirst skill content\n\n\
             ## Skill: m-skill\n\nmiddle skill content\n\n\
             ## Skill: z-skill\n\nlast skill content"
        );
    }

    #[test]
    fn test_read_skills_keeps_content_intact() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".e-agent/skills/with-newlines");
        fs::create_dir_all(&d).unwrap();
        let content = "line 1\nline 2\n\nline 4";
        fs::write(d.join("SKILL.md"), content).unwrap();

        let result = read_skills(dir.path()).unwrap().unwrap();
        assert_eq!(result, format!("## Skill: with-newlines\n\n{content}"));
    }

    #[test]
    fn test_read_skills_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".e-agent/skills/bad-utf8");
        fs::create_dir_all(&d).unwrap();
        let bad = d.join("SKILL.md");
        let mut f = fs::File::create(&bad).unwrap();
        f.write_all(&[0xff, 0xfe, 0x00]).unwrap();
        drop(f);

        let err = read_skills(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("bad-utf8/SKILL.md"),
            "expected path context in error, got: {msg}"
        );
        assert!(
            msg.contains("UTF-8"),
            "expected 'UTF-8' in error, got: {msg}"
        );
    }
}
