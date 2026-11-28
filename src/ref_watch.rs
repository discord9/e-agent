//! The PR/issue watcher: a live session's linked open refs are polled every
//! five minutes, and a real change becomes ONE automatic model turn.
//!
//! The transport is the existing durable `SessionEntry::Notice` projection:
//! the transcript shows a notice (never a user bubble) while the model reads
//! the same text as a user message. The text starts with a header that says
//! the message is automatic and ends with the disclaimer, so no provider and
//! no reader can mistake it for the human speaking.
//!
//! Polling is deliberately cheap and quiet:
//!
//! - The main PR/issue document is requested with `If-None-Match`; a `304`
//!   ends that link's poll, so an unchanged link costs exactly one
//!   conditional request and produces no turn.
//! - Nested reads (checks, reviews, comments) happen only after a `200`.
//! - A `403`/`429` pauses the whole watcher until `X-RateLimit-Reset`.
//! - The first observation of a link is a baseline: it records current state
//!   and the comment watermark, and emits nothing for the past.
//! - A link is dropped as soon as it is MERGED or CLOSED.
//!
//! Everything here is process-local. There is no webhook, no persisted
//! watcher row, and no second scheduler: the loop is one ordinary detached
//! task in the session's existing background registry.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::time::Instant;

use crate::link_status;
use crate::session_store::{SessionRef, SessionStore};

/// One poll every five minutes, the cadence the owner asked for.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// One `gh` call may not hang the watcher for longer than this.
const GH_TIMEOUT: Duration = Duration::from_secs(15);

/// A 403/429 without `X-RateLimit-Reset` still must not become a retry storm.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

/// The exact prefix every automatic message carries. The transcript entry is
/// a `Notice`; this line makes the same fact explicit to the model.
pub(crate) const HEADER: &str = "[自动检测 · 非用户消息]";

/// The exact closing line: automatic origin and the action expectation.
pub(crate) const DISCLAIMER: &str =
    "——本消息由 PR 监视自动生成，不是用户发言；请据此判断并采取必要行动。";

/// One request to the GitHub CLI. `path` is a REST path with its query; the
/// ETag, when present, is sent as `If-None-Match`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub path: String,
    pub etag: Option<String>,
}

/// One `gh api -i` answer, reduced to what the watcher needs.
#[derive(Clone, Debug, Default)]
pub(crate) struct Reply {
    pub status: u16,
    pub etag: Option<String>,
    /// Epoch seconds from `X-RateLimit-Reset` (403/429 only).
    pub reset_at: Option<i64>,
    pub body: Vec<u8>,
}

/// The fetch seam: production runs `gh`, tests answer from a fixture.
pub(crate) type Fetch = dyn Fn(Request) -> BoxFuture<'static, Reply> + Send + Sync;

/// A link that can actually be watched on GitHub. Plain links are omitted:
/// the spec never asks for their status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatchLink {
    repo: String,
    number: u64,
    pr: bool,
    url: String,
}

impl WatchLink {
    fn from_ref(reference: &SessionRef) -> Option<Self> {
        let target = link_status::target(&reference.kind, &reference.url)?;
        Some(Self {
            repo: target.repo().to_owned(),
            number: target.number().parse().ok()?,
            pr: target.is_pr(),
            url: target.url().to_owned(),
        })
    }

    /// `PR #27` / `Issue #12`: what the message names.
    fn label(&self) -> String {
        format!("{} #{}", if self.pr { "PR" } else { "Issue" }, self.number)
    }
}

/// The terminal states the watcher stops on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Merged,
    Closed,
}

/// The review decision that can produce a transition. No decision means
/// neither requested-changes nor approved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewDecision {
    Changes,
    Approved,
}

/// One observation of a link's meaningful state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Observed {
    terminal: Option<Terminal>,
    /// Names of checks currently failing (empty = none).
    failing: Vec<String>,
    review: Option<ReviewDecision>,
}

/// One persisted-in-memory cursor per link. It never leaves the process.
struct LinkWatch {
    link: WatchLink,
    etag: Option<String>,
    baselined: bool,
    prev: Option<Observed>,
    last_comment_at: Option<DateTime<Utc>>,
    last_review_comment_at: Option<DateTime<Utc>>,
    last_review_at: Option<DateTime<Utc>>,
    /// Comment/review ids already emitted. This is what makes an inclusive
    /// `since=` window idempotent when a later poll covers the same second.
    seen: HashSet<i64>,
}

impl LinkWatch {
    fn new(link: WatchLink) -> Self {
        Self {
            link,
            etag: None,
            baselined: false,
            prev: None,
            last_comment_at: None,
            last_review_comment_at: None,
            last_review_at: None,
            seen: HashSet::new(),
        }
    }

