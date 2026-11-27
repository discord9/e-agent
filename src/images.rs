//! The outgoing image budget shared by both provider wires.
//!
//! Two behaviours, both applied to the REQUEST COPY only (history, events,
//! the session store and compaction never see either):
//!
//! 1. **Normalisation.** Every image about to be put on the wire — EXCEPT
//!    the images appended after the newest assistant message (the ones the
//!    offload rule below never considers delivered) — is measured with
//!    DeepSeek's published vision-token calculator (14px patches merged 3:1
//!    per axis into one token cell, `tokens = grid_h * (grid_w + 1) + 2`, see
//!    [`estimated_tokens`]) and scaled DOWN — never up — until the estimate
//!    fits [`MAX_IMAGE_TOKENS`]. The re-encoded variant is cached next to the
//!    original as `<hash>.n<WxH>.<ext>` plus a `.sha256` sidecar; the stored
//!    original is never mutated. A cache entry is used only when the source
//!    still hashes to `hash` AND the variant matches its sidecar, so a stale
//!    or torn entry is re-encoded instead of trusted. An image that already
//!    fits keeps its exact stored bytes and MIME type (no decode of pixels,
//!    no re-encode). The newest images skip this step entirely: their stored
//!    bytes and MIME go out unchanged, bounded only by the 10 MiB ingest cap
//!    (the one case where a larger file is rejected with a clear error, never
//!    silently resized). Stored files are read through that cap and decoded
//!    buffers beyond `MAX_DECODE_BYTES` are rejected with a clear error
//!    rather than risking an unbounded allocation.
//!
//! 2. **Offload.** When a request would carry more than `max_inline` DISTINCT
//!    images that were ALREADY delivered, the oldest delivered ones (by first
//!    appearance in request order) are replaced by a text placeholder naming
//!    their stored hash, so the model knows the image exists and how to get it
//!    back. Images appended after the newest assistant message have never been
//!    on the wire, so they are always kept: a fresh attachment gets at least
//!    one send, and it goes out at full size. [`ImageBudget::project`] is
//!    pure: the same messages always
//!    produce the same placeholders, in the same order, and therefore the same
//!    request bytes.
//!
//! Out of scope by design: retention/expiry by wall-clock time, a "recent
//! turns" heuristic, automatic re-reads on offload, summarisation, and any
//! second storage format. Offloading deliberately trades prompt-cache reuse
//! from the first changed message onward for a bounded image payload.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::agent::{ImagePart, Message};

/// Edge of one vision patch, in pixels (the provider's encoder input).
const PATCH: u32 = 14;
/// Patches merged per axis into one vision-token cell.
const DOWNSAMPLE: u32 = 3;
/// Edge of one vision-token cell, in pixels (`PATCH * DOWNSAMPLE`).
const CELL: u32 = PATCH * DOWNSAMPLE;

/// Hard cap on the estimated vision tokens of one NORMALISED outgoing image.
/// The newest images skip normalisation, so their only limit is the ingest cap.
pub const MAX_IMAGE_TOKENS: u32 = 1024;

/// Largest stored file the wire will read. This is the documented attachment
/// cap (`agent::IMAGE_MAX_BYTES`); checking it BEFORE the read keeps a
/// corrupted or tampered store file from becoming an arbitrary-sized buffer.
const MAX_IMAGE_BYTES: usize = crate::agent::IMAGE_MAX_BYTES;

/// Largest decoded pixel buffer `normalise` will materialise. The encoded
/// cap above bounds the file bytes, not the decoded pixels (a few-hundred-KB
/// PNG can claim billions of pixels), so an oversized decode is rejected with
/// a clear error instead of being attempted.
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// The provider's total-pixel floor (544x544). It participates in the
/// published calculator's accounting only: this module never upscales.
const MIN_PIXELS: f64 = 295_936.0;

/// Default `[images] max_inline`: distinct images kept inline per request.
pub const DEFAULT_MAX_INLINE: usize = 6;

/// JPEG quality for a re-encoded opaque image.
const JPEG_QUALITY: u8 = 85;

/// Vision-token cells the provider patches `width` x `height` into: the
/// image is padded to whole 14px patches and every 3 patches per axis merge
/// into one cell, i.e. `ceil(edge / 42)` cells per axis.
fn token_grid(width: u32, height: u32) -> (u32, u32) {
    let cells = |edge: u32| edge.max(1).div_ceil(CELL);
    (cells(height), cells(width))
}

