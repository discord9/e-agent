use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::*;
use crate::agent::{
    Agent, AgentEvent, AssistantMessage, Message, Model, ModelDeltaKind, POLL_GUARD_ERROR,
    REPEAT_CALL_NOTICE, REPEAT_SHAPE_NOTICE, SessionEntry, Tool, ToolCall, ToolOutput, ToolSpec,
    Usage, WAIT_COMMAND_NOTICE, WAIT_TALK_NOTICE,
};

/// One `get_background_tasks` snapshot, exactly as the tool renders it.
const SNAPSHOT: &str = "1 background task(s) running:\n#1: build (bash)";
/// The same list after `#1` finished: the poll DID learn something.
const FINISHED: &str = "No background tasks running.";

/// One finished call, no background task started, no failure.
fn call(watch: &mut PollWatch, tool: &str, arguments: &str, result: &str) -> Option<Notice> {
    watch.observe(tool, arguments, result, false, false)
}

/// One finished call that FAILED, no background task started.
fn failed(watch: &mut PollWatch, tool: &str, arguments: &str, result: &str) -> Option<Notice> {
    watch.observe(tool, arguments, result, false, true)
}

/// One `bash` call that launched a background task.
fn bash_launch(watch: &mut PollWatch, command: &str, result: &str) -> Option<Notice> {
    watch.observe(
        "bash",
        &json!({ "command": command, "background": true }).to_string(),
        result,
        true,
        false,
    )
}

/// One finished `bash` call with this command.
fn bash(watch: &mut PollWatch, command: &str, result: &str) -> Option<Notice> {
    call(
        watch,
        "bash",
        &json!({ "command": command }).to_string(),
        result,
    )
}

/// Detector A: the second identical call in a row arms exactly one notice; the
/// rest of the group stays quiet.
#[test]
fn a_fires_on_the_first_repeat_and_only_once_per_group() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        None
    );
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        Some(Notice::Repeat)
    );
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        None
    );
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        None
    );
}

/// A different tool or different arguments breaks the chain, and the next
/// chain's first repeat arms its own notice (one notice per turn, so the new
/// chain is observed in a new turn).
#[test]
fn a_broken_chain_lets_the_next_chain_arm_its_own_notice() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    call(&mut watch, "get_background_tasks", "{}", SNAPSHOT);
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        Some(Notice::Repeat)
    );
    // Different tool: the chain is broken.
    assert_eq!(
        call(&mut watch, "read_file", r#"{"path":"a.txt"}"#, "hi"),
        None
    );
    // Different arguments: no chain either, even for the polling tool.
    assert_eq!(
        call(
            &mut watch,
            "get_background_tasks",
            r#"{"limit":1}"#,
            SNAPSHOT
        ),
        None
    );
    assert_eq!(
        call(
            &mut watch,
            "get_background_tasks",
            r#"{"limit":2}"#,
            SNAPSHOT
        ),
        None
    );
    // A new turn, and a fresh shape chain (a read in between), so the polling
    // tool's new chain reaches its own first repeat.
    watch.start_turn();
    assert_eq!(
        call(&mut watch, "read_file", r#"{"path":"b.txt"}"#, "ho"),
        None
    );
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        None
    );
    assert_eq!(
        call(&mut watch, "get_background_tasks", "{}", SNAPSHOT),
        Some(Notice::Repeat)
    );
}

/// Same tool, same arguments, DIFFERENT result (a task finished between the
/// polls) is polling that learns something: no notice, however long it runs.
#[test]
fn a_same_arguments_but_a_changed_task_list_never_arms() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    for result in [SNAPSHOT, FINISHED, SNAPSHOT, FINISHED] {
        assert_eq!(call(&mut watch, "get_background_tasks", "{}", result), None);
    }
}

/// Argument key order is irrelevant to "the same call"; whitespace is too.
#[test]
fn a_reordered_arguments_are_the_same_call() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        call(&mut watch, "echo", r#"{"path":"a","limit":5}"#, "text"),
        None
    );
    assert_eq!(
        call(&mut watch, "echo", r#"{ "limit": 5, "path": "a" }"#, "text"),
        Some(Notice::Repeat)
    );
}

