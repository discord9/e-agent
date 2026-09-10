//! Logical transcript history. Omitting scope preserves the original
//! current-session API; explicit scopes query the configured store read-only.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{Message, SessionEntry, Tool, ToolOutput, ToolSpec};
use crate::session_store::{HistoryEntry, HistoryQuery, SessionStore, derive_workspace_id};

pub const HISTORY_DEFAULT_LIMIT: usize = 20;
pub const HISTORY_MAX_LIMIT: usize = 100;

pub struct History;

#[async_trait]
impl Tool for History {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "history".into(),
            description: "Read logical transcripts. Without `scope`, the current session API is unchanged. Explicit `scope` is `global`, `workspace`, or `session`; scoped list/search return provenance and a cursor. `global` searches this configured store only. IDs require an explicit scope.".into(),
            parameters: json!({
                "oneOf": [
                    {"type":"object","properties":{"action":{"const":"list"},"limit":{"type":"integer","minimum":1,"maximum":100},"scope":{"enum":["global","workspace","session"]},"workspace_id":{"type":"string","minLength":1},"session_id":{"type":"string","minLength":1},"cursor":{"type":"string","minLength":1}},"required":["action"],"additionalProperties":false},
                    {"type":"object","properties":{"action":{"const":"read"},"seq":{"type":"integer"},"scope":{"const":"session"},"workspace_id":{"type":"string","minLength":1},"session_id":{"type":"string","minLength":1}},"required":["action","seq"],"additionalProperties":false},
                    {"type":"object","properties":{"action":{"const":"search"},"query":{"type":"string","minLength":1},"limit":{"type":"integer","minimum":1,"maximum":100},"scope":{"enum":["global","workspace","session"]},"workspace_id":{"type":"string","minLength":1},"session_id":{"type":"string","minLength":1},"cursor":{"type":"string","minLength":1}},"required":["action","query"],"additionalProperties":false}
                ]
            }),
        }
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("history is executed by the session runner".into())
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Scope {
    Global,
    Workspace,
    Session,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    action: String,
    scope: Scope,
    workspace_id: Option<String>,
    session_id: Option<String>,
    query: Option<String>,
    after_workspace: String,
    after_session: String,
    after_seq: i64,
    #[serde(default)]
    after_event_time_us: Option<i64>,
    #[serde(default)]
    after_payload: Option<String>,
}

struct Args {
    action: String,
    limit: usize,
    seq: Option<i64>,
    query: Option<String>,
    scope: Option<Scope>,
    workspace_id: Option<String>,
    session_id: Option<String>,
    cursor: Option<String>,
}

pub async fn execute(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    arguments: &Value,
) -> Result<String, String> {
    let args = parse_arguments(arguments)?;
    if args.scope.is_none() {
        return execute_current(store, root, session, args).await;
    }
    execute_scoped(store, root, session, args).await
}

async fn execute_current(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    args: Args,
) -> Result<String, String> {
    if store.supports_history_query() {
        let entries = store
            .query_history(&HistoryQuery {
                workspace_id: Some(derive_workspace_id(root)),
                session_id: Some(session.to_owned()),
                query: args.query.clone(),
                after: None,
                after_event_time: None,
                after_payload: None,
                exact_seq: args.seq,
                limit: args.limit,
                default_search_window: args.action == "search",
            })
            .await
            .map_err(history_load_error)?;
        return match args.action.as_str() {
            "list" | "search" => Ok(json!({"entries":entries.into_iter().map(|e| json!({"seq":e.seq,"entry":e.entry})).collect::<Vec<_>>()}).to_string()),
            "read" => entries.into_iter().next().map(|e| json!({"seq":e.seq,"entry":e.entry}).to_string()).ok_or_else(|| "history entry not found".into()),
            _ => unreachable!(),
        };
    }
    let entries = store
        .load_with_seq(root, session)
        .await
        .map_err(history_load_error)?;
    match args.action.as_str() {
        "list" => Ok(json!({"entries": ordered_entries(&entries).into_iter().take(args.limit).map(|(seq, entry)| json!({"seq":seq,"entry":entry})).collect::<Vec<_>>()}).to_string()),
        "read" => {
            let seq = args.seq.expect("validated");
            let Some((_, entry)) = entries.iter().find(|(n, _)| *n == seq) else { return Err("history entry not found".into()) };
            Ok(json!({"seq":seq,"entry":entry}).to_string())
        }
        "search" => {
            let query = args.query.expect("validated");
            // Deliberately preserves the legacy newest-100 search window.
            Ok(json!({"entries":ordered_entries(&entries).into_iter().take(HISTORY_MAX_LIMIT).filter(|(_, e)| searchable_content(e).is_some_and(|text| text.contains(&query))).take(args.limit).map(|(seq, entry)| json!({"seq":seq,"entry":entry})).collect::<Vec<_>>()}).to_string())
        }
        _ => unreachable!(),
    }
}

