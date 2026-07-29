use std::future::Future;
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
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::{Buffer, CellDiffOption, CellWidth};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::{Agent, AgentEvent, Message, SessionEntry, preview};
use crate::delegate::Sessions;
use crate::handle::SessionHandle;
use crate::session::Session;

/// Events on the shared UI channel are tagged by session id: `0` is the
/// main agent, anything else is an attached background session. The TUI
/// routes them to the matching scrollback.
#[derive(Clone, Debug)]
struct UiEvent {
    session: u64,
    event: AgentEvent,
}

type UiEventStream = futures_util::stream::Peekable<EventStream>;

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
    handle: &dyn SessionHandle,
    sender: mpsc::UnboundedSender<UiEvent>,
) -> tokio::task::AbortHandle {
    let mut stream = handle.subscribe();
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
    state.attach(
        task_id,
        label,
        entry.handle.clone(),
        entry.model.clone(),
        entry.role.clone(),
        entry.cwd.clone(),
        entry.session_id.clone(),
    );
    let bridge = bridge(task_id, entry.handle.as_ref(), sender.clone());
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
    mut agent: Agent,
    root: PathBuf,
    session_name: String,
    mut persisted: usize,
    background: crate::tools::BackgroundTasks,
    sessions: Sessions,
    model_name: String,
    role_name: Option<String>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
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
        &mut agent,
        &root,
        &labels,
        &mut persisted,
        background,
        sessions,
    )
    .await;
    // Do not return into main: the tokio runtime would then wait on
    // still-running tasks (MCP stdio servers block forever reading their
    // child's stdout), which is why exit used to need a second Ctrl-C.
    // Dropping the terminal restores the shell screen first; the OS reaps
    // any spawned MCP children.
    drop(terminal);
    drop(_guard);
    match result {
        Ok(()) => {
            eprintln!("e-agent: resume with: e-agent --session {}", labels.session);
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
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

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
    }
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: &mut Agent,
    root: &std::path::Path,
    labels: &InputLabels,
    persisted: &mut usize,
    background: crate::tools::BackgroundTasks,
    sessions: Sessions,
) -> anyhow::Result<()> {
    // One UI channel carrying every session's events, tagged by session id
    // (0 = main agent). Main-agent events route to the main scrollback,
    // attached-session events route to the attached view (see push_event).
    let (sender, mut inbox) = mpsc::unbounded_channel::<UiEvent>();
    let (main_handle, main_sink, _main_source) = crate::handle::session_channel();
    let _main_bridge = bridge(0, &main_handle, sender.clone());
    agent.observe(main_sink);
    // Per-turn event forwarder (deltas bypass the session log).
    let (forward, mut forward_inbox) = mpsc::unbounded_channel::<AgentEvent>();
    let forward_sender = sender.clone();
    tokio::spawn(async move {
        while let Some(event) = forward_inbox.recv().await {
            if forward_sender.send(UiEvent { session: 0, event }).is_err() {
                break;
            }
        }
    });
    let mut events = EventStream::new().peekable();
    let mut state = TuiState::from_history(agent.history());
    state.session_id = labels.session.clone();
    state.model_name = labels.model.clone();
    state.cwd = labels.cwd.clone();
    state.role_name = labels.role.clone();

    state.background = Some(background);
    let probe = sessions.clone();
    state.attachable = Some(Box::new(move |id| probe.get(id).is_some()));
    loop {
        draw(terminal, &mut state)?;
        tokio::select! {
            // Background task completed while idle: fold it into the model
            // context and kick off a turn immediately so the agent reacts
            // without waiting for the user's next message. No display line
            // here — the turn boundary emits the completion as a UserPrompt
            // which renders as the dim "finished" line.
            Some((_id, _output)) = agent.next_background_completion() => {
                agent.subscribe(forward.clone());
                let mut ui = Ui {
                    state: &mut state,
                    events: &mut events,
                    inbox: &mut inbox,
                    sessions: &sessions,
                    sender: &sender,
                };
                if run_request(
                    terminal,
                    agent,
                    &mut ui,
                    (root, &labels.session, persisted),
                    String::new(),
                )
                .await?
                {
                    return Ok(());
                }
                while let Some(next) = state.next_queued() {
                    state.push_line(format!("you> {next}"), LineKind::User);
                    state.follow();
                    agent.subscribe(forward.clone());
                    let mut ui = Ui {
                        state: &mut state,
                        events: &mut events,
                        inbox: &mut inbox,
                        sessions: &sessions,
                        sender: &sender,
                    };
                    if run_request(
                        terminal,
                        agent,
                        &mut ui,
                        (root, &labels.session, persisted),
                        next,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
            }
            event = events.next() => match event {
            Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                // Tasks panel open: navigation keys belong to the panel
                // (also while attached, so another session can be picked).
                if state.show_tasks {
                    if let Some(task_id) = state.handle_tasks_panel_key(key) {
                        attach_to_task(&mut state, task_id, &sessions, &sender);
                    } else if key.code == KeyCode::Esc || key.code == KeyCode::F(2) {
                        state.show_tasks = false;
                    } else if key.code == KeyCode::Char('x') {
                        state.cancel_selected_task();
                    } else if let Some(attached) = &mut state.attached {
                        let width = attached_input_width(terminal)?;
                        AttachedView::edit_input(&mut attached.input, key, width);
                    }
                    continue;
                }
                // Attached to a session view: Esc detaches; scroll and
                // steering keys are handled there (the main agent is idle
                // here so there is no turn to cancel).
                if state.attached.is_some() {
                    if key.code == KeyCode::Esc {
                        state.detach();
                    } else if is_scroll_key(key) {
                        let attached = state.attached.as_mut().unwrap();
                        attached.state.handle_scroll(key);
                        drain_ready_scroll_keys(&mut events, &mut attached.state).await;
                    } else {
                        let width = attached_input_width(terminal)?;
                        state.handle_attached_key(key, width);
                    }
                    continue;
                }
                if is_exit(key) {
                    return Ok(());
                }
                if is_scroll_key(key) {
                    state.handle_scroll(key);
                    drain_ready_scroll_keys(&mut events, &mut state).await;
                } else if let Some(prompt) = state.handle_key(key) {
                    if prompt == "/compact" {
                        state.push_line(compaction_banner("compacting…"), LineKind::Compaction);
                        state.busy = Some(BusyState::compacting());
                        state.streamed = false;
                        draw(terminal, &mut state)?;
                        let (result, interruption) = drive(
                            terminal,
                            &mut state,
                            &mut events,
                            &mut inbox,
                            &sessions,
                            &sender,
                            agent.compact(),
                        )
                        .await?;
                        state.busy = None;
                        while let Ok(event) = inbox.try_recv() {
                            state.push_event(event);
                        }
                        if let Some(result) = result {
                            match result {
                                Ok(_summary) => {
                                    // The summary already streamed live via
                                    // the session observer; the closing
                                    // banner is all the closure it needs.
                                    state.push_line(
                                        compaction_banner("compaction"),
                                        LineKind::Compaction,
                                    );
                                }
                                Err(error) => {
                                    state
                                        .push_line(format!("error: {error:#}"), LineKind::ToolError)
                                }
                            }
                        }
                        Session::append(root, &labels.session, &agent.history()[*persisted..])?;
                        *persisted = agent.history().len();
                        if matches!(interruption, Some(Interruption::ExitApp)) {
                            return Ok(());
                        }
                        if matches!(interruption, Some(Interruption::CancelTurn)) {
                            state.push_line("cancelled".into(), LineKind::Dim);
                            // Cancelling with queued prompts should not need
                            // one Ctrl-C per queued turn: fold everything
                            // queued into a single follow-up turn.
                            state.collapse_queue();
                        }
                        state.follow();
                        continue;
                    }
                    state.push_line(format!("you> {prompt}"), LineKind::User);
                    state.follow();
                    agent.subscribe(forward.clone());
                    let mut ui = Ui {
                        state: &mut state,
                        events: &mut events,
                        inbox: &mut inbox,
                        sessions: &sessions,
                        sender: &sender,
                    };
                    if run_request(
                        terminal,
                        agent,
                        &mut ui,
                        (root, &labels.session, persisted),
                        prompt,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                    while let Some(next) = state.next_queued() {
                        state.push_line(format!("you> {next}"), LineKind::User);
                        state.follow();
                        agent.subscribe(forward.clone());
                        let mut ui = Ui {
                            state: &mut state,
                            events: &mut events,
                            inbox: &mut inbox,
                            sessions: &sessions,
                            sender: &sender,
                        };
                        if run_request(
                            terminal,
                            agent,
                            &mut ui,
                            (root, &labels.session, persisted),
                            next,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                    }
                }
            }
            Some(Ok(Event::Paste(text))) => {
                // Windows-style pastes arrive as \r\n; hard_wrap only splits
                // on \n, so normalize or the newlines are swallowed.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if let Some(attached) = &mut state.attached {
                    attached.input.insert(&text);
                } else {
                    state.input.insert(&text);
                }
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => return Ok(()),
        }
        }
    }
}

/// Bundles the per-run UI plumbing so `run_request` stays readable.
struct Ui<'a> {
    state: &'a mut TuiState,
    events: &'a mut UiEventStream,
    inbox: &'a mut mpsc::UnboundedReceiver<UiEvent>,
    sessions: &'a Sessions,
    sender: &'a mpsc::UnboundedSender<UiEvent>,
}

async fn run_request(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: &mut Agent,
    ui: &mut Ui<'_>,
    session: (&std::path::Path, &str, &mut usize),
    prompt: String,
) -> anyhow::Result<bool> {
    let (root, session_name, persisted) = session;
    ui.state.busy = Some(BusyState::thinking());
    // Reset the per-turn stream state. A turn that ended on a plain text
    // answer leaves active_lane = Content (no tool result reset it); without
    // clearing it here the next turn's first delta would append onto the
    // `you> …` user line, dyeing the whole reply in the user color.
    ui.state.streamed = false;
    ui.state.active_lane = None;
    draw(terminal, ui.state)?;
    // Agent::run keeps turning until the model has reacted to every
    // background completion that arrived along the way, so one drive()
    // covers any completion-triggered follow-up turns too.
    let (result, interruption) = drive(
        terminal,
        ui.state,
        ui.events,
        ui.inbox,
        ui.sessions,
        ui.sender,
        agent.run(prompt),
    )
    .await?;
    ui.state.busy = None;
    while let Ok(event) = ui.inbox.try_recv() {
        ui.state.push_event(event);
    }
    if let Some(result) = result {
        match result {
            Ok(answer) => ui.state.push_final_answer(answer),
            Err(error) => ui
                .state
                .push_line(format!("error: {error:#}"), LineKind::ToolError),
        }
    }
    if matches!(interruption, Some(Interruption::CancelTurn)) {
        ui.state.push_line("cancelled".into(), LineKind::Dim);
        ui.state.collapse_queue();
    }
    Session::append(root, session_name, &agent.history()[*persisted..])?;
    *persisted = agent.history().len();
    ui.state.follow();
    draw(terminal, ui.state)?;
    Ok(matches!(interruption, Some(Interruption::ExitApp)))
}

enum Interruption {
    ExitApp,
    CancelTurn,
}

/// Pump an agent future to completion while streaming session events into
/// the scrollback and keeping the UI responsive. Scroll and input editing
/// stay available while work is in flight; Ctrl-C cancels the turn (Esc
/// only leaves the current view: detach from a session or close the panel).
async fn drive<T>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut TuiState,
    events: &mut UiEventStream,
    inbox: &mut mpsc::UnboundedReceiver<UiEvent>,
    sessions: &Sessions,
    sender: &mpsc::UnboundedSender<UiEvent>,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<(Option<anyhow::Result<T>>, Option<Interruption>)> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return Ok((Some(result), None)),
            Some(event) = inbox.recv() => {
                state.push_event(event);
                draw(terminal, state)?;
            }
            event = events.next() => match event {
                // Attached: Esc detaches from the session view instead of
                // cancelling the in-flight turn.
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press
                    && key.code == KeyCode::Esc
                    && state.attached.is_some() =>
                {
                    state.detach();
                    draw(terminal, state)?;
                }
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press && is_cancel(key) => {
                    return Ok((None, Some(Interruption::CancelTurn)));
                }
                // Not attached: Esc during a turn only closes the tasks
                // panel (leave-the-current-view), it never cancels the turn.
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press
                    && key.code == KeyCode::Esc =>
                {
                    if state.show_tasks {
                        state.show_tasks = false;
                        draw(terminal, state)?;
                    }
                }
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press
                    && state.show_tasks =>
                {
                    if let Some(task_id) = state.handle_tasks_panel_key(key) {
                        attach_to_task(state, task_id, sessions, sender);
                    } else if key.code == KeyCode::F(2) {
                        state.show_tasks = false;
                    } else if key.code == KeyCode::Char('x') {
                        state.cancel_selected_task();
                    } else if key.code == KeyCode::Enter {
                        // Enter on a non-attachable task: nothing to attach.
                    } else if let Some(attached) = &mut state.attached {
                        // Panel open while attached: remaining keys edit the
                        // steering input (panel already consumed nav keys).
                        let width = attached_input_width(terminal)?;
                        AttachedView::edit_input(&mut attached.input, key, width);
                    } else {
                        state.handle_scroll(key);
                        state.edit_input(key);
                    }
                    draw(terminal, state)?;
                }
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press
                    && state.attached.is_some() =>
                {
                    if is_scroll_key(key) {
                        let attached = state.attached.as_mut().unwrap();
                        attached.state.handle_scroll(key);
                        drain_ready_scroll_keys(events, &mut attached.state).await;
                    } else {
                        let width = attached_input_width(terminal)?;
                        state.handle_attached_key(key, width);
                    }
                    draw(terminal, state)?;
                }
                Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                    if key.code == KeyCode::F(2) {
                        state.show_tasks = true;
                        state.task_cursor = 0;
                    } else if key.code == KeyCode::Enter {
                        if key.modifiers == KeyModifiers::ALT {
                            state.input.insert_char('\n');
                        } else if let Some(prompt) = state.take_input() {
                            state.queued.push(prompt);
                        }
                    } else if is_scroll_key(key) {
                        state.handle_scroll(key);
                        drain_ready_scroll_keys(events, state).await;
                    } else {
                        state.edit_input(key);
                    }
                    draw(terminal, state)?;
                }
                Some(Ok(Event::Paste(text))) => {
                    // Same \r\n normalization as the idle loop.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    if let Some(attached) = &mut state.attached {
                        attached.input.insert(&text);
                    } else {
                        state.input.insert(&text);
                    }
                    draw(terminal, state)?;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Ok((Some(Err(error.into())), Some(Interruption::ExitApp))),
                None => return Ok((None, Some(Interruption::ExitApp))),
            }
        }
    }
}

