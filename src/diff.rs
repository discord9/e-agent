//! Local workspace diff for the web UI's diff browser.
//!
//! This is deliberately the only runtime `git` call in the tree, and it is
//! narrow: no shell, no user-supplied command line, fixed flags plus one
//! validated revision and one pathspec. Two boundaries keep that narrowness
//! honest:
//!
//! * A fixed argv is not a read-only inspection. `git status` / `git diff`
//!   read working-tree content through the `clean` direction of a
//!   `filter=<name>` attribute, which executes `filter.<name>.clean` (or
//!   `.smudge` / `.process`) with repository-chosen paths.
//!   `--no-ext-diff` / `--no-textconv` do not cover clean filters and
//!   `GIT_OPTIONAL_LOCKS=0` does not constrain a helper, so a repository that
//!   configures such a driver, or that assigns a filter to any path, is
//!   refused with an explanatory payload instead of a partial diff (see
//!   [`unsafe_inspection_reason`]). Both halves of that guard ask git instead
//!   of re-deriving its rules: the driver check reads the effective config
//!   (worktree scope included) and the attribute check feeds every path the
//!   inspection would read to `git check-attr`, so root and nested
//!   `.gitattributes`, `$GIT_DIR/info/attributes` and `core.attributesFile`
//!   are exactly the inputs git itself uses.
//! * `-C <workspace>` does not scope anything: a session rooted at
//!   `repo/subdir` still discovers `repo`. Status, patch and counts are all
//!   limited to the `.` pathspec, so nothing outside the workspace can appear.
//!
//! The base is the merge-base of `HEAD` with `refs/remotes/origin/main`,
//! falling back to `refs/heads/main`, then `refs/heads/master`. Candidates are
//! looked up as exact refs (`show-ref --verify`, not revision resolution, so a
//! tag cannot stand in for a branch) and a candidate is skipped only when it is
//! absent: a base that exists but shares no history with `HEAD`, or a lookup
//! that fails, is reported rather than silently replaced by a weaker
//! candidate. With no base the report contains only uncommitted changes
//! (`uncommitted_only`), compared with `HEAD` — or, for an unborn `HEAD`, with
//! the empty tree, so a staged file can never look clean; a staged path whose
//! working copy was deleted (`AD`) exists only in the index, so the index is
//! diffed for it as well.
//!
//! Untracked files come from `git status --porcelain -z` and are reported as
//! paths only — no content is invented for them. Both streams of every git
//! call are bounded, NUL-delimited listings are streamed record by record, and
//! no partial record is ever reported: a huge patch, a huge listing or a chatty
//! failure costs bounded memory. The response always says when it was cut, and
//! files after the cut are still counted (as a lower bound when the count
//! itself is unknown). Counts are counts of PATHS from the same `--name-only`
//! stream whether or not the patch was cut: a file replaced by a symlink is two
//! patch sections for one path.

use std::path::Path;
use std::process::Stdio;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Upper bound on the unified patch bytes read from git and returned to the
/// browser. A larger patch is cut at a line boundary and marked truncated.
pub const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on each returned file list (`files` and `untracked`).
pub const MAX_FILES: usize = 200;

/// Upper bound on the stdout of a `show-ref` / `merge-base` / `config` call.
const MAX_SMALL_OUTPUT: usize = 64 * 1024;

/// Upper bound on the diagnostics kept from a git call. The rest of the
/// stream is still drained, so a chatty child can neither grow this process
/// nor block on a full pipe.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// Upper bound on one streamed NUL-delimited record (a path).
const MAX_RECORD_BYTES: usize = 64 * 1024;

/// Upper bound on the path list handed to `git check-attr`. A larger
/// workspace is refused rather than partially verified.
const MAX_ATTR_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on `check-attr`'s answer (three NUL fields per input path).
const MAX_ATTR_ANSWER_BYTES: usize = 16 * 1024 * 1024;

/// Fixed diff flags: no colour, no external diff, no textconv, rename
/// detection. (`--no-ext-diff` / `--no-textconv` keep a repository's helpers
/// out of the argv; clean filters are the attribute guard's job.)
const DIFF_FLAGS: [&str; 5] = ["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M"];

/// Every path this inspection would read: the index plus the untracked,
/// non-ignored worktree entries, scoped to the workspace by the `.` pathspec —
/// the same set `git status` and `git diff` can touch. `ls-files` executes
/// nothing.
const LS_PATHS: [&str; 7] = [
    "ls-files",
    "-z",
    "--cached",
    "--others",
    "--exclude-standard",
    "--",
    ".",
];

/// Every `filter.<name>` hook config can define, matched in any scope.
const FILTER_DRIVER: &str = r"^filter\..*\.(clean|smudge|process)$";

/// Base candidates, best first, fully qualified. Qualification is not
/// cosmetic: an unqualified `main` lets the name resolution rules decide what
/// the base is, and revision resolution also accepts a tag named
/// `refs/remotes/origin/main`, so these are looked up as exact refs.
const BASE_CANDIDATES: [(&str, &str); 3] = [
    ("refs/remotes/origin/main", "origin/main"),
    ("refs/heads/main", "main"),
    ("refs/heads/master", "master"),
];

/// Inherited variables that re-point the repository, substitute the history
/// we diff against, or redirect the index. `-C <workspace>` must be the only
/// thing deciding what is inspected, even when the server was started from a
/// git hook or an exported shell.
const GIT_ENV_SCRUB: [&str; 10] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
];

/// One workspace diff report. Serialized directly as the endpoint payload.
#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    /// `true` only when the workspace was actually inspected as a git work
    /// tree. `false` means no diff was produced and `message` says why: not a
    /// repository, a missing worktree, a repository that cannot be inspected
    /// safely, or a failed git call. A payload with `repository: false` must
    /// never be rendered as "no changes".
    pub repository: bool,
    /// Absolute path of the workspace that was inspected.
    pub workspace: String,
    /// Ref name actually used as the diff base (`origin/main`, `main`,
    /// `master`), or `None` for the uncommitted-only fallback.
    pub base: Option<String>,
    /// Merge-base commit SHA the ref resolved to, if any.
    pub base_commit: Option<String>,
    /// Human-readable base label for the UI (`main @ abc1234567`, or
    /// `uncommitted only`).
    pub base_label: String,
    /// True when no merge base resolved and only uncommitted changes are
    /// reported.
    pub uncommitted_only: bool,
    /// Changed tracked files, bounded by [`MAX_FILES`].
    pub files: Vec<DiffFile>,
    /// Untracked paths, bounded by [`MAX_FILES`]. No content is read.
    pub untracked: Vec<String>,
    /// The untracked list hit its bound; see `untracked_total`.
    pub untracked_truncated: bool,
    /// Every untracked path git reported, including the ones omitted from
    /// `untracked` by the response bound.
    pub untracked_total: usize,
    /// At least one response bound was hit.
    pub truncated: bool,
    /// Why the response was cut (`patch exceeds 2 MiB; file limit 200`).
    pub truncated_reason: Option<String>,
    /// Patch bytes actually read (each call is bounded by
    /// [`MAX_PATCH_BYTES`]; an unborn `HEAD` reads the worktree and the index).
    pub patch_bytes: usize,
    /// Tracked changed files known to git, including files omitted from
    /// `files` by the response bound. A lower bound when `total_files_exact`
    /// is false.
    pub total_files: usize,
    /// `total_files` is a complete count. When false the count is a lower
    /// bound (`message` says so) and later files may be missing from it.
    pub total_files_exact: bool,
    /// `total_files - files.len()`: files omitted by the response bound.
    pub omitted_files: usize,
    /// Non-fatal explanation (non-repo workspace, refused repository, git
    /// failure, base note, lower-bound count, …).
    pub message: Option<String>,
}

impl DiffReport {
    fn new(workspace: &Path) -> Self {
        Self {
            repository: false,
            workspace: workspace.display().to_string(),
            base: None,
            base_commit: None,
            base_label: "uncommitted only".to_owned(),
            uncommitted_only: true,
            files: Vec::new(),
            untracked: Vec::new(),
            untracked_truncated: false,
            untracked_total: 0,
            truncated: false,
            truncated_reason: None,
            patch_bytes: 0,
            total_files: 0,
            total_files_exact: true,
            omitted_files: 0,
            message: None,
        }
    }
}

