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
        capped.chars().count() <= 41 && capped.contains('\u{2026}'),
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
        store: SessionStore::Jsonl,
    });
    sessions.insert(1, entry.clone());
    assert!(sessions.get(1).is_some());
    sessions.remove(1);
    assert!(sessions.get(1).is_none());
    // `list` snapshots the entries (task_id, entry) pairs, including after
    // re-insert: the web server's subagent-by-session-id lookup iterates it.
    sessions.insert(2, entry);
    let listed = sessions.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, 2);
    assert_eq!(listed[0].1.session_id, "sub-test");
}

fn delegate_with_url(workspace: &std::path::Path, base_url: String) -> Delegate {
    let workspace = Workspace::new(workspace).unwrap();
    // Construct the model with a dummy key directly: tests must not
    // depend on (or mutate) process env.
    let model = ConfiguredModel::chat(
        crate::model::OpenAiModel::new(base_url, "test-key".into(), "test-model".into(), None)
            .unwrap(),
    );
    let (_, background) = builtins(workspace.clone(), None, false, None);
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
        store: SessionStore::Jsonl,
    })
}

/// A JSONL-backed record for the tests below, mirroring how the delegate
/// tool wires `record_background_tasks_in` in production.
fn jsonl_record(root: std::path::PathBuf) -> crate::session_store::BackgroundRecord {
    crate::session_store::BackgroundRecord {
        root,
        session: "parent".into(),
        store: crate::session_store::SessionStore::Jsonl,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_cancel_during_on_id_cleans_registration_without_completion() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let workspace = Workspace::new(&root).unwrap();
    let (_, mut background) = builtins(workspace, None, false, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(&root, false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(
        slot.clone(),
        sessions.clone(),
        Some(jsonl_record(root.clone())),
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
    let (_, mut background) = builtins(workspace, None, false, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(temp.path(), false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(
        slot.clone(),
        sessions.clone(),
        Some(jsonl_record(temp.path().to_path_buf())),
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
    let (_, mut background) = builtins(workspace, None, false, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let (handle, runner_task, signals) = probe_runner(temp.path(), false);
    let sessions = Sessions::default();
    let slot = Arc::new(Mutex::new(None));
    let cleanup = DelegateCleanup::new(
        slot.clone(),
        sessions.clone(),
        Some(jsonl_record(temp.path().to_path_buf())),
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
    let (_, mut background) = builtins(workspace, None, false, None);
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
async fn sync_failure_cleans_session_and_registry() {
    // 同步 delegate（spawn_silent）的 runner 失败（模型拒绝/panic）也必须
    // 清理：任务面板条目（sessions + registry）不得残留到进程重启。
    // 历史上 sync-failure 分支出现过 ghost 条目（.e-agent/TODO.md
    // "Sync-delegate failure cleanup leak"），本测试钉死该路径。
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut background) = builtins(workspace, None, false, None);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    // panicking model → runner task join 返回 Err → SessionResult::Failed
    let (handle, runner_task, signals) = probe_runner(temp.path(), true);
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
    let result = done_rx.await.expect("sync delegate must deliver a result");
    assert!(
        matches!(result, SessionResult::Failed(_)),
        "panicking model must fail the runner, got: {result:?}"
    );
    // 失败后：任务面板条目（registry + sessions）必须全部清理
    let mut tries = 0;
    loop {
        if background.running().is_empty() && sessions.sessions.lock().unwrap().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
        tries += 1;
        assert!(tries < 1000, "cleanup never completed");
    }
    assert!(
        completions.try_recv().is_err(),
        "sync delegate sends no completion event"
    );
}

#[tokio::test]
async fn sync_abandon_aborts_runner_and_cleans_session() {
    // 主 agent 取消（POST /api/sessions/{id}/cancel）放弃正在同步等待的
    // delegate 时（drop 主侧 done_rx），生产 sync wrapper 必须通过
    // done_tx.closed() 感知并 abort 子 runner，且立即清理 registry/sessions。
    // 历史 bug：主侧放弃后子 subagent 成为孤儿继续运行（跑 bash、改文件、
    // 写 session）直到自然完成，任务面板还显示运行中（.e-agent/TODO.md
    // "Sync delegate cancel leaks the subagent"），本测试钉死该路径。
    //
    // 与 sync_cancel/sync_failure 直接构造 spawn_silent 不同，本测试走真实
    // 的 Delegate::execute(background:false)：Delegate 的子模型指向一个
    // 读完整请求后永不响应的阻塞 stub。stub 收到请求 = 子 runner 已进入
    // 模型调用（且 execute 已走到 done_rx.await、wrapper 已 spawn），以此
    // 作为"已开始"信号；然后 abort 持有 execute future 的 task（等价于主
    // runner 取消 turn 时 drop execute future → 主侧 done_rx 被 drop），
    // 断言生产 wrapper 通过 done_tx.closed() 感知放弃并 abort 子 runner
    // （stub 观察到 in-flight 连接被掐断），且 registry/sessions 清理干净。
    let temp = tempfile::tempdir().unwrap();
    let (base_url, stub) = blocking_model().await;
    let mut tool = delegate_with_url(temp.path(), base_url);
    let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
    tool.set_event_sender(sender);
    // 断言阶段需要的 registry/sessions 观测句柄（tool 本体随后移入 task）。
    let background = tool.background.clone();
    let sessions = tool.sessions();

    // 真实 execute（background:false）：spawn 后主侧持有的是 execute
    // future（其内部 await done_rx，持有主侧 done_rx 的一端）。
    let execute = tokio::spawn(async move {
        tool.execute(json!({
            "task": "block forever",
            "workspace": temp.path().to_str().unwrap(),
            "background": false
        }))
        .await
    });

    // 已开始信号：子 runner 的模型请求已被 stub 完整读取（runner 卡在
    // 永不响应的 SSE 读取上）；再确认 sync wrapper 已注册进 background
    // registry，保证 execute 一定已走到 done_rx.await，之后才模拟放弃。
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stub.request_received.notified(),
    )
    .await
    .expect("subagent runner must reach its model call");
    let mut tries = 0;
    while background.running().is_empty() {
        tokio::task::yield_now().await;
        tries += 1;
        assert!(tries < 1000, "sync wrapper was never spawned");
    }
    assert_eq!(
        sessions.sessions.lock().unwrap().len(),
        1,
        "sync delegate registers its subagent in the task panel"
    );

    // 模拟主侧放弃：abort 持有 execute future 的 task（主 runner 取消时
    // execute future 被 drop，其持有的 done_rx 一并被 drop → 生产 wrapper
    // 的 done_tx.closed() 分支触发）。
    execute.abort();

    // 子 runner 必须被 abort：其 in-flight 模型请求的连接被掐断（stub 的
    // 后续 read 返回 EOF/错误），而不是自然完成（stub 从不响应）。
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stub.connection_closed.notified(),
    )
    .await
    .expect("abandoned sync delegate must abort the subagent runner mid-call");
    // wrapper cleanup 已执行：任务面板条目（background registry + sessions）
    // 清空（sync 路径的 recovery record 为 None，清理对象就是 registry）。
    let mut tries = 0;
    loop {
        if background.running().is_empty() && sessions.sessions.lock().unwrap().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
        tries += 1;
        assert!(tries < 1000, "cleanup never completed");
    }
    // execute future 确实是被主侧放弃中止的（abort 的 JoinHandle 报
    // cancelled），而不是自然返回了结果。
    let joined = execute.await;
    assert!(
        matches!(&joined, Err(error) if error.is_cancelled()),
        "execute future must be cancelled by the main-side abandon, got: {joined:?}"
    );
    assert!(
        completions.try_recv().is_err(),
        "sync delegate sends no completion event"
    );
}

#[tokio::test]
async fn panicking_inner_model_cleans_up_and_sends_one_failure_completion() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(temp.path()).unwrap();
    let (_, mut background) = builtins(workspace, None, false, None);
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
    successful_model_n(answer, 1).await
}

/// Like `successful_model`, but answers `requests` sequential model calls
/// (one per subagent runner) on the same listener — a single delegate
/// execute consumes one answer, so a resume test needs two.
async fn successful_model_n(answer: &str, requests: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let answer = serde_json::to_string(answer).unwrap();
    tokio::spawn(async move {
        for _ in 0..requests {
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
        }
    });
    format!("http://{address}")
}

/// Like `successful_model`, but also captures the full HTTP request body so
/// tests can assert on the wire `tools` array and the system prompt.
async fn capturing_model() -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();
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
        request.extend_from_slice(&rest);
        *captured_clone.lock().unwrap() = request;

        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
            .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    (format!("http://{address}"), captured)
}

struct BlockingStub {
    /// Fired once the subagent runner's HTTP request has been fully read
    /// (the runner is in-flight, blocked awaiting the never-sent response).
    request_received: Arc<Notify>,
    /// Fired when the connection is torn down after the request (EOF or
    /// reset) — the runner's in-flight model future was dropped/aborted.
    connection_closed: Arc<Notify>,
}

/// A local model endpoint that reads the runner's request and then never
/// responds. Doubles as the probe for the sync-abandon test:
/// `request_received` proves the subagent runner reached its model call
/// (execute has spawned the sync wrapper and is awaiting `done_rx`), and
/// `connection_closed` proves the runner was aborted — dropping the
/// in-flight reqwest future closes the TCP connection, which the stub's
/// post-request read observes as EOF or a reset error.
async fn blocking_model() -> (String, BlockingStub) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let request_received = Arc::new(Notify::new());
    let connection_closed = Arc::new(Notify::new());
    let hook_received = request_received.clone();
    let hook_closed = connection_closed.clone();
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
        // Runner is in-flight and blocked awaiting the response we never send.
        hook_received.notify_one();
        // After the request the runner never writes again, so the next read
        // returns only when the connection is torn down (clean EOF on socket
        // close, or a reset error) — i.e. the in-flight future was aborted.
        let mut buf = [0; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        hook_closed.notify_one();
    });
    (
        format!("http://{address}"),
        BlockingStub {
            request_received,
            connection_closed,
        },
    )
}

#[tokio::test]
async fn delegate_requires_workspace_parameter() {
    let temp = tempfile::tempdir().unwrap();
    let tool = delegate(temp.path());

    // Missing workspace: rejected before any subagent is spawned.
    let error = tool
        .execute(json!({"task": "hello", "background": false}))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "delegate requires a workspace parameter: absolute path of the working directory"
    );

    // An empty/whitespace workspace is equally a missing parameter.
    let error = tool
        .execute(json!({"task": "hello", "workspace": "  ", "background": false}))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "delegate requires a workspace parameter: absolute path of the working directory"
    );

    // `.` is no longer silently reinterpreted as the parent workspace: it
    // is a relative path and reroot() rejects it.
    let error = tool
        .execute(json!({"task": "hello", "workspace": ".", "background": false}))
        .await
        .unwrap_err();
    assert!(
        error.contains("invalid `workspace`"),
        "`.` must be rejected as a non-absolute workspace, got: {error}"
    );
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
    assert_eq!(spec.parameters["required"], json!(["task", "workspace"]));
    let workspace = &spec.parameters["properties"]["workspace"];
    assert_eq!(workspace["type"], "string");
    assert!(
        workspace["description"]
            .as_str()
            .unwrap()
            .contains("REQUIRED"),
        "workspace must be documented as required, got: {workspace}"
    );
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
        .record_background_tasks_in(record_root.clone(), "parent", SessionStore::Jsonl);

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
            images: vec![],
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
    let (tools, _) = builtins(workspace, None, false, None);
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

/// Run one subagent through a capturing mock model and return the wire
/// request body (tools array + system prompt) plus the tool result.
async fn run_subagent_and_capture(
    temp: &tempfile::TempDir,
    role_template: &str,
    with_sandbox: bool,
) -> (String, String) {
    let directory = temp.path().join("agents");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("auditor.md"), role_template).unwrap();

    let (url, captured) = capturing_model().await;
    let mut tool = delegate_with_url(temp.path(), url).with_roles_root(temp.path().to_path_buf());
    if with_sandbox {
        tool = tool.with_sandbox(Some(crate::config::Sandbox {
            enabled: true,
            network: true,
            workspace_writable: true,
            writable_paths: vec!["/mnt/big/cargo-home".into()],
            readable_paths: vec!["~/.rustup".into()],
        }));
    }
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    tool.set_event_sender(sender);

    let output = tool
        .execute(json!({
            "task": "audit",
            "role": "auditor",
            "workspace": temp.path().to_str().unwrap(),
            "background": false
        }))
        .await
        .unwrap();
    let request = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    (output, request)
}

#[tokio::test]
// Test-only env isolation: the std Mutex guard is held across .execute()
// awaits to serialize XDG_CONFIG_HOME mutation with roles.rs tests.
#[allow(clippy::await_holding_lock)]
async fn read_only_role_subagent_gets_no_write_tools_and_a_read_only_system_note() {
    let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg-empty");
    std::fs::create_dir_all(&xdg).unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    let (output, request) = run_subagent_and_capture(
        &temp,
        "---\nread_only = true\n---\nAudit the workspace and report.\n",
        false,
    )
    .await;
    assert!(output.contains("done"), "{output}");

    // Tool list: no write/edit; no bash either (fail closed: the delegate
    // inherits no sandbox policy in these tests).
    assert!(request.contains("\"read_file\""), "{request}");
    assert!(!request.contains("\"write_file\""), "{request}");
    assert!(!request.contains("\"edit_file\""), "{request}");
    assert!(!request.contains("\"bash\""), "{request}");
    assert!(
        request.contains("get_background_tasks") && request.contains("cancel_background_task"),
        "{request}"
    );
    // The system prompt carries the read-only declaration.
    assert!(
        request.contains(
            "This role is read-only: no write_file/edit_file; bash, when present, runs in a read-only sandbox with network disabled."
        ),
        "{request}"
    );

    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[tokio::test]
// Test-only env isolation: the std Mutex guard is held across .execute()
// awaits to serialize XDG_CONFIG_HOME mutation with roles.rs tests.
#[allow(clippy::await_holding_lock)]
async fn read_only_role_subagent_keeps_bash_when_a_sandbox_is_configured() {
    let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg-empty");
    std::fs::create_dir_all(&xdg).unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    let (output, request) = run_subagent_and_capture(
        &temp,
        "---\nread_only = true\n---\nAudit the workspace and report.\n",
        true,
    )
    .await;
    assert!(output.contains("done"), "{output}");

    assert!(request.contains("\"read_file\""), "{request}");
    assert!(!request.contains("\"write_file\""), "{request}");
    assert!(!request.contains("\"edit_file\""), "{request}");
    assert!(request.contains("\"bash\""), "{request}");
    // The sandboxed bash description reflects the narrowed read-only policy.
    assert!(
        request.contains("workspace is read-only") && request.contains("network is disabled"),
        "{request}"
    );
    assert!(
        request.contains("~/.rustup"),
        "readable roots survive into the subagent's sandbox description: {request}"
    );

    unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
}

#[tokio::test]
// Test-only env isolation: the std Mutex guard is held across .execute()
// awaits to serialize XDG_CONFIG_HOME mutation with roles.rs tests.
#[allow(clippy::await_holding_lock)]
async fn ordinary_role_keeps_write_tools_without_the_read_only_note() {
    let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg-empty");
    std::fs::create_dir_all(&xdg).unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

    let (output, request) = run_subagent_and_capture(&temp, "Fix things.", false).await;
    assert!(output.contains("done"), "{output}");

    assert!(request.contains("\"write_file\""), "{request}");
    assert!(request.contains("\"edit_file\""), "{request}");
    assert!(request.contains("\"bash\""), "{request}");
    assert!(
        !request.contains("This role is read-only"),
        "ordinary roles must not carry the read-only system note: {request}"
    );

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
    let (_, mut parent_background) = builtins(workspace.clone(), None, false, None);
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
    // A custom workspace must be a directory within existing authority.
    let custom_path = parent.path().join("custom");
    std::fs::create_dir(&custom_path).unwrap();
    // Put a marker file in each workspace.
    std::fs::write(custom_path.join("sentinel.txt"), "custom").unwrap();
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
            "workspace": custom_path.to_str().unwrap(),
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
        .execute(json!({
            "task": "hello",
            "workspace": temp.path().to_str().unwrap(),
            "background": false
        }))
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

/// Poll the JSONL metadata store until the subagent session's row appears
/// (the parent writes it fire-and-forget via `spawn_subagent_meta_create`
/// at spawn time) and return its title.
async fn subagent_meta_title(root: &std::path::Path, session_id: &str) -> Option<String> {
    let store = SessionStore::Jsonl;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let meta = store
            .list_meta(root)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == session_id);
        if let Some(meta) = meta {
            return meta.title;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "subagent metadata row for `{session_id}` never appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn fresh_spawn_records_label_as_subagent_session_title() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let base_url = successful_model("finished answer").await;
    let mut tool = delegate_with_url(temp.path(), base_url).persist_sessions(root.clone());
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    tool.set_event_sender(sender);

    let answer = tool
        .execute(json!({
            "task": "hello",
            "label": "my panel title",
            "workspace": temp.path().to_str().unwrap(),
            "background": false
        }))
        .await
        .unwrap();
    let session_id = answer
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("sync success contains the subagent session id");
    assert!(
        session_id.starts_with("sub-"),
        "fresh spawn must allocate a new session id, got {session_id}"
    );

    let title = subagent_meta_title(&root, session_id).await;
    assert_eq!(
        title.as_deref(),
        Some("my panel title"),
        "a fresh spawn records its task-panel label as the session title"
    );
}

#[tokio::test]
async fn fresh_spawn_without_label_records_the_fallback_title() {
    // No caller label: task_label falls back to the role name or a task
    // preview, so a fresh spawn's title is never empty.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let base_url = successful_model("finished answer").await;
    let mut tool = delegate_with_url(temp.path(), base_url).persist_sessions(root.clone());
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    tool.set_event_sender(sender);

    let answer = tool
        .execute(json!({
            "task": "hello world task",
            "workspace": temp.path().to_str().unwrap(),
            "background": false
        }))
        .await
        .unwrap();
    let session_id = answer
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("sync success contains the subagent session id");

    let title = subagent_meta_title(&root, session_id).await;
    assert_eq!(
        title.as_deref(),
        Some("hello world task"),
        "fallback title (task preview) is recorded"
    );
}

#[tokio::test]
async fn resume_keeps_the_original_subagent_session_title() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let base_url = successful_model_n("finished answer", 2).await;
    let mut tool = delegate_with_url(temp.path(), base_url).persist_sessions(root.clone());
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    tool.set_event_sender(sender);
    let workspace = temp.path().to_str().unwrap().to_owned();

    // 1. Fresh spawn records the caller's label as the title.
    let answer = tool
        .execute(json!({
            "task": "first task",
            "label": "first title",
            "workspace": workspace.clone(),
            "background": false
        }))
        .await
        .unwrap();
    let session_id = answer
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("sync success contains the subagent session id");
    assert_eq!(
        subagent_meta_title(&root, session_id).await.as_deref(),
        Some("first title")
    );

    // 2. Resume with a different label: the resumed create_meta passes no
    //    title (existing row → backfill no-op), so the original title is
    //    preserved, never overwritten.
    let answer = tool
        .execute(json!({
            "task": "follow-up",
            "label": "second title",
            "resume": session_id,
            "workspace": workspace,
            "background": false
        }))
        .await
        .unwrap();
    let resumed_id = answer
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("subagent session: "))
        .expect("sync success contains the subagent session id");
    assert_eq!(resumed_id, session_id, "resume reuses the session id");

    // Let the fire-and-forget meta create run; the title must not move.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        subagent_meta_title(&root, session_id).await.as_deref(),
        Some("first title"),
        "resume must never overwrite the title recorded by the original spawn"
    );
}