fn draw<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
) -> Result<(), B::Error> {
    terminal
        .draw(|frame| {
            // Paint first, then every region below paints its own surface. This
            // keeps the alternate screen free of terminal-default holes.
            frame.render_widget(
                Block::default().style(SOLARIZED_LIGHT.screen_style()),
                frame.area(),
            );
            let attached = state.attached.is_some();
            let inner_input_width = usize::from(frame.area().width.saturating_sub(2)).max(1);
            let input_rows = if let Some(attached) = &state.attached {
                attached.input.visual_rows(inner_input_width)
            } else {
                state.input.visual_rows(inner_input_width)
            };
            let input_height = (input_rows + 2)
                .min((usize::from(frame.area().height) / 3).max(3))
                .max(3) as u16;
            let queue_height = if attached || state.queued.is_empty() {
                0
            } else {
                // One row per queued prompt, so every pending message is
                // visible (not just the head).
                state.queued.len() as u16
            };
            let running: Vec<crate::tools::BackgroundTaskInfo> = state
                .background
                .as_ref()
                .map(|background| background.running())
                .unwrap_or_default();
            // Panel open: full list. Panel closed but tasks running: a one-line
            // hint so background work never goes completely unnoticed.
            const OUTPUT_LINES: usize = 8;
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
                let queued_lines: Vec<Line> = state
                    .queued
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
                let attachable = |id: u64| {
                    state
                        .attached
                        .as_ref()
                        .is_some_and(|attached| attached.id == id)
                        || state
                            .attachable
                            .as_ref()
                            .is_some_and(|attachable| attachable(id))
                };
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
                    if index == state.task_cursor && attachable(task.id) {
                        style = Style::default()
                            .fg(SOLARIZED_LIGHT.text)
                            .bg(SOLARIZED_LIGHT.selection)
                            .add_modifier(Modifier::BOLD);
                    }
                    let hint = if attachable(task.id) {
                        ""
                    } else {
                        "  (no view)"
                    };
                    let row = if let Some(role) = &task.role {
                        format!("  #{}: [{}] {}{}", task.id, role, task.label, hint)
                    } else {
                        format!("  #{}: {}{}", task.id, task.label, hint)
                    };
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
                        .block(SOLARIZED_LIGHT.block("tasks (F2 hide · Enter attach · x cancel)")),
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
            // Body text (Normal) is parsed as markdown line-by-line so code
            // fences track state across lines; other kinds keep flat styling.
            let mut markdown = crate::markdown::MarkdownLines::new();
            let visual: Vec<Line> = scroll_state
                .lines
                .iter()
                .flat_map(|line| {
                    if line.kind == LineKind::Normal {
                        // A Normal line may embed newlines (replayed content,
                        // a non-streamed final answer); split them so each
                        // becomes its own visual row. The code-fence state
                        // still carries across these segments.
                        line.text
                            .split('\n')
                            .flat_map(|segment| {
                                let spans = markdown.render_line(segment);
                                crate::markdown::wrap_spans(&spans, inner_width)
                            })
                            .collect::<Vec<Line>>()
                    } else {
                        hard_wrap(&line.text, inner_width)
                            .into_iter()
                            .map(move |row| styled_scroll_line(row, line.kind))
                            .collect()
                    }
                })
                .collect();
            let total_rows = visual.len();
            let paragraph = Paragraph::new(visual).style(SOLARIZED_LIGHT.screen_style());
            let height = usize::from(output.height);
            let max_scroll = total_rows.saturating_sub(height);
            scroll_state.max_scroll = max_scroll;
            scroll_state.scroll = scroll_state.scroll.min(max_scroll);
            frame.render_widget(
                paragraph.scroll((scroll_state.scroll.min(u16::MAX as usize) as u16, 0)),
                output,
            );
            force_wide_trailing_cell_updates(frame.buffer_mut(), output);
            if let Some(attached) = &mut state.attached {
                let status = if attached.finished {
                    "finished"
                } else {
                    "running"
                };
                let block = SOLARIZED_LIGHT
                    .block(format!(
                        "subagent #{}: {} ({}) — Esc detach · Enter steer · Ctrl-C interrupt",
                        attached.id,
                        preview(&attached.label, 40),
                        status
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
                attached.input_scroll =
                    cursor_row.saturating_sub(inner_input_height.saturating_sub(1));
                let input_lines: Vec<Line> = hard_wrap(&attached.input.text, inner_input_width)
                    .into_iter()
                    .map(|line| Line::styled(line, SOLARIZED_LIGHT.panel_style()))
                    .collect();
                frame.render_widget(
                    Paragraph::new(input_lines)
                        .style(SOLARIZED_LIGHT.panel_style())
                        .block(block)
                        .scroll((attached.input_scroll.min(u16::MAX as usize) as u16, 0)),
                    input,
                );
                {
                    let usage = cwd_usage_text(&attached.state.cwd, attached.state.tokens_context);
                    let width = UnicodeWidthStr::width(usage.as_str()) as u16 + 1;
                    if input.width > width + 1 {
                        let area = ratatui::layout::Rect {
                            x: input.right().saturating_sub(width + 1),
                            y: input.bottom() - 1,
                            width,
                            height: 1,
                        };
                        frame.render_widget(
                            Paragraph::new(usage).style(
                                Style::default()
                                    .fg(SOLARIZED_LIGHT.orange)
                                    .bg(SOLARIZED_LIGHT.panel),
                            ),
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
            let usage = cwd_usage_text(&state.cwd, state.tokens_context);
            let (cursor_row, cursor_col) = state.input.wrapped_cursor(inner_input_width);
            let inner_input_height = usize::from(input.height.saturating_sub(2));
            let input_scroll = cursor_row.saturating_sub(inner_input_height.saturating_sub(1));
            let input_lines: Vec<Line> = hard_wrap(&state.input.text, inner_input_width)
                .into_iter()
                .map(|line| Line::styled(line, SOLARIZED_LIGHT.panel_style()))
                .collect();
            frame.render_widget(
                Paragraph::new(input_lines)
                    .style(SOLARIZED_LIGHT.panel_style())
                    .block(input_block)
                    .scroll((input_scroll.min(u16::MAX as usize) as u16, 0)),
                input,
            );
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
                        Paragraph::new(usage).style(
                            Style::default()
                                .fg(SOLARIZED_LIGHT.orange)
                                .bg(SOLARIZED_LIGHT.panel),
                        ),
                        area,
                    );
                }
            }
            frame.set_cursor_position((
                input.x + 1 + (cursor_col as u16).min(input.width.saturating_sub(2)),
                input.y
                    + 1
                    + ((cursor_row - input_scroll) as u16).min(input.height.saturating_sub(2)),
            ));
        })
        .map(|_| ())
}

/// Ratatui's diff skips the cells covered by wide glyphs. Always emit the
/// following visible cell so it cannot retain terminal-default attributes.
fn force_wide_trailing_cell_updates(buffer: &mut Buffer, area: Rect) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let width = buffer[(x, y)].cell_width();
            let trailing = x.saturating_add(width);
            if width > 1 && trailing < area.right() {
                buffer[(trailing, y)].set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
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
/// with an optional ` · role` suffix (muted). Shared by the main and attached
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
            format!(" · {role}"),
            Style::default().fg(SOLARIZED_LIGHT.muted),
        ));
    }
    spans
}

