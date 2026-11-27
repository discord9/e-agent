//! Minimal Agent Client Protocol (ACP) transport over a child process's stdio.
//!
//! One agent child speaks newline-delimited JSON-RPC 2.0 on stdin/stdout. This
//! module owns the process, exactly one reader task (which correlates replies
//! by id and fans every other message out on a channel), and the client half of
//! the ACP session lifecycle: `initialize`, `session/new`, `session/prompt`,
//! `session/update`, `session/request_permission` and `session/cancel`.
//! [`crate::acp_model::AcpModel`] wraps it as a [`crate::agent::Model`].
//!
//! This is meant to become the single ACP transport once the permission tool on
//! `feat/acp-permission` migrates onto it; do not grow a second one.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{ModelDeltaKind, Usage};
use crate::codex::DeltaSink;

/// The only ACP protocol version this client speaks (`initialize`).
const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC 2.0 "method not found".
const METHOD_NOT_FOUND: i64 = -32601;

/// Outstanding client requests, correlated by JSON-RPC id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// Why the reader task stopped (`None` when the client dropped it).
type Closed = Arc<Mutex<Option<String>>>;

/// The agent's stdin, shared with the cancel-on-drop path.
type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// The error object of a failed client request.
struct RpcError {
    code: i64,
    message: String,
}

/// A message the agent sent that is not the reply to one of our requests.
enum Incoming {
    /// A JSON-RPC notification (`session/update`, …).
    Notification { method: String, params: Value },
    /// An agent→client request; only `session/request_permission` is expected.
    Request { id: Value, method: String },
}

/// A live ACP agent process (or an injected test transport) speaking JSON-RPC
/// 2.0 over a byte stream.
pub(crate) struct AcpClient {
    /// The spawned agent; `None` for a transport injected by tests.
    child: Option<Child>,
    writer: Writer,
    next_id: u64,
    pending: Pending,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    reader: tokio::task::JoinHandle<()>,
    closed: Closed,
}

