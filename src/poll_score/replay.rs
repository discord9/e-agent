//! Corpus replay harness for the wait-thrash detectors (test support).
//!
//! An episode is one turn's worth of traffic:
//!
//! ```json
//! {"id": "p-counter-echo", "label": "positive", "variant": "B", "note": "…",
//!  "reasoning": ["…per round…"],
//!  "calls": [{"name": "bash", "args": {"command": "echo wait1"}, "result": "wait1"}]}
//! ```
//!
//! Every field is optional and unknown fields are ignored, so a corpus written
//! by someone else still loads. `label` is `positive` | `negative` | `unclear`
//! (anything else counts as unclear and is reported but not scored), `variant`
//! is the detector expected to fire (`A`…`E` or `none`), and `id` defaults to
//! the episode's position.
//!
//! The replay walks an episode exactly like the runner's turn loop
//! (`SessionRunner::run`) does: one [`PollWatch::start_turn`] at the top, then
//! per round the injected background completion
//! ([`PollWatch::background_completed`], from `"completion": true` on that
//! call), the round's reasoning ([`PollWatch::reasoning`], aligned with `calls`
//! by index), and the round's finished tool calls ([`PollWatch::observe`],
//! with `"background_started": true` forwarding
//! [`crate::agent::ToolOutput::background_started`]). A call without `result`
//! returns its own canonical arguments, so identical calls return identical
//! text and different calls different text — what a poll-like tool does — and a
//! call whose result was too long to record carries `"result_sha256"` instead
//! (see [`digest_result`]).
//!
//! Point the loader at a real-log corpus without touching code:
//!
//! ```sh
//! POLL_CORPUS=.e-agent/corpus/episodes.jsonl \
//!   cargo test --lib poll_score::replay -- --nocapture
//! ```
//!
//! Both a JSON array and JSON lines are accepted; unparsable lines are counted
//! and reported, never fatal.

use serde::Deserialize;
use serde_json::Value;

use super::*;

/// The shipped synthetic corpus, compiled in so the replay needs no cwd.
const SCENARIOS: &str = include_str!("scenarios.json");
/// Adversarial probes whose correct answer is a judgement call. Reported by
/// the summary table, never scored: this file is where "is the rule right?"
/// is asked, and it is deliberately allowed to disagree with the detectors.
const PROBES: &str = include_str!("probes.json");

/// One episode of traffic.
#[derive(Debug, Default, Deserialize)]
struct Episode {
    /// Optional short name for the table; defaults to `#<position>`.
    #[serde(default)]
    id: String,
    /// `positive` (a notice is required) | `negative` (silence is required;
    /// `negative-but-similar` is accepted as the same thing) | `unclear`
    /// (reported, not scored).
    #[serde(default)]
    label: String,
    /// The detector that SHOULD fire: `A`, `B`, `C`, `D`, `E` or `none`.
    #[serde(default)]
    variant: String,
    /// Human explanation of the shape.
    #[serde(default)]
    note: String,
    /// Finished tool calls, in order.
    #[serde(default)]
    calls: Vec<EpisodeCall>,
    /// Reasoning of the round that issued `calls[i]`, aligned by index.
    #[serde(default)]
    reasoning: Vec<String>,
}

/// One tool call of an episode.
#[derive(Debug, Default, Deserialize)]
struct EpisodeCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Value,
    /// The result text; defaults to the call's canonical arguments.
    #[serde(default)]
    result: Option<String>,
    /// The digest of a result too long to record in the corpus (the miner
    /// writes this instead of a clipped excerpt).
    #[serde(default)]
    result_sha256: Option<String>,
    /// A background completion is injected at the model-call boundary right
    /// before this call runs.
    #[serde(default)]
    completion: bool,
    /// [`crate::agent::ToolOutput::background_started`] for this call.
    #[serde(default)]
    background_started: bool,
    /// The call FAILED (a `Result::Err` in the live path). Detector F only sees
    /// failures, so a corpus that wants F must say which calls failed.
    #[serde(default)]
    error: bool,
}

/// What one replayed episode did.
#[derive(Default)]
struct Replayed {
    /// The first (and only) detector that fired, with the call index it fired
    /// on.
    fired: Option<(Notice, usize)>,
    notices: usize,
}

impl Replayed {
    fn record(&mut self, notice: Option<Notice>, index: usize) {
        if let Some(notice) = notice {
            self.notices += 1;
            self.fired.get_or_insert((notice, index));
        }
    }
}

