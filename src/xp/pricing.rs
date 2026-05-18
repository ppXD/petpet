//! Per-model USD pricing for usage events.
//!
//! ## What this answers
//!
//! Given a `(provider, model, TokenDelta)` triple, how many USD did
//! that single event cost? The aggregate-query helpers in
//! [`crate::xp::cost_query`] use this to surface daily / weekly /
//! lifetime costs for the "feeding bill" UI on top.
//!
//! ## Rate source & accuracy
//!
//! Rates are hand-coded from Anthropic / OpenAI public pricing pages
//! AND, for models without published per-token rates, back-derived
//! from observed usage vs known-good aggregate costs (e.g. CodexBar's
//! local computation, or Anthropic's `api.anthropic.com/api/oauth/usage`
//! response). Each entry below documents WHERE it came from.
//!
//! Where to update:
//!   - For an Anthropic model: <https://www.anthropic.com/pricing#api>
//!   - For an OpenAI model:    <https://openai.com/api/pricing/>
//!   - For DeepSeek / others:  their respective pricing docs
//!
//! ## Match strategy (substring with specificity ordering)
//!
//! Model strings in our logs look like `claude-opus-4-7`,
//! `claude-haiku-4-5-20251001`, `gpt-5.5`, `anthropic/claude-sonnet-4`,
//! `deepseek-v4-flash-free`, etc. We CANNOT use exact match: model
//! versions change (Anthropic ships dated variants like
//! `-20251001`), and tools sometimes namespace with `provider/`
//! prefixes.
//!
//! Instead we walk `TIERS` in order and pick the first whose `match_keys`
//! all appear in the lowercased model name. Order is **most-specific
//! first** so `gpt-5-nano` matches its own tier before falling back to
//! generic `gpt-5`. Free-tier markers (`-free`) come earlier than
//! their paid counterparts.
//!
//! ## Returned cost
//!
//! Cost in USD as `f64`. We deliberately don't use a fixed-point
//! decimal type — cents-precision is meaningless when most events
//! cost <$0.01 each and the rates themselves carry rounding noise.

use crate::event::ProviderId;
use crate::event::TokenDelta;

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

/// One row of the lookup table: which provider, what substrings to
/// match in the lowercased model name (ALL must appear), and the
/// rates to apply.
///
/// `provider` is `Some(p)` to scope a tier to one provider (e.g.
/// Anthropic Sonnet rates apply only to claude_code logs) and `None`
/// for cross-provider matches (rare; mostly for free-tier markers).
struct Tier {
    provider: Option<ProviderId>,
    /// Lowercased substrings — all must appear in the lowercased
    /// model name. Order doesn't matter inside the slice.
    match_keys: &'static [&'static str],
    rates: ModelPricing,
    /// One-line description of provenance / source — kept here
    /// rather than in a separate doc so the table reads top-to-
    /// bottom as "what we charge and why". `#[allow(dead_code)]`
    /// because the field exists for documentation / future export
    /// (e.g. CLI `petpet pricing --explain`); it's not read at
    /// runtime today but removing it would lose the audit trail.
    #[allow(dead_code)]
    note: &'static str,
}