/// One changed tracked file.
#[derive(Debug, Clone, Serialize)]
pub struct DiffFile {
    pub path: String,
    /// Previous path, present only for a rename.
    pub old_path: Option<String>,
    /// `added` / `modified` / `deleted` / `renamed`.
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    /// Binary file: no textual hunks are available.
    pub binary: bool,
    /// The byte bound cut this file's hunks. The lines present are complete.
    pub truncated: bool,
    pub hunks: Vec<DiffHunk>,
}

/// One unified-diff hunk. `lines` are plain lines with their original
/// `+` / `-` / space prefix preserved.
#[derive(Debug, Clone, Serialize)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<String>,
}

/// An explanatory payload for a workspace that could not be inspected at all
/// (for example a subagent whose worktree is gone). `repository: false` plus
/// `message` is the contract the web UI renders, so it never substitutes
/// another workspace's diff and never claims "no changes".
pub fn unavailable(workspace: &Path, message: impl Into<String>) -> DiffReport {
    refuse(DiffReport::new(workspace), message)
}

/// Mark a report as "nothing was inspected": `repository: false` plus the
/// explanation. Every failure path goes through here, and the result carries
/// nothing but its workspace, so a half-filled file list can never read as a
/// clean tree.
fn refuse(report: DiffReport, message: impl Into<String>) -> DiffReport {
    DiffReport {
        message: Some(message.into()),
        ..DiffReport::new(Path::new(&report.workspace))
    }
}

/// Compute the workspace's local diff. A workspace that cannot be inspected
/// returns a normal report with `repository: false` and an explanatory
/// `message`; it never errors or panics.
pub async fn workspace_diff(root: &Path) -> DiffReport {
    let mut report = DiffReport::new(root);

    // 1. Is this a git work tree? Anything else gets an explanatory payload.
    let work_tree = match git_run(
        root,
        &["rev-parse", "--is-inside-work-tree"],
        MAX_SMALL_OUTPUT,
    )
    .await
    {
        Ok(run) => run,
        Err(error) => return refuse(report, format!("cannot run git: {error}")),
    };
    if !work_tree.ok() {
        let detail = work_tree.stderr_text();
        return refuse(
            report,
            if detail.is_empty() {
                format!("not a git repository: {}", root.display())
            } else {
                format!("not a git repository: {} ({detail})", root.display())
            },
        );
    }
    if work_tree.text() != "true" {
        return refuse(report, format!("not a git work tree: {}", root.display()));
    }

    // 2. Refuse a repository whose configuration would make the inspection
    //    execute a helper. This runs before anything reads worktree content.
    if let Some(reason) = unsafe_inspection_reason(root).await {
        return refuse(report, format!("cannot inspect safely: {reason}"));
    }
    report.repository = true;

    let mut notes: Vec<String> = Vec::new();

    // 3. Base: the merge-base of HEAD with the first candidate ref that
    //    exists. Only an absent ref advances to the next candidate; an
    //    existing base that is unrelated to HEAD, and a lookup that fails, are
    //    reported — never quietly swapped for `main` and never read as "no
    //    base".
    let head = match ref_commit(root, "HEAD").await {
        Ok(head) => head,
        Err(error) => return refuse(report, format!("cannot resolve HEAD: {error}")),
    };
    if head.is_none() {
        notes.push(
            "HEAD is unborn (no commits yet); comparing the index and the working tree with the empty tree"
                .to_owned(),
        );
    }
    let base = match &head {
        Some(head) => resolve_base(root, head, &mut notes).await,
        None => None,
    };
    if let Some((name, sha)) = &base {
        report.uncommitted_only = false;
        report.base = Some(name.clone());
        report.base_commit = Some(sha.clone());
        report.base_label = format!("{name} @ {}", short_sha(sha));
    }

    // With a base, `git diff <base>` is base → working tree, so uncommitted
    // edits ride on top of the committed range. Without a base, compare
    // against HEAD when it exists so staged and unstaged edits both appear;
    // an unborn HEAD has no revision to name, so the empty tree stands in —
    // plain `git diff` would compare index and working tree and hide a staged
    // file behind a clean-looking panel. `base` stays null and the UI says
    // "uncommitted only". `unborn_tree` is the one case where the index has
    // to be consulted as well.
    let (diff_rev, unborn_tree) = match (&base, &head) {
        (Some((_, merge)), _) => (Some(merge.clone()), None),
        (None, Some(head)) => (Some(head.clone()), None),
        (None, None) => match empty_tree_oid(root).await {
            Some(oid) => (Some(oid.clone()), Some(oid)),
            None => {
                return refuse(
                    report,
                    "cannot resolve the empty tree for an unborn HEAD".to_owned(),
                );
            }
        },
    };

    // 4. Uncommitted entries (paths only — never content), scoped to the
    //    workspace subtree so a session rooted at `repo/subdir` cannot report
    //    a sibling outside it.
    let mut untracked: Vec<String> = Vec::new();
    let mut untracked_total = 0usize;
    let mut rename_source = false;
    // Unborn HEAD only: staged paths whose working copy is gone (`AD`). They
    // exist in neither the empty tree nor the worktree, so the workspace
    // patch below cannot show them and the index has to be diffed as well.
    let mut staged_gone: Vec<String> = Vec::new();
    let listed = git_nul_records(
        root,
        &[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
        |record| {
            // Porcelain v1 -z: two status columns, a space, then the path. A
            // rename or copy is followed by a second record holding the
            // source path, which has to be consumed by position: an old name
            // beginning `?? ` is not an untracked file.
            if std::mem::take(&mut rename_source) {
                return;
            }
            if record.len() < 3 {
                return;
            }
            if unborn_tree.is_some() && record[0] == b'A' && record[1] == b'D' {
                staged_gone.push(String::from_utf8_lossy(&record[3..]).into_owned());
            }
            if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
                rename_source = true;
            }
            if record.starts_with(b"??") {
                untracked_total += 1;
                if untracked.len() < MAX_FILES {
                    untracked.push(String::from_utf8_lossy(&record[3..]).into_owned());
                }
            }
        },
    )
    .await;
    if let Err(error) = listed {
        return refuse(report, format!("cannot list uncommitted changes: {error}"));
    }
    report.untracked_truncated = untracked_total > untracked.len();
    report.untracked = untracked;
    report.untracked_total = untracked_total;

    // 5. The narrow diff call: fixed flags, one validated revision, and the
    //    same `.` pathspec as the status call.
    let mut args = DIFF_FLAGS.to_vec();
    if let Some(rev) = diff_rev.as_deref() {
        args.push(rev);
    }
    args.extend(["--", "."]);
    let diff = match git_run(root, &args, MAX_PATCH_BYTES).await {
        Ok(run) => run,
        Err(error) => return refuse(report, format!("cannot run git diff: {error}")),
    };
    let patch_cut = diff.stdout_cut;
    if !patch_cut && !diff.ok() {
        return refuse(report, format!("git diff failed: {}", diff.stderr_text()));
    }
    let patch = diff.stdout;
    report.patch_bytes = patch.len();
    let mut reasons: Vec<String> = Vec::new();
    if patch_cut {
        reasons.push(format!(
            "patch exceeds {} MiB",
            MAX_PATCH_BYTES / (1024 * 1024)
        ));
    }

    let mut files = parse_patch(&patch, patch_cut);
    let mut staged_paths = 0usize;
    let mut staged_exact = true;
    if let Some(tree) = &unborn_tree
        && !staged_gone.is_empty()
    {
        // `empty tree → index`: the only place a staged path whose working
        // copy was deleted (`AD`) still exists. Only those paths are kept, so
        // the worktree content above stays authoritative for everything else.
        let mut staged_args = DIFF_FLAGS.to_vec();
        staged_args.extend(["--cached", tree, "--", "."]);
        match git_run(root, &staged_args, MAX_PATCH_BYTES).await {
            Ok(run) if run.ok() => {
                report.patch_bytes += run.stdout.len();
                staged_exact = !run.stdout_cut;
                for file in parse_patch(&run.stdout, run.stdout_cut) {
                    if staged_gone.iter().any(|path| path == &file.path) {
                        staged_paths += 1;
                        if files.len() < MAX_FILES {
                            files.push(file);
                        }
                    }
                }
            }
            other => {
                staged_exact = false;
                let detail = match other {
                    Ok(run) => run.stderr_text(),
                    Err(error) => error,
                };
                notes.push(format!(
                    "cannot show staged paths deleted from the working tree: {detail}"
                ));
            }
        }
    }
    report.files = files;

    // Count by streaming `--name-only` records, so `total_files` counts PATHS
    // whether or not the patch was cut: a typechange (a file replaced by a
    // symlink) is two patch sections for one path. A failed count leaves the
    // parsed sections as a lower bound.
    let mut total = report.files.len();
    let mut total_exact = false;
    match count_changed_files(root, diff_rev.as_deref()).await {
        Ok(count) => {
            total = count + staged_paths;
            total_exact = staged_exact;
        }
        Err(error) => notes.push(format!("cannot count changed files: {error}")),
    }
    if !total_exact {
        notes.push(format!("changed-file count is a lower bound ({total}+)"));
    }
    report.total_files = total;
    report.total_files_exact = total_exact;
    report.omitted_files = total.saturating_sub(report.files.len());
    if report.files.len() >= MAX_FILES && report.omitted_files > 0 {
        reasons.push(format!("file limit {MAX_FILES}"));
    }
    if report.untracked_truncated {
        reasons.push(format!(
            "untracked list limit {MAX_FILES} of {}",
            report.untracked_total
        ));
    }
    if report.omitted_files > 0 && reasons.is_empty() {
        reasons.push("files omitted".to_owned());
    }
    report.truncated = !reasons.is_empty();
    report.truncated_reason = if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("; "))
    };
    report.message = if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    };
    report
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(10).collect()
}

