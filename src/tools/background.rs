use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use crate::agent::{AgentEvent, BackgroundTrace, preview};

use super::bash::{Shell, run_bash};

#[derive(Clone)]
pub struct BackgroundTasks {
    registry: Arc<BackgroundRegistry>,
    /// Compatibility sender for callers which schedule directly on a registry
    /// (delegates do this). Bash deliberately does not use this field.
    /// Wrapped in `Arc<Mutex<Option<_>>>` so that `Agent::new`'s
    /// `set_event_sender` on one clone (the Delegate tool's) is visible to
    /// every other clone (e.g. the LiveSession's), which share the registry.
    sender: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>>>,
    /// Background bash timeout; `None` = no timeout (run forever).
    timeout: Option<Duration>,
    sandbox: Option<crate::config::Sandbox>,
}

struct BackgroundRegistry {
    next_id: AtomicU64,
    running: std::sync::Mutex<Vec<RunningTask>>,
}

type Completion =
    Arc<std::sync::Mutex<Option<Box<dyn FnOnce(u64, String, BackgroundTrace) + Send>>>>;

/// Structured exit metadata of a finished background task's underlying run,
/// written by the task's own work through an out-slot (bash: `run_bash`
/// records code/signal/status; delegate: the subagent result's success
/// flag). `spawn_inner` merges it with the task's start/duration timing
/// into the completion [`BackgroundTrace`] passed to the widened
/// `on_complete` callback.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskExit {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    /// "completed" | "failed" | "killed".
    pub status: Option<String>,
}

/// Shared out-slot for a task's exit metadata, following the
/// `process_group_slot` pattern: the work closure writes it, `spawn_inner`
/// reads it at finish time.
pub type ExitSlot = Arc<std::sync::Mutex<TaskExit>>;

/// A fresh, empty exit slot for callers that record no structured exit
/// metadata (their trace still carries start/duration/kind).
pub fn new_exit_slot() -> ExitSlot {
    Arc::new(std::sync::Mutex::new(TaskExit::default()))
}

/// Fill a task's exit slot with a truthful `failed` status when it is still
/// empty. Shell spawn/IO failures return from `run_bash` BEFORE the slot is
/// populated, which would otherwise leave the completion trace with
/// start/duration/kind but no status. Explicit metadata already written by
/// the timeout/cancel paths (`killed`/SIGKILL) is preserved.
pub(super) fn mark_failed_if_empty(exit: &mut TaskExit) {
    if exit.status.is_none() {
        exit.exit_code = None;
        exit.signal = None;
        exit.status = Some("failed".into());
    }
}

impl RunningTask {
    /// The completion trace at finish time: start/duration timing from the
    /// registry push, exit fields from the task's own work out-slot, and
    /// the kind derived from the shell (`None` shell = delegate).
    fn completion_trace(&self) -> BackgroundTrace {
        let exit = self.exit.lock().unwrap();
        BackgroundTrace {
            started_at_ms: self.started_at_ms,
            duration_ms: Some(self.started_at.elapsed().as_millis() as u64),
            exit_code: exit.exit_code,
            signal: exit.signal.clone(),
            status: exit.status.clone(),
            kind: self
                .shell_name
                .clone()
                .map(|name| name.to_ascii_lowercase())
                .or_else(|| Some("delegate".to_owned())),
        }
    }
}

/// A live, read-only snapshot of a background task's combined stdout+stderr
/// tail (capped at 16KB), shared between the running `bash` capture and the
/// TUI panel. Only `BackgroundTasks::start` attaches one.
pub type OutputSlot = Arc<std::sync::Mutex<Vec<u8>>>;

const SLOT_LIMIT: usize = 16 * 1024;

pub(super) fn slot_append(slot: &OutputSlot, chunk: &[u8]) {
    let mut bytes = slot.lock().unwrap();
    bytes.extend_from_slice(chunk);
    if bytes.len() > SLOT_LIMIT {
        let excess = bytes.len() - SLOT_LIMIT;
        bytes.drain(..excess);
    }
}

