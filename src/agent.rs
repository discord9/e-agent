use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use crate::output_receipt::{FieldId, bound_field};
use crate::session_store::EntryLocation;

/// Content of the synthetic error result inserted for every tool call left
/// unanswered by an interrupted turn (cancel, provider error, crash). Also
/// the legacy marker: sessions written before the `synthetic` flag existed
/// (commit 92159c7) recognized these placeholders by this literal text.
const INTERRUPTED: &str = "[turn interrupted before a tool result was produced]";

/// Model-facing content of a poll-guard error: a consecutive
/// `get_background_tasks` call with an unchanged running-task snapshot
/// within one turn, at or above the configured reminder threshold but below
/// the termination threshold (the 2nd for subagents, the 3rd and 4th for
/// the main agent).
pub(crate) const POLL_GUARD_ERROR: &str = "poll guard: get_background_tasks was already called with this exact task list this turn; end the turn and wait for the automatic [background task N completed] injection instead of polling again";

/// Internal termination sentinel returned at the configured termination
/// threshold — the THIRD consecutive unchanged-snapshot poll for subagents,
/// the FIFTH for the main agent. Never enters history, model context, or
/// UI: the batch loops (`Agent::run_loop` and `SessionRunner`) substitute
/// [`POLL_GUARD_ERROR`] as the committed content and use the sentinel only
/// to set the turn-termination latch after the full sibling batch has been
/// durably committed.
pub(crate) const POLL_GUARD_SENTINEL: &str = "\u{0}poll-guard-terminate";

/// Termination notice emitted after the full tool batch (and, in the runner
/// path, the `commit_backgrounds` safe point) when the poll guard fired,
/// ending the current turn. The next turn starts with a reset guard.
pub(crate) const POLL_GUARD_TERMINATION_NOTICE: &str =
    "repeated get_background_tasks calls with an unchanged task list; ending this turn";

/// True when a tool result is the internal poll-guard termination sentinel
/// (an unchanged-snapshot poll at the configured termination threshold:
/// 3rd for subagents, 5th for the main agent). Batch loops use this to set
/// a local latch; the sentinel itself never reaches history/UI.
pub(crate) fn is_poll_guard_terminate(result: &Result<ToolOutput, String>) -> bool {
    matches!(result, Err(error) if error == POLL_GUARD_SENTINEL)
}

/// Map a tool error to the content that may enter history/UI: the internal
/// poll-guard sentinel becomes the model-facing [`POLL_GUARD_ERROR`] text;
/// every other error passes through unchanged.
pub(crate) fn tool_error_content(error: &str) -> &str {
    if error == POLL_GUARD_SENTINEL {
        POLL_GUARD_ERROR
    } else {
        error
    }
}

/// Number of most-recent messages kept verbatim when compacting a
/// single-task (subagent) session whose only User message is the initial
/// prompt. Such sessions have no "conversation before the current turn", so
/// the compaction window becomes everything except this recent tail of tool
/// activity, which keeps the agent working after the compaction.
const RETAIN_TAIL: usize = 20;

/// How a session's compaction chooses its retained tail. The mode is set
/// explicitly by the caller — it is NEVER inferred from the history (a
/// user-count heuristic misclassifies a main session whose FIRST turn is a
/// long tool loop, compacting its sole prompt away).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactionMode {
    /// Main-agent sessions (SessionFactory/REPL/TUI/web and btw forks): the
    /// current prompt is the LAST actual-user message, and compaction keeps
    /// that prompt plus the bounded recent activity tail.
    #[default]
    Main,
    /// Delegated subagent sessions: a single initial prompt followed by a
    /// tool-call loop (and, after the first compaction, no actual user in
    /// the retained tail at all). Compaction keeps a recent tail of tool
    /// activity instead of a user turn, and repeated compaction works
    /// without any retained actual user.
    SingleTask,
}

/// Compaction summary prompt. The trailing sentence is a positive
/// constraint: output the summary body directly, never narrate the plan to
/// summarize (the metadiscourse-stub accident shape).
const COMPACTION_SUMMARY_PROMPT: &str = "Summarize the earlier conversation. Preserve the user's goals, decisions made, files changed, and unfinished work. Be concise and use Chinese or English to match the conversation language. Output a plain-text summary only: no tool calls, no XML/DSML/function-call markup, no code blocks. Start directly with the summary content. Do not describe what you are about to do, do not introduce the summary, do not use phrases like 'here is a summary' or 'I will summarize' or 'this is a compaction request' — output only the summary itself.";

