use std::io;
use std::path::PathBuf;

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
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::{Agent, AgentEvent, Message, preview};
use crate::session::Session;

pub async fn run(mut agent: Agent, root: PathBuf, session_name: String) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    run_inner(&mut terminal, &mut agent, &root, &session_name).await
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
    session_name: &str,
) -> anyhow::Result<()> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let event_sender = sender.clone();
    agent.set_event_handler(Box::new(move |event| {
        let _ = event_sender.send(event);
    }));
    let mut events = EventStream::new();
    let mut state = TuiState::from_transcript(agent.transcript());
    loop {
        draw(terminal, &mut state)?;
        match events.next().await {
            Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                if is_exit(key) {
                    return Ok(());
                }
                if let Some(prompt) = state.handle_key(key) {
                    if prompt == "/compact" {
                        state.thinking = true;
                        draw(terminal, &mut state)?;
                        let result = agent.compact().await;
                        state.thinking = false;
                        match result {
                            Ok(summary) => state.push_line(
                                format!("compacted: {}", preview(&summary, 500)),
                                LineKind::Normal,
                            ),
                            Err(error) => {
                                state.push_line(format!("error: {error:#}"), LineKind::Normal)
                            }
                        }
                        Session::save(root, session_name, agent.transcript())?;
                        state.follow();
                        continue;
                    }
                    state.push_line(format!("you> {prompt}"), LineKind::Normal);
                    state.follow();
                    agent.subscribe(sender.clone());
                    if run_request(
                        terminal,
                        agent,
                        &mut state,
                        &mut events,
                        &mut receiver,
                        (root, session_name),
                        prompt,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
            }
            Some(Ok(Event::Paste(text))) => state.input.insert(&text),
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error.into()),
            None => return Ok(()),
        }
    }
}

async fn run_request(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: &mut Agent,
    state: &mut TuiState,
    events: &mut EventStream,
    receiver: &mut mpsc::UnboundedReceiver<AgentEvent>,
    session: (&std::path::Path, &str),
    prompt: String,
) -> anyhow::Result<bool> {
    state.thinking = true;
    state.streamed = false;
    draw(terminal, state)?;
    let (result, exit) = {
        let request = agent.run(prompt);
        tokio::pin!(request);
        loop {
            tokio::select! {
                result = &mut request => break (Some(result), false),
                Some(event) = receiver.recv() => {
                    state.push_agent_event(event);
                    draw(terminal, state)?;
                }
                event = events.next() => match event {
                    Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press && is_exit(key) => break (None, true),
                    Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                        state.handle_scroll(key);
                        draw(terminal, state)?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break (Some(Err(error.into())), true),
                    None => break (None, true),
                }
            }
        }
    };
    state.thinking = false;
    while let Ok(event) = receiver.try_recv() {
        state.push_agent_event(event);
    }
    if let Some(result) = result {
        match result {
            Ok(answer) => state.push_final_answer(answer),
            Err(error) => state.push_line(format!("error: {error:#}"), LineKind::Normal),
        }
    }
    Session::save(session.0, session.1, agent.transcript())?;
    state.follow();
    draw(terminal, state)?;
    Ok(exit)
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut TuiState,
) -> io::Result<()> {
    terminal
        .draw(|frame| {
            let [output, input] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());
            let inner_width = usize::from(output.width).max(1);
            let visual: Vec<Line> = state
                .lines
                .iter()
                .flat_map(|line| {
                    let style = match line.kind {
                        LineKind::Normal => Style::default(),
                        LineKind::Dim => Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                        LineKind::Added => Style::default().bg(Color::Rgb(20, 60, 20)),
                        LineKind::Removed => Style::default().bg(Color::Rgb(70, 20, 20)),
                    };
                    hard_wrap(&line.text, inner_width)
                        .into_iter()
                        .map(move |row| Line::styled(row, style))
                })
                .collect();
            let total_rows = visual.len();
            let paragraph = Paragraph::new(visual);
            let height = usize::from(output.height);
            let max_scroll = total_rows.saturating_sub(height);
            state.max_scroll = max_scroll;
            state.scroll = state.scroll.min(max_scroll);
            frame.render_widget(
                paragraph.scroll((state.scroll.min(u16::MAX as usize) as u16, 0)),
                output,
            );
            let title = if state.thinking {
                "thinking…"
            } else {
                "input"
            };
            frame.render_widget(
                Paragraph::new(state.input.text.as_str())
                    .block(Block::default().borders(Borders::ALL).title(title))
                    .wrap(Wrap { trim: false }),
                input,
            );
            if !state.thinking {
                frame.set_cursor_position((
                    input.x + 1 + state.input.display_width().min(u16::MAX as usize) as u16,
                    input.y + 1,
                ));
            }
        })
        .map(|_| ())
}

