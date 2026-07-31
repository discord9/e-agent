use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{CellDiffOption, CellWidth};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::{AgentEvent, preview};
use crate::delegate::Sessions;
use crate::runner::{SessionHandle as RunnerHandle, SessionResult, SessionStatus, SessionTask};

/// Events on the shared UI channel are tagged by session id: `0` is the
/// main agent, anything else is an attached background session. The TUI
/// routes them to the matching scrollback.
#[derive(Clone, Debug)]
struct UiEvent {
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
            LineKind::Thinking => Style::default()
                .fg(self.violet)
                .bg(self.background)
                .add_modifier(Modifier::DIM),
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

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    handle: RunnerHandle,
    labels: &InputLabels,
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
                    if is_scroll_key(key) { state.handle_scroll(key); drain_ready_scroll_keys(&mut events,&mut state).await; }
                    else if let Some(prompt)=state.handle_key(key) { if prompt=="/compact" { handle.compact(); } else { if !state.session_title_set { set_terminal_title(&sanitize_title(&prompt)); state.session_title_set=true; } handle.prompt(prompt); } }
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

/// Ratatui's diff skips cells covered by wide glyphs. Mark the visible
/// cell immediately after each wide glyph so it is always emitted, preventing
/// stale terminal cells from persisting after scrolling.
fn force_wide_trailing_cell_updates(
    buffer: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
) {
    for y in area.y..area.bottom() {
        let mut x = area.x;
        while x < area.right() {
            let width = buffer[(x, y)].cell_width();
            if width > 1 {
                let after = x.saturating_add(width);
                if after < area.right() {
                    buffer[(after, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
                }
                x = after;
            } else {
                x += 1;
            }
        }
    }
}

fn draw<'a, B: ratatui::backend::Backend>(
    terminal: &'a mut Terminal<B>,
    state: &mut TuiState,
) -> Result<ratatui::CompletedFrame<'a>, B::Error> {
    terminal.draw(|frame| {
        // Paint first, then every region below paints its own surface. This
        // keeps the alternate screen free of terminal-default holes.
        frame.render_widget(
            Block::default().style(SOLARIZED_LIGHT.screen_style()),
            frame.area(),
        );
        let inner_input_width = usize::from(frame.area().width.saturating_sub(2)).max(1);
        let input_rows = if let Some(attached) = &state.attached {
            attached.input.visual_rows(inner_input_width)
        } else {
            state.input.visual_rows(inner_input_width)
        };
        let input_height = (input_rows + 2)
            .min((usize::from(frame.area().height) / 3).max(3))
            .max(3) as u16;
        let queued = state
            .attached
            .as_ref()
            .map_or(&state.queued, |attached| &attached.state.queued);
        let queue_height = if queued.is_empty() {
            0
        } else {
            // One row per queued prompt, so every pending message is
            // visible (not just the head).
            queued.len() as u16
        };
        let running: Vec<crate::tools::BackgroundTaskInfo> = state
            .background
            .as_ref()
            .map(|background| background.running())
            .unwrap_or_default();
        // Detail view: full-screen over the output/panel area. The spool Arc
        // keeps the page alive after the task leaves the registry.
        if let Some(detail) = &mut state.task_detail {
            if !running.iter().any(|task| task.id == detail.id) {
                detail.finished = true;
            }
            let area = frame.area();
            render_task_detail(frame, detail, area);
            force_wide_trailing_cell_updates(frame.buffer_mut(), area);
            return;
        }
        // Clamp cursor so it never points past the end (tasks may have
        // completed since the last render).
        state.task_cursor = state.task_cursor.min(running.len().saturating_sub(1));
        // Panel open: full list. Panel closed but tasks running: a one-line
        // hint so background work never goes completely unnoticed.
        const OUTPUT_LINES: usize = 1;
        let selected_output = if state.show_tasks {
            running
                .get(state.task_cursor)
                .map(|task| String::from_utf8_lossy(&task.output).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let output_line_count = selected_output.lines().count().min(OUTPUT_LINES);
        let tasks_height = if state.show_tasks {
            // border (2) + header (1) + one row per task + output tail
            (running.len() as u16 + 3 + output_line_count as u16).max(3)
        } else {
            u16::from(!running.is_empty())
        };
        let [output, queue_bar, tasks_bar, input] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(queue_height),
            Constraint::Length(tasks_height),
            Constraint::Length(input_height),
        ])
        .areas(frame.area());
        if queue_height > 0 {
            let queued_lines: Vec<Line> = queued
                .iter()
                .enumerate()
                .map(|(index, prompt)| {
                    Line::styled(
                        format!("queued {}: {}", index + 1, preview(prompt, 60)),
                        SOLARIZED_LIGHT.queue_style(),
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(queued_lines).style(SOLARIZED_LIGHT.queue_style()),
                queue_bar,
            );
        }
        if tasks_height > 0 && !state.show_tasks {
            // Collapsed hint.
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!(
                        "▸ {} background task(s) running (F2 to view)",
                        running.len()
                    ),
                    Style::default()
                        .fg(SOLARIZED_LIGHT.yellow)
                        .bg(SOLARIZED_LIGHT.panel)
                        .add_modifier(Modifier::DIM),
                ))
                .style(SOLARIZED_LIGHT.panel_style()),
                tasks_bar,
            );
        }
        if tasks_height > 0 && state.show_tasks {
            let mut lines = vec![Line::styled(
                if running.is_empty() {
                    "no background tasks running".to_owned()
                } else {
                    format!("{} background task(s) running", running.len())
                },
                Style::default()
                    .fg(SOLARIZED_LIGHT.text)
                    .bg(SOLARIZED_LIGHT.panel)
                    .add_modifier(Modifier::BOLD),
            )];
            for (index, task) in running.iter().enumerate() {
                let mut style = Style::default()
                    .fg(SOLARIZED_LIGHT.muted)
                    .bg(SOLARIZED_LIGHT.panel);
                if index == state.task_cursor {
                    style = Style::default()
                        .fg(SOLARIZED_LIGHT.text)
                        .bg(SOLARIZED_LIGHT.selection)
                        .add_modifier(Modifier::BOLD);
                }
                let row = format_task_label(task, "", 40);
                lines.push(Line::styled(row, style));
            }
            if !selected_output.is_empty() {
                let skip = selected_output.lines().count().saturating_sub(OUTPUT_LINES);
                for line in selected_output.lines().skip(skip) {
                    lines.push(Line::styled(
                        format!("  │ {}", preview(line, 120)),
                        Style::default()
                            .fg(SOLARIZED_LIGHT.muted)
                            .bg(SOLARIZED_LIGHT.panel)
                            .add_modifier(Modifier::DIM),
                    ));
                }
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .style(SOLARIZED_LIGHT.panel_style())
                    .block(SOLARIZED_LIGHT.block("tasks (F2 hide  ↑↓ attach  Ctrl-C/x cancel)")),
                tasks_bar,
            );
        }
        let inner_width = usize::from(output.width).max(1);
        // The scrollback rendered depends on whether we are attached to
        // a session view; the rendering pipeline is identical.
        let scroll_state: &mut TuiState = match &mut state.attached {
            Some(attached) => &mut attached.state,
            None => state,
        };
        // Store the output width for scroll-accounting in handle_scroll.
        let old_width = scroll_state.inner_width;
        scroll_state.inner_width = inner_width;
        scroll_state.output_height = usize::from(output.height);
        // A terminal width change re-wraps every line. In follow mode
        // anchor_tail below re-derives the window from the tail; a
        // scrolled-up (non-follow) window keeps a local_offset computed
        // against the old width, which would drift the viewport top
        // (shrink: jump to earlier content; grow: jump forward). Re-anchor
        // it at the new width instead.
        if !scroll_state.window.follow_bottom && old_width > 0 && old_width != inner_width {
            reanchor_window_on_resize(
                &scroll_state.lines,
                &mut scroll_state.window,
                old_width,
                inner_width,
                usize::from(output.height),
            );
        }
        // When following, anchor the window at the tail.
        if scroll_state.window.follow_bottom {
            scroll_state.window.anchor_tail(
                &scroll_state.lines,
                inner_width,
                usize::from(output.height),
            );
        }
        // Render a bounded local tail. The cursor captured when scrolling
        // away from a streaming tail keeps later deltas out of this view.
        let local_lines = local_window_lines(&scroll_state.lines, &scroll_state.window);
        let visual = render_bounded_window(&local_lines, inner_width);
        let total_rows = visual.len();
        let height = usize::from(output.height);
        // Clamp local_offset in follow mode.
        if scroll_state.window.follow_bottom {
            scroll_state.window.local_offset = total_rows.saturating_sub(height);
        }
        scroll_state.window.local_offset = scroll_state
            .window
            .local_offset
            .min(total_rows.saturating_sub(height).max(0));
        // When following with fewer rows than the viewport, bottom-align
        // so the last visual row touches the input boundary.
        let render_top = if scroll_state.window.follow_bottom && total_rows < height {
            output.bottom() - total_rows as u16
        } else {
            output.y
        };
        {
            let buf = frame.buffer_mut();
            buf.set_style(output, SOLARIZED_LIGHT.screen_style());
            let scroll_offset = scroll_state.window.local_offset;
            for (row_idx, line) in visual.iter().enumerate().skip(scroll_offset) {
                let y = render_top + (row_idx - scroll_offset) as u16;
                if y >= output.bottom() {
                    break;
                }
                buf.set_line(output.x, y, line, output.width);
            }
            // Restore glyph style on trailing cells set_stringn reset so the
            // completed buffer carries the correct background (this, plus
            // ratatui's built-in wide→narrow trailing emission, is what
            // prevents stale terminal cells).
            for y in output.y..output.bottom() {
                let mut x = output.x;
                while x < output.right() {
                    let width = buf[(x, y)].cell_width();
                    if width > 1 {
                        let glyph_style = buf[(x, y)].style();
                        let last = (x + width).min(output.right());
                        for cx in x + 1..last {
                            buf[(cx, y)].set_style(glyph_style);
                        }
                        x += width;
                    } else {
                        x += 1;
                    }
                }
            }
        }
        if let Some(attached) = &mut state.attached {
            let status: String = attached.title_status();
            let hint = match &*attached.status.borrow() {
                SessionStatus::Busy | SessionStatus::Compacting => {
                    "Esc detach  Enter steer  Ctrl-C interrupt"
                }
                SessionStatus::Idle => {
                    if attached.finished {
                        "Esc detach"
                    } else {
                        "Esc detach  Enter steer"
                    }
                }
                SessionStatus::Finished(_) => "Esc detach",
            };
            let block = SOLARIZED_LIGHT
                .block(format!(
                    "{} — subagent #{}: {} — {hint}",
                    status,
                    attached.id,
                    preview(&attached.label, 40),
                ))
                .title_top(
                    Line::from(attached.state.session_id.clone())
                        .alignment(Alignment::Right)
                        .style(Style::default().fg(SOLARIZED_LIGHT.muted)),
                )
                .title_bottom({
                    let spans = model_role_spans(
                        &attached.state.model_name,
                        attached.state.role_name.as_deref(),
                    );
                    Line::from(spans).alignment(Alignment::Left)
                });
            let (cursor_row, cursor_col) = attached.input.wrapped_cursor(inner_input_width);
            let inner_input_height = usize::from(input.height.saturating_sub(2));
            attached.input_scroll = cursor_row.saturating_sub(inner_input_height.saturating_sub(1));
            let input_lines: Vec<Line> = hard_wrap(&attached.input.text, inner_input_width)
                .into_iter()
                .map(|line| Line::styled(line, SOLARIZED_LIGHT.panel_style()))
                .collect();
            let inner = block.inner(input);
            frame.render_widget(block, input);
            {
                let buf = frame.buffer_mut();
                buf.set_style(inner, SOLARIZED_LIGHT.panel_style());
                let scroll_offset = attached.input_scroll;
                for (row_idx, line_text) in input_lines.iter().enumerate().skip(scroll_offset) {
                    let y = inner.y + (row_idx - scroll_offset) as u16;
                    if y >= inner.bottom() {
                        break;
                    }
                    buf.set_line(inner.x, y, line_text, inner.width);
                }
                // Restore glyph style on trailing cells set_stringn reset.
                for y in inner.y..inner.bottom() {
                    let mut x = inner.x;
                    while x < inner.right() {
                        let width = buf[(x, y)].cell_width();
                        if width > 1 {
                            let glyph_style = buf[(x, y)].style();
                            let last = (x + width).min(inner.right());
                            for cx in x + 1..last {
                                buf[(cx, y)].set_style(glyph_style);
                            }
                            x += width;
                        } else {
                            x += 1;
                        }
                    }
                }
            }
            {
                let (usage, fg) = cwd_usage_text(
                    &attached.state.cwd,
                    attached.state.tokens_context,
                    attached.state.context_window,
                );
                let width = UnicodeWidthStr::width(usage.as_str()) as u16 + 1;
                if input.width > width + 1 {
                    let area = ratatui::layout::Rect {
                        x: input.right().saturating_sub(width + 1),
                        y: input.bottom() - 1,
                        width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(usage)
                            .style(Style::default().fg(fg).bg(SOLARIZED_LIGHT.panel)),
                        area,
                    );
                }
            }
            frame.set_cursor_position((
                input.x + 1 + (cursor_col as u16).min(input.width.saturating_sub(2)),
                input.y
                    + 1
                    + ((cursor_row - attached.input_scroll) as u16)
                        .min(input.height.saturating_sub(2)),
            ));
            let area = frame.area();
            force_wide_trailing_cell_updates(frame.buffer_mut(), area);
            return;
        }
        // Input-box chrome: top-left = status, top-right = session id,
        // bottom-left = model (agent role goes here later), bottom-right =
        // cwd + context tokens.
        let title = state.busy.map_or(String::new(), BusyState::title);
        let input_block = SOLARIZED_LIGHT
            .block(title)
            .title_top(
                Line::from(state.session_id.clone())
                    .alignment(Alignment::Right)
                    .style(Style::default().fg(SOLARIZED_LIGHT.muted)),
            )
            .title_bottom({
                let spans = model_role_spans(&state.model_name, state.role_name.as_deref());
                Line::from(spans).alignment(Alignment::Left)
            });
        // cwd is always shown; the context-token count appears once the
        // first turn has reported usage.
        let (usage, fg) = cwd_usage_text(&state.cwd, state.tokens_context, state.context_window);
        let (cursor_row, cursor_col) = state.input.wrapped_cursor(inner_input_width);
        let inner_input_height = usize::from(input.height.saturating_sub(2));
        let input_scroll = cursor_row.saturating_sub(inner_input_height.saturating_sub(1));
        let input_lines: Vec<Line> = hard_wrap(&state.input.text, inner_input_width)
            .into_iter()
            .map(|line| Line::styled(line, SOLARIZED_LIGHT.panel_style()))
            .collect();
        let inner = input_block.inner(input);
        frame.render_widget(input_block, input);
        {
            let buf = frame.buffer_mut();
            buf.set_style(inner, SOLARIZED_LIGHT.panel_style());
            let scroll_offset = input_scroll;
            for (row_idx, line_text) in input_lines.iter().enumerate().skip(scroll_offset) {
                let y = inner.y + (row_idx - scroll_offset) as u16;
                if y >= inner.bottom() {
                    break;
                }
                buf.set_line(inner.x, y, line_text, inner.width);
            }
            // Restore glyph style on trailing cells set_stringn reset.
            for y in inner.y..inner.bottom() {
                let mut x = inner.x;
                while x < inner.right() {
                    let width = buf[(x, y)].cell_width();
                    if width > 1 {
                        let glyph_style = buf[(x, y)].style();
                        let last = (x + width).min(inner.right());
                        for cx in x + 1..last {
                            buf[(cx, y)].set_style(glyph_style);
                        }
                        x += width;
                    } else {
                        x += 1;
                    }
                }
            }
        }
        {
            let width = UnicodeWidthStr::width(usage.as_str()) as u16 + 1;
            if input.width > width + 1 {
                let area = ratatui::layout::Rect {
                    x: input.right().saturating_sub(width + 1),
                    y: input.bottom() - 1,
                    width,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(usage).style(Style::default().fg(fg).bg(SOLARIZED_LIGHT.panel)),
                    area,
                );
            }
        }
        frame.set_cursor_position((
            input.x + 1 + (cursor_col as u16).min(input.width.saturating_sub(2)),
            input.y + 1 + ((cursor_row - input_scroll) as u16).min(input.height.saturating_sub(2)),
        ));
        let area = frame.area();
        force_wide_trailing_cell_updates(frame.buffer_mut(), area);
    })
}