    /// One link's poll. `Failed` leaves every cursor untouched so the next
    /// poll retries from the same state instead of inventing a transition.
    async fn poll(&mut self, fetch: &Fetch, wall: DateTime<Utc>) -> Step {
        let main = if self.link.pr {
            format!("repos/{}/pulls/{}", self.link.repo, self.link.number)
        } else {
            format!("repos/{}/issues/{}", self.link.repo, self.link.number)
        };
        let reply = fetch(Request {
            path: main,
            etag: self.etag.clone(),
        })
        .await;
        match reply.status {
            304 => {
                if reply.etag.is_some() {
                    self.etag = reply.etag;
                }
                return Step::Idle;
            }
            403 | 429 => {
                return Step::RateLimited {
                    reset_at: reply.reset_at,
                };
            }
            200 => {}
            _ => return Step::Failed,
        }
        let Some(doc) = serde_json::from_slice::<Value>(&reply.body).ok() else {
            return Step::Failed;
        };
        let Some(observed) = self.observe(&doc) else {
            return Step::Failed;
        };
        // A terminal link is never polled again. During a baseline it is
        // simply dropped; after a baseline it emits exactly one line.
        if let Some(terminal) = observed.terminal {
            let line = if self.baselined {
                let previous = self.prev.as_ref().and_then(|prev| prev.terminal);
                (previous != Some(terminal)).then(|| match terminal {
                    Terminal::Merged => format!("{} 已合并", self.link.label()),
                    Terminal::Closed => format!("{} 已关闭", self.link.label()),
                })
            } else {
                None
            };
            self.etag = reply.etag;
            self.prev = Some(observed);
            self.baselined = true;
            return match line {
                Some(line) => Step::EmitStop(vec![line]),
                None => Step::Stop,
            };
        }

        // Nested reads are part of one state observation: any failure skips
        // the whole update rather than comparing against a hole.
        let mut observed = observed;
        let mut notes = Vec::new();
        if self.link.pr {
            let Some(sha) = doc.pointer("/head/sha").and_then(Value::as_str) else {
                return Step::Failed;
            };
            let check_runs = match self
                .fetch_json(
                    fetch,
                    format!("repos/{}/commits/{sha}/check-runs", self.link.repo),
                )
                .await
            {
                Ok(value) => value,
                Err(step) => return step,
            };
            let statuses = match self
                .fetch_json(
                    fetch,
                    format!("repos/{}/commits/{sha}/status", self.link.repo),
                )
                .await
            {
                Ok(value) => value,
                Err(step) => return step,
            };
            observed.failing = failing_checks(&check_runs, &statuses);
            let raw_reviews = match self
                .fetch_json(
                    fetch,
                    format!(
                        "repos/{}/pulls/{}/reviews?per_page=100",
                        self.link.repo, self.link.number
                    ),
                )
                .await
            {
                Ok(value) => value,
                Err(step) => return step,
            };
            observed.review = review_decision(&raw_reviews);
            let reviews = parse_notes(&raw_reviews);
            if !self.baselined {
                // Baseline: every existing review is already "seen"; the
                // decision above is the state a later poll compares against.
                let latest = reviews.iter().map(|note| note.at).max();
                self.last_review_at = latest;
                self.seen.extend(reviews.iter().map(|note| note.id));
            } else {
                notes.extend(self.new_notes(reviews, NoteKind::Review).lines);
            }
        }

        if !self.baselined {
            self.etag = reply.etag;
            self.prev = Some(observed);
            self.baselined = true;
            // Nothing before this instant may ever surface as news.
            self.last_comment_at = Some(wall);
            self.last_review_comment_at = Some(wall);
            return Step::Idle;
        }

        let previous = self.prev.clone().unwrap_or_default();
        let mut lines = transitions(&self.link, &previous, &observed);
        let mut comments = Vec::new();

        // New issue/conversation comments, from the persisted watermark.
        if let Some(last) = self.last_comment_at {
            let path = format!(
                "repos/{}/issues/{}/comments?since={}&per_page=100",
                self.link.repo,
                self.link.number,
                since(last)
            );
            match self.fetch_json(fetch, path).await {
                Ok(value) => {
                    let notes = self.new_notes(parse_notes(&value), NoteKind::Comment);
                    comments.extend(notes.lines);
                }
                Err(Step::RateLimited { reset_at }) => {
                    return Step::RateLimited { reset_at };
                }
                Err(_) => {}
            }
        }
        // Inline review comments on a PR.
        if self.link.pr
            && let Some(last) = self.last_review_comment_at
        {
            let path = format!(
                "repos/{}/pulls/{}/comments?since={}&per_page=100",
                self.link.repo,
                self.link.number,
                since(last)
            );
            match self.fetch_json(fetch, path).await {
                Ok(value) => {
                    let notes = self.new_notes(parse_notes(&value), NoteKind::ReviewComment);
                    comments.extend(notes.lines);
                }
                Err(Step::RateLimited { reset_at }) => {
                    return Step::RateLimited { reset_at };
                }
                Err(_) => {}
            }
        }

        // New reviews (with a body) and comments share one display shape.
        // Only mention a comment burst when no transition already names the
        // link; the sample shape stays one transition line + one comment.
        comments.extend(notes);
        if lines.is_empty() && !comments.is_empty() {
            lines.push(format!("{} 收到新评论", self.link.label()));
        }
        lines.extend(comments);
        self.etag = reply.etag;
        self.prev = Some(observed);
        if lines.is_empty() {
            Step::Idle
        } else {
            Step::Emit(lines)
        }
    }