fn is_exit(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
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
    input: InputBuffer,
    lines: Vec<DisplayLine>,
    scroll: usize,
    max_scroll: usize,
    thinking: bool,
    streamed: bool,
    active_lane: Option<ActiveStreamLane>,
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
}

#[derive(Clone, Copy, PartialEq)]
enum ActiveStreamLane {
    Reasoning,
    Content,
}

impl TuiState {
    fn from_transcript(messages: &[Message]) -> Self {
        let mut state = Self::default();
        for message in messages {
            match message {
                Message::User { content } => {
                    if content.starts_with("[compacted summary of earlier conversation]") {
                        state.push_line(content.clone(), LineKind::Normal);
                    } else if content.starts_with("[background task ") {
                        state.push_line(content.clone(), LineKind::Dim);
                    } else {
                        state.push_line(format!("you> {content}"), LineKind::Normal);
                    }
                }
                Message::Assistant(message) => {
                    if let Some(reasoning) =
                        message.reasoning.as_deref().filter(|text| !text.is_empty())
                    {
                        state.push_line(
                            format!("thinking: {}", preview(reasoning, 1000)),
                            LineKind::Dim,
                        );
                    }
                    if let Some(content) =
                        message.content.as_deref().filter(|text| !text.is_empty())
                    {
                        state.push_line(content.to_owned(), LineKind::Normal);
                    }
                    for call in &message.tool_calls {
                        state.push_tool_call(&call.name, &call.arguments);
                    }
                }
                Message::Tool {
                    content, is_error, ..
                } => state.push_line(
                    format!(
                        "  {}: {}",
                        if *is_error { "error" } else { "ok" },
                        preview(content, 500)
                    ),
                    LineKind::Normal,
                ),
            }
        }
        state.follow();
        state
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
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
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => self.handle_scroll(key),
            KeyCode::Enter => {
                let prompt = std::mem::take(&mut self.input.text);
                self.input.cursor = 0;
                return (!prompt.trim().is_empty()).then_some(prompt);
            }
            _ => {}
        }
        None
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

