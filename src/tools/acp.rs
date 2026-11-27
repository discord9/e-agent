//! Minimal ACP (Agent Client Protocol) client: the `acp_prompt` tool is an
//! ORACLE-STYLE reviewer, so it talks to an external ACP agent (default
//! `agy acp --stdio`) over newline-delimited JSON-RPC 2.0 on stdin/stdout.
//!
//! **One ACP process per call, and all session state lives in the agent.**
//! Each call spawns `acp`, and the process group is SIGKILLed when the call
//! finishes OR the turn is cancelled (a cancel drops this future), so no agent
//! outlives the call that started it. The conversation itself lives in the
//! agent; e-agent only remembers WHICH ACP session id it last used, in the
//! session store's own metadata row (`sessions.acp_session_id`, next to
//! `model`/`title`/`pinned`), keyed by the session it belongs to —
//! `(workspace_id, session_id)`. That is a cache of the last id, never a
//! transcript, and it is the only reason `acp_prompt` opens the store.
//! `session/load` across two different processes is proven to restore the
//! conversation, which is why one process per call is enough and a
//! session/process pool stays out of scope.
//!
//! **Reuse is by mapping and never by guessing** (a cold ACP session costs the
//! agent's whole system prompt again, ~12k input tokens, while a loaded one
//! mostly hits its prompt cache). One workspace routinely hosts several
//! concurrent e-agent sessions, so "the newest ACP session for this cwd" is
//! NOT usable: it could attach another session's reviewer history and the
//! reviewer would answer from a conversation nobody can see. The id is
//! therefore read and written per session:
//!
//! 1. in-process cache hit for this session with the same `workspace` →
//!    `session/load`.
//! 2. else the stored `acp_session_id`: non-null → `session/load` when the
//!    agent advertises `agentCapabilities.loadSession` (the row is already
//!    scoped to this workspace), else `session/new` with one line naming the
//!    id that stays unused; null (or a store error) → `session/new` plus one
//!    line saying no saved session existed.
//! 3. `fresh: true` → `session/new` regardless, overwriting the stored value.
//!
//! The id is written after a successful `session/new` and after a successful
//! `session/load`, so reuse survives an e-agent restart. It is never shared
//! between e-agent sessions, and a workspace argument that differs from the
//! cached one starts a new conversation rather than silently reusing an id
//! bound to another cwd. A failed `session/load` falls back to `session/new`
//! VISIBLY — exactly one line naming the abandoned id and the new one. Silent
//! fallback is the one thing that must not happen: the caller would believe
//! the reviewer still remembers earlier turns when it does not. Starting over
//! is fine; pretending is not.
//!
//! **Permission policy: ask the human when there is one, never edit
//! silently.** A `session/request_permission` request is answered by the human
//! whenever this session has a user-facing frontend: it becomes ONE blocking
//! question (through `SessionHandle::ask_user`, so the runner parks the turn
//! exactly as it does for `request_user_input`) whose options ARE the ACP
//! options — `optionId` → `id`, `name` → `label`, `kind` → `description`,
//! nothing added — and the chosen id is sent back as the selected `optionId`.
//! An unanswered, cancelled or aborted question, and an id ACP did not offer,
//! all resolve as `cancelled`, so the agent is never left waiting.
//!
//! A session with NO frontend (a delegate / btw / fork child, or a tool bound
//! outside the runner) keeps the kind-based policy: the request is allowed
//! ("allow once") only when its `toolCall.kind` is in `[acp] allow_kinds`
//! (default `read`, `search`, `fetch`, `think`); everything else (edit /
//! delete / move / execute / anything unknown) is denied with `cancelled` and
//! reported in the result. As an oracle this agent should read and search
//! freely, but a reviewer that silently edits the workspace is not a reviewer.
//! `[acp] danger = true` allows every kind instead and says so once in the
//! result. A child never asks: suspending there would park a turn nobody can
//! answer.
//!
//! **Every ask has its own identity, and a dead child never parks the tool.**
//! The question's `call_id` is not the JSON-RPC id (every call spawns a fresh
//! process, so an agent happily reuses `9001`): it is
//! `acp-permission-<nonce>-<toolCallId>-<digest>` — a fresh random nonce per
//! ask, the ACP tool-call id, and a digest of the requested action plus the
//! offered options. A late answer therefore always names the ask it was given
//! for; the tool keeps the identity of the ask still awaiting an answer and
//! drops any answer that is not for it, so request N's approval can never
//! authorize request N+1. The answer is awaited concurrently with the agent's
//! exit and with cancellation (`tokio::select!`): the read loop never blocks on
//! a human, so a child that dies with a request pending closes stdout, which
//! wakes the parked ask, drops its pending entry, and fails the in-flight
//! `session/prompt` — the tool returns a plain error string to the model
//! instead of hanging until a timeout. An answer the tool decides by itself
//! (the kind policy, `[acp] danger`) selects the agent's own "allow once"
//! option; the human path sends the chosen `optionId` verbatim, so an
//! `allow_always` choice is never rewritten into an `allow_once` grant (or the
//! other way round) — every id is used exactly as the agent offered it.
//!
//! **`session/load` is used only when the agent advertises it.** The
//! `agentCapabilities.loadSession` flag from `initialize` is recorded, and an
//! agent that does not set it gets `session/new` (plus one visible line naming
//! the id left unused) instead. A load replays the restored history as
//! `session/update` notifications before its response; the read loop processes
//! them in stream order ahead of that response, and every chunk that arrives
//! while the replay phase is open is discarded, so restored history is never
//! mistaken for — or allowed to crowd out — this call's answer. Reuse always
//! runs in the `cwd` of the current call, cold (store-hit) reuse included: the
//! agent is spawned in it and `session/load` is asked for it.
//!
//! Non-goals (deliberately not implemented): a session/process pool, streaming
//! `session/update` notifications to the UI (they are accumulated and returned
//! as one result), MCP injection (`mcpServers` is always `[]`), a permission UI
//! of its own (the request reuses the existing `WaitingInput` question
//! frontends), remembered grants (every request is answered on its own),
//! `session/close`, `session/cancel`, `session/set_mode`, several prompts
//! inside one call, and any `session/list` based discovery (it cannot tell two
//! concurrent e-agent sessions in one workspace apart).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot, watch};

