//! StateManager: applies XP deltas to `pet_state` and detects
//! level + stage transitions.
//!
//! Post-refactor: `current_level` is a direct lookup into a
//! `LevelCurve` (explicit per-level XP). `stage_level` is determined
//! by evaluating each stage's `trigger` against the current context
//! and picking the highest-index stage that fires.

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::db::DbHandle;
use crate::template::types::{
    LevelCurve, Stage, TriggerContext, METRIC_LEVEL, METRIC_PET_AGE_DAYS, METRIC_XP_TOTAL,
};
use crate::xp::types::PetStageRow;

#[derive(Debug, Clone)]
pub struct AppliedDelta {
    pub pet_id: String,
    pub xp_before: i64,
    pub xp_after: i64,

    /// Granular level (0..=max_level). Direct lookup from LevelCurve.
    pub current_level_before: u32,
    pub current_level_after: u32,

    /// Active stage's index (its position in the Vec<Stage>). 0-based.
    pub stage_index_before: u32,
    pub stage_index_after: u32,

    /// Convenience legacy alias — same as stage_index_*. Kept for
    /// callers that still reason about "anchor levels" (i.e. the
    /// trigger's min_level_required) rather than indices.
    pub stage_level_before: u32,
    pub stage_level_after: u32,

    pub stage_before: Option<PetStageRow>,
    pub stage_after: Option<PetStageRow>,
}

impl AppliedDelta {
    pub fn leveled_up(&self) -> bool {
        self.current_level_after > self.current_level_before
    }

    pub fn evolved(&self) -> bool {
        self.stage_index_after > self.stage_index_before
    }
}

pub struct StateManager {
    db: Arc<DbHandle>,
    /// Serialises the read-modify-write inside `apply_delta` /
    /// `rebuild` so concurrent ingest doesn't lose XP via TOCTOU.
    ///
    /// Without this lock, two concurrent `ingest_usage` calls can:
    ///   1. Task A reads pet_state.total_xp = 100
    ///   2. Task B reads pet_state.total_xp = 100 (same snapshot)
    ///   3. Task A writes 100 + 10 = 110
    ///   4. Task B writes 100 + 5 = 105     ← overwrites A's 110
    /// Net: A's 10 XP is "lost from `pet_state`" — the xp_event row
    /// is still written (different PK), so `sum(xp_event) > pet_state`.
    ///
    /// Empirically reproduced as ~8% XP loss in a 1.5-hour heavy
    /// Claude-Code session (988 XP in xp_event vs 910 XP in
    /// pet_state).
    ///
    /// petpet runs at most one active pet at a time, so a single
    /// global mutex is fine — contention is bounded by the live
    /// ingest rate (a handful of events per second worst-case).
    /// If we ever support multi-pet concurrent ingest, swap this
    /// for a per-pet mutex map.
    apply_lock: tokio::sync::Mutex<()>,
}