/// Format the bottom-right overlay text: cwd with optional context-token
/// count. Shared by the main and attached views. A cwd under `$HOME` is
/// shortened to `~/…` to keep the overlay narrow.
fn cwd_usage_text(cwd: &str, tokens_context: u64) -> String {
    let cwd = shorten_home(cwd);
    if tokens_context > 0 {
        format!("{} · ctx {}", cwd, format_tokens(tokens_context))
    } else {
        cwd.into_owned()
    }
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
fn compaction_banner(label: &str) -> String {
    format!("──── {label} ────")
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
    lines
        .iter()
        .map(|line| hard_wrap(&line.text, width).len())
        .sum()
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
    scroll: usize,
    max_scroll: usize,
    busy: Option<BusyState>,
    streamed: bool,
    active_lane: Option<ActiveStreamLane>,
    tokens_context: u64,
    /// Prompts submitted while a turn is in flight; drained (never persisted)
    /// once the current turn ends.
    queued: Vec<String>,
    /// Arguments of an in-flight edit_file call, rendered as a numbered diff
    /// when its result (which carries the line number) arrives.
    pending_edit: Option<(String, String, String)>,
    /// Shared running-task registry, for the tasks panel.
    background: Option<crate::tools::BackgroundTasks>,
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
    handle: Arc<dyn SessionHandle>,
    /// Input buffer for steering prompts.
    input: InputBuffer,
    /// Vertical scroll of the steering input (multi-line).
    input_scroll: usize,
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

impl AttachedView {
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

struct DisplayLine {
    text: String,
    kind: LineKind,
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

impl TuiState {
    fn from_history(entries: &[SessionEntry]) -> Self {
        let mut state = Self::default();
        for entry in entries {
            match entry {
                SessionEntry::Message { message } => state.push_message(message),
                SessionEntry::Notice { text } => {
                    state.push_line(text.clone(), LineKind::Dim);
                }
                SessionEntry::Compaction { summary, .. } => {
                    state.push_line(compaction_banner("compaction"), LineKind::Compaction);
                    state.push_line(
                        format!("compacted: {}", preview(summary, 150)),
                        LineKind::Dim,
                    );
                }
            }
        }
        state.follow();
        state
    }

    fn push_message(&mut self, message: &Message) {
        match message {
            Message::System { content } => {
                self.push_line(format!("system: {}", preview(content, 500)), LineKind::Dim);
            }
            Message::User { content } => {
                self.push_line(format!("you> {content}"), LineKind::User);
            }
            Message::Assistant(message) => {
                if let Some(reasoning) =
                    message.reasoning.as_deref().filter(|text| !text.is_empty())
                {
                    self.push_line(
                        format!("thinking: {}", preview(reasoning, 1000)),
                        LineKind::Thinking,
                    );
                }
                if let Some(content) = message.content.as_deref().filter(|text| !text.is_empty()) {
                    self.push_line(content.to_owned(), LineKind::Normal);
                }
                for call in &message.tool_calls {
                    self.push_tool_call(&call.name, &call.arguments);
                }
            }
            Message::Tool {
                content, is_error, ..
            } => self.push_tool_result(content, *is_error),
        }
    }

    fn take_input(&mut self) -> Option<String> {
        let prompt = std::mem::take(&mut self.input.text);
        self.input.cursor = 0;
        (!prompt.trim().is_empty()).then_some(prompt)
    }

    fn next_queued(&mut self) -> Option<String> {
        if self.queued.is_empty() {
            None
        } else {
            Some(self.queued.remove(0))
        }
    }

    /// Fold every queued prompt into one so a cancelled turn is followed by
    /// a single combined turn instead of N turns needing N cancels.
    fn collapse_queue(&mut self) {
        if self.queued.len() > 1 {
            let joined = self.queued.join("\n\n");
            self.queued = vec![joined];
        }
    }

    /// Attach to a background session: snapshot its event log into a fresh
    /// scrollback and follow its live stream from now on (see `bridge`).
    #[allow(clippy::too_many_arguments)]
    fn attach(
        &mut self,
        id: u64,
        label: String,
        handle: Arc<dyn SessionHandle>,
        model_name: String,
        role_name: Option<String>,
        cwd: String,
        session_id: String,
    ) {
        let mut state = TuiState {
            model_name,
            role_name,
            cwd,
            session_id,
            ..TuiState::default()
        };
        let mut finished = false;
        for event in handle.snapshot() {
            // The completion may have raced into the log before the attach;
            // the view must still flip to finished.
            if matches!(event, AgentEvent::BackgroundCompleted { .. }) {
                finished = true;
            }
            state.push_agent_event(event);
        }
        state.follow();
        self.attached = Some(Box::new(AttachedView {
            id,
            label,
            state,
            bridge: None,
            handle,
            input: InputBuffer::default(),
            input_scroll: 0,
            finished,
        }));
    }

    fn detach(&mut self) {
        self.attached = None;
    }

    /// Keys pressed while attached: F2 toggles the tasks panel (so another
    /// session can be selected), scroll keys move the attached scrollback,
    /// Enter steers the session (queues a prompt for its next turn), Ctrl-C
    /// cancels its in-flight turn, everything else edits the steering input.
    fn handle_attached_key(&mut self, key: KeyEvent, input_width: usize) {
        if key.code == KeyCode::F(2) {
            self.show_tasks = !self.show_tasks;
            self.task_cursor = 0;
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
                let prompt = std::mem::take(&mut attached.input.text);
                attached.input.cursor = 0;
                if !prompt.trim().is_empty() && !attached.finished {
                    // send_input records a UserPrompt event in the session
                    // log/stream, which renders the `you>` line (queued or
                    // not) via the normal event path.
                    attached.handle.send_input(prompt);
                }
            }
            _ if is_cancel(key) && !attached.finished => attached.handle.cancel(),
            _ => AttachedView::edit_input(&mut attached.input, key, input_width),
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
                    attached.finished = true;
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
                self.task_cursor = 0;
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
    /// Up/Down move the attach cursor, Enter attaches to a subagent task.
    /// Returns the attach request `(task_id)` if one was made.
    fn handle_tasks_panel_key(&mut self, key: KeyEvent) -> Option<u64> {
        let running = self
            .background
            .as_ref()
            .map(|background| background.running())
            .unwrap_or_default();
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
                let task = running.get(self.task_cursor)?;
                // Only tasks with a live session handle can be attached.
                let attachable = self.attachable.as_ref()?;
                if !attachable(task.id) {
                    return None;
                }
                return Some(task.id);
            }
            _ => {}
        }
        None
    }

    /// `x` in the tasks panel cancels the selected background task, unless it
    /// is an attachable subagent session (those steer through their own view).
    fn cancel_selected_task(&mut self) {
        let running = self
            .background
            .as_ref()
            .map(|background| background.running())
            .unwrap_or_default();
        let Some(task) = running.get(self.task_cursor.min(running.len().saturating_sub(1))) else {
            return;
        };
        if self.attachable.as_ref().is_some_and(|probe| probe(task.id)) {
            return;
        }
        if let Some(label) = self.background.as_ref().and_then(|b| b.cancel(task.id)) {
            self.push_line(
                format!("cancelled task #{}: {}", task.id, label),
                LineKind::Dim,
            );
        }
    }

    fn handle_scroll(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1).min(self.max_scroll),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10).min(self.max_scroll),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.follow(),
            _ => {}
        }
    }

    fn at_bottom(&self) -> bool {
        self.scroll >= self.max_scroll
    }

    fn push_agent_event_inner(&mut self, event: AgentEvent) {
        let at_bottom = self.at_bottom();
        let ends_delta = matches!(
            &event,
            AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. }
        );
        match event {
            AgentEvent::Notice(text) => {
                self.push_line(text, LineKind::Dim);
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
            AgentEvent::Usage { context_input, .. } => {
                self.tokens_context = context_input;
            }
        }
        if ends_delta {
            self.streamed = false;
            self.active_lane = None;
        }
        if at_bottom {
            self.follow();
        }
    }

    fn follow(&mut self) {
        // Clamped to the real bottom (wrapped rows minus viewport) in draw().
        self.scroll = usize::MAX;
    }

    fn push_final_answer(&mut self, answer: String) {
        if !self.streamed {
            self.push_line(answer, LineKind::Normal);
        }
    }

    fn push_line(&mut self, text: String, kind: LineKind) {
        self.lines.push(DisplayLine { text, kind });
    }

    fn push_tool_call(&mut self, name: &str, arguments: &str) {
        if name == "edit_file"
            && let Some((path, old, new)) = parse_edit_arguments(arguments)
        {
            self.pending_edit = Some((path.clone(), old, new));
            self.push_line(format!("tool: edit_file {path}"), LineKind::ToolCall);
            return;
        }
        self.push_line(
            format!("tool: {name} {}", preview(arguments, 200)),
            LineKind::ToolCall,
        );
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
mod tests {
    use super::*;

    /// Test helper: attach with default metadata, mirroring the old 3-arg
    /// signature for minimal test churn.
    fn attach_test(state: &mut TuiState, id: u64, label: &str, handle: Arc<dyn SessionHandle>) {
        state.attach(
            id,
            label.into(),
            handle,
            String::new(),
            None,
            String::new(),
            String::new(),
        );
    }

    #[test]
    fn f2_toggles_tasks_panel() {
        let mut state = TuiState::default();
        assert!(!state.show_tasks);
        let key = || KeyEvent::new(KeyCode::F(2), KeyModifiers::empty());
        assert_eq!(state.handle_key(key()), None);
        assert!(state.show_tasks);
        assert_eq!(state.handle_key(key()), None);
        assert!(!state.show_tasks);
    }

    #[test]
    fn f2_toggles_the_panel_while_attached() {
        let (handle, _sink, _source) = crate::handle::session_channel();
        let mut state = TuiState::default();
        attach_test(&mut state, 7, "demo", Arc::new(handle));
        assert!(!state.show_tasks);
        state.handle_attached_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()), 80);
        assert!(state.show_tasks, "F2 opens the panel while attached");
        assert!(state.attached.is_some(), "attach view is kept");
        state.handle_attached_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()), 80);
        assert!(!state.show_tasks);
    }

    #[test]
    fn tasks_panel_nav_clamps_cursor_and_ignores_non_nav_keys() {
        // Panel nav does not need real running tasks to test cursor
        // movement: with no background set the cursor just stays put and
        // plain characters fall through.
        let mut state = TuiState {
            show_tasks: true,
            ..Default::default()
        };
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(state.handle_tasks_panel_key(down), None);
        assert_eq!(state.task_cursor, 0, "no tasks: cursor stays at 0");
        assert_eq!(state.handle_tasks_panel_key(up), None);
        assert_eq!(state.task_cursor, 0);
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(state.handle_tasks_panel_key(a), None);
    }

    #[tokio::test]
    async fn reattaching_forwards_each_delta_once() {
        let (handle, sink, _source) = crate::handle::session_channel();
        let (sender, mut inbox) = mpsc::unbounded_channel();
        let mut state = TuiState::default();
        attach_test(&mut state, 7, "demo", Arc::new(handle.clone()));
        state.attached.as_mut().unwrap().bridge = Some(bridge(7, &handle, sender.clone()));
        state.detach();
        tokio::task::yield_now().await;

        attach_test(&mut state, 7, "demo", Arc::new(handle.clone()));
        state.attached.as_mut().unwrap().bridge = Some(bridge(7, &handle, sender));
        sink.emit(AgentEvent::AssistantDelta("你".into()));
        let first = tokio::time::timeout(Duration::from_secs(1), inbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.session, 7);
        assert!(matches!(first.event, AgentEvent::AssistantDelta(text) if text == "你"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err(),
            "both the old and new bridges forwarded the same delta"
        );
    }

    #[test]
    fn attached_view_replays_snapshot_and_marks_finished_on_completion() {
        let (handle, sink, _source) = crate::handle::session_channel();
        let mut state = TuiState::default();
        sink.emit(AgentEvent::AssistantDelta("partial".into()));
        sink.emit(AgentEvent::ToolCall {
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        sink.emit(AgentEvent::ToolResult {
            is_error: false,
            content: "files".into(),
        });
        attach_test(&mut state, 7, "demo task", Arc::new(handle));
        let lines: Vec<_> = state
            .attached
            .as_ref()
            .unwrap()
            .state
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(
            lines,
            ["partial", r#"tool: bash {"command":"ls"}"#, "  ok: files",]
        );
        // The transient BackgroundCompleted flips the attached view to
        // finished but renders nothing (the persistent "finished" line
        // comes from the UserPrompt at the turn boundary).
        assert!(!state.attached.as_ref().unwrap().finished);
        state.push_event(UiEvent {
            session: 0,
            event: AgentEvent::BackgroundCompleted {
                id: 7,
                output: "done".into(),
            },
        });
        assert!(state.attached.as_ref().unwrap().finished);
        assert_eq!(
            state
                .attached
                .as_ref()
                .unwrap()
                .state
                .lines
                .last()
                .unwrap()
                .text,
            "  ok: files",
            "transient completion renders no line"
        );
        state.detach();
        assert!(state.attached.is_none());
    }

    #[test]
    fn attach_after_completion_marks_finished_from_the_snapshot() {
        // Regression: a completion that raced into the session log before
        // the attach left the view stuck in "running" forever.
        let (handle, sink, _source) = crate::handle::session_channel();
        sink.emit(AgentEvent::AssistantText("work".into()));
        sink.emit(AgentEvent::BackgroundCompleted {
            id: 3,
            output: "done".into(),
        });
        let mut state = TuiState::default();
        attach_test(&mut state, 3, "demo task", Arc::new(handle));
        assert!(state.attached.as_ref().unwrap().finished);
    }

    #[test]
    fn background_completion_notice_renders_as_dim_line() {
        // The turn-boundary completion is a structured Notice event (not a
        // UserPrompt with a magic prefix); the main view renders it dim.
        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::Notice(
            "[background task 2 completed]\nall good".into(),
        ));
        assert_eq!(
            state.lines.last().unwrap().text,
            "[background task 2 completed]\nall good"
        );
        assert_eq!(state.lines.last().unwrap().kind, LineKind::Dim);
        // A regular user prompt still renders as user input.
        state.push_agent_event(AgentEvent::UserPrompt("hello".into()));
        assert_eq!(state.lines.last().unwrap().text, "you> hello");
    }

    #[test]
    fn attached_enter_steers_and_ctrl_c_cancels_through_the_handle() {
        let (handle, _sink, mut source) = crate::handle::session_channel();
        let mut state = TuiState::default();
        attach_test(&mut state, 7, "demo task", Arc::new(handle));
        {
            let attached = state.attached.as_mut().unwrap();
            attached.input.insert("please also check tests");
        }
        state.handle_attached_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), 80);
        assert_eq!(
            source.try_recv(),
            Some(crate::handle::Steer::Prompt(
                "please also check tests".into()
            ))
        );
        // The steering prompt is recorded in the session log (snapshot
        // replays it as a `you>` line on the next event).
        assert_eq!(
            state.attached.as_ref().unwrap().handle.snapshot(),
            vec![AgentEvent::UserPrompt("please also check tests".into())]
        );
        // Re-attach replays the snapshot including the queued prompt.
        let handle = state.attached.as_ref().unwrap().handle.clone();
        attach_test(&mut state, 7, "demo task", handle);
        let lines = &state.attached.as_ref().unwrap().state.lines;
        assert!(
            lines
                .iter()
                .any(|line| line.text == "you> please also check tests")
        );
        state.handle_attached_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 80);
        assert_eq!(source.try_recv(), Some(crate::handle::Steer::Cancel));
        // Finished sessions no longer accept steering.
        state.push_event(UiEvent {
            session: 0,
            event: AgentEvent::BackgroundCompleted {
                id: 7,
                output: "done".into(),
            },
        });
        state.handle_attached_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 80);
        assert_eq!(source.try_recv(), None);
    }

    #[test]
    fn attached_view_scrolls_independently() {
        let (handle, _sink, _source) = crate::handle::session_channel();
        let mut state = TuiState {
            scroll: 2,
            ..Default::default()
        };
        attach_test(&mut state, 1, "task", Arc::new(handle));
        {
            let attached = state.attached.as_mut().unwrap();
            attached.state.max_scroll = 9;
            attached.state.scroll = 5;
        }
        // Scrolling while attached moves the subagent view only.
        state.handle_attached_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()), 80);
        assert_eq!(state.scroll, 2, "main scroll untouched");
        assert_eq!(state.attached.as_ref().unwrap().state.scroll, 4);
        state.handle_attached_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), 80);
        assert_eq!(state.attached.as_ref().unwrap().state.scroll, 5);
    }

    #[test]
    fn input_autoheight_and_wrapped_cursor_track_visual_rows() {
        let mut input = InputBuffer::default();
        input.insert("ab\ncdef");
        assert_eq!(input.visual_rows(10), 2);
        // "ab" (1 row) + newline + "cdef" wraps to 2 rows at width 3.
        assert_eq!(input.visual_rows(3), 3);
        input.end();
        assert_eq!(input.wrapped_cursor(3), (2, 1));
        input.home();
        assert_eq!(input.wrapped_cursor(3), (0, 0));
        // Exactly full row: reserve one more row so the cursor stays visible.
        let mut input = InputBuffer::default();
        input.insert("ab");
        assert_eq!(input.visual_rows(2), 2);
    }

    #[tokio::test]
    async fn ready_scroll_keys_are_coalesced_without_consuming_following_input() {
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let typing = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let mut events = futures_util::stream::iter(vec![
            Ok::<_, io::Error>(Event::Key(down)),
            Ok(Event::Key(page_down)),
            Ok(Event::Key(typing)),
        ])
        .peekable();
        let mut state = TuiState {
            max_scroll: 20,
            ..Default::default()
        };

        state.handle_scroll(down);
        drain_ready_scroll_keys(&mut events, &mut state).await;
        assert_eq!(state.scroll, 12, "the ready scroll run is applied in order");
        assert!(matches!(
            events.next().await,
            Some(Ok(Event::Key(key))) if key == typing
        ));

        let mut step = 0;
        let delayed_scroll = futures_util::stream::poll_fn(move |cx| match step {
            0 => {
                step = 1;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            1 => {
                step = 2;
                std::task::Poll::Ready(Some(Ok::<_, io::Error>(Event::Key(down))))
            }
            _ => std::task::Poll::Ready(None),
        });
        let mut events = delayed_scroll
            .chain(futures_util::stream::iter(vec![Ok(Event::Key(typing))]))
            .peekable();
        let mut state = TuiState {
            max_scroll: 20,
            ..Default::default()
        };
        state.handle_scroll(down);
        drain_ready_scroll_keys(&mut events, &mut state).await;
        assert_eq!(state.scroll, 2, "a woken scroll key joins the quiet window");
        assert!(matches!(
            events.next().await,
            Some(Ok(Event::Key(key))) if key == typing
        ));

        let (handle, _sink, _source) = crate::handle::session_channel();
        let mut parent = TuiState {
            scroll: 7,
            ..Default::default()
        };
        attach_test(&mut parent, 1, "task", Arc::new(handle));
        let mut events = futures_util::stream::iter(vec![
            Ok::<_, io::Error>(Event::Key(down)),
            Ok(Event::Key(typing)),
        ])
        .peekable();
        {
            let attached = parent.attached.as_mut().unwrap();
            attached.state.max_scroll = 20;
            attached.state.scroll = 0;
            attached.state.handle_scroll(down);
            drain_ready_scroll_keys(&mut events, &mut attached.state).await;
            assert_eq!(attached.state.scroll, 2);
        }
        assert_eq!(
            parent.scroll, 7,
            "attached coalescing leaves main scroll alone"
        );
        assert!(matches!(
            events.next().await,
            Some(Ok(Event::Key(key))) if key == typing
        ));

        let release = KeyEvent::new_with_kind(
            KeyCode::Down,
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        );
        let mut events =
            futures_util::stream::iter(vec![Ok::<_, io::Error>(Event::Key(release))]).peekable();
        drain_ready_scroll_keys(&mut events, &mut state).await;
        assert!(matches!(
            events.next().await,
            Some(Ok(Event::Key(key))) if key == release
        ));
    }

    #[test]
    fn wide_scroll_line_forces_its_following_visible_cell_to_update() {
        let backend = ratatui::backend::TestBackend::new(12, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::default();
        state.push_line("你好".into(), LineKind::Normal);

        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 0)].diff_option, CellDiffOption::AlwaysUpdate);
        assert_eq!(buffer[(4, 0)].diff_option, CellDiffOption::AlwaysUpdate);
        assert_eq!(buffer[(1, 0)].diff_option, CellDiffOption::None);
    }

    #[test]
    fn cursor_sits_at_the_insertion_point() {
        let backend = ratatui::backend::TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::default();
        state.input.insert("abc");
        state.input.left();
        state.input.left();
        draw(&mut terminal, &mut state).unwrap();
        let position = terminal.backend().cursor_position();
        // border at column 0, content starts at column 1, cursor between a/b -> on 'b'
        assert_eq!((position.x, position.y), (2, 8));

        state.input.end();
        draw(&mut terminal, &mut state).unwrap();
        let position = terminal.backend().cursor_position();
        // end of text: one cell past 'c', which is where the next char lands
        assert_eq!((position.x, position.y), (4, 8));
    }

    #[test]
    fn draw_paints_the_entire_terminal_with_theme_backgrounds() {
        let backend = ratatui::backend::TestBackend::new(32, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::default();
        state.push_line("assistant text".into(), LineKind::Normal);
        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content().iter().all(|cell| cell.bg != Color::Reset),
            "every cell should have an explicit Solarized surface"
        );
        assert_eq!(buffer[(0, 0)].bg, SOLARIZED_LIGHT.background);
        assert_eq!(buffer[(0, 9)].bg, SOLARIZED_LIGHT.panel);
    }

    #[test]
    fn scrolling_redraw_keeps_blank_scrollback_cells_on_solarized_surfaces() {
        let backend = ratatui::backend::TestBackend::new(12, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "  leading and trailing  ".into(),
                    kind: LineKind::Normal,
                },
                DisplayLine {
                    text: "   ".into(),
                    kind: LineKind::ToolCall,
                },
                DisplayLine {
                    text: "+    7 wrapped diff text  ".into(),
                    kind: LineKind::Added,
                },
                DisplayLine {
                    text: "-    8 trailing   ".into(),
                    kind: LineKind::Removed,
                },
            ],
            ..Default::default()
        };

        state.follow();
        draw(&mut terminal, &mut state).unwrap();
        state.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        draw(&mut terminal, &mut state).unwrap();
        assert_eq!(state.scroll, 0);
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            SOLARIZED_LIGHT.background,
            "the leading blank cell on a normal line is repainted"
        );
        assert_eq!(
            terminal.backend().buffer()[(0, 2)].bg,
            SOLARIZED_LIGHT.element,
            "all-space tool line has its semantic surface"
        );

        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        draw(&mut terminal, &mut state).unwrap();
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        draw(&mut terminal, &mut state).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            SOLARIZED_LIGHT.element,
            "scrolling reuses the terminal and repaints the all-space row"
        );

        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        draw(&mut terminal, &mut state).unwrap();
        state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        draw(&mut terminal, &mut state).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            SOLARIZED_LIGHT.background,
            "scrolling back up clears the tool surface from the leading blank cell"
        );
        state.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        draw(&mut terminal, &mut state).unwrap();

        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .chunks(12)
                .take(5)
                .flatten()
                .all(|cell| cell.bg != Color::Reset),
            "every output scrollback cell stays on an explicit Solarized surface after scrolling"
        );
    }

    #[test]
    fn attached_view_paints_input_corners_like_the_main_view() {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let (handle, sink, _source) = crate::handle::session_channel();
        sink.emit(AgentEvent::Usage {
            context_input: 1_500,
            session: crate::agent::Usage {
                input_tokens: 1_500,
                output_tokens: 0,
            },
        });
        let mut state = TuiState::default();
        state.attach(
            7,
            "demo task".into(),
            Arc::new(handle),
            "deepseek-v4-flash".into(),
            Some("fixer".into()),
            "/repo".into(),
            "sub-abc123".into(),
        );

        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..60)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let bottom = row_text(11);
        assert!(
            bottom.contains("deepseek-v4-flash · fixer"),
            "bottom-left shows model · role, got: {bottom:?}"
        );
        assert!(
            bottom.contains("/repo · ctx 1.5k"),
            "bottom-right shows cwd · ctx, got: {bottom:?}"
        );
        let top = row_text(9);
        assert!(
            top.contains("sub-abc123"),
            "top-right shows the session id, got: {top:?}"
        );
    }

    #[test]
    fn cwd_usage_text_shortens_home_to_tilde() {
        let home = std::env::var_os("HOME").expect("tests run with HOME set");
        let home = home.to_string_lossy().into_owned();
        assert_eq!(
            cwd_usage_text(&format!("{home}/work"), 0),
            "~/work",
            "paths under $HOME collapse to ~"
        );
        assert_eq!(cwd_usage_text(&home, 0), "~", "$HOME itself collapses to ~");
        assert_eq!(
            cwd_usage_text("/elsewhere", 0),
            "/elsewhere",
            "paths outside $HOME stay absolute"
        );
        assert_eq!(
            cwd_usage_text(&format!("{home}x"), 0),
            format!("{home}x"),
            "a sibling sharing the prefix is not shortened"
        );
        assert_eq!(
            cwd_usage_text(&format!("{home}/work"), 1_500),
            "~/work · ctx 1.5k",
            "token count still appended after shortening"
        );
    }

    #[tokio::test]
    async fn tasks_panel_tags_rows_with_the_agent_role() {
        let backend = ratatui::backend::TestBackend::new(50, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_, mut background) =
            crate::tools::builtins(crate::workspace::Workspace::new(".").unwrap());
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        background.set_event_sender(sender);
        background
            .spawn_with_id(
                "find the render site".into(),
                Some("explorer".into()),
                None,
                |_| {},
                || async { "done".into() },
            )
            .unwrap();
        background
            .spawn_with_id(
                "sleep 5".into(),
                None,
                None,
                |_| {},
                || async { "done".into() },
            )
            .unwrap();
        let mut state = TuiState {
            background: Some(background),
            show_tasks: true,
            ..Default::default()
        };

        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            text.contains("[explorer] find the render site"),
            "role-tagged row, got: {text:?}"
        );
        assert!(
            text.contains("sleep 5"),
            "untagged bash row still shows its label"
        );
        assert!(
            !text.contains("[] sleep 5"),
            "bash row has no empty role tag"
        );
    }

    #[test]
    fn draw_renders_normal_lines_as_markdown() {
        // A heading, an inline-code line, and a fenced code block all get
        // their markdown styles, and every painted cell keeps an explicit
        // Solarized background (no Reset leaking through the markdown path).
        let backend = ratatui::backend::TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::default();
        state.push_line("## Summary".into(), LineKind::Normal);
        state.push_line("run `cargo test` now".into(), LineKind::Normal);
        state.push_line("```".into(), LineKind::Normal);
        state.push_line("let x = 1;".into(), LineKind::Normal);
        state.push_line("```".into(), LineKind::Normal);
        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content().iter().all(|cell| cell.bg != Color::Reset),
            "markdown path must keep explicit Solarized backgrounds"
        );
        // The code-block line sits on the element (panel) background.
        let code_row_y = 3; // "## Summary", inline line, fence, then code
        assert_eq!(buffer[(0, code_row_y)].bg, SOLARIZED_LIGHT.element);
    }

    #[test]
    fn markdown_preserves_embedded_newlines_across_a_code_block() {
        // A Normal line embedding newlines must render one visual row per
        // segment (no collapsed newlines), and the code fence opened on one
        // segment still applies to the next.
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::default();
        state.push_line(
            "before\n```\ncode line\n```\nafter".into(),
            LineKind::Normal,
        );
        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..40)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .trim_end()
                .to_owned()
        };
        assert_eq!(row_text(0), "before");
        assert_eq!(row_text(1), "```");
        assert_eq!(row_text(2), "code line");
        assert_eq!(row_text(3), "```");
        assert_eq!(row_text(4), "after");
        // The code line keeps the panel background from the still-open fence.
        assert_eq!(buffer[(0, 2)].bg, SOLARIZED_LIGHT.element);
        // Lines after the fence close are back on the body background.
        assert_eq!(buffer[(0, 4)].bg, SOLARIZED_LIGHT.background);
    }

    #[test]
    fn draw_uses_semantic_solarized_message_styles() {
        let backend = ratatui::backend::TestBackend::new(50, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "normal".into(),
                    kind: LineKind::Normal,
                },
                DisplayLine {
                    text: "dim".into(),
                    kind: LineKind::Dim,
                },
                DisplayLine {
                    text: "user".into(),
                    kind: LineKind::User,
                },
                DisplayLine {
                    text: "tool".into(),
                    kind: LineKind::ToolCall,
                },
                DisplayLine {
                    text: "ok".into(),
                    kind: LineKind::ToolResult,
                },
                DisplayLine {
                    text: "error".into(),
                    kind: LineKind::ToolError,
                },
                DisplayLine {
                    text: "+    7 added".into(),
                    kind: LineKind::Added,
                },
                DisplayLine {
                    text: "-    7 removed".into(),
                    kind: LineKind::Removed,
                },
            ],
            ..Default::default()
        };
        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, SOLARIZED_LIGHT.text);
        assert_eq!(buffer[(0, 1)].fg, SOLARIZED_LIGHT.muted);
        assert!(buffer[(0, 1)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(0, 2)].fg, SOLARIZED_LIGHT.cyan);
        assert_eq!(buffer[(0, 3)].bg, SOLARIZED_LIGHT.element);
        assert_eq!(buffer[(0, 3)].fg, SOLARIZED_LIGHT.violet);
        assert_eq!(buffer[(0, 4)].fg, SOLARIZED_LIGHT.green);
        assert_eq!(buffer[(0, 5)].fg, SOLARIZED_LIGHT.red);
        assert_eq!(buffer[(7, 6)].bg, SOLARIZED_LIGHT.diff_added_background);
        assert_eq!(
            buffer[(0, 6)].bg,
            SOLARIZED_LIGHT.diff_added_line_number_background
        );
        assert_eq!(buffer[(7, 7)].bg, SOLARIZED_LIGHT.diff_removed_background);
        assert_eq!(
            buffer[(0, 7)].bg,
            SOLARIZED_LIGHT.diff_removed_line_number_background
        );
    }

    #[test]
    fn queued_bar_is_bold_high_contrast_and_fills_its_row() {
        let backend = ratatui::backend::TestBackend::new(50, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState::default();
        state.queued.push("follow up".into());
        draw(&mut terminal, &mut state).unwrap();

        let buffer = terminal.backend().buffer();
        // input is three rows high; with one queue row, the banner sits at y=6.
        let first = &buffer[(0, 6)];
        assert_eq!(first.fg, SOLARIZED_LIGHT.ink);
        assert_eq!(first.bg, SOLARIZED_LIGHT.blue);
        assert!(first.modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(49, 6)].bg, SOLARIZED_LIGHT.blue);
    }

    #[test]
    fn input_title_distinguishes_compaction_from_model_thinking() {
        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState {
            busy: Some(BusyState::thinking()),
            ..Default::default()
        };
        draw(&mut terminal, &mut state).unwrap();
        let thinking_title: String = terminal.backend().buffer().content()[7 * 40..8 * 40]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(thinking_title.contains("thinking…"));
        assert!(thinking_title.contains(BusyState::SPINNER[0].to_string().as_str()));
        assert!(!thinking_title.contains("compaction…"));

        state.busy = Some(BusyState::compacting());
        draw(&mut terminal, &mut state).unwrap();
        let compaction_title: String = terminal.backend().buffer().content()[7 * 40..8 * 40]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(compaction_title.contains("compaction…"));
        assert!(!compaction_title.contains("thinking…"));
    }

    #[test]
    fn input_edits_unicode_at_character_boundaries() {
        let mut input = InputBuffer::default();
        input.insert("你好a");
        input.left();
        input.backspace();
        input.insert("世");
        input.end();
        input.delete();
        assert_eq!(input.text, "你世a");
        assert_eq!(input.cursor, 3);
    }

    #[test]
    fn unicode_cursor_uses_terminal_display_width() {
        let mut input = InputBuffer::default();
        input.insert("你好a");
        assert_eq!(input.wrapped_cursor(80), (0, 5));
    }

    #[test]
    fn reasoning_and_content_deltas_use_separate_lines_without_final_duplicate() {
        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::ReasoningDelta("plan".into()));
        state.push_agent_event(AgentEvent::ReasoningDelta(" more".into()));
        state.push_agent_event(AgentEvent::AssistantDelta("hel".into()));
        state.push_agent_event(AgentEvent::AssistantDelta("lo".into()));
        state.push_final_answer("hello".into());
        assert_eq!(state.lines.len(), 2);
        assert_eq!(state.lines[0].text, "thinking: plan more");
        assert_eq!(state.lines[0].kind, LineKind::Thinking);
        assert_eq!(state.lines[1].text, "hello");
        assert_eq!(state.lines[1].kind, LineKind::Normal);
    }

    #[test]
    fn empty_content_delta_does_not_split_the_reasoning_line() {
        // kimi interleaves empty `content: ""` chunks into the reasoning
        // stream. Each one must NOT flip the active lane, or the reasoning
        // text scatters across many `thinking: ` lines.
        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::AssistantDelta("".into()));
        state.push_agent_event(AgentEvent::ReasoningDelta("plan".into()));
        state.push_agent_event(AgentEvent::AssistantDelta("".into()));
        state.push_agent_event(AgentEvent::ReasoningDelta(" more".into()));
        let thinking: Vec<_> = state
            .lines
            .iter()
            .filter(|line| line.kind == LineKind::Thinking)
            .collect();
        assert_eq!(thinking.len(), 1, "reasoning must stay on one line");
        assert_eq!(thinking[0].text, "thinking: plan more");
    }

    #[test]
    fn reply_after_plain_text_turn_does_not_append_to_the_user_line() {
        // A turn ending on a plain text answer leaves active_lane = Content
        // (no ToolCall/ToolResult reset it). run_request clears it per turn;
        // simulate that reset, then the next turn's first delta must start a
        // fresh Normal line instead of appending onto the `you> …` line.
        let mut state = TuiState::default();
        // turn 1: plain text answer, lane left as Content.
        state.push_agent_event(AgentEvent::AssistantDelta("first answer".into()));
        assert_eq!(state.active_lane, Some(ActiveStreamLane::Content));
        // user submits the next prompt; run_request then resets the lane.
        state.push_line("you> next question".into(), LineKind::User);
        state.streamed = false;
        state.active_lane = None; // the per-turn reset in run_request
        // turn 2's first delta must open its own line.
        state.push_agent_event(AgentEvent::AssistantDelta("second answer".into()));
        assert_eq!(state.lines[1].text, "you> next question");
        assert_eq!(state.lines[1].kind, LineKind::User);
        assert_eq!(state.lines[2].text, "second answer");
        assert_eq!(state.lines[2].kind, LineKind::Normal);
    }

    #[test]
    fn without_a_lane_reset_the_reply_would_append_to_the_user_line() {
        // Pin the failure mode the run_request reset prevents: a stale
        // Content lane makes the next delta append onto the `you> …` line,
        // keeping its User kind (the "reply dyed as user input" bug).
        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::AssistantDelta("first answer".into()));
        state.push_line("you> next question".into(), LineKind::User);
        // NO lane reset (the bug): the stale Content lane appends.
        state.push_agent_event(AgentEvent::AssistantDelta("second".into()));
        assert_eq!(state.lines[1].text, "you> next questionsecond");
        assert_eq!(state.lines[1].kind, LineKind::User);
    }

    #[test]
    fn restored_transcript_is_replayed_as_tui_lines() {
        let messages = vec![
            Message::User {
                content: "hello".into(),
            },
            Message::Assistant(crate::agent::AssistantMessage {
                content: Some("checking".into()),
                tool_calls: vec![crate::agent::ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"README.md"}"#.into(),
                }],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "read_file".into(),
                content: "contents".into(),
                is_error: false,
            },
            Message::Assistant(crate::agent::AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        ];
        let state =
            TuiState::from_history(&messages.into_iter().map(Into::into).collect::<Vec<_>>());
        let lines: Vec<_> = state.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            lines,
            [
                "you> hello",
                "checking",
                r#"tool: read_file {"path":"README.md"}"#,
                "  ok: contents",
                "done",
            ]
        );
        assert_eq!(state.scroll, usize::MAX);
    }

    #[test]
    fn new_events_do_not_yank_a_scrolled_up_view() {
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "one".into(),
                    kind: LineKind::Normal,
                },
                DisplayLine {
                    text: "two".into(),
                    kind: LineKind::Normal,
                },
            ],
            max_scroll: 2,
            ..Default::default()
        };
        state.scroll = 0;
        state.push_agent_event(AgentEvent::AssistantText("three".into()));
        assert_eq!(state.scroll, 0);
        state.scroll = state.max_scroll;
        state.push_agent_event(AgentEvent::AssistantText("four".into()));
        assert_eq!(state.scroll, usize::MAX);
    }

    #[test]
    fn home_and_end_jump_to_session_edges() {
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "one".into(),
                    kind: LineKind::Normal,
                },
                DisplayLine {
                    text: "two".into(),
                    kind: LineKind::Normal,
                },
            ],
            max_scroll: 42,
            ..Default::default()
        };
        state.scroll = 10;
        state.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(state.scroll, 0);
        state.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(state.scroll, usize::MAX);
        state.input.insert("xy");
        state.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(state.input.cursor, 0);
        state.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(state.input.cursor, 2);
    }

    #[test]
    fn scrolling_is_bounded_and_events_append_echo_lines() {
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "one".into(),
                    kind: LineKind::Normal,
                },
                DisplayLine {
                    text: "two".into(),
                    kind: LineKind::Normal,
                },
            ],
            max_scroll: 2,
            ..Default::default()
        };
        state.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(state.scroll, 2);
        state.push_agent_event(AgentEvent::ToolResult {
            is_error: false,
            content: "done".into(),
        });
        assert_eq!(state.lines.last().unwrap().text, "  ok: done");
        assert_eq!(state.lines.last().unwrap().kind, LineKind::ToolResult);
        assert_eq!(state.scroll, usize::MAX);

        state.push_agent_event(AgentEvent::ToolResult {
            is_error: true,
            content: "failed".into(),
        });
        assert_eq!(state.lines.last().unwrap().text, "  error: failed");
        assert_eq!(state.lines.last().unwrap().kind, LineKind::ToolError);
    }

    #[test]
    fn wrapped_rows_counts_embedded_newlines_and_wrapping() {
        let lines = vec![
            DisplayLine {
                text: "short".into(),
                kind: LineKind::Normal,
            },
            DisplayLine {
                text: "one\ntwo\nthree".into(),
                kind: LineKind::Normal,
            },
            DisplayLine {
                text: "x".repeat(25),
                kind: LineKind::Normal,
            },
        ];
        // 1 + 3 + exact hard wrap of 25 cells at width 10
        assert_eq!(wrapped_rows(&lines, 10), 1 + 3 + 3);
        let cjk = vec![DisplayLine {
            text: "你好世界".into(),
            kind: LineKind::Normal,
        }];
        // Each CJK char is 2 cells: "你好" then "世界" at width 5.
        assert_eq!(wrapped_rows(&cjk, 5), 2);
    }

    #[test]
    fn prompts_submitted_while_thinking_queue_and_drain_in_order() {
        let mut state = TuiState::default();
        state.input.insert("first");
        let first = state.take_input().unwrap();
        state.queued.push(first);
        state.input.insert("second");
        let second = state.take_input().unwrap();
        state.queued.push(second);
        assert!(state.input.text.is_empty());
        assert_eq!(state.next_queued().unwrap(), "first");
        assert_eq!(state.next_queued().unwrap(), "second");
        assert!(state.next_queued().is_none());
    }

    #[test]
    fn edit_file_tool_calls_render_as_a_numbered_diff_on_result() {
        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::ToolCall {
            name: "edit_file".into(),
            arguments: r#"{"path":"src/a.rs","old":"fn a() {}\nfn b() {}","new":"fn a() { 1 }"}"#
                .into(),
        });
        assert_eq!(state.lines.len(), 1);
        assert_eq!(state.lines[0].text, "tool: edit_file src/a.rs");
        assert_eq!(state.lines[0].kind, LineKind::ToolCall);
        state.push_agent_event(AgentEvent::ToolResult {
            is_error: false,
            content: "file edited (line 7)".into(),
        });
        let lines: Vec<_> = state
            .lines
            .iter()
            .map(|line| (line.text.as_str(), line.kind))
            .collect();
        assert_eq!(
            lines,
            [
                ("tool: edit_file src/a.rs", LineKind::ToolCall),
                ("-    7 fn a() {}", LineKind::Removed),
                ("-    8 fn b() {}", LineKind::Removed),
                ("+    7 fn a() { 1 }", LineKind::Added),
            ]
        );

        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::ToolCall {
            name: "edit_file".into(),
            arguments: "not json".into(),
        });
        assert_eq!(state.lines.len(), 1);
        assert!(state.lines[0].text.starts_with("tool: edit_file not json"));
        assert_eq!(state.lines[0].kind, LineKind::ToolCall);
    }

    #[test]
    fn replay_marks_internal_messages_and_reasoning_dim() {
        let entries = vec![
            SessionEntry::Notice {
                text: "[background task 1 completed]\nexit code: 0".into(),
            },
            SessionEntry::Notice {
                text: "[compacted summary of earlier conversation]\nwe did things".into(),
            },
            SessionEntry::Message {
                message: Message::Assistant(crate::agent::AssistantMessage {
                    content: Some("answer".into()),
                    tool_calls: vec![],
                    reasoning: Some("plan".into()),
                }),
            },
        ];
        let state = TuiState::from_history(&entries);
        assert_eq!(state.lines.len(), 4);
        assert_eq!(
            state.lines[0].kind,
            LineKind::Dim,
            "background completion stays dim"
        );
        assert_eq!(
            state.lines[1].kind,
            LineKind::Dim,
            "compacted summary stays a muted notice"
        );
        assert_eq!(state.lines[2].text, "thinking: plan");
        assert_eq!(state.lines[2].kind, LineKind::Thinking);
        assert_eq!(state.lines[3].text, "answer");
        assert_eq!(state.lines[3].kind, LineKind::Normal);
    }

    #[test]
    fn replay_shows_compaction_entries_as_a_banner_and_summary_line() {
        let entries = vec![
            SessionEntry::Message {
                message: Message::User {
                    content: "old work".into(),
                },
            },
            SessionEntry::Compaction {
                summary: "summary of old work".into(),
                retained: vec![Message::User {
                    content: "kept".into(),
                }],
            },
            SessionEntry::Message {
                message: Message::User {
                    content: "new work".into(),
                },
            },
        ];
        let state = TuiState::from_history(&entries);
        let lines: Vec<_> = state
            .lines
            .iter()
            .map(|line| (line.text.as_str(), line.kind))
            .collect();
        assert_eq!(
            lines,
            [
                ("you> old work", LineKind::User),
                ("──── compaction ────", LineKind::Compaction),
                ("compacted: summary of old work", LineKind::Dim),
                ("you> new work", LineKind::User),
            ],
            "retained tail must not be duplicated in the scrollback"
        );
    }
}