use super::spec;
use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::config::AcpConfig;
use crate::runner::{SessionHandle, UserInputRequest, UserOption, UserQuestion};
use crate::session_store::SessionStore;
use crate::workspace::Workspace;

/// Protocol version announced in `initialize` (`protocolVersion: 1`).
const PROTOCOL_VERSION: u32 = 1;
/// Budget for one handshake request (`initialize`, `session/new`,
/// `session/load`, `session/set_model`).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for one `session/prompt`. A prompt legitimately takes minutes,
/// so this only breaks a wedged child; cancelling the turn is the real escape
/// hatch (it drops this future, which kills the process group).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Kept stderr tail, attached to errors: an agent CLI prints its login
/// guidance there rather than inside a JSON-RPC error.
const STDERR_LIMIT: usize = 4 * 1024;
/// Per-buffer cap on accumulated assistant/thought text: a runaway agent must
/// not turn one tool result into a megabyte.
const TEXT_LIMIT: usize = 64 * 1024;
/// Cap on a `toolCall` summary quoted in a permission line.
const SUMMARY_LIMIT: usize = 120;
/// Cap on the `rawInput.command` quoted in a permission question.
const COMMAND_LIMIT: usize = 200;
/// The question id every permission ask uses (one question per request).
const PERMISSION_QUESTION: &str = "permission";
/// Cache bucket of a tool that was never bound to an e-agent session (direct
/// `Agent::run`, tests): process-local only, never written to disk.
const UNBOUND_SESSION: &str = "<unbound>";

/// The registered `acp_prompt` tool. Main session only — the session factory
/// decides (see `session_factory::register_acp`), so delegate/fork children
/// never inherit it.
pub(crate) fn acp_tool(
    workspace: Workspace,
    config: AcpConfig,
    store: SessionStore,
    root: PathBuf,
    session: &str,
) -> Box<dyn Tool> {
    Box::new(AcpPrompt {
        workspace,
        config,
        store,
        root,
        session: session.to_owned(),
        cached: StdMutex::new(HashMap::new()),
        handle: None,
    })
}

const DESCRIPTION: &str = "Run ONE prompt through an external ACP (Agent Client Protocol) agent \
     over stdio (default `agy acp --stdio`) and return its answer. This is a REVIEW oracle: calls \
     in one e-agent session automatically reuse the same ACP conversation (`session/load`), so the \
     agent remembers earlier calls in the same workspace; a changed workspace or `fresh: true` \
     starts a new conversation, and the result says so. Each call still starts and kills its own \
     agent process, and the agent works with the same ambient authority as `bash` without the \
     sandbox — its file changes are NOT cap-std confined. A permission request is forwarded to the \
     user as one question carrying the agent's own options when this session has a frontend; from a \
     session without one (a subagent) only read-only tool kinds (read/search/fetch/think) are \
     allowed, and edits, deletes, moves and command execution are denied and reported in the result.";

struct AcpPrompt {
    workspace: Workspace,
    config: AcpConfig,
    /// The session store holding `sessions.acp_session_id`: the ACP id is
    /// read from there on the first call of a session and written back after
    /// every `session/new` / `session/load`. The tool never opens a database
    /// itself.
    store: SessionStore,
    /// The workspace root the store is scoped to (its `workspace_id`).
    root: PathBuf,
    /// The e-agent session id, bound by the runner (`set_session_handle`).
    /// Empty until then, which shares one anonymous, never-persisted bucket.
    session: String,
    /// The in-process cache: e-agent session id → the ACP session id last
    /// used for it, plus the workspace that id is bound to. The store is the
    /// durable copy; this only skips the read within one process.
    cached: StdMutex<HashMap<String, Cached>>,
    /// The e-agent session this call belongs to (bound by the runner's
    /// `set_session_handle`). `None` (a tool built outside a runner), or a
    /// session whose `ask_user` refuses, means there is no frontend to ask:
    /// permission requests then keep the kind-based policy. A child
    /// (delegate/btw/fork) session refuses because it has no frontend.
    handle: Option<SessionHandle>,
}

#[derive(Clone)]
struct Cached {
    workspace: PathBuf,
    session_id: String,
}

/// What one call decided to open, resolved before any process spawns.
enum Open {
    /// `session/new`.
    New,
    /// `session/load` the id this e-agent session last used.
    Load(String),
}

/// The decision for one call plus the notice it must leave in the result.
struct Plan {
    open: Open,
    notice: Option<Notice>,
}

