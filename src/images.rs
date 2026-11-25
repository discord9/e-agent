//! The outgoing image budget shared by both provider wires.
//!
//! Two behaviours, both applied to the REQUEST COPY only (history, events,
//! the session store and compaction never see either):
//!
//! 1. **Normalisation.** Every image about to be put on the wire is measured
//!    with DeepSeek's published vision-token calculator (14px patches merged
//!    3:1 per axis into one token cell, `tokens = grid_h * (grid_w + 1) + 2`,
//!    see [`estimated_tokens`]) and scaled DOWN — never up — until the
//!    estimate fits [`MAX_IMAGE_TOKENS`]. The re-encoded variant is cached
//!    next to the original as `<hash>.n<WxH>.<ext>`; the original file is
//!    never mutated. An image that already fits keeps its exact stored bytes
//!    and MIME type (no decode of pixels, no re-encode).
//!
//! 2. **Offload.** When a request would carry more than `max_inline` DISTINCT
//!    images, the oldest ones (by first appearance in request order) are
//!    replaced by a text placeholder naming their stored hash, so the model
//!    knows the image exists and how to get it back. [`ImageBudget::project`]
//!    is pure: the same messages always produce the same placeholders, in
//!    the same order, and therefore the same request bytes.
//!
//! Out of scope by design: retention/expiry by wall-clock time, a "recent
//! turns" heuristic, automatic re-reads on offload, summarisation, and any
//! second storage format. Offloading deliberately trades prompt-cache reuse
//! from the first changed message onward for a bounded image payload.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::agent::{ImagePart, Message};

/// Edge of one vision patch, in pixels (the provider's encoder input).
const PATCH: u32 = 14;
/// Patches merged per axis into one vision-token cell.
const DOWNSAMPLE: u32 = 3;
/// Edge of one vision-token cell, in pixels (`PATCH * DOWNSAMPLE`).
const CELL: u32 = PATCH * DOWNSAMPLE;

/// Hard cap on the estimated vision tokens of one outgoing image.
pub const MAX_IMAGE_TOKENS: u32 = 1024;

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

/// `tokens = grid_h * (grid_w + 1) + 2`.
fn grid_tokens(width: u32, height: u32) -> u32 {
    let (rows, cols) = token_grid(width, height);
    rows * (cols + 1) + 2
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
        let patches = |edge: f64, scale: f64| (edge * scale / f64::from(PATCH)) as u32 * PATCH;
        (patches(width, scale), patches(height, scale))
    };
    (best_width.max(1), best_height.max(1))
}

/// Wire bytes for one stored image: `(bytes, MIME)`, normalised to the token
/// budget. `None` only when the stored file is missing — the caller keeps
/// today's `[image missing: <hash>]` degradation instead of failing the
/// request. Bytes this build cannot decode are forwarded unchanged.
pub fn wire_image(store: Option<&Path>, image: &ImagePart) -> Option<(Vec<u8>, String)> {
    let original = crate::agent::load_image_bytes(store, &image.hash)?;
    Some(normalise(store?, &image.hash, &image.mime, &original))
}

/// Normalise `original` (the stored bytes for `hash`, declared as `mime`) to
/// the token budget. Returns the bytes to put on the wire and their MIME.
///
/// An image that already fits is returned byte-identical with its original
/// MIME. Otherwise the re-encoded variant is cached as
/// `<hash>.n<WxH>.<ext>` under `store` and reused on later requests; the
/// stored original is never touched.
pub fn normalise(store: &Path, hash: &str, mime: &str, original: &[u8]) -> (Vec<u8>, String) {
    // An image this build cannot decode is forwarded byte-identical: its
    // token cost is unknown, but dropping bytes the provider may accept is
    // worse than sending them unmeasured.
    let unchanged = || (original.to_vec(), mime.to_owned());
    let Some(decoder) = image::ImageReader::new(std::io::Cursor::new(original))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_decoder().ok())
    else {
        return unchanged();
    };
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    let alpha = image::ImageDecoder::color_type(&decoder).has_alpha();
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
    // A cache hit short-circuits the (expensive) pixel decode. A zero-byte
    // file is a torn write, never a valid variant.
    if let Ok(bytes) = std::fs::read(&cache)
        && !bytes.is_empty()
    {
        return (bytes, target_mime.to_owned());
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
    write_cache(&cache, &bytes);
    (bytes, target_mime.to_owned())
}

/// Best-effort cache write: a temp file renamed into place, so a concurrent
/// reader never sees a partial variant. Cache failures never fail the
/// request — the encoded bytes are already in hand.
fn write_cache(path: &Path, bytes: &[u8]) {
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&temp, bytes).is_ok() {
        let _ = std::fs::rename(&temp, path);
    } else {
        let _ = std::fs::remove_file(&temp);
    }
}

/// Placeholder text that replaces ONE offloaded image part on the wire. It
/// names the stored hash (the file name in the global image store, see
/// [`crate::agent::image_store_dir`]) so the attachment can be read again,
/// and says how many older images the budget dropped.
fn placeholder(hash: &str, omitted: usize) -> String {
    format!("[image omitted: {omitted} older image(s); read again with read_image (hash {hash})]")
}

/// The offload half of the budget: which image occurrences become a text
/// placeholder. Pure and deterministic — it depends only on the messages and
/// `max_inline`, never on the clock, the image store, or earlier requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageBudget {
    /// Offloaded hash -> the placeholder text that replaces each occurrence.
    placeholders: BTreeMap<String, String>,
}

impl ImageBudget {
    /// Keep the most recent `max_inline` distinct images (by first appearance
    /// in request order); every occurrence of an older image becomes a
    /// placeholder. `max_inline = 0` never offloads.
    pub fn project(messages: &[Message], max_inline: usize) -> Self {
        let mut order: Vec<&str> = Vec::new();
        let mut seen = HashSet::new();
        for message in messages {
            for image in message_images(message) {
                if seen.insert(image.hash.as_str()) {
                    order.push(image.hash.as_str());
                }
            }
        }
        let keep = if max_inline == 0 {
            order.len()
        } else {
            max_inline.min(order.len())
        };
        let dropped = &order[..order.len() - keep];
        let placeholders = dropped
            .iter()
            .map(|hash| ((*hash).to_owned(), placeholder(hash, dropped.len())))
            .collect();
        Self { placeholders }
    }

    /// Placeholder text for an offloaded image hash, if it is offloaded.
    pub fn placeholder(&self, hash: &str) -> Option<&str> {
        self.placeholders.get(hash).map(String::as_str)
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
