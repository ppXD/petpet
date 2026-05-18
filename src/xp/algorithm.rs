//! The XP formula — invariant, deterministic, NOT user-customizable.
//!
//! # Why this lives in its own file
//!
//! Cross-pet comparability (leaderboards, achievements, shared screenshots)
//! requires that every petpet install computes XP identically given the same
//! `(tokens, model, pet_level)` triple. The formula here is the only XP source
//! of truth — `pricing.rs` is for USD display only and never feeds back into
//! XP. Any change here breaks cross-pet comparability silently, so the file
//! is intentionally small and the constants are wrapped in anchor tests that
//! pin the output for canonical inputs.
//!
//! # The formula
//!
//! ```text
//! weighted = input         × 1.00
//!          + output        × 5.00
//!          + reasoning     × 5.00     // OpenAI o-series internal thinking
//!          + cache_creation× 1.25
//!          + cache_read    × 0.10
//!
//! raw      = weighted
//!          ÷ 1000                     // TOKEN_DIVISOR
//!          × tier_multiplier(tier)    // {Frontier:1.5, Mid:1.0, Mini:0.5}
//!          × confidence.factor()      // {Exact:1.0, Heuristic:0.7, Unknown:0.4}
//!          × growth_curve(pet_level)  // 1 / (1 + 0.02 × level)
//!
//! xp       = clamp(round(raw), 1, tier_xp_cap(tier))   when weighted > 0
//!          = 0                                          when weighted == 0
//! ```
//!
//! # Design choices, justified
//!
//! ## Token weights (1.0 / 5.0 / 5.0 / 1.25 / 0.1)
//!
//! Mirror Anthropic's billing ratio (output = 5× input, cache_creation =
//! 1.25× input, cache_read = 0.1× input). OpenAI's pricing for the same
//! axes happens to follow nearly identical ratios, so the formula behaves
//! provider-agnostically: switching between Claude / GPT / Gemini doesn't
//! change the relative XP per token type. Reasoning tokens are billed
//! like output by OpenAI (o-series); we mirror.
//!
//! ## TOKEN_DIVISOR = 1000
//!
//! Calibrates "1000 weighted tokens ≈ 1 XP base unit". Anchored against
//! real session sizes: a single small `PreToolUse` (~500 tokens) → 0.5–2
//! XP, a mid-sized tool response (~5K) → 5–30 XP, a long reasoning chain
//! (~30K) → 30–150 XP, full session (~100K) → 100–500 XP. Empirical, but
//! once shipped this divisor is invariant — change it and everyone's
//! pets reset.
//!
//! ## Tier multipliers (1.5 / 1.0 / 0.5)
//!
//! Tier is a *stable* proxy for "compute / capability cost" — Anthropic
//! can re-price Opus tomorrow and our Frontier multiplier doesn't move.
//! USD-anchored multipliers would jump on every vendor price change.
//!
//! ## Confidence factor (1.0 / 0.7 / 0.4)
//!
//! New models flow through pet without our registry knowing them yet.
//! Heuristic name-matching gives a tier guess (Mini / Mid / Frontier)
//! but we discount the XP by 0.3 to signal "this is a guess". If even
//! the heuristic can't tell (defaults to Mid), we discount further to
//! 0.4 — still nonzero so the pet still grows, but the user sees less
//! XP than for known models, which incentivises updating the registry.
//!
//! ## Growth curve `1 / (1 + 0.02 × level)`
//!
//! MMORPG-style diminishing returns. At level 0 the curve is 1.0; level
//! 25 → 0.67; level 50 → 0.50; level 100 → 0.33. Prevents whale users
//! ($1000/day API spend) from instantly maxing out, while still giving
//! continuous progress (never zero).
//!
//! ## Tier XP caps (500 / 200 / 80)
//!
//! Per-event ceiling that prevents one anomalous event from dumping a
//! massive XP boost. Real-world outliers: hook payload parsing bugs
//! occasionally report 100M+ token counts. Without this cap, one bad
//! event would saturate the pet for hours. With it, the worst case is
//! `cap × events_per_session` — bounded.

