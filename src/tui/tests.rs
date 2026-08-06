use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyEvent, KeyModifiers};

use super::*;
use crate::config::InputKeys;
use ratatui::buffer::CellDiffOption;

/// Test helper: attach with default metadata, mirroring the old 3-arg
/// signature for minimal test churn.
fn attach_test(state: &mut TuiState, id: u64, label: &str, handle: RunnerHandle) {
    let snapshot = handle.snapshot();
    let status = handle.status();
    state.attach(
        id,
        label.into(),
        handle,
        String::new(),
        None,
        String::new(),
        String::new(),
        None,
        snapshot,
        status,
    );
}

/// Verify that the lookbehind used by render_window is bounded even
/// when the full scrollback contains 10k preceding lines.
#[test]
fn lookbehind_is_bounded_with_10k_prefix() {
    // 10,000 preceding lines followed by one Normal line.
    let mut lines: Vec<DisplayLine> = (0..10_000)
        .map(|i| DisplayLine {
            text: format!("line {i}"),
            kind: LineKind::Dim,
            collapsed_summary: None,
        })
        .collect();
    lines.push(DisplayLine {
        text: "the actual rendered line".into(),
        kind: LineKind::Normal,
        collapsed_summary: None,
    });
    let source_start = 10_000;
    let lb = lookbehind_start(&lines, source_start);
    // The look-behind must be bounded to at most FENCE_LOOKBEHIND
    // preceding segments; with 10k identical-line entries each is 1
    // segment, so the start is at most FENCE_LOOKBEHIND away.
    assert!(
        source_start - lb <= FENCE_LOOKBEHIND + 1,
        "lookbehind_start gap was {} (max {})",
        source_start - lb,
        FENCE_LOOKBEHIND + 1
    );
    // Render the single line with the bounded prefix to prove it
    // completes without iterating the full 10k prefix.
    let rendered = render_window(&lines, source_start, source_start + 1, 80, false);
    assert_eq!(rendered.len(), 1);
    assert_eq!(rendered[0].spans.len(), 1);
    assert_eq!(rendered[0].spans[0].content, "the actual rendered line");
}

#[test]
fn is_exit_matches_esc_or_ctrl_c_idle() {
    // Idle: Esc exits.
    assert!(is_exit(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    // Idle: Ctrl+C exits (idle has no active turn to cancel).
    assert!(is_exit(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    // A plain character is not an exit.
    assert!(!is_exit(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE
    )));
    assert!(!is_exit(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::empty(),
    )));
}

#[test]
fn paste_routes_to_active_input_and_normalizes_line_endings() {
    let pasted = "one\r\ntwo\rthree";
    let mut state = TuiState::default();

    state.handle_paste(pasted);
    assert_eq!(state.input.text, "one\ntwo\nthree", "main paste");

    state.input.insert(" main");
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    attach_test(&mut state, 7, "demo", handle);
    state.handle_paste(pasted);

    assert_eq!(
        state.attached.as_ref().unwrap().input.text,
        "one\ntwo\nthree",
        "attached paste uses the same CRLF/CR normalization"
    );
    assert_eq!(state.input.text, "one\ntwo\nthree main");
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
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 7, "demo", handle);
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
    assert_eq!(
        state.handle_tasks_panel_key(down),
        TaskSelection::None,
        "no tasks: cursor stays at 0"
    );
    assert_eq!(state.task_cursor, 0, "no tasks: cursor stays at 0");
    assert_eq!(state.handle_tasks_panel_key(up), TaskSelection::None);
    assert_eq!(state.task_cursor, 0);
    let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
    assert_eq!(state.handle_tasks_panel_key(a), TaskSelection::None);
}

#[tokio::test]
async fn tasks_panel_selection_routes_bash_to_detail_and_delegate_to_attach() {
    let temp = tempfile::tempdir().unwrap();
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(temp.path()).unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    background
        .start(
            crate::workspace::Workspace::new(temp.path()).unwrap(),
            "echo hello; sleep 30".into(),
            false,
        )
        .unwrap();
    background
        .spawn_with_id(
            "delegate task".into(),
            None,
            None,
            None,
            |_| {},
            || async { std::future::pending::<String>().await },
        )
        .unwrap();
    let running = background.running();
    assert_eq!(running.len(), 2);
    assert_eq!(running[0].kind, "bash");
    assert_eq!(running[1].kind, "delegate");

    let mut state = TuiState {
        background: Some(background.clone()),
        show_tasks: true,
        ..Default::default()
    };
    let delegate_id = running[1].id;
    state.attachable = Some(Box::new(move |id| id == delegate_id));
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
    // Enter on the bash row opens the full-output detail view.
    assert_eq!(
        state.handle_tasks_panel_key(enter),
        TaskSelection::OpenDetail(running[0].id)
    );
    // Enter on the delegate row attaches (original behavior).
    state.task_cursor = 1;
    assert_eq!(
        state.handle_tasks_panel_key(enter),
        TaskSelection::Attach(running[1].id)
    );
    // Cursor moves open the selected task's view immediately, like Enter:
    // down onto the delegate row attaches…
    state.task_cursor = 0;
    assert_eq!(
        state.handle_tasks_panel_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty())),
        TaskSelection::Attach(running[1].id)
    );
    // …and up back onto the bash row opens its full-output detail.
    assert_eq!(
        state.handle_tasks_panel_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty())),
        TaskSelection::OpenDetail(running[0].id)
    );
    // Down again re-attaches to the delegate row.
    assert_eq!(
        state.handle_tasks_panel_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty())),
        TaskSelection::Attach(running[1].id)
    );
    background.cancel(running[0].id);
    background.cancel(running[1].id);
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn open_task_detail_clears_attached_and_remembers_panel_state() {
    let temp = tempfile::tempdir().unwrap();
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(temp.path()).unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    background
        .start(
            crate::workspace::Workspace::new(temp.path()).unwrap(),
            "echo hello; sleep 30".into(),
            false,
        )
        .unwrap();
    let id = background.running()[0].id;
    for _ in 0..50 {
        if background
            .spool(id)
            .is_some_and(|spool| spool.line_count() >= 1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState {
        background: Some(background.clone()),
        show_tasks: true,
        cwd: temp.path().display().to_string(),
        session_id: "detail-attach-clear".into(),
        ..Default::default()
    };
    attach_test(&mut state, id, "bash task", handle);
    assert!(state.attached.is_some());

    // Opening the detail view drops the attached session — the detail is
    // an independent full-screen view and must never stack on attach —
    // and hides the panel, remembering it for Esc.
    state.open_task_detail(id);
    assert!(state.task_detail.is_some());
    assert!(
        state.attached.is_none(),
        "detail must clear the attached view"
    );
    assert!(!state.show_tasks, "detail is full-screen: panel hidden");
    assert!(
        state.detail_return_panel,
        "opened from the panel: Esc must return there"
    );

    // Esc restores the tasks panel (the state before the detail opened).
    state.handle_task_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(state.task_detail.is_none());
    assert!(state.show_tasks, "Esc returns to the tasks panel");

    // F2 closes the detail AND the panel, back to the main view.
    state.open_task_detail(id);
    assert!(state.task_detail.is_some());
    state.handle_task_detail_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()));
    assert!(state.task_detail.is_none());
    assert!(!state.show_tasks, "F2 closes the detail and the panel");

    background.cancel(id);
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn open_task_detail_from_main_view_esc_returns_to_main_view() {
    let temp = tempfile::tempdir().unwrap();
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(temp.path()).unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    background
        .start(
            crate::workspace::Workspace::new(temp.path()).unwrap(),
            "echo hello; sleep 30".into(),
            false,
        )
        .unwrap();
    let id = background.running()[0].id;
    for _ in 0..50 {
        if background
            .spool(id)
            .is_some_and(|spool| spool.line_count() >= 1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Detail opened with no panel showing: Esc returns to the main view.
    let mut state = TuiState {
        background: Some(background.clone()),
        show_tasks: false,
        ..Default::default()
    };
    state.open_task_detail(id);
    assert!(state.task_detail.is_some());
    assert!(!state.show_tasks);
    assert!(
        !state.detail_return_panel,
        "opened from the main view: Esc returns to the main view"
    );
    state.handle_task_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(state.task_detail.is_none());
    assert!(!state.show_tasks, "Esc returns to the main view");

    background.cancel(id);
    tokio::task::yield_now().await;
}

#[test]
fn task_detail_opens_at_head_and_pages() {
    let spool = Arc::new(crate::tools::TaskSpool::new());
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!("line {i:03}\n"));
    }
    spool.append(text.as_bytes());
    let mut state = TuiState {
        inner_width: 80,
        output_height: 10,
        ..Default::default()
    };
    let scroll = |code| KeyEvent::new(code, KeyModifiers::empty());

    // Open shows the head page (first viewport of lines) in follow
    // mode; the view only slides to the tail once output grows.
    let mut detail = TaskDetail::new(1, "demo".into(), spool, false);
    detail.load_head(80, 10);
    detail.last_seen_lines = detail.spool.line_count();
    assert!(detail.window.follow_bottom);
    assert_eq!(detail.base_line, 0);
    assert_eq!(detail.lines.first().unwrap().text, "line 000");
    assert_eq!(detail.lines.last().unwrap().text, "line 009");
    state.task_detail = Some(detail);

    // PgUp freezes and pages up one viewport at a time.
    state.handle_detail_scroll(scroll(KeyCode::PageUp));
    let detail = state.task_detail.as_ref().unwrap();
    assert!(!detail.window.follow_bottom);
    assert_eq!(detail.base_line, 0);
    assert_eq!(detail.lines.first().unwrap().text, "line 000");
    state.handle_detail_scroll(scroll(KeyCode::PageUp));
    assert_eq!(state.task_detail.as_ref().unwrap().base_line, 0);

    // Home jumps to the first page.
    state.handle_detail_scroll(scroll(KeyCode::Home));
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 0);
    assert_eq!(detail.lines.first().unwrap().text, "line 000");
    assert_eq!(detail.window.local_offset, 0);
    // Up at the very first page stays put.
    state.handle_detail_scroll(scroll(KeyCode::Up));
    assert_eq!(state.task_detail.as_ref().unwrap().base_line, 0);

    // End restores follow at the tail and syncs last_seen_lines so
    // the next draw does not re-jump the anchored tail page.
    state.handle_detail_scroll(scroll(KeyCode::End));
    let detail = state.task_detail.as_ref().unwrap();
    assert!(detail.window.follow_bottom);
    assert_eq!(detail.base_line, 290);
    assert_eq!(detail.last_seen_lines, 300);
    // Down while following is a no-op.
    state.handle_detail_scroll(scroll(KeyCode::Down));
    assert!(state.task_detail.as_ref().unwrap().window.follow_bottom);

    // Up freezes; Down pages forward to the tail page, then a second
    // Down at the spool tail resumes follow.
    state.handle_detail_scroll(scroll(KeyCode::Up));
    let detail = state.task_detail.as_ref().unwrap();
    assert!(!detail.window.follow_bottom);
    assert_eq!(detail.base_line, 280);
    state.handle_detail_scroll(scroll(KeyCode::Down));
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 290);
    assert!(!detail.window.follow_bottom);
    state.handle_detail_scroll(scroll(KeyCode::Down));
    assert!(state.task_detail.as_ref().unwrap().window.follow_bottom);
}

#[test]
fn task_detail_growth_follows_tail_from_head() {
    let spool = Arc::new(crate::tools::TaskSpool::new());
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!("line {i:03}\n"));
    }
    spool.append(text.as_bytes());
    let mut state = TuiState {
        inner_width: 80,
        output_height: 10,
        ..Default::default()
    };
    let scroll = |code| KeyEvent::new(code, KeyModifiers::empty());
    // Same open path as open_task_detail: head page + follow armed
    // with last_seen_lines at the current spool count.
    let mut detail = TaskDetail::new(1, "demo".into(), spool.clone(), false);
    detail.load_head(80, 10);
    detail.last_seen_lines = detail.spool.line_count();
    state.task_detail = Some(detail);

    let backend = ratatui::backend::TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();

    // First draw with no growth: the head page stays on screen.
    draw(&mut terminal, &mut state).unwrap();
    let buffer = terminal.backend().buffer();
    let row: String = (1..79)
        .map(|x| buffer[(x, 1)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "line 000");
    assert!(state.task_detail.as_ref().unwrap().window.follow_bottom);

    // Output grows -> the next draw slides to the live tail.
    let mut more = String::new();
    for i in 300..320 {
        more.push_str(&format!("line {i:03}\n"));
    }
    spool.append(more.as_bytes());
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert!(detail.window.follow_bottom);
    assert_eq!(detail.base_line, 310);
    assert_eq!(detail.lines.last().unwrap().text, "line 319");
    let buffer = terminal.backend().buffer();
    let row: String = (1..79)
        .map(|x| buffer[(x, 10)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "line 319");

    // No growth -> a draw leaves the page untouched.
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 310);
    assert_eq!(detail.last_seen_lines, 320);

    // PgUp freezes; growth while frozen does not move the view.
    state.handle_detail_scroll(scroll(KeyCode::PageUp));
    let detail = state.task_detail.as_ref().unwrap();
    assert!(!detail.window.follow_bottom);
    assert_eq!(detail.base_line, 300);
    let mut more = String::new();
    for i in 320..325 {
        more.push_str(&format!("line {i:03}\n"));
    }
    spool.append(more.as_bytes());
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert!(!detail.window.follow_bottom);
    assert_eq!(detail.base_line, 300);

    // End resumes follow at the new tail and syncs last_seen_lines,
    // so the following draw does not re-jump.
    state.handle_detail_scroll(scroll(KeyCode::End));
    let detail = state.task_detail.as_ref().unwrap();
    assert!(detail.window.follow_bottom);
    assert_eq!(detail.base_line, 315);
    assert_eq!(detail.last_seen_lines, 325);
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 315);
}