/// Replay one episode through the detectors, round by round, exactly as the
/// live agent loop drives them.
fn replay(episode: &Episode) -> Replayed {
    let mut watch = PollWatch::default();
    let mut replayed = Replayed::default();
    // A true turn starts the detectors' per-turn counters.
    watch.start_turn();
    for (index, call) in episode.calls.iter().enumerate() {
        // `inject_pending_background` runs at the top of every round.
        if call.completion {
            watch.background_completed();
        }
        // Then the round's reasoning (one assistant message) ...
        if let Some(reasoning) = episode.reasoning.get(index) {
            replayed.record(watch.reasoning(Some(reasoning)), index);
        }
        // ... then its finished tool calls.
        let arguments = call.args.to_string();
        let result = match (&call.result, &call.result_sha256) {
            (Some(result), _) => result.clone(),
            (None, Some(sha256)) => digest_result(sha256),
            (None, None) => canonicalize_arguments(&call.args),
        };
        // `bash background: true` always starts a background task in the live
        // path, so the launch flag is inferred when a corpus only records the
        // argument.
        let started = call.background_started
            || (call.name == "bash"
                && call
                    .args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false));
        replayed.record(
            watch.observe(&call.name, &arguments, &result, started, call.error),
            index,
        );
    }
    // Reasoning past the last call is a round that called no tool: the turn
    // ends there, so the counters exist for it but nothing follows.
    for reasoning in episode.reasoning.iter().skip(episode.calls.len()) {
        replayed.record(watch.reasoning(Some(reasoning)), episode.calls.len());
    }
    replayed
}

/// The result stand-in for a call whose text was too long to record: a digest
/// reference, deliberately longer than [`NEAR_RESULT_BYTES`] so detector B
/// keeps comparing digests exactly (its branch for any result this long) and
/// so nothing pretends to be an excerpt of the text. Two calls with the same
/// full result produce the same marker; two different results never do.
fn digest_result(sha256: &str) -> String {
    format!(
        "result text elided from the corpus (sha256 {sha256}); it is identified by this digest, \
         so an identical result compares equal and a different one does not"
    )
}

/// The letters used in the corpus for each detector.
fn detector_name(notice: Notice) -> &'static str {
    match notice {
        Notice::Repeat => "A",
        Notice::Shape => "B",
        Notice::Sleep => "C",
        Notice::Noop => "D",
        Notice::WaitTalk => "E",
        Notice::SameError => "F",
    }
}

/// The detector a `variant` field names, or `"-"` for `none`/unknown.
fn variant_name(variant: &str) -> &str {
    match variant {
        "A" | "B" | "C" | "D" | "E" | "F" => variant,
        _ => "-",
    }
}

/// Per-corpus tallies plus the findings the table cannot show in one row.
#[derive(Default)]
struct Summary {
    positives: usize,
    positives_detected: usize,
    negatives: usize,
    negatives_silent: usize,
    unscored: usize,
    /// Positives per expected detector, to prove the corpus covers all five.
    positives_by_variant: std::collections::BTreeMap<&'static str, usize>,
    failures: Vec<String>,
    mismatches: Vec<String>,
    /// Unscored episodes that fired anyway: `probes.json`'s findings.
    findings: Vec<String>,
}