use crate::event::TokenDelta;
use crate::model::Tier;

// ─── Constants — DO NOT CHANGE without bumping a major version ──────

/// Token-axis weights, mirroring Anthropic's billing ratio.
pub const TOKEN_WEIGHT_INPUT: f64 = 1.00;
pub const TOKEN_WEIGHT_OUTPUT: f64 = 5.00;
pub const TOKEN_WEIGHT_REASONING: f64 = 5.00;
pub const TOKEN_WEIGHT_CACHE_CREATION: f64 = 1.25;
pub const TOKEN_WEIGHT_CACHE_READ: f64 = 0.10;

/// 1000 weighted tokens ≈ 1 XP base unit.
pub const TOKEN_DIVISOR: f64 = 1000.0;

/// Growth-curve slope. At level N, multiplier = 1 / (1 + GROWTH_K × N).
pub const GROWTH_K: f64 = 0.02;

// ─── Confidence: how sure are we about the model's tier ─────────────

/// Reflects how the model's tier was determined. Lower confidence →
/// lower XP, so users see weaker growth on never-before-seen models
/// and have an incentive to update the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// Model is in the registry / family table — tier is authoritative.
    Exact,
    /// Heuristic matched a Mini / Frontier keyword in the model name —
    /// tier is a strong guess but not registry-verified.
    Heuristic,
    /// Neither registry nor heuristic produced a tier signal — falling
    /// back to Mid as a best-effort default.
    Unknown,
}

impl Confidence {
    /// Multiplier applied to the raw XP.
    pub fn factor(self) -> f64 {
        match self {
            Confidence::Exact => 1.0,
            Confidence::Heuristic => 0.7,
            Confidence::Unknown => 0.4,
        }
    }
}

// ─── Tier helpers ───────────────────────────────────────────────────

/// Tier multiplier. `Tier::Unknown` is defensive — callers should have
/// resolved Unknown via the heuristic before reaching here. If we DO
/// see Unknown, treat as Mid (the heuristic's own default).
pub fn tier_multiplier(tier: Tier) -> f64 {
    match tier {
        Tier::Frontier => 1.5,
        Tier::Mid => 1.0,
        Tier::Mini => 0.5,
        Tier::Unknown => 1.0,
    }
}

/// Per-event XP ceiling by tier. Prevents one outlier event (parsing bug,
/// flood, malicious replay) from spiking the pet's level.
pub fn tier_xp_cap(tier: Tier) -> i64 {
    match tier {
        Tier::Frontier => 500,
        Tier::Mid => 200,
        Tier::Mini => 80,
        Tier::Unknown => 100,
    }
}

/// Diminishing-returns curve. Returns 1.0 at level 0, ≈0.5 at level 50,
/// ≈0.33 at level 100. Never zero so progress never fully stalls.
pub fn growth_curve(pet_level: u32) -> f64 {
    1.0 / (1.0 + GROWTH_K * pet_level as f64)
}

// ─── The formula ────────────────────────────────────────────────────

