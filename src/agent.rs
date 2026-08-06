use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Content of the synthetic error result inserted for every tool call left
/// unanswered by an interrupted turn (cancel, provider error, crash). Also
/// the legacy marker: sessions written before the `synthetic` flag existed
/// (commit 92159c7) recognized these placeholders by this literal text.
const INTERRUPTED: &str = "[turn interrupted before a tool result was produced]";

/// Insert a synthetic error result for every tool call left unanswered by an
/// interrupted turn (cancel, provider error, crash), so the derived context
/// always satisfies the provider's tool_call/tool-result pairing rule.
pub(crate) fn repair_tool_pairs(messages: Vec<Message>) -> Vec<Message> {
    fn flush(pending: &mut Vec<ToolCall>, out: &mut Vec<Message>) {
        for call in pending.drain(..) {
            out.push(Message::Tool {
                call_id: call.id,
                name: call.name,
                content: INTERRUPTED.into(),
                is_error: true,
                synthetic: true,
            });
        }
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    for message in messages {
        match &message {
            Message::Tool {
                call_id,
                synthetic: true,
                ..
            } => {
                // Synthetic placeholder from an interrupted-turn snapshot
                // (e.g. captured in a compaction `retained` window). Skip it
                // without consuming the pending call: the real result may
                // land later (compaction race), and if it never does the
                // final flush re-synthesizes an equivalent placeholder.
                let _ = call_id;
            }
            Message::Tool { call_id, .. } => {
                if pending.iter().any(|call| call.id == *call_id) {
                    pending.retain(|call| &call.id != call_id);
                    out.push(message);
                }
                // Orphan tool result: no pending tool_call matches this
                // call_id (e.g. the same result was already answered by a
                // synthetic entry captured in a compaction snapshot, or the
                // call itself was compacted away). Dropping it keeps the
                // provider's tool_call/tool-result pairing rule intact.
                //
                // Ordering invariant: the real result for a tool_call always
                // lands inside the retained context window, so an orphan here
                // can only be a duplicate of a result that already paired —
                // never a first-time result. If a future change inserts
                // messages between a placeholder and its real result, this
                // drop would silently lose a real result; keep the
                // "real result within the retained window" invariant in mind.
            }
            Message::Assistant(assistant) => {
                flush(&mut pending, &mut out);
                pending = assistant.tool_calls.clone();
                out.push(message);
            }
            Message::System { .. } | Message::User { .. } => {
                flush(&mut pending, &mut out);
                out.push(message);
            }
        }
    }
    flush(&mut pending, &mut out);
    out
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    /// Content SHA-256 hex digest; the file lives at
    /// `<image store>/<hash>` (see [`image_store_dir`]).
    pub hash: String,
    /// MIME type, e.g. `image/png` (sniffed from the extension whitelist).
    pub mime: String,
}

/// Single-image size cap for `read_image` and the REPL `/image` command.
pub const IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;

/// Structured prefix a `read_image` tool result carries so the runner can
/// split the image reference from the display summary:
/// `__EA_IMAGE__<hash>,<mime>__EA_IMAGE_END__<summary>`.
pub const IMAGE_MARKER_START: &str = "__EA_IMAGE__";
pub const IMAGE_MARKER_END: &str = "__EA_IMAGE_END__";

/// Split a tool result that may start with an image marker into the display
/// summary (marker stripped) and the optional [`ImagePart`]. Results without
/// the marker pass through untouched.
pub fn split_image_marker(result: &str) -> (String, Option<ImagePart>) {
    if let Some(rest) = result.strip_prefix(IMAGE_MARKER_START)
        && let Some((hash_mime, summary)) = rest.split_once(IMAGE_MARKER_END)
        && let Some((hash, mime)) = hash_mime.split_once(',')
        && !hash.is_empty()
        && !mime.is_empty()
    {
        (
            summary.to_owned(),
            Some(ImagePart {
                hash: hash.to_owned(),
                mime: mime.to_owned(),
            }),
        )
    } else {
        (result.to_owned(), None)
    }
}

/// The `path` argument of a tool call, for synthetic image-attach messages.
pub fn tool_path_argument(arguments: &str) -> Option<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("path").and_then(Value::as_str).map(str::to_owned))
}

/// Remove image attachments from user messages. Non-vision models cannot
/// consume image parts, so compaction strips them before sending the
/// request (the vision gate would otherwise reject it and lock the session).
pub fn strip_images(messages: &mut [Message]) {
    for message in messages {
        if let Message::User { images, .. } = message {
            images.clear();
        }
    }
}

/// Global content-addressed image store: `$XDG_STATE_HOME/e-agent/images`,
/// falling back to `~/.config/e-agent/images` — the same base the crash
/// directory uses in main.rs. None when neither variable is set.
pub fn image_store_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|x| !x.is_empty()) {
        Some(PathBuf::from(xdg).join("e-agent/images"))
    } else {
        crate::home_dir().map(|home| home.join(".config/e-agent/images"))
    }
}

