//! Minimal stdio MCP client: connects to local MCP servers, lists their
//! tools, and exposes them as dynamic [`Tool`] implementations.
//!
//! Non-goals (deliberately not implemented): remote MCP over HTTP/SSE,
//! OAuth, resources/prompts, server-initiated notifications, `listChanged`
//! refresh, server restart on crash, concurrent initialization.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use crate::agent::{Tool, ToolSpec};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_LIMIT: usize = 64 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration for one local MCP server, deserialized from the config file.
/// Unknown keys are a parse error (`deny_unknown_fields`): a misspelled
/// field inside `[mcp."<name>"]` is refused instead of silently ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Command plus arguments, e.g. `["/path/to/engram", "mcp", "--tools=agent"]`.
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory override; defaults to the workspace root.
    pub cwd: Option<PathBuf>,
    /// Whether this server is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// One running MCP server connection.
struct McpServer {
    stdin: Mutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicU64,
    _child: Child,
}

impl McpServer {
    /// Spawn a server process, perform the initialize handshake, and return
    /// the connection plus the server instructions (if any).
    async fn connect(
        name: &str,
        config: &McpServerConfig,
        workspace_root: &Path,
    ) -> anyhow::Result<(Arc<Self>, Option<String>)> {
        let (program, args) = config
            .command
            .split_first()
            .ok_or_else(|| anyhow!("mcp server `{name}`: command is empty"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(&config.env)
            .current_dir(config.cwd.as_deref().unwrap_or(workspace_root))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Drain stderr so the child cannot deadlock on a full pipe.
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("mcp server `{name}`: failed to spawn `{program}`"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("mcp server `{name}`: no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("mcp server `{name}`: no stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("mcp server `{name}`: no stderr"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && let Some(id) = value.get("id").and_then(Value::as_u64)
                {
                    let sender = reader_pending.lock().await.remove(&id);
                    if let Some(sender) = sender {
                        let result = if let Some(error) = value.get("error") {
                            Err(format!("mcp error: {error}"))
                        } else {
                            Ok(value.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = sender.send(result);
                    }
                }
            }
        });
        tokio::spawn(async move {
            let mut total = 0usize;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        total += count;
                        if total > STDERR_LIMIT {
                            // Keep draining but stop counting; the pipe stays open.
                        }
                    }
                }
            }
        });

        let server = Arc::new(Self {
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
        });

        let initialize = server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "e-agent", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        server
            .notify("notifications/initialized", json!({}))
            .await?;
        let instructions = initialize
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok((server, instructions))
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&message).await?;
        match tokio::time::timeout(CALL_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result.map_err(|error| anyhow!("{error}")),
            Ok(Err(_)) => Err(anyhow!("mcp server closed the connection")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("mcp request timed out after {CALL_TIMEOUT:?}"))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&message).await
    }

    async fn send(&self, message: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn list_tools(&self) -> anyhow::Result<Vec<Value>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("mcp tools/list response missing `tools` array"))?;
        Ok(tools.clone())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| "mcp tools/call response missing `content` array".to_owned())?;
        let mut text = String::new();
        for item in content {
            if let Some(text_item) = item.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(text_item);
            }
        }
        if text.len() > RESULT_LIMIT {
            text.truncate(RESULT_LIMIT);
            text.push_str("\n...[truncated]");
        }
        Ok(text)
    }
}

/// A dynamic MCP tool: forwards `execute` to `tools/call` on the server.
struct McpTool {
    server: Arc<McpServer>,
    remote_name: String,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        self.server.call_tool(&self.remote_name, arguments).await
    }
}

/// Connect to all configured MCP servers, list their tools, and return
/// (tools, system-prompt instructions). Failures are logged as warnings and
/// do not abort startup. Server definitions come from the unified TOML
/// config (`[mcp.<name>]` sections, see config.rs).
pub async fn connect_all(
    servers: HashMap<String, McpServerConfig>,
    workspace_root: &Path,
) -> (Vec<Box<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut instructions = Vec::new();
    for (name, server_config) in servers {
        if !server_config.enabled {
            continue;
        }
        match McpServer::connect(&name, &server_config, workspace_root).await {
            Ok((server, server_instructions)) => {
                if let Some(text) = server_instructions {
                    instructions.push(format!("## MCP server `{name}`\n\n{text}"));
                }
                match server.list_tools().await {
                    Ok(list) => {
                        let mut tool_count = 0usize;
                        for tool in list {
                            let remote_name = tool
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            if remote_name.is_empty() {
                                continue;
                            }
                            let description = tool
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            let parameters = tool
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or_else(|| json!({"type": "object"}));
                            let spec = ToolSpec {
                                name: format!("{name}_{remote_name}"),
                                description,
                                parameters,
                            };
                            tools.push(Box::new(McpTool {
                                server: server.clone(),
                                remote_name,
                                spec,
                            }));
                            tool_count += 1;
                        }
                        eprintln!("e-agent: mcp server `{name}` connected ({tool_count} tools)");
                    }
                    Err(error) => eprintln!(
                        "e-agent: warning: mcp server `{name}` tools/list failed: {error:#}"
                    ),
                }
            }
            Err(error) => {
                eprintln!("e-agent: warning: mcp server `{name}` failed to start: {error:#}")
            }
        }
    }
    (tools, instructions)
}

#[cfg(test)]
mod tests;