/// A status line the result must carry. Each variant names what happened and
/// the session now in use.
#[derive(Clone)]
enum Notice {
    /// The mapped ACP session belongs to another workspace.
    WorkspaceChanged { from: PathBuf, to: PathBuf },
    /// A `session/load` failed, so a new session was started.
    LoadFailed { id: String, reason: String },
    /// The session store had no ACP id for this session yet.
    Absent,
}

#[async_trait]
impl Tool for AcpPrompt {
    fn spec(&self) -> ToolSpec {
        spec(
            "acp_prompt",
            DESCRIPTION,
            json!({
                "prompt": {"type": "string", "description": "The complete task for the external agent. It remembers earlier calls in this workspace, so a follow-up can refer back to them; pass `fresh: true` to start over."},
                "workspace": {"type": "string", "description": "Working directory (`cwd`) for the agent: absolute, or relative to YOUR workspace root. It must already exist and stay inside the workspace or an authorized external directory. Defaults to your workspace root. Changing it starts a new conversation, because an ACP session is bound to its cwd."},
                "fresh": {"type": "boolean", "description": "Start a NEW conversation instead of resuming (`session/new`) — use it when the previous one is off track or its session is gone."},
            }),
            &["prompt"],
        )
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if prompt.is_empty() {
            return Err("`prompt` must be a non-empty string".into());
        }
        let cwd = self.cwd(&arguments)?;
        let fresh = arguments
            .get("fresh")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let plan = self.plan(&cwd, fresh).await;
        let run = self.run(plan, prompt, &cwd, self.handle.clone());
        match tokio::time::timeout(PROMPT_TIMEOUT, run).await {
            Ok(result) => result.map(ToolOutput::text),
            // The deadline drops `run`, which drops the client and thereby
            // kills the agent's process group.
            Err(_) => Err(format!(
                "acp: the agent did not finish within {}s; its process group was killed",
                PROMPT_TIMEOUT.as_secs()
            )),
        }
    }

    fn set_session_handle(&mut self, session_id: &str, handle: crate::runner::SessionHandle) {
        self.session = session_id.to_owned();
        self.handle = Some(handle);
    }
}

impl AcpPrompt {
    /// Resolve the `workspace` argument: the tool's own root by default,
    /// otherwise a `reroot`ed child (the same capability rule `delegate`
    /// uses), so a path outside the workspace or an authorized external
    /// directory is refused before anything spawns.
    fn cwd(&self, arguments: &Value) -> Result<PathBuf, String> {
        let Some(raw) = arguments.get("workspace").and_then(Value::as_str) else {
            return Ok(self.workspace.root().to_path_buf());
        };
        let workspace = self
            .workspace
            .reroot(self.workspace.root().join(raw))
            .map_err(|error| format!("invalid `workspace` path `{raw}`: {error}"))?;
        if !workspace.root().is_dir() {
            return Err(format!("`workspace` `{raw}` is not a directory"));
        }
        Ok(workspace.root().to_path_buf())
    }

    fn bucket(&self) -> &str {
        if self.session.is_empty() {
            UNBOUND_SESSION
        } else {
            &self.session
        }
    }

    /// Decode how this call reaches the ACP session it may continue.
    async fn plan(&self, cwd: &Path, fresh: bool) -> Plan {
        if fresh {
            return Plan {
                open: Open::New,
                notice: None,
            };
        }
        let cached = self.cached.lock().unwrap().get(self.bucket()).cloned();
        if let Some(cached) = &cached
            && cached.workspace == cwd
        {
            return Plan {
                open: Open::Load(cached.session_id.clone()),
                notice: None,
            };
        }
        if let Some(cached) = &cached {
            // An ACP session is bound to its cwd, and the stored id belongs
            // to that workspace: reusing it for a different one would attach
            // a conversation nobody can see.
            return Plan {
                open: Open::New,
                notice: Some(Notice::WorkspaceChanged {
                    from: cached.workspace.clone(),
                    to: cwd.to_path_buf(),
                }),
            };
        }
        // No in-process entry (first call of this e-agent session, or a
        // restart): ask the session store. The row is `(workspace_id,
        // session_id)`, so another e-agent session's reviewer can never be
        // picked up here — unlike any "most recent for this cwd" guess.
        let stored = match self.store.acp_session_id(&self.root, &self.session).await {
            Ok(stored) => stored,
            Err(error) => {
                // A store hiccup must not fail the review: start fresh.
                tracing::warn!("e-agent: acp: cannot read the stored ACP session id: {error:#}");
                None
            }
        };
        match stored
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
        {
            Some(id) => Plan {
                open: Open::Load(id),
                notice: None,
            },
            None => Plan {
                open: Open::New,
                notice: Some(Notice::Absent),
            },
        }
    }

    /// Record the id in the in-process cache and in the session store. A
    /// failed write is logged, never fatal: a review must not fail because
    /// the mapping could not be saved.
    async fn remember(&self, cwd: &Path, session_id: &str) {
        self.cached.lock().unwrap().insert(
            self.bucket().to_owned(),
            Cached {
                workspace: cwd.to_path_buf(),
                session_id: session_id.to_owned(),
            },
        );
        if self.session.is_empty() {
            // Unbound (direct `Agent::run`, tests): there is no row to write.
            return;
        }
        if let Err(error) = self
            .store
            .set_acp_session_id(&self.root, &self.session, Some(session_id))
            .await
        {
            tracing::warn!("e-agent: acp: cannot store the ACP session id: {error:#}");
        }
    }