/// `tokens = grid_h * (grid_w + 1) + 2`, saturating at `u32::MAX`. Extreme
/// dimensions must never wrap: a wrapped count could look under the cap and
/// authorize sending a far too large image.
fn grid_tokens(width: u32, height: u32) -> u32 {
    let (rows, cols) = token_grid(width, height);
    let tokens = u64::from(rows)
        .saturating_mul(u64::from(cols).saturating_add(1))
        .saturating_add(2);
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

/// Bytes a decoder would materialise for `width` x `height`, or `None` on
/// overflow. Never wraps.
fn decoded_bytes(width: u32, height: u32, color: image::ColorType) -> Option<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(u64::from(color.bytes_per_pixel()))
}

/// Estimated vision tokens the provider bills for an image of `width` x
/// `height`, reproducing the published calculator's `v41` preset (14px
/// patch, 3:1 downsample per axis, 1024-token cap, 544x544 pixel floor).
///
/// Images whose total pixel count is under the floor are counted as if the
/// provider had upscaled them to it — that is the provider's own accounting,
/// not ours: [`fit_to_budget`] never upscales. Images over the cap are
/// counted at the dimensions the calculator would shrink them to, so the
/// result never exceeds [`MAX_IMAGE_TOKENS`]. Examples (from the published
/// calculator): 1300x1300 and 5000x5000 both cost 994 tokens, 544x544 and
/// 100x100 both cost 184, 1920x1080 costs 968.
pub fn estimated_tokens(width: u32, height: u32) -> u32 {
    let area = f64::from(width) * f64::from(height);
    let (width, height) = if area > 0.0 && area < MIN_PIXELS {
        let scale = (MIN_PIXELS / area).sqrt();
        let scaled = |edge: u32| (f64::from(edge) * scale) as u32;
        (scaled(width), scaled(height))
    } else {
        (width, height)
    };
    // The calculator reports the size AFTER its own cap resize, so an
    // over-budget image is counted at the dimensions it would shrink to.
    let (width, height) = fit_to_budget(width, height).unwrap_or((width, height));
    grid_tokens(width, height)
}

/// Dimensions an image must be scaled DOWN to so its estimate fits
/// [`MAX_IMAGE_TOKENS`], or `None` when it already fits (the caller then
/// reuses the stored bytes unchanged). Never returns a larger dimension: the
/// provider's 544x544 floor is not applied here.
pub fn fit_to_budget(width: u32, height: u32) -> Option<(u32, u32)> {
    if grid_tokens(width, height) <= MAX_IMAGE_TOKENS {
        return None;
    }
    // The published calculator shrinks to a grid with the same aspect ratio,
    // then feeds its own result back in until it converges (10 rounds).
    let mut dims = (width.max(1), height.max(1));
    for _ in 0..10 {
        if grid_tokens(dims.0, dims.1) <= MAX_IMAGE_TOKENS {
            break;
        }
        dims = solve_resize(dims.0, dims.1);
    }
    // Never upscale, never return an empty image.
    let mut dims = (
        dims.0.clamp(1, width.max(1)),
        dims.1.clamp(1, height.max(1)),
    );
    // The published solve is exact for its own presets; keep a deterministic
    // safety net for the float boundary so the cap always holds.
    for _ in 0..256 {
        if grid_tokens(dims.0, dims.1) <= MAX_IMAGE_TOKENS {
            break;
        }
        dims = (
            ((f64::from(dims.0) * 0.95) as u32).max(1),
            ((f64::from(dims.1) * 0.95) as u32).max(1),
        );
    }
    Some(dims)
}

/// Port of the published calculator's `solveResizeRatio` (`v41` preset): a
/// target of whole 14px patches, at the same aspect ratio, whose token grid
/// fits [`MAX_IMAGE_TOKENS`].
fn solve_resize(width: u32, height: u32) -> (u32, u32) {
    let (width, height) = (f64::from(width.max(1)), f64::from(height.max(1)));
    // The calculator solves in units of columns, using the height/width
    // ratio, because a cell column costs one extra token per row.
    let ratio = height / width;
    let cols = ((f64::from(MAX_IMAGE_TOKENS) - 2.0) / ratio + 0.25).sqrt() - 0.5;
    let rows = cols * ratio;
    let cells = |cells: f64, edge: f64| ((cells * edge) as u32).max(1);
    let (best_width, best_height) = if cols < 1.0 {
        // Too tall for a single cell column: one cell wide, as many rows as
        // the cap allows (`rows * 2 + 2 <= cap`).
        (
            cells(1.0, f64::from(CELL)),
            cells(
                ((f64::from(MAX_IMAGE_TOKENS) - 2.0) / 2.0).floor(),
                f64::from(CELL),
            ),
        )
    } else if rows < 1.0 {
        // Too wide for a single cell row: one cell tall, as many columns as
        // the cap allows (`1 * (cols + 1) + 2 <= cap`).
        (
            cells(f64::from(MAX_IMAGE_TOKENS) - 3.0, f64::from(CELL)),
            cells(1.0, f64::from(CELL)),
        )
    } else {
        let scale =
            (cols.trunc() * f64::from(CELL) / width).min(rows.trunc() * f64::from(CELL) / height);
        // The f64 cast saturates; the patch multiplication must not wrap.
        let patches = |edge: f64, scale: f64| {
            ((edge * scale / f64::from(PATCH)) as u32).saturating_mul(PATCH)
        };
        (patches(width, scale), patches(height, scale))
    };
    (best_width.max(1), best_height.max(1))
}

