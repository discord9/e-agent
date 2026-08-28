//! The always-on `read_output` tool pages full persisted fields named by an
//! `eout1` ref. Direct numeric refs are bound to the runner's current
//! session/store; historical long refs are decoded for compatibility.

use async_trait::async_trait;
use serde_json::json;

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::output_receipt::{FieldPage, MAX_RECEIPT_LEN, OutputRef, ReceiptError, parse_ref};
use crate::session_store::SessionStore;

/// `read_output` page size bounds (bytes).
pub const READ_OUTPUT_DEFAULT_LIMIT: usize = 12 * 1024;
pub const READ_OUTPUT_MAX_LIMIT: usize = 32 * 1024;

/// The always-on `read_output` tool. Zero state: the runner intercepts
/// execution by name with the session's store.
pub struct ReadOutput;

#[async_trait]
impl Tool for ReadOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_output".into(),
            description: "Read the exact full text of one field of the session history that was \
                          truncated in a message (background completion output, tool result, \
                          notice, historical user/assistant content, or compaction summary/\
                          retained text). Pass the `ref` string from the `[truncated: ...; \
                          read_output ref=...]` marker verbatim. `offset` is the byte offset \
                          into the full field (default 0); `limit` is the maximum bytes to \
                          return (default 12288, max 32768). The response is a JSON page: \
                          {\"offset\", \"length\", \"text\", \"next_offset\", \"total_bytes\", \
                          \"sha256\"} — keep paging with `next_offset` until it is null. \
                          Never fabricate a `ref`; if a message has no `ref`, the full text is \
                          already there."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "ref": {
                        "type": "string",
                        "maxLength": MAX_RECEIPT_LEN,
                        "description": "the eout1 receipt from a [truncated: ... read_output ref=...] marker"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "byte offset into the full field (default 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 32768,
                        "description": "maximum bytes to return (default 12288)"
                    }
                },
                "required": ["ref"],
                "additionalProperties": false
            }),
        }
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolOutput, String> {
        Err("read_output is executed by the session runner".into())
    }
}

/// Parse the closed `read_output` arguments: `ref` (required string),
/// optional `offset` (default 0) and `limit` (default 12288, max 32768).
/// Unknown fields are rejected (closed schema). Returns `(ref, offset,
/// limit)` or a plain model-facing error.
pub fn parse_arguments(arguments: &serde_json::Value) -> Result<(String, usize, usize), String> {
    let object = arguments
        .as_object()
        .ok_or("read_output arguments must be a JSON object")?;
    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !matches!(key.as_str(), "ref" | "offset" | "limit"))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "read_output received unknown field(s): {}",
            unknown
                .iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let reference = object
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .ok_or("read_output requires `ref` (the eout1 receipt string from a truncated marker)")?
        .to_owned();
    let offset = match object.get("offset") {
        None => 0,
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or("read_output `offset` must be a non-negative integer")?,
    };
    let limit = match object.get("limit") {
        None => READ_OUTPUT_DEFAULT_LIMIT,
        Some(value) => {
            let limit = value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or("read_output `limit` must be a non-negative integer")?;
            if limit == 0 || limit > READ_OUTPUT_MAX_LIMIT {
                return Err(format!(
                    "read_output `limit` must be between 1 and {READ_OUTPUT_MAX_LIMIT} bytes"
                ));
            }
            limit
        }
    };
    Ok((reference, offset, limit))
}

/// Execute one `read_output` call against the session's store: resolve the
/// ref, page the exact persisted field, and render the closed JSON page.
pub async fn execute(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    reference: &str,
    offset: usize,
    limit: usize,
) -> Result<String, String> {
    let page = match resolve_page(store, root, session, reference, offset, limit).await {
        Ok(page) => page,
        Err(error) => return Err(format!("read_output error: {}", error.detail)),
    };
    Ok(render_page(&page))
}

/// Resolve + page (the store I/O is the only await).
async fn resolve_page(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    reference: &str,
    offset: usize,
    limit: usize,
) -> Result<FieldPage, ReceiptError> {
    let bytes = match parse_ref(reference)? {
        OutputRef::Direct { entry_id, field } => {
            store
                .read_field_direct(root, session, entry_id, field)
                .await
        }
        OutputRef::Legacy(verified) => {
            if verified.location.session != session {
                return Err(ReceiptError::new(
                    crate::output_receipt::ReceiptErrorKind::Invalid,
                    "ref not found",
                ));
            }
            store.read_field(root, &verified).await
        }
    };
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => {
            // The store error may carry backend connection strings, paths,
            // or DB chains. Never expose those to the model: log the detail
            // internally only and return a fixed, pathless message.
            tracing::error!("read_output: cannot read persisted field: {error:#}");
            return Err(ReceiptError::new(
                crate::output_receipt::ReceiptErrorKind::Invalid,
                "cannot read the persisted field",
            ));
        }
    };
    crate::output_receipt::page_field(&bytes, offset, limit)
}