    /// One process, one prompt: handshake (new or loaded session), remember
    /// the id, `session/prompt`, receipt.
    async fn run(
        &self,
        plan: Plan,
        prompt: &str,
        cwd: &Path,
        handle: Option<SessionHandle>,
    ) -> Result<String, String> {
        let client = Client::connect(&self.config, cwd, &plan, handle).await?;
        // Remembered BEFORE the prompt: a failed prompt must not orphan a
        // freshly created session id.
        self.remember(cwd, &client.session_id).await;
        // A `session/load` replay is fully behind us once its response was
        // used (the read loop reads in stream order), so from here on an
        // update is this call's answer, never restored history.
        client.replay_finished();
        let result = client
            .request(
                "session/prompt",
                json!({
                    "sessionId": client.session_id,
                    "prompt": [{"type": "text", "text": prompt}],
                }),
                PROMPT_TIMEOUT,
            )
            .await;
        let stop_reason = match result {
            Ok(result) => result
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            Err(error) => return Err(client.describe(error)),
        };
        Ok(client.receipt(&stop_reason))
    }
}

/// One live agent process plus its JSON-RPC plumbing.
struct Client {
    /// Kills the process group when the client drops: a finished prompt (one
    /// process per call) and a cancelled turn both end the child here.
    _child: KillOnDrop,
    /// Dropped with the client: a parked permission answer sees the closed
    /// channel and stops waiting, so a cancelled turn leaves nothing behind.
    _cancel: watch::Sender<()>,
    shared: Arc<Shared>,
    /// `agentInfo.name` / `agentInfo.version` from `initialize`, so the
    /// receipt names the binary that actually answered.
    agent: (String, String),
    /// `agentCapabilities.loadSession` from `initialize`: `session/load` is
    /// only ever called when the agent advertised it.
    load_supported: bool,
    session_id: String,
    /// Status lines the result must carry (abandoned or unknown sessions).
    notices: Vec<Notice>,
    /// Bounded stderr tail, surfaced in error messages.
    stderr: Arc<StdMutex<String>>,
}

impl Client {
    async fn connect(
        config: &AcpConfig,
        cwd: &Path,
        plan: &Plan,
        handle: Option<SessionHandle>,
    ) -> Result<Self, String> {
        let mut client = Self::spawn(config, cwd, plan, handle)?;
        if let Err(error) = client.handshake(config, cwd, &plan.open).await {
            // The child dies with `client` on this early return.
            return Err(client.describe(error));
        }
        Ok(client)
    }

    /// Spawn the configured agent, wire the pipes, and start the reader task.
    fn spawn(
        config: &AcpConfig,
        cwd: &Path,
        plan: &Plan,
        handle: Option<SessionHandle>,
    ) -> Result<Self, String> {
        let mut command = Command::new(&config.binary);
        command
            .args(&config.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Drain stderr so the child cannot deadlock on a full pipe.
            .stderr(Stdio::piped());
        // Own process group, so cancelling also kills the agent's children
        // (the bash / run_rust precedent).
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| {
            format!(
                "acp: cannot start `{}`: {error} (set `[acp] binary` / `args`)",
                config.binary
            )
        })?;
        let stdin = child.stdin.take().ok_or("acp: the agent has no stdin")?;
        let stdout = child.stdout.take().ok_or("acp: the agent has no stdout")?;
        let stderr = child.stderr.take().ok_or("acp: the agent has no stderr")?;
        let stderr = drain_stderr(stderr);
        let (gone, _) = watch::channel(false);
        let (cancel, cancelled) = watch::channel(());
        let shared = Arc::new(Shared {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(Pending::default()),
            next_id: AtomicU64::new(1),
            state: StdMutex::new(State::default()),
            asking: StdMutex::new(PendingPermission::default()),
            replaying: AtomicBool::new(false),
            gone,
            cancel: cancelled,
            allow_kinds: config.allow_kinds.clone(),
            danger: config.danger,
            handle,
        });
        tokio::spawn(read_loop(shared.clone(), stdout));
        Ok(Self {
            _child: KillOnDrop(child),
            _cancel: cancel,
            shared,
            agent: ("unknown".into(), String::new()),
            load_supported: false,
            session_id: String::new(),
            notices: plan.notice.clone().into_iter().collect(),
            stderr,
        })
    }

