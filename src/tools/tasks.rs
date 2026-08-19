use super::*;

use async_trait::async_trait;

use super::background::BackgroundTasks;

/// Termination thresholds for the unchanged-snapshot poll guard on
/// `get_background_tasks`: the `count >= threshold` poll returns the
/// internal termination sentinel, and reminders fire from the
/// `threshold - 2` poll (never before the second) through `threshold - 1`.
/// Subagents terminate on the 3rd unchanged poll, the main agent on the
/// 5th.
pub(super) const SUBAGENT_POLL_GUARD_THRESHOLD: u8 = 3;
pub(super) const MAIN_POLL_GUARD_THRESHOLD: u8 = 5;

/// Per-turn poll-guard state for `get_background_tasks` calls.
#[derive(Default)]
pub(super) struct PollGuardState {
    /// Sorted running-task ID snapshot observed by the last poll this turn;
    /// `None` before the first poll.
    last_snapshot: Option<Vec<u64>>,
    /// Consecutive polls of the SAME snapshot within the current turn.
    count: u8,
}

pub(super) struct GetBackgroundTasks {
    pub(super) background: BackgroundTasks,
    /// The calling session's own id (subagents only). A running delegate
    /// task whose `display_meta.subagent_session_id` matches is the calling
    /// subagent itself and is annotated as such in the output. The main
    /// agent (and any caller without a session id) passes `None`.
    pub(super) self_session_id: Option<String>,
    /// Unchanged-snapshot poll guard: `Some(threshold)` enables escalation —
    /// consecutive `get_background_tasks` calls with an unchanged NON-EMPTY
    /// running-task snapshot within one turn return the model-facing
    /// [`crate::agent::POLL_GUARD_ERROR`] from the `threshold - 2` poll and
    /// the internal termination sentinel ([`crate::agent::POLL_GUARD_SENTINEL`])
    /// from the `threshold` poll onward; the batch loops map the sentinel to
    /// POLL_GUARD_ERROR content and use it to end the turn after the full
    /// sibling batch. Empty snapshots never escalate and clear any guard
    /// state. `None` disables the guard. Subagents use threshold 3
    /// (reminder on the 2nd poll), the main agent threshold 5 (reminder on
    /// the 3rd and 4th).
    pub(super) poll_guard: Option<u8>,
    guard: std::sync::Mutex<PollGuardState>,
}

impl GetBackgroundTasks {
    pub(super) fn new(
        background: BackgroundTasks,
        self_session_id: Option<String>,
        poll_guard: Option<u8>,
    ) -> Self {
        Self {
            background,
            self_session_id,
            poll_guard,
            guard: std::sync::Mutex::new(PollGuardState::default()),
        }
    }
}

#[async_trait]
impl Tool for GetBackgroundTasks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_background_tasks".into(),
            description: "Return a one-time snapshot of currently running background tasks. \
                Do not poll or repeatedly call this tool to wait for completion. \
                Results are delivered automatically as [background task N completed]; \
                continue independent work or end the turn while waiting."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    fn on_turn_start(&mut self) {
        // Per-true-turn reset: a fresh prompt, a queued prompt, an idle
        // background-completion follow-up, or a direct Agent::run call
        // starts with a clean slate. Never called mid-round, mid-batch, or
        // around compaction, so those never reset the guard.
        if self.poll_guard.is_some() {
            *self.guard.lock().unwrap() = PollGuardState::default();
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput, String> {
        let tasks = self.background.running();
        // Empty snapshots never escalate: with no running tasks there is
        // nothing to wait for, so repeated polls stay the plain "no tasks"
        // output and never latch the reminder/sentinel. Any observed
        // non-empty guard state is cleared so a later real snapshot starts
        // counting fresh (an empty poll is also an ID-set change from any
        // previous non-empty snapshot).
        if tasks.is_empty() {
            if self.poll_guard.is_some() {
                *self.guard.lock().unwrap() = PollGuardState::default();
            }
            return Ok(ToolOutput::text("No background tasks running."));
        }
        // Poll guard: escalate on the SORTED running-task ID snapshot. Any
        // ID-set change (new task, completion, cancellation) resets the
        // count; output growth or task ordering never does. A repeated
        // non-empty snapshot escalates at the configured threshold: the
        // `threshold - 2` poll (never before the second) returns the
        // model-facing reminder, `threshold` and beyond the internal
        // termination sentinel.
        if let Some(threshold) = self.poll_guard {
            let mut ids: Vec<u64> = tasks.iter().map(|task| task.id).collect();
            ids.sort_unstable();
            let mut guard = self.guard.lock().unwrap();
            if guard.last_snapshot.as_deref() != Some(ids.as_slice()) {
                guard.last_snapshot = Some(ids);
                guard.count = 1;
            } else {
                guard.count = guard.count.saturating_add(1);
            }
            let reminder_start = threshold.saturating_sub(2).max(2);
            if guard.count >= threshold {
                // At or beyond the threshold: internal sentinel — the batch
                // loops map it to POLL_GUARD_ERROR for history/UI and end
                // the turn after the full sibling batch.
                return Err(crate::agent::POLL_GUARD_SENTINEL.to_owned());
            }
            if guard.count >= reminder_start {
                return Err(crate::agent::POLL_GUARD_ERROR.to_owned());
            }
        }
        let mut out = format!("{} background task(s) running:\n", tasks.len());
        for task in tasks.iter() {
            let role = task.role.as_deref().unwrap_or(&task.kind);
            let tags = task
                .display_meta
                .as_ref()
                .map(|meta| {
                    let mut t = String::new();
                    if meta.background {
                        t.push_str(" [background]");
                    }
                    if let Some(ws) = &meta.workspace {
                        use crate::agent::preview;
                        t.push_str(&format!(" [workspace: {}]", preview(ws, 40)));
                    }
                    t
                })
                .unwrap_or_default();
            // A delegate task whose subagent_session_id matches the calling
            // subagent's own session id IS the caller (the subagent itself
            // appears in the parent's shared registry as a delegate entry).
            let self_marker = if task
                .display_meta
                .as_ref()
                .and_then(|meta| meta.subagent_session_id.as_deref())
                .is_some_and(|sid| self.self_session_id.as_deref() == Some(sid))
            {
                " [self]"
            } else {
                ""
            };
            out.push_str(&format!(
                "#{}: {} ({}){}{}\n",
                task.id, task.label, role, tags, self_marker
            ));
            // 完整命令原文：label 是源头截断的 100 字符预览，这里给模型
            // 未被截断的原始命令（bash 任务才有；delegate 任务无命令，
            // 省略该行——向后兼容旧格式，首行 `#id: label (role)` 不变）。
            if let Some(command) = &task.full_command {
                out.push_str(&format!("    command: {command}\n"));
            }
        }
        out.truncate(out.trim_end().len());
        Ok(ToolOutput::text(out))
    }
}

pub(super) struct CancelBackgroundTask {
    pub(super) background: BackgroundTasks,
}

#[async_trait]
impl Tool for CancelBackgroundTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "cancel_background_task".into(),
            description: "Cancel a currently running background bash or delegate task.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "background task id"}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let id = arguments
            .as_object()
            .ok_or("tool arguments must be a JSON object")?
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("`id` must be a non-negative integer")?;
        self.background
            .cancel(id)
            .ok_or_else(|| format!("background task {id} is not running"))?;
        Ok(ToolOutput::text(format!("cancelled background task {id}")))
    }
}
