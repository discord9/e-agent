//! Session-link status: the chip's state dot and the one line behind it.
//!
//! A status comes from the GitHub CLI (`gh`), is cached in memory for 60
//! seconds per (repo, number, kind), and is NEVER persisted: the store keeps
//! the link identity only, so a restarted process re-reads reality instead of
//! trusting whatever it saw before. There is no background polling — the HTTP
//! layer asks when a frontend opens or explicitly refreshes a session's links.
//!
//! Every failure (no `gh`, no network, a rate limit, a non-GitHub URL, an
//! unparseable answer) maps to `unknown`. Nothing here guesses: an `unknown`
//! link keeps its row, its label, and its place in the list.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use serde::Serialize;
use serde_json::Value;

/// How long one fetched status is reused: long enough to collapse a session
/// open plus an explicit refresh into one call, short enough that a stale
/// chip cannot outlive the human's patience.
pub(crate) const CACHE_TTL: Duration = Duration::from_secs(60);

/// One `gh` call may not hang a request for longer than this.
const GH_TIMEOUT: Duration = Duration::from_secs(10);

/// The state tokens. A token is BOTH the colour class the chip's dot paints
/// and the source of the word the detail line prints. `unknown` paints no dot
/// at all — the chip never shows a placeholder word.
pub(crate) const DRAFT: &str = "draft";
pub(crate) const READY: &str = "ready";
pub(crate) const APPROVED: &str = "approved";
pub(crate) const OPEN: &str = "open";
pub(crate) const NEEDS: &str = "needs";
pub(crate) const MERGED: &str = "merged";
pub(crate) const CLOSED: &str = "closed";
pub(crate) const UNKNOWN: &str = "unknown";

/// The word the detail line prints for a token: the chip only has room for a
/// dot, so the word has to explain the colour.
fn word(state: &str) -> &'static str {
    match state {
        READY => "READY",
        APPROVED => "APPROVED",
        OPEN => "OPEN",
        NEEDS => "NEEDS YOU",
        DRAFT => "DRAFT",
        MERGED => "MERGED",
        CLOSED => "CLOSED",
        _ => "UNKNOWN",
    }
}

/// GitHub's effective review decision. This is the ONE normalization shared
/// by the chip (which reads GitHub's `reviewDecision`) and the watcher (which
/// derives the same decision from the reviews list): both consume the same
/// tokens, so they cannot disagree about what "changes requested" or
/// "approved" means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewDecision {
    Changes,
    Approved,
}

impl ReviewDecision {
    /// Map one GitHub token. Case-insensitive; any other token (including
    /// `DISMISSED`, which a reviews-list walker must handle itself because it
    /// clears an earlier decision) is not a decision.
    pub(crate) fn from_token(raw: &str) -> Option<Self> {
        match raw.to_ascii_uppercase().as_str() {
            "CHANGES_REQUESTED" => Some(Self::Changes),
            "APPROVED" => Some(Self::Approved),
            _ => None,
        }
    }
}

/// The pieces a watcher needs for a SILENT baseline. This is never part of
/// the wire shape: it is the same observation the chip cache made, kept so a
/// link added while a session is live can start watching from that state
/// instead of treating the add as the first observation five minutes later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatchBaseline {
    /// `"merged"` or `"closed"`, if this observation is already terminal.
    pub terminal: Option<&'static str>,
    /// Names of checks failing in this observation.
    pub failing: Vec<String>,
    /// The normalized review decision, if one exists.
    pub review: Option<ReviewDecision>,
}

/// One link's status: the state token the chip's dot paints, the single
/// detail line (expanded row / popover) that carries everything else, and
/// the structured pieces a detail popover needs to build real links instead
/// of guessing from that line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct LinkStatus {
    pub state: &'static str,
    /// `state · merge · review · checks · head · title` (the historical
    /// one-line tooltip/expanded-row text).
    pub detail: String,
    /// The same line WITHOUT the title. The popover shows the title as its
    /// own plain-text row and this as the state line.
    pub state_line: String,
    /// The PR/issue title exactly as GitHub returned it (`None` when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Full head commit SHA. The short 7 characters stay in `state_line`;
    /// the full value is what a commit URL needs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Internal only: the same observation in the shape the PR watcher
    /// compares against. Never serialized.
    #[serde(skip)]
    pub(crate) watch: Option<WatchBaseline>,
}

impl LinkStatus {
    /// The state word first, then only the dimensions that were actually
    /// obtained — never a placeholder for a field `gh` did not answer.
    fn new(state: &'static str, parts: Vec<String>, title: Option<String>) -> Self {
        let mut line = vec![word(state).to_owned()];
        line.extend(parts);
        let state_line = line.join(" · ");
        let detail = match &title {
            Some(title) => format!("{state_line} · {title}"),
            None => state_line.clone(),
        };
        Self {
            state,
            detail,
            state_line,
            title,
            head: None,
            watch: None,
        }
    }

    /// Nothing could be fetched: no dot on the chip, and the word lives in the
    /// detail line only.
    pub(crate) fn unknown() -> Self {
        Self {
            state: UNKNOWN,
            detail: word(UNKNOWN).to_owned(),
            state_line: word(UNKNOWN).to_owned(),
            title: None,
            head: None,
            watch: None,
        }
    }
}

/// One GitHub pull request or issue a stored link points at.
#[derive(Clone)]
pub(crate) struct Target {
    /// The link's normalized URL — what `gh` is asked about.
    pub(crate) url: String,
    /// `owner/repo`, for the "is this the session's own repository" test.
    pub(crate) repo: String,
    pub(crate) number: String,
    /// `true` for a pull request, `false` for an issue.
    pub(crate) pr: bool,
}

impl Target {
    /// The canonical GitHub OBJECT this link names — one entry per (repo,
    /// number, kind). Storage and chips are keyed by URL, but a watcher polls
    /// and deduplicates by object: `…/pull/27` and `…/pull/27#discussion_r1`
    /// are two URLs for one pull request.
    pub(crate) fn key(&self) -> String {
        let kind = if self.pr { "pr" } else { "issue" };
        format!("{}#{} ({kind})", self.repo, self.number)
    }
}