/// Compute base XP for a usage event.
///
/// Returns 0 when there are no tokens (no event registers). Otherwise
/// returns at least 1 XP — the pet should "react" to any real event,
/// even a tiny one.
pub fn compute_base_xp(
    tokens: &TokenDelta,
    tier: Tier,
    confidence: Confidence,
    pet_level: u32,
) -> i64 {
    let weighted = tokens.input as f64 * TOKEN_WEIGHT_INPUT
        + tokens.output as f64 * TOKEN_WEIGHT_OUTPUT
        + tokens.reasoning as f64 * TOKEN_WEIGHT_REASONING
        + tokens.cache_creation as f64 * TOKEN_WEIGHT_CACHE_CREATION
        + tokens.cache_read as f64 * TOKEN_WEIGHT_CACHE_READ;

    if weighted <= 0.0 {
        return 0;
    }

    let raw = (weighted / TOKEN_DIVISOR)
        * tier_multiplier(tier)
        * confidence.factor()
        * growth_curve(pet_level);

    let cap = tier_xp_cap(tier);
    let xp = raw.round() as i64;
    xp.clamp(1, cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: u64, output: u64, reasoning: u64, cc: u64, cr: u64) -> TokenDelta {
        TokenDelta {
            input,
            output,
            reasoning,
            cache_creation: cc,
            cache_read: cr,
        }
    }

    // ─── Constants invariant tests ──────────────────────────────────
    // These guard against accidental edits to the formula. If you
    // intentionally change a constant, update the anchor below AND
    // bump the algorithm major version (cross-pet comparability is
    // broken).

    #[test]
    fn token_weights_pinned() {
        assert_eq!(TOKEN_WEIGHT_INPUT, 1.00);
        assert_eq!(TOKEN_WEIGHT_OUTPUT, 5.00);
        assert_eq!(TOKEN_WEIGHT_REASONING, 5.00);
        assert_eq!(TOKEN_WEIGHT_CACHE_CREATION, 1.25);
        assert_eq!(TOKEN_WEIGHT_CACHE_READ, 0.10);
    }

    #[test]
    fn token_divisor_pinned() {
        assert_eq!(TOKEN_DIVISOR, 1000.0);
    }

    #[test]
    fn growth_k_pinned() {
        assert_eq!(GROWTH_K, 0.02);
    }

    #[test]
    fn confidence_factors_pinned() {
        assert_eq!(Confidence::Exact.factor(), 1.0);
        assert_eq!(Confidence::Heuristic.factor(), 0.7);
        assert_eq!(Confidence::Unknown.factor(), 0.4);
    }

    #[test]
    fn tier_multipliers_pinned() {
        assert_eq!(tier_multiplier(Tier::Frontier), 1.5);
        assert_eq!(tier_multiplier(Tier::Mid), 1.0);
        assert_eq!(tier_multiplier(Tier::Mini), 0.5);
        // Unknown defaults to Mid multiplier — heuristic should have
        // resolved this before reaching the scorer.
        assert_eq!(tier_multiplier(Tier::Unknown), 1.0);
    }

    #[test]
    fn tier_xp_caps_pinned() {
        assert_eq!(tier_xp_cap(Tier::Frontier), 500);
        assert_eq!(tier_xp_cap(Tier::Mid), 200);
        assert_eq!(tier_xp_cap(Tier::Mini), 80);
        assert_eq!(tier_xp_cap(Tier::Unknown), 100);
    }

    #[test]
    fn growth_curve_anchors() {
        // Pin specific levels — if these drift, level pacing changed.
        assert!((growth_curve(0) - 1.0).abs() < 1e-9);
        assert!((growth_curve(25) - (1.0 / 1.5)).abs() < 1e-9); // ≈0.667
        assert!((growth_curve(50) - 0.5).abs() < 1e-9);
        assert!((growth_curve(100) - (1.0 / 3.0)).abs() < 1e-9); // ≈0.333
    }

    // ─── Formula anchor tests ───────────────────────────────────────
    // Each test pins the EXACT output for a canonical input. These
    // are the cross-pet comparability invariants. If any of these
    // breaks, the formula has changed and someone's pet is going to
    // grow at a different rate than yesterday.

    #[test]
    fn zero_tokens_yield_zero_xp() {
        let t = tokens(0, 0, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 0);
    }

    #[test]
    fn single_input_token_yields_one_xp() {
        // weighted = 1, raw = 0.001 * 1.5 * 1.0 * 1.0 = 0.0015 → round = 0
        // but min-clamp lifts it to 1 because weighted > 0.
        let t = tokens(1, 0, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 1);
    }

    #[test]
    fn typical_small_event_frontier_exact() {
        // weighted = 500*1 + 200*5 = 1500
        // raw = 1.5 * 1.5 * 1.0 * 1.0 = 2.25 → round = 2
        let t = tokens(500, 200, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 2);
    }

    #[test]
    fn typical_mid_event_mid_exact() {
        // weighted = 2000*1 + 2000*5 = 12000
        // raw = 12 * 1.0 * 1.0 * 1.0 = 12 → 12
        let t = tokens(2000, 2000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 12);
    }

    #[test]
    fn cache_heavy_anthropic_session() {
        // Realistic Anthropic turn: tiny input, modest output, heavy cache_read.
        //   weighted = 100*1 + 1500*5 + 900_000*0.1 + 2000*1.25
        //            = 100 + 7500 + 90_000 + 2500 = 100_100
        // raw = 100.1 * 1.0 * 1.0 * 1.0 = 100.1 → 100
        let t = tokens(100, 1500, 0, 2000, 900_000);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 100);
    }

    #[test]
    fn frontier_cap_applies_at_huge_event() {
        // 1M output tokens at Frontier with Exact confidence:
        //   weighted = 1_000_000 * 5 = 5_000_000
        //   raw = 5000 * 1.5 * 1.0 * 1.0 = 7500
        // Cap at 500.
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(
            compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0),
            500
        );
    }

    #[test]
    fn mini_cap_applies_at_huge_event() {
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mini, Confidence::Exact, 0), 80);
    }

    #[test]
    fn mid_cap_applies_at_huge_event() {
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 200);
    }

    #[test]
    fn growth_curve_halves_at_level_50() {
        // weighted = 2000 + 10_000 = 12_000
        // raw at level 0  = 12 * 1.0 * 1.0 * 1.0  = 12
        // raw at level 50 = 12 * 1.0 * 1.0 * 0.5  = 6
        let t = tokens(2000, 2000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 12);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 50), 6);
    }

    #[test]
    fn heuristic_confidence_reduces_by_30pct() {
        // Same event as `typical_mid_event_mid_exact` but Heuristic.
        // raw = 12 * 1.0 * 0.7 * 1.0 = 8.4 → 8
        let t = tokens(2000, 2000, 0, 0, 0);
        assert_eq!(
            compute_base_xp(&t, Tier::Mid, Confidence::Heuristic, 0),
            8
        );
    }

    #[test]
    fn unknown_confidence_still_grants_xp() {
        // Critical UX invariant: NEVER zero on a real event, even if we
        // know nothing about the model.
        let t = tokens(2000, 2000, 0, 0, 0);
        // raw = 12 * 1.0 * 0.4 * 1.0 = 4.8 → 5
        assert_eq!(
            compute_base_xp(&t, Tier::Mid, Confidence::Unknown, 0),
            5
        );
        // Even a single token still yields at least 1 XP.
        let tiny = tokens(1, 0, 0, 0, 0);
        assert_eq!(
            compute_base_xp(&tiny, Tier::Mid, Confidence::Unknown, 0),
            1
        );
    }

    #[test]
    fn output_tokens_weighted_five_times_input() {
        // Sanity: 5K input vs 1K output should yield the SAME xp,
        // because output is weighted 5× input.
        let a = tokens(5000, 0, 0, 0, 0);
        let b = tokens(0, 1000, 0, 0, 0);
        let xa = compute_base_xp(&a, Tier::Mid, Confidence::Exact, 0);
        let xb = compute_base_xp(&b, Tier::Mid, Confidence::Exact, 0);
        assert_eq!(xa, xb);
        assert_eq!(xa, 5); // both = 5000 weighted → 5 xp
    }

    #[test]
    fn cache_read_one_tenth_of_input() {
        // 10K cache_read should yield the SAME xp as 1K input.
        let a = tokens(0, 0, 0, 0, 10_000);
        let b = tokens(1000, 0, 0, 0, 0);
        let xa = compute_base_xp(&a, Tier::Mid, Confidence::Exact, 0);
        let xb = compute_base_xp(&b, Tier::Mid, Confidence::Exact, 0);
        assert_eq!(xa, xb);
        assert_eq!(xa, 1);
    }
}
