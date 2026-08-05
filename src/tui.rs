use std::io;
use std::path::PathBuf;

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};
use tokio::sync::mpsc;

use crate::agent::{AgentEvent, SessionEntry};
use crate::delegate::Sessions;
use crate::runner::{SessionHandle as RunnerHandle, SessionResult, SessionStatus, SessionTask};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod format_tool_call_tests;
#[cfg(test)]
mod ux_tests;

#[cfg(test)]
mod format_task_label_tests;

mod keys;
mod render;
mod scroll;
mod state;

pub(crate) use keys::*;
pub(crate) use render::*;
pub(crate) use scroll::*;
pub(crate) use state::*;

/// Events on the shared UI channel are tagged by session id: `0` is the
/// main agent, anything else is an attached background session. The TUI
/// routes them to the matching scrollback.
#[derive(Clone, Debug)]
pub(crate) struct UiEvent {
    session: u64,
    event: AgentEvent,
}

/// The one built-in look for the deliberately small TUI. This mirrors the
/// adjusted Solarized Light OpenCode theme instead of offering a theme system.
#[derive(Clone, Copy)]
struct Palette {
    ink: Color,
    background: Color,
    panel: Color,
    element: Color,
    selection: Color,
    subtle: Color,
    text: Color,
    muted: Color,
    blue: Color,
    cyan: Color,
    violet: Color,
    green: Color,
    yellow: Color,
    orange: Color,
    red: Color,
    diff_added_background: Color,
    diff_removed_background: Color,
    diff_added_line_number_background: Color,
    diff_removed_line_number_background: Color,
}

const SOLARIZED_LIGHT: Palette = Palette {
    ink: Color::Rgb(0, 43, 54),                                     // #002b36
    background: Color::Rgb(253, 246, 227),                          // #fdf6e3
    panel: Color::Rgb(248, 240, 218),                               // #f8f0da
    element: Color::Rgb(238, 232, 213),                             // #eee8d5
    selection: Color::Rgb(230, 223, 200),                           // #e6dfc8
    subtle: Color::Rgb(216, 207, 184),                              // #d8cfb8
    text: Color::Rgb(0, 43, 54),                                    // #002b36
    muted: Color::Rgb(147, 161, 161),                               // #93a1a1
    blue: Color::Rgb(38, 139, 210),                                 // #268bd2
    cyan: Color::Rgb(42, 161, 152),                                 // #2aa198
    violet: Color::Rgb(108, 113, 196),                              // #6c71c4
    green: Color::Rgb(133, 153, 0),                                 // #859900
    yellow: Color::Rgb(181, 137, 0),                                // #b58900
    orange: Color::Rgb(203, 75, 22),                                // #cb4b16
    red: Color::Rgb(220, 50, 47),                                   // #dc322f
    diff_added_background: Color::Rgb(238, 243, 210),               // #eef3d2
    diff_removed_background: Color::Rgb(246, 221, 213),             // #f6ddd5
    diff_added_line_number_background: Color::Rgb(227, 235, 189),   // #e3ebbd
    diff_removed_line_number_background: Color::Rgb(239, 207, 199), // #efcfc7
};

impl Palette {
    fn screen_style(self) -> Style {
        Style::default().bg(self.background).fg(self.text)
    }

    fn panel_style(self) -> Style {
        Style::default().bg(self.panel).fg(self.text)
    }

    fn block(self, title: impl Into<Line<'static>>) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .style(self.panel_style())
            .border_style(Style::default().fg(self.subtle).bg(self.panel))
            .title_style(Style::default().fg(self.blue).add_modifier(Modifier::BOLD))
    }

    fn line_style(self, kind: LineKind) -> Style {
        match kind {
            LineKind::Normal => self.screen_style(),
            LineKind::Dim => Style::default()
                .fg(self.muted)
                .bg(self.background)
                .add_modifier(Modifier::DIM),
            LineKind::User => Style::default()
                .fg(self.cyan)
                .bg(self.background)
                .add_modifier(Modifier::BOLD),
            LineKind::ToolCall => Style::default()
                .fg(self.violet)
                .bg(self.element)
                .add_modifier(Modifier::BOLD),
            LineKind::ToolResult => Style::default().fg(self.green).bg(self.background),
            LineKind::ToolError => Style::default()
                .fg(self.red)
                .bg(self.background)
                .add_modifier(Modifier::BOLD),
            LineKind::Added => Style::default()
                .fg(self.green)
                .bg(self.diff_added_background),
            LineKind::Removed => Style::default()
                .fg(self.red)
                .bg(self.diff_removed_background),
            LineKind::Compaction => Style::default()
                .fg(self.violet)
                .bg(self.element)
                .add_modifier(Modifier::BOLD),
            // Bright violet reads clearly as "the model is thinking"
            // against the dark body text and the grey dim notices (a DIM
            // violet previously collapsed toward grey). No ITALIC: some
            // terminals (e.g. Termux's Android fonts) render synthetic
            // italics with clipped glyphs.
            LineKind::Thinking => Style::default().fg(self.violet).bg(self.background),
        }
    }

