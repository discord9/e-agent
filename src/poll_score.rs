//! Advisory detectors for a turn spent waiting instead of ending.
//!
//! `get_background_tasks` renders only `#id: label (role)`, optional tags and
//! (for bash tasks) the full command — no output, no progress, no timestamps —
//! and a background completion is pushed into the next model call on its own,
//! so an identical second call cannot change the answer. The file keeps its
//! historical name; nothing here scores anything. Five BINARY detectors watch
//! finished tool calls and the current turn's reasoning, and the first one to
//! fire reports one [`Notice`]; the caller — the runner's turn loop
//! (`SessionRunner::run` in `src/runner.rs`), the one path a main session and
//! every subagent actually take — renders it as a single advisory
//! `SessionEntry::Notice` on the next request, through the session's one event
//! fanout. Nothing here edits a tool result, ends a turn, or keeps a score.
//!
//! | detector | trigger | fires on |
//! |----------|---------|----------|
//! | [`Notice::Repeat`]   | same tool + canonical arguments + identical result | 2nd call |
//! | [`Notice::Shape`]    | same digit-normalised `bash`/poll shape AND result | 3rd call |
//! | [`Notice::Sleep`]    | `bash` whose first command is `sleep`              | 2nd in a turn |
//! | [`Notice::Noop`]     | `bash` whose first command is a no-op (no `|`/`>`)  | 3rd in a turn |
//! | [`Notice::WaitTalk`] | consecutive assistant messages repeating `wait`     | 2nd message in a turn |
//! | [`Notice::SameError`]| same tool returning the same failure, any arguments   | 3rd call in a turn |
//!
//! Two of them are deliberately narrow, because a shape that looks like a poll
//! is often real work: [`Notice::Shape`] also requires the RESULTS to match (a
//! numeric item selector such as `./bench --size 100/200/300` produces
//! different results, a poll counter does not), and [`Notice::WaitTalk`] also
//! requires the turn to be doing nothing but polling / shell no-ops (saying
//! "wait" while debugging a borrow error is prose, not waiting).
//!
//! Precedence, highest first: Repeat, Shape, Sleep, Noop, WaitTalk. A `sleep`
//! is also a no-op (so `Sleep` and `Noop` both accrue on the same call), but
//! `Sleep` fires one call earlier and wins there. Shape never overlaps Repeat
//! on the same call: Repeat needs (tool, arguments, result) to be identical and
//! a `bash` shape match needs the arguments to differ by more than key order.
//! The once-per-turn latch ([`PollWatch::arm`]) caps delivery at one notice per
//! turn, so two detectors can never produce two notices in the same turn.
//!
//! ## Who owns a repeated poll
//!
//! The shipped unchanged-snapshot poll guard already answers a repeated
//! `get_background_tasks` call (a reminder, then a termination), so the
//! detectors never observe a result that guard answered: one repetition gets
//! exactly one advisory, and it is the guard's. Everything else — repeated
//! `bash` polls, repeated failures, shell waits, waiting talk — is the
//! detectors' to report.
//!
//! ## Evidence, not excerpts
//!
//! A notice cites the events it observed — the round of the turn, the tool
//! name, and the digest of the call's canonical arguments ([`arguments_digest`])
//! — and never a copy or a clipped excerpt of tool output: the model already
//! holds the real result, so an excerpt could only misreport it. Because such a
//! citation names rounds, [`PollWatch::invalidate`] drops the whole chain when
//! those rounds stop existing in context (after a compaction) or the session
//! moves to another model.

use serde_json::Value;

/// Detector A fires on the FIRST repeat — the SECOND occurrence of the same
/// call. One identical pair is proof already: the second call returned
/// everything the first one did, so it cannot have changed the answer.
const REPEAT_AT: u32 = 1;

/// Detector B fires on the THIRD consecutive same-shape call. Calls whose
/// arguments differ (even only by a counter) can still be two genuinely
/// different steps, so it takes a third before the run is called thrash.
const SHAPE_AT: u32 = 3;

