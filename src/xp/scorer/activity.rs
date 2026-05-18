//! ActivityScorer: maps `ActivityEvent` to XP via a flat per-rule `xp` value.
//!
//! Each rule's match predicate picks WHICH events the rule fires for; the
//! scorer just reads `config.xp` and returns it. Rules are how granularity
//! is achieved (per provider / kind / tool / success / agent_type).
//!
//! Config schema:
//! ```jsonc
//! { "xp": 5 }                                  // simple
//! { "xp": -2, "reason": "tool failure" }       // with override
//! ```

use serde::Deserialize;
use serde_json::Value;

use crate::event::ActivityEvent;
use crate::xp::resolver::LoadedRule;
use crate::xp::types::XpComputation;

pub struct ActivityScorer;

#[derive(Deserialize, Debug)]
struct ActivityConfig {
    xp: i64,
    #[serde(default)]
    reason: Option<String>,
}

impl ActivityScorer {
    pub fn score(_ae: &ActivityEvent, rule: &LoadedRule) -> Option<XpComputation> {
        let cfg = parse_config(&rule.config)?;
        if cfg.xp == 0 {
            return None;
        }
        let reason = cfg
            .reason
            .or_else(|| rule.description.clone())
            .unwrap_or_else(|| format!("activity via {}", rule.id));
        Some(XpComputation {
            xp_delta: cfg.xp,
            reason,
            rule_id: rule.id.clone(),
        })
    }
}

fn parse_config(v: &Value) -> Option<ActivityConfig> {
    serde_json::from_value(v.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ActivityKind, ProviderId};
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn activity(kind: ActivityKind) -> ActivityEvent {
        ActivityEvent {
            id: Uuid::new_v4(),
            provider: ProviderId::ClaudeCode,
            session_id: None,
            project_path: None,
            timestamp: Utc::now(),
            kind,
        }
    }

    fn rule(cfg: Value) -> LoadedRule {
        LoadedRule {
            id: "r".into(),
            source_type: "activity".into(),
            match_predicate: json!({}),
            match_field_count: 0,
            config: cfg,
            priority: 100,
            description: None,
        }
    }

    #[test]
    fn positive_xp() {
        let r = rule(json!({"xp": 5}));
        let ae = activity(ActivityKind::AssistantStop);
        let c = ActivityScorer::score(&ae, &r).unwrap();
        assert_eq!(c.xp_delta, 5);
    }

    #[test]
    fn negative_xp() {
        let r = rule(json!({"xp": -2, "reason": "tool failure"}));
        let ae = activity(ActivityKind::ToolUseEnd {
            name: "Bash".into(),
            success: false,
            tool_use_id: None,
        });
        let c = ActivityScorer::score(&ae, &r).unwrap();
        assert_eq!(c.xp_delta, -2);
        assert!(c.reason.contains("tool failure"));
    }

    #[test]
    fn zero_xp_returns_none() {
        let r = rule(json!({"xp": 0}));
        let ae = activity(ActivityKind::AssistantStop);
        assert!(ActivityScorer::score(&ae, &r).is_none());
    }
}
