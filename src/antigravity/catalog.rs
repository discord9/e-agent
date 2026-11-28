//! The Antigravity plan's fixed model catalog.
//!
//! The wire's model-list response for this account carries 37 entries. The
//! table below is generated from that capture: every id, display name,
//! family, tier, `maxTokens` (`context_window`), and `maxOutputTokens`
//! (`max_output_tokens`) comes from the wire; no number is guessed. A
//! profile's `model` is validated against the table while the config is
//! resolved: a typo fails there, naming the model and the closest family's
//! ids, instead of surviving until the first request's 404.
//!
//! Inclusion policy: 37 wire entries -> 34 chat models here.
//! - `tab_flash_lite_preview` and `tab_jump_flash_lite_preview` are the CLI's
//!   own tab-completion UI previews (16,384-token context, 4,096 output), not
//!   selectable chat models, so they are [`EXCLUDED`].
//! - `gemini-3.1-flash-image` is an image-GENERATION model (65,536 context,
//!   65,536 output, no `supportsThinking`, not `recommended`). This adapter
//!   only builds text and function parts and only reads text/function
//!   responses, so it cannot use the model; it is [`EXCLUDED`]. Supporting it
//!   needs an image-output path, not a chat catalog entry.
//! - Everything else is a chat model and is included.
//!
//! Two ids carry no `displayName` on the wire (`gemini-4-hydrogen-sonar` and
//! `gemini-3.8-flash-tiered`); the id stands in rather than inventing a label.
//! The wire reports the same label (`Gemini 3.5 Flash Lite`) for the
//! `gemini-2.5-flash*` aliases; the catalog keeps it verbatim.
//!
//! Every included model reports `supportsImages: true` except
//! `gpt-oss-120b-medium`, which omits the field (recorded as false). The
//! adapter now sends image input as `inlineData` parts, so `supports_vision`
//! is the MODEL's wire capability: true exactly where `supportsImages` is
//! true, and false for `gpt-oss-120b-medium`. A profile may still narrow this
//! with `vision = false`, but it cannot claim a model the wire says is
//! text-only.
//!
//! `recommended` is the wire flag. Every included model has it set, so it
//! does not single one out; the default below is still one of them.

use anyhow::{Result, bail};

/// The model an Antigravity profile uses when it omits `model`.
///
/// `gemini-4-neon-high` carries `recommended: true` on the wire (as does
/// every included model, so the flag alone does not choose it). It is the top
/// reasoning tier of the primary Gemini 4 Neon family, and it does not depend
/// on the flash-only `-tiered` 404 fallback.
pub(crate) const DEFAULT_MODEL: &str = "gemini-4-neon-high";

/// The reasoning tier an id carries, when its suffix names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Low,
    Medium,
    High,
    XHigh,
}

impl Tier {
    /// The id suffix for this tier (`-low`, `-medium`, `-high`, `-xhigh`).
    #[allow(dead_code)] // table metadata: the catalog tests assert every row's tier
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Tier::Low => "-low",
            Tier::Medium => "-medium",
            Tier::High => "-high",
            Tier::XHigh => "-xhigh",
        }
    }
}

/// One pinned model id. Some fields are metadata that nothing on the agent
/// path consumes yet; the catalog tests assert all of them so a regeneration
/// cannot silently lose one.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogModel {
    /// Wire id sent as the request's `model`.
    pub id: &'static str,
    /// `displayName` from the wire; the id when the wire has no label.
    pub display_name: &'static str,
    /// The ids sharing this model family. A trailing tier is stripped; a
    /// named variant (`-sonar`, `-tiered`, `-thinking`, `-lite`) joins its
    /// base family when the capture has one, otherwise it is its own family.
    pub family: &'static str,
    /// The reasoning tier the id carries, if any.
    pub tier: Option<Tier>,
    /// `maxTokens` from the wire, in tokens.
    pub context_window: u64,
    /// `maxOutputTokens` from the wire, in tokens.
    pub max_output_tokens: u64,
    /// `supportsImages` from the wire (absent = false): the MODEL accepts
    /// image input.
    pub supports_images: bool,
    /// This ADAPTER's answer: equal to [`Self::supports_images`]. The request
    /// builder sends `inlineData` parts, so a model that accepts images is
    /// vision-capable here too; `vision = false` in the profile can narrow it.
    pub supports_vision: bool,
    /// `recommended` from the wire.
    pub recommended: bool,
}

