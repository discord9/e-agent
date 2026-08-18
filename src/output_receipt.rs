//! Durable `eout1` output refs.
//!
//! New refs directly name a persisted entry in the current session:
//! `eout1.<entry-id>.<field-code>`. Historical self-contained long refs
//! remain accepted for transcript compatibility.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::agent::SessionEntry;
use crate::session_store::{EntryLocation, LocatedKey, workspace_id_fingerprint};

pub const RECEIPT_VERSION: &str = "eout1";
pub const MAX_RECEIPT_LEN: usize = 4096;
pub const PROJECTION_HEAD_BYTES: usize = 8 * 1024;
pub const PROJECTION_TAIL_BYTES: usize = 4 * 1024;
pub const PROJECTION_THRESHOLD: usize = PROJECTION_HEAD_BYTES + PROJECTION_TAIL_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptErrorKind {
    Invalid,
    Unavailable,
    Integrity,
    Utf8Boundary,
    OutOfRange,
}
impl ReceiptErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Integrity => "integrity",
            Self::Utf8Boundary => "utf8-boundary",
            Self::OutOfRange => "out-of-range",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptError {
    pub kind: ReceiptErrorKind,
    pub detail: String,
}
impl ReceiptError {
    pub fn new(kind: ReceiptErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
    pub fn display(&self) -> String {
        format!("{}: {}", self.kind.label(), self.detail)
    }
}
impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}
impl std::error::Error for ReceiptError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldId {
    BgOutput,
    ToolContent,
    NoticeText,
    AssistantContent,
    UserContent,
    CompactionSummary,
    CompactionRetained,
}
impl FieldId {
    /// The one-character durable direct-ref field code.
    pub fn short_code(self) -> char {
        match self {
            Self::BgOutput => 'b',
            Self::ToolContent => 't',
            Self::NoticeText => 'n',
            Self::AssistantContent => 'a',
            Self::UserContent => 'u',
            Self::CompactionSummary => 's',
            Self::CompactionRetained => 'r',
        }
    }
    pub fn from_short_code(code: &str) -> Option<Self> {
        Some(match code {
            "b" => Self::BgOutput,
            "t" => Self::ToolContent,
            "n" => Self::NoticeText,
            "a" => Self::AssistantContent,
            "u" => Self::UserContent,
            "s" => Self::CompactionSummary,
            "r" => Self::CompactionRetained,
            _ => return None,
        })
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::BgOutput => "bg_output",
            Self::ToolContent => "tool_content",
            Self::NoticeText => "notice_text",
            Self::AssistantContent => "assistant_content",
            Self::UserContent => "user_content",
            Self::CompactionSummary => "compaction_summary",
            Self::CompactionRetained => "compaction_retained",
        }
    }
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "bg_output" => Self::BgOutput,
            "tool_content" => Self::ToolContent,
            "notice_text" => Self::NoticeText,
            "assistant_content" => Self::AssistantContent,
            "user_content" => Self::UserContent,
            "compaction_summary" => Self::CompactionSummary,
            "compaction_retained" => Self::CompactionRetained,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRef {
    pub location: EntryLocation,
    pub field: FieldId,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputRef {
    Direct { entry_id: i64, field: FieldId },
    Legacy(VerifiedRef),
}

/// Format a direct durable ref from its persisted location. SQLite and
/// Greptime locations deliberately use their logical seq, not event time.
pub fn issue_direct(location: &EntryLocation, field: FieldId) -> String {
    let entry_id = match location.key {
        LocatedKey::Jsonl { ordinal } => ordinal,
        LocatedKey::Sqlite { seq, .. } | LocatedKey::Greptime { seq, .. } => seq,
    };
    format!("{RECEIPT_VERSION}.{entry_id}.{}", field.short_code())
}

#[derive(serde::Deserialize)]
struct LegacyPayload {
    v: String,
    f: String,
    b: String,
    fp: String,
    bi: String,
    s: String,
    #[serde(default)]
    o: Option<i64>,
    #[serde(default)]
    q: Option<i64>,
    #[serde(default)]
    e: Option<i64>,
    h: String,
    t: u64,
    #[serde(default)]
    #[allow(dead_code)]
    m: String,
}

/// Parse a direct durable ref or a historical self-contained long ref.
/// Historical `m` is intentionally accepted and ignored.
pub fn parse_ref(reference: &str) -> Result<OutputRef, ReceiptError> {
    let invalid = |detail: &str| {
        ReceiptError::new(
            ReceiptErrorKind::Invalid,
            format!("invalid eout1 ref: {detail}"),
        )
    };
    if reference.len() > MAX_RECEIPT_LEN {
        return Err(invalid("ref exceeds the maximum length"));
    }
    let rest = reference
        .strip_prefix("eout1.")
        .ok_or_else(|| invalid("missing the `eout1.` prefix"))?;
    let parts: Vec<_> = rest.split('.').collect();
    if parts.len() == 2 {
        let entry_id = parts[0]
            .parse::<i64>()
            .ok()
            .filter(|entry_id| *entry_id >= 0)
            .ok_or_else(|| invalid("entry id must be a non-negative integer"))?;
        let field = FieldId::from_short_code(parts[1])
            .ok_or_else(|| invalid("unknown field code (expected one of b,t,n,a,u,s,r)"))?;
        return Ok(OutputRef::Direct { entry_id, field });
    }
    if parts.len() > 2 {
        return Err(invalid(
            "unexpected extra components (expected `eout1.<entry-id>.<field-code>`)",
        ));
    }
    let encoded = rest;
    let json = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid("not a direct `eout1.<entry-id>.<field-code>` ref"))?;
    let payload: LegacyPayload =
        serde_json::from_slice(&json).map_err(|_| invalid("malformed legacy ref payload"))?;
    if payload.v != RECEIPT_VERSION || payload.t > usize::MAX as u64 {
        return Err(invalid("unsupported legacy ref version or size"));
    }
    let field = FieldId::from_code(&payload.f).ok_or_else(|| invalid("unknown legacy field"))?;
    let key = match (payload.b.as_str(), payload.o, payload.q, payload.e) {
        ("jsonl", Some(ordinal), None, None) => LocatedKey::Jsonl { ordinal },
        ("sqlite", None, Some(seq), Some(event_time_us)) => {
            LocatedKey::Sqlite { seq, event_time_us }
        }
        ("greptime", None, Some(seq), Some(event_time_us)) => {
            LocatedKey::Greptime { seq, event_time_us }
        }
        _ => return Err(invalid("malformed legacy ref location")),
    };
    Ok(OutputRef::Legacy(VerifiedRef {
        location: EntryLocation {
            backend: match key {
                LocatedKey::Jsonl { .. } => "jsonl",
                LocatedKey::Sqlite { .. } => "sqlite",
                LocatedKey::Greptime { .. } => "greptime",
            },
            fingerprint: payload.fp,
            backend_fp: payload.bi,
            session: payload.s,
            key,
            entry_hash: payload.h,
        },
        field,
        total: payload.t as usize,
    }))
}

