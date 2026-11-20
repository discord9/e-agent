//! Manual, ignored, three-arm PLUMBING run for the poll-audit path
//! (heuristic → second-stage auditor → advisory `Notice`).
//!
//! What this proves (and nothing more):
//!
//! 1. One pre-decision `SessionEntry` prefix and one initial fixture world are
//!    cloned into three arms — H (heuristic only, no auditor), L (heuristic +
//!    the product LLM auditor) and S (heuristic + a deterministic offline
//!    auditor) — in a seeded, randomized arm order, each in its own store and
//!    world.
//! 2. All three arms start from the identical durable prefix and world.
//! 3. Every observed consequence is the deterministic consequence of the
//!    recorded action, replayed from the initial world independently of the
//!    live fixture; the same action always commits the same consequence in
//!    every arm, so divergent actions still produce valid consequences. With
//!    the deterministic scripted actor the whole pre-`Notice` trajectory is
//!    identical across arms; a real actor's own choices are recorded, never
//!    scored.
//! 4. An advisory `Notice` is only ever committed at a safe boundary (directly
//!    after a committed tool result, with no assistant tool call left open)
//!    and is delivered to a later provider request.
//! 5. The H arm never commits and never delivers a Notice, while L/S hand the
//!    real product projection (classified `status_probe` events, no raw
//!    commands) to their auditor at the real heuristic trigger.
//!
//! This is milestone 1: it does NOT measure efficacy, model compliance, or
//! notice impact, and it never scores H vs L vs S.
//!
//! Run it manually (ignored by default; a normal `cargo test` makes no
//! provider call):
//!
//! ```sh
//! XDG_CONFIG_HOME=$HOME/.config \
//!   cargo test --lib poll_audit_trajectory -- --ignored --nocapture
//! ```
//!
//! Same run through the documented DeepSeek substitute (the oracle profile is
//! unavailable), with the key file given explicitly:
//!
//! ```sh
//! E_AGENT_POLL_AUDIT_DEEPSEEK=1 \
//! E_AGENT_POLL_AUDIT_DEEPSEEK_KEY_FILE=/home/discord9/e-agent/.e-agent/tmp_key \
//!   cargo test --lib poll_audit_trajectory -- --ignored --nocapture
//! ```
//!
//! The actor and the auditor must resolve, through e-agent's own config path,
//! to the oracle model id [`ORACLE_MODEL`]. When no configured profile does,
//! the test fails with the exact blocker and the configured profile → wire
//! model table instead of silently substituting another model.
//!
//! `E_AGENT_POLL_AUDIT_PROFILE=<profile>` pins the profile explicitly (it must
//! still resolve to [`ORACLE_MODEL`]).
//!
//! `E_AGENT_POLL_AUDIT_ACTOR=script` replaces the actor and the L auditor with
//! deterministic offline stand-ins and needs no config or credential. That mode
//! exists ONLY to self-check this harness's plumbing (pairing, boundaries,
//! delivery, consequence replay) where no provider is available; it is not the
//! experiment and proves nothing about any model.
//!
//! `E_AGENT_POLL_AUDIT_DEBUG=1` additionally dumps every actor request decision
//! and every arm's durable entry list to stderr, for reading a manual run.
//!
//! DeepSeek substitute (NOT the frozen oracle): setting
//! `E_AGENT_POLL_AUDIT_DEEPSEEK=1` replaces the config-profile resolution with
//! one test-only DeepSeek chat profile used by BOTH the actor and the auditor:
//! `https://api.deepseek.com` + a key file (default
//! `/home/discord9/e-agent/.e-agent/tmp_key`) + `deepseek-v4-flash`, each
//! overridable through `E_AGENT_POLL_AUDIT_DEEPSEEK_BASE_URL`,
//! `E_AGENT_POLL_AUDIT_DEEPSEEK_KEY_FILE` and `E_AGENT_POLL_AUDIT_DEEPSEEK_MODEL`.
//! That flag exists because the oracle's `gpt-4.1-2025-04-14` contract was
//! unavailable in this environment; a DeepSeek run is a clearly labelled
//! substitute for plumbing/behavior observation, never an oracle comparison and
//! never an efficacy measurement. The default path keeps the exact-model
//! blocker and still refuses to substitute anything.

use super::*;

use crate::agent::{AssistantMessage, ModelDeltaKind, Tool, Usage, preview};
use crate::config::{AuthMode, Config, ResolvedModel};
use crate::model::{ConfiguredModel, OpenAiModel};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The oracle model for BOTH the actor and the auditor. Never substituted.
const ORACLE_MODEL: &str = "gpt-4.1-2025-04-14";
/// Actor provider rounds per arm (the auditor's own calls are not rounds).
const MAX_ACTOR_ROUNDS: usize = 12;
/// User turns per arm.
const USER_TURNS: usize = 3;
/// Fixed seed for the arm order and the per-arm session ids.
const SEED: u64 = 0x0005_EEDA_0D17;
const ENV_ACTOR: &str = "E_AGENT_POLL_AUDIT_ACTOR";
const ENV_PROFILE: &str = "E_AGENT_POLL_AUDIT_PROFILE";
const SCRIPTED_ACTOR: &str = "script";
/// Explicit opt-in for the documented DeepSeek substitute (test-only profile).
const ENV_DEEPSEEK: &str = "E_AGENT_POLL_AUDIT_DEEPSEEK";
const ENV_DEEPSEEK_KEY_FILE: &str = "E_AGENT_POLL_AUDIT_DEEPSEEK_KEY_FILE";
const ENV_DEEPSEEK_MODEL: &str = "E_AGENT_POLL_AUDIT_DEEPSEEK_MODEL";
const ENV_DEEPSEEK_BASE_URL: &str = "E_AGENT_POLL_AUDIT_DEEPSEEK_BASE_URL";
/// Test-only DeepSeek substitute defaults. Nothing here is product config: the
/// key file is read only when [`ENV_DEEPSEEK`] is set, and never written.
const DEEPSEEK_KEY_FILE: &str = "/home/discord9/e-agent/.e-agent/tmp_key";
const DEEPSEEK_MODEL: &str = "deepseek-v4-flash";
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";
/// Upper bound for one turn and for one audit delivery.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const AUDIT_GRACE: Duration = Duration::from_secs(30);

const FILE_PATH: &str = "src/notes.txt";
const BUILD_LOG: &str = "build.log";
const INITIAL_FILE: &str = "fn flaky_timeout() -> u64 {\n    todo!(\"pick a bound\")\n}\n";
const EDIT_OLD: &str = "todo!(\"pick a bound\")";
const EDIT_NEW: &str = "200";

/// The experiment system prompt: part of the cloned pre-decision state, so it
/// is identical in every arm and never written to the durable log.
const SYSTEM_PROMPT: &str = "You are working in a SIMULATED fixture workspace. There is no real shell, \
no network and no database: the `bash` tool only simulates a fixed command list and `apply_edit` only \
edits the single file `src/notes.txt`. Supported `bash` commands: `ls`, `ps`, `date`, `git status`, \
`cat build.log`, `cat src/notes.txt`, `cargo build`. Any other command fails with an explicit error. \
A background build is already running; it finishes only after the prepared source edit has landed. \
Use tools; do not describe what you would do.";

