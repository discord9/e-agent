//! ACP (Agent Client Protocol) session driver: a session whose turns are an
//! external ACP conversation over newline-delimited JSON-RPC 2.0 on the
//! agent's stdin/stdout, instead of our model/tool loop.
//!
//! **Why a driver and not a `Model`.** ACP is the other end of a conversation,
//! not one provider round: the agent owns its own history, asks the client for
//! permission *mid-turn*, and answers with a stop reason. A `Model` impl has
//! one request/one response and no place for a question, so it would have to
//! fake a turn as a round. `SessionRunner` therefore owns exactly one driver
//! (this one or `Agent`); everything generic stays generic: the event fanout
//! (`Shared::emit_agent`), the `WaitingInput` question, transcript append,
//! cancellation, the registry entry, the task panel and the web session view.
//!
//! **One process per session.** `connect` spawns the agent once
//! (`crate::config::DEFAULT_ACP_ARGS` = `agy acp --stdio`), runs `initialize` +
//! `session/new`, and keeps the process for the session's lifetime. `shutdown`
//! SIGKILLs the process group and reaps it; dropping the driver does the same
//! without waiting, so neither an abandoned child nor a parent teardown leaves
//! an agent running. Not a pool: a session's process is its session.
//!
//! **The agent's history is authoritative for execution; ours is display
//! only.** The session transcript (in `runner`) is what the user reads. Nothing
//! from it is ever sent back as a prompt: a turn is one `session/prompt` on the
//! live ACP session, and the agent's own conversation is what it remembers.
//!
//! **`session/request_permission` is a question, never a policy.** The request
//! is forwarded verbatim to the session layer as one blocking question whose
//! options ARE the agent's options (`optionId` → id, `name` → label, `kind` →
//! description); the reply is the chosen `optionId` byte for byte. There is no
//! auto-allow: a request nobody answered is answered `cancelled`, so an
//! approval no human saw can never be granted. A request that arrives while
//! another is still awaiting an answer is refused (`cancelled`), never decided.
//!
//! **A dead agent is never a hang.** The reader task owns the only view of the
//! process's stdout: when it closes, every in-flight request fails and ONE
//! terminal [`DriverEvent::Died`] is delivered, so a turn fails visibly instead
//! of waiting forever.
//!
//! Non-goals for this milestone (deliberately not implemented): tool-call cards
//! for the agent's own calls (`tool_call`/`tool_call_update` are dropped before
//! they reach the session), usage/context accounting (`usage_update` is
//! dropped), `/model`,
//! compaction, cross-process `session/load`, MCP passthrough (`mcpServers` is
//! always `[]`), TUI parity, several queued prompts, and `session/list`
//! discovery.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::config::AcpConfig;
use crate::runner::UserOption;

/// ACP protocol version spoken by this client.
const PROTOCOL_VERSION: u32 = 1;
/// Deadline for setup calls (`initialize`, `session/new`, `session/set_model`).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded stderr tail kept for error messages.
const STDERR_LIMIT: usize = 4 * 1024;
/// Tool-call title limit in the permission question.
const SUMMARY_LIMIT: usize = 120;
/// Raw command limit in the permission question.
const COMMAND_LIMIT: usize = 200;
/// The single question id a permission request uses (the answer is an option
/// id, so the question id itself carries no information).
pub(crate) const PERMISSION_QUESTION: &str = "permission";

/// One `session/request_permission` as the agent sent it: the JSON-RPC id it
/// must be answered on and the request params. Ids are echoed back as `Value`s
/// verbatim — `agy` uses string ids (`"1"`) while our own requests are numbers.
pub(crate) struct PermissionRequest {
    pub(crate) id: Value,
    pub(crate) params: Value,
}

/// What the driver tells the session layer. Not an event channel: these are
/// driver→runner signals, and every user-visible projection still goes through
/// the session's single `Shared::emit_agent` fanout.
pub(crate) enum DriverEvent {
    /// `agent_message_chunk` text, in arrival order.
    Delta(String),
    /// `agent_thought_chunk` text.
    Thought(String),
    Permission(PermissionRequest),
    /// The `session/prompt` response arrived: the turn is over.
    Finished {
        stop_reason: String,
    },
    /// The agent is gone (or the turn failed at the wire). Delivered at most
    /// once per session.
    Died(String),
}

