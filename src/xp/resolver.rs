//! Rule resolver: pick the most-specific enabled rule that matches the
//! input context, from one pet's own rule set.
//!
//! Specificity ordering:
//!   1. priority DESC (manually set on each rule)
//!   2. match-field count DESC (more fields = more specific)
//! Highest tuple wins.
//!
//! There is no longer a "species_id" axis — each pet has its own rules
//! loaded from its `pet.json`, so every rule in the cache is already
//! scoped to that one pet.

use anyhow::Result;
use serde_json::Value;

use crate::template::types::TemplateRule;
use crate::xp::types::{MatchContext, XpSourceType};

#[derive(Debug, Clone)]
pub struct LoadedRule {
    pub id: String,
    pub source_type: String,
    pub match_predicate: Value,
    pub match_field_count: u32,
    pub config: Value,
    pub priority: i32,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RuleCache {
    rules: Vec<LoadedRule>,
}

impl RuleCache {
    /// Build from a pet's snapshot (`pet.json` rules array).
    pub fn from_template_rules(rules: &[TemplateRule]) -> Self {
        let loaded = rules
            .iter()
            .map(|r| LoadedRule {
                id: r.id.clone(),
                source_type: r.source_type.clone(),
                match_field_count: count_match_fields(&r.match_predicate),
                match_predicate: r.match_predicate.clone(),
                config: r.config.clone(),
                priority: r.priority as i32,
                description: r.description.clone(),
            })
            .collect();
        Self { rules: loaded }
    }

    pub fn rules(&self) -> &[LoadedRule] {
        &self.rules
    }

    /// Find the best matching rule for a given (source_type, ctx).
    pub fn resolve(
        &self,
        source_type: XpSourceType,
        ctx: &MatchContext,
    ) -> Option<&LoadedRule> {
        let src = source_type.as_str();
        self.rules
            .iter()
            .filter(|r| r.source_type == src)
            .filter(|r| matches_predicate(&r.match_predicate, ctx))
            .max_by_key(|r| (r.priority, r.match_field_count as i32))
    }
}

/// Apply the `match` JSON predicate against a context. All non-null
/// fields the predicate sets must equal the corresponding context
/// fields; unknown keys fail-safe (no match).
pub fn matches_predicate(predicate: &Value, ctx: &MatchContext) -> bool {
    let Some(obj) = predicate.as_object() else {
        return true;
    };
    if obj.is_empty() {
        return true;
    }
    for (key, want_value) in obj {
        if want_value.is_null() {
            continue;
        }
        let got: Option<String> = match key.as_str() {
            "vendor" => ctx.vendor.clone(),
            "family" => ctx.family.clone(),
            "model" => ctx.model.clone(),
            "raw" => ctx.raw.clone(),
            "tier" => ctx.tier.clone(),
            "provider" => ctx.provider.clone(),
            "client" => ctx.client.clone(),
            "kind" => ctx.kind.clone(),
            "tool" => ctx.tool.clone(),
            "agent_type" => ctx.agent_type.clone(),
            "success" => ctx.success.map(|b| b.to_string()),
            _ => return false,
        };
        let want_str = match want_value {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            _ => return false,
        };
        if got.as_deref() != Some(want_str.as_str()) {
            return false;
        }
    }
    true
}

pub fn count_match_fields(predicate: &Value) -> u32 {
    predicate
        .as_object()
        .map(|o| o.iter().filter(|(_, v)| !v.is_null()).count() as u32)
        .unwrap_or(0)
}

/// Convenience: read every rule from a pet.json document into a fresh
/// RuleCache.
pub fn rule_cache_from_pet(pet: &crate::template::types::PetDoc) -> Result<RuleCache> {
    Ok(RuleCache::from_template_rules(&pet.rules))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_opus_47() -> MatchContext {
        MatchContext {
            vendor: Some("anthropic".into()),
            family: Some("claude-opus".into()),
            model: Some("claude-opus-4-7".into()),
            tier: Some("frontier".into()),
            provider: Some("claude_code".into()),
            ..Default::default()
        }
    }

    fn rule(id: &str, predicate: Value, priority: i32) -> LoadedRule {
        LoadedRule {
            id: id.into(),
            source_type: "usage".into(),
            match_field_count: count_match_fields(&predicate),
            match_predicate: predicate,
            config: json!({}),
            priority,
            description: None,
        }
    }

    #[test]
    fn empty_predicate_matches_anything() {
        assert!(matches_predicate(&json!({}), &ctx_opus_47()));
    }

    #[test]
    fn exact_model_matches() {
        let p = json!({"model": "claude-opus-4-7"});
        assert!(matches_predicate(&p, &ctx_opus_47()));
        let p2 = json!({"model": "claude-opus-4-6"});
        assert!(!matches_predicate(&p2, &ctx_opus_47()));
    }

    #[test]
    fn family_matches_when_specific_does_not() {
        let p = json!({"family": "claude-opus"});
        assert!(matches_predicate(&p, &ctx_opus_47()));
    }

    #[test]
    fn missing_context_field_fails() {
        let p = json!({"agent_type": "Plan"});
        assert!(!matches_predicate(&p, &ctx_opus_47()));
    }

    #[test]
    fn resolver_picks_highest_priority_field_count() {
        let cache = RuleCache {
            rules: vec![
                rule("default", json!({}), 10),
                rule("frontier", json!({"tier": "frontier"}), 100),
                rule("family_opus", json!({"family": "claude-opus"}), 200),
                rule("model_47", json!({"model": "claude-opus-4-7"}), 300),
            ],
        };
        let best = cache.resolve(XpSourceType::Usage, &ctx_opus_47());
        assert_eq!(best.unwrap().id, "model_47");
    }

    #[test]
    fn resolver_falls_back_when_specific_doesnt_match() {
        let cache = RuleCache {
            rules: vec![
                rule("default", json!({}), 10),
                rule("family_opus", json!({"family": "claude-opus"}), 200),
                rule("model_46", json!({"model": "claude-opus-4-6"}), 300),
            ],
        };
        let best = cache.resolve(XpSourceType::Usage, &ctx_opus_47());
        assert_eq!(best.unwrap().id, "family_opus");
    }

    #[test]
    fn unknown_predicate_key_fails_safe() {
        let p = json!({"weird_unknown_key": "x"});
        assert!(!matches_predicate(&p, &ctx_opus_47()));
    }
}
