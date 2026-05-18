//! Bundled model registry loader.
//!
//! Loads `data/models.json` at compile time via `include_str!` and parses
//! it lazily into an in-memory `Registry` on first access. This is the
//! source of truth for both per-model pricing and per-model tier /
//! vendor / family identification — replacing the hardcoded tables in
//! `pricing.rs` and `model.rs` over the next phases.
//!
//! # What this PR does (and does not)
//!
//! This file is **purely additive** — the existing `pricing.rs::lookup`
//! and `model.rs::identify_*` functions still drive every existing
//! caller. The registry exposes a parallel lookup API but no caller
//! uses it yet. Migration happens in follow-up PRs (Phase 2b-2/3/4),
//! one consumer at a time, with full test coverage at each step.
//!
//! # Resolution order (matches `pricing.rs` semantics today)
//!
//! 1. **Special markers**: walk in declaration order. A free-tier
//!    suffix marker (`-free`) fires for ANY model name ending with it,
//!    regardless of vendor. This is critical: `claude-opus-4-7-free`
//!    via OpenRouter must resolve to $0 even though it'd otherwise
//!    match the Anthropic Opus tier.
//! 2. **Exact alias**: O(1) hash lookup against `match.exact_aliases`.
//! 3. **Substring match**: walk `models[]` in JSON declaration order,
//!    return first entry whose `match.substring_keys` ALL appear in
//!    the normalized model name. JSON authors are responsible for
//!    ordering most-specific-first (e.g. `gpt-5-codex` before `gpt-5`).
//! 4. **Miss**: return `None`. Callers fall back to
//!    [`fallback_tier_for`] using the heuristic.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::model::{ModelIdent, Tier};

// ─── Deserialization types ──────────────────────────────────────────

/// The top-level shape of `data/models.json`.
#[derive(Debug, Deserialize)]
pub struct RegistryFile {
    pub schema_version: u32,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub registry_url: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    pub fallback_tier_pricing: FallbackTierPricing,
    #[serde(default)]
    pub special_markers: Vec<SpecialMarker>,
    pub models: Vec<ModelOrSection>,
}

/// Per-million-token rates, one block per model entry.
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct PricingPer1m {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
    pub reasoning: f64,
}

/// Conservative fallback rates per tier — used when the heuristic
/// classifies a never-before-seen model into a tier but the registry
/// has no exact entry.
#[derive(Debug, Deserialize)]
pub struct FallbackTierPricing {
    #[serde(default)]
    pub comment: Option<String>,
    pub frontier: PricingPer1m,
    pub mid: PricingPer1m,
    pub mini: PricingPer1m,
    /// Reserved for future on-device / local models. Currently treated
    /// as `Mini` by [`for_tier`] until the `Tier::Local` variant lands.
    pub local: PricingPer1m,
}

impl FallbackTierPricing {
    /// Pick the right fallback bucket for a tier. `Unknown` resolves to
    /// `mid` (the heuristic's own default when no signal matched).
    pub fn for_tier(&self, tier: Tier) -> &PricingPer1m {
        match tier {
            Tier::Frontier => &self.frontier,
            Tier::Mid | Tier::Unknown => &self.mid,
            Tier::Mini => &self.mini,
        }
    }
}

/// Two shapes coexist in the `models[]` array: section divider stubs
/// (just `{ "_section": "── Anthropic Claude ──" }`) for human readers
/// and real entries. The untagged enum filters the dividers out during
/// parse so callers iterate only real models.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ModelOrSection {
    Model(ModelEntry),
    Section {
        #[serde(rename = "_section")]
        _section: String,
    },
}

/// One real model entry.
#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub vendor: String,
    pub family: String,
    /// Stored as a string ("frontier" / "mid" / "mini" / "local") rather
    /// than the `Tier` enum because `local` isn't a Tier variant yet —
    /// we map at lookup time via [`parse_tier_str`].
    pub tier: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(rename = "match")]
    pub match_spec: ModelMatchSpec,
    pub pricing_per_1m_usd: PricingPer1m,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub released: Option<String>,
    #[serde(default)]
    pub deprecated: bool,
    pub source: SourceBlock,
}