/// Object key order is irrelevant, array order is not, and the canonical form
/// is compact with unicode passed through.
#[test]
fn canonicalisation_sorts_keys_and_preserves_array_order() {
    let one = json!({"b": {"z": 1, "a": [1, 2]}, "a": "héllo 世界", "c": null});
    let other = json!({"c": null, "a": "héllo 世界", "b": {"a": [1, 2], "z": 1}});
    assert_eq!(canonicalize_arguments(&one), canonicalize_arguments(&other));
    assert_eq!(
        canonicalize_arguments(&one),
        r#"{"a":"héllo 世界","b":{"a":[1,2],"z":1},"c":null}"#
    );
    // Compact: no whitespace is introduced between tokens.
    assert_eq!(
        canonicalize_arguments(&json!({"a": 1, "b": [1, 2]})),
        r#"{"a":1,"b":[1,2]}"#
    );
    // Arrays keep their order: the same elements swapped are different calls.
    assert_ne!(
        canonicalize_arguments(&json!({"a": [1, 2]})),
        canonicalize_arguments(&json!({"a": [2, 1]}))
    );
    // Sorting is recursive, not just at the top level.
    assert_eq!(
        canonicalize_arguments(&json!({"m": {"y": {"b": 1, "a": 2}}})),
        r#"{"m":{"y":{"a":2,"b":1}}}"#
    );
}

/// Detector B: three consecutive calls whose commands differ only by a counter
/// shape as one command, so the third one arms.
#[test]
fn b_fires_on_the_third_same_shape_bash_call() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "echo wait1", "wait1"), None);
    assert_eq!(bash(&mut watch, "echo wait2", "wait2"), None);
    assert_eq!(bash(&mut watch, "echo wait3", "wait3"), Some(Notice::Shape));
    // The chain keeps counting, so the same shape run gets no second notice.
    assert_eq!(bash(&mut watch, "echo wait4", "wait4"), None);
}

/// A workspace write between the polls is progress, so the shape chain starts
/// over instead of reaching three.
#[test]
fn b_stays_silent_when_a_write_between_the_polls_made_progress() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "echo wait1", "wait1"), None);
    assert_eq!(
        call(
            &mut watch,
            "write_file",
            &json!({"path": "x.txt", "content": "y"}).to_string(),
            "wrote x.txt"
        ),
        None
    );
    assert_eq!(bash(&mut watch, "echo wait2", "wait2"), None);
    assert_eq!(bash(&mut watch, "echo wait3", "wait3"), None);
}

/// A numeric item selector is not a poll counter: a sweep that reports its own
/// full results keeps coming back with a different answer, so B stays silent.
#[test]
fn b_ignores_a_numeric_sweep_that_reports_full_results() {
    let report = |size: &str, mean: &str, allocs: &str| {
        format!(
            "size={size}\n  iterations: 200\n  mean: {mean}\n  stddev: 0.8ms\n  p50: 12.1ms\n  p99: 14.8ms\n  throughput: 8.1 Mops/s\n  allocations: {allocs}\n  peak memory: 1.2MB\n  status: ok"
        )
    };
    let mut watch = PollWatch::default();
    watch.start_turn();
    for (size, mean, allocs) in [
        ("100", "12.4ms", "3"),
        ("200", "48.2ms", "7"),
        ("300", "130.9ms", "0"),
    ] {
        let command = format!("./bench --size {size}");
        assert_eq!(
            bash(&mut watch, &command, &report(size, mean, allocs)),
            None
        );
    }
}

/// The same shape with the SAME answer still arms B, however long the answer
/// is (the result is part of the shape, not a veto).
#[test]
fn b_fires_when_the_long_results_match_exactly() {
    let report = "size=1\n  iterations: 200\n  mean: 12.4ms\n  stddev: 0.8ms\n  p50: 12.1ms\n  p99: 14.8ms\n  throughput: 8.1 Mops/s\n  allocations: 3\n  peak memory: 1.2MB\n  status: ok";
    assert!(
        report.len() > NEAR_RESULT_BYTES,
        "the exact-match branch needs a long result"
    );
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "./bench --size 1", report), None);
    assert_eq!(bash(&mut watch, "./bench --size 2", report), None);
    assert_eq!(
        bash(&mut watch, "./bench --size 3", report),
        Some(Notice::Shape)
    );
}