/// The 34 chat models from the 37-entry capture, grouped by family.
pub(crate) const MODELS: &[CatalogModel] = &[
    CatalogModel {
        id: "gemini-4-neon-high",
        display_name: "Gemini 4 Neon (High)",
        family: "gemini-4-neon",
        tier: Some(Tier::High),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-neon-xhigh",
        display_name: "Gemini 4 Neon (XHigh)",
        family: "gemini-4-neon",
        tier: Some(Tier::XHigh),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-neon-medium",
        display_name: "Gemini 4 Neon (Medium)",
        family: "gemini-4-neon",
        tier: Some(Tier::Medium),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-neon-low",
        display_name: "Gemini 4 Neon (Low)",
        family: "gemini-4-neon",
        tier: Some(Tier::Low),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-carbon-high",
        display_name: "Gemini 4 Carbon (High)",
        family: "gemini-4-carbon",
        tier: Some(Tier::High),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-carbon-medium",
        display_name: "Gemini 4 Carbon (Medium)",
        family: "gemini-4-carbon",
        tier: Some(Tier::Medium),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-carbon-low",
        display_name: "Gemini 4 Carbon (Low)",
        family: "gemini-4-carbon",
        tier: Some(Tier::Low),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-argon-high",
        display_name: "Gemini 4 Argon (High)",
        family: "gemini-4-argon",
        tier: Some(Tier::High),
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-argon-medium",
        display_name: "Gemini 4 Argon (Medium)",
        family: "gemini-4-argon",
        tier: Some(Tier::Medium),
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-argon-low",
        display_name: "Gemini 4 Argon (Low)",
        family: "gemini-4-argon",
        tier: Some(Tier::Low),
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-high",
        display_name: "Gemini 4 Hydrogen (High)",
        family: "gemini-4-hydrogen",
        tier: Some(Tier::High),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-xhigh",
        display_name: "Gemini 4 Hydrogen (XHigh)",
        family: "gemini-4-hydrogen",
        tier: Some(Tier::XHigh),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-medium",
        display_name: "Gemini 4 Hydrogen (Medium)",
        family: "gemini-4-hydrogen",
        tier: Some(Tier::Medium),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-low",
        display_name: "Gemini 4 Hydrogen (Low)",
        family: "gemini-4-hydrogen",
        tier: Some(Tier::Low),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-sonar",
        // No displayName on the wire: use the id rather than invent one.
        display_name: "gemini-4-hydrogen-sonar",
        family: "gemini-4-hydrogen",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-tiered-high",
        display_name: "Gemini 4 Hydrogen (High)",
        family: "gemini-4-hydrogen",
        tier: Some(Tier::High),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-4-hydrogen-tiered-xhigh",
        display_name: "Gemini 4 Hydrogen (XHigh)",
        family: "gemini-4-hydrogen",
        tier: Some(Tier::XHigh),
        context_window: 1_048_576,
        max_output_tokens: 262_144,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3.8-flash-high",
        display_name: "Gemini 3.8 Flash (High)",
        family: "gemini-3.8-flash",
        tier: Some(Tier::High),
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3.8-flash-medium",
        display_name: "Gemini 3.8 Flash (Medium)",
        family: "gemini-3.8-flash",
        tier: Some(Tier::Medium),
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3.8-flash-low",
        display_name: "Gemini 3.8 Flash (Low)",
        family: "gemini-3.8-flash",
        tier: Some(Tier::Low),
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3.8-flash-tiered",
        // No displayName on the wire: use the id rather than invent one.
        display_name: "gemini-3.8-flash-tiered",
        family: "gemini-3.8-flash",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3.5-flash-lite",
        display_name: "Gemini 3.5 Flash Lite",
        family: "gemini-3.5-flash-lite",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_535,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3.1-flash-lite",
        display_name: "Gemini 3.1 Flash Lite",
        family: "gemini-3.1-flash-lite",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_535,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-3-flash",
        display_name: "Gemini 3 Flash",
        family: "gemini-3-flash",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-2.5-flash",
        display_name: "Gemini 3.5 Flash Lite",
        family: "gemini-2.5-flash",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_535,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-2.5-flash-lite",
        display_name: "Gemini 3.5 Flash Lite",
        family: "gemini-2.5-flash",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_535,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gemini-2.5-flash-thinking",
        display_name: "Gemini 3.5 Flash Lite",
        family: "gemini-2.5-flash",
        tier: None,
        context_window: 1_048_576,
        max_output_tokens: 65_535,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "claude-opus-5-5-high",
        display_name: "Claude Opus 5.5 (High)",
        family: "claude-opus-5-5",
        tier: Some(Tier::High),
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "claude-opus-5-5-medium",
        display_name: "Claude Opus 5.5 (Medium)",
        family: "claude-opus-5-5",
        tier: Some(Tier::Medium),
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "claude-opus-5-5-low",
        display_name: "Claude Opus 5.5 (Low)",
        family: "claude-opus-5-5",
        tier: Some(Tier::Low),
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "claude-sonnet-5-5-high",
        display_name: "Claude Sonnet 5.5 (High)",
        family: "claude-sonnet-5-5",
        tier: Some(Tier::High),
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "claude-sonnet-5-5-medium",
        display_name: "Claude Sonnet 5.5 (Medium)",
        family: "claude-sonnet-5-5",
        tier: Some(Tier::Medium),
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "claude-sonnet-5-5-low",
        display_name: "Claude Sonnet 5.5 (Low)",
        family: "claude-sonnet-5-5",
        tier: Some(Tier::Low),
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_images: true,
        supports_vision: true,
        recommended: true,
    },
    CatalogModel {
        id: "gpt-oss-120b-medium",
        display_name: "GPT-OSS 120B (Medium)",
        family: "gpt-oss-120b",
        tier: Some(Tier::Medium),
        context_window: 131_072,
        max_output_tokens: 32_768,
        supports_images: false,
        supports_vision: false,
        recommended: true,
    },
];

