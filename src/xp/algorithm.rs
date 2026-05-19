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
//!          ÷ 60_000                   // TOKEN_DIVISOR
//!          × tier_multiplier(tier)    // {Frontier:1.5, Mid:1.0, Mini:0.7}
//!          × confidence.factor()      // {Exact:1.0, Heuristic:0.7, Unknown:0.4}
//!          × growth_curve(pet_level)  // 1 / (1 + 0.02 × level)
//!
//! xp       = clamp(round(raw), 0, tier_xp_cap(tier))   when weighted > 0
//!          = 0                                          when weighted == 0
//! ```
//!
//! # v0.3.0 rebalance (cross-pet break, intentional)
//!
//! Prior versions used `TOKEN_DIVISOR=1000` + caps `500/200/80`, which let
//! a single Frontier event dump ~750 XP onto a fresh pet (cap=500 × max
//! rule_mult=1.5). That made `Wukong` hatch + reach stage 2 from ONE
//! Opus 4.7 conversation, which contradicts the design goal that pets
//! grow over weeks of real usage.
//!
//! This rebalance keeps the formula shape but tightens every magnitude
//! knob simultaneously so per-event XP becomes proportional to "this was
//! a meaningful interaction" rather than "this was a big API call". With
//! the new constants:
//!
//!   - Typical Opus 4.7 event (~50K weighted): 1-2 XP
//!   - Large session-end event (~140K weighted): ~4 XP
//!   - Cap-saturating outlier:                 ≤10 XP (× rule_mult ≤ 20)
//!
//! Combined with flattened `levels.json` curves and rule_mult-based
//! difficulty (Unicorn easy = 1.0, Sun medium = 0.5), the timeline
//! becomes: ~30 days to L99 easy / ~60 days medium for a 25 conv/day
//! Opus 4.7 user. See `templates/builtin/*/levels.json` for the curve.
//!
//! Existing pets keep their `total_xp` but their displayed level
//! re-resolves under the new `levels.json` — no migration, by design.
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
//! ## TOKEN_DIVISOR = 60_000
//!
//! Calibrates "60K weighted tokens ≈ 1 XP base unit". Anchored against
//! real session-event sizes: a small turn (~5K weighted) → 0 XP (noise
//! threshold), a mid turn (~50K weighted, typical Opus with cache_read)
//! → 1-2 XP, a long turn (~140K weighted) → 3-4 XP, a cap-saturating
//! outlier (~1M+ weighted) → tier_xp_cap. Empirical, but once shipped
//! this divisor is invariant — change it and everyone's pets reset.
//!
//! ## Tier multipliers (1.5 / 1.0 / 0.7)
//!
//! Tier is a *stable* proxy for "compute / capability cost" — Anthropic
//! can re-price Opus tomorrow and our Frontier multiplier doesn't move.
//! USD-anchored multipliers would jump on every vendor price change.
//! Mini was 0.5 pre-rebalance; bumped to 0.7 so weaker-model events
//! (haiku, 4o-mini) still occasionally round above zero — otherwise
//! Mini users get an "is this even working?" UX.
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
//! ## Tier XP caps (10 / 6 / 3 / 5)
//!
//! Per-event ceiling that prevents one anomalous event from dumping a
//! massive XP boost. Real-world outliers: hook payload parsing bugs
//! occasionally report 100M+ token counts. Without this cap, one bad
//! event would saturate the pet for hours. With it, the worst case is
//! `cap × events_per_session × max_rule_mult(2.0)` — bounded.
//!
//! ## Floor = 0 (was 1)
//!
//! Pre-rebalance every nonzero-weighted event was lifted to at least 1
//! XP. With the tightened divisor that floor amplified noise events
//! (hook parsing artifacts, tiny tool responses) into the same XP
//! tier as real LLM turns. Now sub-threshold events return 0 silently
//! and only meaningful events (≳15-30K weighted) contribute XP.

use crate::event::TokenDelta;
use crate::model::Tier;

// ─── Constants — DO NOT CHANGE without bumping a major version ──────

/// Token-axis weights, mirroring Anthropic's billing ratio.
pub const TOKEN_WEIGHT_INPUT: f64 = 1.00;
pub const TOKEN_WEIGHT_OUTPUT: f64 = 5.00;
pub const TOKEN_WEIGHT_REASONING: f64 = 5.00;
pub const TOKEN_WEIGHT_CACHE_CREATION: f64 = 1.25;
pub const TOKEN_WEIGHT_CACHE_READ: f64 = 0.10;