const PREFIX_PROMPT: &str = "Inspect this fixture repository before changing anything: list the files \
and read src/notes.txt. A background build is already running.";
const TURN_1: &str = "The background build is still running. Before anything else, call the `bash` \
tool three times in ONE reply with exactly these commands, in this order: `ps`, `cat build.log`, `ps`. \
Then, in your next reply, call `bash` once with `cat build.log` and report whether the build finished. \
Do not edit any file in this turn.";
const TURN_2: &str = "Apply the prepared fix: call `apply_edit` with path `src/notes.txt`, old \
`todo!(\"pick a bound\")` and new `200`. Do not poll anything in this turn; report the new file content.";
const TURN_3: &str = "Verify the fix: call `bash` with `cargo build`, then `bash` with `cat build.log`. \
Report the build result and the final file content.";
const TURN_PROMPTS: [&str; USER_TURNS] = [TURN_1, TURN_2, TURN_3];
const ROUND_CAP_TEXT: &str =
    "[experiment round cap reached] the provider round budget is spent; ending this turn.";

// ---------------------------------------------------------------- fixture world

/// One actor action against the fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Bash {
        command: String,
    },
    Edit {
        path: String,
        old: String,
        new: String,
    },
}

impl Action {
    fn from_call(call: &ToolCall) -> Result<Self, String> {
        let arguments: Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("{} arguments are not JSON: {error}", call.name))?;
        match call.name.as_str() {
            "bash" => arguments
                .get("command")
                .and_then(Value::as_str)
                .map(|command| Action::Bash {
                    command: command.trim().to_owned(),
                })
                .ok_or_else(|| "bash fixture requires a string `command`".to_owned()),
            "apply_edit" => Ok(Action::Edit {
                path: arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                old: arguments
                    .get("old")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                new: arguments
                    .get("new")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            other => Err(format!("the fixture exposes no `{other}` tool")),
        }
    }

    fn label(&self) -> String {
        match self {
            Action::Bash { command } => format!("bash:{command}"),
            Action::Edit { path, old, new } => format!("apply_edit:{path}:{old}->{new}"),
        }
    }
}

/// The initial world, cloned into every arm. Deliberately small: one editable
/// file plus one simulated background build whose readiness is a pure function
/// of the file content, so the independently verifiable edit is the only thing
/// that can finish the build.
#[derive(Clone, Debug, PartialEq, Eq)]
struct World {
    file: String,
    /// Every applied action, in order. The durable session log is the
    /// authoritative record; this trail only proves the fixture state moved
    /// with exactly those actions.
    applied: Vec<String>,
}

impl World {
    fn initial() -> Self {
        Self {
            file: INITIAL_FILE.to_owned(),
            applied: Vec::new(),
        }
    }

    /// The simulated build finishes exactly when the prepared edit landed.
    fn build_ready(&self) -> bool {
        self.file != INITIAL_FILE
    }

    /// Pure consequence of one action: never reads the clock, the arm, or any
    /// provider state.
    fn consequence(&self, action: &Action) -> (Self, Result<String, String>) {
        let mut next = self.clone();
        let result = match action {
            Action::Edit { path, old, new } => {
                if path != FILE_PATH {
                    Err(format!(
                        "fixture workspace has exactly one file ({FILE_PATH}); `{path}` does not exist"
                    ))
                } else if old.is_empty() || !next.file.contains(old.as_str()) {
                    Err(format!(
                        "edit rejected: the `old` text is not present in {FILE_PATH}"
                    ))
                } else {
                    next.file = next.file.replacen(old, new, 1);
                    Ok(format!(
                        "edited {FILE_PATH}: replaced 1 occurrence\n--- {FILE_PATH} ---\n{}",
                        next.file
                    ))
                }
            }
            Action::Bash { command } => match command.as_str() {
                "ls" => Ok(format!("{BUILD_LOG}\n{FILE_PATH}")),
                "ps" => Ok(format!(
                    "PID TTY      TIME CMD\n4242 ?    00:00:00 build-sim\nbuild: {}",
                    if self.build_ready() {
                        "finished"
                    } else {
                        "running"
                    }
                )),
                "date" => Ok("Thu Jan  1 00:00:07 UTC 1970 (simulated clock)".to_owned()),
                "git status" => Ok(
                    "On branch main\nnothing to commit, working tree clean (simulated)".to_owned(),
                ),
                "cat build.log" => Ok(if self.build_ready() {
                    "line 3: compiling\nline 4: build finished OK".to_owned()
                } else {
                    "line 3: compiling (build still running)".to_owned()
                }),
                "cat src/notes.txt" => Ok(next.file.clone()),
                "cargo build" => {
                    if self.build_ready() {
                        Ok("build finished OK".to_owned())
                    } else {
                        Err(
                            "error: fixture build failed: the prepared source edit has not been \
                             applied yet"
                                .to_owned(),
                        )
                    }
                }
                other => Err(format!(
                    "fixture bash does not simulate `{other}`; supported commands: ls, ps, date, \
                     git status, cat build.log, cat src/notes.txt, cargo build"
                )),
            },
        };
        if result.is_ok() {
            next.applied.push(action.label());
        }
        (next, result)
    }
}

fn apply_action(world: &Arc<Mutex<World>>, action: Action) -> Result<ToolOutput, String> {
    let mut guard = world.lock().unwrap();
    let (next, result) = guard.consequence(&action);
    *guard = next;
    result.map(ToolOutput::text)
}

/// Simulated `bash`: no process, no network, no database, no shell parsing.
struct FixtureBash {
    world: Arc<Mutex<World>>,
}

#[async_trait]
impl Tool for FixtureBash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Simulated shell for the fixture workspace (no real process, no network). \
                          Supported commands: ls, ps, date, git status, cat build.log, cat src/notes.txt, \
                          cargo build. Any other command fails with an explicit error."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "bash fixture requires a string `command`".to_owned())?
            .trim()
            .to_owned();
        apply_action(&self.world, Action::Bash { command })
    }
}

/// The single fixture file editor.
struct FixtureEdit {
    world: Arc<Mutex<World>>,
}

#[async_trait]
impl Tool for FixtureEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_edit".into(),
            description: "Apply one literal text replacement to the fixture file src/notes.txt. \
                          This is the only file that exists in the fixture."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                },
                "required": ["path", "old", "new"],
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        apply_action(
            &self.world,
            Action::Edit {
                path: arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                old: arguments
                    .get("old")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                new: arguments
                    .get("new")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            },
        )
    }
}

/// The pre-decision prefix: one inspection turn already committed, ending on a
/// tool result, so the runner's next decision is an assistant decision. Every
/// arm clones exactly these entries and exactly [`World::initial`].
fn pre_decision_prefix() -> Vec<SessionEntry> {
    let world = World::initial();
    let listing = world
        .consequence(&Action::Bash {
            command: "ls".into(),
        })
        .1
        .expect("ls is supported");
    let reading = world
        .consequence(&Action::Bash {
            command: "cat src/notes.txt".into(),
        })
        .1
        .expect("read is supported");
    vec![
        Message::User {
            content: PREFIX_PROMPT.to_owned(),
            images: Vec::new(),
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("Inspecting the fixture workspace.".into()),
            tool_calls: vec![
                ToolCall {
                    id: "prefix-ls".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}).to_string(),
                },
                ToolCall {
                    id: "prefix-read".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "cat src/notes.txt"}).to_string(),
                },
            ],
            reasoning: None,
        })
        .into(),
        Message::Tool {
            call_id: "prefix-ls".into(),
            name: "bash".into(),
            content: listing,
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        }
        .into(),
        Message::Tool {
            call_id: "prefix-read".into(),
            name: "bash".into(),
            content: reading,
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        }
        .into(),
    ]
}

