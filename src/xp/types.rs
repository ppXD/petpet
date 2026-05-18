//! Shared types for the XP system.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{ActivityEvent, ActivityKind, UsageEvent};

/// All the discrete kinds of XP source we know how to compute.
/// Adding a new source = new variant + new Scorer + new seed rule.
#[derive(Debug, Clone)]
pub enum XpSource<'a> {
    Usage(&'a UsageEvent),
    Activity(&'a ActivityEvent),
    Manual(ManualGrant),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XpSourceType {
    Usage,
    Activity,
    Manual,
    Daily,
}

impl XpSourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            XpSourceType::Usage => "usage",
            XpSourceType::Activity => "activity",
            XpSourceType::Manual => "manual",
            XpSourceType::Daily => "daily",
        }
    }
}

/// Manual XP grant — admin / CLI / promo / penalty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualGrant {
    pub xp_delta: i64,
    pub reason: String,
    /// Deterministic ID for dedup; caller is responsible for uniqueness.
    pub ref_id: String,
}

/// What a scorer returns for one input. The XPEngine attaches this to an
/// xp_event row.
#[derive(Debug, Clone)]
pub struct XpComputation {
    pub xp_delta: i64,
    pub reason: String,
    pub rule_id: RuleId,
}

pub type RuleId = String;

/// The pet object as stored in the `pet` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pet {
    pub id: String,
    pub name: String,
    /// Which template the pet was instantiated from (e.g. "ember"). The
    /// template itself can be edited/deleted afterwards — the pet's own
    /// snapshot (`snapshot_path/pet.json`) is what determines behavior.
    pub template_id: String,
    /// Absolute path to this pet's snapshot folder
    /// (`~/.petpet/pets/<uuid>/`) holding pet.json + asset copies.
    pub snapshot_path: String,
    pub born_at: DateTime<Utc>,
    pub is_active: bool,
    pub origin_device_id: String,
    /// When the user finalized naming at the hatch-time ceremony.
    /// `None` = naming still mutable; `Some(ts)` = locked forever.
    pub name_finalized_at: Option<DateTime<Utc>>,
}

/// One row of `pet_stage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PetStageRow {
    pub species_id: String,
    pub level: u32,
    pub name: String,
    pub xp_required: i64,
    pub sprite_key: String,
    pub flavor: Option<String>,
    /// Arbitrary JSON the frontend uses to drive stage-entry ceremonies
    /// (overlays, modals, custom hardcoded scenes). Backend stays agnostic.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Predicate input — all the dimensions a rule's `match` JSON can filter on.
/// Optional everywhere because predicates are AND-of-optional: a rule with
/// `{"family":"claude-opus"}` only constrains `family`; `model`, `tool`, etc.
/// are wildcards.
#[derive(Debug, Default, Clone)]
pub struct MatchContext {
    // Model-derived (for usage events)
    pub vendor: Option<String>,
    pub family: Option<String>,
    pub model: Option<String>,
    pub raw: Option<String>,
    pub tier: Option<String>,

    // Source provider (for both usage and activity)
    pub provider: Option<String>,
    pub client: Option<String>,

    // Activity-only
    pub kind: Option<String>,
    pub tool: Option<String>,
    pub success: Option<bool>,
    pub agent_type: Option<String>,
}

impl MatchContext {
    pub fn from_usage(ue: &UsageEvent, ident: &crate::model::ModelIdent) -> Self {
        Self {
            vendor: Some(ident.vendor.as_str().to_string()),
            family: Some(ident.family.clone()),
            model: Some(ident.model.clone()),
            raw: Some(ue.model.clone()),
            tier: Some(ident.tier.as_str().to_string()),
            provider: Some(ue.provider.as_str().to_string()),
            client: ue.client.clone(),
            ..Default::default()
        }
    }

    pub fn from_activity(ae: &ActivityEvent) -> Self {
        let (kind, tool, success, agent_type) = match &ae.kind {
            ActivityKind::SessionStart { .. } => ("session_start".into(), None, None, None),
            ActivityKind::SessionEnd { .. } => ("session_end".into(), None, None, None),
            ActivityKind::UserPromptSubmit { .. } => {
                ("user_prompt_submit".into(), None, None, None)
            }
            ActivityKind::UserPromptExpansion { .. } => {
                ("user_prompt_expansion".into(), None, None, None)
            }
            ActivityKind::AssistantStop => ("assistant_stop".into(), None, None, None),
            ActivityKind::AssistantStopFailure { .. } => {
                ("assistant_stop_failure".into(), None, None, None)
            }
            ActivityKind::ToolUseStart { name, .. } => {
                ("tool_use_start".into(), Some(name.clone()), None, None)
            }
            ActivityKind::ToolUseEnd { name, success, .. } => (
                "tool_use_end".into(),
                Some(name.clone()),
                Some(*success),
                None,
            ),
            ActivityKind::ToolBatchEnd { .. } => ("tool_batch_end".into(), None, None, None),
            ActivityKind::PermissionRequest { tool_name } => (
                "permission_request".into(),
                Some(tool_name.clone()),
                None,
                None,
            ),
            ActivityKind::PermissionDenied { tool_name, .. } => (
                "permission_denied".into(),
                Some(tool_name.clone()),
                None,
                None,
            ),
            ActivityKind::SubagentStart { agent_type } => (
                "subagent_start".into(),
                None,
                None,
                Some(agent_type.clone()),
            ),
            ActivityKind::SubagentStop { agent_type } => (
                "subagent_stop".into(),
                None,
                None,
                Some(agent_type.clone()),
            ),
            ActivityKind::TaskCreated { .. } => ("task_created".into(), None, None, None),
            ActivityKind::TaskCompleted { .. } => ("task_completed".into(), None, None, None),
            ActivityKind::PreCompact { .. } => ("pre_compact".into(), None, None, None),
            ActivityKind::PostCompact { .. } => ("post_compact".into(), None, None, None),
            ActivityKind::Notification { .. } => ("notification".into(), None, None, None),
            ActivityKind::Other { event_name } => (event_name.to_lowercase(), None, None, None),
        };
        let kind: String = kind;
        Self {
            provider: Some(ae.provider.as_str().to_string()),
            kind: Some(kind),
            tool,
            success,
            agent_type,
            ..Default::default()
        }
    }
}

/// One typed activity input record (used by ActivityScorer).
#[derive(Debug, Clone)]
pub struct ActivityInput {
    pub kind: String,
    pub tool: Option<String>,
    pub success: Option<bool>,
    pub agent_type: Option<String>,
}
