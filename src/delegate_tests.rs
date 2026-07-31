use super::*;
use crate::agent::{AssistantMessage, Message, Model, ModelDeltaKind, Usage};
use crate::tools::builtins;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[test]
fn task_label_falls_back_label_then_role_then_task() {
    assert_eq!(
        task_label(Some("  fix the typo  "), Some("fixer"), "a long task"),
        "fix the typo",
        "caller label wins, trimmed"
    );
    assert_eq!(
        task_label(Some("   "), Some("fixer"), "a long task"),
        "fixer",
        "blank label falls through to the role"
    );
    assert_eq!(
        task_label(None, Some("fixer"), "a long task"),
        "fixer",
        "no label falls back to the role"
    );
    assert_eq!(
        task_label(None, None, "a long task"),
        "a long task",
        "no label and no role previews the task"
    );
    assert_eq!(
        task_label(Some("first line\nsecond\tline"), None, "task"),
        "first line second line",
        "labels are normalized onto one started-task line"
    );
    assert_eq!(
        task_label(None, None, "first line\nsecond line"),
        "first line second line",
        "task-preview labels are normalized too"
    );
    let long = "x".repeat(200);
    let capped = task_label(Some(&long), None, "task");
    assert!(
        capped.chars().count() <= 61 && capped.contains('\u{2026}'),
        "caller label is capped with a middle ellipsis, got: {capped:?}"
    );
}

#[test]
fn sync_cancelled_is_an_error_with_session_id() {
    let error = sync_result("sub-cancelled", SessionResult::Cancelled).unwrap_err();
    assert_eq!(
        error, "subagent session: sub-cancelled\nsubagent cancelled",
        "cancelled must never be formatted as a successful sync answer"
    );
}

