//! Local workspace diff for the web UI's diff browser.
//!
//! This is deliberately the only runtime `git` call in the tree, and it is
//! narrow: no shell, no user-supplied command line, fixed flags plus one
//! validated revision. The base is the merge-base of `HEAD` with
//! `origin/main`, falling back to `main`, then `master`; when none resolves
//! the report contains only uncommitted changes (`uncommitted_only`). The
//! unified patch already includes uncommitted edits on top of the base, so
//! there is no second "unstaged" query.
//!
//! Untracked files come from `git status --porcelain` and are reported as
//! paths only — no content is invented for them. The response is bounded
//! (patch bytes, file count) and always says when it was cut; files after
//! the cut are still counted, never silently dropped.

use std::path::Path;
use std::process::Stdio;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Upper bound on the unified patch bytes read from git and returned to the
/// browser. A larger patch is cut at a line boundary and marked truncated.
pub const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on each returned file list (`files` and `untracked`).
pub const MAX_FILES: usize = 200;

/// Upper bound on the `git diff --name-only` fallback used to count files
/// after the patch itself was cut.
const MAX_NAME_LIST_BYTES: usize = 1024 * 1024;

/// One workspace diff report. Serialized directly as the endpoint payload.
#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    /// `false` for a non-repo / missing workspace: `message` explains why.
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
    /// The untracked list hit its bound.
    pub untracked_truncated: bool,
    /// At least one response bound was hit.
    pub truncated: bool,
    /// Why the response was cut (`patch exceeds 2 MiB; file limit 200`).
    pub truncated_reason: Option<String>,
    /// Patch bytes actually read (never more than [`MAX_PATCH_BYTES`]).
    pub patch_bytes: usize,
    /// Tracked changed files known to git, including files omitted from
    /// `files` by the response bound.
    pub total_files: usize,
    /// `total_files - files.len()`: files omitted by the response bound.
    pub omitted_files: usize,
    /// Non-fatal explanation (non-repo workspace, git failure, …).
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
            truncated: false,
            truncated_reason: None,
            patch_bytes: 0,
            total_files: 0,
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