async fn execute_scoped(
    store: &SessionStore,
    root: &std::path::Path,
    current_session: &str,
    args: Args,
) -> Result<String, String> {
    let scope = args.scope.expect("scoped");
    if args.action == "read" && scope != Scope::Session {
        return Err("history read requires scope `session` or no scope".into());
    }
    let current_workspace = derive_workspace_id(root);
    let workspace_id = match scope {
        Scope::Global => None,
        Scope::Workspace | Scope::Session => {
            Some(args.workspace_id.clone().unwrap_or(current_workspace))
        }
    };
    let session_id = match scope {
        Scope::Session => Some(
            args.session_id
                .clone()
                .unwrap_or_else(|| current_session.to_owned()),
        ),
        _ => None,
    };
    if args.action == "read" {
        if store.supports_history_query() {
            let entries = store
                .query_history(&HistoryQuery {
                    workspace_id: workspace_id.clone(),
                    session_id: session_id.clone(),
                    query: None,
                    after: None,
                    after_event_time: None,
                    after_payload: None,
                    exact_seq: args.seq,
                    limit: 1,
                    default_search_window: false,
                })
                .await
                .map_err(history_load_error)?;
            let Some(entry) = entries.into_iter().next() else {
                return Err("history entry not found".into());
            };
            return Ok(json!({"workspace_id":entry.workspace_id,"session_id":entry.session_id,"seq":entry.seq,"entry":entry.entry}).to_string());
        }
        let entries = store
            .load_history_session(
                root,
                workspace_id.as_deref().expect("session workspace"),
                session_id.as_deref().expect("session id"),
            )
            .await
            .map_err(history_load_error)?;
        let seq = args.seq.expect("validated");
        let Some((_, entry)) = entries.into_iter().find(|(n, _)| *n == seq) else {
            return Err("history entry not found".into());
        };
        return Ok(
            json!({"workspace_id":workspace_id,"session_id":session_id,"seq":seq,"entry":entry})
                .to_string(),
        );
    }
    let cursor = args
        .cursor
        .as_deref()
        .map(|raw| {
            serde_json::from_str::<Cursor>(raw).map_err(|_| "invalid history cursor".to_owned())
        })
        .transpose()?;
    if let Some(cursor) = &cursor {
        if cursor.action != args.action
            || cursor.scope != scope
            || cursor.workspace_id != workspace_id
            || cursor.session_id != session_id
            || cursor.query != args.query
        {
            return Err("history cursor does not match this request".into());
        }
        if cursor.after_workspace.is_empty()
            || cursor.after_session.is_empty()
            || (store.history_query_uses_timestamp_cursor() && cursor.after_event_time_us.is_none())
        {
            return Err("invalid history cursor".into());
        }
        crate::session::validate_session_name(&cursor.after_session)
            .map_err(|_| "invalid history cursor")?;
        if workspace_id
            .as_ref()
            .is_some_and(|id| id != &cursor.after_workspace)
            || session_id
                .as_ref()
                .is_some_and(|id| id != &cursor.after_session)
        {
            return Err("history cursor is outside selected scope".into());
        }
    }
    if store.supports_history_query() {
        let entries = store
            .query_history(&HistoryQuery {
                workspace_id: workspace_id.clone(),
                session_id: session_id.clone(),
                query: args.query.clone(),
                after: cursor.as_ref().map(|c| {
                    (
                        c.after_workspace.clone(),
                        c.after_session.clone(),
                        c.after_seq,
                    )
                }),
                after_event_time: store
                    .history_query_uses_timestamp_cursor()
                    .then(|| cursor.as_ref().and_then(|c| c.after_event_time_us))
                    .flatten()
                    .map(crate::session_store::us_to_datetime),
                after_payload: store
                    .history_query_uses_timestamp_cursor()
                    .then(|| cursor.as_ref().and_then(|c| c.after_payload.clone()))
                    .flatten(),
                exact_seq: None,
                limit: args.limit + 1,
                default_search_window: false,
            })
            .await
            .map_err(history_load_error)?;
        let more = entries.len() > args.limit;
        let page = entries.into_iter().take(args.limit).collect::<Vec<_>>();
        let next_cursor = more.then(|| {
            page.last()
                .map(|e| {
                    serde_json::to_string(&Cursor {
                        action: args.action.clone(),
                        scope,
                        workspace_id,
                        session_id,
                        query: args.query.clone(),
                        after_workspace: e.workspace_id.clone(),
                        after_session: e.session_id.clone(),
                        after_seq: e.seq,
                        after_event_time_us: e.event_time.map(crate::session_store::datetime_to_us),
                        after_payload: e.cursor_payload.clone(),
                    })
                    .expect("cursor serializes")
                })
                .expect("nonempty page")
        });
        return Ok(json!({"entries":page.iter().map(scoped_entry).collect::<Vec<_>>(),"next_cursor":next_cursor}).to_string());
    }
    let mut entries = Vec::new();
    let mut after = cursor
        .as_ref()
        .map(|c| (c.after_workspace.clone(), c.after_session.clone()));
    let mut pending_cursor = cursor.as_ref();
    'scan: loop {
        let keys = if let Some(cursor) = pending_cursor.take() {
            vec![(cursor.after_workspace.clone(), cursor.after_session.clone())]
        } else {
            store
                .history_session_keys(
                    root,
                    workspace_id.as_deref(),
                    session_id.as_deref(),
                    after.as_ref().map(|(w, s)| (w.as_str(), s.as_str())),
                    32,
                )
                .await
                .map_err(history_load_error)?
        };
        if keys.is_empty() {
            break;
        }
        for (workspace, session) in keys {
            let loaded = store
                .load_history_session(root, &workspace, &session)
                .await
                .map_err(history_load_error)?;
            for (seq, entry) in ordered_entries(&loaded) {
                if cursor.as_ref().is_some_and(|c| {
                    c.after_workspace == workspace
                        && c.after_session == session
                        && seq >= c.after_seq
                }) {
                    continue;
                }
                if args.action == "search"
                    && !searchable_content(entry).is_some_and(|text| {
                        text.contains(args.query.as_deref().expect("search query"))
                    })
                {
                    continue;
                }
                entries.push(HistoryEntry {
                    workspace_id: workspace.clone(),
                    session_id: session.clone(),
                    seq,
                    event_time: None,
                    cursor_payload: None,
                    entry: entry.clone(),
                });
                if entries.len() > args.limit {
                    break 'scan;
                }
            }
            after = Some((workspace, session));
        }
    }
    let more = entries.len() > args.limit;
    let page: Vec<_> = entries.into_iter().take(args.limit).collect();
    let next_cursor = if more {
        page.last().map(|e| {
            serde_json::to_string(&Cursor {
                action: args.action.clone(),
                scope,
                workspace_id,
                session_id,
                query: args.query.clone(),
                after_workspace: e.workspace_id.clone(),
                after_session: e.session_id.clone(),
                after_seq: e.seq,
                after_event_time_us: e.event_time.map(crate::session_store::datetime_to_us),
                after_payload: e.cursor_payload.clone(),
            })
            .expect("cursor serializes")
        })
    } else {
        None
    };
    Ok(json!({"entries":page.iter().map(scoped_entry).collect::<Vec<_>>(),"next_cursor":next_cursor}).to_string())
}

