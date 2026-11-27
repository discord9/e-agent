use std::path::{Path, PathBuf};

use super::*;

/// A solid-colour image encoded as PNG (fast to compress, so the large
/// fixtures stay cheap).
fn png_rgb(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(width, height, image::Rgb(rgb)));
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
    if !alpha {
        return png_rgb(width, height, [30, 60, 90]);
    }
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([200, 30, 30, 128]),
    ));
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// A solid-colour JPEG, used to stand in for a cached variant.
fn jpeg_rgb(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(width, height, image::Rgb(rgb)));
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode_image(&image)
        .unwrap();
    bytes
}

/// The content address the image store files `bytes` under.
fn digest(bytes: &[u8]) -> String {
    crate::agent::image_sha256(bytes)
}

/// The one variant file (ignoring the `.sha256` sidecar) in `store`.
fn variant_path(store: &Path) -> PathBuf {
    let mut variants: Vec<_> = std::fs::read_dir(store)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.extension().is_some_and(|ext| ext == "sha256"))
        .collect();
    assert_eq!(variants.len(), 1, "exactly one variant: {variants:?}");
    variants.pop().unwrap()
}

/// Centre pixel of a JPEG/PNG payload.
fn centre_pixel(bytes: &[u8]) -> [u8; 3] {
    let image = image::load_from_memory(bytes).unwrap().to_rgb8();
    image.get_pixel(image.width() / 2, image.height() / 2).0
}

fn user_with(hash: &str) -> Message {
    Message::User {
        content: "look at this".into(),
        images: vec![ImagePart {
            hash: hash.into(),
            mime: "image/png".into(),
        }],
    }
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant(crate::agent::AssistantMessage {
        content: Some(text.into()),
        tool_calls: Vec::new(),
        reasoning: None,
    })
}

/// Vectors from DeepSeek's published image-token calculator
/// (<https://api-docs.deepseek.com/quick_start/token_usage/>, the "Image
/// Token Calculator"; `v41` preset of the shipped JS: patch 14, downsample
/// 3:1 per axis, cap 1024, 544x544 pixel floor). The two 994s are the
/// documented pair: "a 2000x2000 image and a 5000x5000 image consume the same
/// number of tokens after resizing".
#[test]
fn published_calculator_examples() {
    for (width, height, tokens) in [
        (1, 1, 184),
        (100, 100, 184),
        (543, 544, 184),
        (544, 544, 184),
        (800, 600, 317),
        (1000, 1000, 602),
        (1300, 1300, 994),
        (1920, 1080, 968),
        (2000, 2000, 994),
        (4000, 3000, 1001),
        (5000, 5000, 994),
        (10000, 100, 722),
    ] {
        assert_eq!(
            estimated_tokens(width, height),
            tokens,
            "{width}x{height} must match the published calculator"
        );
    }
    // The provider's 544x544 floor is accounting only: a tiny image is
    // billed as if upscaled, but it is never actually upscaled for the wire.
    assert_eq!(estimated_tokens(100, 100), estimated_tokens(544, 544));
    assert_eq!(fit_to_budget(100, 100), None);
    assert_eq!(fit_to_budget(544, 544), None);
}

/// Property test over a deterministic sweep of sizes: whatever fits is left
/// alone, whatever does not is scaled DOWN to a grid under the cap, and the
/// math never produces an upscale or an empty image.
#[test]
fn huge_images_normalise_under_the_cap_and_tiny_ones_are_untouched() {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..128 {
        let width = 1 + (next() % 9_000) as u32;
        let height = 1 + (next() % 9_000) as u32;
        match fit_to_budget(width, height) {
            None => {
                assert!(
                    grid_tokens(width, height) <= MAX_IMAGE_TOKENS,
                    "{width}x{height} fits but costs {} tokens",
                    grid_tokens(width, height)
                );
            }
            Some((target_width, target_height)) => {
                assert!(
                    target_width <= width && target_height <= height,
                    "{width}x{height} upscaled to {target_width}x{target_height}"
                );
                assert!(target_width >= 1 && target_height >= 1);
                assert!(
                    grid_tokens(target_width, target_height) <= MAX_IMAGE_TOKENS,
                    "{width}x{height} -> {target_width}x{target_height} still costs {} tokens",
                    grid_tokens(target_width, target_height)
                );
                assert!(estimated_tokens(target_width, target_height) <= MAX_IMAGE_TOKENS);
            }
        }
    }
    // The published solve, checked explicitly: 4000x3000 keeps its 4:3 ratio
    // in whole 14px patches and lands at 1001 estimated tokens.
    assert_eq!(fit_to_budget(4000, 3000), Some((1512, 1134)));
    assert_eq!(grid_tokens(1512, 1134), 1001);
}