/// Documented limit, kept as a test so it cannot change silently: a result that
/// differs ONLY in its digits is the same answer as far as B can tell — which
/// is exactly what makes `echo wait1/2/3` a repeat. A terse numeric sweep looks
/// identical, so it still arms (see `probes.json` `u-terse-sweep-output`).
#[test]
fn b_treats_terse_digit_only_results_as_the_same_answer() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "./bench --size 100", "12ms"), None);
    assert_eq!(bash(&mut watch, "./bench --size 200", "48ms"), None);
    assert_eq!(
        bash(&mut watch, "./bench --size 300", "130ms"),
        Some(Notice::Shape)
    );
}

/// File tools are never shape-checked: reading five different files in a row is
/// ordinary work.
#[test]
fn b_never_covers_file_tools_with_different_paths() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    for (path, text) in [("a.txt", "a"), ("b.txt", "b"), ("c.txt", "c")] {
        let arguments = json!({ "path": path }).to_string();
        assert_eq!(call(&mut watch, "read_file", &arguments, text), None);
    }
}

/// Three consecutive `bash` calls with genuinely different commands are three
/// different steps, not a repeat.
#[test]
fn b_stays_silent_for_genuinely_different_commands() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "ls", "a b c"), None);
    assert_eq!(bash(&mut watch, "cat a", "hello"), None);
    assert_eq!(bash(&mut watch, "pwd", "/work"), None);
}

/// A poll that keeps learning something (the task set moves) is legitimate: no
/// detector fires, and the waiting counters reset on every change.
#[test]
fn polls_that_keep_learning_something_never_arm() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    for count in 2..=5 {
        let snapshot = format!("{count} background task(s) running:\n#1: build (bash)");
        assert_eq!(
            call(&mut watch, "get_background_tasks", "{}", &snapshot),
            None
        );
    }
}

/// Detector C: the second `sleep`-first command of a turn arms one notice.
#[test]
fn c_fires_on_the_second_sleep_of_a_turn() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "sleep 30; echo ok", "ok"), None);
    assert_eq!(
        bash(&mut watch, "sleep 30; git log -1 --oneline", "abc1234"),
        Some(Notice::Sleep)
    );
}

/// One short sleep is legitimate pacing.
#[test]
fn c_stays_silent_for_a_single_sleep() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "sleep 5 && curl -s url", "body"), None);
}

/// A sleep that is not the first command is work, not waiting (`make test` in
/// front of it), however often it repeats.
#[test]
fn c_ignores_a_sleep_that_is_not_the_first_command() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    // Two genuinely different real commands, so detector A has nothing to say
    // either: the sleep tail must not make C fire.
    assert_eq!(
        bash(&mut watch, "make test && sleep 1", "tests passed"),
        None
    );
    assert_eq!(bash(&mut watch, "make lint && sleep 1", "lint clean"), None);
}

/// Waiting that produced something is not waiting: an injected completion
/// resets the sleep count.
#[test]
fn c_resets_when_a_background_completion_arrives() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "sleep 30", "ok"), None);
    watch.background_completed();
    assert_eq!(bash(&mut watch, "sleep 45", "ok"), None);
}

/// Detector D: alternating no-op commands accumulate in ONE counter, so shapes
/// B cannot see still arm on the third call.
#[test]
fn d_fires_on_the_third_no_op_command_with_alternating_shapes() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "date -u +%H:%M:%S", "20:15:01"), None);
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    assert_eq!(
        bash(&mut watch, "date -u +%H:%M:%S", "20:15:09"),
        Some(Notice::Noop)
    );
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
}

/// A real command resets the no-op counter.
#[test]
fn d_resets_on_a_real_command() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    // The no-ops differ, so this is D's counter and not detector A's repeat.
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    assert_eq!(bash(&mut watch, "echo noop2", "noop2"), None);
    assert_eq!(bash(&mut watch, "cargo build", "compiled"), None);
    assert_eq!(bash(&mut watch, "echo noop3", "noop3"), None);
}

/// A pipeline runs a real program, so the head word `echo` does not make the
/// command a no-op.
#[test]
fn d_ignores_a_pipeline() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        bash(&mut watch, "echo y | ./scripts/confirm.sh", "confirmed"),
        None
    );
    assert_eq!(
        bash(&mut watch, "echo n | ./scripts/confirm.sh", "cancelled"),
        None
    );
    assert_eq!(
        bash(&mut watch, "echo y | ./scripts/confirm.sh", "confirmed"),
        None
    );
}

