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
//! - The first observation of a link is a SILENT baseline: it records the
//!   current state and the comment watermark and emits nothing for the past.
//!   A restart and a fresh add follow the same rule, so a restart never
//!   replays what changed while the process was down.
//! - An added link's baseline is the ADD-TIME observation of that link in
//!   that session, captured once; a display refresh updates the chip only and
//!   can never replace it.
//! - Notifications are new activity only: a transition or a new comment or
//!   review after the baseline. Nothing re-announces a state that was already
//!   true when the baseline was taken.
//! - A link is dropped as soon as it is MERGED or CLOSED, and a terminal link
//!   is never polled — or resurrected — again.
//! - A list read that cannot be proven complete is not an observation at all:
//!   the whole burst is skipped and the next poll retries. GitHub's list
//!   endpoints are ascending and ignore `sort`/`direction`, so a list is only
//!   ever read to its end (bounded), never assumed newest-first.
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

use crate::link_status::{self, ReviewDecision, Target};
use crate::session_store::SessionStore;

/// One poll every five minutes, the cadence the owner asked for.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// One `gh` call may not hang the watcher for longer than this.
const GH_TIMEOUT: Duration = Duration::from_secs(15);

/// A 403/429 without `X-RateLimit-Reset` still must not become a retry storm.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

/// How many pages one list read may follow, and how many items it may gather,
/// before the read is declared INCOMPLETE.
///
/// GitHub's list endpoints are ASCENDING and ignore `sort`/`direction` (for
/// reviews and issue comments the parameters are simply not honoured), so the
/// only way to know a list is whole is to walk it to its end. These caps bound
/// one pathological PR: a read that hits either cap answers `None`, which is
/// never reported as the whole list and never committed, and the next poll
/// tries again. A list that can *never* be walked within these caps (more than
/// 5000 reviews, or 5000 comments inside one `since` window) leaves that link
/// silent rather than reporting partial history — a deliberate trade, because a
/// partial list cannot tell new activity from old.
const MAX_PAGES: usize = 50;
const MAX_ITEMS: usize = 5000;

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
    /// The `Link` header verbatim; `rel="next"` is where page two lives.
    pub link: Option<String>,
    /// Epoch seconds from `X-RateLimit-Reset` (403/429 only).
    pub reset_at: Option<i64>,
    pub body: Vec<u8>,
}

/// The fetch seam: production runs `gh`, tests answer from a fixture.
pub(crate) type Fetch = dyn Fn(Request) -> BoxFuture<'static, Reply> + Send + Sync;

/// `PR acme/widget#27` / `Issue acme/widget#12`: the message names the
/// REPOSITORY as well as the number, so two repositories' `#27` can never be
/// mistaken for each other in one notice.
fn label(link: &Target) -> String {
    format!(
        "{} {}#{}",
        if link.pr { "PR" } else { "Issue" },
        link.repo,
        link.number
    )
}

/// The terminal states the watcher stops on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    Merged,
    Closed,
}

/// One observation of a link's meaningful state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Observed {
    terminal: Option<Terminal>,
    /// Names of checks currently failing (empty = none).
    failing: Vec<String>,
    review: Option<ReviewDecision>,
}

/// One resource's last successful read: the validator to hand back on the
/// next request, the document that validator stood for, and the page it
/// advertised so an unchanged first page still leads to its later pages.
#[derive(Clone, Debug)]
struct Resource {
    etag: Option<String>,
    doc: Value,
    next: Option<String>,
}

/// One `gh` answer: the document and where its next page lives.
struct Page {
    doc: Value,
    next: Option<String>,
}

/// One poll's staged reads. The PR document says nothing about a check that
/// failed on the same head SHA, so every independently mutable resource gets
/// its OWN validator: a `304` may skip parsing that resource only.
struct Reads<'a> {
    fetch: &'a Fetch,
    known: &'a HashMap<String, Resource>,
    fresh: HashMap<String, Resource>,
}

impl<'a> Reads<'a> {
    fn new(fetch: &'a Fetch, known: &'a HashMap<String, Resource>) -> Self {
        Self {
            fetch,
            known,
            fresh: HashMap::new(),
        }
    }

    /// The documents a SUCCESSFUL poll commits. A failed poll drops them, so
    /// the next poll re-reads from the previous validators instead of
    /// pretending it saw something it never compared.
    fn into_fresh(self) -> HashMap<String, Resource> {
        self.fresh
    }

    /// One conditional read. `304` reuses the last parsed document for THIS
    /// path only; a `304` for a resource that was never parsed is a hole in
    /// the observation, not a confirmation of it.
    async fn read(&mut self, path: String) -> Result<Page, Step> {
        let etag = self
            .known
            .get(&path)
            .and_then(|resource| resource.etag.clone());
        let reply = (self.fetch)(Request {
            path: path.clone(),
            etag,
        })
        .await;
        match reply.status {
            200 => {
                let doc = serde_json::from_slice::<Value>(&reply.body).map_err(|_| Step::Failed)?;
                let next = next_page(reply.link.as_deref());
                self.fresh.insert(
                    path,
                    Resource {
                        etag: reply.etag,
                        doc: doc.clone(),
                        next: next.clone(),
                    },
                );
                Ok(Page { doc, next })
            }
            304 => {
                let Some(known) = self.known.get(&path) else {
                    return Err(Step::Failed);
                };
                // The list is unchanged on THIS page, but its own advertised
                // next page may still have grown: keep following it.
                let page = Page {
                    doc: known.doc.clone(),
                    next: next_page(reply.link.as_deref()).or_else(|| known.next.clone()),
                };
                self.fresh.insert(
                    path,
                    Resource {
                        etag: known.etag.clone(),
                        doc: known.doc.clone(),
                        next: page.next.clone(),
                    },
                );
                Ok(page)
            }
            403 | 429 => Err(Step::RateLimited {
                reset_at: reply.reset_at,
            }),
            _ => Err(Step::Failed),
        }
    }

    /// One paginated list, followed to its END. GitHub's lists are ascending
    /// and cannot be reversed or cut at a watermark from the request side, so
    /// "the newest item is on page six" is the normal case for a busy PR and
    /// stopping early would simply lose it.
    ///
    /// `items` says where one page's items live. `None` means the walk hit
    /// [`MAX_PAGES`] or [`MAX_ITEMS`] first: the read is INCOMPLETE, and every
    /// caller treats it as a failed observation — no commit, no cursor, no
    /// transition, retry on the next poll. Returning a truncated list as if it
    /// were whole is the one thing this must never do.
    async fn walk(&mut self, path: String, items: Items) -> Result<Option<Vec<Value>>, Step> {
        let mut collected = Vec::new();
        let mut next = Some(path);
        let mut pages = 0;
        while let Some(path) = next {
            if pages == MAX_PAGES {
                return Ok(None);
            }
            pages += 1;
            let page = self.read(path).await?;
            let array = match items {
                Items::Whole => page.doc.as_array().cloned(),
                Items::Under(key) => page.doc.get(key).and_then(Value::as_array).cloned(),
            };
            let Some(array) = array else {
                return Err(Step::Failed);
            };
            collected.extend(array);
            if collected.len() > MAX_ITEMS {
                return Ok(None);
            }
            next = page.next;
        }
        Ok(Some(collected))
    }
}

