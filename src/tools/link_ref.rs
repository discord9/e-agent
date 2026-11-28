//! Session PR/issue/plain links, split by verb: `link_ref` adds,
//! `unlink_ref` removes, `list_refs` reads. The URL is the identity; a `kind` + `number`
//! selector is resolved against the session workspace's `origin` remote (or
//! an explicit `repo`) — only where that mapping is known (github.com) — and
//! a full URL stays accepted as the escape hatch everywhere else.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::session_store::{
    SessionRef, SessionStore, parse_ref_url, ref_kind_from_url, session_ref_label_for_kind,
};

/// The three registered tools. Zero state: the runner intercepts execution
/// by name and binds the session's store/root/session.
pub(crate) struct LinkRef;
pub(crate) struct UnlinkRef;
pub(crate) struct ListRefs;

/// The closed schema shared by `link_ref` and `unlink_ref`: exactly one of
/// `kind` + `number` (with optional `repo`) or `url`.
fn selector_spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({
            "type": "object",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["pr", "issue"],
                            "description": "which list the link belongs to"
                        },
                        "number": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "pull-request or issue number"
                        },
                        "repo": {
                            "type": "string",
                            "description": "optional owner/repo or github.com/owner/repo; defaults to this workspace's git origin (github.com remotes only)"
                        }
                    },
                    "required": ["kind", "number"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["pr", "issue", "link"],
                            "description": "explicit kind for a URL; `link` is a plain non-PR/issue link"
                        },
                        "url": {
                            "type": "string",
                            "description": "absolute http(s) URL; skips repository resolution"
                        }
                    },
                    "required": ["kind", "url"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "absolute http(s) URL; skips repository resolution and infers pr/issue from a recognised path (anything else needs `kind`)"
                        }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }
            ]
        }),
    }
}

#[async_trait]
impl Tool for LinkRef {
    fn spec(&self) -> ToolSpec {
        selector_spec(
            "link_ref",
            "Link this session to a pull request or issue. `kind` + `number` resolve against \
             this workspace's git `origin` remote when it is a github.com remote, or against an \
             explicit `repo` (owner/repo, GitHub by default, or github.com/owner/repo); any other \
             host must pass a full `url`, which replaces `kind` + `number` and skips repository \
             resolution. `kind: \"link\"` records a plain non-PR/issue URL and requires `url`. \
             Returns the session's current links: {\"prs\": [{\"label\": \"owner/repo#25\", \
             \"url\": \"https://github.com/owner/repo/pull/25\"}], \"issues\": [...], \"links\": [...]}.",
        )
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("link_ref is executed by the session runner".into())
    }
}

#[async_trait]
impl Tool for UnlinkRef {
    fn spec(&self) -> ToolSpec {
        selector_spec(
            "unlink_ref",
            "Remove one pull-request or issue link from this session. Takes the same selectors \
             as `link_ref`: `kind` + `number` (resolved against this workspace's git `origin` \
             when it is a github.com remote, or against `repo`) or a full `url` — pass the URL \
             `list_refs` returned, which is the exact stored identity (with `kind: \"link\"` \
             for a plain link). Removing something that is not linked is an error naming it. \
             Returns the session's current links: {\"prs\": [{\"label\": \"owner/repo#25\", \
             \"url\": \"https://github.com/owner/repo/pull/25\"}], \"issues\": [...], \"links\": [...]}.",
        )
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("unlink_ref is executed by the session runner".into())
    }
}

#[async_trait]
impl Tool for ListRefs {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_refs".into(),
            description: "Return this session's current pull-request, issue, and plain links \
                          without changing them. Takes no arguments. Every entry carries both \
                          the display `label` AND the `url` — take the `url` from here and pass \
                          it to `unlink_ref` verbatim, because two links can share a label \
                          (`example.org/search?q=a` vs `?q=b`). The result is {\"prs\": \
                          [{\"label\": \"owner/repo#25\", \"url\": \"https://github.com/owner/repo/pull/25\"}], \
                          \"issues\": [...], \"links\": [...]}."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("list_refs is executed by the session runner".into())
    }
}

enum Selector {
    Url {
        url: String,
        /// Explicit `kind` when supplied; `None` infers pr/issue from a
        /// recognised URL path (and refuses anything else).
        kind: Option<&'static str>,
    },
    Number {
        kind: &'static str,
        number: u64,
        repo: Option<String>,
    },
}