/// Keep wrapping text-only so scroll accounting is unchanged, then add the
/// small amount of semantic paint needed for the row being rendered.
fn styled_scroll_line(row: &str, kind: LineKind) -> Line<'static> {
    let style = SOLARIZED_LIGHT.line_style(kind);
    let numbered_diff = matches!(kind, LineKind::Added | LineKind::Removed)
        && row.len() >= 7
        && matches!(row.as_bytes().first(), Some(b'+' | b'-'))
        && row.as_bytes()[1] == b' '
        && row.as_bytes()[2..6]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b' ')
        && row.as_bytes()[6] == b' ';
    if numbered_diff {
        let number_style = SOLARIZED_LIGHT
            .diff_line_number_style(kind)
            .expect("numbered diff has an added or removed kind");
        Line::from(vec![
            Span::styled(row[..7].to_owned(), number_style),
            Span::styled(row[7..].to_owned(), style),
        ])
    } else {
        Line::styled(row.to_owned(), style)
    }
}

fn format_tokens(count: u64) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// Build the bottom-left title spans for the input block: model name (violet)
/// with an optional ` role` suffix (muted). Shared by the main and attached
/// views. Returns an empty vec when `model_name` is empty.
fn model_role_spans(model_name: &str, role_name: Option<&str>) -> Vec<Span<'static>> {
    if model_name.is_empty() {
        return Vec::new();
    }
    let mut spans = vec![Span::styled(
        model_name.to_owned(),
        Style::default().fg(SOLARIZED_LIGHT.violet),
    )];
    if let Some(role) = role_name {
        spans.push(Span::styled(
            format!(" {role}"),
            Style::default().fg(SOLARIZED_LIGHT.muted),
        ));
    }
    spans
}