/// Where a `delegate { agent: "..." }` call launches its agent: the program,
/// its arguments, and the model to select after `session/new`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentLaunch {
    pub(crate) binary: String,
    pub(crate) args: Vec<String>,
    pub(crate) model: Option<String>,
}

impl AgentLaunch {
    /// Resolve an agent NAME to a launch. A name names the configured `[acp]`
    /// agent when it matches (so `[acp] model`/`args` apply), else it is the
    /// program itself with the default stdio arguments. A name is a bare
    /// program name — an explicit path is what the global `[acp] binary` is
    /// for, because a name is chosen by the model.
    pub(crate) fn resolve(name: &str, configured: Option<&AcpConfig>) -> Result<Self, String> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "`agent` must be a bare agent name (letters, digits, `-`, `_`, `.`): \
                 {name:?}; a program path belongs in the global `[acp] binary` setting"
            ));
        }
        if let Some(config) = configured {
            let configured_name = Path::new(&config.binary)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(&config.binary);
            if configured_name == name {
                return Ok(Self {
                    binary: config.binary.clone(),
                    args: config.args.clone(),
                    model: config.model.clone(),
                });
            }
        }
        Ok(Self {
            binary: name.to_owned(),
            args: crate::config::DEFAULT_ACP_ARGS
                .iter()
                .map(|arg| (*arg).to_owned())
                .collect(),
            model: None,
        })
    }
}

/// One live ACP agent process plus its JSON-RPC plumbing.
pub(crate) struct AcpDriver {
    /// Owns the child and kills+reaps it (on shutdown, or on drop).
    process: AgentProcess,
    pipe: Arc<Pipe>,
    /// Display name of the agent that actually answered (`agentInfo.name`,
    /// else the program).
    name: String,
    /// The ACP session this driver's turns run in.
    session_id: String,
    /// Driver→session signals.
    events: mpsc::UnboundedReceiver<DriverEvent>,
}

impl AcpDriver {
    /// Spawn the agent and open one ACP session: `initialize` → `session/new`
    /// (→ `session/set_model`). The process is the session's for as long as the
    /// driver lives; an error here drops it, so a failed launch leaves nothing
    /// running.
    pub(crate) async fn connect(launch: &AgentLaunch, cwd: &Path) -> Result<Self, String> {
        let (process, pipe, events) = Pipe::spawn(launch, cwd)?;
        let mut driver = Self {
            process,
            pipe,
            name: launch.binary.clone(),
            session_id: String::new(),
            events,
        };
        if let Err(error) = driver.handshake(launch, cwd).await {
            // The agent dies with `driver` on this early return.
            return Err(driver.describe(error).await);
        }
        Ok(driver)
    }