/// Detector C fires on the SECOND `sleep`-first command in a turn: one short
/// sleep is legitimate pacing, a second one is waiting instead of ending the
/// turn.
const SLEEP_AT: u32 = 2;

/// Detector D fires on the THIRD no-op command in a turn. D's set is wider than
/// C's (`date`, `echo`, `pwd`, …), so it needs a third occurrence; C, which
/// recognises the single clearest wait command, already fires at two.
const NOOP_AT: u32 = 3;

/// Detector E fires on the SECOND assistant message of a turn whose reasoning
/// carries the `wait` stem. One message is a stray thought, two are a habit.
const WAIT_TALK_AT: u32 = 2;

/// `wait` stems needed INSIDE one message to count as sustained waiting talk
/// (`wait`, `waits`, `waiting`, `waited`), so a single stray "wait" is ignored.
const WAIT_STEMS_PER_MESSAGE: usize = 2;

/// Detector F fires on the THIRD consecutive call that comes back with the same
/// failure from the same tool. The first two can be legitimate probing (a wrong
/// path followed by a corrected one); by the third, a different argument has
/// already been shown not to change the reason.
const SAME_ERROR_AT: u32 = 3;

/// Detector B compares results, not just command shapes: a result at most this
/// many bytes only has to match with its digits erased (a poll echoes the
/// counter back — `wait1`/`wait2`), while anything longer must match exactly,
/// because a long output that differs even in its digits is a different run
/// (`./bench --size 100` reports its own timings), not the same answer again.
const NEAR_RESULT_BYTES: usize = 120;

