//! The always-on, read-only `read_output` tool: pages the exact full
//! persisted field named by a MAC-protected `eout1` receipt (the `ref`
//! embedded in bounded provider projections). Registered on every session —
//! main, read-only main, ordinary/read-only subagents, and btw forks — and
//! intercepted by the session runner (which holds the store + the receipt
//! codec), exactly like the goal tools. The fallback `execute` only fires
//! on direct `Agent::run` paths (tests) and refuses.
//!
//! Security model: possession of a valid receipt is a bearer read
//! capability for exactly one persisted field. The runner only resolves
//! receipts whose MAC verifies (constant-time, dedicated restart-stable
//! key) and whose session matches the runner's own session; the store
//! re-checks the workspace fingerprint and the pinned entry hash. There is
//! no listing, search, path traversal, or DB access beyond the pinned
//! field, and the schema is CLOSED (`additionalProperties: false`).
//!
//! Error classes are stable and model-facing: `invalid`, `unavailable`,
//! `integrity`, `utf8-boundary`, `out-of-range` (see
//! [`crate::output_receipt::ReceiptErrorKind`]). No backend credentials,
//! connection strings, or paths ever appear in the output or errors.

use async_trait::async_trait;
use serde_json::json;

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::output_receipt::{
    FieldPage, MAX_RECEIPT_LEN, ReceiptCodec, ReceiptError, ReceiptErrorKind,
};
use crate::session_store::SessionStore;

/// `read_output` page size bounds (bytes).
pub const READ_OUTPUT_DEFAULT_LIMIT: usize = 16 * 1024;
pub const READ_OUTPUT_MAX_LIMIT: usize = 32 * 1024;

/// The always-on `read_output` tool. Zero state: the runner intercepts
/// execution by name with the session's store + codec.
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
                          return (default 16384, max 32768). The response is a JSON page: \
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
                        "description": "maximum bytes to return (default 16384)"
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
/// optional `offset` (default 0) and `limit` (default 16384, max 32768).
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

/// Execute one `read_output` call against the session's store: verify the
/// receipt, resolve the exact persisted field, page it, and render the
/// closed JSON page. `session` must equal the receipt's session (a receipt
/// is a bearer capability scoped to the session that issued it). Errors are
/// plain strings with the stable `read_output error: <class>: <detail>`
/// shape.
pub async fn execute(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    codec: &ReceiptCodec,
    reference: &str,
    offset: usize,
    limit: usize,
) -> Result<String, String> {
    let page = match resolve_page(store, root, session, codec, reference, offset, limit).await {
        Ok(page) => page,
        Err(error) => return Err(format!("read_output error: {error}")),
    };
    Ok(render_page(&page))
}

/// Verify + resolve + page (pure-ish; the store I/O is the only await).
async fn resolve_page(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    codec: &ReceiptCodec,
    reference: &str,
    offset: usize,
    limit: usize,
) -> Result<FieldPage, ReceiptError> {
    let verified = codec.verify(reference)?;
    if verified.location.session != session {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "ref is for a different session than the one issuing this call",
        ));
    }
    let bytes = match store.read_field(root, &verified).await {
        Ok(bytes) => bytes,
        Err(error) => {
            // The store error may carry backend connection strings, paths,
            // or DB chains. Never expose those to the model: log the detail
            // internally only and return a fixed, pathless message.
            eprintln!("read_output: cannot read persisted field: {error:#}");
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
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

    /// Schema regression: the `ref` property carries the codec's encoded
    /// length bound (`maxLength` = [`MAX_RECEIPT_LEN`]) so an oversized
    /// receipt is rejected at the schema layer, matching
    /// `ReceiptCodec::verify`'s pre-decode bound.
    #[test]
    fn ref_schema_max_length_matches_the_codec_bound() {
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
        use crate::output_receipt::{FieldId, ReceiptCodec};
        use crate::session_store::{EntryLocation, LocatedKey, SessionStore};

        let codec_dir = tempfile::tempdir().unwrap();
        let codec = ReceiptCodec::load_from_dir(codec_dir.path()).unwrap();
        let root = tempfile::tempdir().unwrap();
        // The session's `.jsonl` path is a DIRECTORY: opening it fails with
        // a path-bearing io error inside `Session::read_field`.
        let sess_dir = root.path().join(".e-agent/sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::create_dir(sess_dir.join("sess.jsonl")).unwrap();
        let location = EntryLocation {
            backend: "jsonl",
            fingerprint: crate::session_store::workspace_root_fingerprint(root.path()),
            backend_fp: crate::session_store::backend_instance_fingerprint(
                "jsonl",
                &crate::session_store::derive_workspace_id(root.path()),
            ),
            session: "sess".into(),
            key: LocatedKey::Jsonl { ordinal: 0 },
            entry_hash: "0".repeat(64),
        };
        let receipt = codec.issue(&location, FieldId::BgOutput, 3).unwrap();
        let text = execute(
            &SessionStore::Jsonl,
            root.path(),
            "sess",
            &codec,
            &receipt,
            0,
            100,
        )
        .await
        .unwrap_err();
        assert!(
            text.starts_with("read_output error: integrity: cannot read the persisted field"),
            "{text}"
        );
        assert!(
            !text.contains("sess.jsonl") && !text.contains(root.path().to_str().unwrap()),
            "paths must never leak into the model-facing error: {text}"
        );
    }
}
