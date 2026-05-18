//! XPCalculator: routes inputs to the right scorer + selected rule.
//!
//! In the per-pet snapshot model, the calculator is constructed with a
//! `RuleCache` built from one pet's own rules. There is no DB lookup
//! at runtime — every rule lives in `pet.json` and was loaded once.

use crate::event::{ActivityEvent, UsageEvent};
use crate::model::ModelIdent;
use crate::xp::resolver::RuleCache;
use crate::xp::scorer::{ActivityScorer, ManualScorer, UsageScorer};
use crate::xp::types::{ManualGrant, MatchContext, XpComputation, XpSourceType};

pub struct XPCalculator {
    pub rules: RuleCache,
}

impl XPCalculator {
    pub fn new(rules: RuleCache) -> Self {
        Self { rules }
    }

    pub fn score_usage(&self, ue: &UsageEvent) -> Option<XpComputation> {
        let ident = ModelIdent::parse(&ue.model);
        let ctx = MatchContext::from_usage(ue, &ident);
        let rule = self.rules.resolve(XpSourceType::Usage, &ctx)?;
        UsageScorer::score(ue, rule)
    }

    pub fn score_activity(&self, ae: &ActivityEvent) -> Option<XpComputation> {
        let ctx = MatchContext::from_activity(ae);
        let rule = self.rules.resolve(XpSourceType::Activity, &ctx)?;
        ActivityScorer::score(ae, rule)
    }

    pub fn score_manual(&self, grant: &ManualGrant) -> Option<XpComputation> {
        ManualScorer::score(grant)
    }
}
