//! UsageScorer: drives the hardcoded XP formula in
//! [`crate::xp::algorithm`] and applies the per-pet rule's multiplier
//! as a clamped scale on top.
//!
//! # Config schema (post-Phase-2b-4)
//!
//! Only one field is honoured:
//!
//! ```jsonc
//! { "multiplier": 1.5 }   // clamped to [0.5, 2.0]
//! ```
//!
//! The old `weight` / `divisor` / `min_xp` / `max_xp` fields are
//! tolerated (serde silently ignores them) so existing pet snapshots
//! don't fail to load, but they no longer affect the result. The
//! formula itself is invariant and lives in `algorithm.rs`.
//!
//! # Why the rule's multiplier is clamped
//!
//! Per-pet personality (e.g. a pet that "prefers" Opus, gets more XP
//! for it) is still expressed through the rule's `multiplier`. But the
//! algorithm is supposed to be invariant cross-pet, so allowing
//! unbounded multipliers would break leaderboard / achievement parity.
//! `[0.5, 2.0]` gives template authors expressive range without
//! breaking the invariant.
//!
//! # Per-event flow
//!
//! ```text
//! UsageEvent
//!   ↓ parse model
//! ModelIdent
//!   ↓ validate
//!   ↓ (tier, confidence) — Registry first, heuristic fallback
//!   ↓ algorithm::compute_base_xp(tokens, tier, confidence, pet_level)
//! base_xp
//!   ↓ apply rule's clamped multiplier
//! XpComputation
//! ```

use serde::Deserialize;
use serde_json::Value;

use crate::event::UsageEvent;
use crate::model::ModelIdent;
use crate::xp::algorithm::compute_base_xp;
use crate::xp::classification::classify;
use crate::xp::heuristic::{validate_model_name, validate_tokens};
use crate::xp::resolver::LoadedRule;
use crate::xp::types::XpComputation;

pub struct UsageScorer;

/// The clamp range for the per-pet rule's `multiplier` field. Wider
/// than [1.0, 1.0] (so templates have expressive room) but narrower
/// than [0, ∞) (so cross-pet comparability holds).
pub const RULE_MULT_MIN: f64 = 0.5;
pub const RULE_MULT_MAX: f64 = 2.0;

#[derive(Deserialize, Default, Debug)]
struct UsageConfig {
    /// Per-pet preference scale. Clamped to [`RULE_MULT_MIN`,
    /// `RULE_MULT_MAX`] before being applied — wider ranges are
    /// silently narrowed.
    #[serde(default = "default_multiplier")]
    multiplier: f64,
    // `weight` / `divisor` / `min_xp` / `max_xp` fields are accepted
    // but ignored (serde tolerates them) so pre-2b-4 snapshots load
    // without error. The algorithm itself is invariant.
}

fn default_multiplier() -> f64 {
    1.0
}

impl UsageScorer {
    pub fn score(
        ue: &UsageEvent,
        ident: &ModelIdent,
        rule: &LoadedRule,
        pet_level: u32,
    ) -> Option<XpComputation> {
        // Anti-cheat: reject inputs that can't be real before the
        // formula sees them.
        if !validate_model_name(&ident.model) {
            return None;
        }
        if !validate_tokens(&ue.tokens) {
            return None;
        }

        // Resolve the model's tier + how confident we are. Delegated
        // to `crate::xp::classification` so the dashboard's "guessed"
        // badge and the scorer's confidence factor never disagree.
        // Registry-known models → Exact; hardcoded fallback / keyword
        // heuristic → Heuristic; nothing → Mid + Unknown (still > 0
        // XP so the pet keeps growing).
        let (tier, confidence) = classify(ident);

        // Base XP from the invariant formula.
        let base = compute_base_xp(&ue.tokens, tier, confidence, pet_level);
        if base == 0 {
            return None;
        }

        // Per-pet personality: multiplier scaled by the rule's value,
        // clamped to a range that preserves cross-pet comparability.
        let cfg: UsageConfig = serde_json::from_value(rule.config.clone()).unwrap_or_default();
        let mult = cfg.multiplier.clamp(RULE_MULT_MIN, RULE_MULT_MAX);

        let xp_delta = (base as f64 * mult).round() as i64;
        if xp_delta == 0 {
            return None;
        }

        Some(XpComputation {
            xp_delta,
            reason: rule
                .description
                .clone()
                .unwrap_or_else(|| format!("usage via {}", rule.id)),
            rule_id: rule.id.clone(),
        })
    }
}