/// Insert a synthetic error result for every tool call left unanswered by an
/// interrupted turn (cancel, provider error, crash), so the derived context
/// always satisfies the provider's tool_call/tool-result pairing rule.
/// Test-facing: production projection uses the item-aware
/// [`repair_item_pairs`] so located metadata stays aligned.
#[cfg(test)]
pub(crate) fn repair_tool_pairs(messages: Vec<Message>) -> Vec<Message> {
    fn flush(pending: &mut Vec<ToolCall>, out: &mut Vec<Message>) {
        for call in pending.drain(..) {
            out.push(Message::Tool {
                call_id: call.id,
                name: call.name,
                content: INTERRUPTED.into(),
                images: vec![],
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

/// Item-aware tool-call pairing repair: the same rules as
/// [`repair_tool_pairs`], applied to [`ContextItem`]s so the located
/// metadata stays aligned with the surviving messages. Synthetic
/// placeholders carry no located key and are never bounded.
fn repair_item_pairs(items: Vec<ContextItem>) -> Vec<ContextItem> {
    fn flush(pending: &mut Vec<ToolCall>, out: &mut Vec<ContextItem>) {
        for call in pending.drain(..) {
            out.push(ContextItem {
                message: Message::Tool {
                    call_id: call.id,
                    name: call.name,
                    content: INTERRUPTED.into(),
                    images: vec![],
                    is_error: true,
                    synthetic: true,
                },
                prefix: String::new(),
                location: None,
                field: None,
                field_total: None,
                actual_user: false,
                keep_full: false,
            });
        }
    }

    let mut out = Vec::with_capacity(items.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    for item in items {
        match &item.message {
            Message::Tool {
                call_id,
                synthetic: true,
                ..
            } => {
                let _ = call_id;
            }
            Message::Tool { call_id, .. } => {
                if pending.iter().any(|call| call.id == *call_id) {
                    pending.retain(|call| &call.id != call_id);
                    out.push(item);
                }
            }
            Message::Assistant(assistant) => {
                flush(&mut pending, &mut out);
                pending = assistant.tool_calls.clone();
                out.push(item);
            }
            Message::System { .. } | Message::User { .. } => {
                flush(&mut pending, &mut out);
                out.push(item);
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

/// A structured tool result: display/content text plus optional image
/// attachments. `read_image` fills `images`; the runner persists them on the
/// `Message::Tool` entry and each wire encodes them natively (chat: one
/// aggregated temporary user wire message per consecutive tool batch;
/// responses: a `function_call_output` output array). Every other tool
/// returns text only via [`ToolOutput::text`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub images: Vec<ImagePart>,
}

impl ToolOutput {
    /// Plain text result with no image attachments.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            images: Vec::new(),
        }
    }
}

/// Remove image attachments from user AND tool messages, text-degrading the
/// message (a short note is appended where images were dropped). Non-vision
/// models cannot consume image parts, so the REQUEST COPY strips them before
/// sending (the vision gate would otherwise reject it and lock the session).
/// The persisted history is untouched: switching back to a vision model
/// restores the images on the next request.
pub fn strip_images(messages: &mut [Message]) {
    for message in messages {
        let (content, images) = match message {
            Message::User { content, images } => (content, images),
            Message::Tool {
                content, images, ..
            } => (content, images),
            _ => continue,
        };
        if !images.is_empty() {
            images.clear();
            content.push_str("（当前模型不支持图片，已跳过附加）");
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

/// Vision gate shared by both wires: messages with images (user attachments
/// or image-bearing tool results) require a vision-capable model. Non-vision
/// models get a clear error instead of a malformed or silently degraded
/// request.
pub fn ensure_vision_supported(
    model: &str,
    vision: bool,
    messages: &[Message],
) -> anyhow::Result<()> {
    let has_images = messages.iter().any(|message| match message {
        Message::User { images, .. } | Message::Tool { images, .. } => !images.is_empty(),
        _ => false,
    });
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
        /// Image attachments produced by the tool (e.g. `read_image`) as
        /// content-hash references into the global image store (never inline
        /// base64 in the session). Only the reference is persisted; the wire
        /// layer re-reads the file and encodes it.
        /// `#[serde(default)]` keeps old session files (no `images` field)
        /// loadable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImagePart>,
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
    /// Model reasoning, persisted for display/audit only. By default never
    /// sent back to the provider (see WireMessage); the sole exception is
    /// the explicit DeepSeek compatibility profile (`deepseek_compat =
    /// true`) with thinking mode on a tool-call assistant turn, where the
    /// original `reasoning_content` must be echoed back to the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Lifecycle status of the session's current goal. Serialized as the
/// lowercase name (`active`, `paused`, `blocked`, `completed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Completed,
}

impl GoalStatus {
    pub fn label(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Completed => "completed",
        }
    }
}

/// Immutable goal snapshot; every change appends a new `GoalUpdated` entry
/// with a bumped `revision` (writers CAS on `id` + `revision`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSnapshot {
    pub id: String,
    pub revision: u64,
    pub objective: String,
    pub success_criteria: Vec<String>,
    pub status: GoalStatus,
    pub progress: String,
    /// Completion evidence, kept after completion; pure analysis may use an
    /// explicit `unverified: <analysis>` string.
    pub evidence: Vec<String>,
    pub blocked_reason: Option<String>,
}

/// One goal transition (`update_goal` tool + human `/goal` commands).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalAction {
    Progress {
        progress: String,
    },
    Pause,
    Resume,
    Block {
        reason: String,
    },
    /// Requires non-empty evidence (checked in [`transition_goal`]).
    Complete,
    /// Tombstone: `SessionEntry::GoalUpdated { goal: None }`.
    Clear,
}

/// Create the first revision of a goal (human-only). Allowed only with no
/// current goal or a completed one (cleared reads as "no goal").
pub fn create_goal(
    current: Option<&GoalSnapshot>,
    objective: String,
    success_criteria: Vec<String>,
) -> Result<GoalSnapshot, String> {
    if let Some(goal) = current
        && goal.status != GoalStatus::Completed
    {
        return Err(format!(
            "cannot create a new goal while `{}` is still {}: complete or clear it first",
            goal.id,
            goal.status.label()
        ));
    }
    let objective = objective.trim();
    if objective.is_empty() {
        return Err("goal objective must not be empty".into());
    }
    Ok(GoalSnapshot {
        id: crate::session::new_id_prefixed("goal-"),
        revision: 1,
        objective: objective.to_owned(),
        success_criteria: success_criteria
            .into_iter()
            .map(|criterion| criterion.trim().to_owned())
            .filter(|criterion| !criterion.is_empty())
            .collect(),
        status: GoalStatus::Active,
        progress: String::new(),
        evidence: Vec::new(),
        blocked_reason: None,
    })
}

/// Apply one action under an `id` + `revision` CAS; `None` = cleared (the
/// caller persists the tombstone). `evidence` is appended before status
/// checks so `Complete` can carry fresh evidence. Plain-string errors.
pub fn transition_goal(
    current: Option<&GoalSnapshot>,
    id: &str,
    revision: u64,
    action: &GoalAction,
    success_criteria: Option<Vec<String>>,
    evidence: Vec<String>,
) -> Result<Option<GoalSnapshot>, String> {
    let Some(goal) = current else {
        return Err("no goal is set for this session".into());
    };
    if goal.id != id {
        return Err(format!(
            "goal id mismatch: current goal is `{}`, caller supplied `{id}`",
            goal.id
        ));
    }
    if goal.revision != revision {
        return Err(format!(
            "goal revision mismatch: current revision is {}, caller supplied {revision} \
             (concurrent update? re-read with get_goal)",
            goal.revision
        ));
    }
    let mut next = goal.clone();
    if let Some(criteria) = success_criteria {
        next.success_criteria = criteria
            .into_iter()
            .map(|criterion| criterion.trim().to_owned())
            .filter(|criterion| !criterion.is_empty())
            .collect();
    }
    // THIS call's evidence, trimmed of blanks, kept apart so `Complete` can
    // require fresh evidence in the same call (prior accumulated evidence
    // never satisfies completion).
    let fresh_evidence: Vec<String> = evidence
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect();
    next.evidence.extend(fresh_evidence.clone());
    match action {
        GoalAction::Progress { progress } => {
            if next.status == GoalStatus::Completed {
                return Err("cannot update progress: goal is already completed".into());
            }
            let progress = progress.trim();
            if progress.is_empty() {
                return Err("`progress` must not be empty".into());
            }
            next.progress = progress.to_owned();
        }
        GoalAction::Pause => {
            if next.status != GoalStatus::Active {
                return Err("cannot pause a goal that is not active".into());
            }
            next.status = GoalStatus::Paused;
        }
        GoalAction::Resume => {
            if !matches!(next.status, GoalStatus::Paused | GoalStatus::Blocked) {
                return Err("cannot resume a goal that is not paused or blocked".into());
            }
            next.status = GoalStatus::Active;
            next.blocked_reason = None;
        }
        GoalAction::Block { reason } => {
            if next.status == GoalStatus::Completed {
                return Err("cannot block a completed goal".into());
            }
            let reason = reason.trim();
            if reason.is_empty() {
                return Err("`blocked_reason` must not be empty".into());
            }
            next.status = GoalStatus::Blocked;
            next.blocked_reason = Some(reason.to_owned());
        }
        GoalAction::Complete => {
            if next.status == GoalStatus::Completed {
                return Err("goal is already completed".into());
            }
            if fresh_evidence.is_empty() {
                return Err(
                    "cannot complete a goal without evidence in this update: pass non-empty \
                     `evidence` with the complete call (e.g. an explicit `unverified: <analysis>` \
                     string for pure analysis)"
                        .into(),
                );
            }
            next.status = GoalStatus::Completed;
            next.blocked_reason = None;
        }
        GoalAction::Clear => return Ok(None),
    }
    next.revision += 1;
    Ok(Some(next))
}

/// Short provider-context projection, capped at [`LIMIT`] chars. This is
/// what the model sees on every call; the full machine-usable snapshot is
/// [`goal_snapshot_json`] (the `get_goal` tool), and the complete snapshot
/// lives in history.
pub fn goal_projection_text(goal: &GoalSnapshot) -> String {
    const LIMIT: usize = 400;
    let criteria = if goal.success_criteria.is_empty() {
        "-".to_owned()
    } else {
        goal.success_criteria
            .iter()
            .take(3)
            .map(|criterion| preview(criterion, 80))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let mut text = format!(
        "id {} | {} | {}\ncriteria: {criteria}",
        goal.id,
        goal.status.label(),
        preview(&goal.objective, 120),
    );
    if !goal.progress.is_empty() {
        text.push_str(&format!("\nprogress: {}", preview(&goal.progress, 120)));
    }
    if let Some(reason) = &goal.blocked_reason {
        text.push_str(&format!("\nblocked: {}", preview(reason, 120)));
    }
    if goal.status == GoalStatus::Completed && !goal.evidence.is_empty() {
        text.push_str(&format!(
            "\nevidence: {}",
            goal.evidence
                .iter()
                .take(3)
                .map(|item| preview(item, 60))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    preview(&text, LIMIT)
}

/// Full machine-usable goal snapshot as pretty JSON (`get_goal` tool). Every
/// field the model needs for a CAS update round trip is present: id,
/// revision, objective, the FULL success_criteria/evidence arrays, status,
/// progress and blocked_reason. The short provider projection stays the
/// domain of [`goal_projection_text`].
pub fn goal_snapshot_json(goal: &GoalSnapshot) -> String {
    serde_json::to_string_pretty(goal).unwrap_or_else(|_| goal_projection_text(goal))
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
        /// Trace metadata (start/duration/exit/status/kind); all `None` on
        /// legacy events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
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
    /// A goal snapshot update was durably committed (`None` = cleared).
    /// Rendered by the TUI/web as a Notice line + GoalBar refresh; not a
    /// user prompt.
    GoalUpdated {
        goal: Option<GoalSnapshot>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDeltaKind {
    Content,
    Reasoning,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Provider-reported cache-hit prompt tokens (DeepSeek
    /// `prompt_cache_hit_tokens` / OpenAI `input_tokens_details.cached_tokens`);
    /// `None` on providers that don't report them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_tokens: Option<u64>,
    /// Provider-reported cache-miss prompt tokens (DeepSeek
    /// `prompt_cache_miss_tokens`); `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_tokens: Option<u64>,
    /// Provider-reported reasoning token count (DeepSeek/OpenAI
    /// `completion_tokens_details.reasoning_tokens`). Only the COUNT is
    /// kept — reasoning text is never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider finish reason (enum string, e.g. "stop" | "length" |
    /// "tool_calls"); absent when the provider stream doesn't carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Structured trace metadata of a finished background task, carried
/// end-to-end from the registry completion through the transient
/// [`AgentEvent::BackgroundCompleted`] (and the durable-commit notice
/// [`AgentEvent::BackgroundCompletionNotice`]) to the persisted
/// [`SessionEntry::BackgroundCompletion`]. All fields optional: legacy rows
/// and providers that don't report a field stay `None`, and serialization
/// omits them (serde default + skip), so old persisted JSON and the SSE
/// wire stay valid.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTrace {
    /// Epoch milliseconds at task start (`None` on legacy events/rows).
    pub started_at_ms: Option<u64>,
    /// Wall duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Process exit code (`None` for delegates and signal-killed tasks).
    pub exit_code: Option<i32>,
    /// Terminating signal (bash tasks killed by a signal, e.g. "SIGTERM").
    pub signal: Option<String>,
    /// "completed" | "failed" | "killed".
    pub status: Option<String>,
    /// "bash" | "delegate".
    pub kind: Option<String>,
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
        /// Provenance of the current turn inside `retained`: the index of
        /// the actual user prompt that opens the retained tail (`Some(0)`
        /// for main-session compactions, where the split is the last
        /// actual-user message). `None` when the retained tail is a
        /// subagent tool-activity window that contains no actual user, or
        /// for compactions persisted before this field existed (the
        /// projection then falls back to the first user-shaped retained
        /// message). Written by `prepare_compaction` from the context
        /// items' actual-user provenance and read back on resume, so the
        /// retained projection never has to guess which retained message
        /// is the current prompt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_prompt_at: Option<usize>,
        /// Distinguishes "known to contain NO current prompt" from "legacy
        /// provenance absent": a single-task retained tail (tool window
        /// with no actual user) sets this true, so the resume projection
        /// never falls back to the first user-shaped retained message — a
        /// retained background completion must not be misread as the
        /// current prompt. `false` (legacy compactions persisted before
        /// this field existed) keeps the user-shaped fallback.
        #[serde(default, skip_serializing_if = "is_false")]
        no_current_prompt: bool,
    },
    /// A system-injected notice (background completion, task-kill report)
    /// rendered in the TUI as a dim line and surfaced to the model as a
    /// user message.
    Notice {
        text: String,
    },
    /// A structured background completion entry. Persisted in the session
    /// log with the full output. The TUI renders a truncated preview; the
    /// provider REQUEST copy bounds the output (UTF-8-safe head+tail plus a
    /// session-local `read_output` ref, see
    /// [`crate::output_receipt`]) so an oversized completion can never
    /// blow up the provider context — the persisted entry stays full.
    /// Backwards-compatible: old `Notice` entries are read without guessing
    /// string prefixes.
    BackgroundCompletion {
        id: u64,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Trace metadata (epoch-ms start, wall duration, exit code,
        /// terminating signal, "completed"|"failed"|"killed", and
        /// "bash"|"delegate"). All optional: legacy rows persisted before
        /// these fields existed deserialize with them `None` (serde
        /// default) and serialize without them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
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
    /// A harness error (provider/model call failure, compaction failure,
    /// image rejection, termination reason). Durable for resume / late
    /// attach audit only: `Agent::context` filters this variant, so it
    /// never reaches the provider. Old sessions without this variant load
    /// naturally (serde payload is additive).
    Error {
        text: String,
    },
    /// A goal snapshot update: the COMPLETE latest snapshot (`goal: None`
    /// is the clear tombstone). Append-only; every update bumps
    /// `goal.revision`, and the latest entry wins on resume/fork.
    GoalUpdated {
        goal: Option<GoalSnapshot>,
    },
}

impl From<Message> for SessionEntry {
    fn from(message: Message) -> Self {
        Self::Message { message }
    }
}

/// `skip_serializing_if` helper for the `no_current_prompt` marker: false
/// (legacy) compactions keep the old wire shape (no field), so persisted
/// sessions and the history-response JSON are unchanged.
fn is_false(value: &bool) -> bool {
    !*value
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
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String>;
    fn set_event_sender(&mut self, _sender: mpsc::UnboundedSender<AgentEvent>) {}
    /// True when the tool already delivers background completions through a
    /// channel of its own (e.g. bound to a shared registry); Agent::new
    /// leaves such tools alone.
    fn has_event_sender(&self) -> bool {
        false
    }
    /// Called once at the start of each true turn (fresh/queued user prompt,
    /// idle background-completion follow-up, direct `Agent::run` call) so
    /// per-turn tool state can reset. Never called mid-model-round,
    /// mid-tool-batch, or around manual/auto compaction. Default: no-op.
    fn on_turn_start(&mut self) {}
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

/// One provider-context message plus the metadata the bounded projection
/// needs: the source entry's located key (when one exists), the eligible
/// field, the never-bounded message prefix, and whether the message is an
/// actual user message (the CURRENT one is always kept full).
struct ContextItem {
    message: Message,
    /// Leading text of `message` that is never bounded (e.g. the
    /// `[background task N completed]` header); the field starts at
    /// `message.content[prefix.len()..]`. Empty for plain messages.
    prefix: String,
    /// Located key of the persisted source entry (`None` for legacy/
    /// test in-memory entries and synthesized messages — such fields stay
    /// full).
    location: Option<EntryLocation>,
    /// The field to bound; `None` derives from the message kind (user /
    /// assistant / tool content). System messages are never bounded.
    field: Option<FieldId>,
    /// The FULL field byte length the receipt must bind. `None` = the
    /// field is exactly this message's own content (`text.len()`); for a
    /// message inside a compaction's retained tail, the receipt binds the
    /// whole persisted `compaction_retained` array length (what
    /// `read_output` returns).
    field_total: Option<usize>,
    /// True for real `Message::User` history entries (NOT background
    /// completion / notice projections). The last actual-user item is the
    /// current prompt and stays full.
    actual_user: bool,
    /// Never bound (current actual user).
    keep_full: bool,
}

impl ContextItem {
    fn system(message: Message) -> Self {
        Self {
            message,
            prefix: String::new(),
            location: None,
            field: None,
            field_total: None,
            actual_user: false,
            keep_full: false,
        }
    }
}

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    history: Vec<SessionEntry>,
    event_handler: Option<Box<dyn FnMut(AgentEvent) + Send>>,
    max_tool_rounds: Option<usize>,
    background_receiver: mpsc::UnboundedReceiver<AgentEvent>,
    pending_background: VecDeque<(u64, String, Option<String>, BackgroundTrace)>,
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
    /// Role-derived directive injected into compacted summary projections.
    /// Not persisted in sessions.
    compaction_reminder: Option<String>,
    /// Latest goal snapshot, folded from the append-only history
    /// (GoalUpdated entries). Projected into every provider context, so it
    /// survives compaction and resume.
    goal: Option<GoalSnapshot>,
    /// Whether the NEWEST GoalUpdated entry is a clear tombstone. Distinguishes
    /// "never set" from "cleared", so the provider context can override a
    /// stale compaction summary that still mentions the old goal.
    goal_cleared: bool,
    /// Exact physical located keys of the persisted history, aligned 1:1
    /// with `history` (`None` = the entry has no located key: legacy/test
    /// in-memory entries, entries pushed without a location).
    entry_locations: Vec<Option<EntryLocation>>,
    /// How compaction picks its retained tail (see [`CompactionMode`]).
    /// Set by the caller — never inferred from the history. Main by
    /// default; delegated subagents (src/delegate.rs) opt into
    /// `SingleTask`.
    compaction_mode: CompactionMode,
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
            compaction_reminder: None,
            goal: None,
            goal_cleared: false,
            entry_locations: Vec::new(),
            compaction_mode: CompactionMode::Main,
        }
    }

    /// Set the compaction mode explicitly (see [`CompactionMode`]). Main by
    /// default; delegated subagents call this with `SingleTask`. The mode
    /// must come from the caller's session kind, never from a heuristic on
    /// the history contents.
    pub fn with_compaction_mode(mut self, mode: CompactionMode) -> Self {
        self.compaction_mode = mode;
        self
    }

    /// Extra system context prepended to every model call. Not persisted in
    /// sessions.
    pub fn set_context_prefix(&mut self, prefix: String) {
        self.context_prefix = Some(prefix);
    }

    /// Install a role-derived directive for compacted summary projections.
    /// Not persisted in sessions.
    pub fn set_compaction_reminder(&mut self, text: String) {
        self.compaction_reminder = Some(text);
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
    /// models cannot consume image parts — the vision gate
    /// (`ensure_vision_supported`) would reject every later model call and
    /// lock the session — so the runner strips them from request copies
    /// (history keeps them) and rejects explicit `/image` prompts on such
    /// models.
    pub(crate) fn supports_vision(&self) -> bool {
        self.model.supports_vision()
    }

    pub fn set_event_handler(&mut self, handler: Box<dyn FnMut(AgentEvent) + Send>) {
        self.event_handler = Some(handler);
    }

    /// Per-true-turn tool hook: every tool's `on_turn_start` runs once at
    /// the start of a true turn (fresh user prompt, queued prompt, idle
    /// background-completion follow-up, direct `Agent::run` call) so
    /// per-turn tool state (e.g. the poll guard) resets. Never
    /// called mid-model-round, mid-tool-batch, or around manual/auto
    /// compaction.
    pub(crate) fn start_turn(&mut self) {
        for tool in &mut self.tools {
            tool.on_turn_start();
        }
    }

    /// Full append-only history (what is persisted and shown in the TUI).
    pub fn history(&self) -> &[SessionEntry] {
        &self.history
    }

    /// Replace the whole history (session resume). To ADD one entry to an
    /// already-loaded history, use [`Self::push_entry`] — calling this again
    /// would wipe the restored entries. Resets the located metadata to
    /// "no located keys" (call [`Self::restore_locations`] right after when
    /// the locations are known).
    pub fn restore_history(&mut self, history: Vec<SessionEntry>) {
        let history = Self::migrate_legacy_placeholders(history);
        let (goal, cleared) = Self::fold_goal(&history);
        self.goal = goal;
        self.goal_cleared = cleared;
        self.entry_locations = vec![None; history.len()];
        self.history = history;
    }

    /// Restore the exact physical located keys aligned with the history
    /// restored by [`Self::restore_history`]. Must be called with a vector
    /// of the same length; defensive truncation keeps the alignment
    /// invariant when a caller passes a mismatched length (missing/extra
    /// entries then read as unlocated, which only disables receipts).
    pub fn restore_locations(&mut self, locations: Vec<Option<EntryLocation>>) {
        self.entry_locations = locations;
        self.entry_locations.resize(self.history.len(), None);
    }

    /// Restore history + locations in one call (the session-resume path).
    pub fn restore_located(
        &mut self,
        history: Vec<SessionEntry>,
        locations: Vec<Option<EntryLocation>>,
    ) {
        self.restore_history(history);
        self.restore_locations(locations);
    }

    /// Latest goal snapshot folded from the history (`None` = none/cleared).
    pub fn goal(&self) -> Option<GoalSnapshot> {
        self.goal.clone()
    }

    /// Whether the newest goal update was a clear tombstone — distinct from
    /// never having set a goal. Drives the provider-context override.
    #[cfg(test)]
    pub(crate) fn goal_cleared(&self) -> bool {
        self.goal_cleared
    }

    /// Fold the NEWEST `GoalUpdated` entry out of a history slice. A clear
    /// tombstone (`goal: None`) wins like any other update; the returned
    /// bool is true exactly when the newest update is a tombstone (so a
    /// "cleared" goal is distinguishable from "never set").
    fn fold_goal(history: &[SessionEntry]) -> (Option<GoalSnapshot>, bool) {
        match history
            .iter()
            .rev()
            .find(|entry| matches!(entry, SessionEntry::GoalUpdated { .. }))
        {
            Some(SessionEntry::GoalUpdated { goal: Some(goal) }) => (Some(goal.clone()), false),
            Some(SessionEntry::GoalUpdated { goal: None }) => (None, true),
            _ => (None, false),
        }
    }

    /// Append a single entry to the history (e.g. a startup notice injected
    /// after resume). The entry has no located key: its oversized fields
    /// stay FULL in provider projections (the total-budget stage will fail
    /// closed later). When the entry IS persisted, prefer
    /// [`Self::apply_entry_located`] so receipts can be issued.
    pub fn push_entry(&mut self, entry: SessionEntry) {
        self.entry_locations.push(None);
        self.history.push(entry);
    }

    /// Apply one entry with its exact physical located key (the runner's
    /// durable-append path: the store's `append_located` resolved before
    /// this, so a receipt emitted later always points at a persisted row).
    pub(crate) fn apply_entry_located(
        &mut self,
        entry: SessionEntry,
        location: Option<EntryLocation>,
    ) {
        if let SessionEntry::GoalUpdated { goal } = &entry {
            self.goal = goal.clone();
            self.goal_cleared = goal.is_none();
        }
        self.entry_locations.push(location);
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
                | SessionEntry::ForkedFrom { .. }
                | SessionEntry::Error { .. }
                | SessionEntry::GoalUpdated { .. } => {}
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
    /// everything after it. This is the canonical, FULL/LOSSLESS projection
    /// (the session goal is folded in first; oversized persisted fields are
    /// NOT bounded here). Provider request copies are bounded by
    /// [`Self::context_request`]; `prepare_compaction` keeps this full view
    /// for its retained tail and slices the bounded copy for the request.
    pub fn context(&self) -> Vec<Message> {
        self.repaired_items()
            .into_iter()
            .map(|item| item.message)
            .collect()
    }

    /// The bounded provider request copy: identical to [`Self::context`]
    /// except that eligible oversized persisted fields (background
    /// completion output, tool content, notice text, historical
    /// user/assistant content, compaction summary/retained) are projected
    /// as a UTF-8-safe head+tail plus a session-local `eout1` ref
    /// (`read_output` ref). System messages, the session goal, the CURRENT
    /// actual user message, tool call ids/names/arguments, reasoning, and
    /// images are kept exact. A field is left full when no located key
    /// exists for its entry or when no registry is installed —
    /// never an unusable ref.
    pub fn context_request(&self) -> Vec<Message> {
        self.repaired_items()
            .into_iter()
            .map(|item| self.project_item(&item))
            .collect()
    }

    /// [`Self::context_items`] with tool-call pairing repaired (the same
    /// `repair_tool_pairs` pass, item-aware so the located metadata stays
    /// aligned with the surviving messages).
    fn repaired_items(&self) -> Vec<ContextItem> {
        repair_item_pairs(self.context_items())
    }

    /// One item per provider message, in context order, carrying the
    /// located key of the source entry (when one exists), the eligible
    /// field, the never-bounded message prefix, and the actual-user flag
    /// that keeps the CURRENT user prompt full.
    fn context_items(&self) -> Vec<ContextItem> {
        let mut items = Vec::new();
        if let Some(prefix) = &self.context_prefix {
            items.push(ContextItem::system(Message::System {
                content: prefix.clone(),
            }));
        }
        // The session goal is projected into EVERY provider call (before
        // compaction summaries, so it survives compaction and resume). It
        // is not a user prompt and never lands in history as a message.
        // After a clear, an explicit "none (cleared)" override is injected
        // so a stale compaction summary can never re-introduce the old goal.
        if let Some(goal) = &self.goal {
            items.push(ContextItem::system(Message::System {
                content: format!("[session goal]\n{}", goal_projection_text(goal)),
            }));
        } else if self.goal_cleared {
            items.push(ContextItem::system(Message::System {
                content: "[session goal]\nnone (cleared)".into(),
            }));
        }
        let mut start = 0;
        if let Some(index) = self
            .history
            .iter()
            .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        {
            let SessionEntry::Compaction {
                summary,
                retained,
                current_prompt_at,
                no_current_prompt,
            } = &self.history[index]
            else {
                unreachable!()
            };
            let location = self
                .entry_locations
                .get(index)
                .and_then(|location| location.clone());
            let header = "[compacted summary of earlier conversation]\n";
            let prefix = match &self.compaction_reminder {
                Some(reminder) => format!("{header}\n[standing directive: {reminder}]\n\n"),
                None => header.to_owned(),
            };
            items.push(ContextItem {
                message: Message::User {
                    content: format!("{prefix}{summary}"),
                    images: vec![],
                },
                prefix,
                location: location.clone(),
                field: Some(FieldId::CompactionSummary),
                field_total: None,
                actual_user: false,
                keep_full: false,
            });
            // The compaction's retained tail is persisted inside the
            // compaction entry: its located key is the compaction entry's,
            // and the paged field is the FULL retained array
            // (`compaction_retained`). The receipt for any bounded retained
            // message binds the whole array's byte length (what read_output
            // returns), not the single message's content length.
            let retained_total = serde_json::to_vec(retained)
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            // The actual-user provenance of the retained tail is persisted
            // in the compaction entry (`current_prompt_at`: the index of
            // the current prompt inside `retained`). For legacy compactions
            // persisted before the field existed, fall back to the first
            // user-shaped retained message (the retained tail opens with
            // the current prompt for main sessions). A SINGLE-TASK retained
            // tail carries no real user at all: the explicit
            // `no_current_prompt` marker keeps the projection from falling
            // back — a retained background completion is user-shaped but is
            // never the current prompt.
            let current_prompt_at = match (current_prompt_at, no_current_prompt) {
                (Some(index), _) => Some(*index),
                (None, true) => None,
                (None, false) => retained
                    .iter()
                    .position(|message| matches!(message, Message::User { .. })),
            };
            for (i, message) in retained.iter().enumerate() {
                let is_user = matches!(message, Message::User { .. });
                items.push(ContextItem {
                    message: message.clone(),
                    prefix: String::new(),
                    location: location.clone(),
                    field: Some(FieldId::CompactionRetained),
                    field_total: Some(retained_total),
                    actual_user: is_user && Some(i) == current_prompt_at,
                    keep_full: false,
                });
            }
            start = index + 1;
        }
        for (i, entry) in self.history[start..].iter().enumerate() {
            let i = start + i;
            let location = self
                .entry_locations
                .get(i)
                .and_then(|location| location.clone());
            match entry {
                SessionEntry::Message { message } => items.push(ContextItem {
                    message: message.clone(),
                    prefix: String::new(),
                    location,
                    field: None,
                    field_total: None,
                    actual_user: matches!(message, Message::User { .. }),
                    keep_full: false,
                }),
                // Notices are system-injected events; surface them to the
                // model as user messages so the model reacts to background
                // completions and task-death notices. Arbitrary
                // user/session Notices pass through the same projection
                // (field = the whole text).
                SessionEntry::Notice { text } => items.push(ContextItem {
                    message: Message::User {
                        content: text.clone(),
                        images: vec![],
                    },
                    prefix: String::new(),
                    location,
                    field: Some(FieldId::NoticeText),
                    field_total: None,
                    actual_user: false,
                    keep_full: false,
                }),
                // Structured background completions: same surface as
                // before, but the header (id + label) is never bounded —
                // only the output field carries a receipt when oversized.
                SessionEntry::BackgroundCompletion {
                    id, output, label, ..
                } => {
                    let header = match label.as_ref().map(|l| l.trim()).filter(|l| !l.is_empty()) {
                        Some(l) => format!("[background task {id} completed: {l}]"),
                        None => format!("[background task {id} completed]"),
                    };
                    items.push(ContextItem {
                        message: Message::User {
                            content: format!("{header}\n{output}"),
                            images: vec![],
                        },
                        prefix: format!("{header}\n"),
                        location,
                        field: Some(FieldId::BgOutput),
                        field_total: None,
                        actual_user: false,
                        keep_full: false,
                    });
                }
                // Fork provenance is audit/display only; never put it on
                // the provider wire.
                SessionEntry::ForkedFrom { .. } => {}
                // Compactions other than the newest one cannot appear after
                // `start` (rposition found the last); defensive no-op.
                SessionEntry::Compaction { .. } => {}
                // Harness errors are audit-only: they never enter the
                // provider context (a failed call must not leak its own
                // error text into the next call).
                SessionEntry::Error { .. } => {}
                // Goal updates are projected at the front of context();
                // replaying them here would duplicate the projection.
                SessionEntry::GoalUpdated { .. } => {}
            }
        }
        // The CURRENT actual user message (the current turn's prompt) is
        // always kept full: mark the LAST actual-user item.
        if let Some(last) = items.iter().rposition(|item| item.actual_user) {
            items[last].keep_full = true;
        }
        // The newest pager result must be complete in the next provider
        // request: it is precisely the page the model asked to inspect.
        if let Some(last) = items.iter().rposition(
            |item| matches!(&item.message, Message::Tool { name, .. } if name == "read_output"),
        ) {
            items[last].keep_full = true;
        }
        items
    }

    /// Project one context item to its bounded request copy (see
    /// [`Self::context_request`] for the rules).
    fn project_item(&self, item: &ContextItem) -> Message {
        if item.keep_full {
            return item.message.clone();
        }
        let field = match item.field {
            Some(field) => field,
            None => match &item.message {
                Message::User { .. } => FieldId::UserContent,
                Message::Assistant(_) => FieldId::AssistantContent,
                Message::Tool { .. } => FieldId::ToolContent,
                Message::System { .. } => return item.message.clone(),
            },
        };
        // Receipt only when a persisted location exists; legacy/test in-memory
        // entries stay full.
        let Some(location) = &item.location else {
            return item.message.clone();
        };
        let field_total = |text_len: usize| item.field_total.unwrap_or(text_len);
        match &item.message {
            Message::User { content, images } => {
                let rest = &content[item.prefix.len()..];
                let bounded = bound_field(rest, location, field, field_total(rest.len()));
                Message::User {
                    content: format!("{}{}", item.prefix, bounded),
                    images: images.clone(),
                }
            }
            Message::Assistant(assistant) => {
                debug_assert!(item.prefix.is_empty());
                let mut assistant = assistant.clone();
                if let Some(content) = &assistant.content {
                    assistant.content = Some(bound_field(
                        content,
                        location,
                        field,
                        field_total(content.len()),
                    ));
                }
                Message::Assistant(assistant)
            }
            Message::Tool {
                call_id,
                name,
                content,
                images,
                is_error,
                synthetic,
            } => {
                debug_assert!(item.prefix.is_empty());
                Message::Tool {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: bound_field(content, location, field, field_total(content.len())),
                    images: images.clone(),
                    is_error: *is_error,
                    synthetic: *synthetic,
                }
            }
            Message::System { .. } => item.message.clone(),
        }
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
    pub async fn next_background_completion(
        &mut self,
    ) -> Option<(u64, String, Option<String>, BackgroundTrace)> {
        loop {
            match self.background_receiver.recv().await {
                Some(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label,
                    started_at_ms,
                    duration_ms,
                    exit_code,
                    signal,
                    status,
                    kind,
                }) => {
                    let trace = BackgroundTrace {
                        started_at_ms,
                        duration_ms,
                        exit_code,
                        signal: signal.clone(),
                        status: status.clone(),
                        kind: kind.clone(),
                    };
                    self.pending_background.push_back((
                        id,
                        output.clone(),
                        label.clone(),
                        trace.clone(),
                    ));
                    // No fanout here either: idle and mid-turn completions
                    // both land in the session log as a user message at the
                    // next turn boundary. The TUI prints this return value
                    // itself; fanning out would duplicate the line.
                    return Some((id, output, label, trace));
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
        // A true turn starts here: fresh/queued user prompt, idle
        // background-completion follow-up, or a direct Agent::run call.
        // Per-turn tool state (poll guard) resets; model rounds,
        // mid-tool-batch, and manual/auto compaction never do.
        self.start_turn();
        self.drain_background();
        self.inject_pending_background().await?;
        // Reset the auto-compact latch at the start of each new user turn so
        // a failed compaction doesn't permanently prevent future attempts.
        self.auto_compacted = false;
        if !prompt.is_empty() {
            // Direct-Agent path (tests / Agent::run): no durable store, so
            // no located key — the current prompt stays FULL anyway (the
            // current actual user is never bounded).
            self.apply_entry_located(
                Message::User {
                    content: prompt,
                    images: vec![],
                }
                .into(),
                None,
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
        self.inject_pending_background().await?;
        result.map(|answer| (answer, injected_at_end))
    }

    /// Summarize the replaced history and append a compaction entry. Main
    /// retains the current prompt plus a bounded recent activity tail, while
    /// the full history stays append-only.
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
        // The canonical FULL context (lossless; never bounded here) and its
        // item metadata. The bounded compaction REQUEST is derived from the
        // same items below; the retained projection is reconstructed below.
        let items = self.repaired_items();
        let context: Vec<Message> = items.iter().map(|item| item.message.clone()).collect();
        // Main retains the current prompt separately from a bounded recent
        // activity tail. The compacted request and retained projection are
        // therefore deliberately non-contiguous.
        let (split, current_prompt) = match self.compaction_mode {
            CompactionMode::Main => {
                let Some(current_prompt) = items.iter().rposition(|item| item.actual_user) else {
                    anyhow::bail!("nothing to compact");
                };
                let candidate = (current_prompt + 1).max(context.len().saturating_sub(RETAIN_TAIL));
                // Never cut through an assistant tool-call batch. Move a
                // tool cut backward to the assistant that opened the batch,
                // retaining that complete batch even if the tail exceeds
                // RETAIN_TAIL by its size. This also keeps pending calls and
                // their synthetic results together after repair_item_pairs.
                let split = if candidate < context.len()
                    && matches!(context[candidate], Message::Tool { .. })
                {
                    context[..candidate]
                        .iter()
                        .rposition(|message| matches!(message, Message::Assistant(_)))
                        .unwrap_or(candidate)
                } else {
                    candidate
                };
                (split, Some(current_prompt))
            }
            CompactionMode::SingleTask => {
                // Single-task behavior is a bounded tool-activity window;
                // repeated compaction must work without an actual user.
                let start = context.len().saturating_sub(RETAIN_TAIL);
                if start == 0 {
                    anyhow::bail!("nothing to compact");
                }
                let split = match context[start..]
                    .iter()
                    .position(|message| matches!(message, Message::Assistant(_)))
                {
                    Some(offset) => start + offset,
                    None => match context[..start]
                        .iter()
                        .rposition(|message| matches!(message, Message::Assistant(_)))
                    {
                        Some(index) => index,
                        None => anyhow::bail!("nothing to compact"),
                    },
                };
                (split, None)
            }
        };
        // Do not call the model unless the portion actually replaced by the
        // compaction contains assistant/tool activity. In Main mode the
        // current prompt is retained and supplied to the summary request,
        // so a short first turn is correctly a no-op.
        let compactable = match self.compaction_mode {
            CompactionMode::Main => items[..split].iter().enumerate().any(|(index, item)| {
                index != current_prompt.unwrap_or(usize::MAX)
                    && matches!(item.message, Message::Assistant(_) | Message::Tool { .. })
            }),
            CompactionMode::SingleTask => items[..split]
                .iter()
                .any(|item| matches!(item.message, Message::Assistant(_) | Message::Tool { .. })),
        };
        if !compactable {
            anyhow::bail!("nothing to compact");
        }
        // Main's request includes the current prompt and older activity from
        // its turn; the retained vector below is [prompt] + [recent tail].
        // SingleTask keeps the historical contiguous request window.
        let mut request: Vec<Message> = items[..split]
            .iter()
            .map(|item| self.project_item(item))
            .collect();
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
            content: COMPACTION_SUMMARY_PROMPT.into(),
            images: vec![],
        });
        let retained = match self.compaction_mode {
            CompactionMode::Main => {
                let current_prompt = current_prompt.expect("Main compaction has a prompt");
                let mut retained = Vec::with_capacity(1 + context.len() - split);
                retained.push(context[current_prompt].clone());
                retained.extend_from_slice(&context[split..]);
                retained
            }
            CompactionMode::SingleTask => context[split..].to_vec(),
        };
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
        // Single model call, no sanity gate: any non-empty summary without
        // tool calls is accepted and persisted as-is.
        let (response, usage) = model.complete(&request, &[], Some(&mut on_delta)).await?;
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
                retained,
                // Preserve the actual-user provenance through the
                // persisted entry: Main reconstructs retained as the
                // current prompt followed by its activity tail, so it is
                // always Some(0). Single-task tool-window tails carry no
                // actual user → `None`, plus the explicit marker so resume
                // never mistakes a background completion for the prompt.
                current_prompt_at: (self.compaction_mode == CompactionMode::Main).then_some(0),
                no_current_prompt: self.compaction_mode == CompactionMode::SingleTask,
            },
            summary,
            usage,
        })
    }

    pub(crate) fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub(crate) async fn execute_tool(&mut self, call: &ToolCall) -> Result<ToolOutput, String> {
        Self::execute_on(&self.tools, call).await
    }

    pub(crate) fn apply_entry(&mut self, entry: SessionEntry) {
        self.apply_entry_located(entry, None);
    }

    pub(crate) fn apply_usage(&mut self, usage: Option<Usage>, refresh_context: bool) {
        self.record_usage(usage, refresh_context);
    }

    pub(crate) fn emit_event(&mut self, event: AgentEvent) {
        self.emit(event);
    }

    pub(crate) async fn after_tool_entry(
        &mut self,
        call: &ToolCall,
        result: &Result<ToolOutput, String>,
    ) -> Result<(), String> {
        if result.is_ok()
            && (call.name == "bash" || call.name == "pwsh")
            && is_background_call(call)
            && !is_detached_background_call(call)
            && let Some(id) = started_task_id(
                result
                    .as_ref()
                    .map(|output| output.content.as_str())
                    .unwrap_or_default(),
            )
        {
            self.running_background.insert(id);
            if let Some(record) = &self.background_record {
                let command = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|args| args["command"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| call.arguments.clone());
                let label = preview(&command, 100);
                if let Err(error) = record
                    .store
                    .record_background_start_durable(
                        &record.root,
                        &record.session,
                        id,
                        &label,
                        Some(&command),
                        None,
                    )
                    .await
                {
                    self.running_background.remove(&id);
                    return Err(format!("cannot record background task owner: {error:#}"));
                }
            }
        }
        Ok(())
    }

    fn push_message(&mut self, message: Message) {
        // Direct-Agent paths (tests / compact()): the entry is not tracked
        // by a durable store here, so it has no located key — oversized
        // fields stay FULL in projections (never an unusable receipt ref).
        self.apply_entry_located(message.into(), None);
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
                    ..Usage::default()
                },
            });
        }
    }

    pub(crate) async fn complete_round(
        &mut self,
        specs: &[ToolSpec],
    ) -> anyhow::Result<RoundOutput> {
        let mut produced_content_delta = false;
        // The BOUNDED request copy: canonical `context()` stays full and
        // lossless; oversized eligible persisted fields are projected as
        // head+tail + a session-local `read_output` ref.
        let mut context = self.context_request();
        // Non-vision models cannot consume image parts. Strip them from the
        // *request* only, so the wire gate never rejects the whole history
        // and the session is not locked (this is the fallback that lets
        // sessions with legacy image-bearing history — e.g. short
        // single-task histories where compaction cannot run — keep
        // working). The persisted history is untouched: switching back to a
        // vision model restores the images on the next request.
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
            self.inject_pending_background().await?;
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
            // Poll guard: the terminating unchanged-snapshot
            // get_background_tasks poll (3rd for subagents, 5th for the
            // main agent) returns an internal sentinel. The sentinel must
            // never enter history/UI — the committed content is the
            // model-facing POLL_ERROR — and the local latch only ends the
            // turn AFTER the full sibling batch, so every call in this
            // assistant batch gets a real ToolResult (no repair_tool_pairs
            // hole).
            let mut poll_terminate = false;
            for call in &assistant.tool_calls {
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = Self::execute_on(&self.tools, call).await;
                if is_poll_guard_terminate(&result) {
                    poll_terminate = true;
                }
                self.after_tool_entry(call, &result)
                    .await
                    .map_err(anyhow::Error::msg)?;
                // One canonical image-bearing Tool entry: the text summary
                // plus the structured image references ride on the Tool
                // message itself (no marker parsing, no synthetic User).
                // Non-vision models never see the images: the request copy
                // is stripped at send time (strip_images), while history
                // keeps them so a later vision model regains them.
                let (content, images) = match &result {
                    Ok(output) => (output.content.clone(), output.images.clone()),
                    Err(error) => (tool_error_content(error).to_owned(), Vec::new()),
                };
                let is_error = result.is_err();
                self.emit(AgentEvent::ToolResult {
                    is_error,
                    content: content.clone(),
                });
                self.push_message(Message::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content,
                    images,
                    is_error,
                    synthetic: false,
                });
            }
            if poll_terminate {
                // The full batch is committed; emit the termination notice
                // and end the turn. A follow-up turn (queued prompt or a
                // background completion folded at turn end) starts with the
                // guard reset.
                self.emit(AgentEvent::Notice(POLL_GUARD_TERMINATION_NOTICE.into()));
                return Ok(String::new());
            }
        }
    }

    /// Execute a tool call against a tool list. Associated function (not a
    /// method) so the returned future does not borrow `&self`, keeping
    /// `Agent::run` futures `Send` for use in `tokio::spawn`.
    async fn execute_on(tools: &[Box<dyn Tool>], call: &ToolCall) -> Result<ToolOutput, String> {
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

    async fn inject_pending_background(&mut self) -> anyhow::Result<()> {
        while !self.pending_background.is_empty() {
            let entry = self.peek_background_entry().expect("pending entry");
            self.apply_entry(entry);
            self.ack_background_entry()
                .await
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    pub(crate) fn drain_background_ready(&mut self) {
        while let Ok(AgentEvent::BackgroundCompleted {
            id,
            output,
            label,
            started_at_ms,
            duration_ms,
            exit_code,
            signal,
            status,
            kind,
        }) = self.background_receiver.try_recv()
        {
            self.pending_background.push_back((
                id,
                output.clone(),
                label.clone(),
                BackgroundTrace {
                    started_at_ms,
                    duration_ms,
                    exit_code,
                    signal: signal.clone(),
                    status: status.clone(),
                    kind: kind.clone(),
                },
            ));
            if let Some(subscriber) = &self.subscriber {
                let _ = subscriber.send(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label,
                    started_at_ms,
                    duration_ms,
                    exit_code,
                    signal,
                    status,
                    kind,
                });
            }
        }
    }

    pub(crate) async fn wait_background_ready(&mut self) -> bool {
        self.next_background_completion().await.is_some()
    }

    pub(crate) fn peek_background_entry(&self) -> Option<SessionEntry> {
        self.pending_background
            .front()
            .map(
                |(id, output, label, trace)| SessionEntry::BackgroundCompletion {
                    id: *id,
                    output: output.clone(),
                    label: label.clone(),
                    started_at_ms: trace.started_at_ms,
                    duration_ms: trace.duration_ms,
                    exit_code: trace.exit_code,
                    signal: trace.signal.clone(),
                    status: trace.status.clone(),
                    kind: trace.kind.clone(),
                },
            )
    }

    pub(crate) async fn ack_background_entry(&mut self) -> Result<(), String> {
        let Some((id, _output, _label, _trace)) = self.pending_background.front() else {
            return Ok(());
        };
        if let Some(record) = &self.background_record {
            record
                .store
                .clear_background_task_durable(&record.root, &record.session, *id)
                .await
                .map_err(|error| format!("cannot clear background task owner: {error:#}"))?;
        }
        let (id, _output, _label, _trace) = self
            .pending_background
            .pop_front()
            .expect("background entry remains pending until durable clear");
        self.running_background.remove(&id);
        Ok(())
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

    /// Successful-compaction latch reset (runner's `compact_operation`
    /// success path). Unlike `reset_auto_compact_request` (the failure /
    /// cancel path, whose semantics must stay untouched), this only clears
    /// the latch: `last_context_input` keeps its pre-compaction baseline
    /// (refresh_context=false), so the next regular round re-evaluates it
    /// against the window in the run loop.
    ///
    /// Debounce: this is safe because the run loop re-checks
    /// `last_context_input` only at the END of a regular round. If that
    /// round turned the page on the still-large current turn, the fresh
    /// `last_context_input` reflects the post-compaction real size — below
    /// 80% it stays silent. If the current turn is still huge (didn't turn
    /// the page), the end of the NEXT round evaluates it once more — never
    /// twice within the same round.
    pub(crate) fn clear_auto_compacted(&mut self) {
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