impl Summary {
    /// Replay one episode, print its row, and tally it.
    fn add(&mut self, position: usize, episode: &Episode) {
        let id = if episode.id.is_empty() {
            format!("#{position}")
        } else {
            episode.id.clone()
        };
        let replayed = replay(episode);
        let fired = replayed
            .fired
            .map(|(notice, _)| detector_name(notice))
            .unwrap_or("-");
        let index = replayed
            .fired
            .map(|(_, index)| index.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let expected = variant_name(&episode.variant);
        let verdict = match episode.label.as_str() {
            "positive" => {
                self.positives += 1;
                let key = match episode.variant.as_str() {
                    "A" => "A",
                    "B" => "B",
                    "C" => "C",
                    "D" => "D",
                    "E" => "E",
                    "F" => "F",
                    _ => "-",
                };
                *self.positives_by_variant.entry(key).or_default() += 1;
                if replayed.notices == 0 {
                    self.failures.push(format!(
                        "{id}: positive episode produced 0 notices ({})",
                        episode.note
                    ));
                    "FAIL"
                } else {
                    self.positives_detected += 1;
                    "PASS"
                }
            }
            "negative" | "negative-but-similar" => {
                self.negatives += 1;
                if replayed.notices == 0 {
                    self.negatives_silent += 1;
                    "PASS"
                } else {
                    self.failures.push(format!(
                        "{id}: negative episode produced {} notice(s) ({})",
                        replayed.notices, episode.note
                    ));
                    "FAIL"
                }
            }
            _ => {
                // `unclear` and anything a foreign corpus uses: reported, not
                // scored. An unscored episode that fired is a finding about
                // the RULES (they may be right or wrong), so it is listed.
                self.unscored += 1;
                if replayed.notices > 0 {
                    self.findings.push(format!(
                        "{id}: unscored episode fired {fired} at call {index} ({})",
                        episode.note
                    ));
                }
                "SKIP"
            }
        };
        if episode.label == "positive" && expected != "-" && fired != expected {
            self.mismatches.push(format!(
                "{id}: expected detector {expected}, got {fired} at call {index} ({})",
                episode.note
            ));
        }
        println!(
            "{id:<32} {:<9} {fired:<6} {index:<7} {notices:<8} {expected:<9} {verdict}",
            episode.label,
            notices = replayed.notices,
        );
    }

    /// The headline counts plus every finding behind the PASS/FAIL column.
    fn finish(&self, source: &str) {
        println!(
            "\n{source}: positives detected {}/{}, negatives silent {}/{}, unscored {}, failures {}, variant mismatches {}, unscored findings {}",
            self.positives_detected,
            self.positives,
            self.negatives_silent,
            self.negatives,
            self.unscored,
            self.failures.len(),
            self.mismatches.len(),
            self.findings.len(),
        );
        for failure in &self.failures {
            println!("  FAIL {failure}");
        }
        for mismatch in &self.mismatches {
            println!("  VARIANT {mismatch}");
        }
        for finding in &self.findings {
            println!("  FINDING {finding}");
        }
    }
}

/// Replay a whole corpus and print the table.
fn report(source: &str, episodes: &[Episode]) -> Summary {
    println!(
        "\n{:<32} {:<9} {:<6} {:<7} {:<8} {:<9} result",
        "scenario", "label", "fired", "call #", "notices", "expected"
    );
    let calls: usize = episodes.iter().map(|episode| episode.calls.len()).sum();
    let missing = episodes
        .iter()
        .flat_map(|episode| &episode.calls)
        .filter(|call| call.result.is_none() && call.result_sha256.is_none())
        .count();
    if missing > 0 {
        println!(
            "note: {missing} of {calls} call(s) carry neither \"result\" nor \"result_sha256\"; \
             their canonical arguments are fed instead, so detector B's \"same answer\" test cannot \
             be verified from this corpus"
        );
    }
    let mut summary = Summary::default();
    for (position, episode) in episodes.iter().enumerate() {
        summary.add(position, episode);
    }
    summary.finish(source);
    summary
}

/// Per-episode first firing, without printing: the log miner reports its
/// per-detector tallies from this, so the numbers it prints come from the same
/// driver this harness uses.
pub(super) fn fired_per_episode(text: &str) -> Vec<Option<&'static str>> {
    let (episodes, skipped) = parse_corpus(text);
    assert_eq!(
        skipped, 0,
        "{skipped} unparsable line(s) in the mined corpus"
    );
    episodes
        .iter()
        .map(|episode| {
            replay(episode)
                .fired
                .map(|(notice, _)| detector_name(notice))
        })
        .collect()
}

/// Parse a corpus: a JSON array, or one JSON object per line. Unparsable lines
/// are counted instead of aborting, so a foreign corpus cannot break the run.
fn parse_corpus(text: &str) -> (Vec<Episode>, usize) {
    if let Ok(episodes) = serde_json::from_str::<Vec<Episode>>(text) {
        return (episodes, 0);
    }
    let mut episodes = Vec::new();
    let mut skipped = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Episode>(line) {
            Ok(episode) => episodes.push(episode),
            Err(_) => skipped += 1,
        }
    }
    (episodes, skipped)
}