    fn push_agent_event(&mut self, event: AgentEvent) {
        let at_bottom = self.at_bottom();
        let ends_delta = matches!(
            &event,
            AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. }
        );
        match event {
            AgentEvent::AssistantText(text) => self.push_line(text, LineKind::Normal),
            AgentEvent::AssistantDelta(text) => {
                if self.active_lane == Some(ActiveStreamLane::Content) {
                    self.lines.last_mut().unwrap().text.push_str(&text);
                } else {
                    self.push_line(text, LineKind::Normal);
                    self.streamed = true;
                    self.active_lane = Some(ActiveStreamLane::Content);
                }
            }
            AgentEvent::ReasoningDelta(text) => {
                if self.active_lane == Some(ActiveStreamLane::Reasoning) {
                    self.lines.last_mut().unwrap().text.push_str(&text);
                } else {
                    self.push_line(format!("thinking: {text}"), LineKind::Dim);
                    self.active_lane = Some(ActiveStreamLane::Reasoning);
                }
            }
            AgentEvent::ToolCall { name, arguments } => self.push_tool_call(&name, &arguments),
            AgentEvent::ToolResult { is_error, content } => self.push_line(
                format!(
                    "  {}: {}",
                    if is_error { "error" } else { "ok" },
                    preview(&content, 500)
                ),
                LineKind::Normal,
            ),
            AgentEvent::BackgroundCompleted { id, output } => self.push_line(
                format!("background task {id} finished: {}", preview(&output, 500)),
                LineKind::Normal,
            ),
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
            self.push_line(format!("tool: edit_file {path}"), LineKind::Normal);
            self.push_diff_side(&old, "- ", LineKind::Removed);
            self.push_diff_side(&new, "+ ", LineKind::Added);
            return;
        }
        self.push_line(
            format!("tool: {name} {}", preview(arguments, 200)),
            LineKind::Normal,
        );
    }

    fn push_diff_side(&mut self, text: &str, prefix: &str, kind: LineKind) {
        const DIFF_LINE_LIMIT: usize = 30;
        let mut lines = text.lines();
        for line in lines.by_ref().take(DIFF_LINE_LIMIT) {
            self.push_line(format!("{prefix}{line}"), kind);
        }
        let remaining = lines.count();
        if remaining > 0 {
            self.push_line(format!("{prefix}… ({remaining} more lines)"), kind);
        }
    }
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

    fn display_width(&self) -> usize {
        UnicodeWidthStr::width(&self.text[..self.byte_index()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(input.display_width(), 5);
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
        assert_eq!(state.lines[0].kind, LineKind::Dim);
        assert_eq!(state.lines[1].text, "hello");
        assert_eq!(state.lines[1].kind, LineKind::Normal);
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
        let state = TuiState::from_transcript(&messages);
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
        assert_eq!(state.scroll, usize::MAX);
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
    fn edit_file_tool_calls_render_as_a_colored_diff() {
        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::ToolCall {
            name: "edit_file".into(),
            arguments: r#"{"path":"src/a.rs","old":"fn a() {}\nfn b() {}","new":"fn a() { 1 }"}"#
                .into(),
        });
        let lines: Vec<_> = state
            .lines
            .iter()
            .map(|line| (line.text.as_str(), line.kind))
            .collect();
        assert_eq!(
            lines,
            [
                ("tool: edit_file src/a.rs", LineKind::Normal),
                ("- fn a() {}", LineKind::Removed),
                ("- fn b() {}", LineKind::Removed),
                ("+ fn a() { 1 }", LineKind::Added),
            ]
        );

        let mut state = TuiState::default();
        state.push_agent_event(AgentEvent::ToolCall {
            name: "edit_file".into(),
            arguments: "not json".into(),
        });
        assert_eq!(state.lines.len(), 1);
        assert!(state.lines[0].text.starts_with("tool: edit_file not json"));
    }

    #[test]
    fn replay_marks_internal_messages_and_reasoning_dim() {
        let messages = vec![
            Message::User {
                content: "[background task 1 completed]\nexit code: 0".into(),
            },
            Message::User {
                content: "[compacted summary of earlier conversation]\nwe did things".into(),
            },
            Message::Assistant(crate::agent::AssistantMessage {
                content: Some("answer".into()),
                tool_calls: vec![],
                reasoning: Some("plan".into()),
            }),
        ];
        let state = TuiState::from_transcript(&messages);
        assert_eq!(state.lines.len(), 4);
        assert_eq!(
            state.lines[0].kind,
            LineKind::Dim,
            "background completion stays dim"
        );
        assert_eq!(
            state.lines[1].kind,
            LineKind::Normal,
            "compacted summary uses normal color"
        );
        assert_eq!(state.lines[2].text, "thinking: plan");
        assert_eq!(state.lines[2].kind, LineKind::Dim);
        assert_eq!(state.lines[3].text, "answer");
        assert_eq!(state.lines[3].kind, LineKind::Normal);
    }
}