// ------------------------------------------------------------------- the arms

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    /// The heuristic stage alone: no auditor is installed, so a fired heuristic
    /// never commits and never delivers anything.
    Only,
    /// The product second stage: the real LLM auditor.
    Llm,
    /// The product second stage, fed by a deterministic offline auditor.
    Offline,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Arm::Only => "H:heuristic-only",
            Arm::Llm => "L:heuristic+llm-auditor",
            Arm::Offline => "S:heuristic+offline-auditor",
        }
    }
}

/// Deterministic offline auditor rule. Both variants read ONLY the real product
/// projection built by `poll_audit_events`, never the fixture or the logs.
#[derive(Clone, Copy, Debug)]
enum OfflineRule {
    /// S arm: escalate on the same three consecutive status probes the
    /// heuristic itself counts.
    ThreeProbes,
    /// Harness self-check stand-in for the L arm (script mode only).
    AnyProbe,
}

#[derive(Clone, Debug, Default)]
struct AuditRecord {
    events: usize,
    status_probes: usize,
    programs: Vec<String>,
    verdict: String,
}

impl AuditRecord {
    fn escalates(&self) -> bool {
        self.verdict.contains("SUSPECT") || self.verdict.contains("FAIL")
    }
}

/// Read the auditor's request exactly as the product built it: the last user
/// message is the classified event projection.
fn audit_projection(messages: &[Message]) -> AuditRecord {
    let raw = messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User { content, .. } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let events = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| value.get("events").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let mut programs = Vec::new();
    let mut status_probes = 0;
    for event in &events {
        if event.pointer("/args/kind").and_then(Value::as_str) == Some("status_probe") {
            status_probes += 1;
            if let Some(program) = event.pointer("/args/program").and_then(Value::as_str) {
                programs.push(program.to_owned());
            }
        }
    }
    AuditRecord {
        events: events.len(),
        status_probes,
        programs,
        verdict: String::new(),
    }
}

struct OfflineAuditor {
    rule: OfflineRule,
    label: String,
    records: Arc<Mutex<Vec<AuditRecord>>>,
}

#[async_trait]
impl Model for OfflineAuditor {
    fn name(&self) -> &str {
        &self.label
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let mut record = audit_projection(messages);
        let escalate = match self.rule {
            OfflineRule::ThreeProbes => record.status_probes >= 3,
            OfflineRule::AnyProbe => record.status_probes > 0,
        };
        record.verdict = if escalate { "SUSPECT" } else { "CLEAN" }.to_owned();
        let verdict = record.verdict.clone();
        self.records.lock().unwrap().push(record);
        Ok((
            AssistantMessage {
                content: Some(format!("{{\"verdict\":\"{verdict}\"}}")),
                tool_calls: Vec::new(),
                reasoning: None,
            },
            None,
        ))
    }
}

/// The real auditor: forwards the product request unchanged and records the
/// exact projection plus the raw verdict text.
struct RecordedAuditor {
    inner: Box<dyn Model>,
    records: Arc<Mutex<Vec<AuditRecord>>>,
}

#[async_trait]
impl Model for RecordedAuditor {
    fn set_request_session_id(&mut self, id: &str) {
        self.inner.set_request_session_id(id);
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let mut record = audit_projection(messages);
        let (assistant, usage) = self.inner.complete(messages, tools, on_delta).await?;
        record.verdict = assistant.content.clone().unwrap_or_default();
        self.records.lock().unwrap().push(record);
        Ok((assistant, usage))
    }
}

// ------------------------------------------------------------------ the actor

/// Wraps the arm's actor model: records every request projection the runner
/// issued (so the test can prove a `Notice` reached a request) and enforces the
/// round budget on real provider calls.
struct RecordingActor {
    inner: Box<dyn Model>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    rounds: Arc<AtomicUsize>,
    capped: Arc<AtomicBool>,
}

