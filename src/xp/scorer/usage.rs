//! UsageScorer: maps a `UsageEvent` to an XP delta via weighted token formula.
//!
//! Config schema (loaded from xp_rule.config JSON):
//! ```jsonc
//! {
//!   "weight": {
//!     "input":          1.0,
//!     "output":         4.0,
//!     "reasoning":      6.0,
//!     "cache_creation": 2.0,
//!     "cache_read":     0.0
//!   },
//!   "divisor":    1000.0,
//!   "multiplier": 1.0,
//!   // optional floor & ceil
//!   "min_xp":     null,
//!   "max_xp":     null
//! }
//! ```
//! Result: `xp = round((Σ token_axis * weight_axis) / divisor * multiplier)`,
//! optionally clamped to `[min_xp, max_xp]`.

use serde::Deserialize;
use serde_json::Value;

use crate::event::UsageEvent;
use crate::xp::resolver::LoadedRule;
use crate::xp::types::XpComputation;

pub struct UsageScorer;

#[derive(Deserialize, Default, Debug)]
struct UsageConfig {
    #[serde(default)]
    weight: UsageWeights,
    #[serde(default = "default_divisor")]
    divisor: f64,
    #[serde(default = "default_multiplier")]
    multiplier: f64,
    #[serde(default)]
    min_xp: Option<i64>,
    #[serde(default)]
    max_xp: Option<i64>,
}

fn default_divisor() -> f64 {
    1000.0
}
fn default_multiplier() -> f64 {
    1.0
}

#[derive(Deserialize, Default, Debug)]
struct UsageWeights {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    reasoning: f64,
    #[serde(default)]
    cache_creation: f64,
    #[serde(default)]
    cache_read: f64,
}

impl UsageScorer {
    pub fn score(ue: &UsageEvent, rule: &LoadedRule) -> Option<XpComputation> {
        let cfg = parse_config(&rule.config)?;
        if cfg.divisor == 0.0 {
            return None;
        }
        let weighted = ue.tokens.input as f64 * cfg.weight.input
            + ue.tokens.output as f64 * cfg.weight.output
            + ue.tokens.reasoning as f64 * cfg.weight.reasoning
            + ue.tokens.cache_creation as f64 * cfg.weight.cache_creation
            + ue.tokens.cache_read as f64 * cfg.weight.cache_read;
        let raw = weighted / cfg.divisor * cfg.multiplier;
        let mut xp = raw.round() as i64;
        if let Some(min) = cfg.min_xp {
            if xp < min {
                xp = min;
            }
        }
        if let Some(max) = cfg.max_xp {
            if xp > max {
                xp = max;
            }
        }
        if xp == 0 {
            return None;
        }
        Some(XpComputation {
            xp_delta: xp,
            reason: rule
                .description
                .clone()
                .unwrap_or_else(|| format!("usage via {}", rule.id)),
            rule_id: rule.id.clone(),
        })
    }
}

fn parse_config(v: &Value) -> Option<UsageConfig> {
    serde_json::from_value(v.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, ProviderId, SourceRef, TokenDelta};
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn usage_event(input: u64, output: u64, reasoning: u64) -> UsageEvent {
        UsageEvent {
            id: Uuid::new_v4(),
            provider: ProviderId::ClaudeCode,
            client: None,
            session_id: "s".into(),
            project_path: None,
            git_branch: None,
            model: "claude-opus-4-7".into(),
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

    #[test]
    fn simple_weighted_formula() {
        let r = rule(json!({
            "weight": {"input": 1.0, "output": 4.0, "reasoning": 6.0},
            "divisor": 1000.0,
            "multiplier": 1.0
        }));
        let ue = usage_event(1000, 500, 300);
        // weighted = 1000*1 + 500*4 + 300*6 = 4800
        // xp = 4800 / 1000 * 1.0 = 4.8 → 5
        let c = UsageScorer::score(&ue, &r).unwrap();
        assert_eq!(c.xp_delta, 5);
    }

    #[test]
    fn multiplier_applied() {
        let r = rule(json!({
            "weight": {"output": 4.0},
            "divisor": 1000.0,
            "multiplier": 3.0
        }));
        let ue = usage_event(0, 1000, 0);
        // weighted = 4000; / 1000 * 3 = 12
        let c = UsageScorer::score(&ue, &r).unwrap();
        assert_eq!(c.xp_delta, 12);
    }

    #[test]
    fn zero_tokens_returns_none() {
        let r = rule(json!({"weight": {"output": 4.0}, "divisor": 1000.0}));
        let ue = usage_event(0, 0, 0);
        assert!(UsageScorer::score(&ue, &r).is_none());
    }

    #[test]
    fn min_xp_clamp() {
        let r = rule(json!({
            "weight": {"output": 4.0},
            "divisor": 100000.0,  // huge divisor → small raw xp
            "multiplier": 1.0,
            "min_xp": 1
        }));
        let ue = usage_event(0, 100, 0);
        // raw = 400/100000 = 0.004 → round to 0 → clamp to min 1
        // Actually round(0.004) = 0, then min clamp to 1, then xp != 0 so emit
        let c = UsageScorer::score(&ue, &r).unwrap();
        assert_eq!(c.xp_delta, 1);
    }

    #[test]
    fn max_xp_clamp() {
        let r = rule(json!({
            "weight": {"output": 4.0},
            "divisor": 1.0,
            "multiplier": 1.0,
            "max_xp": 100
        }));
        let ue = usage_event(0, 1000, 0);
        // raw = 4000 → cap 100
        let c = UsageScorer::score(&ue, &r).unwrap();
        assert_eq!(c.xp_delta, 100);
    }
}
