//! Versioned `eout1` receipt codec for session-backed provider projections.
//!
//! Oversized persisted machine-generated text (background-completion output,
//! tool results, notices, historical user/assistant content, compaction
//! summaries/retained tails) is projected to provider requests as a bounded
//! UTF-8-safe head+tail plus a MAC-protected receipt (`read_output` ref).
//! The receipt names exactly one persisted field at one exact physical
//! location; possession of a valid receipt is a bearer read capability for
//! that field only (see `src/tools/output.rs` and the runner's `read_output`
//! interception).
//!
//! Receipt format: `eout1.<base64url(canonical JSON)>` where the JSON
//! payload is
//!
//! ```json
//! {"v":"eout1","f":"<field code>","b":"<backend code>","fp":"<workspace fingerprint>",
//!  "bi":"<backend-instance fingerprint>","s":"<session id>","o":<jsonl ordinal>,
//!  "q":<seq>,"e":<event_time_us>,"h":"<entry sha256 hex>","t":<total field bytes>,
//!  "m":"<HMAC-SHA256 hex>"}
//! ```
//!
//! Exactly one locator shape is present: `o` (JSONL ordinal), or `q`+`e`
//! (seq + event_time µs for SQLite/Greptime). The HMAC is computed over the
//! canonical JSON with `m` empty, so any field tampering invalidates the MAC.
//! No backend connection string, path, or credential ever appears in the
//! receipt (only a workspace fingerprint derived from the canonical root, a
//! backend-instance fingerprint — an HMAC over the normalized SQLite path /
//! Greptime configuration keyed by the receipt secret, so it is not an
//! offline verifier for guessed connection strings — a backend kind code,
//! the session id, and the physical key).
//!
//! The key is a dedicated 32-byte secret at `<state dir>/e-agent/receipt.key`
//! (XDG state, falling back to `~/.config/e-agent`), created atomically with
//! mode 0600 on first use and reused across restarts. It is never the web
//! token and never leaves the machine.
//!
//! Stable error classes (see [`ReceiptErrorKind`]): `invalid`,
//! `unavailable`, `integrity`, `utf8-boundary`, `out-of-range`. Errors are
//! model-facing plain strings via the `read_output` tool; no backend
//! credentials appear in any of them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::agent::SessionEntry;
use crate::session_store::{EntryLocation, LocatedKey, workspace_id_fingerprint};

type HmacSha256 = Hmac<Sha256>;

/// The only receipt version this codec issues and accepts.
pub const RECEIPT_VERSION: &str = "eout1";

/// Upper bound on the ENCODED receipt string length (`eout1.<base64url>`).
/// A legitimate receipt is a few hundred chars (the payload is bounded by
/// [`crate::session_store::MAX_SESSION_ID_LEN`] and fixed-size
/// fingerprints/hashes); this bound is enforced BEFORE any base64/JSON
/// decoding so a hostile oversized `ref` cannot consume unbounded decode
/// work or memory.
pub const MAX_RECEIPT_LEN: usize = 4096;

/// Provider-context projection budget, shared by every eligible field:
/// when a field's UTF-8 byte length exceeds
/// `PROJECTION_HEAD_BYTES + PROJECTION_TAIL_BYTES`, the request copy keeps
/// the first [`PROJECTION_HEAD_BYTES`] bytes and the last
/// [`PROJECTION_TAIL_BYTES`] bytes (both cuts snapped to UTF-8 char
/// boundaries) joined by a marker carrying the `eout1` receipt. The
/// persisted field is never touched — only the derived request copy is
/// bounded.
pub const PROJECTION_HEAD_BYTES: usize = 8 * 1024;
pub const PROJECTION_TAIL_BYTES: usize = 4 * 1024;
/// A field at or below this size is never bounded (byte-exact projection).
pub const PROJECTION_THRESHOLD: usize = PROJECTION_HEAD_BYTES + PROJECTION_TAIL_BYTES;

/// Stable, model-facing error classes for receipt issue/verify and
/// `read_output` paging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptErrorKind {
    /// Malformed receipt string / unsupported version / bad field shape.
    Invalid,
    /// The receipt key or the backend store is not available.
    Unavailable,
    /// MAC mismatch, entry hash mismatch, backend/session/workspace
    /// mismatch, missing field, or field size drift.
    Integrity,
    /// A byte range would split a UTF-8 character.
    Utf8Boundary,
    /// The requested byte offset is past the end of the field.
    OutOfRange,
}

impl ReceiptErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ReceiptErrorKind::Invalid => "invalid",
            ReceiptErrorKind::Unavailable => "unavailable",
            ReceiptErrorKind::Integrity => "integrity",
            ReceiptErrorKind::Utf8Boundary => "utf8-boundary",
            ReceiptErrorKind::OutOfRange => "out-of-range",
        }
    }
}

/// A receipt/read error with a stable class and a plain-text detail. The
/// detail never contains backend credentials, connection strings, or paths.
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
    /// Render as `read_output error: <class>: <detail>` (the tool prefixes
    /// it with the `read_output` tool name; this is the stable substring).
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

/// The one persisted field a receipt may name. The code is the wire token
/// stored inside the receipt and bound by the MAC; the extraction reads the
/// exact persisted bytes of that field out of a [`SessionEntry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldId {
    /// `SessionEntry::BackgroundCompletion.output`.
    BgOutput,
    /// `Message::Tool.content` (all persisted builtin / MCP / delegate
    /// results).
    ToolContent,
    /// `SessionEntry::Notice.text`.
    NoticeText,
    /// Historical `Message::Assistant.content` (the current actual user and
    /// system messages are never bounded; see the projection rules).
    AssistantContent,
    /// Historical `Message::User.content`.
    UserContent,
    /// `SessionEntry::Compaction.summary`.
    CompactionSummary,
    /// `SessionEntry::Compaction.retained` (the full serialized message
    /// array).
    CompactionRetained,
}

impl FieldId {
    pub fn code(self) -> &'static str {
        match self {
            FieldId::BgOutput => "bg_output",
            FieldId::ToolContent => "tool_content",
            FieldId::NoticeText => "notice_text",
            FieldId::AssistantContent => "assistant_content",
            FieldId::UserContent => "user_content",
            FieldId::CompactionSummary => "compaction_summary",
            FieldId::CompactionRetained => "compaction_retained",
        }
    }
    pub fn from_code(code: &str) -> Option<FieldId> {
        Some(match code {
            "bg_output" => FieldId::BgOutput,
            "tool_content" => FieldId::ToolContent,
            "notice_text" => FieldId::NoticeText,
            "assistant_content" => FieldId::AssistantContent,
            "user_content" => FieldId::UserContent,
            "compaction_summary" => FieldId::CompactionSummary,
            "compaction_retained" => FieldId::CompactionRetained,
            _ => return None,
        })
    }
}