/// 60K weighted tokens ≈ 1 XP base unit. See module doc for the rebalance
/// rationale; this used to be 1000 (60× tighter now).
pub const TOKEN_DIVISOR: f64 = 60_000.0;

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
        Tier::Mini => 0.7,
        Tier::Unknown => 1.0,
    }
}

/// Per-event XP ceiling by tier. Prevents one outlier event (parsing bug,
/// flood, malicious replay) from spiking the pet's level. Tight caps
/// (10/6/3/5) post-rebalance — see module doc.
pub fn tier_xp_cap(tier: Tier) -> i64 {
    match tier {
        Tier::Frontier => 10,
        Tier::Mid => 6,
        Tier::Mini => 3,
        Tier::Unknown => 5,
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
/// Returns 0 when there are no tokens (no event registers) AND when
/// the weighted token count falls below the rounding threshold (~30K
/// for Frontier / Mid, higher for Mini). Sub-threshold "noise" events
/// (hook parsing artifacts, tiny tool responses) silently grant 0 XP;
/// only meaningful LLM interactions contribute. See module doc.
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
    xp.clamp(0, cap)
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
        // v0.3.0 rebalance: was 1000, now 60K. Rationale in module doc.
        assert_eq!(TOKEN_DIVISOR, 60_000.0);
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
        // v0.3.0 rebalance: was 0.5, now 0.7 — see module doc.
        assert_eq!(tier_multiplier(Tier::Mini), 0.7);
        // Unknown defaults to Mid multiplier — heuristic should have
        // resolved this before reaching the scorer.
        assert_eq!(tier_multiplier(Tier::Unknown), 1.0);
    }

    #[test]
    fn tier_xp_caps_pinned() {
        // v0.3.0 rebalance: tight per-event ceilings. Used to be
        // 500/200/80/100 — see module doc.
        assert_eq!(tier_xp_cap(Tier::Frontier), 10);
        assert_eq!(tier_xp_cap(Tier::Mid), 6);
        assert_eq!(tier_xp_cap(Tier::Mini), 3);
        assert_eq!(tier_xp_cap(Tier::Unknown), 5);
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
    //
    // v0.3.0 anchors: realistic session-event sizes (5K - 1M+ weighted)
    // pinned against the new DIVISOR=60K / cap=10/6/3/5 / floor=0 regime.

    #[test]
    fn zero_tokens_yield_zero_xp() {
        let t = tokens(0, 0, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 0);
    }

    #[test]
    fn trivial_event_yields_zero_xp_under_floor() {
        // Pre-v0.3.0 the floor was 1 — any nonzero weighted event got
        // at least 1 XP. Post-rebalance the floor is 0 so noise events
        // (1-token diffs, hook parsing artifacts) silently return 0.
        let t = tokens(1, 0, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 0);
    }

    #[test]
    fn small_event_below_noise_threshold_yields_zero() {
        // weighted = 500 + 1000 = 1500
        // raw = 1500/60_000 × 1.5 × 1.0 × 1.0 = 0.0375 → round 0
        let t = tokens(500, 200, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 0);
    }

    #[test]
    fn noise_threshold_anchored_for_frontier() {
        // The "noise threshold" — weighted below which Frontier+Exact
        // rounds to 0. With DIVISOR=60K and Frontier multiplier=1.5:
        //   raw = weighted/60_000 × 1.5
        //   round-up boundary at raw = 0.5 → weighted = 20_000
        // So any Frontier event with weighted < 20K grants 0 XP.
        let just_below = tokens(0, 3_999, 0, 0, 0); // weighted = 19_995
        assert_eq!(
            compute_base_xp(&just_below, Tier::Frontier, Confidence::Exact, 0),
            0
        );
        let just_above = tokens(0, 4_001, 0, 0, 0); // weighted = 20_005
        assert_eq!(
            compute_base_xp(&just_above, Tier::Frontier, Confidence::Exact, 0),
            1
        );
    }

    #[test]
    fn typical_opus_event_yields_one_xp() {
        // Realistic mid-sized Opus 4.7 turn with cache:
        //   10K input + 3K output + 10K cache_creation + 100K cache_read
        //   weighted = 10_000 + 15_000 + 12_500 + 10_000 = 47_500
        //   raw = 47_500/60_000 × 1.5 = 1.1875 → round 1
        let t = tokens(10_000, 3_000, 0, 10_000, 100_000);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 1);
    }

    #[test]
    fn large_session_event_yields_few_xp() {
        // Realistic large Opus 4.7 turn with reasoning + heavy cache:
        //   20K input + 5K output + 10K reasoning + 20K cache_creation + 200K cache_read
        //   weighted = 20_000 + 25_000 + 50_000 + 25_000 + 20_000 = 140_000
        //   raw = 140_000/60_000 × 1.5 = 3.5 → round-half-away-from-zero 4
        let t = tokens(20_000, 5_000, 10_000, 20_000, 200_000);
        assert_eq!(compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0), 4);
    }

    #[test]
    fn cache_heavy_anthropic_session() {
        // Realistic Anthropic turn: tiny input, modest output, heavy cache_read.
        //   weighted = 100*1 + 1500*5 + 900_000*0.1 + 2000*1.25
        //            = 100 + 7500 + 90_000 + 2500 = 100_100
        // raw = 100_100/60_000 × 1.0 × 1.0 × 1.0 = 1.668 → round 2
        let t = tokens(100, 1500, 0, 2000, 900_000);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 2);
    }

    #[test]
    fn frontier_cap_applies_at_huge_event() {
        // 1M output tokens at Frontier with Exact confidence:
        //   weighted = 1_000_000 * 5 = 5_000_000
        //   raw = 5_000_000/60_000 × 1.5 = 125
        // Cap at 10 (post-rebalance).
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(
            compute_base_xp(&t, Tier::Frontier, Confidence::Exact, 0),
            10
        );
    }

    #[test]
    fn mini_cap_applies_at_huge_event() {
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mini, Confidence::Exact, 0), 3);
    }

    #[test]
    fn mid_cap_applies_at_huge_event() {
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 6);
    }

    #[test]
    fn unknown_cap_applies_at_huge_event() {
        let t = tokens(0, 1_000_000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Unknown, Confidence::Exact, 0), 5);
    }

    #[test]
    fn growth_curve_halves_at_level_50() {
        // Event sized to clear the noise threshold AND stay under Mid's cap=6.
        //   weighted = 40_000 + 200_000 = 240_000
        //   raw at level 0  = 240_000/60_000 × 1.0 × 1.0 × 1.0 = 4
        //   raw at level 50 = 4 × 0.5 = 2
        let t = tokens(40_000, 40_000, 0, 0, 0);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 0), 4);
        assert_eq!(compute_base_xp(&t, Tier::Mid, Confidence::Exact, 50), 2);
    }

    #[test]
    fn heuristic_confidence_reduces_by_30pct() {
        // Same event as `growth_curve_halves_at_level_50` but Heuristic.
        //   raw = 4 × 0.7 = 2.8 → round 3
        let t = tokens(40_000, 40_000, 0, 0, 0);
        assert_eq!(
            compute_base_xp(&t, Tier::Mid, Confidence::Heuristic, 0),
            3
        );
    }

    #[test]
    fn unknown_confidence_grants_reduced_xp_for_real_events() {
        // Post-rebalance Unknown still produces nonzero on substantial
        // events — but the pre-rebalance "always ≥ 1 XP on any nonzero
        // weighted" invariant is gone (floor 0). Tiny events return 0.
        let big = tokens(40_000, 40_000, 0, 0, 0);
        // raw = 4 × 0.4 = 1.6 → round 2
        assert_eq!(compute_base_xp(&big, Tier::Mid, Confidence::Unknown, 0), 2);

        // Pre-rebalance this returned 1 (floor); now returns 0.
        let tiny = tokens(1, 0, 0, 0, 0);
        assert_eq!(compute_base_xp(&tiny, Tier::Mid, Confidence::Unknown, 0), 0);
    }

    #[test]
    fn output_tokens_weighted_five_times_input() {
        // Sanity: 300K input vs 60K output should yield the SAME xp
        // (both = 300K weighted), because output is weighted 5× input.
        // Scaled up vs pre-rebalance so we clear the noise threshold.
        let a = tokens(300_000, 0, 0, 0, 0);
        let b = tokens(0, 60_000, 0, 0, 0);
        let xa = compute_base_xp(&a, Tier::Mid, Confidence::Exact, 0);
        let xb = compute_base_xp(&b, Tier::Mid, Confidence::Exact, 0);
        assert_eq!(xa, xb);
        // raw = 300_000/60_000 × 1.0 = 5 → 5
        assert_eq!(xa, 5);
    }

    #[test]
    fn cache_read_one_tenth_of_input() {
        // 3M cache_read vs 300K input — both = 300K weighted.
        let a = tokens(0, 0, 0, 0, 3_000_000);
        let b = tokens(300_000, 0, 0, 0, 0);
        let xa = compute_base_xp(&a, Tier::Mid, Confidence::Exact, 0);
        let xb = compute_base_xp(&b, Tier::Mid, Confidence::Exact, 0);
        assert_eq!(xa, xb);
        assert_eq!(xa, 5);
    }
}
