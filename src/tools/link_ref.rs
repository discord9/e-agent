//! Session PR/issue/plain links, split by verb: `link_ref` adds,
//! `unlink_ref` removes, `list_refs` reads. The URL is the identity; a `kind` + `number`
//! selector is resolved against the workspace's `origin` remote (or an
//! explicit `repo`), and a full URL stays accepted as an escape hatch.

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
                            "description": "optional owner/repo or host/owner/repo; defaults to this workspace's git origin"
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
                            "description": "absolute http(s) URL; skips repository resolution and infers pr/issue from the path"
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
             this workspace's git `origin` remote unless `repo` (owner/repo or \
             host/owner/repo) names another repository; a full `url` replaces `kind` + \
             `number` and skips repository resolution. `kind: \"link\"` records a plain \
             non-PR/issue URL and requires `url`. Returns the session's current links: \
             {\"prs\": [\"owner/repo#25\"], \"issues\": [\"owner/repo#22\"], \"links\": [\"host/path\"]}.",
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
             unless `repo` names another repository) or a full `url` (with `kind: \"link\"` \
             for a plain link). Removing something that is not linked is an error naming it. \
             Returns the session's current links: \
             {\"prs\": [\"owner/repo#25\"], \"issues\": [\"owner/repo#22\"], \"links\": [\"host/path\"]}.",
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
                          without changing them. Takes no arguments. The result is \
                          {\"prs\": [\"owner/repo#25\"], \"issues\": [\"owner/repo#22\"], \"links\": [\"host/path\"]}."
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
        /// Explicit `kind` when supplied; `None` infers pr/issue from the path.
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
pub(crate) async fn execute(
    tool: &str,
    store: &SessionStore,
    root: &Path,
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
    let resolved = resolve_selector(root, selector).await?;
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

async fn resolve_selector(root: &Path, selector: Selector) -> Result<Resolved, String> {
    match selector {
        Selector::Url { url, kind } => {
            let parsed = parse_ref_url(&url)?;
            let url = parsed.to_string();
            let kind = kind.unwrap_or_else(|| ref_kind_from_url(&url));
            Ok(Resolved {
                kind,
                display: session_ref_label_for_kind(kind, &url),
                url,
            })
        }
        Selector::Number { kind, number, repo } => {
            let url = resolve_number(root, kind, number, repo.as_deref()).await?;
            let display = match repo {
                Some(repo) => format!("{repo}#{number}"),
                None => number.to_string(),
            };
            Ok(Resolved { kind, url, display })
        }
    }
}

/// Render the public view shared by all three tools: displayed labels split
/// by kind.
fn render(refs: Vec<SessionRef>) -> String {
    let mut prs = Vec::new();
    let mut issues = Vec::new();
    let mut links = Vec::new();
    for link in refs {
        match link.kind.as_str() {
            "pr" => prs.push(link.label),
            "issue" => issues.push(link.label),
            _ => links.push(link.label),
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

/// Resolve `kind` + `number` through an explicit `repo` or the workspace's
/// `origin` remote. Every failure names the full-URL fallback instead of
/// guessing a repository.
pub(crate) async fn resolve_number(
    root: &Path,
    kind: &str,
    number: u64,
    repo: Option<&str>,
) -> Result<String, String> {
    if kind != "pr" && kind != "issue" {
        return Err(format!(
            "unknown link kind {kind:?}; expected \"pr\" or \"issue\""
        ));
    }
    let (host, path) = match repo {
        Some(repo) => parse_repo_selector(repo)?,
        None => {
            let remote = origin_remote(root).await?;
            parse_origin_remote(&remote).map_err(|_| {
                format!(
                    "cannot read 'origin' ({remote}) as host/owner/repo — pass a full URL instead"
                )
            })?
        }
    };
    let action = if kind == "pr" { "pull" } else { "issues" };
    Ok(format!("https://{host}/{path}/{action}/{number}"))
}

/// Parse an explicit `owner/repo` (default host `github.com`) or
/// `host/owner/repo` selector.
fn parse_repo_selector(repo: &str) -> Result<(String, String), String> {
    let repo = repo.trim().trim_end_matches('/');
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let parts: Vec<&str> = repo.split('/').collect();
    let usable = |part: &str| !part.is_empty() && !part.chars().any(char::is_whitespace);
    match parts.as_slice() {
        [owner, name] if usable(owner) && usable(name) => {
            Ok(("github.com".to_owned(), format!("{owner}/{name}")))
        }
        [host, owner, name] if usable(host) && usable(owner) && usable(name) => {
            Ok((host.to_string(), format!("{owner}/{name}")))
        }
        _ => Err("`repo` must be owner/repo or host/owner/repo — pass a full URL instead".into()),
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

/// Parse a git remote into `(host, path-without-.git)`. Accepts scp-like
/// (`git@github.com:owner/repo.git`), URL (`https://`, `ssh://`, `git://`)
/// and rejects local paths / anything without a host.
fn parse_origin_remote(remote: &str) -> Result<(String, String), ()> {
    let remote = remote.trim();
    let (host, path) = if remote.contains("://") {
        let parsed = url::Url::parse(remote).map_err(|_| ())?;
        let host = parsed.host_str().ok_or(())?.to_owned();
        let port = parsed
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        (
            format!("{host}{port}"),
            parsed.path().trim_matches('/').to_owned(),
        )
    } else if let Some((authority, path)) = remote.split_once(':') {
        let host = authority.rsplit('@').next().unwrap_or(authority).to_owned();
        (host, path.trim_matches('/').to_owned())
    } else {
        return Err(());
    };
    let path = path.strip_suffix(".git").unwrap_or(&path).to_owned();
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if host.is_empty() || host.chars().any(char::is_whitespace) || segments.len() < 2 {
        return Err(());
    }
    Ok((host, path))
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
    fn parses_common_git_remotes() {
        for (remote, host, path) in [
            (
                "git@github.com:acme/widget.git",
                "github.com",
                "acme/widget",
            ),
            (
                "https://github.com/acme/widget.git",
                "github.com",
                "acme/widget",
            ),
            (
                "https://github.com/acme/widget",
                "github.com",
                "acme/widget",
            ),
            (
                "ssh://git@gitlab.example.com:2222/group/project.git",
                "gitlab.example.com:2222",
                "group/project",
            ),
            (
                "git://github.com/acme/widget.git",
                "github.com",
                "acme/widget",
            ),
        ] {
            assert_eq!(
                parse_origin_remote(remote).unwrap(),
                (host.to_owned(), path.to_owned()),
                "{remote}"
            );
        }
        for remote in [
            "/home/me/repo",
            "file:///home/me/repo",
            "git@github.com:repo",
        ] {
            assert!(parse_origin_remote(remote).is_err(), "{remote}");
        }
    }

    #[tokio::test]
    async fn number_form_resolves_against_origin() {
        let temp = git_workspace(Some("git@github.com:acme/widget.git"));
        assert_eq!(
            resolve_number(temp.path(), "pr", 25, None).await.unwrap(),
            "https://github.com/acme/widget/pull/25"
        );
        assert_eq!(
            resolve_number(temp.path(), "issue", 22, None)
                .await
                .unwrap(),
            "https://github.com/acme/widget/issues/22"
        );
    }

    #[tokio::test]
    async fn explicit_repo_skips_origin_resolution() {
        let temp = tempfile::tempdir().unwrap(); // deliberately NOT a git repo
        assert_eq!(
            resolve_number(temp.path(), "pr", 25, Some("discord9/e-agent"))
                .await
                .unwrap(),
            "https://github.com/discord9/e-agent/pull/25"
        );
        assert_eq!(
            resolve_number(
                temp.path(),
                "issue",
                22,
                Some("gitlab.example.com/group/project")
            )
            .await
            .unwrap(),
            "https://gitlab.example.com/group/project/issues/22"
        );
        assert_eq!(
            parse_repo_selector("acme/widget").unwrap(),
            ("github.com".to_owned(), "acme/widget".to_owned())
        );
        assert!(parse_repo_selector("owner").is_err());
        assert!(parse_repo_selector("host/owner/repo/extra").is_err());
    }

    #[tokio::test]
    async fn number_form_without_git_repo_errors_readably() {
        let temp = tempfile::tempdir().unwrap();
        let error = resolve_number(temp.path(), "pr", 1, None)
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
        let error = resolve_number(temp.path(), "issue", 22, None)
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

    #[tokio::test]
    async fn unparsable_origin_names_the_raw_remote() {
        let temp = git_workspace(Some("/local/path/repo"));
        let error = resolve_number(temp.path(), "pr", 25, None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "cannot read 'origin' (/local/path/repo) as host/owner/repo — pass a full URL instead"
        );
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn tool_output_is_labels_split_by_kind() {
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
            r#"{"prs":["acme/widget#25"],"issues":["gitlab.com/acme/widget#22"],"links":["example.com/docs/x"]}"#
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
            let error = execute(tool, &SessionStore::Jsonl, temp.path(), "s", &arguments)
                .await
                .unwrap_err();
            assert_eq!(
                error, "this backend does not support session links",
                "{tool} must not probe git on JSONL"
            );
        }
    }

    #[cfg(feature = "sqlite")]
    async fn in_memory_store() -> SessionStore {
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
        let linked = execute(
            "link_ref",
            &store,
            root,
            "s",
            &json!({"kind": "pr", "number": 25}),
        )
        .await
        .unwrap();
        assert_eq!(
            linked,
            r#"{"prs":["discord9/e-agent#25"],"issues":[],"links":[]}"#
        );
        let listed = execute("list_refs", &store, root, "s", &json!({}))
            .await
            .unwrap();
        assert_eq!(listed, linked);
        let unlinked = execute(
            "unlink_ref",
            &store,
            root,
            "s",
            &json!({"url": "https://github.com/discord9/e-agent/pull/25"}),
        )
        .await
        .unwrap();
        assert_eq!(unlinked, r#"{"prs":[],"issues":[],"links":[]}"#);
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn unlink_of_an_unlinked_number_names_it() {
        let store = in_memory_store().await;
        let temp = git_workspace(Some("git@github.com:discord9/e-agent.git"));
        let root = temp.path();
        let error = execute(
            "unlink_ref",
            &store,
            root,
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
            "s",
            &json!({"kind": "link", "url": "https://example.com/docs/x"}),
        )
        .await
        .unwrap();
        assert_eq!(
            linked,
            r#"{"prs":[],"issues":[],"links":["example.com/docs/x"]}"#
        );
        let listed = execute("list_refs", &store, root, "s", &json!({}))
            .await
            .unwrap();
        assert_eq!(listed, linked);
        let unlinked = execute(
            "unlink_ref",
            &store,
            root,
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
            "s",
            &json!({"kind": "link", "url": "https://example.com/docs/x"}),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Link example.com/docs/x is not linked");
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
