use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

use crate::agent::{AgentEvent, Message, SessionEntry, preview};
use crate::runner::{SessionHandle as RunnerHandle, SessionResult, SessionStatus};

use super::*;

/// Horizontal rule marking compaction boundaries in the scrollback. Rendered
/// with LineKind::Compaction (violet on the element surface) so it stands
/// out from regular log lines.
/// Truncate background completion output for TUI display. Shows head and
/// tail with a middle marker indicating how many lines and chars were
/// omitted. The full output is always preserved in the session entry /
/// model context / persisted JSONL / Greptime payload.
///
/// Visual budget:
///   - At most 5 head lines + 1 marker line + 3 tail lines = 9 lines total.
///   - Each retained line is middle-elided with `preview(120)` (Unicode-safe,
///     CJK-safe) so a single long line never wraps across the terminal.
///   - Short output (≤ 8 logical lines) whose every line also fits 120 chars
///     is returned unchanged.
///
/// The marker reports both dropped lines and total omitted chars (sum of chars
/// from dropped lines plus chars trimmed from retained lines via middle-elision).
/// "Chars omitted" = len(original) - len(visible result including the marker).
pub(crate) fn truncate_background_output(output: &str) -> String {
    const HEAD_LINES: usize = 5;
    const TAIL_LINES: usize = 3;
    const MAX_VISUAL_LINES: usize = HEAD_LINES + TAIL_LINES; // 8 (before truncation)
    const MAX_LINE_CHARS: usize = 120;

    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();

    // Elide a single line to MAX_LINE_CHARS via middle ellipsis.
    let elide = |line: &str| -> String {
        if line.chars().count() > MAX_LINE_CHARS {
            preview(line, MAX_LINE_CHARS)
        } else {
            line.to_owned()
        }
    };

    // Case 1: short output, no line too long → return unchanged.
    let any_long = lines.iter().any(|l| l.chars().count() > MAX_LINE_CHARS);
    if total_lines <= MAX_VISUAL_LINES && !any_long {
        return output.to_owned();
    }

    let total_orig_chars: usize = output.chars().count();

    // Case 2: short output (≤ 8 lines) but some lines exceed 120 chars.
    // Keep every line with middle elision on the long ones, then append
    // a chars-omitted note.
    if total_lines <= MAX_VISUAL_LINES {
        let elided: Vec<String> = lines.iter().map(|l| elide(l)).collect();
        let body = elided.join("\n");
        let visible = body.chars().count();
        let omitted = total_orig_chars.saturating_sub(visible);
        if omitted > 0 {
            return format!("{body}\n\u{2026} ({omitted} chars omitted)");
        }
        return body;
    }

    // Case 3: many lines (> 8).
    let head = &lines[..HEAD_LINES];
    let tail = &lines[total_lines - TAIL_LINES..];
    let omitted_lines = total_lines - HEAD_LINES - TAIL_LINES;

    let elided_head: Vec<String> = head.iter().map(|l| elide(l)).collect();
    let elided_tail: Vec<String> = tail.iter().map(|l| elide(l)).collect();
    let head_chars: usize = elided_head.iter().map(|s| s.chars().count()).sum();
    let tail_chars: usize = elided_tail.iter().map(|s| s.chars().count()).sum();

    // Build marker.  The marker text itself is part of the visible result,
    // so we need an estimated length to compute the chars-omitted value,
    // then refine once. Blank lines around the marker make the elision
    // obvious instead of blending into the head/tail output.
    let est_marker_len = 64usize;
    let rough_omitted = total_orig_chars.saturating_sub(head_chars + tail_chars + est_marker_len);
    let marker =
        format!("\n\n\u{2026} ({omitted_lines} lines omitted, {rough_omitted} chars omitted)\n");
    // Refine with actual marker length.
    let actual_omitted =
        total_orig_chars.saturating_sub(head_chars + tail_chars + marker.chars().count());
    let marker =
        format!("\n\n\u{2026} ({omitted_lines} lines omitted, {actual_omitted} chars omitted)\n");

    let mut result = elided_head.join("\n");
    result.push_str(&marker);
    for line in &elided_tail {
        result.push('\n');
        result.push_str(line);
    }
    result
}

#[derive(Default)]
pub(crate) struct TuiState {
    /// This session's id, shown in the input border (so it can be resumed
    /// later with --session).
    pub(crate) session_id: String,
    pub(crate) model_name: String,
    /// Agent role name displayed next to the model name; `None` when no role
    /// template exists.
    pub(crate) role_name: Option<String>,
    pub(crate) cwd: String,
    /// Workspace root for store-backed history loads: older compaction
    /// segments are read on demand when the user scrolls to the top.
    /// `cwd` is the display string; this is the real `&Path` the store
    /// needs.
    pub(crate) root: PathBuf,
    pub(crate) input: InputBuffer,
    pub(crate) lines: Vec<DisplayLine>,
    /// Bounded local rendering window.
    pub(crate) window: ScrollWindow,
    /// Terminal width (in cells) for the output area, updated on every draw.
    pub(crate) inner_width: usize,
    /// Terminal height (in rows) for the output area, updated on every draw.
    pub(crate) output_height: usize,
    pub(crate) busy: Option<BusyState>,
    pub(crate) streamed: bool,
    pub(crate) active_lane: Option<ActiveStreamLane>,
    pub(crate) tokens_context: u64,
    /// Configured context window (token count) from the model profile, used
    /// to display a usage percentage and trigger the red style at >= 80%.
    pub(crate) context_window: Option<u64>,
    /// Transient projection of prompts accepted while a turn is in flight.
    /// Consumption removes one item from the front; this is never persisted.
    pub(crate) queued: std::collections::VecDeque<String>,
    /// Arguments of an in-flight edit_file call, rendered as a numbered diff
    /// when its result (which carries the line number) arrives.
    pub(crate) pending_edit: Option<(String, String, String)>,
    /// Shared running-task registry, for the tasks panel.
    pub(crate) background: Option<crate::tools::BackgroundTasks>,
    /// Session store for background-task record bookkeeping when cancelling
    /// from the tasks panel; `None` (tests, `Default`) falls back to the
    /// JSONL record file directly.
    pub(crate) store: Option<crate::session_store::SessionStore>,
    /// The main session's model; btw fork subagents (`/btw`) inherit it
    /// (`delegate::BtwContext::model`).
    pub(crate) model: Option<crate::model::ConfiguredModel>,
    /// The workspace the main session works in; btw fork subagents inherit
    /// it.
    pub(crate) workspace: Option<crate::workspace::Workspace>,
    /// bwrap policy inherited by btw fork subagents (`None` = sandbox
    /// disabled, wired by `run_inner` together with the other components).
    pub(crate) sandbox: Option<crate::config::Sandbox>,
    /// Session backend configuration for btw fork subagent persistence.
    pub(crate) backend: Option<crate::config::SessionBackend>,
    /// Read-only policy inherited by btw fork subagents (a read-only parent
    /// forks read-only subagents).
    pub(crate) read_only: bool,
    /// Parent session's background record (root + session id + store):
    /// records the btw task for the killed-on-exit notice and supplies the
    /// `parent_session_id` metadata link.
    pub(crate) record_in: Option<crate::session_store::BackgroundRecord>,
    /// Live subagent registry btw fork subagents register into (F2 task
    /// panel attach).
    pub(crate) sessions: Option<crate::delegate::Sessions>,
    /// Older-history paging state, driven by `handle_scroll` + the run loop:
    ///
    /// - `older_pending`: an Up/PageUp at the scrollback top (or Home,
    ///   which also sets `older_is_jump`) asked for older history; the run
    ///   loop performs the async load.
    /// - `older_loading`: a load is in flight (re-entrancy guard).
    /// - `older_done`: the store reported no more history (or JSONL, whose
    ///   full session was already loaded at startup).
    /// - `older_cursor`: next `before_seq` for `load_older`; `None` until
    ///   the first load seeds it from `head_seq`.
    /// - `older_is_jump`: Home queues the *oldest* segment in one load
    ///   (`load_oldest`) instead of the stepwise PageUp path (`load_older`
    ///   per segment); `load_older_history` consumes the flag once.
    pub(crate) older_pending: bool,
    pub(crate) older_loading: bool,
    pub(crate) older_done: bool,
    pub(crate) older_cursor: Option<i64>,
    pub(crate) older_is_jump: bool,
    /// Newer-history paging state — the mirror of the older_* fields,
    /// driven by reaching the end of the loaded lines while scrolling down
    /// (see `extend_window_down`):
    ///
    /// - `newer_pending`: the run loop should load the next middle segment.
    /// - `newer_loading`: a load is in flight (re-entrancy guard).
    /// - `newer_done`: the store reported no more middle segments (the
    ///   head segment was reached; it is already loaded).
    /// - `newer_cursor`: next `after_seq` for `load_newer`; `None` until
    ///   `load_oldest` seeds it (or the session has no compaction at all).
    pub(crate) newer_pending: bool,
    pub(crate) newer_loading: bool,
    pub(crate) newer_done: bool,
    pub(crate) newer_cursor: Option<i64>,
    /// Index of the head segment's first line in `lines`. The head
    /// segment (loaded at startup) always occupies `lines[head_start..]`;
    /// older segments are prepended in front of it and middle segments
    /// are spliced in just before it, each insertion shifting `head_start`
    /// forward by the inserted line count. Initial 0: startup replay of
    /// the head segment begins at `lines[0]`.
    pub(crate) head_start: usize,
    /// Probe for whether a background task has an attachable session
    /// (wired to the session registry by the runner).
    pub(crate) attachable: Option<Box<dyn Fn(u64) -> bool + Send>>,
    /// Whether the background-tasks panel is visible.
    pub(crate) show_tasks: bool,
    /// Cursor (index into the running-task list) for attach selection.
    pub(crate) task_cursor: usize,
    /// Attached session view; when set, draw renders this instead of the
    /// main scrollback and Esc detaches instead of cancelling the turn.
    pub(crate) attached: Option<Box<AttachedView>>,
    /// Full-output detail view for a selected background bash task; when
    /// set, draw renders it full-screen and keys route to it first.
    pub(crate) task_detail: Option<TaskDetail>,
    /// Panel visibility at the moment the detail view opened; Esc restores
    /// it (return to the tasks panel if it was open, else to the main
    /// view). F2 closes both, so it ignores this.
    pub(crate) detail_return_panel: bool,
    /// Whether the terminal title has been set to a user-derived value.
    /// Once true, subsequent prompts never overwrite the title.
    pub(crate) session_title_set: bool,
    /// Steering input preserved per background-task id across detach /
    /// re-attach and panel-driven task switches, so browsing the tasks
    /// panel never discards a half-typed prompt.
    pub(crate) stashed_input: std::collections::HashMap<u64, String>,
}

