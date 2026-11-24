//! Log miner: e-agent session JSONL → replay corpus (test support).
//!
//! The detectors are measured against real traffic instead of only the
//! synthetic scenarios: this walks a session file, cuts it into turns, and
//! writes one corpus line per turn in the format [`super::replay`] already
//! consumes:
//!
//! ```json
//! {"id": "<session>@<lines>", "label": "unclear", "variant": "none",
//!  "note": "…", "file": "…", "seq_start": 12, "seq_end": 30,
//!  "calls": [{"name": "bash", "args": {"command": "sleep 5"}, "result": "…"}],
//!  "reasoning": ["…"]}
//! ```
//!
//! **Nothing from a real log is labelled.** `label` is always `unclear` and
//! `variant` always `none`, so the replay reports what fired and scores
//! nothing: which firings are TRUE positives is a human judgement, and this
//! file deliberately does not guess it.
//!
//! ## What a turn is
//!
//! A turn starts at a `Message::User` entry and runs to the next one. Only a
//! real user prompt starts a turn: `Notice` and `BackgroundCompletion`
//! entries are user-SHAPED in the provider request but persisted as their own
//! variants, so they never split a turn. `Compaction` entries (whose
//! `retained` tail repeats earlier messages) are skipped, so no turn is
//! counted twice.
//!
//! ## What the detectors see
//!
//! The corpus is built to reproduce what [`super::PollWatch`] was actually
//! fed, not what is merely present in the file:
//!
//! - A call's `result` is the committed `Message::Tool` content, matched by
//!   `call_id`, falling back to the oldest call still awaiting a result. The
//!   text is recorded as-is (capped at [`MAX_RESULT_CHARS`], flagged with
//!   `"result_truncated": true`), never replaced by the arguments: detector B
//!   asks whether two calls came back with the same answer and detector F
//!   asks whether three calls failed the same way, so a substituted
//!   "result" would answer the wrong question.
//! - The result's `is_error` flag rides along as `"error": true` (detector F
//!   only sees failures; a corpus that omits the flag makes F unmeasurable).
//! - A `synthetic` result (interrupted-turn placeholder) is dropped: the live
//!   loop repairs history with it and never observes it.
//! - A result the shipped `get_background_tasks` snapshot guard answered is
//!   dropped for the same reason ([`crate::agent::POLL_GUARD_ERROR`],
//!   [`crate::agent::POLL_GUARD_TERMINATION_NOTICE`], the legacy sentinel).
//! - A call with no result after it (turn aborted) is dropped: `observe` runs
//!   after the result exists, so emitting it with `"result": null` would feed
//!   the harness's argument fallback and invent an answer the tool never
//!   gave. The run prints how many calls were dropped for this reason.
//! - `completion` is set on the first call of the round that follows a
//!   `BackgroundCompletion` entry, which is where the live loop calls
//!   `background_completed()`.
//! - `background_started` cannot be read directly (`ToolOutput` is never
//!   persisted); it is inferred at BATCH level from the
//!   [`crate::agent::BACKGROUND_REMINDER`] notice the loop applies after a
//!   batch that started background work, and applied to that batch's calls.
//! - Reasoning is credited only to rounds that issued tool calls — a message
//!   with none ends the turn and the live loop checks that before calling
//!   `reasoning()`, so feeding it would fire on the closing message.
//!
//! ## Known extraction limits (reported on the summary line)
//!
//! The corpus format aligns `reasoning[i]` with `calls[i]`, one message per
//! round. A round that issued N>1 calls therefore repeats its reasoning N
//! times, which can over-count detector E's consecutive-message streak on
//! batched rounds (`batched_rounds` in each episode's note). Turns, calls and
//! results are exact; E's streak length on a batched round is an upper bound.
//! Whether a message had NO reasoning is exact (`""`), because an absent
//! reasoning field and an empty one both reset the streak.
//!
//! Run it over this machine's sessions (the summary prints to stdout, the
//! corpus goes to `$POLL_MINE_OUT` or `.e-agent/corpus/real.jsonl`):
//!
//! ```sh
//! cargo test --lib poll_score::mine -- --nocapture
//! ```

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agent::{
    BACKGROUND_REMINDER, Message, POLL_GUARD_ERROR, POLL_GUARD_SENTINEL,
    POLL_GUARD_TERMINATION_NOTICE, SessionEntry,
};