/// The target a link names, or `None` when GitHub cannot be asked: a plain
/// link, another forge, or a shape GitHub does not serve. A `None` link is
/// `unknown`, never an error page.
pub(crate) fn target(kind: &str, raw: &str) -> Option<Target> {
    let pr = match kind {
        "pr" => true,
        "issue" => false,
        _ => return None,
    };
    let parsed = crate::session_store::parse_ref_url(raw).ok()?;
    if parsed.port().is_some() {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let segments: Vec<&str> = parsed.path_segments()?.filter(|s| !s.is_empty()).collect();
    let [owner, repo, action, number] = segments.as_slice() else {
        return None;
    };
    let action_matches = if pr {
        matches!(*action, "pull" | "pulls")
    } else {
        matches!(*action, "issues" | "issue")
    };
    let usable = |part: &str| !part.is_empty() && !part.chars().any(char::is_whitespace);
    if !action_matches || !usable(owner) || !usable(repo) || !usable(number) {
        return None;
    }
    number.parse::<u64>().ok()?;
    Some(Target {
        url: parsed.to_string(),
        repo: format!("{owner}/{repo}"),
        number: (*number).to_owned(),
        pr,
    })
}

/// `#27` when this link names a PR/issue in `own_repo` (the session's own
/// origin), `None` otherwise: another repo or host keeps the qualified label,
/// and without a resolvable origin nothing is shortened — "same repository"
/// is a claim that needs evidence.
pub(crate) fn same_repo_short(kind: &str, url: &str, own_repo: Option<&str>) -> Option<String> {
    let target = target(kind, url)?;
    (Some(target.repo.as_str()) == own_repo).then(|| format!("#{}", target.number))
}

/// Ask `gh` about one link.
pub(crate) async fn gh_status(target: &Target) -> LinkStatus {
    gh_status_with("gh", target).await
}

// Test-only tripwire: a cache-only path must never reach the command seam.
// Thread-local so parallel tests that DO exercise `gh` are unaffected.
#[cfg(test)]
thread_local! {
    static FORBID_GH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    static PREWARM_URLS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn take_prewarm_urls_for_test() -> Vec<String> {
    PREWARM_URLS.with(|urls| std::mem::take(&mut *urls.borrow_mut()))
}

#[cfg(test)]
pub(crate) struct NoGhGuard;

#[cfg(test)]
impl NoGhGuard {
    pub(crate) fn arm() -> Self {
        FORBID_GH.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for NoGhGuard {
    fn drop(&mut self) {
        FORBID_GH.with(|flag| flag.set(false));
    }
}

/// The command seam: tests point `program` at a fixture script, or at a path
/// that does not exist, instead of standing up a second implementation.
async fn gh_status_with(program: &str, target: &Target) -> LinkStatus {
    #[cfg(test)]
    FORBID_GH.with(|flag| {
        assert!(!flag.get(), "a cache-only read reached the gh command seam");
    });
    let mut command = tokio::process::Command::new(program);
    // A cancelled watcher (or a dropped request) must not leave `gh` running.
    command.kill_on_drop(true);
    if target.pr {
        command
            .arg("pr")
            .arg("view")
            .arg(&target.url)
            .arg("--json")
            .arg(
                "state,isDraft,mergedAt,title,reviewDecision,statusCheckRollup,headRefOid,\
             mergeStateStatus",
            );
    } else {
        command
            .arg("issue")
            .arg("view")
            .arg(&target.url)
            .arg("--json")
            .arg("state,title");
    }
    let Ok(Ok(output)) = tokio::time::timeout(GH_TIMEOUT, command.output()).await else {
        return LinkStatus::unknown();
    };
    if !output.status.success() {
        return LinkStatus::unknown();
    }
    let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) else {
        return LinkStatus::unknown();
    };
    if target.pr {
        pr_status(&json)
    } else {
        issue_status(&json)
    }
}

/// Map one `gh pr view --json …` document.
///
/// Blockers win, in the owner's precedence: a merge conflict (`DIRTY`), any
/// failing check, or a `CHANGES_REQUESTED` review is NEEDS YOU; then DRAFT;
/// then the terminal MERGED/CLOSED; then green READY — OPEN, merge CLEAN, no
/// failing or pending check (or no checks at all), and no changes requested —
/// and only then blue APPROVED for a review that is approved while the PR is
/// not green yet (pending checks, BEHIND, a merge state GitHub cannot compute).
/// Everything else open is OPEN (in flight). A state GitHub does not report is
/// `unknown` rather than an interpretation.
fn pr_status(json: &Value) -> LinkStatus {
    let text = |name: &str| json.get(name).and_then(Value::as_str);
    let draft = json
        .get("isDraft")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reported = text("state").map(str::to_ascii_uppercase);
    // `gh pr view --json state` reports GitHub's THREE PR states: a merged PR
    // answers `MERGED` (never `CLOSED`), so it is a terminal state, not an
    // uninterpretable one.
    let is_open = reported.as_deref() == Some("OPEN");
    let is_merged = reported.as_deref() == Some("MERGED");
    if !is_open && !is_merged && reported.as_deref() != Some("CLOSED") {
        return LinkStatus::unknown();
    }
    let merged = is_merged
        || json.get("merged").and_then(Value::as_bool).unwrap_or(false)
        || text("mergedAt").is_some();
    let review_token = text("reviewDecision").map(str::to_ascii_uppercase);
    let review = review_token.as_deref().and_then(ReviewDecision::from_token);
    let merge = text("mergeStateStatus").map(str::to_ascii_uppercase);
    let checks = checks(json.get("statusCheckRollup"));
    let failing = checks.as_ref().is_some_and(|c| c.failed > 0);
    let settled = checks
        .as_ref()
        .is_none_or(|c| c.failed == 0 && c.pending == 0);
    let conflict = merge.as_deref() == Some("DIRTY");
    let state = if conflict || failing || review == Some(ReviewDecision::Changes) {
        NEEDS
    } else if draft {
        DRAFT
    } else if merged {
        MERGED
    } else if !is_open {
        CLOSED
    } else if settled && merge.as_deref() == Some("CLEAN") {
        READY
    } else if review == Some(ReviewDecision::Approved) {
        APPROVED
    } else {
        OPEN
    };
    let mut parts = Vec::new();
    if let Some(merge) = merge {
        parts.push(format!("merge {}", merge.to_ascii_lowercase()));
    }
    if let Some(review) = review_token.filter(|review| !review.is_empty()) {
        parts.push(format!(
            "review {}",
            review.to_ascii_lowercase().replace('_', " ")
        ));
    }
    if let Some(checks) = checks {
        parts.push(checks.label());
    }
    if let Some(head) = text("headRefOid").and_then(short_sha) {
        parts.push(format!("head {head}"));
    }
    let title = text("title")
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let mut status = LinkStatus::new(state, parts, title);
    status.head = text("headRefOid")
        .map(str::trim)
        .filter(|head| !head.is_empty())
        .map(str::to_owned);
    status.watch = Some(WatchBaseline {
        terminal: if merged {
            Some("merged")
        } else if !is_open {
            Some("closed")
        } else {
            None
        },
        failing: failing_names(json.get("statusCheckRollup")),
        review,
    });
    status
}

/// An issue has a state and a title — no merge, checks, or review — so it can
/// only be OPEN (in flight) or CLOSED; anything else is `unknown`.
fn issue_status(json: &Value) -> LinkStatus {
    let state = match json.get("state").and_then(Value::as_str) {
        Some(state) if state.eq_ignore_ascii_case("open") => OPEN,
        Some(state) if state.eq_ignore_ascii_case("closed") => CLOSED,
        _ => return LinkStatus::unknown(),
    };
    let title = json
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let mut status = LinkStatus::new(state, Vec::new(), title);
    status.watch = Some(WatchBaseline {
        terminal: (state == CLOSED).then_some("closed"),
        failing: Vec::new(),
        review: None,
    });
    status
}

/// The check rollup, bucketed.
struct Checks {
    good: u32,
    failed: u32,
    pending: u32,
}

impl Checks {
    /// `3✓/1✗/1 pending`, `3✓/1✗`, or `no checks`.
    fn label(&self) -> String {
        if self.good + self.failed + self.pending == 0 {
            return "no checks".into();
        }
        let mut counts = Vec::new();
        for (count, suffix) in [(self.good, "✓"), (self.failed, "✗")] {
            if count > 0 {
                counts.push(format!("{count}{suffix}"));
            }
        }
        if self.pending > 0 {
            counts.push(format!("{} pending", self.pending));
        }
        format!("checks {}", counts.join("/"))
    }
}

/// `None` when the field itself is absent: that is not the same as "no
/// checks", and it keeps the PR away from green.
fn checks(rollup: Option<&Value>) -> Option<Checks> {
    let mut checks = Checks {
        good: 0,
        failed: 0,
        pending: 0,
    };
    for item in rollup?.as_array()? {
        match bucket(item) {
            Bucket::Good => checks.good += 1,
            Bucket::Failed => checks.failed += 1,
            Bucket::Pending => checks.pending += 1,
        }
    }
    Some(checks)
}

enum Bucket {
    Good,
    Failed,
    Pending,
}

/// The names of currently failing checks in a `statusCheckRollup`, in API
/// order and de-duplicated. This is the baseline half the chip cache shares
/// with the PR watcher.
fn failing_names(rollup: Option<&Value>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in rollup.and_then(Value::as_array).into_iter().flatten() {
        if !matches!(bucket(item), Bucket::Failed) {
            continue;
        }
        let name = if item
            .get("__typename")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("StatusContext"))
        {
            item.get("context").and_then(Value::as_str)
        } else {
            item.get("name").and_then(Value::as_str)
        };
        if let Some(name) = name.filter(|name| !name.is_empty())
            && !names.iter().any(|seen| seen == name)
        {
            names.push(name.to_owned());
        }
    }
    names
}

/// `statusCheckRollup` mixes `CheckRun` (`status` + `conclusion`) with
/// `StatusContext` (`state`). Anything unrecognised counts as pending: it is
/// not a green result, and claiming green is the one thing this must not do.
fn bucket(item: &Value) -> Bucket {
    let text = |name: &str| {
        item.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase()
    };
    if text("__typename") == "STATUSCONTEXT" {
        return match text("state").as_str() {
            "SUCCESS" => Bucket::Good,
            "ERROR" | "FAILURE" => Bucket::Failed,
            _ => Bucket::Pending,
        };
    }
    if text("status") != "COMPLETED" {
        return Bucket::Pending;
    }
    match text("conclusion").as_str() {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => Bucket::Good,
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE" => {
            Bucket::Failed
        }
        _ => Bucket::Pending,
    }
}

/// The 7 characters a human compares against `git log --oneline`.
fn short_sha(sha: &str) -> Option<String> {
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.chars().take(7).collect())
}

