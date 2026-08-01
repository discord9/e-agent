use ratatui::buffer::{CellDiffOption, CellWidth};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::agent::preview;

use super::*;

/// Ratatui's diff skips cells covered by wide glyphs. Mark the visible
/// cell immediately after each wide glyph so it is always emitted, preventing
/// stale terminal cells from persisting after scrolling.
pub(crate) fn force_wide_trailing_cell_updates(
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

pub(crate) fn draw<'a, B: ratatui::backend::Backend>(
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
        // When following, anchor the window. Terminal-style output: content
        // that fits the viewport is anchored at the head (starts at the top
        // of the scrollback instead of emerging from the bottom); once it
        // overflows, follow anchors at the tail (follows the latest output).
        // A user scroll (Up/PageUp) freezes by clearing follow_bottom and End
        // restores following — the anchor decision only runs while following.
        if scroll_state.window.follow_bottom {
            let height = usize::from(output.height);
            if scrollback_fits_viewport(&scroll_state.lines, inner_width, height) {
                scroll_state.window.anchor_head(&scroll_state.lines);
            } else {
                scroll_state
                    .window
                    .anchor_tail(&scroll_state.lines, inner_width, height);
            }
        }
        // Render a bounded local tail. The cursor captured when scrolling
        // away from a streaming tail keeps later deltas out of this view.
        let local_lines = local_window_lines(&scroll_state.lines, &scroll_state.window);
        let at_top = scroll_state.window.source_start == 0
            && scroll_state.window.source_end <= MAX_RENDER_SOURCE_LINES
            && scroll_state.window.frozen_tail_cursor.is_none();
        let visual = render_bounded_window(&local_lines, inner_width, at_top);
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
        // Terminal-style placement for a following window that renders fewer
        // rows than the viewport: a head-anchored (fits) window starts at the
        // top; only a tail-anchored window whose bounded source budget is
        // shorter than a very tall viewport bottom-aligns, keeping the live
        // tail against the input boundary.
        let render_top = if scroll_state.window.follow_bottom
            && total_rows < height
            && scroll_state.window.source_start > 0
        {
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
pub(crate) fn styled_scroll_line(row: &str, kind: LineKind) -> Line<'static> {
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

pub(crate) fn format_tokens(count: u64) -> String {
    if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// Build the bottom-left title spans for the input block: model name (violet)
/// with an optional ` role` suffix (muted). Shared by the main and attached
/// views. Returns an empty vec when `model_name` is empty.
pub(crate) fn model_role_spans(model_name: &str, role_name: Option<&str>) -> Vec<Span<'static>> {
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
pub(crate) fn cwd_usage_text(
    cwd: &str,
    tokens_context: u64,
    context_window: Option<u64>,
) -> (String, Color) {
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
/// Display-only; on Windows the backslash separators and case differences
/// may prevent the collapse (acceptable — the overlay just stays absolute).
pub(crate) fn shorten_home(cwd: &str) -> std::borrow::Cow<'_, str> {
    match crate::home_dir() {
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

/// Render the local copy and retain its most recent visual rows. The local
/// copy is byte-bounded before markdown parsing; trimming after wrapping also
/// bounds the actual ratatui history passed to the frame.
pub(crate) fn render_bounded_window(
    lines: &[DisplayLine],
    width: usize,
    keep_head: bool,
) -> Vec<ratatui::text::Line<'static>> {
    let mut rows = render_window(lines, 0, lines.len(), width);
    if rows.len() > MAX_RENDER_VISUAL_ROWS {
        if keep_head {
            // Viewport at the absolute top (Home / PageUp at the top): keep
            // the FIRST visual rows so the true beginning is visible. The
            // tail-biased drain would show a later page (the head segment's
            // first lines can wrap to thousands of visual rows).
            rows.truncate(MAX_RENDER_VISUAL_ROWS);
        } else {
            rows.drain(..rows.len() - MAX_RENDER_VISUAL_ROWS);
        }
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
pub(crate) fn render_task_detail(
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
    let visual = render_bounded_window(&local, width, false);
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
        // The detail view is a viewer, not a chat scrollback: content
        // always renders top-aligned from the content viewport top, so a
        // page shorter than the viewport (opened head page or a short
        // tail page) starts at the top instead of leaving a blank gap
        // between the fixed command banner and bottom-aligned rows.
        let render_top = content_top;
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

/// Render `lines[source_start..source_end]` into styled visual `Line`s at
/// `width`. The markdown code-fence state is primed by replaying at most
/// `FENCE_LOOKBEHIND` preceding lines through the MarkdownLines renderer
/// (see `lookbehind_start`). A code fence opened earlier may be invisible
/// to the priming pass and produce incorrect styling.
pub(crate) fn render_window(
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
        } else if line.kind != LineKind::Dim {
            // Dim lines are asides (background completions, notices,
            // cancelled-task hints) that interleave with a streaming
            // assistant message; they must not reset the markdown state
            // mid-message or the rest of the body loses its formatting.
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

/// Format a single task row for the F2 task panel, incorporating structured
/// display metadata from delegate calls.
///
/// Output format: `  #<id>: [<role>] <label>[background][ workspace: <ws>]`
/// where `[background]` reflects the effective execution mode and
/// `[workspace: …]` is only shown when explicitly set. "bash" tasks never
/// have these tags.
pub(crate) fn format_task_label(
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