    async fn handshake(&mut self, launch: &AgentLaunch, cwd: &Path) -> Result<(), String> {
        let initialize = self
            .pipe
            .request(
                "initialize",
                json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}}),
                Some(CALL_TIMEOUT),
            )
            .await?;
        let info = initialize.get("agentInfo");
        let name = info
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if let Some(name) = name {
            self.name = name.to_owned();
        }
        let session = self
            .pipe
            .request(
                "session/new",
                json!({"cwd": cwd.display().to_string(), "mcpServers": []}),
                Some(CALL_TIMEOUT),
            )
            .await?;
        self.session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| "acp: `session/new` returned no `sessionId`".to_owned())?
            .to_owned();
        // The driver channel is created before the handshake so a reply can
        // never race a receiver that does not exist yet.
        if let Some(model) = &launch.model {
            self.pipe
                .request(
                    "session/set_model",
                    json!({"sessionId": self.session_id, "modelId": model}),
                    Some(CALL_TIMEOUT),
                )
                .await?;
        }
        Ok(())
    }

    /// The agent's display name.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The ACP session id this driver's turns run in.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Take the driver-signal receiver. The session layer owns it, so a turn
    /// can await a signal and a steering command at the same time.
    pub(crate) fn take_events(&mut self) -> mpsc::UnboundedReceiver<DriverEvent> {
        std::mem::replace(&mut self.events, mpsc::unbounded_channel().1)
    }

    /// Start ONE turn: `session/prompt` with the whole task as one text block.
    /// The response is forwarded as [`DriverEvent::Finished`] (its
    /// `stopReason` ends the turn) or as [`DriverEvent::Died`] when the wire
    /// failed. Fire-and-forget on purpose: the session layer keeps handling
    /// commands and permission requests while the turn runs.
    pub(crate) fn prompt(&self, text: &str) {
        let pipe = Arc::clone(&self.pipe);
        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}],
        });
        tokio::spawn(async move {
            // No wall-clock deadline: an agent may legitimately work for a long
            // time. The turn cannot hang silently — process death closes stdout
            // and fails every in-flight request.
            match pipe.request("session/prompt", params, None).await {
                Ok(result) => {
                    let stop_reason = result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    pipe.emit(DriverEvent::Finished { stop_reason }).await;
                }
                Err(error) => pipe.emit_death(error).await,
            }
        });
    }

    /// Ask the agent to stop the in-flight turn. `session/cancel` is a
    /// notification (no response): the agent's own `stopReason` still decides
    /// how the turn ends on the wire.
    pub(crate) fn cancel(&self) {
        let pipe = Arc::clone(&self.pipe);
        let params = json!({"sessionId": self.session_id});
        tokio::spawn(async move {
            let _ = pipe.notify("session/cancel", params).await;
        });
    }

    /// Send one permission decision: the chosen `optionId`, verbatim.
    pub(crate) fn select_permission(&self, id: &Value, option_id: &str) {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {"outcome": {"outcome": "selected", "optionId": option_id}},
        });
        let pipe = Arc::clone(&self.pipe);
        tokio::spawn(async move {
            let _ = pipe.send(&reply).await;
        });
    }

    /// Refuse one permission request (`cancelled`): nobody decided it, the
    /// agent is gone, or another request already owns the open question. The
    /// agent is never left waiting on a decision.
    pub(crate) fn refuse_permission(&self, id: &Value) {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": {"outcome": {"outcome": "cancelled"}},
        });
        let pipe = Arc::clone(&self.pipe);
        tokio::spawn(async move {
            let _ = pipe.send(&reply).await;
        });
    }

    /// Kill the agent's process group and reap it. The session is over.
    pub(crate) async fn shutdown(&mut self) {
        self.process.kill().await;
    }

    /// Error text with the agent's stderr tail attached.
    pub(crate) async fn describe(&self, error: String) -> String {
        self.pipe.describe(error)
    }
}

/// The agent process. `Drop` (or [`Self::kill`]) SIGKILLs the whole process
/// group — Unix spawns the child as its own group leader, so the agent's
/// grandchildren die with it — and reaps the child.
struct AgentProcess {
    child: Option<Child>,
}