/// The live false positive: one `echo` in front of a real command. The head
/// word is `echo`, but the line does work, so D must stay silent.
#[test]
fn d_ignores_a_command_chain() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        bash(&mut watch, "echo start; cat -n a.py", "start\n1 import os"),
        None
    );
    assert_eq!(
        bash(&mut watch, "echo start; cat -n b.py", "start\n1 import sys"),
        None
    );
    assert_eq!(
        bash(&mut watch, "echo start; cat -n c.py", "start\n1 import re"),
        None
    );
}

/// A lone `echo` with a quoted variable is still a no-op: the operators only
/// count outside quotes.
#[test]
fn d_counts_a_bare_echo_with_a_quoted_variable() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        bash(&mut watch, "echo \"HOME=$HOME\"", "HOME=/home/x"),
        None
    );
    assert_eq!(bash(&mut watch, "echo \"PWD=$PWD\"", "PWD=/work"), None);
    assert_eq!(
        bash(&mut watch, "echo \"HOME=$HOME\"", "HOME=/home/x"),
        Some(Notice::Noop)
    );
}

/// A redirect writes a file, so it is not a no-op either.
#[test]
fn d_ignores_a_redirect() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "echo one > notes.txt", "wrote"), None);
    assert_eq!(bash(&mut watch, "echo two > notes.txt", "wrote"), None);
    assert_eq!(bash(&mut watch, "echo three >> notes.txt", "wrote"), None);
    // The operator only counts outside quotes: printing it is still a no-op.
    assert_eq!(bash(&mut watch, "printf 'a > b'", "a > b"), None);
}

/// A workspace write resets the no-op counter too.
#[test]
fn d_resets_when_a_write_file_lands_between_the_no_ops() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "date", "20:15"), None);
    assert_eq!(
        call(
            &mut watch,
            "edit_file",
            &json!({"path": "x.txt"}).to_string(),
            "edited x.txt"
        ),
        None
    );
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    assert_eq!(bash(&mut watch, "date", "20:16"), None);
}

/// Detector E: a second consecutive message whose reasoning repeats the wait
/// stem arms, provided the turn is doing nothing but shell no-ops. A single
/// stray "wait" per message never does.
#[test]
fn e_fires_on_the_second_wait_talk_message() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        watch.reasoning(Some("Wait. I should wait for the completion notice.")),
        None
    );
    // Corroboration: the round that message described did nothing but poll.
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    assert_eq!(
        watch.reasoning(Some("Wait. I will wait for the completion notice again.")),
        Some(Notice::WaitTalk)
    );
}

/// The interjection sense of "wait" is not waiting for a task: when the turn is
/// doing real work (reading code), E stays silent.
#[test]
fn e_stays_silent_when_the_turn_is_doing_real_work() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        watch.reasoning(Some("Wait, that is the wrong borrow. Wait, it is in a.rs.")),
        None
    );
    assert_eq!(
        call(&mut watch, "read_file", r#"{"path":"a.rs"}"#, "fn a() {}"),
        None
    );
    assert_eq!(
        watch.reasoning(Some(
            "Wait, the call site is wrong. Wait, no, it is the borrow."
        )),
        None
    );
}

/// Waiting talk without a wait-shaped turn is prose: consecutive messages with
/// no poll and no no-op among them never arm E.
#[test]
fn e_stays_silent_without_a_poll_or_noop_round() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    for talk in [
        "Wait. I will wait for the completion notice.",
        "Wait. I am still waiting for the completion notice.",
        "Wait. Waiting is all I can do.",
    ] {
        assert_eq!(watch.reasoning(Some(talk)), None);
    }
}

/// The same reasoning string twice in a row is ONE message that batched rounds
/// projected twice: it neither advances nor breaks the streak.
#[test]
fn e_ignores_a_duplicated_reasoning_string() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    let talk = "Wait. I will wait for the completion notice.";
    assert_eq!(watch.reasoning(Some(talk)), None);
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    // The duplicate counts once: still a streak of ONE message.
    assert_eq!(watch.reasoning(Some(talk)), None);
    assert_eq!(bash(&mut watch, "date", "20:15"), None);
    // The next genuinely different wait-narrating message is the second one.
    assert_eq!(
        watch.reasoning(Some("Wait. I am still waiting for it.")),
        Some(Notice::WaitTalk)
    );
}

/// Two different wait-narrating messages still fire on their own (the dedup is
/// about exact repeats, not about the waiting rule itself).
#[test]
fn e_fires_on_two_different_wait_messages() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        watch.reasoning(Some("Wait. I will wait for the completion notice.")),
        None
    );
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    assert_eq!(
        watch.reasoning(Some("Wait. I am still waiting for it.")),
        Some(Notice::WaitTalk)
    );
}