/// Extreme dimensions must saturate, never wrap: a wrapped token count could
/// look under the cap and authorize sending a far too large image.
#[test]
fn extreme_dimensions_saturate_instead_of_wrapping() {
    assert_eq!(grid_tokens(u32::MAX, u32::MAX), u32::MAX);
    assert!(estimated_tokens(u32::MAX, u32::MAX) <= MAX_IMAGE_TOKENS);
    for (width, height) in [(u32::MAX, u32::MAX), (u32::MAX, 1), (1, u32::MAX)] {
        let (target_width, target_height) = fit_to_budget(width, height).unwrap();
        assert!(target_width >= 1 && target_height >= 1);
        assert!(target_width <= width && target_height <= height);
        assert!(
            grid_tokens(target_width, target_height) <= MAX_IMAGE_TOKENS,
            "{width}x{height} -> {target_width}x{target_height} still over the cap"
        );
    }
    // Decoded-size arithmetic is checked too: a huge square overflows u64 and
    // is rejected rather than wrapping to a small allocation.
    assert_eq!(
        decoded_bytes(u32::MAX, u32::MAX, image::ColorType::Rgba8),
        None
    );
    assert!(decoded_bytes(100, 100, image::ColorType::Rgb8).is_some());
}

#[test]
fn a_4000x3000_png_shrinks_and_keeps_its_aspect() {
    let store = tempfile::tempdir().unwrap();
    let original = png(4000, 3000, false);
    let hash = digest(&original);
    let (bytes, mime) = normalise(store.path(), &hash, "image/png", &original).unwrap();
    assert_eq!(mime, "image/jpeg", "an opaque photo re-encodes as JPEG");
    assert_ne!(bytes, original);
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1512, 1134));
    // Aspect ratio preserved within a pixel.
    let expected = 1512.0 * 3000.0 / 4000.0;
    assert!((f64::from(decoded.height()) - expected).abs() <= 1.0);
    assert!(grid_tokens(decoded.width(), decoded.height()) <= MAX_IMAGE_TOKENS);
}

#[test]
fn alpha_survives_as_png() {
    let store = tempfile::tempdir().unwrap();
    let original = png(2000, 1500, true);
    let hash = digest(&original);
    let (bytes, mime) = normalise(store.path(), &hash, "image/png", &original).unwrap();
    assert_eq!(mime, "image/png", "anything with alpha stays PNG");
    assert_ne!(bytes, original);
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert!(decoded.color().has_alpha());
    assert!(decoded.width() < 2000);
}

#[test]
fn a_fitting_image_is_returned_byte_identical() {
    let store = tempfile::tempdir().unwrap();
    let original = png(100, 100, false);
    let hash = digest(&original);
    let (bytes, mime) = normalise(store.path(), &hash, "image/png", &original).unwrap();
    assert_eq!(bytes, original, "identical dimensions: no re-encode");
    assert_eq!(mime, "image/png", "the source MIME is preserved");
    // Nothing was written next to the original.
    assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
}

/// A cache entry is only usable when BOTH checks pass: the source still
/// hashes to the key, and the variant matches its own `.sha256` sidecar.
#[test]
fn the_encoded_variant_is_cached_and_reused() {
    let store = tempfile::tempdir().unwrap();
    let original = png(4000, 3000, false);
    let hash = digest(&original);
    let first = normalise(store.path(), &hash, "image/png", &original).unwrap();
    let cached = variant_path(store.path());
    assert_eq!(
        cached.file_name().unwrap().to_str().unwrap(),
        format!("{hash}.n1512x1134.jpg")
    );
    assert_eq!(std::fs::read(&cached).unwrap(), first.0);
    assert_eq!(
        std::fs::read_to_string(cache_digest_path(&cached)).unwrap(),
        digest(&first.0),
        "the variant carries its own digest"
    );

    // A valid, self-consistent entry is reused verbatim instead of being
    // re-encoded.
    let sentinel = jpeg_rgb(1512, 1134, [200, 30, 30]);
    std::fs::write(&cached, &sentinel).unwrap();
    std::fs::write(cache_digest_path(&cached), digest(&sentinel)).unwrap();
    assert_eq!(
        normalise(store.path(), &hash, "image/png", &original)
            .unwrap()
            .0,
        sentinel,
        "a validated cache hit short-circuits the pixel decode"
    );

    // A variant that does not match its sidecar is stale: invalidate and
    // re-encode from the source instead of sending it.
    let tampered = jpeg_rgb(1512, 1134, [30, 30, 200]);
    std::fs::write(&cached, &tampered).unwrap();
    let third = normalise(store.path(), &hash, "image/png", &original).unwrap();
    assert_ne!(third.0, tampered, "a tampered variant is never sent");
    assert_eq!(third, first, "the source re-encodes deterministically");
    assert_eq!(
        std::fs::read_to_string(cache_digest_path(&cached)).unwrap(),
        digest(&first.0),
        "the cache is restored from the real source"
    );
    // With the real variant back, a fourth call leaves the cache untouched.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let before = std::fs::metadata(&cached).unwrap().modified().unwrap();
    let fourth = normalise(store.path(), &hash, "image/png", &original).unwrap();
    assert_eq!(fourth, first);
    let after = std::fs::metadata(&cached).unwrap().modified().unwrap();
    assert_eq!(before, after, "a cache hit must not rewrite the variant");
    // The stored original is never mutated.
    assert!(std::fs::metadata(store.path().join(&hash)).is_err());
}