/// The 60-second in-memory cache. Nothing here reaches the session store.
#[derive(Default)]
pub(crate) struct StatusCache {
    entries: Mutex<HashMap<String, (Instant, LinkStatus)>>,
    /// One fetch per link at a time. The chip endpoint and an add-time
    /// prewarm race here; the loser waits and reads the winner's answer
    /// instead of issuing a second `gh` call.
    in_flight: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl StatusCache {
    fn fresh(&self, key: &str, now: Instant) -> Option<LinkStatus> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (fetched_at, status) = entries.get(key)?;
        (now.saturating_duration_since(*fetched_at) < CACHE_TTL).then(|| status.clone())
    }

    /// Store one DISPLAY observation. This is what the chip paints and what a
    /// refresh rewrites; it is deliberately not an add-time baseline, so no
    /// refresh can move the state the watcher compares against.
    fn store(&self, key: &str, now: Instant, status: LinkStatus) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // An expired window can never be served again, so it is dropped here
        // instead of being kept for the life of the process: the cache stays
        // bounded by the links read within one TTL.
        entries.retain(|_, (fetched_at, _)| now.saturating_duration_since(*fetched_at) < CACHE_TTL);
        entries.insert(key.to_owned(), (now, status));
    }

    fn forget(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key);
    }

    /// The stored entry even when it is older than the chip window. A forced
    /// refresh uses this to tell "another refresh just landed" (return it)
    /// from "the cache is warm from before" (fetch anyway).
    fn stamped(&self, key: &str) -> Option<(Instant, LinkStatus)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries.get(key).cloned()
    }
}

/// The ADD-TIME observation of one link in one session: the only source of a
/// watcher's silent baseline. It is captured once, when that session adds the
/// link, and never replaced by a later display refresh — a refresh moves the
/// chip, and if it could move this too, a failure that landed between the add
/// and the watcher's first poll would silently become the baseline and never
/// be reported. Process-local; bounded by a TTL.
///
/// The entry is published when the ADD happens, before the `gh` observation
/// exists, and the observation fills it in. That ordering is the point: a
/// watcher that synchronizes while the observation is still in flight waits
/// for it (see [`AddBaselines::take`]) instead of taking its own, later read
/// as the baseline — which is how a close right after an add used to vanish.
#[derive(Default)]
struct AddBaselines {
    entries: Mutex<HashMap<Added, AddPending>>,
    /// How many times each session unlinked each GitHub OBJECT. A watch (or a
    /// terminal marker) recorded under an older count describes a link that
    /// no longer exists, so it must re-baseline from a fresh add rather than
    /// compare against state it can no longer vouch for.
    ///
    /// Keyed by session AND object, never by the object alone: a link is one
    /// session's row, and a process-wide count would make one session's
    /// unlink re-baseline every other session watching the same pull request —
    /// silently swallowing a change nobody asked that session to forget.
    removed: Mutex<HashMap<Added, u64>>,
}

/// One announced add whose observation the watcher has not taken yet.
struct AddPending {
    /// When the row was published: the watcher's baseline boundary.
    at: DateTime<Utc>,
    /// When this entry was published, which is what bounds its lifetime —
    /// `at` is the caller's add instant and nothing says it is "now".
    announced: DateTime<Utc>,
    /// The observation, once the prewarm answers.
    answer: tokio::sync::watch::Receiver<Option<LinkStatus>>,
}

/// One link ADDED in one session. Two sessions adding the same link keep
/// their own baselines: each was observed at its own add instant.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Added {
    session: String,
    link: String,
}

impl Added {
    fn new(session: &str, link: &str) -> Self {
        Self {
            session: session.to_owned(),
            link: link.to_owned(),
        }
    }
}

/// How long one add-time baseline survives. The watcher needs it from the add
/// until its first poll (one cadence), so an hour is generous; keeping every
/// baseline forever would grow with every link this process ever saw.
const BASELINE_TTL: chrono::TimeDelta = chrono::TimeDelta::minutes(60);

/// How long the watcher waits for an add-time observation that is still in
/// flight. Past it the link is baselined by the watcher's own read: a `gh`
/// call that never answers must not freeze a whole session's watcher.
const BASELINE_WAIT: Duration = Duration::from_secs(30);