/// Match block for a real model entry.
#[derive(Debug, Deserialize)]
pub struct ModelMatchSpec {
    #[serde(default)]
    pub substring_keys: Vec<String>,
    #[serde(default)]
    pub exact_aliases: Vec<String>,
}

/// Special marker (e.g. `-free` suffix catch-all). Different match
/// shape from `ModelEntry` — kept as a separate type to keep both
/// shapes serde-honest and prevent half-populated entries.
#[derive(Debug, Deserialize)]
pub struct SpecialMarker {
    pub id: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(rename = "match")]
    pub match_spec: SpecialMatchSpec,
    pub tier: String,
    pub pricing_per_1m_usd: PricingPer1m,
    pub source: SourceBlock,
}

#[derive(Debug, Deserialize)]
pub struct SpecialMatchSpec {
    #[serde(default)]
    pub suffix: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SourceBlock {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default)]
    pub url: Option<String>,
    pub note: String,
}

// ─── Tier string parsing ────────────────────────────────────────────

/// `data/models.json` uses lowercase strings for tier; map to the
/// `Tier` enum. `"local"` collapses to `Mini` for now — when the
/// `Tier::Local` variant lands, change this mapping.
pub fn parse_tier_str(s: &str) -> Tier {
    match s {
        "frontier" => Tier::Frontier,
        "mid" => Tier::Mid,
        "mini" | "local" => Tier::Mini,
        _ => Tier::Unknown,
    }
}

// ─── Public registry handle ─────────────────────────────────────────

/// Loaded, indexed registry. Construct via [`Registry::bundled`] for
/// the embedded copy or [`Registry::from_json`] for a custom one.
pub struct Registry {
    /// Parsed JSON. Section dividers preserved here for transparency;
    /// the lookup paths skip them.
    pub data: RegistryFile,
    /// `exact_alias` → index into `data.models`. Built at construction
    /// time so per-event lookup is O(1) for known aliases.
    by_alias: HashMap<String, usize>,
}

/// One successfully-resolved registry hit. Unifies `ModelEntry` and
/// `SpecialMarker` so callers don't branch on which one matched.
#[derive(Debug, Clone)]
pub struct ResolvedEntry<'a> {
    pub pricing: &'a PricingPer1m,
    pub tier: Tier,
    pub vendor: Option<&'a str>,
    pub family: Option<&'a str>,
    /// `id` of the matched entry — `"claude-opus-4-7"` for models,
    /// `"_free_tier_suffix"` for special markers.
    pub source_id: &'a str,
    pub deprecated: bool,
    pub source: &'a SourceBlock,
}