/// Defense in depth for the places a revision reaches git's argv. Our own
/// candidates and git's own hashes already satisfy this; rejecting anything
/// that is not a plausible ref keeps the invariant if a caller ever changes.
fn validate_ref(name: &str) -> Option<&str> {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
    {
        return None;
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_'))
    {
        return None;
    }
    Some(name)
}

/// A `git` invocation that cannot be steered by inherited state. `-C
/// <workspace>` decides the repository, inherited `GIT_*` selection/history
/// variables are removed, terminal prompts are off, and `core.fsmonitor` is
/// disabled because a repository can point it at a hook that status would run.
fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    for name in GIT_ENV_SCRUB {
        command.env_remove(name);
    }
    command
}

/// One bounded git invocation.
struct GitRun {
    /// At most the caller's cap (`Capped` strategy only; empty otherwise);
    /// complete only when `stdout_cut` is false.
    stdout: Vec<u8>,
    /// Diagnostic prefix only: never more than [`MAX_STDERR_BYTES`].
    stderr: Vec<u8>,
    /// Exit code, or `None` when the process was killed at its stdout bound.
    code: Option<i32>,
    /// stdout hit its bound and the process was killed.
    stdout_cut: bool,
}

impl GitRun {
    fn ok(&self) -> bool {
        !self.stdout_cut && self.code == Some(0)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    fn stderr_text(&self) -> String {
        stderr_prefix(&self.stderr)
    }
}

/// Trimmed stderr diagnostics, marked as cut when only the prefix is present.
fn stderr_prefix(stderr: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(stderr);
    let detail = lossy.trim();
    if stderr.len() >= MAX_STDERR_BYTES {
        format!("{detail}…")
    } else {
        detail.to_owned()
    }
}

/// Run one git command with both streams bounded.
async fn git_run(root: &Path, args: &[&str], cap: usize) -> Result<GitRun, String> {
    run_bounded(git_command(root, args), None, cap).await
}

/// Run a prepared command, keeping at most `cap` stdout bytes: one extra byte
/// distinguishes an exact fit from a cut, and a longer output is cut (the
/// child is killed). `stdin` is fed to the child when given.
async fn run_bounded(
    command: Command,
    stdin: Option<Vec<u8>>,
    cap: usize,
) -> Result<GitRun, String> {
    let mut stdout = Vec::new();
    let run = run_frame(command, stdin, |chunk| {
        stdout.extend_from_slice(chunk);
        stdout.len() <= cap + 1
    })
    .await?;
    let cut = stdout.len() > cap;
    stdout.truncate(cap);
    Ok(GitRun {
        stdout,
        stdout_cut: cut,
        ..run
    })
}

/// The one spawn → concurrent drain → wait frame every git call goes through.
///
/// `visit` receives stdout in chunks and returns `false` when the caller has
/// what it needs; the child is then killed and `stdout_cut` is set. `stdin` is
/// written from its own task, so a blocked write cannot deadlock against a
/// full stdout pipe, and a write the child never read fails the run: input
/// that was never read was never verified.
async fn run_frame(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> Result<GitRun, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run git: {error}"))?;
    let (mut out, mut err) = match (child.stdout.take(), child.stderr.take()) {
        (Some(out), Some(err)) => (out, err),
        _ => return Err("git pipes were not captured".to_owned()),
    };
    // Drain stderr concurrently: a chatty child must never fill the pipe and
    // block the read below, and only a bounded prefix is retained.
    let stderr_reader = tokio::spawn(async move {
        let mut prefix = Vec::new();
        if (&mut err)
            .take(MAX_STDERR_BYTES as u64)
            .read_to_end(&mut prefix)
            .await
            .is_ok()
        {
            let _ = tokio::io::copy(&mut err, &mut tokio::io::sink()).await;
        }
        prefix
    });
    let feeder = match (stdin, child.stdin.take()) {
        (Some(data), Some(mut pipe)) => Some(tokio::spawn(async move {
            pipe.write_all(&data)
                .await
                .map_err(|error| format!("cannot feed git: {error}"))
        })),
        _ => None,
    };
    let mut failure = None;
    let mut cut = false;
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        match out.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                if !visit(&chunk[..read]) {
                    cut = true;
                    break;
                }
            }
            Err(error) => {
                failure = Some(format!("cannot read git output: {error}"));
                break;
            }
        }
    }
    let abandoned = failure.is_some() || cut;
    if abandoned {
        // Kill first: an abandoned child drains neither pipe, so joining the
        // stdin writer below can never block on a full one.
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    if let Some(feeder) = feeder
        && let Err(message) = feeder
            .await
            .unwrap_or_else(|error| Err(format!("cannot feed git: {error}")))
    {
        failure.get_or_insert(message);
    }
    let code = if abandoned {
        None
    } else {
        child
            .wait()
            .await
            .map_err(|error| format!("cannot wait for git: {error}"))?
            .code()
    };
    let stderr = stderr_reader.await.unwrap_or_default();
    match failure {
        Some(failure) => Err(failure),
        None => Ok(GitRun {
            stdout: Vec::new(),
            stderr,
            code,
            stdout_cut: cut,
        }),
    }
}

/// Stream a git command's NUL-delimited stdout, handing every *complete*
/// record to `visit`. Memory stays at one chunk plus one record; a record
/// longer than [`MAX_RECORD_BYTES`] or a stream that ends mid-record is an
/// error, because half a path must never be reported as data.
async fn git_nul_records(
    root: &Path,
    args: &[&str],
    mut visit: impl FnMut(&[u8]),
) -> Result<(), String> {
    let label = args.join(" ");
    let mut pending: Vec<u8> = Vec::new();
    let mut over_long = false;
    let run = run_frame(git_command(root, args), None, |chunk| {
        pending.extend_from_slice(chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == 0) {
            visit(&pending[..end]);
            pending.drain(..=end);
        }
        over_long = pending.len() > MAX_RECORD_BYTES;
        !over_long
    })
    .await?;
    if over_long {
        return Err(format!("git {label} produced an over-long record"));
    }
    if !pending.is_empty() {
        return Err(format!("git {label} ended in the middle of a record"));
    }
    if run.code != Some(0) {
        return Err(format!(
            "git {label} failed: {}",
            stderr_prefix(&run.stderr)
        ));
    }
    Ok(())
}