/// Test helper: build a historical self-contained long ref for a legacy
/// location. The `m` member is emitted as a placeholder and ignored on
/// parse — there is no MAC/keying anymore.
#[cfg(test)]
pub fn issue_legacy_for_test(location: &EntryLocation, field: FieldId, total: usize) -> String {
    let (o, q, e) = match location.key {
        LocatedKey::Jsonl { ordinal } => (Some(ordinal), None, None),
        LocatedKey::Sqlite { seq, event_time_us } | LocatedKey::Greptime { seq, event_time_us } => {
            (None, Some(seq), Some(event_time_us))
        }
    };
    let payload = serde_json::json!({"v": RECEIPT_VERSION, "f": field.code(), "b": location.backend, "fp": location.fingerprint, "bi": location.backend_fp, "s": location.session, "o": o, "q": q, "e": e, "h": location.entry_hash, "t": total, "m": "ignored"});
    format!(
        "{RECEIPT_VERSION}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("serializable receipt"))
    )
}

/// Test helper: decode a historical self-contained long ref.
#[cfg(test)]
pub fn verify_legacy_for_test(reference: &str) -> Result<VerifiedRef, ReceiptError> {
    match parse_ref(reference)? {
        OutputRef::Legacy(verified) => Ok(verified),
        OutputRef::Direct { .. } => Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            "expected a legacy long ref",
        )),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::agent::image_sha256(bytes)
}

pub fn field_bytes(entry: &SessionEntry, field: FieldId) -> Option<Vec<u8>> {
    match (entry, field) {
        (SessionEntry::BackgroundCompletion { output, .. }, FieldId::BgOutput) => {
            Some(output.as_bytes().to_vec())
        }
        (
            SessionEntry::Message {
                message: crate::agent::Message::Tool { content, .. },
            },
            FieldId::ToolContent,
        ) => Some(content.as_bytes().to_vec()),
        (SessionEntry::Notice { text }, FieldId::NoticeText) => Some(text.as_bytes().to_vec()),
        (
            SessionEntry::Message {
                message: crate::agent::Message::Assistant(assistant),
            },
            FieldId::AssistantContent,
        ) => assistant
            .content
            .as_ref()
            .map(|content| content.as_bytes().to_vec()),
        (
            SessionEntry::Message {
                message: crate::agent::Message::User { content, .. },
            },
            FieldId::UserContent,
        ) => Some(content.as_bytes().to_vec()),
        (SessionEntry::Compaction { summary, .. }, FieldId::CompactionSummary) => {
            Some(summary.as_bytes().to_vec())
        }
        (SessionEntry::Compaction { retained, .. }, FieldId::CompactionRetained) => {
            serde_json::to_vec(retained).ok()
        }
        _ => None,
    }
}