/// A live view into a running background session: its own scrollback
/// rebuilt from the event stream, rendered with the same pipeline as the
/// main view. While attached you can steer the session through its handle:
/// Enter queues a prompt for its next turn, Ctrl-C cancels its in-flight
/// turn.
pub(crate) struct AttachedView {
    pub(crate) id: u64,
    pub(crate) label: String,
    pub(crate) state: TuiState,
    /// The live-event bridge for this attachment. Dropping the view aborts it
    /// so re-attaching cannot forward each delta twice.
    pub(crate) bridge: Option<tokio::task::AbortHandle>,
    /// The session seam: snapshot/subscribe/send_input/cancel.
    pub(crate) handle: RunnerHandle,
    /// Input buffer for steering prompts.
    pub(crate) input: InputBuffer,
    /// Vertical scroll of the steering input (multi-line).
    pub(crate) input_scroll: usize,
    /// Live status of the attached session, kept in sync with the runner:
    /// drives the title text and the finished flag so the view reflects the
    /// runner's real state (Busy/Compacting/Idle/Finished) instead of
    /// lagging behind until the parent's BackgroundCompleted arrives.
    pub(crate) status: tokio::sync::watch::Receiver<SessionStatus>,
    /// Set once the session's completion event has arrived (view becomes a
    /// static record; further events are impossible).
    pub(crate) finished: bool,
}

impl Drop for AttachedView {
    fn drop(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            bridge.abort();
        }
    }
}

/// Full-output viewer for a background bash task. The page holds one
/// viewport of spool lines (plain Dim text, no markdown); `window` is the
/// existing [`ScrollWindow`] machinery reused for page-internal scrolling.
/// Scrolling past a page boundary loads the adjacent page from the spool.
pub(crate) struct TaskDetail {
    pub(crate) id: u64,
    pub(crate) label: String,
    /// Full untruncated command for bash tasks (`None` for delegate/open
    /// without a snapshot). Rendered as a fixed banner under the header so
    /// the complete command is visible even though the panel label and the
    /// header stay preview-truncated.
    pub(crate) command: Option<String>,
    /// Keeps the full output alive after the task completes (the registry
    /// drops its reference when the task leaves `running()`).
    pub(crate) spool: Arc<crate::tools::TaskSpool>,
    pub(crate) finished: bool,
    /// 0-based spool line number of `lines[0]`.
    pub(crate) base_line: usize,
    /// Current page of spool lines.
    pub(crate) lines: Vec<DisplayLine>,
    /// Page-internal window (source_* index into `lines`).
    pub(crate) window: ScrollWindow,
    /// Spool line count observed at the last reload (open or tail slide).
    /// While following, draw reloads the tail only when the spool has
    /// grown past this — opening shows the head page, and new output
    /// slides the view to the live tail (like `tail -f` from the top).
    pub(crate) last_seen_lines: usize,
    /// Terminal width the page was last rendered at, used to detect a
    /// resize so a scrolled-up (non-follow) page can be re-anchored at
    /// the new width. `0` means "no frame rendered yet".
    pub(crate) rendered_width: usize,
    /// Timestamp of the last draw-driven tail reload. Follow-mode draw
    /// reloads are throttled to [`TAIL_RELOAD_INTERVAL`] so a task that
    /// appends output every frame does not pay for a full spool re-read
    /// and re-wrap on every single frame; the cached page keeps rendering
    /// in between. Explicit user actions (End, scroll-to-tail) call
    /// `load_tail` directly and bypass the throttle.
    pub(crate) last_tail_reload: Option<std::time::Instant>,
}

impl TaskDetail {
    pub(crate) fn new(
        id: u64,
        label: String,
        spool: Arc<crate::tools::TaskSpool>,
        finished: bool,
    ) -> Self {
        Self {
            id,
            label,
            command: None,
            spool,
            finished,
            base_line: 0,
            lines: Vec::new(),
            window: ScrollWindow::new(),
            last_seen_lines: 0,
            rendered_width: 0,
            last_tail_reload: None,
        }
    }

    /// Fetch spool lines `[base, base+max_lines)` into the page.
    /// `anchor_bottom` positions the viewport at the page's last rows
    /// (used when paging up into the previous page). Does not touch
    /// `follow_bottom`; callers set it (load_tail enables follow).
    pub(crate) fn load_page(
        &mut self,
        base: usize,
        max_lines: usize,
        anchor_bottom: bool,
        width: usize,
        height: usize,
    ) {
        let window = self.spool.window(base, max_lines, MAX_RENDER_BYTES);
        let Some(window) = window else {
            // Empty spool (or base past the end): empty page.
            self.base_line = base;
            self.lines.clear();
            self.window.source_start = 0;
            self.window.source_end = 0;
            self.window.local_offset = 0;
            return;
        };
        self.base_line = window.first_line;
        self.lines = window
            .lines
            .into_iter()
            .map(|text| DisplayLine {
                text,
                kind: LineKind::Dim,
            })
            .collect();
        self.window.source_start = 0;
        self.window.source_end = self.lines.len();
        self.window.local_offset = 0;
        if anchor_bottom {
            let total =
                render_bounded_window(&local_window_lines(&self.lines, &self.window), width, false)
                    .len();
            self.window.local_offset = total.saturating_sub(height);
        }
    }

    /// Anchor the page at the spool tail and enable follow (draw reloads
    /// the tail page every frame while `follow_bottom` stays true).
    pub(crate) fn load_tail(&mut self, width: usize, height: usize) {
        let capacity = height.max(1);
        let total = self.spool.line_count();
        self.load_page(
            total.saturating_sub(capacity),
            capacity,
            false,
            width,
            height,
        );
        self.window.follow_bottom = true;
        self.window.frozen_tail_cursor = None;
        self.window.frozen_source_end = 0;
    }

    /// Anchor the page at the spool head and enable follow. Draw reloads
    /// the tail only once the spool has grown past `last_seen_lines`, so
    /// opening a large task shows the first page and then slides to the
    /// live tail as output streams in.
    pub(crate) fn load_head(&mut self, width: usize, height: usize) {
        let capacity = height.max(1);
        self.load_page(0, capacity, false, width, height);
        self.window.follow_bottom = true;
        self.window.frozen_tail_cursor = None;
        self.window.frozen_source_end = 0;
    }