impl AddBaselines {
    /// Publish one PENDING add and hand back the channel the prewarm fills.
    /// The entry exists before the observation does, so an add whose `gh`
    /// call is still running is never mistaken for a link the watcher has
    /// simply never seen.
    fn announce(&self, session: &str, key: &str) -> tokio::sync::watch::Sender<Option<LinkStatus>> {
        self.announce_at(session, key, Utc::now())
    }

    fn announce_at(
        &self,
        session: &str,
        key: &str,
        at: DateTime<Utc>,
    ) -> tokio::sync::watch::Sender<Option<LinkStatus>> {
        let now = Utc::now();
        let (answer, pending) = tokio::sync::watch::channel(None);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // A link whose session ended or whose add is long past leaves an
        // entry nobody can use.
        entries.retain(|_, added| now.signed_duration_since(added.announced) < BASELINE_TTL);
        entries.insert(
            Added::new(session, key),
            AddPending {
                at,
                announced: now,
                answer: pending,
            },
        );
        answer
    }

    /// Consume one add's baseline. `None` when this session never announced
    /// the link (a restart, whose rows all predate the watcher), when the
    /// observation did not land within [`BASELINE_WAIT`], or when the add
    /// itself failed: the watcher then takes its OWN silent read as the
    /// baseline, at the moment it sees the link.
    async fn take(&self, session: &str, key: &str) -> Option<(DateTime<Utc>, LinkStatus)> {
        let added = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&Added::new(session, key))?;
        let status = wait_for_add(added.answer, BASELINE_WAIT).await?;
        (status.watch.is_some()).then_some((added.at, status))
    }

    fn removed(&self, session: &str, key: &str) -> u64 {
        self.removed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&Added::new(session, key))
            .copied()
            .unwrap_or_default()
    }

    /// A removal invalidates everything THIS SESSION's link owned: the
    /// pending add (it can never seed a re-add) and every watch seeded under
    /// the previous lifetime (the count moved). Another session's link to the
    /// same object is a different row, a different add, and a different
    /// lifetime, so it keeps its baseline.
    fn forget(&self, session: &str, key: &str) {
        *self
            .removed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(Added::new(session, key))
            .or_insert(0) += 1;
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|added, _| !(added.session == session && added.link == key));
    }
}

/// Wait, bounded, for one add-time observation. A closed channel means the
/// prewarm went away without answering — a failed add, not a baseline.
async fn wait_for_add(
    mut answer: tokio::sync::watch::Receiver<Option<LinkStatus>>,
    limit: Duration,
) -> Option<LinkStatus> {
    if let Some(status) = (*answer.borrow()).clone() {
        return Some(status);
    }
    let arrived = async {
        loop {
            if answer.changed().await.is_err() {
                return None;
            }
            if let Some(status) = (*answer.borrow()).clone() {
                return Some(status);
            }
        }
    };
    tokio::time::timeout(limit, arrived).await.ok().flatten()
}

fn baselines() -> &'static AddBaselines {
    static BASELINES: OnceLock<AddBaselines> = OnceLock::new();
    BASELINES.get_or_init(AddBaselines::default)
}

/// The process-wide cache: one window per link, shared by every session and
/// every frontend of this server.
pub(crate) fn cache() -> &'static StatusCache {
    static CACHE: OnceLock<StatusCache> = OnceLock::new();
    CACHE.get_or_init(StatusCache::default)
}

/// A cached observation without a fetch. `None` means "we did not look";
/// `Some(unknown)` means the observation was attempted and failed.
pub(crate) fn cached(target: &Target) -> Option<LinkStatus> {
    cache().fresh(&target.key(), Instant::now())
}

/// The state token for one stored link, only when a fresh observation exists.
/// Plain links (and non-GitHub URLs) have no target and therefore no status.
pub(crate) fn cached_state(kind: &str, url: &str) -> Option<&'static str> {
    let target = target(kind, url)?;
    cached(&target).map(|status| status.state)
}

/// Consume the add-time baseline for one link in one session, waiting
/// (bounded) for the add-time observation if it is still in flight. `None`
/// means this session never announced the link — a row that predates the
/// watcher, i.e. a restart — or that the add itself failed, so the watcher
/// takes its own silent read at the moment it first sees the link. Used by
/// the PR watcher only; the chip cache TTL does not apply, and a DISPLAY
/// refresh writes the chip cache, never this.
pub(crate) async fn take_add(
    session: &str,
    target: &Target,
) -> Option<(DateTime<Utc>, LinkStatus)> {
    baselines().take(session, &target.key()).await
}

/// How many times THIS SESSION has unlinked this object. A watch recorded
/// under an older count was seeded for a link that has since been unlinked.
pub(crate) fn removals(session: &str, target: &Target) -> u64 {
    baselines().removed(session, &target.key())
}

/// Drop every cached observation for one link in one session (removal). The
/// add-time baseline goes with it: it belongs to the lifetime that just ended,
/// so a re-add must establish its own. That session's removal count moves,
/// which is how its running watcher learns that its baseline no longer
/// describes a live link.
///
/// The chip cache is process-wide (one link, one observation) and is cleared
/// for everyone; only the LIFETIME below is per session, because only the
/// lifetime describes one session's row.
pub(crate) fn forget(session: &str, kind: &str, url: &str) {
    let Some(target) = target(kind, url) else {
        return;
    };
    cache().forget(&target.key());
    baselines().forget(session, &target.key());
}

/// Status for one link at `now`: a fresh entry is reused, otherwise exactly
/// one `gh` call happens and its result — success OR failure — is cached, so
/// neither a burst of clients nor a rate-limited repository becomes a
/// fan-out storm. Concurrent callers for the same link wait for that one
/// call instead of starting their own.
pub(crate) async fn status(cache: &StatusCache, target: &Target, now: Instant) -> LinkStatus {
    status_with(cache, target, now, || gh_status(target)).await
}

/// A user-requested refresh: never answers from the existing cache. Concurrent
/// refreshes for the same link coalesce on the same gate: a caller that
/// waited for another fetch returns that just-stored answer instead of
/// issuing a second `gh` call.
pub(crate) async fn refresh(cache: &StatusCache, target: &Target, now: Instant) -> LinkStatus {
    status_force(cache, target, now, || gh_status(target)).await
}

/// Capture one link's ADD-TIME observation in the background. This is the
/// link-add path: it returns immediately (a `gh` timeout must never stall a
/// tool result) and the result lands in the 60 s chip window AND as that
/// session's pending baseline for the watcher. A display refresh never
/// overwrites the baseline, and a cache hit older than the add is fetched
/// again instead of reused: the baseline has to describe the link as it was
/// ADDED.
pub(crate) fn prewarm(session: &str, kind: &str, url: &str) {
    #[cfg(test)]
    {
        // Unit tests must not launch real `gh` processes or pollute the
        // process-wide cache; the fetch/baseline behavior is covered by
        // `prewarm_for_test`. Record the call so the add path can prove it
        // asks for exactly one observation.
        PREWARM_URLS.with(|urls| urls.borrow_mut().push(format!("{kind} {url}")));
        let _ = session;
    }
    #[cfg(not(test))]
    prewarm_live(session, kind, url);
}