impl AgentProcess {
    /// SIGKILL the process group and wait for the child, so no zombie outlives
    /// the session that started it.
    async fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        kill_group(&child);
        let _ = child.kill().await;
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        kill_group(&child);
        let _ = child.start_kill();
        // Reap off the drop path: `wait` is async and `Drop` is not.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

#[cfg(unix)]
fn kill_group(child: &Child) {
    if let Some(group) = child
        .id()
        .and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_child: &Child) {}

/// One in-flight request: the channel its reply is delivered on. The method
/// name is already in the error text of every path that needs it.
struct PendingRequest {
    sender: oneshot::Sender<Result<Value, String>>,
}

/// Everything the reader task and the request side share: writes to the agent,
/// the in-flight request table, and the driver-event sender.
struct Pipe {
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    next_id: AtomicU64,
    events: mpsc::UnboundedSender<DriverEvent>,
    /// Set once when stdout closed: the failure is reported exactly once.
    dead: AtomicBool,
    stderr: Arc<StdMutex<String>>,
}

impl Pipe {
    fn spawn(
        launch: &AgentLaunch,
        cwd: &Path,
    ) -> Result<
        (
            AgentProcess,
            Arc<Pipe>,
            mpsc::UnboundedReceiver<DriverEvent>,
        ),
        String,
    > {
        let mut command = Command::new(&launch.binary);
        command
            .args(&launch.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Drain stderr so the agent cannot deadlock on a full pipe.
            .stderr(Stdio::piped())
            // Belt for an abrupt teardown that skips `Drop`.
            .kill_on_drop(true);
        // Own process group, so killing the session also kills the agent's
        // children (the bash / run_rust precedent).
        #[cfg(unix)]
        command.process_group(0);
        // ETXTBSY: exec'ing a file that some process still holds open for
        // writing. It happens when the agent binary was just installed or
        // rewritten, and in tests whose stub script is written moments before
        // the spawn, so retry briefly instead of failing the session.
        let mut attempts = 0;
        let mut child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error) if error.raw_os_error() == Some(26) && attempts < 20 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(format!(
                        "acp: cannot start `{}`: {error} (set `[acp] binary` / `args`)",
                        launch.binary
                    ));
                }
            }
        };
        let stdin = child.stdin.take().ok_or("acp: the agent has no stdin")?;
        let stdout = child.stdout.take().ok_or("acp: the agent has no stdout")?;
        let stderr = child.stderr.take().ok_or("acp: the agent has no stderr")?;
        let stderr = drain_stderr(stderr);
        let (events, receiver) = mpsc::unbounded_channel();
        let pipe = Arc::new(Pipe {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
            dead: AtomicBool::new(false),
            stderr,
        });
        tokio::spawn(read_loop(Arc::clone(&pipe), stdout));
        // The reader only ever sends; the receiver lives in the driver, so a
        // dropped driver ends `recv` instead of keeping the pipe alive.
        Ok((AgentProcess { child: Some(child) }, pipe, receiver))
    }

    /// Deliver one driver event. A closed receiver (session gone) is not an
    /// error: nobody is listening any more.
    async fn emit(&self, event: DriverEvent) {
        let _ = self.events.send(event);
    }

    /// Write one JSON-RPC frame: the object, a newline, and a flush.
    async fn send(&self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(message)
            .map_err(|error| format!("acp: cannot encode a frame: {error}"))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .map_err(|error| format!("acp: cannot write to the agent: {error}"))?;
        stdin
            .flush()
            .await
            .map_err(|error| format!("acp: cannot flush the agent's stdin: {error}"))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Send one request and await its response. `timeout` bounds setup calls;
    /// a turn passes `None` because an agent may work for a long time.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(id, PendingRequest { sender });
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.send(&message).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let await_reply = async {
            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "acp: the agent closed the connection during `{method}`"
                )),
            }
        };
        match timeout {
            Some(limit) => match tokio::time::timeout(limit, await_reply).await {
                Ok(result) => result,
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    Err(format!(
                        "acp: `{method}` timed out after {}s",
                        limit.as_secs()
                    ))
                }
            },
            None => await_reply.await,
        }
    }

    /// The agent's stdout closed: fail every in-flight request and report the
    /// death exactly once.
    async fn child_gone(&self, error: String) {
        // Dropping the senders resolves every in-flight receiver with a
        // "connection closed" error instead of parking it forever.
        self.pending.lock().await.clear();
        self.emit_death(error).await;
    }

    /// Report the agent's death ONCE per session, whoever noticed it first
    /// (the reader task or a failed request). A second report would be a
    /// second terminal notice for one failure.
    async fn emit_death(&self, error: String) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        self.emit(DriverEvent::Died(self.describe(error))).await;
    }

    fn describe(&self, error: String) -> String {
        let tail = self.stderr.lock().unwrap();
        let tail = tail.trim();
        if tail.is_empty() {
            error
        } else {
            format!("{error}\nagent stderr: {tail}")
        }
    }
}

/// Read frames until the agent closes stdout. Unparsable lines (a banner, a
/// stray log line) are skipped rather than failing the session.
async fn read_loop(pipe: Arc<Pipe>, stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        dispatch(&pipe, value).await;
    }
    pipe.child_gone("acp: the agent exited".into()).await;
}

/// Route one received frame: a server request (only a permission request is
/// expected), a response to one of our requests, or a notification.
async fn dispatch(pipe: &Arc<Pipe>, value: Value) {
    let id = value.get("id").cloned().filter(|id| !id.is_null());
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (id, method) {
        // A permission request is forwarded to the session layer, which owns
        // the question. The reader never waits on a human: it must keep reading
        // so a dead agent is noticed immediately (see `read_loop`).
        (Some(id), Some(method)) if method == "session/request_permission" => {
            pipe.emit(DriverEvent::Permission(PermissionRequest {
                id,
                params: value.get("params").cloned().unwrap_or(Value::Null),
            }))
            .await;
        }
        // Any other server request: an unanswered request would hang the agent,
        // so it is refused with -32601.
        (Some(id), Some(method)) => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method `{method}` is not supported")},
            });
            let _ = pipe.send(&reply).await;
        }
        // A response to one of our requests. Replies may arrive out of order,
        // so the id is the only routing key.
        (Some(id), None) => {
            let Some(id) = id.as_u64() else { return };
            let Some(request) = pipe.pending.lock().await.remove(&id) else {
                return;
            };
            let result = match value.get("error") {
                Some(error) => Err(rpc_error(error)),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = request.sender.send(result);
        }
        (None, Some(method)) => dispatch_notification(pipe, &method, value.get("params")).await,
        (None, None) => {}
    }
}