    fn diff_line_number_style(self, kind: LineKind) -> Option<Style> {
        match kind {
            LineKind::Added => Some(
                Style::default()
                    .fg(self.green)
                    .bg(self.diff_added_line_number_background),
            ),
            LineKind::Removed => Some(
                Style::default()
                    .fg(self.red)
                    .bg(self.diff_removed_line_number_background),
            ),
            _ => None,
        }
    }

    fn queue_style(self) -> Style {
        Style::default()
            .fg(self.ink)
            .bg(self.blue)
            .add_modifier(Modifier::BOLD)
    }
}

/// Bridge a session handle's event stream into the shared UI channel,
/// tagging each event with `session_id`. The caller aborts the returned
/// handle when the view detaches; otherwise re-attaching would leave two
/// bridges forwarding every future delta.
fn bridge(
    session_id: u64,
    mut stream: tokio::sync::broadcast::Receiver<AgentEvent>,
    sender: mpsc::UnboundedSender<UiEvent>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        loop {
            match stream.recv().await {
                Ok(event) => {
                    if sender
                        .send(UiEvent {
                            session: session_id,
                            event,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // Lagged: events were missed. A re-attach restores full
                // fidelity from the snapshot.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .abort_handle()
}

/// Attach the TUI to a background task's session: replay its event log into
/// a fresh view and follow its live stream from now on.
fn attach_to_task(
    state: &mut TuiState,
    task_id: u64,
    sessions: &Sessions,
    sender: &mpsc::UnboundedSender<UiEvent>,
) {
    let Some(entry) = sessions.get(task_id) else {
        return;
    };
    let label = state
        .background
        .as_ref()
        .and_then(|background| {
            background
                .running()
                .into_iter()
                .find(|task| task.id == task_id)
        })
        .map(|task| task.label)
        .unwrap_or_default();
    let (snapshot, live, status) = entry.handle.attach();
    state.attach(
        task_id,
        label,
        entry.handle.clone(),
        entry.model.clone(),
        entry.role.clone(),
        entry.cwd.clone(),
        entry.session_id.clone(),
        entry.context_window,
        snapshot,
        status,
    );
    let bridge = bridge(task_id, live, sender.clone());
    state.attached.as_mut().unwrap().bridge = Some(bridge);
}

/// Width of the attached steering input's content area (frame minus its
/// borders), matching what draw() renders into.
fn attached_input_width(
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) -> anyhow::Result<usize> {
    Ok(usize::from(terminal.size()?.width.saturating_sub(2)).max(1))
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    handle: RunnerHandle,
    _task: SessionTask,
    root: PathBuf,
    session_name: String,
    background: crate::tools::BackgroundTasks,
    sessions: Sessions,
    model_name: String,
    role_name: Option<String>,
    context_window: Option<u64>,
    store: crate::session_store::SessionStore,
    model: crate::model::ConfiguredModel,
    workspace: crate::workspace::Workspace,
    sandbox: Option<crate::config::Sandbox>,
    session_backend: crate::config::SessionBackend,
    read_only: bool,
    record_in: Option<crate::session_store::BackgroundRecord>,
    factory: std::sync::Arc<crate::session_factory::SessionFactory>,
    input_keys: crate::config::InputKeys,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let _guard = TerminalGuard::new();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let labels = InputLabels {
        session: session_name,
        model: model_name,
        cwd: root.display().to_string(),
        role: role_name,
    };
    let result = run_inner(
        &mut terminal,
        handle,
        &labels,
        root,
        background,
        sessions,
        context_window,
        store,
        model,
        workspace,
        sandbox,
        session_backend,
        read_only,
        record_in,
        factory,
        input_keys,
    )
    .await;
    drop(terminal);
    drop(_guard);
    match result {
        // The session failed. The main screen is restored now (terminal and
        // guard dropped above), so the failure text is safe to surface via
        // Err: main prints it on stderr and exits non-zero. Printing it
        // before LeaveAlternateScreen would write into the alternate screen
        // buffer and be discarded on rmcup (silent EXIT 0).
        Ok(Some(failure)) => Err(anyhow::anyhow!("session failed: {failure}")),
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Text painted around the input box border (session id, model, role, cwd).
struct InputLabels {
    session: String,
    model: String,
    cwd: String,
    role: Option<String>,
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Self {
        let _ = execute!(io::stdout(), SetTitle("e-agent"));
        Self
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetTitle(""),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    handle: RunnerHandle,
    labels: &InputLabels,
    root: PathBuf,
    background: crate::tools::BackgroundTasks,
    sessions: Sessions,
    context_window: Option<u64>,
    store: crate::session_store::SessionStore,
    model: crate::model::ConfiguredModel,
    workspace: crate::workspace::Workspace,
    sandbox: Option<crate::config::Sandbox>,
    backend: crate::config::SessionBackend,
    read_only: bool,
    record_in: Option<crate::session_store::BackgroundRecord>,
    factory: std::sync::Arc<crate::session_factory::SessionFactory>,
    input_keys: crate::config::InputKeys,
) -> anyhow::Result<Option<String>> {
    let (sender, mut inbox) = mpsc::unbounded_channel::<UiEvent>();
    let (snapshot, mut live, mut status) = handle.attach();
    let forward = sender.clone();
    tokio::spawn(async move {
        while let Ok(event) = live.recv().await {
            if forward.send(UiEvent { session: 0, event }).is_err() {
                break;
            }
        }
    });
    let mut events = EventStream::new().peekable();
    let mut state = TuiState {
        keys: input_keys,
        ..TuiState::default()
    };
    for event in snapshot {
        state.push_event(UiEvent { session: 0, event });
    }
    state.session_id = labels.session.clone();
    state.model_name = labels.model.clone();
    state.cwd = labels.cwd.clone();
    state.root = root;
    state.role_name = labels.role.clone();
    state.context_window = context_window;
    state.background = Some(background);
    state.store = Some(store);
    // btw fork context (delegate::BtwContext), wired from the factory like
    // the server's /btw endpoint does: the subagent inherits the main
    // session's model, workspace, sandbox, read-only policy and backend.
    state.model = Some(model);
    state.workspace = Some(workspace);
    state.sandbox = sandbox;
    state.backend = Some(backend);
    state.read_only = read_only;
    state.record_in = record_in;
    state.sessions = Some(sessions.clone());
    state.factory = Some(factory);
    set_terminal_title(&labels.session);
    let probe = sessions.clone();
    state.attachable = Some(Box::new(move |id| probe.get(id).is_some()));
    loop {
        state.busy = match &*status.borrow() {
            SessionStatus::Busy => Some(BusyState::thinking()),
            SessionStatus::Compacting => Some(BusyState::compacting()),
            _ => None,
        };
        // Keep the attached view in step with its runner's real status, the
        // same way the main view refreshes above. This closes the window
        // where a finished delegate still shows "thinking" until the parent
        // agent's next commit_backgrounds round trips a BackgroundCompleted.
        if let Some(attached) = &mut state.attached {
            match attached.status.borrow().clone() {
                SessionStatus::Busy => attached.state.busy = Some(BusyState::thinking()),
                SessionStatus::Compacting => attached.state.busy = Some(BusyState::compacting()),
                SessionStatus::Idle => attached.state.busy = None,
                SessionStatus::Finished(_) => {
                    attached.finished = true;
                    attached.state.busy = None;
                }
            }
        }
        // Esc-priority: service urgent keys already queued BEFORE paying
        // for a (potentially slow) draw. A slow frame — e.g. a cargo build
        // dumping tens of thousands of lines into the task-detail view —
        // must never delay leaving the view: Esc/Ctrl-C/x/F2/scroll are
        // consumed here (via the same handler the select uses, so both
        // paths behave identically) and the loop redraws after them.
        // Non-urgent keys stay in the stream and are handled by the select
        // below, keeping normal typing latency unchanged.
        if let Some(key) = peek_urgent_key(&mut events).await {
            let _ = events.next().await; // consume the peeked key
            match handle_pressed_key(
                &mut state,
                key,
                &handle,
                &status,
                &sessions,
                &sender,
                terminal,
                &mut events,
            )
            .await?
            {
                KeyHandled::Continue => continue,
                KeyHandled::Exit => return Ok(None),
            }
        }
        draw(terminal, &mut state)?;
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    return Ok(None);
                }
                if let SessionStatus::Finished(result) = &*status.borrow() {
                    // A terminal state with a failure must be visible: the
                    // TUI exits silently otherwise and looks like a crash.
                    // Carry the failure text out of the alternate screen;
                    // run() prints it only after the main screen is restored.
                    return match result {
                        SessionResult::Failed(text) => Ok(Some(text.clone())),
                        _ => Ok(None),
                    };
                }
            }
            Some(first) = inbox.recv() => route_idle_events(&mut state, first, &mut inbox),
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                    match handle_pressed_key(&mut state, key, &handle, &status, &sessions, &sender, terminal, &mut events).await? {
                        KeyHandled::Continue => continue,
                        KeyHandled::Exit => return Ok(None),
                    }
                }
                Some(Ok(Event::Paste(text))) => state.handle_paste(&text),
                // Resize needs no explicit handling: the next draw()
                // re-derives the layout from the new terminal size, and a
                // scrolled-up (non-follow) window is re-anchored inside
                // draw on the width change, so the viewport top stays put.
                Some(Ok(Event::Resize(_, _))) => {}
                Some(Ok(_)) => {}, Some(Err(error)) => return Err(error.into()), None => return Ok(None),
            }
        }
    }
}

/// Outcome of handling a pressed key in the main loop.
enum KeyHandled {
    /// Key consumed; the loop redraws on its next iteration.
    Continue,
    /// Esc/Ctrl-C from the idle main view: exit the TUI.
    Exit,
}

/// Keys that must be serviced before a (potentially slow) draw: leaving a
/// view (Esc/F2), cancelling (Ctrl-C/x) and scrolling. A slow frame must
/// never delay these — Esc getting stuck in the task-detail view was the
/// reported bug.
fn is_urgent_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || key.code == KeyCode::F(2)
        || is_cancel(key)
        || key.code == KeyCode::Char('x')
        || is_scroll_key(key)
}

/// Non-blocking peek for an already-queued urgent key. Returns a COPY of
/// the key without consuming it, so non-urgent events keep their stream
/// position for the select below. The caller consumes with `events.next()`.
async fn peek_urgent_key(
    events: &mut futures_util::stream::Peekable<EventStream>,
) -> Option<KeyEvent> {
    // StreamExt::peek is pin-projected (same as drain_ready_scroll_keys).
    // A 1 ms timeout keeps this effectively non-blocking: when a key is
    // already queued the peek resolves on its first poll (no wait at all);
    // when the queue is empty the loop falls through to draw immediately.
    let Ok(next) = tokio::time::timeout(
        std::time::Duration::from_millis(1),
        std::pin::Pin::new(&mut *events).peek(),
    )
    .await
    else {
        return None;
    };
    match next {
        Some(Ok(Event::Key(key)))
            if key.kind == crossterm::event::KeyEventKind::Press && is_urgent_key(*key) =>
        {
            Some(*key)
        }
        _ => None,
    }
}

/// Handle one pressed key in the main loop. Shared by the pre-draw urgent
/// peek and the select's event branch so both paths behave identically.
/// The branches mirror the original inline select-arm handler exactly:
/// task detail, tasks panel, attached session, then the main view
/// (cancel / exit / scroll / input keys).
#[allow(clippy::too_many_arguments)]
async fn handle_pressed_key(
    state: &mut TuiState,
    key: KeyEvent,
    handle: &RunnerHandle,
    status: &tokio::sync::watch::Receiver<SessionStatus>,
    sessions: &Sessions,
    sender: &mpsc::UnboundedSender<UiEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    events: &mut futures_util::stream::Peekable<EventStream>,
) -> anyhow::Result<KeyHandled> {
    if state.task_detail.is_some() {
        state.handle_task_detail_key(key);
        return Ok(KeyHandled::Continue);
    }
    if state.show_tasks {
        match state.handle_tasks_panel_key(key) {
            TaskSelection::Attach(id) => attach_to_task(state, id, sessions, sender),
            TaskSelection::OpenDetail(id) => state.open_task_detail(id),
            TaskSelection::None => {
                let _ = state.handle_panel_key(key);
            }
        }
        return Ok(KeyHandled::Continue);
    }
    if state.attached.is_some() {
        if key.code == KeyCode::Esc {
            state.detach();
        } else if is_scroll_key(key) {
            state.attached.as_mut().unwrap().state.handle_scroll(key);
        } else {
            let width = attached_input_width(terminal)?;
            state.handle_attached_key(key, width);
        }
        return Ok(KeyHandled::Continue);
    }
    let active = matches!(
        &*status.borrow(),
        SessionStatus::Busy | SessionStatus::Compacting
    );
    if active && is_cancel(key) {
        handle.cancel();
        return Ok(KeyHandled::Continue);
    }
    if !active && is_exit(key) {
        return Ok(KeyHandled::Exit);
    }
    if is_scroll_key(key) {
        state.handle_scroll(key);
        drain_ready_scroll_keys(events, state).await;
        if state.older_pending {
            state.load_older_history().await;
        }
        if state.newer_pending {
            state.load_newer_history().await;
        }
    } else if let Some(prompt) = state.handle_key(key) {
        if prompt == "/compact" {
            handle.compact();
        } else if prompt == "/undo" {
            handle_undo(state);
        } else if prompt == "/help" {
            state.push_agent_event(AgentEvent::Notice(HELP_TEXT.to_string()));
        } else if let Some(command) = parse_model(&prompt) {
            handle_model(command, state, handle);
        } else if let Some(command) = parse_rename(&prompt) {
            handle_rename(command, state).await;
        } else if let Some(command) = parse_btw(&prompt) {
            handle_btw(command, state).await;
        } else if let Some(command) = parse_fork(&prompt) {
            handle_fork(command, state).await;
        } else {
            if !state.session_title_set {
                set_terminal_title(&sanitize_title(&prompt));
                state.session_title_set = true;
            }
            handle.prompt(prompt);
        }
    }
    Ok(KeyHandled::Continue)
}

/// `/help` output: the supported slash commands (kept in sync with the web
/// UI's SLASH_COMMANDS in `src/ui/sessions.js`).
const HELP_TEXT: &str = "\
/compact - 压缩上下文
/rename <标题> - 重命名会话
/btw <问题> - fork 旁路 subagent
/fork - 从历史消息 fork
/undo - 撤销文件操作";

fn route_idle_events(
    state: &mut TuiState,
    first: UiEvent,
    inbox: &mut mpsc::UnboundedReceiver<UiEvent>,
) {
    state.push_event(first);
    while let Ok(event) = inbox.try_recv() {
        state.push_event(event);
    }
}

/// Run a `/undo` command: reverse the most recent file operation
/// (`edit_file` / `write_file`) via the in-process undo stack. The TUI is a
/// local process, so this calls the tools layer directly — the same
/// function the server's `POST /api/sessions/{id}/undo` endpoint invokes.
/// Success and failure are both surfaced as a Notice in the scrollback
/// (display-only, same as `/rename` and `/btw`).
fn handle_undo(state: &mut TuiState) {
    let message = match crate::tools::undo_file_op() {
        Ok(message) => message,
        Err(error) => error,
    };
    state.push_agent_event(AgentEvent::Notice(message));
}

/// A parsed `/rename` command from the input line.
#[derive(Debug, PartialEq, Eq)]
enum RenameCommand {
    /// Bare `/rename` — show usage.
    Usage,
    /// `/rename ` with empty arguments — clear the title.
    Clear,
    /// `/rename <title>` — set the title.
    Set(String),
}

/// Parse a `/rename` command. Returns `None` for any other input so the
/// caller falls through to the normal prompt path. Command matching is
/// strict (`/rename` plus a space separator): `/renamexxx` stays a prompt.
fn parse_rename(prompt: &str) -> Option<RenameCommand> {
    if prompt == "/rename" {
        return Some(RenameCommand::Usage);
    }
    let rest = prompt.strip_prefix("/rename ")?;
    let rest = rest.trim();
    if rest.is_empty() {
        Some(RenameCommand::Clear)
    } else {
        Some(RenameCommand::Set(rest.to_string()))
    }
}

/// Run a `/rename` command: persist the title via the store (Greptime
/// appends a snapshot row with the new title; JSONL is a no-op Ok) and
/// mirror the result into the terminal title. Success and failure are both
/// surfaced as a Notice (pushed into the TUI scrollback — the session
/// handle exposes no event-injection path, so the notice is display-only)
/// so the rename never fails silently.
async fn handle_rename(command: RenameCommand, state: &mut TuiState) {
    let Some(store) = state.store.clone() else {
        return; // No store wired (tests): nothing to persist.
    };
    let root = state.root.clone();
    let session_name = state.session_id.clone();
    match command {
        RenameCommand::Usage => state.push_agent_event(AgentEvent::Notice(
            "用法：/rename <标题>（留空标题可清除）".to_string(),
        )),
        RenameCommand::Clear => match store.set_title(&root, &session_name, None).await {
            Ok(()) => {
                set_terminal_title(&sanitize_title(&session_name));
                state.push_agent_event(AgentEvent::Notice("已清除标题".to_string()));
            }
            Err(error) => {
                state.push_agent_event(AgentEvent::Notice(format!("重命名失败：{error:#}")))
            }
        },
        RenameCommand::Set(title) => {
            match store.set_title(&root, &session_name, Some(&title)).await {
                Ok(()) => {
                    set_terminal_title(&sanitize_title(&title));
                    state.push_agent_event(AgentEvent::Notice(format!("已重命名：{title}")));
                }
                Err(error) => {
                    state.push_agent_event(AgentEvent::Notice(format!("重命名失败：{error:#}")))
                }
            }
        }
    }
}

/// A parsed `/model` command from the input line.
#[derive(Debug, PartialEq, Eq)]
enum ModelCommand {
    /// Bare `/model` — show the current model and usage.
    Usage,
    /// `/model <profile>` — switch the session's model at runtime.
    Switch(String),
}

/// Parse a `/model` command. Returns `None` for any other input so the
/// caller falls through to the normal prompt path. Command matching is
/// strict (`/model` plus a space separator): `/modelxxx` stays a prompt.
fn parse_model(prompt: &str) -> Option<ModelCommand> {
    if prompt == "/model" {
        return Some(ModelCommand::Usage);
    }
    let rest = prompt.strip_prefix("/model ")?;
    let rest = rest.trim();
    if rest.is_empty() {
        Some(ModelCommand::Usage)
    } else {
        Some(ModelCommand::Switch(rest.to_string()))
    }
}

/// Run a `/model` command: resolve the profile through the session factory
/// (the same config + `--base-url`/`--model` overrides the process started
/// with) and switch the session's model at runtime. The runner installs the
/// new model on its agent; the display name is mirrored into the input
/// border immediately. Success and failure are both surfaced as a Notice.
fn handle_model(command: ModelCommand, state: &mut TuiState, handle: &RunnerHandle) {
    match command {
        ModelCommand::Usage => {
            let current = state.model_name.clone();
            state.push_agent_event(AgentEvent::Notice(format!(
                "当前模型：{current}。用法：/model <profile>（如 /model deepseek/flash）"
            )));
        }
        ModelCommand::Switch(profile) => {
            let Some(factory) = state.factory.clone() else {
                state.push_agent_event(AgentEvent::Notice(
                    "无法解析模型：进程没有配置（无 config.toml）".to_string(),
                ));
                return;
            };
            match factory.resolve_profile(&profile) {
                Ok(configured) => {
                    let name = configured.display_name().to_owned();
                    state.model_name = name.clone();
                    state.model = Some(configured.clone());
                    handle.switch_model(Box::new(configured));
                    state.push_agent_event(AgentEvent::Notice(format!("已切换到 {name}")));
                }
                Err(error) => {
                    state.push_agent_event(AgentEvent::Notice(format!(
                        "未知模型 profile：{error:#}"
                    )));
                }
            }
        }
    }
}

/// A parsed `/btw` command from the input line.
#[derive(Debug, PartialEq, Eq)]
enum BtwCommand {
    /// Bare `/btw` (or `/btw ` with only whitespace) — show usage.
    Usage,
    /// `/btw <question>` — fork a persistent interactive subagent.
    Ask(String),
}

/// Parse a `/btw` command. Returns `None` for any other input so the
/// caller falls through to the normal prompt path. Command matching is
/// strict (`/btw` plus a space separator): `/btwxxx` stays a prompt.
fn parse_btw(prompt: &str) -> Option<BtwCommand> {
    if prompt == "/btw" {
        return Some(BtwCommand::Usage);
    }
    let rest = prompt.strip_prefix("/btw ")?;
    let rest = rest.trim();
    if rest.is_empty() {
        Some(BtwCommand::Usage)
    } else {
        Some(BtwCommand::Ask(rest.to_string()))
    }
}

/// A parsed `/fork` command from the input line.
#[derive(Debug, PartialEq, Eq)]
enum ForkCommand {
    /// Bare `/fork` — fork at the most recent completed-turn boundary
    /// (equivalent to `/fork 1`).
    Latest,
    /// `/fork N` — fork at the N-th completed-turn boundary counted from
    /// the newest (1-based).
    At(usize),
    /// `/fork` with invalid arguments (empty, zero, or non-numeric) —
    /// show usage.
    Usage,
}

/// Parse a `/fork` command. Returns `None` for any other input so the
/// caller falls through to the normal prompt path. Command matching is
/// strict (`/fork` plus a space separator): `/forkxxx` stays a prompt.
fn parse_fork(prompt: &str) -> Option<ForkCommand> {
    if prompt == "/fork" {
        return Some(ForkCommand::Latest);
    }
    let rest = prompt.strip_prefix("/fork ")?;
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(ForkCommand::Usage);
    }
    match rest.parse::<usize>() {
        Ok(n) if n > 0 => Some(ForkCommand::At(n)),
        _ => Some(ForkCommand::Usage),
    }
}

/// Run a `/fork` command: copy this session's history up to the N-th
/// completed-turn boundary (counted from the newest) into a fresh
/// `fork-…` session, mirroring the `--fork` CLI / web fork path. The TUI
/// cannot switch sessions at runtime, so the new id is surfaced as a
/// Notice with the `--session` flag to open it in a new terminal. Success
/// and failure are both surfaced as a Notice (display only, same as
/// `/rename` and `/btw`).
async fn handle_fork(command: ForkCommand, state: &mut TuiState) {
    let n = match command {
        ForkCommand::Usage => {
            state.push_agent_event(AgentEvent::Notice(
                "用法：/fork [N]（从最新往上数第 N 个完成的回合边界 fork 出新会话，默认 N=1）"
                    .to_string(),
            ));
            return;
        }
        ForkCommand::Latest => 1,
        ForkCommand::At(n) => n,
    };
    let Some(store) = state.store.clone() else {
        state.push_agent_event(AgentEvent::Notice(
            "无法 fork：TUI 未接线（缺少会话存储）".to_string(),
        ));
        return;
    };
    let Some(backend) = state.backend.clone() else {
        state.push_agent_event(AgentEvent::Notice(
            "无法 fork：TUI 未接线（缺少会话后端）".to_string(),
        ));
        return;
    };
    let root = match &state.record_in {
        Some(record) => record.root.clone(),
        None => state.root.clone(),
    };
    if root.as_os_str().is_empty() {
        state.push_agent_event(AgentEvent::Notice(
            "无法 fork：TUI 未接线（缺少会话根目录）".to_string(),
        ));
        return;
    }
    let session_id = state.session_id.clone();
    let with_seq = match store.load_with_seq(&root, &session_id).await {
        Ok(with_seq) => with_seq,
        Err(error) => {
            state.push_agent_event(AgentEvent::Notice(format!("无法 fork：{error:#}")));
            return;
        }
    };
    // Turn boundaries paired with their 0-based entry index (for the
    // 1-based `at`) and backend seq (provenance on the ForkedFrom marker).
    let boundaries: Vec<(usize, i64)> = with_seq
        .iter()
        .enumerate()
        .filter(|(_, (_, entry))| crate::agent::is_turn_boundary(entry))
        .map(|(index, (seq, _))| (index, *seq))
        .collect();
    if boundaries.len() < n {
        state.push_agent_event(AgentEvent::Notice(format!(
            "无法 fork：只有 {} 个可 fork 的回合边界",
            boundaries.len()
        )));
        return;
    }
    let (boundary_index, boundary_seq) = boundaries[boundaries.len() - n];
    let at = boundary_index + 1; // 1-based full-history index, inclusive.
    let entries: Vec<SessionEntry> = with_seq.into_iter().map(|(_, entry)| entry).collect();
    let prefix = match crate::agent::fork_prefix(&entries, Some(at)) {
        Ok(prefix) => prefix,
        Err(error) => {
            state.push_agent_event(AgentEvent::Notice(format!("无法 fork：{error}")));
            return;
        }
    };
    let new_id = crate::session::new_id_prefixed("fork-");
    let fork_store =
        match crate::session_store::SessionStore::connect(&backend, &root, &new_id).await {
            Ok(store) => store,
            Err(error) => {
                state.push_agent_event(AgentEvent::Notice(format!("无法 fork：{error:#}")));
                return;
            }
        };
    // The marker sits at the fork point: source prefix first, then the
    // marker (seq = the boundary's original seq), then the new session's
    // own messages — same layout as `SessionFactory::build`'s fork branch.
    let marker = SessionEntry::ForkedFrom {
        source: session_id,
        at: prefix.len(),
        // JSONL has no event_time column; kept as an optional provenance slot.
        event_time: None,
        seq: Some(boundary_seq),
    };
    let prefix_len = prefix.len();
    let mut fork_entries = Vec::with_capacity(prefix_len + 1);
    fork_entries.extend(prefix);
    fork_entries.push(marker);
    let result = match backend {
        // Atomic create-or-replace for a brand-new JSONL session file.
        crate::config::SessionBackend::Jsonl => {
            fork_store.rewrite(&root, &new_id, &fork_entries).await
        }
        // Greptime: fresh session (no rows); append writes contiguous seqs.
        crate::config::SessionBackend::Greptime { .. } => {
            fork_store.append(&root, &new_id, &fork_entries).await
        }
        // SQLite: same append-only semantics as Greptime.
        crate::config::SessionBackend::Sqlite { .. } => {
            fork_store.append(&root, &new_id, &fork_entries).await
        }
    };
    match result {
        Ok(()) => state.push_agent_event(AgentEvent::Notice(format!(
            "已 fork 到新会话：{new_id}（保留 {prefix_len} 条历史）。新终端用 --session {new_id} 打开。"
        ))),
        Err(error) => state.push_agent_event(AgentEvent::Notice(format!("无法 fork：{error:#}"))),
    }
}

/// Assemble the [`crate::delegate::BtwContext`] for `/btw` from the state
/// wired by `run_inner`. Returns `None` when a required component is
/// missing — only possible in unit-test state without a run loop, since
/// `run_inner` sets every field together.
fn btw_context(state: &TuiState) -> Option<crate::delegate::BtwContext> {
    Some(crate::delegate::BtwContext {
        model: state.model.clone()?,
        context_window: state.context_window,
        workspace: state.workspace.clone()?,
        sandbox: state.sandbox.clone(),
        read_only: state.read_only,
        background: state.background.clone()?,
        sessions: state.sessions.clone()?,
        persist_root: state.root.clone(),
        backend: state.backend.clone()?,
        record_in: state.record_in.clone(),
    })
}

/// Run a `/btw` command: fork this session's full history into a
/// persistent interactive "btw fork" subagent and start it with the
/// question as its first user message. The main session keeps running
/// untouched — the fork is deliberately NOT auto-attached; it shows up in
/// the F2 task panel and can be attached to there. Success and failure are
/// both surfaced as a Notice (pushed into the TUI scrollback — display
/// only, same as `/rename`).
async fn handle_btw(command: BtwCommand, state: &mut TuiState) {
    let question = match command {
        BtwCommand::Usage => {
            state.push_agent_event(AgentEvent::Notice(
                "用法：/btw <问题>（fork 出独立子代理继续探讨，F2 任务面板可 attach）".to_string(),
            ));
            return;
        }
        BtwCommand::Ask(question) => question,
    };
    let Some(context) = btw_context(state) else {
        state.push_agent_event(AgentEvent::Notice(
            "btw 创建失败：TUI 未接线（缺少模型/工作区/后端配置）".to_string(),
        ));
        return;
    };
    match crate::delegate::spawn_btw_subagent(&state.session_id, &question, context).await {
        Ok(id) => state.push_agent_event(AgentEvent::Notice(format!(
            "已创建 btw subagent：{id}（F2 任务面板可 attach）"
        ))),
        Err(error) => state.push_agent_event(AgentEvent::Notice(format!("btw 创建失败：{error}"))),
    }
}

#[cfg(test)]
mod rename_tests {
    use super::{RenameCommand, parse_rename};

    #[test]
    fn parse_rename_commands() {
        assert_eq!(parse_rename("/rename"), Some(RenameCommand::Usage));
        assert_eq!(parse_rename("/rename "), Some(RenameCommand::Clear));
        assert_eq!(parse_rename("/rename   "), Some(RenameCommand::Clear));
        assert_eq!(
            parse_rename("/rename 新标题"),
            Some(RenameCommand::Set("新标题".to_string()))
        );
        assert_eq!(
            parse_rename("/rename   padded title  "),
            Some(RenameCommand::Set("padded title".to_string()))
        );
        // Non-rename input falls through to the normal prompt path.
        assert_eq!(parse_rename("/renamexxx"), None);
        assert_eq!(parse_rename("/compact"), None);
        assert_eq!(parse_rename("hello"), None);
    }
}

#[cfg(test)]
mod btw_tests {
    use super::{BtwCommand, parse_btw};

    #[test]
    fn parse_btw_commands() {
        // Bare `/btw` shows usage.
        assert_eq!(parse_btw("/btw"), Some(BtwCommand::Usage));
        assert_eq!(parse_btw("/btw "), Some(BtwCommand::Usage));
        assert_eq!(parse_btw("/btw   "), Some(BtwCommand::Usage));
        assert_eq!(
            parse_btw("/btw 为什么这个 bug 会出现？"),
            Some(BtwCommand::Ask("为什么这个 bug 会出现？".to_string()))
        );
        assert_eq!(
            parse_btw("/btw   padded question  "),
            Some(BtwCommand::Ask("padded question".to_string()))
        );
        // Non-btw input falls through to the normal prompt path.
        assert_eq!(parse_btw("/btwxxx"), None);
        assert_eq!(parse_btw("/btwx question"), None);
        assert_eq!(parse_btw("/compact"), None);
        assert_eq!(parse_btw("/rename x"), None);
        assert_eq!(parse_btw("hello"), None);
    }
}

#[cfg(test)]
mod fork_tests {
    use super::{ForkCommand, parse_fork};

    #[test]
    fn parse_fork_commands() {
        // Bare `/fork` means the most recent boundary (like /fork 1).
        assert_eq!(parse_fork("/fork"), Some(ForkCommand::Latest));
        assert_eq!(parse_fork("/fork 1"), Some(ForkCommand::At(1)));
        assert_eq!(parse_fork("/fork 3"), Some(ForkCommand::At(3)));
        // Padded argument is trimmed.
        assert_eq!(parse_fork("/fork  3"), Some(ForkCommand::At(3)));
        // Empty, zero, or non-numeric argument shows usage.
        assert_eq!(parse_fork("/fork "), Some(ForkCommand::Usage));
        assert_eq!(parse_fork("/fork   "), Some(ForkCommand::Usage));
        assert_eq!(parse_fork("/fork 0"), Some(ForkCommand::Usage));
        assert_eq!(parse_fork("/fork -1"), Some(ForkCommand::Usage));
        assert_eq!(parse_fork("/fork abc"), Some(ForkCommand::Usage));
        assert_eq!(parse_fork("/fork 3x"), Some(ForkCommand::Usage));
        // Non-fork input falls through to the normal prompt path.
        assert_eq!(parse_fork("/forkxxx"), None);
        assert_eq!(parse_fork("/forkx 3"), None);
        assert_eq!(parse_fork("/compact"), None);
        assert_eq!(parse_fork("/btw x"), None);
        assert_eq!(parse_fork("/rename x"), None);
        assert_eq!(parse_fork("hello"), None);
    }
}

#[cfg(test)]
mod model_tests {
    use super::{ModelCommand, parse_model};

    #[test]
    fn parse_model_commands() {
        assert_eq!(parse_model("/model"), Some(ModelCommand::Usage));
        assert_eq!(parse_model("/model "), Some(ModelCommand::Usage));
        assert_eq!(parse_model("/model   "), Some(ModelCommand::Usage));
        assert_eq!(
            parse_model("/model chatgpt/sol"),
            Some(ModelCommand::Switch("chatgpt/sol".to_string()))
        );
        assert_eq!(
            parse_model("/model   deepseek/flash  "),
            Some(ModelCommand::Switch("deepseek/flash".to_string()))
        );
        // Non-model input falls through to the normal prompt path.
        assert_eq!(parse_model("/modelxxx"), None);
        assert_eq!(parse_model("/modelx gpt"), None);
        assert_eq!(parse_model("/compact"), None);
        assert_eq!(parse_model("hello"), None);
    }
}