/// Where one page's items live: the page document itself is the array (every
/// `list` endpoint), or they sit under a key (`check-runs`, `status`).
#[derive(Clone, Copy)]
enum Items {
    Whole,
    Under(&'static str),
}

/// The notes one poll wants to report, plus the dedup ids and the cursor it
/// must commit for them. Pure: nothing here is visible until the poll ends.
struct Notes {
    lines: Vec<String>,
    /// Ids at or after the live watermark; older ones can never be re-read.
    seen: Vec<(i64, DateTime<Utc>)>,
    /// The `since=` cursor this poll leaves behind (comment kinds only).
    cursor: Option<DateTime<Utc>>,
}

/// One persisted-in-memory cursor per link. It never leaves the process.
struct LinkWatch {
    link: Target,
    /// One validator per independently mutable REST resource, keyed by path.
    resources: HashMap<String, Resource>,
    prev: Option<Observed>,
    last_comment_at: Option<DateTime<Utc>>,
    last_review_comment_at: Option<DateTime<Utc>>,
    /// The fixed instant this watch began. A review older than it is history
    /// and can never be news; the reviews list is re-read without a `since=`
    /// request cursor, so this boundary never moves.
    review_since: DateTime<Utc>,
    /// Emitted note ids with their timestamps. This is what makes an
    /// inclusive `since=` window idempotent when a later poll covers the same
    /// second, and the timestamp is what lets the set be pruned back to the
    /// live boundary.
    seen: HashMap<(NoteKind, i64), DateTime<Utc>>,
}

impl LinkWatch {
    fn new(link: Target, wall: DateTime<Utc>) -> Self {
        Self {
            link,
            resources: HashMap::new(),
            prev: None,
            last_comment_at: None,
            last_review_comment_at: None,
            review_since: wall,
            seen: HashMap::new(),
        }
    }

    /// Seed the silent baseline from the ADD-TIME observation of this link in
    /// this session — never from a later display refresh, which would move
    /// the baseline past the very change the watcher exists to report. The
    /// first real poll still compares against it, so a change between the add
    /// and that poll is news; the add itself is not.
    fn seed(&mut self, wall: DateTime<Utc>, baseline: &link_status::WatchBaseline) {
        self.prev = Some(Observed {
            terminal: match baseline.terminal {
                Some("merged") => Some(Terminal::Merged),
                Some("closed") => Some(Terminal::Closed),
                _ => None,
            },
            failing: baseline.failing.clone(),
            review: baseline.review,
        });
        self.review_since = wall;
        // Everything before the add observation is history; only later
        // comments/reviews are news.
        self.last_comment_at = Some(wall);
        self.last_review_comment_at = Some(wall);
    }

    fn main_path(&self) -> String {
        let collection = if self.link.pr { "pulls" } else { "issues" };
        format!("repos/{}/{collection}/{}", self.link.repo, self.link.number)
    }

    fn watermark(&self, kind: NoteKind) -> Option<DateTime<Utc>> {
        match kind {
            NoteKind::Comment => self.last_comment_at,
            NoteKind::ReviewComment => self.last_review_comment_at,
            // Reviews have no `since=` request cursor: the watch-start
            // boundary is fixed, and every later review id is deduped.
            NoteKind::Review => Some(self.review_since),
        }
    }

    fn set_watermark(&mut self, kind: NoteKind, at: Option<DateTime<Utc>>) {
        match kind {
            NoteKind::Comment => self.last_comment_at = at,
            NoteKind::ReviewComment => self.last_review_comment_at = at,
            NoteKind::Review => {}
        }
    }

    /// One link's poll. Nothing a poll reads is committed until the WHOLE
    /// observation succeeded, so a `429` on the comments endpoint can never
    /// consume a review that same poll also failed to deliver: the next poll
    /// re-reads that review from the unchanged watermark and reports it.
    async fn poll(&mut self, fetch: &Fetch, wall: DateTime<Utc>) -> Step {
        let mut reads = Reads::new(fetch, &self.resources);
        let main = match reads.read(self.main_path()).await {
            Ok(page) => page,
            Err(step) => return step,
        };
        let Some(observed) = self.observe(&main.doc) else {
            return Step::Failed;
        };
        // A terminal link is never polled again. During a baseline it is
        // simply dropped; after a baseline it emits exactly one line.
        if let Some(terminal) = observed.terminal {
            let line = if self.prev.is_some() {
                let previous = self.prev.as_ref().and_then(|prev| prev.terminal);
                (previous != Some(terminal)).then(|| match terminal {
                    Terminal::Merged => format!("{} 已合并", label(&self.link)),
                    Terminal::Closed => format!("{} 已关闭", label(&self.link)),
                })
            } else {
                None
            };
            self.commit(reads.into_fresh(), observed, Vec::new());
            return match line {
                Some(line) => Step::EmitStop(vec![line]),
                None => Step::Stop,
            };
        }

        let baseline = self.prev.is_none();
        let mut observed = observed;
        let mut staged: Vec<(NoteKind, Notes)> = Vec::new();
        let mut reviewed: Vec<String> = Vec::new();
        if self.link.pr {
            let Some(sha) = main.doc.pointer("/head/sha").and_then(Value::as_str) else {
                return Step::Failed;
            };
            let sha = sha.to_owned();
            // Checks and statuses are small and both are paginated, so they
            // are followed to the end: a check that fails on page two is on
            // the SAME commit, so the PR document cannot vouch for it. A read
            // that cannot be completed is not an observation at all.
            let check_runs = match reads
                .walk(
                    format!(
                        "repos/{}/commits/{sha}/check-runs?per_page=100",
                        self.link.repo
                    ),
                    Items::Under("check_runs"),
                )
                .await
            {
                Ok(Some(items)) => items,
                Ok(None) => return Step::Failed,
                Err(step) => return step,
            };
            let statuses = match reads
                .walk(
                    format!("repos/{}/commits/{sha}/status?per_page=100", self.link.repo),
                    Items::Under("statuses"),
                )
                .await
            {
                Ok(Some(items)) => items,
                Ok(None) => return Step::Failed,
                Err(step) => return step,
            };
            observed.failing = failing_checks(&check_runs, &statuses);
            // The reviews endpoint has no `since=` cursor and ignores
            // `sort`/`direction`, so the list is read to its end and history
            // is filtered here (see `stage_notes`): a review that arrives
            // while the watcher runs is appended to the LAST page.
            let reviews = match reads
                .walk(
                    format!(
                        "repos/{}/pulls/{}/reviews?per_page=100",
                        self.link.repo, self.link.number
                    ),
                    Items::Whole,
                )
                .await
            {
                Ok(Some(items)) => items,
                Ok(None) => return Step::Failed,
                Err(step) => return step,
            };
            let review_notes = parse_notes(&Value::Array(reviews));
            let notes = self.stage_notes(review_notes, NoteKind::Review);
            reviewed = notes.lines.clone();
            staged.push((NoteKind::Review, notes));
        }

        if baseline {
            // Nothing before this instant may ever surface as news.
            self.last_comment_at = Some(wall);
            self.last_review_comment_at = Some(wall);
            self.commit(reads.into_fresh(), observed, staged);
            return Step::Idle;
        }

        let previous = self.prev.clone().unwrap_or_default();
        let mut lines = transitions(&self.link, &previous, &observed);
        let mut comments = Vec::new();

        // New issue/conversation comments. The endpoint's own `since=` filter
        // is the watermark (it is honoured, unlike `sort`/`direction`), so the
        // answer is ascending and only holds comments newer than the cursor;
        // the walk still runs to its end so a burst cannot be cut off, and ids
        // keep an inclusive `since=` window idempotent.
        if let Some(last) = self.last_comment_at {
            let path = format!(
                "repos/{}/issues/{}/comments?since={}&per_page=100",
                self.link.repo,
                self.link.number,
                since(last)
            );
            let items = match reads.walk(path, Items::Whole).await {
                Ok(Some(items)) => items,
                Ok(None) => return Step::Failed,
                Err(step) => return step,
            };
            let notes = self.stage_notes(parse_notes(&Value::Array(items)), NoteKind::Comment);
            comments.extend(notes.lines.iter().cloned());
            staged.push((NoteKind::Comment, notes));
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
            let items = match reads.walk(path, Items::Whole).await {
                Ok(Some(items)) => items,
                Ok(None) => return Step::Failed,
                Err(step) => return step,
            };
            let notes =
                self.stage_notes(parse_notes(&Value::Array(items)), NoteKind::ReviewComment);
            comments.extend(notes.lines.iter().cloned());
            staged.push((NoteKind::ReviewComment, notes));
        }

        // New reviews (with a body) and comments share one display shape.
        // Only mention a comment burst when no transition already names the
        // link; the sample shape stays one transition line + one comment.
        comments.extend(reviewed);
        if lines.is_empty() && !comments.is_empty() {
            lines.push(format!("{} 收到新评论", label(&self.link)));
        }
        lines.extend(comments);
        self.commit(reads.into_fresh(), observed, staged);
        if lines.is_empty() {
            Step::Idle
        } else {
            Step::Emit(lines)
        }
    }