impl AcpClient {
    /// Spawn `command` in `cwd` and start reading its stdout.
    pub(crate) fn spawn(command: &str, args: &[String], cwd: &Path) -> anyhow::Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The agent's own diagnostics would interleave with the TUI/REPL
            // scrollback; protocol failures already name their cause.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot spawn ACP agent `{command}`"))?;
        let stdout = child.stdout.take().context("ACP agent has no stdout")?;
        let stdin = child.stdin.take().context("ACP agent has no stdin")?;
        Ok(Self::from_io(Some(child), stdout, stdin))
    }

    /// Build a client over an existing byte-stream pair. Tests drive a stub
    /// agent over in-memory pipes, mirroring how `src/codex/tests.rs` points
    /// the HTTP wire at a local stub server.
    pub(crate) fn from_io<R, W>(child: Option<Child>, stdout: R, stdin: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::default();
        let closed: Closed = Arc::default();
        let (notifications, incoming) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_loop(
            stdout,
            pending.clone(),
            closed.clone(),
            notifications,
        ));
        Self {
            child,
            writer: Arc::new(tokio::sync::Mutex::new(
                Box::new(stdin) as Box<dyn AsyncWrite + Send + Unpin>
            )),
            next_id: 0,
            pending,
            incoming,
            reader,
            closed,
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send(&self, value: &Value) -> anyhow::Result<()> {
        write_message(&self.writer, value).await
    }

    /// An error for a request that can no longer be answered.
    fn gone(&self, method: &str) -> anyhow::Error {
        match self.closed.lock().unwrap().clone() {
            Some(reason) => anyhow!("ACP agent child died before answering `{method}`: {reason}"),
            None => anyhow!("ACP agent child died before answering `{method}`"),
        }
    }

    async fn respond_ok(&self, id: &Value, result: Value) -> anyhow::Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
            .context("cannot answer an ACP agent request")
    }

    async fn respond_error(&self, id: &Value, code: i64, message: &str) -> anyhow::Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .await
        .context("cannot answer an ACP agent request")
    }

    /// Send one request and service the agent's side traffic until its reply
    /// arrives: `session/update` chunks stream to `on_delta`, and agent→client
    /// requests are answered inline so a turn can never deadlock.
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        on_delta: DeltaSink<'_>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id();
        let (reply, mut replies) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, reply);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.send(&request).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let mut on_delta = on_delta;
        loop {
            // An agent writes a turn's chunks BEFORE its `session/prompt`
            // result, so already-queued notifications are serviced first:
            // the callback order and the accumulated text must agree even
            // when both are readable in the same poll.
            if let Ok(message) = self.incoming.try_recv() {
                self.service(message, &mut on_delta).await?;
                continue;
            }
            // Selected values are handled after the select ends, so serviced
            // requests may await the writer while both channels are borrowed.
            enum Wake {
                Reply(Result<Result<Value, RpcError>, oneshot::error::RecvError>),
                Incoming(Option<Incoming>),
            }
            let wake = tokio::select! {
                reply = &mut replies => Wake::Reply(reply),
                incoming = self.incoming.recv() => Wake::Incoming(incoming),
            };
            match wake {
                Wake::Reply(Ok(reply)) => {
                    // Flush chunks that landed between the reply and this
                    // poll before reporting the turn complete.
                    while let Ok(message) = self.incoming.try_recv() {
                        self.service(message, &mut on_delta).await?;
                    }
                    return self.decode(method, reply);
                }
                Wake::Reply(Err(_)) => return Err(self.gone(method)),
                Wake::Incoming(None) => {
                    // The channel closes only after the reader has failed
                    // every outstanding request, but a reply may have landed
                    // just before the child exited: that reply still wins.
                    return match replies.try_recv() {
                        Ok(reply) => self.decode(method, reply),
                        Err(_) => Err(self.gone(method)),
                    };
                }
                Wake::Incoming(Some(message)) => {
                    self.service(message, &mut on_delta).await?;
                }
            }
        }
    }

    /// Service one agent→client message received while a request is in flight.
    async fn service(&self, message: Incoming, on_delta: &mut DeltaSink<'_>) -> anyhow::Result<()> {
        match message {
            Incoming::Notification { method, params } => {
                if method == "session/update"
                    && let Some(text) = agent_message_chunk(&params)
                    && let Some(callback) = on_delta.as_deref_mut()
                {
                    callback(ModelDeltaKind::Content, text);
                }
                Ok(())
            }
            Incoming::Request { id, method } => {
                if method == "session/request_permission" {
                    // No permission UI exists yet: always decline. Never
                    // auto-allow, and never leave the agent waiting.
                    self.respond_ok(&id, json!({"outcome": {"outcome": "cancelled"}}))
                        .await
                } else {
                    self.respond_error(
                        &id,
                        METHOD_NOT_FOUND,
                        &format!("e-agent does not implement `{method}`"),
                    )
                    .await
                }
            }
        }
    }

    /// Decode a correlated JSON-RPC reply.
    fn decode(&self, method: &str, reply: Result<Value, RpcError>) -> anyhow::Result<Value> {
        match reply {
            Ok(value) => Ok(value),
            Err(error) => Err(anyhow!(
                "ACP `{method}` failed (JSON-RPC {}): {}",
                error.code,
                error.message
            )),
        }
    }

    /// `initialize`: the agent must speak protocol version 1.
    pub(crate) async fn initialize(&mut self) -> anyhow::Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    // No filesystem or terminal capability is delegated.
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false,
                    },
                }),
                None,
            )
            .await
            .context("ACP `initialize` failed")?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("ACP `initialize` returned no protocolVersion"))?;
        if version != PROTOCOL_VERSION {
            bail!(
                "ACP agent speaks protocol version {version}; e-agent requires {PROTOCOL_VERSION}"
            );
        }
        Ok(())
    }

    /// `session/new`: open the stateful conversation and return its id.
    pub(crate) async fn new_session(&mut self, cwd: &Path) -> anyhow::Result<String> {
        let result = self
            .request(
                "session/new",
                json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
                None,
            )
            .await
            .context("ACP `session/new` failed")?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("ACP `session/new` returned no sessionId"))
    }

    /// `session/prompt`: run one turn, streaming `agent_message_chunk` text to
    /// `on_delta` and accumulating it into the returned answer.
    pub(crate) async fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        on_delta: DeltaSink<'_>,
    ) -> anyhow::Result<(String, Option<Usage>)> {
        let mut answer = String::new();
        let mut cancel = CancelGuard {
            writer: self.writer.clone(),
            session_id: session_id.to_owned(),
            finished: false,
        };
        let reply = {
            let mut on_delta = on_delta;
            let mut sink = |kind: ModelDeltaKind, chunk: &str| {
                answer.push_str(chunk);
                if let Some(callback) = on_delta.as_deref_mut() {
                    callback(kind, chunk);
                }
            };
            self.request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}],
                }),
                Some(&mut sink),
            )
            .await
        };
        cancel.finished = true;
        let reply = reply.context("ACP `session/prompt` failed")?;
        let finish_reason = reply
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let usage = reply
            .get("usage")
            .and_then(|usage| usage_from(usage, finish_reason));
        Ok((answer, usage))
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.reader.abort();
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        // Reap off the drop path: `Drop` cannot await `wait()`.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