#[test]
fn task_detail_tail_reload_is_throttled() {
    // Draw-driven tail reloads are rate-limited to TAIL_RELOAD_INTERVAL:
    // a frame within 100 ms of the previous reload keeps the cached page,
    // and the pending growth survives to the next due frame.
    let spool = Arc::new(crate::tools::TaskSpool::new());
    let mut text = String::new();
    for i in 0..10 {
        text.push_str(&format!("line {i:03}\n"));
    }
    spool.append(text.as_bytes());
    let mut state = TuiState {
        task_detail: Some(TaskDetail::new(1, "demo".into(), spool.clone(), false)),
        ..Default::default()
    };
    let backend = ratatui::backend::TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();

    // First draw: the initial reload is always due (10 lines, tail page
    // base 0 with a 10-row viewport).
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 0);
    assert_eq!(detail.last_seen_lines, 10);

    // Growth inside the throttle window: the cached page stays and
    // last_seen_lines is NOT advanced, so the growth is not dropped.
    let mut more = String::new();
    for i in 10..30 {
        more.push_str(&format!("line {i:03}\n"));
    }
    spool.append(more.as_bytes());
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 0, "throttled frame must keep the page");
    assert_eq!(detail.last_seen_lines, 10);

    // After the interval the next frame reloads the live tail.
    std::thread::sleep(TAIL_RELOAD_INTERVAL + Duration::from_millis(20));
    draw(&mut terminal, &mut state).unwrap();
    let detail = state.task_detail.as_ref().unwrap();
    assert_eq!(detail.base_line, 20);
    assert_eq!(detail.last_seen_lines, 30);
}