    /// Commit one poll: the staged documents, the state they were compared
    /// against, and the note cursors/dedup ids all move together here.
    fn commit(
        &mut self,
        fresh: HashMap<String, Resource>,
        observed: Observed,
        staged: Vec<(NoteKind, Notes)>,
    ) {
        self.resources = fresh;
        self.prev = Some(observed);
        for (kind, notes) in staged {
            for (id, at) in notes.seen {
                self.seen.insert((kind, id), at);
            }
            if notes.cursor.is_some() {
                self.set_watermark(kind, notes.cursor);
            }
        }
        self.prune();
    }

    /// Keep dedup ids only within each kind's live `since` boundary: a note
    /// older than the watermark can never be re-read, so remembering its id
    /// is pure growth. Reviews have no request cursor, so every review id is
    /// kept: the full list is re-read on every poll and `seen` alone keeps
    /// the PR's history silent.
    fn prune(&mut self) {
        let comment = self.last_comment_at;
        let review_comment = self.last_review_comment_at;
        self.seen.retain(|(kind, _), at| {
            let since = match kind {
                NoteKind::Comment => comment,
                NoteKind::ReviewComment => review_comment,
                NoteKind::Review => return true,
            };
            since.is_none_or(|since| *at >= since)
        });
    }

    fn observe(&self, doc: &Value) -> Option<Observed> {
        if self.link.pr {
            observe_pr(doc)
        } else {
            observe_issue(doc)
        }
    }