/// The canonical receipt payload (field order is the MAC canonical form).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct ReceiptPayload {
    v: String,
    f: String,
    b: String,
    fp: String,
    /// Backend-instance fingerprint (hex HMAC-SHA256 of backend kind +
    /// normalized SQLite path / Greptime configuration / JSONL root,
    /// keyed by the receipt secret — never an offline verifier).
    bi: String,
    s: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    o: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    q: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    e: Option<i64>,
    h: String,
    t: u64,
    m: String,
}

impl ReceiptPayload {
    fn from_location(location: &EntryLocation, field: FieldId, total: usize) -> ReceiptPayload {
        let (o, q, e) = match &location.key {
            LocatedKey::Jsonl { ordinal } => (Some(*ordinal), None, None),
            LocatedKey::Sqlite { seq, event_time_us } => (None, Some(*seq), Some(*event_time_us)),
            LocatedKey::Greptime { seq, event_time_us } => (None, Some(*seq), Some(*event_time_us)),
        };
        ReceiptPayload {
            v: RECEIPT_VERSION.to_owned(),
            f: field.code().to_owned(),
            b: location.backend.to_owned(),
            fp: location.fingerprint.clone(),
            bi: location.backend_fp.clone(),
            s: location.session.clone(),
            o,
            q,
            e,
            h: location.entry_hash.clone(),
            t: total as u64,
            m: String::new(),
        }
    }
}

/// A verified receipt: the exact physical location, the field, and the
/// total field byte length bound by the MAC. `read_output` passes this to
/// [`crate::session_store::SessionStore::read_field`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRef {
    pub location: EntryLocation,
    pub field: FieldId,
    pub total: usize,
}

/// HMAC-SHA256 receipt codec. Cheap to clone; holds the 32-byte key.
#[derive(Clone, Debug)]
pub struct ReceiptCodec {
    key: [u8; 32],
}

/// The XDG-state directory that holds the receipt key: `$XDG_STATE_HOME/
/// e-agent`, falling back to `~/.config/e-agent` (the same state-dir
/// fallback the image store uses in `src/agent.rs`). None when neither
/// variable is set.
pub fn receipt_key_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|x| !x.is_empty()) {
        Some(PathBuf::from(xdg).join("e-agent"))
    } else {
        crate::home_dir().map(|home| home.join(".config/e-agent"))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::agent::image_sha256(bytes)
}

fn hmac_tag(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn hmac_verify(key: &[u8; 32], data: &[u8], tag: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.verify_slice(tag).is_ok()
}

impl ReceiptCodec {
    /// A codec with an explicit key (tests and direct callers).
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// The 32-byte receipt secret, for keyed derivations that must stay
    /// tied to the receipt key (e.g. the backend-instance fingerprint in
    /// `src/session_store.rs`). `pub(crate)`: the key never leaves the
    /// crate.
    pub(crate) fn key(&self) -> &[u8; 32] {
        &self.key
    }

    /// The process-wide codec: loads (or atomically creates) the dedicated
    /// restart-stable key under the state dir. Cached for the process
    /// lifetime; any failure is also cached (deterministic per process).
    /// `Err(Unavailable)` when no state dir exists or the key cannot be
    /// created/read; `Err(Integrity)` when an existing key file has an
    /// invalid length.
    pub fn load() -> Result<Self, ReceiptError> {
        static CACHE: OnceLock<Result<ReceiptCodec, String>> = OnceLock::new();
        let cached = CACHE.get_or_init(|| {
            let Some(dir) = receipt_key_dir() else {
                return Err("no state directory (XDG_STATE_HOME / HOME unset)".to_owned());
            };
            Self::load_from_dir(&dir).map_err(|error| error.display())
        });
        match cached {
            Ok(codec) => Ok(codec.clone()),
            Err(detail) => Err(ReceiptError::new(
                ReceiptErrorKind::Unavailable,
                detail.clone(),
            )),
        }
    }

    /// Load (or atomically create) the key under `dir/receipt.key`.
    /// Public for tests and for callers that resolve their own state dir.
    ///
    /// Existing-file path is fail-closed: a symlink, a non-regular file, a
    /// file owned by another user, or a file with group/other permission
    /// bits is rejected (`Integrity`) — the key must be a plain private
    /// 0600 regular file owned by the current user. Creation is atomic
    /// (`create_new`, so a racing creator wins and is read back) and the
    /// mode is 0600 from the moment the file exists (the mode is applied
    /// at `open`, not after a write window); write/sync/chmod/read failures
    /// are surfaced, never swallowed.
    pub fn load_from_dir(dir: &Path) -> Result<Self, ReceiptError> {
        std::fs::create_dir_all(dir).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Unavailable,
                format!("cannot create receipt key directory: {error}"),
            )
        })?;
        let path = dir.join("receipt.key");
        match open_existing_private_key(&path) {
            Ok(mut file) => {
                use std::io::Read as _;
                let mut key = [0u8; 32];
                let mut read = Vec::new();
                file.read_to_end(&mut read).map_err(|error| {
                    ReceiptError::new(
                        ReceiptErrorKind::Unavailable,
                        format!("cannot read receipt key: {error}"),
                    )
                })?;
                if read.len() != 32 {
                    return Err(ReceiptError::new(
                        ReceiptErrorKind::Integrity,
                        format!(
                            "receipt key file has invalid length {} (expected 32); refusing to \
                             overwrite a private key",
                            read.len()
                        ),
                    ));
                }
                key.copy_from_slice(&read);
                Ok(Self { key })
            }
            Err(error) if error.kind == ReceiptErrorKind::Unavailable => {
                // The existing-file probe failed with NotFound (or the
                // platform cannot probe): try atomic private creation.
                create_private_key(&path)
            }
            Err(error) => Err(error),
        }
    }

    /// Issue a receipt for `location` + `field` with the given total field
    /// byte length. Deterministic: the same (location, field, total) always
    /// yields the same receipt string. Errors (serialization) surface as
    /// [`ReceiptErrorKind::Invalid`]; callers that cannot issue fall back to
    /// leaving the field full (no unusable ref is ever emitted).
    pub fn issue(
        &self,
        location: &EntryLocation,
        field: FieldId,
        total: usize,
    ) -> Result<String, ReceiptError> {
        let payload = ReceiptPayload::from_location(location, field, total);
        let canonical = serde_json::to_vec(&payload).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Invalid,
                format!("cannot serialize receipt: {error}"),
            )
        })?;
        let tag = hmac_tag(&self.key, &canonical);
        let mut final_payload = payload;
        final_payload.m = hex_encode(&tag);
        let json = serde_json::to_vec(&final_payload).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Invalid,
                format!("cannot serialize receipt: {error}"),
            )
        })?;
        Ok(format!(
            "{RECEIPT_VERSION}.{}",
            URL_SAFE_NO_PAD.encode(json)
        ))
    }
    /// Strict parse + MAC verification of a receipt string. Returns the
    /// verified location/field/total binding or a stable-class error. The
    /// MAC check is constant-time (HMAC `verify_slice`).
    pub fn verify(&self, receipt: &str) -> Result<VerifiedRef, ReceiptError> {
        if receipt.len() > MAX_RECEIPT_LEN {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Invalid,
                format!(
                    "ref exceeds the maximum encoded length ({MAX_RECEIPT_LEN} bytes); \
                     refusing to decode an oversized receipt"
                ),
            ));
        }
        let rest = receipt
            .strip_prefix(&format!("{RECEIPT_VERSION}."))
            .ok_or_else(|| {
                ReceiptError::new(
                    ReceiptErrorKind::Invalid,
                    "ref must start with the eout1 version prefix",
                )
            })?;
        let json = URL_SAFE_NO_PAD.decode(rest).map_err(|_| {
            ReceiptError::new(ReceiptErrorKind::Invalid, "ref is not valid base64url")
        })?;
        let payload: ReceiptPayload = serde_json::from_slice(&json).map_err(|_| {
            ReceiptError::new(ReceiptErrorKind::Invalid, "ref payload is not valid JSON")
        })?;
        let payload = validate_payload(payload)?;

        // Constant-time MAC verification over the canonical JSON with an
        // empty m (the same bytes `issue` MACed).
        let mut canonical = payload.clone();
        canonical.m.clear();
        let canonical = serde_json::to_vec(&canonical).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Invalid,
                format!("cannot canonicalize receipt: {error}"),
            )
        })?;
        let tag = hex_decode(&payload.m).ok_or_else(|| {
            ReceiptError::new(ReceiptErrorKind::Invalid, "receipt MAC is not hex")
        })?;
        if !hmac_verify(&self.key, &canonical, &tag) {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "receipt MAC mismatch (tampered or foreign receipt)",
            ));
        }
        let backend_code = match payload.b.as_str() {
            "jsonl" => "jsonl",
            "sqlite" => "sqlite",
            "greptime" => "greptime",
            _ => unreachable!("validated backend"),
        };
        let key = match backend_code {
            "jsonl" => LocatedKey::Jsonl {
                ordinal: payload.o.unwrap_or(0),
            },
            "sqlite" => LocatedKey::Sqlite {
                seq: payload.q.unwrap_or(0),
                event_time_us: payload.e.unwrap_or(0),
            },
            "greptime" => LocatedKey::Greptime {
                seq: payload.q.unwrap_or(0),
                event_time_us: payload.e.unwrap_or(0),
            },
            _ => unreachable!("validated backend"),
        };
        Ok(VerifiedRef {
            location: EntryLocation {
                backend: backend_code,
                fingerprint: payload.fp.clone(),
                backend_fp: payload.bi.clone(),
                session: payload.s.clone(),
                key,
                entry_hash: payload.h.clone(),
            },
            field: FieldId::from_code(&payload.f).expect("validated field code"),
            total: payload.t as usize,
        })
    }
}
/// Open an existing `receipt.key` with the fail-closed private-key
/// checks: the file must be a regular file (a symlink or special file
/// is rejected — a planted or foreign key must never be read), owned
/// by the current user, with no group/other permission bits (0600 or
/// stricter). The pre-open probe is re-verified through the opened fd
/// (type, owner, mode, and device+inode identity) so a symlink or
/// pathname swap between probe and open cannot slip a foreign file in.
/// Returns `Err(Unavailable)` ONLY for a genuinely
/// missing file (the caller then creates it atomically); every other
/// rejection is `Integrity`.
#[cfg(unix)]
fn open_existing_private_key(path: &Path) -> Result<std::fs::File, ReceiptError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReceiptError::new(ReceiptErrorKind::Unavailable, "missing"));
        }
        Err(error) => {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Unavailable,
                format!("cannot stat receipt key: {error}"),
            ));
        }
    };
    if meta.file_type().is_symlink() {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt key is a symlink; refusing to read a possibly foreign key",
        ));
    }
    if !meta.file_type().is_file() {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt key is not a regular file; refusing to read it",
        ));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt key is owned by another user; refusing to read it",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt key has group/other permissions; refusing to read it",
        ));
    }
    let file = std::fs::File::open(path).map_err(|error| {
        ReceiptError::new(
            ReceiptErrorKind::Unavailable,
            format!("cannot open receipt key: {error}"),
        )
    })?;
    // TOCTOU guard: the opened file must be the exact file the probe
    // validated. The pathname metadata (type/owner/mode/device+inode) is
    // re-verified through the opened fd so a symlink or pathname swap
    // between probe and open cannot slip a foreign file in.
    let opened = file.metadata().map_err(|error| {
        ReceiptError::new(
            ReceiptErrorKind::Unavailable,
            format!("cannot stat opened receipt key: {error}"),
        )
    })?;
    verify_opened_key_fd(&opened, &meta)?;
    Ok(file)
}