    /// `initialize` → `session/new` | `session/load` → optional
    /// `session/set_model`.
    async fn handshake(
        &mut self,
        config: &AcpConfig,
        cwd: &Path,
        open: &Open,
    ) -> Result<(), String> {
        let initialize = self
            .shared
            .request(
                "initialize",
                json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {}}),
                CALL_TIMEOUT,
            )
            .await?;
        let info = initialize.get("agentInfo");
        let field = |name: &str| {
            info.and_then(|info| info.get(name))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        self.agent = (field("name"), field("version"));
        if self.agent.0.is_empty() {
            self.agent.0 = "unknown".into();
        }
        // Record the negotiated capability: `session/load` is not part of the
        // protocol for an agent that does not advertise it, so asking anyway
        // would be an unanswered (or erroring) request.
        self.load_supported = initialize
            .get("agentCapabilities")
            .and_then(|caps| caps.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cwd_text = cwd.display().to_string();
        self.session_id = match open {
            Open::New => self.new_session(&cwd_text).await?,
            Open::Load(id) if self.load_supported => match self.load_session(id, &cwd_text).await {
                Ok(()) => id.clone(),
                Err(reason) => {
                    // Visible fallback: the call continues on a NEW session,
                    // and the result says which id was abandoned.
                    self.notices.push(Notice::LoadFailed {
                        id: id.clone(),
                        reason: one_line(&reason),
                    });
                    self.new_session(&cwd_text).await?
                }
            },
            // The agent never advertised `session/load`: start a new session
            // and say which id stays unused. The caller persists the fresh id
            // exactly like any other new one.
            Open::Load(id) => {
                self.notices.push(Notice::LoadFailed {
                    id: id.clone(),
                    reason: "the agent does not support `session/load` \
                             (`agentCapabilities.loadSession` is false)"
                        .into(),
                });
                self.new_session(&cwd_text).await?
            }
        };
        if let Some(model) = &config.model {
            self.shared
                .request(
                    "session/set_model",
                    json!({"sessionId": self.session_id, "modelId": model}),
                    CALL_TIMEOUT,
                )
                .await?;
        }
        Ok(())
    }

    async fn new_session(&self, cwd: &str) -> Result<String, String> {
        let session = self
            .shared
            .request(
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
                CALL_TIMEOUT,
            )
            .await?;
        session
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "acp: `session/new` returned no `sessionId`".to_owned())
    }

    async fn load_session(&self, id: &str, cwd: &str) -> Result<(), String> {
        // The replay phase opens BEFORE the request goes out and stays open
        // until [`Self::replay_finished`]: every `session/update` chunk the
        // agent sends to restore history arrives in this window, is read in
        // stream order ahead of the load response, and is dropped instead of
        // being accumulated as this call's answer.
        self.shared.replaying.store(true, Ordering::Relaxed);
        self.shared
            .request(
                "session/load",
                json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                CALL_TIMEOUT,
            )
            .await
            .map(|_| ())
    }

    /// Close the replay phase: from here on, `session/update` text is this
    /// call's answer. Called once the load response has been used (or a new
    /// session started), right before `session/prompt` is sent — nothing else
    /// can produce answer text in between.
    fn replay_finished(&self) {
        self.shared.replaying.store(false, Ordering::Relaxed);
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.shared.request(method, params, timeout).await
    }

    /// Attach the agent's stderr tail to a model-facing error.
    fn describe(&self, error: String) -> String {
        append_stderr(error, &self.stderr)
    }

    /// The model-facing tool result: `agentInfo` plus session id header, any
    /// session status notice, the assistant text (thought text only as a
    /// fallback), the `stopReason`, and every permission decision.
    fn receipt(&self, stop_reason: &str) -> String {
        let state = self.shared.state.lock().unwrap().clone();
        let agent = if self.agent.1.is_empty() {
            self.agent.0.clone()
        } else {
            format!("{} {}", self.agent.0, self.agent.1)
        };
        let mut out = format!("acp: {agent} · session {}\n", self.session_id);
        for notice in &self.notices {
            out.push_str(&render_notice(notice, &self.session_id));
            out.push('\n');
        }
        // Assistant text wins; thought text is the fallback when the agent
        // streamed no answer text at all.
        let body = if state.assistant.trim().is_empty() {
            &state.thought
        } else {
            &state.assistant
        };
        if body.trim().is_empty() {
            out.push_str("(the agent returned no text)\n");
        } else {
            out.push_str(body.trim_end());
            out.push('\n');
        }
        out.push_str(&format!("stopReason: {stop_reason}"));
        if state.danger_allowed {
            out.push_str("\ndanger: allowed every permission request ([acp] danger = true)");
        }
        for decision in &state.permissions {
            out.push('\n');
            out.push_str(decision);
        }
        if state.ignored > 0 {
            out.push_str(&format!("\nignored updates: {}", state.ignored));
        }
        if state.truncated {
            out.push_str(&format!(
                "\n[text truncated: one buffer reached {TEXT_LIMIT} bytes]"
            ));
        }
        out
    }
}

/// Kills the whole agent process group when dropped: the prompt finished (one
/// process per call, no reuse) or the turn was cancelled (the tool future was
/// dropped). Unix spawns the child as its own group leader, so SIGKILL to the
/// group also ends the agent's grandchildren; Windows degrades to the
/// top-level process (the bash precedent).
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(group) = self
                .0
                .id()
                .and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
            {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
        }
        #[cfg(not(unix))]
        let _ = self.0.start_kill();
    }
}

/// One in-flight request: the method (a dead child's error names it) and the
/// channel its reply is delivered on.
struct PendingRequest {
    method: String,
    sender: oneshot::Sender<Result<Value, String>>,
}

type Pending = HashMap<u64, PendingRequest>;

/// The permission ask whose answer is still awaited. One slot: ACP answers one
/// request at a time and the runner allows one open question, so the slot holds
/// the *identity* of the ask — never just an id an agent may repeat — and an
/// answer that arrives after a newer ask was registered is dropped instead of
/// authorizing it.
#[derive(Default)]
struct PendingPermission {
    current: Option<String>,
}

impl PendingPermission {
    /// Make `identity` the only ask an answer may still resolve.
    fn claim(&mut self, identity: &str) {
        self.current = Some(identity.to_owned());
    }

    /// Release `identity` from the slot. `true` when it was still the ask
    /// awaiting an answer; `false` means a newer ask owns the slot, so the
    /// answer belongs to a superseded request and must be discarded.
    fn release(&mut self, identity: &str) -> bool {
        if self.current.as_deref() == Some(identity) {
            self.current = None;
            true
        } else {
            false
        }
    }
}