    pub(crate) fn step_up(&mut self, width: usize, height: usize) {
        self.window.follow_bottom = false;
        if self.window.local_offset > 0 {
            self.window.local_offset -= 1;
        } else if self.base_line > 0 {
            let step = self.lines.len().max(1);
            self.load_page(
                self.base_line.saturating_sub(step),
                step,
                true,
                width,
                height,
            );
        }
    }

    pub(crate) fn step_down(&mut self, width: usize, height: usize) {
        if self.window.follow_bottom {
            return;
        }
        let total =
            render_bounded_window(&local_window_lines(&self.lines, &self.window), width, false)
                .len();
        if self.window.local_offset.saturating_add(height) < total {
            self.window.local_offset += 1;
        } else if self.base_line + self.lines.len() < self.spool.line_count() {
            let step = self.lines.len().max(1);
            self.load_page(self.base_line + step, step, false, width, height);
        } else {
            // Spool tail reached: resume follow (draw re-fetches the tail).
            self.window.follow_bottom = true;
        }
    }

    pub(crate) fn page_up(&mut self, width: usize, height: usize) {
        self.window.follow_bottom = false;
        if self.window.local_offset >= height {
            self.window.local_offset -= height;
        } else if self.base_line > 0 {
            let into_prev = self.window.local_offset;
            let step = self.lines.len().max(1);
            self.load_page(
                self.base_line.saturating_sub(step),
                step,
                true,
                width,
                height,
            );
            self.window.local_offset = self.window.local_offset.saturating_add(into_prev);
        } else {
            self.window.local_offset = 0;
        }
    }

    pub(crate) fn page_down(&mut self, width: usize, height: usize) {
        if self.window.follow_bottom {
            return;
        }
        let total =
            render_bounded_window(&local_window_lines(&self.lines, &self.window), width, false)
                .len();
        if self.window.local_offset.saturating_add(height) < total {
            self.window.local_offset += height;
        } else if self.base_line + self.lines.len() < self.spool.line_count() {
            let step = self.lines.len().max(1);
            self.load_page(self.base_line + step, step, false, width, height);
        } else {
            self.window.follow_bottom = true;
        }
    }
}

impl AttachedView {
    /// Status text for the input-border title. Prefers the live runner
    /// status; falls back to the view's own busy/finished flags when the
    /// receiver is stale (attach raced ahead of the runner, or the test
    /// channel never moves status).
    pub(crate) fn title_status(&self) -> String {
        match &*self.status.borrow() {
            SessionStatus::Busy | SessionStatus::Compacting => self
                .state
                .busy
                .map_or_else(|| BusyState::thinking().title(), BusyState::title),
            SessionStatus::Idle => {
                if self.finished {
                    "finished".into()
                } else if let Some(busy) = &self.state.busy {
                    (*busy).title()
                } else {
                    "idle".into()
                }
            }
            SessionStatus::Finished(result) => match result {
                SessionResult::Completed(_) => "finished".into(),
                SessionResult::Failed(_) => "failed".into(),
                SessionResult::Cancelled => "cancelled".into(),
                SessionResult::Closed => "closed".into(),
            },
        }
    }