use super::replay;

/// Where the miner writes real-traffic episodes when `$POLL_MINE_OUT` is
/// unset. Untracked scratch: the corpus is a measurement input, not source.
const REAL_CORPUS: &str = ".e-agent/corpus/real.jsonl";

/// Cap on a recorded result's recorded length, mirroring the argument cap the
/// corpus format uses. Counted in CHARS, never bytes, so a multi-byte result
/// cannot be cut inside a code point; a truncated result says so with
/// `"result_truncated": true`, which the harness ignores and the audit reads.
const MAX_RESULT_CHARS: usize = 600;

/// The first `max_chars` characters of `text`, and whether anything was cut.
fn truncate(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => (text[..cut].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// One finished tool call, reduced to what the corpus format carries.
struct MinedCall {
    /// The provider call id, used to pair the call with its result.
    id: String,
    name: String,
    args: Value,
    result: Option<String>,
    /// The tool call FAILED (`is_error` on the committed `Message::Tool`).
    /// Detector F only ever sees failures, so the corpus must say which.
    error: bool,
    completion: bool,
    background_started: bool,
    /// False when the detectors never observed the call (synthetic,
    /// guard-answered, or never answered); such a call is not written.
    keep: bool,
}

/// One turn's worth of traffic, plus how much was dropped on the way.
#[derive(Default)]
struct MinedTurn {
    /// 1-based line numbers of the turn's first and last entries.
    seq_start: usize,
    seq_end: usize,
    calls: Vec<MinedCall>,
    /// Aligned with `calls` by index (see the module docs).
    reasoning: Vec<String>,
    rounds: usize,
    /// Rounds that issued more than one call.
    batched_rounds: usize,
    /// Assistant messages that carried reasoning text.
    reasoning_messages: usize,
    completions: usize,
    synthetic: usize,
    guard_answered: usize,
    unanswered: usize,
}

impl MinedTurn {
    /// Drop what the detectors never saw, keeping `reasoning` aligned.
    fn finish(mut self) -> Option<Self> {
        let mut kept = Vec::with_capacity(self.calls.len());
        let mut reasoning = Vec::with_capacity(self.reasoning.len());
        for (index, call) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            // Synthetic and guard-answered calls were already counted when
            // their result arrived; only a call that never got a result at all
            // is unanswered.
            if !call.keep {
                continue;
            }
            if call.result.is_none() {
                self.unanswered += 1;
                continue;
            }
            kept.push(call);
            reasoning.push(std::mem::take(&mut self.reasoning[index]));
        }
        self.calls = kept;
        self.reasoning = reasoning;
        // A turn that ran a tool round is part of the population even when
        // every one of its calls was unobservable; a turn that only chatted
        // ran no round and is not an episode.
        (!self.calls.is_empty() || self.rounds > 0).then_some(self)
    }

    /// The corpus line for this turn. Fields the harness does not know
    /// (`id`, `file`, `seq_start`, `seq_end`, `note`) are ignored by its
    /// `Deserialize` and exist for the audit trail.
    fn to_json(&self, session: &str, file: &str) -> Value {
        let lines = if self.seq_start == self.seq_end {
            self.seq_start.to_string()
        } else {
            format!("{}-{}", self.seq_start, self.seq_end)
        };
        let note = format!(
            "{file}:{lines}; {} round(s), {} batched; {} reasoning message(s); \
             {} completion(s); dropped {} synthetic, {} guard-answered, {} unanswered call(s)",
            self.rounds,
            self.batched_rounds,
            self.reasoning_messages,
            self.completions,
            self.synthetic,
            self.guard_answered,
            self.unanswered,
        );
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|call| {
                // The recorded text, never a substitute: `null` only when the
                // session holds no result at all (see the module docs).
                let recorded = call.result.as_deref();
                let shown = recorded.map(|text| truncate(text, MAX_RESULT_CHARS));
                let mut call_json = json!({
                    "name": call.name,
                    "args": call.args,
                    "result": shown.as_ref().map_or(Value::Null, |(text, _)| {
                        Value::String(text.clone())
                    }),
                    "error": call.error,
                });
                let object = call_json.as_object_mut().expect("json! built an object");
                if call.completion {
                    object.insert("completion".to_owned(), Value::Bool(true));
                }
                if call.background_started {
                    object.insert("background_started".to_owned(), Value::Bool(true));
                }
                if shown.is_some_and(|(_, truncated)| truncated) {
                    object.insert("result_truncated".to_owned(), Value::Bool(true));
                }
                call_json
            })
            .collect();
        json!({
            "id": format!("{session}@{lines}"),
            "label": "unclear",
            "variant": "none",
            "note": note,
            "file": file,
            "seq_start": self.seq_start,
            "seq_end": self.seq_end,
            "calls": calls,
            "reasoning": self.reasoning,
        })
    }
}