/// The messages must be CONSECUTIVE: an ordinary message in between breaks the
/// streak.
#[test]
fn e_needs_consecutive_wait_talk_messages() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        watch.reasoning(Some("Wait. I will wait for the notice.")),
        None
    );
    assert_eq!(bash(&mut watch, "echo noop", "noop"), None);
    assert_eq!(
        watch.reasoning(Some("Now let me look at the borrow check.")),
        None
    );
    assert_eq!(
        watch.reasoning(Some("Wait. I will wait for the notice again.")),
        None
    );
}

#[test]
fn e_ignores_a_single_wait_in_one_message() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(watch.reasoning(Some("Wait.")), None);
    assert_eq!(watch.reasoning(Some("I'll wait.")), None);
    // A message that never mentions waiting at all is not sustained talk.
    assert_eq!(
        watch.reasoning(Some("I will not wait, just check once.")),
        None
    );
}

#[test]
fn e_resets_after_real_work() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        watch.reasoning(Some("Wait. I need to wait for the notice.")),
        None
    );
    // A real command is progress: the waiting habit starts over.
    assert_eq!(bash(&mut watch, "cargo build", "compiled"), None);
    assert_eq!(
        watch.reasoning(Some("Wait. I still need to wait for the notice.")),
        None
    );
}

/// Detector F: the real corpus family — a different path every time, the same
/// failure every time. The third one (in a turn) arms exactly one notice.
#[test]
fn f_fires_on_the_third_same_error_from_different_paths() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    let error = "read failed: No such file or directory (os error 2)";
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"~/a.rs"}"#, error),
        None
    );
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"/b.rs"}"#, error),
        None
    );
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"c.rs"}"#, error),
        Some(Notice::SameError)
    );
    // The chain keeps counting, so the same family gets no second notice.
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"d.rs"}"#, error),
        None
    );
}

/// A success is progress: it clears the failure chain.
#[test]
fn f_stays_silent_when_a_success_interrupts_the_failures() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    let error = "read failed: No such file or directory (os error 2)";
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"a.rs"}"#, error),
        None
    );
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"b.rs"}"#, error),
        None
    );
    assert_eq!(
        call(&mut watch, "read_file", r#"{"path":"c.rs"}"#, "fn c() {}"),
        None
    );
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"d.rs"}"#, error),
        None
    );
}

/// Different failures are different failures: an errno change is not a repeat,
/// and a different tool never accumulates with this one.
#[test]
fn f_keeps_different_errors_and_tools_apart() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"a"}"#, "os error 2"),
        None
    );
    assert_eq!(
        failed(&mut watch, "read_file", r#"{"path":"b"}"#, "os error 3"),
        None
    );
    assert_eq!(
        failed(&mut watch, "bash", r#"{"command":"cat a"}"#, "os error 3"),
        None
    );
    assert_eq!(
        failed(&mut watch, "bash", r#"{"command":"cat b"}"#, "os error 3"),
        None
    );
}

/// A background launch is not a blocking wait: `bash background: true` with a
/// sleep inside never feeds C.
#[test]
fn c_ignores_a_backgrounded_sleep() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(
        bash_launch(
            &mut watch,
            "sleep 60",
            "started background task 1: sleep 60"
        ),
        None
    );
    assert_eq!(
        bash_launch(
            &mut watch,
            "sleep 90",
            "started background task 2: sleep 90"
        ),
        None
    );
    assert_eq!(
        watch.observe("bash", r#"{"command":"sleep 5"}"#, "ok", false, false),
        None
    );
}

/// The wait stem scanner counts `wait`, `waits`, `waiting` and `waited`, and
/// only at a word boundary.
#[test]
fn wait_stems_counts_the_stem_at_word_boundaries() {
    assert_eq!(wait_stems("wait"), 1);
    assert_eq!(wait_stems("Wait. Waiting. It waits, it waited."), 4);
    assert_eq!(wait_stems("await a waiter"), 0);
    assert_eq!(wait_stems("no waiting here"), 1);
}

/// Only one detector may produce a notice per turn: a sleep is also a no-op, so
/// C and D accrue on the same call, and C (higher precedence) wins.
#[test]
fn sleep_is_counted_by_c_and_d_but_only_c_fires() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "sleep 30", "ok"), None);
    assert_eq!(bash(&mut watch, "sleep 30", "ok"), Some(Notice::Repeat));
    // A: identical call, so A wins over C and D on this very call.
    assert_eq!(bash(&mut watch, "sleep 30", "ok"), None);
}

