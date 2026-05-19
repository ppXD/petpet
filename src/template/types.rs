//! Template + pet snapshot types.
//!
//! Schema (post-refactor):
//!
//! ```text
//! Template
//! ├── meta, species, labels, theme, assets       (identity / catalog)
//! ├── levels: LevelCurve                          (explicit per-level XP)
//! ├── stages: Vec<Stage>                          (visual evolutions, 0-indexed)
//! │   └── each Stage:
//! │       ├── id ("stage_N")
//! │       ├── trigger: polymorphic AND/OR
//! │       ├── assets (sprite + frames)
//! │       ├── attributes (free-form JSON)
//! │       └── events: HashMap<event_name, ceremony array>
//! │              ← loaded from on_*.json / idle.json sidecars
//! └── rules: Vec<TemplateRule>                    (XP scoring)
//! ```

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ═══════════════════════════════════════════════════════════════
// Top-level Template
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    #[serde(rename = "$schema", default = "default_template_schema")]
    pub schema: String,

    pub meta: TemplateMeta,
    pub species: TemplateSpecies,

    #[serde(default)]
    pub labels: Vec<Label>,

    #[serde(default)]
    pub theme: TemplateTheme,

    #[serde(default)]
    pub assets: TemplateAssets,

    /// Per-level XP curve. Required at load time (loader merges
    /// `levels.json` sidecar if missing here).
    #[serde(default)]
    pub levels: LevelCurve,

    /// Sequential evolution stages. Required at load time (loader scans
    /// `stages/stage_N/` folders if not inlined here).
    #[serde(default)]
    pub stages: Vec<Stage>,

    /// XP scoring rules. May be inlined or in `rules.json` sidecar.
    #[serde(default)]
    pub rules: Vec<TemplateRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    /// Display ordering hint for the egg-picker UI. Lower = shown
    /// earlier. Templates without an explicit value (e.g. user-imported
    /// community ones) sort after explicit-order templates, alphabetically.
    /// Builtin difficulty ladder: unicorn=1, sun=2, kingkong=3.
    #[serde(default)]
    pub display_order: Option<i32>,
}

/// Author shape on disk. Templates in the wild use either:
///   - bare string: `"author": "petpet-builtin"` (the 3 builtins
///     ship this form for historical reasons; npm-style brevity)
///   - object: `"author": {"name": "Mars", "url": "..."}` (richer,
///     what `template_create` writes when scaffolding so the
///     creator's optional URL field has a place to live)
///
/// Both load. The untagged enum picks the matching variant by
/// shape — strings deserialize as `Simple`, objects as `Detailed`.
/// Re-serializing round-trips each variant verbatim, so a hand-
/// edited template.json keeps its chosen form across pack/unpack.
///
/// Before this enum existed, `author` was `Option<String>`; user
/// templates scaffolded by the creator wrote the object form,
/// failed deserialization at load time ("invalid type: map,
/// expected a string"), and got silently dropped from
/// `template_list` — invisible to the user with no error banner.
/// Hence the test below pins both shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Author {
    Simple(String),
    Detailed {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
}