/// Mine one session file's text. `bad` counts lines that are not a
/// `SessionEntry` (a newer/older format, or a torn write); they are skipped
/// instead of aborting the measurement.
fn mine_session(text: &str) -> (Vec<MinedTurn>, usize) {
    let mut turns = Vec::new();
    let mut current: Option<MinedTurn> = None;
    let mut bad = 0;
    // A completion injected between rounds is consumed by the next round's
    // first call: `background_completed()` runs at the model-call boundary.
    let mut pending_completion = false;
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionEntry>(line) else {
            bad += 1;
            continue;
        };
        match entry {
            SessionEntry::Message {
                message: Message::User { .. },
            } => {
                turns.extend(current.take().and_then(MinedTurn::finish));
                current = Some(MinedTurn {
                    seq_start: line_no,
                    seq_end: line_no,
                    ..MinedTurn::default()
                });
            }
            SessionEntry::Message {
                message: Message::Assistant(assistant),
            } => {
                let Some(turn) = current.as_mut() else {
                    continue;
                };
                turn.seq_end = line_no;
                // A message with no tool calls ends the turn, and the live
                // loop returns before the detectors see its reasoning.
                if assistant.tool_calls.is_empty() {
                    continue;
                }
                turn.rounds += 1;
                if assistant.tool_calls.len() > 1 {
                    turn.batched_rounds += 1;
                }
                let reasoning = assistant.reasoning.clone().unwrap_or_default();
                if assistant.reasoning.is_some() {
                    turn.reasoning_messages += 1;
                }
                for (position, call) in assistant.tool_calls.iter().enumerate() {
                    turn.calls.push(MinedCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        // A malformed argument string is kept verbatim as a
                        // JSON string, so two identical malformed calls still
                        // compare equal, exactly as `PollWatch` compares them.
                        args: serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| Value::String(call.arguments.clone())),
                        result: None,
                        error: false,
                        completion: position == 0 && std::mem::take(&mut pending_completion),
                        background_started: false,
                        keep: true,
                    });
                    turn.reasoning.push(reasoning.clone());
                }
            }
            SessionEntry::Message {
                message:
                    Message::Tool {
                        call_id,
                        content,
                        is_error,
                        synthetic,
                        ..
                    },
            } => {
                let Some(turn) = current.as_mut() else {
                    continue;
                };
                turn.seq_end = line_no;
                let unmatched = |turn: &MinedTurn| {
                    turn.calls
                        .iter()
                        .position(|call| call.result.is_none() && call.keep)
                };
                let by_id = turn
                    .calls
                    .iter()
                    .position(|call| call.result.is_none() && call.id == call_id);
                // A repair may have rewritten the id, so fall back to the
                // oldest call still awaiting a result.
                let Some(call_index) = by_id.or_else(|| unmatched(turn)) else {
                    continue;
                };
                if synthetic {
                    turn.synthetic += 1;
                    turn.calls[call_index].keep = false;
                    continue;
                }
                if is_error && guard_answered(&content) {
                    turn.guard_answered += 1;
                    turn.calls[call_index].keep = false;
                    continue;
                }
                turn.calls[call_index].error = is_error;
                turn.calls[call_index].result = Some(content);
            }
            SessionEntry::Notice { text } => {
                let Some(turn) = current.as_mut() else {
                    continue;
                };
                turn.seq_end = line_no;
                // Batch-level `ToolOutput::background_started`, the only trace
                // the loop leaves: the reminder is applied after a batch that
                // launched background work.
                if text == BACKGROUND_REMINDER {
                    for call in &mut turn.calls {
                        call.background_started = true;
                    }
                }
            }
            SessionEntry::BackgroundCompletion { .. } => {
                pending_completion = true;
                if let Some(turn) = current.as_mut() {
                    turn.seq_end = line_no;
                    turn.completions += 1;
                }
            }
            // Compaction (repeats earlier messages), goals, errors, forks: no
            // tool call and no reasoning of their own.
            _ => {}
        }
    }
    turns.extend(current.take().and_then(MinedTurn::finish));
    (turns, bad)
}