/// The cache key is the source digest. Replacing the stored bytes while
/// keeping the old hash must invalidate the entry: the old variant no longer
/// matches the image.
#[test]
fn a_stale_variant_for_changed_source_bytes_is_not_used() {
    let store = tempfile::tempdir().unwrap();
    let red = png_rgb(2000, 1500, [200, 30, 30]);
    let hash = digest(&red);
    let first = normalise(store.path(), &hash, "image/png", &red).unwrap();
    let cached = variant_path(store.path());
    let red_pixel = centre_pixel(&first.0);
    assert!(red_pixel[0] > red_pixel[2], "the first variant is red");

    // Same key, different bytes (a replaced/tampered store file).
    let blue = png_rgb(2000, 1500, [30, 30, 200]);
    let second = normalise(store.path(), &hash, "image/png", &blue).unwrap();
    assert_ne!(second.0, first.0, "the stale variant is not reused");
    let blue_pixel = centre_pixel(&second.0);
    assert!(
        blue_pixel[2] > blue_pixel[0],
        "the wire bytes come from the actual source: {blue_pixel:?}"
    );
    assert!(
        !cached.exists(),
        "a variant filed under a digest the source no longer has is invalidated"
    );
}

#[test]
fn a_zero_byte_cache_file_is_not_trusted() {
    let store = tempfile::tempdir().unwrap();
    let original = png(4000, 3000, false);
    let hash = digest(&original);
    let cache = store.path().join(format!("{hash}.n1512x1134.jpg"));
    std::fs::write(&cache, b"").unwrap();
    // Even a sidecar that "matches" the empty file must not authorize it.
    std::fs::write(cache_digest_path(&cache), digest(b"")).unwrap();
    let (bytes, mime) = normalise(store.path(), &hash, "image/png", &original).unwrap();
    assert_eq!(mime, "image/jpeg");
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1512, 1134));
    assert!(!std::fs::read(&cache).unwrap().is_empty());
}

/// The encoded-file cap is enforced BEFORE the read, so a huge or tampered
/// store file is rejected with a clear model-facing error instead of being
/// materialised.
#[test]
fn an_oversize_stored_file_is_rejected_with_a_clear_error() {
    let store = tempfile::tempdir().unwrap();
    let huge = store.path().join("huge");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
    let image = ImagePart {
        hash: "huge".into(),
        mime: "image/png".into(),
    };
    let error = wire_image(Some(store.path()), &image).unwrap_err();
    assert!(error.contains("image too large"), "{error}");
    assert!(error.contains("huge"), "{error}");
    assert!(
        error.contains(&MAX_IMAGE_BYTES.to_string()),
        "the limit is stated: {error}"
    );
    // Missing files keep the existing degradation.
    let missing = ImagePart {
        hash: "nope".into(),
        mime: "image/png".into(),
    };
    assert_eq!(
        wire_image(Some(store.path()), &missing).unwrap_err(),
        "[image missing: nope]"
    );
    // The same cap is enforced when bytes are handed in directly.
    let oversized = vec![0u8; MAX_IMAGE_BYTES + 1];
    let error = normalise(store.path(), "big", "image/png", &oversized).unwrap_err();
    assert!(error.contains("image too large"), "{error}");
    assert_eq!(std::fs::read(&huge).unwrap().len(), MAX_IMAGE_BYTES + 1);
}