/// One page of a `read_output` read: exact bytes of the requested range
/// snapped to UTF-8 char boundaries, plus the page/cursor metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldPage {
    /// The actual byte offset served (may be < the requested offset when a
    /// multibyte char straddled it — the whole char is included).
    pub offset: usize,
    /// Bytes in `text`.
    pub length: usize,
    /// UTF-8-valid page text.
    pub text: String,
    /// Offset of the next page (`None` when the end of the field is
    /// reached).
    pub next_offset: Option<usize>,
    /// Total field byte length (the receipt-bound size).
    pub total_bytes: usize,
    /// SHA-256 hex of the FULL field (integrity anchor, not just the page).
    pub sha256: String,
}

/// Page `bytes` starting at `offset` with at most `limit` bytes, snapping
/// both cuts to UTF-8 char boundaries so `text` is always valid UTF-8.
///
/// Errors:
/// - `offset > bytes.len()` → [`ReceiptErrorKind::OutOfRange`]
/// - a boundary that cannot be resolved (only possible on malformed UTF-8
///   inside a persisted String, which serde prevents) →
///   [`ReceiptErrorKind::Utf8Boundary`]
pub fn page_field(bytes: &[u8], offset: usize, limit: usize) -> Result<FieldPage, ReceiptError> {
    if offset > bytes.len() {
        return Err(ReceiptError::new(
            ReceiptErrorKind::OutOfRange,
            format!(
                "offset {offset} is past the end of the field ({} bytes)",
                bytes.len()
            ),
        ));
    }
    // The persisted field is always valid UTF-8 (JSON strings are UTF-8).
    // Snap the start back to a char boundary so a multibyte char straddling
    // the requested offset is served whole.
    let start = utf8_back_boundary(bytes, offset);
    let requested_end = offset.saturating_add(limit).min(bytes.len());
    let mut end = utf8_back_boundary(bytes, requested_end);
    // A tiny limit that falls inside a single multibyte char resolves to an
    // EMPTY page with `next_offset == requested offset` — a paging loop
    // would stall forever on it (e.g. limit=1 at offset=0 on a field that
    // opens with `α`). Guarantee progress: when the range snaps to zero
    // chars but the field still has content, extend the end forward past
    // the char at the boundary so the page carries at least one FULL char
    // (next_offset then strictly exceeds the requested offset; the
    // start-snap already permits pages slightly over their requested
    // range, this is the same whole-char rule at the end).
    if end == start && end < bytes.len() {
        let width = match bytes[end] {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            // Defensive: `end` is a char boundary, so a continuation byte
            // cannot appear here on valid UTF-8.
            _ => 1,
        };
        end = (end + width).min(bytes.len());
    }
    if end < start {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Utf8Boundary,
            "page range does not resolve to valid UTF-8 boundaries",
        ));
    }
    let text = std::str::from_utf8(&bytes[start..end]).map_err(|_| {
        ReceiptError::new(
            ReceiptErrorKind::Utf8Boundary,
            "persisted field is not valid UTF-8",
        )
    })?;
    let next_offset = (end < bytes.len()).then_some(end);
    Ok(FieldPage {
        offset: start,
        length: end - start,
        text: text.to_owned(),
        next_offset,
        total_bytes: bytes.len(),
        sha256: sha256_hex(bytes),
    })
}

/// UTF-8-safe head+tail projection with a session-local short receipt. `total` is
/// the FULL field byte length the receipt binds — for plain messages it
/// equals `text.len()`; for a message inside a compaction's retained tail
/// it is the length of the WHOLE persisted `compaction_retained` field
/// (the array), which is what `read_output` returns for that receipt.
/// Returns the field byte-identical when it fits the budget or when no
/// located key exists (leave full — never emit an unusable ref).
pub fn bound_field(text: &str, location: &EntryLocation, field: FieldId, _total: usize) -> String {
    let text_total = text.len();
    if text_total <= PROJECTION_THRESHOLD {
        return text.to_owned();
    }
    let head_len = utf8_back_boundary(text.as_bytes(), PROJECTION_HEAD_BYTES);
    let tail_start = utf8_front_boundary(text.as_bytes(), text_total - PROJECTION_TAIL_BYTES);
    if tail_start <= head_len {
        // Degenerate (e.g. a single huge multibyte char): serve the head
        // only — the tail would overlap it.
        let receipt = issue_direct(location, field);
        return format!(
            "{}\n[truncated: {} bytes omitted; read_output ref={receipt}]\n",
            &text[..head_len],
            text_total - head_len
        );
    }
    let receipt = issue_direct(location, field);
    let omitted = tail_start - head_len;
    format!(
        "{}\n[truncated: {omitted} bytes omitted; read_output ref={receipt}]\n{}",
        &text[..head_len],
        &text[tail_start..]
    )
}

