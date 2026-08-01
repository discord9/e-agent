use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use crate::agent::{AgentEvent, preview};

use super::bash::{Shell, run_bash};

#[derive(Clone)]
pub struct BackgroundTasks {
    registry: Arc<BackgroundRegistry>,
    /// Compatibility sender for callers which schedule directly on a registry
    /// (delegates do this). Bash deliberately does not use this field.
    sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    /// Background bash timeout; `None` = no timeout (run forever).
    timeout: Option<Duration>,
    sandbox: Option<crate::config::Sandbox>,
}

struct BackgroundRegistry {
    next_id: AtomicU64,
    running: std::sync::Mutex<Vec<RunningTask>>,
}

type Completion = Arc<std::sync::Mutex<Option<Box<dyn FnOnce(u64, String) + Send>>>>;

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
    output: Option<OutputSlot>,
    spool: Option<Arc<TaskSpool>>,
    display_meta: Option<TaskDisplayMeta>,
    completion: Completion,
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
}

impl BackgroundTasks {
    pub(super) fn new(timeout: Option<Duration>, sandbox: Option<crate::config::Sandbox>) -> Self {
        Self {
            registry: Arc::new(BackgroundRegistry {
                next_id: AtomicU64::new(1),
                running: std::sync::Mutex::new(Vec::new()),
            }),
            sender: None,
            timeout,
            sandbox,
        }
    }

    /// Set the channel used to deliver background completions. Called by the
    /// agent for tools that hold a shared clone of this registry.
    pub fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.sender = Some(sender);
    }

    /// Whether background completion delivery is currently usable.
    ///
    /// This is a read-only preflight; spawning still performs its own final
    /// sender check to guard against the receiver closing in the meantime.
    pub fn completion_delivery_available(&self) -> bool {
        self.sender
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
                kind: if task.output.is_some() {
                    "bash".into()
                } else {
                    "delegate".into()
                },
                output: task
                    .output
                    .as_ref()
                    .map(|slot| slot.lock().unwrap().clone())
                    .unwrap_or_default(),
                display_meta: task.display_meta.clone(),
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
        let task = {
            let mut running = self.registry.running.lock().unwrap();
            let index = running.iter().position(|task| task.id == id)?;
            running.remove(index)
        };
        task.handle.abort();
        if let Some(done) = task.completion.lock().unwrap().take() {
            done(id, "background task cancelled".into());
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
            self.sender.clone(),
            self.sandbox.clone(),
        )
    }

    /// Start a background bash command with an explicit completion sender and
    /// sandbox policy.
    ///
    /// `sandbox` is the POLICY OF THE CALLING BASH FACADE, not the shared
    /// registry's: a subagent's Bash tool passes its own (possibly read-only
    /// narrowed) sandbox so background commands cannot escape into the
    /// parent's wider policy. `start()` passes the registry's default.
    pub fn start_with_sender(
        &self,
        workspace: Workspace,
        command: String,
        protect_git: bool,
        sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
        sandbox: Option<crate::config::Sandbox>,
    ) -> Result<String, String> {
        let shell = Shell::detect()?;
        let process_group = Arc::new(AtomicI32::new(0));
        let output: OutputSlot = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spool: Arc<TaskSpool> = Arc::new(TaskSpool::new());
        let pg = process_group.clone();
        let slot = output.clone();
        let full = spool.clone();
        let command_for_detail = command.clone();
        let timeout = self.timeout;
        let running = self.registry.clone();
        self.spawn_with_id_to(
            sender,
            preview(&command, 100),
            None,
            Some(process_group),
            None, // display_meta
            move |id| {
                let mut running = running.running.lock().unwrap();
                if let Some(task) = running.iter_mut().find(|task| task.id == id) {
                    task.output = Some(output);
                    task.spool = Some(spool);
                    task.full_command = Some(command_for_detail);
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
                )
                .await
                {
                    Ok(output) | Err(output) => output,
                }
            },
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
        self.spawn_with_id(label, role, process_group, None, |_| {}, work)
    }

    /// Like [`Self::spawn`], but invokes `on_id` with the allocated task id
    /// before the work starts (so callers can register per-task state under
    /// the same id).
    pub fn spawn_with_id<F, Fut>(
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
        self.spawn_with_id_to(
            self.sender.clone(),
            label,
            role,
            process_group,
            display_meta,
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
            on_id,
            work,
            move |id, output| {
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label: Some(completion_label.clone()),
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
            on_id,
            work,
            |_, _| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_inner<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        on_id: impl FnOnce(u64),
        work: F,
        on_complete: impl FnOnce(u64, String) + Send + 'static,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let id = self.registry.next_id.fetch_add(1, Ordering::Relaxed);
        let started = format!("started background task {id}: {label}");
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
            let completed = if let Some(running) = running.upgrade() {
                let mut running = running.running.lock().unwrap();
                if let Some(index) = running.iter().position(|task| task.id == id) {
                    running.remove(index);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if completed && let Some(done) = task_completion.lock().unwrap().take() {
                done(id, output);
            }
        });
        self.registry.running.lock().unwrap().push(RunningTask {
            id,
            label,
            full_command: None,
            role,
            process_group: process_group.unwrap_or_else(|| Arc::new(AtomicI32::new(0))),
            handle: Arc::new(handle),
            output: None,
            spool: None,
            display_meta,
            completion,
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