    /// Input editing on the attached steering buffer. Vertical arrow keys
    /// stay scroll keys (see handle_attached_key), so a multi-line buffer
    /// edits at the cursor without vertical movement.
    pub(crate) fn edit_input(input: &mut InputBuffer, key: KeyEvent, _width: usize) {
        match key.code {
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                input.insert_char(character)
            }
            KeyCode::Left => input.left(),
            KeyCode::Right => input.right(),
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => input.home(),
            KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => input.end(),
            KeyCode::Backspace => input.backspace(),
            KeyCode::Delete => input.delete(),
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum BusyKind {
    Thinking,
    Compacting,
}

/// In-flight work shown in the input border. The spinner frame advances on
/// every routed session event (deltas, tool calls, …), so it spins while
/// the model streams and freezes when the provider stalls — a stuck frame
/// is itself the signal. Each view (main / attached) owns its TuiState, so
/// sessions spin independently.
#[derive(Clone, Copy)]
pub(crate) struct BusyState {
    pub(crate) kind: BusyKind,
    pub(crate) frame: usize,
}

impl BusyState {
    pub(crate) const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    pub(crate) fn thinking() -> Self {
        Self {
            kind: BusyKind::Thinking,
            frame: 0,
        }
    }

    pub(crate) fn compacting() -> Self {
        Self {
            kind: BusyKind::Compacting,
            frame: 0,
        }
    }

    pub(crate) fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub(crate) fn title(self) -> String {
        let label = match self.kind {
            BusyKind::Thinking => "thinking…",
            BusyKind::Compacting => "compaction…",
        };
        format!(
            "{} {label}",
            Self::SPINNER[self.frame % Self::SPINNER.len()]
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ActiveStreamLane {
    Reasoning,
    Content,
}

/// Outcome of processing a key press while the tasks panel is open.
/// Shared by the idle and drive event loops so the routing logic is
/// tested once instead of duplicated.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PanelAction {
    /// Esc or F2: close the panel.
    ClosePanel,
    /// `x` or Ctrl-C: cancel the selected task.
    CancelTask,
    /// Key was not a panel action; caller should fall through.
    Passthrough,
}

/// Outcome of a selection key while the tasks panel is open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskSelection {
    /// Attach to the selected task's session (delegate tasks, and cursor
    /// moves to an attachable task).
    Attach(u64),
    /// Open the full-output detail view (bash task selected with Enter or
    /// a cursor move).
    OpenDetail(u64),
    /// Key was not a selection; caller should fall through.
    None,
}

impl TuiState {
    pub(crate) fn push_background_completion(
        &mut self,
        id: u64,
        output: &str,
        label: Option<&str>,
    ) {
        self.push_line(background_completion_header(id, label), LineKind::Dim);
        let truncated = truncate_background_output(output);
        for line in truncated.lines() {
            self.push_line(line.to_owned(), LineKind::Dim);
        }
    }

    pub(crate) fn take_input(&mut self) -> Option<String> {
        let prompt = std::mem::take(&mut self.input.text);
        self.input.cursor = 0;
        (!prompt.trim().is_empty()).then_some(prompt)
    }

    /// Atomically drain ALL queued prompts, join them with `\n\n`, and
    /// return the combined batch (or None when the queue is empty).
    /// Each call leaves the queue empty, so the caller should loop over
    /// this helper only until it returns None — any prompts enqueued
    /// during the follow-up turn become a separate batch on the next call.
    /// Attach to a background session: snapshot its event log into a fresh
    /// scrollback and follow its live stream from now on (see `bridge`).
    /// `status` is the live runner-status receiver from `RunnerHandle::attach`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach(
        &mut self,
        id: u64,
        label: String,
        handle: RunnerHandle,
        model_name: String,
        role_name: Option<String>,
        cwd: String,
        session_id: String,
        context_window: Option<u64>,
        snapshot: Vec<AgentEvent>,
        status: tokio::sync::watch::Receiver<SessionStatus>,
    ) {
        // Switching straight from one attached task to another (F2 panel →
        // attach) replaces the view without a detach; keep its input too.
        self.stash_attached_input();
        let mut state = TuiState {
            model_name,
            role_name,
            cwd,
            session_id,
            context_window,
            ..TuiState::default()
        };
        let mut finished = false;
        for event in snapshot {
            // The completion may have raced into the log before the attach;
            // the view must still flip to finished.
            if matches!(event, AgentEvent::BackgroundCompleted { .. }) {
                finished = true;
            }
            state.push_agent_event(event);
        }
        state.follow();
        if !finished {
            state.busy = Some(BusyState::thinking());
        }
        let mut input = InputBuffer::default();
        if let Some(stashed) = self.stashed_input.remove(&id) {
            input.insert(&stashed);
        }
        self.attached = Some(Box::new(AttachedView {
            id,
            label,
            state,
            bridge: None,
            handle,
            input,
            input_scroll: 0,
            status,
            finished,
        }));
    }

    /// Preserve the attached steering input keyed by task id so a detach /
    /// re-attach or a panel-driven switch to another task cannot lose a
    /// half-typed prompt. A previously stashed value for the same id is
    /// replaced (an already-sent prompt must not resurrect itself).
    pub(crate) fn stash_attached_input(&mut self) {
        if let Some(attached) = &mut self.attached {
            self.stashed_input.remove(&attached.id);
            if !attached.input.text.is_empty() {
                self.stashed_input
                    .insert(attached.id, attached.input.text.clone());
            }
        }
    }

    pub(crate) fn detach(&mut self) {
        self.stash_attached_input();
        self.attached = None;
    }

    /// Keys pressed while attached: F2 toggles the tasks panel (so another
    /// session can be selected), scroll keys move the attached scrollback,
    /// Enter steers the session (queues a prompt for its next turn), Ctrl-C
    /// cancels its in-flight turn, everything else edits the steering input.
    pub(crate) fn handle_attached_key(&mut self, key: KeyEvent, input_width: usize) {
        if key.code == KeyCode::F(2) {
            self.show_tasks = !self.show_tasks;
            self.task_cursor = self.cursor_at_attached();
            return;
        }
        let Some(attached) = &mut self.attached else {
            return;
        };
        match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => attached.state.handle_scroll(key),
            KeyCode::Enter if key.modifiers == KeyModifiers::ALT => {
                attached.input.insert_char('\n');
            }
            KeyCode::Enter => {
                // Finished sessions must not silently swallow the input:
                // keep the buffer intact, send nothing, and say why.
                if attached.finished
                    || matches!(&*attached.status.borrow(), SessionStatus::Finished(_))
                {
                    if !attached.input.text.trim().is_empty() {
                        attached.state.push_line(
                            "subagent finished — prompt not sent, press Esc to detach".into(),
                            LineKind::Dim,
                        );
                    }
                } else {
                    let prompt = std::mem::take(&mut attached.input.text);
                    attached.input.cursor = 0;
                    if !prompt.trim().is_empty() {
                        attached.handle.prompt(prompt);
                    }
                }
            }
            _ if is_cancel(key) && !attached.finished => {
                // Non-empty input: Ctrl-C clears the line so the in-flight
                // turn keeps running; only an empty buffer cancels it.
                if attached.input.text.trim().is_empty() {
                    attached.handle.cancel();
                } else {
                    attached.input.text.clear();
                    attached.input.cursor = 0;
                }
            }
            _ => AttachedView::edit_input(&mut attached.input, key, input_width),
        }
    }

    /// Mark an attached view finished and surface queued-but-unanswered
    /// prompts. Prompts the user queued while the turn was in flight may
    /// have been accepted by the runner and persisted, yet never answered
    /// because the session ended before its next turn; the view must make
    /// that distinguishable from a still-pending prompt.
    pub(crate) fn mark_attached_finished(attached: &mut AttachedView) {
        attached.finished = true;
        attached.state.busy = None;
        let queued = attached.state.queued.len();
        if queued > 0 {
            attached.state.push_line(
                format!("{queued} queued prompt(s) not answered (session finished)"),
                LineKind::Dim,
            );
        }
    }

    /// Route an event to the main scrollback or the attached view. The one
    /// event that belongs to both is `BackgroundCompleted` of the attached
    /// session: the main agent sees it as a completion, the attached view
    /// uses it to mark itself finished.
    pub(crate) fn push_event(&mut self, ui: UiEvent) {
        match ui.session {
            0 => {
                // Main agent. Its BackgroundCompleted events also mark an
                // attached view of that session as finished.
                if let AgentEvent::BackgroundCompleted { id, .. } = &ui.event
                    && let Some(attached) = &mut self.attached
                    && attached.id == *id
                {
                    Self::mark_attached_finished(attached);
                    attached.state.push_agent_event(ui.event.clone());
                }
                self.push_agent_event(ui.event);
            }
            session => {
                // Attached background session: route to its view. If we are
                // not currently attached to it the event is dropped (the
                // session log still holds it; re-attach re-snapshots).
                if let Some(attached) = &mut self.attached
                    && attached.id == session
                {
                    if matches!(ui.event, AgentEvent::BackgroundCompleted { .. }) {
                        Self::mark_attached_finished(attached);
                    }
                    attached.state.push_agent_event(ui.event);
                }
            }
        }
    }

    /// Every routed session event counts as a heartbeat: the receiving
    /// view's spinner (and the main view's, since BackgroundCompleted of an
    /// attached session lands in both) advances one frame.
    pub(crate) fn push_agent_event(&mut self, event: AgentEvent) {
        if let Some(busy) = &mut self.busy {
            busy.advance();
        }
        self.push_agent_event_inner(event);
    }

    /// Paste into whichever input is currently active, normalizing terminal
    /// line endings exactly as ordinary main-input paste did.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(attached) = &mut self.attached {
            attached.input.insert(&text);
        } else {
            self.input.insert(&text);
        }
    }

    pub(crate) fn edit_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.insert_char(character)
            }
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => self.input.home(),
            KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => self.input.end(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            _ => {}
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        match key.code {
            KeyCode::F(2) => {
                self.show_tasks = !self.show_tasks;
                self.task_cursor = self.cursor_at_attached();
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => self.handle_scroll(key),
            KeyCode::Enter => {
                // Alt+Enter inserts a newline (Shift+Enter is not
                // distinguishable in most terminals).
                if key.modifiers == KeyModifiers::ALT {
                    self.input.insert_char('\n');
                } else {
                    return self.take_input();
                }
            }
            _ => self.edit_input(key),
        }
        None
    }

    /// Keys pressed while the tasks panel is open return here first:
    /// The task-list index of the currently attached session, or 0 when
    /// nothing is attached (or the attached task is not in the list).
    pub(crate) fn cursor_at_attached(&self) -> usize {
        let Some(attached) = &self.attached else {
            return 0;
        };
        self.background
            .as_ref()
            .map(|background| background.running())
            .unwrap_or_default()
            .iter()
            .position(|task| task.id == attached.id)
            .unwrap_or(0)
    }

    /// Up/Down move the cursor and immediately open the selected task's
    /// view — full-output detail for bash tasks, attach for delegate tasks;
    /// Enter does the same. Returns the requested action, if any.
    pub(crate) fn handle_tasks_panel_key(&mut self, key: KeyEvent) -> TaskSelection {
        let running = self
            .background
            .as_ref()
            .map(|background| background.running())
            .unwrap_or_default();
        // Clamp cursor before any navigation: tasks may have finished since
        // the last key press, leaving the cursor past the end.
        self.task_cursor = self.task_cursor.min(running.len().saturating_sub(1));
        let attachable = |state: &TuiState, id: u64| -> bool {
            state
                .attachable
                .as_ref()
                .is_some_and(|attachable| attachable(id))
        };
        match key.code {
            KeyCode::Up => {
                self.task_cursor = self.task_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                if !running.is_empty() {
                    self.task_cursor = (self.task_cursor + 1).min(running.len() - 1);
                }
            }
            KeyCode::Enter => {
                let Some(task) = running.get(self.task_cursor) else {
                    return TaskSelection::None;
                };
                return if task.kind != "delegate" {
                    TaskSelection::OpenDetail(task.id)
                } else if attachable(self, task.id) {
                    // Delegate: attach, exactly like a cursor move.
                    TaskSelection::Attach(task.id)
                } else {
                    TaskSelection::None
                };
            }
            _ => return TaskSelection::None,
        }
        // After any cursor move, open the selected task's view immediately:
        // bash tasks open the full-output detail, delegate sessions attach
        // (live steering) — the same instant-open UX as Enter, so the panel
        // never leaves the cursor on a task the user has already picked.
        let Some(task) = running.get(self.task_cursor) else {
            return TaskSelection::None;
        };
        if task.kind != "delegate" {
            TaskSelection::OpenDetail(task.id)
        } else if attachable(self, task.id) {
            TaskSelection::Attach(task.id)
        } else {
            TaskSelection::None
        }
    }

    /// Open the full-output detail view for a background bash task. The
    /// first frame shows the head page with follow armed; while output
    /// keeps growing the view slides to the live tail, and a paused task
    /// stays on the current page. The detail view is an independent
    /// full-screen view: it never stacks on the tasks panel or an attached
    /// session, and Esc returns to the view that was showing before it
    /// opened (panel or main view).
    pub(crate) fn open_task_detail(&mut self, id: u64) {
        let Some(background) = &self.background else {
            return;
        };
        let Some(spool) = background.spool(id) else {
            return;
        };
        let task = background.running().into_iter().find(|task| task.id == id);
        let label = task
            .as_ref()
            .map(|task| task.label.clone())
            .unwrap_or_default();
        let command = task.and_then(|task| task.full_command);
        let mut detail = TaskDetail::new(id, label, spool, false);
        detail.command = command;
        detail.load_head(self.inner_width.max(1), self.output_height.max(1));
        detail.last_seen_lines = detail.spool.line_count();
        self.task_detail = Some(detail);
        // Remember where Esc should land, then make the detail view truly
        // full-screen: panel hidden, attached session dropped (its
        // half-typed input is stashed for a later re-attach).
        self.detail_return_panel = self.show_tasks;
        self.show_tasks = false;
        self.detach();
    }

    /// Keys while the full-output detail view is open. Esc returns to the
    /// view that was showing before the detail opened (the tasks panel when
    /// it was open, the main view otherwise), F2 closes both, x/Ctrl-C
    /// cancels the task by id, scroll keys page through the spool.
    pub(crate) fn handle_task_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.task_detail = None;
                self.show_tasks = self.detail_return_panel;
            }
            KeyCode::F(2) => {
                self.task_detail = None;
                self.show_tasks = false;
            }
            _ if key.code == KeyCode::Char('x') || is_cancel(key) => {
                if let Some(id) = self.task_detail.as_ref().map(|detail| detail.id) {
                    self.cancel_task(id);
                }
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => self.handle_detail_scroll(key),
            _ => {}
        }
    }

    /// Page through the detail view: line keys move within the current
    /// page, crossing a page boundary loads the adjacent page from the
    /// spool (rebase of the page-internal window).
    pub(crate) fn handle_detail_scroll(&mut self, key: KeyEvent) {
        let Some(detail) = &mut self.task_detail else {
            return;
        };
        let width = self.inner_width.max(1);
        let height = self.output_height.max(1);
        match key.code {
            KeyCode::Up => detail.step_up(width, height),
            KeyCode::Down => detail.step_down(width, height),
            KeyCode::PageUp => detail.page_up(width, height),
            KeyCode::PageDown => detail.page_down(width, height),
            KeyCode::Home => {
                detail.window.follow_bottom = false;
                detail.load_page(0, height, false, width, height);
            }
            KeyCode::End => {
                detail.load_tail(width, height);
                // Sync so the next draw does not re-jump the freshly
                // anchored tail page.
                detail.last_seen_lines = detail.spool.line_count();
            }
            _ => {}
        }
    }

    /// Cancel a background task by id: registry cancel + session
    /// bookkeeping + a dim line in the main scrollback.
    pub(crate) fn cancel_task(&mut self, id: u64) {
        if let Some(label) = self
            .background
            .as_ref()
            .and_then(|background| background.cancel(id))
        {
            match &self.store {
                Some(store) => store.clear_background_task(
                    std::path::Path::new(&self.cwd),
                    &self.session_id,
                    id,
                ),
                // No store wired (tests): JSONL record file directly.
                None => crate::session::Session::clear_background_task(
                    std::path::Path::new(&self.cwd),
                    &self.session_id,
                    id,
                ),
            }
            self.push_line(format!("cancelled task #{id}: {label}"), LineKind::Dim);
        }
    }

    /// `x` / Ctrl-C in the tasks panel cancels the selected background task.
    pub(crate) fn cancel_selected_task(&mut self) {
        let running = self
            .background
            .as_ref()
            .map(|background| background.running())
            .unwrap_or_default();
        let index = self.task_cursor.min(running.len().saturating_sub(1));
        let Some(task) = running.get(index) else {
            return;
        };
        self.cancel_task(task.id);
        // Clamp cursor after removal so it never points past the end.
        let len = self
            .background
            .as_ref()
            .map(|b| b.running().len())
            .unwrap_or(0);
        self.task_cursor = self.task_cursor.min(len.saturating_sub(1));
    }

    /// Process a non-navigation key while the tasks panel is open.
    /// Returns the action taken; the caller dispatches side effects
    /// (attach, edit input, draw) that are not part of the panel state.
    pub(crate) fn handle_panel_key(&mut self, key: KeyEvent) -> PanelAction {
        match key.code {
            KeyCode::F(2) | KeyCode::Esc => {
                self.show_tasks = false;
                PanelAction::ClosePanel
            }
            _ if key.code == KeyCode::Char('x') || is_cancel(key) => {
                self.cancel_selected_task();
                PanelAction::CancelTask
            }
            _ => PanelAction::Passthrough,
        }
    }

    pub(crate) fn handle_scroll(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                // Freeze window on first scroll away from bottom.
                let was_following = self.window.follow_bottom;
                self.window.follow_bottom = false;
                if was_following {
                    // Snapshot: freeze source_end at current position.
                    // Do NOT subtract the active streaming line — instead
                    // snapshot its text so deltas cannot mutate the viewport.
                    self.window.source_end = self.lines.len();
                    self.window.frozen_source_end = self.window.source_end;
                    if self.active_lane.is_some() {
                        self.window.frozen_tail_cursor =
                            self.lines.last().map(|line| line.text.len());
                    }
                }
                let w = self.inner_width.max(1);
                if self.window.local_offset > 0 {
                    self.window.local_offset -= 1;
                } else if self.window.source_start > 0 {
                    self.window.source_start -= 1;
                    let n = line_visual_rows(&self.lines[self.window.source_start], w);
                    self.window.local_offset = n.saturating_sub(1);
                }
                if self.at_scrollback_top() {
                    self.request_older();
                }
            }
            KeyCode::Down => {
                if self.window.follow_bottom {
                    // In follow mode: no-op, stay at bottom.
                    return;
                }
                self.window.local_offset += 1;
                // When frozen and at the local bottom, extend forward.
                extend_window_down(self, 1, |state| {
                    if state.window.frozen_tail_cursor.is_some()
                        && state.window.source_end > state.window.frozen_source_end
                    {
                        state.window.frozen_tail_cursor = None;
                    }
                });
            }
            KeyCode::PageUp => {
                let was_following = self.window.follow_bottom;
                self.window.follow_bottom = false;
                if was_following {
                    self.window.source_end = self.lines.len();
                    self.window.frozen_source_end = self.window.source_end;
                    if self.active_lane.is_some() {
                        self.window.frozen_tail_cursor =
                            self.lines.last().map(|line| line.text.len());
                    }
                }
                let step = 10usize;
                let w = self.inner_width.max(1);
                let mut deficit = step.saturating_sub(self.window.local_offset);
                let mut prepended_rows = 0usize;
                while deficit > 0 && self.window.source_start > 0 {
                    self.window.source_start -= 1;
                    let rows = line_visual_rows(&self.lines[self.window.source_start], w);
                    prepended_rows += rows;
                    deficit = deficit.saturating_sub(rows);
                    if deficit == 0 {
                        self.window.local_offset = self
                            .window
                            .local_offset
                            .saturating_add(prepended_rows)
                            .saturating_sub(step);
                        break;
                    }
                }
                if deficit > 0 {
                    self.window.local_offset = 0;
                } else if prepended_rows == 0 {
                    self.window.local_offset = self.window.local_offset.saturating_sub(step);
                }
                if self.at_scrollback_top() {
                    self.request_older();
                }
            }
            KeyCode::PageDown => {
                if self.window.follow_bottom {
                    // In follow mode: no-op, stay at bottom.
                    return;
                }
                self.window.local_offset += 10;
                // When frozen and at the local bottom, extend forward by
                // enough to show 10 more visual rows.
                extend_window_down(self, 10, |state| {
                    if state.window.frozen_tail_cursor.is_some()
                        && state.window.source_end > state.window.frozen_source_end
                    {
                        state.window.frozen_tail_cursor = None;
                    }
                });
            }
            KeyCode::Home => {
                self.window.follow_bottom = false;
                // Home goes to the true start of the loaded scrollback.
                // The window covers the FIRST lines, not the whole range:
                // local_window_lines renders the *last*
                // MAX_RENDER_SOURCE_LINES of the window, so a [0, len)
                // window would show the tail 256 lines, not the beginning.
                self.window.source_end = self.lines.len().min(MAX_RENDER_SOURCE_LINES);
                self.window.source_start = 0;
                self.window.local_offset = 0;
                self.window.frozen_tail_cursor = None;
                // If older history is still unloaded, queue a
                // jump-to-oldest load (`load_oldest` in one step) rather
                // than the stepwise PageUp path: with N compaction
                // segments the user must not press Home N times to reach
                // the true beginning. `older_done` (or no store) leaves
                // Home as a pure jump to the loaded start.
                if self.store.is_some()
                    && !self.older_done
                    && !self.older_pending
                    && !self.older_loading
                {
                    self.older_pending = true;
                    self.older_is_jump = true;
                }
            }
            KeyCode::End => {
                self.window.follow_bottom = true;
                self.window.frozen_tail_cursor = None;
            }
            _ => {}
        }
    }

    pub(crate) fn at_bottom(&self) -> bool {
        self.window.follow_bottom
    }

    /// True when the viewport shows the very first loaded line (no local
    /// lines above it) — the trigger point for loading older history.
    pub(crate) fn at_scrollback_top(&self) -> bool {
        self.window.source_start == 0 && self.window.local_offset == 0
    }

    /// Queue an asynchronous load of the next older compaction segment (the
    /// stepwise PageUp path). The run loop performs the actual load
    /// (`load_older_history`) when this flag is set; repeated scroll keys
    /// at the top collapse into a single request. No-op without a store,
    /// once history is exhausted, or while a load is pending/in flight.
    /// Resets `older_is_jump` so a Home-queued jump flag can never leak
    /// into a stepwise load.
    pub(crate) fn request_older(&mut self) {
        if self.store.is_some() && !self.older_done && !self.older_loading && !self.older_pending {
            self.older_pending = true;
            self.older_is_jump = false;
        }
    }

    /// Load the next older compaction segment from the store and splice it
    /// in front of the scrollback. Called by the run loop after
    /// `handle_scroll` set `older_pending`. When `older_is_jump` is set
    /// (Home), loads the *oldest* segment in one step via `load_oldest`
    /// and positions the viewport at the true beginning; otherwise pages
    /// backward one segment at a time (Up/PageUp): the first call seeds
    /// `older_cursor` from the store's head-start seq, later calls page
    /// with the cursor returned by `load_older` until it reports `None`.
    /// Errors surface as a dim line and never abort the loop; a JSONL
    /// store (already fully loaded) reports "done" immediately.
    pub(crate) async fn load_older_history(&mut self) {
        self.older_pending = false;
        let Some(store) = self.store.clone() else {
            return;
        };
        let root = self.root.clone();
        let session_id = self.session_id.clone();
        self.older_loading = true;
        if std::mem::take(&mut self.older_is_jump) {
            // Home: fetch the whole oldest segment in one load.
            match store.load_oldest(&root, &session_id).await {
                Ok((entries, cursor)) => {
                    let lines: Vec<DisplayLine> =
                        entries.iter().flat_map(session_entry_to_lines).collect();
                    if !lines.is_empty() {
                        self.prepend_lines(lines);
                    }
                    // If the user paged up stepwise before pressing Home,
                    // the middle segments between `older_cursor` and the
                    // head are already in `lines`; there is nothing left
                    // for load_newer to fetch.
                    self.newer_cursor = if self.older_cursor.is_some() {
                        None
                    } else {
                        cursor
                    };
                    self.older_done = true;
                    // Show the true beginning: prepend_lines shifted
                    // source_start forward by the loaded lines. The window
                    // covers the FIRST lines (local_window_lines renders
                    // the last MAX_RENDER_SOURCE_LINES of the window).
                    self.window.source_start = 0;
                    self.window.source_end = self.lines.len().min(MAX_RENDER_SOURCE_LINES);
                    self.window.local_offset = 0;
                }
                Err(error) => {
                    self.push_line(
                        format!("failed to load older history: {error:#}"),
                        LineKind::Dim,
                    );
                }
            }
            self.older_loading = false;
            return;
        }
        let before = match self.older_cursor {
            Some(cursor) => cursor,
            None => match store.head_seq(&root, &session_id).await {
                Ok(Some(seq)) => seq,
                Ok(None) => {
                    // No compaction entry at all: the head segment covers
                    // the whole session, so there is nothing older.
                    self.older_done = true;
                    self.older_loading = false;
                    return;
                }
                Err(error) => {
                    self.push_line(
                        format!("failed to load older history: {error:#}"),
                        LineKind::Dim,
                    );
                    self.older_loading = false;
                    return;
                }
            },
        };
        match store.load_older(&root, &session_id, before, None).await {
            Ok((entries, cursor)) => {
                let lines: Vec<DisplayLine> =
                    entries.iter().flat_map(session_entry_to_lines).collect();
                if !lines.is_empty() {
                    self.prepend_lines(lines);
                }
                self.older_cursor = cursor;
                if cursor.is_none() {
                    self.older_done = true;
                }
            }
            Err(error) => {
                self.push_line(
                    format!("failed to load older history: {error:#}"),
                    LineKind::Dim,
                );
            }
        }
        self.older_loading = false;
    }

    /// Queue an asynchronous load of the next newer middle compaction
    /// segment. The run loop performs the actual load
    /// (`load_newer_history`) when this flag is set; repeated requests
    /// collapse into a single one. No-op without a store, once the middle
    /// segments are exhausted, without a seeded cursor, or while a load is
    /// pending/in flight.
    pub(crate) fn request_newer(&mut self) {
        if self.store.is_some()
            && self.newer_cursor.is_some()
            && !self.newer_done
            && !self.newer_loading
            && !self.newer_pending
        {
            self.newer_pending = true;
        }
    }

    /// Load the next newer middle compaction segment from the store and
    /// splice it in just before the head segment. Called by the run loop
    /// after `extend_window_down` set `newer_pending` (the user scrolled
    /// to the end of the loaded lines). `newer_cursor` holds the
    /// `after_seq`; `load_newer` returns the segment and the next cursor,
    /// and `None` marks `newer_done` (the head segment was reached — it
    /// is already loaded, so nothing is duplicated). Errors surface as a
    /// dim line and never abort the loop; a JSONL store (already fully
    /// loaded) reports "done" immediately.
    pub(crate) async fn load_newer_history(&mut self) {
        self.newer_pending = false;
        let Some(store) = self.store.clone() else {
            return;
        };
        let root = self.root.clone();
        let session_id = self.session_id.clone();
        self.newer_loading = true;
        let after = match self.newer_cursor {
            Some(cursor) => cursor,
            None => {
                // No middle segments to fetch (never seeded).
                self.newer_loading = false;
                return;
            }
        };
        match store.load_newer(&root, &session_id, after).await {
            Ok((entries, cursor)) => {
                let lines: Vec<DisplayLine> =
                    entries.iter().flat_map(session_entry_to_lines).collect();
                if !lines.is_empty() {
                    self.splice_newer_lines(lines);
                }
                self.newer_cursor = cursor;
                if cursor.is_none() {
                    self.newer_done = true;
                }
            }
            Err(error) => {
                self.push_line(
                    format!("failed to load newer history: {error:#}"),
                    LineKind::Dim,
                );
            }
        }
        self.newer_loading = false;
    }

    /// Splice newly loaded middle-segment lines in just before the head
    /// segment and shift every window index at or after the insertion
    /// point forward by the inserted count, so the viewport keeps showing
    /// the same content. `local_offset` (a visual-row offset inside the
    /// window) is untouched. Only reached while scrolled away from the
    /// bottom; follow mode is already off. The head segment always ends
    /// at `lines.len()`, so an insertion before it never invalidates the
    /// "loaded end" the down-scroll trigger watches.
    pub(crate) fn splice_newer_lines(&mut self, newer: Vec<DisplayLine>) {
        let n = newer.len();
        if n == 0 {
            return;
        }
        let insert_at = self.head_start;
        self.lines.splice(insert_at..insert_at, newer);
        // Indices at or after the insertion point (including exactly at
        // it: the head segment's first line) all move by n.
        if self.window.source_start >= insert_at {
            self.window.source_start += n;
        }
        if self.window.source_end >= insert_at {
            self.window.source_end += n;
        }
        self.head_start = insert_at + n;
    }

    /// Splice `older` in front of the scrollback, keeping the visible
    /// window anchored on the same content: both window indices shift by
    /// the number of prepended lines while `local_offset` (a visual-row
    /// offset inside the window) is untouched. The head segment moves
    /// with the insertion, so `head_start` shifts by the same count.
    /// Only reached while scrolled away from the bottom (Up/PageUp at the
    /// local top), so follow mode is already off; frozen-tail state is
    /// unchanged.
    pub(crate) fn prepend_lines(&mut self, older: Vec<DisplayLine>) {
        let n = older.len();
        if n == 0 {
            return;
        }
        self.lines.splice(0..0, older);
        self.window.source_start += n;
        self.window.source_end += n;
        self.head_start += n;
    }

    pub(crate) fn push_agent_event_inner(&mut self, event: AgentEvent) {
        let ends_delta = matches!(
            &event,
            AgentEvent::UserPrompt(_) | AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. }
        );
        match event {
            AgentEvent::PromptQueued(text) => self.queued.push_back(text),
            AgentEvent::PromptConsumed => {
                self.queued.pop_front();
            }
            AgentEvent::Notice(text) => {
                self.active_lane = None;
                if text.starts_with("──── auto-compact") {
                    self.push_line(text, LineKind::Compaction);
                } else {
                    self.push_line(text, LineKind::Dim);
                }
            }
            AgentEvent::Error(text) => {
                self.active_lane = None;
                self.push_line(format!("error: {text}"), LineKind::Dim);
            }
            AgentEvent::UserPrompt(text) => {
                self.push_line(format!("you> {text}"), LineKind::User);
            }
            AgentEvent::AssistantText(text) => self.push_line(text, LineKind::Normal),
            // Empty deltas carry nothing; letting them flip the active lane
            // would fragment a single line into many pieces.
            AgentEvent::AssistantDelta(text) if !text.is_empty() => {
                if self.active_lane == Some(ActiveStreamLane::Content) {
                    self.lines.last_mut().unwrap().text.push_str(&text);
                } else {
                    self.push_line(text, LineKind::Normal);
                    self.streamed = true;
                    self.active_lane = Some(ActiveStreamLane::Content);
                }
            }
            AgentEvent::ReasoningDelta(text) if !text.is_empty() => {
                if self.active_lane == Some(ActiveStreamLane::Reasoning) {
                    self.lines.last_mut().unwrap().text.push_str(&text);
                } else {
                    self.push_line(format!("thinking: {text}"), LineKind::Thinking);
                    self.active_lane = Some(ActiveStreamLane::Reasoning);
                }
            }
            AgentEvent::AssistantDelta(_) | AgentEvent::ReasoningDelta(_) => {}
            AgentEvent::ToolCall { name, arguments } => self.push_tool_call(&name, &arguments),
            AgentEvent::ToolResult { is_error, content } => {
                self.push_tool_result(&content, is_error)
            }
            // Transient live signal (per-turn subscriber only): the
            // persistent display line comes from the UserPrompt emitted at
            // the turn boundary (handled above), so rendering this too
            // would duplicate it.
            AgentEvent::BackgroundCompleted { .. } => {}
            AgentEvent::BackgroundCompletionNotice {
                id, output, label, ..
            } => {
                // End the streaming lane BEFORE the aside lines: a
                // completion notice interleaving with an in-flight
                // assistant message must not become the append target for
                // the next delta (the body would merge into the Dim line
                // and lose its markdown rendering). Same as Notice/Error.
                self.active_lane = None;
                self.push_background_completion(id, &output, label.as_deref());
            }
            AgentEvent::Usage { context_input, .. } => {
                self.tokens_context = context_input;
            }
        }
        if ends_delta {
            self.streamed = false;
            self.active_lane = None;
        }
        if self.at_bottom() {
            self.follow();
        }
    }

    pub(crate) fn follow(&mut self) {
        self.window.follow_bottom = true;
        self.window.frozen_tail_cursor = None;
        self.window.frozen_source_end = 0;
        // source_start/source_end will be anchored at the tail on next draw.
    }

    pub(crate) fn push_line(&mut self, text: String, kind: LineKind) {
        let was_empty = self.lines.is_empty();
        self.lines.push(DisplayLine { text, kind });
        if !was_empty && self.window.follow_bottom {
            self.window.source_end = self.lines.len();
        }
        if was_empty {
            // First line: initialize window.
            self.window.source_start = 0;
            self.window.source_end = 1;
            self.window.follow_bottom = true;
            self.window.local_offset = 0;
        }
    }

    pub(crate) fn push_tool_call(&mut self, name: &str, arguments: &str) {
        if name == "edit_file"
            && let Some((path, old, new)) = parse_edit_arguments(arguments)
        {
            self.pending_edit = Some((path.clone(), old, new));
            self.push_line(format!("tool: edit_file {path}"), LineKind::ToolCall);
            return;
        }
        self.push_line(format_tool_call(name, arguments), LineKind::ToolCall);
    }

    pub(crate) fn push_tool_result(&mut self, content: &str, is_error: bool) {
        if let Some((_, old, new)) = self.pending_edit.take()
            && !is_error
        {
            let line = parse_edited_line(content);
            self.push_diff_side(&old, "- ", LineKind::Removed, line);
            self.push_diff_side(&new, "+ ", LineKind::Added, line);
            return;
        }
        self.push_line(
            format!(
                "  {}: {}",
                if is_error { "error" } else { "ok" },
                preview(content, 500)
            ),
            if is_error {
                LineKind::ToolError
            } else {
                LineKind::ToolResult
            },
        );
    }

    pub(crate) fn push_diff_side(
        &mut self,
        text: &str,
        prefix: &str,
        kind: LineKind,
        start: Option<usize>,
    ) {
        const DIFF_LINE_LIMIT: usize = 30;
        let mut lines = text.lines();
        let mut number = start.unwrap_or(0);
        for line in lines.by_ref().take(DIFF_LINE_LIMIT) {
            let label = start.map_or_else(String::new, |_| format!("{number:>4} "));
            self.push_line(format!("{prefix}{label}{line}"), kind);
            number += 1;
        }
        let remaining = lines.count();
        if remaining > 0 {
            self.push_line(format!("{prefix}… ({remaining} more lines)"), kind);
        }
    }
}