/// The production prewarm (split out so test builds contain no unreachable
/// fetch path).
#[cfg(not(test))]
fn prewarm_live(session: &str, kind: &str, url: &str) {
    let Some(target) = target(kind, url) else {
        return;
    };
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    // The add instant: a hit stamped before it predates the add.
    let added = Instant::now();
    let session = session.to_owned();
    // Announce BEFORE the fetch: the row is already visible to the watcher,
    // so the pending slot (not the watcher's own later read) is what its
    // baseline comes from.
    let slot = baselines().announce(&session, &target.key());
    runtime.spawn(async move {
        let fetched = target.clone();
        prewarm_with(
            &target,
            added,
            slot,
            || async move { gh_status(&fetched).await },
        )
        .await;
    });
}

/// Capture one add-time baseline: reuse an observation that landed AT OR
/// AFTER the add (a concurrent refresh's answer), otherwise fetch — a hit
/// from before the add may describe a state the user never added. The answer
/// (including a failure) always fills the announced slot, so a waiting
/// watcher never waits for something that already happened.
async fn prewarm_with<F, Fut>(
    target: &Target,
    added: Instant,
    slot: tokio::sync::watch::Sender<Option<LinkStatus>>,
    fetch: F,
) -> LinkStatus
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = LinkStatus>,
{
    let key = target.key();
    let status = match cache().stamped(&key) {
        Some((stored_at, status)) if stored_at >= added => status,
        _ => status_force(cache(), target, Instant::now(), fetch).await,
    };
    let _ = slot.send(Some(status.clone()));
    status
}