/// SHA-256 hex digest of `bytes` (the content address used as file name).
pub fn image_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// MIME sniffing from a lowercase extension whitelist: png, jpeg/jpg,
/// webp, gif. None for anything else.
pub fn image_mime_from_extension(path: &str) -> Option<&'static str> {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => Some("image/png"),
        Some("jpeg" | "jpg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        Some("gif") => Some("image/gif"),
        _ => None,
    }
}

/// Store image bytes under their SHA-256 hex hash, creating the store
/// directory as needed. Already-present files are skipped (cross-session
/// dedup). Returns the hash.
pub fn store_image_bytes(store: &Path, bytes: &[u8]) -> Result<String, String> {
    let hash = image_sha256(bytes);
    std::fs::create_dir_all(store)
        .map_err(|error| format!("create image store {}: {error}", store.display()))?;
    let target = store.join(&hash);
    match std::fs::write(&target, bytes) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("write image {}: {error}", target.display())),
    }
    Ok(hash)
}

/// Load stored image bytes for `hash` (wire side). None when the store or
/// the file is missing — the wire then degrades to a text placeholder
/// instead of failing the whole request.
pub fn load_image_bytes(store: Option<&Path>, hash: &str) -> Option<Vec<u8>> {
    let store = store?;
    std::fs::read(store.join(hash)).ok()
}

/// Validate (extension whitelist + size cap) and store an image read from
/// the ambient filesystem. Used by the REPL `/image` command: a human
/// explicitly attaching a file, not a model tool call, so ambient fs access
/// is appropriate (mirrors the crash-directory handling in main.rs).
pub fn attach_image_from_path(path: &str) -> Result<ImagePart, String> {
    let mime = image_mime_from_extension(path).ok_or_else(|| {
        format!("unsupported image type for {path}: expected .png, .jpeg/.jpg, .webp, or .gif")
    })?;
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read image {path}: {error}"))?;
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(format!(
            "image {path} is {} bytes, exceeding the {} MiB limit",
            bytes.len(),
            IMAGE_MAX_BYTES / (1024 * 1024)
        ));
    }
    let store = image_store_dir().ok_or("no image store: XDG_STATE_HOME or HOME is not set")?;
    let hash = store_image_bytes(&store, &bytes)?;
    Ok(ImagePart {
        hash,
        mime: mime.into(),
    })
}

