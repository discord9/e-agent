//! An external ACP agent exposed as a [`Model`].
//!
//! ACP agents are stateful conversations: they own the history, so a turn sends
//! only the newest user text and the returned [`AssistantMessage`] is just the
//! text streamed during that turn. That is exactly the shape the runner already
//! consumes, so a persisted session can keep talking to the same agent process
//! without any new UI.

use std::path::PathBuf;

use anyhow::{anyhow, bail};

use crate::acp_client::AcpClient;
use crate::agent::{AssistantMessage, Message, Model, ModelDeltaKind, ToolSpec, Usage};
use crate::codex::DeltaSink;

/// One conversation with an external ACP agent process.
///
/// The child is spawned lazily on the first [`Model::complete`] and kept alive
/// across turns. One child per instance, no global registry: cloning the model
/// (the session factory clones models per session) yields a fresh, unspawned
/// conversation rather than sharing a child.
pub struct AcpModel {
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    /// The live transport and its ACP session id, both set by the first turn.
    client: Option<AcpClient>,
    session_id: Option<String>,
    /// Set after a fatal transport/protocol error. Resuming a dead agent session
    /// is a non-goal, so later turns fail loudly instead of silently starting a
    /// fresh agent that lost the conversation.
    failed: Option<String>,
}

impl AcpModel {
    pub fn new(command: impl Into<String>, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            command: command.into(),
            args,
            cwd,
            client: None,
            session_id: None,
            failed: None,
        }
    }

    /// Test entry point: drive the model over an injected transport (a stub
    /// agent on the other end of in-memory pipes) instead of a child process.
    #[cfg(test)]
    fn with_client(client: AcpClient, cwd: PathBuf) -> Self {
        Self {
            command: "stub".into(),
            args: Vec::new(),
            cwd,
            client: Some(client),
            session_id: None,
            failed: None,
        }
    }

    /// `initialize` + `session/new` exactly once, on the first turn.
    async fn ensure_session(&mut self) -> anyhow::Result<String> {
        if let Some(session_id) = &self.session_id {
            return Ok(session_id.clone());
        }
        if self.client.is_none() {
            self.client = Some(AcpClient::spawn(&self.command, &self.args, &self.cwd)?);
        }
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| anyhow!("ACP client is not running"))?;
        client.initialize().await?;
        let session_id = client.new_session(&self.cwd).await?;
        self.session_id = Some(session_id.clone());
        Ok(session_id)
    }

    async fn turn(
        &mut self,
        messages: &[Message],
        on_delta: DeltaSink<'_>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let session_id = self.ensure_session().await?;
        let text = newest_user_text(messages)?;
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| anyhow!("ACP client is not running"))?;
        let (content, usage) = client.prompt(&session_id, &text, on_delta).await?;
        Ok((
            AssistantMessage {
                content: Some(content),
                tool_calls: Vec::new(),
                reasoning: None,
            },
            usage,
        ))
    }
}

impl Clone for AcpModel {
    fn clone(&self) -> Self {
        Self::new(self.command.clone(), self.args.clone(), self.cwd.clone())
    }
}

#[async_trait::async_trait]
impl Model for AcpModel {
    fn set_request_session_id(&mut self, _id: &str) {}

    async fn complete(
        &mut self,
        messages: &[Message],
        _tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        if let Some(failure) = &self.failed {
            bail!("ACP agent session is dead after an earlier failure: {failure}");
        }
        let result = self.turn(messages, on_delta).await;
        if let Err(error) = &result {
            // Never leave the child running after a failed turn.
            self.failed = Some(format!("{error:#}"));
            self.session_id = None;
            self.client = None;
        }
        result
    }

    fn name(&self) -> &str {
        &self.command
    }

    fn supports_vision(&self) -> bool {
        false
    }
}

/// ACP is stateful: the agent holds the conversation, so a turn sends only the
/// newest user text instead of replaying history.
fn newest_user_text(messages: &[Message]) -> anyhow::Result<String> {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .ok_or_else(|| anyhow!("ACP model requires a user message to prompt the agent"))
}

#[cfg(test)]
mod tests;