/// Header line for a background-task completion, shared by the live event
/// path (`push_background_completion`) and the persisted-entry replay
/// (`session_entry_to_lines`) so the two can never drift apart.
fn background_completion_header(id: u64, label: Option<&str>) -> String {
    match label.filter(|l| !l.trim().is_empty()) {
        Some(l) => {
            let previewed = crate::agent::preview(l, 60);
            format!("[background task {id} completed: {previewed}]")
        }
        None => format!("[background task {id} completed]"),
    }
}

/// Convert one persisted `SessionEntry` into the display lines the TUI
/// would have rendered for the same event, mirroring `push_agent_event_inner`
/// (the replay projection is `runner::entry_event` → the same event kinds).
/// Older compaction segments loaded from the store splice in front of the
/// head segment, which the TUI renders through exactly this replay path, so
/// the seam is invisible. `Message::System` and assistant messages without
/// text render nothing, matching the replay.
pub(crate) fn session_entry_to_lines(entry: &SessionEntry) -> Vec<DisplayLine> {
    match entry {
        SessionEntry::Message {
            message: Message::System { .. },
        } => Vec::new(),
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => vec![DisplayLine {
            text: format!("you> {content}"),
            kind: LineKind::User,
        }],
        SessionEntry::Message {
            message: Message::Assistant(message),
        } => message
            .content
            .clone()
            .map(|text| {
                vec![DisplayLine {
                    text,
                    kind: LineKind::Normal,
                }]
            })
            .unwrap_or_default(),
        SessionEntry::Message {
            message: Message::Tool {
                content, is_error, ..
            },
        } => vec![DisplayLine {
            text: format!(
                "  {}: {}",
                if *is_error { "error" } else { "ok" },
                preview(content, 500)
            ),
            kind: if *is_error {
                LineKind::ToolError
            } else {
                LineKind::ToolResult
            },
        }],
        SessionEntry::Compaction { summary, .. } => vec![DisplayLine {
            text: format!("compacted: {summary}"),
            kind: LineKind::Dim,
        }],
        SessionEntry::Notice { text } => vec![DisplayLine {
            text: text.clone(),
            kind: if text.starts_with("──── auto-compact") {
                LineKind::Compaction
            } else {
                LineKind::Dim
            },
        }],
        SessionEntry::BackgroundCompletion {
            id, output, label, ..
        } => {
            let mut lines = vec![DisplayLine {
                text: background_completion_header(*id, label.as_deref()),
                kind: LineKind::Dim,
            }];
            for line in truncate_background_output(output).lines() {
                lines.push(DisplayLine {
                    text: line.to_owned(),
                    kind: LineKind::Dim,
                });
            }
            lines
        }
        SessionEntry::ForkedFrom { source, at, .. } => vec![DisplayLine {
            text: format!("forked from {source} at entry {at}"),
            kind: LineKind::Dim,
        }],
    }
}