/// `session/update` notifications: only assistant and thought text is
/// forwarded. Tool-call cards, usage and command lists are milestone non-goals,
/// so every other update is dropped here rather than half-rendered.
async fn dispatch_notification(pipe: &Arc<Pipe>, method: &str, params: Option<&Value>) {
    if method != "session/update" {
        return;
    }
    let Some(update) = params.and_then(|params| params.get("update")) else {
        return;
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = update
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "agent_message_chunk" if !text.is_empty() => {
            pipe.emit(DriverEvent::Delta(text.to_owned())).await
        }
        "agent_thought_chunk" if !text.is_empty() => {
            pipe.emit(DriverEvent::Thought(text.to_owned())).await
        }
        _ => {}
    }
}

/// Drain stderr so the agent cannot block on a full pipe, keeping a bounded
/// tail for error messages.
fn drain_stderr(mut stderr: ChildStderr) -> Arc<StdMutex<String>> {
    let sink = Arc::new(StdMutex::new(String::new()));
    let writer = sink.clone();
    tokio::spawn(async move {
        let mut chunk = [0u8; 2048];
        loop {
            let count = match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(count) => count,
            };
            let text = String::from_utf8_lossy(&chunk[..count]);
            let mut tail = writer.lock().unwrap();
            push_bounded(&mut tail, &text, STDERR_LIMIT);
        }
    });
    sink
}

/// Append `text` to `buffer`, keeping it at or below `limit` bytes on a char
/// boundary. Returns true when anything was dropped.
fn push_bounded(buffer: &mut String, text: &str, limit: usize) -> bool {
    if text.is_empty() {
        return false;
    }
    let room = limit.saturating_sub(buffer.len());
    if room == 0 {
        return true;
    }
    if text.len() <= room {
        buffer.push_str(text);
        return false;
    }
    let mut cut = room;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    buffer.push_str(&text[..cut]);
    true
}

/// Model-facing text for a JSON-RPC error object. `-32000` with an
/// authentication message is the "not logged in" bucket (`agy acp` answers
/// exactly that until a login exists): it is named explicitly so the user is
/// told what to fix instead of retrying blindly.
fn rpc_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    match code {
        Some(-32000) if message.to_ascii_lowercase().contains("auth") => format!(
            "acp: the agent requires authentication ({message}); log in to that agent first, then retry"
        ),
        Some(code) => format!("acp error {code}: {message}"),
        None => format!("acp: {message}"),
    }
}

/// The identity of one permission ask — the `call_id` of the question the
/// session publishes: `acp-permission-<nonce>-<toolCallId>-<digest>`.
///
/// * `<nonce>` is a fresh random 64-bit value per ask, so two asks never share
///   an identity (an agent reuses its JSON-RPC ids request after request).
/// * `<toolCallId>` is the ACP tool call the request is about.
/// * `<digest>` fingerprints the REQUEST as received: the `toolCall` object and
///   the offered options.
///
/// An answer therefore *names* the request it was given for: request N's
/// approval cannot authorize request N+1, and an answer for a request that is
/// no longer open is refused by the session layer.
///
/// This binds an answer to a REQUEST, never to an executed action: the human
/// sees the tool call's title and at most [`COMMAND_LIMIT`] command characters,
/// and the reply is one of the offered option ids.
pub(crate) fn permission_identity(params: &Value) -> String {
    let tool_call = params.get("toolCall");
    format!(
        "acp-permission-{:016x}-{}-{}",
        rand::random::<u64>(),
        id_token(text_field(tool_call, "toolCallId").unwrap_or("unknown")),
        action_digest(params),
    )
}