/// A model-facing rejection message for an image over the documented byte
/// cap. Clear about what was refused and why; the request itself still goes
/// out, with this text in place of the image.
fn too_large(hash: &str, len: u64) -> String {
    format!("[image too large: {hash} is {len} bytes; the limit is {MAX_IMAGE_BYTES} bytes]")
}

/// Read at most `cap` bytes, so no store file can be materialised whole
/// before its size is known.
fn load_capped(path: &Path, cap: usize) -> Result<Vec<u8>, LoadError> {
    let file = std::fs::File::open(path).map_err(|_| LoadError::Missing)?;
    let len = file.metadata().map_err(|_| LoadError::Missing)?.len();
    if len > cap as u64 {
        return Err(LoadError::TooLarge(len));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    let mut reader = file.take(cap as u64 + 1);
    reader
        .read_to_end(&mut bytes)
        .map_err(|_| LoadError::Missing)?;
    // The metadata length can lie (a growing file); the read itself is the
    // authority.
    if bytes.len() > cap {
        return Err(LoadError::TooLarge(bytes.len() as u64));
    }
    Ok(bytes)
}

enum LoadError {
    Missing,
    TooLarge(u64),
}

/// Wire bytes for one stored image: `(bytes, MIME)`, normalised to the token
/// budget. `full_size` bypasses [`normalise`] entirely for an image appended
/// after the newest assistant message: its stored bytes and MIME go out
/// unchanged (the 10 MiB ingest cap still applies). `Err` is the model-facing
/// text part that replaces the image (the existing `[image missing: <hash>]`
/// degradation, or a clear too-large rejection); it never fails the whole
/// request. Bytes this build cannot decode are forwarded unchanged.
pub fn wire_image(
    store: Option<&Path>,
    image: &ImagePart,
    full_size: bool,
) -> Result<(Vec<u8>, String), String> {
    let missing = || format!("[image missing: {}]", image.hash);
    let Some(store) = store else {
        return Err(missing());
    };
    let original = match load_capped(&store.join(&image.hash), MAX_IMAGE_BYTES) {
        Ok(bytes) => bytes,
        Err(LoadError::Missing) => return Err(missing()),
        Err(LoadError::TooLarge(len)) => return Err(too_large(&image.hash, len)),
    };
    if full_size {
        return Ok((original, image.mime.clone()));
    }
    normalise(store, &image.hash, &image.mime, &original)
}

/// `Ok` when the cache entry is self-consistent AND the source still hashes
/// to the key it is filed under.
///
/// A stale or wrong variant (a torn write, a replaced store file, an entry
/// from a different image) must never authorize sending bytes that no longer
/// match the image: the source digest is re-derived on every use and the
/// cached variant carries its own SHA-256 sidecar. A mismatch invalidates the
/// entry and re-encodes from the actual source bytes.
fn read_cache(cache: &Path) -> Option<Vec<u8>> {
    let bytes = load_capped(cache, MAX_IMAGE_BYTES).ok()?;
    // A zero-byte file is a torn write, never a valid variant.
    if bytes.is_empty() {
        invalidate_cache(cache);
        return None;
    }
    // The sidecar is one 64-char digest; read it through the same cap so a
    // tampered sidecar cannot become a huge allocation either.
    let digest_bytes = load_capped(&cache_digest_path(cache), 128).ok()?;
    let Ok(digest) = std::str::from_utf8(&digest_bytes) else {
        invalidate_cache(cache);
        return None;
    };
    if digest.trim() != crate::agent::image_sha256(&bytes) {
        invalidate_cache(cache);
        return None;
    }
    Some(bytes)
}

/// The `.sha256` sidecar that makes a cached variant self-validating.
fn cache_digest_path(cache: &Path) -> PathBuf {
    let mut name = cache.as_os_str().to_os_string();
    name.push(".sha256");
    PathBuf::from(name)
}

/// A unique temp name for one write. Two concurrent sends of the same image
/// must never share a temp file: the old `<pid>`-only name let them truncate
/// and interleave each other's bytes.
fn cache_temp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{serial}", std::process::id()));
    path.with_file_name(name)
}

