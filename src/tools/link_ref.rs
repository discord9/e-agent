//! `link_ref`: record, remove, or list the pull-request / issue links of
//! THIS session. The URL is the identity; a bare number is resolved against
//! the workspace's `origin` remote, and a full URL stays accepted as an
//! escape hatch.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::session_store::{SessionRef, SessionStore, ref_kind_from_url};
use crate::tools::spec;

/// The registered tool. Zero state: the runner intercepts execution by name
/// and binds the session's store/root/session.
pub(crate) struct LinkRef;

#[async_trait]
impl Tool for LinkRef {
    fn spec(&self) -> ToolSpec {
        spec(
            "link_ref",
            "Record, remove, or list a pull-request / issue link on THIS session. Use `pr: 25` \
             or `issue: 22` and the number is resolved against the current workspace's git \
             `origin` remote (never guessed); pass a full `url` only for another repository or \
             host. `remove: true` removes the same identifier, `list: true` shows the current \
             links without changing them. The result is \
             {\"prs\": [\"owner/repo#25\"], \"issues\": [\"owner/repo#22\"]}.",
            json!({
                "pr": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "pull-request number in this workspace's origin repository"
                },
                "issue": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "issue number in this workspace's origin repository"
                },
                "url": {
                    "type": "string",
                    "description": "absolute http(s) URL (escape hatch for another repo or host)"
                },
                "remove": {
                    "type": "boolean",
                    "description": "remove this exact link instead of adding it (default false)"
                },
                "list": {
                    "type": "boolean",
                    "description": "return the current links without changing them (default false)"
                }
            }),
            &[],
        )
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("link_ref is executed by the session runner".into())
    }
}

/// One parsed `link_ref` call.
enum Action {
    List,
    Add {
        kind: &'static str,
        url: String,
    },
    Remove {
        kind: &'static str,
        url: String,
        number: Option<u64>,
    },
}

/// Execute one `link_ref` call with the runner-bound session context.
pub(crate) async fn execute(
    store: &SessionStore,
    root: &Path,
    session: &str,
    arguments: &Value,
) -> Result<String, String> {
    // The backend check comes first: on JSONL even `pr: 25` must report the
    // explicit unsupported error, never a git-resolution failure.
    if !store.supports_session_refs() {
        return Err(crate::session_store::SESSION_REFS_UNSUPPORTED.into());
    }
    let action = parse_action(root, arguments).await?;
    match action {
        Action::List => Ok(render(
            store
                .list_refs(root, session)
                .await
                .map_err(|e| format!("{e:#}"))?,
        )),
        Action::Add { kind, url } => Ok(render(
            store
                .add_ref(root, session, kind, &url)
                .await
                .map_err(|e| format!("{e:#}"))?,
        )),
        Action::Remove { kind, url, number } => {
            let refs = store
                .list_refs(root, session)
                .await
                .map_err(|e| format!("{e:#}"))?;
            if !refs.iter().any(|link| link.kind == kind && link.url == url) {
                let what = if kind == "pr" { "PR" } else { "Issue" };
                let identifier = number.map(|n| n.to_string()).unwrap_or(url);
                return Err(format!("{what} {identifier} is not linked"));
            }
            Ok(render(
                store
                    .remove_ref(root, session, kind, &url)
                    .await
                    .map_err(|e| format!("{e:#}"))?,
            ))
        }
    }
}

async fn parse_action(root: &Path, arguments: &Value) -> Result<Action, String> {
    let object = arguments
        .as_object()
        .ok_or("link_ref arguments must be a JSON object")?;
    let list = optional_bool(object.get("list"), "list")?.unwrap_or(false);
    let remove = optional_bool(object.get("remove"), "remove")?.unwrap_or(false);
    let pr = optional_number(object.get("pr"), "pr")?;
    let issue = optional_number(object.get("issue"), "issue")?;
    let url = optional_string(object.get("url"), "url")?;

    if list {
        if remove || pr.is_some() || issue.is_some() || url.is_some() {
            return Err("link_ref `list: true` cannot be combined with a link identifier".into());
        }
        return Ok(Action::List);
    }

    let (kind, url, number) = match (pr, issue, url) {
        (Some(_), Some(_), _) => {
            return Err("link_ref takes either `pr` or `issue`, not both".into());
        }
        (Some(number), None, None) => (
            "pr",
            resolve_number(root, "pr", number).await?,
            Some(number),
        ),
        (None, Some(number), None) => (
            "issue",
            resolve_number(root, "issue", number).await?,
            Some(number),
        ),
        (None, None, Some(url)) => (ref_kind_from_url(&url), url, None),
        (Some(_), None, Some(_)) | (None, Some(_), Some(_)) => {
            return Err("link_ref takes either a number (`pr`/`issue`) or `url`, not both".into());
        }
        (None, None, None) => {
            return Err(
                "link_ref needs `pr: <number>`, `issue: <number>`, `url: <full URL>`, or `list: true`"
                    .into(),
            );
        }
    };

    Ok(if remove {
        Action::Remove { kind, url, number }
    } else {
        Action::Add { kind, url }
    })
}