struct Resolved {
    kind: &'static str,
    url: String,
    /// How an absent link is named in an `unlink_ref` error.
    display: String,
}

/// Execute one of the three tools with the runner-bound session context.
///
/// `root` is the persistence root the store is bound to; `workspace` is the
/// session's EXECUTION workspace, the only directory a Git remote may be read
/// from (`None` = unknown, e.g. a historical session, so the number form
/// requires a full URL). A delegate's two directories differ: its rows and
/// its transcript live under the parent's persistence root, but it works in
/// its own workspace, and only that workspace's origin may be resolved.
pub(crate) async fn execute(
    tool: &str,
    store: &SessionStore,
    root: &Path,
    workspace: Option<&Path>,
    session: &str,
    arguments: &Value,
) -> Result<String, String> {
    // The backend check comes first: on JSONL every selector must report the
    // explicit unsupported error, never a git-resolution failure.
    if !store.supports_session_refs() {
        return Err(crate::session_store::SESSION_REFS_UNSUPPORTED.into());
    }
    if tool == "list_refs" {
        let object = arguments
            .as_object()
            .ok_or("list_refs arguments must be a JSON object")?;
        if !object.is_empty() {
            return Err("list_refs takes no arguments".into());
        }
        return Ok(render(
            store
                .list_refs(root, session)
                .await
                .map_err(|e| format!("{e:#}"))?,
        ));
    }
    if tool != "link_ref" && tool != "unlink_ref" {
        return Err(format!("unknown tool {tool:?}"));
    }
    let object = arguments
        .as_object()
        .ok_or_else(|| format!("{tool} arguments must be a JSON object"))?;
    let selector = parse_selector(tool, object)?;
    let resolved = resolve_selector(workspace, selector).await?;
    if tool == "unlink_ref" {
        let refs = store
            .list_refs(root, session)
            .await
            .map_err(|e| format!("{e:#}"))?;
        if !refs
            .iter()
            .any(|link| link.kind == resolved.kind && link.url == resolved.url)
        {
            let what = match resolved.kind {
                "pr" => "PR",
                "issue" => "Issue",
                _ => "Link",
            };
            return Err(format!("{what} {} is not linked", resolved.display));
        }
        return Ok(render(
            store
                .remove_ref(root, session, resolved.kind, &resolved.url)
                .await
                .map_err(|e| format!("{e:#}"))?,
        ));
    }
    Ok(render(
        store
            .add_ref(root, session, resolved.kind, &resolved.url)
            .await
            .map_err(|e| format!("{e:#}"))?,
    ))
}

fn parse_selector(tool: &str, object: &Map<String, Value>) -> Result<Selector, String> {
    let url = optional_string(object.get("url"), "url")?;
    let repo = optional_string(object.get("repo"), "repo")?;
    let kind = optional_string(object.get("kind"), "kind")?;
    let number = optional_number(object.get("number"), "number")?;
    match (url, kind, number) {
        (Some(url), kind, None) => {
            if repo.is_some() {
                return Err(format!(
                    "{tool} `repo` only applies to `kind` + `number`, not `url`"
                ));
            }
            let kind = kind.as_deref().map(parse_kind).transpose()?;
            Ok(Selector::Url { url, kind })
        }
        (None, Some(kind), Some(number)) => {
            let kind = parse_kind(&kind)?;
            if kind == "link" {
                return Err(format!(
                    "{tool} `kind`: \"link\" requires a `url` — a plain link has no number form; pass a full URL instead"
                ));
            }
            Ok(Selector::Number { kind, number, repo })
        }
        (Some(_), _, Some(_)) => Err(format!(
            "{tool} takes either `url` or `kind` + `number`, not both"
        )),
        _ => Err(format!(
            "{tool} needs `kind` + `number` (optional `repo`), `kind` + `url`, or `url`"
        )),
    }
}

fn parse_kind(kind: &str) -> Result<&'static str, String> {
    match kind {
        "pr" => Ok("pr"),
        "issue" => Ok("issue"),
        "link" => Ok("link"),
        other => Err(format!(
            "unknown link kind {other:?}; expected \"pr\", \"issue\", or \"link\""
        )),
    }
}