pub(crate) fn format_tool_call(name: &str, arguments: &str) -> String {
    use serde_json::Value;
    use std::fmt::Write;

    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return format!("tool: {name} {}", preview(arguments, 200));
    };

    match name {
        "bash" | "pwsh" => {
            let cmd = value
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(arguments);
            let desc = value
                .get("description")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            if let Some(desc) = desc {
                format!("bash: {cmd}  # {desc}")
            } else {
                format!("bash: {cmd}")
            }
        }
        "read_file" => {
            let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let offset = value.get("offset").and_then(|v| v.as_u64());
            let limit = value.get("limit").and_then(|v| v.as_u64());
            let mut s = format!("read: {path}");
            if let Some(o) = offset {
                let _ = write!(s, " [{o}]");
            }
            if let Some(l) = limit {
                let _ = write!(s, " [{l}]");
            }
            s
        }
        "write_file" => {
            let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let content = value.get("content").and_then(|v| v.as_str()).unwrap_or("");
            format!("write: {path} ({} bytes)", content.len())
        }
        "delegate" => {
            let role = value
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("delegate");
            let task = value.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let task = preview(task, 120);
            let label = value
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let bg = value
                .get("background")
                .is_none_or(|v| v.as_bool().unwrap_or(false));
            let ws = value
                .get("workspace")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let mut s = if let Some(l) = label {
                format!("{role}: {} — {task}", preview(l, 40))
            } else {
                format!("{role}: {task}")
            };
            if bg {
                s.push_str(" [background]");
            }
            if let Some(w) = ws {
                let _ = write!(s, " [workspace: {}]", preview(w, 40));
            }
            s
        }
        "web_search" => {
            let query = value
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or(arguments);
            format!("search: {query}")
        }
        "get_background_tasks" => "tasks".to_string(),
        _ => format!("tool: {name} {}", preview(arguments, 200)),
    }
}