/// Vision gate shared by both wires: user messages with images require a
/// vision-capable model. Non-vision models get a clear error instead of a
/// malformed or silently degraded request.
pub fn ensure_vision_supported(
    model: &str,
    vision: bool,
    messages: &[Message],
) -> anyhow::Result<()> {
    let has_images = messages
        .iter()
        .any(|message| matches!(message, Message::User { images, .. } if !images.is_empty()));
    if has_images && !vision {
        anyhow::bail!("model {model} does not support image input");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// System-prompt-style context (e.g. AGENTS.md or MCP instructions). Sent
    /// to the provider with role "system". Persisted if it ever lands in
    /// history, but the current context prefix is kept out of history.
    System {
        content: String,
    },
    User {
        content: String,
        /// Attached images as content-hash references into the global image
        /// store (never inline base64 in the session). Only the reference is
        /// persisted; the wire layer re-reads the file and encodes it.
        /// `#[serde(default)]` keeps old session files (no `images` field)
        /// loadable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImagePart>,
    },
    Assistant(AssistantMessage),
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
        /// Synthetic interrupted-turn placeholder from a snapshot/compaction
        /// repair, never a real tool result. Used instead of matching the
        /// placeholder text so a real result with identical content is not
        /// mistaken for it.
        #[serde(default)]
        synthetic: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Model reasoning, persisted for display/audit only. Never sent back
    /// to the provider (see WireMessage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A prompt accepted while the runner is busy. Transient UI projection;
    /// kept in the in-memory event log for attach replay, but never persisted
    /// as a `SessionEntry`.
    PromptQueued(String),
    /// The runner consumed the oldest queued prompt. Transient UI projection.
    PromptConsumed,
    UserPrompt(String),
    AssistantText(String),
    AssistantDelta(String),
    ReasoningDelta(String),
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        is_error: bool,
        content: String,
    },
    /// A system-injected notice (background completion, task-kill report)
    /// rendered in the TUI as a dim line.
    Notice(String),
    /// A turn failed. Recorded even when no frontend is attached.
    Error(String),
    /// Emitted on the turn boundary when a background task's completion is
    /// folded into the model context as a `[background task N completed]`
    /// message. Not part of the session event log.
    BackgroundCompleted {
        id: u64,
        output: String,
        label: Option<String>,
    },
    /// A structured background completion notice for TUI scrollback display.
    /// Unlike `BackgroundCompleted` (the transient arrival/drain signal),
    /// this is fanned out to observers and persisted as a
    /// `SessionEntry::BackgroundCompletion`. The TUI renders it once as a
    /// dim line; the model sees the full text via `context()`.
    BackgroundCompletionNotice {
        id: u64,
        output: String,
        label: Option<String>,
    },
    Usage {
        /// Input tokens of the most recent regular turn, approximating the
        /// context window currently in use. Compaction calls do not refresh
        /// this (their input is the pre-compaction context).
        context_input: u64,
        /// Configured model context window in tokens (`None` when the model
        /// profile did not set one); lets clients render a usage percentage.
        context_window: Option<u64>,
        /// Cumulative tokens for this process.
        session: Usage,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDeltaKind {
    Content,
    Reasoning,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One entry in the append-only session history. The model context is
/// derived from the history: the latest compaction summary plus everything
/// after it. Older entries stay persisted for display/audit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        message: Message,
    },
    Compaction {
        summary: String,
        /// The turn kept verbatim for the model context; not rendered again
        /// in the TUI (it duplicates messages already before this entry).
        retained: Vec<Message>,
    },
    /// A system-injected notice (background completion, task-kill report)
    /// rendered in the TUI as a dim line and surfaced to the model as a
    /// user message.
    Notice {
        text: String,
    },
    /// A structured background completion entry. Persisted in the session
    /// log with the full output. The TUI renders a truncated preview; the
    /// model context sees the full text. Backwards-compatible: old
    /// `Notice` entries are read without guessing string prefixes.
    BackgroundCompletion {
        id: u64,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// A new session forked from an existing one: source session id, the
    /// 1-based entry index it was forked at, and the source's event_time +
    /// seq of that entry (provenance; never sent to the provider).
    ForkedFrom {
        source: String,
        at: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_time: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
}

impl From<Message> for SessionEntry {
    fn from(message: Message) -> Self {
        Self::Message { message }
    }
}

/// True for entries that end a completed turn: an assistant message with no
/// pending tool calls, or a compaction. Forking may only cut the history at
/// such a boundary, so the forked session never starts mid-turn.
/// `pub(crate)` because the Web fork-candidates endpoint (`src/server.rs`)
/// lists exactly these positions so the frontend only ever forks at a
/// boundary.
pub(crate) fn is_turn_boundary(entry: &SessionEntry) -> bool {
    matches!(entry, SessionEntry::Compaction { .. })
        || matches!(
            entry,
            SessionEntry::Message {
                message: Message::Assistant(assistant),
            } if assistant.tool_calls.is_empty()
        )
}

/// The history prefix a fork keeps: `entries[0..=boundary]`, where
/// `boundary` is the last completed-turn boundary when `at` is None, or
/// exactly `at - 1` (1-based, inclusive) when `at` is given. Trailing
/// `Notice` / `BackgroundCompletion` / `ForkedFrom` entries after the
/// boundary are dropped.
///
/// Errors (returned as plain strings, model-facing style):
/// - empty session, or no completed turn to fork at;
/// - `at` out of range (1-based, so 0 is rejected too);
/// - `at` pointing at an entry that is not a turn boundary.
pub fn fork_prefix(
    entries: &[SessionEntry],
    at: Option<usize>,
) -> Result<Vec<SessionEntry>, String> {
    if entries.is_empty() {
        return Err("no completed turn in session".into());
    }
    let boundary = match at {
        Some(n) => {
            if n == 0 || n > entries.len() {
                return Err(format!(
                    "fork point {n} is out of range: session has {} entries",
                    entries.len()
                ));
            }
            n - 1
        }
        None => match entries.iter().rposition(is_turn_boundary) {
            Some(index) => index,
            None => return Err("no completed turn in session".into()),
        },
    };
    if at.is_some() && !is_turn_boundary(&entries[boundary]) {
        return Err(format!(
            "fork point {} is not a turn boundary \
             (an assistant message with no pending tool calls, or a compaction)",
            at.unwrap_or(0)
        ));
    }
    Ok(entries[..=boundary].to_vec())
}

/// Token accounting for one provider call, if the provider reports it.

#[async_trait]
pub trait Model: Send {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)>;

    /// Display name for the UI (e.g. input-box label). Defaults to "?".
    fn name(&self) -> &str {
        "?"
    }

    /// Whether the model accepts image input. Used to gate read_image's
    /// image attachment before it enters history.
    fn supports_vision(&self) -> bool {
        false
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String, String>;
    fn set_event_sender(&mut self, _sender: mpsc::UnboundedSender<AgentEvent>) {}
    /// True when the tool already delivers background completions through a
    /// channel of its own (e.g. bound to a shared registry); Agent::new
    /// leaves such tools alone.
    fn has_event_sender(&self) -> bool {
        false
    }
}

pub(crate) struct RoundOutput {
    pub(crate) assistant: AssistantMessage,
    pub(crate) usage: Option<Usage>,
    pub(crate) produced_content_delta: bool,
}

pub(crate) struct CompactionOutput {
    pub(crate) entry: SessionEntry,
    pub(crate) summary: String,
    pub(crate) usage: Option<Usage>,
}

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    history: Vec<SessionEntry>,
    event_handler: Option<Box<dyn FnMut(AgentEvent) + Send>>,
    max_tool_rounds: Option<usize>,
    background_receiver: mpsc::UnboundedReceiver<AgentEvent>,
    pending_background: VecDeque<(u64, String, Option<String>)>,
    subscriber: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Long-lived session sinks (e.g. a TUI view). Unlike
    /// `event_handler` and `subscriber` (per-turn), these survive across
    /// turns and receive every emitted event. Sinks are dropped once their
    /// session handle is gone.
    running_background: HashSet<u64>,
    /// Where in-flight background tasks are recorded (workspace root +
    /// session + the store that owns the record), so resuming the same
    /// session can report what died. None in tests.
    background_record: Option<crate::session_store::BackgroundRecord>,
    session_input_tokens: u64,
    session_output_tokens: u64,
    last_context_input: u64,
    /// Maximum context window in tokens. When set, triggers auto-compaction
    /// whenever `last_context_input` exceeds 80% of this value.
    context_window: Option<u64>,
    /// Set to true when auto-compact fires. Reset to false when
    /// record_usage reports context below 80% of the window.
    auto_compacted: bool,
    /// Workspace and server instructions prepended to every model call.
    /// Not persisted in sessions.
    context_prefix: Option<String>,
}

impl Agent {
    pub fn new(model: Box<dyn Model>, mut tools: Vec<Box<dyn Tool>>) -> Self {
        let (background_sender, background_receiver) = mpsc::unbounded_channel();
        for tool in &mut tools {
            // A tool may own an explicit sink (notably a Bash facade). Such
            // completions retain the session that spawned them.
            if !tool.has_event_sender() {
                tool.set_event_sender(background_sender.clone());
            }
        }
        Self {
            model,
            tools,
            history: Vec::new(),
            event_handler: None,
            max_tool_rounds: None,
            background_receiver,
            pending_background: VecDeque::new(),
            subscriber: None,
            running_background: HashSet::new(),
            background_record: None,
            session_input_tokens: 0,
            session_output_tokens: 0,
            last_context_input: 0,
            context_window: None,
            auto_compacted: false,
            context_prefix: None,
        }
    }

    /// Extra system context prepended to every model call. Not persisted in
    /// sessions.
    pub fn set_context_prefix(&mut self, prefix: String) {
        self.context_prefix = Some(prefix);
    }

    /// Record in-flight background tasks under this workspace root + session
    /// through the given store, so resuming the same session later can
    /// report what died with this process.
    pub fn record_background_tasks_in(
        &mut self,
        root: std::path::PathBuf,
        session: &str,
        store: crate::session_store::SessionStore,
    ) {
        self.background_record = Some(crate::session_store::BackgroundRecord {
            root,
            session: session.to_owned(),
            store,
        });
    }

    /// Cap the number of tool-call rounds per turn. None (the default) means
    /// unlimited: a turn runs until the model stops calling tools.
    pub fn max_tool_rounds(mut self, rounds: usize) -> Self {
        self.max_tool_rounds = Some(rounds);
        self
    }

    /// Set the context window (token count). When set, the agent
    /// auto-compacts when usage exceeds 80% of this value.
    pub fn set_context_window(&mut self, window: u64) {
        self.context_window = Some(window);
    }

    /// Switch the session's model at runtime (web `/model`, TUI `/model`).
    /// The replacement applies from the next model call on: `complete`,
    /// `prepare_compaction` and `supports_vision` all read
    /// `self.model`, so behavior follows the new model without further
    /// wiring.
    pub fn set_model(&mut self, model: Box<dyn Model>) {
        self.model = model;
    }

    /// The current model's wire name ([`Model::name`]; the display name is
    /// only reachable on the concrete `ConfiguredModel` wrapper, not
    /// through the `dyn Model` seam). Used for token-usage accounting.
    pub(crate) fn model_name(&self) -> String {
        self.model.name().to_owned()
    }

    /// Whether the session's current model accepts image input. Non-vision
    /// models cannot consume `Message::User` image parts — the vision gate
    /// (`ensure_vision_supported`) would reject every later model call and
    /// lock the session — so the runner skips read_image attachments and
    /// rejects explicit `/image` prompts on such models.
    pub(crate) fn supports_vision(&self) -> bool {
        self.model.supports_vision()
    }

    pub fn set_event_handler(&mut self, handler: Box<dyn FnMut(AgentEvent) + Send>) {
        self.event_handler = Some(handler);
    }

    /// Full append-only history (what is persisted and shown in the TUI).
    pub fn history(&self) -> &[SessionEntry] {
        &self.history
    }

    /// Replace the whole history (session resume). To ADD one entry to an
    /// already-loaded history, use [`Self::push_entry`] — calling this again
    /// would wipe the restored entries.
    pub fn restore_history(&mut self, history: Vec<SessionEntry>) {
        self.history = Self::migrate_legacy_placeholders(history);
    }

    /// Append a single entry to the history (e.g. a startup notice injected
    /// after resume).
    pub fn push_entry(&mut self, entry: SessionEntry) {
        self.history.push(entry);
    }

    /// One-time migration for sessions written before the `synthetic` flag
    /// existed (commit 92159c7). Back then the interrupted-turn placeholder
    /// was recognized only by its literal text, and placeholders captured in
    /// compaction `retained` snapshots persist WITHOUT the flag — they
    /// deserialize as `synthetic: false`, so `repair_tool_pairs` would
    /// consume them like real results and the real result arriving later
    /// would become a silently dropped orphan (the model would never see
    /// it). Mark legacy placeholders here at load time so the structured
    /// field alone decides from now on.
    fn migrate_legacy_placeholders(history: Vec<SessionEntry>) -> Vec<SessionEntry> {
        let mut history = history;
        for entry in &mut history {
            match entry {
                SessionEntry::Message { message } => Self::mark_legacy_placeholder(message),
                SessionEntry::Compaction { retained, .. } => {
                    for message in retained {
                        Self::mark_legacy_placeholder(message);
                    }
                }
                SessionEntry::Notice { .. }
                | SessionEntry::BackgroundCompletion { .. }
                | SessionEntry::ForkedFrom { .. } => {}
            }
        }
        history
    }

    /// Flag a placeholder written before the `synthetic` field existed:
    /// exactly the interrupted-turn text, an error result, and no flag yet.
    /// A real result with identical text but `is_error: false` is left
    /// untouched.
    fn mark_legacy_placeholder(message: &mut Message) {
        if let Message::Tool {
            content,
            is_error: true,
            synthetic,
            ..
        } = message
            && !*synthetic
            && content == INTERRUPTED
        {
            *synthetic = true;
        }
    }

    /// Messages sent to the provider: the latest compaction summary plus
    /// everything after it.
    pub fn context(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        if let Some(prefix) = &self.context_prefix {
            messages.push(Message::System {
                content: prefix.clone(),
            });
        }
        let mut start = 0;
        if let Some(index) = self
            .history
            .iter()
            .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        {
            let SessionEntry::Compaction { summary, retained } = &self.history[index] else {
                unreachable!()
            };
            messages.push(Message::User {
                content: format!("[compacted summary of earlier conversation]\n{summary}"),
                images: vec![],
            });
            messages.extend(retained.iter().cloned());
            start = index + 1;
        }
        messages.extend(
            self.history[start..]
                .iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Message { message } => Some(message.clone()),
                    SessionEntry::Compaction { .. } => None,
                    // Notices are system-injected events; surface them to the
                    // model as user messages so the model reacts to background
                    // completions and task-death notices.
                    SessionEntry::Notice { text } => Some(Message::User {
                        content: text.clone(),
                        images: vec![],
                    }),
                    // Structured background completions: same surface as
                    // before, but derived from the structured variant rather
                    // than a string-prefixed Notice.
                    SessionEntry::BackgroundCompletion { id, output, label } => {
                        let header =
                            match label.as_ref().map(|l| l.trim()).filter(|l| !l.is_empty()) {
                                Some(l) => format!("[background task {id} completed: {l}]"),
                                None => format!("[background task {id} completed]"),
                            };
                        Some(Message::User {
                            content: format!("{header}\n{output}"),
                            images: vec![],
                        })
                    }
                    // Fork provenance is audit/display only; never put it on
                    // the provider wire.
                    SessionEntry::ForkedFrom { .. } => None,
                }),
        );
        repair_tool_pairs(messages)
    }

    pub fn subscribe(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        self.subscriber = Some(sender);
    }

    pub fn background_task_ids(&self) -> &HashSet<u64> {
        &self.running_background
    }
    /// Whether this session tracks any background task that must complete
    /// before the session may finalize. Only non-detached background bash
    /// tasks are tracked: a detached daemon runs in the shared registry but
    /// never blocks its spawning session from finishing.
    pub(crate) fn has_blocking_background(&self) -> bool {
        !self.running_background.is_empty()
    }

    /// Names of the registered tools (for tests and diagnostics).
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.spec().name).collect()
    }

    /// Wait for the next background task completion. Used by the TUI to
    /// wake an idle agent. The event is also queued for injection into the
    /// next model call.
    pub async fn next_background_completion(&mut self) -> Option<(u64, String, Option<String>)> {
        loop {
            match self.background_receiver.recv().await {
                Some(AgentEvent::BackgroundCompleted { id, output, label }) => {
                    self.pending_background
                        .push_back((id, output.clone(), label.clone()));
                    // No fanout here either: idle and mid-turn completions
                    // both land in the session log as a user message at the
                    // next turn boundary. The TUI prints this return value
                    // itself; fanning out would duplicate the line.
                    return Some((id, output, label));
                }
                Some(_) => {}
                None => return None,
            }
        }
    }

    /// Run turns until the model has reacted to every background
    /// completion: completions arriving mid-turn are folded into the
    /// history at the turn's end, and this loop immediately starts a
    /// follow-up turn so the model reacts instead of waiting for the next
    /// user prompt. Returns the last turn's answer.
    pub async fn run(&mut self, prompt: String) -> anyhow::Result<String> {
        let mut prompt = prompt;
        loop {
            let (answer, injected_at_end) = self.run_turn(std::mem::take(&mut prompt)).await?;
            if !injected_at_end {
                self.subscriber = None;
                return Ok(answer);
            }
        }
    }

    /// One turn. The returned bool is true when completions that arrived
    /// mid-turn were folded into the history at the turn's end without the
    /// model reacting to them yet — run() uses it to start a follow-up turn.
    async fn run_turn(&mut self, prompt: String) -> anyhow::Result<(String, bool)> {
        self.drain_background();
        self.inject_pending_background();
        // Reset the auto-compact latch at the start of each new user turn so
        // a failed compaction doesn't permanently prevent future attempts.
        self.auto_compacted = false;
        if !prompt.is_empty() {
            self.history.push(
                Message::User {
                    content: prompt,
                    images: vec![],
                }
                .into(),
            );
        }
        let specs: Vec<_> = self.tools.iter().map(|tool| tool.spec()).collect();

        let result = self.run_loop(&specs).await;
        self.drain_background();
        // Completions that arrived during this turn were drained into
        // pending but the loop ended before injecting them; fold them into
        // the history now so the finished line renders immediately instead
        // of waiting for the next prompt. run() then loops so the model
        // reacts to them right away.
        let injected_at_end = !self.pending_background.is_empty();
        self.inject_pending_background();
        result.map(|answer| (answer, injected_at_end))
    }

    /// Summarize everything before the current turn and append it as a
    /// compaction entry. The current turn is kept verbatim inside the entry
    /// so the derived context still sees it, while the full history stays
    /// append-only.
    pub async fn compact(&mut self) -> anyhow::Result<String> {
        let prepared = self.prepare_compaction().await?;
        let summary = prepared.summary;
        self.apply_entry(prepared.entry);
        // 直接调用 Agent 的路径（测试/直连 API 使用者）没有 store 访问权
        // （`background_record` 仅用于后台任务登记，且测试中为 None），这里
        // 不落盘 usage；生产路径的压缩统一走 runner 的 compact_operation
        // （runner.rs 的 apply_usage 落盘点，kind="compact"），与该处不是
        // 同一事件，不会重复写入。
        self.apply_usage(prepared.usage, false);
        Ok(summary)
    }

    pub(crate) async fn prepare_compaction(&mut self) -> anyhow::Result<CompactionOutput> {
        let context = self.context();
        let Some(split) = context
            .iter()
            .rposition(|message| matches!(message, Message::User { .. }))
        else {
            anyhow::bail!("nothing to compact");
        };
        if split == 0 {
            anyhow::bail!("nothing to compact");
        }
        // Skip the context prefix (System messages) — only compact if
        // there is actual conversation history (at least one assistant or
        // tool message) before the retained user turn.
        if !context[..split]
            .iter()
            .any(|msg| matches!(msg, Message::Assistant(_) | Message::Tool { .. }))
        {
            anyhow::bail!("nothing to compact");
        }
        let mut request = context[..split].to_vec();
        // Only non-vision models need image stripping on the compaction
        // request itself (the wire gate would otherwise reject it). The
        // persisted history and the retained tail keep images untouched:
        // `complete_round` strips them from the request at send time under
        // a non-vision model, and a vision model regains them on switch
        // back — stripping retained here would be lossy for vision models.
        if !self.model.supports_vision() {
            strip_images(&mut request);
        }
        request.push(Message::User {
            content: "Summarize the earlier conversation. Preserve the user's goals, decisions made, files changed, and unfinished work. Be concise and use Chinese or English to match the conversation language.".into(),
            images: vec![],
        });
        let response = {
            let model = &mut self.model;
            let event_handler = &mut self.event_handler;
            let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
                let event = match kind {
                    ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                    ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
                };
                if let Some(handler) = event_handler {
                    handler(event.clone());
                }
            };
            model.complete(&request, &[], Some(&mut on_delta)).await?
        };
        let (response, usage) = response;
        if !response.tool_calls.is_empty() {
            anyhow::bail!("compaction response contains tool calls");
        }
        let summary = response
            .content
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("compaction produced empty summary"))?;
        Ok(CompactionOutput {
            entry: SessionEntry::Compaction {
                summary: summary.clone(),
                retained: context[split..].to_vec(),
            },
            summary,
            usage,
        })
    }

    pub(crate) fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub(crate) async fn execute_tool(&mut self, call: &ToolCall) -> Result<String, String> {
        Self::execute_on(&self.tools, call).await
    }

    pub(crate) fn apply_entry(&mut self, entry: SessionEntry) {
        self.history.push(entry);
    }

    pub(crate) fn apply_usage(&mut self, usage: Option<Usage>, refresh_context: bool) {
        self.record_usage(usage, refresh_context);
    }

    pub(crate) fn emit_event(&mut self, event: AgentEvent) {
        self.emit(event);
    }

    pub(crate) fn after_tool_entry(&mut self, call: &ToolCall, result: &Result<String, String>) {
        if result.is_ok()
            && (call.name == "bash" || call.name == "pwsh")
            && is_background_call(call)
            && !is_detached_background_call(call)
            && let Some(id) = started_task_id(result.as_deref().unwrap_or_default())
        {
            self.running_background.insert(id);
            if let Some(record) = &self.background_record {
                let command = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|args| args["command"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| call.arguments.clone());
                let label = preview(&command, 100);
                record.store.record_background_start(
                    &record.root,
                    &record.session,
                    id,
                    &label,
                    Some(&command),
                    None,
                );
            }
        }
    }

    fn push_message(&mut self, message: Message) {
        self.history.push(message.into());
    }

    fn record_usage(&mut self, usage: Option<Usage>, refresh_context: bool) {
        if let Some(usage) = usage {
            self.session_input_tokens += usage.input_tokens;
            self.session_output_tokens += usage.output_tokens;
            if refresh_context {
                self.last_context_input = usage.input_tokens;
            }
            // Reset auto-compacted flag when context drops below 80% of the
            // window (compaction succeeded and usage decreased).
            if self.auto_compacted
                && let Some(window) = self.context_window
                && window > 0
                && (self.last_context_input as u128) * 100 < (window as u128) * 80
            {
                self.auto_compacted = false;
            }
            self.emit(AgentEvent::Usage {
                context_input: self.last_context_input,
                context_window: self.context_window,
                session: Usage {
                    input_tokens: self.session_input_tokens,
                    output_tokens: self.session_output_tokens,
                },
            });
        }
    }

    pub(crate) async fn complete_round(
        &mut self,
        specs: &[ToolSpec],
    ) -> anyhow::Result<RoundOutput> {
        let mut produced_content_delta = false;
        let mut context = self.context();
        // Non-vision models cannot consume image parts. Strip them from the
        // *request* only, so the wire gate never rejects the whole history
        // and the session is not locked (this is the fallback that lets
        // sessions with legacy image-bearing history — e.g. split==0 where
        // compaction cannot run — keep working). The persisted history is
        // untouched: switching back to a vision model restores the images
        // on the next request.
        if !self.model.supports_vision() {
            strip_images(&mut context);
        }
        let model = &mut self.model;
        let event_handler = &mut self.event_handler;
        let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
            if kind == ModelDeltaKind::Content {
                produced_content_delta = true;
            }
            let event = match kind {
                ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
            };
            if let Some(handler) = event_handler {
                handler(event.clone());
            }
        };
        let (assistant, usage) = model.complete(&context, specs, Some(&mut on_delta)).await?;
        Ok(RoundOutput {
            assistant,
            usage,
            produced_content_delta,
        })
    }

    async fn run_loop(&mut self, specs: &[ToolSpec]) -> anyhow::Result<String> {
        let mut rounds = 0usize;
        loop {
            if let Some(limit) = self.max_tool_rounds
                && rounds >= limit
            {
                anyhow::bail!("tool call limit ({limit}) reached");
            }
            rounds += 1;
            self.drain_background();
            self.inject_pending_background();
            let round = self.complete_round(specs).await?;
            let RoundOutput {
                assistant,
                usage,
                produced_content_delta: produced_delta,
            } = round;
            self.record_usage(usage, true);
            // Auto-compact when usage exceeds 80% of the configured context window.
            if let Some(window) = self.context_window
                && window > 0
                && !self.auto_compacted
                && (self.last_context_input as u128) * 100 >= (window as u128) * 80
            {
                self.auto_compacted = true;
                self.emit(AgentEvent::Notice("──── auto-compacting… ────".into()));
                if let Err(error) = self.compact().await {
                    self.auto_compacted = false;
                    self.emit(AgentEvent::Notice(format!(
                        "auto-compaction error: {error:#}"
                    )));
                }
                self.emit(AgentEvent::Notice("──── auto-compaction ────".into()));
            }
            if assistant.tool_calls.is_empty() {
                let answer = assistant.content.clone().unwrap_or_default();
                self.push_message(Message::Assistant(assistant));
                return Ok(answer);
            }

            if !produced_delta
                && let Some(content) = assistant
                    .content
                    .as_deref()
                    .filter(|content| !content.is_empty())
            {
                self.emit(AgentEvent::AssistantText(content.into()));
            }
            self.push_message(Message::Assistant(assistant.clone()));
            for call in &assistant.tool_calls {
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = Self::execute_on(&self.tools, call).await;
                self.after_tool_entry(call, &result);
                // A read_image result carries a structured image marker;
                // strip it so the Tool message/event keep only the text
                // summary (base64 never enters the scrollback), then attach
                // the image as a synthetic User message right after the tool
                // result (images can only ride on user role messages).
                // Non-vision models cannot consume image parts: keep the
                // text summary but skip the attachment, so the session is
                // not locked out of every later model call by the vision
                // gate (compaction would fail the same way).
                let (mut summary, image) = match &result {
                    Ok(content) => split_image_marker(content),
                    Err(error) => (error.clone(), None),
                };
                let supports_vision = self.model.supports_vision();
                let image = if image.is_some() && !supports_vision {
                    summary.push_str("（当前模型不支持图片，已跳过附加）");
                    None
                } else {
                    image
                };
                self.emit(AgentEvent::ToolResult {
                    is_error: result.is_err(),
                    content: summary.clone(),
                });
                self.push_message(Message::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: summary,
                    is_error: result.is_err(),
                    synthetic: false,
                });
                if let Some(image) = image {
                    let path = tool_path_argument(&call.arguments).unwrap_or_default();
                    self.push_message(Message::User {
                        content: format!("[image attached: {path}]"),
                        images: vec![image],
                    });
                }
            }
        }
    }

    /// Execute a tool call against a tool list. Associated function (not a
    /// method) so the returned future does not borrow `&self`, keeping
    /// `Agent::run` futures `Send` for use in `tokio::spawn`.
    async fn execute_on(tools: &[Box<dyn Tool>], call: &ToolCall) -> Result<String, String> {
        let arguments = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let Some(tool) = tools.iter().find(|tool| tool.spec().name == call.name) else {
            return Err(format!("unknown tool: {}", call.name));
        };
        tool.execute(arguments).await
    }

    fn emit(&mut self, event: AgentEvent) {
        if let Some(handler) = &mut self.event_handler {
            handler(event.clone());
        }
    }

    fn inject_pending_background(&mut self) {
        while !self.pending_background.is_empty() {
            let entry = self.peek_background_entry().expect("pending entry");
            self.apply_entry(entry);
            self.ack_background_entry();
        }
    }

    pub(crate) fn drain_background_ready(&mut self) {
        while let Ok(AgentEvent::BackgroundCompleted { id, output, label }) =
            self.background_receiver.try_recv()
        {
            self.pending_background
                .push_back((id, output.clone(), label.clone()));
            if let Some(subscriber) = &self.subscriber {
                let _ = subscriber.send(AgentEvent::BackgroundCompleted { id, output, label });
            }
        }
    }

    pub(crate) async fn wait_background_ready(&mut self) -> bool {
        self.next_background_completion().await.is_some()
    }

    pub(crate) fn peek_background_entry(&self) -> Option<SessionEntry> {
        self.pending_background.front().map(|(id, output, label)| {
            SessionEntry::BackgroundCompletion {
                id: *id,
                output: output.clone(),
                label: label.clone(),
            }
        })
    }

    pub(crate) fn ack_background_entry(&mut self) {
        if let Some((id, _output, _label)) = self.pending_background.pop_front() {
            self.running_background.remove(&id);
            if let Some(record) = &self.background_record {
                record
                    .store
                    .clear_background_task(&record.root, &record.session, id);
            }
        }
    }

    pub(crate) fn take_auto_compact_request(&mut self) -> bool {
        let requested = self.context_window.is_some_and(|window| {
            window > 0
                && !self.auto_compacted
                && (self.last_context_input as u128) * 100 >= (window as u128) * 80
        });
        if requested {
            self.auto_compacted = true;
        }
        requested
    }

    pub(crate) fn reset_auto_compact_request(&mut self) {
        self.auto_compacted = false;
    }

    fn drain_background(&mut self) {
        self.drain_background_ready();
    }
}

fn is_background_call(call: &ToolCall) -> bool {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| value.get("background").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn is_detached_background_call(call: &ToolCall) -> bool {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| value.get("detached").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn started_task_id(output: &str) -> Option<u64> {
    output
        .strip_prefix("started background task ")?
        .split_once(':')?
        .0
        .parse()
        .ok()
}

pub fn preview(text: &str, max_chars: usize) -> String {
    let total: Vec<char> = text.chars().collect();
    if total.len() <= max_chars || max_chars <= 1 {
        // max_chars 0 → empty; max_chars 1 → a single ellipsis only
        if max_chars == 0 {
            return String::new();
        }
        if total.len() <= max_chars {
            return text.to_owned();
        }
        // max_chars == 1 and text longer than 1: show just "…"
        return "\u{2026}".to_owned();
    }
    // Keep a 2:1 head-to-tail ratio within a budget of max_chars (including "…").
    let marker_len = 1; // "…"
    let available = max_chars - marker_len;
    let head_count = (available * 2 / 3).max(1);
    let tail_count = available - head_count;
    let head: String = total[..head_count].iter().collect();
    let tail: String = total[total.len() - tail_count..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

#[cfg(test)]
mod tests;