/// Write `bytes` to `path` through a unique temp file and an atomic rename.
/// False on any failure; cache writes are best-effort.
fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    let temp = cache_temp_path(path);
    if std::fs::write(&temp, bytes).is_ok() && std::fs::rename(&temp, path).is_ok() {
        true
    } else {
        let _ = std::fs::remove_file(&temp);
        false
    }
}

fn invalidate_cache(cache: &Path) {
    let _ = std::fs::remove_file(cache);
    let _ = std::fs::remove_file(cache_digest_path(cache));
}

/// Best-effort cache write: the variant, then its digest sidecar, both via a
/// unique temp file renamed into place. A concurrent reader never sees a
/// partial variant; a crash between the two renames leaves an entry whose
/// sidecar does not match, which the next read invalidates. Cache failures
/// never fail the request — the encoded bytes are already in hand.
fn write_cache(cache: &Path, bytes: &[u8]) {
    let digest = crate::agent::image_sha256(bytes);
    if write_atomic(cache, bytes) {
        let _ = write_atomic(&cache_digest_path(cache), digest.as_bytes());
    }
}

/// Normalise `original` (the stored bytes for `hash`, declared as `mime`) to
/// the token budget. Returns the bytes to put on the wire and their MIME.
/// [`wire_image`] skips this for the newest images (`full_size`).
///
/// An image that already fits is returned byte-identical with its original
/// MIME. Otherwise the re-encoded variant is cached as
/// `<hash>.n<WxH>.<ext>` (plus a `.sha256` sidecar) under `store` and reused
/// on later requests; the stored original is never touched.
pub fn normalise(
    store: &Path,
    hash: &str,
    mime: &str,
    original: &[u8],
) -> Result<(Vec<u8>, String), String> {
    // An image this build cannot decode is forwarded byte-identical: its
    // token cost is unknown, but dropping bytes the provider may accept is
    // worse than sending them unmeasured. Size is checked first so the
    // decode never sees an unbounded buffer.
    let unchanged = || Ok((original.to_vec(), mime.to_owned()));
    if original.len() > MAX_IMAGE_BYTES {
        return Err(too_large(hash, original.len() as u64));
    }
    let Some(decoder) = image::ImageReader::new(std::io::Cursor::new(original))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_decoder().ok())
    else {
        return unchanged();
    };
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    let color = image::ImageDecoder::color_type(&decoder);
    match decoded_bytes(width, height, color) {
        Some(bytes) if bytes > MAX_DECODE_BYTES => {
            return Err(format!(
                "[image too large: {hash} decodes to {bytes} bytes, over the {MAX_DECODE_BYTES}-byte decode limit]"
            ));
        }
        None => {
            return Err(format!(
                "[image too large: {hash} decodes to an unrepresentable size]"
            ));
        }
        Some(_) => {}
    }
    let alpha = color.has_alpha();
    let Some((target_width, target_height)) = fit_to_budget(width, height) else {
        return unchanged();
    };
    let (extension, target_mime) = if alpha {
        ("png", "image/png")
    } else {
        ("jpg", "image/jpeg")
    };
    let cache = store.join(format!(
        "{hash}.n{target_width}x{target_height}.{extension}"
    ));
    // The cache key is the source digest: only trust either side when the
    // bytes currently at `<store>/<hash>` still hash to `hash`.
    let cache_key_matches = crate::agent::image_sha256(original) == hash;
    if cache_key_matches {
        // A validated cache hit short-circuits the (expensive) pixel decode.
        if let Some(bytes) = read_cache(&cache) {
            return Ok((bytes, target_mime.to_owned()));
        }
    } else {
        // The source no longer matches its content address: any variant filed
        // under it is stale for the actual bytes.
        invalidate_cache(&cache);
    }
    let Ok(decoded) = image::DynamicImage::from_decoder(decoder) else {
        return unchanged();
    };
    let resized = decoded.resize_exact(
        target_width,
        target_height,
        image::imageops::FilterType::Triangle,
    );
    let mut bytes = Vec::new();
    let encoded = if alpha {
        resized.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
            .encode_image(&resized)
    };
    if encoded.is_err() {
        return unchanged();
    }
    // Never file a variant under a digest the source does not actually have:
    // it could never be validated on the next use.
    if cache_key_matches {
        write_cache(&cache, &bytes);
    }
    Ok((bytes, target_mime.to_owned()))
}