impl Registry {
    /// Load the registry embedded in the binary at compile time.
    pub fn bundled() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            let json = include_str!("../../data/models.json");
            Self::from_json(json).expect("bundled data/models.json must parse")
        })
    }

    /// Parse a registry from a JSON string. Used by tests and by future
    /// remote-sync code that overrides the bundled copy.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let data: RegistryFile = serde_json::from_str(json)?;
        let mut by_alias = HashMap::new();
        for (i, entry) in data.models.iter().enumerate() {
            if let ModelOrSection::Model(m) = entry {
                for alias in &m.match_spec.exact_aliases {
                    by_alias.insert(alias.clone(), i);
                }
            }
        }
        Ok(Self { data, by_alias })
    }

    /// Iterator over real model entries (skips section dividers).
    pub fn models(&self) -> impl Iterator<Item = &ModelEntry> {
        self.data.models.iter().filter_map(|e| match e {
            ModelOrSection::Model(m) => Some(m),
            ModelOrSection::Section { .. } => None,
        })
    }

    /// Count of real models (excluding section dividers).
    pub fn model_count(&self) -> usize {
        self.models().count()
    }

    /// Lookup a model name. Resolution order documented in the module
    /// docstring. Returns `None` only when nothing matches — callers
    /// then fall back to [`FallbackTierPricing::for_tier`] using the
    /// heuristic tier from `crate::xp::heuristic::fallback_tier`.
    pub fn lookup(&self, model_str: &str) -> Option<ResolvedEntry<'_>> {
        // Normalize through the same pipeline ModelIdent uses so the
        // registry sees consistent input regardless of caller. This
        // strips vendor prefix, date suffix, dots-to-dashes.
        let normalized = ModelIdent::parse(model_str).model;

        // 1. Special markers (free-tier etc.) — must fire BEFORE vendor
        //    entries so `claude-opus-4-7-free` resolves to $0 not $5/$25.
        for marker in &self.data.special_markers {
            if let Some(suffix) = &marker.match_spec.suffix {
                if normalized.ends_with(suffix) {
                    return Some(ResolvedEntry {
                        pricing: &marker.pricing_per_1m_usd,
                        tier: parse_tier_str(&marker.tier),
                        vendor: None,
                        family: None,
                        source_id: &marker.id,
                        deprecated: false,
                        source: &marker.source,
                    });
                }
            }
        }

        // 2. Exact alias — O(1) hash hit.
        if let Some(&idx) = self.by_alias.get(&normalized) {
            if let ModelOrSection::Model(m) = &self.data.models[idx] {
                return Some(model_to_resolved(m));
            }
        }

        // 3. Substring match — walk in declaration order, first match
        //    wins (JSON authors ordered most-specific first).
        for entry in &self.data.models {
            if let ModelOrSection::Model(m) = entry {
                if !m.match_spec.substring_keys.is_empty()
                    && m.match_spec
                        .substring_keys
                        .iter()
                        .all(|k| normalized.contains(k))
                {
                    return Some(model_to_resolved(m));
                }
            }
        }

        None
    }
}