#[async_trait]
impl Model for RecordingActor {
    fn set_request_session_id(&mut self, id: &str) {
        self.inner.set_request_session_id(id);
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        // Record every request projection the runner issued, including the
        // capped one: that is exactly the context a provider would have seen
        // (and how the test proves a Notice reached a request). `rounds` counts
        // only real provider calls.
        self.requests.lock().unwrap().push(messages.to_vec());
        if self.rounds.load(Ordering::SeqCst) >= MAX_ACTOR_ROUNDS {
            self.capped.store(true, Ordering::SeqCst);
            return Ok((
                AssistantMessage {
                    content: Some(ROUND_CAP_TEXT.into()),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
                None,
            ));
        }
        self.rounds.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(messages, tools, on_delta).await
    }
}

/// Deterministic offline stand-in for the actor in script mode. It reads the
/// REAL product request projection (not a side channel), so the harness still
/// exercises heuristic → auditor → `Notice`.
struct ScriptedActor {
    calls: usize,
    arm: &'static str,
}

impl ScriptedActor {
    fn reply(&mut self, messages: &[Message]) -> AssistantMessage {
        let notice_seen = messages.iter().any(
            |message| matches!(message, Message::User { content, .. } if content == POLL_AUDIT_NOTICE),
        );
        let position = messages.iter().rposition(|message| match message {
            Message::User { content, .. } => TURN_PROMPTS.contains(&content.as_str()),
            _ => false,
        });
        let (turn, round) = match position {
            Some(index) => {
                let prompt = match &messages[index] {
                    Message::User { content, .. } => content.as_str(),
                    _ => "",
                };
                (
                    TURN_PROMPTS
                        .iter()
                        .position(|candidate| *candidate == prompt)
                        .unwrap_or(0),
                    messages[index + 1..]
                        .iter()
                        .filter(|message| matches!(message, Message::Assistant(_)))
                        .count(),
                )
            }
            None => (0, 0),
        };
        if std::env::var("E_AGENT_POLL_AUDIT_DEBUG").is_ok() {
            let users = messages
                .iter()
                .filter_map(|message| match message {
                    Message::User { content, .. } => Some(preview(content, 40)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            eprintln!(
                "[scripted {}] msgs={} turn={turn} round={round} notice={notice_seen} users={users:?}",
                self.arm,
                messages.len()
            );
        }
        if turn == 0 {
            // The polling turn: this stand-in keeps watching the build until
            // something stops it. The ONLY thing that stops it is the product
            // Notice appearing in its real request projection.
            if notice_seen {
                return text_only("audit acknowledged: no further status checks; ending the turn.");
            }
            return match round {
                0 => tool_round(&mut self.calls, &["ps", "cat build.log", "ps"]),
                1..=3 => tool_round(&mut self.calls, &["cat build.log"]),
                _ => text_only("the build is still running; ending this turn."),
            };
        }
        // Turns 2 and 3 are independent work, never status probes.
        match (turn, round) {
            (1, 0) => AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: format!("edit-{}", self.calls),
                    name: "apply_edit".into(),
                    arguments: json!({"path": FILE_PATH, "old": EDIT_OLD, "new": EDIT_NEW})
                        .to_string(),
                }],
                reasoning: None,
            },
            (1, _) => text_only("the prepared fix is applied."),
            (2, 0) => tool_round(&mut self.calls, &["cargo build"]),
            _ => text_only("verified: the fixture build finished and the file holds the fix."),
        }
    }
}

fn text_only(content: &str) -> AssistantMessage {
    AssistantMessage {
        content: Some(content.to_owned()),
        tool_calls: Vec::new(),
        reasoning: None,
    }
}

fn tool_round(calls: &mut usize, commands: &[&str]) -> AssistantMessage {
    let tool_calls = commands
        .iter()
        .map(|command| {
            let id = format!("script-{calls}");
            *calls += 1;
            ToolCall {
                id,
                name: "bash".into(),
                arguments: json!({"command": command}).to_string(),
            }
        })
        .collect();
    AssistantMessage {
        content: None,
        tool_calls,
        reasoning: None,
    }
}

#[async_trait]
impl Model for ScriptedActor {
    fn name(&self) -> &str {
        "scripted-fixture-actor"
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        Ok((self.reply(messages), None))
    }
}

// --------------------------------------------------------- model resolution

/// Resolve the oracle profile through e-agent's own config path, or fail with
/// the exact blocker. Never substitutes another model id.
fn resolve_oracle(config: &Config) -> Result<String, String> {
    let mut table = Vec::new();
    let mut matches = Vec::new();
    for profile in config.model_profiles() {
        match config.resolve_profile(&profile) {
            Ok(resolved) => {
                if resolved.model == ORACLE_MODEL {
                    matches.push(profile.clone());
                }
                table.push(format!("{profile} -> {}", resolved.model));
            }
            Err(error) => table.push(format!("{profile} -> <unresolved: {error:#}>")),
        }
    }
    if let Ok(pinned) = std::env::var(ENV_PROFILE) {
        let resolved = config
            .resolve_profile(&pinned)
            .map_err(|error| format!("{ENV_PROFILE}={pinned} cannot be resolved: {error:#}"))?;
        if resolved.model != ORACLE_MODEL {
            return Err(format!(
                "BLOCKER: {ENV_PROFILE}={pinned} resolves to wire model `{}`, but the oracle requires \
                 `{ORACLE_MODEL}`. Refusing to substitute.\nconfigured profiles:\n  {}",
                resolved.model,
                table.join("\n  ")
            ));
        }
        return Ok(pinned);
    }
    if matches.is_empty() {
        return Err(format!(
            "BLOCKER: no configured model profile resolves to the oracle model `{ORACLE_MODEL}`, and no \
             provider credential for it is configured through e-agent. Refusing to substitute another \
             model: the actor and the auditor must both be `{ORACLE_MODEL}`.\nconfigured profiles:\n  {}",
            table.join("\n  ")
        ));
    }
    matches.sort();
    Ok(matches.remove(0))
}

/// The documented DeepSeek substitute: ONE test-only chat profile shared by the
/// actor and the auditor. It exists because the frozen oracle
/// (`ORACLE_MODEL`) had no configured profile/credential in this environment.
/// Nothing here is product configuration: the key file is read once per model
/// build and is never logged or persisted.
#[derive(Clone, Debug)]
struct DeepseekSubstitute {
    base_url: String,
    key_file: std::path::PathBuf,
    model: String,
}

impl DeepseekSubstitute {
    fn from_env() -> Self {
        let read = |name: &str, fallback: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| fallback.to_owned())
        };
        Self {
            base_url: read(ENV_DEEPSEEK_BASE_URL, DEEPSEEK_BASE_URL),
            key_file: std::path::PathBuf::from(read(ENV_DEEPSEEK_KEY_FILE, DEEPSEEK_KEY_FILE)),
            model: read(ENV_DEEPSEEK_MODEL, DEEPSEEK_MODEL),
        }
    }

    fn enabled() -> bool {
        matches!(
            std::env::var(ENV_DEEPSEEK).as_deref().map(str::trim),
            Ok("1") | Ok("true") | Ok("yes")
        )
    }

    fn api_key(&self) -> Result<String, String> {
        let key = std::fs::read_to_string(&self.key_file).map_err(|error| {
            format!(
                "BLOCKER: cannot read the DeepSeek substitute key file {} ({error}). Point \
                 {ENV_DEEPSEEK_KEY_FILE} at a readable key, or unset {ENV_DEEPSEEK} to use the \
                 configured oracle profile path.",
                self.key_file.display()
            )
        })?;
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err(format!(
                "BLOCKER: the DeepSeek substitute key file {} is empty",
                self.key_file.display()
            ));
        }
        Ok(key)
    }

    fn describe(&self) -> String {
        format!(
            "DeepSeek substitute `{}` at {} (key file {}, NOT the frozen oracle `{ORACLE_MODEL}`)",
            self.model,
            self.base_url,
            self.key_file.display()
        )
    }

    fn build(&self, role: &str) -> Result<Box<dyn Model>, String> {
        let key = self.api_key()?;
        let model = OpenAiModel::new(self.base_url.clone(), key, self.model.clone(), None)
            .map_err(|error| {
                format!("cannot build the {role} DeepSeek substitute model: {error:#}")
            })?;
        let mut configured = ConfiguredModel::chat(model);
        configured.display = Some(format!("deepseek-substitute/{}", self.model));
        Ok(Box::new(configured))
    }
}

fn build_model(config: &Config, profile: &str, role: &str) -> Result<Box<dyn Model>, String> {
    let resolved: ResolvedModel = config
        .resolve_profile(profile)
        .map_err(|error| format!("{role} profile `{profile}` cannot be resolved: {error:#}"))?;
    if resolved.model != ORACLE_MODEL {
        return Err(format!(
            "BLOCKER: {role} profile `{profile}` resolves to `{}`, not the oracle `{ORACLE_MODEL}`",
            resolved.model
        ));
    }
    if resolved.auth != AuthMode::ApiKey {
        return Err(format!(
            "BLOCKER: {role} profile `{profile}` uses {:?} auth, which cannot serve the chat model \
             `{ORACLE_MODEL}`",
            resolved.auth
        ));
    }
    let model = OpenAiModel::new(
        resolved.base_url.clone(),
        resolved.api_key.clone(),
        resolved.model.clone(),
        resolved.reasoning_effort.clone(),
    )
    .map_err(|error| format!("cannot build {role} model `{}`: {error:#}", resolved.model))?
    .with_vision(resolved.vision)
    .with_thinking(resolved.thinking)
    .with_deepseek_compat(resolved.deepseek_compat);
    let mut configured = ConfiguredModel::chat(model);
    configured.display = Some(resolved.display);
    Ok(Box::new(configured))
}

// ------------------------------------------------------------- arm execution

struct Experiment {
    config: Option<Config>,
    profile: String,
    scripted: bool,
    /// `Some` only for the explicit, documented DeepSeek substitute.
    substitute: Option<DeepseekSubstitute>,
    /// The wire model id both real backends must resolve to.
    expected_model: String,
}