/// Full combined stdout+stderr of a background bash task, keep-first capped
/// at [`FULL_SPOOL_LIMIT`]. The 16 KiB tail slot (panel preview, model-facing
/// output) is untouched; the TUI detail view reads the full output
/// exclusively through [`TaskSpool::window`]. The Arc in [`RunningTask`]
/// keeps the data alive after the task leaves the registry, so a finished
/// task's output stays paged in the detail view.
pub struct TaskSpool {
    state: std::sync::Mutex<SpoolState>,
}

pub(super) const FULL_SPOOL_LIMIT: usize = 16 * 1024 * 1024; // 16 MiB keep-first
pub(super) const CHECKPOINT_INTERVAL: usize = 256; // one (line_no, byte_offset) per 256 lines
pub(super) const LINE_TRUNCATED_MARKER: &str = " [line truncated]";

struct SpoolState {
    /// Append-only bytes; stops growing (and sets `truncated`) at the cap.
    bytes: Vec<u8>,
    /// Completed lines (`\n` count), maintained incrementally on append.
    line_count: usize,
    /// Sparse index: line `line_no` (0-based) starts at `byte_offset`.
    checkpoints: Vec<(usize, usize)>,
    /// Whether the keep-first cap was hit (tail is missing).
    truncated: bool,
}

impl Default for TaskSpool {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskSpool {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SpoolState {
                bytes: Vec::new(),
                line_count: 0,
                checkpoints: Vec::new(),
                truncated: false,
            }),
        }
    }

    /// Append a chunk, maintaining the line count and sparse checkpoints.
    /// Once the cap is reached further bytes are discarded.
    pub fn append(&self, chunk: &[u8]) {
        let mut state = self.state.lock().unwrap();
        if state.truncated {
            return;
        }
        let room = FULL_SPOOL_LIMIT.saturating_sub(state.bytes.len());
        let take = chunk.len().min(room);
        if take < chunk.len() {
            state.truncated = true;
        }
        let base = state.bytes.len();
        state.bytes.extend_from_slice(&chunk[..take]);
        let mut line = state.line_count;
        for (index, byte) in chunk[..take].iter().enumerate() {
            if *byte == b'\n' {
                line += 1;
                if line.is_multiple_of(CHECKPOINT_INTERVAL) {
                    state.checkpoints.push((line, base + index + 1));
                }
            }
        }
        state.line_count = line;
    }

    /// Total recorded bytes (never exceeds [`FULL_SPOOL_LIMIT`]).
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total lines: completed lines plus a final unterminated line.
    pub fn line_count(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.line_count + usize::from(has_partial_tail(&state))
    }

    /// Whether the keep-first cap was hit.
    pub fn truncated(&self) -> bool {
        self.state.lock().unwrap().truncated
    }

    /// Full spool contents (keep-first capped at [`FULL_SPOOL_LIMIT`]),
    /// cloned under the lock. The web `/output` endpoint serves these raw
    /// bytes; lossy UTF-8 decoding happens at the wire boundary.
    pub fn bytes(&self) -> Vec<u8> {
        self.state.lock().unwrap().bytes.clone()
    }

    /// A line-aligned window starting at 0-based spool line `start_line`,
    /// at most `max_lines` lines and `max_bytes` bytes of content. Line
    /// content is UTF-8-lossy decoded (matching the panel preview); a
    /// partial trailing character on the final unterminated line is clamped
    /// away until the character completes. A single line that exceeds the
    /// byte budget keeps its tail plus a truncation marker. Returns `None`
    /// when `start_line` is past the end of the spool.
    pub fn window(
        &self,
        start_line: usize,
        max_lines: usize,
        max_bytes: usize,
    ) -> Option<SpoolWindow> {
        let state = self.state.lock().unwrap();
        let total = state.line_count + usize::from(has_partial_tail(&state));
        if start_line >= total {
            return None;
        }
        let max_lines = max_lines.max(1);
        let max_bytes = max_bytes.max(1);
        let mut cursor = line_offset(&state, start_line);
        let mut lines = Vec::new();
        let mut used = 0usize;
        while lines.len() < max_lines && cursor < state.bytes.len() && used < max_bytes {
            let line_end = state.bytes[cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(state.bytes.len(), |pos| cursor + pos);
            // The final unterminated line clamps a partial trailing char;
            // terminated lines decode lossy like the panel preview.
            let content_end = if line_end < state.bytes.len() {
                line_end
            } else {
                utf8_floor(&state.bytes, line_end)
            };
            let content_len = content_end - cursor;
            if used + content_len > max_bytes {
                // Single line exceeds the remaining budget: keep its tail.
                // Skip it entirely when even the marker would not fit.
                let budget = max_bytes - used;
                if budget >= LINE_TRUNCATED_MARKER.len() {
                    lines.push(truncated_tail_line(
                        &state.bytes,
                        cursor,
                        content_end,
                        budget,
                    ));
                }
                break;
            }
            lines.push(String::from_utf8_lossy(&state.bytes[cursor..content_end]).into_owned());
            used += content_len;
            if line_end >= state.bytes.len() {
                break;
            }
            cursor = line_end + 1;
        }
        Some(SpoolWindow {
            lines,
            first_line: start_line,
            line_count: total,
            truncated: state.truncated,
        })
    }
}