/// True when the shipped snapshot guard already answered this result, so the
/// detectors never observed it. Mirrors the live agent's `poll_guard_fired`.
fn guard_answered(content: &str) -> bool {
    content.contains(POLL_GUARD_ERROR)
        || content.contains(POLL_GUARD_TERMINATION_NOTICE)
        || content.ends_with(POLL_GUARD_SENTINEL)
}

/// Every `.jsonl` session file under `root` (sidecars excluded: the JSONL
/// backend keeps `*.background.jsonl` / `*.meta.jsonl` next to each session).
fn session_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            name.ends_with(".jsonl")
                && !name.ends_with(".background.jsonl")
                && !name.ends_with(".meta.jsonl")
        })
        .collect();
    files.sort();
    files
}

/// The session roots to mine: `$POLL_SESSIONS` (colon-separated, files or
/// directories) when set, else `~/.e-agent/sessions` and the workspace's own
/// `.e-agent/sessions`.
fn session_roots() -> Vec<PathBuf> {
    if let Some(roots) = std::env::var_os("POLL_SESSIONS") {
        return std::env::split_paths(&roots).collect();
    }
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".e-agent/sessions"));
    }
    roots.push(PathBuf::from(".e-agent/sessions"));
    roots
}

/// The tally the measurement prints, kept out of the test body so the numbers
/// and the written corpus cannot disagree.
#[derive(Default)]
struct Tally {
    sessions: usize,
    bad_lines: usize,
    turns: usize,
    turns_with_three_calls: usize,
    turns_with_reasoning: usize,
    calls: usize,
    batched_rounds: usize,
    /// Emitted calls that carry a recorded result, and emitted calls that do
    /// not (`"result": null` — never a substitute).
    with_result: usize,
    without_result: usize,
    /// Emitted calls whose result text was cut at [`MAX_RESULT_CHARS`].
    truncated_results: usize,
    /// Emitted calls the tool reported as errors (`"error": true`).
    error_calls: usize,
    /// Calls dropped because the detectors never observed them, by reason.
    dropped: (usize, usize, usize),
    /// First-firing detector per replayed turn, from the harness.
    fired: std::collections::BTreeMap<&'static str, usize>,
    silent: usize,
}