/// Everything the reader task and the request side both need: writes to the
/// child, the in-flight request table, and the accumulated notification state.
struct Shared {
    stdin: Mutex<ChildStdin>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    state: StdMutex<State>,
    /// The permission ask currently awaiting a human answer (its identity).
    asking: StdMutex<PendingPermission>,
    /// Set while a `session/load` replay may still be arriving: those
    /// `session/update` chunks are restored history, never answer text.
    replaying: AtomicBool,
    /// Flipped when the child's stdout closes (the agent is gone): a parked
    /// permission answer stops waiting and every in-flight request fails.
    gone: watch::Sender<bool>,
    /// Closed when the `Client` drops (the tool future was cancelled or the
    /// call finished): a parked permission answer stops waiting.
    cancel: watch::Receiver<()>,
    /// `[acp] allow_kinds`: tool kinds answered "allow once".
    allow_kinds: Vec<String>,
    /// `[acp] danger`: answer every permission request "allow once".
    danger: bool,
    /// The e-agent session handle used to forward a permission request to the
    /// human as a question. `None` keeps the kind-based policy.
    handle: Option<SessionHandle>,
}

impl Shared {
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(
            id,
            PendingRequest {
                method: method.to_owned(),
                sender,
            },
        );
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.send(&message).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "acp: the agent closed the connection during `{method}`"
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!(
                    "acp: `{method}` timed out after {}s",
                    timeout.as_secs()
                ))
            }
        }
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

    /// Route one received frame: a server request (only a permission request
    /// is expected), a response to one of our requests, or a notification.
    async fn dispatch(self: &Arc<Self>, value: Value) {
        let id = value.get("id").cloned().filter(|id| !id.is_null());
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match (id, method) {
            // A permission request becomes one human question, awaited in its
            // OWN task: the read loop must keep reading while a human decides,
            // otherwise a child that dies with a request pending could never
            // be noticed (its exit is only visible on stdout).
            (Some(id), Some(method)) if method == "session/request_permission" => {
                let shared = Arc::clone(self);
                let params = value.get("params").cloned();
                tokio::spawn(async move {
                    let reply = shared.answer_permission_request(params, &id).await;
                    let _ = shared.send(&reply).await;
                });
            }
            // Any other server request: an unanswered request would hang the
            // agent, so it is refused with -32601.
            (Some(id), Some(method)) => {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("method `{method}` is not supported")},
                });
                let _ = self.send(&reply).await;
            }
            // A response to one of our requests. Replies may arrive out of
            // order, so the id is the only routing key.
            (Some(id), None) => {
                let Some(id) = id.as_u64() else { return };
                let Some(request) = self.pending.lock().await.remove(&id) else {
                    return;
                };
                let result = match value.get("error") {
                    Some(error) => Err(rpc_error(error)),
                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = request.sender.send(result);
            }
            (None, Some(method)) => self.record_notification(&method, value.get("params")),
            (None, None) => {}
        }
    }

    /// Answer one `session/request_permission` with the human when this
    /// session has a frontend, and by tool kind otherwise.
    ///
    /// With a frontend the request is forwarded through `ask_user` as ONE
    /// question whose options ARE the ACP options (one-to-one: `optionId` →
    /// `id`, `name` → `label`, `kind` → `description`); the chosen id is what
    /// ACP receives back. Nothing is invented: a request that offers no usable
    /// option, an unanswerable one (no frontend, a child session, a dead
    /// session), and an answer that names no offered id all fall back to
    /// `cancelled`/the kind policy rather than guessing. The answer is used
    /// only while this ask is still the pending one, and it is awaited in its
    /// own task so the read loop keeps reading.
    async fn answer_permission_request(&self, params: Option<Value>, id: &Value) -> Value {
        let params = params.as_ref();
        let tool_call = params.and_then(|params| params.get("toolCall"));
        let Some(handle) = &self.handle else {
            return self.answer_permission(params, id);
        };
        let Some(options) = question_options(params) else {
            // ACP offered no usable option: there is nothing to ask.
            return self.answer_permission(params, id);
        };
        // The question's id IS the ask's identity: unique per request (fresh
        // nonce) and covering what is being authorized, so the answer the
        // frontend sends back can only ever be an answer to THIS ask.
        let identity = permission_identity(params);
        let request = UserInputRequest {
            call_id: identity.clone(),
            questions: vec![UserQuestion {
                id: PERMISSION_QUESTION.to_owned(),
                prompt: permission_prompt(tool_call),
                options: options.clone(),
            }],
        };
        self.asking.lock().unwrap().claim(&identity);
        // `ask_user` refuses (no frontend / not live): the kind policy owns
        // this request, and a child session never surfaces a question.
        let Ok(answer) = handle.ask_user(request) else {
            self.asking.lock().unwrap().release(&identity);
            return self.answer_permission(params, id);
        };
        // Wait for the answer CONCURRENTLY with the ways it can never arrive:
        // the agent's exit (stdout closed) and the cancellation of this turn
        // (the `Client`, hence the `cancel` sender, dropped). Either one drops
        // the pending entry and answers `cancelled` instead of parking this
        // ask — and the call — forever.
        let chosen = tokio::select! {
            biased;
            answer = answer => answer.unwrap_or(None),
            _ = self.agent_gone() => {
                self.asking.lock().unwrap().release(&identity);
                return cancelled_permission(id);
            }
            _ = self.cancelled() => {
                self.asking.lock().unwrap().release(&identity);
                return cancelled_permission(id);
            }
        };
        if !self.asking.lock().unwrap().release(&identity) {
            // A newer ask owns the slot: this answer belongs to a superseded
            // request (an agent may reuse a JSON-RPC id and a tool-call id
            // call after call), so it is dropped rather than allowed to
            // authorize the request that is pending now.
            return cancelled_permission(id);
        }
        let kind = text_field(tool_call, "kind").unwrap_or("unknown");
        let summary = clip(&tool_call_summary(tool_call));
        let mut state = self.state.lock().unwrap();
        // A selection needs no receipt line: ACP's own option (allow or
        // reject) decides what happens, and the agent reports that itself.
        // `cancelled` is different — the request was never decided, so the
        // model is told a human (or the end of the question) stopped it.
        let outcome = match chosen.filter(|chosen| options.iter().any(|o| o.id == *chosen)) {
            Some(option_id) => json!({"outcome": {"outcome": "selected", "optionId": option_id}}),
            None => {
                // No answer, a cancellation/abort, or an id outside the
                // offered options (the runner already rejects those): ACP is
                // told `cancelled` and the receipt names the blocker.
                state
                    .permissions
                    .push(format!("denied {kind} by the user: {summary}"));
                json!({"outcome": {"outcome": "cancelled"}})
            }
        };
        drop(state);
        json!({"jsonrpc": "2.0", "id": id.clone(), "result": outcome})
    }

    /// Resolves once the agent's stdout closed (see [`Shared::child_gone`]).
    async fn agent_gone(&self) {
        let mut gone = self.gone.subscribe();
        if *gone.borrow() {
            return;
        }
        let _ = gone.changed().await;
    }

    /// Resolves when the `Client` is dropped: the tool future was cancelled
    /// (turn cancel, deadline) or the call already finished.
    async fn cancelled(&self) {
        let mut cancel = self.cancel.clone();
        let _ = cancel.changed().await;
    }

    /// The agent's stdout closed: the child is gone or going, so nothing that
    /// waits for it may keep waiting. The parked permission ask is woken (its
    /// pending entry is dropped by [`Self::answer_permission_request`]), and
    /// every in-flight request fails with a plain error — a `session/prompt`
    /// reply that can never arrive must reach the model as an error instead of
    /// hanging until its timeout.
    async fn child_gone(&self) {
        self.gone.send_replace(true);
        let mut pending = self.pending.lock().await;
        for (_, request) in pending.drain() {
            let _ = request.sender.send(Err(format!(
                "acp: the agent exited during `{}`",
                request.method
            )));
        }
    }

    /// Answer one `session/request_permission` by tool kind: the kinds in
    /// `[acp] allow_kinds` (and every kind under `danger`) get the agent's
    /// "allow once" option, everything else is denied with `cancelled` and
    /// recorded for the receipt.
    fn answer_permission(&self, params: Option<&Value>, id: &Value) -> Value {
        let tool_call = params.and_then(|params| params.get("toolCall"));
        let kind = text_field(tool_call, "kind").unwrap_or("unknown");
        let summary = clip(&tool_call_summary(tool_call));
        let allowed_kind =
            !kind.is_empty() && self.allow_kinds.iter().any(|allowed| allowed == kind);
        let option = (self.danger || allowed_kind)
            .then(|| auto_allow_option(params))
            .flatten();
        let mut state = self.state.lock().unwrap();
        let outcome = match option {
            Some(option_id) => {
                // Allowed requests stay OUT of the receipt (a review call
                // makes dozens of reads); only `danger` leaves one line.
                state.danger_allowed |= self.danger;
                json!({"outcome": {"outcome": "selected", "optionId": option_id}})
            }
            None => {
                let reason = if self.danger || allowed_kind {
                    format!("denied {kind} (no allow option offered): {summary}")
                } else {
                    format!("denied {kind}: {summary}")
                };
                state.permissions.push(reason);
                json!({"outcome": {"outcome": "cancelled"}})
            }
        };
        drop(state);
        json!({"jsonrpc": "2.0", "id": id.clone(), "result": outcome})
    }

    /// `session/update` notifications: message and thought chunks are
    /// accumulated (kept apart), every other update kind is only counted.
    /// Chunks that belong to a `session/load` replay are restored history and
    /// are dropped entirely: they never become this call's answer, and a long
    /// replay can therefore never crowd the answer out.
    fn record_notification(&self, method: &str, params: Option<&Value>) {
        if method != "session/update" || self.replaying.load(Ordering::Relaxed) {
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
        let mut state = self.state.lock().unwrap();
        match kind {
            "agent_message_chunk" => {
                state.truncated |= push_bounded(&mut state.assistant, text, TEXT_LIMIT)
            }
            "agent_thought_chunk" => {
                state.truncated |= push_bounded(&mut state.thought, text, TEXT_LIMIT)
            }
            // tool_call / tool_call_update / usage_update /
            // available_commands_update / plan / …: counted, not rendered.
            _ => state.ignored += 1,
        }
    }
}

