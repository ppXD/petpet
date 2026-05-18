//! Per-model USD pricing for usage events.
//!
//! # What this answers
//!
//! Given a `(provider, model, TokenDelta)` triple, how many USD did
//! that single event cost? The aggregate-query helpers in
//! [`crate::xp::cost_query`] use this to surface daily / weekly /
//! lifetime costs for the "feeding bill" UI on top.
//!
//! # Where the numbers come from
//!
//! As of Phase 2b-2 this module is a thin wrapper around
//! [`crate::xp::registry::Registry`], which reads the bundled
//! `data/models.json` (and, in a future phase, syncs from the remote
//! `petpet-model-registry` repo). The previously-hardcoded 38-tier
//! table is gone — adding a model now means editing the JSON, not the
//! Rust source.
//!
//! See `data/models.json` for per-entry provenance: vendor-official
//! pricing pages, third-party hosts (Together AI for Llama), and
//! CodexBar-style linear-system reconciliations for opaque rates.
//!
//! # Behaviour change vs Phase 2a
//!
//! The old hardcoded table constrained Anthropic / OpenAI / Gemini
//! tiers by `ProviderId` — so `compute_cost_usd(OpenCode,
//! "claude-opus-4-7", ...)` returned $0 because the Opus tier was
//! marked `provider: Some(ClaudeCode)`. The registry is provider-
//! agnostic, which is the correct behaviour: a user who proxies Claude
//! Opus through OpenCode still paid Anthropic Opus rates. The
//! `provider` parameter is preserved on the public API for backward
//! compatibility but no longer constrains the match.

use crate::event::ProviderId;
use crate::event::TokenDelta;
use crate::xp::registry::{self, Registry};

/// Per-million-token rates in USD for each token category.
///
/// Anthropic Sonnet 4 example: `input=3.00, output=15.00,
/// cache_read=0.30, cache_creation=3.75`. Reasoning is OpenAI-style
/// "internal thinking" tokens (o-series); zero for models that
/// don't expose it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    pub cache_read_per_1m: f64,
    pub cache_creation_per_1m: f64,
    pub reasoning_per_1m: f64,
}

impl ModelPricing {
    /// USD cost for one event's token delta.
    pub fn cost_usd(&self, tokens: &TokenDelta) -> f64 {
        let per_million = |toks: u64, rate: f64| (toks as f64) * rate / 1_000_000.0;
        per_million(tokens.input, self.input_per_1m)
            + per_million(tokens.output, self.output_per_1m)
            + per_million(tokens.cache_read, self.cache_read_per_1m)
            + per_million(tokens.cache_creation, self.cache_creation_per_1m)
            + per_million(tokens.reasoning, self.reasoning_per_1m)
    }

    /// Zero pricing — used for "free" models / unknown models so they
    /// contribute nothing to aggregates without erroring.
    pub const FREE: ModelPricing = ModelPricing {
        input_per_1m: 0.0,
        output_per_1m: 0.0,
        cache_read_per_1m: 0.0,
        cache_creation_per_1m: 0.0,
        reasoning_per_1m: 0.0,
    };
}

impl From<&registry::PricingPer1m> for ModelPricing {
    fn from(p: &registry::PricingPer1m) -> Self {
        ModelPricing {
            input_per_1m: p.input,
            output_per_1m: p.output,
            cache_read_per_1m: p.cache_read,
            cache_creation_per_1m: p.cache_creation,
            reasoning_per_1m: p.reasoning,
        }
    }
}

/// Look up pricing for one `(provider, model)` pair. Returns `None`
/// if no tier matches — caller treats that as $0 (silent), not as
/// an error.
///
/// `provider` is preserved on the signature for backward compatibility
/// with callers that pass it through, but the registry is provider-
/// agnostic and ignores it. Two callers that pass different providers
/// for the same model string get the same pricing — which is correct,
/// because the model's cost is determined by the model, not by which
/// tool logged the event.
pub fn lookup(_provider: ProviderId, model: &str) -> Option<ModelPricing> {
    Registry::bundled()
        .lookup(model)
        .map(|entry| entry.pricing.into())
}

