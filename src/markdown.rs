//! Minimal markdown-to-spans rendering for the model's body text.
//!
//! NOT a CommonMark parser — just enough structure for model output to be
//! scannable at a glance: fenced code blocks get a panel background, ATX
//! headings are bold/blue, lists get a subtle bullet, and inline `code` /
//! **bold** / *italic* are styled. Everything is line-oriented except the
//! code-fence state, so it composes with the line-based scrollback. Zero
//! dependencies, styles drawn from the Solarized Light palette.
//!
//! Only `LineKind::Normal` (assistant body text) goes through this; other
//! kinds keep their flat styling.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Colors pulled from the Solarized Light palette (duplicated from tui.rs's
/// theme so this module stays self-contained and testable).
const TEXT: Color = Color::Rgb(0, 43, 54); // #002b36
const BACKGROUND: Color = Color::Rgb(253, 246, 227); // #fdf6e3
const ELEMENT: Color = Color::Rgb(238, 232, 213); // #eee8d5
const BLUE: Color = Color::Rgb(38, 139, 210); // #268bd2
const VIOLET: Color = Color::Rgb(108, 113, 196); // #6c71c4

/// Reusable style set so a whole logical line keeps one background (crucial
/// inside code blocks, where every span must share the panel background).
struct Styles {
    base: Style,
    code: Style,
    bold: Style,
    italic: Style,
}

impl Styles {
    fn body() -> Self {
        let base = Style::default().fg(TEXT).bg(BACKGROUND);
        Self {
            base,
            code: Style::default().fg(VIOLET).bg(ELEMENT),
            bold: base.add_modifier(Modifier::BOLD),
            italic: base.add_modifier(Modifier::ITALIC),
        }
    }

    fn code_block() -> Self {
        let base = Style::default().fg(TEXT).bg(ELEMENT);
        Self {
            base,
            code: base, // inside a fence, `` loses meaning; keep panel bg
            bold: base.add_modifier(Modifier::BOLD),
            italic: base.add_modifier(Modifier::ITALIC),
        }
    }
}

/// Stateful line renderer: only the code-fence state carries across lines.
pub struct MarkdownLines {
    in_code_block: bool,
}

impl Default for MarkdownLines {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownLines {
    pub fn new() -> Self {
        Self {
            in_code_block: false,
        }
    }

    /// Render one logical line to styled spans. Inline markdown is parsed
    /// outside code blocks only.
    pub fn render_line(&mut self, text: &str) -> Vec<Span<'static>> {
        let trimmed = text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            self.in_code_block = !self.in_code_block;
            // Show the fence itself dimmed on the panel color, as a delimiter.
            return vec![Span::styled(
                text.to_owned(),
                Style::default()
                    .fg(VIOLET)
                    .bg(ELEMENT)
                    .add_modifier(if rest.is_empty() {
                        Modifier::empty()
                    } else {
                        Modifier::BOLD
                    }),
            )];
        }
        if self.in_code_block {
            let styles = Styles::code_block();
            return vec![Span::styled(text.to_owned(), styles.base)];
        }

        let styles = Styles::body();
        // Heading: `#`..`######` followed by a space.
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            return vec![Span::styled(
                text.to_owned(),
                Style::default()
                    .fg(BLUE)
                    .bg(BACKGROUND)
                    .add_modifier(Modifier::BOLD),
            )];
        }
        // List marker: `- ` / `* ` / `+ ` or `N. ` / `N) `. Style the marker,
        // then inline-parse the rest.
        if let Some((marker, rest)) = split_list_marker(trimmed) {
            let mut spans = vec![Span::styled(
                marker.to_owned(),
                Style::default().fg(BLUE).bg(BACKGROUND),
            )];
            spans.extend(parse_inline(rest, &styles));
            return spans;
        }
        parse_inline(text, &styles)
    }
}

/// Split a list line into its marker (`- `, `1. `, …) and the remaining text.
fn split_list_marker(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        return Some((&text[..2], &text[2..]));
    }
    // Ordered: digits then '.' or ')' then space.
    let digits = text.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &text[digits..];
        let mut chars = rest.chars();
        if matches!(chars.next(), Some('.' | ')')) && chars.next() == Some(' ') {
            let marker_len = digits + 2;
            return Some((&text[..marker_len], &text[marker_len..]));
        }
    }
    None
}

/// Parse inline `code`, **bold**, and *italic* into styled spans. First
/// delimiter wins; an unmatched delimiter is emitted literally.
fn parse_inline(text: &str, styles: &Styles) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // Nearest upcoming delimiter of any kind.
        let next = [
            rest.find('`').map(|i| (i, "`")),
            rest.find("**").map(|i| (i, "**")),
            rest.find('*').map(|i| (i, "*")),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(i, _)| *i);
        let Some((index, marker)) = next else {
            spans.push(Span::styled(rest.to_owned(), styles.base));
            break;
        };
        if index > 0 {
            spans.push(Span::styled(rest[..index].to_owned(), styles.base));
        }
        let after = &rest[index + marker.len()..];
        match find_closing(after, marker) {
            Some(close) => {
                let style = match marker {
                    "`" => styles.code,
                    "**" => styles.bold,
                    _ => styles.italic,
                };
                spans.push(Span::styled(after[..close].to_owned(), style));
                rest = &after[close + marker.len()..];
            }
            None => {
                // Unmatched: emit the marker literally and continue after it.
                spans.push(Span::styled(marker.to_owned(), styles.base));
                rest = after;
            }
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), styles.base));
    }
    spans
}