fn model_to_resolved(m: &ModelEntry) -> ResolvedEntry<'_> {
    ResolvedEntry {
        pricing: &m.pricing_per_1m_usd,
        tier: parse_tier_str(&m.tier),
        vendor: Some(&m.vendor),
        family: Some(&m.family),
        source_id: &m.id,
        deprecated: m.deprecated,
        source: &m.source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Parse smoke ────────────────────────────────────────────────

    #[test]
    fn bundled_registry_parses() {
        // Force lazy init; the OnceLock unwraps here, so any parse
        // error in data/models.json shows up as a test failure rather
        // than a runtime panic on first user event.
        let r = Registry::bundled();
        assert!(r.model_count() >= 40, "expected ~48 models, got {}", r.model_count());
        assert!(!r.data.special_markers.is_empty(), "free-tier marker must exist");
    }

    #[test]
    fn schema_version_pinned() {
        let r = Registry::bundled();
        assert_eq!(r.data.schema_version, 1);
    }

    #[test]
    fn section_dividers_filtered_from_iteration() {
        let r = Registry::bundled();
        // Total entries in JSON array > model count (because of section
        // divider stubs being included in `data.models` but skipped by
        // the iterator).
        assert!(r.data.models.len() > r.model_count());
    }

    // ─── Resolution: special markers ────────────────────────────────

    #[test]
    fn free_suffix_resolves_to_zero_for_any_vendor() {
        let r = Registry::bundled();
        for model in [
            "claude-opus-4-7-free",
            "deepseek-v4-flash-free",
            "qwen3.6-plus-free",
            "openrouter/llama-3.3-70b-instruct-free",
            "gpt-5-nano-free",
        ] {
            let hit = r.lookup(model).unwrap_or_else(|| panic!("no hit for {model}"));
            assert_eq!(hit.pricing.input, 0.0, "{model} input should be free");
            assert_eq!(hit.pricing.output, 0.0, "{model} output should be free");
            assert_eq!(hit.source_id, "_free_tier_suffix");
        }
    }

    #[test]
    fn free_marker_takes_precedence_over_vendor_entry() {
        let r = Registry::bundled();
        // claude-opus-4-7 alone → Anthropic Opus 4.7 pricing ($5/$25)
        let paid = r.lookup("claude-opus-4-7").unwrap();
        assert_eq!(paid.pricing.input, 5.00);

        // claude-opus-4-7-free → free marker pricing ($0)
        let free = r.lookup("claude-opus-4-7-free").unwrap();
        assert_eq!(free.pricing.input, 0.00);
    }

    // ─── Resolution: exact alias ────────────────────────────────────

    #[test]
    fn exact_alias_anthropic_opus_4_7() {
        let r = Registry::bundled();
        let hit = r.lookup("claude-opus-4-7").unwrap();
        assert_eq!(hit.source_id, "claude-opus-4-7");
        assert_eq!(hit.pricing.input, 5.00);
        assert_eq!(hit.pricing.output, 25.00);
        assert_eq!(hit.pricing.cache_read, 0.50);
        assert_eq!(hit.pricing.cache_creation, 6.25);
        assert_eq!(hit.tier, Tier::Frontier);
        assert_eq!(hit.vendor, Some("anthropic"));
        assert_eq!(hit.family, Some("claude-opus"));
    }

    #[test]
    fn vendor_prefix_normalized_then_matched() {
        let r = Registry::bundled();
        let hit = r.lookup("anthropic/claude-opus-4-7").unwrap();
        assert_eq!(hit.source_id, "claude-opus-4-7");
        assert_eq!(hit.pricing.input, 5.00);
    }

    #[test]
    fn dated_snapshot_resolves_to_base() {
        let r = Registry::bundled();
        // ModelIdent::parse strips the trailing -YYYYMMDD.
        let hit = r.lookup("claude-haiku-4-5-20251001").unwrap();
        assert_eq!(hit.source_id, "claude-haiku-4-5");
        assert_eq!(hit.tier, Tier::Mini);
    }

    // ─── Resolution: substring specificity order ────────────────────

    #[test]
    fn gpt5_nano_specific_beats_gpt5_generic() {
        // `gpt-5-nano` must resolve to the nano entry, not the GPT-5
        // catch-all. Specificity order in JSON puts nano first.
        let r = Registry::bundled();
        let nano = r.lookup("gpt-5-nano").unwrap();
        assert_eq!(nano.source_id, "gpt-5-nano");
        assert_eq!(nano.pricing.input, 0.05);
        assert_eq!(nano.tier, Tier::Mini);

        let base = r.lookup("gpt-5").unwrap();
        assert_eq!(base.source_id, "gpt-5");
        assert_eq!(base.pricing.input, 1.25);
        assert_eq!(base.tier, Tier::Frontier);
    }

    #[test]
    fn opus_4_7_specific_beats_generic_opus() {
        // Critical invariant from the CodexBar reconciliation: opus-4-7
        // must NOT fall through to the generic Opus tier ($15/$75).
        let r = Registry::bundled();
        let specific = r.lookup("claude-opus-4-7").unwrap();
        let generic = r.lookup("claude-opus-4-5").unwrap();
        assert_ne!(specific.pricing, generic.pricing);
        assert_eq!(specific.pricing.cache_read, 0.50);
        assert_eq!(generic.pricing.cache_read, 1.50);
    }

    #[test]
    fn flash_lite_specific_beats_flash() {
        let r = Registry::bundled();
        let lite = r.lookup("gemini-2.5-flash-lite").unwrap();
        let regular = r.lookup("gemini-2.5-flash").unwrap();
        assert_eq!(lite.source_id, "gemini-2-5-flash-lite");
        assert_eq!(regular.source_id, "gemini-2-5-flash");
        assert!(lite.pricing.input < regular.pricing.input);
    }

    #[test]
    fn o3_mini_specific_beats_o3() {
        let r = Registry::bundled();
        let mini = r.lookup("o3-mini").unwrap();
        let base = r.lookup("o3").unwrap();
        assert_eq!(mini.source_id, "o3-mini");
        assert_eq!(base.source_id, "o3");
        assert_eq!(mini.tier, Tier::Mini);
        assert_eq!(base.tier, Tier::Frontier);
    }

    // ─── Resolution: unknown ────────────────────────────────────────

    #[test]
    fn unknown_model_returns_none() {
        let r = Registry::bundled();
        assert!(r.lookup("future-model-99x").is_none());
        assert!(r.lookup("totally-bogus-thing").is_none());
    }

    #[test]
    fn empty_string_does_not_panic() {
        let r = Registry::bundled();
        assert!(r.lookup("").is_none());
    }

    // ─── Fallback tier pricing accessor ─────────────────────────────

    #[test]
    fn fallback_tier_lookup_works_for_every_variant() {
        let r = Registry::bundled();
        let f = &r.data.fallback_tier_pricing;
        // Just confirm each tier has a non-degenerate bucket.
        assert!(f.for_tier(Tier::Frontier).output > 0.0);
        assert!(f.for_tier(Tier::Mid).output > 0.0);
        assert!(f.for_tier(Tier::Mini).output > 0.0);
        // Unknown → Mid bucket.
        assert_eq!(f.for_tier(Tier::Unknown).input, f.for_tier(Tier::Mid).input);
    }

    // ─── parse_tier_str ─────────────────────────────────────────────

    #[test]
    fn parse_tier_str_handles_all_cases() {
        assert_eq!(parse_tier_str("frontier"), Tier::Frontier);
        assert_eq!(parse_tier_str("mid"), Tier::Mid);
        assert_eq!(parse_tier_str("mini"), Tier::Mini);
        // Local collapses to Mini for now (until Tier::Local lands).
        assert_eq!(parse_tier_str("local"), Tier::Mini);
        assert_eq!(parse_tier_str("garbage"), Tier::Unknown);
    }

    // ─── Compat with existing pricing.rs anchors ────────────────────
    //
    // These tests pin "registry returns the same numbers pricing.rs
    // returns today" for every input pricing.rs has a test for.
    // Phase 2b-2 will migrate pricing.rs to call into the registry;
    // these tests stay green through that change.

    #[test]
    fn compat_opus_4_7_rates_match_pricing_rs() {
        let r = Registry::bundled();
        let p = r.lookup("claude-opus-4-7").unwrap().pricing;
        assert_eq!(p.input, 5.00);
        assert_eq!(p.output, 25.00);
        assert_eq!(p.cache_read, 0.50);
        assert_eq!(p.cache_creation, 6.25);
    }

    #[test]
    fn compat_sonnet_dated_resolves() {
        let r = Registry::bundled();
        // pricing.rs::tests::sonnet_dated_variant_resolves expects:
        //   input=3.00, cache_read=0.30
        let p = r
            .lookup("claude-sonnet-4-5-20250929")
            .unwrap_or_else(|| panic!("sonnet dated must resolve"))
            .pricing;
        assert_eq!(p.input, 3.00);
        assert_eq!(p.cache_read, 0.30);
    }

    #[test]
    fn compat_haiku_dated_resolves() {
        let r = Registry::bundled();
        let p = r.lookup("claude-haiku-4-5-20251001").unwrap().pricing;
        assert_eq!(p.input, 1.00);
        assert_eq!(p.output, 5.00);
    }

    /// CodexBar reconciliation invariant — mirrors
    /// `pricing.rs::tests::five_sixteen_local_aggregate_matches_codexbar_within_1pct`.
    /// If registry rates ever drift from the CodexBar-validated numbers,
    /// daily aggregates will diverge.
    #[test]
    fn compat_five_sixteen_codexbar_within_1pct() {
        let r = Registry::bundled();
        let p = r.lookup("claude-opus-4-7").unwrap().pricing;
        // Exact petpet aggregate for 5/16 LOCAL UTC+8 opus-4-7.
        let input = 9_394_f64;
        let output = 981_958_f64;
        let cache_read = 392_559_814_f64;
        let cache_creation = 3_385_391_f64;
        let cost = input * p.input / 1_000_000.0
            + output * p.output / 1_000_000.0
            + cache_read * p.cache_read / 1_000_000.0
            + cache_creation * p.cache_creation / 1_000_000.0;
        let codexbar_reported = 241.89_f64;
        let pct_diff = (cost - codexbar_reported).abs() / codexbar_reported * 100.0;
        assert!(
            pct_diff < 1.0,
            "registry-derived ${cost:.2} vs CodexBar's ${codexbar_reported} differs by {pct_diff:.2}%"
        );
    }
}