/// Compute the USD cost of one usage event. Unknown models contribute
/// $0 silently so they don't break aggregates while we wait for the
/// registry to catch up.
pub fn compute_cost_usd(provider: ProviderId, model: &str, tokens: &TokenDelta) -> f64 {
    lookup(provider, model)
        .map(|p| p.cost_usd(tokens))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A few sanity tokens patterns that real Anthropic responses
    /// look like — heavy cache_read, modest output, tiny input.
    fn typical_opus_turn() -> TokenDelta {
        TokenDelta {
            input: 6,
            output: 1500,
            cache_read: 900_000,
            cache_creation: 2_000,
            reasoning: 0,
        }
    }

    /// Pin the claude-opus-4-7 special case. Back-derived from
    /// CodexBar's 5/16 reconciliation; if this drifts, our daily
    /// aggregates will diverge from CodexBar / Anthropic's authoritative
    /// numbers. Rates follow Anthropic's standard ratios
    /// (5× / 0.1× / 1.25× anchored on input) at Sonnet × 1.667.
    #[test]
    fn opus_4_7_uses_back_derived_rates() {
        let p = lookup(ProviderId::ClaudeCode, "claude-opus-4-7")
            .expect("opus-4-7 must have a tier");
        assert_eq!(p.input_per_1m, 5.00);
        assert_eq!(p.output_per_1m, 25.00);
        assert_eq!(p.cache_read_per_1m, 0.50);
        assert_eq!(p.cache_creation_per_1m, 6.25);
    }

    /// Specificity order: `opus-4-7` matches BEFORE generic `opus`.
    /// If this regresses, opus-4-7 will get billed at Opus list
    /// price ($15/$75) and the aggregate will jump 6×.
    #[test]
    fn opus_4_7_beats_generic_opus() {
        let specific = lookup(ProviderId::ClaudeCode, "claude-opus-4-7").unwrap();
        let generic = lookup(ProviderId::ClaudeCode, "claude-opus-4-5-20251101").unwrap();
        assert_ne!(specific, generic, "tier specificity broke");
        assert_eq!(specific.cache_read_per_1m, 0.50);
        assert_eq!(generic.cache_read_per_1m, 1.50, "generic Opus is list-price");
    }

    /// Sonnet family matches dated variants.
    #[test]
    fn sonnet_dated_variant_resolves() {
        let p = lookup(ProviderId::ClaudeCode, "claude-sonnet-4-5-20250929").unwrap();
        assert_eq!(p.input_per_1m, 3.00);
        assert_eq!(p.cache_read_per_1m, 0.30);
    }

    /// Haiku family.
    #[test]
    fn haiku_resolves() {
        let p = lookup(ProviderId::ClaudeCode, "claude-haiku-4-5-20251001").unwrap();
        assert_eq!(p.input_per_1m, 1.00);
        assert_eq!(p.output_per_1m, 5.00);
    }

    /// Free-tier markers always resolve to $0, regardless of family.
    /// Critical for the OpenRouter / Qwen-free cases we see in real data.
    #[test]
    fn any_dash_free_suffix_is_zero() {
        for model in [
            "deepseek-v4-flash-free",
            "qwen3.6-plus-free",
            "claude-opus-4-7-free",
            "openrouter/llama-3.3-70b-instruct-free",
        ] {
            let cost = compute_cost_usd(ProviderId::OpenCode, model, &typical_opus_turn());
            assert_eq!(cost, 0.0, "free model {model} must cost $0");
        }
    }

    /// Unknown model → $0 (silent), NOT a panic. Lets future models
    /// flow through aggregates without breaking the pet UI before
    /// we ship a price for them.
    #[test]
    fn unknown_model_is_silent_zero() {
        let cost = compute_cost_usd(
            ProviderId::ClaudeCode,
            "claude-bananacake-v99",
            &typical_opus_turn(),
        );
        assert_eq!(cost, 0.0);
    }

    /// Cost-of-a-typical-turn smoke test. With current opus-4-7
    /// rates ($5/$25/$0.50/$6.25), a 900k-cache_read + 1.5k-output
    /// turn should cost roughly $0.49.
    #[test]
    fn opus_4_7_typical_turn_cost_ballpark() {
        let cost = compute_cost_usd(
            ProviderId::ClaudeCode,
            "claude-opus-4-7",
            &typical_opus_turn(),
        );
        // Compute exactly to pin the formula:
        //   in:    6      * 5    / 1M = 0.000030
        //   out:   1500   * 25   / 1M = 0.0375
        //   cr:    900000 * 0.50 / 1M = 0.45
        //   cc:    2000   * 6.25 / 1M = 0.0125
        //                              ─────────
        //                              ≈ 0.50
        assert!(
            (0.49..=0.51).contains(&cost),
            "expected ~$0.50, got ${cost:.4}"
        );
    }

    /// Pin the CodexBar-reconciliation invariant: feeding our EXACT
    /// 5/16 LOCAL (UTC+8) data through `compute_cost_usd` must
    /// produce a number within ±1% of CodexBar's $241.89.
    ///
    /// Numbers are queried verbatim from the petpet usage_event
    /// table after the message.id dedup fix. The TOTAL across the
    /// four buckets is 396,936,557 tokens — within 64,000 of
    /// CodexBar's reported 397M (rounded). The $-side match
    /// confirms our pricing formula is correct under Anthropic's
    /// real billing rules.
    ///
    /// If this drifts beyond 1%, either:
    ///   1. Anthropic published new prices for claude-opus-4-7 (update
    ///      data/models.json), or
    ///   2. We accidentally regressed the formula / rates.
    #[test]
    fn five_sixteen_local_aggregate_matches_codexbar_within_1pct() {
        let tokens = TokenDelta {
            input: 9_394,
            output: 981_958,
            cache_read: 392_559_814,
            cache_creation: 3_385_391,
            reasoning: 0,
        };
        let cost = compute_cost_usd(ProviderId::ClaudeCode, "claude-opus-4-7", &tokens);
        let codexbar_reported = 241.89_f64;
        let pct_diff = (cost - codexbar_reported).abs() / codexbar_reported * 100.0;
        assert!(
            pct_diff < 1.0,
            "5/16 cost ${cost:.2} vs CodexBar's ${codexbar_reported} differs by {pct_diff:.2}% — \
             rates may need tweaking or Anthropic published new prices"
        );
    }

    /// Behaviour change vs Phase 2a: cross-provider Claude lookup now
    /// returns Anthropic rates instead of $0. A user proxying Opus
    /// through OpenCode / Aider really did pay Anthropic prices, so
    /// the registry's provider-agnostic match is correct.
    #[test]
    fn opencode_claude_opus_now_resolves_to_anthropic_rates() {
        let cost = compute_cost_usd(
            ProviderId::OpenCode,
            "claude-opus-4-7",
            &typical_opus_turn(),
        );
        // Same as opus_4_7_typical_turn_cost_ballpark — ~$0.50.
        assert!(
            (0.49..=0.51).contains(&cost),
            "OpenCode + claude-opus-4-7 must now match Anthropic rates, got ${cost:.4}"
        );
    }
}