/// The summary table over BOTH corpus files: `scenarios.json` (scored) and
/// `probes.json` (reported, unscored).
///
/// It fails when a positive episode produces zero notices or a negative
/// episode produces one. Unscored probes never fail the run — they exist to
/// keep the judgement calls visible (`FINDING` lines) instead of quietly
/// fitting the rules.
#[test]
fn summary_table_positives_fire_and_negatives_stay_silent() {
    let mut failures = Vec::new();
    let mut positives = 0;
    let mut negatives = 0;
    let mut positives_by_variant: std::collections::BTreeMap<&'static str, usize> =
        Default::default();
    for (source, text) in [("scenarios.json", SCENARIOS), ("probes.json", PROBES)] {
        let (episodes, skipped) = parse_corpus(text);
        assert_eq!(skipped, 0, "{source} must parse completely");
        let summary = report(source, &episodes);
        failures.extend(summary.failures);
        positives += summary.positives;
        negatives += summary.negatives;
        for (detector, count) in summary.positives_by_variant {
            *positives_by_variant.entry(detector).or_default() += count;
        }
    }
    println!(
        "
BOTH FILES: {positives} positive scenario(s), {negatives} negative scenario(s), {} failure(s)",
        failures.len()
    );

    // The corpus must stay big enough and must exercise every detector.
    assert!(positives >= 12, "{positives} positives");
    assert!(negatives >= 12, "{negatives} negatives");
    for detector in ["A", "B", "C", "D", "E", "F"] {
        assert!(
            positives_by_variant.get(detector).copied().unwrap_or(0) > 0,
            "no positive scenario for detector {detector}: {positives_by_variant:?}"
        );
    }
    assert!(
        failures.is_empty(),
        "{} scenario(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Point the same replay at another corpus (e.g. real traffic) without
/// touching code:
///
/// ```sh
/// POLL_CORPUS=.e-agent/corpus/episodes.jsonl \
///   cargo test --lib poll_score::replay -- --nocapture
/// ```
///
/// Unset, unreadable, or empty corpora skip instead of failing: this is a
/// debugging entry point, not a gate.
#[test]
fn corpus_from_env_replays_a_file_on_disk() {
    let Some(path) = std::env::var_os("POLL_CORPUS") else {
        println!("POLL_CORPUS is unset: external corpus replay skipped");
        return;
    };
    let path = std::path::PathBuf::from(path);
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!(
            "POLL_CORPUS={} is unreadable: replay skipped",
            path.display()
        );
        return;
    };
    let (episodes, skipped) = parse_corpus(&text);
    println!(
        "replaying {} episode(s) from {} ({skipped} unparsable line(s) skipped)",
        episodes.len(),
        path.display()
    );
    let summary = report(&path.display().to_string(), &episodes);
    assert!(
        summary.failures.is_empty(),
        "{} episode(s) in {} failed:\n{}",
        summary.failures.len(),
        path.display(),
        summary.failures.join("\n")
    );
}

/// The real-traffic corpus the log miner writes
/// (`cargo test --lib poll_score::mine -- --ignored --nocapture`). Untracked scratch, so
/// its presence is machine state, never a checked-in fixture.
const REAL_CORPUS: &str = ".e-agent/corpus/real.jsonl";

/// Replay the mined real-traffic corpus when this machine has one, and skip
/// when it does not: `POLL_CORPUS` overrides the default path (same variable
/// the test above uses), so a corpus elsewhere is one env var away.
///
/// Every episode the miner writes is labelled `unclear`, so this scores
/// nothing and cannot fail on a judgement call — it prints which detector
/// fired on which real turn (with the file and line range in each row), which
/// is the input to the human audit.
#[test]
fn real_corpus_replays_when_present() {
    let path = std::env::var_os("POLL_CORPUS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(REAL_CORPUS));
    if !path.exists() {
        println!(
            "{} does not exist: real-traffic replay skipped",
            path.display()
        );
        return;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("{} is unreadable: replay skipped", path.display());
        return;
    };
    let (episodes, skipped) = parse_corpus(&text);
    assert_eq!(
        skipped,
        0,
        "{skipped} unparsable line(s) in {}",
        path.display()
    );
    println!(
        "replaying {} real turn(s) from {} (all `unclear`: no verdicts)",
        episodes.len(),
        path.display()
    );
    let summary = report(&path.display().to_string(), &episodes);
    assert!(
        summary.failures.is_empty(),
        "{} turn(s) in {} failed:\n{}",
        summary.failures.len(),
        path.display(),
        summary.failures.join("\n")
    );
}

/// A long result carried as a digest still compares exactly: three calls of one
/// shape with the same digest are detector B's repeat, three different digests
/// are not. The marker is longer than [`NEAR_RESULT_BYTES`], so B answers from
/// the digest itself — never as if a clipped excerpt were the whole text.
#[test]
fn a_digested_result_replays_with_exact_equality() {
    let sha = |bytes: &[u8]| crate::agent::image_sha256(bytes);
    assert_eq!(sha(b"x").len(), 64);
    assert!(
        digest_result(&sha(b"x")).len() > NEAR_RESULT_BYTES,
        "detector B must take the exact-digest branch"
    );
    let episode = |digests: [&str; 3]| -> Episode {
        serde_json::from_value(serde_json::json!({
            "id": "digest",
            "calls": [
                {"name": "bash", "args": {"command": "./probe --id 1"}, "result_sha256": digests[0]},
                {"name": "bash", "args": {"command": "./probe --id 2"}, "result_sha256": digests[1]},
                {"name": "bash", "args": {"command": "./probe --id 3"}, "result_sha256": digests[2]},
            ],
        }))
        .expect("a digest corpus line parses")
    };
    let same = sha(b"the very same long result");
    let repeated = replay(&episode([&same, &same, &same]));
    assert_eq!(
        repeated.fired.map(|(notice, _)| detector_name(notice)),
        Some("B"),
        "three calls with an identical full result are a repeat"
    );
    let other = sha(b"a different long result");
    assert_eq!(
        replay(&episode([&same, &other, &same])).fired,
        None,
        "a different result breaks the shape chain"
    );
}