/// Accumulated `session/update` and permission state for one prompt.
#[derive(Clone, Default)]
struct State {
    /// `agent_message_chunk` text, in arrival order.
    assistant: String,
    /// `agent_thought_chunk` text, kept separate: only a fallback body.
    thought: String,
    /// How many non-text updates arrived (tool_call, usage_update, …).
    ignored: usize,
    /// One line per DENIED request: `denied <kind>: <summary>`.
    permissions: Vec<String>,
    /// At least one request was allowed by `[acp] danger`.
    danger_allowed: bool,
    /// A text buffer hit [`TEXT_LIMIT`].
    truncated: bool,
}

/// Render one session status line, with the session actually in use.
fn render_notice(notice: &Notice, session_id: &str) -> String {
    match notice {
        Notice::WorkspaceChanged { from, to } => format!(
            "[acp] workspace changed from {} to {}; started a new session {session_id}",
            from.display(),
            to.display()
        ),
        Notice::LoadFailed { id, reason } => format!(
            "[acp] could not resume session {id} (load failed: {reason}); started a new session {session_id}"
        ),
        Notice::Absent => format!(
            "[acp] no saved ACP session for this e-agent session; started a new session {session_id}"
        ),
    }
}

/// Read frames until the child closes stdout. Unparsable lines (a banner, a
/// stray log line) are skipped rather than failing the call. A permission
/// answer is awaited in its own task (see `Shared::dispatch`), so closing
/// stdout — the child exiting — is always observed right away.
async fn read_loop(shared: Arc<Shared>, stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        shared.dispatch(value).await;
    }
    // The connection is over: nothing that waits for the agent may keep
    // waiting for a reply it can no longer send.
    shared.child_gone().await;
}