impl StateManager {
    pub fn new(db: Arc<DbHandle>) -> Self {
        Self {
            db,
            apply_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Apply an XP delta. The caller supplies the pet's full level
    /// curve + stages (loaded from pet.json snapshot).
    ///
    /// The whole read-modify-write of `pet_state` is held under
    /// `apply_lock` so concurrent ingest cannot lose deltas — see
    /// the struct comment for the race details.
    pub async fn apply_delta(
        &self,
        pet_id: &str,
        levels: &LevelCurve,
        stages: &[Stage],
        delta: i64,
        occurred_at: DateTime<Utc>,
        pet_age_days: u32,
    ) -> Result<AppliedDelta> {
        let _guard = self.apply_lock.lock().await;
        let prior = self.db.get_pet_state(pet_id).await?;
        let xp_before = prior.as_ref().map(|s| s.total_xp).unwrap_or(0);
        let xp_after = xp_before + delta;

        let current_level_before = levels.current_level(xp_before);
        let current_level_after = levels.current_level(xp_after);

        let ctx_before = build_ctx(current_level_before, xp_before, pet_age_days);
        let ctx_after = build_ctx(current_level_after, xp_after, pet_age_days);
        let stage_index_before = stage_index_for(stages, &ctx_before);
        let stage_index_after = stage_index_for(stages, &ctx_after);

        self.db
            .upsert_pet_state(pet_id, xp_after, current_level_after, Some(occurred_at))
            .await?;

        let stage_before = stage_to_row(stages, stage_index_before, levels);
        let stage_after = stage_to_row(stages, stage_index_after, levels);

        let stage_level_before = stages
            .get(stage_index_before as usize)
            .map(|s| s.trigger.min_level_required())
            .unwrap_or(0);
        let stage_level_after = stages
            .get(stage_index_after as usize)
            .map(|s| s.trigger.min_level_required())
            .unwrap_or(0);

        Ok(AppliedDelta {
            pet_id: pet_id.to_string(),
            xp_before,
            xp_after,
            current_level_before,
            current_level_after,
            stage_index_before,
            stage_index_after,
            stage_level_before,
            stage_level_after,
            stage_before,
            stage_after,
        })
    }

    /// Full recompute from xp_event history. Takes the same lock as
    /// `apply_delta` so a rebuild can't race with an in-flight ingest.
    pub async fn rebuild(
        &self,
        pet_id: &str,
        levels: &LevelCurve,
        stages: &[Stage],
        pet_age_days: u32,
    ) -> Result<AppliedDelta> {
        let _guard = self.apply_lock.lock().await;
        let sum = self.db.sum_xp_for_pet(pet_id).await?;
        let prior = self.db.get_pet_state(pet_id).await?;
        let xp_before = prior.as_ref().map(|s| s.total_xp).unwrap_or(0);

        let current_level_before = levels.current_level(xp_before);
        let current_level_after = levels.current_level(sum);

        let ctx_before = build_ctx(current_level_before, xp_before, pet_age_days);
        let ctx_after = build_ctx(current_level_after, sum, pet_age_days);
        let stage_index_before = stage_index_for(stages, &ctx_before);
        let stage_index_after = stage_index_for(stages, &ctx_after);

        let last_active = self.db.latest_xp_event_time(pet_id).await?;
        self.db
            .upsert_pet_state(pet_id, sum, current_level_after, last_active)
            .await?;

        let stage_before = stage_to_row(stages, stage_index_before, levels);
        let stage_after = stage_to_row(stages, stage_index_after, levels);

        Ok(AppliedDelta {
            pet_id: pet_id.to_string(),
            xp_before,
            xp_after: sum,
            current_level_before,
            current_level_after,
            stage_index_before,
            stage_index_after,
            stage_level_before: stages
                .get(stage_index_before as usize)
                .map(|s| s.trigger.min_level_required())
                .unwrap_or(0),
            stage_level_after: stages
                .get(stage_index_after as usize)
                .map(|s| s.trigger.min_level_required())
                .unwrap_or(0),
            stage_before,
            stage_after,
        })
    }
}

/// Build a `TriggerContext` populated with the engine's canonical metrics.
/// When a new metric is added to `KNOWN_METRICS`, extend this builder
/// (and add the corresponding param to plumbing call sites).
pub fn build_ctx(current_level: u32, total_xp: i64, pet_age_days: u32) -> TriggerContext {
    TriggerContext::new()
        .with(METRIC_LEVEL, current_level as f64)
        .with(METRIC_XP_TOTAL, total_xp as f64)
        .with(METRIC_PET_AGE_DAYS, pet_age_days as f64)
}

/// Find the highest-index stage whose trigger evaluates true for ctx.
/// Returns 0 if no stage matches (impossible if stage_0 has a level-0
/// trigger, which is validated at load time).
pub fn stage_index_for(stages: &[Stage], ctx: &TriggerContext) -> u32 {
    let mut best = 0u32;
    for (i, stage) in stages.iter().enumerate() {
        if stage.trigger.evaluate(ctx) {
            best = i as u32;
        }
    }
    best
}

/// Build a legacy PetStageRow view of a Stage at given index. Used by
/// engine.rs PetStateUpdate construction (which still uses the legacy
/// shape for backwards compat with frontend types).
fn stage_to_row(stages: &[Stage], index: u32, levels: &LevelCurve) -> Option<PetStageRow> {
    let stage = stages.get(index as usize)?;
    let xp = levels
        .xp_for_level(stage.trigger.min_level_required())
        .unwrap_or(0);
    Some(PetStageRow {
        species_id: String::new(),
        level: stage.trigger.min_level_required(),
        name: stage.name.clone(),
        xp_required: xp,
        sprite_key: stage.id.clone(),
        flavor: stage.flavor.clone(),
        metadata: serde_json::json!({
            "idle": stage.events.get("idle").cloned().unwrap_or_default(),
            "on_enter": stage.events.get("on_enter").cloned().unwrap_or_default(),
        }),
    })
}

/// Find the next stage strictly above current_index (for "next
/// evolution preview"). Returns None when at max stage.
pub fn next_evolution<'a>(stages: &'a [Stage], current_index: u32) -> Option<&'a Stage> {
    stages.get((current_index + 1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::types::LevelEntry;

    fn tiered_levels_fixture() -> LevelCurve {
        let mut entries = vec![LevelEntry {
            level: 0,
            xp_required: 0,
        }];
        // Hatch
        entries.push(LevelEntry {
            level: 1,
            xp_required: 4000,
        });
        // T1: L2-L10 at +50/level
        let mut prev = 4000;
        for l in 2..=10 {
            prev += 50;
            entries.push(LevelEntry {
                level: l,
                xp_required: prev,
            });
        }
        // T2: L11-L30 at +200/level
        for l in 11..=30 {
            prev += 200;
            entries.push(LevelEntry {
                level: l,
                xp_required: prev,
            });
        }
        // T3: L31-L65 at +700/level
        for l in 31..=65 {
            prev += 700;
            entries.push(LevelEntry {
                level: l,
                xp_required: prev,
            });
        }
        // T4: L66-L99 at +2000/level
        for l in 66..=99 {
            prev += 2000;
            entries.push(LevelEntry {
                level: l,
                xp_required: prev,
            });
        }
        LevelCurve {
            max_level: 99,
            entries,
        }
    }

    #[test]
    fn level_curve_exact_lookup() {
        let lc = tiered_levels_fixture();
        assert_eq!(lc.current_level(0), 0);
        assert_eq!(lc.current_level(4000), 1);
        assert_eq!(lc.current_level(4050), 2);
        assert_eq!(lc.current_level(4450), 10); // hatch + 9*50
    }

    #[test]
    fn level_curve_saturates_at_max() {
        let lc = tiered_levels_fixture();
        let max_xp = lc.entries.last().unwrap().xp_required;
        assert_eq!(lc.current_level(max_xp), 99);
        assert_eq!(lc.current_level(max_xp + 10_000_000), 99);
    }

    #[test]
    fn level_curve_xp_for_next_level() {
        let lc = tiered_levels_fixture();
        // L1 → L2 costs T1 = 50
        assert_eq!(lc.xp_for_next_level(1), Some(50));
        // L10 → L11 costs T2 = 200
        assert_eq!(lc.xp_for_next_level(10), Some(200));
        // L65 → L66 costs T4 = 2000
        assert_eq!(lc.xp_for_next_level(65), Some(2000));
        // L99 = max → None
        assert_eq!(lc.xp_for_next_level(99), None);
    }

    // ─── Concurrent ingest race regression ──────────────────────────
    //
    // Before this PR, two concurrent `apply_delta` calls could race on
    // the pet_state read-modify-write — both read the same prior xp,
    // both wrote prior+own_delta, the later write overwrote the
    // earlier. Net: one delta's worth of XP was silently dropped from
    // `pet_state` even though both `xp_event` rows landed. Empirically
    // ~8% XP loss over a heavy ingest session.
    //
    // The fix is a single Mutex held across the read-modify-write.
    // This test fires N concurrent applies of +1 XP each and asserts
    // the final state matches N exactly — pre-fix, a few would be
    // lost; post-fix, none.
    #[tokio::test]
    async fn apply_delta_serialises_concurrent_ingest() {
        use crate::db::DbHandle;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let pet_id = "race-test-pet";
        db.insert_pet(
            pet_id,
            "Racer",
            "test-template",
            "/tmp/snap",
            chrono::Utc::now(),
            true,
            "device",
        )
        .await
        .expect("insert pet");
        db.upsert_pet_state(pet_id, 0, 0, None).await.expect("init state");

        let levels = tiered_levels_fixture();
        let stages: Vec<Stage> = Vec::new();
        let mgr = Arc::new(StateManager::new(db.clone()));
        let now = chrono::Utc::now();

        // Fan out N concurrent apply_delta(+1) calls.
        let n: usize = 200;
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let mgr = mgr.clone();
            let levels = levels.clone();
            let stages = stages.clone();
            let pet_id = pet_id.to_string();
            handles.push(tokio::spawn(async move {
                mgr.apply_delta(&pet_id, &levels, &stages, 1, now, 0)
                    .await
                    .expect("apply_delta")
            }));
        }
        for h in handles {
            let _ = h.await.expect("task");
        }

        // After N applies of +1 each, pet_state.total_xp MUST be N.
        // Pre-fix this test would fail with total_xp < N (some
        // concurrent writes lost).
        let final_state = db
            .get_pet_state(pet_id)
            .await
            .expect("state")
            .expect("row");
        assert_eq!(
            final_state.total_xp, n as i64,
            "concurrent apply_delta lost XP: expected {n}, got {}",
            final_state.total_xp
        );
    }
}