/// Canonical pricing table. Walked top-to-bottom; FIRST match wins,
/// so order entries **most-specific first**.
///
/// Categories of entries:
///   1. Free-tier markers (e.g. `-free` suffix) — must come before
///      paid generic matches that share the same model family
///   2. Specific dated variants if priced differently from base
///   3. Generic family matches (`opus`, `sonnet`, `haiku`, etc.)
///
/// If no tier matches, [`lookup`] returns `None` and the caller
/// treats the event as zero-cost (rather than erroring) so unknown
/// or future models silently contribute nothing instead of breaking
/// the aggregate.
const TIERS: &[Tier] = &[
    // ─── Free tiers ────────────────────────────────────────────
    // OpenRouter / mirror endpoints frequently expose `…-free`
    // variants that cost $0. We catch them by suffix before any
    // paid family-level match below.
    Tier {
        provider: None,
        match_keys: &["-free"],
        rates: ModelPricing::FREE,
        note: "any -free suffix model (OpenRouter free tier, Qwen free, etc.)",
    },

    // ─── Anthropic Claude ──────────────────────────────────────
    // List prices from <https://www.anthropic.com/pricing#api>:
    //   Opus 4:    $15 / $75 / $1.50 / $18.75 per 1M tokens
    //   Sonnet 4:   $3 / $15 / $0.30 / $3.75
    //   Haiku 3.5:  $1 / $5  / $0.10 / $1.25
    //
    // Special case: `claude-opus-4-7` priced as Sonnet × 1.667
    // (≈ Opus / 3). Back-derived from CodexBar's 5/16 reconciliation:
    // exact petpet aggregate for that day was input=9,394 /
    // output=981,958 / cache_read=392,559,814 / cache_create=3,385,391,
    // and CodexBar's tooltip showed $241.89 for that same data set.
    // Solving the four-rate linear system under Anthropic's standard
    // ratios (output = 5× input, cache_read = 0.1× input, cache_create
    // = 1.25× input) lands at $5/$25/$0.50/$6.25 → computed cost
    // $242.04 (0.06% from CodexBar's number). Likely a future Anthropic
    // "Opus mid-tier" or extended-context Sonnet variant, but the
    // rates aren't on the public pricing page as of writing.
    Tier {
        provider: Some(ProviderId::ClaudeCode),
        match_keys: &["opus-4-7"],
        rates: ModelPricing {
            input_per_1m: 5.00,
            output_per_1m: 25.00,
            cache_read_per_1m: 0.50,
            cache_creation_per_1m: 6.25,
            reasoning_per_1m: 0.0,
        },
        note: "claude-opus-4-7: Sonnet×1.667, back-derived from CodexBar 5/16 reconciliation (0.06% match). NOT on Anthropic's public table.",
    },
    Tier {
        provider: Some(ProviderId::ClaudeCode),
        match_keys: &["opus"],
        rates: ModelPricing {
            input_per_1m: 15.00,
            output_per_1m: 75.00,
            cache_read_per_1m: 1.50,
            cache_creation_per_1m: 18.75,
            reasoning_per_1m: 0.0,
        },
        note: "Anthropic Opus 4/4.5 list pricing (catch-all after opus-4-7 special case)",
    },
    Tier {
        provider: Some(ProviderId::ClaudeCode),
        match_keys: &["sonnet"],
        rates: ModelPricing {
            input_per_1m: 3.00,
            output_per_1m: 15.00,
            cache_read_per_1m: 0.30,
            cache_creation_per_1m: 3.75,
            reasoning_per_1m: 0.0,
        },
        note: "Anthropic Sonnet 4/4.5 list pricing",
    },
    Tier {
        provider: Some(ProviderId::ClaudeCode),
        match_keys: &["haiku"],
        rates: ModelPricing {
            input_per_1m: 1.00,
            output_per_1m: 5.00,
            cache_read_per_1m: 0.10,
            cache_creation_per_1m: 1.25,
            reasoning_per_1m: 0.0,
        },
        note: "Anthropic Haiku 3.5/4.5 list pricing",
    },

    // ─── OpenAI / Codex CLI ────────────────────────────────────
    // List prices from <https://openai.com/api/pricing/>.
    // gpt-5.5 isn't a published model name today (May 2026); priced
    // at GPT-5 rates as a defensible default. Override here when
    // OpenAI announces actual pricing.
    Tier {
        provider: Some(ProviderId::Codex),
        match_keys: &["gpt-5-nano"],
        rates: ModelPricing {
            input_per_1m: 0.05,
            output_per_1m: 0.40,
            cache_read_per_1m: 0.005,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 0.40,
        },
        note: "GPT-5 nano (most specific first)",
    },
    Tier {
        provider: Some(ProviderId::Codex),
        match_keys: &["gpt-5-mini"],
        rates: ModelPricing {
            input_per_1m: 0.25,
            output_per_1m: 2.00,
            cache_read_per_1m: 0.025,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 2.00,
        },
        note: "GPT-5 mini",
    },
    Tier {
        provider: Some(ProviderId::Codex),
        match_keys: &["gpt-5"],
        rates: ModelPricing {
            input_per_1m: 1.25,
            output_per_1m: 10.00,
            cache_read_per_1m: 0.125,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 10.00,
        },
        note: "GPT-5 family (catch-all after gpt-5-mini / gpt-5-nano)",
    },
    Tier {
        provider: Some(ProviderId::Codex),
        match_keys: &["o4-mini"],
        rates: ModelPricing {
            input_per_1m: 0.275,
            output_per_1m: 1.10,
            cache_read_per_1m: 0.0688,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 1.10,
        },
        note: "OpenAI o4-mini",
    },
    Tier {
        provider: Some(ProviderId::Codex),
        match_keys: &["o3-mini"],
        rates: ModelPricing {
            input_per_1m: 1.10,
            output_per_1m: 4.40,
            cache_read_per_1m: 0.55,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 4.40,
        },
        note: "OpenAI o3-mini",
    },
    Tier {
        provider: Some(ProviderId::Codex),
        match_keys: &["o3"],
        rates: ModelPricing {
            input_per_1m: 2.00,
            output_per_1m: 8.00,
            cache_read_per_1m: 0.50,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 8.00,
        },
        note: "OpenAI o3 (catch-all after o3-mini)",
    },

    // ─── Gemini ────────────────────────────────────────────────
    // List from Google AI docs. Gemini doesn't break out cache
    // tokens like Anthropic; cache_read_per_1m left at 0.
    Tier {
        provider: Some(ProviderId::Gemini),
        match_keys: &["2.5-pro"],
        rates: ModelPricing {
            input_per_1m: 1.25,
            output_per_1m: 5.00,
            cache_read_per_1m: 0.0,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 0.0,
        },
        note: "Gemini 2.5 Pro",
    },
    Tier {
        provider: Some(ProviderId::Gemini),
        match_keys: &["flash-lite"],
        rates: ModelPricing {
            input_per_1m: 0.075,
            output_per_1m: 0.30,
            cache_read_per_1m: 0.0,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 0.0,
        },
        note: "Gemini Flash-Lite",
    },
    Tier {
        provider: Some(ProviderId::Gemini),
        match_keys: &["flash"],
        rates: ModelPricing {
            input_per_1m: 0.15,
            output_per_1m: 0.60,
            cache_read_per_1m: 0.0,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 0.0,
        },
        note: "Gemini Flash (catch-all after flash-lite)",
    },

    // ─── DeepSeek (OpenCode, OpenRouter, etc.) ─────────────────
    // List from <https://api-docs.deepseek.com/quick_start/pricing>.
    Tier {
        provider: None,
        match_keys: &["deepseek"],
        rates: ModelPricing {
            input_per_1m: 0.014,
            output_per_1m: 0.28,
            cache_read_per_1m: 0.014,
            cache_creation_per_1m: 0.0,
            reasoning_per_1m: 0.28,
        },
        note: "DeepSeek (all variants — Chat / Coder / V4-flash priced similarly)",
    },

    // ─── Aider's many-model mode ───────────────────────────────
    // Aider's `analytics-log` uses LiteLLM-style names like
    // `anthropic/claude-3-5-sonnet-20241022` or
    // `gemini/gemini-2.5-flash`. The provider-prefix matching above
    // (`opus`/`sonnet`/`haiku`/`gemini-…`) already catches them
    // because we lowercase and substring-search. No extra entries
    // needed.
];

/// Look up pricing for one `(provider, model)` pair. Returns `None`
/// if no tier matches — caller treats that as $0 (silent), not as
/// an error.
pub fn lookup(provider: ProviderId, model: &str) -> Option<ModelPricing> {
    let lc = model.to_ascii_lowercase();
    for tier in TIERS {
        if let Some(p) = tier.provider {
            if p != provider {
                continue;
            }
        }
        if tier.match_keys.iter().all(|k| lc.contains(k)) {
            return Some(tier.rates);
        }
    }
    None
}

/// Compute the USD cost of one usage event. Unknown models contribute
/// $0 silently so they don't break aggregates while we wait for the
/// pricing table to catch up.
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
    ///      the tier), or
    ///   2. We accidentally regressed the formula / rates.
    #[test]
    fn five_sixteen_local_aggregate_matches_codexbar_within_1pct() {
        // Exact petpet aggregate for 5/16 LOCAL UTC+8 opus-4-7.
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
}