#[test]
fn registry_tracks_live_sessions() {
    let sessions = Sessions::default();
    assert!(sessions.get(1).is_none());
    let workspace = tempfile::tempdir().unwrap();
    let model = ConfiguredModel::chat(
        crate::model::OpenAiModel::new(
            "http://localhost".into(),
            "test-key".into(),
            "test-model".into(),
            None,
        )
        .unwrap(),
    );
    let agent = Agent::new(Box::new(model), vec![]);
    let (_runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        workspace.path().to_path_buf(),
        "sub-test".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let entry = Arc::new(SessionEntry {
        handle,
        model: "test-model".into(),
        role: None,
        cwd: "/tmp".into(),
        session_id: "sub-test".into(),
        context_window: None,
    });
    sessions.insert(1, entry);
    assert!(sessions.get(1).is_some());
    sessions.remove(1);
    assert!(sessions.get(1).is_none());
}

fn delegate_with_url(workspace: &std::path::Path, base_url: String) -> Delegate {
    let workspace = Workspace::new(workspace).unwrap();
    // Construct the model with a dummy key directly: tests must not
    // depend on (or mutate) process env.
    let model = ConfiguredModel::chat(
        crate::model::OpenAiModel::new(base_url, "test-key".into(), "test-model".into(), None)
            .unwrap(),
    );
    let (_, background) = builtins(workspace.clone(), None);
    Delegate::new(model, workspace, background)
}

fn delegate(workspace: &std::path::Path) -> Delegate {
    delegate_with_url(workspace, "http://localhost".into())
}

struct ProbeModel {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    future_dropped: Arc<Notify>,
    model_dropped: Arc<Notify>,
    side_effects: Arc<AtomicUsize>,
    panic: bool,
}

struct FutureDropProbe(Arc<Notify>);

impl Drop for FutureDropProbe {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl Drop for ProbeModel {
    fn drop(&mut self) {
        self.model_dropped.notify_one();
    }
}

#[async_trait]
impl Model for ProbeModel {
    async fn complete(
        &mut self,
        _: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let _probe = FutureDropProbe(self.future_dropped.clone());
        self.entered.notify_one();
        assert!(!self.panic, "controlled model panic");
        self.release.notified().await;
        self.side_effects.fetch_add(1, Ordering::SeqCst);
        Ok((
            AssistantMessage {
                content: Some("finished".into()),
                tool_calls: vec![],
                reasoning: None,
            },
            None,
        ))
    }
}

struct ProbeSignals {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    future_dropped: Arc<Notify>,
    model_dropped: Arc<Notify>,
    side_effects: Arc<AtomicUsize>,
}

fn probe_runner(
    root: &std::path::Path,
    panic: bool,
) -> (SessionHandle, crate::runner::SessionTask, ProbeSignals) {
    let signals = ProbeSignals {
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        future_dropped: Arc::new(Notify::new()),
        model_dropped: Arc::new(Notify::new()),
        side_effects: Arc::new(AtomicUsize::new(0)),
    };
    let agent = Agent::new(
        Box::new(ProbeModel {
            entered: signals.entered.clone(),
            release: signals.release.clone(),
            future_dropped: signals.future_dropped.clone(),
            model_dropped: signals.model_dropped.clone(),
            side_effects: signals.side_effects.clone(),
            panic,
        }),
        vec![],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        root.to_path_buf(),
        "probe".into(),
        IdlePolicy::FinishWhenIdle,
    );
    (handle, runner.start(Some("start".into())), signals)
}

fn probe_entry(handle: SessionHandle) -> Arc<SessionEntry> {
    Arc::new(SessionEntry {
        handle,
        model: "probe".into(),
        role: None,
        cwd: "/tmp".into(),
        session_id: "sub-probe".into(),
        context_window: None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_cancel_during_on_id_cleans_registration_without_completion() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let workspace = Workspace::new(&root).unwrap();
    let (_, mut background) = builtins(workspace, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(&root, false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(
        slot.clone(),
        sessions.clone(),
        Some((root.clone(), "parent".into())),
    );
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let work_runs = Arc::new(AtomicUsize::new(0));
    let spawn_background = background.clone();
    let hook_slot = slot.clone();
    let hook_sessions = sessions.clone();
    let hook_handle = handle.clone();
    let hook_root = root.clone();
    let hook_entered = entered.clone();
    let hook_release = release.clone();
    let spawn_work_runs = work_runs.clone();

    let spawn = tokio::task::spawn_blocking(move || {
        spawn_background.spawn_with_id(
            "probe".into(),
            None,
            None,
            None,
            move |id| {
                *hook_slot.lock().unwrap() = Some(id);
                hook_sessions.insert(id, probe_entry(hook_handle));
                crate::session::Session::record_background_start(
                    &hook_root,
                    "parent",
                    id,
                    "probe",
                    Some("sub-probe"),
                )
                .unwrap();
                hook_entered.wait();
                hook_release.wait();
            },
            move || {
                spawn_work_runs.fetch_add(1, Ordering::SeqCst);
                let cleanup = cleanup;
                async move {
                    let result = Delegate::runner_result(&handle, runner_task).await;
                    cleanup.finish();
                    result_output(result).1
                }
            },
        )
    });

    entered.wait();
    assert!(sessions.get(1).is_some());
    assert_eq!(background.cancel(1).as_deref(), Some("probe"));
    release.wait();
    assert!(spawn.await.unwrap().is_ok(), "spawn must not panic or fail");
    signals.model_dropped.notified().await;
    signals.release.notify_one();
    tokio::task::yield_now().await;
    assert!(background.running().is_empty());
    assert!(sessions.sessions.lock().unwrap().is_empty());
    assert!(crate::session::Session::take_unfinished_background(&root, "parent").is_empty());
    assert_eq!(work_runs.load(Ordering::SeqCst), 0);
    assert_eq!(signals.side_effects.load(Ordering::SeqCst), 0);
    assert!(matches!(
        completions.try_recv(),
        Ok(AgentEvent::BackgroundCompleted { output, .. }) if output == "background task cancelled"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn background_cancel_before_first_yield_cleans_everything() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut background) = builtins(workspace, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(temp.path(), false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(
        slot.clone(),
        sessions.clone(),
        Some((temp.path().to_path_buf(), "parent".into())),
    );
    let hook_slot = slot.clone();
    let hook_sessions = sessions.clone();
    let hook_handle = handle.clone();
    let hook_root = temp.path().to_path_buf();
    background
        .spawn_with_id(
            "probe".into(),
            None,
            None,
            None,
            move |id| {
                *hook_slot.lock().unwrap() = Some(id);
                hook_sessions.insert(id, probe_entry(hook_handle.clone()));
                crate::session::Session::record_background_start(
                    &hook_root,
                    "parent",
                    id,
                    "probe",
                    Some("sub-probe"),
                )
                .unwrap();
            },
            move || {
                let cleanup = cleanup;
                async move {
                    let result = Delegate::runner_result(&handle, runner_task).await;
                    cleanup.finish();
                    result_output(result).1
                }
            },
        )
        .unwrap();

    assert_eq!(background.cancel(1).as_deref(), Some("probe"));
    signals.model_dropped.notified().await;
    signals.release.notify_one();
    tokio::task::yield_now().await;
    assert!(background.running().is_empty());
    assert!(sessions.sessions.lock().unwrap().is_empty());
    assert!(crate::session::Session::take_unfinished_background(temp.path(), "parent").is_empty());
    assert_eq!(signals.side_effects.load(Ordering::SeqCst), 0);
    assert!(matches!(
        completions.try_recv(),
        Ok(AgentEvent::BackgroundCompleted { output, .. }) if output == "background task cancelled"
    ));
}

#[tokio::test]
async fn background_cancel_while_joining_aborts_inner_without_completion() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut background) = builtins(workspace, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(temp.path(), false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(
        slot.clone(),
        sessions.clone(),
        Some((temp.path().to_path_buf(), "parent".into())),
    );
    let hook_sessions = sessions.clone();
    let hook_handle = handle.clone();
    let hook_root = temp.path().to_path_buf();
    background
        .spawn_with_id(
            "probe".into(),
            None,
            None,
            None,
            move |id| {
                *slot.lock().unwrap() = Some(id);
                hook_sessions.insert(id, probe_entry(hook_handle.clone()));
                crate::session::Session::record_background_start(
                    &hook_root,
                    "parent",
                    id,
                    "probe",
                    Some("sub-probe"),
                )
                .unwrap();
            },
            move || {
                let cleanup = cleanup;
                async move {
                    let result = Delegate::runner_result(&handle, runner_task).await;
                    cleanup.finish();
                    result_output(result).1
                }
            },
        )
        .unwrap();

    signals.entered.notified().await;
    assert_eq!(background.cancel(1).as_deref(), Some("probe"));
    signals.future_dropped.notified().await;
    signals.model_dropped.notified().await;
    signals.release.notify_one();
    tokio::task::yield_now().await;
    assert!(background.running().is_empty());
    assert!(sessions.sessions.lock().unwrap().is_empty());
    assert!(crate::session::Session::take_unfinished_background(temp.path(), "parent").is_empty());
    assert_eq!(signals.side_effects.load(Ordering::SeqCst), 0);
    assert!(matches!(
        completions.try_recv(),
        Ok(AgentEvent::BackgroundCompleted { output, .. }) if output == "background task cancelled"
    ));
}

#[tokio::test]
async fn sync_cancel_cleans_session_and_closes_result_channel() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut background) = builtins(workspace, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(temp.path(), false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(slot.clone(), sessions.clone(), None);
    let hook_sessions = sessions.clone();
    let hook_handle = handle.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    background
        .spawn_silent(
            "probe".into(),
            None,
            None,
            None,
            move |id| {
                *slot.lock().unwrap() = Some(id);
                hook_sessions.insert(id, probe_entry(hook_handle.clone()));
            },
            move || {
                let cleanup = cleanup;
                async move {
                    let result = Delegate::runner_result(&handle, runner_task).await;
                    cleanup.finish();
                    let _ = done_tx.send(result.clone());
                    result_output(result).1
                }
            },
        )
        .unwrap();

    signals.entered.notified().await;
    assert_eq!(background.cancel(1).as_deref(), Some("probe"));
    assert_eq!(
        done_rx.await.unwrap_err().to_string(),
        "channel closed",
        "sync delegate reports its existing channel-closed error"
    );
    signals.future_dropped.notified().await;
    signals.model_dropped.notified().await;
    assert!(background.running().is_empty());
    assert!(sessions.sessions.lock().unwrap().is_empty());
    assert!(completions.try_recv().is_err());
}

#[tokio::test]
async fn panicking_inner_model_cleans_up_and_sends_one_failure_completion() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut background) = builtins(workspace, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(temp.path(), true);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(slot.clone(), sessions.clone(), None);
    let hook_sessions = sessions.clone();
    let hook_handle = handle.clone();
    background
        .spawn_with_id(
            "probe".into(),
            None,
            None,
            None,
            move |id| {
                *slot.lock().unwrap() = Some(id);
                hook_sessions.insert(id, probe_entry(hook_handle.clone()));
            },
            move || {
                let cleanup = cleanup;
                async move {
                    let result = Delegate::runner_result(&handle, runner_task).await;
                    cleanup.finish();
                    result_output(result).1
                }
            },
        )
        .unwrap();

    let event = completions.recv().await.unwrap();
    assert!(matches!(
        event,
        AgentEvent::BackgroundCompleted { output, .. }
            if output.starts_with("subagent failed:") && output.contains("controlled model panic")
    ));
    signals.model_dropped.notified().await;
    assert!(background.running().is_empty());
    assert!(sessions.sessions.lock().unwrap().is_empty());
    assert!(
        completions.try_recv().is_err(),
        "exactly one completion is sent"
    );
}

async fn successful_model(answer: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let answer = serde_json::to_string(answer).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            request.extend_from_slice(&chunk[..count]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let received_body = request.len() - header_end;
        let mut rest = vec![0; content_length - received_body];
        stream.read_exact(&mut rest).await.unwrap();

        let body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{answer}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn rejects_empty_task() {
    let temp = tempfile::tempdir().unwrap();
    let delegate = delegate(temp.path());
    assert!(
        delegate
            .execute(json!({"task": "  "}))
            .await
            .unwrap_err()
            .contains("must not be empty")
    );
    assert!(
        delegate
            .execute(json!({}))
            .await
            .unwrap_err()
            .contains("must be a string")
    );
}

#[test]
fn spec_defaults_background_true_without_requiring_it() {
    let temp = tempfile::tempdir().unwrap();
    let spec = delegate(temp.path()).spec();
    let background = &spec.parameters["properties"]["background"];
    assert_eq!(background["type"], "boolean");
    assert_eq!(background["default"], true);
    assert_eq!(spec.parameters["required"], json!(["task"]));
    assert!(
        spec.description
            .contains("By default it runs in the background")
    );
    assert!(spec.description.contains("`background: false`"));
}

#[tokio::test]
async fn rejects_present_non_boolean_background_values() {
    let temp = tempfile::tempdir().unwrap();
    let tool = delegate(temp.path());
    for value in [Value::Null, json!("true"), json!(1), json!({}), json!([])] {
        let error = tool
            .execute(json!({"task": "hello", "background": value}))
            .await
            .unwrap_err();
        assert_eq!(error, "`background` must be a boolean");
    }
}

#[tokio::test]
async fn background_delivery_preflight_fails_before_later_work() {
    // The missing sender must win over resume loading and workspace
    // construction, and must not allocate or persist anything.
    let temp = tempfile::tempdir().unwrap();
    let persist_root = temp.path().join("subagent-sessions");
    let record_root = temp.path().join("parent-session");
    let delegate = delegate(temp.path())
        .persist_sessions(persist_root.clone())
        .record_background_tasks_in(record_root.clone(), "parent");

    let error = delegate
        .execute(json!({
            "task": "hi",
            "resume": "sub-does-not-exist",
            "workspace": "/nonexistent-path-that-surely-does-not-exist-12345"
        }))
        .await
        .unwrap_err();

    assert_eq!(error, "background task delivery is unavailable");
    assert!(delegate.background.running().is_empty());
    assert!(delegate.sessions.sessions.lock().unwrap().is_empty());
    assert!(!persist_root.exists(), "resume store must not connect");
    assert!(
        !record_root.exists(),
        "background record must not be written"
    );
}

#[tokio::test]
async fn resume_requires_persistence_and_an_existing_session() {
    // No persistence configured: nothing to resume from.
    let temp = tempfile::tempdir().unwrap();
    let no_persist = delegate(temp.path());
    assert!(
        no_persist
            .execute(json!({"task": "hi", "resume": "sub-x", "background": false}))
            .await
            .unwrap_err()
            .contains("requires subagent session persistence")
    );

    // Persistence configured but the session id does not exist.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let tool: Delegate = delegate(temp.path()).persist_sessions(root);
    assert!(
        tool.execute(json!({"task": "hi", "resume": "sub-does-not-exist", "background": false}))
            .await
            .unwrap_err()
            .contains("no such subagent session")
    );
}

#[test]
fn resume_loads_the_previous_transcript_as_starting_context() {
    // The core resume invariant: a persisted sub- session can be loaded
    // back, and its length marks where new-turn appends begin (so the
    // loaded history is NOT re-persisted).
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let prior = vec![
        crate::agent::SessionEntry::from(crate::agent::Message::User {
            content: "earlier task".into(),
        }),
        crate::agent::SessionEntry::from(crate::agent::Message::Assistant(
            crate::agent::AssistantMessage {
                content: Some("earlier answer".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        )),
    ];
    crate::session::Session::append(&root, "sub-prior", &prior).unwrap();

    let loaded = crate::session::Session::load(&root, "sub-prior").unwrap();
    assert_eq!(loaded.entries.len(), prior.len());
    // persisted_len starts at the loaded length, so the next append only
    // writes entries from index `loaded.len()` onward.
    let new_entries = &prior[loaded.entries.len()..];
    assert!(new_entries.is_empty(), "loaded history is not re-persisted");
}

#[tokio::test]
async fn spec_disallows_nested_delegation_by_design() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (tools, _) = builtins(workspace, None);
    let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
    assert!(!names.contains(&"delegate".to_owned()));
    assert!(names.contains(&"bash".to_owned()));
}

#[tokio::test]
// Test-only env isolation: the std Mutex guard is held across .execute()
// awaits to serialize XDG_CONFIG_HOME mutation with roles.rs tests. The
// critical sections are short and never contend in practice.
#[allow(clippy::await_holding_lock)]
async fn role_requires_a_roles_root_and_a_known_role() {
    let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    // Isolate from the developer's real global agents directory.
    let xdg = temp.path().join("xdg-empty");
    std::fs::create_dir_all(&xdg).unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    // No roles root: any role is rejected.
    let plain = delegate(temp.path());
    assert!(
        plain
            .execute(json!({"task": "hi", "role": "fixer", "background": false}))
            .await
            .unwrap_err()
            .contains("roles are not configured")
    );

    // Roles root set, but the requested role has no template file.
    let rooted = delegate(temp.path()).with_roles_root(temp.path().to_path_buf());
    let error = rooted
        .execute(json!({"task": "hi", "role": "fixer", "background": false}))
        .await
        .unwrap_err();
    assert!(error.contains("unknown role `fixer`"), "{error}");

    // A template on disk (workspace `agents/`) makes the role valid.
    let directory = temp.path().join("agents");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("fixer.md"), "You fix things.").unwrap();
    let spec = rooted.spec();
    let roles = spec.parameters["properties"]["role"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(roles, &vec![json!("fixer")]);

    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[tokio::test]
async fn shared_background_registry_completion_reaches_its_configured_channel() {
    // A bash tool bound to the parent's BackgroundTasks keeps that
    // sender when wrapped in an Agent (Agent::new must not retarget
    // it): a background command's completion arrives on the parent's
    // channel even after the subagent is dropped. End-to-end subagent
    // behaviour is covered by agent.rs's shared-sender test; here we
    // pin the runner wiring.
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut parent_background) = builtins(workspace.clone(), None);
    let (parent_sender, mut parent_receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    parent_background.set_event_sender(parent_sender);

    let started = parent_background
        .start(workspace, "echo shared".into(), false)
        .unwrap();
    assert!(started.starts_with("started background task"));

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), parent_receiver.recv())
        .await
        .expect("parent channel got the completion")
        .unwrap();
    assert!(matches!(
        event,
        AgentEvent::BackgroundCompleted { output, .. } if output.contains("shared")
    ));
}

#[tokio::test]
async fn delegate_uses_custom_workspace() {
    let parent = tempfile::tempdir().unwrap();
    // Create a separate tempdir as the custom workspace.
    let custom = tempfile::tempdir().unwrap();
    // Put a marker file in each workspace.
    std::fs::write(custom.path().join("sentinel.txt"), "custom").unwrap();
    std::fs::write(parent.path().join("sentinel.txt"), "parent").unwrap();

    // 1) An invalid (non-existent) workspace path is rejected at
    //    parameter-validation time, before any subagent is spawned.
    let tool = delegate(parent.path());
    let err = tool
        .execute(json!({
            "task": "irrelevant",
            "workspace": "/nonexistent-path-that-surely-does-not-exist-12345",
            "background": false
        }))
        .await
        .unwrap_err();
    assert!(
        err.contains("invalid `workspace`"),
        "expected invalid-workspace error, got: {err}"
    );

    // 2) A valid custom workspace is accepted; the subagent tries to
    //    contact the dummy model (localhost) and fails with a connection
    //    error — but crucially the workspace error is NOT raised.
    let tool = delegate(parent.path());
    let answer = tool
        .execute(json!({
            "task": "read sentinel.txt and report its content",
            "workspace": custom.path().to_str().unwrap(),
            "background": false
        }))
        .await
        .unwrap_err();
    assert!(
        answer.contains("\nsubagent failed:"),
        "expected model-connection failure, got: {answer}"
    );
    assert!(
        answer.starts_with("subagent session: sub-"),
        "sync failure must identify its subagent session, got: {answer}"
    );
    assert!(
        !answer.contains("invalid `workspace`"),
        "valid workspace path should not produce a workspace error, got: {answer}"
    );
}

#[tokio::test]
async fn sync_success_contains_session_id_and_answer() {
    let temp = tempfile::tempdir().unwrap();
    let base_url = successful_model("finished answer").await;
    let tool = delegate_with_url(temp.path(), base_url);

    let output = tool
        .execute(json!({"task": "hello", "background": false}))
        .await
        .unwrap();
    let mut lines = output.lines();
    let session_id = lines
        .next()
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("sync success contains the subagent session id");
    assert!(session_id.starts_with("sub-"));
    assert_eq!(lines.collect::<Vec<_>>(), ["finished answer"]);
}

#[tokio::test]
async fn omitted_background_defaults_to_one_completion_with_session_id() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let base_url = successful_model("finished answer").await;
    let mut tool = delegate_with_url(temp.path(), base_url).persist_sessions(root);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    tool.set_event_sender(sender);

    let answer = tool
        .execute(json!({
            "task": "hello",
            "label": "first line\nsecond line"
        }))
        .await
        .unwrap();
    let mut lines = answer.lines();
    assert_eq!(
        lines.next(),
        Some("started background task 1: first line second line")
    );
    let immediate_session = lines
        .next()
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("immediate result contains the subagent session id");
    assert!(immediate_session.starts_with("sub-"));
    assert_eq!(lines.next(), None);

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("timed out waiting for background completion")
        .unwrap();
    assert!(tool.background.running().is_empty());
    assert!(tool.sessions.sessions.lock().unwrap().is_empty());
    assert!(
        receiver.try_recv().is_err(),
        "exactly one completion is sent"
    );
    match event {
        AgentEvent::BackgroundCompleted { output, .. } => {
            assert_eq!(
                output,
                format!("subagent session: {immediate_session}\nfinished answer"),
                "successful completion must retain the immediate session id and answer"
            );
        }
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }
}

#[tokio::test]
async fn background_failure_completion_retains_session_id() {
    let temp = tempfile::tempdir().unwrap();
    let mut tool = delegate(temp.path());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    tool.set_event_sender(sender);

    let answer = tool
        .execute(json!({"task": "hello", "background": true}))
        .await
        .unwrap();
    let immediate_session = answer
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("immediate result contains the subagent session id");

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("timed out waiting for background failure")
        .unwrap();
    match event {
        AgentEvent::BackgroundCompleted { output, .. } => assert!(
            output.starts_with(&format!(
                "subagent session: {immediate_session}\nsubagent failed:"
            )),
            "failed completion must retain the main-branch session format, got: {output}"
        ),
        other => panic!("expected BackgroundCompleted, got {other:?}"),
    }
}

#[tokio::test]
async fn resume_replays_scrollback_into_session_sink() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");

    // Create a persisted prior session with two entries.
    let prior = vec![
        crate::agent::SessionEntry::from(crate::agent::Message::User {
            content: "earlier task".into(),
        }),
        crate::agent::SessionEntry::from(crate::agent::Message::Assistant(
            crate::agent::AssistantMessage {
                content: Some("earlier answer".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        )),
    ];
    crate::session::Session::append(&root, "sub-resume-scrollback", &prior).unwrap();

    let mut tool = delegate(temp.path()).persist_sessions(root);
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    tool.set_event_sender(sender);

    // Omitting background defaults to a background delegate while resuming.
    let answer = tool
        .execute(json!({"task": "new prompt", "resume": "sub-resume-scrollback"}))
        .await
        .unwrap();
    assert!(answer.starts_with("started background task"));

    let id: u64 = answer
        .strip_prefix("started background task ")
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.trim().parse().ok())
        .expect("could not extract task id");

    // Give the subagent task a moment to emit the scrollback events.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let entry = tool.sessions().get(id).expect("session entry missing");
    let snapshot = entry.handle.snapshot();

    // The snapshot should contain the prior UserPrompt before the new one.
    let user_texts: Vec<&str> = snapshot
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserPrompt(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        user_texts.len() >= 2,
        "expected at least two UserPrompt events (prior + new), got {user_texts:?}"
    );
    assert_eq!(user_texts[0], "earlier task");
    // The new prompt may or may not be last (replayed prior events
    // come first); just check it appears.
    assert!(
        user_texts.contains(&"new prompt"),
        "expected 'new prompt' in UserPrompt events, got {user_texts:?}"
    );

    // The prior AssistantText must be in the log too.
    let assistant_texts: Vec<&str> = snapshot
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        assistant_texts.contains(&"earlier answer"),
        "expected 'earlier answer' in AssistantText events, got {assistant_texts:?}"
    );
}