/// Resolve one selector into the exact (kind, url) pair that is compared,
/// inserted and deleted. Both arms produce the NORMALIZED URL (`url` parses
/// and re-serializes it), so the value used for the `unlink_ref` compare is
/// byte-identical to the stored row's.
async fn resolve_selector(
    workspace: Option<&Path>,
    selector: Selector,
) -> Result<Resolved, String> {
    match selector {
        Selector::Url { url, kind } => {
            let parsed = parse_ref_url(&url)?;
            let url = parsed.to_string();
            // An explicit kind is honoured verbatim. Without one, only a
            // recognised PR/issue path may be inferred — a docs URL is not a
            // pull request, so it is refused instead of filed as one.
            let kind = match kind {
                Some(kind) => kind,
                None => ref_kind_from_url(&url).ok_or_else(|| {
                    format!(
                        "{url} is not a pull-request or issue URL — pass `kind`: \"pr\", \"issue\", or \"link\""
                    )
                })?,
            };
            Ok(Resolved {
                kind,
                display: session_ref_label_for_kind(kind, &url),
                url,
            })
        }
        Selector::Number { kind, number, repo } => {
            let url = resolve_number(workspace, kind, number, repo.as_deref()).await?;
            let display = match repo {
                Some(repo) => format!("{repo}#{number}"),
                None => number.to_string(),
            };
            Ok(Resolved { kind, url, display })
        }
    }
}

/// Render the public view shared by all three tools: entries split by kind,
/// each carrying the display `label` AND the exact stored `url`.
///
/// The URL is not decoration: labels are lossy (`example.org/search?q=a` and
/// `?q=b` render identically), so a caller that wants to `unlink_ref` what it
/// just listed must pass the `url` back, never the label.
fn render(refs: Vec<SessionRef>) -> String {
    let mut prs = Vec::new();
    let mut issues = Vec::new();
    let mut links = Vec::new();
    for link in refs {
        let entry = json!({ "label": link.label, "url": link.url });
        match link.kind.as_str() {
            "pr" => prs.push(entry),
            "issue" => issues.push(entry),
            _ => links.push(entry),
        }
    }
    json!({ "prs": prs, "issues": issues, "links": links }).to_string()
}

fn optional_number(value: Option<&Value>, name: &str) -> Result<Option<u64>, String> {
    value
        .map(|value| {
            let number = value
                .as_u64()
                .ok_or_else(|| format!("`{name}` must be a non-negative integer"))?;
            if number == 0 {
                return Err(format!("`{name}` must be at least 1"));
            }
            Ok(number)
        })
        .transpose()
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>, String> {
    value
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("`{name}` must be a string"))
        })
        .transpose()
}

/// The one host whose PR/issue URL shape is known without being told:
/// GitHub's `/pull/N` and `/issues/N`.
const GITHUB_HOST: &str = "github.com";

/// Why an implicit number resolution is refused when the session workspace is
/// unknown (a historical session): no directory to read `origin` from.
const WORKSPACE_UNKNOWN: &str = "this session's workspace is unknown — pass a full URL instead";

/// Resolve `kind` + `number` through an explicit `repo` or the workspace's
/// `origin` remote.
///
/// The result is NORMALIZED exactly like a user-supplied URL
/// ([`parse_ref_url`]), so the value `unlink_ref` compares against is the
/// value `link_ref` stored — `git@GITHUB.COM:acme/widget.git` resolves to the
/// same lowercase identity every backend stores, instead of a second,
/// differently-cased one.
pub(crate) async fn resolve_number(
    workspace: Option<&Path>,
    kind: &str,
    number: u64,
    repo: Option<&str>,
) -> Result<String, String> {
    if kind != "pr" && kind != "issue" {
        return Err(format!(
            "unknown link kind {kind:?}; expected \"pr\" or \"issue\""
        ));
    }
    let path = match repo {
        Some(repo) => parse_repo_selector(repo)?,
        None => {
            let Some(workspace) = workspace else {
                return Err(WORKSPACE_UNKNOWN.into());
            };
            let remote = origin_remote(workspace).await?;
            parse_origin_remote(&remote).map_err(|why| {
                format!(
                    "cannot map 'origin' ({remote}) to a GitHub web URL ({why}) — pass a full URL instead"
                )
            })?
        }
    };
    let action = if kind == "pr" { "pull" } else { "issues" };
    Ok(parse_ref_url(&format!("https://{GITHUB_HOST}/{path}/{action}/{number}"))?.to_string())
}