#[cfg(test)]
mod format_tool_call_tests {
    use super::*;
    use serde_json::json;

    // ── delegate ────────────────────────────────────────────────────────
    // Compressed from 22 individual tests into table-driven cases below.

    #[test]
    fn delegate_label_cases() {
        let cases = [
            // (role, label, task, expected_substr)
            (
                Some("coder"),
                Some("My Label"),
                "Do the thing",
                "coder: My Label — Do the thing [background]",
            ),
            (
                Some("coder"),
                None,
                "Do the thing",
                "coder: Do the thing [background]",
            ),
            (
                Some("coder"),
                Some("  "),
                "Do the thing",
                "coder: Do the thing [background]",
            ),
            (
                Some("coder"),
                Some("  My Label  "),
                "Do the thing",
                "coder: My Label — Do the thing [background]",
            ),
            (
                None,
                None,
                "just a task",
                "delegate: just a task [background]",
            ),
        ];
        for (role, label, task, expected) in &cases {
            let mut map = serde_json::Map::new();
            map.insert("task".into(), json!(task));
            if let Some(r) = role {
                map.insert("role".into(), json!(r));
            }
            if let Some(l) = label {
                map.insert("label".into(), json!(l));
            }
            let json = serde_json::to_string(&map).unwrap();
            let out = format_tool_call("delegate", &json);
            assert_eq!(
                out, *expected,
                "case role={role:?} label={label:?} task={task:?}"
            );
        }
    }