#[tokio::test]
async fn subagent_meta_create_without_label_leaves_title_none() {
    // The helper-level label is optional: recording a row with no label
    // (e.g. a resume) leaves the session unnamed instead of inventing one.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let session_id = format!("sub-{}", crate::session::new_id());
    spawn_subagent_meta_create(
        SessionStore::Jsonl,
        root.clone(),
        session_id.clone(),
        "test-model".into(),
        None,
        None,
        1,
        None,
    );
    let title = subagent_meta_title(&root, &session_id).await;
    assert_eq!(title, None, "no label ⇒ no title (session stays unnamed)");
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
            "label": "first line\nsecond line",
            "workspace": temp.path().to_str().unwrap()
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
        .execute(json!({
            "task": "hello",
            "workspace": temp.path().to_str().unwrap(),
            "background": true
        }))
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
            images: vec![],
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
        .execute(json!({
            "task": "new prompt",
            "resume": "sub-resume-scrollback",
            "workspace": temp.path().to_str().unwrap()
        }))
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

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// btw fork subagent
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `btw_fork_entries` cuts the source history at the last completed turn,
/// then stamps the `ForkedFrom` marker + the explanation notice. Entries
/// after the boundary are dropped; an empty session (or one with no
/// completed turn) is rejected.
#[test]
fn btw_fork_entries_cuts_at_last_completed_turn() {
    use crate::agent::SessionEntry as AgentEntry;
    let user = |text: &str| AgentEntry::Message {
        message: Message::User {
            content: text.into(),
            images: vec![],
        },
    };
    let assistant = |text: &str| AgentEntry::Message {
        message: Message::Assistant(AssistantMessage {
            content: Some(text.into()),
            tool_calls: vec![],
            reasoning: None,
        }),
    };
    let entries = vec![
        user("hello"),
        assistant("hi"),
        user("question 1"),
        assistant("answer 1"),
        // Trailing entries after the last completed turn are dropped.
        AgentEntry::Notice {
            text: "[background task 1 completed]".into(),
        },
    ];
    let fork = btw_fork_entries("web-main", &entries).unwrap();
    assert_eq!(fork.len(), 6, "prefix + marker + notice: {fork:?}");
    assert_eq!(fork[0], user("hello"));
    assert_eq!(fork[1], assistant("hi"));
    assert_eq!(fork[2], user("question 1"));
    assert_eq!(fork[3], assistant("answer 1"));
    assert_eq!(
        fork[4],
        AgentEntry::ForkedFrom {
            source: "web-main".into(),
            at: 4,
            event_time: None,
            seq: None,
        }
    );
    assert_eq!(
        fork[5],
        AgentEntry::Notice {
            text: "（btw fork：在主线之外继续探讨）".into(),
        }
    );
    // No completed turn → error (mirrors `fork_prefix`).
    let no_boundary = vec![user("only a question")];
    assert!(btw_fork_entries("web-main", &no_boundary).is_err());
    assert!(btw_fork_entries("web-main", &[]).is_err());
}

/// Full JSONL spawn: the source session's history is forked into a fresh
/// `btw-…` session (prefix + ForkedFrom marker + notice), the question
/// lands as the first user message, the subagent is registered in the task
/// registry + `Sessions` with a `btw:` label, and cancelling the task
/// cleans up the registration and the parent's background record. The
/// runner model points at a dead port, so the first turn fails fast and
/// the subagent settles into Idle (WaitForInput — it never finishes on its
/// own, which is exactly the persistent semantics).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_btw_subagent_forks_history_and_registers_persistent_subagent() {
    use crate::agent::SessionEntry as AgentEntry;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    // A source session with one completed turn.
    let source_entries = vec![
        AgentEntry::Message {
            message: Message::User {
                content: "main question".into(),
                images: vec![],
            },
        },
        AgentEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: Some("main answer".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        },
    ];
    crate::session::Session::append(&root, "web-main", &source_entries).unwrap();

    let workspace = Workspace::new(&root).unwrap();
    let (_, mut background) = builtins(workspace.clone(), None, false, None);
    let (sender, _completions) = tokio::sync::mpsc::unbounded_channel();
    background.set_event_sender(sender);
    let sessions = Sessions::default();
    // Dead port: connection refused instantly, no retry hang.
    let model = ConfiguredModel::chat(
        crate::model::OpenAiModel::new(
            "http://127.0.0.1:9".into(),
            "test-key".into(),
            "test-model".into(),
            None,
        )
        .unwrap(),
    );
    let id = spawn_btw_subagent(
        "web-main",
        "side question",
        BtwContext {
            model,
            context_window: None,
            workspace,
            sandbox: None,
            read_only: false,
            background: background.clone(),
            sessions: sessions.clone(),
            persist_root: root.clone(),
            backend: SessionBackend::Jsonl,
            record_in: Some(crate::session_store::BackgroundRecord {
                root: root.clone(),
                session: "web-main".into(),
                store: crate::session_store::SessionStore::Jsonl,
            }),
        },
    )
    .await
    .unwrap();
    assert!(id.starts_with("btw-"), "btw session id, got {id}");

    // Registered in the task registry (attachable via the TUI panel / web
    // task panel) and the live-session registry.
    let tasks = background.running();
    assert_eq!(tasks.len(), 1, "one btw task: {tasks:?}");
    assert_eq!(tasks[0].kind, "delegate");
    assert!(
        tasks[0].label.starts_with("btw: "),
        "label: {}",
        tasks[0].label
    );
    let task_id = tasks[0].id;
    let entry = sessions.get(task_id).expect("btw session registered");
    assert_eq!(entry.session_id, id);
    assert_eq!(entry.model, "test-model");
    assert_eq!(entry.role, None);

    // The btw session file: fork prefix + marker + notice, then the
    // question as the first user message (committed by the runner; poll
    // because the commit is async).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let loaded = loop {
        let loaded = crate::session::Session::load(&root, &id).unwrap().entries;
        if loaded.len() >= 5 || std::time::Instant::now() > deadline {
            break loaded;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(
        loaded.len(),
        5,
        "2 prefix + marker + notice + question: {loaded:?}"
    );
    assert_eq!(loaded[0], source_entries[0]);
    assert_eq!(loaded[1], source_entries[1]);
    assert_eq!(
        loaded[2],
        AgentEntry::ForkedFrom {
            source: "web-main".into(),
            at: 2,
            event_time: None,
            seq: None,
        }
    );
    assert_eq!(
        loaded[3],
        AgentEntry::Notice {
            text: "（btw fork：在主线之外继续探讨）".into(),
        }
    );
    assert_eq!(
        loaded[4],
        AgentEntry::Message {
            message: Message::User {
                content: "side question".into(),
                images: vec![],
            },
        }
    );

    // The task stays registered (WaitForInput: the runner does not finish
    // when idle), so the subagent is persistent — not cleaned up like a
    // completed delegate.
    assert_eq!(background.running().len(), 1);
    assert!(sessions.get(task_id).is_some());

    // Cancelling the task (the current close path for a btw subagent)
    // aborts the runner and cleans up the registration + the parent's
    // background record.
    assert!(background.cancel(task_id).is_some());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let sessions_empty = sessions.sessions.lock().unwrap().is_empty();
        let tasks_empty = background.running().is_empty();
        if sessions_empty && tasks_empty {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cleanup did not complete: sessions_empty={sessions_empty} tasks_empty={tasks_empty}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The wrapper's Drop-based cleanup clears the parent's background
    // record immediately after removing the in-memory registration (no
    // await between the two), but the OS may preempt the runtime thread
    // in that window — poll for the record to clear instead of asserting
    // once (the record MUST disappear: a cancelled btw task is not a
    // killed-on-exit record).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if crate::session::Session::take_unfinished_background(&root, "web-main").is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cancelled btw task must not leave a killed-on-exit record"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