/// Drain stderr so the child cannot block on a full pipe, keeping a bounded
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

/// Collapse an error into one line, so a notice stays exactly one line even
/// when the agent's stderr tail is attached.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Attach the child's stderr tail to a model-facing error.
fn append_stderr(error: String, stderr: &StdMutex<String>) -> String {
    let tail = stderr.lock().unwrap();
    let tail = tail.trim();
    if tail.is_empty() {
        error
    } else {
        format!("{error}\nagent stderr: {tail}")
    }
}

/// Model-facing text for a JSON-RPC error object. `-32000` with an
/// authentication message is the "not logged in" bucket (`agy acp` answers
/// exactly that until a login exists): it is named explicitly so the model
/// tells the user what to fix instead of retrying blindly.
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
/// human answers: `acp-permission-<nonce>-<toolCallId>-<digest>`.
///
/// * `<nonce>` is a fresh random 64-bit value per ask, so two asks never share
///   an identity — not inside one process (an agent reuses JSON-RPC ids call
///   after call) and not across e-agent restarts.
/// * `<toolCallId>` is the ACP tool call the request is about.
/// * `<digest>` fingerprints the requested action and the offered options.
///
/// An answer that comes back therefore *names* the ask it was given for:
/// request N's approval cannot authorize request N+1, and a stale answer is
/// dropped by [`PendingPermission::release`].
fn permission_identity(params: Option<&Value>) -> String {
    let tool_call = params.and_then(|params| params.get("toolCall"));
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
/// options ACP offered. Identical requests share it; anything else — another
/// tool call, another command, another set of choices — does not.
fn action_digest(params: Option<&Value>) -> String {
    let field = |name: &str| {
        params
            .and_then(|params| params.get(name))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut hasher = Sha256::new();
    hasher.update(json!({"toolCall": field("toolCall"), "options": field("options")}).to_string());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The reply for a permission request nothing can answer any more (the agent
/// is gone, the turn was cancelled, or the answer was stale): ACP is told
/// `cancelled`, so the agent is never left waiting on a decision.
fn cancelled_permission(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id.clone(), "result": {"outcome": {"outcome": "cancelled"}}})
}

/// The option id to select when the TOOL decides to allow a request (the kind
/// policy, or `[acp] danger`): the agent's own "allow once" option, then any
/// other `allow_*` option it offered — used verbatim, never substituted for a
/// different id. A request offering neither stays denied. (When a human
/// answers, the chosen `optionId` is sent back exactly as chosen, so an
/// `allow_always` decision is never rewritten into an `allow_once` grant.)
fn auto_allow_option(params: Option<&Value>) -> Option<String> {
    let options = params
        .and_then(|params| params.get("options"))
        .and_then(Value::as_array)?;
    let find = |wanted: &dyn Fn(&str) -> bool| {
        options
            .iter()
            .find(|option| {
                option
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(wanted)
            })
            .and_then(option_id)
    };
    find(&|kind: &str| kind == "allow_once")
        .or_else(|| find(&|kind: &str| kind.starts_with("allow")))
}

fn option_id(option: &Value) -> Option<String> {
    option
        .get("optionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
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

/// The question a permission request becomes: what the agent wants to do —
/// `toolCall.title` plus, when present, the raw command (cut to
/// [`COMMAND_LIMIT`] characters). Exactly one question per request.
fn permission_prompt(tool_call: Option<&Value>) -> String {
    let title = text_field(tool_call, "title").unwrap_or("an unknown tool call");
    match command_of(tool_call) {
        Some(command) => format!("{title}: {}", truncate_command(command)),
        None => title.to_owned(),
    }
}

/// The ACP options of one permission request, mapped one-to-one to question
/// options (`optionId` → `id`, `name` → `label`, `kind` → `description`).
/// `None` when the request offers no usable option id: there is then nothing
/// to ask, and the kind policy answers instead of inventing choices.
fn question_options(params: Option<&Value>) -> Option<Vec<UserOption>> {
    let offered = params?
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

/// One trimmed, non-empty string field of an optional JSON object: a
/// `toolCall` field (`title`/`kind`/`toolCallId`) or an option's
/// `name`/`kind`.
fn text_field<'a>(object: Option<&'a Value>, name: &str) -> Option<&'a str> {
    object
        .and_then(|object| object.get(name))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests;
