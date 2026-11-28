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
//! - Nested reads (checks, reviews, comments) happen only after a `200`. The
//!   aggregate review decision is the exception: GitHub publishes it only
//!   through GraphQL, which carries no validator, so it is one extra request
//!   per pull request per poll — an approving review with an empty body
//!   changes nothing else, and a decision reconstructed from individual votes
//!   is not the decision GitHub applies.
//! - A `403`/`429` pauses the whole watcher until `X-RateLimit-Reset`.
//! - The first observation of a link is a SILENT baseline: it records the
//!   current state and the comment watermark and emits nothing for the past.
//!   A restart never replays what changed while the process was down.
//! - An added link's baseline is the ADD-TIME observation of that link in
//!   that session, captured once and handed over through a PENDING slot the
//!   add publishes before its `gh` call finishes. A link whose baseline is
//!   still in flight is waited for (bounded), never silently replaced by the
//!   watcher's own later read — that is how a close right after an add used
//!   to vanish. The same slot is what tells a fresh add apart from a restart:
//!   a restart has no pending add at all.
//! - A removal invalidates a watch (and the terminal marker) it owned, so an
//!   unlink → close → re-add never reports a close that happened while the
//!   link was absent, and an unlink → re-add is watched again. Ownership is
//!   ONE SESSION's row: unlinking a pull request in another session leaves
//!   this watcher's baseline untouched.
//! - Notifications are new activity only: a transition or a new comment or
//!   review after the baseline. Nothing re-announces a state that was already
//!   true when the baseline was taken. Comments are matched by ID against a
//!   whole-second boundary (GitHub stamps seconds, the local clock does not).
//! - One GitHub OBJECT is one watch: two stored URLs for one pull request are
//!   polled and deduplicated once, so one comment is one line.
//! - A link is dropped as soon as it is MERGED or CLOSED, and a terminal link
//!   is never polled — or resurrected — again.
//! - A read that cannot be proven complete — a list that hit a cap, a record
//!   that cannot be placed in time or identified — is not an observation at
//!   all: the whole burst is skipped and the next poll retries. GitHub's list
//!   endpoints are ascending and ignore `sort`/`direction`, so a list is only
//!   ever read to its end (bounded), never assumed newest-first.
//!
//! Everything here is process-local. There is no webhook, no persisted
//! watcher row, and no second scheduler: the loop is one ordinary detached
//! task in the session's existing background registry.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, SubsecRound, Utc};
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
/// ETag, when present, is sent as `If-None-Match`. `query` turns the request
/// into the `gh api graphql` call GitHub publishes the aggregate review
/// decision through — the same interface the add-time status path uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub path: String,
    pub etag: Option<String>,
    pub query: Option<String>,
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
            query: None,
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

    /// One pull request's AGGREGATE review decision. `reviewDecision` is a
    /// GraphQL-only field — `repos/{repo}/pulls/{n}` does not carry it — so it
    /// is read through the interface that does expose it, the same call the
    /// add-time status path makes. Never reconstructed from individual votes:
    /// one approval on a branch that needs two is not an approval.
    ///
    /// It is staged with the rest of the poll: a decision that could not be
    /// read is not "no decision", because silence would lose a real approval.
    async fn decision(&mut self, link: &Target) -> Result<Option<ReviewDecision>, Step> {
        let Some(query) = decision_query(link) else {
            return Err(Step::Failed);
        };
        let reply = (self.fetch)(Request {
            path: "graphql".to_owned(),
            etag: None,
            query: Some(query),
        })
        .await;
        if reply.status != 200 {
            return Err(Step::Failed);
        }
        let doc = serde_json::from_slice::<Value>(&reply.body).map_err(|_| Step::Failed)?;
        // A `200` can still be a failure: GraphQL reports it in `errors`.
        if doc.get("errors").is_some() {
            return Err(Step::Failed);
        }
        let pr = doc
            .pointer("/data/repository/pullRequest")
            .ok_or(Step::Failed)?;
        // `null` is a real answer (no decision yet, or `REVIEW_REQUIRED`); a
        // missing or non-string field is a document the watcher cannot read.
        match pr.get("reviewDecision").ok_or(Step::Failed)? {
            Value::Null => Ok(None),
            Value::String(token) => Ok(ReviewDecision::from_token(token)),
            _ => Err(Step::Failed),
        }
    }

    /// One paginated list, followed to its END. GitHub's lists are ascending
    /// and cannot be reversed or cut at a watermark from the request side, so
    /// "the newest item is on page six" is the normal case for a busy PR and
    /// stopping early would simply lose it.
    ///
    /// `items` says where one page's items live. `None` means the walk hit
    /// [`MAX_PAGES`] or [`MAX_ITEMS`]: the read is INCOMPLETE, and every
    /// caller treats it as a failed observation — no commit, no cursor, no
    /// transition, retry on the next poll. The caps are checked INCLUSIVELY,
    /// so a list that exactly fills one cannot be told apart from a list with
    /// one more item behind it: "at the cap" always means nothing committed
    /// and nothing fired. Returning a truncated list as if it were whole is
    /// the one thing this must never do.
    async fn walk(&mut self, path: String, items: Items) -> Result<Option<Vec<Value>>, Step> {
        let mut collected = Vec::new();
        let mut next = Some(path);
        let mut pages = 0;
        while let Some(path) = next {
            if pages >= MAX_PAGES {
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
            if collected.len() >= MAX_ITEMS {
                return Ok(None);
            }
            next = page.next;
        }
        if pages >= MAX_PAGES {
            return Ok(None);
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
    /// The canonical GitHub object this watch polls (`acme/widget#27 (pr)`).
    /// Two stored URLs that name one object share this watch, so one comment
    /// can never become two lines.
    key: String,
    /// The removal count this watch was seeded under. A link that was
    /// unlinked since holds state no baseline can vouch for, so the watch is
    /// rebuilt instead of compared.
    removals: u64,
    /// The URL (and label) this watch reports under. Any of the object's
    /// URLs labels the same notice, so the first linked one is used.
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
    /// The whole SECOND the baseline was taken in, while its first read is
    /// still pending. GitHub stamps `created_at` in whole seconds, so a
    /// comment posted in the same second as the add cannot be told apart from
    /// one that predates it; those ids are recorded as history (never
    /// announced) and everything later is matched by ID. Cleared once that
    /// first read has happened.
    boundary: Option<DateTime<Utc>>,
    /// Emitted note ids with their timestamps. This is what makes an
    /// inclusive `since=` window idempotent when a later poll covers the same
    /// second, and the timestamp is what lets the set be pruned back to the
    /// live boundary.
    seen: HashMap<(NoteKind, i64), DateTime<Utc>>,
}

impl LinkWatch {
    fn new(link: Target, key: String, removals: u64, wall: DateTime<Utc>) -> Self {
        Self {
            key,
            removals,
            link,
            resources: HashMap::new(),
            prev: None,
            last_comment_at: None,
            last_review_comment_at: None,
            review_since: floor_second(wall),
            boundary: None,
            seen: HashMap::new(),
        }
    }

    /// Seed the silent baseline from the ADD-TIME observation of this link in
    /// this session — never from a later display refresh, which would move
    /// the baseline past the very change the watcher exists to report. The
    /// first real poll still compares against it, so a change between the add
    /// and that poll is news; the add itself is not.
    fn seed(&mut self, at: DateTime<Utc>, baseline: &link_status::WatchBaseline) {
        self.prev = Some(Observed {
            terminal: match baseline.terminal {
                Some("merged") => Some(Terminal::Merged),
                Some("closed") => Some(Terminal::Closed),
                _ => None,
            },
            failing: baseline.failing.clone(),
            review: baseline.review,
        });
        // Everything before the add observation is history; only later
        // comments/reviews are news. Whole seconds, because that is the
        // resolution GitHub reports and the boundary second is handled by ID.
        let boundary = floor_second(at);
        self.review_since = boundary;
        self.last_comment_at = Some(boundary);
        self.last_review_comment_at = Some(boundary);
        self.boundary = Some(boundary);
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
            // GitHub's own aggregate decision, read through GraphQL and
            // staged like every other part of the poll.
            observed.review = match reads.decision(&self.link).await {
                Ok(review) => review,
                Err(step) => return step,
            };
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
            observed.failing = match failing_checks(&check_runs, &statuses) {
                Some(failing) => failing,
                None => return Step::Failed,
            };
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
            let review_notes = match parse_notes(&Value::Array(reviews)) {
                Ok(notes) => notes,
                Err(step) => return step,
            };
            let notes = self.stage_notes(review_notes, NoteKind::Review);
            reviewed = notes.lines.clone();
            staged.push((NoteKind::Review, notes));
        }

        if baseline {
            // Nothing before this instant may ever surface as news. The
            // boundary is a WHOLE SECOND — the resolution GitHub stamps — and
            // its ids are recorded by the first comment read instead of being
            // rejected by a fractional local clock.
            let boundary = floor_second(wall);
            self.last_comment_at = Some(boundary);
            self.last_review_comment_at = Some(boundary);
            self.boundary = Some(boundary);
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
            let notes = match parse_notes(&Value::Array(items)) {
                Ok(notes) => self.stage_notes(notes, NoteKind::Comment),
                Err(step) => return step,
            };
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
            let notes = match parse_notes(&Value::Array(items)) {
                Ok(notes) => self.stage_notes(notes, NoteKind::ReviewComment),
                Err(step) => return step,
            };
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
        // The baseline second has now been read once: from here on a comment
        // stamped in it is a real later one, matched by id alone.
        self.boundary = None;
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
        let boundary = self.boundary;
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
            if self.seen.contains_key(&(kind, note.id)) {
                continue;
            }
            seen.push((note.id, note.at));
            if kind != NoteKind::Review {
                cursor = Some(cursor.map_or(note.at, |current| current.max(note.at)));
            }
            // A note stamped inside the BASELINE second is history: GitHub
            // stamps whole seconds, so the add's own second cannot be split
            // into "before" and "after". It is recorded by id (so it can
            // never surface later) and its LATER siblings in that same second
            // — which no baseline could have seen — still fire.
            if boundary.is_some_and(|boundary| note.at <= boundary) {
                continue;
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
    /// One watch per GitHub OBJECT, in store order — `#7` and `#7#discussion`
    /// are one pull request and must not report the same comment twice.
    links: Vec<LinkWatch>,
    paused_until: Option<Instant>,
    /// Terminal objects, remembered with the removal count they finished
    /// under, so a STILL-LINKED merged ref cannot restart a watch while a
    /// re-add after an unlink is a genuinely new link.
    done: HashMap<String, u64>,
    /// The first sync baselines everything already present (a restart must
    /// not report the past). Links that appear LATER are adds, and seed from
    /// THAT session's pending add-time observation instead.
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
        self.sync(links, initial, wall).await;
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
                    stopped.push((watch.key.clone(), watch.removals));
                }
                Step::Stop => stopped.push((watch.key.clone(), watch.removals)),
                Step::RateLimited { reset_at } => {
                    let wait = backoff(reset_at, wall);
                    self.paused_until = Some(now + wait);
                    break;
                }
            }
        }
        for (key, removals) in stopped {
            self.done.insert(key, removals);
        }
        self.links
            .retain(|watch| !self.done.contains_key(&watch.key));
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

    /// Keep state for exactly the currently linked OBJECTS, in store order.
    ///
    /// One rule covers restart and add: the first observation of a link is a
    /// SILENT baseline, never news. A restart has no pending add to inherit,
    /// so the link is baselined by its own first poll; a link that appears
    /// while the watcher runs is an ADD and seeds from that session's pending
    /// add-time observation — waiting for it when its `gh` call is still in
    /// flight, because taking the watcher's own later read as the baseline is
    /// exactly how a close right after an add used to vanish. A terminal add
    /// is dropped here, and only an add — never a later poll — can resurrect
    /// it (see `done`).
    async fn sync(&mut self, links: &[Target], initial: bool, wall: DateTime<Utc>) {
        // One GitHub object, one watch: two stored URLs for one pull request
        // (`#7` and `#7#discussion`) are one object with one seen-set, so one
        // comment is one line. URL stays the storage and chip identity.
        let mut keys = HashSet::new();
        let mut current: Vec<&Target> = Vec::new();
        for link in links {
            if keys.insert(link.key()) {
                current.push(link);
            }
        }
        // `done` exists to keep a STILL-LINKED merged ref from restarting a
        // watch, not to remember every ref this process ever finished: an
        // unlinked OBJECT is forgotten here — and so is one whose removal
        // count moved, since a re-add is a new link.
        self.done.retain(|key, removals| {
            current.iter().any(|link| {
                &link.key() == key && link_status::removals(&self.session, link) == *removals
            })
        });
        let mut existing: HashMap<String, LinkWatch> = self
            .links
            .drain(..)
            .map(|watch| (watch.key.clone(), watch))
            .collect();
        for link in current {
            let key = link.key();
            if self.done.contains_key(&key) {
                continue;
            }
            let removals = link_status::removals(&self.session, link);
            let watch = match existing.remove(&key) {
                // The object was unlinked since this watch was seeded: its
                // baseline describes a link that no longer exists, so it is
                // rebuilt rather than compared (unlink → close → re-add must
                // not report a close that never happened while linked).
                Some(watch) if watch.removals == removals => watch,
                _ => {
                    let mut watch = LinkWatch::new(link.clone(), key.clone(), removals, wall);
                    if !initial
                        && let Some((at, status)) = link_status::take_add(&self.session, link).await
                        && let Some(baseline) = &status.watch
                    {
                        watch.seed(at, baseline);
                        if baseline.terminal.is_some() {
                            self.done.insert(key, removals);
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

/// Map one REST pull-request document to the state that matters. The REST
/// document carries the state, whether it merged, and the head SHA — but NOT
/// the aggregate review decision, which is GraphQL-only and read separately
/// (`Reads::decision`). Reading it from here would be a fiction.
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

/// The one GraphQL read the watcher makes: GitHub's aggregate review
/// decision for one pull request. Owner, repository, and number are re-checked
/// here rather than quoted into the query on trust.
fn decision_query(link: &Target) -> Option<String> {
    let (owner, name) = link.repo.split_once('/')?;
    let safe = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let number = link.number.parse::<u64>().ok()?;
    (safe(owner) && safe(name)).then(|| {
        format!(
            "query RefWatchDecision {{ repository(owner: \"{owner}\", name: \"{name}\") \
             {{ pullRequest(number: {number}) {{ reviewDecision }} }} }}"
        )
    })
}

/// A wall-clock instant cut back to a WHOLE SECOND — the resolution GitHub
/// stamps `created_at` with, and the reason membership is decided by ID.
fn floor_second(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(0)
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
/// not failing — the same conservative split as `link_status`. `None` is a
/// record the watcher cannot READ: a field that is present but of the wrong
/// type is not "pending" and not "gone", and treating it as either loses a
/// failure or re-announces it later.
fn failing_checks(check_runs: &[Value], statuses: &[Value]) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !name.is_empty() && !names.iter().any(|seen| seen == name) {
            names.push(name.to_owned());
        }
    };
    for run in check_runs {
        if !field(run, "status")?.eq_ignore_ascii_case("COMPLETED") {
            continue;
        }
        if matches!(
            field(run, "conclusion")?.to_ascii_uppercase().as_str(),
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE"
        ) {
            push(&field(run, "name")?);
        }
    }
    for context in statuses {
        if matches!(
            field(context, "state")?.to_ascii_uppercase().as_str(),
            "ERROR" | "FAILURE"
        ) {
            push(&field(context, "context")?);
        }
    }
    Some(names)
}

/// One string field of a check record: `""` when absent (an unfinished run
/// reports no conclusion), `None` when it is present but not a string.
fn field(item: &Value, name: &str) -> Option<String> {
    match item.get(name) {
        None => Some(String::new()),
        Some(Value::String(raw)) => Some(raw.clone()),
        Some(_) => None,
    }
}

/// One page of notes, VALIDATED. An item the watcher cannot identify (a
/// missing or non-integer id) or cannot place in time (a timestamp that is
/// absent or unparseable) is not absent activity: dropping it would put it
/// behind a cursor that can never re-read it, and an id of `0` would bypass
/// dedupe and be announced forever. Such a page fails the whole staged read
/// instead — the same path as an incomplete page — and the next poll retries.
fn parse_notes(value: &Value) -> Result<Vec<Note>, Step> {
    let mut notes = Vec::new();
    for item in value.as_array().ok_or(Step::Failed)? {
        let id = item
            .get("id")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or(Step::Failed)?;
        let raw = item
            .get("created_at")
            .or_else(|| item.get("submitted_at"))
            .and_then(Value::as_str)
            .ok_or(Step::Failed)?;
        let at = DateTime::parse_from_rfc3339(raw)
            .map_err(|_| Step::Failed)?
            .with_timezone(&Utc);
        let body = match item.get("body") {
            None => String::new(),
            Some(Value::String(body)) => body.clone(),
            Some(_) => return Err(Step::Failed),
        };
        notes.push(Note {
            id,
            author: item
                .pointer("/user/login")
                .and_then(Value::as_str)
                .unwrap_or("ghost")
                .to_owned(),
            at,
            body,
        });
    }
    notes.sort_by_key(|note| (note.at, note.id));
    Ok(notes)
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
        .arg("github.com");
    match &request.query {
        // A GraphQL read is a POST: `-f` supplies the body, and the answer
        // carries no ETag to revalidate, so none is sent.
        Some(query) => {
            command.arg("-f").arg(format!("query={query}"));
        }
        None => {
            command.arg("--method").arg("GET");
        }
    }
    command.arg("-H").arg("Accept: application/vnd.github+json");
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
        link_in("acme/widget", pr, number)
    }

    /// A link in any repository. A test that UNLINKS (which moves its own
    /// session's removal count for that object) uses its own repository so it
    /// cannot disturb a test watching `acme/widget`, nor one of two sessions
    /// watching the same fixture.
    fn link_in(repo: &str, pr: bool, number: u64) -> Target {
        let (kind, action) = if pr {
            ("pr", "pull")
        } else {
            ("issue", "issues")
        };
        crate::link_status::target(
            kind,
            &format!("https://github.com/{repo}/{action}/{number}"),
        )
        .expect("a watcher test link is a GitHub PR/issue")
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
    }

    /// A whole second plus a fraction: the local clock, which GitHub never
    /// reports and a watermark must never be compared against.
    fn at_sub(seconds: i64, nanos: u32) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, nanos).unwrap()
    }

    /// A REST `repos/{repo}/pulls/{n}` document, shaped like the real one:
    /// state, `merged`, `draft`, `head.sha` — and NO `reviewDecision`, which
    /// GitHub publishes only through GraphQL. A fixture that invented that
    /// field would let the watcher pass a test by reading something GitHub
    /// never sends.
    fn pr_body(state: &str, merged: bool, sha: &str) -> Value {
        json!({
            "state": state,
            "merged": merged,
            "draft": false,
            "head": {"sha": sha},
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

        /// Answer the watcher's one GraphQL read with GitHub's aggregate
        /// review decision. `None` is GitHub's own `null` — no decision
        /// (`REVIEW_REQUIRED`), never a guess.
        fn decision(&self, review: Option<&str>) {
            self.set(
                "graphql",
                "graphql-v1",
                json!({"data": {"repository": {"pullRequest": {"reviewDecision": review}}}}),
            );
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

    /// One open pull request with two green checks, no reviews, and no
    /// aggregate decision, in any repository.
    fn seed_pr(fake: &FakeGh, repo: &str, number: u64, sha: &str) {
        fake.set(
            &format!("repos/{repo}/pulls/{number}"),
            "pr-v1",
            pr_body("OPEN", false, sha),
        );
        fake.set(
            &format!("repos/{repo}/commits/{sha}/check-runs"),
            "checks-v1",
            checks(&[("build", "SUCCESS"), ("test", "SUCCESS")]),
        );
        fake.set(
            &format!("repos/{repo}/commits/{sha}/status"),
            "status-v1",
            no_statuses(),
        );
        fake.set(
            &format!("repos/{repo}/pulls/{number}/reviews"),
            "reviews-v1",
            reviews(&[]),
        );
        fake.set(
            &format!("repos/{repo}/issues/{number}/comments"),
            "comments-v1",
            comments(&[]),
        );
        fake.set(
            &format!("repos/{repo}/pulls/{number}/comments"),
            "review-comments-v1",
            comments(&[]),
        );
        fake.decision(None);
    }

    fn seed_green(fake: &FakeGh, sha: &str) {
        seed_pr(fake, "acme/widget", 27, sha);
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
            // every other REST resource revalidates with If-None-Match. The
            // one exception is the GraphQL decision read, which carries no
            // validator: GitHub publishes `reviewDecision` nowhere else, so
            // it is re-read on every poll.
            assert!(
                request.etag.is_some()
                    || request.path.contains("?since=")
                    || request.query.is_some(),
                "an unchanged poll only sends conditional reads plus the one \
                 decision read: {request:?}"
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
        let mut watcher = Watcher::for_session("add-77");
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // Add #77 while the watcher is live: the status prewarm observed it
        // as OPEN with no failures. The watcher's built-in poll (now) sees
        // that as its baseline, so the add itself emits nothing.
        seed_baseline_for_test(
            "add-77",
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
        fake2.decision(None);
        seed_baseline_for_test(
            "add-78",
            "pr",
            &stable.url,
            status_for_test(NEEDS, None, vec!["build".into()], None),
        );
        let fetch2 = fake2.fetch();
        let mut watcher2 = Watcher::for_session("add-78");
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

    /// The review transition comes from the AGGREGATE decision GitHub
    /// publishes through GraphQL — the same field the chip reads — never from
    /// summing individual votes, and never from a field the REST document
    /// does not carry.
    #[tokio::test]
    async fn review_decisions_and_legacy_status_contexts_are_transitions() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        // The add already saw an approval, so the baseline carries APPROVED:
        // only a NEW changes-requested decision is a transition.
        fake.decision(Some("APPROVED"));
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
        fake.decision(Some("CHANGES_REQUESTED"));
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
        fake.decision(Some("CHANGES_REQUESTED"));
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
        fake.decision(Some("APPROVED"));
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
        fake.decision(Some("REVIEW_REQUIRED"));
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
        fake.decision(Some("REVIEW_REQUIRED"));
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
        fake.decision(Some("APPROVED"));
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
        // A session of its own: the pending baseline slot is per session, and
        // the tests run in parallel.
        let mut watcher = Watcher::for_session("manual-refresh");
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );
        let link = link(true, 27);

        // The add saw a green PR…
        seed_baseline_for_test(
            "manual-refresh",
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
        let mut watcher = Watcher::for_session("merge-before-poll");
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );
        let link = link(true, 27);

        seed_baseline_for_test(
            "merge-before-poll",
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
        let mut watcher = Watcher::for_session("restart");
        let pr = link(true, 27);
        let issue = link(false, 9);
        // What the previous process knew: #27 was green. After a restart that
        // memory is not a baseline — the first observation is, and a pending
        // add is never taken on the first sync.
        seed_baseline_for_test(
            "restart",
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

    /// Item 2: the row is published BEFORE the add-time observation lands, so
    /// a poll that runs in between must wait for that observation instead of
    /// taking its own, later read as the baseline. Otherwise a close right
    /// after the add is recorded as the baseline and never reported.
    #[tokio::test]
    async fn a_close_right_after_an_add_whose_baseline_is_in_flight_is_reported() {
        use crate::link_status::{OPEN, prewarm_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_pr(&fake, "add-probe/widget", 11, "abc123");
        let fetch = fake.fetch();
        // A session of its own: the pending baseline slot is per session.
        let mut watcher = Watcher::for_session("in-flight-add");
        let link = link_in("add-probe/widget", true, 11);
        // The watcher is already running: this is not a restart.
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // The add publishes the row, and its observation is still in flight.
        let prewarm = prewarm_for_test("in-flight-add", "pr", &link.url, || async {
            status_for_test(OPEN, None, Vec::new(), None)
        })
        .expect("a pending add");
        // The PR is CLOSED by the time the watcher's poll reaches it.
        fake.set(
            "repos/add-probe/widget/pulls/11",
            "pr-closed",
            pr_body("CLOSED", false, "abc123"),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a close after the add is news, not the baseline");
        assert!(
            message.contains("PR add-probe/widget#11 已关闭"),
            "{message}"
        );
        prewarm.await.unwrap();
    }

    /// Item 2's fallback: an add whose observation FAILED (or never arrived)
    /// is baselined by the watcher's own read at the moment it sees the link
    /// — silently, and only from then on.
    #[tokio::test]
    async fn a_failed_add_observation_falls_back_to_the_watchers_own_silent_read() {
        use crate::link_status::{OPEN, prewarm_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_pr(&fake, "add-fail-probe/widget", 12, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::for_session("failed-add");
        let link = link_in("add-fail-probe/widget", true, 12);
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // `gh` did not answer the add: the watcher takes its own read as the
        // baseline at that moment.
        let prewarm = prewarm_for_test("failed-add", "pr", &link.url, || async {
            crate::link_status::LinkStatus::unknown()
        })
        .expect("a pending add");
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "the watcher's own read is the silent baseline"
        );
        prewarm.await.unwrap();

        // Only a change AFTER that baseline is news.
        fake.set(
            "repos/add-fail-probe/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE"), ("test", "SUCCESS")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("a change after the watcher's baseline is one notice");
        assert!(
            message.contains("PR add-fail-probe/widget#12 检查失败：build（1 项）"),
            "{message}"
        );
        let _ = OPEN;
        let _ = status_for_test;
    }

    /// Item 3: a removal invalidates the watch it owns, so unlink → close →
    /// re-add never reports a close that happened while the link was absent.
    #[tokio::test]
    async fn an_unlinked_then_closed_then_readded_link_never_reports_the_close() {
        use crate::link_status::{CLOSED, OPEN, seed_baseline_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_pr(&fake, "unlink-probe/widget", 7, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::for_session("unlink-close-readd");
        let link = link_in("unlink-probe/widget", true, 7);
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // Live and OPEN, seeded from its add-time observation.
        seed_baseline_for_test(
            "unlink-close-readd",
            "pr",
            &link.url,
            status_for_test(OPEN, None, Vec::new(), None),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None
        );

        // Unlinked, then CLOSED while absent, and only then re-added.
        crate::link_status::forget("unlink-close-readd", "pr", &link.url);
        fake.set(
            "repos/unlink-probe/widget/pulls/7",
            "pr-closed",
            pr_body("CLOSED", false, "abc123"),
        );
        seed_baseline_for_test(
            "unlink-close-readd",
            "pr",
            &link.url,
            status_for_test(CLOSED, Some("closed"), Vec::new(), None),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "a close that happened while the link was unlinked is not news"
        );
    }

    /// Item 3: a terminal URL that is unlinked and re-added is watched again —
    /// the terminal marker belongs to the lifetime that ended.
    #[tokio::test]
    async fn an_unlinked_terminal_link_that_is_readded_is_watched_again() {
        use crate::link_status::{MERGED, OPEN, seed_baseline_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_pr(&fake, "readd-probe/widget", 9, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::for_session("terminal-readd");
        let link = link_in("readd-probe/widget", true, 9);
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // Added already MERGED: a terminal add is dropped, never polled.
        fake.set(
            "repos/readd-probe/widget/pulls/9",
            "pr-merged",
            pr_body("CLOSED", true, "abc123"),
        );
        seed_baseline_for_test(
            "terminal-readd",
            "pr",
            &link.url,
            status_for_test(MERGED, Some("merged"), Vec::new(), None),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None
        );
        assert_eq!(watcher.done.len(), 1, "the terminal object is remembered");

        // The human unlinks it and re-adds it after a reopen. This is a NEW
        // link: it must be watched again.
        crate::link_status::forget("terminal-readd", "pr", &link.url);
        fake.set(
            "repos/readd-probe/widget/pulls/9",
            "pr-open",
            pr_body("OPEN", false, "abc123"),
        );
        seed_baseline_for_test(
            "terminal-readd",
            "pr",
            &link.url,
            status_for_test(OPEN, None, Vec::new(), None),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "the re-add is a silent baseline"
        );
        fake.set(
            "repos/readd-probe/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE"), ("test", "SUCCESS")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
            .await
            .expect("a re-added link is watched again");
        assert!(
            message.contains("PR readd-probe/widget#9 检查失败：build（1 项）"),
            "{message}"
        );
    }

    /// Item 12: an unlink is one session's row. Two sessions watch ONE pull
    /// request and one of them unlinks it; the other's watch must keep
    /// comparing against its own baseline. A process-wide lifetime would
    /// rebuild it silently on the next sync and swallow a failure nobody in
    /// that session unlinked.
    #[tokio::test]
    async fn an_unlink_in_one_session_does_not_rebaseline_the_other() {
        use crate::link_status::{READY, seed_baseline_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_pr(&fake, "scope-probe/widget", 11, "abc123");
        let fetch = fake.fetch();
        let link = link_in("scope-probe/widget", true, 11);
        let mut unlinked = Watcher::for_session("scope-a");
        let mut kept = Watcher::for_session("scope-b");
        for watcher in [&mut unlinked, &mut kept] {
            assert_eq!(
                watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
                None,
                "a restart with no links is silent"
            );
        }
        // Both sessions added it while live, each with its own (green)
        // add-time observation.
        for session in ["scope-a", "scope-b"] {
            seed_baseline_for_test(
                session,
                "pr",
                &link.url,
                status_for_test(READY, None, Vec::new(), None),
            );
        }
        for watcher in [&mut unlinked, &mut kept] {
            assert_eq!(
                watcher
                    .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                    .await,
                None,
                "the add itself is never news"
            );
        }

        // Session A unlinks the PR, so its next poll sees no links at all.
        crate::link_status::forget("scope-a", "pr", &link.url);
        assert_eq!(
            unlinked.poll(&[], &*fetch, Instant::now(), at(2)).await,
            None
        );

        // B never unlinked it: its baseline still describes a live link, so
        // the failure is reported to B.
        fake.set(
            "repos/scope-probe/widget/commits/abc123/check-runs",
            "checks-v2",
            checks(&[("build", "FAILURE")]),
        );
        let message = kept
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
            .await
            .expect("B's link was never unlinked, so its baseline is still its own");
        assert!(
            message.contains("PR scope-probe/widget#11 检查失败：build（1 项）"),
            "{message}"
        );
    }

    /// Item 4: an approving review with an EMPTY body changes nothing the REST
    /// pull document carries, so the notice can only come from GitHub's own
    /// aggregate decision — read through GraphQL, the interface that exposes
    /// it. Counting individual reviews is not an option.
    #[tokio::test]
    async fn an_approving_review_with_no_body_is_one_notice() {
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

        // Alice approves with no text. `pulls/27` keeps its ETag, the review
        // list gains an entry with an empty body — and only the aggregate
        // decision actually moves.
        fake.set(
            "repos/acme/widget/pulls/27/reviews",
            "reviews-v2",
            reviews(&[(1, "alice", "APPROVED", "2023-11-14T22:13:21Z", "")]),
        );
        fake.decision(Some("APPROVED"));
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("a bodyless approval is one notice");
        assert!(message.contains("PR acme/widget#27 已批准"), "{message}");
        assert!(
            !message.contains("收到新评论"),
            "an empty review has no excerpt: {message}"
        );
        assert!(
            fake.requests()
                .iter()
                .any(|request| request.query.is_some()),
            "the decision was read through GraphQL, not from the REST document"
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "the approval is not repeated"
        );
    }

    /// Item 4's fixture honesty: GitHub's REST pull document carries NO
    /// `reviewDecision`. A stray field there is not the decision, and a test
    /// that only passes because a fixture invents one is worth less than no
    /// test at all.
    #[tokio::test]
    async fn a_review_decision_on_the_rest_document_is_not_the_decision() {
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

        let mut invented = pr_body("OPEN", false, "abc123");
        invented["reviewDecision"] = json!("APPROVED");
        fake.set("repos/acme/widget/pulls/27", "pr-v2", invented);
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a field REST does not publish cannot become a notice"
        );
    }

    /// Item 5: an item with an unusable timestamp must fail the staged read.
    /// Dropping it and committing the later cursor buries it behind the
    /// watermark forever.
    #[tokio::test]
    async fn an_unparseable_timestamp_fails_the_read_instead_of_losing_the_later_one() {
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
            comments(&[
                (1, "alice", "not-a-timestamp", "坏掉的那条"),
                (2, "bob", "2023-11-14T22:13:26Z", "后到的那条"),
            ]),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a page the watcher cannot read is not an observation"
        );

        // The same two comments, both well formed: nothing was lost behind a
        // cursor the failed poll never committed.
        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v3",
            comments(&[
                (1, "alice", "2023-11-14T22:13:25Z", "坏掉的那条"),
                (2, "bob", "2023-11-14T22:13:26Z", "后到的那条"),
            ]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("the retry delivers both comments");
        assert!(message.contains("坏掉的那条"), "{message}");
        assert!(message.contains("后到的那条"), "{message}");
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "neither comment is reported twice"
        );
    }

    /// Item 5: an id the watcher cannot read used to become `0`, which
    /// bypasses dedupe and is announced on every poll.
    #[tokio::test]
    async fn a_comment_without_an_id_is_not_announced_again_and_again() {
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
            json!([{"user": {"login": "alice"}, "created_at": "2023-11-14T22:13:25Z", "body": "没有 id"}]),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "an item with no id cannot be deduped, so it is not activity"
        );

        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v3",
            comments(&[(1, "alice", "2023-11-14T22:13:25Z", "没有 id 的评论")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("the readable version of the comment is one notice");
        assert_eq!(message.matches("没有 id 的评论").count(), 1, "{message}");
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "the comment is reported once, not on every poll"
        );
    }

    /// Item 5: a check record whose payload cannot be read is not "not
    /// failing". Reading it as such drops a failure and then re-announces it
    /// when the payload comes back.
    #[tokio::test]
    async fn a_check_payload_that_cannot_be_read_is_not_a_cleared_failure() {
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
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-red",
            checks(&[("build", "FAILURE"), ("test", "SUCCESS")]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
            .await
            .expect("the failing check is one notice");
        assert!(
            message.contains("PR acme/widget#27 检查失败：build（1 项）"),
            "{message}"
        );

        // The same check rendered as an object: unreadable, not "gone".
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-broken",
            json!({"check_runs": [
                {"name": "build", "status": "COMPLETED", "conclusion": {}},
                {"name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ]}),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
                .await,
            None,
            "an unreadable check record is not a cleared failure"
        );

        // The payload is back and unchanged: the failure is NOT re-announced.
        fake.set(
            "repos/acme/widget/commits/abc123/check-runs",
            "checks-red-again",
            checks(&[("build", "FAILURE"), ("test", "SUCCESS")]),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "a still-failing check is never re-announced"
        );
    }

    /// Item 6: the baseline is taken at a fractional local instant, but GitHub
    /// stamps whole seconds. Membership must be decided by ID against a
    /// whole-second boundary: a comment from BEFORE the add in that second is
    /// history, and a NEW one in that same second is news.
    #[tokio::test]
    async fn a_comment_in_the_add_second_fires_once_it_is_new() {
        use crate::link_status::{OPEN, seed_baseline_at_for_test, status_for_test};

        let fake = FakeGh::default();
        seed_pr(&fake, "same-second/widget", 3, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::for_session("same-second");
        let link = link_in("same-second/widget", true, 3);
        assert_eq!(
            watcher.poll(&[], &*fetch, Instant::now(), at(0)).await,
            None
        );

        // The add happened at 22:13:20.800 — inside the second GitHub reports
        // as `2023-11-14T22:13:20Z`.
        seed_baseline_at_for_test(
            "same-second",
            "pr",
            &link.url,
            at_sub(0, 800_000_000),
            status_for_test(OPEN, None, Vec::new(), None),
        );
        // A comment that was already there when the link was added.
        fake.set(
            "repos/same-second/widget/issues/3/comments",
            "comments-boundary",
            comments(&[(1, "alice", "2023-11-14T22:13:20Z", "加之前就有的评论")]),
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a comment from before the add is history"
        );

        // A NEW comment, in the SAME second (GitHub still says 22:13:20Z).
        fake.set(
            "repos/same-second/widget/issues/3/comments",
            "comments-same-second",
            comments(&[
                (1, "alice", "2023-11-14T22:13:20Z", "加之前就有的评论"),
                (2, "bob", "2023-11-14T22:13:20Z", "加之后马上来的评论"),
            ]),
        );
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("a same-second comment after the add is news");
        assert!(
            message.contains("bob · \"加之后马上来的评论\""),
            "{message}"
        );
        assert!(
            !message.contains("加之前就有的评论"),
            "the pre-add comment never fires: {message}"
        );
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(3))
                .await,
            None,
            "the same-second comment is reported once"
        );
    }

    /// Item 8: `…/pull/27` and `…/pull/27#discussion_r…` are two stored rows
    /// for one GitHub object. One watch, one seen-set, one line — while the
    /// URL stays the storage and chip identity.
    #[tokio::test]
    async fn two_urls_for_one_pull_request_report_one_comment() {
        let fake = FakeGh::default();
        seed_green(&fake, "abc123");
        let fetch = fake.fetch();
        let mut watcher = Watcher::default();
        let plain = link(true, 27);
        let anchor = crate::link_status::target(
            "pr",
            "https://github.com/acme/widget/pull/27#discussion_r1729",
        )
        .expect("a second URL for the same pull request");
        assert_ne!(plain.url, anchor.url, "two stored rows");
        assert_eq!(plain.key(), anchor.key(), "one GitHub object");
        assert_eq!(
            watcher
                .poll(
                    &[plain.clone(), anchor.clone()],
                    &*fetch,
                    Instant::now(),
                    at(0)
                )
                .await,
            None
        );

        fake.set(
            "repos/acme/widget/issues/27/comments",
            "comments-v2",
            comments(&[(1, "alice", "2023-11-14T22:13:21Z", "一条评论两条 URL")]),
        );
        let message = watcher
            .poll(&[plain, anchor], &*fetch, Instant::now(), at(1))
            .await
            .expect("one comment is one notice");
        assert_eq!(
            message.matches("一条评论两条 URL").count(),
            1,
            "one comment, one line: {message}"
        );
        assert_eq!(
            message.matches("收到新评论").count(),
            1,
            "one comment burst, one header: {message}"
        );
    }

    /// Item 9: a list that exactly fills the cap cannot be told apart from a
    /// list with one more item behind it, so "at the cap" commits nothing and
    /// fires nothing.
    #[tokio::test]
    async fn a_list_that_exactly_fills_the_cap_commits_nothing() {
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

        // Exactly MAX_PAGES pages of exactly 100 checks: MAX_ITEMS items, no
        // `rel="next"` after the last one, and the failing run on that page.
        let list = "repos/acme/widget/commits/abc123/check-runs?per_page=100";
        for page in 1..=MAX_PAGES {
            let path = if page == 1 {
                list.to_owned()
            } else {
                format!("{list}&page={page}")
            };
            let mut names: Vec<String> = (0..99).map(|i| format!("green-{page}-{i}")).collect();
            let mut conclusions: Vec<&str> = vec!["SUCCESS"; 99];
            if page == MAX_PAGES {
                names.push("build".to_owned());
                conclusions.push("FAILURE");
            } else {
                names.push(format!("green-{page}-last"));
                conclusions.push("SUCCESS");
            }
            let items: Vec<(&str, &str)> =
                names.iter().map(String::as_str).zip(conclusions).collect();
            fake.set(&path, &format!("checks-{page}"), checks(&items));
            if page < MAX_PAGES {
                fake.link(&path, &format!("{list}&page={}", page + 1));
            }
        }
        assert_eq!(
            watcher
                .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(1))
                .await,
            None,
            "a list at the cap is never reported as the whole one"
        );

        // A list that fits: the failure is news now, which proves the at-cap
        // read committed no state at all.
        fake.links.lock().unwrap().clear();
        fake.set(list, "checks-fits", checks(&[("build", "FAILURE")]));
        let message = watcher
            .poll(std::slice::from_ref(&link), &*fetch, Instant::now(), at(2))
            .await
            .expect("the retry after an at-cap read still reports the failure");
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
}