impl Experiment {
    fn config(&self) -> &Config {
        self.config
            .as_ref()
            .expect("real mode resolved the config up front")
    }

    /// The one real-model builder: the configured oracle profile by default, or
    /// the explicit DeepSeek substitute profile.
    fn build(&self, role: &str) -> Result<Box<dyn Model>, String> {
        match &self.substitute {
            Some(substitute) => substitute.build(role),
            None => build_model(self.config(), &self.profile, role),
        }
    }

    fn actor(
        &self,
        arm: Arm,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        rounds: Arc<AtomicUsize>,
        capped: Arc<AtomicBool>,
    ) -> Result<Box<dyn Model>, String> {
        let inner: Box<dyn Model> = if self.scripted {
            Box::new(ScriptedActor {
                calls: 0,
                arm: arm.label(),
            })
        } else {
            self.build("actor")?
        };
        Ok(Box::new(RecordingActor {
            inner,
            requests,
            rounds,
            capped,
        }))
    }

    fn auditor(
        &self,
        arm: Arm,
        records: Arc<Mutex<Vec<AuditRecord>>>,
    ) -> Result<Option<Box<dyn Model>>, String> {
        Ok(match (arm, self.scripted) {
            (Arm::Only, _) => None,
            (Arm::Llm, true) => Some(Box::new(OfflineAuditor {
                rule: OfflineRule::AnyProbe,
                label: "scripted-auditor(always-SUSPECT)".into(),
                records,
            })),
            (Arm::Llm, false) => Some(Box::new(RecordedAuditor {
                inner: self.build("auditor")?,
                records,
            })),
            (Arm::Offline, _) => Some(Box::new(OfflineAuditor {
                rule: OfflineRule::ThreeProbes,
                label: "deterministic-offline-auditor".into(),
                records,
            })),
        })
    }
}

/// One observed actor step: the recorded call plus its durable result, matched
/// in commit order.
#[derive(Clone, Debug)]
struct Step {
    action: Action,
    content: String,
    is_error: bool,
    /// Whether an audit `Notice` was already durable when this step committed.
    after_notice: bool,
}

struct ArmOutcome {
    arm: Arm,
    root: std::path::PathBuf,
    session: String,
    actor_model: String,
    auditor_model: String,
    initial_world: World,
    entries: Vec<SessionEntry>,
    steps: Vec<Step>,
    world: World,
    requests: Vec<Vec<Message>>,
    rounds: usize,
    capped: bool,
    audit: Vec<AuditRecord>,
    notices: Vec<usize>,
    stopped_by: String,
    elapsed: Duration,
}

fn audit_notices(entries: &[SessionEntry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| match entry {
            SessionEntry::Notice { text } if text == POLL_AUDIT_NOTICE => Some(index),
            _ => None,
        })
        .collect()
}

/// Pair the durable assistant tool calls with their tool results, in order.
/// `from` skips the cloned prefix: its tool results are cloned state, not
/// consequences of this arm's own actions (the prefix is verified structurally
/// against the canonical clone instead).
fn observed_steps(entries: &[SessionEntry], from: usize) -> Result<Vec<Step>, String> {
    let mut pending: Vec<(ToolCall, bool)> = Vec::new();
    let mut steps = Vec::new();
    let mut notice_seen = false;
    for entry in &entries[from..] {
        match entry {
            SessionEntry::Notice { text } if text == POLL_AUDIT_NOTICE => notice_seen = true,
            SessionEntry::Message {
                message: Message::Assistant(assistant),
            } => pending.extend(
                assistant
                    .tool_calls
                    .iter()
                    .map(|call| (call.clone(), notice_seen)),
            ),
            SessionEntry::Message {
                message:
                    Message::Tool {
                        call_id,
                        content,
                        is_error,
                        ..
                    },
            } => {
                let index = pending
                    .iter()
                    .position(|(call, _)| &call.id == call_id)
                    .ok_or_else(|| format!("tool result `{call_id}` has no committed call"))?;
                let (call, after_notice) = pending.remove(index);
                steps.push(Step {
                    action: Action::from_call(&call)?,
                    content: content.clone(),
                    is_error: *is_error,
                    after_notice,
                });
            }
            _ => {}
        }
    }
    Ok(steps)
}

fn turn_prompt_count(entries: &[SessionEntry], from: usize) -> usize {
    entries[from..]
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                SessionEntry::Message {
                    message: Message::User { .. }
                }
            )
        })
        .count()
}

fn requests_with_notice(requests: &[Vec<Message>]) -> usize {
    requests
        .iter()
        .filter(|request| {
            request.iter().any(
                |message| matches!(message, Message::User { content, .. } if content == POLL_AUDIT_NOTICE),
            )
        })
        .count()
}

async fn load_entries(root: &std::path::Path, session: &str) -> Vec<SessionEntry> {
    // A poll may catch a partially flushed JSONL tail; that is transient.
    SessionStore::Jsonl
        .load(root, session)
        .await
        .map(|loaded| loaded.entries)
        .unwrap_or_default()
}