/// Placeholder text that replaces ONE offloaded image part on the wire. It
/// names the stored hash (the file name in the global image store, see
/// [`crate::agent::image_store_dir`]); `read_image` accepts that bare hash as
/// its `path`, so the attachment can be read again, and the text says how many
/// older images the budget dropped.
fn placeholder(hash: &str, omitted: usize) -> String {
    format!("[image omitted: {omitted} older image(s); read again with read_image (hash {hash})]")
}

/// The recency half of the budget: which image occurrences become a text
/// placeholder, and which hashes were appended after the newest assistant
/// message (and therefore go out at full size). Pure and deterministic — it
/// depends only on the messages and `max_inline`, never on the clock, the
/// image store, or earlier requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageBudget {
    /// Offloaded hash -> the placeholder text that replaces each occurrence.
    placeholders: BTreeMap<String, String>,
    /// Hashes whose first occurrence is after the newest assistant message:
    /// never delivered, never offloaded, and sent at full size.
    newest: HashSet<String>,
}

impl ImageBudget {
    /// Keep the most recent `max_inline` DISTINCT DELIVERED images (by first
    /// appearance in request order); every occurrence of an older delivered
    /// image becomes a placeholder. `max_inline = 0` never offloads.
    ///
    /// Delivered means the model has already answered once with that image in
    /// its request: its first occurrence precedes the newest assistant
    /// message. Everything after that message was appended during the current
    /// round (a fresh attachment, a tool result) and has never been on the
    /// wire, so it is NEVER offloaded — a newly attached image always gets at
    /// least one send, at full size. This stays a pure function of the
    /// messages: it needs no cross-request state, and identical messages still
    /// project to identical request bytes.
    pub fn project(messages: &[Message], max_inline: usize) -> Self {
        let mut order: Vec<(&str, usize)> = Vec::new();
        let mut seen = HashSet::new();
        for (index, message) in messages.iter().enumerate() {
            for image in message_images(message) {
                if seen.insert(image.hash.as_str()) {
                    order.push((image.hash.as_str(), index));
                }
            }
        }
        // The newest model response proves every earlier message (and every
        // image in it) was part of a request the provider already saw.
        let delivered_through = messages
            .iter()
            .rposition(|message| matches!(message, Message::Assistant(_)));
        // Exactly the complement of `delivered` below: the offload rule never
        // considers these, and `wire_image` sends them at full size.
        let newest: HashSet<String> = order
            .iter()
            .filter(|(_, index)| delivered_through.is_none_or(|last| *index >= last))
            .map(|(hash, _)| (*hash).to_owned())
            .collect();
        if max_inline == 0 {
            return Self {
                placeholders: BTreeMap::new(),
                newest,
            };
        }
        let delivered: Vec<&str> = order
            .iter()
            .filter(|(_, index)| delivered_through.is_some_and(|last| *index < last))
            .map(|(hash, _)| *hash)
            .collect();
        let keep = max_inline.min(delivered.len());
        let dropped = &delivered[..delivered.len() - keep];
        let placeholders = dropped
            .iter()
            .map(|hash| ((*hash).to_owned(), placeholder(hash, dropped.len())))
            .collect();
        Self {
            placeholders,
            newest,
        }
    }

    /// Placeholder text for an offloaded image hash, if it is offloaded.
    pub fn placeholder(&self, hash: &str) -> Option<&str> {
        self.placeholders.get(hash).map(String::as_str)
    }

    /// Whether `hash` was appended after the newest assistant message: the
    /// images [`Self::project`] never considers delivered or offloadable, and
    /// the only ones sent at full size.
    pub fn is_newest(&self, hash: &str) -> bool {
        self.newest.contains(hash)
    }
}

/// Image attachments of a message. Only user and tool messages carry them
/// (assistant messages never do), matching the walk the image-rejection
/// classifier uses.
pub fn message_images(message: &Message) -> &[ImagePart] {
    match message {
        Message::User { images, .. } | Message::Tool { images, .. } => images,
        Message::System { .. } | Message::Assistant(_) => &[],
    }
}

#[cfg(test)]
mod tests;