/// Format the bottom-right overlay text: cwd with optional context-token
/// count and optional window/percentage display. Returns the text and the
/// foreground color (orange normally, red when >= 80% of context window).
/// Shared by the main and attached views. A cwd under `$HOME` is shortened
/// to `~/…` to keep the overlay narrow.
fn cwd_usage_text(cwd: &str, tokens_context: u64, context_window: Option<u64>) -> (String, Color) {
    let cwd = shorten_home(cwd);
    if tokens_context == 0 {
        return (cwd.into_owned(), SOLARIZED_LIGHT.orange);
    }
    let (text, pct_high) = match context_window {
        Some(window) if window > 0 => {
            let pct = (tokens_context as f64 / window as f64 * 100.0).round() as u64;
            let high = (tokens_context as u128) * 100 >= (window as u128) * 80;
            (
                format!(
                    "{} {} {}%",
                    cwd,
                    format_tokens(tokens_context),
                    pct.min(100)
                ),
                high,
            )
        }
        _ => (format!("{} {}", cwd, format_tokens(tokens_context)), false),
    };
    let fg = if pct_high {
        SOLARIZED_LIGHT.red
    } else {
        SOLARIZED_LIGHT.orange
    };
    (text, fg)
}

/// Replace a leading `$HOME` with `~` (e.g. `/home/alice/work` → `~/work`).
fn shorten_home(cwd: &str) -> std::borrow::Cow<'_, str> {
    match std::env::var_os("HOME") {
        Some(home) => {
            let home = home.to_string_lossy();
            if cwd == home.as_ref() {
                "~".into()
            } else if let Some(rest) = cwd.strip_prefix(home.as_ref()) {
                if let Some(rest) = rest.strip_prefix('/') {
                    format!("~/{rest}").into()
                } else {
                    cwd.into()
                }
            } else {
                cwd.into()
            }
        }
        None => cwd.into(),
    }
}

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
fn truncate_background_output(output: &str) -> String {
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
    // then refine once.
    let est_marker_len = 64usize;
    let rough_omitted = total_orig_chars.saturating_sub(head_chars + tail_chars + est_marker_len);
    let marker =
        format!("\n\u{2026} ({omitted_lines} lines omitted, {rough_omitted} chars omitted)");
    // Refine with actual marker length.
    let actual_omitted =
        total_orig_chars.saturating_sub(head_chars + tail_chars + marker.chars().count());
    let marker =
        format!("\n\u{2026} ({omitted_lines} lines omitted, {actual_omitted} chars omitted)");

    let mut result = elided_head.join("\n");
    result.push_str(&marker);
    for line in &elided_tail {
        result.push('\n');
        result.push_str(line);
    }
    result
}

fn is_scroll_key(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
    )
}

/// Apply a consecutive scroll-key burst until the stream stays quiet briefly.
/// Peeking preserves the first unrelated event for the normal event loop.
async fn drain_ready_scroll_keys<S>(
    events: &mut futures_util::stream::Peekable<S>,
    state: &mut TuiState,
) where
    S: futures_util::Stream<Item = io::Result<Event>> + Unpin,
{
    loop {
        let Ok(next) = tokio::time::timeout(
            Duration::from_millis(4),
            std::pin::Pin::new(&mut *events).peek(),
        )
        .await
        else {
            break;
        };
        let Some(Ok(Event::Key(key))) = next else {
            break;
        };
        if key.kind != crossterm::event::KeyEventKind::Press || !is_scroll_key(*key) {
            break;
        }
        let Some(Ok(Event::Key(key))) = events.next().await else {
            unreachable!("peeked scroll key must remain available")
        };
        state.handle_scroll(key);
    }
}

/// Keys that exit the app from the idle prompt.
fn is_exit(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc || is_cancel(key)
}

/// Keys that cancel an in-flight turn. Esc is intentionally NOT here: it is
/// reserved for "leave the current view" (detach from a subagent, close the
/// tasks panel) so its meaning is consistent everywhere.
fn is_cancel(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Safe terminal-title string: strip ASCII/C1 controls, collapse whitespace,
/// middle-ellipsis preview at 40 Unicode chars.
fn sanitize_title(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut ws = false;
    for c in raw.chars() {
        let code = c as u32;
        if code <= 0x08
            || (0x0E..=0x1F).contains(&code)
            || code == 0x7F
            || (0x80..=0x9F).contains(&code)
        {
            continue;
        }
        if c.is_whitespace() {
            if !ws {
                out.push(' ');
                ws = true;
            }
        } else {
            out.push(c);
            ws = false;
        }
    }
    crate::agent::preview(out.trim(), 40)
}

/// First `Message::User` from history, skipping Notice/Compaction/BackgroundCompletion.
fn set_terminal_title(title: &str) {
    let _ = execute!(io::stdout(), SetTitle(format!("e-agent — {title}")));
}

/// Hard-wrap text at `width` terminal cells (char-boundary safe, CJK-aware).
/// Rendering and scroll accounting share this function so bottom-following
/// is exact instead of estimated.
fn hard_wrap(text: &str, width: usize) -> Vec<&str> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for logical in text.split('\n') {
        let mut start = 0;
        let mut used = 0;
        for (index, character) in logical.char_indices() {
            let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used > 0 && used + char_width > width {
                rows.push(&logical[start..index]);
                start = index;
                used = 0;
            }
            used += char_width;
        }
        rows.push(&logical[start..]);
    }
    rows
}