    fn observe(&self, doc: &Value) -> Option<Observed> {
        if self.link.pr {
            observe_pr(doc)
        } else {
            observe_issue(doc)
        }
    }

    async fn fetch_json(&self, fetch: &Fetch, path: String) -> Result<Value, Step> {
        let reply = fetch(Request { path, etag: None }).await;
        match reply.status {
            200 => serde_json::from_slice::<Value>(&reply.body).map_err(|_| Step::Failed),
            403 | 429 => Err(Step::RateLimited {
                reset_at: reply.reset_at,
            }),
            _ => Err(Step::Failed),
        }
    }

    /// Notes newer than the watermark, with the watermark advanced and ids
    /// remembered. `lines` carries the display form (author + excerpt).
    fn new_notes(&mut self, notes: Vec<Note>, kind: NoteKind) -> Notes {
        let watermark = match kind {
            NoteKind::Comment => &mut self.last_comment_at,
            NoteKind::ReviewComment => &mut self.last_review_comment_at,
            NoteKind::Review => &mut self.last_review_at,
        };
        let mut lines = Vec::new();
        for note in notes {
            if note.id != 0 && !self.seen.insert(note.id) {
                continue;
            }
            if watermark.is_some_and(|since| note.at < since) {
                continue;
            }
            *watermark = Some(watermark.map_or(note.at, |current| current.max(note.at)));
            // A review without a body is already represented by its state
            // transition (approved / changes requested); an empty excerpt
            // would be noise.
            if let Some(line) = comment_line(&note.author, &note.body) {
                lines.push(line);
            }
        }
        Notes { lines }
    }
}

enum Step {
    Idle,
    Emit(Vec<String>),
    EmitStop(Vec<String>),
    Stop,
    RateLimited { reset_at: Option<i64> },
    Failed,
}

#[derive(Clone, Copy)]
enum NoteKind {
    Comment,
    ReviewComment,
    Review,
}

struct Notes {
    lines: Vec<String>,
}

/// One comment or review comment.
#[derive(Clone, Debug)]
struct Note {
    id: i64,
    author: String,
    at: DateTime<Utc>,
    body: String,
}

/// The process-local watcher state. One instance per live session.
#[derive(Default)]
pub(crate) struct Watcher {
    links: Vec<LinkWatch>,
    paused_until: Option<Instant>,
    /// Terminal links are remembered so a later poll of the same stored URL
    /// does not create a fresh, unbaselined watch for a merged/closed ref.
    done: HashSet<String>,
}