/// The commit a ref points at, by exact ref lookup. `rev-parse --verify` is
/// revision *resolution*, which also accepts a tag named
/// `refs/remotes/origin/main`, so a tag could stand in for the base ref.
/// `Ok(None)` means the ref is absent; an `Err` means the lookup itself failed
/// and is reported instead of being read as absence.
async fn ref_commit(root: &Path, name: &str) -> Result<Option<String>, String> {
    let exists = git_run(
        root,
        &["show-ref", "--verify", "--quiet", name],
        MAX_SMALL_OUTPUT,
    )
    .await?;
    match exists.code {
        Some(0) => {}
        Some(1) => return Ok(None),
        _ => {
            return Err(format!("cannot look up {name}: {}", exists.stderr_text()));
        }
    }
    let run = git_run(
        root,
        &["show-ref", "--verify", "--hash", name],
        MAX_SMALL_OUTPUT,
    )
    .await?;
    if !run.ok() {
        return Err(format!("cannot read {name}: {}", run.stderr_text()));
    }
    match validate_ref(&run.text()) {
        Some(sha) => Ok(Some(sha.to_owned())),
        None => Err(format!("{name} is not an object name")),
    }
}

/// The diff base: the merge-base of `HEAD` with the first candidate ref that
/// exists. Only an absent candidate advances to the next one; an existing base
/// that shares no history with `HEAD`, and a lookup that fails, stop the
/// search and are reported — a base is never silently swapped for a weaker
/// candidate and an error is never read as "no base".
async fn resolve_base(
    root: &Path,
    head: &str,
    notes: &mut Vec<String>,
) -> Option<(String, String)> {
    for (qualified, label) in BASE_CANDIDATES {
        let sha = match ref_commit(root, qualified).await {
            Ok(Some(sha)) => sha,
            Ok(None) => continue,
            Err(error) => {
                notes.push(format!(
                    "cannot resolve {label}: {error}; showing uncommitted changes only"
                ));
                return None;
            }
        };
        return match merge_base(root, head, &sha).await {
            Ok(Some(merge)) => Some((label.to_owned(), merge)),
            Ok(None) => {
                notes.push(format!(
                    "{label} exists but shares no history with HEAD; showing uncommitted changes only"
                ));
                None
            }
            Err(error) => {
                notes.push(format!(
                    "cannot compare HEAD with {label}: {error}; showing uncommitted changes only"
                ));
                None
            }
        };
    }
    None
}

/// The merge base of two commits. `Ok(None)` is "no common ancestor"
/// (unrelated histories, a shallow boundary, …); a failed call is an error,
/// not an absent base.
async fn merge_base(root: &Path, left: &str, right: &str) -> Result<Option<String>, String> {
    let run = git_run(root, &["merge-base", left, right], MAX_SMALL_OUTPUT).await?;
    match run.code {
        Some(0) => match validate_ref(&run.text()) {
            Some(sha) => Ok(Some(sha.to_owned())),
            None => Err("merge-base returned an unexpected object name".to_owned()),
        },
        Some(1) => Ok(None),
        _ => Err(run.stderr_text()),
    }
}

/// The object id of the empty tree, so an unborn `HEAD` still has something
/// to diff against. `hash-object` only hashes stdin; it runs no filter.
async fn empty_tree_oid(root: &Path) -> Option<String> {
    let run = git_run(
        root,
        &["hash-object", "-t", "tree", "--stdin"],
        MAX_SMALL_OUTPUT,
    )
    .await
    .ok()?;
    if !run.ok() {
        return None;
    }
    validate_ref(&run.text()).map(str::to_owned)
}

/// Count every tracked changed file (paths only) within the workspace by
/// streaming `--name-only` records: only complete paths are counted and only a
/// counter is kept, so a huge list neither grows memory nor inflates the count
/// with half a path.
async fn count_changed_files(root: &Path, rev: Option<&str>) -> Result<usize, String> {
    let mut args = vec!["diff", "--name-only", "-z", "-M"];
    if let Some(rev) = rev {
        args.push(rev);
    }
    args.extend(["--", "."]);
    let mut count = 0usize;
    git_nul_records(root, &args, |record| {
        if !record.is_empty() {
            count += 1;
        }
    })
    .await?;
    Ok(count)
}

/// Why this repository cannot be inspected without possibly running a
/// repository-configured program, if it cannot.
///
/// `git status` / `git diff` read worktree content through the `clean`
/// direction of a `filter=<name>` attribute, which runs `filter.<name>.clean`
/// (or `.smudge` / `.process`) with repository-chosen paths.
/// `--no-ext-diff` / `--no-textconv` do not cover clean filters and
/// `GIT_OPTIONAL_LOCKS=0` does not constrain a helper, so a fixed argv is not
/// a read-only inspection. No diff of ours needs a filter, so such a
/// repository is refused instead of silently downgraded to a partial diff.
async fn unsafe_inspection_reason(root: &Path) -> Option<String> {
    // 1. A driver the repository itself can add is what turns an attribute
    //    into an execution. `--local` alone misses `--worktree` config (its own
    //    file), so the effective config is read and the scope kept: a
    //    `global`/`system` driver is not the repository's to add, and it can
    //    only run where an applicable attribute names it, which 2. reports.
    match git_run(
        root,
        &["config", "--show-scope", "--get-regexp", FILTER_DRIVER],
        MAX_SMALL_OUTPUT,
    )
    .await
    {
        Ok(run) if run.ok() => {
            let repository = run
                .text()
                .lines()
                .map(str::to_owned)
                .find(|line| !line.starts_with("global\t") && !line.starts_with("system\t"));
            if let Some(line) = repository {
                return Some(format!("a git filter driver is configured ({line})"));
            }
        }
        Ok(run) if run.code == Some(1) => {}
        Ok(run) => return Some(format!("cannot read the git config: {}", run.stderr_text())),
        Err(error) => return Some(error),
    }
    // 2. Any path this inspection would read that carries a filter attribute
    //    would hand repository-controlled paths to that driver.
    match paths_assign_a_filter(root).await {
        Ok(None) => None,
        Ok(Some(path)) => Some(format!("{path} assigns a filter attribute")),
        Err(reason) => Some(reason),
    }
}

/// The first path this inspection would read that carries a `filter`
/// attribute, asked of git itself: `check-attr` applies exactly the inputs git
/// uses — root and nested `.gitattributes`, `$GIT_DIR/info/attributes`,
/// `core.attributesFile` — and executes nothing, so there is no attribute
/// syntax to re-derive and no attribute file to decode. The paths come from
/// `ls-files` (the index plus the untracked, non-ignored worktree entries),
/// scoped by the same `.` pathspec as the status and diff calls. A path whose
/// attributes cannot be established at all — too large a list to feed, a failed
/// call, an answer covering fewer paths than were asked about — is refused
/// rather than assumed clean.
async fn paths_assign_a_filter(root: &Path) -> Result<Option<String>, String> {
    let listed = run_bounded(git_command(root, &LS_PATHS), None, MAX_ATTR_INPUT_BYTES).await?;
    if listed.stdout_cut {
        return Err(format!(
            "more than {} MiB of paths to verify for filter attributes",
            MAX_ATTR_INPUT_BYTES / (1024 * 1024)
        ));
    }
    if !listed.ok() {
        return Err(format!(
            "cannot list the paths to verify: {}",
            listed.stderr_text()
        ));
    }
    // `ls-files -z` is already the `--stdin -z` input format: NUL-separated
    // paths, where a trailing NUL means no half path was cut.
    let input = listed.stdout;
    if input.is_empty() {
        // Nothing to read, so nothing can execute.
        return Ok(None);
    }
    if input.last() != Some(&0) {
        return Err("git ls-files ended in the middle of a record".to_owned());
    }
    let asked = input.iter().filter(|byte| **byte == 0).count();
    let run = run_bounded(
        git_command(root, &["check-attr", "-z", "--stdin", "filter"]),
        Some(input),
        MAX_ATTR_ANSWER_BYTES,
    )
    .await?;
    if run.stdout_cut {
        return Err(format!(
            "more than {} MiB of attribute answers to verify",
            MAX_ATTR_ANSWER_BYTES / (1024 * 1024)
        ));
    }
    if run.code != Some(0) {
        return Err(format!(
            "cannot read the work tree's attributes: {}",
            run.stderr_text()
        ));
    }
    // Three NUL-terminated fields per input path: path, attribute, value
    // (`unspecified` / `unset` / `set` / the value itself).
    let mut answers = run.stdout.split(|byte| *byte == 0);
    let mut answered = 0usize;
    let mut offender: Option<String> = None;
    while let (Some(path), Some(_), Some(value)) = (answers.next(), answers.next(), answers.next())
    {
        answered += 1;
        if offender.is_none() && !matches!(value, b"unspecified" | b"unset") {
            offender = Some(String::from_utf8_lossy(path).into_owned());
        }
    }
    if answered != asked {
        return Err(format!(
            "git check-attr answered for {answered} of {asked} paths"
        ));
    }
    Ok(offender)
}

