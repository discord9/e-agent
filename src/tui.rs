use std::io;
use std::path::PathBuf;

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode};
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

use crate::agent::AgentEvent;
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
    let mut state = TuiState::default();
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
                    if state.task_detail.is_some() { state.handle_task_detail_key(key); continue; }
                    if state.show_tasks { match state.handle_tasks_panel_key(key) { TaskSelection::Attach(id) => attach_to_task(&mut state,id,&sessions,&sender), TaskSelection::OpenDetail(id) => state.open_task_detail(id), TaskSelection::None => { let _=state.handle_panel_key(key); } } continue; }
                    if state.attached.is_some() { if key.code==KeyCode::Esc { state.detach(); } else if is_scroll_key(key) { state.attached.as_mut().unwrap().state.handle_scroll(key); } else { let width=attached_input_width(terminal)?; state.handle_attached_key(key,width); } continue; }
                    let active = matches!(&*status.borrow(), SessionStatus::Busy | SessionStatus::Compacting);
                    if active && is_cancel(key) { handle.cancel(); continue; }
                    if !active && is_exit(key) { return Ok(None); }
                    if is_scroll_key(key) {
                        state.handle_scroll(key);
                        drain_ready_scroll_keys(&mut events, &mut state).await;
                        if state.older_pending {
                            state.load_older_history().await;
                        }
                        if state.newer_pending {
                            state.load_newer_history().await;
                        }
                    }
                    // TODO(btw): 前端 /btw 接线待后端函数就绪后补上。Web 端已先行
                    // 实现（src/ui/app.js sendPrompt 的 /btw 分支，POST
                    // /api/sessions/{id}/btw）。TUI 是本地进程，无法走 HTTP，必须调用
                    // 后端提供的函数（fork 主会话历史 → WaitForInput subagent → 立即
                    // 发问题）。待该函数合入后，在此加一个
                    // `else if prompt == "/btw" || prompt.starts_with("/btw ")` 分支：
                    // 裸 /btw 提示用法，`/btw <问题>` 调后端函数并把结果以 Notice
                    // 推入滚动条（参考 handle_rename 的模式）。当前不接，避免后端
                    // 未合入导致编译失败。
                    else if let Some(prompt)=state.handle_key(key) { if prompt=="/compact" { handle.compact(); } else if let Some(command)=parse_rename(&prompt) { handle_rename(command, &mut state).await; } else { if !state.session_title_set { set_terminal_title(&sanitize_title(&prompt)); state.session_title_set=true; } handle.prompt(prompt); } }
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
