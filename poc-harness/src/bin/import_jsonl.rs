use anyhow::{Context, Result};
use std::path::Path;

const HTTP_SQL: &str = "http://127.0.0.1:15400/v1/sql";
const DB: &str = "e_agent";

fn entry_kind(entry: &serde_json::Value) -> &'static str {
    match entry["type"].as_str().unwrap_or("") {
        "message" => "message",
        "compaction" => "compaction",
        "notice" => "notice",
        _ => "message",
    }
}

fn is_error(entry: &serde_json::Value) -> bool {
    entry["message"]["Tool"]["is_error"].as_bool().unwrap_or(false)
}

fn agent_role(session_id: &str) -> &'static str {
    if session_id.starts_with("sub-") || session_id.starts_with("subagent-") {
        "subagent"
    } else {
        "main"
    }
}

async fn import_file(client: &reqwest::Client, path: &Path) -> Result<usize> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("bad filename")?;
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let mut values = Vec::new();
    let base_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as i64;

    for (seq, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: bad JSON", path.display(), seq + 1))?;
        let kind = entry_kind(&entry);
        let err = is_error(&entry);
        let role = agent_role(stem);
        let ts = base_ts + seq as i64; // monotonic within session
        let escaped = line.replace('\'', "''");
        values.push(format!(
            "('{stem}', {seq}, {ts}, '{kind}', '{escaped}', 1, '{role}', {err})"
        ));
    }

    if values.is_empty() {
        return Ok(0);
    }

    // batch insert in chunks of 200 to avoid oversized SQL
    let mut total = 0;
    for chunk in values.chunks(200) {
        let sql = format!(
            "INSERT INTO session_entries
                (session_id, seq, event_time, entry_kind, payload,
                 schema_version, agent_role, is_error)
             VALUES {}",
            chunk.join(",\n")
        );
        let resp = client
            .post(HTTP_SQL)
            .form(&[("sql", sql.as_str()), ("db", DB)])
            .send()
            .await?
            .text()
            .await?;
        if resp.contains("error") {
            anyhow::bail!("import failed for {stem}: {resp}");
        }
        total += chunk.len();
    }
    Ok(total)
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/discord9/e-agent/.e-agent/sessions".into());
    let client = reqwest::Client::new();
    let mut total_entries = 0;
    let mut total_files = 0;

    let mut paths: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    paths.sort();

    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy();
        match import_file(&client, path).await {
            Ok(0) => println!("  skip {name} (empty)"),
            Ok(n) => {
                println!("  {name}: {n} entries");
                total_entries += n;
                total_files += 1;
            }
            Err(e) => eprintln!("  FAIL {name}: {e:#}"),
        }
    }
    println!("\nImported {total_entries} entries from {total_files} files");
    Ok(())
}