/// Parse a unified patch into bounded file entries. When `truncated` is set,
/// the final partial line is dropped (it was cut by the byte bound), the last
/// section is kept but marked `truncated`, and its complete hunks are still
/// returned. Sections beyond [`MAX_FILES`] are dropped: the count comes from
/// `--name-only`, never from these sections.
fn parse_patch(bytes: &[u8], truncated: bool) -> Vec<DiffFile> {
    let lossy = String::from_utf8_lossy(bytes);
    let text = if truncated {
        // `rsplit_once('\n')` keeps everything before the last newline; the
        // final line is incomplete after a byte cut, so drop it.
        lossy.rsplit_once('\n').map_or("", |(complete, _)| complete)
    } else {
        lossy.as_ref()
    };

    let mut files = Vec::new();
    let mut current: Option<FileBuilder> = None;
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(builder) = current.take()
                && let Some(file) = builder.finish(false)
                && files.len() < MAX_FILES
            {
                files.push(file);
            }
            current = Some(FileBuilder::new(rest));
            continue;
        }
        if let Some(builder) = current.as_mut() {
            builder.line(line);
        }
    }
    if let Some(builder) = current.take()
        && let Some(file) = builder.finish(truncated)
        && files.len() < MAX_FILES
    {
        files.push(file);
    }
    files
}

struct FileBuilder {
    header_old: Option<String>,
    header_new: Option<String>,
    from_path: Option<String>,
    to_path: Option<String>,
    from_dev_null: bool,
    to_dev_null: bool,
    rename_from: Option<String>,
    rename_to: Option<String>,
    new_file: bool,
    deleted_file: bool,
    binary: bool,
    hunks: Vec<DiffHunk>,
    current: Option<DiffHunk>,
}

impl FileBuilder {
    fn new(header_rest: &str) -> Self {
        let (old, new) = split_diff_header(header_rest);
        Self {
            header_old: Some(strip_side_prefix(old, "a/")),
            header_new: Some(strip_side_prefix(new, "b/")),
            from_path: None,
            to_path: None,
            from_dev_null: false,
            to_dev_null: false,
            rename_from: None,
            rename_to: None,
            new_file: false,
            deleted_file: false,
            binary: false,
            hunks: Vec::new(),
            current: None,
        }
    }

    fn line(&mut self, line: &str) {
        // A hunk header opens a hunk wherever it appears: a second, far-apart
        // hunk must not be swallowed by the previous one. Every other metadata
        // line is only meaningful before the first hunk — inside one, an added
        // source line `++ example` renders as `+++ example` and must stay
        // content.
        if line.starts_with("@@") {
            self.finish_hunk();
            self.current = Some(DiffHunk {
                header: line.to_owned(),
                lines: Vec::new(),
            });
            return;
        }
        if self.hunks.is_empty() && self.current.is_none() && self.metadata(line) {
            return;
        }
        let Some(hunk) = self.current.as_mut() else {
            return;
        };
        let Some(first) = line.as_bytes().first() else {
            return;
        };
        if matches!(first, b'+' | b'-' | b' ') {
            hunk.lines.push(line.to_owned());
        }
    }

    /// Consume one pre-hunk metadata line. Returns false when `line` is not
    /// metadata (which can only be hunk content or trailer noise).
    fn metadata(&mut self, line: &str) -> bool {
        if line.starts_with("new file mode ") {
            self.new_file = true;
            return true;
        }
        if line.starts_with("deleted file mode ") {
            self.deleted_file = true;
            return true;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            self.rename_from = Some(rest.to_owned());
            return true;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            self.rename_to = Some(rest.to_owned());
            return true;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if rest == "/dev/null" {
                self.from_dev_null = true;
                self.from_path = None;
            } else {
                self.from_path = Some(strip_side_prefix(rest.to_owned(), "a/"));
            }
            return true;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                self.to_dev_null = true;
                self.to_path = None;
            } else {
                self.to_path = Some(strip_side_prefix(rest.to_owned(), "b/"));
            }
            return true;
        }
        if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
            self.binary = true;
            return true;
        }
        false
    }

    fn finish_hunk(&mut self) {
        if let Some(hunk) = self.current.take() {
            self.hunks.push(hunk);
        }
    }

    fn finish(mut self, truncated_tail: bool) -> Option<DiffFile> {
        self.finish_hunk();
        let renamed = self.rename_from.is_some() || self.rename_to.is_some();
        let status = if renamed {
            "renamed"
        } else if self.new_file || self.from_dev_null {
            "added"
        } else if self.deleted_file || self.to_dev_null {
            "deleted"
        } else {
            "modified"
        };
        let path = match status {
            "added" => self
                .to_path
                .as_ref()
                .or(self.header_new.as_ref())
                .or(self.header_old.as_ref()),
            "deleted" => self
                .from_path
                .as_ref()
                .or(self.header_old.as_ref())
                .or(self.header_new.as_ref()),
            "renamed" => self
                .rename_to
                .as_ref()
                .or(self.to_path.as_ref())
                .or(self.rename_from.as_ref())
                .or(self.from_path.as_ref())
                .or(self.header_new.as_ref()),
            _ => self
                .to_path
                .as_ref()
                .or(self.from_path.as_ref())
                .or(self.header_new.as_ref())
                .or(self.header_old.as_ref()),
        }
        .cloned()?;
        let old_path = if renamed {
            self.rename_from
                .clone()
                .or(self.from_path.clone())
                .filter(|old| old != &path)
        } else {
            None
        };
        let additions = self
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.starts_with('+'))
            .count();
        let deletions = self
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.starts_with('-'))
            .count();
        Some(DiffFile {
            path,
            old_path,
            status: status.to_owned(),
            additions,
            deletions,
            binary: self.binary,
            truncated: truncated_tail,
            hunks: self.hunks,
        })
    }
}

/// Split the rest of a `diff --git <old> <new>` header. Git does not quote
/// ordinary spaces, so the last ` b/` separator is the reliable split; the
/// whitespace fallback covers `diff.noprefix`.
fn split_diff_header(rest: &str) -> (String, String) {
    if let Some(index) = rest.rfind(" b/") {
        return (rest[..index].to_owned(), rest[index + 1..].to_owned());
    }
    let mut parts = rest.splitn(2, ' ');
    (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
    )
}

