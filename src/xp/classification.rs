//! Decide a model's `(Tier, Confidence)` pair from a parsed
//! [`ModelIdent`].
//!
//! Single source of truth for "how confident are we that this is the
//! right tier?" Used by both [`scorer::usage`] (for XP computation)
//! and the dashboard (for the per-model tier / "guessed" badge).
//! Keeping it in one place ensures the UI never shows a different
//! classification from what the scorer actually applied.
//!
//! # Decision tree
//!
//! ```text
//!   Registry knows the model (data/models.json exact / substring hit)
//!   ──────────────────────────────► (ident.tier, Confidence::Exact)
//!
//!   Registry doesn't know but ident.tier is non-Unknown
//!   (i.e. model.rs's hardcoded family-table fallback fired)
//!   ──────────────────────────────► (ident.tier, Confidence::Heuristic)
//!
//!   Registry doesn't know AND model.rs gave Unknown:
//!   try `heuristic::fallback_tier` (keyword scan)
//!     • Confident(Tier) ─────────► (tier,       Confidence::Heuristic)
//!     • Default         ─────────► (Tier::Mid,  Confidence::Unknown)
//! ```
//!
//! # Why model.rs's hardcoded fallback only earns Heuristic
//!
//! Phase 2b-3 left a transitional hardcoded family table in
//! `model.rs::identify_tier` so existing pets keep classifying
//! never-seen models the same way they did pre-migration. That
//! table is a heuristic, not authoritative — only the Registry
//! (data/models.json + future remote sync) qualifies for
//! `Confidence::Exact`. Treating the hardcoded fallback as Exact
//! would inflate XP for any unrecognised-but-family-matched model
//! (e.g. a hypothetical `claude-haiku-9-future`) and hide the
//! "guessed" badge that signals "ping the registry to add this".

use crate::model::{ModelIdent, Tier};
use crate::xp::algorithm::Confidence;
use crate::xp::heuristic::{fallback_tier, FallbackResult};
use crate::xp::registry::Registry;

/// Classify a parsed model identifier into a `(Tier, Confidence)`
/// pair. See module docs for the full decision tree.
///
/// Note: looks up the bundled (and cache-augmented) Registry every
/// call. Lookup is O(1) hash + small substring walk — cheap enough to
/// call per event without caching at the call site.
pub fn classify(ident: &ModelIdent) -> (Tier, Confidence) {
    // The registry's lookup itself normalises the input through
    // `crate::model::normalize`, so passing the raw string is fine
    // (and is what live event scoring receives anyway).
    if Registry::bundled().lookup(&ident.raw).is_some() {
        return (ident.tier, Confidence::Exact);
    }
    if ident.tier != Tier::Unknown {
        return (ident.tier, Confidence::Heuristic);
    }
    match fallback_tier(&ident.model) {
        FallbackResult::Confident(t) => (t, Confidence::Heuristic),
        FallbackResult::Default => (Tier::Mid, Confidence::Unknown),
    }
}

/// Confidence rendered as the lowercase string the dashboard /
/// frontend consumes. Stable across the codebase so React `keys`,
/// CSS classes (`.confidence-exact` etc.), and any analytics
/// dashboards agree.
pub fn confidence_as_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "exact",
        Confidence::Heuristic => "heuristic",
        Confidence::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(raw: &str) -> ModelIdent {
        ModelIdent::parse(raw)
    }

    #[test]
    fn registry_hit_is_exact() {
        // claude-opus-4-7 is in the bundled registry.
        let (tier, conf) = classify(&parsed("claude-opus-4-7"));
        assert_eq!(tier, Tier::Frontier);
        assert_eq!(conf, Confidence::Exact);
    }

    #[test]
    fn registry_hit_via_substring_is_still_exact() {
        // claude-opus-4-99 not in registry as an exact alias, but the
        // `opus-4` substring entry catches it.
        let (tier, conf) = classify(&parsed("claude-opus-4-99"));
        assert_eq!(tier, Tier::Frontier);
        assert_eq!(conf, Confidence::Exact);
    }

    #[test]
    fn registry_miss_with_hardcoded_family_match_is_heuristic() {
        // claude-haiku-9-future: not in registry. `identify_family`
        // matches the "claude-haiku" family table entry in model.rs,
        // so `identify_tier`'s hardcoded fallback returns Mini.
        // We treat that as a Heuristic match — it's the hardcoded
        // family table, not the registry.
        let ident = parsed("claude-haiku-9-future");
        // Sanity: ensure the hardcoded fallback fired (registry miss).
        assert!(Registry::bundled().lookup(&ident.raw).is_none());
        assert_eq!(ident.tier, Tier::Mini);
        let (tier, conf) = classify(&ident);
        assert_eq!(tier, Tier::Mini);
        assert_eq!(conf, Confidence::Heuristic);
    }

    #[test]
    fn registry_miss_with_hardcoded_opus_match_is_heuristic() {
        // claude-opus-99-superalign: registry doesn't know, but
        // hardcoded family table classifies as Frontier via
        // `family.starts_with("claude-opus")`. Must be Heuristic.
        let ident = parsed("claude-opus-99-superalign");
        assert!(Registry::bundled().lookup(&ident.raw).is_none());
        assert_eq!(ident.tier, Tier::Frontier);
        let (tier, conf) = classify(&ident);
        assert_eq!(tier, Tier::Frontier);
        assert_eq!(conf, Confidence::Heuristic);
    }

    #[test]
    fn registry_miss_with_keyword_only_is_heuristic() {
        // future-model-nano: registry miss + model.rs family table
        // doesn't know "future-model" family → ident.tier = Unknown.
        // heuristic.rs::fallback_tier catches "nano" segment → Mini.
        // Caller sees Heuristic confidence.
        let ident = parsed("future-model-nano");
        // model.rs's identify_tier short-circuits on "nano" segment,
        // so ident.tier IS set even though the registry doesn't know.
        // The point of this test: even when model.rs catches via a
        // keyword (vs a family match), classify() still returns
        // Heuristic — Registry is the only Exact source.
        let (tier, conf) = classify(&ident);
        assert_eq!(tier, Tier::Mini);
        assert_eq!(conf, Confidence::Heuristic);
    }

    #[test]
    fn no_signal_anywhere_is_unknown() {
        // zephyr-7000: registry miss, no family match in model.rs,
        // no keyword in heuristic.rs. Default to Mid + Unknown so
        // the pet still grows (just at 0.4× confidence factor).
        let ident = parsed("zephyr-7000");
        assert_eq!(ident.tier, Tier::Unknown);
        let (tier, conf) = classify(&ident);
        assert_eq!(tier, Tier::Mid);
        assert_eq!(conf, Confidence::Unknown);
    }

    #[test]
    fn confidence_as_str_round_trip() {
        // String form is the cross-layer contract — frontend CSS
        // classes (`.confidence-exact`) and analytics dashboards both
        // depend on these literal lowercase values.
        assert_eq!(confidence_as_str(Confidence::Exact), "exact");
        assert_eq!(confidence_as_str(Confidence::Heuristic), "heuristic");
        assert_eq!(confidence_as_str(Confidence::Unknown), "unknown");
    }

    #[test]
    fn free_tier_suffix_resolves_via_registry_marker() {
        // -free fires the registry's special marker, which is still
        // a registry hit → Confidence::Exact (we know exactly what
        // a -free model costs and what tier it's classified at).
        let ident = parsed("claude-opus-4-7-free");
        let (tier, conf) = classify(&ident);
        assert_eq!(tier, Tier::Mini); // free marker's tier
        assert_eq!(conf, Confidence::Exact);
    }
}