/// After the turn's one notice, no detector may add another in the same turn —
/// not even a detector that is due on a later call.
#[test]
fn the_turn_latch_caps_delivery_at_one_notice() {
    let mut watch = PollWatch::default();
    watch.start_turn();
    assert_eq!(bash(&mut watch, "sleep 30; echo ok", "ok"), None);
    assert_eq!(
        bash(&mut watch, "sleep 30; git log -1 --oneline", "abc"),
        Some(Notice::Sleep)
    );
    // Now identical to the previous call: A is due, but the turn already has
    // its notice.
    assert_eq!(
        bash(&mut watch, "sleep 30; git log -1 --oneline", "abc"),
        None
    );
    assert_eq!(bash(&mut watch, "date", "20:15"), None);
}

/// Every detector maps to the constant text that describes what it observed.
#[test]
fn every_detector_maps_to_its_own_text() {
    assert_eq!(Notice::Repeat.text(), REPEAT_CALL_NOTICE);
    assert_eq!(Notice::Shape.text(), REPEAT_SHAPE_NOTICE);
    assert_eq!(Notice::Sleep.text(), WAIT_COMMAND_NOTICE);
    assert_eq!(Notice::Noop.text(), WAIT_COMMAND_NOTICE);
    assert_eq!(Notice::WaitTalk.text(), WAIT_TALK_NOTICE);
}

/// The texts stay short, ASCII, and free of pleading.
#[test]
fn notice_texts_are_short_plain_and_concrete() {
    for text in [
        REPEAT_CALL_NOTICE,
        REPEAT_SHAPE_NOTICE,
        WAIT_COMMAND_NOTICE,
        WAIT_TALK_NOTICE,
    ] {
        assert!(
            text.chars().count() <= 300,
            "{} chars: {text}",
            text.chars().count()
        );
        assert!(text.is_ascii(), "{text}");
        assert!(!text.to_ascii_lowercase().contains("please"), "{text}");
    }
}

/// A tool that answers every call with the same fixed text (or fixed error).
struct FixedTool {
    name: &'static str,
    text: String,
    is_error: bool,
}

impl FixedTool {
    fn ok(name: &'static str, text: &str) -> Box<dyn Tool> {
        Box::new(Self {
            name,
            text: text.into(),
            is_error: false,
        })
    }

    fn err(name: &'static str, text: String) -> Box<dyn Tool> {
        Box::new(Self {
            name,
            text,
            is_error: true,
        })
    }
}

#[async_trait]
impl Tool for FixedTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "test only".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        if self.is_error {
            Err(self.text.clone())
        } else {
            Ok(ToolOutput::text(self.text.clone()))
        }
    }
}

struct ScriptedModel {
    replies: Vec<AssistantMessage>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Model for ScriptedModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok((self.replies.remove(0), None))
    }
}

fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn assistant(calls: Vec<ToolCall>, reasoning: Option<&str>) -> AssistantMessage {
    AssistantMessage {
        content: None,
        tool_calls: calls,
        reasoning: reasoning.map(Into::into),
    }
}

fn answer(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: Some(text.into()),
        tool_calls: vec![],
        reasoning: None,
    }
}

/// How many `SessionEntry::Notice` entries carry exactly this text.
fn notices(agent: &Agent, text: &str) -> usize {
    agent
        .history()
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Notice { text: notice } if notice == text))
        .count()
}

/// Positions of the notice inside one provider request (it is user-SHAPED but
/// is never a real user prompt).
fn notice_positions(request: &[Message], text: &str) -> Vec<usize> {
    request
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message, Message::User { content, .. } if content == text))
        .map(|(index, _)| index)
        .collect()
}