/// A bounded, id-safe form of an agent-supplied call id (spaces, `/`, and
/// non-ASCII become `_`), so a question id stays one readable token.
fn id_token(raw: &str) -> String {
    let token: String = raw
        .chars()
        .take(32)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if token.is_empty() {
        "unknown".into()
    } else {
        token
    }
}

/// 64-bit hex fingerprint of the requested action: the `toolCall` plus the
/// options the agent offered. Identical requests share it; anything else —
/// another tool call, another command, another set of choices — does not.
fn action_digest(params: &Value) -> String {
    let field = |name: &str| params.get(name).cloned().unwrap_or(Value::Null);
    let mut hasher = Sha256::new();
    hasher.update(json!({"toolCall": field("toolCall"), "options": field("options")}).to_string());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn option_id(option: &Value) -> Option<String> {
    option
        .get("optionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// The ACP options of one permission request, mapped one-to-one to question
/// options (`optionId` → `id`, `name` → `label`, `kind` → `description`).
/// `None` when the request offers no usable option id: there is then nothing to
/// ask, so the request is refused rather than answered with an invented choice.
pub(crate) fn question_options(params: &Value) -> Option<Vec<UserOption>> {
    let offered = params
        .get("options")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|option| {
            let id = option_id(option)?;
            let label = text_field(Some(option), "name").map(str::to_owned);
            let description = text_field(Some(option), "kind").map(str::to_owned);
            Some(UserOption {
                label: label.unwrap_or_else(|| id.clone()),
                id,
                description,
            })
        })
        .collect::<Vec<_>>();
    (!offered.is_empty()).then_some(offered)
}

/// The question a permission request becomes: what the agent SAYS it wants to
/// do — `toolCall.title` plus, when present, the raw command (cut to
/// [`COMMAND_LIMIT`] characters). Exactly one question per request.
pub(crate) fn permission_prompt(params: &Value) -> String {
    let tool_call = params.get("toolCall");
    let title = text_field(tool_call, "title").unwrap_or("an unknown tool call");
    match command_of(tool_call) {
        Some(command) => format!("{title}: {}", truncate_command(command)),
        None => title.to_owned(),
    }
}

/// A one-line summary of the `toolCall` a permission request is about:
/// `title` first, then the raw command, then the call id.
fn tool_call_summary(tool_call: Option<&Value>) -> String {
    let Some(tool_call) = tool_call else {
        return "unknown tool call".into();
    };
    text_field(Some(tool_call), "title")
        .or_else(|| command_of(Some(tool_call)))
        .or_else(|| text_field(Some(tool_call), "toolCallId"))
        .unwrap_or("unknown tool call")
        .to_owned()
}

/// `toolCall.rawInput.command`, trimmed and non-empty.
fn command_of(tool_call: Option<&Value>) -> Option<&str> {
    tool_call
        .and_then(|tool_call| tool_call.get("rawInput"))
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
}

/// Cut a command to [`COMMAND_LIMIT`] CHARACTERS on a char boundary, marking
/// the cut so a truncated command is never mistaken for the whole one.
fn truncate_command(command: &str) -> String {
    match command.char_indices().nth(COMMAND_LIMIT) {
        Some((cut, _)) => format!("{}…", &command[..cut]),
        None => command.to_owned(),
    }
}

/// Shorten one line to [`SUMMARY_LIMIT`] bytes on a char boundary.
fn clip(text: &str) -> String {
    if text.len() <= SUMMARY_LIMIT {
        return text.to_owned();
    }
    let mut cut = SUMMARY_LIMIT;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

/// One trimmed, non-empty string field of an optional JSON object: a
/// `toolCall` field (`title`/`kind`/`toolCallId`) or an option's `name`/`kind`.
fn text_field<'a>(object: Option<&'a Value>, name: &str) -> Option<&'a str> {
    object
        .and_then(|object| object.get(name))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// A session-level summary of one permission ask for the transcript line the
/// session commits when a request is refused without asking a human.
pub(crate) fn refusal_line(params: &Value) -> String {
    let kind = text_field(params.get("toolCall"), "kind").unwrap_or("unknown");
    format!(
        "denied {kind}: {}",
        clip(&tool_call_summary(params.get("toolCall")))
    )
}

#[cfg(test)]
#[path = "acp_tests.rs"]
mod tests;