    #[test]
    fn delegate_background_variants() {
        let cases = [
            (true, "delegate: bg task [background]"),
            (false, "delegate: sync task"),
        ];
        // background:true
        let out = format_tool_call("delegate", r#"{"task":"bg task","background":true}"#);
        assert_eq!(out, cases[0].1);
        // background:false
        let out = format_tool_call("delegate", r#"{"task":"sync task","background":false}"#);
        assert_eq!(out, cases[1].1);
        // absent
        // omitted background defaults to the effective background mode
        let out = format_tool_call("delegate", r#"{"task":"plain task"}"#);
        assert_eq!(out, "delegate: plain task [background]");
    }

    #[test]
    fn delegate_workspace_variants() {
        let cases = [
            (
                Some("/some/path"),
                "delegate: with ws [background] [workspace: /some/path]",
            ),
            (
                Some("  /a/b  "),
                "delegate: trim ws [background] [workspace: /a/b]",
            ),
            (Some(""), "delegate: no ws [background]"),
            (None, "delegate: no ws key [background]"),
            (Some("   "), "delegate: ws blank [background]"),
        ];
        for (workspace, expected) in &cases {
            let mut map = serde_json::Map::new();
            map.insert(
                "task".into(),
                json!(match workspace {
                    Some(_)
                        if workspace.unwrap().trim().is_empty()
                            && !workspace.unwrap().is_empty() =>
                        "ws blank",
                    Some(_) if workspace.unwrap().is_empty() => "no ws",
                    Some(_)
                        if workspace.unwrap().contains('/')
                            && workspace.unwrap().contains("trim") =>
                        "trim ws",
                    Some(_) => "with ws",
                    None => "no ws key",
                }),
            );
            // Simpler: just use a map of workspace value -> task label
            let (task, ws_val) = match workspace {
                Some("/some/path") => ("with ws", Some("/some/path")),
                Some("  /a/b  ") => ("trim ws", Some("  /a/b  ")),
                Some("") => ("no ws", Some("")),
                None => ("no ws key", None),
                Some("   ") => ("ws blank", Some("   ")),
                _ => unreachable!(),
            };
            let mut map = serde_json::Map::new();
            map.insert("task".into(), json!(task));
            if let Some(w) = ws_val {
                map.insert("workspace".into(), json!(w));
            }
            let json = serde_json::to_string(&map).unwrap();
            let out = format_tool_call("delegate", &json);
            assert_eq!(out, *expected, "case workspace={workspace:?}");
        }
    }

    #[test]
    fn delegate_long_content_preview() {
        // Long label (50 chars → preview at 40)
        let long_label = "a".repeat(50);
        let json = format!(r#"{{"label":"{long_label}","task":"short"}}"#);
        let out = format_tool_call("delegate", &json);
        let previewed = preview(&long_label, 40);
        assert_eq!(out, format!("delegate: {previewed} — short [background]"));
        assert!(out.contains('…'), "long label must contain ellipsis");

        // Long task (200 chars → preview at 120)
        let long_task = "b".repeat(200);
        let json = format!(r#"{{"task":"{long_task}"}}"#);
        let out = format_tool_call("delegate", &json);
        let previewed = preview(&long_task, 120);
        assert_eq!(out, format!("delegate: {previewed} [background]"));
        assert!(out.contains('…'), "long task must contain ellipsis");

        // Long workspace (preview at 40)
        let long_ws = "/some/very/long/path/that/should/be/truncated/with/ellipsis/for/safety";
        let json = format!(r#"{{"task":"x","workspace":"{long_ws}"}}"#);
        let out = format_tool_call("delegate", &json);
        let previewed = preview(long_ws, 40);
        assert_eq!(
            out,
            format!("delegate: x [background] [workspace: {previewed}]")
        );
        assert!(out.contains('…'), "long workspace must contain ellipsis");
    }

    #[test]
    fn delegate_combined_and_edge_cases() {
        // All fields
        let out = format_tool_call(
            "delegate",
            r#"{"role":"reviewer","label":"CR","task":"review the code changes in PR","background":true,"workspace":"/tmp/review"}"#,
        );
        assert_eq!(
            out,
            "reviewer: CR — review the code changes in PR [background] [workspace: /tmp/review]"
        );

        // Invalid JSON
        let out = format_tool_call("delegate", "not json at all");
        assert!(
            out.starts_with("tool: delegate "),
            "invalid json fallback: {out}"
        );

        // Empty JSON still reflects the default execution mode.
        let out = format_tool_call("delegate", "{}");
        assert_eq!(out, "delegate:  [background]");
    }
}

#[derive(Default)]
pub(crate) struct InputBuffer {
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

impl InputBuffer {
    pub(crate) fn insert(&mut self, text: &str) {
        let byte = self.byte_index();
        self.text.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    pub(crate) fn insert_char(&mut self, character: char) {
        self.insert(&character.to_string());
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub(crate) fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }
    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }
    pub(crate) fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }
    pub(crate) fn backspace(&mut self) {
        if self.cursor > 0 {
            let end = self.byte_index();
            self.cursor -= 1;
            self.text.drain(self.byte_index()..end);
        }
    }
    pub(crate) fn delete(&mut self) {
        let start = self.byte_index();
        if start < self.text.len() {
            let end = self.text[start..]
                .char_indices()
                .nth(1)
                .map_or(self.text.len(), |(index, _)| start + index);
            self.text.drain(start..end);
        }
    }
    pub(crate) fn byte_index(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(index, _)| index)
    }

    /// Visual row count of the input at the given cell width, including
    /// embedded newlines and soft wrapping. Reserves a row for the cursor
    /// when the text ends exactly at a row boundary.
    pub(crate) fn visual_rows(&self, width: usize) -> usize {
        hard_wrap(&self.text, width)
            .len()
            .max(self.wrapped_cursor(width).0 + 1)
    }

    /// Cursor position as (row, column) in visual rows/cells.
    pub(crate) fn wrapped_cursor(&self, width: usize) -> (usize, usize) {
        let before = &self.text[..self.byte_index()];
        let mut row = 0;
        let mut col = 0;
        for (index, segment) in before.split('\n').enumerate() {
            if index > 0 {
                row += 1;
            }
            let segment_width = UnicodeWidthStr::width(segment);
            row += segment_width / width;
            col = segment_width % width;
        }
        (row, col)
    }
}