#[test]
fn task_detail_wrap_budget_bounds_follow_wrap() {
    // Follow mode must not wrap the whole 64 KiB page per frame: the wrap
    // is bounded to the viewport's cell budget, so a page of long lines
    // renders its tail (which is all the viewport shows) while the head
    // of the page is cut instead of being wrapped and thrown away.
    let spool = Arc::new(crate::tools::TaskSpool::new());
    let mut text = String::new();
    for i in 0..10 {
        // 8-char prefix + 592 X's = 600 bytes per line (6 KiB page, over
        // the 4 KiB floor of detail_wrap_budget(78, 10)).
        text.push_str(&format!("line {i:03} {}", "X".repeat(592)));
        text.push('\n');
    }
    spool.append(text.as_bytes());
    let mut state = TuiState {
        task_detail: Some(TaskDetail::new(1, "demo".into(), spool, false)),
        ..Default::default()
    };
    let backend = ratatui::backend::TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    draw(&mut terminal, &mut state).unwrap();
    let buffer = terminal.backend().buffer();
    let page: String = (1..11)
        .map(|y| {
            (1..79)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // The live tail is on screen: the last line's prefix is visible and
    // its wrapped tail fills the bottom row (601 chars → 7 full rows of
    // 78 plus a final row of 55).
    assert!(page.contains("line 009"), "tail page must be rendered");
    let bottom: String = (1..79)
        .map(|x| buffer[(x, 10)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(bottom.trim_end(), &"X".repeat(55));
    // The head of the page (cut by the wrap budget, not wrapped) is gone:
    // a 600-byte line at width 78 wraps to 8 rows, so the full 10-line
    // page would need 80 rows; the budget keeps only the tail, and the
    // earliest line that survives is line 003's tail (prefix cut).
    assert!(
        !page.contains("line 000") && !page.contains("line 003"),
        "wrap budget must cut the page head: {page:?}"
    );
}

#[test]
fn task_detail_render_paints_header_and_finished() {
    let spool = Arc::new(crate::tools::TaskSpool::new());
    let mut text = String::new();
    for i in 0..30 {
        text.push_str(&format!("line {i}\n"));
    }
    spool.append(text.as_bytes());
    let mut state = TuiState {
        task_detail: Some(TaskDetail::new(9, "demo".into(), spool, false)),
        ..Default::default()
    };
    let backend = ratatui::backend::TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    draw(&mut terminal, &mut state).unwrap();
    let buffer = terminal.backend().buffer();
    // Task 9 is not in the (empty) running snapshot → finished.
    assert!(state.task_detail.as_ref().unwrap().finished);
    // Header carries status, line count, and key hints.
    let header: String = (0..120)
        .map(|x| buffer[(x, 0)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(header.contains("finished"), "{header}");
    assert!(header.contains("30 lines"), "{header}");
    assert!(header.contains("Esc close"), "{header}");
    // Follow tail: lines 20..29 rendered as plain Dim text.
    assert_eq!(buffer[(1, 1)].fg, SOLARIZED_LIGHT.muted);
    assert!(buffer[(1, 1)].modifier.contains(Modifier::DIM));
    let row: String = (1..119)
        .map(|x| buffer[(x, 1)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "line 20");
    let row: String = (1..119)
        .map(|x| buffer[(x, 10)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "line 29");
}

#[test]
fn task_detail_render_shows_full_command_banner_below_header() {
    let spool = Arc::new(crate::tools::TaskSpool::new());
    spool.append(b"line 0\nline 1\n");
    // Longer than the 100-char panel label preview: the middle would be
    // elided with "…" in the header, but the banner must show it whole.
    let command = format!(
        "echo {} && echo tail",
        "abcdefghijklmnopqrstuvwxyz0123456789".repeat(4)
    );
    assert!(command.len() > 100);
    let mut state = TuiState {
        task_detail: Some(TaskDetail::new(9, "demo".into(), spool, false)),
        ..Default::default()
    };
    state.task_detail.as_mut().unwrap().command = Some(command.clone());
    let backend = ratatui::backend::TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    draw(&mut terminal, &mut state).unwrap();
    let buffer = terminal.backend().buffer();
    // The header keeps the short label (no alphabet block, no ellipsis
    // over the long command): the full command lives in the banner.
    let header: String = (0..120)
        .map(|x| buffer[(x, 0)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(
        !header.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
        "header should keep the short label: {header}"
    );
    // Banner rows directly under the header: the wrapped full command,
    // not the truncated label.
    let banner: String = (1..3)
        .map(|y| {
            (1..119)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let joined: String = banner.lines().map(str::trim_end).collect();
    assert_eq!(
        joined, command,
        "full command must be visible in the detail banner"
    );
    // Banner is Dim like the spool output text.
    assert!(buffer[(1, 1)].modifier.contains(Modifier::DIM));
    // Output renders top-aligned directly below the fixed banner: the
    // short spool starts at the content top, no blank gap in between.
    let row: String = (1..119)
        .map(|x| buffer[(x, 3)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "line 0");
    let row: String = (1..119)
        .map(|x| buffer[(x, 4)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "line 1");
    let row: String = (1..119)
        .map(|x| buffer[(x, 10)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(
        row.trim_end(),
        "",
        "no bottom-aligned tail below short output"
    );
}

#[test]
fn task_detail_top_aligns_short_output_and_esc_f2_close() {
    let spool = Arc::new(crate::tools::TaskSpool::new());
    spool.append(b"one\ntwo\nthree\n");
    // State as open_task_detail leaves it when opened from the panel:
    // panel hidden, Esc-return target remembered.
    let mut state = TuiState {
        show_tasks: false,
        detail_return_panel: true,
        task_detail: Some(TaskDetail::new(9, "demo".into(), spool, false)),
        ..Default::default()
    };
    let backend = ratatui::backend::TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    draw(&mut terminal, &mut state).unwrap();
    let buffer = terminal.backend().buffer();
    // The detail view is a viewer: fewer rows than the viewport still
    // render top-aligned from the content top (first row at the top, no
    // blank gap, no bottom-aligned tail).
    let row: String = (1..39)
        .map(|x| buffer[(x, 1)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "one");
    let row: String = (1..39)
        .map(|x| buffer[(x, 2)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "two");
    let row: String = (1..39)
        .map(|x| buffer[(x, 3)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "three");
    let row: String = (1..39)
        .map(|x| buffer[(x, 10)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(
        row.trim_end(),
        "",
        "short detail output must not be bottom-aligned"
    );

    // Esc closes the detail and returns to the panel.
    state.handle_task_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(state.task_detail.is_none());
    assert!(state.show_tasks, "Esc returns to the tasks panel");

    // F2 closes the detail AND the panel.
    let spool = Arc::new(crate::tools::TaskSpool::new());
    spool.append(b"x\n");
    state.task_detail = Some(TaskDetail::new(9, "demo".into(), spool, false));
    state.handle_task_detail_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::empty()));
    assert!(state.task_detail.is_none());
    assert!(!state.show_tasks, "F2 closes the detail and the panel");
}

#[test]
fn task_detail_open_anchor_head_top_aligns_short_output() {
    // Opening a detail anchors the head page (load_head) in follow mode;
    // with fewer rows than the viewport the page must still render
    // top-aligned from the content top, not bottom-aligned with a blank
    // gap between the fixed command banner and the output.
    let spool = Arc::new(crate::tools::TaskSpool::new());
    spool.append(b"first\nsecond\n");
    let mut detail = TaskDetail::new(1, "demo".into(), spool, false);
    detail.load_head(38, 10);
    detail.last_seen_lines = detail.spool.line_count();
    assert!(detail.window.follow_bottom);
    assert_eq!(detail.base_line, 0);
    assert_eq!(detail.lines.len(), 2);
    let mut state = TuiState {
        show_tasks: true,
        task_detail: Some(detail),
        ..Default::default()
    };
    let backend = ratatui::backend::TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    draw(&mut terminal, &mut state).unwrap();
    let buffer = terminal.backend().buffer();
    // First row at the content top (below the header border), no gap.
    let row: String = (1..39)
        .map(|x| buffer[(x, 1)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "first");
    let row: String = (1..39)
        .map(|x| buffer[(x, 2)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(row.trim_end(), "second");
    let row: String = (1..39)
        .map(|x| buffer[(x, 10)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert_eq!(
        row.trim_end(),
        "",
        "opened head page must not be bottom-aligned"
    );
}

#[tokio::test]
async fn task_detail_cancel_by_id_keeps_the_spool_paged() {
    let temp = tempfile::tempdir().unwrap();
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(temp.path()).unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    background
        .start(
            crate::workspace::Workspace::new(temp.path()).unwrap(),
            "echo hello; sleep 30".into(),
            false,
        )
        .unwrap();
    let id = background.running()[0].id;
    for _ in 0..50 {
        if background
            .spool(id)
            .is_some_and(|spool| spool.line_count() >= 1)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut state = TuiState {
        background: Some(background.clone()),
        show_tasks: true,
        cwd: temp.path().display().to_string(),
        session_id: "detail-cancel".into(),
        ..Default::default()
    };
    state.open_task_detail(id);
    let detail = state.task_detail.as_ref().unwrap();
    assert!(!detail.finished);
    assert_eq!(detail.lines.first().unwrap().text, "hello");
    // x cancels the task by the detail id (not the panel cursor).
    state.handle_task_detail_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()));
    tokio::task::yield_now().await;
    assert!(background.running().is_empty());
    // The detail stays open; its Arc keeps the spool paged.
    assert!(state.task_detail.is_some());
    assert_eq!(
        state.task_detail.as_ref().unwrap().spool.line_count(),
        1,
        "cancelled task's spool is still readable"
    );
}

#[tokio::test]
async fn atomic_attach_snapshot_and_live_receiver_have_no_gap() {
    let (handle, sink, _source) = crate::runner::session_test_channel();
    sink.emit(AgentEvent::AssistantText("snapshot".into()));
    let (snapshot, mut live, _) = handle.attach();
    sink.emit(AgentEvent::AssistantDelta("live".into()));

    assert_eq!(snapshot, vec![AgentEvent::AssistantText("snapshot".into())]);
    assert_eq!(
        live.recv().await.unwrap(),
        AgentEvent::AssistantDelta("live".into())
    );
}

#[tokio::test]
async fn idle_attached_view_routes_ready_deltas_without_reattach() {
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 7, "demo", handle);
    let (sender, mut inbox) = mpsc::unbounded_channel();
    for text in ["live ", "while ", "idle"] {
        sender
            .send(UiEvent {
                session: 7,
                event: AgentEvent::AssistantDelta(text.into()),
            })
            .unwrap();
    }
    let first = inbox.recv().await.unwrap();
    route_idle_events(&mut state, first, &mut inbox);
    assert!(inbox.try_recv().is_err());
    assert_eq!(
        state.attached.as_ref().unwrap().state.lines[0].text,
        "live while idle"
    );
}

#[tokio::test]
async fn reattaching_forwards_each_delta_once() {
    let (handle, sink, _source) = crate::runner::session_test_channel();
    let (sender, mut inbox) = mpsc::unbounded_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 7, "demo", handle.clone());
    state.attached.as_mut().unwrap().bridge = Some(bridge(7, handle.attach().1, sender.clone()));
    state.detach();
    tokio::task::yield_now().await;

    attach_test(&mut state, 7, "demo", handle.clone());
    state.attached.as_mut().unwrap().bridge = Some(bridge(7, handle.attach().1, sender));
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
    let (handle, sink, _source) = crate::runner::session_test_channel();
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
    attach_test(&mut state, 7, "demo task", handle);
    let lines: Vec<_> = state
        .attached
        .as_ref()
        .unwrap()
        .state
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    assert_eq!(lines, ["partial", "bash: ls", "  ok: files",]);
    // The transient BackgroundCompleted flips the attached view to
    // finished but renders nothing (the persistent "finished" line
    // comes from the UserPrompt at the turn boundary).
    assert!(!state.attached.as_ref().unwrap().finished);
    state.push_event(UiEvent {
        session: 0,
        event: AgentEvent::BackgroundCompleted {
            id: 7,
            output: "done".into(),
            label: None,
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
fn attached_view_clears_spinner_on_background_completed_from_bridge() {
    // BackgroundCompleted arriving via the bridge (session id != 0)
    // must clear the spinner and mark the view finished, same as the
    // main-session path.
    let (handle, sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    // Attach with an unfinished session (no BackgroundCompleted in log).
    sink.emit(AgentEvent::AssistantDelta("working...".into()));
    attach_test(&mut state, 7, "demo task", handle);
    let attached = state.attached.as_ref().unwrap();
    assert!(!attached.finished);
    assert!(attached.state.busy.is_some(), "spinner should be active");
    // Simulate a BackgroundCompleted arriving from the bridge.
    state.push_event(UiEvent {
        session: 7,
        event: AgentEvent::BackgroundCompleted {
            id: 7,
            output: "done".into(),
            label: None,
        },
    });
    let attached = state.attached.as_ref().unwrap();
    assert!(attached.finished);
    assert!(attached.state.busy.is_none(), "spinner should be cleared");
}

#[test]
fn attach_after_completion_marks_finished_from_the_snapshot() {
    // Regression: a completion that raced into the session log before
    // the attach left the view stuck in "running" forever.
    let (handle, sink, _source) = crate::runner::session_test_channel();
    sink.emit(AgentEvent::AssistantText("work".into()));
    sink.emit(AgentEvent::BackgroundCompleted {
        id: 3,
        output: "done".into(),
        label: None,
    });
    let mut state = TuiState::default();
    attach_test(&mut state, 3, "demo task", handle);
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
    let (handle, _sink, mut source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 7, "demo task", handle);
    {
        let attached = state.attached.as_mut().unwrap();
        attached.input.insert("please also check tests");
    }
    state.handle_attached_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), 80);
    assert!(matches!(
        source.try_recv().ok(),
        Some(crate::runner::SessionCommand::Prompt(ref text)) if text == "please also check tests"
    ));
    state.handle_attached_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 80);
    assert!(matches!(
        source.try_recv().ok(),
        Some(crate::runner::SessionCommand::Cancel)
    ));
    // Finished sessions no longer accept steering.
    state.push_event(UiEvent {
        session: 0,
        event: AgentEvent::BackgroundCompleted {
            id: 7,
            output: "done".into(),
            label: None,
        },
    });
    state.handle_attached_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 80);
    assert!(source.try_recv().ok().is_none());
}

#[test]
fn attached_enter_on_finished_keeps_input_and_sends_nothing() {
    // S1: Enter on a finished session must not silently swallow the
    // buffer — the input stays editable, no command goes out, and a dim
    // hint explains why.
    let (handle, _sink, mut source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 9, "done task", handle);
    state.push_event(UiEvent {
        session: 0,
        event: AgentEvent::BackgroundCompleted {
            id: 9,
            output: "done".into(),
            label: None,
        },
    });
    {
        let attached = state.attached.as_mut().unwrap();
        attached.input.insert("still typing");
    }
    state.handle_attached_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), 80);
    assert!(source.try_recv().ok().is_none(), "no command may be sent");
    let attached = state.attached.as_mut().unwrap();
    assert_eq!(
        attached.input.text, "still typing",
        "input must be preserved"
    );
    assert_eq!(
        attached.state.lines.last().unwrap().text,
        "subagent finished — prompt not sent, press Esc to detach"
    );
    assert_eq!(attached.state.lines.last().unwrap().kind, LineKind::Dim);
}

#[test]
fn finish_with_queued_prompts_notes_them_as_unanswered() {
    // S2: prompts queued while the turn was in flight may be persisted
    // by the runner but never answered; the view must surface that
    // instead of leaving "queued" dangling forever.
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 11, "task", handle);
    state.push_event(UiEvent {
        session: 11,
        event: AgentEvent::PromptQueued("first".into()),
    });
    state.push_event(UiEvent {
        session: 11,
        event: AgentEvent::PromptQueued("second".into()),
    });
    assert_eq!(state.attached.as_ref().unwrap().state.queued.len(), 2);
    state.push_event(UiEvent {
        session: 0,
        event: AgentEvent::BackgroundCompleted {
            id: 11,
            output: "done".into(),
            label: None,
        },
    });
    let attached = state.attached.as_ref().unwrap();
    assert!(attached.finished);
    assert_eq!(
        attached.state.lines.last().unwrap().text,
        "2 queued prompt(s) not answered (session finished)"
    );
    assert_eq!(attached.state.lines.last().unwrap().kind, LineKind::Dim);
}

#[test]
fn attached_ctrl_c_clears_input_when_nonempty() {
    // S5: with a non-empty buffer Ctrl-C clears the line instead of
    // cancelling the in-flight turn; an empty buffer still cancels.
    let (handle, _sink, mut source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 7, "demo task", handle);
    {
        let attached = state.attached.as_mut().unwrap();
        attached.input.insert("half-typed");
    }
    state.handle_attached_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 80);
    assert!(
        source.try_recv().ok().is_none(),
        "no cancel while the input is non-empty"
    );
    assert_eq!(state.attached.as_ref().unwrap().input.text, "");
    state.handle_attached_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 80);
    assert!(matches!(
        source.try_recv().ok(),
        Some(crate::runner::SessionCommand::Cancel)
    ));
}

#[test]
fn attached_input_survives_detach_and_reattach() {
    // S6: detaching (Esc) and re-attaching must restore a half-typed
    // steering prompt instead of discarding it.
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 21, "task", handle.clone());
    state.handle_attached_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::empty()), 80);
    state.handle_attached_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty()), 80);
    assert_eq!(state.attached.as_ref().unwrap().input.text, "hi");
    state.detach();
    assert!(state.attached.is_none());
    attach_test(&mut state, 21, "task", handle.clone());
    assert_eq!(
        state.attached.as_ref().unwrap().input.text,
        "hi",
        "detach/reattach must restore the steering input"
    );
    // A sent prompt must not resurrect on the next re-attach.
    state.handle_attached_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), 80);
    assert_eq!(state.attached.as_ref().unwrap().input.text, "");
    state.detach();
    attach_test(&mut state, 21, "task", handle);
    assert_eq!(
        state.attached.as_ref().unwrap().input.text,
        "",
        "a sent prompt must not be stashed"
    );
}

#[test]
fn attached_title_reflects_injected_status() {
    // S3: the title text is driven by the attached session's live
    // status receiver (injected here; run_inner keeps it in sync).
    let backend = ratatui::backend::TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 5, "job", handle);
    let (tx, rx) = tokio::sync::watch::channel(SessionStatus::Idle);
    state.attached.as_mut().unwrap().status = rx;
    let title =
        |terminal: &mut Terminal<ratatui::backend::TestBackend>, state: &mut TuiState| -> String {
            draw(terminal, state).unwrap();
            let buffer = terminal.backend().buffer();
            (0..60)
                .map(|x| buffer[(x, 9)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
    // Idle: mirror what run_inner's status refresh does (no spinner).
    state.attached.as_mut().unwrap().state.busy = None;
    let top = title(&mut terminal, &mut state);
    assert!(top.contains("idle — subagent #5"), "got: {top:?}");
    // Busy and Compacting carry the spinner labels.
    tx.send_replace(SessionStatus::Busy);
    state.attached.as_mut().unwrap().state.busy = Some(BusyState::thinking());
    let top = title(&mut terminal, &mut state);
    assert!(top.contains("thinking… — subagent #5"), "got: {top:?}");
    tx.send_replace(SessionStatus::Compacting);
    state.attached.as_mut().unwrap().state.busy = Some(BusyState::compacting());
    let top = title(&mut terminal, &mut state);
    assert!(top.contains("compaction… — subagent #5"), "got: {top:?}");
    // Finished distinguishes the terminal result variants.
    tx.send_replace(SessionStatus::Finished(SessionResult::Completed(None)));
    let top = title(&mut terminal, &mut state);
    assert!(top.contains("finished — subagent #5"), "got: {top:?}");
    tx.send_replace(SessionStatus::Finished(SessionResult::Failed(
        "boom".into(),
    )));
    let top = title(&mut terminal, &mut state);
    assert!(top.contains("failed — subagent #5"), "got: {top:?}");
    tx.send_replace(SessionStatus::Finished(SessionResult::Cancelled));
    let top = title(&mut terminal, &mut state);
    assert!(top.contains("cancelled — subagent #5"), "got: {top:?}");
    // The dead "Enter steer  Ctrl-C interrupt" hint disappears once the
    // session is finished.
    assert!(
        !top.contains("Enter steer"),
        "finished view must not advertise steering, got: {top:?}"
    );
    assert!(
        top.contains("Esc detach"),
        "finished view must still offer detach, got: {top:?}"
    );
}

#[test]
fn attached_view_scrolls_independently() {
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState {
        window: ScrollWindow {
            local_offset: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    attach_test(&mut state, 1, "task", handle);
    {
        let attached = state.attached.as_mut().unwrap();
        attached.state.lines.push(DisplayLine {
            text: "a".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        });
        attached.state.lines.push(DisplayLine {
            text: "b".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        });
        attached.state.window.source_start = 0;
        attached.state.window.source_end = 2;
        attached.state.window.local_offset = 1;
        attached.state.inner_width = 80;
    }
    // Scrolling while attached moves the subagent view only.
    state.handle_attached_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()), 80);
    assert_eq!(state.window.local_offset, 2, "main scroll untouched");
    assert_eq!(
        state.attached.as_ref().unwrap().state.window.local_offset,
        0
    );
    state.handle_attached_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()), 80);
    assert_eq!(
        state.attached.as_ref().unwrap().state.window.local_offset,
        1
    );
}

#[test]
fn attached_tab_toggles_thinking_collapse_in_the_attached_view() {
    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    attach_test(&mut state, 1, "task", handle);
    assert!(
        state.attached.as_ref().unwrap().state.collapse_thinking.0,
        "attached views default to collapsed"
    );
    // Tab in the attached view flips the ATTACHED view's collapse flag
    // (the main view's flag is a per-view toggle and must stay untouched).
    state.handle_attached_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), 80);
    assert!(!state.attached.as_ref().unwrap().state.collapse_thinking.0);
    assert!(
        state.collapse_thinking.0,
        "main view collapse state must be independent"
    );
    // Tab again collapses back to the summary.
    state.handle_attached_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), 80);
    assert!(state.attached.as_ref().unwrap().state.collapse_thinking.0);
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
    // Start with follow_bottom = false so Down/PageDown are not no-ops.
    let mut state = TuiState {
        window: ScrollWindow {
            follow_bottom: false,
            local_offset: 0,
            source_start: 0,
            source_end: 1,
            ..Default::default()
        },
        lines: vec![DisplayLine {
            text: "hello".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        inner_width: 80,
        ..Default::default()
    };

    state.handle_scroll(down);
    drain_ready_scroll_keys(&mut events, &mut state).await;
    assert_eq!(
        state.window.local_offset, 12,
        "the ready scroll run is applied in order"
    );
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
        window: ScrollWindow {
            follow_bottom: false,
            local_offset: 0,
            source_start: 0,
            source_end: 1,
            ..Default::default()
        },
        lines: vec![DisplayLine {
            text: "hello".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        inner_width: 80,
        ..Default::default()
    };
    state.handle_scroll(down);
    drain_ready_scroll_keys(&mut events, &mut state).await;
    assert_eq!(
        state.window.local_offset, 2,
        "a woken scroll key joins the quiet window"
    );
    assert!(matches!(
        events.next().await,
        Some(Ok(Event::Key(key))) if key == typing
    ));

    let (handle, _sink, _source) = crate::runner::session_test_channel();
    let mut parent = TuiState {
        window: ScrollWindow {
            local_offset: 7,
            ..Default::default()
        },
        ..Default::default()
    };
    attach_test(&mut parent, 1, "task", handle);
    let mut events = futures_util::stream::iter(vec![
        Ok::<_, io::Error>(Event::Key(down)),
        Ok(Event::Key(typing)),
    ])
    .peekable();
    {
        let attached = parent.attached.as_mut().unwrap();
        attached.state.lines.push(DisplayLine {
            text: "hello".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        });
        attached.state.window.source_start = 0;
        attached.state.window.source_end = 1;
        attached.state.window.follow_bottom = false;
        attached.state.inner_width = 80;
        attached.state.handle_scroll(down);
        drain_ready_scroll_keys(&mut events, &mut attached.state).await;
        assert_eq!(attached.state.window.local_offset, 2);
    }
    assert_eq!(
        parent.window.local_offset, 7,
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
fn wide_char_backspace_leaves_no_stale_trailing_cell_in_input() {
    // Regression: typing then deleting a wide char in the input area.
    // Frame 1: type "好" (wide, covering inner.x and x+1), draw.
    // Frame 2: backspace (input empty), draw.  The trailing cell that
    // was covered by 好 in frame 1 must carry the panel background.
    //
    // NOTE: this assertion also passes on the old Paragraph-based code
    // because the input Block repaints the entire frame area every draw
    // (set_style all-area paint already fixes bg).  The real regression
    // is in the scrollback area (Scenario B below) where no Block exists.
    let backend = ratatui::backend::TestBackend::new(12, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = TuiState::default();

    state.input.insert("好");
    draw(&mut terminal, &mut state).unwrap();
    state.input.backspace();
    draw(&mut terminal, &mut state).unwrap();

    let buffer = terminal.backend().buffer();
    // inner content row = y=6 (input.y=5 + 1 border)
    // 好 was at inner.x=1 → trailing cell at x=2
    let trailing = &buffer[(2, 6)];
    assert_eq!(
        trailing.bg, SOLARIZED_LIGHT.panel,
        "trailing cell of deleted wide char in input must get panel bg"
    );
    let sym = trailing.symbol();
    assert!(
        sym.is_empty() || sym == " ",
        "trailing cell must be blank, got {sym:?}"
    );
}

#[test]
fn wide_scroll_lines_leave_no_stale_trailing_cells() {
    // Regression: in the scrollback area (no Block repaint), trailing
    // cells of wide glyphs must carry the line's background style so
    // the completed buffer has the correct background.
    //
    // set_stringn resets covered cells; the fixup loop re-styles them
    // with the glyph's own style.  ratatui's BufferDiff force-emits
    // trailing cells when a previous wide glyph is replaced by narrower
    // content (the previous_width > cell_width path in
    // diff_cell_state), provided the previous background is non-Reset.
    //
    // We test this by verifying that trailing cells in the completed
    // (backend) buffer have the line's Solarized bg after a single
    // draw.
    let backend = ratatui::backend::TestBackend::new(12, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = TuiState::default();
    state.push_line("你好数据".into(), LineKind::Normal);
    state.window.follow_bottom = false;

    // Capture the CompletedFrame to check the rendered buffer.
    // The CompletedFrame.buffer is the non-current buffer after
    // swap_buffers — it holds exactly what the frame rendered.
    let completed = draw(&mut terminal, &mut state).unwrap();
    let frame_buf = completed.buffer;

    // The trailing cells (odd x positions) must carry the Normal
    // line's background style.  This is the condition that allows
    // BufferDiff to force-emit them when a wide glyph is later
    // replaced by narrower content (previous bg non-Reset).
    let line_bg = SOLARIZED_LIGHT.background;
    for trailing_x in [1, 3, 5, 7] {
        assert_eq!(
            frame_buf[(trailing_x, 0)].bg,
            line_bg,
            "trailing cell ({trailing_x}, 0) in frame buffer must have line bg, not Reset"
        );
    }

    // The glyph cells themselves must still be visible.
    assert_eq!(frame_buf[(0, 0)].symbol(), "你");
    assert_eq!(frame_buf[(2, 0)].symbol(), "好");
    assert_eq!(frame_buf[(4, 0)].symbol(), "数");
    assert_eq!(frame_buf[(6, 0)].symbol(), "据");
}

#[test]
fn wide_glyph_scroll_emits_trailing_cell_after_glyph() {
    // Regression: when a wide CJK glyph moves between frames (e.g. " 好"
    // → "好 " after scrolling), Ratatui's diff skips the cell immediately
    // after the wide glyph (x + width) because the logical content appears
    // unchanged (both frames carry a Solarized background).  The terminal
    // physically painted the old wide glyph across its trailing column,
    // so that column must be force-emitted to clear the stale glyph half.
    //
    // The post-render pass in force_wide_trailing_cell_updates marks
    // every cell at x + width with CellDiffOption::AlwaysUpdate so the
    // diff always emits it regardless of logical equality.
    let backend = ratatui::backend::TestBackend::new(6, 4);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = TuiState {
        lines: vec![
            DisplayLine {
                text: " 好".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "好 ".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        ..Default::default()
    };

    // Frame 1: display " 好" (scroll to first line).
    // The wide glyph 好 sits at column 1, width=2, so its trailing
    // cell occupies column 2.  After the draw, the post-render pass
    // marks column 1+2=3 (x + width) with AlwaysUpdate.
    state.window.source_start = 0;
    state.window.source_end = 1;
    state.window.local_offset = 0;
    state.window.follow_bottom = false;
    state.inner_width = 6;
    let completed = draw(&mut terminal, &mut state).unwrap();
    let frame_buf = completed.buffer;

    // 好 at (1), covers (1,2).  Column 3 (x+width=1+2) gets AlwaysUpdate.
    assert_eq!(frame_buf[(1, 0)].symbol(), "好");
    assert_eq!(
        frame_buf[(3, 0)].diff_option,
        CellDiffOption::AlwaysUpdate,
        "cell after a wide glyph must be marked AlwaysUpdate"
    );

    // Frame 2: scroll to show "好 " (second line).
    // Now 好 sits at column 0, width=2, covering columns 0-1.
    // Column 2 becomes a plain space.  The post-render pass marks
    // column 0+2=2 with AlwaysUpdate so the diff emits it even though
    // it logically compares equal to column 2 of of the previous frame
    // (both had Solarized background, both have empty/space symbol).
    state.window.source_start = 1;
    state.window.source_end = 2;
    state.window.local_offset = 0;
    let completed = draw(&mut terminal, &mut state).unwrap();
    let frame_buf = completed.buffer;

    assert_eq!(frame_buf[(0, 0)].symbol(), "好");
    assert_eq!(
        frame_buf[(2, 0)].diff_option,
        CellDiffOption::AlwaysUpdate,
        "cell exposed after wide-glyph scroll must be AlwaysUpdate"
    );
    assert_eq!(frame_buf[(2, 0)].symbol(), " ");
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
                collapsed_summary: None,
            },
            DisplayLine {
                text: "   ".into(),
                kind: LineKind::ToolCall,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "+    7 wrapped diff text  ".into(),
                kind: LineKind::Added,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "-    8 trailing   ".into(),
                kind: LineKind::Removed,
                collapsed_summary: None,
            },
        ],
        ..Default::default()
    };

    state.follow();
    draw(&mut terminal, &mut state).unwrap();
    state.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    draw(&mut terminal, &mut state).unwrap();
    assert_eq!(state.window.local_offset, 0);
    assert_eq!(state.window.source_start, 0);
    assert!(!state.window.follow_bottom);
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
    let (handle, sink, _source) = crate::runner::session_test_channel();
    sink.emit(AgentEvent::Usage {
        context_input: 1_500,
        context_window: Some(4_096),
        session: crate::agent::Usage {
            input_tokens: 1_500,
            output_tokens: 0,
        },
    });
    let mut state = TuiState::default();
    let snapshot = handle.snapshot();
    let status = handle.status();
    state.attach(
        7,
        "demo task".into(),
        handle,
        "deepseek-v4-flash".into(),
        Some("fixer".into()),
        "/repo".into(),
        "sub-abc123".into(),
        None,
        snapshot,
        status,
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
        bottom.contains("deepseek-v4-flash fixer"),
        "bottom-left shows model role, got: {bottom:?}"
    );
    assert!(
        bottom.contains("/repo 1.5k"),
        "bottom-right shows cwd usage, got: {bottom:?}"
    );
    let top = row_text(9);
    assert!(
        top.contains("sub-abc123"),
        "top-right shows the session id, got: {top:?}"
    );
    // The title (top-row of the input block) starts with the status
    // followed by the subagent label. After attach with no
    // BackgroundCompleted, the subagent is in thinking state.
    assert!(
        top.contains("thinking… — subagent #7"),
        "title must show status before subagent label, got: {top:?}"
    );
    // The first display character after the block drawing is the spinner.
    let first_chars: String = top.chars().take(2).collect();
    assert!(
        first_chars.starts_with('┌'),
        "title row must start with the top-left corner, got: {first_chars:?}"
    );
    assert!(
        first_chars.chars().nth(1) == Some('⠋'),
        "spinner must be the second character (after the corner), got: {first_chars:?}"
    );
}

#[test]
fn attached_title_shows_finished_before_subagent_label() {
    let backend = ratatui::backend::TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let (_handle, sink, _source) = crate::runner::session_test_channel();
    // Emit a completion so the attach marks the view as finished.
    sink.emit(AgentEvent::BackgroundCompleted {
        id: 3,
        output: "done".into(),
        label: None,
    });
    let mut state = TuiState::default();
    let snapshot = _handle.snapshot();
    let status = _handle.status();
    state.attach(
        3,
        "quick job".into(),
        _handle,
        String::new(),
        None,
        String::new(),
        String::new(),
        None,
        snapshot,
        status,
    );
    draw(&mut terminal, &mut state).unwrap();

    let buffer = terminal.backend().buffer();
    let top: String = (0..60)
        .map(|x| buffer[(x, 9)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(
        top.contains("finished — subagent #3"),
        "finished view must have status before subagent label, got: {top:?}"
    );
    assert!(
        !top.contains("(finished)"),
        "must not use old parenthesized format, got: {top:?}"
    );
}

#[test]
fn attached_title_shows_idle_status_before_subagent_label() {
    let backend = ratatui::backend::TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let (_handle, sink, _source) = crate::runner::session_test_channel();
    // Emit a user prompt so the session has content but is not busy:
    // with the status receiver the title reflects the runner's real
    // state, and an idle (waiting-for-input) subagent shows "idle" —
    // the old synthetic "running" state is gone.
    sink.emit(AgentEvent::UserPrompt("do work".into()));
    sink.emit(AgentEvent::AssistantText("working...".into()));
    let (handle2, _sink2, _source2) = crate::runner::session_test_channel();
    let mut state = TuiState::default();
    let snapshot = handle2.snapshot();
    let status = handle2.status();
    state.attach(
        5,
        "long job".into(),
        handle2,
        String::new(),
        None,
        String::new(),
        String::new(),
        None,
        snapshot,
        status,
    );
    // The attach with no messages sets busy=thinking; override to
    // simulate an idle subagent (status Idle, no spinner), which is
    // what run_inner's per-iteration status refresh would do.
    let attached = state.attached.as_mut().unwrap();
    attached.state.busy = None;
    attached.finished = false;
    draw(&mut terminal, &mut state).unwrap();

    let buffer = terminal.backend().buffer();
    let top: String = (0..60)
        .map(|x| buffer[(x, 9)].symbol().chars().next().unwrap_or(' '))
        .collect();
    assert!(
        top.contains("idle — subagent #5"),
        "idle view must have status before subagent label, got: {top:?}"
    );
    assert!(
        !top.contains("(idle)"),
        "must not use old parenthesized format, got: {top:?}"
    );
}

#[test]
fn cwd_usage_text_shortens_home_to_tilde() {
    let home = std::env::var_os("HOME").expect("tests run with HOME set");
    let home = home.to_string_lossy().into_owned();
    let (text, fg) = cwd_usage_text(&format!("{home}/work"), 0, None);
    assert_eq!(text, "~/work", "paths under $HOME collapse to ~");
    assert_eq!(fg, SOLARIZED_LIGHT.orange, "zero tokens uses orange");
    let (text, _fg) = cwd_usage_text(&home, 0, None);
    assert_eq!(text, "~", "$HOME itself collapses to ~");
    let (text, _fg) = cwd_usage_text("/elsewhere", 0, None);
    assert_eq!(text, "/elsewhere", "paths outside $HOME stay absolute");
    let (text, _fg) = cwd_usage_text(&format!("{home}x"), 0, None);
    assert_eq!(
        text,
        format!("{home}x"),
        "a sibling sharing the prefix is not shortened"
    );
    let (text, _fg) = cwd_usage_text(&format!("{home}/work"), 1_500, None);
    assert_eq!(
        text, "~/work 1.5k",
        "token count still appended after shortening"
    );
}

#[test]
fn cwd_usage_text_shows_percentage_with_context_window() {
    let (text, fg) = cwd_usage_text("/repo", 45_000, Some(131_072));
    assert_eq!(text, "/repo 45.0k 34%");
    assert_eq!(fg, SOLARIZED_LIGHT.orange, "34% < 80% uses orange");
}

#[test]
fn cwd_usage_text_uses_red_at_80_percent_or_higher() {
    let (text, fg) = cwd_usage_text("/repo", 104_858, Some(131_072));
    // 104858 / 131072 ≈ 80.0% — at the boundary
    assert!(text.contains("80%"), "text = {text:?}");
    assert_eq!(fg, SOLARIZED_LIGHT.red, ">= 80% uses red");

    let (text, fg) = cwd_usage_text("/repo", 131_072, Some(131_072));
    assert!(text.contains("100%"), "text = {text:?}");
    assert_eq!(fg, SOLARIZED_LIGHT.red);

    let (_text, fg) = cwd_usage_text("/repo", 1_500, Some(131_072));
    assert_eq!(fg, SOLARIZED_LIGHT.orange, "1% uses orange");
}

#[test]
fn cwd_usage_text_behaves_normally_without_window() {
    let (text, fg) = cwd_usage_text("/repo", 1_500, None);
    assert_eq!(text, "/repo 1.5k");
    assert_eq!(fg, SOLARIZED_LIGHT.orange);
}

#[tokio::test]
async fn tasks_panel_tags_rows_with_the_agent_role() {
    let backend = ratatui::backend::TestBackend::new(50, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(".").unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    background.set_event_sender(sender);
    background
        .spawn_with_id(
            "find the render site".into(),
            Some("explorer".into()),
            None,
            None, // display_meta
            |_| {},
            || async { "done".into() },
        )
        .unwrap();
    background
        .spawn_with_id(
            "sleep 5".into(),
            None,
            None,
            None, // display_meta
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

#[tokio::test]
async fn tasks_panel_selection_highlight_shows_on_non_attachable_task() {
    // Every selected task row uses SOLARIZED_LIGHT.selection as the
    // background, regardless of attachability (the old guard only
    // highlighted attachable tasks).
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(".").unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    background.set_event_sender(sender);
    for label in ["task-alpha", "task-beta"] {
        background
            .spawn_with_id(
                label.into(),
                None,
                None,
                None,
                |_| {},
                || async { std::future::pending::<String>().await },
            )
            .unwrap();
    }
    let mut state = TuiState {
        background: Some(background),
        show_tasks: true,
        task_cursor: 0,
        ..Default::default()
    };

    let backend = ratatui::backend::TestBackend::new(50, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    draw(&mut terminal, &mut state).unwrap();

    let buf = terminal.backend().buffer();
    // tasks_bar starts at y=6 (height 5); block inner starts at (1, 7).
    // Header is at y=7, task rows at y=8 (cursor=0) and y=9.
    assert_eq!(
        buf[(1, 8)].bg,
        SOLARIZED_LIGHT.selection,
        "selected task row has selection background"
    );
    assert_eq!(
        buf[(1, 9)].bg,
        SOLARIZED_LIGHT.panel,
        "non-selected task row keeps panel background"
    );

    // Same selection for cursor=1 (second task).
    state.task_cursor = 1;
    draw(&mut terminal, &mut state).unwrap();
    let buf = terminal.backend().buffer();
    assert_eq!(
        buf[(1, 9)].bg,
        SOLARIZED_LIGHT.selection,
        "cursor=1 highlights the second task"
    );
    assert_eq!(
        buf[(1, 8)].bg,
        SOLARIZED_LIGHT.panel,
        "non-selected first task keeps panel background"
    );
}

#[tokio::test]
async fn cancelled_task_is_not_reported_as_unfinished_next_start() {
    let temp = tempfile::tempdir().unwrap();
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(temp.path()).unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    background
        .spawn_with_id(
            "task".into(),
            None,
            None,
            None,
            |_| {},
            || async { std::future::pending::<String>().await },
        )
        .unwrap();
    let id = background.running()[0].id;
    crate::session::Session::record_background_start(
        temp.path(),
        "cancel-store",
        id,
        "task",
        None,
        None,
    )
    .unwrap();
    let mut state = TuiState {
        background: Some(background),
        cwd: temp.path().display().to_string(),
        session_id: "cancel-store".into(),
        ..Default::default()
    };

    state.cancel_selected_task();

    assert!(
        crate::session::Session::take_unfinished_background(temp.path(), "cancel-store").is_empty()
    );
}

#[tokio::test]
async fn cancel_selected_task_cancels_any_task_and_clamps_cursor() {
    // cancel_selected_task must remove the task at cursor even when the
    // attachable probe returns true for that id, then clamp cursor.
    let (_, mut background) = crate::tools::builtins(
        crate::workspace::Workspace::new(".").unwrap(),
        None,
        false,
        None,
    );
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    background.set_event_sender(sender);

    // Three never-finishing tasks (ids 1, 2, 3).
    for _ in 0..3 {
        background
            .spawn_with_id(
                "task".into(),
                None,
                None,
                None,
                |_| {},
                || async { std::future::pending::<String>().await },
            )
            .unwrap();
    }
    let running = background.running();
    assert_eq!(running.len(), 3);
    let task2_id = running[1].id; // middle task

    // Attachable probe returns true for task2_id specifically.
    let mut state = TuiState {
        background: Some(background),
        task_cursor: 1,
        ..Default::default()
    };
    state.attachable = Some(Box::new(move |id| id == task2_id));

    // Cancel the attachable task — should succeed despite the probe.
    state.cancel_selected_task();
    tokio::task::yield_now().await; // let spawned task abort settle

    let updated = state
        .background
        .as_ref()
        .map(|b| b.running())
        .unwrap_or_default();
    assert_eq!(
        updated.len(),
        2,
        "cancelled attachable task was removed from registry"
    );
    assert!(
        !updated.iter().any(|t| t.id == task2_id),
        "task {task2_id} was cancelled"
    );
    // Cursor clamped to min(1, 2-1) = 1 (still valid).
    assert_eq!(state.task_cursor, 1);

    // Cancel the last task: cursor should clamp downward.
    state.task_cursor = 1;
    let last_id = state
        .background
        .as_ref()
        .map(|b| b.running())
        .unwrap_or_default()[1]
        .id;
    state.cancel_selected_task();
    tokio::task::yield_now().await;

    let remaining = state
        .background
        .as_ref()
        .map(|b| b.running())
        .unwrap_or_default();
    assert_eq!(remaining.len(), 1);
    assert!(!remaining.iter().any(|t| t.id == last_id));
    // Cursor clamped from 1 to min(1, 1-1) = 0.
    assert_eq!(state.task_cursor, 0);

    // Cancel the last remaining task: cursor stays at 0 (empty list).
    state.cancel_selected_task();
    tokio::task::yield_now().await;
    let empty = state
        .background
        .as_ref()
        .map(|b| b.running())
        .unwrap_or_default();
    assert!(empty.is_empty());
    assert_eq!(state.task_cursor, 0, "cursor at 0 when list is empty");
}

#[test]
fn handle_panel_key_routes_x_ctrl_c_esc_f2() {
    let mut state = TuiState {
        show_tasks: true,
        ..Default::default()
    };

    // Ctrl-C → CancelTask (does not close panel).
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(state.handle_panel_key(ctrl_c), PanelAction::CancelTask);
    assert!(state.show_tasks, "CancelTask does not close panel");

    // x → CancelTask.
    let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty());
    assert_eq!(state.handle_panel_key(x), PanelAction::CancelTask);
    assert!(state.show_tasks);

    // Esc → ClosePanel.
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
    assert_eq!(state.handle_panel_key(esc), PanelAction::ClosePanel);
    assert!(!state.show_tasks, "ClosePanel hides the panel");

    // F2 → ClosePanel.
    state.show_tasks = true;
    let f2 = KeyEvent::new(KeyCode::F(2), KeyModifiers::empty());
    assert_eq!(state.handle_panel_key(f2), PanelAction::ClosePanel);
    assert!(!state.show_tasks);

    // Plain character → Passthrough (panel stays open).
    state.show_tasks = true;
    let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
    assert_eq!(state.handle_panel_key(a), PanelAction::Passthrough);
    assert!(state.show_tasks, "Passthrough leaves the panel open");
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
    state.window.follow_bottom = false;
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
fn dim_line_does_not_reset_markdown_code_fence() {
    // A background-completion aside (Dim) interleaving with a streaming
    // assistant message must not reset the code-fence state: the fenced
    // lines after the aside keep the panel background.
    let backend = ratatui::backend::TestBackend::new(40, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = TuiState::default();
    state.push_line("before\n```\ncode one".into(), LineKind::Normal);
    state.push_line("[background task 3 completed]".to_string(), LineKind::Dim);
    state.push_line("code two\n```\nafter".into(), LineKind::Normal);
    state.window.follow_bottom = false;
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
    assert_eq!(row_text(2), "code one");
    assert_eq!(row_text(3), "[background task 3 completed]");
    assert_eq!(row_text(4), "code two");
    assert_eq!(row_text(5), "```");
    assert_eq!(row_text(6), "after");
    // Both fenced lines keep the panel background across the Dim aside.
    assert_eq!(buffer[(0, 2)].bg, SOLARIZED_LIGHT.element);
    assert_eq!(buffer[(0, 4)].bg, SOLARIZED_LIGHT.element);
    // Lines after the fence close are back on the body background.
    assert_eq!(buffer[(0, 6)].bg, SOLARIZED_LIGHT.background);
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
    state.window.follow_bottom = false;
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
        window: ScrollWindow {
            follow_bottom: false,
            source_end: 8, // matches lines.len()
            ..Default::default()
        },
        lines: vec![
            DisplayLine {
                text: "normal".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "dim".into(),
                kind: LineKind::Dim,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "user".into(),
                kind: LineKind::User,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "tool".into(),
                kind: LineKind::ToolCall,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "ok".into(),
                kind: LineKind::ToolResult,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "error".into(),
                kind: LineKind::ToolError,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "+    7 added".into(),
                kind: LineKind::Added,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "-    7 removed".into(),
                kind: LineKind::Removed,
                collapsed_summary: None,
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
fn busy_enqueue_renders_queue_bar() {
    let backend = ratatui::backend::TestBackend::new(50, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = TuiState {
        busy: Some(BusyState::thinking()),
        ..TuiState::default()
    };
    state.push_agent_event(AgentEvent::PromptQueued("follow up".into()));
    draw(&mut terminal, &mut state).unwrap();

    let buffer = terminal.backend().buffer();
    // input is three rows high; with one queue row, the banner sits at y=6.
    let row: String = buffer.content()[6 * 50..7 * 50]
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(row.contains("queued 1: follow up"));
    let first = &buffer[(0, 6)];
    assert_eq!(first.fg, SOLARIZED_LIGHT.ink);
    assert_eq!(first.bg, SOLARIZED_LIGHT.blue);
    assert!(first.modifier.contains(Modifier::BOLD));
    assert_eq!(buffer[(49, 6)].bg, SOLARIZED_LIGHT.blue);
    assert!(state.lines.is_empty(), "queued prompt entered scrollback");
}

#[test]
fn prompt_consumption_drains_duplicate_prompts_fifo() {
    let mut state = TuiState::default();
    state.push_agent_event(AgentEvent::PromptQueued("same".into()));
    state.push_agent_event(AgentEvent::PromptQueued("same".into()));
    state.push_agent_event(AgentEvent::PromptQueued("last".into()));

    state.push_agent_event(AgentEvent::PromptConsumed);
    assert_eq!(
        state.queued.iter().cloned().collect::<Vec<_>>(),
        ["same", "last"]
    );
    state.push_agent_event(AgentEvent::UserPrompt("same".into()));
    assert_eq!(state.lines.last().unwrap().text, "you> same");
}

#[test]
fn user_prompt_resets_active_stream_lane() {
    let mut state = TuiState::default();
    state.push_agent_event(AgentEvent::AssistantDelta("answer".into()));
    state.push_agent_event(AgentEvent::UserPrompt("next".into()));
    state.push_agent_event(AgentEvent::AssistantDelta("new answer".into()));

    assert_eq!(state.lines.len(), 3);
    assert_eq!(state.lines[1].text, "you> next");
    assert_eq!(state.lines[2].text, "new answer");
}

#[test]
fn background_completion_ends_stream_lane_so_next_delta_starts_fresh_line() {
    // A background-completion notice interleaving with a streaming
    // assistant message must end the active lane: otherwise the next
    // AssistantDelta appends to the Dim line and the body loses its
    // markdown rendering (Dim lines are plain-text wrapped).
    let mut state = TuiState::default();
    state.push_agent_event(AgentEvent::AssistantDelta("```rust\nlet x = 1;".into()));
    state.push_agent_event(AgentEvent::BackgroundCompletionNotice {
        id: 7,
        output: "built ok".into(),
        label: Some("build".into()),
    });
    state.push_agent_event(AgentEvent::AssistantDelta("\nlet y = 2;\n```".into()));

    assert_eq!(
        state.lines.len(),
        4,
        "body, header, body line, fresh body line"
    );
    assert_eq!(state.lines[0].kind, LineKind::Normal);
    assert_eq!(state.lines[0].text, "```rust\nlet x = 1;");
    assert_eq!(state.lines[1].kind, LineKind::Dim);
    assert!(
        state.lines[1]
            .text
            .starts_with("[background task 7 completed")
    );
    assert_eq!(state.lines[2].kind, LineKind::Dim);
    assert_eq!(state.lines[2].text, "built ok");
    // The delta after the notice starts a NEW Normal line instead of
    // appending to the Dim line.
    assert_eq!(state.lines[3].kind, LineKind::Normal);
    assert_eq!(state.lines[3].text, "\nlet y = 2;\n```");
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

/// Extract the text of a rendered visual row (same span-join as the
/// `text` closures in ux_tests).
fn rendered_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Parse the `(N 行)` row count out of a collapsed thinking summary row.
fn summary_row_count(line: &ratatui::text::Line<'_>) -> usize {
    let text = rendered_text(line);
    text.split_once('(')
        .and_then(|(_, rest)| rest.split_once(" 行)"))
        .and_then(|(n, _)| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no (N 行) count in summary row: {text:?}"))
}

#[test]
fn thinking_lines_render_collapsed_by_default() {
    // A fresh session collapses model reasoning to a single summary row.
    let mut state = TuiState::default();
    assert!(
        state.collapse_thinking.0,
        "new sessions default to collapsed"
    );
    state.push_agent_event(AgentEvent::ReasoningDelta("plan ".into()));
    state.push_agent_event(AgentEvent::ReasoningDelta("more".into()));
    assert_eq!(state.lines.len(), 1);
    assert_eq!(state.lines[0].kind, LineKind::Thinking);
    // Scroll accounting and rendering must agree: one visual row.
    assert_eq!(line_visual_rows(&state.lines[0], 40, true), 1);
    let visual = render_window(&state.lines, 0, state.lines.len(), 40, true);
    assert_eq!(
        visual.len(),
        1,
        "collapsed thinking renders one summary row"
    );
    let text = rendered_text(&visual[0]);
    assert!(text.starts_with("▸ thinking"), "{text}");
    assert!(text.contains("(1 行)"), "{text}");
    assert!(text.contains("Tab 展开"), "{text}");
}

#[test]
fn tab_toggles_thinking_collapse_between_summary_and_full_text() {
    let mut state = TuiState::default();
    // Long enough to wrap at width 20, so the expanded form is multi-row.
    state.push_agent_event(AgentEvent::ReasoningDelta("word ".repeat(30)));
    let full_rows = hard_wrap(&state.lines[0].text, 20).len();
    assert!(full_rows > 1, "test text must wrap when expanded");
    assert_eq!(line_visual_rows(&state.lines[0], 20, true), 1);
    assert_eq!(render_window(&state.lines, 0, 1, 20, true).len(), 1);

    // Tab expands: the full wrapped text returns, and scroll accounting
    // follows the same flag.
    let _ = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(!state.collapse_thinking.0);
    assert_eq!(line_visual_rows(&state.lines[0], 20, false), full_rows);
    assert_eq!(
        render_window(&state.lines, 0, 1, 20, false).len(),
        full_rows
    );

    // Tab again collapses back to a single summary row.
    let _ = state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(state.collapse_thinking.0);
    assert_eq!(render_window(&state.lines, 0, 1, 20, true).len(), 1);
}

#[test]
fn collapsed_thinking_summary_count_tracks_streaming_deltas() {
    let mut state = TuiState::default();
    assert!(state.collapse_thinking.0);
    state.push_agent_event(AgentEvent::ReasoningDelta("alpha ".repeat(30)));
    assert_eq!(state.lines.len(), 1);
    let n_before = summary_row_count(&render_window(&state.lines, 0, 1, 20, true)[0]);

    // Streaming deltas keep appending to the same line while collapsed;
    // the summary's row count grows with the accumulated text.
    state.push_agent_event(AgentEvent::ReasoningDelta("beta ".repeat(60)));
    assert_eq!(
        state.lines.len(),
        1,
        "deltas still merge into one thinking line"
    );
    let visual = render_window(&state.lines, 0, 1, 20, true);
    assert_eq!(visual.len(), 1, "still a single collapsed summary row");
    let n_after = summary_row_count(&visual[0]);
    assert!(
        n_after > n_before,
        "summary count grew {n_before} -> {n_after}"
    );
}

#[test]
fn collapsed_thinking_summary_count_survives_local_window_truncation() {
    // Production rendering goes through local_window_lines, which caps the
    // window at MAX_RENDER_BYTES. A thinking block larger than the cap is
    // truncated there; the collapsed summary's (N 行) count must still
    // reflect the FULL pre-truncation text, not the truncated tail. The
    // count now comes from the per-line cache, which is refreshed on the
    // source line when the text changes (the width must be known — a draw
    // has happened — for the cache to be populated).
    let mut state = TuiState::default();
    assert!(state.collapse_thinking.0);
    let width = 20;
    state.inner_width = width;
    state.push_agent_event(AgentEvent::ReasoningDelta("word ".repeat(MAX_RENDER_BYTES)));
    assert_eq!(state.lines.len(), 1);
    assert!(
        state.lines[0].text.len() > MAX_RENDER_BYTES,
        "test text must exceed the local-window byte cap"
    );
    let full_rows = hard_wrap(&state.lines[0].text, width).len();
    let window = ScrollWindow {
        source_start: 0,
        source_end: 1,
        ..ScrollWindow::new()
    };
    // The source line carries the cache computed from its full text.
    let expected_summary = collapsed_summary_for(&state.lines[0].text, width);
    assert_eq!(
        state.lines[0]
            .collapsed_summary
            .as_ref()
            .map(|c| c.text.as_str()),
        Some(expected_summary.as_str()),
        "delta append must cache the full-text summary"
    );

    // Collapsed: the local copy truncates the text (the byte cap applies to
    // every line now) but carries the cache, so the summary count is exact.
    let local = local_window_lines(&state.lines, &window, true);
    assert_eq!(local.len(), 1);
    assert!(
        local[0].text.len() < state.lines[0].text.len(),
        "collapsed truncated thinking must no longer keep its full text"
    );
    assert_eq!(local[0].text.len(), MAX_RENDER_BYTES);
    assert_eq!(
        local[0].collapsed_summary.as_ref().map(|c| c.text.as_str()),
        Some(expected_summary.as_str()),
        "the cache is cloned into the local copy"
    );
    let visual = render_window(&local, 0, 1, width, true);
    assert_eq!(visual.len(), 1, "still a single collapsed summary row");
    assert_eq!(
        summary_row_count(&visual[0]),
        full_rows,
        "summary count must reflect the full pre-truncation text"
    );
    // Sanity: the truncated tail alone would underestimate the count.
    let tail = &state.lines[0].text[state.lines[0].text.len() - MAX_RENDER_BYTES..];
    assert!(hard_wrap(tail, width).len() < full_rows);

    // Expanded (or non-thinking lines): the byte cap stays in force and
    // only the bounded tail is carried into the renderer.
    let local_expanded = local_window_lines(&state.lines, &window, false);
    assert_eq!(local_expanded.len(), 1);
    assert!(
        local_expanded[0].text.len() < state.lines[0].text.len(),
        "expanded path must keep the bounded tail"
    );
    assert!(local_expanded[0].text.len() <= MAX_RENDER_BYTES);
}

#[test]
fn collapsed_thinking_render_reads_cache_and_stays_bounded() {
    // The collapsed render path must read the per-line cache instead of
    // hard-wrapping the full text: rendering a thinking block larger than
    // the 64 KiB local-window cap produces exactly one small summary row
    // whose text is the cached string — the body is never materialized.
    let mut state = TuiState::default();
    assert!(state.collapse_thinking.0);
    let width = 30;
    state.inner_width = width;
    state.push_agent_event(AgentEvent::ReasoningDelta("word ".repeat(MAX_RENDER_BYTES)));
    assert!(state.lines[0].text.len() > MAX_RENDER_BYTES);
    let summary = state.lines[0]
        .collapsed_summary
        .as_ref()
        .map(|c| c.text.clone())
        .expect("cache is populated at append time once the width is known");
    let window = ScrollWindow {
        source_start: 0,
        source_end: 1,
        ..ScrollWindow::new()
    };
    // The bounded local copy feeds rendering, exactly like production draw.
    let local = local_window_lines(&state.lines, &window, true);
    assert_eq!(local[0].text.len(), MAX_RENDER_BYTES);
    assert_eq!(
        local[0].collapsed_summary.as_ref().map(|c| c.text.as_str()),
        Some(summary.as_str()),
        "cache is carried through the truncating local window"
    );
    let visual = render_window(&local, 0, 1, width, true);
    assert_eq!(visual.len(), 1, "collapsed renders exactly one row");
    let text = rendered_text(&visual[0]);
    assert_eq!(text, summary, "render outputs the cache verbatim");
    assert!(
        text.len() < MAX_RENDER_BYTES && !text.contains("word"),
        "rendered output is the small summary, not the wrapped body"
    );
}

#[test]
fn incremental_rows_matches_full_wrap_on_edge_cases() {
    // Every (old, delta) pair must extend the wrapped state exactly as
    // hard_wrap of the concatenated text would: partial-last-row merge,
    // exactly-full last row, empty text / trailing-newline empty row, wide
    // chars (exact fit and overflow), newlines inside and at the head of
    // the delta. Swept across many widths so the boundary conditions
    // (last_width == 0, last_width == width, wide-char fits) all fire.
    let cases: &[(&str, &str)] = &[
        ("ab", "cd"),
        ("ab", "cdef"),
        ("ab", "中"),
        ("ab", "中a"),
        ("a", "中"),        // wide char fits exactly in the free space
        ("ab中", "x"),      // wide char inside the old last row
        ("abcd", "e"),      // last row exactly full
        ("abcd", "\nrest"), // full last row + newline-headed delta
        ("abcd", "\n"),
        ("abcd\nwxyz", "\nrest"),
        ("", "hello"),    // empty text
        ("", "中中"),     // empty text + wide chars
        ("ab\n", "x"),    // trailing newline: empty last row
        ("ab\n", "\n"),   // trailing newline + newline delta
        ("ab", "\nrest"), // partial last row + newline-headed delta
        ("ab", "\n"),     // delta is only a newline
        ("ab", "\n\nrest"),
        // Zero-width characters (combining accent U+0301, variation
        // selector U+FE0F, ZWJ U+200D) never fire a wrap break: they must
        // attach to the old row — even one that is exactly full — instead
        // of starting a phantom new row, exactly like hard_wrap of the
        // concatenated text.
        ("a", "\u{0301}"),                  // zero-width delta onto a full last row
        ("a", "\u{0301}b"),                 // zero-width prefix then a normal char
        ("a", "\u{0301}\n"),                // zero-width prefix then a newline
        ("a", "\u{0301}\nrest"),            // zero-width prefix, newline, more text
        ("a", "\u{FE0F}"),                  // variation selector onto a full last row
        ("a", "\u{FE0F}x"),                 // variation selector then a normal char
        ("a", "x\u{FE0F}"),                 // variation selector after a char
        ("ab", "\u{0301}"),                 // zero-width delta onto a partial last row
        ("ab", "\u{0301}c"),                // zero-width prefix, partial last row
        ("ab", "\u{200D}\u{0301}\u{FE0F}"), // all-zero-width delta
        ("a", "\u{1F468}\u{200D}\u{1F469}"), // ZWJ family sequence
        ("\u{1F468}\u{200D}\u{1F469}", "x"), // old last row holds a ZWJ seq
        ("abcd\u{1F468}\u{200D}\u{1F469}", "\u{0301}"), // full row + ZWJ + accent
    ];
    for width in 1..=8usize {
        for &(old, delta) in cases {
            let (old_rows, old_last) = wrap_state(old, width);
            let got = incremental_rows(delta, width, old_rows, old_last);
            assert_eq!(
                got,
                wrap_state(&format!("{old}{delta}"), width),
                "old={old:?} delta={delta:?} width={width}"
            );
        }
    }
}

#[test]
fn collapsed_summary_incremental_deltas_match_full_wrap() {
    // Streaming many small ReasoningDeltas must maintain the cached
    // (N 行) count incrementally; at every step it must equal a full
    // hard_wrap of the accumulated text, including wide chars, newlines
    // and a delta that exactly fills the last row.
    let mut state = TuiState::default();
    let width = 23;
    state.inner_width = width;
    let deltas = [
        "word ".repeat(5),
        "中中中".to_string(),
        "narrow tail".to_string(),
        "\nnewline head".to_string(),
        "  pad  ".to_string(),
        "\u{0301}".to_string(),  // combining accent: zero-width chunk
        "x\u{FE0F}".to_string(), // variation selector after a char
        "\u{1F468}\u{200D}\u{1F469}".to_string(), // ZWJ family sequence
        "\n".to_string(),
        "final".to_string(),
    ];
    let mut full = String::new();
    for (i, delta) in deltas.iter().enumerate() {
        state.push_agent_event(AgentEvent::ReasoningDelta(delta.clone()));
        if i == 0 {
            full.push_str("thinking: ");
        }
        full.push_str(delta);
        let cache = state
            .lines
            .last()
            .unwrap()
            .collapsed_summary
            .as_ref()
            .expect("cache exists once the width is known");
        assert_eq!(cache.width, width);
        assert_eq!(
            cache.rows,
            wrap_state(&full, width).0,
            "incremental rows diverge after {i} deltas (text {} bytes)",
            full.len()
        );
        assert_eq!(
            cache.last_width,
            wrap_state(&full, width).1,
            "incremental last_width diverges after {i} deltas"
        );
    }
    // The rendered summary carries the same incremental count.
    let visual = render_window(&state.lines, 0, 1, width, true);
    assert_eq!(summary_row_count(&visual[0]), wrap_state(&full, width).0);

    // Resize: the full recompute at the new width must agree with a fresh
    // hard_wrap (and with the next incremental delta after the resize).
    state.refresh_window_collapsed_summaries(width + 7);
    let cache = state
        .lines
        .last()
        .unwrap()
        .collapsed_summary
        .as_ref()
        .unwrap();
    assert_eq!(
        cache.rows,
        wrap_state(&full, width + 7).0,
        "resize recompute must agree with a fresh full wrap"
    );
    state.inner_width = width + 7;
    state.push_agent_event(AgentEvent::ReasoningDelta("post-resize".into()));
    full.push_str("post-resize");
    let cache = state
        .lines
        .last()
        .unwrap()
        .collapsed_summary
        .as_ref()
        .unwrap();
    assert_eq!(
        cache.rows,
        wrap_state(&full, width + 7).0,
        "incremental delta after resize must stay exact"
    );
}

#[test]
fn refresh_window_collapsed_summaries_scan_is_bounded() {
    // Scrolling down from Home extends source_end without advancing
    // source_start (extend_window_down), so the window range can grow far
    // beyond MAX_RENDER_SOURCE_LINES. The per-frame refresh must scan only
    // the lines local_window_lines will actually render — the tail of the
    // window — not the whole browsed history.
    let mut state = TuiState::default();
    // Push with width unknown (inner_width == 0): no caches are populated,
    // so every Thinking line is stale and would be recomputed if scanned.
    let total = MAX_RENDER_SOURCE_LINES * 4;
    for i in 0..total {
        state.push_line(format!("thinking {i}"), LineKind::Thinking);
    }
    assert!(
        state.lines.iter().all(|l| l.collapsed_summary.is_none()),
        "precondition: no caches yet (width unknown at push time)"
    );
    // A window covering the whole scrollback, exactly like after paging
    // down from Home (source_start stayed 0, source_end reached the end).
    state.inner_width = 40;
    state.window.source_start = 0;
    state.window.source_end = total;
    state.window.follow_bottom = false;
    state.refresh_window_collapsed_summaries(40);
    let scanned = state
        .lines
        .iter()
        .filter(|l| l.collapsed_summary.is_some())
        .count();
    assert!(
        scanned <= MAX_RENDER_SOURCE_LINES,
        "refresh scanned {scanned} lines, must be bounded by MAX_RENDER_SOURCE_LINES"
    );
    // The scanned lines are exactly the tail local_window_lines selects:
    // the head of the window stays untouched, the last budget lines are
    // refreshed.
    assert!(
        state.lines[..total - MAX_RENDER_SOURCE_LINES]
            .iter()
            .all(|l| l.collapsed_summary.is_none()),
        "head lines outside the local window must not be scanned"
    );
    assert!(
        state.lines[total - MAX_RENDER_SOURCE_LINES..]
            .iter()
            .all(|l| l.collapsed_summary.is_some()),
        "tail lines inside the local window must all be refreshed"
    );
}

#[test]
fn attached_snapshot_replay_recomputes_summary_on_first_draw() {
    // Attach replay (problem: width unknown at append time): a fresh
    // TuiState (inner_width == 0, no draw yet) replays the snapshot
    // events, so append-time caching is skipped and every replayed
    // thinking line has NO cache. The first draw must recompute the
    // collapsed summary from the COMPLETE source line before
    // local_window_lines truncates it — the (N 行) count must not be
    // underestimated from the truncated tail.
    let (handle, emitter, _commands) = crate::runner::session_test_channel();
    let huge = "word ".repeat(MAX_RENDER_BYTES);
    emitter.emit(AgentEvent::ReasoningDelta(huge));
    let snapshot = handle.snapshot();
    let status = handle.status();
    let mut parent = TuiState::default();
    parent.attach(
        1,
        "task".into(),
        handle,
        String::new(),
        None,
        String::new(),
        String::new(),
        None,
        snapshot,
        status,
    );
    {
        let attached = parent.attached.as_mut().unwrap();
        assert_eq!(attached.state.lines.len(), 1);
        assert_eq!(
            attached.state.inner_width, 0,
            "attach replays before any draw"
        );
        assert!(
            attached.state.lines[0].collapsed_summary.is_none(),
            "no cache may be computed while the width is unknown"
        );
        assert!(attached.state.lines[0].text.len() > MAX_RENDER_BYTES);
    }
    let full_rows = hard_wrap(&parent.attached.as_ref().unwrap().state.lines[0].text, 44).len();

    // First draw at a real terminal width: the draw entry refreshes the
    // cache from the full source line before truncation.
    let backend = ratatui::backend::TestBackend::new(44, 10);
    let mut term = Terminal::new(backend).unwrap();
    draw(&mut term, &mut parent).unwrap();
    let attached = parent.attached.as_mut().unwrap();
    let cache = attached
        .state
        .lines
        .first()
        .unwrap()
        .collapsed_summary
        .as_ref()
        .expect("first draw populates the cache from the complete source line");
    assert_eq!(cache.width, 44, "cache bound to the real inner width");
    assert_eq!(
        cache.rows, full_rows,
        "count from the FULL pre-truncation text"
    );
    // And the truncated-tail fallback would have underestimated: prove the
    // rendered row shows the full count, not the tail's.
    let buffer = term.backend().buffer();
    let row = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .find(|r| r.contains("Tab"))
        .unwrap_or_else(|| panic!("no collapsed summary row rendered"));
    let shown: usize = row
        .split_once('(')
        .and_then(|(_, rest)| rest.split("行").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no (N 行) count in {row:?}"));
    assert_eq!(shown, full_rows);
    let tail = {
        let text = &attached.state.lines[0].text;
        &text[text.len() - MAX_RENDER_BYTES..]
    };
    assert!(
        hard_wrap(tail, 44).len() < full_rows,
        "the truncated tail alone would underestimate — the test must be meaningful"
    );
}

#[test]
fn collapsed_summary_recomputes_on_terminal_resize() {
    // Problem: the cache is bound to the width it was computed at, and a
    // resize only re-anchored the window — an ended reasoning line kept
    // its old-width (N 行) forever. The draw entry must re-bind the cache
    // to the current width from the complete source line.
    let mut state = TuiState {
        inner_width: 20,
        ..Default::default()
    };
    state.push_agent_event(AgentEvent::ReasoningDelta("word ".repeat(200)));
    let cache = state.lines[0].collapsed_summary.as_ref().unwrap();
    assert_eq!(cache.width, 20);
    let w20 = wrap_state(&state.lines[0].text, 20).0;
    assert_eq!(cache.rows, w20);

    // A draw at a wider terminal re-wraps the cache at the new width.
    // The scrollback area spans the full terminal width, so the draw
    // inner width is 44.
    let backend = ratatui::backend::TestBackend::new(44, 10);
    let mut term = Terminal::new(backend).unwrap();
    draw(&mut term, &mut state).unwrap();
    let w44 = wrap_state(&state.lines[0].text, 44).0;
    assert_ne!(w44, w20, "width change must change the count");
    let cache = state.lines[0].collapsed_summary.as_ref().unwrap();
    assert_eq!(cache.width, 44);
    assert_eq!(cache.rows, w44);
    let buffer = term.backend().buffer();
    let row = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .find(|r| r.contains("Tab"))
        .unwrap_or_else(|| panic!("no collapsed summary row rendered"));
    // The buffer splits wide glyphs across cells, so parse the count
    // tolerantly instead of matching the exact summary text.
    let shown: usize = row
        .split_once('(')
        .and_then(|(_, rest)| rest.split("行").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no (N 行) count in {row:?}"));
    assert_eq!(shown, w44);
    assert_ne!(shown, w20);

    // And the NEXT delta after the resize continues incrementally from the
    // new width (a stale-width cache would fall back to a full recompute —
    // correct, but the width-bound path must not regress to the old count).
    state.inner_width = 44;
    state.push_agent_event(AgentEvent::ReasoningDelta("more ".repeat(10)));
    let cache = state.lines[0].collapsed_summary.as_ref().unwrap();
    assert_eq!(
        cache.rows,
        wrap_state(&state.lines[0].text, 44).0,
        "delta after resize extends the width-fresh cache"
    );
}

#[test]
fn frozen_summary_rewraps_at_new_width_but_keeps_freeze_point() {
    // A frozen tail summary is pinned to the freeze-time text (deltas that
    // stream after the freeze must not grow its count), but a terminal
    // resize must re-wrap that freeze-time text at the new width instead of
    // pinning a stale old-width count forever.
    let mut state = TuiState {
        inner_width: 40,
        ..Default::default()
    };
    state.push_agent_event(AgentEvent::ReasoningDelta("alpha ".repeat(30)));
    state.handle_scroll(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    let frozen = state
        .window
        .frozen_tail_summary
        .clone()
        .expect("freeze pins the summary");
    assert_eq!(frozen.width, 40);
    let freeze_len = state.window.frozen_tail_cursor.unwrap();

    // Post-freeze deltas keep growing the live source line.
    state.push_agent_event(AgentEvent::ReasoningDelta("beta ".repeat(60)));
    assert!(state.lines.last().unwrap().text.len() > freeze_len);

    // Resize refresh: the frozen summary is re-wrapped from the text up to
    // the frozen cursor at the new width.
    state.refresh_window_collapsed_summaries(60);
    let re = state
        .window
        .frozen_tail_summary
        .as_ref()
        .expect("frozen summary survives the refresh");
    assert_eq!(re.width, 60);
    let freeze_text = &state.lines.last().unwrap().text[..freeze_len];
    assert_eq!(re.rows, wrap_state(freeze_text, 60).0);
    assert_ne!(
        re.rows,
        wrap_state(freeze_text, 40).0,
        "the re-wrap must change the count so the test is meaningful"
    );
    assert_ne!(
        re.rows,
        wrap_state(&state.lines.last().unwrap().text, 60).0,
        "post-freeze deltas must stay excluded from the frozen count"
    );
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
fn new_events_do_not_yank_a_scrolled_up_view() {
    let mut state = TuiState {
        lines: vec![
            DisplayLine {
                text: "one".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "two".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        ..Default::default()
    };
    state.window.source_start = 0;
    state.window.source_end = 2;
    state.window.follow_bottom = false;
    state.window.local_offset = 0;
    state.inner_width = 80;
    state.push_agent_event(AgentEvent::AssistantText("three".into()));
    assert!(!state.window.follow_bottom);
    assert_eq!(state.window.local_offset, 0);
    state.window.follow_bottom = true;
    state.push_agent_event(AgentEvent::AssistantText("four".into()));
    assert!(state.window.follow_bottom);
}

#[test]
fn home_and_end_jump_to_session_edges() {
    let mut state = TuiState {
        lines: vec![
            DisplayLine {
                text: "one".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "two".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        ..Default::default()
    };
    state.window.local_offset = 10;
    state.window.source_start = 0;
    state.window.source_end = 2;
    state.inner_width = 80;
    state.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(state.window.local_offset, 0);
    assert_eq!(state.window.source_start, 0);
    state.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert!(state.window.follow_bottom);
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
                collapsed_summary: None,
            },
            DisplayLine {
                text: "two".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        ..Default::default()
    };
    state.window.source_start = 0;
    state.window.source_end = 2;
    state.window.local_offset = 0;
    state.inner_width = 80;
    // Up disengages follow mode; then PageDown advances.
    state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert!(!state.window.follow_bottom);
    state.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert!(!state.window.follow_bottom);
    assert_eq!(state.window.local_offset, 10);
    // ToolResult ends a delta; at_bottom is false so follow is NOT called.
    state.push_agent_event(AgentEvent::ToolResult {
        is_error: false,
        content: "done".into(),
    });
    assert_eq!(state.lines.last().unwrap().text, "  ok: done");
    assert_eq!(state.lines.last().unwrap().kind, LineKind::ToolResult);
    assert!(!state.window.follow_bottom);

    // When following, a ToolResult keeps follow_bottom = true.
    state.window.follow_bottom = true;
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
            collapsed_summary: None,
        },
        DisplayLine {
            text: "one\ntwo\nthree".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        },
        DisplayLine {
            text: "x".repeat(25),
            kind: LineKind::Normal,
            collapsed_summary: None,
        },
    ];
    // 1 + 3 + exact hard wrap of 25 cells at width 10
    assert_eq!(wrapped_rows(&lines, 10, false), 1 + 3 + 3);
    let cjk = vec![DisplayLine {
        text: "你好世界".into(),
        kind: LineKind::Normal,
        collapsed_summary: None,
    }];
    // Each CJK char is 2 cells: "你好" then "世界" at width 5.
    assert_eq!(wrapped_rows(&cjk, 5, false), 2);
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

// ── older-history paging (Greptime segmented load) ──────────────────────

#[test]
fn up_at_scrollback_top_requests_older_history() {
    let scroll = |code| KeyEvent::new(code, KeyModifiers::empty());
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        session_id: "s1".into(),
        root: std::path::PathBuf::from("/tmp"),
        lines: vec![DisplayLine {
            text: "only line".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        window: ScrollWindow {
            source_start: 0,
            source_end: 1,
            local_offset: 0,
            follow_bottom: false,
            ..Default::default()
        },
        inner_width: 80,
        ..Default::default()
    };
    // Up at the local top queues one request…
    state.handle_scroll(scroll(KeyCode::Up));
    assert!(state.older_pending);
    // …repeated scroll keys collapse into the same single request…
    state.handle_scroll(scroll(KeyCode::Up));
    state.handle_scroll(scroll(KeyCode::PageUp));
    assert!(state.older_pending);
    // …and once history is exhausted no further requests fire.
    state.older_pending = false;
    state.older_done = true;
    state.handle_scroll(scroll(KeyCode::Up));
    assert!(!state.older_pending);
}

#[test]
fn scrolled_up_mid_scrollback_or_without_store_never_requests_older() {
    let scroll = |code| KeyEvent::new(code, KeyModifiers::empty());
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        lines: vec![
            DisplayLine {
                text: "a".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "b".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        window: ScrollWindow {
            source_start: 1,
            source_end: 2,
            local_offset: 5,
            follow_bottom: false,
            ..Default::default()
        },
        inner_width: 80,
        ..Default::default()
    };
    state.handle_scroll(scroll(KeyCode::Up));
    assert!(!state.older_pending, "mid-window Up does not reach the top");
    assert_eq!(state.window.local_offset, 4);

    // No store wired (tests, Default): even at the very top nothing is
    // requested.
    let mut state = TuiState {
        lines: vec![DisplayLine {
            text: "a".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        window: ScrollWindow {
            source_start: 0,
            source_end: 1,
            local_offset: 0,
            follow_bottom: false,
            ..Default::default()
        },
        inner_width: 80,
        ..Default::default()
    };
    state.handle_scroll(scroll(KeyCode::Up));
    assert!(!state.older_pending);
}

#[test]
fn prepend_lines_shifts_window_indices_and_keeps_viewport() {
    let mut state = TuiState {
        lines: vec![
            DisplayLine {
                text: "head-1".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "head-2".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "head-3".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        window: ScrollWindow {
            source_start: 1,
            source_end: 3,
            local_offset: 2,
            follow_bottom: false,
            frozen_source_end: 3,
            ..Default::default()
        },
        inner_width: 80,
        ..Default::default()
    };
    state.prepend_lines(vec![
        DisplayLine {
            text: "old-1".into(),
            kind: LineKind::Dim,
            collapsed_summary: None,
        },
        DisplayLine {
            text: "old-2".into(),
            kind: LineKind::Dim,
            collapsed_summary: None,
        },
    ]);
    assert_eq!(state.lines.len(), 5);
    assert_eq!(state.lines[0].text, "old-1");
    assert_eq!(state.lines[2].text, "head-1");
    // The window still covers head-2..head-3, now at shifted indices;
    // local_offset (a visual-row offset) and frozen state are untouched.
    assert_eq!(state.window.source_start, 3);
    assert_eq!(state.window.source_end, 5);
    assert_eq!(state.window.local_offset, 2);
    assert_eq!(state.window.frozen_source_end, 3);

    // An empty prepend is a no-op.
    let before = (
        state.lines.len(),
        state.window.source_start,
        state.window.source_end,
    );
    state.prepend_lines(Vec::new());
    assert_eq!(
        (
            state.lines.len(),
            state.window.source_start,
            state.window.source_end
        ),
        before
    );
}

#[test]
fn session_entry_to_lines_maps_persisted_entry_kinds() {
    use crate::agent::{AssistantMessage, Message, SessionEntry, ToolCall};

    let lines = |entry: &SessionEntry| session_entry_to_lines(entry);

    // User prompt → User line, same as the live UserPrompt rendering.
    let out = lines(&SessionEntry::Message {
        message: Message::User {
            content: "hello".into(),
            images: vec![],
        },
    });
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "you> hello");
    assert_eq!(out[0].kind, LineKind::User);

    // Assistant text → Normal line; a tool-call-only assistant renders
    // nothing (matches the resume replay, which has no ToolCall events).
    let out = lines(&SessionEntry::Message {
        message: Message::Assistant(AssistantMessage {
            content: Some("answer".into()),
            tool_calls: vec![],
            reasoning: None,
        }),
    });
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "answer");
    assert_eq!(out[0].kind, LineKind::Normal);
    let out = lines(&SessionEntry::Message {
        message: Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
            reasoning: None,
        }),
    });
    assert!(
        out.is_empty(),
        "tool-call-only assistant renders nothing on replay"
    );

    // Tool result → ok/error line like push_tool_result.
    let out = lines(&SessionEntry::Message {
        message: Message::Tool {
            call_id: "c1".into(),
            name: "bash".into(),
            content: "done".into(),
            is_error: false,
            synthetic: false,
        },
    });
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "  ok: done");
    assert_eq!(out[0].kind, LineKind::ToolResult);
    let out = lines(&SessionEntry::Message {
        message: Message::Tool {
            call_id: "c1".into(),
            name: "bash".into(),
            content: "boom".into(),
            is_error: true,
            synthetic: false,
        },
    });
    assert_eq!(out[0].text, "  error: boom");
    assert_eq!(out[0].kind, LineKind::ToolError);

    // System messages are never displayed.
    let out = lines(&SessionEntry::Message {
        message: Message::System {
            content: "AGENTS.md".into(),
        },
    });
    assert!(out.is_empty());

    // Compaction → dim summary line (same as the resume replay).
    let out = lines(&SessionEntry::Compaction {
        summary: "did things".into(),
        retained: vec![],
    });
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "compacted: did things");
    assert_eq!(out[0].kind, LineKind::Dim);

    // Notice → dim; persisted compaction banners keep their banner kind.
    let out = lines(&SessionEntry::Notice {
        text: "background done".into(),
    });
    assert_eq!(out[0].kind, LineKind::Dim);
    let out = lines(&SessionEntry::Notice {
        text: "──── auto-compaction ────".into(),
    });
    assert_eq!(out[0].kind, LineKind::Compaction);

    // BackgroundCompletion → header + truncated body, all dim.
    let out = lines(&SessionEntry::BackgroundCompletion {
        id: 7,
        output: "line1\nline2".into(),
        label: Some("demo".into()),
    });
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].text, "[background task 7 completed: demo]");
    assert!(out.iter().all(|line| line.kind == LineKind::Dim));

    // ForkedFrom → dim provenance notice.
    let out = lines(&SessionEntry::ForkedFrom {
        source: "abc".into(),
        at: 3,
        event_time: None,
        seq: None,
    });
    assert_eq!(out[0].text, "forked from abc at entry 3");
    assert_eq!(out[0].kind, LineKind::Dim);
}

#[tokio::test]
async fn jsonl_store_load_older_marks_history_done_without_lines() {
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        session_id: "s1".into(),
        root: std::path::PathBuf::from("/tmp"),
        older_pending: true,
        lines: vec![DisplayLine {
            text: "head".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        ..Default::default()
    };
    state.load_older_history().await;
    assert!(!state.older_pending);
    assert!(!state.older_loading);
    assert!(state.older_done, "JSONL already holds the full session");
    assert_eq!(state.lines.len(), 1, "nothing older to splice in");
    assert_eq!(state.older_cursor, None);
}

// ── newer-history paging (downward middle-segment load) ─────────────────

#[test]
fn home_requests_oldest_jump_and_pageup_stays_stepwise() {
    let scroll = |code| KeyEvent::new(code, KeyModifiers::empty());
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        session_id: "s1".into(),
        root: std::path::PathBuf::from("/tmp"),
        lines: vec![DisplayLine {
            text: "only line".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        window: ScrollWindow {
            source_start: 0,
            source_end: 1,
            local_offset: 0,
            follow_bottom: false,
            ..Default::default()
        },
        inner_width: 80,
        ..Default::default()
    };
    // Home queues the oldest-segment jump…
    state.handle_scroll(scroll(KeyCode::Home));
    assert!(state.older_pending);
    assert!(state.older_is_jump, "Home must flag a load_oldest jump");
    // …and repeated Home keys collapse into the same single request.
    state.handle_scroll(scroll(KeyCode::Home));
    assert!(state.older_pending);
    assert!(state.older_is_jump);

    // PageUp at the top instead queues the stepwise path.
    state.older_pending = false;
    state.older_is_jump = false;
    state.handle_scroll(scroll(KeyCode::PageUp));
    assert!(state.older_pending);
    assert!(
        !state.older_is_jump,
        "PageUp must not flag a jump (stepwise load_older)"
    );

    // Once exhausted, Home no longer queues (pure jump to the start).
    state.older_pending = false;
    state.older_done = true;
    state.handle_scroll(scroll(KeyCode::Home));
    assert!(!state.older_pending, "older_done suppresses the request");
}

#[tokio::test]
async fn home_jump_loads_oldest_and_positions_at_beginning() {
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        session_id: "s1".into(),
        root: std::path::PathBuf::from("/tmp"),
        older_pending: true,
        older_is_jump: true,
        lines: vec![DisplayLine {
            text: "head".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        window: ScrollWindow {
            source_start: 1,
            source_end: 1,
            local_offset: 3,
            follow_bottom: false,
            ..Default::default()
        },
        ..Default::default()
    };
    state.load_older_history().await;
    assert!(!state.older_pending);
    assert!(!state.older_loading);
    assert!(
        !state.older_is_jump,
        "the jump flag is one-shot and consumed by the load"
    );
    assert!(state.older_done, "the oldest segment is the end of history");
    assert_eq!(state.newer_cursor, None, "JSONL has no middle segments");
    assert_eq!(
        state.window.source_start, 0,
        "jump must position the viewport at the true beginning"
    );
    assert_eq!(state.window.local_offset, 0);
}

#[test]
fn prepend_lines_shifts_head_start_with_window() {
    let mut state = TuiState {
        lines: vec![DisplayLine {
            text: "head-1".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        window: ScrollWindow {
            source_start: 0,
            source_end: 1,
            ..Default::default()
        },
        head_start: 0,
        ..Default::default()
    };
    state.prepend_lines(vec![
        DisplayLine {
            text: "old-1".into(),
            kind: LineKind::Dim,
            collapsed_summary: None,
        },
        DisplayLine {
            text: "old-2".into(),
            kind: LineKind::Dim,
            collapsed_summary: None,
        },
    ]);
    assert_eq!(state.lines.len(), 3);
    assert_eq!(
        state.head_start, 2,
        "head_start must follow the prepended lines"
    );
    // An empty prepend is a no-op for head_start too.
    state.prepend_lines(Vec::new());
    assert_eq!(state.head_start, 2);
}

#[test]
fn splice_newer_lines_shifts_indices_and_keeps_viewport() {
    // lines = [old-1, old-2, head-1, head-2, head-3], head starts at 2.
    let mut state = TuiState {
        lines: vec![
            DisplayLine {
                text: "old-1".into(),
                kind: LineKind::Dim,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "old-2".into(),
                kind: LineKind::Dim,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "head-1".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "head-2".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "head-3".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        window: ScrollWindow {
            // Viewport inside the head segment (head-2..head-3).
            source_start: 3,
            source_end: 5,
            local_offset: 2,
            follow_bottom: false,
            frozen_source_end: 5,
            ..Default::default()
        },
        head_start: 2,
        inner_width: 80,
        ..Default::default()
    };
    state.splice_newer_lines(vec![
        DisplayLine {
            text: "mid-1".into(),
            kind: LineKind::Dim,
            collapsed_summary: None,
        },
        DisplayLine {
            text: "mid-2".into(),
            kind: LineKind::Dim,
            collapsed_summary: None,
        },
    ]);
    assert_eq!(state.lines.len(), 7);
    assert_eq!(state.lines[0].text, "old-1");
    assert_eq!(state.lines[2].text, "mid-1");
    assert_eq!(state.lines[4].text, "head-1");
    // The head segment moved with the insertion; the window indices at or
    // after the insertion point shifted by 2, so the viewport still shows
    // the same head lines (content stable). local_offset and frozen state
    // are untouched.
    assert_eq!(state.head_start, 4, "head_start follows the insertion");
    assert_eq!(state.window.source_start, 5);
    assert_eq!(state.window.source_end, 7);
    assert_eq!(state.window.local_offset, 2);
    assert_eq!(state.window.frozen_source_end, 5);

    // Boundary case: a window exactly at the insertion point (head-1 at
    // index 2) shifts to the head's new start; a window ending exactly at
    // the boundary shifts past the inserted middle segment.
    let mut boundary = TuiState {
        lines: vec![
            DisplayLine {
                text: "old-1".into(),
                kind: LineKind::Dim,
                collapsed_summary: None,
            },
            DisplayLine {
                text: "head-1".into(),
                kind: LineKind::Normal,
                collapsed_summary: None,
            },
        ],
        window: ScrollWindow {
            source_start: 1,
            source_end: 2,
            local_offset: 0,
            follow_bottom: false,
            ..Default::default()
        },
        head_start: 1,
        inner_width: 80,
        ..Default::default()
    };
    boundary.splice_newer_lines(vec![DisplayLine {
        text: "mid-1".into(),
        kind: LineKind::Dim,
        collapsed_summary: None,
    }]);
    assert_eq!(boundary.lines.len(), 3);
    assert_eq!(boundary.head_start, 2);
    assert_eq!(
        boundary.window.source_start, 2,
        "source_start at the insertion point shifts to the head start"
    );
    assert_eq!(
        boundary.window.source_end, 3,
        "source_end at the insertion point shifts past the middle segment"
    );

    // An empty splice is a no-op.
    let before = (
        state.lines.len(),
        state.head_start,
        state.window.source_start,
        state.window.source_end,
    );
    state.splice_newer_lines(Vec::new());
    assert_eq!(
        (
            state.lines.len(),
            state.head_start,
            state.window.source_start,
            state.window.source_end
        ),
        before
    );
}

#[test]
fn request_newer_is_guarded_and_stops_when_done() {
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        newer_cursor: Some(5),
        ..Default::default()
    };
    state.request_newer();
    assert!(state.newer_pending);
    // Pending: repeated requests collapse into the same single one.
    state.request_newer();
    assert!(state.newer_pending);
    // Loading (in flight): no second request while the load runs.
    state.newer_pending = false;
    state.newer_loading = true;
    state.request_newer();
    assert!(!state.newer_pending);
    state.newer_loading = false;
    // Done: the head segment was reached, nothing more to fetch.
    state.newer_done = true;
    state.request_newer();
    assert!(!state.newer_pending);
    // No seeded cursor: nothing to fetch.
    state.newer_done = false;
    state.newer_cursor = None;
    state.request_newer();
    assert!(!state.newer_pending);
    // No store: never requests.
    state.newer_cursor = Some(5);
    state.store = None;
    state.request_newer();
    assert!(!state.newer_pending);
}

#[tokio::test]
async fn jsonl_store_load_newer_marks_done_without_lines() {
    let mut state = TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        session_id: "s1".into(),
        root: std::path::PathBuf::from("/tmp"),
        newer_pending: true,
        newer_cursor: Some(5),
        lines: vec![DisplayLine {
            text: "head".into(),
            kind: LineKind::Normal,
            collapsed_summary: None,
        }],
        head_start: 0,
        ..Default::default()
    };
    state.load_newer_history().await;
    assert!(!state.newer_pending);
    assert!(!state.newer_loading);
    assert!(
        state.newer_done,
        "JSONL holds the full session: no middle segments"
    );
    assert_eq!(state.lines.len(), 1, "nothing newer to splice in");
    assert_eq!(state.head_start, 0);
    assert_eq!(state.newer_cursor, None);
}

#[tokio::test]
async fn undo_command_reverses_last_file_op_and_notices() {
    // The undo stack is process-global; serialize with the backend undo
    // tests so no other test steals this snapshot.
    let _guard = crate::tools::UNDO_TEST_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "old").unwrap();
    let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
    let tools = crate::tools::file_tools(&workspace);
    let write = tools
        .iter()
        .find(|tool| tool.spec().name == "write_file")
        .expect("file_tools includes write_file");
    write
        .execute(serde_json::json!({"path": "file.txt", "content": "new"}))
        .await
        .unwrap();

    let mut state = TuiState::default();
    super::handle_undo(&mut state);

    assert_eq!(std::fs::read_to_string(path).unwrap(), "old");
    let notice = state.lines.last().unwrap().text.clone();
    assert!(notice.contains("已撤销 write_file: file.txt"), "{notice}");
}

// ── /fork ──────────────────────────────────────────────────────────────

/// Two completed turns: turn 1 is a plain User → Assistant exchange
/// (boundary at index 1), turn 2 is a User → Assistant(tool_calls) →
/// Tool → Assistant exchange (boundary at index 5). Indexes are 0-based
/// JSONL line ordinals, so `load_with_seq` reports seq 1 and seq 5.
fn fork_session_entries() -> Vec<SessionEntry> {
    use crate::agent::{AssistantMessage, Message, ToolCall};
    let mut entries: Vec<SessionEntry> = vec![
        Message::User {
            content: "q1".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("a1".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "q2".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            reasoning: None,
        })
        .into(),
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "ok".into(),
            is_error: false,
            synthetic: false,
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("a2".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
    ];
    // A trailing non-boundary entry must be dropped from every fork.
    entries.push(SessionEntry::Notice {
        text: "[background task 1 completed]\nzzz".into(),
    });
    entries
}

/// The one `fork-…` session file in a temp root's sessions dir, if any.
fn fork_session_file(root: &std::path::Path) -> Option<std::path::PathBuf> {
    let dir = root.join(".e-agent/sessions");
    let mut forks: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("fork-") && name.ends_with(".jsonl"))
        })
        .collect();
    assert!(
        forks.len() <= 1,
        "expected at most one fork session: {forks:?}"
    );
    forks.pop()
}

/// A TuiState wired like `run_inner` does for a Jsonl-backed session,
/// with the given session id and root.
fn fork_state(root: &std::path::Path, session_id: &str) -> TuiState {
    TuiState {
        store: Some(crate::session_store::SessionStore::Jsonl),
        backend: Some(crate::config::SessionBackend::Jsonl),
        root: root.to_path_buf(),
        session_id: session_id.to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn fork_latest_creates_session_up_to_newest_boundary() {
    let temp = tempfile::tempdir().unwrap();
    crate::session::Session::rewrite(temp.path(), "src-session", &fork_session_entries()).unwrap();

    let mut state = fork_state(temp.path(), "src-session");
    super::handle_fork(ForkCommand::Latest, &mut state).await;

    let notice = state.lines.last().unwrap().text.clone();
    assert!(notice.starts_with("已 fork 到新会话：fork-"), "{notice}");
    assert!(notice.contains("保留 6 条历史"), "{notice}");
    let fork_path = fork_session_file(temp.path()).expect("fork session must exist");
    let loaded = crate::session::Session::load(
        temp.path(),
        fork_path.file_stem().unwrap().to_str().unwrap(),
    )
    .unwrap();
    // 6 source entries up to the newest boundary (a2) + ForkedFrom marker;
    // the trailing Notice is dropped.
    assert_eq!(loaded.entries.len(), 7);
    assert_eq!(
        loaded.entries[..6],
        fork_session_entries()[..6],
        "prefix must keep everything up to the newest boundary"
    );
    match loaded.entries.last().unwrap() {
        SessionEntry::ForkedFrom {
            source,
            at,
            event_time: None,
            seq: Some(seq),
        } => {
            assert_eq!(source, "src-session");
            assert_eq!(*at, 6, "marker at = prefix len");
            assert_eq!(*seq, 5, "marker seq = the boundary's JSONL ordinal");
        }
        other => panic!("expected ForkedFrom marker, got {other:?}"),
    }
}

#[tokio::test]
async fn fork_at_n_counts_boundaries_from_newest() {
    let temp = tempfile::tempdir().unwrap();
    crate::session::Session::rewrite(temp.path(), "src-session", &fork_session_entries()).unwrap();

    let mut state = fork_state(temp.path(), "src-session");
    super::handle_fork(ForkCommand::At(2), &mut state).await;

    let notice = state.lines.last().unwrap().text.clone();
    assert!(notice.starts_with("已 fork 到新会话：fork-"), "{notice}");
    assert!(notice.contains("保留 2 条历史"), "{notice}");
    let fork_path = fork_session_file(temp.path()).expect("fork session must exist");
    let loaded = crate::session::Session::load(
        temp.path(),
        fork_path.file_stem().unwrap().to_str().unwrap(),
    )
    .unwrap();
    // 2 source entries up to the first boundary (a1) + ForkedFrom marker.
    assert_eq!(loaded.entries.len(), 3);
    assert_eq!(
        loaded.entries[..2],
        fork_session_entries()[..2],
        "prefix must keep everything up to the 2nd-newest boundary"
    );
    match loaded.entries.last().unwrap() {
        SessionEntry::ForkedFrom {
            source,
            at,
            event_time: None,
            seq: Some(seq),
        } => {
            assert_eq!(source, "src-session");
            assert_eq!(*at, 2);
            assert_eq!(*seq, 1, "marker seq = the first boundary's JSONL ordinal");
        }
        other => panic!("expected ForkedFrom marker, got {other:?}"),
    }
}

#[tokio::test]
async fn fork_beyond_boundary_count_notices_and_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    crate::session::Session::rewrite(temp.path(), "src-session", &fork_session_entries()).unwrap();

    let mut state = fork_state(temp.path(), "src-session");
    super::handle_fork(ForkCommand::At(99), &mut state).await;

    let notice = state.lines.last().unwrap().text.clone();
    assert_eq!(notice, "无法 fork：只有 2 个可 fork 的回合边界", "{notice}");
    assert!(
        fork_session_file(temp.path()).is_none(),
        "out-of-range fork must not create a session"
    );
}

#[tokio::test]
async fn fork_usage_and_unwired_state_notice_without_creating() {
    let temp = tempfile::tempdir().unwrap();

    // Usage: no session file, usage notice.
    let mut state = fork_state(temp.path(), "src-session");
    super::handle_fork(ForkCommand::Usage, &mut state).await;
    assert!(
        state.lines.last().unwrap().text.contains("用法：/fork"),
        "{}",
        state.lines.last().unwrap().text
    );
    assert!(fork_session_file(temp.path()).is_none());

    // Unwired (Default) state: "TUI 未接线" notice, no panic, no session.
    let mut state = TuiState::default();
    super::handle_fork(ForkCommand::Latest, &mut state).await;
    assert!(
        state
            .lines
            .last()
            .unwrap()
            .text
            .contains("无法 fork：TUI 未接线"),
        "{}",
        state.lines.last().unwrap().text
    );
    assert!(fork_session_file(temp.path()).is_none());
}

#[test]
fn swapped_keys_invert_main_view_enter_and_submit() {
    // [tui] submit = "alt+enter", newline = "enter": bare Enter inserts a
    // newline, Alt+Enter submits.
    let mut state = TuiState {
        keys: InputKeys {
            submit_modifiers: KeyModifiers::ALT,
            newline_modifiers: KeyModifiers::NONE,
        },
        ..Default::default()
    };
    state.input.insert("first");
    let submitted = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert!(
        submitted.is_none(),
        "Enter must not submit under swapped keys"
    );
    assert_eq!(state.input.text, "first\n");
    let submitted = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(submitted, Some("first\n".to_owned()));
}

#[test]
fn swapped_keys_apply_to_attached_view() {
    // The attached view inherits the main state's [tui] mapping: bare
    // Enter inserts a newline, Alt+Enter steers.
    let (handle, _sink, mut source) = crate::runner::session_test_channel();
    let mut state = TuiState {
        keys: InputKeys {
            submit_modifiers: KeyModifiers::ALT,
            newline_modifiers: KeyModifiers::NONE,
        },
        ..Default::default()
    };
    attach_test(&mut state, 7, "demo task", handle);
    {
        let attached = state.attached.as_mut().unwrap();
        attached.input.insert("please also check");
    }
    state.handle_attached_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()), 80);
    assert!(
        source.try_recv().ok().is_none(),
        "Enter must not steer under swapped keys"
    );
    assert_eq!(
        state.attached.as_ref().unwrap().input.text,
        "please also check\n"
    );
    state.handle_attached_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), 80);
    assert!(matches!(
        source.try_recv().ok(),
        Some(crate::runner::SessionCommand::Prompt(ref text)) if text == "please also check\n"
    ));
}