#[cfg(test)]
fn wrapped_rows(lines: &[DisplayLine], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines.iter().map(|line| line_visual_rows(line, width)).sum()
}

#[derive(Default)]
struct TuiState {
    /// This session's id, shown in the input border (so it can be resumed
    /// later with --session).
    session_id: String,
    model_name: String,
    /// Agent role name displayed next to the model name; `None` when no role
    /// template exists.
    role_name: Option<String>,
    cwd: String,
    input: InputBuffer,
    lines: Vec<DisplayLine>,
    /// Bounded local rendering window.
    window: ScrollWindow,
    /// Terminal width (in cells) for the output area, updated on every draw.
    inner_width: usize,
    /// Terminal height (in rows) for the output area, updated on every draw.
    output_height: usize,
    busy: Option<BusyState>,
    streamed: bool,
    active_lane: Option<ActiveStreamLane>,
    tokens_context: u64,
    /// Configured context window (token count) from the model profile, used
    /// to display a usage percentage and trigger the red style at >= 80%.
    context_window: Option<u64>,
    /// Transient projection of prompts accepted while a turn is in flight.
    /// Consumption removes one item from the front; this is never persisted.
    queued: std::collections::VecDeque<String>,
    /// Arguments of an in-flight edit_file call, rendered as a numbered diff
    /// when its result (which carries the line number) arrives.
    pending_edit: Option<(String, String, String)>,
    /// Shared running-task registry, for the tasks panel.
    background: Option<crate::tools::BackgroundTasks>,
    /// Session store for background-task record bookkeeping when cancelling
    /// from the tasks panel; `None` (tests, `Default`) falls back to the
    /// JSONL record file directly.
    store: Option<crate::session_store::SessionStore>,
    /// Probe for whether a background task has an attachable session
    /// (wired to the session registry by the runner).
    attachable: Option<Box<dyn Fn(u64) -> bool + Send>>,
    /// Whether the background-tasks panel is visible.
    show_tasks: bool,
    /// Cursor (index into the running-task list) for attach selection.
    task_cursor: usize,
    /// Attached session view; when set, draw renders this instead of the
    /// main scrollback and Esc detaches instead of cancelling the turn.
    attached: Option<Box<AttachedView>>,
    /// Full-output detail view for a selected background bash task; when
    /// set, draw renders it full-screen and keys route to it first.
    task_detail: Option<TaskDetail>,
    /// Whether the terminal title has been set to a user-derived value.
    /// Once true, subsequent prompts never overwrite the title.
    session_title_set: bool,
    /// Steering input preserved per background-task id across detach /
    /// re-attach and panel-driven task switches, so browsing the tasks
    /// panel never discards a half-typed prompt.
    stashed_input: std::collections::HashMap<u64, String>,
}

/// A live view into a running background session: its own scrollback
/// rebuilt from the event stream, rendered with the same pipeline as the
/// main view. While attached you can steer the session through its handle:
/// Enter queues a prompt for its next turn, Ctrl-C cancels its in-flight
/// turn.
struct AttachedView {
    id: u64,
    label: String,
    state: TuiState,
    /// The live-event bridge for this attachment. Dropping the view aborts it
    /// so re-attaching cannot forward each delta twice.
    bridge: Option<tokio::task::AbortHandle>,
    /// The session seam: snapshot/subscribe/send_input/cancel.
    handle: RunnerHandle,
    /// Input buffer for steering prompts.
    input: InputBuffer,
    /// Vertical scroll of the steering input (multi-line).
    input_scroll: usize,
    /// Live status of the attached session, kept in sync with the runner:
    /// drives the title text and the finished flag so the view reflects the
    /// runner's real state (Busy/Compacting/Idle/Finished) instead of
    /// lagging behind until the parent's BackgroundCompleted arrives.
    status: tokio::sync::watch::Receiver<SessionStatus>,
    /// Set once the session's completion event has arrived (view becomes a
    /// static record; further events are impossible).
    finished: bool,
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
struct TaskDetail {
    id: u64,
    label: String,
    /// Full untruncated command for bash tasks (`None` for delegate/open
    /// without a snapshot). Rendered as a fixed banner under the header so
    /// the complete command is visible even though the panel label and the
    /// header stay preview-truncated.
    command: Option<String>,
    /// Keeps the full output alive after the task completes (the registry
    /// drops its reference when the task leaves `running()`).
    spool: Arc<crate::tools::TaskSpool>,
    finished: bool,
    /// 0-based spool line number of `lines[0]`.
    base_line: usize,
    /// Current page of spool lines.
    lines: Vec<DisplayLine>,
    /// Page-internal window (source_* index into `lines`).
    window: ScrollWindow,
    /// Spool line count observed at the last reload (open or tail slide).
    /// While following, draw reloads the tail only when the spool has
    /// grown past this — opening shows the head page, and new output
    /// slides the view to the live tail (like `tail -f` from the top).
    last_seen_lines: usize,
    /// Terminal width the page was last rendered at, used to detect a
    /// resize so a scrolled-up (non-follow) page can be re-anchored at
    /// the new width. `0` means "no frame rendered yet".
    rendered_width: usize,
}

impl TaskDetail {
    fn new(id: u64, label: String, spool: Arc<crate::tools::TaskSpool>, finished: bool) -> Self {
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
        }
    }