impl Tally {
    fn print(&self, written: Option<(&Path, usize)>) {
        println!("log miner over real sessions");
        println!(
            "  sessions read: {} ({} unparsable line(s) skipped)",
            self.sessions, self.bad_lines
        );
        println!("  turns: {}", self.turns);
        println!(
            "  turns with >=3 consecutive tool calls: {}",
            self.turns_with_three_calls
        );
        println!(
            "  turns carrying reasoning: {} / {}",
            self.turns_with_reasoning, self.turns
        );
        println!(
            "  kept tool calls: {} ({} batched round(s))",
            self.calls, self.batched_rounds
        );
        println!(
            "  calls with a recorded result: {} ({} without); {} truncated to {MAX_RESULT_CHARS} chars",
            self.with_result, self.without_result, self.truncated_results
        );
        println!(
            "  calls that failed (\"error\": true): {}",
            self.error_calls
        );
        println!(
            "  dropped calls: {} synthetic, {} guard-answered, {} unanswered",
            self.dropped.0, self.dropped.1, self.dropped.2
        );
        let fired: Vec<String> = self
            .fired
            .iter()
            .map(|(detector, count)| format!("{detector}={count}"))
            .collect();
        println!(
            "  turns each detector fired on (first firing only): {} silent={}",
            if fired.is_empty() {
                "none".to_owned()
            } else {
                fired.join(" ")
            },
            self.silent
        );
        if let Some((path, bytes)) = written {
            println!("  wrote {} ({bytes} bytes)", path.display());
        }
    }
}

/// Mine the readable sessions, write the corpus, print the measurement.
///
/// This is the "CLI": the summary is a `--nocapture` line, the corpus is a
/// file the replay harness already reads. With no readable session file it
/// skips instead of failing — host isolation is not a test failure — and
/// `POLL_SESSIONS` points it at sessions the default roots miss.
#[test]
fn mine_real_sessions_when_readable() {
    let roots = session_roots();
    let files: Vec<PathBuf> = roots.iter().flat_map(|root| session_files(root)).collect();
    if files.is_empty() {
        println!(
            "no readable session file under {roots:?}: log mining skipped (set POLL_SESSIONS)"
        );
        return;
    }
    let out = std::env::var_os("POLL_MINE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(REAL_CORPUS));
    let mut tally = Tally::default();
    let mut corpus = String::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            println!("  unreadable, skipped: {}", file.display());
            continue;
        };
        tally.sessions += 1;
        let session = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("session");
        let (turns, bad) = mine_session(&text);
        tally.bad_lines += bad;
        for turn in &turns {
            tally.turns += 1;
            tally.calls += turn.calls.len();
            tally.batched_rounds += turn.batched_rounds;
            for call in &turn.calls {
                match &call.result {
                    Some(text) => {
                        tally.with_result += 1;
                        if truncate(text, MAX_RESULT_CHARS).1 {
                            tally.truncated_results += 1;
                        }
                    }
                    None => tally.without_result += 1,
                }
                if call.error {
                    tally.error_calls += 1;
                }
            }
            tally.dropped.0 += turn.synthetic;
            tally.dropped.1 += turn.guard_answered;
            tally.dropped.2 += turn.unanswered;
            if turn.calls.len() >= 3 {
                tally.turns_with_three_calls += 1;
            }
            if turn.reasoning_messages > 0 {
                tally.turns_with_reasoning += 1;
            }
            corpus.push_str(
                &turn
                    .to_json(session, &file.display().to_string())
                    .to_string(),
            );
            corpus.push('\n');
        }
    }
    // Count firings by replaying the WRITTEN text, so the printed numbers are
    // exactly what the corpus file produces.
    for fired in replay::fired_per_episode(&corpus) {
        match fired {
            Some(detector) => *tally.fired.entry(detector).or_default() += 1,
            None => tally.silent += 1,
        }
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create corpus directory");
    }
    std::fs::write(&out, &corpus).expect("write corpus");
    tally.print(Some((&out, corpus.len())));
}

#[cfg(test)]
mod extraction_tests {
    use super::*;
    use crate::agent::{AssistantMessage, ToolCall};

