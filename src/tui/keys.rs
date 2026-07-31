use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use futures_util::StreamExt;

use super::*;

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