/// Compact, key-sorted JSON: object keys sorted recursively, array order
/// preserved, no whitespace. Two calls are "the same call" exactly when their
/// canonical forms match.
pub fn canonicalize_arguments(value: &Value) -> String {
    fn push(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                out.push('{');
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    // Reuse serde's string escaping so unicode and quotes
                    // canonicalize exactly like the rest of the wire format.
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    push(&map[key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    push(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    push(value, &mut out);
    out
}

/// First 16 hex chars of the sha256 of a result text, so comparing results
/// costs constant space even for a megabyte-sized tool output.
fn digest(text: &str) -> String {
    crate::agent::image_sha256(text.as_bytes())
        .chars()
        .take(16)
        .collect()
}

/// The name of a call's arguments in a notice citation: the first 16 hex chars
/// of the sha256 of their canonical form, so a notice can point at the exact
/// arguments it observed without quoting them (or any tool output) back to the
/// model.
pub(crate) fn arguments_digest(arguments: &str) -> String {
    let canonical = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => canonicalize_arguments(&value),
        Err(_) => arguments.to_owned(),
    };
    digest(&canonical)
}

/// One observed call, reduced to what "the same call" means (detector A).
#[derive(PartialEq, Eq)]
struct Call {
    tool: String,
    arguments: String,
    digest: String,
}

/// Detector B's notion of "the same shape": the tool, its digit-normalised
/// command, and the result fingerprint (see [`result_fingerprint`]). The result
/// belongs in the key because calls with alike command lines are only a repeat
/// when they also come back with alike answers.
#[derive(PartialEq, Eq)]
struct Shape {
    tool: String,
    command: String,
    result: String,
}

/// Which detector reported the advisory notice. The caller maps it to the
/// constant text that describes what was actually observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// A: the immediately preceding call was the same call.
    Repeat,
    /// B: three consecutive calls of the same digit-normalised shape whose
    /// results match too.
    Shape,
    /// C: a second `sleep`-first command this turn.
    Sleep,
    /// D: a third do-nothing command this turn (no pipeline, no redirect).
    Noop,
    /// E: a second consecutive assistant message that talks about waiting
    /// while the turn does nothing but poll and no-op.
    WaitTalk,
    /// F: the same tool returned the same failure again, whatever the
    /// arguments — retrying with another input changed nothing.
    SameError,
}

/// Consecutive-call and per-turn wait detectors for one session.
///
/// The chains (A, B) are a property of the tool-call sequence and survive a
/// turn boundary; the counters (C, D, E) and the arming latch are per turn and
/// reset in [`Self::start_turn`].
#[derive(Default)]
pub struct PollWatch {
    /// A: the previous call and how many times it has just repeated.
    previous: Option<Call>,
    repeats: u32,
    /// B: the previous shape (command and result) and how many consecutive
    /// calls share it.
    shape: Option<Shape>,
    shapes: u32,
    /// C, D: per-turn counters.
    sleeps: u32,
    noops: u32,
    /// E: consecutive assistant messages whose reasoning repeats the wait stem.
    waits: u32,
    /// E: one of those messages was followed by nothing but polling / shell
    /// no-ops. Waiting talk on its own is prose; it needs a wait-shaped turn.
    wait_activity: bool,
    /// E: the reasoning text of the last message seen, so a string batched
    /// rounds deliver twice is counted once.
    last_reasoning: Option<String>,
    /// F: the previous failed call (tool, digit-normalised error text) and how
    /// many consecutive calls share that failure.
    error: Option<(String, String)>,
    errors: u32,
    /// The shared once-per-turn latch: the first detector to fire owns the
    /// turn's single notice.
    armed: bool,
    /// Digest of the previous `get_background_tasks` result, so a changed
    /// running-task set is recognised as progress.
    poll: Option<String>,
    /// The round of the current turn, counted by the runner's turn loop. Only
    /// used to cite the event a notice refers to.
    round: usize,
}

impl PollWatch {
    /// The next round of this turn starts; returns its 1-based index for the
    /// notice citation. Called once per provider request by the runner's turn
    /// loop.
    pub fn begin_round(&mut self) -> usize {
        self.round = self.round.saturating_add(1);
        self.round
    }

    /// Drop ALL state: the chains cite rounds of the context, and the context
    /// that held them is gone. Called after a compaction and when the session
    /// switches to another model, so a notice can never cite rounds that no
    /// longer exist or claim a repeat across a rewritten history.
    pub fn invalidate(&mut self) {
        *self = Self::default();
    }

    /// A new turn: the per-turn counters and the arming latch start over. The
    /// repeat and shape chains do NOT: a turn boundary is not evidence that the
    /// calls themselves changed.
    pub fn start_turn(&mut self) {
        self.armed = false;
        self.round = 0;
        self.sleeps = 0;
        self.noops = 0;
        self.waits = 0;
        self.wait_activity = false;
        // A new turn's reasoning is a new message even if the text repeats.
        self.last_reasoning = None;
        self.error = None;
        self.errors = 0;
    }

    /// A background completion was injected at a model-call boundary: waiting
    /// did produce something, so the per-turn wait counters start over.
    pub fn background_completed(&mut self) {
        self.sleeps = 0;
        self.noops = 0;
        self.waits = 0;
        self.wait_activity = false;
        self.error = None;
        self.errors = 0;
    }

    /// Detector E, called once per assistant message with its reasoning text.
    ///
    /// Waiting talk on its own proves nothing: "Wait, that is the wrong borrow"
    /// is reasoning about a mistake. E therefore wants TWO CONSECUTIVE messages
    /// that repeat the stem, no real progress between them (any write/edit, or
    /// a real command, clears the streak in [`Self::observe`]), and at least one
    /// of them followed by nothing but polling / shell no-ops
    /// (`wait_activity`).
    pub fn reasoning(&mut self, reasoning: Option<&str>) -> Option<Notice> {
        // The same reasoning string twice in a row is ONE message that batched
        // rounds projected twice, not two thoughts: it neither adds to the
        // streak nor breaks it.
        if let (Some(last), Some(text)) = (self.last_reasoning.as_deref(), reasoning)
            && last == text
        {
            return None;
        }
        self.last_reasoning = reasoning.map(str::to_owned);
        if !reasoning.is_some_and(|text| wait_stems(text) >= WAIT_STEMS_PER_MESSAGE) {
            // A message without the habit in it breaks the streak: two
            // messages ANYWHERE in the turn are not two CONSECUTIVE ones.
            self.waits = 0;
            self.wait_activity = false;
            return None;
        }
        self.waits = self.waits.saturating_add(1);
        if self.waits >= WAIT_TALK_AT && self.wait_activity {
            return self.arm(Some(Notice::WaitTalk));
        }
        None
    }

    /// Detectors A–D, called once per finished tool call.
    ///
    /// `background_started` is [`crate::agent::ToolOutput::background_started`]:
    /// a call that launched background work is progress, whatever it looked
    /// like, and it never counts as a blocking wait. `failed` is the
    /// `Result::is_err` of the call, which detector F needs to tell a repeated
    /// FAILURE from a repeated answer.
    pub fn observe(
        &mut self,
        tool: &str,
        arguments: &str,
        result: &str,
        background_started: bool,
        failed: bool,
    ) -> Option<Notice> {
        let parsed: Option<Value> = serde_json::from_str(arguments).ok();
        let canonical = match parsed.as_ref() {
            Some(value) => canonicalize_arguments(value),
            // Not JSON (a malformed call): compare verbatim. A canonical form
            // is always valid JSON, so the two can never collide.
            None => arguments.to_owned(),
        };
        let command = parsed
            .as_ref()
            .and_then(|value| value.get("command"))
            .and_then(Value::as_str);

        let result_digest = digest(result);
        // An observed side effect: work started outside the shell, a workspace
        // write, or a running-task set that actually moved.
        let tasks_changed = tool == "get_background_tasks"
            && self
                .poll
                .as_ref()
                .is_some_and(|last| last != &result_digest);
        if tool == "get_background_tasks" {
            self.poll = Some(result_digest.clone());
        }
        let side_effect = background_started || mutating_tool(tool) || tasks_changed;
        // Wait counters need a higher bar than the shape chain: a real shell
        // command is work too, even though it is not a "side effect" for
        // detector B. A command that merely WAITS in the shell is not work,
        // even when more follows it (`sleep 30; echo ok`), while a compound
        // command that does something real (`echo …; cat -n f`) is.
        let shell_wait =
            command.is_some_and(|command| is_sleep_wait(command) || noop_command(command));
        let worked = side_effect || (command.is_some() && !shell_wait);

        // A: the same call as the immediately preceding one.
        let same_call = self.previous.as_ref().is_some_and(|previous| {
            previous.tool == tool
                && previous.arguments == canonical
                && previous.digest == result_digest
        });
        self.repeats = if same_call {
            self.repeats.saturating_add(1)
        } else {
            0
        };
        self.previous = Some(Call {
            tool: tool.to_owned(),
            arguments: canonical,
            digest: result_digest.clone(),
        });

        // B: only polling-prone tools. A file tool is NEVER here — reading five
        // different files in a row is ordinary work, not thrash. The result is
        // part of the shape: a numeric item selector (`--size 100/200/300`, or
        // three different numbered test suites) makes the calls do different
        // work, while a poll counter comes back with the same answer.
        let shape = polling_tool(tool).then(|| Shape {
            tool: tool.to_owned(),
            command: command.map(shape_of).unwrap_or_default(),
            result: result_fingerprint(result, &result_digest),
        });
        self.shapes = if side_effect || shape.is_none() || self.shape != shape {
            1
        } else {
            self.shapes.saturating_add(1)
        };
        self.shape = shape;

        // C, D: shell-only waiting. `sleep` is a no-op too, so both counters
        // accrue on it; C just fires first.
        if worked {
            self.sleeps = 0;
            self.noops = 0;
            self.waits = 0;
            self.wait_activity = false;
            self.error = None;
            self.errors = 0;
        }
        // A launch returns immediately: `bash background: true` with a `sleep`
        // inside is not a blocking wait, so it never feeds C.
        let sleep_wait =
            tool == "bash" && !background_started && command.is_some_and(is_sleep_wait);
        let noop = tool == "bash" && command.is_some_and(noop_command);
        if sleep_wait {
            self.sleeps = self.sleeps.saturating_add(1);
        }
        if noop {
            self.noops = self.noops.saturating_add(1);
        }
        // E's corroboration: while a wait-talk streak is running, remember
        // whether the turn is really doing nothing but polling and shell
        // no-ops. `worked` above already ended the streak on real progress, so
        // a call that reaches here with a streak alive made no progress either.
        if self.waits > 0 && (tool == "get_background_tasks" || sleep_wait || noop) {
            self.wait_activity = true;
        }

        // F: the same FAILURE from the same tool, whatever the arguments. The
        // error text is normalised like B's commands (digits erased, whitespace
        // collapsed), so `os error 2` and `os error 3` are different failures
        // while the same message with a different path is the same one.
        let failure = failed.then(|| (tool.to_owned(), error_shape(result)));
        self.errors = match (&failure, &self.error) {
            (Some(current), Some(previous)) if current == previous => self.errors.saturating_add(1),
            (Some(_), _) => 1,
            (None, _) => 0,
        };
        self.error = failure;

        // Precedence: the most literal evidence wins, and only one notice can
        // be armed per turn.
        let notice = if self.repeats == REPEAT_AT {
            Some(Notice::Repeat)
        } else if self.shapes == SHAPE_AT && polling_tool(tool) {
            Some(Notice::Shape)
        } else if sleep_wait && self.sleeps == SLEEP_AT {
            Some(Notice::Sleep)
        } else if self.noops == NOOP_AT {
            Some(Notice::Noop)
        } else if self.errors == SAME_ERROR_AT {
            Some(Notice::SameError)
        } else {
            None
        };
        self.arm(notice)
    }

    /// The shared once-per-turn latch: whichever detector fires first wins, so
    /// two detectors never produce two notices in one turn.
    fn arm(&mut self, notice: Option<Notice>) -> Option<Notice> {
        match notice {
            Some(notice) if !self.armed => {
                self.armed = true;
                Some(notice)
            }
            _ => None,
        }
    }
}

/// Tools whose success changes the workspace, so a run of polling-shaped calls
/// around them is not polling.
fn mutating_tool(tool: &str) -> bool {
    matches!(tool, "write_file" | "edit_file")
}

/// Tools detector B covers: the ones this agent polls with. A file tool is
/// deliberately absent (five different reads in a row must never fire).
fn polling_tool(tool: &str) -> bool {
    matches!(tool, "bash" | "get_background_tasks")
}

/// Byte index just past the first `&&`, `;` or `|` separator, if any.
fn next_separator(text: &str) -> Option<usize> {
    let and = text.find("&&").map(|index| index + 2);
    let semi = text.find(';').map(|index| index + 1);
    let pipe = text.find('|').map(|index| index + 1);
    and.into_iter().chain(semi).chain(pipe).min()
}

/// The head word of the command's FIRST simple command, after stripping a
/// leading `cd …` workspace switch and any `env NAME=value …` prefix:
/// `cd /x && sleep 30` → `sleep`, `env A=1 sleep 30` → `sleep`,
/// `make test && sleep 1` → `make`.
fn head_word(command: &str) -> Option<&str> {
    let mut rest = strip_cd_prefix(command);
    loop {
        let word = rest.split_whitespace().next()?;
        match word {
            "env" => {
                rest = rest.get("env".len()..)?.trim_start();
                while let Some(word) = rest.split_whitespace().next() {
                    if !word.contains('=') {
                        break;
                    }
                    rest = rest.get(word.len()..)?.trim_start();
                }
            }
            other => return Some(other),
        }
    }
}

/// Detector C's classifier: the first simple command is `sleep`. A command
/// that merely CONTAINS a sleep later (`make test && sleep 1`) is real work,
/// not waiting.
fn is_sleep_wait(command: &str) -> bool {
    head_word(command) == Some("sleep")
}

/// The command with any leading `cd … &&` / `cd … ;` workspace switch removed:
/// `cd /x && sleep 30` → `sleep 30`. A bare `cd …` with nothing after it is
/// left alone (it is not a command with an observable effect either way).
fn strip_cd_prefix(command: &str) -> &str {
    let mut rest = command.trim_start();
    loop {
        if rest.split_whitespace().next() != Some("cd") {
            return rest;
        }
        match next_separator(rest) {
            Some(end) => rest = rest.get(end..).map(str::trim_start).unwrap_or_default(),
            None => return rest,
        }
    }
}

/// Detector D's classifier: ONE bare simple command that changes nothing
/// observable — no workspace write, no task state, no other program. Anything
/// compound is real work: a single `echo` in front of `cat -n file`, a pipeline
/// into `./confirm.sh`, or a redirect all run or write something, so only a
/// lone `echo …` / `printf …` / `date` / `true` / `:` / `pwd` / `whoami` /
/// `sleep N` counts.
fn noop_command(command: &str) -> bool {
    let alone = strip_cd_prefix(command);
    if compound_command(alone) {
        return false;
    }
    matches!(
        head_word(alone),
        Some("echo" | "date" | "true" | ":" | "pwd" | "whoami" | "sleep" | "printf")
    )
}

/// True when the command is more than one simple command: an unquoted `;`,
/// `&` (so `&&` too), `|` (so `||` too), `>` (so `>>` too) or a newline.
/// Quoted operators (`printf 'a > b'`, `echo "HOME=$HOME"`) are ordinary text.
fn compound_command(command: &str) -> bool {
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match ch {
            '\'' | '"' => {
                quote = match quote {
                    Some(open) if open == ch => None,
                    None => Some(ch),
                    open => open,
                };
            }
            ';' | '&' | '|' | '>' | '\n' | '\r' if quote.is_none() => return true,
            _ => {}
        }
    }
    false
}

/// Detector B's shape: every run of ASCII digits becomes `#`, whitespace runs
/// collapse to one space, ends are trimmed. `echo wait1` and `echo wait2` both
/// shape as `echo wait#`.
fn shape_of(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut in_digits = false;
    for ch in command.trim().chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
            continue;
        }
        in_digits = false;
        if ch.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    out.trim_end().to_owned()
}