fn strip_side_prefix(path: String, prefix: &str) -> String {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.to_owned(),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Hermetic git invocation: no system/user config influence, fixed
    /// identity for commits, terminal prompts off.
    fn git(root: &Path, args: &[&str]) -> String {
        let output = git_command_for_test(root, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Like [`git`], but reports success instead of asserting it: several
    /// counterexamples below are calls that *do* succeed in the repository
    /// under test.
    fn git_succeeds(root: &Path, args: &[&str]) -> bool {
        git_command_for_test(root, args).status.success()
    }

    fn git_command_for_test(root: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", root)
            .output()
            .expect("git must be installed for the diff tests")
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]);
        // Pin the branch so base resolution has a deterministic candidate
        // (`main`) regardless of the host's init.defaultBranch. The first
        // real commit is created by `commit_all`, so the fixture is exactly
        // one commit plus working-tree changes.
        git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    fn commit_all(root: &Path, message: &str) {
        git(root, &["add", "-A"]);
        git(
            root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn repo(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(tag);
        std::fs::create_dir(&root).unwrap();
        init_repo(&root);
        (temp, root)
    }

    /// One commit, one modified tracked file, one staged new file: both are
    /// reported with their status and unified hunk lines.
    #[tokio::test]
    async fn workspace_diff_reports_modified_and_added_files() {
        let (_temp, root) = repo("repo");
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        commit_all(&root, "tracked");

        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("new.txt"), "hello\n").unwrap();
        git(&root, &["add", "new.txt"]);

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert!(!report.uncommitted_only);
        assert_eq!(report.base.as_deref(), Some("main"));
        assert!(report.base_commit.is_some());
        assert!(!report.truncated);
        assert!(report.total_files_exact);
        assert_eq!(report.total_files, 2);
        assert_eq!(report.omitted_files, 0);

        let new = report
            .files
            .iter()
            .find(|file| file.path == "new.txt")
            .expect("new.txt reported");
        assert_eq!(new.status, "added");
        assert_eq!(new.additions, 1);
        assert_eq!(new.deletions, 0);
        assert!(
            new.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line == "+hello"),
            "new file hunk: {new:#?}"
        );

        let tracked = report
            .files
            .iter()
            .find(|file| file.path == "tracked.txt")
            .expect("tracked.txt reported");
        assert_eq!(tracked.status, "modified");
        assert_eq!(tracked.additions, 1);
        assert_eq!(tracked.deletions, 0);
        let lines: Vec<&str> = tracked
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().map(String::as_str))
            .collect();
        assert!(lines.contains(&"+two"), "hunk: {lines:?}");
        assert!(lines.contains(&" one"), "hunk: {lines:?}");
    }

    /// A plain directory is not a repo: explanatory payload, no panic.
    #[tokio::test]
    async fn workspace_diff_non_repo_returns_explanatory_payload() {
        let temp = tempfile::tempdir().unwrap();
        let report = workspace_diff(temp.path()).await;
        assert!(!report.repository);
        let message = report.message.expect("non-repo message");
        assert!(message.contains("not a git repository"), "{message}");
        assert!(report.files.is_empty());
        assert!(report.untracked.is_empty());
        assert!(!report.truncated);
    }

    /// An over-bound patch is cut at the byte limit, the partial first file
    /// keeps its complete lines, and later files are counted as omitted. The
    /// recount streams whole NUL records, so a cut patch can never make a
    /// half path look like a file.
    #[tokio::test]
    async fn workspace_diff_reports_truncation_with_counts() {
        let (_temp, root) = repo("large");
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        commit_all(&root, "base");

        let line = format!("{}\n", "x".repeat(99));
        let content = line.repeat((MAX_PATCH_BYTES + 4096) / line.len() + 1);
        std::fs::write(root.join("a.txt"), &content).unwrap();
        std::fs::write(root.join("b.txt"), "second\n").unwrap();
        std::fs::write(root.join("c.txt"), "third\n").unwrap();
        git(&root, &["add", "a.txt", "b.txt", "c.txt"]);

        let report = workspace_diff(&root).await;
        assert!(report.repository);
        assert!(report.truncated, "{report:#?}");
        assert_eq!(report.patch_bytes, MAX_PATCH_BYTES);
        let reason = report
            .truncated_reason
            .as_deref()
            .expect("truncation reason");
        assert!(reason.contains("2 MiB"), "{reason}");
        assert!(report.total_files_exact, "streamed count is complete");
        assert_eq!(report.total_files, 3);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.omitted_files, 2);
        let first = &report.files[0];
        assert_eq!(first.path, "a.txt");
        assert!(first.truncated);
        assert!(first.additions > 0, "complete lines kept: {first:#?}");
        assert!(
            first
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.starts_with('+')),
            "partial file still shows complete added lines"
        );
    }

    /// Untracked paths are listed but never turned into a file entry or
    /// fabricated hunk content.
    #[tokio::test]
    async fn workspace_diff_lists_untracked_paths_without_content() {
        let (_temp, root) = repo("untracked");
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        commit_all(&root, "tracked");
        std::fs::write(root.join("untracked.txt"), "do not invent this\n").unwrap();

        let report = workspace_diff(&root).await;
        assert!(report.repository);
        assert_eq!(report.untracked, vec!["untracked.txt".to_owned()]);
        assert_eq!(report.untracked_total, 1);
        assert!(report.files.is_empty(), "no fabricated patch: {report:#?}");
        assert_eq!(report.total_files, 0);
        assert!(!report.truncated);
    }

    /// The one argv ref guard rejects anything that is not a plausible ref.
    #[test]
    fn validate_ref_accepts_only_plausible_refs() {
        assert_eq!(validate_ref("origin/main"), Some("origin/main"));
        assert_eq!(validate_ref("main"), Some("main"));
        assert_eq!(
            validate_ref("refs/heads/feat-x_1.2"),
            Some("refs/heads/feat-x_1.2")
        );
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(validate_ref(sha), Some(sha));

        for unsafe_ref in [
            "",
            "-c",
            "a b",
            "a;rm -rf /",
            "a$(id)",
            "a\nb",
            "a..b",
            "a//b",
            "@/main",
            "main/",
            "main.lock",
            "main@{1}",
        ] {
            assert_eq!(validate_ref(unsafe_ref), None, "{unsafe_ref:?}");
        }
    }

    /// Finding 1: a repository with a `filter=` attribute *and* a configured
    /// clean driver would run that helper while git reads the modified file.
    /// The panel refuses instead — and the counterexample is real, so the
    /// refusal is what stops the helper.
    #[tokio::test]
    async fn workspace_diff_refuses_a_clean_filter_repository() {
        let (_temp, root) = repo("filter");
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        // The portable half of the attack: a cloned `.gitattributes`.
        std::fs::write(root.join(".gitattributes"), "*.txt filter=sentinel\n").unwrap();
        commit_all(&root, "attributes");
        // The driver lives in the repository's own config.
        git(
            &root,
            &[
                "config",
                "filter.sentinel.clean",
                "sh -c 'echo ran > filter-ran.txt; cat'",
            ],
        );
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();

        let report = workspace_diff(&root).await;
        assert!(!report.repository, "{report:#?}");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("cannot inspect safely"), "{message}");
        assert!(message.contains("filter"), "{message}");
        assert!(report.files.is_empty(), "{report:#?}");
        assert!(
            !root.join("filter-ran.txt").exists(),
            "the clean filter must never run: {report:#?}"
        );

        // Counterexample: the same repository does run the helper for the
        // exact argv the panel would have used.
        assert!(git_succeeds(
            &root,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD"]
        ));
        assert!(
            root.join("filter-ran.txt").exists(),
            "the helper is reachable without the guard"
        );

        // The attribute alone is refused as well: a driver the host provides
        // (e.g. git-lfs) would still be executed for these paths.
        git(&root, &["config", "--unset", "filter.sentinel.clean"]);
        let report = workspace_diff(&root).await;
        assert!(!report.repository, "{report:#?}");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("assigns a filter attribute"), "{message}");
    }

    /// Finding 1 (revised): `config --local` does not see a `--worktree`
    /// driver, which lives in `$GIT_DIR/config.worktree`. The effective-config
    /// check does, and the helper never runs.
    #[tokio::test]
    async fn workspace_diff_refuses_a_worktree_scope_driver() {
        let (_temp, root) = repo("worktree-filter");
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        git(&root, &["config", "extensions.worktreeConfig", "true"]);
        git(
            &root,
            &[
                "config",
                "--worktree",
                "filter.wtscope.clean",
                "sh -c 'echo ran > wt-filter-ran.txt; cat'",
            ],
        );
        std::fs::write(root.join(".gitattributes"), "*.txt filter=wtscope\n").unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();

        let report = workspace_diff(&root).await;
        assert!(!report.repository, "{report:#?}");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("filter driver is configured"), "{message}");
        assert!(message.contains("worktree"), "{message}");
        assert!(
            !root.join("wt-filter-ran.txt").exists(),
            "the clean filter must never run: {report:#?}"
        );

        // Counterexample: the local scope lists no driver at all, and the same
        // repository does run the helper for the argv the panel would use.
        assert!(!git_succeeds(
            &root,
            &[
                "config",
                "--local",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process)$",
            ]
        ));
        assert!(git_succeeds(
            &root,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD"]
        ));
        assert!(
            root.join("wt-filter-ran.txt").exists(),
            "the worktree-scope helper is reachable without the guard"
        );
    }

    /// Finding 1 (revised): an attribute file that is not valid UTF-8 is not
    /// skipped by git. A guard that decodes it as text sees nothing (the whole
    /// file is skipped), while the valid assignment after the bad bytes is
    /// exactly what git honours.
    #[tokio::test]
    async fn workspace_diff_refuses_a_non_utf8_attribute_file() {
        let (_temp, root) = repo("nonutf8-attributes");
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        // Latin-1 `é` in a comment, then a live assignment: a text decoder
        // rejects the whole file, git reads the assignment as bytes.
        std::fs::write(
            root.join(".gitattributes"),
            b"# caf\xe9\n*.txt filter=sentinel\n",
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(root.join(".gitattributes")).is_err(),
            "the counterexample needs a file a text decoder rejects"
        );
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();

        let report = workspace_diff(&root).await;
        assert!(!report.repository, "{report:#?}");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("assigns a filter attribute"), "{message}");
        assert!(message.contains("a.txt"), "{message}");
        assert!(
            !root.join("nonutf8-filter-ran.txt").exists(),
            "the clean filter must never run: {report:#?}"
        );

        // The assignment is live, not a decoding artefact: give it a driver
        // and the same repository runs the helper for the argv the panel
        // would have used.
        git(
            &root,
            &[
                "config",
                "filter.sentinel.clean",
                "sh -c 'echo ran > nonutf8-filter-ran.txt; cat'",
            ],
        );
        assert!(git_succeeds(
            &root,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD"]
        ));
        assert!(root.join("nonutf8-filter-ran.txt").exists());
    }

    /// The attribute check follows git's own inputs: `$GIT_DIR/info/
    /// attributes`, a nested `.gitattributes`, and an attribute file above a
    /// subdirectory workspace all apply to the paths that workspace reads.
    #[tokio::test]
    async fn workspace_diff_checks_every_attribute_source() {
        // 1. $GIT_DIR/info/attributes
        let (_temp, root) = repo("info-attributes");
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/attributes"), "*.txt filter=info\n").unwrap();
        let report = workspace_diff(&root).await;
        assert!(!report.repository, "{report:#?}");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("assigns a filter attribute"), "{message}");
        assert!(message.contains("a.txt"), "{message}");

        // 2. a nested .gitattributes
        let (_temp, root) = repo("nested-attributes");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        std::fs::write(root.join("sub/.gitattributes"), "*.txt filter=nested\n").unwrap();
        let report = workspace_diff(&root).await;
        assert!(!report.repository, "{report:#?}");
        assert!(
            report
                .message
                .clone()
                .unwrap_or_default()
                .contains("sub/a.txt"),
            "{report:#?}"
        );

        // 3. a root .gitattributes applies to a subdirectory workspace, which
        //    only lists the paths under its own prefix.
        let (_temp, root) = repo("subdir-attributes");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/in.txt"), "one\n").unwrap();
        std::fs::write(root.join("top.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        std::fs::write(root.join(".gitattributes"), "*.txt filter=root\n").unwrap();
        let report = workspace_diff(&root.join("sub")).await;
        assert!(!report.repository, "{report:#?}");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("in.txt"), "{message}");
    }

    /// Finding 2: `-C repo/subdir` still discovers `repo`. Status, patch and
    /// counts are limited to the workspace pathspec, so a sibling's changes
    /// (tracked and untracked) never appear.
    #[tokio::test]
    async fn workspace_diff_stays_inside_a_subdirectory_workspace() {
        let (_temp, root) = repo("subdir");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inside.txt"), "one\n").unwrap();
        std::fs::write(root.join("sibling.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        std::fs::write(root.join("sub/inside.txt"), "two\n").unwrap();
        std::fs::write(root.join("sibling.txt"), "two\n").unwrap();
        std::fs::write(root.join("sub/untracked.txt"), "u\n").unwrap();
        std::fs::write(root.join("sibling-untracked.txt"), "u\n").unwrap();

        let report = workspace_diff(&root.join("sub")).await;
        assert!(report.repository, "{report:#?}");
        let paths: Vec<&str> = report.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["sub/inside.txt"],
            "a sibling's patch must not appear: {report:#?}"
        );
        assert_eq!(
            report.untracked,
            vec!["sub/untracked.txt".to_owned()],
            "a sibling's untracked path must not appear: {report:#?}"
        );
        assert_eq!(report.untracked_total, 1);
        assert_eq!(report.total_files, 1);
    }

    /// A failed git call with megabytes of stderr keeps only a diagnostic
    /// prefix and still drains the pipe. 400 KiB through a 64 KiB pipe
    /// deadlocks anything that stops reading, and an unbounded `Vec` would
    /// retain it all.
    #[cfg(unix)]
    #[tokio::test]
    async fn git_stderr_is_bounded_to_a_prefix() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("dd if=/dev/zero bs=1000 count=400 1>&2 2>/dev/null; exit 3");
        let run = run_bounded(command, None, 1024).await.unwrap();
        assert_eq!(run.code, Some(3));
        assert_eq!(
            run.stderr.len(),
            MAX_STDERR_BYTES,
            "only a bounded prefix is retained"
        );
        assert!(run.stderr_text().ends_with('…'), "cut diagnostics say so");
    }

    /// Finding 4: an `origin/main` that shares no history with `HEAD` is
    /// reported instead of silently falling back to a local `main`.
    #[tokio::test]
    async fn workspace_diff_reports_an_unrelated_base() {
        let (_temp, root) = repo("unrelated");
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        commit_all(&root, "base");
        // An unrelated commit reachable only as `refs/remotes/origin/main`.
        git(&root, &["checkout", "-q", "--orphan", "other"]);
        std::fs::write(root.join("other.txt"), "other\n").unwrap();
        commit_all(&root, "other");
        let orphan = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["update-ref", "refs/remotes/origin/main", &orphan]);
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();

        // Counterexample: the old candidate list would have used `main` here.
        assert!(!git_succeeds(&root, &["merge-base", "HEAD", "origin/main"]));
        assert!(git_succeeds(&root, &["merge-base", "HEAD", "main"]));

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert!(
            report.base.is_none(),
            "an unrelated base must not stand in for a resolution: {report:#?}"
        );
        assert!(report.uncommitted_only);
        assert_eq!(report.base_label, "uncommitted only");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("origin/main"), "{message}");
        assert!(message.contains("no history"), "{message}");
        let paths: Vec<&str> = report.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, vec!["tracked.txt"], "{report:#?}");
    }

    /// Finding 2 (revised): `rev-parse --verify` is revision *resolution*, so a
    /// tag literally named `refs/remotes/origin/main` satisfies it. The exact
    /// ref lookup sees `origin/main` as absent and resolves `master` instead,
    /// so a tag can no longer stand in for the base.
    #[tokio::test]
    async fn workspace_diff_ignores_a_tag_shadowing_the_base_ref() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("shadow-tag");
        std::fs::create_dir(&root).unwrap();
        init_repo(&root);
        // Only `master` exists, so the fallback chain has to reach it.
        git(&root, &["symbolic-ref", "HEAD", "refs/heads/master"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        commit_all(&root, "base");
        // A shadowing tag pointing at an unrelated commit.
        git(&root, &["checkout", "-q", "--orphan", "orphan"]);
        std::fs::write(root.join("other.txt"), "other\n").unwrap();
        commit_all(&root, "other");
        let orphan = git(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &["update-ref", "refs/tags/refs/remotes/origin/main", &orphan],
        );
        git(&root, &["checkout", "-q", "master"]);
        std::fs::write(root.join("base.txt"), "base\nuncommitted\n").unwrap();

        // Counterexample: revision resolution accepts the tag, and the
        // "base" it resolves to shares no history with HEAD.
        assert!(git_succeeds(
            &root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/main"
            ]
        ));
        assert!(!git_succeeds(
            &root,
            &["merge-base", "HEAD", "refs/remotes/origin/main"]
        ));

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert_eq!(
            report.base.as_deref(),
            Some("master"),
            "a shadowing tag must not resolve as origin/main: {report:#?}"
        );
        assert!(!report.uncommitted_only);
        let paths: Vec<&str> = report.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, vec!["base.txt"], "{report:#?}");
    }

    /// Finding 5: with an unborn `HEAD` a plain `git diff` compares index and
    /// worktree, so a staged file is invisible and the panel claims a clean
    /// tree. The empty tree stands in for the missing revision.
    #[tokio::test]
    async fn workspace_diff_unborn_head_shows_staged_files() {
        let (_temp, root) = repo("unborn");
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        // Counterexample: the revision-less diff sees nothing.
        assert_eq!(git(&root, &["diff", "--no-color"]), "");

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert!(report.uncommitted_only);
        assert_eq!(report.files.len(), 1, "{report:#?}");
        let file = &report.files[0];
        assert_eq!(file.path, "staged.txt");
        assert_eq!(file.status, "added");
        let message = report.message.clone().unwrap_or_default();
        assert!(message.contains("unborn"), "{message}");
    }

    /// Finding 3 (revised): with an unborn `HEAD`, `empty tree → worktree`
    /// sees nothing for a staged path whose working copy was deleted (`AD`) —
    /// the path exists only in the index, so the index is diffed as well.
    #[tokio::test]
    async fn workspace_diff_unborn_head_shows_a_staged_deleted_file() {
        let (_temp, root) = repo("unborn-ad");
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        std::fs::remove_file(root.join("staged.txt")).unwrap();
        assert_eq!(git(&root, &["status", "--porcelain"]), "AD staged.txt");
        let empty = git(&root, &["hash-object", "-t", "tree", "--stdin"]);
        assert_eq!(
            git(&root, &["diff", "--no-color", &empty]),
            "",
            "the empty tree → worktree patch loses the AD path"
        );

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert_eq!(report.files.len(), 1, "{report:#?}");
        let file = &report.files[0];
        assert_eq!(file.path, "staged.txt");
        assert_eq!(file.status, "added");
        assert!(
            file.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line == "+staged"),
            "the staged content is surfaced: {report:#?}"
        );
        assert_eq!(report.total_files, 1);
        assert!(report.total_files_exact, "{report:#?}");
        assert_eq!(report.omitted_files, 0);
        assert!(!report.truncated);
    }

    /// Finding 6: the untracked list is bounded but its total is not. 201 and
    /// 261 untracked paths must not both read as "200 plus a boolean".
    #[tokio::test]
    async fn workspace_diff_reports_the_real_untracked_total() {
        for count in [MAX_FILES + 1, MAX_FILES + 61] {
            let (_temp, root) = repo(&format!("many-{count}"));
            std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
            commit_all(&root, "base");
            for index in 0..count {
                std::fs::write(root.join(format!("u{index:04}.txt")), "u\n").unwrap();
            }

            let report = workspace_diff(&root).await;
            assert!(report.repository, "{report:#?}");
            assert_eq!(report.untracked.len(), MAX_FILES, "{report:#?}");
            assert_eq!(
                report.untracked_total, count,
                "the total is the real count, not the display bound"
            );
            assert!(report.untracked_truncated);
            let reason = report.truncated_reason.unwrap_or_default();
            assert!(reason.contains(&count.to_string()), "{reason}");
        }
    }

    /// Finding 4 (revised): a file replaced by a symlink is two patch sections
    /// for one path, so the count comes from the same `--name-only` stream
    /// whether or not the patch was cut.
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_diff_counts_a_symlink_typechange_once() {
        let (_temp, root) = repo("typechange");
        std::fs::write(root.join("link.txt"), "regular\n").unwrap();
        commit_all(&root, "base");
        std::fs::remove_file(root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("target", root.join("link.txt")).unwrap();
        assert_eq!(git(&root, &["diff", "--name-only"]), "link.txt");
        assert_eq!(
            git(&root, &["diff", "--no-color"])
                .matches("diff --git ")
                .count(),
            2,
            "the counterexample really is two sections"
        );

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert_eq!(report.total_files, 1, "one path, two sections: {report:#?}");
        assert!(report.total_files_exact, "{report:#?}");
        assert_eq!(report.omitted_files, 0);
        assert!(!report.truncated);
        assert!(
            report.files.iter().any(|file| file.path == "link.txt"),
            "{report:#?}"
        );
    }

    /// Finding 7a: a hunk line that starts with `+++ ` is content. Parsing it
    /// as a file header renamed the displayed file.
    #[tokio::test]
    async fn workspace_diff_hunk_lines_are_never_metadata() {
        let (_temp, root) = repo("hunkmeta");
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        commit_all(&root, "base");
        // An added source line whose content is `++ example` renders as
        // `+++ example` inside the hunk.
        std::fs::write(root.join("weird.txt"), "++ example\n").unwrap();
        git(&root, &["add", "weird.txt"]);

        let report = workspace_diff(&root).await;
        assert_eq!(report.files.len(), 1, "{report:#?}");
        let file = &report.files[0];
        assert_eq!(
            file.path, "weird.txt",
            "a hunk line must not rename the file: {report:#?}"
        );
        assert!(
            file.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line == "+++ example"),
            "the line is content: {report:#?}"
        );
    }

    /// Finding 5 (revised): a hunk header opens a hunk wherever it appears, so
    /// a second, far-apart hunk keeps its own header instead of being appended
    /// to the first one.
    #[tokio::test]
    async fn workspace_diff_keeps_a_second_distant_hunk_header() {
        let (_temp, root) = repo("two-hunks");
        let original: String = (1..=40).map(|line| format!("{line}\n")).collect();
        std::fs::write(root.join("two.txt"), &original).unwrap();
        commit_all(&root, "base");
        std::fs::write(
            root.join("two.txt"),
            original
                .replace("\n2\n", "\nTWO\n")
                .replace("\n39\n", "\nTHIRTYNINE\n"),
        )
        .unwrap();

        let report = workspace_diff(&root).await;
        assert_eq!(report.files.len(), 1, "{report:#?}");
        let file = &report.files[0];
        assert_eq!(file.hunks.len(), 2, "both hunk headers kept: {report:#?}");
        assert!(
            file.hunks[0].header.starts_with("@@ -1,"),
            "{:?}",
            file.hunks[0]
        );
        assert!(
            file.hunks[1].header.starts_with("@@ -36,"),
            "the second hunk keeps its header: {:?}",
            file.hunks[1]
        );
        assert!(
            file.hunks[1].lines.iter().any(|line| line == "+THIRTYNINE"),
            "the second hunk owns its lines: {report:#?}"
        );
        assert_eq!(file.additions, 2);
        assert_eq!(file.deletions, 2);
    }

    /// Finding 7b: in `-z` porcelain a rename is followed by a second record
    /// holding the source path. An old name beginning `?? ` is that record,
    /// not an untracked file.
    #[tokio::test]
    async fn workspace_diff_does_not_read_a_rename_source_as_untracked() {
        let (_temp, root) = repo("renamesrc");
        std::fs::write(root.join("?? weird.txt"), "x\n").unwrap();
        commit_all(&root, "base");
        git(&root, &["mv", "?? weird.txt", "renamed.txt"]);

        let report = workspace_diff(&root).await;
        assert!(report.repository, "{report:#?}");
        assert!(
            report.untracked.is_empty(),
            "the rename source is not an untracked path: {report:#?}"
        );
        assert_eq!(report.untracked_total, 0);
        let renamed = report
            .files
            .iter()
            .find(|file| file.path == "renamed.txt")
            .expect("the rename itself is reported");
        assert_eq!(renamed.status, "renamed");
        assert_eq!(renamed.old_path.as_deref(), Some("?? weird.txt"));
    }
}