    /// Notes newer than this kind's watermark and not already emitted, with
    /// the ids and cursor the commit will record. `lines` carries the display
    /// form (author + excerpt).
    fn stage_notes(&self, notes: Vec<Note>, kind: NoteKind) -> Notes {
        let watermark = self.watermark(kind);
        let mut lines = Vec::new();
        let mut seen = Vec::new();
        // Reviews have a fixed history boundary, not a cursor to advance.
        let mut cursor = if kind == NoteKind::Review {
            None
        } else {
            watermark
        };
        for note in notes {
            if watermark.is_some_and(|since| note.at < since) {
                continue;
            }
            if note.id != 0 && self.seen.contains_key(&(kind, note.id)) {
                continue;
            }
            if note.id != 0 {
                seen.push((note.id, note.at));
            }
            if kind != NoteKind::Review {
                cursor = Some(cursor.map_or(note.at, |current| current.max(note.at)));
            }
            // A review without a body is already represented by its state
            // transition (approved / changes requested); an empty excerpt
            // would be noise.
            if let Some(line) = comment_line(&note.author, &note.body) {
                lines.push(line);
            }
        }
        Notes {
            lines,
            seen,
            cursor,
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum NoteKind {
    Comment,
    ReviewComment,
    Review,
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
    /// The session whose links are watched: an add-time baseline belongs to
    /// one session's add, never to the process.
    session: String,
    links: Vec<LinkWatch>,
    paused_until: Option<Instant>,
    /// Terminal links are remembered so a later poll of the same stored URL
    /// does not create a fresh, unbaselined watch for a merged/closed ref.
    done: HashSet<String>,
    /// The first sync baselines everything already present (a restart must
    /// not report the past). Links that appear LATER are adds, and may seed
    /// from THAT session's add-time observation instead.
    started: bool,
}

impl Watcher {
    /// One watcher for one session.
    pub(crate) fn for_session(session: &str) -> Self {
        Self {
            session: session.to_owned(),
            ..Self::default()
        }
    }

    /// Poll every currently linked open PR/issue once. Returns exactly one
    /// message for the whole burst, or `None` when nothing changed.
    pub(crate) async fn poll(
        &mut self,
        links: &[Target],
        fetch: &Fetch,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> Option<String> {
        // The first poll after start is a RESTART: nothing is replayed from
        // before it. Every link's own first observation becomes its baseline,
        // however it changed while the process was down. A link added later
        // is the only kind that arrives with an observation of its own (the
        // add-time one), and that is a silent baseline too.
        let initial = !self.started;
        self.sync(links, initial, wall);
        self.started = true;
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
                    stopped.push(watch.link.url.to_owned());
                }
                Step::Stop => stopped.push(watch.link.url.to_owned()),
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
    ///
    /// One rule covers restart and add: the first observation of a link is a
    /// SILENT baseline, never news. A restart has no observation to inherit,
    /// so the link is baselined by its own first poll; a link that appears
    /// while the watcher runs is an ADD and may seed from that session's
    /// add-time observation instead. A terminal add is dropped here, and only
    /// an add — never a later poll — can resurrect it (see `done`).
    fn sync(&mut self, links: &[Target], initial: bool, wall: DateTime<Utc>) {
        // `done` exists to keep a STILL-LINKED merged ref from restarting a
        // watch, not to remember every ref this process ever finished: an
        // unlinked URL is forgotten here so the set stays bounded by the
        // session's live links.
        self.done
            .retain(|url| links.iter().any(|link| &link.url == url));
        let mut existing: HashMap<String, LinkWatch> = self
            .links
            .drain(..)
            .map(|watch| (watch.link.url.to_owned(), watch))
            .collect();
        for link in links {
            if self.done.contains(&link.url) {
                continue;
            }
            let watch = match existing.remove(&link.url) {
                Some(watch) => watch,
                None => {
                    let mut watch = LinkWatch::new(link.clone(), wall);
                    if !initial
                        && let Some((wall, status)) = link_status::add_baseline(
                            &self.session,
                            if link.pr { "pr" } else { "issue" },
                            &link.url,
                        )
                        && let Some(baseline) = &status.watch
                    {
                        watch.seed(wall, baseline);
                        if baseline.terminal.is_some() {
                            self.done.insert(link.url.to_owned());
                            continue;
                        }
                    }
                    watch
                }
            };
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
    let mut watcher = Watcher::for_session(&session);
    loop {
        let wall = Utc::now();
        let now = Instant::now();
        match store.list_refs(&root, &session).await {
            Ok(refs) => {
                let links = refs
                    .iter()
                    .filter_map(|reference| link_status::target(&reference.kind, &reference.url))
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
        // The AGGREGATE decision GitHub publishes for the PR — the same field
        // the chip reads — so the watcher can never claim "approved" where
        // GitHub's own review requirement still holds the PR at
        // `REVIEW_REQUIRED`. Individual votes are never summed here.
        review: aggregate_review(doc),
    })
}

/// GitHub's aggregate review decision on a pull-request document. An absent
/// or unrecognised value (`REVIEW_REQUIRED`, an empty string) is no decision:
/// silence, never a guess.
fn aggregate_review(doc: &Value) -> Option<ReviewDecision> {
    doc.get("reviewDecision")
        .and_then(Value::as_str)
        .and_then(ReviewDecision::from_token)
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
fn transitions(link: &Target, previous: &Observed, now: &Observed) -> Vec<String> {
    let mut lines = Vec::new();
    if now.review == Some(ReviewDecision::Changes)
        && previous.review != Some(ReviewDecision::Changes)
    {
        lines.push(format!("{} 要求修改", label(link)));
    } else if now.review == Some(ReviewDecision::Approved)
        && previous.review != Some(ReviewDecision::Approved)
    {
        lines.push(format!("{} 已批准", label(link)));
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
            label(link),
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
fn failing_checks(check_runs: &[Value], statuses: &[Value]) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |name: String| {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    };
    for run in check_runs {
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
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE"
        ) {
            push(
                run.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
    }
    for context in statuses {
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
    // The watcher is cancelled by aborting its task; without this the `gh`
    // child would keep running after the session is gone.
    command.kill_on_drop(true);
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
            "link" => reply.link = Some(value.trim().to_owned()),
            "x-ratelimit-reset" => reply.reset_at = value.trim().parse().ok(),
            _ => {}
        }
    }
    reply
}

/// The `rel="next"` target of a `Link` header, as a `gh api` path. `gh`
/// accepts the absolute URL too, but a path keeps the request log (and the
/// test seam) readable.
fn next_page(link: Option<&str>) -> Option<String> {
    for entry in link?.split(',') {
        let entry = entry.trim();
        if !entry.contains("rel=\"next\"") {
            continue;
        }
        let rest = entry.strip_prefix('<')?;
        let end = rest.find('>')?;
        let target = &rest[..end];
        return Some(
            target
                .strip_prefix("https://api.github.com/")
                .unwrap_or(target)
                .to_owned(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex as StdMutex};

    fn link(pr: bool, number: u64) -> Target {
        let (kind, action) = if pr {
            ("pr", "pull")
        } else {
            ("issue", "issues")
        };
        crate::link_status::target(
            kind,
            &format!("https://github.com/acme/widget/{action}/{number}"),
        )
        .expect("a watcher test link is a GitHub PR/issue")
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
    }

    fn pr_body(state: &str, merged: bool, sha: &str) -> Value {
        pr_body_review(state, merged, sha, None)
    }

    /// A pull-request document with GitHub's AGGREGATE review decision, the
    /// field the chip reads. `None` is "no decision" (`REVIEW_REQUIRED`,
    /// which the watcher must never read as approval).
    fn pr_body_review(state: &str, merged: bool, sha: &str, review: Option<&str>) -> Value {
        json!({
            "state": state,
            "merged": merged,
            "draft": false,
            "head": {"sha": sha},
            "reviewDecision": review,
        })
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

    /// The reviews endpoint answers ASCENDING by `submitted_at` and ignores
    /// `sort`/`direction`, so the fixture always answers in that order: a
    /// fixture that reversed it would model a response GitHub never sends.
    fn reviews(items: &[(i64, &str, &str, &str, &str)]) -> Value {
        let mut rows = items.to_vec();
        rows.sort_by(|a, b| a.3.cmp(b.3).then(a.0.cmp(&b.0)));
        json!(
            rows.iter()
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

    /// A tiny ETag-aware GitHub: responses are served per path (the full
    /// path with its query first, then its prefix so one `set` covers every
    /// page), and a request carrying the stored ETag answers 304. Every
    /// request is recorded so a test can assert the conditional
    /// short-circuit.
    type ForcedReply = Arc<StdMutex<Option<(u16, Option<i64>)>>>;

    #[derive(Clone, Default)]
    struct FakeGh {
        responses: Arc<StdMutex<HashMap<String, (String, Value)>>>,
        /// `Link` headers by path, for paginated lists.
        links: Arc<StdMutex<HashMap<String, String>>>,
        requests: Arc<StdMutex<Vec<Request>>>,
        /// When set, every request answers this status (rate-limit tests).
        forced: ForcedReply,
        /// Answers 429 for exactly this path — a rate limit that hits ONE
        /// endpoint mid-poll.
        limited: Arc<StdMutex<Option<String>>>,
        /// Answers 500 for exactly this path — a non-rate-limit failure that
        /// must not consume anything the poll read before it.
        broken: Arc<StdMutex<Option<String>>>,
        /// 304 answers, so a test can prove a resource was revalidated.
        not_modified: Arc<StdMutex<usize>>,
    }

    impl FakeGh {
        fn set(&self, key: &str, etag: &str, body: Value) {
            self.responses
                .lock()
                .unwrap()
                .insert(key.into(), (etag.into(), body));
        }

        /// Serve `path` with a `rel="next"` page at `next`.
        fn link(&self, path: &str, next: &str) {
            self.links.lock().unwrap().insert(
                path.into(),
                format!("<https://api.github.com/{next}>; rel=\"next\""),
            );
        }

        fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        fn not_modified(&self) -> usize {
            *self.not_modified.lock().unwrap()
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
                    if fake.limited.lock().unwrap().as_deref() == Some(request.path.as_str()) {
                        return Reply {
                            status: 429,
                            ..Reply::default()
                        };
                    }
                    if fake.broken.lock().unwrap().as_deref() == Some(request.path.as_str()) {
                        return Reply {
                            status: 500,
                            ..Reply::default()
                        };
                    }
                    let base = request.path.split('?').next().unwrap_or_default();
                    let served = {
                        let responses = fake.responses.lock().unwrap();
                        responses
                            .get(&request.path)
                            .or_else(|| responses.get(base))
                            .cloned()
                    };
                    let Some((etag, body)) = served else {
                        return Reply {
                            status: 404,
                            ..Reply::default()
                        };
                    };
                    let link = {
                        let links = fake.links.lock().unwrap();
                        links
                            .get(&request.path)
                            .or_else(|| links.get(base))
                            .cloned()
                    };
                    if request.etag.as_deref() == Some(etag.as_str()) {
                        *fake.not_modified.lock().unwrap() += 1;
                        return Reply {
                            status: 304,
                            etag: Some(etag),
                            link,
                            ..Reply::default()
                        };
                    }
                    Reply {
                        status: 200,
                        etag: Some(etag),
                        link,
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

        // An unchanged poll only REVALIDATES: every independently mutable
        // resource carries its own ETag, so each request is conditional and
        // a 304 skips that resource's parsing. Nothing changed, so no turn.
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None
        );
        let requests = fake.requests();
        assert!(requests.len() > after_baseline);
        for request in &requests[after_baseline..] {
            // Comment lists are conditioned by their `since=` watermark;
            // every other resource revalidates with If-None-Match.
            assert!(
                request.etag.is_some() || request.path.contains("?since="),
                "an unchanged poll only sends conditional reads: {request:?}"
            );
        }

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
            message.contains("PR acme/widget#27 检查失败：build, test（2 项）"),
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
            message.contains(
                "PR acme/widget#27 收到新评论\ndiscord9 · \"这版先别合，测试挂了 多跑一遍\""
            ),
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
        assert!(message.contains("PR acme/widget#27 已合并"), "{message}");
        assert!(message.contains("Issue acme/widget#9 已关闭"), "{message}");
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
        assert!(message.contains("PR acme/widget#27 已关闭"), "{message}");
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
    async fn an_added_link_baselines_from_its_add_time_observation_silently() {
        use crate::link_status::{NEEDS, OPEN, READY, seed_baseline_for_test, status_for_test};

        let added = crate::link_status::target("pr", "https://github.com/acme/widget/pull/77")
            .expect("a GitHub PR target");
        // The first sync of a session has no add-time observation: the
        // existing link is baselined by its own first poll.
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // Add #77 while the watcher is live: the status prewarm observed it
        // as OPEN with no failures. The watcher's built-in poll (now) sees
        // that as its baseline, so the add itself emits nothing.
        seed_baseline_for_test(
            "",
            "pr",
            &added.url,
            status_for_test(OPEN, None, Vec::new(), None),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&added), &*fetch, Instant::now(), at(1))
                .await,
            None
        );

        // The first transition after the add (checks red now) is one turn.
        fake.set(
            "repos/acme/widget/pulls/77",
            "pr-v1",
            pr_body("OPEN", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v1",
            checks(&[("build", "FAILURE")]),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/status",
            "status-v1",
            no_statuses(),
        );
        fake.set(
            "repos/acme/widget/pulls/77/reviews",
            "reviews-v1",
            reviews(&[(
                1,
                "alice",
                "COMMENTED",
                "2023-11-14T22:13:18Z",
                "添加之前的旧评审",
            )]),
        );
        fake.set(
            "repos/acme/widget/issues/77/comments",
            "comments-v1",
            comments(&[]),
        );
        fake.set(
            "repos/acme/widget/pulls/77/comments",
            "review-comments-v1",
            comments(&[]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&added), &*fetch, Instant::now(), at(2))
            .await
            .expect("the first later transition is one turn");
        assert!(
            message.contains("PR acme/widget#77 检查失败：build（1 项）"),
            "{message}"
        );
        assert!(
            !message.contains("添加之前的旧评审"),
            "a review older than the add observation stays silent: {message}"
        );

        // A second add whose cached observation already IS the current state
        // is a pure baseline: no turn.
        let stable = crate::link_status::target("pr", "https://github.com/acme/widget/pull/78")
            .expect("a GitHub PR target");
        let fake2 = FakeGh::default();
        fake2.set(
            "repos/acme/widget/pulls/78",
            "pr-v1",
            pr_body("OPEN", false, "def456"),
        );
        fake2.set(
            "repos/acme/widget/commits/def456/check-runs",
            "checks-v1",
            checks(&[("build", "FAILURE")]),
        );
        fake2.set(
            "repos/acme/widget/commits/def456/status",
            "status-v1",
            no_statuses(),
        );
        fake2.set(
            "repos/acme/widget/pulls/78/reviews",
            "reviews-v1",
            reviews(&[]),
        );
        fake2.set(
            "repos/acme/widget/issues/78/comments",
            "comments-v1",
            comments(&[]),
        );
        fake2.set(
            "repos/acme/widget/pulls/78/comments",
            "review-comments-v1",
            comments(&[]),
        );
        seed_baseline_for_test(
            "",
            "pr",
            &stable.url,
            status_for_test(NEEDS, None, vec!["build".into()], None),
        );
        let fetch2 = fake2.fetch();
        let mut watcher2 = Watcher::default();
        assert_eq!(
            watcher2.poll(&[], &*fetch2, Instant::now(), at(0)).await,
            None
        );
        assert_eq!(
            watcher2
                .poll(
                    std::slice::from_ref(&stable),
                    &*fetch2,
                    Instant::now(),
                    at(1)
                )
                .await,
            None,
            "adding a link whose cached state is unchanged is silent"
        );
        // The seeded baseline is real: a later green state is not a trigger
        // (only failures are), but a NEW failing name is.
        fake2.set(
            "repos/acme/widget/commits/def456/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE"), ("test", "FAILURE")]),
        );
        let message = watcher2
            .poll(
                std::slice::from_ref(&stable),
                &*fetch2,
                Instant::now(),
                at(2),
            )
            .await
            .expect("a newly failing check after the add is one turn");
        assert!(message.contains("检查失败：test（1 项）"), "{message}");
        let _ = READY;
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

    /// The review transition comes from the AGGREGATE decision the PR
    /// document carries — the same field the chip reads — never from summing
    /// individual votes.
    #[tokio::test]
    async fn review_decisions_and_legacy_status_contexts_are_transitions() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        // The add already saw an approval, so the baseline carries APPROVED:
        // only a NEW changes-requested decision is a transition.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v1",
            pr_body_review("OPEN", false, "abc123", Some("APPROVED")),
        );
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
            pr_body_review("OPEN", false, "abc123", Some("CHANGES_REQUESTED")),
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
        assert!(message.contains("PR acme/widget#27 要求修改"), "{message}");
        assert!(
            message.contains("PR acme/widget#27 检查失败：ci/build（1 项）"),
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

    /// Blocker 1: the PR document is NOT a validator for its checks. A check
    /// that fails on the SAME head SHA leaves the PR representation byte for
    /// byte unchanged, so a `304` on `pulls/27` may skip parsing that one
    /// document — the check endpoints are still read, and the failure is
    /// still delivered.
    #[tokio::test]
    async fn a_check_failing_on_an_unchanged_pr_document_is_still_reported() {
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

        // Only the check run changes. `pulls/27` keeps `pr-v1`, so the next
        // poll's conditional request for it answers 304.
        let before = fake.not_modified();
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a check-only change is still one turn");
        assert!(
            message.contains("PR acme/widget#27 检查失败：build（1 项）"),
            "{message}"
        );
        assert!(
            fake.not_modified() > before,
            "the PR document really was revalidated (304), not re-fetched"
        );
    }

    /// A comment is not part of the PR document: the main document can keep
    /// its ETag and answer 304 while the comments endpoint has news. The poll
    /// must still descend and report it.
    #[tokio::test]
    async fn a_comment_on_an_unchanged_pr_document_is_still_reported() {
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

        let before = fake.not_modified();
        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v2",
            comments(&[(
                1,
                "alice",
                "2023-11-14T22:13:21Z",
                "主文档没动，但这里有一条新评论",
            )]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a comment-only change is still one turn");
        assert!(
            message.contains("PR acme/widget#27 收到新评论"),
            "{message}"
        );
        assert!(
            message.contains("alice · \"主文档没动，但这里有一条新评论\""),
            "{message}"
        );
        assert!(
            fake.not_modified() > before,
            "the PR document really was revalidated (304), not re-fetched"
        );
    }

    /// A failed comment read must not commit the main document's fresh
    /// validators either: the next poll re-reads from the last GOOD state
    /// (which answers 304), then delivers the comment exactly once.
    #[tokio::test]
    async fn a_failed_comment_read_followed_by_a_304_loses_and_duplicates_nothing() {
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

        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v2",
            comments(&[(1, "alice", "2023-11-14T22:13:21Z", "失败一次也不能丢")]),
        );
        *fake.broken.lock().unwrap() = Some(format!(
            "repos/acme/widget/issues/27/comments?since={}&per_page=100",
            since(at(0))
        ));
        let before = fake.not_modified();
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a failed nested read delivers nothing"
        );
        assert!(
            fake.not_modified() > before,
            "the main document answered 304 while the comment read failed"
        );

        // The failed poll committed no cursor and no fresh validators, so the
        // comment is still news now — and the main document still revalidates.
        *fake.broken.lock().unwrap() = None;
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("the comment read that failed is still news");
        assert!(
            message.contains("alice · \"失败一次也不能丢\""),
            "{message}"
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "the comment is not reported twice"
        );
    }

    /// The AGGREGATE decision drives the transition. A dismissal moves
    /// GitHub back to APPROVED, and only that move is the news — a
    /// changes-requested review that GitHub cleared is not.
    #[tokio::test]
    async fn a_cleared_changes_request_then_an_aggregate_approval_is_a_transition() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v1",
            pr_body_review("OPEN", false, "abc123", Some("CHANGES_REQUESTED")),
        );
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v1",
            reviews(&[(
                1,
                "alice",
                "CHANGES_REQUESTED",
                "2023-11-14T22:13:19Z",
                "请先补测试",
            )]),
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

        // Alice's review is DISMISSED and Bob approves, so GitHub's aggregate
        // decision is APPROVED.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body_review("OPEN", false, "abc123", Some("APPROVED")),
        );
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v2",
            reviews(&[
                (
                    1,
                    "alice",
                    "CHANGES_REQUESTED",
                    "2023-11-14T22:13:19Z",
                    "请先补测试",
                ),
                (3, "alice", "DISMISSED", "2023-11-14T22:13:20Z", ""),
                (2, "bob", "APPROVED", "2023-11-14T22:13:21Z", ""),
            ]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a cleared changes-requested + approval is one transition");
        assert!(message.contains("PR acme/widget#27 已批准"), "{message}");
        assert!(!message.contains("要求修改"), "{message}");
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "the approval is not repeated"
        );
    }

    /// One approval on a branch that needs two leaves GitHub at
    /// `REVIEW_REQUIRED`. The watcher reports the APPROVAL as news only when
    /// GitHub's aggregate says so — never by summing votes.
    #[tokio::test]
    async fn one_approval_on_a_two_approval_branch_is_not_approved() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v1",
            pr_body_review("OPEN", false, "abc123", Some("REVIEW_REQUIRED")),
        );
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

        // Bob approves too — still one short of two approvers, so GitHub keeps
        // the PR at REVIEW_REQUIRED and the watcher must say nothing.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body_review("OPEN", false, "abc123", Some("REVIEW_REQUIRED")),
        );
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v2",
            reviews(&[
                (1, "alice", "APPROVED", "2023-11-14T22:13:19Z", ""),
                (2, "bob", "APPROVED", "2023-11-14T22:13:21Z", ""),
            ]),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a single approval is not the PR being approved"
        );

        // The second approval flips the aggregate: exactly one transition.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v3",
            pr_body_review("OPEN", false, "abc123", Some("APPROVED")),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("the aggregate approval is one transition");
        assert!(message.contains("PR acme/widget#27 已批准"), "{message}");
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "the approval is not repeated"
        );
    }

    /// The add-time baseline is captured once. A manual refresh (a DISPLAY
    /// observation) that lands between the add and the first poll must not
    /// become the baseline, or the change it shows would never be delivered.
    #[tokio::test]
    async fn a_manual_refresh_before_the_first_poll_keeps_the_add_time_baseline() {
        use crate::link_status::{
            NEEDS, READY, seed_baseline_for_test, seed_for_test, status_for_test,
        };

        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );
        let link = link(true, 27);

        // The add saw a green PR…
        seed_baseline_for_test(
            "",
            "pr",
            &link.url,
            status_for_test(READY, None, Vec::new(), None),
        );
        // …then a check failed, and the human refreshed the chip before the
        // watcher's first poll: the RED value is a display value only.
        seed_for_test(
            "pr",
            &link.url,
            status_for_test(NEEDS, None, vec!["build".into()], None),
        );
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("OPEN", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE"), ("test", "SUCCESS")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("the green→red change survives the refresh");
        assert!(
            message.contains("PR acme/widget#27 检查失败：build（1 项）"),
            "{message}"
        );
    }

    /// A merge between the add and the first poll is news, not something the
    /// watcher swallows by treating the refreshed MERGED value as its
    /// baseline.
    #[tokio::test]
    async fn a_merge_before_the_first_poll_is_delivered() {
        use crate::link_status::{
            MERGED, READY, seed_baseline_for_test, seed_for_test, status_for_test,
        };

        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );
        let link = link(true, 27);

        seed_baseline_for_test(
            "",
            "pr",
            &link.url,
            status_for_test(READY, None, Vec::new(), None),
        );
        // The chip was refreshed after the merge; that is display-only.
        seed_for_test(
            "pr",
            &link.url,
            status_for_test(MERGED, Some("merged"), Vec::new(), None),
        );
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("CLOSED", true, "abc123"),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a merge before the first poll is one notice");
        assert!(message.contains("PR acme/widget#27 已合并"), "{message}");
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "a merged link is never polled again"
        );
    }

    /// A restart must not produce notifications. Watcher state is in memory,
    /// so after a restart every link is "new": its first observation is a
    /// silent baseline, whatever a previous process (or an earlier add) once
    /// recorded. Nothing that changed while the process was down is replayed,
    /// and only a change AFTER that baseline notifies.
    #[tokio::test]
    async fn a_restart_baselines_silently_and_never_replays_the_past() {
        use crate::link_status::{READY, seed_baseline_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        // The PR is already red and the issue is already closed at start-up.
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v1",
            checks(&[("build", "FAILURE"), ("test", "SUCCESS")]),
        );
        fake.set(
            "repos/acme/widget/issues/9",
            "issue-v1",
            json!({"state": "CLOSED"}),
        );
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let pr = link(true, 27);
        let issue = link(false, 9);
        // What the previous process knew: #27 was green. After a restart that
        // memory is not a baseline — the first observation is.
        seed_baseline_for_test(
            "",
            "pr",
            &pr.url,
            status_for_test(READY, None, Vec::new(), None),
        );
        assert_eq!(
            watcher
                .poll(&[pr.clone(), issue.clone()], &*fetch, Instant::now(), at(0))
                .await,
            None,
            "a restart replays nothing: no failure storm, no 已关闭 notice"
        );

        // One genuine change after the baseline is exactly one notice.
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE"), ("test", "FAILURE")]),
        );
        let message = watcher
            .poll(&[pr.clone(), issue], &*fetch, Instant::now(), at(1))
            .await
            .expect("a change after the baseline is one notice");
        assert!(
            message.contains("PR acme/widget#27 检查失败：test（1 项）"),
            "{message}"
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&pr), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "the same state is never re-announced"
        );
    }

