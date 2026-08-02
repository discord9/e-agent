use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use futures_util::StreamExt;

use super::*;

const TEXT_BURST_GAP: Duration = Duration::from_millis(4);
/// Enter waits a little longer for a following edit event. That is the
/// reliable signal that a terminal delivered a pasted newline as key events;
/// once seen, the wider window remains active for the rest of the batch.
const PASTE_FALLBACK_GAP: Duration = Duration::from_millis(30);

fn is_text_edit_kind(kind: crossterm::event::KeyEventKind) -> bool {
    matches!(
        kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    )
}

pub(crate) fn is_text_burst_start(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete | KeyCode::Enter
    ) && is_text_burst_key(key)
}

pub(crate) fn is_text_burst_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(_) => key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT,
        KeyCode::Backspace | KeyCode::Delete => key.modifiers.is_empty(),
        // Alt+Enter is already newline. CONTROL Enter is included because
        // Windows terminals can add CONTROL spuriously during a paste; it is
        // converted only after Enter is followed by another text edit event.
        KeyCode::Enter => matches!(
            key.modifiers,
            KeyModifiers::NONE | KeyModifiers::ALT | KeyModifiers::CONTROL
        ),
        _ => false,
    }
}

pub(crate) struct TextBurst {
    pub(crate) keys: Vec<KeyEvent>,
    pub(crate) cancelled: bool,
    /// True only when an Enter is followed by another text-edit event.
    pub(crate) fallback: bool,
}

/// Drain one text-edit batch before mutating live input. Ordinary characters
/// use the 4 ms coalescing window. An Enter (including the first event) waits
/// up to 30 ms for a following edit; that ordering confirms paste fallback
/// and switches the remainder of the batch to the same wider idle window.
/// Esc/Ctrl-C cancels and consumes any candidate batch, even one character.
pub(crate) async fn drain_text_burst<S>(
    first: KeyEvent,
    events: &mut futures_util::stream::Peekable<S>,
) -> TextBurst
where
    S: futures_util::Stream<Item = io::Result<Event>> + Unpin,
{
    let mut burst = TextBurst {
        keys: vec![first],
        cancelled: false,
        fallback: false,
    };
    loop {
        let after_enter = matches!(burst.keys.last().map(|key| key.code), Some(KeyCode::Enter));
        let gap = if burst.fallback || after_enter {
            PASTE_FALLBACK_GAP
        } else {
            TEXT_BURST_GAP
        };
        let Ok(next) = tokio::time::timeout(gap, std::pin::Pin::new(&mut *events).peek()).await
        else {
            break;
        };
        let Some(Ok(Event::Key(key))) = next else {
            break;
        };
        if !is_text_edit_kind(key.kind) {
            break;
        }
        if is_cancel(*key) || key.code == KeyCode::Esc {
            let _ = events.next().await;
            burst.cancelled = true;
            break;
        }
        if !is_text_burst_key(*key) {
            break;
        }
        let Some(Ok(Event::Key(key))) = events.next().await else {
            unreachable!("peeked text key must remain available")
        };
        // Enter followed by any accepted edit event is the reliable paste
        // signal. A trailing Enter therefore remains a normal submit.
        if after_enter {
            burst.fallback = true;
        }
        burst.keys.push(key);
    }
    burst
}

pub(crate) fn is_scroll_key(key: KeyEvent) -> bool {
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
pub(crate) async fn drain_ready_scroll_keys<S>(
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
pub(crate) fn is_exit(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc || is_cancel(key)
}

/// Keys that cancel an in-flight turn. Esc is intentionally NOT here: it is
/// reserved for "leave the current view" (detach from a subagent, close the
/// tasks panel) so its meaning is consistent everywhere.
pub(crate) fn is_cancel(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Safe terminal-title string: strip ASCII/C1 controls, collapse whitespace,
/// middle-ellipsis preview at 40 Unicode chars.
pub(crate) fn sanitize_title(raw: &str) -> String {
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
pub(crate) fn set_terminal_title(title: &str) {
    let _ = execute!(io::stdout(), SetTitle(format!("e-agent — {title}")));
}

pub(crate) fn parse_edited_line(content: &str) -> Option<usize> {
    content
        .strip_prefix("file edited (line ")?
        .strip_suffix(')')?
        .parse()
        .ok()
}

pub(crate) fn parse_edit_arguments(arguments: &str) -> Option<(String, String, String)> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    Some((
        value.get("path")?.as_str()?.to_owned(),
        value.get("old")?.as_str()?.to_owned(),
        value.get("new")?.as_str()?.to_owned(),
    ))
}
