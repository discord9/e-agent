use super::*;
use crate::agent::preview;
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