/// A line-aligned slice of a [`TaskSpool`], for the TUI detail view.
pub struct SpoolWindow {
    /// Line contents (no trailing newlines), lossy-decoded.
    pub lines: Vec<String>,
    /// 0-based spool line number of `lines[0]`.
    pub first_line: usize,
    /// Total lines in the spool (completed plus a final unterminated line).
    pub line_count: usize,
    /// Whether the spool hit its keep-first cap (tail is missing).
    pub truncated: bool,
}

/// Whether the spool ends in a line without a trailing newline.
fn has_partial_tail(state: &SpoolState) -> bool {
    state.bytes.last().is_some_and(|byte| *byte != b'\n')
}

/// Byte offset where 0-based `line` starts: binary-search the sparse
/// checkpoints, then scan forward at most [`CHECKPOINT_INTERVAL`] newlines.
fn line_offset(state: &SpoolState, line: usize) -> usize {
    let (mut cp_line, mut offset) = (0usize, 0usize);
    match state.checkpoints.binary_search_by_key(&line, |&(l, _)| l) {
        Ok(index) => {
            cp_line = state.checkpoints[index].0;
            offset = state.checkpoints[index].1;
        }
        Err(index) if index > 0 => {
            cp_line = state.checkpoints[index - 1].0;
            offset = state.checkpoints[index - 1].1;
        }
        Err(_) => {}
    }
    while cp_line < line && offset < state.bytes.len() {
        if state.bytes[offset] == b'\n' {
            cp_line += 1;
        }
        offset += 1;
    }
    offset
}

/// Round a byte offset down to a UTF-8 boundary: a partial trailing
/// multi-byte character is dropped until it completes. (tui.rs has its own
/// `&str`-based helpers; this one works on raw bytes.)
fn utf8_floor(bytes: &[u8], end: usize) -> usize {
    let end = end.min(bytes.len());
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => end,
        Err(error) => error.valid_up_to(),
    }
}

/// Tail of a single over-long line within `budget` bytes, plus a marker.
/// The cut never splits a multi-byte character.
fn truncated_tail_line(bytes: &[u8], start: usize, end: usize, budget: usize) -> String {
    let mut tail_start = end.saturating_sub(budget.saturating_sub(LINE_TRUNCATED_MARKER.len()));
    if tail_start > start {
        while tail_start < end && (bytes[tail_start] & 0xC0) == 0x80 {
            tail_start += 1;
        }
    }
    let mut text = String::from_utf8_lossy(&bytes[tail_start..end]).into_owned();
    if tail_start > start {
        text.push_str(LINE_TRUNCATED_MARKER);
    }
    text
}

