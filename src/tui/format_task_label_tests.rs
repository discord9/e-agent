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
        subagent_session_id: None,
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
        subagent_session_id: None,
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
        subagent_session_id: None,
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