/// Render the closed JSON page schema:
/// `{"offset", "length", "text", "next_offset", "total_bytes", "sha256"}`.
pub fn render_page(page: &FieldPage) -> String {
    json!({
        "offset": page.offset,
        "length": page.length,
        "text": page.text,
        "next_offset": page.next_offset,
        "total_bytes": page.total_bytes,
        "sha256": page.sha256,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_schema_rejects_unknown_fields() {
        let err = parse_arguments(&json!({"ref": "x", "db": "secret"})).unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
        assert!(err.contains("`db`"), "{err}");
    }

    /// Schema regression: the `ref` property carries the long-form encoded
    /// length bound (`maxLength` = [`MAX_RECEIPT_LEN`]).
    #[test]
    fn ref_schema_max_length_matches_the_receipt_bound() {
        let parameters = ReadOutput.spec().parameters;
        assert_eq!(
            parameters["properties"]["ref"]["maxLength"],
            serde_json::json!(MAX_RECEIPT_LEN)
        );
        assert_eq!(parameters["properties"]["ref"]["type"], "string");
        assert!(
            parameters["properties"]["ref"]["description"]
                .as_str()
                .unwrap()
                .contains("eout1 receipt")
        );
    }

    #[test]
    fn parse_arguments_defaults_and_bounds() {
        let (reference, offset, limit) = parse_arguments(&json!({"ref": "eout1.abc"})).unwrap();
        assert_eq!(reference, "eout1.abc");
        assert_eq!(offset, 0);
        assert_eq!(limit, READ_OUTPUT_DEFAULT_LIMIT);

        let (_, offset, limit) =
            parse_arguments(&json!({"ref": "r", "offset": 5, "limit": 100})).unwrap();
        assert_eq!(offset, 5);
        assert_eq!(limit, 100);

        assert!(
            parse_arguments(&json!({"offset": 1})).is_err(),
            "ref required"
        );
        assert!(
            parse_arguments(&json!({"ref": "r", "limit": 0})).is_err(),
            "limit >= 1"
        );
        assert!(
            parse_arguments(&json!({"ref": "r", "limit": 40000})).is_err(),
            "limit <= 32768"
        );
        assert!(
            parse_arguments(&json!({"ref": "r", "offset": -1})).is_err(),
            "offset >= 0"
        );
        assert!(
            parse_arguments(&json!({"ref": "r", "limit": "big"})).is_err(),
            "limit integer"
        );
    }

    #[test]
    fn render_page_is_closed_and_complete() {
        let page = FieldPage {
            offset: 0,
            length: 3,
            text: "abc".into(),
            next_offset: Some(3),
            total_bytes: 10,
            sha256: "f".repeat(64),
        };
        let rendered = render_page(&page);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["offset"], 0);
        assert_eq!(value["length"], 3);
        assert_eq!(value["text"], "abc");
        assert_eq!(value["next_offset"], 3);
        assert_eq!(value["total_bytes"], 10);
        assert_eq!(value["sha256"], "f".repeat(64));
        // Closed schema: exactly these six keys.
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = vec![
            "offset",
            "length",
            "text",
            "next_offset",
            "total_bytes",
            "sha256",
        ];
        expected.sort_unstable();
        let mut keys = keys;
        keys.sort_unstable();
        assert_eq!(keys, expected);
    }

    /// Store failures (which may carry backend connection strings, paths,
    /// or DB chains) are mapped to a FIXED pathless message; the detail is
    /// logged internally only and never reaches the model.
    #[tokio::test]
    async fn execute_maps_store_failures_to_pathless_errors() {
        use crate::session_store::SessionStore;

        let root = tempfile::tempdir().unwrap();
        // The session's `.jsonl` path is a DIRECTORY: opening it fails with
        // a path-bearing io error inside `Session::read_field`.
        let sess_dir = root.path().join(".e-agent/sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::create_dir(sess_dir.join("sess.jsonl")).unwrap();
        let receipt = "eout1.0.b";
        let text = execute(&SessionStore::Jsonl, root.path(), "sess", receipt, 0, 100)
            .await
            .unwrap_err();
        assert!(
            text.starts_with("read_output error: cannot read the persisted field"),
            "{text}"
        );
        assert!(
            !text.contains("sess.jsonl") && !text.contains(root.path().to_str().unwrap()),
            "paths must never leak into the model-facing error: {text}"
        );
    }
}