/// Find where the closing `marker` begins in `text` (the content end).
fn find_closing(text: &str, marker: &str) -> Option<usize> {
    text.find(marker)
}

/// Wrap already-styled spans to `width` terminal columns, preserving each
/// span's style across the break points. Returns one `Line` per visual row.
pub fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Line> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let mut text = span.content.as_ref();
        loop {
            // Fit as much of `text` as possible onto the current row.
            let mut taken = 0usize;
            let mut taken_bytes = 0usize;
            for (byte_index, ch) in text.char_indices() {
                let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + taken + char_width > width {
                    break;
                }
                taken += char_width;
                taken_bytes = byte_index + ch.len_utf8();
            }
            if taken_bytes > 0 {
                current.push(Span::styled(text[..taken_bytes].to_owned(), span.style));
                used += taken;
                text = &text[taken_bytes..];
            }
            if text.is_empty() {
                break;
            }
            // Row full (or a zero-width remainder): start a new row.
            rows.push(Line::from(std::mem::take(&mut current)));
            used = 0;
            // A single char wider than the whole row: drop it to avoid a loop.
            if UnicodeWidthStr::width(text) > 0 && text.chars().next().is_some() {
                let first = text.chars().next().unwrap();
                if UnicodeWidthChar::width(first).unwrap_or(0) > width {
                    text = &text[first.len_utf8()..];
                    if text.is_empty() {
                        break;
                    }
                }
            }
        }
    }
    rows.push(Line::from(current));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles_of(spans: &[Span<'static>]) -> Vec<(String, Style)> {
        spans
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    #[test]
    fn heading_is_bold_blue() {
        let mut md = MarkdownLines::new();
        let spans = md.render_line("## Files changed");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "## Files changed");
        assert_eq!(spans[0].style.fg, Some(BLUE));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_code_bold_italic() {
        let mut md = MarkdownLines::new();
        let spans = md.render_line("use `foo` and **bar** and *baz*");
        let got = styles_of(&spans);
        let code = got.iter().find(|(t, _)| t == "foo").unwrap().1;
        assert_eq!(code.bg, Some(ELEMENT));
        let bold = got.iter().find(|(t, _)| t == "bar").unwrap().1;
        assert!(bold.add_modifier.contains(Modifier::BOLD));
        let italic = got.iter().find(|(t, _)| t == "baz").unwrap().1;
        assert!(italic.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn unmatched_marker_is_literal() {
        let mut md = MarkdownLines::new();
        let spans = md.render_line("a * b");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a * b");
    }

    #[test]
    fn code_block_state_and_background() {
        let mut md = MarkdownLines::new();
        md.render_line("```rust");
        let inside = md.render_line("let **x** = `raw`;");
        // Inside the fence no inline parsing, whole line on panel bg.
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].content, "let **x** = `raw`;");
        assert_eq!(inside[0].style.bg, Some(ELEMENT));
        md.render_line("```");
        let after = md.render_line("**bold** again");
        let bold = after.iter().find(|s| s.content == "bold").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(bold.style.bg, Some(BACKGROUND));
    }

    #[test]
    fn list_marker_is_split_and_styled() {
        let mut md = MarkdownLines::new();
        let spans = md.render_line("- **item** text");
        assert_eq!(spans[0].content, "- ");
        assert_eq!(spans[0].style.fg, Some(BLUE));
        assert!(
            spans
                .iter()
                .any(|s| s.content == "item" && s.style.add_modifier.contains(Modifier::BOLD))
        );

        let ordered = MarkdownLines::new().render_line("2. second");
        assert_eq!(ordered[0].content, "2. ");
    }

    #[test]
    fn wrap_spans_preserves_styles_across_breaks() {
        let spans = vec![
            Span::styled("plain ".to_owned(), Styles::body().base),
            Span::styled("BOLDBOLD".to_owned(), Styles::body().bold),
        ];
        // "plain BOLDBOLD" is 14 cols; width 9 wraps inside the bold span.
        let rows = wrap_spans(&spans, 9);
        let all_text: String = rows
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(all_text, "plain BOLDBOLD");
        // Every span that holds a piece of the bold word stays bold.
        for row in &rows {
            for span in &row.spans {
                if span.content.contains('B') || span.content.contains('O') {
                    assert!(span.style.add_modifier.contains(Modifier::BOLD));
                }
            }
        }
    }
}