impl Watcher {
    /// Poll every currently linked open PR/issue once. Returns exactly one
    /// message for the whole burst, or `None` when nothing changed.
    pub(crate) async fn poll(
        &mut self,
        links: &[WatchLink],
        fetch: &Fetch,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> Option<String> {
        self.sync(links);
        if self.paused_until.is_some_and(|until| now < until) {
            return None;
        }
        let mut lines = Vec::new();
        let mut stopped = Vec::new();
        for watch in &mut self.links {
            match watch.poll(fetch, wall).await {
                Step::Idle | Step::Failed => {}
                Step::Emit(mut emitted) => lines.append(&mut emitted),
                Step::EmitStop(mut emitted) => {
                    lines.append(&mut emitted);
                    stopped.push(watch.link.url.clone());
                }
                Step::Stop => stopped.push(watch.link.url.clone()),
                Step::RateLimited { reset_at } => {
                    let wait = backoff(reset_at, wall);
                    self.paused_until = Some(now + wait);
                    break;
                }
            }
        }
        for url in stopped {
            self.done.insert(url);
        }
        self.links
            .retain(|watch| !self.done.contains(&watch.link.url));
        (!lines.is_empty()).then(|| render(&lines))
    }

    /// How long the loop may sleep after a poll. A rate-limit window that
    /// outlives the normal cadence wins.
    pub(crate) fn next_wait(&self, interval: Duration, now: Instant) -> Duration {
        match self.paused_until {
            Some(until) if until > now => (until - now).max(interval),
            _ => interval,
        }
    }

    /// Keep state for exactly the currently linked refs, in store order.
    /// A newly added link starts unbaselined, so its own past is silent.
    fn sync(&mut self, links: &[WatchLink]) {
        let mut existing: HashMap<String, LinkWatch> = self
            .links
            .drain(..)
            .map(|watch| (watch.link.url.clone(), watch))
            .collect();
        for link in links {
            if self.done.contains(&link.url) {
                continue;
            }
            let watch = existing
                .remove(&link.url)
                .unwrap_or_else(|| LinkWatch::new(link.clone()));
            self.links.push(watch);
        }
    }
}

/// One whole watcher lifetime. It ends when the session's command endpoint is
/// gone, and the runner aborts the registry task in every other exit path.
pub(crate) async fn watch<S>(
    store: SessionStore,
    root: PathBuf,
    session: String,
    interval: Duration,
    program: String,
    mut send: S,
) -> String
where
    S: FnMut(String) -> bool,
{
    let fetch: Box<Fetch> = Box::new(gh_fetch(program));
    let mut watcher = Watcher::default();
    loop {
        let wall = Utc::now();
        let now = Instant::now();
        match store.list_refs(&root, &session).await {
            Ok(refs) => {
                let links = refs
                    .iter()
                    .filter_map(WatchLink::from_ref)
                    .collect::<Vec<_>>();
                if let Some(text) = watcher.poll(&links, &fetch, now, wall).await
                    && !send(text)
                {
                    return "PR watcher stopped: session ended".into();
                }
            }
            Err(error) => tracing::warn!("PR watcher cannot read session links: {error:#}"),
        }
        tokio::time::sleep(watcher.next_wait(interval, now)).await;
    }
}

/// Start one watcher in the session's existing background registry. The task
/// delivers no completion event: its only output is the automatic turn.
pub(crate) fn spawn(
    background: &crate::tools::BackgroundTasks,
    store: SessionStore,
    root: PathBuf,
    session: String,
    interval: Duration,
    program: String,
    send: impl FnMut(String) -> bool + Send + 'static,
) -> Result<u64, String> {
    background.spawn_silent("PR watch".into(), move || async move {
        watch(store, root, session, interval, program, send).await
    })
}

/// Map one REST pull-request document to the state that matters.
fn observe_pr(doc: &Value) -> Option<Observed> {
    let state = doc.get("state")?.as_str()?.to_ascii_uppercase();
    let merged = doc.get("merged").and_then(Value::as_bool).unwrap_or(false)
        || doc
            .get("merged_at")
            .and_then(Value::as_str)
            .is_some_and(|at| !at.is_empty());
    let terminal = if merged {
        Some(Terminal::Merged)
    } else {
        match state.as_str() {
            "OPEN" => None,
            "CLOSED" => Some(Terminal::Closed),
            _ => return None,
        }
    };
    Some(Observed {
        terminal,
        failing: Vec::new(),
        review: None,
    })
}

fn observe_issue(doc: &Value) -> Option<Observed> {
    match doc.get("state")?.as_str()?.to_ascii_uppercase().as_str() {
        "OPEN" => Some(Observed::default()),
        "CLOSED" => Some(Observed {
            terminal: Some(Terminal::Closed),
            ..Observed::default()
        }),
        _ => None,
    }
}

/// The lines a change from `previous` to `now` produces. Terminal states win
/// over everything else (there is no useful "approved" line for a merge).
fn transitions(link: &WatchLink, previous: &Observed, now: &Observed) -> Vec<String> {
    let mut lines = Vec::new();
    if now.review == Some(ReviewDecision::Changes)
        && previous.review != Some(ReviewDecision::Changes)
    {
        lines.push(format!("{} 要求修改", link.label()));
    } else if now.review == Some(ReviewDecision::Approved)
        && previous.review != Some(ReviewDecision::Approved)
    {
        lines.push(format!("{} 已批准", link.label()));
    }
    // A check that was not failing before and is failing now is the
    // transition; a still-failing set is not repeated every poll. This also
    // covers a newly added check failing while another was already red.
    let newly_failing = now
        .failing
        .iter()
        .filter(|name| !previous.failing.contains(name))
        .collect::<Vec<_>>();
    if !newly_failing.is_empty() {
        lines.push(format!(
            "{} 检查失败：{}（{} 项）",
            link.label(),
            newly_failing
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            newly_failing.len()
        ));
    }
    lines
}

/// The sample shape: header + first event on line one, details after it, and
/// the disclaimer as the final line.
fn render(lines: &[String]) -> String {
    let mut text = format!("{HEADER} {}", lines[0]);
    for line in &lines[1..] {
        text.push('\n');
        text.push_str(line);
    }
    text.push('\n');
    text.push_str(DISCLAIMER);
    text
}

/// One-line excerpt: whitespace collapsed, bounded, Chinese quotes kept in
/// the caller's `author · "…"` shape.
fn comment_line(author: &str, body: &str) -> Option<String> {
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if body.is_empty() {
        return None;
    }
    let mut excerpt: String = body.chars().take(100).collect();
    if body.chars().count() > 100 {
        excerpt.push('…');
    }
    Some(format!("{} · \"{excerpt}\"", author.trim()))
}

fn since(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The failing check names from `check-runs` and legacy commit statuses, in
/// API order and de-duplicated. Anything not completed counts as pending,
/// not failing — the same conservative split as `link_status`.
fn failing_checks(check_runs: &Value, statuses: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |name: String| {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    };
    if let Some(runs) = check_runs.get("check_runs").and_then(Value::as_array) {
        for run in runs {
            let text = |name: &str| {
                run.get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_uppercase()
            };
            if text("status") != "COMPLETED" {
                continue;
            }
            if matches!(
                text("conclusion").as_str(),
                "FAILURE"
                    | "TIMED_OUT"
                    | "CANCELLED"
                    | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE"
                    | "STALE"
            ) {
                push(
                    run.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
    }
    if let Some(contexts) = statuses.get("statuses").and_then(Value::as_array) {
        for context in contexts {
            if matches!(
                context
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_uppercase()
                    .as_str(),
                "ERROR" | "FAILURE"
            ) {
                push(
                    context
                        .get("context")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
    }
    names
}

fn parse_notes(value: &Value) -> Vec<Note> {
    let mut notes = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let at = item
                .get("created_at")
                .or_else(|| item.get("submitted_at"))
                .and_then(Value::as_str)
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                .map(|at| at.with_timezone(&Utc))?;
            Some(Note {
                id: item.get("id").and_then(Value::as_i64).unwrap_or(0),
                author: item
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .unwrap_or("ghost")
                    .to_owned(),
                at,
                body: item
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect::<Vec<_>>();
    notes.sort_by_key(|note| (note.at, note.id));
    notes
}

/// GitHub's effective decision: latest review per author.
fn review_decision(value: &Value) -> Option<ReviewDecision> {
    let mut latest: HashMap<&str, DateTime<Utc>> = HashMap::new();
    let mut states: HashMap<&str, &'static str> = HashMap::new();
    for review in value.as_array().into_iter().flatten() {
        let author = review
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let state = review
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();
        let Some(decision) = (match state.as_str() {
            "CHANGES_REQUESTED" => Some("CHANGES_REQUESTED"),
            "APPROVED" => Some("APPROVED"),
            // COMMENTED/DISMISSED/PENDING never change the effective
            // decision; an earlier decisive review by the same author stays.
            _ => None,
        }) else {
            continue;
        };
        if author.is_empty() {
            continue;
        }
        let at = review
            .get("submitted_at")
            .and_then(Value::as_str)
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or_default();
        if latest.get(author).is_none_or(|previous| at >= *previous) {
            latest.insert(author, at);
            states.insert(author, decision);
        }
    }
    if states.values().any(|state| *state == "CHANGES_REQUESTED") {
        Some(ReviewDecision::Changes)
    } else if states.values().any(|state| *state == "APPROVED") {
        Some(ReviewDecision::Approved)
    } else {
        None
    }
}

fn backoff(reset_at: Option<i64>, wall: DateTime<Utc>) -> Duration {
    let Some(reset_at) = reset_at else {
        return DEFAULT_BACKOFF;
    };
    let seconds = (reset_at - wall.timestamp()).max(1) as u64;
    Duration::from_secs(seconds)
}

/// Production fetcher: `gh api -i`, which prints the status line and headers
/// even for a 304 (where it exits non-zero but the answer is still useful).
fn gh_fetch(program: String) -> impl Fn(Request) -> BoxFuture<'static, Reply> + Send + Sync {
    move |request| {
        let program = program.clone();
        Box::pin(async move { gh_get(&program, request).await })
    }
}

async fn gh_get(program: &str, request: Request) -> Reply {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("api")
        .arg("-i")
        .arg("--hostname")
        .arg("github.com")
        .arg("--method")
        .arg("GET")
        .arg("-H")
        .arg("Accept: application/vnd.github+json");
    if let Some(etag) = &request.etag {
        command.arg("-H").arg(format!("If-None-Match: {etag}"));
    }
    command.arg(&request.path);
    let Ok(Ok(output)) = tokio::time::timeout(GH_TIMEOUT, command.output()).await else {
        return Reply::default();
    };
    parse_gh_output(&output.stdout)
}

fn parse_gh_output(bytes: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = match text.find("\r\n\r\n") {
        Some(index) => (&text[..index], &text[index + 4..]),
        None => match text.find("\n\n") {
            Some(index) => (&text[..index], &text[index + 2..]),
            None => (&text[..], ""),
        },
    };
    let mut reply = Reply {
        body: body.as_bytes().to_vec(),
        ..Reply::default()
    };
    for line in head.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("HTTP/") {
            reply.status = rest
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok())
                .unwrap_or(0);
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "etag" => reply.etag = Some(value.trim().to_owned()),
            "x-ratelimit-reset" => reply.reset_at = value.trim().parse().ok(),
            _ => {}
        }
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex as StdMutex};

    fn link(pr: bool, number: u64) -> WatchLink {
        WatchLink {
            repo: "acme/widget".into(),
            number,
            pr,
            url: format!(
                "https://github.com/acme/widget/{}/{number}",
                if pr { "pull" } else { "issues" }
            ),
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
    }

    fn pr_body(state: &str, merged: bool, sha: &str) -> Value {
        json!({"state": state, "merged": merged, "draft": false, "head": {"sha": sha}})
    }

    fn checks(items: &[(&str, &str)]) -> Value {
        json!({
            "total_count": items.len(),
            "check_runs": items.iter().map(|(name, conclusion)| json!({
                "__typename": "CheckRun",
                "name": name,
                "status": "COMPLETED",
                "conclusion": conclusion,
            })).collect::<Vec<_>>()
        })
    }

    fn no_statuses() -> Value {
        json!({"state": "success", "statuses": []})
    }

    fn statuses(items: &[(&str, &str)]) -> Value {
        json!({
            "state": "failure",
            "statuses": items.iter().map(|(context, state)| json!({
                "context": context,
                "state": state,
            })).collect::<Vec<_>>()
        })
    }

    fn reviews(items: &[(i64, &str, &str, &str, &str)]) -> Value {
        json!(
            items
                .iter()
                .map(|(id, author, state, at, body)| json!({
                    "id": id,
                    "user": {"login": author},
                    "state": state,
                    "submitted_at": at,
                    "body": body,
                }))
                .collect::<Vec<_>>()
        )
    }

    fn comments(items: &[(i64, &str, &str, &str)]) -> Value {
        json!(
            items
                .iter()
                .map(|(id, author, at, body)| json!({
                    "id": id,
                    "user": {"login": author},
                    "created_at": at,
                    "body": body,
                }))
                .collect::<Vec<_>>()
        )
    }

    /// A tiny ETag-aware GitHub: responses are served per path prefix, and a
    /// request carrying the stored ETag answers 304. Every request is
    /// recorded so a test can assert the conditional short-circuit.
    type ForcedReply = Arc<StdMutex<Option<(u16, Option<i64>)>>>;

    #[derive(Clone, Default)]
    struct FakeGh {
        responses: Arc<StdMutex<HashMap<String, (String, Value)>>>,
        requests: Arc<StdMutex<Vec<Request>>>,
        /// When set, every request answers this status (rate-limit tests).
        forced: ForcedReply,
    }

    impl FakeGh {
        fn set(&self, key: &str, etag: &str, body: Value) {
            self.responses
                .lock()
                .unwrap()
                .insert(key.into(), (etag.into(), body));
        }

        fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        fn fetch(&self) -> Box<Fetch> {
            let fake = self.clone();
            Box::new(move |request| {
                let fake = fake.clone();
                Box::pin(async move {
                    fake.requests.lock().unwrap().push(request.clone());
                    if let Some((status, reset_at)) = *fake.forced.lock().unwrap() {
                        return Reply {
                            status,
                            reset_at,
                            ..Reply::default()
                        };
                    }
                    let base = request.path.split('?').next().unwrap_or_default();
                    let Some((etag, body)) = fake.responses.lock().unwrap().get(base).cloned()
                    else {
                        return Reply {
                            status: 404,
                            ..Reply::default()
                        };
                    };
                    if request.etag.as_deref() == Some(etag.as_str()) {
                        return Reply {
                            status: 304,
                            etag: Some(etag),
                            ..Reply::default()
                        };
                    }
                    Reply {
                        status: 200,
                        etag: Some(etag),
                        body: serde_json::to_vec(&body).unwrap(),
                        ..Reply::default()
                    }
                })
            })
        }
    }

    fn seed_green(fake: &FakeGh, sha: &str) {
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v1",
            pr_body("OPEN", false, sha),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v1",
            checks(&[("build", "SUCCESS"), ("test", "SUCCESS")]),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/status",
            "status-v1",
            no_statuses(),
        );
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v1",
            reviews(&[]),
        );
        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v1",
            comments(&[]),
        );
        fake.set(
            "repos/acme/widget/pulls/27/comments",
            "review-comments-v1",
            comments(&[]),
        );
    }

    #[tokio::test]
    async fn a_transition_turns_into_one_exact_message_and_an_unchanged_poll_is_silent() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let link = link(true, 27);
        // Baseline: no turn, but the state was observed.
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(0))
                .await,
            None
        );
        let after_baseline = fake.requests().len();

        // A 304-only poll issues exactly one conditional request and no turn.
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None
        );
        let requests = fake.requests();
        assert_eq!(requests.len(), after_baseline + 1);
        assert_eq!(
            requests.last().unwrap().etag.as_deref(),
            Some("pr-v1"),
            "the second poll is the conditional request"
        );

        // Checks turn red: one message, header on line one, disclaimer last.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("OPEN", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE"), ("test", "FAILURE")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("a transition is one turn");
        assert!(message.starts_with(HEADER), "{message}");
        assert!(
            message.contains("PR #27 检查失败：build, test（2 项）"),
            "{message}"
        );
        assert!(message.ends_with(DISCLAIMER), "{message}");

        // The same observed state never fires again.
        assert_eq!(
            watcher.poll(&[link], &*fetch, Instant::now(), at(3)).await,
            None
        );
    }

    #[tokio::test]
    async fn a_new_comment_names_its_author_and_a_one_line_excerpt() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let link = link(true, 27);
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(0))
                .await,
            None
        );
        // A comment bumps the PR document (GitHub's own updated_at does this)
        // and is fetched once through the `since=` watermark.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("OPEN", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v2",
            comments(&[(
                1,
                "discord9",
                "2023-11-14T22:13:21Z",
                "这版先别合，测试挂了\r\n多跑一遍",
            )]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a new comment is one turn");
        assert!(message.starts_with(HEADER), "{message}");
        assert!(
            message.contains("PR #27 收到新评论\ndiscord9 · \"这版先别合，测试挂了 多跑一遍\""),
            "{message}"
        );
        // The same comment id is not announced twice.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v3",
            pr_body("OPEN", false, "abc123"),
        );
        assert_eq!(
            watcher.poll(&[link], &*fetch, Instant::now(), at(2)).await,
            None
        );
    }

    #[tokio::test]
    async fn changes_in_one_poll_coalesce_into_one_message() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        fake.set(
            "repos/acme/widget/issues/9",
            "issue-v1",
            json!({"state": "OPEN"}),
        );
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let pr = link(true, 27);
        let issue = link(false, 9);
        assert_eq!(
            watcher
                .poll(&[pr.clone(), issue.clone()], &*fetch, Instant::now(), at(0))
                .await,
            None
        );
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("OPEN", true, "abc123"),
        );
        fake.set(
            "repos/acme/widget/issues/9",
            "issue-v2",
            json!({"state": "CLOSED"}),
        );
        let message = watcher
            .poll(&[pr, issue], &*fetch, Instant::now(), at(1))
            .await
            .expect("one burst is one turn");
        assert!(message.contains("PR #27 已合并"), "{message}");
        assert!(message.contains("Issue #9 已关闭"), "{message}");
        // Both lines live in the same message, not two messages.
        assert_eq!(
            message
                .lines()
                .filter(|line| line.starts_with(HEADER))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn merging_or_closing_stops_future_polls() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        fake.set(
            "repos/acme/widget/issues/9",
            "issue-v1",
            json!({"state": "OPEN"}),
        );
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let pr = link(true, 27);
        let issue = link(false, 9);
        assert_eq!(
            watcher
                .poll(&[pr.clone(), issue.clone()], &*fetch, Instant::now(), at(0))
                .await,
            None
        );
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("CLOSED", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/issues/9",
            "issue-v2",
            json!({"state": "CLOSED"}),
        );
        let message = watcher
            .poll(&[pr.clone(), issue.clone()], &*fetch, Instant::now(), at(1))
            .await
            .unwrap();
        assert!(message.contains("PR #27 已关闭"), "{message}");
        let calls = fake.requests().len();
        // Both links are no longer watched: even a changed answer is never
        // requested again.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v9",
            pr_body("OPEN", false, "abc123"),
        );
        assert_eq!(
            watcher
                .poll(&[pr, issue], &*fetch, Instant::now(), at(2))
                .await,
            None
        );
        assert_eq!(fake.requests().len(), calls);
    }

    #[tokio::test]
    async fn a_rate_limit_backs_off_until_the_reset_instant() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        *fake.forced.lock().unwrap() = Some((429, Some(1_700_000_061)));
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let link = link(true, 27);
        let started = Instant::now();
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, started, at(0))
                .await,
            None
        );
        assert_eq!(
            fake.requests().len(),
            1,
            "one rejected request, no nested work"
        );
        assert_eq!(
            watcher.next_wait(POLL_INTERVAL, started),
            POLL_INTERVAL,
            "a reset inside the normal cadence does not extend the wait"
        );

        // Inside the window no request is attempted at all.
        *fake.forced.lock().unwrap() = None;
        let early = started + Duration::from_secs(30);
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, early, at(30))
                .await,
            None
        );
        assert_eq!(fake.requests().len(), 1);

        // After the reset the watcher reads again; this first success is a
        // baseline and still emits nothing for the past.
        let reset = started + Duration::from_secs(62);
        assert_eq!(watcher.poll(&[link], &*fetch, reset, at(62)).await, None);
        assert!(
            fake.requests().len() > 1,
            "the reset instant reopens polling"
        );
        assert!(
            fake.requests().last().unwrap().etag.is_none(),
            "the baseline request carries no If-None-Match"
        );
    }

    #[tokio::test]
    async fn review_decisions_and_legacy_status_contexts_are_transitions() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        // The baseline already carries one approval: only a NEW changes
        // requested decision is a transition.
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v1",
            reviews(&[(1, "alice", "APPROVED", "2023-11-14T22:13:19Z", "")]),
        );
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let link = link(true, 27);
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(0))
                .await,
            None
        );

        // A reviewer requests changes and a legacy commit status fails.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("OPEN", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v2",
            reviews(&[
                (1, "alice", "APPROVED", "2023-11-14T22:13:19Z", ""),
                (
                    2,
                    "bob",
                    "CHANGES_REQUESTED",
                    "2023-11-14T22:13:21Z",
                    "请先补测试",
                ),
            ]),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/status",
            "status-v2",
            statuses(&[("ci/build", "FAILURE")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("review + check transitions are one turn");
        assert!(message.contains("PR #27 要求修改"), "{message}");
        assert!(
            message.contains("PR #27 检查失败：ci/build（1 项）"),
            "{message}"
        );
        assert!(message.contains("bob · \"请先补测试\""), "{message}");
        assert!(message.ends_with(DISCLAIMER), "{message}");
    }

    #[test]
    fn a_rate_limit_window_outliving_the_cadence_extends_the_sleep() {
        let mut watcher = Watcher::default();
        let now = Instant::now();
        watcher.paused_until = Some(now + Duration::from_secs(900));
        assert_eq!(
            watcher.next_wait(POLL_INTERVAL, now),
            Duration::from_secs(900)
        );
        watcher.paused_until = Some(now + Duration::from_secs(30));
        assert_eq!(watcher.next_wait(POLL_INTERVAL, now), POLL_INTERVAL);
    }

    #[test]
    fn the_message_shape_is_exactly_the_documented_one() {
        assert_eq!(
            render(&[
                "PR #27 检查失败：build, test（2 项）".into(),
                "discord9 · \"这版先别合，测试挂了\"".into(),
            ]),
            "[自动检测 · 非用户消息] PR #27 检查失败：build, test（2 项）\n\
             discord9 · \"这版先别合，测试挂了\"\n\
             ——本消息由 PR 监视自动生成，不是用户发言；请据此判断并采取必要行动。"
        );
    }

    #[test]
    fn rate_limited_answers_parse_status_headers_and_body() {
        let reply = parse_gh_output(
            b"HTTP/2.0 403 Forbidden\r\nEtag: W/\"x\"\r\nX-RateLimit-Reset: 1700000060\r\n\r\n{\"message\":\"rate\"}",
        );
        assert_eq!(reply.status, 403);
        assert_eq!(reply.etag.as_deref(), Some("W/\"x\""));
        assert_eq!(reply.reset_at, Some(1_700_000_060));
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap()["message"],
            "rate"
        );
        assert_eq!(parse_gh_output(b"").status, 0);
        assert_eq!(
            parse_gh_output(b"HTTP/2.0 304 Not Modified\r\n\r\n").status,
            304
        );
    }
}
