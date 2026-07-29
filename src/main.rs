use std::io::{IsTerminal, Read, Write};

use anyhow::{Context, anyhow};
use e_agent::agent::{Agent, AgentEvent, preview};
use e_agent::config::Config;
use e_agent::delegate::Delegate;
use e_agent::mcp;
use e_agent::model::OpenAiModel;
use e_agent::session::Session;
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
    let mut base_url = None;
    let mut model = None;
    let mut profile = None;
    let mut workspace = None;
    let mut session: Option<String> = None;
    let mut max_rounds = None;
    let mut repl_mode = false;
    let mut prompt = Vec::new();
    let mut arguments = std::env::args().skip(1);
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
                    "usage: e-agent [--profile PROFILE] [--base-url URL] [--model MODEL] [--workspace PATH] [--session|-s ID] [--max-rounds N] [--repl] [PROMPT]\n\nwithout --session a fresh unique session id is created every launch;\npass --session <id> to resume it (ids print on startup)"
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
    // Migrate pre-session-id files (the old implicit `default` session and
    // task-id-named subagent files) to unique ids before anything loads.
    for (old, new) in e_agent::session::migrate_legacy(&root) {
        eprintln!("e-agent: migrated session {old} -> {new}");
    }
    let session = match session {
        Some(name) => name,
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
    let config = Config::load()?;
    let model = match &config {
        Some(config) => {
            let configured = config.resolve(profile.as_deref())?;
            OpenAiModel::new(
                base_url.unwrap_or(configured.base_url),
                configured.api_key,
                model.unwrap_or(configured.model),
                configured.reasoning_effort,
            )?
        }
        None => {
            if profile.is_some() {
                return Err(anyhow!(
                    "--profile requires a config file at $XDG_CONFIG_HOME/e-agent/config.toml or $HOME/.config/e-agent/config.toml"
                ));
            }
            OpenAiModel::from_env(base_url, model)?
        }
    };
    let model_name = model.name().to_owned();
    let subagent_model = match &config {
        Some(config) => config
            .resolve_role("subagent")
            .context("cannot resolve [roles] subagent profile")?
            .map(|resolved| {
                OpenAiModel::new(
                    resolved.base_url,
                    resolved.api_key,
                    resolved.model,
                    resolved.reasoning_effort,
                )
            })
            .transpose()?,
        None => None,
    };
    let (mut tools, background) = builtins(workspace.clone());
    let mcp_servers = config.map(|config| config.mcp).unwrap_or_default();
    let (mcp_tools, mcp_instructions) = mcp::connect_all(mcp_servers, &root).await;
    tools.extend(mcp_tools);
    let mut delegate =
        Delegate::new(model.clone(), workspace, background.clone()).persist_sessions(root.clone());
    if let Some(subagent_model) = subagent_model {
        let name = subagent_model.name().to_owned();
        delegate = delegate.with_subagent_model(subagent_model);
        if !tui_mode {
            eprintln!("e-agent: subagent model {name}");
        }
    }
    let subagent_sessions = delegate.sessions();
    tools.push(Box::new(delegate));
    let mut agent = Agent::new(Box::new(model), tools);
    if !mcp_instructions.is_empty() {
        agent.set_context_prefix(mcp_instructions.join("\n\n"));
    }
    if let Some(rounds) = max_rounds {
        agent = agent.max_tool_rounds(rounds);
    }
    let loaded = Session::load(&root, &session)?;
    let legacy = loaded.legacy;
    agent.restore_history(loaded.entries);
    agent.record_background_tasks_in(root.clone());
    let unfinished = Session::take_unfinished_background(&root);
    if !unfinished.is_empty() {
        let notice = format!(
            "[e-agent exited with {} background task(s) still running; they were killed with the process. Re-run them if still needed:]\n{}",
            unfinished.len(),
            unfinished.join("\n")
        );
        let entry: e_agent::agent::SessionEntry =
            e_agent::agent::Message::User { content: notice }.into();
        // Persist immediately so a crash-before-first-turn cannot inject
        // the same notice again on the next launch.
        Session::append(&root, &session, std::slice::from_ref(&entry))?;
        // Append (NOT restore_history, which would wipe the resumed history).
        agent.push_entry(entry);
    }
    let mut persisted = agent.history().len();
    if legacy {
        Session::rewrite(&root, &session, agent.history())?;
    }

    if tui_mode {
        return tui::run(
            agent,
            root,
            session,
            persisted,
            background,
            subagent_sessions,
            model_name,
        )
        .await;
    }
    set_stderr_events(&mut agent);
    if repl_mode {
        return repl(agent, root, session, persisted).await;
    }
    let answer = run_and_save(&mut agent, &root, &session, &mut persisted, prompt).await?;
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
    root: &std::path::Path,
    session: &str,
    persisted: &mut usize,
    prompt: String,
) -> anyhow::Result<String> {
    let result = agent.run(prompt).await;
    Session::append(root, session, &agent.history()[*persisted..])?;
    *persisted = agent.history().len();
    result
}

async fn repl(
    mut agent: Agent,
    root: std::path::PathBuf,
    session: String,
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
            Session::append(&root, &session, &agent.history()[persisted..])?;
            persisted = agent.history().len();
            match result {
                Ok(summary) => println!("compacted: {}", preview(&summary, 500)),
                Err(error) => eprintln!("e-agent: {error:#}"),
            }
            continue;
        }
        agent.subscribe(sender.clone());
        match run_and_save(&mut agent, &root, &session, &mut persisted, line.to_owned()).await {
            Ok(answer) => println!("{answer}"),
            Err(error) => eprintln!("e-agent: {error:#}"),
        }
        while let Ok(AgentEvent::BackgroundCompleted { id, output }) = receiver.try_recv() {
            eprintln!("background task {id} finished: {}", preview(&output, 500));
        }
    }
    Ok(())
}

fn next_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    arguments
        .next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}