#[derive(Clone)]
struct RunningTask {
    id: u64,
    label: String,
    /// Full untruncated command for bash tasks (`None` for delegate tasks).
    /// The label is preview-truncated at spawn time, so the detail view
    /// reads this field to show the complete command.
    full_command: Option<String>,
    role: Option<String>,
    process_group: Arc<AtomicI32>,
    handle: Arc<tokio::task::JoinHandle<()>>,
    /// Tool name of the shell that ran this task ("bash"/"pwsh"); `None`
    /// for delegate tasks. Drives the `kind` shown in task lists.
    shell_name: Option<String>,
    output: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
    display_meta: Option<TaskDisplayMeta>,
    /// The session that started this task (`Some` for subagent bash tasks,
    /// `None` = unknown / main session). The registry is shared across
    /// sessions, so the listing session id is not the initiator.
    owner_session: Option<String>,
    completion: Completion,
    /// When the task was pushed to the registry (wall clock, epoch ms).
    started_at_ms: Option<u64>,
    /// Monotonic start instant; `elapsed()` at finish yields the duration.
    started_at: std::time::Instant,
    /// Exit metadata out-slot written by the work closure (bash:
    /// `run_bash`'s out-slot; delegate: the subagent result success flag).
    exit: ExitSlot,
}

/// Structured metadata for delegate-task display in the F2 task panel.
/// Avoids label/string re-parsing. Only populated for delegate tasks.
#[derive(Clone, Debug, Default)]
pub struct TaskDisplayMeta {
    /// Effective delegate execution mode (`true` for background).
    pub background: bool,
    /// Explicit user-provided workspace path (trimmed, non-empty); `None`
    /// means the parent's default workspace was inherited.
    pub workspace: Option<String>,
    /// The subagent's session id (delegate tasks only); lets the web task
    /// panel jump straight to the subagent's transcript without label matching.
    pub subagent_session_id: Option<String>,
    /// The resumed subagent session id (`delegate resume: "<id>"`); `None`
    /// otherwise. Lets the task panel show that the subagent is a continuation.
    pub resume: Option<String>,
}

/// A snapshot of one running background task, for display.
#[derive(Clone, Debug)]
pub struct BackgroundTaskInfo {
    pub id: u64,
    pub label: String,
    /// Full untruncated command for bash tasks; `None` for delegate tasks.
    /// The detail view renders this instead of the truncated label.
    pub full_command: Option<String>,
    pub role: Option<String>,
    /// "bash" for background shell commands, "delegate" for subagent tasks.
    pub kind: String,
    /// Combined stdout/stderr tail so far; empty for non-bash tasks.
    pub output: Vec<u8>,
    /// Delegate-specific display metadata; `None` for non-delegate tasks.
    pub display_meta: Option<TaskDisplayMeta>,
    /// The session that started this task (`Some` for subagent bash tasks,
    /// `None` = unknown / main session). Differs from the listing
    /// `session_id` (the registry owner) when a subagent ran the command.
    pub owner_session: Option<String>,
    /// Epoch milliseconds at task start (`None` when the clock read failed
    /// or the row predates the field).
    pub started_at_ms: Option<u64>,
}

impl BackgroundTasks {
    pub(super) fn new(timeout: Option<Duration>, sandbox: Option<crate::config::Sandbox>) -> Self {
        Self {
            registry: Arc::new(BackgroundRegistry {
                next_id: AtomicU64::new(1),
                running: std::sync::Mutex::new(Vec::new()),
            }),
            sender: Arc::new(std::sync::Mutex::new(None)),
            timeout,
            sandbox,
        }
    }