#[cfg(test)]
mod ux_tests {
    use super::*;

    #[test]
    fn session_events_advance_the_busy_spinner_frame() {
        let mut state = TuiState {
            busy: Some(BusyState::thinking()),
            ..Default::default()
        };
        assert_eq!(state.busy.unwrap().frame, 0);
        state.push_agent_event(AgentEvent::ReasoningDelta("hmm".into()));
        assert_eq!(state.busy.unwrap().frame, 1);
        state.push_agent_event(AgentEvent::AssistantDelta("hi".into()));
        assert_eq!(state.busy.unwrap().frame, 2);
        assert_ne!(
            BusyState {
                kind: BusyKind::Thinking,
                frame: 0
            }
            .title(),
            BusyState {
                kind: BusyKind::Thinking,
                frame: 1
            }
            .title(),
            "the spinner glyph changes as events stream in"
        );
        state.busy = None;
        state.push_agent_event(AgentEvent::AssistantDelta("done".into()));
        assert!(state.busy.is_none(), "idle views have nothing to spin");
    }

    #[test]
    fn spinner_frames_are_per_view_not_global() {
        let (handle, _sink, _source) = crate::handle::session_channel();
        let mut parent = TuiState {
            busy: Some(BusyState::thinking()),
            ..Default::default()
        };
        parent.attach(
            1,
            "task".into(),
            Arc::new(handle),
            String::new(),
            None,
            String::new(),
            String::new(),
        );
        let attached = parent.attached.as_mut().unwrap();
        attached.state.busy = Some(BusyState::thinking());
        attached
            .state
            .push_agent_event(AgentEvent::AssistantDelta("sub".into()));
        assert_eq!(
            parent.attached.as_ref().unwrap().state.busy.unwrap().frame,
            1
        );
        assert_eq!(
            parent.busy.unwrap().frame,
            0,
            "attached-session events do not move the main spinner"
        );
    }

