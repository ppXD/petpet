//! ManualScorer: passthrough — caller supplies the XP delta directly.
//!
//! Used by:
//! - admin CLI / UI buttons (compensate user, debug nudge)
//! - achievement reward delivery (achievement unlock → manual grant for the reward)
//! - daily streak bonuses computed externally
//!
//! No rule config needed; the `ManualGrant` carries everything.

use crate::xp::types::{ManualGrant, XpComputation};

pub struct ManualScorer;

impl ManualScorer {
    pub fn score(grant: &ManualGrant) -> Option<XpComputation> {
        if grant.xp_delta == 0 {
            return None;
        }
        Some(XpComputation {
            xp_delta: grant.xp_delta,
            reason: grant.reason.clone(),
            rule_id: format!("manual:{}", grant.ref_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_grant() {
        let g = ManualGrant {
            xp_delta: 100,
            reason: "compensation".into(),
            ref_id: "admin-2026-05-15-001".into(),
        };
        let c = ManualScorer::score(&g).unwrap();
        assert_eq!(c.xp_delta, 100);
        assert_eq!(c.reason, "compensation");
        assert!(c.rule_id.starts_with("manual:"));
    }

    #[test]
    fn zero_is_noop() {
        let g = ManualGrant {
            xp_delta: 0,
            reason: "x".into(),
            ref_id: "x".into(),
        };
        assert!(ManualScorer::score(&g).is_none());
    }
}