/// Re-verify the OPENED key fd against the pre-open pathname probe:
/// the opened file must still be a regular file owned by the current
/// user with no group/other permission bits, and must be the exact file
/// the probe validated (same device AND inode). An inode-only comparison
/// would miss a pathname swap onto a hard link or a same-inode re-bind;
/// a type/owner/mode check without the identity comparison would accept
/// an unrelated private file planted at the path between probe and open.
#[cfg(unix)]
fn verify_opened_key_fd(
    opened: &std::fs::Metadata,
    probe: &std::fs::Metadata,
) -> Result<(), ReceiptError> {
    use std::os::unix::fs::MetadataExt as _;
    if opened.ino() != probe.ino()
        || opened.dev() != probe.dev()
        || !opened.file_type().is_file()
        || opened.uid() != rustix::process::geteuid().as_raw()
        || opened.mode() & 0o077 != 0
    {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt key changed while opening (symlink swap); refusing to read it",
        ));
    }
    Ok(())
}

/// Non-Unix fallback: no mode/ownership enforcement (the platform has
/// no POSIX permission bits); the file is still opened read-only and a
/// missing file reports `Unavailable` so the caller creates it.
#[cfg(not(unix))]
fn open_existing_private_key(path: &Path) -> Result<std::fs::File, ReceiptError> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ReceiptError::new(ReceiptErrorKind::Unavailable, "missing"))
        }
        Err(error) => Err(ReceiptError::new(
            ReceiptErrorKind::Unavailable,
            format!("cannot open receipt key: {error}"),
        )),
    }
}