/// Sends `session/cancel` when a prompt turn future is dropped unfinished (the
/// runner's Cancel drops the in-flight future), so the agent stops working
/// instead of running unobserved.
struct CancelGuard {
    writer: Writer,
    session_id: String,
    finished: bool,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let writer = self.writer.clone();
        let session_id = std::mem::take(&mut self.session_id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = write_message(
                    &writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": {"sessionId": session_id},
                    }),
                )
                .await;
            });
        }
    }
}

/// One reader task: correlate replies by id, fan everything else out.
async fn read_loop<R>(
    stdout: R,
    pending: Pending,
    closed: Closed,
    incoming: mpsc::UnboundedSender<Incoming>,
) where
    R: AsyncRead + Send + Unpin,
{
    let mut lines = BufReader::new(stdout).lines();
    let mut reason = None;
    loop {
        match lines.next_line().await {
            Ok(None) => {
                reason = Some("it exited (its stdout closed)".to_owned());
                break;
            }
            Err(error) => {
                reason = Some(format!("its stdout is unreadable: {error}"));
                break;
            }
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    // A malformed line is the agent's bug to fix, not a reason
                    // to fail the turn: log it and keep reading.
                    tracing::warn!(
                        target: "e_agent",
                        "ACP agent wrote a non-JSON line: {}",
                        crate::agent::preview(&line, 200)
                    );
                    continue;
                };
                match (value.get("method").and_then(Value::as_str), value.get("id")) {
                    (Some(method), Some(id)) if !id.is_null() => {
                        let request = Incoming::Request {
                            id: id.clone(),
                            method: method.to_owned(),
                        };
                        if incoming.send(request).is_err() {
                            break;
                        }
                    }
                    (Some(method), _) => {
                        let notification = Incoming::Notification {
                            method: method.to_owned(),
                            params: value.get("params").cloned().unwrap_or(Value::Null),
                        };
                        if incoming.send(notification).is_err() {
                            break;
                        }
                    }
                    (None, Some(id)) => {
                        if let Some(id) = id.as_u64() {
                            let sender = pending.lock().unwrap().remove(&id);
                            if let Some(sender) = sender {
                                let _ = sender.send(reply_body(&value));
                            }
                        }
                    }
                    (None, None) => {}
                }
            }
        }
    }
    // The agent is gone: fail every outstanding request and record why.
    *closed.lock().unwrap() = reason;
    pending.lock().unwrap().clear();
}

fn reply_body(value: &Value) -> Result<Value, RpcError> {
    match value.get("error") {
        Some(error) => Err(RpcError {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error")
                .to_owned(),
        }),
        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// One JSON-RPC message per line, per ACP's stdio framing.
async fn write_message(writer: &Writer, value: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    let mut writer = writer.lock().await;
    writer
        .write_all(line.as_bytes())
        .await
        .context("cannot write to the ACP agent")?;
    writer
        .flush()
        .await
        .context("cannot flush the ACP agent stdin")
}

/// The text of a `session/update` carrying `agent_message_chunk`.
fn agent_message_chunk(params: &Value) -> Option<&str> {
    let update = params.get("update")?;
    if update.get("sessionUpdate")?.as_str()? != "agent_message_chunk" {
        return None;
    }
    update.get("content")?.get("text")?.as_str()
}

/// Map an ACP prompt-response `usage` object onto e-agent's token accounting.
/// Both the ACP wire casing and snake_case are accepted; `None` when the agent
/// reports no counts at all.
fn usage_from(usage: &Value, finish_reason: Option<String>) -> Option<Usage> {
    let number = |camel: &str, snake: &str| {
        usage
            .get(camel)
            .or_else(|| usage.get(snake))
            .and_then(Value::as_u64)
    };
    let input = number("inputTokens", "input_tokens");
    let output = number("outputTokens", "output_tokens");
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
        cache_hit_tokens: number("cachedReadTokens", "cached_read_tokens"),
        cache_miss_tokens: None,
        reasoning_tokens: number("thoughtTokens", "thought_tokens"),
        finish_reason,
    })
}