/// Compute the workspace's local diff. A non-repo workspace returns a normal
/// report with `repository: false` and an explanatory `message`; it never
/// errors or panics.
pub async fn workspace_diff(root: &Path) -> DiffReport {
    let mut report = DiffReport::new(root);

    // 1. Is this a git work tree? Anything else gets an explanatory payload.
    match git_output(root, &["rev-parse", "--is-inside-work-tree"]).await {
        Ok(output) if output.status.success() => {
            if String::from_utf8_lossy(&output.stdout).trim() != "true" {
                report.message = Some(format!("not a git work tree: {}", root.display()));
                return report;
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            report.message = Some(if detail.is_empty() {
                format!("not a git repository: {}", root.display())
            } else {
                format!("not a git repository: {} ({detail})", root.display())
            });
            return report;
        }
        Err(error) => {
            report.message = Some(format!("cannot run git: {error}"));
            return report;
        }
    }
    report.repository = true;

    // 2. Base: merge-base HEAD origin/main, falling back to main, then
    //    master. A missing ref or an unrelated history simply falls through.
    let mut base: Option<(String, String)> = None;
    for candidate in ["origin/main", "main", "master"] {
        let Some(candidate) = validate_ref(candidate) else {
            continue;
        };
        let Ok(output) = git_output(root, &["merge-base", "HEAD", candidate]).await else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if validate_ref(&sha).is_some() {
            base = Some((candidate.to_owned(), sha));
            break;
        }
    }
    if let Some((name, sha)) = &base {
        report.uncommitted_only = false;
        report.base = Some(name.clone());
        report.base_commit = Some(sha.clone());
        report.base_label = format!("{name} @ {}", short_sha(sha));
    }

    // With a base, `git diff <base>` is base → working tree, so uncommitted
    // edits ride on top of the committed range. Without a base, compare
    // against HEAD when it exists so staged and unstaged edits both appear;
    // `base` stays null and the UI says "uncommitted only".
    let diff_rev = match &base {
        Some((_, sha)) => Some(sha.clone()),
        None => match git_output(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).await {
            Ok(output) if output.status.success() => {
                let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                validate_ref(&sha).is_some().then_some(sha)
            }
            _ => None,
        },
    };

    // 3. Untracked paths (paths only — never content).
    let mut reasons: Vec<String> = Vec::new();
    match git_capped(
        root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
        MAX_PATCH_BYTES,
    )
    .await
    {
        Ok((bytes, cut)) => {
            let mut untracked = Vec::new();
            let mut more = false;
            for entry in bytes.split(|byte| *byte == 0) {
                // Porcelain v1 -z entry: two status columns, a space, path.
                if entry.len() >= 3 && entry.starts_with(b"??") {
                    if untracked.len() >= MAX_FILES {
                        more = true;
                        break;
                    }
                    untracked.push(String::from_utf8_lossy(&entry[3..]).into_owned());
                }
            }
            report.untracked = untracked;
            report.untracked_truncated = more || cut;
        }
        Err(error) => report.message = Some(error),
    }

    // 4. The narrow diff call: fixed flags plus one validated revision.
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M"];
    if let Some(rev) = diff_rev.as_deref() {
        args.push(rev);
    }
    let (patch, patch_cut) = match git_capped(root, &args, MAX_PATCH_BYTES).await {
        Ok(result) => result,
        Err(error) => {
            report.message = Some(error);
            return report;
        }
    };
    report.patch_bytes = patch.len();
    if patch_cut {
        reasons.push(format!(
            "patch exceeds {} MiB",
            MAX_PATCH_BYTES / (1024 * 1024)
        ));
    }

    let parsed = parse_patch(&patch, patch_cut);
    report.files = parsed.files;

    // A cut patch may hide later files: count the full changed-file set so
    // the omitted count is explicit instead of silently losing files.
    let mut total = parsed.total_files;
    if patch_cut && let Some(count) = count_changed_files(root, diff_rev.as_deref()).await {
        total = total.max(count);
    }
    report.total_files = total;
    report.omitted_files = total.saturating_sub(report.files.len());
    if report.files.len() >= MAX_FILES && report.omitted_files > 0 {
        reasons.push(format!("file limit {MAX_FILES}"));
    }
    if report.untracked_truncated {
        reasons.push(format!("untracked list limit {MAX_FILES}"));
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
    report
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(10).collect()
}

/// Defense in depth for the one place a revision reaches git's argv. Our own
/// candidates and merge-base output already satisfy this; rejecting anything
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

/// Remove inherited `GIT_*` repository-selection variables: `-C <workspace>`
/// must be the only thing deciding which repository is inspected, even when
/// the server was launched from a git hook or an exported shell.
fn scrub_git_env(command: &mut Command) -> &mut Command {
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
    command
}

/// Run a git command whose output is small enough to capture whole
/// (`rev-parse`, `merge-base`). Stdin is closed and terminal prompts are off.
async fn git_output(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    scrub_git_env(&mut command)
        .output()
        .await
        .map_err(|error| format!("cannot run git: {error}"))
}

/// Run a git command, reading at most `cap + 1` stdout bytes. The extra byte
/// distinguishes an exact-fit output from a cut one. Returns the bytes (capped)
/// and whether the output was cut.
async fn git_capped(root: &Path, args: &[&str], cap: usize) -> Result<(Vec<u8>, bool), String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = scrub_git_env(&mut command)
        .spawn()
        .map_err(|error| format!("cannot run git: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "git stdout was not captured".to_owned())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "git stderr was not captured".to_owned())?;
    // Drain stderr concurrently so a chatty git process can never fill the
    // pipe and deadlock the read below.
    let stderr_reader = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let mut bytes = Vec::new();
    let read = {
        let mut limited = (&mut stdout).take(cap as u64 + 1);
        limited.read_to_end(&mut bytes).await
    };
    if let Err(error) = read {
        let _ = child.kill().await;
        return Err(format!("cannot read git output: {error}"));
    }
    let cut = bytes.len() > cap;
    if cut {
        bytes.truncate(cap);
        let _ = child.kill().await;
    }
    let status = child
        .wait()
        .await
        .map_err(|error| format!("cannot wait for git: {error}"))?;
    let stderr = stderr_reader.await.unwrap_or_default();
    if !status.success() && !cut {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(format!("git {} failed: {}", args.join(" "), detail.trim()));
    }
    Ok((bytes, cut))
}

/// Count all tracked changed files (paths only) for the truncation report.
async fn count_changed_files(root: &Path, rev: Option<&str>) -> Option<usize> {
    let mut args = vec!["diff", "--name-only", "-z", "-M"];
    if let Some(rev) = rev {
        args.push(rev);
    }
    let (bytes, _) = git_capped(root, &args, MAX_NAME_LIST_BYTES).await.ok()?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .count(),
    )
}

struct ParsedPatch {
    files: Vec<DiffFile>,
    /// All `diff --git` sections seen, including files beyond [`MAX_FILES`].
    total_files: usize,
}

/// Parse a unified patch into bounded file entries. When `truncated` is set,
/// the final partial line is dropped (it was cut by the byte bound), the last
/// section is kept but marked `truncated`, and its complete hunks are still
/// returned.
fn parse_patch(bytes: &[u8], truncated: bool) -> ParsedPatch {
    let lossy = String::from_utf8_lossy(bytes);
    let text = if truncated {
        // `rsplit_once('\n')` keeps everything before the last newline; the
        // final line is incomplete after a byte cut, so drop it.
        lossy.rsplit_once('\n').map_or("", |(complete, _)| complete)
    } else {
        lossy.as_ref()
    };

    let mut files = Vec::new();
    let mut total_files = 0usize;
    let mut current: Option<FileBuilder> = None;
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(builder) = current.take()
                && let Some(file) = builder.finish(false)
                && files.len() < MAX_FILES
            {
                files.push(file);
            }
            total_files += 1;
            if files.len() < MAX_FILES {
                current = Some(FileBuilder::new(rest));
            }
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
    ParsedPatch { files, total_files }
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
        if line.starts_with("new file mode ") {
            self.new_file = true;
            return;
        }
        if line.starts_with("deleted file mode ") {
            self.deleted_file = true;
            return;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            self.rename_from = Some(rest.to_owned());
            return;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            self.rename_to = Some(rest.to_owned());
            return;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if rest == "/dev/null" {
                self.from_dev_null = true;
                self.from_path = None;
            } else {
                self.from_path = Some(strip_side_prefix(rest.to_owned(), "a/"));
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                self.to_dev_null = true;
                self.to_path = None;
            } else {
                self.to_path = Some(strip_side_prefix(rest.to_owned(), "b/"));
            }
            return;
        }
        if line.starts_with("@@") {
            self.finish_hunk();
            self.current = Some(DiffHunk {
                header: line.to_owned(),
                lines: Vec::new(),
            });
            return;
        }
        if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
            self.binary = true;
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
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", root)
            .output()
            .expect("git must be installed for the diff tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
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
    /// keeps its complete lines, and later files are counted as omitted.
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
}