/// Largest index ≤ `len` on a UTF-8 char boundary: walks back over the
/// continuation bytes of a multibyte char split by the cut and drops the
/// char's leading byte too when the char is incomplete. Walks at most 3
/// bytes, so it is O(1). Mirrors `src/tools/bash.rs::utf8_back_boundary`.
pub(crate) fn utf8_back_boundary(bytes: &[u8], len: usize) -> usize {
    let mut pos = len;
    while pos > 0 && (bytes[pos - 1] & 0xC0) == 0x80 {
        pos -= 1;
    }
    if pos == 0 {
        return 0;
    }
    let lead = bytes[pos - 1];
    let conts = len - pos;
    let needed = match lead {
        0xC0..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF7 => 3,
        _ => 0,
    };
    if conts < needed { pos - 1 } else { len }
}

/// Smallest index ≥ `offset` on a UTF-8 char boundary: skips at most 3
/// leading continuation bytes when the tail window starts mid-character.
/// Mirrors `src/tools/bash.rs::utf8_front_boundary`.
pub(crate) fn utf8_front_boundary(bytes: &[u8], offset: usize) -> usize {
    let mut pos = offset;
    let max = (offset + 3).min(bytes.len());
    while pos < max && (bytes[pos] & 0xC0) == 0x80 {
        pos += 1;
    }
    pos
}

/// Compatibility validation used by backend readers. New refs are already
/// session-local; legacy refs retain their physical location fields.
pub(crate) fn validate_location_for_store(
    location: &EntryLocation,
    backend_kind: &str,
    workspace_id: &str,
    session_id: &str,
    expected_backend_fp: &str,
) -> Result<(), ReceiptError> {
    if location.backend != backend_kind
        || location.fingerprint != workspace_id_fingerprint(workspace_id)
        || location.backend_fp != expected_backend_fp
        || location.session != session_id
    {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "ref not found",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn location(key: LocatedKey) -> EntryLocation {
        EntryLocation {
            backend: "jsonl",
            fingerprint: "fp".into(),
            backend_fp: "bi".into(),
            session: "session".into(),
            key,
            entry_hash: "hash".into(),
        }
    }
    #[test]
    fn direct_refs_use_location_identity_and_field_codes() {
        assert_eq!(
            issue_direct(
                &location(LocatedKey::Jsonl { ordinal: 12 }),
                FieldId::ToolContent
            ),
            "eout1.12.t"
        );
        assert_eq!(
            issue_direct(
                &location(LocatedKey::Sqlite {
                    seq: 8,
                    event_time_us: 9
                }),
                FieldId::BgOutput
            ),
            "eout1.8.b"
        );
        assert_eq!(
            issue_direct(
                &location(LocatedKey::Greptime {
                    seq: 37,
                    event_time_us: 9
                }),
                FieldId::CompactionSummary
            ),
            "eout1.37.s"
        );
    }
    #[test]
    fn direct_parser_and_field_codes() {
        for (field, code) in [
            (FieldId::BgOutput, "b"),
            (FieldId::ToolContent, "t"),
            (FieldId::NoticeText, "n"),
            (FieldId::AssistantContent, "a"),
            (FieldId::UserContent, "u"),
            (FieldId::CompactionSummary, "s"),
            (FieldId::CompactionRetained, "r"),
        ] {
            assert_eq!(FieldId::from_short_code(code), Some(field));
            assert_eq!(
                parse_ref(&format!("eout1.12.{code}")).unwrap(),
                OutputRef::Direct {
                    entry_id: 12,
                    field
                }
            );
        }
        for bad in [
            "eout1.-1.t",
            "eout1.x.t",
            "eout1.1.x",
            "eout1.1.t.extra",
            "eout1.tthing",
        ] {
            assert!(parse_ref(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn legacy_long_form_ignores_m() {
        let payload = serde_json::json!({"v":"eout1","f":"tool_content","b":"jsonl","fp":"fp","bi":"bi","s":"session","o":1,"h":"hash","t":42,"m":"ignored"});
        let reference = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        assert!(matches!(
            parse_ref(&reference).unwrap(),
            OutputRef::Legacy(VerifiedRef { total: 42, .. })
        ));
    }
}
