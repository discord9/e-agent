use unicode_width::UnicodeWidthChar;

use super::*;

/// Hard-wrap text at `width` terminal cells (char-boundary safe, CJK-aware).
/// Rendering and scroll accounting share this function so bottom-following
/// is exact instead of estimated.
pub(crate) fn hard_wrap(text: &str, width: usize) -> Vec<&str> {
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
pub(crate) fn wrapped_rows(lines: &[DisplayLine], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines.iter().map(|line| line_visual_rows(line, width)).sum()
}

#[derive(Clone)]
pub(crate) struct DisplayLine {
    pub(crate) text: String,
    pub(crate) kind: LineKind,
}

/// Rendering never materializes more than this much scrollback. Limits are
/// deliberately independent: a very long source line cannot evade the byte
/// cap, and many short lines cannot evade either the line or row cap.
pub(crate) const MAX_RENDER_VISUAL_ROWS: usize = 512;
pub(crate) const MAX_RENDER_SOURCE_LINES: usize = 256;
pub(crate) const MAX_RENDER_BYTES: usize = 64 * 1024;

/// Round a byte offset down to a UTF-8 boundary. `limit` is normally already
/// a boundary (it comes from `String::len()`), but this also makes truncation
/// of a frozen cursor safe if the source was replaced while detached.
pub(crate) fn utf8_floor_boundary(text: &str, limit: usize) -> usize {
    let mut index = limit.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Round a byte offset up to a UTF-8 boundary. This prevents a tail slice
/// from exceeding its byte budget when the budget lands in a multibyte char.
pub(crate) fn utf8_ceil_boundary(text: &str, limit: usize) -> usize {
    let mut index = limit.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Return at most `bytes` of the tail ending at `end`, never splitting a
/// Unicode scalar value. Keeping the tail is what makes a followed giant
/// streaming line useful without allocating or wrapping the whole line.
pub(crate) fn utf8_tail(text: &str, end: usize, bytes: usize) -> &str {
    let end = utf8_floor_boundary(text, end);
    let start = utf8_ceil_boundary(text, end.saturating_sub(bytes));
    &text[start..end]
}

/// Copy only the bounded part of the current source window. A frozen cursor
/// is an offset into the final source line, rather than a cloned tail string:
/// this is both bounded and stable while that line receives streaming deltas.
pub(crate) fn local_window_lines(lines: &[DisplayLine], window: &ScrollWindow) -> Vec<DisplayLine> {
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
pub(crate) struct ScrollWindow {
    /// Index into `TuiState::lines` for the first DisplayLine in the window.
    pub(crate) source_start: usize,
    /// Index (exclusive) into `TuiState::lines` for the end of the window.
    pub(crate) source_end: usize,
    /// Visual-row offset within the rendered window. The first visual row
    /// shown at the viewport top is `rendered[local_offset]`.
    pub(crate) local_offset: usize,
    /// Whether the viewport auto-follows the tail.
    pub(crate) follow_bottom: bool,
    /// Byte cursor in the last line captured when freezing during active
    /// streaming. Rendering stops at this UTF-8-safe cursor, so appended
    /// deltas cannot mutate the frozen viewport without cloning that line.
    pub(crate) frozen_tail_cursor: Option<usize>,
    /// The source_end value at the moment the snapshot was taken; used to
    /// detect when the user has scrolled past the frozen range.
    pub(crate) frozen_source_end: usize,
}

impl Default for ScrollWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollWindow {
    pub(crate) fn new() -> Self {
        Self {
            source_start: 0,
            source_end: 0,
            local_offset: 0,
            follow_bottom: true,
            frozen_tail_cursor: None,
            frozen_source_end: 0,
        }
    }

    /// Populate the window from the head of `lines` (terminal-style anchor
    /// for content that fits the viewport): `source_start = 0`, `source_end`
    /// clamped to the source-line rendering budget, `local_offset = 0` so the
    /// first visual row sits at the viewport top. Follow stays armed, so the
    /// next draw switches to `anchor_tail` once the content overflows the
    /// viewport. Clears any frozen cursor so follow shows the live view.
    pub(crate) fn anchor_head(&mut self, lines: &[DisplayLine]) {
        self.follow_bottom = true;
        self.frozen_tail_cursor = None;
        self.frozen_source_end = 0;
        self.source_start = 0;
        self.source_end = lines.len().min(MAX_RENDER_SOURCE_LINES);
        self.local_offset = 0;
    }

    /// Populate the window from a tail-anchored range of `lines`, choosing
    /// `source_start` by walking backward far enough to fill `height` visual
    /// rows at `width`, without considering more than the bounded history
    /// limits. After this call the window is ready for rendering and
    /// `local_offset` points to the bottom (follow mode).
    /// Clears any frozen cursor so follow shows the live tail.
    pub(crate) fn anchor_tail(&mut self, lines: &[DisplayLine], width: usize, height: usize) {
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
pub(crate) fn reanchor_window_on_resize(
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

/// Terminal-style head-anchoring decision for the main scrollback: while
/// following, content that fits the viewport is anchored at the head (output
/// starts at the top of the scrollback), and content that overflows is
/// anchored at the tail (follows the latest output). Exact for the "fits"
/// case and cheap for the overflow case: more source lines than viewport rows
/// can never fit (every line renders at least one visual row), and a line
/// longer than the viewport's whole cell budget can never fit either.
pub(crate) fn scrollback_fits_viewport(lines: &[DisplayLine], width: usize, height: usize) -> bool {
    if lines.len() > height {
        return false;
    }
    let width = width.max(1);
    let cell_budget = height * width;
    let mut total = 0usize;
    for line in lines {
        // A line whose char count alone exceeds the viewport's cell budget
        // cannot wrap into `height` rows (each char is at least one cell),
        // so skip the expensive markdown wrap entirely.
        if line.text.chars().count() > cell_budget {
            return false;
        }
        total = total.saturating_add(line_visual_rows(line, width));
        if total > height {
            return false;
        }
    }
    true
}

/// Count the visual rows a single DisplayLine would produce at `width`.
/// For Normal lines, uses the same markdown rendering pipeline as
/// `render_window` so that inline-code/bold delimiters are stripped before
/// wrapping — this keeps estimates consistent with actual rendering.
/// For non-Normal lines, falls back to `hard_wrap` (matches `styled_scroll_line`).
pub(crate) fn line_visual_rows(line: &DisplayLine, width: usize) -> usize {
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
pub(crate) const FENCE_LOOKBEHIND: usize = 32;

/// Return the look-behind start index used by `render_window`. Exposed for
/// deterministic testing: the result is `source_start.saturating_sub(n)`
/// where `n ≤ FENCE_LOOKBEHIND` but is also clamped so that every newline
/// segment inside the look-behind range is replayed, not just DisplayLine
/// boundaries.
pub(crate) fn lookbehind_start(lines: &[DisplayLine], source_start: usize) -> usize {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineKind {
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

/// After `handle_scroll` increments `local_offset`, check whether the
/// window needs to be extended forward (when the local end is reached and
/// more source lines exist). Adds enough `DisplayLine` entries to cover
/// `step` visual rows. When the true end is reached, resumes follow mode.
/// `on_extension` is called after any source_end growth (used to clear the
/// frozen-tail cursor once the user scrolls past the freeze point).
pub(crate) fn extend_window_down(
    state: &mut TuiState,
    step: usize,
    on_extension: impl FnOnce(&mut TuiState),
) {
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
    // Reached the end of the loaded lines while scrolling down: queue the
    // next newer middle segment (request_newer is self-guarding, so
    // repeated keys collapse and done/loading states suppress it). The
    // loaded lines always end with the head segment, so this fires once
    // per middle segment as the user pages down, until load_newer reports
    // None (the head segment is already loaded — nothing to fetch).
    if state.window.source_end >= state.lines.len() && state.newer_cursor.is_some() {
        state.request_newer();
    }
}