    /// Blocker 2: a poll that read a review and then hit a 429 on the
    /// comments endpoint must not consume the review. Nothing is marked seen
    /// or watermarked until the whole observation succeeded, so after the
    /// reset the same review is news — exactly once.
    #[tokio::test]
    async fn a_rate_limited_comment_read_does_not_consume_the_review_it_read() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
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

        // A new review arrived; the comments endpoint is rate limited in the
        // same poll.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-v2",
            pr_body("OPEN", false, "abc123"),
        );
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v2",
            reviews(&[(
                2,
                "bob",
                "COMMENTED",
                "2023-11-14T22:13:21Z",
                "看了下，还没有结论",
            )]),
        );
        *fake.limited.lock().unwrap() = Some(
            "repos/acme/widget/issues/27/comments?since=2023-11-14T22:13:20Z&per_page=100".into(),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, started, at(1))
                .await,
            None,
            "a rate-limited poll delivers nothing"
        );

        // After the reset the review is delivered, and delivered once.
        *fake.limited.lock().unwrap() = None;
        let message = watcher
            .poll(
                std::slice::from_ref(&link),
                &*fetch,
                started + Duration::from_secs(120),
                at(2),
            )
            .await
            .expect("the review read before the 429 is still news");
        assert!(
            message.contains("bob · \"看了下，还没有结论\""),
            "{message}"
        );
        assert!(
            message.contains("PR acme/widget#27 收到新评论"),
            "{message}"
        );
        assert_eq!(
            watcher
                .poll(
                    std::slice::from_ref(&link),
                    &*fetch,
                    started + Duration::from_secs(240),
                    at(3)
                )
                .await,
            None,
            "the review is not reported twice"
        );
    }

    /// A read that cannot be proven complete is not an observation: the burst
    /// is skipped, nothing is committed, and the next poll retries. A
    /// truncated list reported as whole would hide the failing run forever.
    #[tokio::test]
    async fn an_incomplete_check_list_is_skipped_and_retried() {
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

        // A pathological commit: more pages of checks than one poll may read,
        // with the failing run past the cap.
        let list = "repos/acme/widget/commits/abc123/check-runs?per_page=100";
        for page in 1..=MAX_PAGES + 1 {
            let path = if page == 1 {
                list.to_owned()
            } else {
                format!("{list}&page={page}")
            };
            let body = if page == MAX_PAGES + 1 {
                checks(&[("build", "FAILURE")])
            } else {
                checks(&[("build", "SUCCESS")])
            };
            fake.set(&path, &format!("checks-{page}"), body);
            if page <= MAX_PAGES {
                fake.link(&path, &format!("{list}&page={}", page + 1));
            }
        }
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a truncated list is never reported as the whole one"
        );

        // The list fits again: the failure is news now, and only once.
        fake.links.lock().unwrap().clear();
        fake.set(list, "checks-ok", checks(&[("build", "SUCCESS")]));
        fake.set(
            &format!("{list}&page=2"),
            "checks-ok-p2",
            checks(&[("build", "FAILURE")]),
        );
        fake.link(list, &format!("{list}&page=2"));
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("the retry after an incomplete read still reports the failure");
        assert!(
            message.contains("PR acme/widget#27 检查失败：build（1 项）"),
            "{message}"
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "the failure is not reported twice"
        );
    }

    /// Serve an ascending paginated review list at `base`, a hundred items
    /// per page, chaining `rel="next"` the way GitHub does.
    fn paginate_reviews(
        fake: &FakeGh,
        base: &str,
        version: &str,
        items: &[(i64, &str, &str, &str, &str)],
    ) {
        let query = "per_page=100";
        let pages = items.len().div_ceil(100).max(1);
        for page in 1..=pages {
            let start = (page - 1) * 100;
            let end = (start + 100).min(items.len());
            let path = if page == 1 {
                format!("{base}?{query}")
            } else {
                format!("{base}?{query}&page={page}")
            };
            fake.set(
                &path,
                &format!("{base}-{version}-p{page}"),
                reviews(&items[start..end]),
            );
            if page < pages {
                fake.link(&path, &format!("{base}?{query}&page={}", page + 1));
            }
        }
    }

    /// The comment endpoints cannot be reversed either: `sort`/`direction`
    /// are ignored, so a burst that spills onto page two is only complete if
    /// the walk follows `rel="next"` to the end.
    #[tokio::test]
    async fn a_comment_on_the_second_page_is_still_reported() {
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

        // A hundred and fifty comments land at once: the newest is on page
        // two, exactly where an ascending list puts it.
        let base = "repos/acme/widget/issues/27/comments";
        let query = format!("?since={}&per_page=100", since(at(0)));
        let page = |page: usize| {
            if page == 1 {
                format!("{base}{query}")
            } else {
                format!("{base}{query}&page={page}")
            }
        };
        let first: Vec<(i64, &str, &str, &str)> = (1..=100)
            .map(|id| (id, "bot", "2023-11-14T22:13:21Z", ""))
            .collect();
        let mut second: Vec<(i64, &str, &str, &str)> = (101..=150)
            .map(|id| (id, "bot", "2023-11-14T22:13:22Z", ""))
            .collect();
        second.last_mut().unwrap().1 = "alice";
        second.last_mut().unwrap().3 = "第二页才有的评论";
        fake.set(&page(1), "comments-p1", comments(&first));
        fake.set(&page(2), "comments-p2", comments(&second));
        fake.link(&page(1), &page(2));

        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a comment on a later page is still news");
        assert!(
            message.contains("alice · \"第二页才有的评论\""),
            "{message}"
        );
        assert!(
            fake.requests()
                .iter()
                .any(|request| request.path == page(2)),
            "the second page was followed"
        );
    }

    /// Review 501 on a six-page PR. The reviews endpoint is ascending and has
    /// no `since=` cursor, so the new review is appended to the LAST page and
    /// only a walk to the end can see it — exactly once.
    #[tokio::test]
    async fn review_501_on_a_six_page_pr_is_seen_exactly_once() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let base = "repos/acme/widget/pulls/27/reviews";
        // Five hundred reviews of history, all older than the add.
        let old: Vec<(i64, &str, &str, &str, &str)> = (1..=500)
            .map(|id| (id, "alice", "COMMENTED", "2023-11-14T22:13:19Z", "早期评审"))
            .collect();
        paginate_reviews(&fake, base, "v1", &old);

        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let link = link(true, 27);
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(0))
                .await,
            None
        );

        // Review 501 lands at the END: the list is six pages long and the
        // review is on the last one.
        let mut fresh = old.clone();
        fresh.push((
            501,
            "bob",
            "CHANGES_REQUESTED",
            "2023-11-14T22:13:21Z",
            "第六页的评审",
        ));
        paginate_reviews(&fake, base, "v2", &fresh);

        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("the newest review of a six-page PR is still news");
        assert!(message.contains("bob · \"第六页的评审\""), "{message}");
        assert_eq!(
            message.matches("第六页的评审").count(),
            1,
            "review 501 is reported exactly once: {message}"
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "the same review is never reported twice"
        );
        assert!(
            fake.requests()
                .iter()
                .any(|request| request.path.contains("page=6")),
            "the walk followed the list to the page review 501 lives on"
        );
    }

    /// A check run that fails on the SECOND page of the same commit is still
    /// delivered: the checks list is followed to the end.
    #[tokio::test]
    async fn a_check_failing_on_the_second_page_of_check_runs_is_delivered() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let list = "repos/acme/widget/commits/abc123/check-runs?per_page=100";
        let names: Vec<String> = (0..100).map(|i| format!("green-{i}")).collect();
        let first: Vec<(&str, &str)> = names
            .iter()
            .map(|name| (name.as_str(), "SUCCESS"))
            .collect();
        fake.set(list, "checks-p1", checks(&first));
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs?per_page=100&page=2",
            "checks-p2",
            checks(&[]),
        );
        fake.link(
            list,
            "repos/acme/widget/commits/abc123/check-runs?per_page=100&page=2",
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

        // Only page TWO changes; page one keeps its ETag and answers 304.
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs?per_page=100&page=2",
            "checks-p2b",
            checks(&[("build", "FAILURE")]),
        );
        let before = fake.not_modified();
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a check on page two of the same commit is still news");
        assert!(
            message.contains("PR acme/widget#27 检查失败：build（1 项）"),
            "{message}"
        );
        assert!(
            fake.not_modified() > before,
            "page one was revalidated, not re-read"
        );
        assert!(
            fake.requests()
                .iter()
                .any(|request| request.path.ends_with("check-runs?per_page=100&page=2")),
            "the second page was followed"
        );
    }

    /// Blocker 5: process-local memory stays bounded. Dedup ids are kept only
    /// within each kind's live `since` boundary, and a terminal URL is
    /// remembered only while it is still linked.
    #[tokio::test]
    async fn churned_dedup_ids_and_terminal_urls_stay_bounded() {
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

        // Ten polls, twenty fresh comment ids each: a hundred ids a minute on
        // a busy PR. Only the ids the live `since` boundary can still replay
        // may be remembered.
        for tick in 1..=10i64 {
            fake.set(
                "repos/acme/widget/pulls/27",
                &format!("pr-v{tick}"),
                pr_body("OPEN", false, "abc123"),
            );
            fake.set(
                "repos/acme/widget/issues/27/comments",
                &format!("comments-v{tick}"),
                json!(
                    (0..20)
                        .map(|i| json!({
                            "id": tick * 100 + i,
                            "user": {"login": "alice"},
                            "created_at": format!("2023-11-14T22:1{tick}:19Z"),
                            "body": format!("burst {tick}"),
                        }))
                        .collect::<Vec<_>>()
                ),
            );
            let _ = watcher
                .poll(
                    std::slice::from_ref(&link),
                    &*fetch,
                    Instant::now(),
                    at(tick),
                )
                .await;
            let seen = watcher.links[0].seen.len();
            assert!(
                seen <= 40,
                "dedup ids stay at the live boundary after {tick} polls, saw {seen}"
            );
        }

        // A merged link is remembered only while it is still linked.
        fake.set(
            "repos/acme/widget/pulls/27",
            "pr-merged",
            pr_body("CLOSED", true, "abc123"),
        );
        assert!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(11))
                .await
                .is_some()
        );
        assert_eq!(watcher.done.len(), 1, "the terminal URL is remembered");
        // The human unlinks it: nothing about it may be remembered any more.
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(12)).await,
            None
        );
        assert!(watcher.done.is_empty(), "an unlinked URL is forgotten");
    }
}
