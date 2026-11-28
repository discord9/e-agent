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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// One link's status: the state token the chip's dot paints, plus the single
/// detail line (tooltip / expanded row) that carries everything else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct LinkStatus {
    pub state: &'static str,
    pub detail: String,
}

impl LinkStatus {
    /// The state word first, then only the dimensions that were actually
    /// obtained — never a placeholder for a field `gh` did not answer.
    fn new(state: &'static str, parts: Vec<String>) -> Self {
        let mut line = vec![word(state).to_owned()];
        line.extend(parts);
        Self {
            state,
            detail: line.join(" · "),
        }
    }

    /// Nothing could be fetched: no dot on the chip, and the word lives in the
    /// detail line only.
    pub(crate) fn unknown() -> Self {
        Self {
            state: UNKNOWN,
            detail: word(UNKNOWN).to_owned(),
        }
    }
}

/// One GitHub pull request or issue a stored link points at.
pub(crate) struct Target {
    /// The link's normalized URL — what `gh` is asked about.
    url: String,
    /// `owner/repo`, for the "is this the session's own repository" test.
    repo: String,
    number: String,
    /// `true` for a pull request, `false` for an issue.
    pr: bool,
}

impl Target {
    /// `owner/repo` this target lives in.
    pub(crate) fn repo(&self) -> &str {
        &self.repo
    }

    /// The PR/issue number, as GitHub wrote it.
    pub(crate) fn number(&self) -> &str {
        &self.number
    }

    /// `true` for a pull request, `false` for an issue.
    pub(crate) fn is_pr(&self) -> bool {
        self.pr
    }

    /// The normalized link URL (the stored identity).
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The cache key: one entry per (repo, number, kind).
    fn key(&self) -> String {
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

/// The command seam: tests point `program` at a fixture script, or at a path
/// that does not exist, instead of standing up a second implementation.
async fn gh_status_with(program: &str, target: &Target) -> LinkStatus {
    let mut command = tokio::process::Command::new(program);
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
    let merged =
        json.get("merged").and_then(Value::as_bool).unwrap_or(false) || text("mergedAt").is_some();
    let reported = text("state").map(str::to_ascii_uppercase);
    let is_open = reported.as_deref() == Some("OPEN");
    if !is_open && reported.as_deref() != Some("CLOSED") {
        return LinkStatus::unknown();
    }
    let review = text("reviewDecision").map(str::to_ascii_uppercase);
    let merge = text("mergeStateStatus").map(str::to_ascii_uppercase);
    let checks = checks(json.get("statusCheckRollup"));
    let failing = checks.as_ref().is_some_and(|c| c.failed > 0);
    let settled = checks
        .as_ref()
        .is_none_or(|c| c.failed == 0 && c.pending == 0);
    let conflict = merge.as_deref() == Some("DIRTY");
    let state = if conflict || failing || review.as_deref() == Some("CHANGES_REQUESTED") {
        NEEDS
    } else if draft {
        DRAFT
    } else if merged {
        MERGED
    } else if !is_open {
        CLOSED
    } else if settled && merge.as_deref() == Some("CLEAN") {
        READY
    } else if review.as_deref() == Some("APPROVED") {
        APPROVED
    } else {
        OPEN
    };
    let mut parts = Vec::new();
    if let Some(merge) = merge {
        parts.push(format!("merge {}", merge.to_ascii_lowercase()));
    }
    if let Some(review) = review.filter(|review| !review.is_empty()) {
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
    if let Some(title) = text("title").map(str::trim).filter(|t| !t.is_empty()) {
        parts.push(title.to_owned());
    }
    LinkStatus::new(state, parts)
}

/// An issue has a state and a title — no merge, checks, or review — so it can
/// only be OPEN (in flight) or CLOSED; anything else is `unknown`.
fn issue_status(json: &Value) -> LinkStatus {
    let state = match json.get("state").and_then(Value::as_str) {
        Some(state) if state.eq_ignore_ascii_case("open") => OPEN,
        Some(state) if state.eq_ignore_ascii_case("closed") => CLOSED,
        _ => return LinkStatus::unknown(),
    };
    let mut parts = Vec::new();
    if let Some(title) = json
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        parts.push(title.to_owned());
    }
    LinkStatus::new(state, parts)
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

    fn store(&self, key: &str, now: Instant, status: LinkStatus) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries.insert(key.to_owned(), (now, status));
    }
}

/// The process-wide cache: one window per link, shared by every session and
/// every frontend of this server.
pub(crate) fn cache() -> &'static StatusCache {
    static CACHE: OnceLock<StatusCache> = OnceLock::new();
    CACHE.get_or_init(StatusCache::default)
}

/// Status for one link at `now`: a fresh entry is reused, otherwise exactly
/// one `gh` call happens and its result — success OR failure — is cached, so
/// neither a burst of clients nor a rate-limited repository becomes a
/// fan-out storm.
pub(crate) async fn status(cache: &StatusCache, target: &Target, now: Instant) -> LinkStatus {
    status_with(cache, target, now, || gh_status(target)).await
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
    if let Some(hit) = cache.fresh(&target.key(), now) {
        return hit;
    }
    let status = fetch().await;
    cache.store(&target.key(), now, status.clone());
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
        // Never an interpretation: a missing state, a wrong type, or a state
        // GitHub grows later is `unknown` (and paints no dot).
        for garbage in [
            json!({}),
            json!({"state": 7}),
            json!({"state": "MERGED"}),
            json!({"state": null}),
        ] {
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
        // Nothing obtained: one word, no placeholder fields.
        assert_eq!(LinkStatus::unknown().detail, "UNKNOWN");
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
            async { LinkStatus::new(OPEN, vec!["fixture".into()]) }
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
}