/// Render the tool's public view: displayed labels split by kind.
fn render(refs: Vec<SessionRef>) -> String {
    let mut prs = Vec::new();
    let mut issues = Vec::new();
    for link in refs {
        if link.kind == "pr" {
            prs.push(link.label);
        } else {
            issues.push(link.label);
        }
    }
    json!({ "prs": prs, "issues": issues }).to_string()
}

fn optional_bool(value: Option<&Value>, name: &str) -> Result<Option<bool>, String> {
    value
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("link_ref `{name}` must be a boolean"))
        })
        .transpose()
}

fn optional_number(value: Option<&Value>, name: &str) -> Result<Option<u64>, String> {
    value
        .map(|value| {
            let number = value
                .as_u64()
                .ok_or_else(|| format!("link_ref `{name}` must be a non-negative integer"))?;
            if number == 0 {
                return Err(format!("link_ref `{name}` must be at least 1"));
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
                .ok_or_else(|| format!("link_ref `{name}` must be a string"))
        })
        .transpose()
}

/// Resolve `pr: N` / `issue: N` through the workspace's `origin` remote.
///
/// The number form is the primary path; every failure names the full-URL
/// fallback instead of guessing a repository.
pub(crate) async fn resolve_number(root: &Path, kind: &str, number: u64) -> Result<String, String> {
    let remote = origin_remote(root).await?;
    let (host, path) = parse_origin_remote(&remote).map_err(|_| {
        format!("cannot read 'origin' ({remote}) as host/owner/repo — pass a full URL instead")
    })?;
    let action = if kind == "pr" { "pull" } else { "issues" };
    Ok(format!("https://{host}/{path}/{action}/{number}"))
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
        let temp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:acme/widget.git"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            resolve_number(temp.path(), "pr", 25).await.unwrap(),
            "https://github.com/acme/widget/pull/25"
        );
        assert_eq!(
            resolve_number(temp.path(), "issue", 22).await.unwrap(),
            "https://github.com/acme/widget/issues/22"
        );
    }

    #[tokio::test]
    async fn number_form_without_git_repo_errors_readably() {
        let temp = tempfile::tempdir().unwrap();
        let error = resolve_number(temp.path(), "pr", 1).await.unwrap_err();
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
        let temp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let error = resolve_number(temp.path(), "issue", 22).await.unwrap_err();
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
        let temp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["remote", "add", "origin", "/local/path/repo"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let error = resolve_number(temp.path(), "pr", 25).await.unwrap_err();
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
        ]);
        assert_eq!(
            rendered,
            r#"{"prs":["acme/widget#25"],"issues":["gitlab.com/acme/widget#22"]}"#
        );
    }

    #[tokio::test]
    async fn jsonl_reports_unsupported_before_touching_git() {
        let temp = tempfile::tempdir().unwrap();
        let error = execute(&SessionStore::Jsonl, temp.path(), "s", &json!({"pr": 25}))
            .await
            .unwrap_err();
        assert_eq!(
            error, "this backend does not support session links",
            "a JSONL workspace must not be probed for git"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn remove_of_an_unlinked_number_names_it() {
        let temp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:acme/widget.git"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let store = SessionStore::Sqlite {
            session: std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::session_sqlite::SqliteSession::connect(":memory:", "workspace", "s")
                    .await
                    .unwrap(),
            )),
            path: ":memory:".into(),
        };
        let error = execute(&store, temp.path(), "s", &json!({"pr": 25, "remove": true}))
            .await
            .unwrap_err();
        assert_eq!(error, "PR 25 is not linked");
        let error = execute(
            &store,
            temp.path(),
            "s",
            &json!({"issue": 22, "remove": true}),
        )
        .await
        .unwrap_err();
        assert_eq!(error, "Issue 22 is not linked");
    }
}