fn is_github_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    host == GITHUB_HOST || host == "www.github.com"
}

/// Parse an explicit `owner/repo` (documented as the GitHub default) or
/// `github.com/owner/repo` selector.
///
/// Implicit resolution only covers a mapping that is actually known. Any
/// other host does not share GitHub's URL shape (GitLab's pull requests live
/// at `/-/merge_requests/N`), so it must pass a full URL instead of being
/// guessed at.
fn parse_repo_selector(repo: &str) -> Result<String, String> {
    let repo = repo.trim().trim_end_matches('/');
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let parts: Vec<&str> = repo.split('/').collect();
    let usable = |part: &str| !part.is_empty() && !part.chars().any(char::is_whitespace);
    match parts.as_slice() {
        [owner, name] if usable(owner) && usable(name) => Ok(format!("{owner}/{name}")),
        [host, owner, name] if usable(owner) && usable(name) && is_github_host(host) => {
            Ok(format!("{owner}/{name}"))
        }
        _ => Err(
            "`repo` must be owner/repo or github.com/owner/repo — any other host must pass a full URL"
                .into(),
        ),
    }
}

async fn origin_remote(root: &Path) -> Result<String, String> {
    let output = run_git(root, &["remote", "get-url", "origin"]).await?;
    if output.status.success() {
        let remote = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if remote.is_empty() {
            return Err(format!(
                "no git remote 'origin' in {} — pass a full URL instead",
                root.display()
            ));
        }
        return Ok(remote);
    }
    // Distinguish "not a repository" from "a repository without origin".
    let inside = run_git(root, &["rev-parse", "--is-inside-work-tree"]).await?;
    if inside.status.success() {
        Err(format!(
            "no git remote 'origin' in {} — pass a full URL instead",
            root.display()
        ))
    } else {
        Err(format!(
            "not a git repository: {} — pass a full URL instead",
            root.display()
        ))
    }
}

async fn run_git(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .map_err(|error| format!("cannot run git: {error} — pass a full URL instead"))
}

/// Parse a git remote into `owner/repo`, but ONLY when the web mapping is
/// known: a plain github.com transport, scp-like (`git@github.com:owner/repo
/// .git`) or a `https://`/`ssh://`/`git://` URL, without a port.
///
/// Everything else is refused with the reason, because the transport does not
/// tell us the web endpoint: an SSH port is not an HTTPS port, a mirror or
/// alias host is not the forge, and a non-GitHub forge does not serve
/// GitHub's `/pull/N` path. The caller turns the reason into "pass a full URL
/// instead" rather than inventing `https://host:2222/group/project/pull/25`.
fn parse_origin_remote(remote: &str) -> Result<String, &'static str> {
    let remote = remote.trim();
    let (host, path) = if let Some((_scheme, rest)) = remote.split_once("://") {
        // URL form: the authority may carry `user@` and a `:port`.
        let authority_end = rest.find('/').ok_or("it has no repository path")?;
        let authority = &rest[..authority_end];
        let path = &rest[authority_end + 1..];
        if authority
            .rsplit('@')
            .next()
            .unwrap_or(authority)
            .contains(':')
        {
            return Err("it carries a port");
        }
        (authority.rsplit('@').next().unwrap_or(authority), path)
    } else if let Some((authority, path)) = remote.split_once(':') {
        // scp-like form: `user@host:owner/repo`.
        (authority.rsplit('@').next().unwrap_or(authority), path)
    } else {
        return Err("it is not a git remote URL");
    };
    if host.is_empty() {
        return Err("it has no host");
    }
    if host.chars().any(char::is_whitespace) {
        return Err("its host is not a host name");
    }
    if !is_github_host(host) {
        return Err("its host is not github.com");
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        [owner, repo] if !owner.chars().any(char::is_whitespace) => Ok(format!("{owner}/{repo}")),
        _ => Err("it is not owner/repo"),
    }
}