    /// Set the channel used to deliver background completions. Called by the
    /// agent for tools that hold a shared clone of this registry.
    pub fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }

    /// Whether background completion delivery is currently usable.
    ///
    /// This is a read-only preflight; spawning still performs its own final
    /// sender check to guard against the receiver closing in the meantime.
    pub fn completion_delivery_available(&self) -> bool {
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }

    /// Snapshot of currently running background tasks, for the TUI panel.
    pub fn running(&self) -> Vec<BackgroundTaskInfo> {
        self.registry
            .running
            .lock()
            .unwrap()
            .iter()
            .map(|task| BackgroundTaskInfo {
                id: task.id,
                label: task.label.clone(),
                full_command: task.full_command.clone(),
                role: task.role.clone(),
                kind: task.shell_name.clone().unwrap_or_else(|| "delegate".into()),
                output: task
                    .output
                    .as_ref()
                    .map(|slot| slot.lock().unwrap().clone())
                    .unwrap_or_default(),
                display_meta: task.display_meta.clone(),
                owner_session: task.owner_session.clone(),
                started_at_ms: task.started_at_ms,
            })
            .collect()
    }

    /// Full-output spool of a running bash task; `None` for delegate tasks
    /// and tasks without an output spool. The returned Arc keeps the data
    /// alive after the task finishes, so the TUI detail view can keep
    /// paging through a completed task's output.
    pub fn spool(&self, id: u64) -> Option<Arc<TaskSpool>> {
        self.registry
            .running
            .lock()
            .unwrap()
            .iter()
            .find(|task| task.id == id)
            .and_then(|task| task.spool.clone())
    }

    /// Full combined stdout+stderr of a running bash task (keep-first
    /// capped at [`FULL_SPOOL_LIMIT`]); `None` for unknown task ids and
    /// delegate tasks (which have no output spool). Unlike
    /// [`Self::running`]'s 16 KiB output tail this is the untruncated full
    /// output — the web frontend polls it via
    /// `GET /api/sessions/{id}/tasks/{task_id}/output`.
    pub fn output(&self, id: u64) -> Option<Vec<u8>> {
        self.registry
            .running
            .lock()
            .unwrap()
            .iter()
            .find(|task| task.id == id)
            .and_then(|task| task.spool.as_ref().map(|spool| spool.bytes()))
    }

    /// Cancel a running background task. Aborting its future drops any
    /// in-flight `run_bash`, which kills the process group via its guard.
    /// Returns the cancelled task's label, or `None` if no such task.
    pub fn cancel(&self, id: u64) -> Option<String> {
        self.cancel_matching(id, |_| true)
    }

    /// Cancel a task only when its owner matches the calling subagent. The
    /// ownership check and removal share the registry mutex, so a task cannot
    /// change from visible to cancelled between separate operations.
    pub fn cancel_owned(&self, id: u64, owner_session: &str) -> Option<String> {
        self.cancel_matching(id, |task| {
            task.owner_session.as_deref() == Some(owner_session)
        })
    }

    fn cancel_matching(&self, id: u64, matches: impl Fn(&RunningTask) -> bool) -> Option<String> {
        let task = {
            let mut running = self.registry.running.lock().unwrap();
            let index = running
                .iter()
                .position(|task| task.id == id && matches(task))?;
            running.remove(index)
        };
        task.handle.abort();
        if let Some(done) = task.completion.lock().unwrap().take() {
            let mut exit = task.exit.lock().unwrap();
            // The process was aborted: no exit code was observed. The work
            // may already have written completed/failed into the slot
            // (run_bash records the slot, then its wrapper removes the task
            // from the registry) — but the accepted cancel wins: once the
            // task is removed here, the delivered trace MUST read
            // killed/SIGKILL, never a stale completed/failed from the race.
            exit.exit_code = None;
            exit.signal = Some("SIGKILL".into());
            exit.status = Some("killed".into());
            drop(exit);
            done(
                id,
                "background task cancelled".into(),
                task.completion_trace(),
            );
        }
        Some(task.label)
    }

    /// Start a background bash command. Returns a human-readable "started"
    /// message containing the task id.
    /// `protect_git` controls whether `<workspace>/.git` is bound read-only
    /// inside the sandbox (subagent = true, main agent = false).
    pub fn start(
        &self,
        workspace: Workspace,
        command: String,
        protect_git: bool,
    ) -> Result<String, String> {
        self.start_with_sender(
            workspace,
            command,
            protect_git,
            self.sender.lock().unwrap().clone(),
            self.sandbox.clone(),
            None,
        )
    }

    /// Start a background bash command with an explicit completion sender and
    /// sandbox policy.
    ///
    /// `sandbox` is the POLICY OF THE CALLING BASH FACADE, not the shared
    /// registry's: a subagent's Bash tool passes its own (possibly read-only
    /// narrowed) sandbox so background commands cannot escape into the
    /// parent's wider policy. `start()` passes the registry's default.
    ///
    /// `owner_session` is the session that started the task (`Some` for a
    /// subagent's bash, `None` for the main session / unknown): the registry
    /// is shared, so the task panel needs it to show the true initiator
    /// instead of the registry owner.
    pub fn start_with_sender(
        &self,
        workspace: Workspace,
        command: String,
        protect_git: bool,
        sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
        sandbox: Option<crate::config::Sandbox>,
        owner_session: Option<String>,
    ) -> Result<String, String> {
        let sender = sender
            .filter(|sender| !sender.is_closed())
            .ok_or("background task delivery is unavailable")?;
        let label = preview(&command, 100);
        let completion_label = label.clone();
        self.spawn_bash_command(
            label,
            workspace,
            command,
            protect_git,
            sandbox,
            owner_session,
            move |id, output, trace| {
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label: Some(completion_label.clone()),
                    started_at_ms: trace.started_at_ms,
                    duration_ms: trace.duration_ms,
                    exit_code: trace.exit_code,
                    signal: trace.signal,
                    status: trace.status,
                    kind: trace.kind,
                });
            },
        )
    }

    /// Start a background bash command as a detached daemon / long-lived
    /// service / watcher. The task runs in the shared registry and stays
    /// visible in the task panel, but never delivers a completion event:
    /// it must not block the spawning session from finishing
    /// (FinishWhenIdle ignores it), and no completion is promised to a
    /// session that may already have ended. The task leaves the registry
    /// when it finishes on its own.
    ///
    /// `owner_session` is the session that started the task (`Some` for a
    /// subagent's bash, `None` for the main session / unknown), see
    /// [`Self::start_with_sender`].
    pub fn start_detached(
        &self,
        workspace: Workspace,
        command: String,
        protect_git: bool,
        sandbox: Option<crate::config::Sandbox>,
        owner_session: Option<String>,
    ) -> Result<String, String> {
        let label = preview(&command, 100);
        self.spawn_bash_command(
            label,
            workspace,
            command,
            protect_git,
            sandbox,
            owner_session,
            // Detached: on_complete is a no-op, so no completion entry is
            // ever produced for this task.
            |_, _, _| {},
        )
    }

    /// Shared bash background spawn: register the task, run the command
    /// under the facade's sandbox policy, and remove it from the registry
    /// when it completes. `on_complete` decides what happens at completion
    /// (deliver a `BackgroundCompleted` event, or nothing for detached
    /// tasks).
    #[allow(clippy::too_many_arguments)]
    fn spawn_bash_command(
        &self,
        label: String,
        workspace: Workspace,
        command: String,
        protect_git: bool,
        sandbox: Option<crate::config::Sandbox>,
        owner_session: Option<String>,
        on_complete: impl FnOnce(u64, String, BackgroundTrace) + Send + 'static,
    ) -> Result<String, String> {
        let shell = Shell::detect()?;
        let process_group = Arc::new(AtomicI32::new(0));
        let output: OutputSlot = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spool: Arc<TaskSpool> = Arc::new(TaskSpool::new());
        let exit_slot = new_exit_slot();
        let pg = process_group.clone();
        let slot = output.clone();
        let full = spool.clone();
        let command_for_detail = command.clone();
        let timeout = self.timeout;
        let running = self.registry.clone();
        let shell_name = shell.tool_name.to_owned();
        self.spawn_inner(
            label,
            None,
            Some(process_group),
            None, // display_meta
            owner_session,
            exit_slot.clone(),
            move |id| {
                let mut running = running.running.lock().unwrap();
                if let Some(task) = running.iter_mut().find(|task| task.id == id) {
                    task.output = Some(output);
                    task.spool = Some(spool);
                    task.full_command = Some(command_for_detail);
                    task.shell_name = Some(shell_name.clone());
                }
            },
            move || async move {
                match run_bash(
                    &shell,
                    &workspace,
                    &command,
                    timeout,
                    protect_git,
                    Some(pg),
                    Some(slot),
                    Some(full),
                    sandbox.as_ref(),
                    Some(exit_slot.clone()),
                )
                .await
                {
                    Ok(output) => output,
                    Err(output) => {
                        // Shell spawn/IO failures return BEFORE the exit
                        // slot is populated, which would leave the trace
                        // with start/duration/kind but no failed status.
                        // Record a truthful "failed" only when the slot is
                        // still empty — explicit killed metadata already
                        // written by the timeout/cancel paths is preserved.
                        mark_failed_if_empty(&mut exit_slot.lock().unwrap());
                        output
                    }
                }
            },
            on_complete,
        )
    }

    /// Register and spawn a background future. Completion is delivered as
    /// [`AgentEvent::BackgroundCompleted`]. `process_group` is only used for
    /// kill-on-drop cleanup; pass `None` for non-process tasks.
    pub fn spawn<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        self.spawn_with_id(
            label,
            role,
            process_group,
            None,
            new_exit_slot(),
            |_| {},
            work,
        )
    }

    /// Like [`Self::spawn`], but invokes `on_id` with the allocated task id
    /// before the work starts (so callers can register per-task state under
    /// the same id). `exit_slot` is the out-slot the work closure writes
    /// its structured exit metadata into (bash: `run_bash`'s out-slot;
    /// delegate: the subagent result's success flag); callers that produce
    /// no trace metadata pass a default slot.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_id<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        exit_slot: ExitSlot,
        on_id: impl FnOnce(u64),
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        self.spawn_with_id_to(
            self.sender.lock().unwrap().clone(),
            label,
            role,
            process_group,
            display_meta,
            exit_slot,
            on_id,
            work,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_id_to<F, Fut>(
        &self,
        sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        exit_slot: ExitSlot,
        on_id: impl FnOnce(u64),
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let sender = sender
            .filter(|sender| !sender.is_closed())
            .ok_or("background task delivery is unavailable")?;
        let completion_label = label.clone();
        self.spawn_inner(
            label,
            role,
            process_group,
            display_meta,
            // Delegate tasks are spawned by the main session's Delegate tool
            // (subagents never delegate); the listing session id is already
            // the initiator, so no separate owner.
            None,
            exit_slot,
            on_id,
            work,
            move |id, output, trace| {
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label: Some(completion_label.clone()),
                    started_at_ms: trace.started_at_ms,
                    duration_ms: trace.duration_ms,
                    exit_code: trace.exit_code,
                    signal: trace.signal,
                    status: trace.status,
                    kind: trace.kind,
                });
            },
        )
    }

    /// Spawn a registered task that runs to completion but does NOT send a
    /// completion event. Used by synchronous delegate: the subagent must be
    /// visible in the task panel, but its answer is
    /// returned as the tool result, so a completion notice would duplicate.
    pub fn spawn_silent<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        on_id: impl FnOnce(u64),
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        self.spawn_inner(
            label,
            role,
            process_group,
            display_meta,
            None,
            new_exit_slot(),
            on_id,
            work,
            |_, _, _| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_inner<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        owner_session: Option<String>,
        exit_slot: ExitSlot,
        on_id: impl FnOnce(u64),
        work: F,
        on_complete: impl FnOnce(u64, String, BackgroundTrace) + Send + 'static,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let id = self.registry.next_id.fetch_add(1, Ordering::Relaxed);
        let started = format!("started background task {id}: {label}");
        let started_at = std::time::Instant::now();
        let started_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis() as u64);
        let (work_tx, work_rx) = tokio::sync::oneshot::channel::<F>();
        // The task holds only a weak registry reference. Therefore dropping
        // the final BackgroundTasks owner tears down the registry and kills
        // its process groups, while dropping an ordinary shared clone does not.
        let running = Arc::downgrade(&self.registry);
        let completion: Completion = Arc::new(std::sync::Mutex::new(Some(Box::new(on_complete))));
        let task_completion = completion.clone();
        let handle = tokio::spawn(async move {
            let Ok(work) = work_rx.await else {
                if let Some(running) = running.upgrade() {
                    running.running.lock().unwrap().retain(|task| task.id != id);
                }
                return;
            };
            let output = work().await;
            // Remove the task from the registry and read its start timing +
            // the exit metadata its own work recorded (the out-slot shared
            // with the caller's work closure), then deliver the completion
            // with the merged trace. `completed` is false when the task was
            // cancelled (removed from the registry) while the work was
            // still in flight — the cancel path owns the completion then.
            let trace = if let Some(running) = running.upgrade() {
                let mut running = running.running.lock().unwrap();
                if let Some(index) = running.iter().position(|task| task.id == id) {
                    let task = running.remove(index);
                    Some(task.completion_trace())
                } else {
                    None
                }
            } else {
                None
            };
            if let (Some(trace), Some(done)) = (trace, task_completion.lock().unwrap().take()) {
                done(id, output, trace);
            }
        });
        self.registry.running.lock().unwrap().push(RunningTask {
            id,
            label,
            full_command: None,
            role,
            process_group: process_group.unwrap_or_else(|| Arc::new(AtomicI32::new(0))),
            handle: Arc::new(handle),
            shell_name: None,
            output: None,
            spool: None,
            display_meta,
            owner_session,
            completion,
            started_at_ms,
            started_at,
            exit: exit_slot,
        });
        on_id(id);
        if let Err(work) = work_tx.send(work) {
            drop(work);
        }
        Ok(started)
    }
}

