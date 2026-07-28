mod agent;
mod model;
mod tools;
mod workspace;

use std::io::{IsTerminal, Read, Write};

use agent::Agent;
use model::OpenAiModel;
use tools::builtins;
use workspace::Workspace;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("e-agent: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut base_url = None;
    let mut model = None;
    let mut workspace = None;
    let mut prompt = Vec::new();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--base-url" => base_url = Some(next_value(&mut arguments, "--base-url")?),
            "--model" => model = Some(next_value(&mut arguments, "--model")?),
            "--workspace" => workspace = Some(next_value(&mut arguments, "--workspace")?),
            "--help" | "-h" => {
                println!(
                    "usage: e-agent [--base-url URL] [--model MODEL] [--workspace PATH] [PROMPT]"
                );
                return Ok(());
            }
            value => prompt.push(value.to_owned()),
        }
    }
    let interactive = prompt.is_empty() && std::io::stdin().is_terminal();
    let prompt = if !prompt.is_empty() {
        prompt.join(" ")
    } else if interactive {
        String::new()
    } else {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        input
    };
    if !interactive && prompt.trim().is_empty() {
        return Err("a prompt argument or stdin content is required".into());
    }
    let workspace = Workspace::new(match workspace {
        Some(path) => path.into(),
        None => std::env::current_dir()?,
    })?;
    let model = OpenAiModel::from_env(base_url, model)?;
    let mut agent = Agent::new(Box::new(model), builtins(workspace)).print_tool_calls(true);
    if interactive {
        return repl(agent).await;
    }
    println!("{}", agent.run(prompt).await?);
    Ok(())
}

async fn repl(mut agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    loop {
        print!("e-agent> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" {
            break;
        }
        match agent.run(line.to_owned()).await {
            Ok(answer) => println!("{answer}"),
            Err(error) => eprintln!("e-agent: {error}"),
        }
    }
    Ok(())
}

fn next_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))
}