/// Wire entries deliberately not offered as chat models, with the reason
/// [`validate`] reports.
pub(crate) const EXCLUDED: &[(&str, &str)] = &[
    (
        "tab_flash_lite_preview",
        "UI preview entry (16,384-token context), not a selectable chat model",
    ),
    (
        "tab_jump_flash_lite_preview",
        "UI preview entry (16,384-token context), not a selectable chat model",
    ),
    (
        "gemini-3.1-flash-image",
        "image-generation model; this text-only adapter cannot use its image output",
    ),
];

/// Look up one pinned id.
pub(crate) fn find(id: &str) -> Option<&'static CatalogModel> {
    MODELS.iter().find(|model| model.id == id)
}

/// Reject an id that is not in the catalog. An excluded id reports its
/// exclusion reason; an unknown id names the closest family's ids.
pub(crate) fn validate(id: &str) -> Result<()> {
    if find(id).is_some() {
        return Ok(());
    }
    if let Some((_, reason)) = EXCLUDED.iter().find(|(excluded, _)| *excluded == id) {
        bail!("Antigravity model `{id}` is excluded from the catalog: {reason}");
    }
    match closest_family(id) {
        Some(family) => {
            let ids = MODELS
                .iter()
                .filter(|model| model.family == family)
                .map(|model| model.id)
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "unknown Antigravity model `{id}`; the fixed catalog's closest family `{family}` serves: {ids}"
            )
        }
        None => bail!(
            "unknown Antigravity model `{id}`; the fixed catalog's families are: {}",
            family_names().join(", ")
        ),
    }
}

/// The family whose name shares the longest prefix with `id` (its tier suffix
/// stripped first). `gemini-4-neno-high` is one edit from `gemini-4-neon`; an
/// unrelated id gets no suggestion and the error lists the families instead.
fn closest_family(id: &str) -> Option<&'static str> {
    let base = strip_tier(id);
    let mut best: Option<(&'static str, usize)> = None;
    for model in MODELS {
        let prefix = base
            .chars()
            .zip(model.family.chars())
            .take_while(|(left, right)| left == right)
            .count();
        if best.is_none_or(|(_, length)| prefix > length) {
            best = Some((model.family, prefix));
        }
    }
    best.filter(|(_, prefix)| *prefix >= 4)
        .map(|(family, _)| family)
}

/// `id` without a reasoning-tier suffix, if it carries one.
fn strip_tier(id: &str) -> &str {
    for suffix in ["-xhigh", "-high", "-medium", "-low"] {
        if let Some(base) = id.strip_suffix(suffix) {
            return base;
        }
    }
    id
}

/// Family names in catalog order, deduplicated.
fn family_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    for model in MODELS {
        if !names.contains(&model.family) {
            names.push(model.family);
        }
    }
    names
}