/// Wait until `wanted` turn prompts are durable AND the runner is idle again.
async fn await_turns(
    root: &std::path::Path,
    session: &str,
    prefix_len: usize,
    wanted: usize,
    status: &mut tokio::sync::watch::Receiver<SessionStatus>,
) -> Result<(), String> {
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        let entries = load_entries(root, session).await;
        if turn_prompt_count(&entries, prefix_len) >= wanted && is_idle(status) {
            // The store append and the status flip are not atomic: let the
            // turn's last commits land before the caller reads them.
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Ok(());
        }
        if let SessionStatus::Finished(result) = &*status.borrow() {
            return Err(format!("session finished early: {result:?}"));
        }
        if Instant::now() > deadline {
            return Err(format!(
                "turn {wanted} did not finish within {TURN_TIMEOUT:?}; status={:?} entries={}",
                *status.borrow(),
                entries.len()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn is_idle(status: &tokio::sync::watch::Receiver<SessionStatus>) -> bool {
    matches!(&*status.borrow(), SessionStatus::Idle)
}

/// Wait until the audit is settled: either every committed `Notice` has reached
/// a provider request, or the auditor returned a non-escalating verdict (so no
/// `Notice` is expected). Quiescence is 8 consecutive settled polls.
async fn await_audit_settled(
    root: &std::path::Path,
    session: &str,
    requests: &Arc<Mutex<Vec<Vec<Message>>>>,
    audit: &Arc<Mutex<Vec<AuditRecord>>>,
    has_auditor: bool,
) {
    if !has_auditor {
        return;
    }
    let deadline = Instant::now() + AUDIT_GRACE;
    let mut quiet = 0;
    while Instant::now() < deadline {
        let entries = load_entries(root, session).await;
        let notices = audit_notices(&entries).len();
        let delivered = {
            let requests = requests.lock().unwrap();
            requests_with_notice(&requests)
        };
        let (records, escalating) = {
            let audit = audit.lock().unwrap();
            (
                audit.len(),
                audit.iter().filter(|record| record.escalates()).count(),
            )
        };
        let settled = records > 0
            && delivered >= notices
            && (escalating == 0 || notices >= 1)
            && notices <= escalating;
        if settled {
            quiet += 1;
            if quiet >= 8 {
                return;
            }
        } else {
            quiet = 0;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_arm(
    experiment: &Experiment,
    arm: Arm,
    seed: u64,
    prefix: &[SessionEntry],
) -> Result<ArmOutcome, String> {
    let started = Instant::now();
    let root = tempfile::tempdir().map_err(|error| format!("cannot create arm root: {error}"))?;
    let session = format!(
        "poll-audit-arm-{seed}-{}",
        arm.label().replace([':', '+'], "-")
    );
    let initial_world = World::initial();
    let world = Arc::new(Mutex::new(initial_world.clone()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let rounds = Arc::new(AtomicUsize::new(0));
    let capped = Arc::new(AtomicBool::new(false));
    let audit = Arc::new(Mutex::new(Vec::new()));

    let actor = experiment.actor(arm, requests.clone(), rounds.clone(), capped.clone())?;
    let actor_model = actor.name().to_owned();
    let (mut agent, notice_sender) = Agent::new_with_notice_sender(
        actor,
        vec![
            Box::new(FixtureBash {
                world: world.clone(),
            }),
            Box::new(FixtureEdit {
                world: world.clone(),
            }),
        ],
    );
    agent.set_context_prefix(SYSTEM_PROMPT.to_owned());
    agent.restore_history(prefix.to_vec());
    let (runner, handle) = SessionRunner::new_with_bootstrap(
        agent,
        SessionStore::Jsonl,
        root.path().into(),
        session.clone(),
        IdlePolicy::WaitForInput,
        SessionBootstrap {
            recovery_tasks: Vec::new(),
            legacy: true,
            initial_entries: Vec::new(),
        },
    )
    .await
    .map_err(|error| format!("cannot bootstrap the {} arm: {error:#}", arm.label()))?;
    let auditor = experiment.auditor(arm, audit.clone())?;
    let has_auditor = auditor.is_some();
    let auditor_model = auditor
        .as_ref()
        .map(|auditor| auditor.name().to_owned())
        .unwrap_or_else(|| "none".to_owned());
    let runner = match auditor {
        Some(auditor) => runner.with_auditor_model(Some(auditor), notice_sender),
        None => runner,
    };
    let (_, _events, mut status) = handle.attach();
    let mut task = runner.start(Some(TURN_PROMPTS[0].to_owned()));

    let mut stopped_by = format!("completed all {USER_TURNS} user turns");
    let mut drive = await_turns(root.path(), &session, prefix.len(), 1, &mut status).await;
    for turn in 2..=USER_TURNS {
        if let Err(error) = &drive {
            stopped_by = format!("aborted before turn {turn}: {error}");
            break;
        }
        let submission = handle.submit_prompt(TURN_PROMPTS[turn - 1].to_owned());
        if submission != PromptSubmission::Queued {
            stopped_by = format!("turn {turn} prompt was not queued: {submission:?}");
            break;
        }
        drive = await_turns(root.path(), &session, prefix.len(), turn, &mut status).await;
    }
    if let Err(error) = &drive {
        stopped_by = format!("{stopped_by} ({error})");
    }

    await_audit_settled(root.path(), &session, &requests, &audit, has_auditor).await;

    // Authorized cancel: release any in-flight operation, then stop the arm.
    handle.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    task.abort();

    let entries = SessionStore::Jsonl
        .load(root.path(), &session)
        .await
        .map_err(|error| format!("cannot read the {} arm log: {error:#}", arm.label()))?
        .entries;
    if std::env::var("E_AGENT_POLL_AUDIT_DEBUG").is_ok() {
        eprintln!("[entries {}] stop={stopped_by}", arm.label());
        for (index, entry) in entries.iter().enumerate() {
            let text = match entry {
                SessionEntry::Message {
                    message: Message::User { content, .. },
                } => format!("user {:?}", preview(content, 60)),
                SessionEntry::Message {
                    message: Message::Assistant(assistant),
                } => format!(
                    "assistant content={:?} calls={:?}",
                    assistant.content.as_deref().map(|c| preview(c, 40)),
                    assistant
                        .tool_calls
                        .iter()
                        .map(|call| call.arguments.clone())
                        .collect::<Vec<_>>()
                ),
                SessionEntry::Message {
                    message: Message::Tool { content, .. },
                } => format!("tool {:?}", preview(content, 40)),
                SessionEntry::Notice { text } => format!("notice {:?}", preview(text, 30)),
                other => format!("{other:?}"),
            };
            eprintln!("  [{index}] {text}");
        }
    }
    Ok(ArmOutcome {
        arm,
        root: root.path().to_path_buf(),
        session,
        actor_model,
        auditor_model,
        initial_world,
        steps: observed_steps(&entries, prefix.len())?,
        notices: audit_notices(&entries),
        entries,
        world: world.lock().unwrap().clone(),
        requests: requests.lock().unwrap().clone(),
        rounds: rounds.load(Ordering::SeqCst),
        capped: capped.load(Ordering::SeqCst),
        audit: audit.lock().unwrap().clone(),
        stopped_by,
        elapsed: started.elapsed(),
    })
}

// --------------------------------------------------------------- the test

/// Deterministic xorshift: the arm order is randomized yet reproducible from
/// [`SEED`].
fn seeded_order(seed: u64) -> [Arm; 3] {
    let mut arms = [Arm::Only, Arm::Llm, Arm::Offline];
    let mut state = seed | 1;
    for index in (1..arms.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        arms.swap(index, (state % (index as u64 + 1)) as usize);
    }
    arms
}

/// A safe boundary for an audit `Notice` is defined by what must NOT be true:
/// no assistant tool call may still be open at `index`, i.e. the Notice never
/// lands inside a partially executed sibling batch. Both live positions are
/// legal and are recorded for the report: directly after a committed tool
/// result (the verdict was ready at the batch boundary) and after the turn's
/// final assistant message (the verdict arrived while the turn was ending).
fn audit_notice_boundary(entries: &[SessionEntry], index: usize) -> Result<&'static str, String> {
    let mut open = std::collections::HashSet::new();
    for entry in &entries[..index] {
        match entry {
            SessionEntry::Message {
                message: Message::Assistant(assistant),
            } => open.extend(assistant.tool_calls.iter().map(|call| call.id.clone())),
            SessionEntry::Message {
                message:
                    Message::Tool {
                        call_id,
                        synthetic: false,
                        ..
                    },
            } => {
                open.remove(call_id);
            }
            _ => {}
        }
    }
    if !open.is_empty() {
        return Err(format!(
            "the audit Notice at {index} landed inside an open tool batch: {open:?}"
        ));
    }
    Ok(match entries.get(index.wrapping_sub(1)) {
        Some(SessionEntry::Message {
            message: Message::Tool { .. },
        }) => "after-tool-result",
        Some(SessionEntry::Message {
            message: Message::Assistant(_),
        }) => "after-turn-end",
        Some(SessionEntry::Message {
            message: Message::User { .. },
        }) => "after-user-prompt",
        Some(_) => "after-entry",
        None => "at-log-start",
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual real-API run: needs a configured profile that resolves to gpt-4.1-2025-04-14"]
async fn poll_audit_three_arm_trajectory_plumbing() {
    let scripted = std::env::var(ENV_ACTOR)
        .map(|value| value.trim() == SCRIPTED_ACTOR)
        .unwrap_or(false);
    // The default real backend resolves the oracle through e-agent's own config
    // path BEFORE any provider call. The explicit DeepSeek substitute replaces
    // exactly that resolution (and nothing else); script mode needs neither.
    let substitute = if DeepseekSubstitute::enabled() {
        assert!(
            !scripted,
            "{ENV_DEEPSEEK} and {ENV_ACTOR}={SCRIPTED_ACTOR} are mutually exclusive"
        );
        Some(DeepseekSubstitute::from_env())
    } else {
        None
    };
    let config = if scripted || substitute.is_some() {
        None
    } else {
        Some(
            Config::load()
                .expect("the e-agent config must load")
                .unwrap_or_else(|| {
                    panic!(
                        "BLOCKER: no e-agent config found (checked \
                         $XDG_CONFIG_HOME/e-agent/config.toml and \
                         ~/.config/e-agent/config.toml). The actor and the auditor must both resolve, \
                         through e-agent's own config path, to the oracle model `{ORACLE_MODEL}`."
                    )
                }),
        )
    };
    let profile = match &config {
        Some(config) => match resolve_oracle(config) {
            Ok(profile) => profile,
            Err(blocker) => panic!("{blocker}"),
        },
        None => String::new(),
    };
    let expected_model = match &substitute {
        Some(substitute) => substitute.model.clone(),
        None => ORACLE_MODEL.to_owned(),
    };
    let experiment = Experiment {
        config,
        profile: profile.clone(),
        scripted,
        substitute,
        expected_model,
    };
    if !scripted {
        // Fail before any provider call when the real backend cannot be built
        // (missing oracle profile, unreadable substitute key, ...).
        for role in ["actor", "auditor"] {
            if let Err(blocker) = experiment.build(role) {
                panic!("{blocker}");
            }
        }
        if let Some(substitute) = &experiment.substitute {
            eprintln!(
                "NOTE: running the DeepSeek substitute, {}",
                substitute.describe()
            );
        }
    }

    let prefix = pre_decision_prefix();
    let order = seeded_order(SEED);
    for arm in [Arm::Only, Arm::Llm, Arm::Offline] {
        assert_eq!(
            order.iter().filter(|candidate| **candidate == arm).count(),
            1,
            "the seeded order {order:?} must contain {} exactly once",
            arm.label()
        );
    }

    let mut outcomes = std::collections::BTreeMap::new();
    for (position, arm) in order.into_iter().enumerate() {
        let outcome = run_arm(&experiment, arm, SEED + position as u64, &prefix)
            .await
            .unwrap_or_else(|error| panic!("{} arm did not run: {error}", arm.label()));
        outcomes.insert(arm.label(), outcome);
    }
    let labels: [&str; 3] = [Arm::Only.label(), Arm::Llm.label(), Arm::Offline.label()];

    // 1. Identical cloned prefixes/worlds, and no shared state between arms.
    for label in labels {
        let outcome = &outcomes[label];
        assert_eq!(
            outcome.initial_world,
            World::initial(),
            "{label}: the fixture world did not start from the canonical initial world"
        );
        assert_eq!(
            outcome.entries.get(..prefix.len()),
            Some(prefix.as_slice()),
            "{label}: the durable log does not start with the cloned pre-decision prefix"
        );
        assert!(
            outcome
                .requests
                .iter()
                .all(|request| request.iter().any(
                    |message| matches!(message, Message::User { content, .. } if content == PREFIX_PROMPT)
                )),
            "{label}: every provider request must carry the cloned prefix prompt"
        );
    }
    for (index, label) in labels.iter().enumerate() {
        for other in &labels[index + 1..] {
            let (left, right) = (&outcomes[*label], &outcomes[*other]);
            // Each arm owns its own store root and session id, so no entry,
            // world or audit record can be shared between arms.
            assert_ne!(left.root, right.root, "arms must not share a store root");
            assert_ne!(
                left.session, right.session,
                "arms must not share a session id"
            );
            assert!(
                !left.entries.iter().any(|entry| matches!(
                    entry,
                    SessionEntry::Message { message: Message::User { content, .. } }
                        if content.contains(right.session.as_str())
                )),
                "{label}: an entry of the {other} arm leaked into this arm's log"
            );
        }
    }

    // 2. Budgets, and the same scenario in every arm.
    for label in labels {
        let outcome = &outcomes[label];
        assert!(
            outcome.rounds <= MAX_ACTOR_ROUNDS,
            "{label}: {} provider rounds exceed the budget of {MAX_ACTOR_ROUNDS}",
            outcome.rounds
        );
        let turns = turn_prompt_count(&outcome.entries, prefix.len());
        assert_eq!(
            turns, USER_TURNS,
            "{label}: expected {USER_TURNS} user turns, found {turns} (stop: {})",
            outcome.stopped_by
        );
        if !scripted {
            assert_eq!(
                outcome.actor_model, experiment.expected_model,
                "{label}: the actor must run the backend's declared wire model"
            );
            if outcome.arm == Arm::Llm {
                assert_eq!(
                    outcome.auditor_model, experiment.expected_model,
                    "L: the LLM auditor must run the same wire model as the actor"
                );
            }
        }
    }

    // 3. Every observed consequence is the deterministic consequence of the
    //    recorded action, replayed from the initial world per arm. The replay
    //    never reads the live fixture, so it is the independent verification.
    for label in labels {
        let outcome = &outcomes[label];
        let mut replayed = World::initial();
        for step in &outcome.steps {
            let (next, expected) = replayed.consequence(&step.action);
            match (expected, step.is_error) {
                (Ok(text), false) => assert_eq!(
                    text,
                    step.content,
                    "{label}: `{}` returned a consequence the fixture rule does not predict",
                    step.action.label()
                ),
                (Err(text), true) => assert_eq!(
                    text,
                    step.content,
                    "{label}: `{}` returned an error the fixture rule does not predict",
                    step.action.label()
                ),
                (expected, is_error) => panic!(
                    "{label}: `{}` ok/error mismatch (rule ok={}, recorded is_error={is_error})",
                    step.action.label(),
                    expected.is_ok()
                ),
            }
            replayed = next;
        }
        assert_eq!(
            replayed, outcome.world,
            "{label}: the live fixture world diverged from the replay of the recorded actions"
        );
        // Invariants written against the scenario, not against the rule:
        // readiness is exactly "the prepared edit landed", and the edit really
        // changed the file.
        let edited = outcome
            .steps
            .iter()
            .any(|step| matches!(step.action, Action::Edit { .. }) && !step.is_error);
        assert_eq!(
            edited,
            outcome.world.build_ready(),
            "{label}: build readiness must be exactly the applied edit"
        );
        if edited {
            assert!(
                outcome.world.file.contains(EDIT_NEW) && !outcome.world.file.contains(EDIT_OLD),
                "{label}: the applied edit is not visible in the fixture file: {:?}",
                outcome.world.file
            );
        } else {
            assert_eq!(
                outcome.world.file, INITIAL_FILE,
                "{label}: an arm that never edited must keep the initial file"
            );
        }
    }

    // 4. Divergent actions keep valid consequences: whenever two arms took the
    //    SAME action at the same index, the committed consequence must be
    //    identical, in every arm pair, however far their trajectories diverge
    //    afterwards.
    for (index, label) in labels.iter().enumerate() {
        for other in &labels[index + 1..] {
            let (left, right) = (&outcomes[*label].steps, &outcomes[*other].steps);
            for step in 0..left.len().min(right.len()) {
                if left[step].action == right[step].action {
                    assert_eq!(
                        (left[step].content.as_str(), left[step].is_error),
                        (right[step].content.as_str(), right[step].is_error),
                        "{label} vs {other}: the same action `{}` at step {step} produced different \
                         consequences",
                        left[step].action.label()
                    );
                }
            }
        }
    }

    // 4b. The paired pre-Notice trajectory. The deterministic scripted actor
    //     must reproduce it exactly in every arm; with a real actor a different
    //     model CHOICE before the Notice is recorded (never scored) while a
    //     differing consequence for the same action is already caught above.
    let paired_len = labels
        .iter()
        .map(|label| {
            let steps = &outcomes[*label].steps;
            steps
                .iter()
                .position(|step| step.after_notice)
                .unwrap_or(steps.len())
        })
        .min()
        .unwrap();
    let reference = outcomes[Arm::Only.label()].steps.clone();
    let mut divergences = Vec::new();
    for label in labels {
        let steps = &outcomes[label].steps;
        if scripted {
            assert!(
                steps.len() >= paired_len,
                "{label}: ended after {} steps, before the paired prefix of {paired_len}",
                steps.len()
            );
        }
        for index in 0..paired_len.min(steps.len()) {
            if (steps[index].action.label(), steps[index].content.as_str())
                != (
                    reference[index].action.label(),
                    reference[index].content.as_str(),
                )
            {
                divergences.push(format!("{label} vs H at step {index}"));
                break;
            }
        }
    }
    if scripted {
        assert!(
            divergences.is_empty(),
            "the scripted actor must keep the pre-Notice trajectory identical to the cloned prefix: \
             {divergences:?}"
        );
    }

    // 5. Notice delivery: only the audited arms may commit one, always at a
    //    safe boundary, and always delivered to a later provider request.
    let mut delivered = false;
    for label in labels {
        let outcome = &outcomes[label];
        let boundaries = outcome
            .notices
            .iter()
            .map(|index| {
                audit_notice_boundary(&outcome.entries, *index)
                    .unwrap_or_else(|error| panic!("{label}: {error}"))
            })
            .collect::<Vec<_>>();
        for kind in &boundaries {
            assert!(
                matches!(
                    *kind,
                    "after-tool-result" | "after-turn-end" | "after-user-prompt"
                ),
                "{label}: the audit Notice landed at an unexpected boundary: {kind}"
            );
        }
        let with_notice = requests_with_notice(&outcome.requests);
        if outcome.arm == Arm::Only {
            assert!(
                outcome.notices.is_empty(),
                "H: the heuristic-only arm must never commit an audit Notice"
            );
            assert_eq!(
                with_notice, 0,
                "H: the heuristic-only arm must never deliver an audit Notice"
            );
            assert!(
                outcome.audit.is_empty(),
                "H: the heuristic-only arm must never invoke an auditor"
            );
            continue;
        }
        assert!(
            !outcome.audit.is_empty(),
            "{label}: the auditor was never invoked (fewer than three consecutive status probes?)"
        );
        let escalating = outcome
            .audit
            .iter()
            .filter(|record| record.escalates())
            .count();
        assert!(
            outcome.audit.iter().all(|record| record.status_probes >= 3),
            "{label}: the auditor was handed a projection without three status probes: {:?}",
            outcome.audit
        );
        assert!(
            outcome.notices.len() <= escalating,
            "{label}: {} audit Notices for {escalating} escalating verdicts",
            outcome.notices.len()
        );
        assert!(
            with_notice >= outcome.notices.len(),
            "{label}: {} committed audit Notices but only {with_notice} provider requests carried one",
            outcome.notices.len()
        );
        if outcome.arm == Arm::Offline {
            assert_eq!(
                outcome.notices.len(),
                escalating,
                "S: the deterministic auditor's verdict must decide the committed Notice 1:1"
            );
        }
        delivered |= !outcome.notices.is_empty();
    }
    assert!(
        delivered,
        "BLOCKER: no audit Notice was committed in any arm, so delivery plumbing was not exercised. \
         The heuristic needs three consecutive status probes in one turn; the actor produced: {}",
        labels
            .iter()
            .map(|label| format!(
                "{} -> {:?}",
                label,
                outcomes[*label]
                    .steps
                    .iter()
                    .map(|step| step.action.label())
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>()
            .join(" | ")
    );

    // 6. Report. Never an efficacy claim.
    println!(
        "\npoll-audit three-arm plumbing run (seed {SEED:#x}, order {:?}, mode {})",
        seeded_order(SEED).map(Arm::label),
        if scripted {
            "scripted self-check (no provider)"
        } else {
            "real API"
        }
    );
    match &experiment.substitute {
        Some(substitute) => println!("  backend: {}", substitute.describe()),
        None => println!(
            "  oracle model `{ORACLE_MODEL}`, profile `{profile}`{}",
            if scripted {
                " (not used in this mode)"
            } else {
                ""
            }
        ),
    }
    println!(
        "  paired prefix: {paired_len} steps before the first post-Notice step; \
         pre-Notice divergences: {divergences:?}"
    );
    for label in labels {
        let outcome = &outcomes[label];
        let requests_summary = format!("{} requests", outcome.requests.len());
        println!(
            "  {label}: actor={} auditor={} rounds={}{} turns={} steps={} notices={} verdicts={:?} \
             world(edited={}, build_ready={}) requests={} elapsed={:?} stop={}",
            outcome.actor_model,
            outcome.auditor_model,
            outcome.rounds,
            if outcome.capped { "(capped)" } else { "" },
            turn_prompt_count(&outcome.entries, prefix.len()),
            outcome.steps.len(),
            outcome.notices.len(),
            outcome
                .audit
                .iter()
                .map(|record| record.verdict.clone())
                .collect::<Vec<_>>(),
            outcome
                .steps
                .iter()
                .any(|step| matches!(step.action, Action::Edit { .. })),
            outcome.world.build_ready(),
            requests_summary,
            outcome.elapsed,
            outcome.stopped_by
        );
        for record in &outcome.audit {
            println!(
                "    auditor projection: events={} status_probes={} programs={:?} verdict={}",
                record.events, record.status_probes, record.programs, record.verdict
            );
        }
        if !outcome.notices.is_empty() {
            let boundaries = outcome
                .notices
                .iter()
                .map(|index| audit_notice_boundary(&outcome.entries, *index).unwrap_or("invalid"))
                .collect::<Vec<_>>();
            println!(
                "    notice boundaries: {boundaries:?} at entries {:?} of {}",
                outcome.notices,
                outcome.entries.len()
            );
        }
        println!(
            "    actions={:?}",
            outcome
                .steps
                .iter()
                .map(|step| step.action.label())
                .collect::<Vec<_>>()
        );
    }
    println!("  result: PLUMBING ONLY — no efficacy, compliance, or notice-impact claim.\n");
}
