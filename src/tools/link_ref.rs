//! `link_ref`: the main agent records a pull-request or issue URL on its own
//! session. Values come only from this tool or the owner's manual web edit —
//! nothing is discovered from branch names, `gh`, or session titles.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::session_store::{SessionRef, SessionStore, split_refs};
use crate::tools::spec;

/// The registered tool, main session only (the session factory decides
/// registration; a delegate/fork child builds the common builtins without
/// it). `store` is the session's already-open handle and `root`/`session`
/// name the row it writes.
pub(crate) fn link_ref_tool(store: SessionStore, root: PathBuf, session: String) -> Box<dyn Tool> {
    Box::new(LinkRef {
        store,
        root,
        session,
    })
}

struct LinkRef {
    store: SessionStore,
    root: PathBuf,
    session: String,
}

#[async_trait::async_trait]
impl Tool for LinkRef {
    fn spec(&self) -> ToolSpec {
        spec(
            "link_ref",
            "Record or remove one pull-request or issue URL on THIS session. The URL is the \
             identity (the same `#14` in two repositories is two different links), so pass the \
             whole URL, never a number. `kind` defaults to `pr`; `remove: true` deletes the exact \
             URL again. Returns this session's current PR and issue lists.",
            json!({
                "url": {
                    "type": "string",
                    "description": "absolute http(s) URL of the pull request or issue"
                },
                "kind": {
                    "type": "string",
                    "enum": ["pr", "issue"],
                    "description": "which list the URL belongs to (default \"pr\")"
                },
                "remove": {
                    "type": "boolean",
                    "description": "remove this exact URL instead of adding it (default false)"
                }
            }),
            &["url"],
        )
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let object = arguments
            .as_object()
            .ok_or("link_ref arguments must be a JSON object")?;
        let url = object
            .get("url")
            .and_then(Value::as_str)
            .ok_or("link_ref `url` must be a string")?;
        let kind = match object.get("kind") {
            None | Some(Value::Null) => "pr",
            Some(Value::String(kind)) => kind,
            Some(_) => return Err("link_ref `kind` must be a string".into()),
        };
        let remove = match object.get("remove") {
            None => false,
            Some(Value::Bool(remove)) => *remove,
            Some(_) => return Err("link_ref `remove` must be a boolean".into()),
        };
        // Validation happens before the store is touched, so a refused URL
        // never rewrites the session's metadata row.
        let link = SessionRef::new(kind, url)?;
        let refs = self
            .store
            .update_refs(&self.root, &self.session, |refs| {
                if remove {
                    refs.retain(|existing| existing != &link);
                } else {
                    refs.push(link.clone());
                }
                Ok(())
            })
            .await
            .map_err(|error| format!("cannot update session links: {error:#}"))?;
        let (prs, issues) = split_refs(&refs);
        Ok(ToolOutput::text(
            json!({ "prs": prs, "issues": issues }).to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxPolicy;
    use crate::session_store::SessionMeta;
    use crate::workspace::Workspace;

    async fn store_with_session() -> (tempfile::TempDir, SessionStore, String) {
        let temp = tempfile::tempdir().unwrap();
        let session = format!("refs-{}", crate::session::new_id());
        let store = SessionStore::Jsonl;
        store
            .create_meta(
                temp.path(),
                &session,
                Some("gpt-x"),
                Some("main"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        (temp, store, session)
    }

    fn tool(root: &std::path::Path, store: &SessionStore, session: &str) -> Box<dyn Tool> {
        link_ref_tool(store.clone(), root.to_path_buf(), session.to_owned())
    }

    async fn call(tool: &dyn Tool, arguments: Value) -> Result<Value, String> {
        let output = tool.execute(arguments).await?;
        Ok(serde_json::from_str(&output.content).expect("link_ref returns JSON lists"))
    }

    async fn stored(
        root: &std::path::Path,
        store: &SessionStore,
        session: &str,
    ) -> Vec<SessionRef> {
        let mut list: Vec<SessionMeta> = store.list_meta(root).await.unwrap();
        list.retain(|meta| meta.session_id == session);
        list.pop().expect("session metadata row").refs
    }

    /// Both kinds are added, the returned lists are exactly what was stored,
    /// and `remove` deletes one URL again.
    #[tokio::test]
    async fn add_and_remove_both_kinds_returns_stored_lists() {
        let (temp, store, session) = store_with_session().await;
        let tool = tool(temp.path(), &store, &session);
        let pr = "https://github.com/acme/widget/pull/14";
        let issue = "https://github.com/acme/widget/issues/22";

        let after_pr = call(&*tool, json!({"url": pr})).await.unwrap();
        assert_eq!(after_pr, json!({"prs": [pr], "issues": []}));
        let after_issue = call(&*tool, json!({"url": issue, "kind": "issue"}))
            .await
            .unwrap();
        assert_eq!(after_issue, json!({"prs": [pr], "issues": [issue]}));
        assert_eq!(
            stored(temp.path(), &store, &session).await,
            vec![
                SessionRef::new("pr", pr).unwrap(),
                SessionRef::new("issue", issue).unwrap(),
            ],
            "the returned lists must match what was stored"
        );

        let after_remove = call(&*tool, json!({"url": pr, "remove": true}))
            .await
            .unwrap();
        assert_eq!(after_remove, json!({"prs": [], "issues": [issue]}));
        assert_eq!(
            stored(temp.path(), &store, &session).await,
            vec![SessionRef::new("issue", issue).unwrap()]
        );
        // Removing an absent URL is an idempotent no-op, not an error.
        let again = call(&*tool, json!({"url": pr, "remove": true}))
            .await
            .unwrap();
        assert_eq!(again, json!({"prs": [], "issues": [issue]}));
    }

    /// The same URL twice collapses to one entry; the same numbering in a
    /// different repository stays a different link.
    #[tokio::test]
    async fn duplicate_url_collapses() {
        let (temp, store, session) = store_with_session().await;
        let tool = tool(temp.path(), &store, &session);
        let pr = "https://github.com/acme/widget/pull/14";
        let other = "https://github.com/acme/gadget/pull/14";
        for url in [pr, pr, other] {
            call(&*tool, json!({"url": url})).await.unwrap();
        }
        let listed = call(
            &*tool,
            json!({"url": "https://github.com/acme/widget/pull/99"}),
        )
        .await
        .unwrap();
        assert_eq!(
            listed,
            json!({"prs": [pr, other, "https://github.com/acme/widget/pull/99"], "issues": []})
        );
        let refs = stored(temp.path(), &store, &session).await;
        assert_eq!(refs.iter().filter(|link| link.url == pr).count(), 1);
        assert_eq!(refs.len(), 3, "insertion order, one entry per exact URL");
    }

    /// Credentials, a non-http scheme, whitespace and an unknown kind are
    /// refused as plain strings — and nothing is written.
    #[tokio::test]
    async fn invalid_urls_and_unknown_kinds_are_refused() {
        let (temp, store, session) = store_with_session().await;
        let tool = tool(temp.path(), &store, &session);
        for (arguments, expected) in [
            (
                json!({"url": "https://user:secret@github.com/acme/widget/pull/1"}),
                "credentials",
            ),
            (
                json!({"url": "ftp://github.com/acme/widget/pull/1"}),
                "http",
            ),
            (
                json!({"url": "https://github.com/acme/widget/pull/1 extra"}),
                "whitespace",
            ),
            (
                json!({"url": "https://github.com/acme/wid get/pull/1"}),
                "whitespace",
            ),
            (
                json!({"url": "https://github.com/acme/widget/pull/1", "kind": "commit"}),
                "unknown ref kind",
            ),
        ] {
            let error = tool.execute(arguments).await.unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        assert!(
            stored(temp.path(), &store, &session).await.is_empty(),
            "a refused link must not rewrite the metadata row"
        );
    }

    /// A delegate/fork child builds the common builtin set and never sees
    /// `link_ref`, so a child session cannot call it.
    #[test]
    fn child_session_cannot_call_link_ref() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let child = crate::tools::builtins_with_background(
            workspace,
            crate::tools::BackgroundTasks::new(None, SandboxPolicy::fixed(None)),
            SandboxPolicy::fixed(None),
            false,
            true,
            Some("sub".into()),
        );
        assert!(
            !child.iter().any(|tool| tool.spec().name == "link_ref"),
            "delegate/fork children must not expose link_ref"
        );
    }
}