/// A header can claim an enormous image with almost no bytes behind it. The
/// decoded-size guard must reject it with a clear error BEFORE the decoder
/// materialises the pixel buffer.
#[test]
fn a_huge_declared_decode_is_rejected_with_a_clear_error() {
    let store = tempfile::tempdir().unwrap();
    let mut jpeg = jpeg_rgb(8, 8, [30, 60, 90]);
    let sof = jpeg
        .windows(2)
        .position(|window| window == [0xFF, 0xC0])
        .expect("a baseline SOF0 segment");
    // SOF0: marker, length, precision, then 16-bit height and width.
    jpeg[sof + 5..sof + 7].copy_from_slice(&20_000u16.to_be_bytes());
    jpeg[sof + 7..sof + 9].copy_from_slice(&20_000u16.to_be_bytes());
    let hash = digest(&jpeg);
    let error = normalise(store.path(), &hash, "image/jpeg", &jpeg).unwrap_err();
    assert!(error.contains("image too large"), "{error}");
    assert!(error.contains("decode limit"), "{error}");
    assert!(error.contains(&hash), "{error}");
    // Nothing was decoded, encoded, or cached.
    assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
}

#[test]
fn an_undecodable_image_is_forwarded_unchanged() {
    // Unmeasurable is not the same as missing: the bytes go out untouched
    // (and no variant is cached) instead of being dropped from the request.
    let store = tempfile::tempdir().unwrap();
    let original = b"not an image";
    let (bytes, mime) = normalise(store.path(), &digest(original), "image/png", original).unwrap();
    assert_eq!(bytes, original);
    assert_eq!(mime, "image/png");
    assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
}

/// Two writers must never share a temp file, and a writer must never clobber
/// another writer's in-flight temp.
#[test]
fn cache_temp_names_are_unique_per_write() {
    let first = cache_temp_path(Path::new("abc.n10x10.jpg"));
    let second = cache_temp_path(Path::new("abc.n10x10.jpg"));
    assert_ne!(first, second, "each write gets its own temp name");
    assert!(
        first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(".tmp."),
        "{first:?}"
    );

    // Simulate another same-pid writer's in-flight temp using the old
    // `<pid>`-only naming: the write must not touch it.
    let store = tempfile::tempdir().unwrap();
    let cache = store.path().join("abc.n10x10.jpg");
    let in_flight = cache.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&in_flight, b"other writer").unwrap();
    write_cache(&cache, b"complete variant");
    assert_eq!(std::fs::read(&cache).unwrap(), b"complete variant");
    assert_eq!(std::fs::read(&in_flight).unwrap(), b"other writer");
}

#[test]
fn concurrent_cache_writes_leave_one_complete_payload() {
    let store = tempfile::tempdir().unwrap();
    let cache = store.path().join("concurrent.n10x10.jpg");
    let first = vec![b'a'; 4 * 1024 * 1024];
    let second = vec![b'b'; 4 * 1024 * 1024];
    std::thread::scope(|scope| {
        scope.spawn(|| write_cache(&cache, &first));
        scope.spawn(|| write_cache(&cache, &second));
    });
    let bytes = std::fs::read(&cache).unwrap();
    assert!(
        bytes == first || bytes == second,
        "one writer's complete payload must win, never an interleaving"
    );
    for entry in std::fs::read_dir(store.path()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".tmp."), "temp file left behind: {name}");
    }
}

#[test]
fn the_oldest_delivered_images_are_offloaded_and_the_newest_stay() {
    let mut messages: Vec<Message> = (0..10)
        .map(|index| user_with(&format!("h{index}")))
        .collect();
    // The model already answered once with all ten in context: all delivered.
    messages.push(assistant_text("seen"));
    let budget = ImageBudget::project(&messages, 6);
    assert_eq!(
        budget.placeholder("h0"),
        Some("[image omitted: 4 older image(s); read again with read_image (hash h0)]"),
        "the placeholder names the stored hash and how to recover it"
    );
    for index in 0..4 {
        assert!(budget.placeholder(&format!("h{index}")).is_some());
    }
    for index in 4..10 {
        assert_eq!(budget.placeholder(&format!("h{index}")), None);
    }
    // Pure: the same messages project to the same budget, every time.
    assert_eq!(budget, ImageBudget::project(&messages, 6));
    assert_eq!(budget, ImageBudget::project(&messages.clone(), 6));
}