fn scoped_entry(entry: &HistoryEntry) -> Value {
    json!({"workspace_id":entry.workspace_id,"session_id":entry.session_id,"seq":entry.seq,"entry":entry.entry})
}
fn history_load_error(error: anyhow::Error) -> String {
    tracing::error!("history: cannot load entries: {error:#}");
    "cannot load session history".into()
}
fn ordered_entries(entries: &[(i64, SessionEntry)]) -> Vec<(i64, &SessionEntry)> {
    let mut out = entries
        .iter()
        .map(|(seq, entry)| (*seq, entry))
        .collect::<Vec<_>>();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}
fn searchable_content(entry: &SessionEntry) -> Option<&str> {
    match entry {
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => Some(content),
        SessionEntry::Message {
            message: Message::Assistant(a),
        } => a.content.as_deref(),
        SessionEntry::Notice { text } => Some(text),
        _ => None,
    }
}

fn parse_arguments(arguments: &Value) -> Result<Args, String> {
    let object = arguments
        .as_object()
        .ok_or("history arguments must be a JSON object")?;
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or("history requires `action` (`list`, `read`, or `search`)")?
        .to_owned();
    if !matches!(action.as_str(), "list" | "read" | "search") {
        return Err(format!(
            "unknown history action `{action}` (known: list, read, search)"
        ));
    }
    let allowed: &[&str] = match action.as_str() {
        "list" => &[
            "action",
            "limit",
            "scope",
            "workspace_id",
            "session_id",
            "cursor",
        ],
        "read" => &["action", "seq", "scope", "workspace_id", "session_id"],
        _ => &[
            "action",
            "query",
            "limit",
            "scope",
            "workspace_id",
            "session_id",
            "cursor",
        ],
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("history received unknown field `{key}`"));
    }
    let scope = match object.get("scope") {
        None => None,
        Some(value) => Some(serde_json::from_value(value.clone()).map_err(|_| {
            "history `scope` must be `global`, `workspace`, or `session`".to_owned()
        })?),
    };
    if scope.is_none() && (object.contains_key("workspace_id") || object.contains_key("session_id"))
    {
        return Err("history IDs require an explicit `scope`".into());
    }
    let id = |key: &str| -> Result<Option<String>, String> {
        match object.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .map(Some)
                .ok_or_else(|| format!("history `{key}` must be a non-empty string")),
        }
    };
    let limit = match object.get("limit") {
        None => HISTORY_DEFAULT_LIMIT,
        Some(v) => usize::try_from(
            v.as_u64()
                .ok_or("history `limit` must be a non-negative integer")?,
        )
        .map_err(|_| "history `limit` must be a non-negative integer")?,
    };
    if !(1..=HISTORY_MAX_LIMIT).contains(&limit) {
        return Err(format!(
            "history `limit` must be between 1 and {HISTORY_MAX_LIMIT}"
        ));
    }
    let seq = object
        .get("seq")
        .map(|v| v.as_i64().ok_or("history `seq` must be an integer"))
        .transpose()?;
    let query = object
        .get("query")
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or("history `query` must be a non-empty string")
        })
        .transpose()?;
    if action == "read" && seq.is_none() {
        return Err("history read requires `seq` (an integer)".into());
    }
    if action == "search" && query.is_none() {
        return Err("history search requires `query` (a non-empty string)".into());
    }
    let workspace_id = id("workspace_id")?;
    let session_id = id("session_id")?;
    let cursor = id("cursor")?;
    if let Some(id) = &session_id {
        crate::session::validate_session_name(id).map_err(|_| "invalid history session_id")?;
    }
    match scope {
        None if cursor.is_some() => return Err("history cursor requires an explicit scope".into()),
        Some(Scope::Global) if workspace_id.is_some() || session_id.is_some() => {
            return Err("global history does not accept workspace_id or session_id".into());
        }
        Some(Scope::Workspace) if session_id.is_some() => {
            return Err("workspace history does not accept session_id".into());
        }
        Some(Scope::Session) if session_id.is_none() => {
            return Err("session history requires session_id".into());
        }
        _ => {}
    }
    if action == "read" && scope.is_some_and(|scope| scope != Scope::Session) {
        return Err("history read requires scope session or no scope".into());
    }
    Ok(Args {
        action,
        limit,
        seq,
        query,
        scope,
        workspace_id,
        session_id,
        cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AssistantMessage, SessionEntry};

    fn notice(text: &str) -> SessionEntry {
        SessionEntry::Notice { text: text.into() }
    }

    fn user(content: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::User {
                content: content.into(),
                images: vec![],
            },
        }
    }

    fn assistant(content: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: Some(content.into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        }
    }

    #[test]
    fn schema_preserves_actions_and_adds_explicit_selectors() {
        let parameters = History.spec().parameters;
        assert_eq!(parameters["oneOf"].as_array().unwrap().len(), 3);
        assert_eq!(
            parameters["oneOf"][0]["properties"]["action"]["const"],
            json!("list")
        );
        assert_eq!(
            parameters["oneOf"][1]["properties"]["action"]["const"],
            json!("read")
        );
        assert_eq!(
            parameters["oneOf"][2]["properties"]["action"]["const"],
            json!("search")
        );
        for variant in parameters["oneOf"].as_array().unwrap() {
            assert_eq!(variant["additionalProperties"], json!(false));
            assert!(variant["properties"].get("workspace").is_none());
            assert!(variant["properties"].get("session_id").is_some());
            assert!(variant["properties"].get("scope").is_some());
        }
        assert_eq!(
            parameters["oneOf"][0]["properties"]["limit"]["maximum"],
            json!(100)
        );
        assert_eq!(
            parameters["oneOf"][2]["properties"]["limit"]["maximum"],
            json!(100)
        );
    }

    #[test]
    fn action_specific_validation_rejects_invalid_fields() {
        assert!(parse_arguments(&json!({"action": "list", "seq": 1})).is_err());
        assert!(parse_arguments(&json!({"action": "read", "seq": 1, "limit": 2})).is_err());
        assert!(parse_arguments(&json!({"action": "search", "query": "x", "seq": 1})).is_err());
        assert!(parse_arguments(&json!({"action": "search", "query": ""})).is_err());
        assert!(parse_arguments(&json!({"action": "search"})).is_err());
        assert!(parse_arguments(&json!({"action": "search", "query": "x", "limit": 0})).is_err());
        assert!(parse_arguments(&json!({"action": "read"})).is_err());
    }

    #[tokio::test]
    async fn list_is_newest_first_and_capped_without_pagination_fields() {
        let temp = tempfile::tempdir().unwrap();
        let entries: Vec<_> = (0..25).map(|index| notice(&index.to_string())).collect();
        SessionStore::Jsonl
            .append(temp.path(), "current", &entries)
            .await
            .unwrap();
        let result: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "list", "limit": 3}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["seq"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![24, 23, 22]
        );
        assert!(result.get("next_cursor").is_none());
        assert!(result.get("has_more").is_none());
    }

    #[tokio::test]
    async fn search_is_literal_whitelisted_bounded_ordered_and_limited() {
        let temp = tempfile::tempdir().unwrap();
        let mut entries = vec![user("old needle")];
        entries.extend((1..100).map(|index| {
            if index == 99 {
                notice("older needle")
            } else if index == 98 {
                notice("another needle")
            } else {
                notice(&format!("filler {index}"))
            }
        }));
        entries.push(user("new NEEDLE literal"));
        entries.push(assistant("needle assistant"));
        let excluded_start = entries.len() as i64;
        entries.push(SessionEntry::Message {
            message: Message::Tool {
                call_id: "tool".into(),
                name: "tool".into(),
                content: "needle excluded".into(),
                images: vec![],
                is_error: false,
                synthetic: false,
            },
        });
        entries.push(SessionEntry::Compaction {
            summary: "needle excluded".into(),
            retained: vec![],
            current_prompt_at: None,
            no_current_prompt: false,
        });
        entries.push(SessionEntry::BackgroundCompletion {
            id: 1,
            output: "needle excluded".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
        });
        entries.push(SessionEntry::Error {
            text: "needle excluded".into(),
        });
        SessionStore::Jsonl
            .append(temp.path(), "current", &entries)
            .await
            .unwrap();

        let result: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "search", "query": "needle", "limit": 2}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let found = result["entries"].as_array().unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["seq"], json!(101)); // assistant, newest matching
        assert_eq!(found[1]["seq"], json!(99)); // newest-100 scan bound excludes seq 0
        assert_eq!(found[0]["entry"]["type"], json!("message"));
        assert_eq!(found[1]["entry"]["type"], json!("notice"));

        let all_matches: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "search", "query": "needle", "limit": 100}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let all_matches = all_matches["entries"].as_array().unwrap();
        let excluded_seqs = (excluded_start..excluded_start + 4).collect::<Vec<_>>();
        assert!(
            all_matches
                .iter()
                .all(|entry| { !excluded_seqs.contains(&entry["seq"].as_i64().unwrap()) })
        );
        assert_eq!(
            all_matches
                .iter()
                .map(|entry| (
                    entry["seq"].as_i64().unwrap(),
                    entry["entry"]["type"].as_str().unwrap(),
                ))
                .collect::<Vec<_>>(),
            vec![(101, "message"), (99, "notice"), (98, "notice")]
        );

        let empty: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "search", "query": "NEEDLE"}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(empty["entries"].as_array().unwrap().len(), 1);
        assert_eq!(empty["entries"][0]["seq"], json!(100));
    }

    #[tokio::test]
    async fn read_returns_complete_entry_and_not_found_and_empty_list() {
        let temp = tempfile::tempdir().unwrap();
        let complete = notice(&"x".repeat(5_000));
        SessionStore::Jsonl
            .append(temp.path(), "current", std::slice::from_ref(&complete))
            .await
            .unwrap();
        let read: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "read", "seq": 0}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(read["entry"], serde_json::to_value(complete).unwrap());
        assert_eq!(
            execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "read", "seq": 9})
            )
            .await
            .unwrap_err(),
            "history entry not found"
        );
        let empty: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "empty",
                &json!({"action": "list"}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(empty, json!({"entries": []}));
    }

    #[tokio::test]
    async fn selector_fields_are_rejected_before_store_access_for_each_session() {
        let temp = tempfile::tempdir().unwrap();
        for session in ["one", "two"] {
            let error = execute(
                &SessionStore::Jsonl,
                temp.path(),
                session,
                &json!({
                    "action": "list", "workspace": "other", "session": "other",
                }),
            )
            .await
            .unwrap_err();
            assert!(error.contains("unknown field"));
        }
    }
    #[tokio::test]
    async fn default_shape_and_search_window_are_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let entries = (0..102)
            .map(|i| notice(&format!("{} needle", i)))
            .collect::<Vec<_>>();
        SessionStore::Jsonl
            .append(temp.path(), "current", &entries)
            .await
            .unwrap();
        let result: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action":"search","query":"needle","limit":100}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(result.get("next_cursor").is_none());
        assert_eq!(result["entries"].as_array().unwrap().len(), 100);
        assert_eq!(result["entries"][99]["seq"], 2);
    }
    #[tokio::test]
    async fn scoped_workspace_paginates_and_keeps_provenance() {
        let temp = tempfile::tempdir().unwrap();
        SessionStore::Jsonl
            .append(temp.path(), "one", &[notice("needle")])
            .await
            .unwrap();
        SessionStore::Jsonl
            .append(temp.path(), "two", &[notice("needle")])
            .await
            .unwrap();
        let ws = derive_workspace_id(temp.path());
        let first: Value = serde_json::from_str(&execute(&SessionStore::Jsonl,temp.path(),"one",&json!({"action":"search","scope":"workspace","workspace_id":ws,"query":"needle","limit":1})).await.unwrap()).unwrap();
        assert_eq!(first["entries"].as_array().unwrap().len(), 1);
        assert!(first["entries"][0].get("workspace_id").is_some());
        let second: Value = serde_json::from_str(&execute(&SessionStore::Jsonl,temp.path(),"one",&json!({"action":"search","scope":"workspace","workspace_id":ws,"query":"needle","limit":1,"cursor":first["next_cursor"]})).await.unwrap()).unwrap();
        assert_ne!(
            first["entries"][0]["session_id"],
            second["entries"][0]["session_id"]
        );
    }
    #[test]
    fn ids_without_scope_and_invalid_scope_are_rejected() {
        assert!(parse_arguments(&json!({"action":"list","session_id":"x"})).is_err());
        assert!(parse_arguments(&json!({"action":"list","scope":"bad"})).is_err());
        assert!(parse_arguments(&json!({"action":"read","scope":"workspace","seq":1})).is_err());
    }
    #[tokio::test]
    async fn scoped_pages_cross_sessions_without_missing_entries_or_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::Jsonl;
        for session in ["a", "b", "c"] {
            store
                .append(
                    temp.path(),
                    session,
                    &[
                        notice("needle old"),
                        notice("not a match"),
                        notice("needle new"),
                    ],
                )
                .await
                .unwrap();
        }
        std::fs::write(
            temp.path().join(".e-agent/sessions/a.meta.jsonl"),
            "not a transcript",
        )
        .unwrap();
        let mut args = json!({"action":"search","scope":"global","query":"needle","limit":1});
        let mut found = Vec::new();
        for _ in 0..10 {
            let value: Value = serde_json::from_str(
                &execute(&store, temp.path(), "current", &args)
                    .await
                    .unwrap(),
            )
            .unwrap();
            for item in value["entries"].as_array().unwrap() {
                found.push((
                    item["session_id"].as_str().unwrap().to_owned(),
                    item["seq"].as_i64().unwrap(),
                ));
            }
            if value["next_cursor"].is_null() {
                break;
            }
            args["cursor"] = value["next_cursor"].clone();
        }
        assert_eq!(
            found,
            vec![
                ("a".into(), 2),
                ("a".into(), 0),
                ("b".into(), 2),
                ("b".into(), 0),
                ("c".into(), 2),
                ("c".into(), 0)
            ]
        );
    }

    #[tokio::test]
    async fn explicit_search_finds_old_entries_and_binds_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::Jsonl;
        let text = format!("needle {}", "x".repeat(5000));
        let mut entries = vec![notice(&text)];
        entries.extend((0..105).map(|_| notice("filler")));
        store.append(temp.path(), "a", &entries).await.unwrap();
        let args = json!({"action":"search","scope":"session","session_id":"a","query":"needle"});
        let value: Value =
            serde_json::from_str(&execute(&store, temp.path(), "other", &args).await.unwrap())
                .unwrap();
        assert_eq!(value["entries"][0]["seq"], 0);
        assert_eq!(value["entries"][0]["entry"]["text"], text);
        let mut list = json!({"action":"list","scope":"session","session_id":"a","limit":1});
        let first: Value =
            serde_json::from_str(&execute(&store, temp.path(), "other", &list).await.unwrap())
                .unwrap();
        list["cursor"] = first["next_cursor"].clone();
        list["session_id"] = json!("other");
        assert!(
            execute(&store, temp.path(), "other", &list)
                .await
                .unwrap_err()
                .contains("cursor")
        );
        assert!(
            execute(
                &store,
                temp.path(),
                "other",
                &json!({"action":"list","scope":"workspace","workspace_id":"/foreign"})
            )
            .await
            .is_err()
        );
    }
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_scopes_page_workspace_identity_and_logical_winners() {
        use crate::config::SessionBackend;
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("history.db");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let backend = SessionBackend::Sqlite {
            path: Some(db.to_str().unwrap().to_owned()),
        };
        let current = SessionStore::connect(&backend, &a, "same").await.unwrap();
        current
            .append(&a, "same", &[notice("needle a0"), notice("needle a1")])
            .await
            .unwrap();
        let foreign = SessionStore::connect(&backend, &b, "same").await.unwrap();
        foreign
            .append(&b, "same", &[notice("needle b0")])
            .await
            .unwrap();
        let third = SessionStore::connect(&backend, &b, "z").await.unwrap();
        third.append(&b, "z", &[notice("needle z0")]).await.unwrap();
        let raw_db = turso::Builder::new_local(db.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let raw = raw_db.connect().unwrap();
        let ws_a = derive_workspace_id(&a);
        let ws_b = derive_workspace_id(&b);
        // This newer version must suppress the old matching payload.
        raw.execute("INSERT INTO session_entries (workspace_id,session_id,seq,event_time_us,entry_kind,payload) VALUES (?1,'same',1,9000000000000000,'notice',?2)",
            (ws_a.as_str(),serde_json::to_string(&notice("replacement")).unwrap())).await.unwrap();
        let mut args = json!({"action":"search","scope":"global","query":"needle","limit":1});
        let mut found = Vec::new();
        for _ in 0..6 {
            let value: Value =
                serde_json::from_str(&execute(&current, &a, "same", &args).await.unwrap()).unwrap();
            for item in value["entries"].as_array().unwrap() {
                found.push((
                    item["workspace_id"].as_str().unwrap().to_owned(),
                    item["session_id"].as_str().unwrap().to_owned(),
                    item["seq"].as_i64().unwrap(),
                ));
            }
            if value["next_cursor"].is_null() {
                break;
            }
            args["cursor"] = value["next_cursor"].clone();
        }
        assert_eq!(
            found,
            vec![
                (ws_a.clone(), "same".into(), 0),
                (ws_b.clone(), "same".into(), 0),
                (ws_b.clone(), "z".into(), 0)
            ]
        );
        let second_page: Value = serde_json::from_str(&execute(&current, &a, "same", &json!({"action":"search","scope":"global","query":"needle","limit":1,"cursor":serde_json::to_string(&Cursor { action: "search".into(), scope: Scope::Global, workspace_id: None, session_id: None, query: Some("needle".into()), after_workspace: ws_a.clone(), after_session: "same".into(), after_seq: 0, after_event_time_us: None, after_payload: None }) .unwrap()})).await.unwrap()).unwrap();
        assert!(!second_page["entries"].as_array().unwrap().is_empty());
        let selected: Value = serde_json::from_str(&execute(&current,&a,"same",&json!({"action":"read","scope":"session","workspace_id":ws_b,"session_id":"same","seq":0})).await.unwrap()).unwrap();
        assert_eq!(selected["entry"]["text"], "needle b0");
        let default: Value = serde_json::from_str(
            &execute(&current, &a, "same", &json!({"action":"read","seq":0}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(default["entry"]["text"], "needle a0");
    }
}