#[cfg(test)]
fn git_workspace(origin: Option<&str>) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(temp.path())
        .status()
        .unwrap();
    assert!(status.success());
    if let Some(origin) = origin {
        let status = std::process::Command::new("git")
            .args(["remote", "add", "origin", origin])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
    }
    temp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remotes_only() {
        for (remote, path) in [
            ("git@github.com:acme/widget.git", "acme/widget"),
            ("https://github.com/acme/widget.git", "acme/widget"),
            ("https://github.com/acme/widget", "acme/widget"),
            ("ssh://git@github.com/acme/widget.git", "acme/widget"),
            ("git://github.com/acme/widget.git", "acme/widget"),
            ("git@GITHUB.COM:acme/widget.git", "acme/widget"),
        ] {
            assert_eq!(parse_origin_remote(remote).unwrap(), path, "{remote}");
        }
        // Not a git transport, not a host, or a host whose web mapping is
        // UNKNOWN: every one of these must be refused instead of guessed.
        for (remote, why) in [
            ("/home/me/repo", "it is not a git remote URL"),
            ("file:///home/me/repo", "it has no host"),
            ("git@github.com:repo", "it is not owner/repo"),
            (
                "ssh://git@github.com:2222/acme/widget.git",
                "it carries a port",
            ),
            (
                "ssh://git@gitlab.example.com:2222/group/project.git",
                "it carries a port",
            ),
            (
                "git@gitlab.example.com:group/project.git",
                "its host is not github.com",
            ),
            (
                "https://mirror.example.com/github.com/acme/widget.git",
                "its host is not github.com",
            ),
        ] {
            assert_eq!(parse_origin_remote(remote).unwrap_err(), why, "{remote}");
        }
    }

    #[tokio::test]
    async fn number_form_resolves_against_origin() {
        let temp = git_workspace(Some("git@github.com:acme/widget.git"));
        let root = Some(temp.path());
        assert_eq!(
            resolve_number(root, "pr", 25, None).await.unwrap(),
            "https://github.com/acme/widget/pull/25"
        );
        assert_eq!(
            resolve_number(root, "issue", 22, None).await.unwrap(),
            "https://github.com/acme/widget/issues/22"
        );
    }

    /// Blocker 3: an SSH-with-port (or mirror/alias) origin has no known web
    /// endpoint — the number form must refuse, never invent
    /// `https://gitlab.example.com:2222/group/project/pull/25`.
    #[tokio::test]
    async fn ssh_port_origin_refuses_to_invent_a_web_url() {
        let temp = git_workspace(Some("ssh://git@gitlab.example.com:2222/group/project.git"));
        let error = resolve_number(Some(temp.path()), "pr", 25, None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "cannot map 'origin' (ssh://git@gitlab.example.com:2222/group/project.git) \
             to a GitHub web URL (it carries a port) — pass a full URL instead"
        );
        assert!(!error.contains("2222/pull"), "{error}");
        // A GitHub origin behind an SSH port is refused the same way.
        let temp = git_workspace(Some("ssh://git@github.com:2222/acme/widget.git"));
        assert!(
            resolve_number(Some(temp.path()), "pr", 25, None)
                .await
                .unwrap_err()
                .contains("it carries a port")
        );
        // A non-GitHub forge without a port is still not a known mapping.
        let temp = git_workspace(Some("git@gitlab.example.com:group/project.git"));
        assert!(
            resolve_number(Some(temp.path()), "pr", 25, None)
                .await
                .unwrap_err()
                .contains("its host is not github.com")
        );
    }

    #[tokio::test]
    async fn explicit_repo_skips_origin_resolution() {
        let temp = tempfile::tempdir().unwrap(); // deliberately NOT a git repo
        assert_eq!(
            resolve_number(Some(temp.path()), "pr", 25, Some("discord9/e-agent"))
                .await
                .unwrap(),
            "https://github.com/discord9/e-agent/pull/25"
        );
        assert_eq!(
            resolve_number(Some(temp.path()), "pr", 25, Some("github.com/acme/widget"))
                .await
                .unwrap(),
            "https://github.com/acme/widget/pull/25"
        );
        assert_eq!(parse_repo_selector("acme/widget").unwrap(), "acme/widget");
        assert!(parse_repo_selector("owner").is_err());
        assert!(parse_repo_selector("host/owner/repo/extra").is_err());
        // A non-GitHub host does not share GitHub's URL shape: refuse.
        assert!(
            parse_repo_selector("gitlab.example.com/group/project")
                .unwrap_err()
                .contains("must pass a full URL")
        );
    }

    /// Blocker 4: a session whose workspace is unknown (historical session)
    /// has no directory to read `origin` from, so the number form asks for a
    /// full URL; an explicit `repo` still resolves.
    #[tokio::test]
    async fn number_form_without_a_known_workspace_requires_a_url() {
        assert_eq!(
            resolve_number(None, "pr", 25, None).await.unwrap_err(),
            "this session's workspace is unknown — pass a full URL instead"
        );
        assert_eq!(
            resolve_number(None, "pr", 25, Some("acme/widget"))
                .await
                .unwrap(),
            "https://github.com/acme/widget/pull/25"
        );
    }

    #[tokio::test]
    async fn number_form_without_git_repo_errors_readably() {
        let temp = tempfile::tempdir().unwrap();
        let error = resolve_number(Some(temp.path()), "pr", 1, None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            format!(
                "not a git repository: {} — pass a full URL instead",
                temp.path().display()
            )
        );
    }

    #[tokio::test]
    async fn number_form_without_origin_errors_readably() {
        let temp = git_workspace(None);
        let error = resolve_number(Some(temp.path()), "issue", 22, None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            format!(
                "no git remote 'origin' in {} — pass a full URL instead",
                temp.path().display()
            )
        );
    }

    /// A remote that is not a remote at all (a local path) cannot be mapped
    /// to a web URL either, and the error names the raw remote.
    #[tokio::test]
    async fn unparsable_origin_names_the_raw_remote() {
        let temp = git_workspace(Some("/local/path/repo"));
        let error = resolve_number(Some(temp.path()), "pr", 25, None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "cannot map 'origin' (/local/path/repo) to a GitHub web URL \
             (it is not a git remote URL) — pass a full URL instead"
        );
    }

    /// Blocker 2: the resolved URL is built (and normalized) ONCE, so the
    /// `unlink_ref` compare sees exactly the value `link_ref` stored even
    /// when the origin spells its host in another case.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn uppercase_host_origin_add_and_unlink_share_one_identity() {
        let store = tool_tests::in_memory_store().await;
        let temp = git_workspace(Some("git@GITHUB.COM:acme/widget.git"));
        let root = temp.path();
        let workspace = Some(temp.path());
        let linked = execute(
            "link_ref",
            &store,
            root,
            workspace,
            "refs-upper",
            &json!({"kind": "pr", "number": 25}),
        )
        .await
        .unwrap();
        assert_eq!(
            linked,
            r#"{"prs":[{"label":"acme/widget#25","url":"https://github.com/acme/widget/pull/25"}],"issues":[],"links":[]}"#
        );
        let stored = store.list_refs(root, "refs-upper").await.unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|link| link.url.as_str())
                .collect::<Vec<_>>(),
            ["https://github.com/acme/widget/pull/25"],
            "the stored identity is the lowercase form"
        );
        let unlinked = execute(
            "unlink_ref",
            &store,
            root,
            workspace,
            "refs-upper",
            &json!({"kind": "pr", "number": 25}),
        )
        .await
        .expect("the number form must unlink what it just linked");
        assert_eq!(unlinked, r#"{"prs":[],"issues":[],"links":[]}"#);
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn tool_output_carries_labels_and_urls() {
        let rendered = render(vec![
            SessionRef {
                kind: "pr".into(),
                url: "https://github.com/acme/widget/pull/25".into(),
                label: "acme/widget#25".into(),
            },
            SessionRef {
                kind: "issue".into(),
                url: "https://gitlab.com/acme/widget/-/issues/22".into(),
                label: "gitlab.com/acme/widget#22".into(),
            },
            SessionRef {
                kind: "link".into(),
                url: "https://example.com/docs/x".into(),
                label: "example.com/docs/x".into(),
            },
        ]);
        assert_eq!(
            rendered,
            r#"{"prs":[{"label":"acme/widget#25","url":"https://github.com/acme/widget/pull/25"}],"issues":[{"label":"gitlab.com/acme/widget#22","url":"https://gitlab.com/acme/widget/-/issues/22"}],"links":[{"label":"example.com/docs/x","url":"https://example.com/docs/x"}]}"#
        );
    }

    #[tokio::test]
    async fn jsonl_reports_unsupported_before_touching_git() {
        let temp = tempfile::tempdir().unwrap();
        for (tool, arguments) in [
            ("link_ref", json!({"kind": "pr", "number": 25})),
            ("unlink_ref", json!({"kind": "pr", "number": 25})),
            ("list_refs", json!({})),
        ] {
            let error = execute(
                tool,
                &SessionStore::Jsonl,
                temp.path(),
                None,
                "s",
                &arguments,
            )
            .await
            .unwrap_err();
            assert_eq!(
                error, "this backend does not support session links",
                "{tool} must not probe git on JSONL"
            );
        }
    }

    #[cfg(feature = "sqlite")]
    pub(super) async fn in_memory_store() -> SessionStore {
        let session = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::session_sqlite::SqliteSession::connect(":memory:", "workspace", "s")
                .await
                .expect("in-memory SQLite"),
        ));
        SessionStore::Sqlite {
            session,
            path: ":memory:".into(),
        }
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn link_then_list_then_unlink_round_trip() {
        let store = in_memory_store().await;
        let temp = git_workspace(Some("git@github.com:discord9/e-agent.git"));
        let root = temp.path();
        let workspace = Some(root);
        let linked = execute(
            "link_ref",
            &store,
            root,
            workspace,
            "s",
            &json!({"kind": "pr", "number": 25}),
        )
        .await
        .unwrap();
        assert_eq!(
            linked,
            r#"{"prs":[{"label":"discord9/e-agent#25","url":"https://github.com/discord9/e-agent/pull/25"}],"issues":[],"links":[]}"#
        );
        let listed = execute("list_refs", &store, root, workspace, "s", &json!({}))
            .await
            .unwrap();
        assert_eq!(listed, linked);
        // The URL list_refs returned is what unlink_ref accepts verbatim.
        let listed: Value = serde_json::from_str(&listed).unwrap();
        let url = listed["prs"][0]["url"].as_str().unwrap();
        let unlinked = execute(
            "unlink_ref",
            &store,
            root,
            workspace,
            "s",
            &json!({ "url": url }),
        )
        .await
        .unwrap();
        assert_eq!(unlinked, r#"{"prs":[],"issues":[],"links":[]}"#);
    }

    /// Item 5: two links that render to the same label stay distinguishable,
    /// because every list entry carries its URL.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn identical_labels_stay_unlinkable_through_their_urls() {
        let store = in_memory_store().await;
        let root = std::path::Path::new("/not-a-repo");
        for url in [
            "https://example.org/search?q=a",
            "https://example.org/search?q=b",
        ] {
            execute(
                "link_ref",
                &store,
                root,
                None,
                "s",
                &json!({"kind": "link", "url": url}),
            )
            .await
            .unwrap();
        }
        let listed: Value = serde_json::from_str(
            &execute("list_refs", &store, root, None, "s", &json!({}))
                .await
                .unwrap(),
        )
        .unwrap();
        let links = listed["links"].as_array().unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(
            links[0]["label"], links[1]["label"],
            "the two URLs really do render identically"
        );
        let urls: Vec<&str> = links
            .iter()
            .map(|link| link["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            [
                "https://example.org/search?q=a",
                "https://example.org/search?q=b"
            ]
        );
        // Removing the second URL leaves exactly the first.
        let left = execute(
            "unlink_ref",
            &store,
            root,
            None,
            "s",
            &json!({ "kind": "link", "url": urls[1] }),
        )
        .await
        .unwrap();
        let left: Value = serde_json::from_str(&left).unwrap();
        assert_eq!(left["links"].as_array().unwrap().len(), 1);
        assert_eq!(left["links"][0]["url"], urls[0]);
    }

    /// Item 8: an explicit kind wins over the URL's shape — selecting Issue
    /// and pasting a URL with no issue marker records an ISSUE.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn explicit_url_kind_beats_the_url_path() {
        let store = in_memory_store().await;
        let root = std::path::Path::new("/not-a-repo");
        let linked: Value = serde_json::from_str(
            &execute(
                "link_ref",
                &store,
                root,
                None,
                "s",
                &json!({"kind": "issue", "url": "https://example.org/tracker/7"}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(linked["prs"].as_array().unwrap().len(), 0);
        assert_eq!(linked["issues"].as_array().unwrap().len(), 1);
        assert_eq!(linked["issues"][0]["url"], "https://example.org/tracker/7");
        // No kind at all still infers a RECOGNISED PR/issue path…
        let inferred: Value = serde_json::from_str(
            &execute(
                "link_ref",
                &store,
                root,
                None,
                "s",
                &json!({"url": "https://example.org/own/repo/pull/8"}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(inferred["prs"].as_array().unwrap().len(), 1);
        // …but a URL that is neither a PR nor an issue is not silently filed
        // as one: the caller must say what it is.
        let error = execute(
            "link_ref",
            &store,
            root,
            None,
            "s",
            &json!({"url": "https://example.org/docs/x"}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "https://example.org/docs/x is not a pull-request or issue URL — \
             pass `kind`: \"pr\", \"issue\", or \"link\""
        );
        assert!(
            store
                .list_refs(root, "s")
                .await
                .unwrap()
                .iter()
                .all(|link| link.url != "https://example.org/docs/x"),
            "a refused URL must not be stored"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn unlink_of_an_unlinked_number_names_it() {
        let store = in_memory_store().await;
        let temp = git_workspace(Some("git@github.com:discord9/e-agent.git"));
        let root = temp.path();
        let workspace = Some(root);
        let error = execute(
            "unlink_ref",
            &store,
            root,
            workspace,
            "s",
            &json!({"kind": "pr", "number": 25}),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "PR 25 is not linked");
        let error = execute(
            "unlink_ref",
            &store,
            root,
            workspace,
            "s",
            &json!({"kind": "issue", "number": 22}),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Issue 22 is not linked");
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn plain_link_round_trip_uses_host_path_label() {
        let store = in_memory_store().await;
        let root = std::path::Path::new("/not-a-repo");
        let linked = execute(
            "link_ref",
            &store,
            root,
            None,
            "s",
            &json!({"kind": "link", "url": "https://example.com/docs/x"}),
        )
        .await
        .unwrap();
        assert_eq!(
            linked,
            r#"{"prs":[],"issues":[],"links":[{"label":"example.com/docs/x","url":"https://example.com/docs/x"}]}"#
        );
        let listed = execute("list_refs", &store, root, None, "s", &json!({}))
            .await
            .unwrap();
        assert_eq!(listed, linked);
        let unlinked = execute(
            "unlink_ref",
            &store,
            root,
            None,
            "s",
            &json!({"kind": "link", "url": "https://example.com/docs/x"}),
        )
        .await
        .unwrap();
        assert_eq!(unlinked, r#"{"prs":[],"issues":[],"links":[]}"#);
        let error = execute(
            "unlink_ref",
            &store,
            root,
            None,
            "s",
            &json!({"kind": "link", "url": "https://example.com/docs/x"}),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Link example.com/docs/x is not linked");
    }

    /// Blocker 4 at the tool boundary: the Git a number resolves against is
    /// the session's EXECUTION workspace, not the persistence root the store
    /// is bound to. Here the store root (repo A) must be ignored.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn number_resolution_reads_the_execution_workspace_not_the_store_root() {
        let store = in_memory_store().await;
        let parent = git_workspace(Some("git@github.com:acme/parent.git"));
        let child = git_workspace(Some("git@github.com:acme/child.git"));
        let linked: Value = serde_json::from_str(
            &execute(
                "link_ref",
                &store,
                parent.path(),
                Some(child.path()),
                "s",
                &json!({"kind": "pr", "number": 7}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            linked["prs"][0]["url"], "https://github.com/acme/child/pull/7",
            "the child's own origin resolves, never the persistence root's"
        );
    }

    #[test]
    fn selectors_are_exclusive_and_closed() {
        let parse = |value: Value| {
            let object = value.as_object().unwrap();
            parse_selector("link_ref", object)
        };
        assert!(matches!(
            parse(json!({"url": "https://github.com/a/b/pull/1"})).unwrap(),
            Selector::Url { kind: None, .. }
        ));
        assert!(matches!(
            parse(json!({"kind": "link", "url": "https://example.com/docs/x"})).unwrap(),
            Selector::Url {
                kind: Some("link"),
                ..
            }
        ));
        assert!(matches!(
            parse(json!({"kind": "pr", "number": 1})).unwrap(),
            Selector::Number { .. }
        ));
        for invalid in [
            json!({"kind": "pr", "number": 1, "url": "https://github.com/a/b/pull/1"}),
            json!({"url": "https://github.com/a/b/pull/1", "repo": "a/b"}),
            json!({"kind": "pr"}),
            json!({"kind": "link", "number": 1}),
            json!({"kind": "commit", "number": 1}),
            json!({}),
        ] {
            assert!(parse(invalid.clone()).is_err(), "{invalid}");
        }
    }
}
