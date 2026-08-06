use super::*;

use async_trait::async_trait;

use super::background::BackgroundTasks;

pub(super) struct GetBackgroundTasks {
    pub(super) background: BackgroundTasks,
    /// The calling session's own id (subagents only). A running delegate
    /// task whose `display_meta.subagent_session_id` matches is the calling
    /// subagent itself and is annotated as such in the output. The main
    /// agent (and any caller without a session id) passes `None`.
    pub(super) self_session_id: Option<String>,
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

    async fn execute(&self, _arguments: Value) -> Result<String, String> {
        let tasks = self.background.running();
        if tasks.is_empty() {
            return Ok("No background tasks running.".into());
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
        Ok(out)
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

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let id = arguments
            .as_object()
            .ok_or("tool arguments must be a JSON object")?
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("`id` must be a non-negative integer")?;
        self.background
            .cancel(id)
            .ok_or_else(|| format!("background task {id} is not running"))?;
        Ok(format!("cancelled background task {id}"))
    }
}