// Kept for any internal callers; serde doesn't need it but it
// documents the original parse path.
#[allow(dead_code)]
fn parse_config(v: &Value) -> Option<UsageConfig> {
    serde_json::from_value(v.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, ProviderId, SourceRef, TokenDelta};
    use crate::model::Tier;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn usage_event(model: &str, input: u64, output: u64, reasoning: u64) -> UsageEvent {
        UsageEvent {
            id: Uuid::new_v4(),
            provider: ProviderId::ClaudeCode,
            client: None,
            session_id: "s".into(),
            project_path: None,
            git_branch: None,
            model: model.into(),
            timestamp: Utc::now(),
            tokens: TokenDelta {
                input,
                output,
                cache_read: 0,
                cache_creation: 0,
                reasoning,
            },
            kind: EventKind::Turn { stop_reason: None },
            source: SourceRef {
                file: "test".into(),
                byte_offset: 0,
                line: 1,
            },
        }
    }

    fn rule(cfg: Value) -> LoadedRule {
        LoadedRule {
            id: "r".into(),
            source_type: "usage".into(),
            match_predicate: json!({}),
            match_field_count: 0,
            config: cfg,
            priority: 100,
            description: Some("test rule".into()),
        }
    }

    // ─── Formula integration: registry-known model ──────────────────

    #[test]
    fn opus_event_uses_frontier_tier_exact_confidence() {
        // claude-opus-4-7 → Registry says Frontier, Exact confidence.
        // Tokens scaled post-v0.3.0 rebalance to clear noise threshold:
        // input=40_000, output=40_000 → weighted = 40_000 + 200_000 = 240_000
        // base = 240_000/60_000 × 1.5 × 1.0 × 1.0 = 6 → 6
        // multiplier=1.0 → final = 6
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-opus-4-7", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 6);
    }

    #[test]
    fn sonnet_event_uses_mid_tier_exact_confidence() {
        // claude-sonnet-4 → Registry says Mid, Exact.
        // input=40_000 output=40_000 → weighted=240_000
        // base = 240_000/60_000 × 1.0 × 1.0 × 1.0 = 4 → 4
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-sonnet-4", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 4);
    }

    #[test]
    fn haiku_event_uses_mini_tier_exact_confidence() {
        // claude-haiku-4-5 → Mini, Exact.
        // input=20_000 output=20_000 → weighted=120_000 (kept under
        // Mini cap=3 so we verify tier multiplier, not the cap path).
        // base = 120_000/60_000 × 0.7 × 1.0 × 1.0 = 1.4 → 1
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-haiku-4-5", 20_000, 20_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 1);
    }

    // ─── Heuristic fallback for never-seen models ───────────────────

    #[test]
    fn unknown_opus_variant_falls_back_to_frontier_heuristic() {
        // claude-opus-9-5: not in registry. model.rs's hardcoded
        // family table catches `family.starts_with("claude-opus")`
        // and returns Frontier. Classification (post-Phase-2b-3b-
        // confidence-fix) treats the hardcoded match as Heuristic
        // (not Exact), so XP is dampened by 0.7×.
        //
        // input=40_000, output=40_000 → weighted = 240_000
        // base = 240_000/60_000 × 1.5 × 0.7 × 1.0 = 4.2 → 4
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-opus-9-5", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 4);
    }

    #[test]
    fn truly_unknown_model_uses_unknown_confidence() {
        // "zephyr-7000": no registry hit, model.rs hardcoded fallback
        // also returns Tier::Unknown. Scorer hits classify() → heuristic
        // → FallbackResult::Default → (Mid, Unknown).
        // input=40_000 output=40_000 → weighted=240_000
        // base = 240_000/60_000 × 1.0 × 0.4 × 1.0 = 1.6 → 2
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("zephyr-7000", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        assert_eq!(ident.tier, Tier::Unknown);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 2);
    }

    #[test]
    fn heuristic_mini_keyword_attenuates_to_zero_seven() {
        // "future-model-nano": not in registry, model.rs hardcoded
        // fallback catches the "nano" segment → Tier::Mini. The
        // confidence-fix makes this Heuristic (not Exact) since the
        // registry didn't recognise the model name.
        //
        // input=40_000 output=40_000 → weighted = 240_000
        // base = 240_000/60_000 × 0.7 × 0.7 × 1.0 = 1.96 → 2
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("future-model-nano", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        assert_eq!(ident.tier, Tier::Mini);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 2);
    }

    // ─── Rule multiplier clamping ───────────────────────────────────

    #[test]
    fn rule_multiplier_under_one_scales_down() {
        // base = 4 (from sonnet test, 240K weighted), multiplier=0.5 → 2
        let r = rule(json!({ "multiplier": 0.5 }));
        let ue = usage_event("claude-sonnet-4", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 2);
    }

    #[test]
    fn rule_multiplier_above_two_clamps_to_two() {
        // base = 4 (sonnet, mid). multiplier=100.0 in JSON, clamped to 2.0.
        // final = 4×2 = 8 (cap is per-tier base, applies pre-multiplier;
        // post-multiplier max is cap × 2.0 = 12 for Mid — under that.)
        let r = rule(json!({ "multiplier": 100.0 }));
        let ue = usage_event("claude-sonnet-4", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 8);
    }

    #[test]
    fn rule_multiplier_below_half_clamps_to_half() {
        // multiplier=0.01 clamped to 0.5. base=4×0.5=2
        let r = rule(json!({ "multiplier": 0.01 }));
        let ue = usage_event("claude-sonnet-4", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 2);
    }

    #[test]
    fn legacy_rule_fields_ignored() {
        // Old config with weight / divisor / min_xp / max_xp — the
        // scorer should silently ignore them all and use only
        // multiplier (defaulting to 1.0 here since not specified).
        let r = rule(json!({
            "weight": {"input": 99.0, "output": 99.0},
            "divisor": 1.0,
            "multiplier": 1.0,
            "min_xp": 100,
            "max_xp": 100
        }));
        let ue = usage_event("claude-sonnet-4", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        // Despite max_xp:100 and divisor:1.0, result is still 4
        // (the new algorithm's output, not the legacy config's).
        assert_eq!(c.xp_delta, 4);
    }

    // ─── Growth curve threading ─────────────────────────────────────

    #[test]
    fn growth_curve_threads_through_pet_level() {
        // base at level 0 = 4 (sonnet, 240K weighted),
        // base at level 50 = 4 × 0.5 = 2.
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-sonnet-4", 40_000, 40_000, 0);
        let ident = ModelIdent::parse(&ue.model);

        let c0 = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        let c50 = UsageScorer::score(&ue, &ident, &r, 50).unwrap();
        assert_eq!(c0.xp_delta, 4);
        assert_eq!(c50.xp_delta, 2);
    }

    // ─── Validation / anti-cheat short-circuits ─────────────────────

    #[test]
    fn zero_tokens_returns_none() {
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-opus-4-7", 0, 0, 0);
        let ident = ModelIdent::parse(&ue.model);
        assert!(UsageScorer::score(&ue, &ident, &r, 0).is_none());
    }

    #[test]
    fn implausible_tokens_returns_none() {
        // > MAX_TOKENS_PER_EVENT (5M) → rejected pre-formula.
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-opus-4-7", 6_000_000, 0, 0);
        let ident = ModelIdent::parse(&ue.model);
        assert!(UsageScorer::score(&ue, &ident, &r, 0).is_none());
    }

    #[test]
    fn invalid_model_name_returns_none() {
        let r = rule(json!({ "multiplier": 1.0 }));
        // Uppercase signals normalization was skipped (the normalized
        // form is always lowercase). validate_model_name rejects.
        // We construct ModelIdent directly to skip parse's normalization.
        let ue = usage_event("Claude\u{200E}Opus", 1000, 1000, 0);
        let mut ident = ModelIdent::parse(&ue.model);
        ident.model = "Claude\u{200E}Opus".into(); // simulate bypass
        assert!(UsageScorer::score(&ue, &ident, &r, 0).is_none());
    }

    // ─── Free-tier marker ───────────────────────────────────────────

    #[test]
    fn free_tier_model_still_grants_xp_at_mini_tier() {
        // claude-opus-4-7-free → Registry's free marker → Tier::Mini.
        // Free pricing doesn't mean zero XP; the user still used the
        // model. input=20K output=20K → weighted=120_000
        // base = 120_000/60_000 × 0.7 × 1.0 × 1.0 = 1.4 → 1
        let r = rule(json!({ "multiplier": 1.0 }));
        let ue = usage_event("claude-opus-4-7-free", 20_000, 20_000, 0);
        let ident = ModelIdent::parse(&ue.model);
        assert_eq!(ident.tier, Tier::Mini);
        let c = UsageScorer::score(&ue, &ident, &r, 0).unwrap();
        assert_eq!(c.xp_delta, 1);
    }
}