    /// Fetch spool lines `[base, base+max_lines)` into the page.
    /// `anchor_bottom` positions the viewport at the page's last rows
    /// (used when paging up into the previous page). Does not touch
    /// `follow_bottom`; callers set it (load_tail enables follow).
    fn load_page(
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
                render_bounded_window(&local_window_lines(&self.lines, &self.window), width).len();
            self.window.local_offset = total.saturating_sub(height);
        }
    }

    /// Anchor the page at the spool tail and enable follow (draw reloads
    /// the tail page every frame while `follow_bottom` stays true).
    fn load_tail(&mut self, width: usize, height: usize) {
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
    fn load_head(&mut self, width: usize, height: usize) {
        let capacity = height.max(1);
        self.load_page(0, capacity, false, width, height);
        self.window.follow_bottom = true;
        self.window.frozen_tail_cursor = None;
        self.window.frozen_source_end = 0;
    }

    fn step_up(&mut self, width: usize, height: usize) {
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

    fn step_down(&mut self, width: usize, height: usize) {
        if self.window.follow_bottom {
            return;
        }
        let total =
            render_bounded_window(&local_window_lines(&self.lines, &self.window), width).len();
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

    fn page_up(&mut self, width: usize, height: usize) {
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

    fn page_down(&mut self, width: usize, height: usize) {
        if self.window.follow_bottom {
            return;
        }
        let total =
            render_bounded_window(&local_window_lines(&self.lines, &self.window), width).len();
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
    fn title_status(&self) -> String {
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
    fn edit_input(input: &mut InputBuffer, key: KeyEvent, _width: usize) {
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

#[derive(Clone)]
struct DisplayLine {
    text: String,
    kind: LineKind,
}

/// Rendering never materializes more than this much scrollback. Limits are
/// deliberately independent: a very long source line cannot evade the byte
/// cap, and many short lines cannot evade either the line or row cap.
const MAX_RENDER_VISUAL_ROWS: usize = 512;
const MAX_RENDER_SOURCE_LINES: usize = 256;
const MAX_RENDER_BYTES: usize = 64 * 1024;

/// Round a byte offset down to a UTF-8 boundary. `limit` is normally already
/// a boundary (it comes from `String::len()`), but this also makes truncation
/// of a frozen cursor safe if the source was replaced while detached.
fn utf8_floor_boundary(text: &str, limit: usize) -> usize {
    let mut index = limit.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Round a byte offset up to a UTF-8 boundary. This prevents a tail slice
/// from exceeding its byte budget when the budget lands in a multibyte char.
fn utf8_ceil_boundary(text: &str, limit: usize) -> usize {
    let mut index = limit.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Return at most `bytes` of the tail ending at `end`, never splitting a
/// Unicode scalar value. Keeping the tail is what makes a followed giant
/// streaming line useful without allocating or wrapping the whole line.
fn utf8_tail(text: &str, end: usize, bytes: usize) -> &str {
    let end = utf8_floor_boundary(text, end);
    let start = utf8_ceil_boundary(text, end.saturating_sub(bytes));
    &text[start..end]
}

/// Copy only the bounded part of the current source window. A frozen cursor
/// is an offset into the final source line, rather than a cloned tail string:
/// this is both bounded and stable while that line receives streaming deltas.
fn local_window_lines(lines: &[DisplayLine], window: &ScrollWindow) -> Vec<DisplayLine> {
    let end = window.source_end.min(lines.len());
    let start = window.source_start.min(end);
    let mut remaining = MAX_RENDER_BYTES;
    let mut local = Vec::new();
    for index in (start..end).rev().take(MAX_RENDER_SOURCE_LINES) {
        if remaining == 0 {
            break;
        }
        let line = &lines[index];
        let cursor = if index + 1 == end {
            window.frozen_tail_cursor.unwrap_or(line.text.len())
        } else {
            line.text.len()
        };
        let text = utf8_tail(&line.text, cursor, remaining);
        remaining = remaining.saturating_sub(text.len());
        local.push(DisplayLine {
            text: text.to_owned(),
            kind: line.kind,
        });
    }
    local.reverse();
    local
}

/// Render the local copy and retain its most recent visual rows. The local
/// copy is byte-bounded before markdown parsing; trimming after wrapping also
/// bounds the actual ratatui history passed to the frame.
fn render_bounded_window(lines: &[DisplayLine], width: usize) -> Vec<ratatui::text::Line<'static>> {
    let mut rows = render_window(lines, 0, lines.len(), width);
    if rows.len() > MAX_RENDER_VISUAL_ROWS {
        rows.drain(..rows.len() - MAX_RENDER_VISUAL_ROWS);
    }
    rows
}

/// Full-screen detail view for a background bash task: a bordered header
/// (#id: label — status — N lines · X MiB (truncated) — key hints), a fixed
/// banner with the FULL untruncated command (hard-wrapped, Dim), then the
/// current spool page rendered as plain Dim text through the bounded window
/// pipeline (no markdown). While `follow_bottom` the tail page is re-fetched
/// only when the spool has grown since the last reload, so a running task's
/// output keeps streaming in while a paused one keeps its current page.
fn render_task_detail(
    frame: &mut ratatui::Frame,
    detail: &mut TaskDetail,
    area: ratatui::layout::Rect,
) {
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let height = usize::from(area.height.saturating_sub(2)).max(1);
    let (bytes, lines, truncated) = (
        detail.spool.len(),
        detail.spool.line_count(),
        detail.spool.truncated(),
    );
    // Fixed banner under the header: the full untruncated command wrapped
    // to the inner width. Reserves rows so the spool output never covers
    // it; a pathological command may fill the whole viewport, in which
    // case there is simply no room left for output.
    let banner: Vec<ratatui::text::Line<'static>> = detail
        .command
        .as_deref()
        .filter(|command| !command.is_empty())
        .map(|command| {
            hard_wrap(command, width)
                .into_iter()
                .map(|row| {
                    ratatui::text::Line::styled(
                        row.to_owned(),
                        SOLARIZED_LIGHT.line_style(LineKind::Dim),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let banner_rows = banner.len().min(height);
    let content_height = height.saturating_sub(banner_rows);
    // Follow slides to the tail only when the spool has grown since the
    // last reload: opening shows the head page, a paused task keeps the
    // current page, and new output pulls the view to the live tail.
    if detail.window.follow_bottom && lines > detail.last_seen_lines {
        detail.last_seen_lines = lines;
        detail.load_tail(width, height);
    }
    // A terminal width change re-wraps the page; a scrolled-up
    // (non-follow) window's local_offset is stale, so re-anchor it at the
    // new width (base_line is untouched). Follow mode is handled above
    // (load_tail on spool growth) and by the next page reload.
    let old_width = detail.rendered_width;
    detail.rendered_width = width;
    if !detail.window.follow_bottom && old_width > 0 && old_width != width {
        reanchor_window_on_resize(&detail.lines, &mut detail.window, old_width, width, height);
    }
    let status = if detail.finished {
        "finished"
    } else {
        "running"
    };
    let header = format!(
        "#{}: {} — {} — {} lines · {:.1} MiB{} — ↑↓ scroll  PgUp/PgDn  Home/End  Esc close  F2 close panel  Ctrl-C/x cancel",
        detail.id,
        crate::agent::preview(&detail.label, 60),
        status,
        lines,
        bytes as f64 / (1024.0 * 1024.0),
        if truncated { " (truncated)" } else { "" },
    );
    let block = SOLARIZED_LIGHT.block(header);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let local = local_window_lines(&detail.lines, &detail.window);
    let visual = render_bounded_window(&local, width);
    let total_rows = visual.len();
    if detail.window.follow_bottom {
        detail.window.local_offset = total_rows.saturating_sub(content_height);
    }
    detail.window.local_offset = detail
        .window
        .local_offset
        .min(total_rows.saturating_sub(content_height).max(0));
    {
        let buf = frame.buffer_mut();
        buf.set_style(inner, SOLARIZED_LIGHT.screen_style());
        // Full-command banner first; it is fixed (not scrollable).
        for (row_idx, line) in banner.iter().enumerate() {
            let y = inner.y + row_idx as u16;
            if y >= inner.bottom() {
                break;
            }
            buf.set_line(inner.x, y, line, inner.width);
        }
        let scroll_offset = detail.window.local_offset;
        let content_top = inner.y + banner_rows as u16;
        // When following with fewer rows than the content viewport,
        // bottom-align like the main scrollback so the tail touches the
        // bottom border (below the fixed command banner).
        let render_top = if detail.window.follow_bottom && total_rows < content_height {
            inner
                .bottom()
                .saturating_sub(total_rows as u16)
                .max(content_top)
        } else {
            content_top
        };
        for (row_idx, line) in visual.iter().enumerate().skip(scroll_offset) {
            let y = render_top + (row_idx - scroll_offset) as u16;
            if y >= inner.bottom() {
                break;
            }
            buf.set_line(inner.x, y, line, inner.width);
        }
        // Restore glyph style on trailing cells set_stringn reset (same as
        // the scrollback path, so wide glyphs keep their style).
        for y in inner.y..inner.bottom() {
            let mut x = inner.x;
            while x < inner.right() {
                let width = buf[(x, y)].cell_width();
                if width > 1 {
                    let glyph_style = buf[(x, y)].style();
                    let last = (x + width).min(inner.right());
                    for cx in x + 1..last {
                        buf[(cx, y)].set_style(glyph_style);
                    }
                    x += width;
                } else {
                    x += 1;
                }
            }
        }
    }
}

/// Bounded local rendering window over a sub-range of the scrollback's
/// `DisplayLine` entries.
///
/// Instead of rendering the full history to compute a global scroll offset,
/// we maintain a window over `lines[source_start .. source_end]`, produce
/// visual rows from that range, and track a `local_offset` (visual-row
/// index) within that window.
///
/// While `follow_bottom` is true the window anchors at the tail and grows
/// with appended content. When the user scrolls away from the bottom, the
/// window becomes a stable snapshot — appended events and streaming changes
/// outside the window do not mutate the visible viewport.
struct ScrollWindow {
    /// Index into `TuiState::lines` for the first DisplayLine in the window.
    source_start: usize,
    /// Index (exclusive) into `TuiState::lines` for the end of the window.
    source_end: usize,
    /// Visual-row offset within the rendered window. The first visual row
    /// shown at the viewport top is `rendered[local_offset]`.
    local_offset: usize,
    /// Whether the viewport auto-follows the tail.
    follow_bottom: bool,
    /// Byte cursor in the last line captured when freezing during active
    /// streaming. Rendering stops at this UTF-8-safe cursor, so appended
    /// deltas cannot mutate the frozen viewport without cloning that line.
    frozen_tail_cursor: Option<usize>,
    /// The source_end value at the moment the snapshot was taken; used to
    /// detect when the user has scrolled past the frozen range.
    frozen_source_end: usize,
}

impl Default for ScrollWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollWindow {
    fn new() -> Self {
        Self {
            source_start: 0,
            source_end: 0,
            local_offset: 0,
            follow_bottom: true,
            frozen_tail_cursor: None,
            frozen_source_end: 0,
        }
    }

    /// Populate the window from a tail-anchored range of `lines`, choosing
    /// `source_start` by walking backward far enough to fill `height` visual
    /// rows at `width`, without considering more than the bounded history
    /// limits. After this call the window is ready for rendering and
    /// `local_offset` points to the bottom (follow mode).
    /// Clears any frozen cursor so follow shows the live tail.
    fn anchor_tail(&mut self, lines: &[DisplayLine], width: usize, height: usize) {
        let total = lines.len();
        self.follow_bottom = true;
        self.frozen_tail_cursor = None;
        self.frozen_source_end = 0;
        self.source_end = total;
        // Walk backward counting visual rows until we have enough.
        // Include the first preceding DisplayLine that crosses the
        // viewport-height boundary so a giant assistant block is not
        // excluded entirely when followed by a short user line.
        self.source_start = total;
        let mut accumulated = 0usize;
        while self.source_start > 0
            && accumulated < height.min(MAX_RENDER_VISUAL_ROWS)
            && total - self.source_start < MAX_RENDER_SOURCE_LINES
        {
            self.source_start -= 1;
            accumulated =
                accumulated.saturating_add(line_visual_rows(&lines[self.source_start], width));
        }
        self.local_offset = 0; // will be set after rendering
    }
}

/// Replay the previous frame at the old width and find which source
/// line held the viewport-top visual row, then rebuild the window at
/// the new width with that line's first visual row at the viewport
/// top. `local_offset` was computed against the old wrap geometry, so
/// without this a width change would drift the scrolled-up viewport
/// (shrink: top jumps to earlier content; grow: content jumps around).
///
/// Only `source_start`/`local_offset` are rewritten; `source_end`,
/// `frozen_tail_cursor`, `frozen_source_end` and `follow_bottom` are
/// left untouched so a frozen streaming snapshot stays frozen and a
/// scrolled-up view stays scrolled up.
fn reanchor_window_on_resize(
    lines: &[DisplayLine],
    window: &mut ScrollWindow,
    old_width: usize,
    new_width: usize,
    height: usize,
) {
    let old_width = old_width.max(1);
    let new_width = new_width.max(1);
    let height = height.max(1);
    let local_old = local_window_lines(lines, window);
    if local_old.is_empty() {
        return;
    }
    // Replay the previous frame's bounded rendering at the old width,
    // with the same head-drain as render_bounded_window so the trimmed
    // row stream matches what was drawn; `dropped` maps local_offset
    // (an index into the trimmed stream) back onto the full stream.
    let visual_old = render_window(&local_old, 0, local_old.len(), old_width);
    let dropped = visual_old.len().saturating_sub(MAX_RENDER_VISUAL_ROWS);
    let target_row = window.local_offset.saturating_add(dropped);
    // Find the source line whose visual block contains the viewport
    // top. local_old mirrors lines[base..source_end] with
    // base = source_end - local_old.len(): local_window_lines keeps
    // the *last* MAX_RENDER_SOURCE_LINES lines of the window, which is
    // not necessarily lines[source_start] when the window is wider
    // than the line budget.
    let mut block = 0usize;
    let mut rows_before = 0usize;
    for (index, line) in local_old.iter().enumerate() {
        let rows = line_visual_rows(line, old_width);
        if target_row < rows_before.saturating_add(rows) {
            block = index;
            break;
        }
        rows_before = rows_before.saturating_add(rows);
    }
    let base = window.source_end.saturating_sub(local_old.len());
    let anchor_source = base
        .saturating_add(block)
        .min(lines.len().saturating_sub(1));
    // Rebuild at the new width: walk back from the anchor source line
    // (same shape as anchor_tail) until the lines above it cover
    // `height` visual rows, then point local_offset at the anchor
    // line's first visual row in the new wrap. The draw-time clamp
    // still fills the viewport when the anchor is too close to the
    // window tail to fill `height` rows below it.
    window.source_start = anchor_source;
    let mut rows_above = 0usize;
    while window.source_start > 0
        && rows_above < height.min(MAX_RENDER_VISUAL_ROWS)
        && anchor_source - window.source_start < MAX_RENDER_SOURCE_LINES
    {
        window.source_start -= 1;
        rows_above =
            rows_above.saturating_add(line_visual_rows(&lines[window.source_start], new_width));
    }
    window.local_offset = rows_above;
}

/// Count the visual rows a single DisplayLine would produce at `width`.
/// For Normal lines, uses the same markdown rendering pipeline as
/// `render_window` so that inline-code/bold delimiters are stripped before
/// wrapping — this keeps estimates consistent with actual rendering.
/// For non-Normal lines, falls back to `hard_wrap` (matches `styled_scroll_line`).
fn line_visual_rows(line: &DisplayLine, width: usize) -> usize {
    let width = width.max(1);
    if line.kind == LineKind::Normal {
        let mut md = crate::markdown::MarkdownLines::new();
        line.text
            .split('\n')
            .flat_map(|segment| {
                let spans = md.render_line(segment);
                crate::markdown::wrap_spans(&spans, width)
            })
            .count()
    } else {
        hard_wrap(&line.text, width).len()
    }
}

/// How many of the lines immediately before `source_start` to replay through
/// the Markdown renderer so code-fence state is seeded correctly. A fence
/// opened before this range may produce mis-styled body text inside the
/// window; in practice terminal-width code blocks rarely span >32 lines.
const FENCE_LOOKBEHIND: usize = 32;

/// Return the look-behind start index used by `render_window`. Exposed for
/// deterministic testing: the result is `source_start.saturating_sub(n)`
/// where `n ≤ FENCE_LOOKBEHIND` but is also clamped so that every newline
/// segment inside the look-behind range is replayed, not just DisplayLine
/// boundaries.
fn lookbehind_start(lines: &[DisplayLine], source_start: usize) -> usize {
    let mut count = 0usize;
    let mut idx = source_start;
    while idx > 0 {
        idx -= 1;
        let line = &lines[idx];
        count += if line.kind == LineKind::Normal {
            // Each embedded newline resets the search for ` ``` ` line,
            // so the segment count is what matters for fence scanning.
            line.text.split('\n').count()
        } else {
            // Non-Normal lines reset MarkdownLines fully, so the fence
            // state is reset after one such line; scanning further back
            // is pointless.
            1
        };
        if count > FENCE_LOOKBEHIND {
            // We have scanned enough segments; push `idx` forward one
            // (past the segment that tipped the count) so the returned
            // range stays inside the budget.
            return idx + 1;
        }
    }
    0
}

/// Render `lines[source_start..source_end]` into styled visual `Line`s at
/// `width`. The markdown code-fence state is primed by replaying at most
/// `FENCE_LOOKBEHIND` preceding lines through the MarkdownLines renderer
/// (see `lookbehind_start`). A code fence opened earlier may be invisible
/// to the priming pass and produce incorrect styling.
fn render_window(
    lines: &[DisplayLine],
    source_start: usize,
    source_end: usize,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let width = width.max(1);
    let mut markdown = crate::markdown::MarkdownLines::new();
    // Prime the fence state with a bounded prefix window.
    let lb_start = lookbehind_start(lines, source_start);
    for line in &lines[lb_start..source_start] {
        if line.kind == LineKind::Normal {
            for segment in line.text.split('\n') {
                markdown.render_line(segment);
            }
        } else {
            markdown = crate::markdown::MarkdownLines::new();
        }
    }
    lines[source_start..source_end]
        .iter()
        .flat_map(|line| {
            if line.kind == LineKind::Normal {
                line.text
                    .split('\n')
                    .flat_map(|segment| {
                        let spans = markdown.render_line(segment);
                        crate::markdown::wrap_spans(&spans, width)
                    })
                    .collect::<Vec<_>>()
            } else {
                hard_wrap(&line.text, width)
                    .into_iter()
                    .map(|row| styled_scroll_line(row, line.kind))
                    .collect::<Vec<_>>()
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Normal,
    Dim,
    Added,
    Removed,
    User,
    ToolCall,
    ToolResult,
    ToolError,
    /// Full-width banner marking where a compaction happened in the log.
    Compaction,
    /// Model reasoning ("thinking: …"); a faint violet so it reads as
    /// secondary yet stays distinct from the dark body text and from the
    /// neutral grey of `Dim` system notices.
    Thinking,
}

#[derive(Clone, Copy)]
enum BusyKind {
    Thinking,
    Compacting,
}

/// In-flight work shown in the input border. The spinner frame advances on
/// every routed session event (deltas, tool calls, …), so it spins while
/// the model streams and freezes when the provider stalls — a stuck frame
/// is itself the signal. Each view (main / attached) owns its TuiState, so
/// sessions spin independently.
#[derive(Clone, Copy)]
struct BusyState {
    kind: BusyKind,
    frame: usize,
}

impl BusyState {
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    fn thinking() -> Self {
        Self {
            kind: BusyKind::Thinking,
            frame: 0,
        }
    }

    fn compacting() -> Self {
        Self {
            kind: BusyKind::Compacting,
            frame: 0,
        }
    }

    fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    fn title(self) -> String {
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
enum ActiveStreamLane {
    Reasoning,
    Content,
}

/// Outcome of processing a key press while the tasks panel is open.
/// Shared by the idle and drive event loops so the routing logic is
/// tested once instead of duplicated.
#[derive(Debug, PartialEq, Eq)]
enum PanelAction {
    /// Esc or F2: close the panel.
    ClosePanel,
    /// `x` or Ctrl-C: cancel the selected task.
    CancelTask,
    /// Key was not a panel action; caller should fall through.
    Passthrough,
}

/// Outcome of a selection key while the tasks panel is open.
#[derive(Debug, PartialEq, Eq)]
enum TaskSelection {
    /// Attach to the selected task's session (delegate tasks, and cursor
    /// moves to an attachable task).
    Attach(u64),
    /// Open the full-output detail view (Enter on a bash task).
    OpenDetail(u64),
    /// Key was not a selection; caller should fall through.
    None,
}

impl TuiState {
    fn push_background_completion(&mut self, id: u64, output: &str, label: Option<&str>) {
        let header = match label.filter(|l| !l.trim().is_empty()) {
            Some(l) => {
                let previewed = crate::agent::preview(l, 60);
                format!("[background task {id} completed: {previewed}]")
            }
            None => format!("[background task {id} completed]"),
        };
        self.push_line(header, LineKind::Dim);
        let truncated = truncate_background_output(output);
        for line in truncated.lines() {
            self.push_line(line.to_owned(), LineKind::Dim);
        }
    }

    fn take_input(&mut self) -> Option<String> {
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
    fn attach(
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
    fn stash_attached_input(&mut self) {
        if let Some(attached) = &mut self.attached {
            self.stashed_input.remove(&attached.id);
            if !attached.input.text.is_empty() {
                self.stashed_input
                    .insert(attached.id, attached.input.text.clone());
            }
        }
    }

    fn detach(&mut self) {
        self.stash_attached_input();
        self.attached = None;
    }

    /// Keys pressed while attached: F2 toggles the tasks panel (so another
    /// session can be selected), scroll keys move the attached scrollback,
    /// Enter steers the session (queues a prompt for its next turn), Ctrl-C
    /// cancels its in-flight turn, everything else edits the steering input.
    fn handle_attached_key(&mut self, key: KeyEvent, input_width: usize) {
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
    fn mark_attached_finished(attached: &mut AttachedView) {
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
    fn push_event(&mut self, ui: UiEvent) {
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
    fn push_agent_event(&mut self, event: AgentEvent) {
        if let Some(busy) = &mut self.busy {
            busy.advance();
        }
        self.push_agent_event_inner(event);
    }

    /// Paste into whichever input is currently active, normalizing terminal
    /// line endings exactly as ordinary main-input paste did.
    fn handle_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(attached) = &mut self.attached {
            attached.input.insert(&text);
        } else {
            self.input.insert(&text);
        }
    }

    fn edit_input(&mut self, key: KeyEvent) {
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

    fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
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
    fn cursor_at_attached(&self) -> usize {
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

    /// Up/Down move the cursor and immediately attach to the selected task;
    /// Enter opens the full-output detail view for bash tasks and attaches
    /// for delegate tasks. Returns the requested action, if any.
    fn handle_tasks_panel_key(&mut self, key: KeyEvent) -> TaskSelection {
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
                return if task.kind == "bash" {
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
        // After any cursor move, attach to the newly selected task (if
        // attachable). Attaching is cheap — it replays an in-memory snapshot
        // and opens a broadcast channel, no I/O.
        let Some(task) = running.get(self.task_cursor) else {
            return TaskSelection::None;
        };
        if attachable(self, task.id) {
            TaskSelection::Attach(task.id)
        } else {
            TaskSelection::None
        }
    }

    /// Open the full-output detail view for a background bash task. The
    /// first frame shows the head page with follow armed; while output
    /// keeps growing the view slides to the live tail, and a paused task
    /// stays on the current page.
    fn open_task_detail(&mut self, id: u64) {
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
    }

    /// Keys while the full-output detail view is open. Esc returns to the
    /// tasks panel, F2 closes both, x/Ctrl-C cancels the task by id, scroll
    /// keys page through the spool.
    fn handle_task_detail_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.task_detail = None,
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
    fn handle_detail_scroll(&mut self, key: KeyEvent) {
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
    fn cancel_task(&mut self, id: u64) {
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
    fn cancel_selected_task(&mut self) {
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
    fn handle_panel_key(&mut self, key: KeyEvent) -> PanelAction {
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

    fn handle_scroll(&mut self, key: KeyEvent) {
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
                // Home is bounded too: exposing the beginning of an
                // unbounded transcript would force a full-history render.
                self.window.source_end = self.lines.len();
                self.window.source_start = self
                    .window
                    .source_end
                    .saturating_sub(MAX_RENDER_SOURCE_LINES);
                self.window.local_offset = 0;
                self.window.frozen_tail_cursor = None;
            }
            KeyCode::End => {
                self.window.follow_bottom = true;
                self.window.frozen_tail_cursor = None;
            }
            _ => {}
        }
    }

    fn at_bottom(&self) -> bool {
        self.window.follow_bottom
    }

    fn push_agent_event_inner(&mut self, event: AgentEvent) {
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

    fn follow(&mut self) {
        self.window.follow_bottom = true;
        self.window.frozen_tail_cursor = None;
        self.window.frozen_source_end = 0;
        // source_start/source_end will be anchored at the tail on next draw.
    }

    fn push_line(&mut self, text: String, kind: LineKind) {
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

    fn push_tool_call(&mut self, name: &str, arguments: &str) {
        if name == "edit_file"
            && let Some((path, old, new)) = parse_edit_arguments(arguments)
        {
            self.pending_edit = Some((path.clone(), old, new));
            self.push_line(format!("tool: edit_file {path}"), LineKind::ToolCall);
            return;
        }
        self.push_line(format_tool_call(name, arguments), LineKind::ToolCall);
    }

    fn push_tool_result(&mut self, content: &str, is_error: bool) {
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

    fn push_diff_side(&mut self, text: &str, prefix: &str, kind: LineKind, start: Option<usize>) {
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

/// After `handle_scroll` increments `local_offset`, check whether the
/// window needs to be extended forward (when the local end is reached and
/// more source lines exist). Adds enough `DisplayLine` entries to cover
/// `step` visual rows. When the true end is reached, resumes follow mode.
/// `on_extension` is called after any source_end growth (used to clear the
/// frozen-tail cursor once the user scrolls past the freeze point).
fn extend_window_down(state: &mut TuiState, step: usize, on_extension: impl FnOnce(&mut TuiState)) {
    let w = state.inner_width.max(1);
    let height = state.output_height.max(1);
    // Count the same bounded local copy that draw renders. Live deltas may
    // have made the source tail taller without changing the frozen cursor.
    let mut total_visual =
        render_bounded_window(&local_window_lines(&state.lines, &state.window), w).len();
    // The viewport-bottom check: the last visible row is
    // local_offset + height - 1.  Scrolling can advance until
    // local_offset + height >= total_visual.
    let at_visual_bottom = state.window.local_offset.saturating_add(height) >= total_visual;
    // If at the visual bottom and more source lines exist, extend forward.
    if at_visual_bottom && state.window.source_end < state.lines.len() {
        let mut added = 0usize;
        while state.window.source_end < state.lines.len() && added < step {
            let rows = line_visual_rows(&state.lines[state.window.source_end], w);
            state.window.source_end += 1;
            added += rows;
            total_visual += rows;
        }
        if state.window.source_end >= state.lines.len() {
            state.window.follow_bottom = true;
        }
        state.window.local_offset = state
            .window
            .local_offset
            .min(total_visual.saturating_sub(1).max(0));
        on_extension(state);
    } else if !state.window.follow_bottom
        && state.window.frozen_tail_cursor.is_some()
        && state.window.source_end >= state.lines.len()
        && at_visual_bottom
    {
        // Already at the true end — only deltas accumulated on the last
        // line. Switch to follow and clear the frozen cursor.
        state.window.follow_bottom = true;
        state.window.frozen_tail_cursor = None;
    }
}

fn parse_edited_line(content: &str) -> Option<usize> {
    content
        .strip_prefix("file edited (line ")?
        .strip_suffix(')')?
        .parse()
        .ok()
}

fn parse_edit_arguments(arguments: &str) -> Option<(String, String, String)> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    Some((
        value.get("path")?.as_str()?.to_owned(),
        value.get("old")?.as_str()?.to_owned(),
        value.get("new")?.as_str()?.to_owned(),
    ))
}

/// Format a single task row for the F2 task panel, incorporating structured
/// display metadata from delegate calls.
///
/// Output format: `  #<id>: [<role>] <label>[background][ workspace: <ws>]`
/// where `[background]` reflects the effective execution mode and
/// `[workspace: …]` is only shown when explicitly set. "bash" tasks never
/// have these tags.
pub fn format_task_label(
    task: &crate::tools::BackgroundTaskInfo,
    hint: &str,
    ws_max: usize,
) -> String {
    let base = if let Some(role) = &task.role {
        format!("  #{}: [{}] {}", task.id, role, task.label)
    } else {
        format!("  #{}: {}", task.id, task.label)
    };
    let mut s = base;
    if let Some(meta) = &task.display_meta {
        if meta.background {
            s.push_str(" [background]");
        }
        if let Some(ws) = &meta.workspace {
            use std::fmt::Write;
            let _ = write!(s, " [workspace: {}]", crate::agent::preview(ws, ws_max));
        }
    }
    s.push_str(hint);
    s
}

fn format_tool_call(name: &str, arguments: &str) -> String {
    use serde_json::Value;
    use std::fmt::Write;

    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return format!("tool: {name} {}", preview(arguments, 200));
    };

    match name {
        "bash" => {
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

#[cfg(test)]
mod format_task_label_tests {
    use super::*;
    use crate::tools::{BackgroundTaskInfo, TaskDisplayMeta};

    fn make_task(
        id: u64,
        role: Option<&str>,
        label: &str,
        display_meta: Option<TaskDisplayMeta>,
    ) -> BackgroundTaskInfo {
        BackgroundTaskInfo {
            id,
            label: label.into(),
            full_command: None,
            role: role.map(String::from),
            kind: "delegate".into(),
            output: vec![],
            display_meta,
        }
    }

    #[test]
    fn task_label_no_meta() {
        let task = make_task(1, None, "hello", None);
        assert_eq!(format_task_label(&task, "", 40), "  #1: hello");
    }

    #[test]
    fn task_label_with_role() {
        let task = make_task(2, Some("coder"), "write tests", None);
        assert_eq!(
            format_task_label(&task, "", 40),
            "  #2: [coder] write tests"
        );
    }

    #[test]
    fn task_label_background() {
        // Metadata stores the effective mode, including an omitted argument
        // that defaulted to background execution.
        let meta = TaskDisplayMeta {
            background: true,
            workspace: None,
        };
        let task = make_task(3, None, "bg task", Some(meta));
        assert_eq!(
            format_task_label(&task, "", 40),
            "  #3: bg task [background]"
        );
    }

    #[test]
    fn task_label_workspace() {
        let meta = TaskDisplayMeta {
            background: false,
            workspace: Some("/tmp/work".into()),
        };
        let task = make_task(4, Some("dev"), "deploy", Some(meta));
        assert_eq!(
            format_task_label(&task, "", 40),
            "  #4: [dev] deploy [workspace: /tmp/work]"
        );
    }

    #[test]
    fn task_label_background_and_workspace() {
        let meta = TaskDisplayMeta {
            background: true,
            workspace: Some("/custom/path".into()),
        };
        let task = make_task(5, None, "full", Some(meta));
        assert_eq!(
            format_task_label(&task, "", 40),
            "  #5: full [background] [workspace: /custom/path]"
        );
    }

    #[test]
    fn task_label_bash_no_tags() {
        let task = BackgroundTaskInfo {
            id: 10,
            label: "echo hi".into(),
            full_command: None,
            role: None,
            kind: "bash".into(),
            output: vec![],
            display_meta: None,
        };
        assert_eq!(format_task_label(&task, "", 40), "  #10: echo hi");
    }
}

#[derive(Default)]
struct InputBuffer {
    text: String,
    cursor: usize,
}

impl InputBuffer {
    fn insert(&mut self, text: &str) {
        let byte = self.byte_index();
        self.text.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    fn insert_char(&mut self, character: char) {
        self.insert(&character.to_string());
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }
    fn home(&mut self) {
        self.cursor = 0;
    }
    fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }
    fn backspace(&mut self) {
        if self.cursor > 0 {
            let end = self.byte_index();
            self.cursor -= 1;
            self.text.drain(self.byte_index()..end);
        }
    }
    fn delete(&mut self) {
        let start = self.byte_index();
        if start < self.text.len() {
            let end = self.text[start..]
                .char_indices()
                .nth(1)
                .map_or(self.text.len(), |(index, _)| start + index);
            self.text.drain(start..end);
        }
    }
    fn byte_index(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(index, _)| index)
    }

    /// Visual row count of the input at the given cell width, including
    /// embedded newlines and soft wrapping. Reserves a row for the cursor
    /// when the text ends exactly at a row boundary.
    fn visual_rows(&self, width: usize) -> usize {
        hard_wrap(&self.text, width)
            .len()
            .max(self.wrapped_cursor(width).0 + 1)
    }

    /// Cursor position as (row, column) in visual rows/cells.
    fn wrapped_cursor(&self, width: usize) -> (usize, usize) {
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ux_tests;