impl Author {
    /// The display name to render in the picker. None when an
    /// `Author::Detailed { name: None, .. }` was authored — that's
    /// valid JSON but useless to display; caller falls back to id
    /// prefix or source label.
    pub fn name(&self) -> Option<&str> {
        match self {
            Author::Simple(s) => Some(s.as_str()),
            Author::Detailed { name, .. } => name.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSpecies {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default_pet_name: Option<String>,
    #[serde(default)]
    pub flavor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateTheme {
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub secondary: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub palette: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateAssets {
    #[serde(default)]
    pub sheet: Option<String>,
    #[serde(default)]
    pub frames: Option<String>,
    #[serde(default)]
    pub thumb: Option<String>,
}

// ═══════════════════════════════════════════════════════════════
// Labels (kept from previous design)
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Label {
    Simple(String),
    Detailed {
        text: String,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        fg: Option<String>,
    },
}

impl Label {
    pub fn text(&self) -> &str {
        match self {
            Label::Simple(s) => s,
            Label::Detailed { text, .. } => text,
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// LevelCurve — explicit per-level XP table
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LevelCurve {
    pub max_level: u32,
    /// Dense entries from level 0 to max_level inclusive.
    /// Loader validates monotonicity + density.
    pub entries: Vec<LevelEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LevelEntry {
    pub level: u32,
    pub xp_required: i64,
}

impl LevelCurve {
    /// Highest level with `xp_required <= total_xp`. O(log N) via binary
    /// search (entries are pre-sorted by level / xp_required).
    pub fn current_level(&self, total_xp: i64) -> u32 {
        // Linear is fine for 100 entries; keeps logic simple.
        let mut best = 0;
        for e in &self.entries {
            if e.xp_required <= total_xp {
                best = e.level;
            } else {
                break;
            }
        }
        best
    }

    /// XP required to reach a specific level. None if level > max_level.
    pub fn xp_for_level(&self, level: u32) -> Option<i64> {
        self.entries
            .iter()
            .find(|e| e.level == level)
            .map(|e| e.xp_required)
    }

    /// XP cost to go from `level` to `level+1`. None at max.
    pub fn xp_for_next_level(&self, level: u32) -> Option<i64> {
        if level >= self.max_level {
            return None;
        }
        let cur = self.xp_for_level(level)?;
        let next = self.xp_for_level(level + 1)?;
        Some(next - cur)
    }
}

// ═══════════════════════════════════════════════════════════════
// Stage — one visual evolution
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    /// Equals folder name "stage_N". Loader validates this.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub flavor: Option<String>,

    /// Activation condition (polymorphic AND/OR).
    pub trigger: Trigger,

    #[serde(default)]
    pub assets: StageAssets,

    /// Free-form per-stage metadata (size/element/personality/etc.).
    /// Frontend may pick keys it understands and ignore the rest.
    #[serde(default = "default_attributes")]
    pub attributes: Value,

    /// Auto-discovered event handlers (filename → ceremony array).
    /// Loader scans `on_*.json` + `idle.json` files in the stage folder
    /// and populates this map. Author never writes this field manually
    /// (it's a runtime convenience).
    #[serde(default)]
    pub events: BTreeMap<String, Vec<Value>>,
}

fn default_attributes() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageAssets {
    #[serde(default)]
    pub sprite: Option<String>,
    #[serde(default)]
    pub frames: Option<String>,
}

// ═══════════════════════════════════════════════════════════════
// Trigger — generic predicate + structural AND / OR composition
// ═══════════════════════════════════════════════════════════════
//
// Leaf nodes are `Predicate { metric, op, value }` — fully generic.
// Composites (`all_of` / `any_of`) preserve structural semantics.
// New metrics (e.g. `events_count`, `streak_days`) are added by:
//   1. Appending to `KNOWN_METRICS` below.
//   2. Inserting into the runtime `TriggerContext` at every call site.
// No enum modifications required.

/// Activation condition. Untagged JSON: composites match by presence
/// of `all_of` / `any_of`; leaves match by presence of `metric` + `value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Trigger {
    AllOf { all_of: Vec<Trigger> },
    AnyOf { any_of: Vec<Trigger> },
    Leaf(Predicate),
}

/// A single comparison against one metric in the runtime context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Predicate {
    pub metric: String,
    #[serde(default)]
    pub op: Op,
    pub value: f64,
}

/// Comparison operator. Defaults to `>=` so most stage gates can omit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Op {
    #[default]
    #[serde(rename = ">=")]
    Gte,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Lte,
}

/// Metric names the engine populates in `TriggerContext`. Templates
/// referencing anything not in `KNOWN_METRICS` are rejected at load
/// time. Adding a new metric: append below + insert into context at
/// every call site (no enum changes).
pub const METRIC_LEVEL: &str = "level";
pub const METRIC_XP_TOTAL: &str = "xp_total";
pub const METRIC_PET_AGE_DAYS: &str = "pet_age_days";

pub const KNOWN_METRICS: &[&str] = &[METRIC_LEVEL, METRIC_XP_TOTAL, METRIC_PET_AGE_DAYS];

impl Trigger {
    /// Minimum `level` that could possibly satisfy this trigger. Used
    /// by the loader to enforce stage trigger monotonicity. Predicates
    /// on other metrics return 0 (no level constraint).
    pub fn min_level_required(&self) -> u32 {
        match self {
            Trigger::Leaf(p) if p.metric == METRIC_LEVEL => match p.op {
                Op::Gte | Op::Eq => p.value.max(0.0) as u32,
                Op::Gt => (p.value.max(-1.0) as u32).saturating_add(1),
                _ => 0,
            },
            Trigger::Leaf(_) => 0,
            Trigger::AllOf { all_of } => all_of
                .iter()
                .map(|t| t.min_level_required())
                .max()
                .unwrap_or(0),
            Trigger::AnyOf { any_of } => any_of
                .iter()
                .map(|t| t.min_level_required())
                .min()
                .unwrap_or(0),
        }
    }

    /// Evaluate against runtime context.
    pub fn evaluate(&self, ctx: &TriggerContext) -> bool {
        match self {
            Trigger::Leaf(p) => {
                let actual = ctx.get(&p.metric);
                match p.op {
                    Op::Gte => actual >= p.value,
                    Op::Gt => actual > p.value,
                    Op::Eq => (actual - p.value).abs() < f64::EPSILON,
                    Op::Lt => actual < p.value,
                    Op::Lte => actual <= p.value,
                }
            }
            Trigger::AllOf { all_of } => all_of.iter().all(|t| t.evaluate(ctx)),
            Trigger::AnyOf { any_of } => any_of.iter().any(|t| t.evaluate(ctx)),
        }
    }

    /// Conservative depth check to prevent runaway recursion in malicious
    /// or malformed templates.
    pub fn depth(&self) -> u32 {
        match self {
            Trigger::AllOf { all_of } => 1 + all_of.iter().map(|t| t.depth()).max().unwrap_or(0),
            Trigger::AnyOf { any_of } => 1 + any_of.iter().map(|t| t.depth()).max().unwrap_or(0),
            Trigger::Leaf(_) => 1,
        }
    }

    /// Walk every predicate's metric name. Used at load time to reject
    /// templates referencing metrics the engine does not provide.
    pub fn collect_metrics<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Trigger::Leaf(p) => out.push(&p.metric),
            Trigger::AllOf { all_of } => {
                all_of.iter().for_each(|t| t.collect_metrics(out))
            }
            Trigger::AnyOf { any_of } => {
                any_of.iter().for_each(|t| t.collect_metrics(out))
            }
        }
    }
}

/// Runtime metric bag. Unknown keys read back as 0.0. The engine
/// populates known metrics at each evaluation site; load-time validation
/// ensures triggers only reference metrics that will be present.
#[derive(Debug, Clone, Default)]
pub struct TriggerContext {
    metrics: HashMap<String, f64>,
}

impl TriggerContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style insert. `ctx = TriggerContext::new().with("level", 5.0)`.
    pub fn with(mut self, key: impl Into<String>, value: f64) -> Self {
        self.metrics.insert(key.into(), value);
        self
    }

    pub fn set(&mut self, key: impl Into<String>, value: f64) {
        self.metrics.insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> f64 {
        self.metrics.get(key).copied().unwrap_or(0.0)
    }
}

// ═══════════════════════════════════════════════════════════════
// XP rules (unchanged from previous schema)
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateRule {
    pub id: String,
    pub source_type: String,
    #[serde(default, rename = "match")]
    pub match_predicate: Value,
    pub config: Value,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_priority() -> i64 {
    100
}

// ═══════════════════════════════════════════════════════════════
// PetDoc — pet snapshot file
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PetDoc {
    #[serde(rename = "$schema", default = "default_pet_schema")]
    pub schema: String,

    pub id: String,
    pub name: String,
    pub born_at: DateTime<Utc>,
    #[serde(default)]
    pub name_finalized_at: Option<DateTime<Utc>>,
    pub origin_device_id: String,

    pub origin: PetOrigin,

    pub species: TemplateSpecies,
    pub levels: LevelCurve,
    pub stages: Vec<Stage>,
    pub rules: Vec<TemplateRule>,

    #[serde(default)]
    pub theme: TemplateTheme,
    #[serde(default)]
    pub assets: TemplateAssets,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PetOrigin {
    pub template_id: String,
    pub template_version: String,
    pub source: String,
    pub snapshotted_at: DateTime<Utc>,
}

fn default_template_schema() -> String {
    "petpet-template/v1".to_string()
}

fn default_pet_schema() -> String {
    "petpet-pet/v1".to_string()
}

// ═══════════════════════════════════════════════════════════════
// Compatibility shim — convert new Stage to legacy PetStageRow used
// by XPEngine/StateManager until those are fully refactored too.
// ═══════════════════════════════════════════════════════════════

use crate::xp::types::PetStageRow;

impl PetDoc {
    /// Derive `PetStageRow` views from the new stages array for callers
    /// (like XPEngine) that still expect the legacy shape. The
    /// `level` field maps to the trigger's `min_level_required()`.
    pub fn stages_as_pet_stage_rows(&self) -> Vec<PetStageRow> {
        let species_id = self.origin.template_id.clone();
        self.stages
            .iter()
            .map(|s| PetStageRow {
                species_id: species_id.clone(),
                level: s.trigger.min_level_required(),
                name: s.name.clone(),
                xp_required: self
                    .levels
                    .xp_for_level(s.trigger.min_level_required())
                    .unwrap_or(0),
                sprite_key: s.id.clone(),
                flavor: s.flavor.clone(),
                metadata: serde_json::json!({
                    "idle": s.events.get("idle").cloned().unwrap_or_default(),
                    "on_enter": s.events.get("on_enter").cloned().unwrap_or_default(),
                }),
            })
            .collect()
    }
}

#[cfg(test)]
mod trigger_tests {
    use super::*;

    fn parse(s: &str) -> Trigger {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("parse failed for {s}: {e}"))
    }

    #[test]
    fn predicate_defaults_op_to_gte() {
        let t = parse(r#"{"metric": "level", "value": 10}"#);
        let p = match &t {
            Trigger::Leaf(p) => p,
            _ => panic!("expected Leaf"),
        };
        assert_eq!(p.metric, "level");
        assert_eq!(p.op, Op::Gte);
        assert_eq!(p.value, 10.0);
    }

    #[test]
    fn predicate_parses_explicit_op() {
        let t = parse(r#"{"metric": "xp_total", "op": ">", "value": 5000}"#);
        assert!(matches!(t, Trigger::Leaf(Predicate { op: Op::Gt, .. })));
    }

    #[test]
    fn evaluate_gte_against_context() {
        let t = parse(r#"{"metric": "level", "value": 10}"#);
        let ctx = TriggerContext::new().with(METRIC_LEVEL, 10.0);
        assert!(t.evaluate(&ctx));
        let ctx2 = TriggerContext::new().with(METRIC_LEVEL, 9.0);
        assert!(!t.evaluate(&ctx2));
    }

    #[test]
    fn evaluate_unknown_metric_reads_zero() {
        // Predicate references a metric not in context — treated as 0.0.
        let t = parse(r#"{"metric": "ghost", "value": 1}"#);
        let ctx = TriggerContext::new().with(METRIC_LEVEL, 999.0);
        assert!(!t.evaluate(&ctx));
    }

    #[test]
    fn all_of_requires_every_child() {
        let t = parse(
            r#"{"all_of": [
                {"metric": "level", "value": 5},
                {"metric": "xp_total", "value": 1000}
            ]}"#,
        );
        let pass = TriggerContext::new()
            .with(METRIC_LEVEL, 5.0)
            .with(METRIC_XP_TOTAL, 1000.0);
        let fail = TriggerContext::new()
            .with(METRIC_LEVEL, 5.0)
            .with(METRIC_XP_TOTAL, 999.0);
        assert!(t.evaluate(&pass));
        assert!(!t.evaluate(&fail));
    }

    #[test]
    fn any_of_passes_when_one_child_passes() {
        let t = parse(
            r#"{"any_of": [
                {"metric": "level", "value": 100},
                {"metric": "xp_total", "value": 10}
            ]}"#,
        );
        let ctx = TriggerContext::new()
            .with(METRIC_LEVEL, 5.0)
            .with(METRIC_XP_TOTAL, 50.0);
        assert!(t.evaluate(&ctx));
    }

    #[test]
    fn min_level_required_all_of_takes_max() {
        let t = parse(
            r#"{"all_of": [
                {"metric": "level", "value": 5},
                {"metric": "level", "value": 12},
                {"metric": "xp_total", "value": 99999}
            ]}"#,
        );
        assert_eq!(t.min_level_required(), 12);
    }

    #[test]
    fn min_level_required_any_of_takes_min() {
        let t = parse(
            r#"{"any_of": [
                {"metric": "level", "value": 30},
                {"metric": "level", "value": 50}
            ]}"#,
        );
        assert_eq!(t.min_level_required(), 30);
    }

    #[test]
    fn min_level_required_ignores_non_level_predicates() {
        let t = parse(r#"{"metric": "xp_total", "value": 5000}"#);
        assert_eq!(t.min_level_required(), 0);
    }

    #[test]
    fn collect_metrics_walks_full_tree() {
        let t = parse(
            r#"{"all_of": [
                {"metric": "level", "value": 1},
                {"any_of": [
                    {"metric": "xp_total", "value": 100},
                    {"metric": "pet_age_days", "value": 7}
                ]}
            ]}"#,
        );
        let mut found = Vec::new();
        t.collect_metrics(&mut found);
        assert_eq!(found, vec!["level", "xp_total", "pet_age_days"]);
    }
}