/// Atomically create a fresh 32-byte key with mode 0600. On Unix the
/// 0600 mode is applied at `open` (the file never exists with broader
/// permissions — no exposure window), then re-enforced via chmod whose
/// failure is surfaced (never ignored), and write/sync failures remove
/// the partial file and error. A racing creator wins via `create_new`
/// and its key is read back through the fail-closed open checks.
fn create_private_key(path: &Path) -> Result<ReceiptCodec, ReceiptError> {
    use rand::RngCore as _;
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(unix))]
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    match created {
        Ok(mut file) => {
            use std::io::Write as _;
            if let Err(error) = file.write_all(&key).and_then(|_| file.sync_all()) {
                let _ = std::fs::remove_file(path);
                return Err(ReceiptError::new(
                    ReceiptErrorKind::Unavailable,
                    format!("cannot write receipt key: {error}"),
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                    |error| {
                        let _ = std::fs::remove_file(path);
                        ReceiptError::new(
                            ReceiptErrorKind::Unavailable,
                            format!("cannot set receipt key permissions: {error}"),
                        )
                    },
                )?;
            }
            Ok(ReceiptCodec { key })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A racing creator won (cross-process concurrent creation
            // is safe): read its equally-random key, fail-closed.
            let mut file = open_existing_private_key(path)?;
            use std::io::Read as _;
            let mut read = Vec::new();
            file.read_to_end(&mut read).map_err(|error| {
                ReceiptError::new(
                    ReceiptErrorKind::Unavailable,
                    format!("cannot read receipt key: {error}"),
                )
            })?;
            if read.len() != 32 {
                return Err(ReceiptError::new(
                    ReceiptErrorKind::Integrity,
                    format!(
                        "receipt key file has invalid length {} (expected 32); refusing to \
                             overwrite a private key",
                        read.len()
                    ),
                ));
            }
            key.copy_from_slice(&read);
            Ok(ReceiptCodec { key })
        }
        Err(error) => Err(ReceiptError::new(
            ReceiptErrorKind::Unavailable,
            format!("cannot create receipt key: {error}"),
        )),
    }
}

/// Strict field/shape validation, shared by [`ReceiptCodec::verify`].
fn validate_payload(payload: ReceiptPayload) -> Result<ReceiptPayload, ReceiptError> {
    if payload.v != RECEIPT_VERSION {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            format!("unsupported receipt version {:?}", payload.v),
        ));
    }
    if FieldId::from_code(&payload.f).is_none() {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            format!("unknown receipt field {:?}", payload.f),
        ));
    }
    if !matches!(payload.b.as_str(), "jsonl" | "sqlite" | "greptime") {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            format!("unknown receipt backend {:?}", payload.b),
        ));
    }
    if payload.fp.len() != 32 || !payload.fp.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            "receipt workspace fingerprint is not 32 hex characters",
        ));
    }
    if payload.bi.len() != 32 || !payload.bi.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            "receipt backend-instance fingerprint is not 32 hex characters",
        ));
    }
    if payload.h.len() != 64 || !payload.h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            "receipt entry hash is not 64 hex characters",
        ));
    }
    if payload.m.len() != 64 || !payload.m.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            "receipt MAC is not 64 hex characters",
        ));
    }
    if payload.s.is_empty()
        || payload.s.len() > crate::session_store::MAX_SESSION_ID_LEN
        || !payload
            .s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Invalid,
            format!(
                "receipt session id is invalid (must be 1..={} chars of [a-zA-Z0-9_-])",
                crate::session_store::MAX_SESSION_ID_LEN
            ),
        ));
    }
    match payload.b.as_str() {
        "jsonl" => {
            if payload.o.is_none() || payload.q.is_some() || payload.e.is_some() {
                return Err(ReceiptError::new(
                    ReceiptErrorKind::Invalid,
                    "jsonl receipt must carry exactly an ordinal locator",
                ));
            }
            if payload.o.unwrap() < 0 {
                return Err(ReceiptError::new(
                    ReceiptErrorKind::Invalid,
                    "receipt ordinal must be non-negative",
                ));
            }
        }
        _ => {
            if payload.o.is_some() || payload.q.is_none() || payload.e.is_none() {
                return Err(ReceiptError::new(
                    ReceiptErrorKind::Invalid,
                    "seq backend receipt must carry exactly a seq+event_time locator",
                ));
            }
            if payload.q.unwrap() < 0 || payload.e.unwrap() < 0 {
                return Err(ReceiptError::new(
                    ReceiptErrorKind::Invalid,
                    "receipt seq/event_time must be non-negative",
                ));
            }
        }
    }
    Ok(payload)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// Extract the exact persisted bytes of `field` from a deserialized entry.
/// `None` when the entry does not carry that field (e.g. `assistant_content`
/// with no content, or the wrong field for the entry kind).
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