impl Drop for BackgroundRegistry {
    fn drop(&mut self) {
        #[cfg(unix)]
        for task in self.running.lock().unwrap().iter() {
            if let Some(process_group) =
                rustix::process::Pid::from_raw(task.process_group.load(Ordering::Acquire))
            {
                let _ = rustix::process::kill_process_group(
                    process_group,
                    rustix::process::Signal::KILL,
                );
            }
        }
        #[cfg(windows)]
        for task in self.running.lock().unwrap().iter() {
            // Degraded kill: terminate the top-level process only. The
            // process tree is not enumerated (no Job Object yet — that is a
            // later milestone), so grandchildren may survive.
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_TERMINATE, TerminateProcess,
            };
            let pid = task.process_group.load(Ordering::Acquire) as u32;
            if pid != 0 {
                unsafe {
                    let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                    if !handle.is_null() {
                        let _ = TerminateProcess(handle, 1);
                        CloseHandle(handle);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `start`（主会话便捷入口）owner 为 None；`start_with_sender` 显式传
    /// `owner_session`（subagent 的 bash）时 `running()` 原样透出——任务面板
    /// 据此显示真正的发起者，而不是 registry 所属会话。
    #[tokio::test]
    async fn start_owner_session_shows_in_running() {
        let mut background = BackgroundTasks::new(None, None);
        let (sender, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        background.set_event_sender(sender);
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        background
            .start(workspace.clone(), "sleep 30".to_string(), false)
            .expect("main-session background bash task starts");
        background
            .start_with_sender(
                workspace,
                "sleep 30".to_string(),
                false,
                background.sender.lock().unwrap().clone(),
                None,
                Some("sub-abc".into()),
            )
            .expect("subagent background bash task starts");
        let running = background.running();
        assert_eq!(running.len(), 2);
        assert_eq!(
            running[0].owner_session, None,
            "main-session task has no owner"
        );
        assert_eq!(
            running[1].owner_session.as_deref(),
            Some("sub-abc"),
            "subagent task carries its own session id"
        );
        for task in &running {
            background.cancel(task.id);
        }
        assert!(background.running().is_empty());
    }
}
