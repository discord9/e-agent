use super::*;

/// A solid-colour image encoded as PNG (fast to compress, so the large
/// fixtures stay cheap).
fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
    let image = if alpha {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([200, 30, 30, 128]),
        ))
    } else {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([30, 60, 90]),
        ))
    };
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
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

#[test]
fn a_4000x3000_png_shrinks_and_keeps_its_aspect() {
    let store = tempfile::tempdir().unwrap();
    let original = png(4000, 3000, false);
    let (bytes, mime) = normalise(store.path(), "big", "image/png", &original);
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
    let (bytes, mime) = normalise(store.path(), "alpha", "image/png", &original);
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
    let (bytes, mime) = normalise(store.path(), "small", "image/png", &original);
    assert_eq!(bytes, original, "identical dimensions: no re-encode");
    assert_eq!(mime, "image/png", "the source MIME is preserved");
    // Nothing was written next to the original.
    assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
}

#[test]
fn the_encoded_variant_is_cached_and_reused() {
    let store = tempfile::tempdir().unwrap();
    let original = png(4000, 3000, false);
    let first = normalise(store.path(), "hash-cache", "image/png", &original);
    let cached: Vec<_> = std::fs::read_dir(store.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(cached.len(), 1, "exactly one variant is written");
    let cached_path = cached[0].clone();
    assert_eq!(
        cached_path.file_name().unwrap().to_str().unwrap(),
        "hash-cache.n1512x1134.jpg"
    );
    assert_eq!(std::fs::read(&cached_path).unwrap(), first.0);

    // The second call reads the cache instead of re-encoding: a sentinel
    // payload the encoder could never produce comes back verbatim.
    std::fs::write(&cached_path, b"sentinel-variant").unwrap();
    let second = normalise(store.path(), "hash-cache", "image/png", &original);
    assert_eq!(second.0, b"sentinel-variant");
    assert_eq!(second.1, first.1);

    // With the real variant back, a third call is byte-identical and leaves
    // the cache file untouched (same mtime, no rewrite).
    std::fs::write(&cached_path, &first.0).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let before = std::fs::metadata(&cached_path).unwrap().modified().unwrap();
    let third = normalise(store.path(), "hash-cache", "image/png", &original);
    assert_eq!(third, first);
    let after = std::fs::metadata(&cached_path).unwrap().modified().unwrap();
    assert_eq!(before, after, "a cache hit must not rewrite the variant");
    // The stored original is never mutated.
    assert!(std::fs::metadata(store.path().join("hash-cache")).is_err());
}

#[test]
fn a_zero_byte_cache_file_is_not_trusted() {
    let store = tempfile::tempdir().unwrap();
    let original = png(4000, 3000, false);
    std::fs::write(store.path().join("torn.n1512x1134.jpg"), b"").unwrap();
    let (bytes, mime) = normalise(store.path(), "torn", "image/png", &original);
    assert_eq!(mime, "image/jpeg");
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1512, 1134));
    assert!(
        !std::fs::read(store.path().join("torn.n1512x1134.jpg"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_undecodable_image_is_forwarded_unchanged() {
    // Unmeasurable is not the same as missing: the bytes go out untouched
    // (and no variant is cached) instead of being dropped from the request.
    let store = tempfile::tempdir().unwrap();
    let (bytes, mime) = normalise(store.path(), "junk", "image/png", b"not an image");
    assert_eq!(bytes, b"not an image");
    assert_eq!(mime, "image/png");
    assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
}

#[test]
fn the_oldest_images_are_offloaded_and_the_newest_stay() {
    let messages: Vec<Message> = (0..10)
        .map(|index| user_with(&format!("h{index}")))
        .collect();
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

#[test]
fn max_inline_zero_never_offloads() {
    let messages: Vec<Message> = (0..10)
        .map(|index| user_with(&format!("h{index}")))
        .collect();
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