    /// A user prompt entry.
    fn prompt(text: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::User {
                content: text.to_owned(),
                images: Vec::new(),
            },
        }
    }

    /// An assistant round: its reasoning and its tool calls.
    fn round(reasoning: Option<&str>, calls: &[(&str, &str)]) -> SessionEntry {
        SessionEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: calls
                    .iter()
                    .enumerate()
                    .map(|(index, (name, arguments))| ToolCall {
                        id: format!("call_{index}"),
                        name: (*name).to_owned(),
                        arguments: (*arguments).to_owned(),
                    })
                    .collect(),
                reasoning: reasoning.map(str::to_owned),
            }),
        }
    }

    /// A committed tool result for `call_n` of its round.
    fn result(call: &str, name: &str, content: &str, is_error: bool) -> SessionEntry {
        SessionEntry::Message {
            message: Message::Tool {
                call_id: call.to_owned(),
                name: name.to_owned(),
                content: content.to_owned(),
                images: Vec::new(),
                is_error,
                synthetic: false,
            },
        }
    }

    /// The JSONL text the agent itself would write for these entries, so the
    /// test exercises the real on-disk format, not a hand-rolled imitation.
    fn session_text(entries: &[SessionEntry]) -> String {
        entries
            .iter()
            .map(|entry| serde_json::to_string(entry).expect("serialize entry"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One realistic session: a control turn plus one turn per detector, one
    /// unobservable turn, and the dropped-call bookkeeping.
    fn fixture() -> String {
        // Sized for the whole fixture: one entry per line of a real session.
        let mut entries = Vec::with_capacity(48);

        // Turn 1 — real work, with the batch-level background reminder the
        // loop applies after a batch that started background work.
        entries.push(prompt("build it"));
        entries.push(round(
            None,
            &[
                ("write_file", r#"{"path":"a.rs","content":"x"}"#),
                ("bash", r#"{"command":"cargo build 2>&1"}"#),
            ],
        ));
        entries.push(result("call_0", "write_file", "wrote a.rs", false));
        entries.push(result("call_1", "bash", "Compiling", false));
        entries.push(SessionEntry::Notice {
            text: BACKGROUND_REMINDER.to_owned(),
        });

        // Turn 2 — detector A: the same poll, byte-identical result, with a
        // background completion injected between the two rounds.
        entries.push(prompt("poll it"));
        entries.push(round(None, &[("get_background_tasks", "{}")]));
        entries.push(result(
            "call_0",
            "get_background_tasks",
            "#1: build (bash)",
            false,
        ));
        entries.push(SessionEntry::BackgroundCompletion {
            id: 1,
            output: "build ok".to_owned(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
            cancellation_source: None,
        });
        entries.push(round(None, &[("get_background_tasks", "{}")]));
        entries.push(result(
            "call_0",
            "get_background_tasks",
            "#1: build (bash)",
            false,
        ));

        // Turn 3 — detector C: two sleeps that differ by their number.
        entries.push(prompt("wait a bit"));
        entries.push(round(None, &[("bash", r#"{"command":"sleep 5"}"#)]));
        entries.push(result("call_0", "bash", "", false));
        entries.push(round(None, &[("bash", r#"{"command":"sleep 10"}"#)]));
        entries.push(result("call_0", "bash", "", false));

        // Turn 4 — detector D: three do-nothing commands.
        entries.push(prompt("look around"));
        entries.push(round(None, &[("bash", r#"{"command":"echo hi"}"#)]));
        entries.push(result("call_0", "bash", "hi", false));
        entries.push(round(None, &[("bash", r#"{"command":"date"}"#)]));
        entries.push(result("call_0", "bash", "Mon Nov 24", false));
        entries.push(round(None, &[("bash", r#"{"command":"pwd"}"#)]));
        entries.push(result("call_0", "bash", "/w", false));

        // Turn 5 — detector B: three calls of the same digit-normalised shape
        // whose terse results echo the counter.
        entries.push(prompt("wait for it"));
        for counter in 1..=3 {
            entries.push(round(
                None,
                &[("bash", &format!(r#"{{"command":"echo wait{counter}"}}"#))],
            ));
            entries.push(result("call_0", "bash", &format!("wait{counter}"), false));
        }

        // Turn 6 — detector E: two consecutive messages narrating a wait,
        // separated by nothing but a poll and a sleep.
        entries.push(prompt("is it done"));
        entries.push(round(
            Some("I'll start it and then wait; waiting is cheap."),
            &[("get_background_tasks", "{}")],
        ));
        entries.push(result(
            "call_0",
            "get_background_tasks",
            "#1: build (bash)",
            false,
        ));
        entries.push(round(
            Some("still waiting. I will wait for the completion."),
            &[("bash", r#"{"command":"sleep 5"}"#)],
        ));
        entries.push(result("call_0", "bash", "", false));

        // Turn 7 — no call the detectors ever observed: a synthetic
        // interrupted placeholder, a guard-answered poll, and an unanswered
        // call.
        entries.push(prompt("try again"));
        entries.push(round(
            None,
            &[
                ("bash", r#"{"command":"sleep 1"}"#),
                ("get_background_tasks", "{}"),
                ("bash", r#"{"command":"ls"}"#),
            ],
        ));
        entries.push(SessionEntry::Message {
            message: Message::Tool {
                call_id: "call_0".to_owned(),
                name: "bash".to_owned(),
                content: "[turn interrupted before a tool result was produced]".to_owned(),
                images: Vec::new(),
                is_error: true,
                synthetic: true,
            },
        });
        entries.push(result(
            "call_1",
            "get_background_tasks",
            POLL_GUARD_ERROR,
            true,
        ));

        session_text(&entries)
    }

    #[test]
    fn mines_the_session_format_into_a_replay_corpus() {
        let text = fixture();
        let (turns, bad) = mine_session(&text);
        assert_eq!(bad, 0, "every fixture line is a SessionEntry");
        assert_eq!(turns.len(), 7, "one episode per turn that ran a tool round");

        // Turn 1: the control turn, with the batch-level background flag.
        assert_eq!(turns[0].calls.len(), 2);
        assert!(turns[0].calls.iter().all(|call| call.background_started));
        assert_eq!(turns[0].reasoning_messages, 0);

        // Turn 2: the completion is consumed by the round after it.
        assert!(!turns[1].calls[0].completion);
        assert!(
            turns[1].calls[1].completion,
            "round 2 follows the completion"
        );
        assert_eq!(turns[1].completions, 1);

        // Turn 7: nothing was observed, and every drop is accounted for.
        let dropped = &turns[6];
        assert!(dropped.calls.is_empty(), "no unobserved call is written");
        assert_eq!(
            (
                dropped.synthetic,
                dropped.guard_answered,
                dropped.unanswered
            ),
            (1, 1, 1)
        );

        // Reasoning is credited only to rounds that ran tools, and stays
        // aligned with `calls`.
        assert_eq!(turns[5].reasoning.len(), turns[5].calls.len());
        assert!(turns[5].reasoning[0].contains("wait"));
        assert!(!turns[0].reasoning.iter().any(|text| !text.is_empty()));

        // Every line is unscored and carries its provenance.
        let corpus: Vec<Value> = turns
            .iter()
            .map(|turn| turn.to_json("fixture", "fixture.jsonl"))
            .collect();
        for episode in &corpus {
            assert_eq!(episode["label"], "unclear");
            assert_eq!(episode["variant"], "none");
            assert_eq!(episode["file"], "fixture.jsonl");
            let start = episode["seq_start"].as_u64().expect("seq_start");
            let end = episode["seq_end"].as_u64().expect("seq_end");
            assert!(start >= 1 && end >= start, "{start}..{end}");
        }

        // The replay this corpus feeds must show one firing per detector on
        // the matching turn, and silence elsewhere.
        let corpus_text: String = corpus
            .iter()
            .map(|episode| format!("{episode}\n"))
            .collect();
        let fired = replay::fired_per_episode(&corpus_text);
        assert_eq!(
            fired,
            vec![
                None,
                Some("A"),
                Some("C"),
                Some("D"),
                Some("B"),
                Some("E"),
                None
            ],
            "detector per turn"
        );
    }
}