/// UTF-8-safe head+tail projection with a MAC-protected receipt. `total` is
/// the FULL field byte length the receipt binds — for plain messages it
/// equals `text.len()`; for a message inside a compaction's retained tail
/// it is the length of the WHOLE persisted `compaction_retained` field
/// (the array), which is what `read_output` returns for that receipt.
/// Returns the field byte-identical when it fits the budget, when no
/// located key exists, or when the codec cannot issue a receipt (leave
/// full — never emit an unusable ref).
pub fn bound_field(
    codec: &ReceiptCodec,
    text: &str,
    location: &EntryLocation,
    field: FieldId,
    total: usize,
) -> String {
    let text_total = text.len();
    if text_total <= PROJECTION_THRESHOLD {
        return text.to_owned();
    }
    let head_len = utf8_back_boundary(text.as_bytes(), PROJECTION_HEAD_BYTES);
    let tail_start = utf8_front_boundary(text.as_bytes(), text_total - PROJECTION_TAIL_BYTES);
    if tail_start <= head_len {
        // Degenerate (e.g. a single huge multibyte char): serve the head
        // only — the tail would overlap it.
        let receipt = match codec.issue(location, field, total) {
            Ok(receipt) => receipt,
            Err(_) => return text.to_owned(),
        };
        return format!(
            "{}\n[truncated: {} bytes omitted; read_output ref={receipt}]\n",
            &text[..head_len],
            text_total - head_len
        );
    }
    let receipt = match codec.issue(location, field, total) {
        Ok(receipt) => receipt,
        Err(_) => return text.to_owned(),
    };
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

/// Validate that a location is usable by a store bound to `backend_kind`,
/// `workspace_id`, `session_id`, and the backend-instance fingerprint
/// `expected_backend_fp` (the store's own precomputed
/// [`crate::session_store::backend_instance_fingerprint`]). The receipt's
/// backend-instance fingerprint must match — a receipt issued against
/// another database file or connection is rejected BEFORE any query.
/// Shared by the SQLite and Greptime backends' `read_field` (JSONL checks
/// its own way).
pub(crate) fn validate_location_for_store(
    location: &EntryLocation,
    backend_kind: &str,
    workspace_id: &str,
    session_id: &str,
    expected_backend_fp: &str,
) -> Result<(), ReceiptError> {
    if location.backend != backend_kind {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt backend does not match this store",
        ));
    }
    if location.fingerprint != workspace_id_fingerprint(workspace_id) {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt is for a different workspace",
        ));
    }
    if location.backend_fp != expected_backend_fp {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt is for a different backend instance (another database file or connection)",
        ));
    }
    if location.session != session_id {
        return Err(ReceiptError::new(
            ReceiptErrorKind::Integrity,
            "receipt is for a different session",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;
    use crate::session_store::{EntryLocation, LocatedKey};

    fn temp_codec() -> (tempfile::TempDir, ReceiptCodec) {
        let dir = tempfile::tempdir().unwrap();
        let codec = ReceiptCodec::load_from_dir(dir.path()).unwrap();
        (dir, codec)
    }

    fn location(backend: &'static str, session: &str) -> EntryLocation {
        EntryLocation {
            backend,
            fingerprint: "a".repeat(32),
            backend_fp: "c".repeat(32),
            session: session.into(),
            key: match backend {
                "jsonl" => LocatedKey::Jsonl { ordinal: 3 },
                "sqlite" => LocatedKey::Sqlite {
                    seq: 4,
                    event_time_us: 1_700_000_000_000_000,
                },
                _ => LocatedKey::Greptime {
                    seq: 4,
                    event_time_us: 1_700_000_000_000_000,
                },
            },
            entry_hash: "b".repeat(64),
        }
    }

    #[test]
    fn issue_and_verify_roundtrip_all_locator_shapes() {
        let (_dir, codec) = temp_codec();
        for backend in ["jsonl", "sqlite", "greptime"] {
            let loc = location(backend, "test-session");
            let receipt = codec.issue(&loc, FieldId::BgOutput, 12_345).unwrap();
            assert!(receipt.starts_with("eout1."), "{receipt}");
            let verified = codec.verify(&receipt).unwrap();
            assert_eq!(verified.location, loc);
            assert_eq!(verified.field, FieldId::BgOutput);
            assert_eq!(verified.total, 12_345);
            // No credentials/paths anywhere in the receipt string.
            assert!(!receipt.contains('/'), "{receipt}");
            assert!(!receipt.contains('\\'), "{receipt}");
            assert!(!receipt.contains("conn"), "{receipt}");
        }
    }

    #[test]
    fn issue_is_deterministic_for_the_same_binding() {
        let (_dir, codec) = temp_codec();
        let loc = location("sqlite", "sess-1");
        let a = codec.issue(&loc, FieldId::ToolContent, 999).unwrap();
        let b = codec.issue(&loc, FieldId::ToolContent, 999).unwrap();
        assert_eq!(a, b);
        // A different field or total yields a different receipt.
        assert_ne!(a, codec.issue(&loc, FieldId::NoticeText, 999).unwrap());
        assert_ne!(a, codec.issue(&loc, FieldId::ToolContent, 1000).unwrap());
    }

    #[test]
    fn restart_stable_key_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let a = ReceiptCodec::load_from_dir(dir.path()).unwrap();
        let b = ReceiptCodec::load_from_dir(dir.path()).unwrap();
        let loc = location("jsonl", "s");
        // Same key → same receipt across "restarts" (fresh codec objects).
        assert_eq!(
            a.issue(&loc, FieldId::NoticeText, 77).unwrap(),
            b.issue(&loc, FieldId::NoticeText, 77).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_file_created_private_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let _codec = ReceiptCodec::load_from_dir(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("receipt.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn key_file_tampered_to_wrong_length_is_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let _codec = ReceiptCodec::load_from_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("receipt.key"), b"short").unwrap();
        let err = ReceiptCodec::load_from_dir(dir.path()).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Integrity);
        assert!(
            err.detail.contains("invalid length"),
            "detail: {}",
            err.detail
        );
    }

    #[test]
    fn tampered_receipts_are_rejected() {
        let (_dir, codec) = temp_codec();
        let loc = location("sqlite", "sess-1");
        let receipt = codec.issue(&loc, FieldId::BgOutput, 12_345).unwrap();

        // Structural tampering: decode the payload, change a field value,
        // re-encode with the ORIGINAL MAC → the MAC check must fail with
        // integrity (constant-time verify).
        let decode = |receipt: &str| -> Vec<u8> {
            let rest = receipt.strip_prefix("eout1.").unwrap();
            URL_SAFE_NO_PAD.decode(rest).unwrap()
        };
        let mut payload: serde_json::Value = serde_json::from_slice(&decode(&receipt)).unwrap();
        payload["t"] = serde_json::json!(1);
        let tampered = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let err = codec.verify(&tampered).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Integrity, "{err}");
        assert!(err.detail.contains("MAC mismatch"), "{err}");

        // Raw-byte corruption may break base64url/JSON decoding → invalid.
        let mut corrupted = receipt.clone();
        let mid = corrupted.len() / 2;
        let replacement = if corrupted.as_bytes()[mid] == b'A' {
            b'B'
        } else {
            b'A'
        };
        corrupted.replace_range(mid..mid + 1, &(replacement as char).to_string());
        let err = codec.verify(&corrupted).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid, "{err}");

        // Wrong version prefix.
        let err = codec
            .verify(&format!("eout2.{}", receipt.split_once('.').unwrap().1))
            .unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);

        // Missing prefix.
        let err = codec.verify(&receipt[6..]).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);

        // Not base64url.
        let err = codec.verify("eout1.!!!").unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);

        // Garbage.
        let err = codec.verify("garbage").unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);

        // Empty.
        let err = codec.verify("").unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);
    }

    #[test]
    fn strict_payload_validation_rejects_bad_shapes() {
        let (_dir, codec) = temp_codec();
        let loc = location("sqlite", "sess-1");
        let good = codec.issue(&loc, FieldId::BgOutput, 100).unwrap();
        let decode = |receipt: &str| {
            let rest = receipt.strip_prefix("eout1.").unwrap();
            URL_SAFE_NO_PAD.decode(rest).unwrap()
        };
        let json: serde_json::Value = serde_json::from_slice(&decode(&good)).unwrap();
        let json = json;

        // Unknown field code.
        let mut bad = json.clone();
        bad["f"] = serde_json::json!("nope");
        let receipt = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(bad.to_string().as_bytes())
        );
        let err = codec.verify(&receipt).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);
        assert!(err.detail.contains("unknown receipt field"));

        // Unknown backend code.
        let mut bad = json.clone();
        bad["b"] = serde_json::json!("oracle");
        let receipt = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(bad.to_string().as_bytes())
        );
        assert_eq!(
            codec.verify(&receipt).unwrap_err().kind,
            ReceiptErrorKind::Invalid
        );

        // sqlite receipt without event_time locator.
        let mut bad = json.clone();
        bad["e"] = serde_json::Value::Null;
        let receipt = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(bad.to_string().as_bytes())
        );
        let err = codec.verify(&receipt).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);
        assert!(
            err.detail.contains("seq+event_time"),
            "detail: {}",
            err.detail
        );

        // jsonl receipt carrying a seq locator too.
        let mut bad = json.clone();
        bad["b"] = serde_json::json!("jsonl");
        bad["o"] = serde_json::json!(0);
        let receipt = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(bad.to_string().as_bytes())
        );
        assert_eq!(
            codec.verify(&receipt).unwrap_err().kind,
            ReceiptErrorKind::Invalid
        );
    }

    #[test]
    fn field_bytes_extracts_exactly() {
        use crate::agent::{AssistantMessage, Message};
        let entry = SessionEntry::BackgroundCompletion {
            id: 1,
            output: "out".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
        };
        assert_eq!(
            field_bytes(&entry, FieldId::BgOutput).unwrap(),
            b"out".to_vec()
        );
        assert_eq!(field_bytes(&entry, FieldId::ToolContent), None);
        let tool = SessionEntry::Message {
            message: Message::Tool {
                call_id: "c".into(),
                name: "bash".into(),
                content: "résultat".into(),
                images: vec![],
                is_error: false,
                synthetic: false,
            },
        };
        assert_eq!(
            field_bytes(&tool, FieldId::ToolContent).unwrap(),
            "résultat".as_bytes().to_vec()
        );
        let assistant = SessionEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![],
                reasoning: None,
            }),
        };
        assert_eq!(field_bytes(&assistant, FieldId::AssistantContent), None);
        let comp = SessionEntry::Compaction {
            summary: "sum".into(),
            retained: vec![Message::User {
                content: "hi".into(),
                images: vec![],
            }],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        assert_eq!(
            field_bytes(&comp, FieldId::CompactionSummary).unwrap(),
            b"sum".to_vec()
        );
        let retained = field_bytes(&comp, FieldId::CompactionRetained).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<Message>>(&retained).unwrap(),
            comp_retained_messages()
        );
    }

    fn comp_retained_messages() -> Vec<Message> {
        vec![Message::User {
            content: "hi".into(),
            images: vec![],
        }]
    }

    #[test]
    fn bound_field_fits_and_oversized_with_receipt() {
        let (_dir, codec) = temp_codec();
        let loc = location("jsonl", "s");
        // Fits the budget: byte-identical.
        assert_eq!(
            bound_field(&codec, "small", &loc, FieldId::NoticeText, 5),
            "small"
        );
        let at_budget = "a".repeat(PROJECTION_THRESHOLD);
        assert_eq!(
            bound_field(
                &codec,
                &at_budget,
                &loc,
                FieldId::NoticeText,
                at_budget.len()
            ),
            at_budget
        );
        // Oversized: head + marker + receipt + tail.
        let big = format!(
            "{}{}{}",
            "h".repeat(PROJECTION_HEAD_BYTES),
            "MIDDLE",
            "t".repeat(PROJECTION_TAIL_BYTES)
        );
        let bounded = bound_field(&codec, &big, &loc, FieldId::NoticeText, big.len());
        assert!(bounded.starts_with(&"h".repeat(PROJECTION_HEAD_BYTES)));
        assert!(bounded.contains("[truncated: 6 bytes omitted"));
        assert!(bounded.contains("read_output ref=eout1."));
        assert!(bounded.ends_with(&"t".repeat(PROJECTION_TAIL_BYTES)));
        assert!(!bounded.contains("MIDDLE"));
        // The embedded receipt verifies against the same binding.
        let ref_start = bounded.find("ref=eout1.").unwrap() + "ref=".len();
        let ref_end = bounded[ref_start..].find(']').unwrap() + ref_start;
        let receipt = &bounded[ref_start..ref_end];
        let verified = codec.verify(receipt).unwrap();
        assert_eq!(verified.location, loc);
        assert_eq!(verified.field, FieldId::NoticeText);
        assert_eq!(verified.total, big.len());
    }

    #[test]
    fn bound_field_multibyte_seams_never_split() {
        let (_dir, codec) = temp_codec();
        let loc = location("jsonl", "s");
        let head = "α".repeat(PROJECTION_HEAD_BYTES / 2);
        let rest = "€".repeat(3_000);
        let big = format!("{head}€{}€{rest}zz", "M".repeat(6));
        assert!(big.len() > PROJECTION_THRESHOLD);
        let bounded = bound_field(&codec, &big, &loc, FieldId::ToolContent, big.len());
        assert!(
            std::str::from_utf8(bounded.as_bytes()).is_ok(),
            "valid UTF-8"
        );
        // The 6-char 'M' filler can never appear in the head/tail/marker
        // (the base64url receipt alphabet allows single letters, so assert
        // on the full filler run, not a single char).
        assert!(!bounded.contains("MMMMMM"), "omitted middle leaked");
    }

    #[test]
    fn page_field_snaps_boundaries_and_reports_metadata() {
        // 5 chars: 'a' 'α' (2 bytes) 'b' — byte layout: a(0) α(1,2) b(3).
        let bytes = "aαb".as_bytes();
        assert_eq!(bytes.len(), 4);
        // Whole field.
        let page = page_field(bytes, 0, 100).unwrap();
        assert_eq!(page.offset, 0);
        assert_eq!(page.length, 4);
        assert_eq!(page.text, "aαb");
        assert_eq!(page.next_offset, None);
        assert_eq!(page.total_bytes, 4);
        assert_eq!(page.sha256.len(), 64);

        // Offset 2 is mid-α: snap back to 1 (whole α served).
        let page = page_field(bytes, 2, 1).unwrap();
        assert_eq!(page.offset, 1);
        assert_eq!(page.text, "α");
        assert_eq!(page.next_offset, Some(3));

        // Offset 3 limit 100 → "b", end of field.
        let page = page_field(bytes, 3, 100).unwrap();
        assert_eq!(page.offset, 3);
        assert_eq!(page.text, "b");
        assert_eq!(page.next_offset, None);

        // Offset at end → empty page, no error.
        let page = page_field(bytes, 4, 100).unwrap();
        assert_eq!(page.length, 0);
        assert_eq!(page.next_offset, None);

        // Past the end → out-of-range.
        let err = page_field(bytes, 5, 100).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::OutOfRange);

        // Empty field.
        let page = page_field(b"", 0, 100).unwrap();
        assert_eq!(page.length, 0);
        assert_eq!(page.total_bytes, 0);
        assert_eq!(page.next_offset, None);

        // Paging chain covers exactly once: 0,2 → snaps to 1 (α), next 3.
        let p1 = page_field(bytes, 0, 2).unwrap();
        assert_eq!(p1.offset, 0);
        assert_eq!(p1.text, "a");
        assert_eq!(p1.next_offset, Some(1));
        let p2 = page_field(bytes, p1.next_offset.unwrap(), 2).unwrap();
        assert_eq!(p2.text, "α");
        assert_eq!(p2.next_offset, Some(3));
        let p3 = page_field(bytes, p2.next_offset.unwrap(), 2).unwrap();
        assert_eq!(p3.text, "b");
        assert_eq!(p3.next_offset, None);
    }

    #[test]
    fn page_field_limit_within_multibyte_char_advances() {
        // Regression: limit=1 with a field that OPENS on a multibyte char
        // used to resolve to an EMPTY page with next_offset == requested
        // offset (0), stalling a paging loop forever. The page must carry
        // at least one full char so next_offset strictly advances.
        // 'α' is 2 bytes, '中' is 3 bytes, '😀' is 4 bytes.
        let bytes = "α中😀".as_bytes();
        assert_eq!(bytes.len(), 9);

        // 2-byte char at offset 0 with limit 1 → the whole char, next = 2.
        let p1 = page_field(bytes, 0, 1).unwrap();
        assert_eq!(p1.offset, 0);
        assert_eq!(p1.text, "α");
        assert_eq!(p1.next_offset, Some(2));
        assert!(p1.next_offset.unwrap() > 0);

        // 3-byte char at offset 2 with limit 1 → whole char, next = 5.
        let p2 = page_field(bytes, p1.next_offset.unwrap(), 1).unwrap();
        assert_eq!(p2.offset, 2);
        assert_eq!(p2.text, "中");
        assert_eq!(p2.next_offset, Some(5));
        assert!(p2.next_offset.unwrap() > 2);

        // 4-byte char at offset 5 with limit 1 → whole char; it ends the
        // field, so there is no next page.
        let p3 = page_field(bytes, p2.next_offset.unwrap(), 1).unwrap();
        assert_eq!(p3.offset, 5);
        assert_eq!(p3.text, "😀");
        assert_eq!(p3.next_offset, None);
        assert!(
            p3.length > 0,
            "the char is served whole, never an empty page"
        );

        // A mid-char offset with a limit too small to reach the char's end
        // also advances (never returns the same offset twice).
        let mid = page_field(bytes, 3, 1).unwrap(); // inside '中' (bytes 2..5)
        assert_eq!(mid.offset, 2);
        assert_eq!(mid.text, "中");
        assert_eq!(mid.next_offset, Some(5));
        assert!(mid.next_offset.unwrap() > 3);

        // limit=1 paging covers the field exactly once.
        let mut offset = 0;
        let mut collected = String::new();
        loop {
            let page = page_field(bytes, offset, 1).unwrap();
            collected.push_str(&page.text);
            match page.next_offset {
                Some(next) => {
                    assert!(next > offset, "cursor must advance: {offset} -> {next}");
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(collected, "α中😀");
    }

    #[test]
    fn utf8_boundaries_never_exceed_the_buffer() {
        let bytes = "a€b".as_bytes(); // a(0) €(1,2,3) b(4)
        assert_eq!(utf8_back_boundary(bytes, 0), 0);
        assert_eq!(utf8_back_boundary(bytes, 3), 1);
        assert_eq!(utf8_back_boundary(bytes, 4), 4);
        assert_eq!(utf8_front_boundary(bytes, 1), 1);
        assert_eq!(utf8_front_boundary(bytes, 2), 4);
        assert_eq!(utf8_front_boundary(bytes, 3), 4);
        assert_eq!(utf8_front_boundary(bytes, 5), 5);
        // All-ASCII.
        assert_eq!(utf8_back_boundary(b"abc", 2), 2);
        assert_eq!(utf8_front_boundary(b"abc", 2), 2);
    }

    /// The fingerprint is a hash of the workspace id, never the raw path.
    #[test]
    fn workspace_fingerprint_is_hashed_not_raw() {
        let fp = workspace_id_fingerprint("/home/user/ws");
        assert_eq!(fp.len(), 32);
        assert!(!fp.contains("home"), "raw path must never leak: {fp}");
        assert_ne!(
            workspace_id_fingerprint("/a"),
            workspace_id_fingerprint("/b")
        );
    }

    /// Backend-instance fingerprint: a KEYED hash of kind + instance
    /// identity, never the raw config; different instances (paths /
    /// connection strings) fingerprint differently, and the same instance
    /// under two different receipt keys fingerprints differently — the
    /// published `bi` cannot be verified offline against guessed
    /// low-entropy connection strings.
    #[test]
    fn backend_instance_fingerprint_is_hashed_and_distinct() {
        use crate::session_store::{
            backend_instance_fingerprint, keyed_backend_instance_fingerprint,
        };
        let sqlite_a = backend_instance_fingerprint("sqlite", "/data/sessions.db");
        assert_eq!(sqlite_a.len(), 32);
        assert!(!sqlite_a.contains("sessions"), "config must never leak");
        assert_ne!(
            sqlite_a,
            backend_instance_fingerprint("sqlite", "/other/db.sqlite")
        );
        // The kind is part of the identity: same instance string under a
        // different backend kind fingerprints differently.
        assert_ne!(
            sqlite_a,
            backend_instance_fingerprint("greptime", "/data/sessions.db")
        );
        // Deterministic.
        assert_eq!(
            sqlite_a,
            backend_instance_fingerprint("sqlite", "/data/sessions.db")
        );
        // KEYED: the same instance under two different receipt secrets
        // yields two different digests (HMAC, not plain SHA-256).
        let key_a = [1u8; 32];
        let key_b = [2u8; 32];
        assert_ne!(
            keyed_backend_instance_fingerprint(&key_a, "sqlite", "/data/sessions.db"),
            keyed_backend_instance_fingerprint(&key_b, "sqlite", "/data/sessions.db"),
        );
        // The keyed core is still a 32-hex, instance-sensitive digest.
        let digest = keyed_backend_instance_fingerprint(&key_a, "sqlite", "/data/sessions.db");
        assert_eq!(digest.len(), 32);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(
            digest,
            keyed_backend_instance_fingerprint(&key_a, "sqlite", "/other/db.sqlite")
        );
    }

    /// A receipt bound to one backend instance is rejected by a store
    /// bound to a different instance — before any query.
    #[test]
    fn validate_location_rejects_foreign_backend_instance() {
        use crate::session_store::backend_instance_fingerprint;
        let (_dir, codec) = temp_codec();
        let mut loc = location("sqlite", "sess-1");
        loc.fingerprint = crate::session_store::workspace_id_fingerprint("ws-1");
        loc.backend_fp = backend_instance_fingerprint("sqlite", "/db/one.db");
        let receipt = codec.issue(&loc, FieldId::BgOutput, 10).unwrap();
        let verified = codec.verify(&receipt).unwrap();
        // Same instance passes.
        assert!(
            validate_location_for_store(
                &verified.location,
                "sqlite",
                "ws-1",
                "sess-1",
                &backend_instance_fingerprint("sqlite", "/db/one.db"),
            )
            .is_ok()
        );
        // A different database file is rejected with integrity.
        let err = validate_location_for_store(
            &verified.location,
            "sqlite",
            "ws-1",
            "sess-1",
            &backend_instance_fingerprint("sqlite", "/db/two.db"),
        )
        .unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Integrity, "{err}");
        assert!(err.detail.contains("backend instance"), "{err}");
    }

    /// The encoded receipt length is bounded BEFORE decoding, so a hostile
    /// oversized ref fails `invalid` without any base64/JSON work.
    #[test]
    fn oversized_receipt_is_rejected_before_decode() {
        let (_dir, codec) = temp_codec();
        let huge = format!("eout1.{}", "A".repeat(MAX_RECEIPT_LEN + 1));
        let err = codec.verify(&huge).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);
        assert!(err.detail.contains("maximum encoded length"), "{err}");
    }

    /// A receipt carrying an over-long session id is rejected as invalid.
    #[test]
    fn overlong_session_id_in_receipt_is_rejected() {
        let (_dir, codec) = temp_codec();
        let loc = location("jsonl", "s");
        let good = codec.issue(&loc, FieldId::BgOutput, 10).unwrap();
        let decode = |receipt: &str| {
            let rest = receipt.strip_prefix("eout1.").unwrap();
            URL_SAFE_NO_PAD.decode(rest).unwrap()
        };
        let mut payload: serde_json::Value = serde_json::from_slice(&decode(&good)).unwrap();
        payload["s"] = serde_json::json!("x".repeat(crate::session_store::MAX_SESSION_ID_LEN + 1));
        let receipt = format!(
            "eout1.{}",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let err = codec.verify(&receipt).unwrap_err();
        assert_eq!(err.kind, ReceiptErrorKind::Invalid);
        assert!(err.detail.contains("session id"), "detail: {}", err.detail);
    }

    #[cfg(unix)]
    mod unix_key_checks {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn write_key(dir: &Path, contents: &[u8]) {
            std::fs::write(dir.join("receipt.key"), contents).unwrap();
        }

        /// A symlinked key is rejected (integrity), never followed.
        #[test]
        fn symlinked_key_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            write_key(target.path(), &[7u8; 32]);
            symlink(
                target.path().join("receipt.key"),
                dir.path().join("receipt.key"),
            )
            .unwrap();
            let err = ReceiptCodec::load_from_dir(dir.path()).unwrap_err();
            assert_eq!(err.kind, ReceiptErrorKind::Integrity, "{err}");
            assert!(err.detail.contains("symlink"), "{err}");
        }

        /// A non-regular key (a directory) is rejected, not opened.
        #[test]
        fn non_regular_key_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("receipt.key")).unwrap();
            let err = ReceiptCodec::load_from_dir(dir.path()).unwrap_err();
            assert_eq!(err.kind, ReceiptErrorKind::Integrity, "{err}");
            assert!(err.detail.contains("not a regular file"), "{err}");
        }

        /// A key with group/other permission bits (0644, 0604, …) is
        /// rejected: the key must stay private.
        #[test]
        fn world_readable_key_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            write_key(dir.path(), &[7u8; 32]);
            std::fs::set_permissions(
                dir.path().join("receipt.key"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            let err = ReceiptCodec::load_from_dir(dir.path()).unwrap_err();
            assert_eq!(err.kind, ReceiptErrorKind::Integrity, "{err}");
            assert!(err.detail.contains("group/other permissions"), "{err}");
            // A stricter mode (0400) is fine.
            std::fs::set_permissions(
                dir.path().join("receipt.key"),
                std::fs::Permissions::from_mode(0o400),
            )
            .unwrap();
            assert!(ReceiptCodec::load_from_dir(dir.path()).is_ok());
        }

        /// A key owned by another user is rejected (skip when running as
        /// root, which can read anything and would make the uid mismatch
        /// untestable).
        #[test]
        fn foreign_owned_key_is_rejected() {
            if rustix::process::geteuid().as_raw() == 0 {
                eprintln!("skipping: running as root");
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            write_key(dir.path(), &[7u8; 32]);
            std::fs::set_permissions(
                dir.path().join("receipt.key"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            // chown to uid 0 (root): the key is then owned by another user.
            let path = dir.path().join("receipt.key");
            // Use the `chown` command (no libc dependency).
            let status = std::process::Command::new("chown")
                .arg("0:0")
                .arg(&path)
                .status();
            if status.map(|s| !s.success()).unwrap_or(true) {
                eprintln!("skipping: cannot chown");
                return;
            }
            let err = ReceiptCodec::load_from_dir(dir.path()).unwrap_err();
            assert_eq!(err.kind, ReceiptErrorKind::Integrity, "{err}");
            assert!(err.detail.contains("owned by another user"), "{err}");
        }

        /// The created key is 0600 from the very first moment it exists:
        /// creation applies the mode at open (no exposure window) and the
        /// read-back load accepts it.
        #[test]
        fn created_key_is_0600_without_exposure_window() {
            let dir = tempfile::tempdir().unwrap();
            let _codec = ReceiptCodec::load_from_dir(dir.path()).unwrap();
            let path = dir.path().join("receipt.key");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            // Reload succeeds through the fail-closed existing-file checks.
            assert!(ReceiptCodec::load_from_dir(dir.path()).is_ok());
        }

        /// Cross-process concurrent creation is safe: racing creators
        /// (threads each doing a full load) all end up with the SAME
        /// working key file — one wins the atomic create_new, the others
        /// read it back.
        #[test]
        fn concurrent_creators_agree_on_one_key() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_path_buf();
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    std::thread::spawn(move || ReceiptCodec::load_from_dir(&path))
                })
                .collect();
            let codecs: Vec<ReceiptCodec> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap().unwrap())
                .collect();
            let loc = location("jsonl", "s");
            let receipts: Vec<String> = codecs
                .iter()
                .map(|codec| codec.issue(&loc, FieldId::BgOutput, 5).unwrap())
                .collect();
            for receipt in &receipts {
                assert_eq!(receipt, &receipts[0], "all creators share one key");
            }
            let mode = std::fs::metadata(path.join("receipt.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        /// The opened-fd re-check validates type, owner, mode AND
        /// device+inode identity against the pre-open probe: a pathname
        /// swap onto a different file (even a same-size private regular
        /// file) between probe and open is rejected, as are a loosened
        /// mode, a non-regular file, and a changed owner.
        #[test]
        fn opened_fd_recheck_rejects_swaps_and_foreign_files() {
            let dir = tempfile::tempdir().unwrap();
            let key = dir.path().join("receipt.key");
            std::fs::write(&key, [7u8; 32]).unwrap();
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
            let probe = std::fs::symlink_metadata(&key).unwrap();
            // The same file passes the re-check.
            assert!(verify_opened_key_fd(&std::fs::metadata(&key).unwrap(), &probe).is_ok());
            // A DIFFERENT file (different device+inode) is rejected: the
            // pathname swapped between probe and open.
            let other = dir.path().join("other.key");
            std::fs::write(&other, [7u8; 32]).unwrap();
            std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(verify_opened_key_fd(&std::fs::metadata(&other).unwrap(), &probe).is_err());
            // The same file with group/other permission bits added after
            // the probe is rejected (mode re-check on the opened fd).
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(verify_opened_key_fd(&std::fs::metadata(&key).unwrap(), &probe).is_err());
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
            // A non-regular file (directory) at the path is rejected.
            let dirpath = dir.path().join("adir");
            std::fs::create_dir(&dirpath).unwrap();
            assert!(verify_opened_key_fd(&std::fs::metadata(&dirpath).unwrap(), &probe).is_err());
            // Ownership changed after the probe (chown to root) is
            // rejected (owner re-check on the opened fd). Skipped when
            // running as root (the uid cannot be made to differ).
            if rustix::process::geteuid().as_raw() != 0 {
                let status = std::process::Command::new("chown")
                    .arg("0:0")
                    .arg(&key)
                    .status();
                if status.map(|s| s.success()).unwrap_or(false) {
                    assert!(
                        verify_opened_key_fd(&std::fs::metadata(&key).unwrap(), &probe).is_err()
                    );
                } else {
                    eprintln!("skipping: cannot chown");
                }
            }
        }
    }
}