/// `fetch` and `now` are the test seams: a counter-backed fixture instead of
/// the real CLI, and an injected clock instead of `sleep`.
async fn status_with<F, Fut>(
    cache: &StatusCache,
    target: &Target,
    now: Instant,
    fetch: F,
) -> LinkStatus
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = LinkStatus>,
{
    let key = target.key();
    if let Some(hit) = cache.fresh(&key, now) {
        return hit;
    }
    let gate = {
        let mut in_flight = cache.in_flight.lock().await;
        // A cancelled read drops its guard without removing the entry; an
        // `Arc` nobody else holds is that leftover, not a live read, so the
        // map cannot grow with every abandoned request.
        in_flight.retain(|_, gate| Arc::strong_count(gate) > 1);
        in_flight
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = gate.lock().await;
    if let Some(hit) = cache.fresh(&key, now) {
        return hit;
    }
    let status = fetch().await;
    cache.store(&key, now, status.clone());
    cache.in_flight.lock().await.remove(&key);
    status
}

async fn status_force<F, Fut>(
    cache: &StatusCache,
    target: &Target,
    now: Instant,
    fetch: F,
) -> LinkStatus
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = LinkStatus>,
{
    let key = target.key();
    let begun = Instant::now();
    let gate = {
        let mut in_flight = cache.in_flight.lock().await;
        in_flight
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = gate.lock().await;
    // If a concurrent refresh stored its result while we waited, that is our
    // answer. A merely warm pre-existing entry is not: the user asked to
    // bypass the cache, so it is fetched again.
    if let Some((stored_at, status)) = cache.stamped(&key)
        && stored_at > begun
    {
        return status;
    }
    let status = fetch().await;
    cache.store(&key, now, status.clone());
    cache.in_flight.lock().await.remove(&key);
    status
}

/// A DISPLAY observation only — exactly what a chip read or a manual refresh
/// stores. It is never an add-time baseline.
#[cfg(test)]
pub(crate) fn seed_for_test(kind: &str, url: &str, status: LinkStatus) {
    let Some(target) = target(kind, url) else {
        return;
    };
    cache().store(&target.key(), Instant::now(), status);
}

/// Record an add-time baseline without a fetch, the way `prewarm` does for a
/// session that added the link: the slot is announced and answered at once.
#[cfg(test)]
pub(crate) fn seed_baseline_for_test(session: &str, kind: &str, url: &str, status: LinkStatus) {
    seed_baseline_at_for_test(session, kind, url, Utc::now(), status);
}

/// The same, with the ADD INSTANT spelled out: the baseline boundary is what
/// decides which second's comments are history.
#[cfg(test)]
pub(crate) fn seed_baseline_at_for_test(
    session: &str,
    kind: &str,
    url: &str,
    at: DateTime<Utc>,
    status: LinkStatus,
) {
    let Some(target) = target(kind, url) else {
        return;
    };
    let slot = baselines().announce_at(session, &target.key(), at);
    let _ = slot.send(Some(status));
}

/// The test seam for one add: `fetch` plays `gh`, and the result fills this
/// session's pending baseline. The slot is announced HERE, synchronously, so
/// a watcher that runs before the spawned task does still sees a pending add.
#[cfg(test)]
pub(crate) fn prewarm_for_test<F, Fut>(
    session: &str,
    kind: &str,
    url: &str,
    fetch: F,
) -> Option<tokio::task::JoinHandle<()>>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = LinkStatus> + Send + 'static,
{
    let target = target(kind, url)?;
    let added = Instant::now();
    let slot = baselines().announce(session, &target.key());
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    Some(runtime.spawn(async move {
        prewarm_with(&target, added, slot, fetch).await;
    }))
}

#[cfg(test)]
pub(crate) fn status_for_test(
    state: &'static str,
    terminal: Option<&'static str>,
    failing: Vec<String>,
    review: Option<&'static str>,
) -> LinkStatus {
    let mut status = LinkStatus::new(state, Vec::new(), None);
    status.watch = Some(WatchBaseline {
        terminal,
        failing,
        review: review.and_then(ReviewDecision::from_token),
    });
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Three green checks, one red, one still running.
    fn mixed_checks() -> Value {
        json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "NEUTRAL"},
            {"__typename": "StatusContext", "state": "SUCCESS"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
            {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""}
        ])
    }

    fn green_checks() -> Value {
        json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "StatusContext", "state": "SUCCESS"}
        ])
    }

    /// Everything green except one check that is still running: not ready,
    /// but nothing failed either.
    fn pending_checks() -> Value {
        json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": ""}
        ])
    }

    /// A `gh pr view --json …` document shaped like the real one.
    fn pr_fixture(
        state: &str,
        draft: bool,
        merged_at: Option<&str>,
        review: Option<&str>,
        merge: Option<&str>,
        checks: Value,
    ) -> Value {
        json!({
            "state": state,
            "isDraft": draft,
            "mergedAt": merged_at,
            "title": "One identity for a link",
            "reviewDecision": review,
            "mergeStateStatus": merge,
            "headRefOid": "255e5c0deadbeefcafe",
            "statusCheckRollup": checks,
        })
    }

    fn target_of(kind: &str, url: &str) -> Target {
        target(kind, url).expect("a GitHub target")
    }

    #[test]
    fn maps_the_state_dot_from_the_owner_palette() {
        let open = |review, merge, checks| {
            pr_status(&pr_fixture("OPEN", false, None, review, merge, checks)).state
        };
        // Green: OPEN, merge CLEAN, every check settled and green.
        assert_eq!(open(None, Some("CLEAN"), green_checks()), READY);
        assert_eq!(open(None, Some("CLEAN"), json!([])), READY);
        // A green PR stays green when the review is approved: "but not yet
        // green" is exactly what makes an approved PR blue.
        assert_eq!(open(Some("APPROVED"), Some("CLEAN"), green_checks()), READY);
        // Blue: approved, but not green — pending checks here, BEHIND there.
        assert_eq!(
            open(Some("APPROVED"), Some("CLEAN"), pending_checks()),
            APPROVED
        );
        assert_eq!(
            open(Some("APPROVED"), Some("BEHIND"), green_checks()),
            APPROVED
        );
        // Blue too: open and in flight (awaiting review, merge state unknown).
        assert_eq!(open(None, Some("BLOCKED"), green_checks()), OPEN);
        assert_eq!(open(None, None, green_checks()), OPEN);
        // Orange: blockers win — a conflict, a failing check, or a review
        // that asks for changes.
        assert_eq!(open(None, Some("DIRTY"), green_checks()), NEEDS);
        assert_eq!(open(None, Some("CLEAN"), mixed_checks()), NEEDS);
        assert_eq!(
            open(Some("CHANGES_REQUESTED"), Some("CLEAN"), green_checks()),
            NEEDS
        );
        // Grey: a draft, unless a blocker outranks it.
        let draft = |merge| {
            pr_status(&pr_fixture(
                "OPEN",
                true,
                None,
                None,
                Some(merge),
                green_checks(),
            ))
            .state
        };
        assert_eq!(draft("CLEAN"), DRAFT);
        assert_eq!(draft("DIRTY"), NEEDS);
        // Purple / dark red: terminal states.
        let terminal = |merged_at, checks| {
            pr_status(&pr_fixture("CLOSED", false, merged_at, None, None, checks)).state
        };
        assert_eq!(terminal(Some("2026-01-02T03:04:05Z"), json!([])), MERGED);
        assert_eq!(terminal(None, json!([])), CLOSED);
        // The `gh api …/pulls/N` shape carries a `merged` bool instead.
        assert_eq!(
            pr_status(&json!({"state": "closed", "merged": true})).state,
            MERGED
        );
        // `gh pr view --json state` uses GitHub's three PR states; MERGED is
        // a real terminal state, not an interpretation.
        assert_eq!(pr_status(&json!({"state": "MERGED"})).state, MERGED);
        // Never an interpretation: a missing state, a wrong type, or a state
        // GitHub grows later is `unknown` (and paints no dot).
        for garbage in [json!({}), json!({"state": 7}), json!({"state": null})] {
            assert_eq!(pr_status(&garbage).state, UNKNOWN, "{garbage}");
        }
        // Issues: open/closed only.
        assert_eq!(
            issue_status(&json!({"state": "OPEN", "title": "t"})).state,
            OPEN
        );
        assert_eq!(issue_status(&json!({"state": "closed"})).state, CLOSED);
        assert_eq!(issue_status(&json!({"state": "OPENED"})).state, UNKNOWN);
        assert_eq!(issue_status(&json!({})).state, UNKNOWN);
    }

    #[test]
    fn the_detail_line_carries_state_merge_review_checks_head_and_title() {
        assert_eq!(
            pr_status(&pr_fixture(
                "OPEN",
                false,
                None,
                Some("APPROVED"),
                Some("CLEAN"),
                mixed_checks()
            ))
            .detail,
            "NEEDS YOU · merge clean · review approved · checks 3✓/1✗/1 pending · \
             head 255e5c0 · One identity for a link"
        );
        // Only what was obtained: an issue has no merge, review, checks, or head.
        assert_eq!(
            issue_status(&json!({"state": "OPEN", "title": "Crash on start"})).detail,
            "OPEN · Crash on start"
        );
        let pr = pr_status(&pr_fixture(
            "OPEN",
            false,
            None,
            Some("APPROVED"),
            Some("CLEAN"),
            mixed_checks(),
        ));
        assert_eq!(
            pr.state_line,
            "NEEDS YOU · merge clean · review approved · checks 3✓/1✗/1 pending · head 255e5c0"
        );
        assert_eq!(pr.title.as_deref(), Some("One identity for a link"));
        assert_eq!(pr.head.as_deref(), Some("255e5c0deadbeefcafe"));
        // Nothing obtained: one word, no placeholder fields, no invented links.
        assert_eq!(LinkStatus::unknown().detail, "UNKNOWN");
        assert_eq!(LinkStatus::unknown().state_line, "UNKNOWN");
        assert_eq!(LinkStatus::unknown().title, None);
        assert_eq!(LinkStatus::unknown().head, None);
        // An absent rollup is not the same as an empty one.
        assert!(checks(None).is_none());
        assert!(checks(Some(&Value::Null)).is_none());
        assert_eq!(checks(Some(&json!([]))).unwrap().label(), "no checks");
        let only_green = json!([{"__typename": "StatusContext", "state": "SUCCESS"}]);
        assert_eq!(checks(Some(&only_green)).unwrap().label(), "checks 1✓");
    }

    #[test]
    fn only_github_pr_and_issue_urls_have_a_target() {
        assert!(target_of("pr", "https://github.com/acme/widget/pull/27").pr);
        assert!(!target_of("issue", "https://github.com/acme/widget/issues/22").pr);
        assert_eq!(
            target_of("pr", "https://github.com/acme/widget/pull/27").key(),
            "acme/widget#27 (pr)"
        );
        for (kind, url) in [
            ("link", "https://github.com/acme/widget/pull/27"),
            ("pr", "https://gitlab.com/acme/widget/pull/27"),
            ("pr", "https://github.com/acme/widget/compare/main...dev"),
            ("pr", "https://github.com/acme/widget/pull/27/files"),
            ("pr", "https://github.com/acme/widget/pull/not-a-number"),
            ("issue", "https://github.com/acme/widget/pull/27"),
        ] {
            assert!(target(kind, url).is_none(), "{kind} {url}");
        }
    }

    #[test]
    fn only_the_sessions_own_repository_is_shortened() {
        let url = "https://github.com/acme/widget/pull/27";
        assert_eq!(
            same_repo_short("pr", url, Some("acme/widget")).as_deref(),
            Some("#27")
        );
        assert_eq!(same_repo_short("pr", url, Some("acme/other")), None);
        // No resolvable origin: no claim of "same repository".
        assert_eq!(same_repo_short("pr", url, None), None);
        // A plain link or a foreign host is never shortened.
        assert_eq!(
            same_repo_short("link", "https://acme/widget/pull/27", Some("acme/widget")),
            None
        );
    }

    /// A missing CLI (or one that is not executable) is `unknown`, not a
    /// failed request.
    #[tokio::test]
    async fn a_missing_gh_is_unknown() {
        let target = target_of("pr", "https://github.com/acme/widget/pull/27");
        let status = gh_status_with("/nonexistent/gh-for-e-agent-tests", &target).await;
        assert_eq!(status, LinkStatus::unknown());
    }

    /// The real command path: parse a fixture answer, and fail closed on a
    /// non-zero exit or on a non-JSON answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_command_path_parses_gh_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.display().to_string()
        };
        let target = target_of("pr", "https://github.com/acme/widget/pull/27");
        let answer = pr_fixture("OPEN", false, None, None, Some("CLEAN"), green_checks());
        let ok = script("gh-ok", &format!("cat <<'EOF'\n{answer}\nEOF"));
        assert_eq!(gh_status_with(&ok, &target).await.state, READY);
        let refused = script("gh-refused", "echo 'gh: Not Found (HTTP 404)' >&2; exit 1");
        assert_eq!(gh_status_with(&refused, &target).await.state, UNKNOWN);
        let noisy = script("gh-noisy", "echo 'not json'");
        assert_eq!(gh_status_with(&noisy, &target).await.state, UNKNOWN);
    }

    #[tokio::test]
    async fn the_cache_hits_inside_sixty_seconds_and_refetches_after() {
        let cache = StatusCache::default();
        let pr = target_of("pr", "https://github.com/acme/widget/pull/27");
        let other = target_of("pr", "https://github.com/acme/widget/pull/28");
        let calls = AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { LinkStatus::new(OPEN, vec!["fixture".into()], None) }
        };
        let base = Instant::now();
        for now in [base, base + Duration::from_secs(59)] {
            assert_eq!(status_with(&cache, &pr, now, &fetch).await.state, OPEN);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "two reads inside 60 seconds are one fetch"
        );
        assert_eq!(status_with(&cache, &other, base, &fetch).await.state, OPEN);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a different link is a different entry"
        );
        assert_eq!(
            status_with(&cache, &pr, base + CACHE_TTL, &fetch)
                .await
                .state,
            OPEN
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "after expiry the status is fetched again"
        );
    }

    /// A rate limit must not become a burst of retries: a failed fetch is
    /// cached like any other answer.
    #[tokio::test]
    async fn a_failed_fetch_is_cached_as_unknown() {
        let cache = StatusCache::default();
        let issue = target_of("issue", "https://github.com/acme/widget/issues/29");
        let calls = AtomicUsize::new(0);
        let failing = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { LinkStatus::unknown() }
        };
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(
                status_with(&cache, &issue, now, &failing).await.state,
                UNKNOWN
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn cached_state_separates_not_looked_from_looked_and_failed() {
        let url = "https://github.com/cache-probe/widget/pull/41";
        // A unique URL: the process-wide cache has no entry for it.
        assert_eq!(cached_state("pr", url), None, "a cold cache is silent");
        seed_for_test("pr", url, status_for_test(READY, None, Vec::new(), None));
        assert_eq!(cached_state("pr", url), Some(READY));
        seed_for_test("pr", url, LinkStatus::unknown());
        assert_eq!(
            cached_state("pr", url),
            Some(UNKNOWN),
            "a failed observation is not a cold cache"
        );
        forget("cache-probe", "pr", url);
        assert_eq!(cached_state("pr", url), None, "removal drops the entry");
        assert!(
            baselines()
                .entries
                .lock()
                .unwrap()
                .get(&Added::new("cache-probe", &target_of("pr", url).key()))
                .is_none(),
            "a display observation is never an add-time baseline"
        );
        // A plain link can never have a status.
        assert_eq!(cached_state("link", "https://example.org/x"), None);
    }

    /// The add-time baseline is captured ONCE: a later display refresh (the
    /// chip, a manual refresh) rewrites the 60-second window and leaves the
    /// baseline alone, so a change between the add and the watcher's first
    /// poll stays reportable. A hit older than the add is fetched again.
    #[tokio::test]
    async fn an_add_captures_its_own_baseline_and_a_refresh_never_replaces_it() {
        let url = "https://github.com/prewarm-probe/widget/pull/42";
        let target = target_of("pr", url);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let green = status_for_test(READY, None, Vec::new(), None);
        let handle = prewarm_for_test("s1", "pr", url, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            let green = green.clone();
            async move { green }
        })
        .expect("a link is prewarmed");
        handle.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one add, one fetch");
        assert_eq!(cached_state("pr", url), Some(READY));

        // A manual refresh (or an add from another session) stores the CHIP
        // value only: this session's baseline still says green.
        status_force(cache(), &target, Instant::now(), || async {
            status_for_test(NEEDS, None, vec!["build".into()], None)
        })
        .await;
        assert_eq!(cached_state("pr", url), Some(NEEDS));
        assert_eq!(
            take_add("s1", &target)
                .await
                .map(|(_, status)| status.state),
            Some(READY),
            "a refresh never replaces the add-time baseline"
        );

        // A hit that landed AFTER the add is a real add-time observation and
        // is reused, so an immediate re-add does not spend a second `gh` call.
        let added = Instant::now();
        let refreshed = status_force(cache(), &target, Instant::now(), || async {
            status_for_test(APPROVED, None, Vec::new(), None)
        })
        .await;
        assert_eq!(refreshed.state, APPROVED);
        let slot = baselines().announce("s2", &target.key());
        let reused = prewarm_with(&target, added, slot, || async {
            panic!("a hit stamped after the add is reused");
        })
        .await;
        assert_eq!(reused.state, APPROVED);
        assert_eq!(
            take_add("s2", &target)
                .await
                .map(|(_, status)| status.state),
            Some(APPROVED)
        );

        // A cache hit from BEFORE a new add cannot seed it: the add fetches.
        let later = Instant::now();
        let handle = prewarm_for_test("s3", "pr", url, || async {
            status_for_test(OPEN, None, Vec::new(), None)
        })
        .expect("an add always captures a baseline");
        handle.await.unwrap();
        assert!(
            cache().stamped(&target.key()).unwrap().0 >= later,
            "the pre-add hit was replaced by a fresh observation"
        );
        assert_eq!(
            take_add("s3", &target)
                .await
                .map(|(_, status)| status.state),
            Some(OPEN)
        );
        forget("s3", "pr", url);
    }

    /// The add-time hand-off is a SLOT, not a value: the row is published
    /// before `gh` answers, so the watcher must be able to wait for the
    /// observation instead of baselining on its own later read. A pending add
    /// that never answers falls back to "no baseline at all" (the watcher's
    /// own read), and a removal invalidates everything the link owned.
    #[tokio::test]
    async fn a_pending_add_is_waited_for_and_a_removal_invalidates_it() {
        let baselines = Arc::new(AddBaselines::default());
        let key = "pending-probe/widget#27 (pr)";

        // In flight: `take` waits for the answer instead of reporting none.
        let slot = baselines.announce("s", key);
        let waiter = baselines.clone();
        let waiting = tokio::spawn(async move { waiter.take("s", key).await });
        // The entry is gone from the map once taken, so this is the only
        // reader; answering it is what unblocks the wait.
        slot.send(Some(status_for_test(READY, None, Vec::new(), None)))
            .unwrap();
        let (at, status) = waiting
            .await
            .unwrap()
            .expect("a pending add answers its baseline");
        assert_eq!(status.state, READY);
        assert!(at <= Utc::now());

        // A restart (no announcement) has no baseline to take.
        assert!(baselines.take("s", key).await.is_none());

        // A FAILED add answers with `unknown`, which is not a baseline: the
        // watcher takes its own silent read instead of seeding silence.
        let slot = baselines.announce("s", key);
        slot.send(Some(LinkStatus::unknown())).unwrap();
        assert!(baselines.take("s", key).await.is_none());

        // A removal drops the pending add and moves the lifetime counter that
        // a watch compares itself against. This half uses the PROCESS-wide
        // baseline store `forget` writes, on a URL no other test touches.
        let url = "https://github.com/pending-probe/widget/pull/31";
        let target = target_of("pr", url);
        assert_eq!(removals("removed", &target), 0);
        seed_baseline_for_test(
            "removed",
            "pr",
            url,
            status_for_test(READY, None, Vec::new(), None),
        );
        forget("removed", "pr", url);
        assert_eq!(removals("removed", &target), 1, "removal moves the count");
        assert!(
            take_add("removed", &target).await.is_none(),
            "a removed link can never seed a re-add"
        );
        let _ = &baselines;
    }

    /// One session's unlink is one session's row. The chip cache is shared
    /// (one link, one observation), but the LIFETIME a watch compares itself
    /// against belongs to the session that unlinked: unlinking in A must not
    /// re-baseline B's watch for the same pull request, which is how a change
    /// B never asked to forget used to disappear.
    #[tokio::test]
    async fn a_removal_in_one_session_leaves_another_sessions_link_alone() {
        let url = "https://github.com/removal-scope/widget/pull/44";
        let target = target_of("pr", url);
        seed_baseline_for_test(
            "session-a",
            "pr",
            url,
            status_for_test(READY, None, Vec::new(), None),
        );
        seed_baseline_for_test(
            "session-b",
            "pr",
            url,
            status_for_test(READY, None, Vec::new(), None),
        );
        assert_eq!(removals("session-b", &target), 0);

        forget("session-a", "pr", url);

        assert_eq!(removals("session-a", &target), 1, "A's link ended");
        assert_eq!(
            removals("session-b", &target),
            0,
            "B's link is a different row and still runs"
        );
        assert!(
            take_add("session-a", &target).await.is_none(),
            "A's link was removed: it can never seed a re-add"
        );
        assert!(
            take_add("session-b", &target).await.is_some(),
            "B's own add-time baseline is untouched by A's unlink"
        );
        // The counter is per session: a third session never unlinked it.
        assert_eq!(removals("session-c", &target), 0);
    }

    #[tokio::test]
    async fn concurrent_status_reads_share_one_fetch() {
        let cache = StatusCache::default();
        let target = target_of("pr", "https://github.com/single-flight/widget/pull/43");
        let calls = AtomicUsize::new(0);
        let ready = status_for_test(READY, None, Vec::new(), None);
        let fetch = |calls: &AtomicUsize, ready: &LinkStatus| {
            calls.fetch_add(1, Ordering::SeqCst);
            let ready = ready.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                ready
            }
        };
        let now = Instant::now();
        let (first, second) = tokio::join!(
            status_with(&cache, &target, now, || fetch(&calls, &ready)),
            status_with(&cache, &target, now, || fetch(&calls, &ready)),
        );
        assert_eq!(first.state, READY);
        assert_eq!(second.state, READY);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one link, one gh call");
    }

    /// Blocker 8: `gh pr view --json state` reports GitHub's three PR
    /// states, and a real merged PR answers `MERGED` — not `CLOSED`. The
    /// real response shape keeps its MERGED dot instead of falling back to
    /// `unknown`.
    #[test]
    fn a_merged_pr_from_the_cli_keeps_its_merged_dot() {
        // Exactly what `gh pr view <url> --json state,isDraft,mergedAt,title,
        // reviewDecision,statusCheckRollup,headRefOid,mergeStateStatus`
        // prints for a merged PR.
        let merged = json!({
            "state": "MERGED",
            "isDraft": false,
            "mergedAt": "2026-01-02T03:04:05Z",
            "title": "One identity for a link",
            "reviewDecision": "APPROVED",
            "mergeStateStatus": "UNKNOWN",
            "headRefOid": "255e5c0deadbeefcafe",
            "statusCheckRollup": [],
        });
        let status = pr_status(&merged);
        assert_eq!(status.state, MERGED);
        assert!(
            status.state_line.starts_with("MERGED"),
            "{}",
            status.state_line
        );
        assert_eq!(
            status.watch.expect("a merged PR is a terminal baseline"),
            WatchBaseline {
                terminal: Some("merged"),
                failing: Vec::new(),
                review: Some(ReviewDecision::Approved),
            }
        );
        // Without `mergedAt` the state alone still says merged.
        assert_eq!(pr_status(&json!({"state": "MERGED"})).state, MERGED);
        assert_eq!(pr_status(&json!({"state": "CLOSED"})).state, CLOSED);
    }

    /// Blocker 5: a long-lived process must not accumulate one entry per link
    /// it ever read. An expired window is evicted on the next store, and an
    /// add-time baseline nobody can use is dropped on the next put.
    #[tokio::test]
    async fn churned_cache_and_baselines_stay_bounded() {
        let cache = StatusCache::default();
        let now = Instant::now();
        for number in 1..=200 {
            let target = target_of(
                "pr",
                &format!("https://github.com/acme/widget/pull/{number}"),
            );
            cache.store(&target.key(), now, LinkStatus::unknown());
        }
        assert_eq!(cache.entries.lock().unwrap().len(), 200);

        // One TTL later every one of those windows is unusable: storing a new
        // observation evicts them instead of keeping them forever.
        let later = now + CACHE_TTL + Duration::from_secs(1);
        cache.store("acme/widget#1 (pr)", later, LinkStatus::unknown());
        assert_eq!(
            cache.entries.lock().unwrap().len(),
            1,
            "expired cache windows are evicted"
        );

        let baselines = AddBaselines::default();
        let (answer, pending) = tokio::sync::watch::channel(None);
        baselines.entries.lock().unwrap().insert(
            Added::new("s1", "acme/widget#1 (pr)"),
            AddPending {
                at: Utc::now(),
                announced: Utc::now() - chrono::TimeDelta::hours(2),
                answer: pending,
            },
        );
        let _ = baselines.announce("s1", "acme/widget#2 (pr)");
        assert!(
            baselines
                .entries
                .lock()
                .unwrap()
                .get(&Added::new("s1", "acme/widget#1 (pr)"))
                .is_none(),
            "an add nobody can use any more is dropped on the next announce"
        );
        assert!(
            baselines
                .entries
                .lock()
                .unwrap()
                .contains_key(&Added::new("s1", "acme/widget#2 (pr)"))
        );
        drop(answer);
    }
}