/// The first repeat of a batch of identical polls arms ONE notice, which rides
/// the next request as a user-shaped item — not a user message, not a prompt.
#[tokio::test]
async fn repeat_notice_rides_the_next_request_as_one_notice_entry() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        replies: vec![
            assistant(
                vec![
                    tool_call("c1", "get_background_tasks", "{}"),
                    tool_call("c2", "get_background_tasks", "{}"),
                    tool_call("c3", "get_background_tasks", "{}"),
                ],
                None,
            ),
            answer("done"),
        ],
        requests: requests.clone(),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![FixedTool::ok("get_background_tasks", SNAPSHOT)],
    );
    let captured = events.clone();
    agent.set_event_handler(Box::new(move |event| {
        captured.lock().unwrap().push(event);
    }));
    assert_eq!(agent.run("poll".into()).await.unwrap(), "done");

    assert_eq!(notices(&agent, REPEAT_CALL_NOTICE), 1);
    // The entry is a Notice, never a user message.
    assert!(
        !agent.history().iter().any(|entry| matches!(
            entry,
            SessionEntry::Message {
                message: Message::User { content, .. }
            } if content == REPEAT_CALL_NOTICE
        )),
        "{:?}",
        agent.history()
    );
    // It rides the NEXT request, after the whole tool batch.
    let requests = requests.lock().unwrap();
    let request = &requests[1];
    let positions = notice_positions(request, REPEAT_CALL_NOTICE);
    let last_tool = request
        .iter()
        .rposition(|message| matches!(message, Message::Tool { .. }))
        .expect("a tool result in the second request");
    assert_eq!(positions, vec![last_tool + 1], "{request:?}");
    drop(requests);
    // And no UserPrompt was created for it.
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::UserPrompt(_))),
        "{:?}",
        events.lock().unwrap()
    );
}

/// The shipped snapshot guard wins: a result it already answered is not
/// observed, so one call never produces two messages in one request.
#[tokio::test]
async fn a_guarded_result_is_never_observed() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let guarded = format!("{SNAPSHOT}\n\n{POLL_GUARD_ERROR}");
    let model = ScriptedModel {
        replies: vec![
            assistant(
                vec![
                    tool_call("c1", "get_background_tasks", "{}"),
                    tool_call("c2", "get_background_tasks", "{}"),
                ],
                None,
            ),
            answer("done"),
        ],
        requests: requests.clone(),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![FixedTool::err("get_background_tasks", guarded)],
    );
    assert_eq!(agent.run("poll".into()).await.unwrap(), "done");

    // Two byte-identical results would otherwise be Detector A's first repeat.
    assert_eq!(notices(&agent, REPEAT_CALL_NOTICE), 0);
    let requests = requests.lock().unwrap();
    assert!(notice_positions(&requests[1], REPEAT_CALL_NOTICE).is_empty());
}

/// Detector C reaches the model the same way: two sleeps in one turn arm one
/// advisory notice on the next request, and the turn still finishes.
#[tokio::test]
async fn sleep_wait_notice_rides_the_next_request() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        replies: vec![
            assistant(
                vec![
                    tool_call("c1", "bash", r#"{"command":"sleep 30; echo ok"}"#),
                    tool_call("c2", "bash", r#"{"command":"sleep 30; git log -1"}"#),
                ],
                None,
            ),
            answer("done"),
        ],
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Box::new(model), vec![FixedTool::ok("bash", "ok")]);
    assert_eq!(agent.run("wait".into()).await.unwrap(), "done");

    assert_eq!(notices(&agent, WAIT_COMMAND_NOTICE), 1);
    let requests = requests.lock().unwrap();
    assert_eq!(notice_positions(&requests[1], WAIT_COMMAND_NOTICE).len(), 1);
}

/// Detector E: the turn's reasoning is in scope where the guard runs, so
/// sustained waiting talk arms exactly one notice on the next request.
#[tokio::test]
async fn wait_talk_notice_rides_the_next_request() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        replies: vec![
            assistant(
                vec![tool_call("c1", "bash", r#"{"command":"date"}"#)],
                Some("Wait. I need to wait for the completion notice."),
            ),
            assistant(
                vec![tool_call("c2", "bash", r#"{"command":"echo noop"}"#)],
                Some("Wait. I will wait for the notice again."),
            ),
            answer("done"),
        ],
        requests: requests.clone(),
    };
    let mut agent = Agent::new(Box::new(model), vec![FixedTool::ok("bash", "ok")]);
    assert_eq!(agent.run("wait".into()).await.unwrap(), "done");

    assert_eq!(notices(&agent, WAIT_TALK_NOTICE), 1);
    assert_eq!(notices(&agent, WAIT_COMMAND_NOTICE), 0);
    let requests = requests.lock().unwrap();
    assert_eq!(notice_positions(&requests[2], WAIT_TALK_NOTICE).len(), 1);
}