    #[test]
    fn cancelling_collapses_queued_prompts_into_one_turn() {
        let mut state = TuiState {
            queued: vec!["one".into(), "two".into(), "three".into()],
            ..Default::default()
        };
        state.collapse_queue();
        assert_eq!(state.queued, vec!["one\n\ntwo\n\nthree".to_owned()]);
        // A single queued prompt (or none) is left alone.
        let mut state = TuiState {
            queued: vec!["only".into()],
            ..Default::default()
        };
        state.collapse_queue();
        assert_eq!(state.queued, vec!["only".to_owned()]);
    }

    #[test]
    fn cwd_shows_in_the_input_border_before_any_token_usage() {
        let backend = ratatui::backend::TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = TuiState {
            cwd: "/home/me/project".into(),
            tokens_context: 0,
            ..Default::default()
        };
        draw(&mut terminal, &mut state).unwrap();
        let bottom: String = terminal.backend().buffer().content()[9 * 60..10 * 60]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(bottom.contains("/home/me/project"), "bottom row: {bottom}");
        assert!(!bottom.contains("ctx"), "bottom row: {bottom}");

        state.tokens_context = 1234;
        draw(&mut terminal, &mut state).unwrap();
        let bottom: String = terminal.backend().buffer().content()[9 * 60..10 * 60]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(bottom.contains("ctx 1.2k"), "bottom row: {bottom}");
    }

    #[test]
    fn pasted_crlf_is_normalized_to_plain_newlines() {
        let mut input = InputBuffer::default();
        let pasted = "one\r\ntwo\rthree"
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        input.insert(&pasted);
        assert_eq!(input.text, "one\ntwo\nthree");
        assert_eq!(input.visual_rows(40), 3);
    }

    #[test]
    fn alt_enter_inserts_a_newline_instead_of_submitting() {
        let mut state = TuiState::default();
        state.input.insert("first");
        let submitted = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert!(submitted.is_none());
        assert_eq!(state.input.text, "first\n");
        let submitted = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(submitted, Some("first\n".to_owned()));
    }
}