/// Finding 1: an image that has never been on the wire must never be
/// offloaded. A batch larger than `max_inline` goes out whole; only after a
/// model response are those images eligible for eviction.
#[test]
fn a_new_image_is_never_offloaded_before_it_is_sent() {
    // Ten brand-new images, no model response yet: nothing is delivered, so
    // nothing may be dropped even though max_inline is six.
    let mut messages: Vec<Message> = (0..10)
        .map(|index| user_with(&format!("h{index}")))
        .collect();
    let fresh = ImageBudget::project(&messages, 6);
    assert_eq!(fresh, ImageBudget::default(), "nothing is evictable yet");
    for index in 0..10 {
        assert_eq!(fresh.placeholder(&format!("h{index}")), None);
    }

    // After a response all ten count as delivered: the oldest four are
    // deferred, the newest six stay.
    messages.push(assistant_text("seen"));
    let delivered = ImageBudget::project(&messages, 6);
    for index in 0..4 {
        assert!(delivered.placeholder(&format!("h{index}")).is_some());
    }
    for index in 4..10 {
        assert_eq!(delivered.placeholder(&format!("h{index}")), None);
    }

    // A batch of eight more new images in the next round: all are kept even
    // though they exceed max_inline; the delivered ones still obey the cap.
    messages.extend((0..8).map(|index| user_with(&format!("new{index}"))));
    let batch = ImageBudget::project(&messages, 6);
    for index in 0..8 {
        assert_eq!(
            batch.placeholder(&format!("new{index}")),
            None,
            "a never-delivered image is never offloaded"
        );
    }
    for index in 0..4 {
        assert!(batch.placeholder(&format!("h{index}")).is_some());
    }
    for index in 4..10 {
        assert_eq!(batch.placeholder(&format!("h{index}")), None);
    }
}

#[test]
fn max_inline_zero_never_offloads() {
    let mut messages: Vec<Message> = (0..10)
        .map(|index| user_with(&format!("h{index}")))
        .collect();
    messages.push(assistant_text("seen"));
    let budget = ImageBudget::project(&messages, 0);
    assert_eq!(budget, ImageBudget::default(), "nothing is offloaded");
    for index in 0..10 {
        assert_eq!(budget.placeholder(&format!("h{index}")), None);
    }
}

#[test]
fn repeated_hashes_count_once_and_offload_together() {
    // Three distinct images, one repeated: the distinct count drives the
    // window, and an offloaded hash loses ALL of its occurrences.
    let messages = vec![
        user_with("a"),
        user_with("b"),
        user_with("a"),
        user_with("c"),
        assistant_text("seen"),
    ];
    let budget = ImageBudget::project(&messages, 2);
    assert!(budget.placeholder("a").is_some());
    assert_eq!(budget.placeholder("b"), None);
    assert_eq!(budget.placeholder("c"), None);
}

#[test]
fn tool_images_count_towards_the_same_window() {
    let mut messages = vec![user_with("u0")];
    for index in 1..10 {
        messages.push(Message::Tool {
            call_id: format!("call-{index}"),
            name: "read_image".into(),
            content: "read".into(),
            is_error: false,
            images: vec![ImagePart {
                hash: format!("t{index}"),
                mime: "image/png".into(),
            }],
            synthetic: false,
        });
    }
    messages.push(assistant_text("seen"));
    let budget = ImageBudget::project(&messages, 6);
    // The newest six survive: t4..t9.
    assert!(budget.placeholder("u0").is_some());
    for index in 1..4 {
        assert!(budget.placeholder(&format!("t{index}")).is_some());
    }
    for index in 4..10 {
        assert_eq!(budget.placeholder(&format!("t{index}")), None);
    }
}

/// A tool batch appends several brand-new images at once. They are all after
/// the newest assistant message, so none may be offloaded in that request;
/// once the model has answered they follow the ordinary window.
#[test]
fn a_tool_batch_of_new_images_is_never_offloaded() {
    let mut messages = vec![user_with("u0")];
    messages.push(assistant_text("reading"));
    for index in 0..7 {
        messages.push(Message::Tool {
            call_id: format!("call-{index}"),
            name: "read_image".into(),
            content: "read".into(),
            is_error: false,
            images: vec![ImagePart {
                hash: format!("t{index}"),
                mime: "image/png".into(),
            }],
            synthetic: false,
        });
    }
    let fresh = ImageBudget::project(&messages, 2);
    assert_eq!(
        fresh.placeholder("u0"),
        None,
        "u0 is the only delivered image and fits"
    );
    for index in 0..7 {
        assert_eq!(
            fresh.placeholder(&format!("t{index}")),
            None,
            "tool result {index} has never been sent"
        );
    }

    // After the next response every image is delivered: the newest two stay.
    messages.push(assistant_text("seen"));
    let later = ImageBudget::project(&messages, 2);
    assert!(later.placeholder("u0").is_some());
    for index in 0..5 {
        assert!(later.placeholder(&format!("t{index}")).is_some());
    }
    assert_eq!(later.placeholder("t5"), None);
    assert_eq!(later.placeholder("t6"), None);
}
