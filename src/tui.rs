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
use unicode_width::UnicodeWidthStr;

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
        draw(terminal, &state)?;
        match events.next().await {
            Some(Ok(Event::Key(key))) if key.kind == crossterm::event::KeyEventKind::Press => {
                if is_exit(key) {
                    return Ok(());
                }
                if let Some(prompt) = state.handle_key(key) {
                    if prompt == "/compact" {
                        state.thinking = true;
                        draw(terminal, &state)?;
                        let result = agent.compact().await;
                        state.thinking = false;
                        match result {
                            Ok(summary) => state
                                .push_line(format!("compacted: {}", preview(&summary, 500)), false),
                            Err(error) => state.push_line(format!("error: {error:#}"), false),
                        }
                        Session::save(root, session_name, agent.transcript())?;
                        state.follow();
                        continue;
                    }
                    state.push_line(format!("you> {prompt}"), false);
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
            Err(error) => state.push_line(format!("error: {error:#}"), false),
        }
    }
    Session::save(session.0, session.1, agent.transcript())?;
    state.follow();
    draw(terminal, state)?;
    Ok(exit)
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, state: &TuiState) -> io::Result<()> {
    terminal
        .draw(|frame| {
            let [output, input] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());
            frame.render_widget(
                Paragraph::new(
                    state
                        .lines
                        .iter()
                        .map(|line| {
                            Line::styled(
                                line.text.as_str(),
                                if line.dim {
                                    Style::default()
                                        .fg(Color::DarkGray)
                                        .add_modifier(Modifier::DIM)
                                } else {
                                    Style::default()
                                },
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .block(Block::default().borders(Borders::ALL).title("e-agent"))
                .wrap(Wrap { trim: false })
                .scroll((state.scroll.min(u16::MAX as usize) as u16, 0)),
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

#[derive(Default)]
struct TuiState {
    input: InputBuffer,
    lines: Vec<DisplayLine>,
    scroll: usize,
    thinking: bool,
    streamed: bool,
    active_lane: Option<ActiveStreamLane>,
}

struct DisplayLine {
    text: String,
    dim: bool,
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
                    state.push_line(format!("you> {content}"), false);
                }
                Message::Assistant(message) => {
                    if let Some(content) =
                        message.content.as_deref().filter(|text| !text.is_empty())
                    {
                        state.push_line(content.to_owned(), false);
                    }
                    for call in &message.tool_calls {
                        state.push_line(
                            format!("tool: {} {}", call.name, preview(&call.arguments, 200)),
                            false,
                        );
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
                    false,
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
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.handle_scroll(key)
            }
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
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1).min(self.lines.len()),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10).min(self.lines.len()),
            _ => {}
        }
    }

    fn at_bottom(&self) -> bool {
        self.scroll >= self.lines.len().saturating_sub(1)
    }

    fn push_agent_event(&mut self, event: AgentEvent) {
        let at_bottom = self.at_bottom();
        let ends_delta = matches!(
            &event,
            AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. }
        );
        match event {
            AgentEvent::AssistantText(text) => self.push_line(text, false),
            AgentEvent::AssistantDelta(text) => {
                if self.active_lane == Some(ActiveStreamLane::Content) {
                    self.lines.last_mut().unwrap().text.push_str(&text);
                } else {
                    self.push_line(text, false);
                    self.streamed = true;
                    self.active_lane = Some(ActiveStreamLane::Content);
                }
            }
            AgentEvent::ReasoningDelta(text) => {
                if self.active_lane == Some(ActiveStreamLane::Reasoning) {
                    self.lines.last_mut().unwrap().text.push_str(&text);
                } else {
                    self.push_line(format!("thinking: {text}"), true);
                    self.active_lane = Some(ActiveStreamLane::Reasoning);
                }
            }
            AgentEvent::ToolCall { name, arguments } => {
                self.push_line(format!("tool: {name} {}", preview(&arguments, 200)), false)
            }
            AgentEvent::ToolResult { is_error, content } => self.push_line(
                format!(
                    "  {}: {}",
                    if is_error { "error" } else { "ok" },
                    preview(&content, 500)
                ),
                false,
            ),
            AgentEvent::BackgroundCompleted { id, output } => self.push_line(
                format!("background task {id} finished: {}", preview(&output, 500)),
                false,
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
        self.scroll = self.lines.len().saturating_sub(1);
    }

    fn push_final_answer(&mut self, answer: String) {
        if !self.streamed {
            self.push_line(answer, false);
        }
    }

    fn push_line(&mut self, text: String, dim: bool) {
        self.lines.push(DisplayLine { text, dim });
        self.active_lane = None;
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
        assert!(state.lines[0].dim);
        assert_eq!(state.lines[1].text, "hello");
        assert!(!state.lines[1].dim);
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
        assert_eq!(state.scroll, state.lines.len() - 1);
    }

    #[test]
    fn new_events_do_not_yank_a_scrolled_up_view() {
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "one".into(),
                    dim: false,
                },
                DisplayLine {
                    text: "two".into(),
                    dim: false,
                },
            ],
            ..Default::default()
        };
        state.follow();
        state.scroll = 0;
        state.push_agent_event(AgentEvent::AssistantText("three".into()));
        assert_eq!(state.scroll, 0);
        state.scroll = state.lines.len() - 1;
        state.push_agent_event(AgentEvent::AssistantText("four".into()));
        assert_eq!(state.scroll, state.lines.len() - 1);
    }

    #[test]
    fn scrolling_is_bounded_and_events_append_echo_lines() {
        let mut state = TuiState {
            lines: vec![
                DisplayLine {
                    text: "one".into(),
                    dim: false,
                },
                DisplayLine {
                    text: "two".into(),
                    dim: false,
                },
            ],
            ..Default::default()
        };
        state.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(state.scroll, 2);
        state.push_agent_event(AgentEvent::ToolResult {
            is_error: false,
            content: "done".into(),
        });
        assert_eq!(state.lines.last().unwrap().text, "  ok: done");
        assert_eq!(state.scroll, 2);
    }
}