/// Detector F's error fingerprint: whitespace collapsed and ends trimmed, so the
/// same failure reported with a different layout is the same failure. Digits are
/// deliberately KEPT: `os error 2` and `os error 3` are different failures, and
/// erasing them would merge an errno change into a repeat.
fn error_shape(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Detector B's result fingerprint: a SHORT result is compared with its digits
/// erased (a poll echoes the counter back — `wait1`, `wait2`), while a longer
/// one must match exactly, because a long output that differs even in its digits
/// is a different run, not the same answer repeated.
fn result_fingerprint(result: &str, digest: &str) -> String {
    if result.len() <= NEAR_RESULT_BYTES {
        format!("n:{}", shape_of(result))
    } else {
        format!("h:{digest}")
    }
}

/// Detector E's classifier: how often `wait` appears as a word stem (`wait`,
/// `waits`, `waiting`, `waited`), case-insensitively.
fn wait_stems(text: &str) -> usize {
    /// True when `text` starts a new word (end of string counts too).
    fn word_end(text: &str) -> bool {
        match text.chars().next() {
            Some(ch) => !ch.is_alphanumeric(),
            None => true,
        }
    }
    let lower = text.to_ascii_lowercase();
    lower
        .match_indices("wait")
        .filter(|(index, _)| {
            let before_ok = match lower[..*index].chars().next_back() {
                Some(ch) => !ch.is_alphanumeric(),
                None => true,
            };
            let rest = &lower[*index + "wait".len()..];
            // `waiter` is not waiting talk: only the inflections of the stem
            // count, plus any word that simply ends there (`wait.`).
            let mut after_ok = word_end(rest);
            for suffix in ["s", "ing", "ed"] {
                if let Some(after) = rest.strip_prefix(suffix) {
                    after_ok |= word_end(after);
                }
            }
            before_ok && after_ok
        })
        .count()
}

#[cfg(test)]
mod mine;
#[cfg(test)]
mod replay;
#[cfg(test)]
mod tests;
