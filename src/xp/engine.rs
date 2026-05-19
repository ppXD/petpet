//! XPEngine: top-level orchestrator binding everything together.
//!
//! Per-pet snapshot model: each active pet has its own `PetDoc` loaded
//! from disk (`pet.json`), and its own `XPCalculator` built from that
//! doc's rule set. The engine holds at most one active pet at a time;
//! switching pets reloads from disk.
//!
//! Entry points (called by the desktop ingestion fan-out task):
//! - [`XPEngine::ingest_usage`] when a usage event lands
//! - [`XPEngine::ingest_activity`] when a hook / log-derived activity lands
//! - [`XPEngine::grant_manual`] for CLI / admin direct grants
//! - [`XPEngine::pick_template`] for the first-time egg-selection UI

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::db::DbHandle;
use crate::event::{ActivityEvent, UsageEvent};
use crate::template::registry::TemplateRegistry;
use crate::template::snapshot::{create_pet_from_template, load_pet_doc, update_pet_identity};
use crate::template::types::{LevelCurve, PetDoc, Stage};
use crate::xp::calculator::XPCalculator;
use crate::xp::resolver::RuleCache;
use crate::xp::state::{
    build_ctx, next_evolution, stage_index_for, AppliedDelta, StateManager,
};
use crate::xp::types::{ManualGrant, Pet, PetStageRow, XpSourceType};

#[derive(Clone, Serialize)]
pub struct PetSummary {
    pub pet: Pet,
    pub current_level: u32,
    pub total_xp: i64,
    pub stage_name: String,
    /// Stage id (e.g. "stage_0", "stage_5"). Frontend uses this to
    /// detect egg-stage pets (which have no PNG sprite) so it can
    /// render a procedural egg placeholder instead of a missing image.
    pub stage_id: String,
    /// Absolute path to the sprite for the pet's CURRENT stage. May be
    /// empty if the stage has no PNG (e.g. stage_0 ships only the
    /// metadata — frontend renders an SVG placeholder in that case).
    pub sprite_path: String,
}
use crate::xp::writer::{XpEventRecord, XpEventWriter};

/// Reported back to Tauri after applying a delta.
#[derive(Debug, Clone, Serialize)]
pub struct PetStateUpdate {
    pub pet_id: String,
    pub species_id: String,
    pub name: String,
    pub name_finalized: bool,

    pub total_xp: i64,
    pub current_level: u32,
    pub xp_in_level: i64,
    pub xp_for_next_level: Option<i64>,

    pub stage_level: u32,
    pub stage_name: Option<String>,
    pub sprite_key: Option<String>,
    pub stage_flavor: Option<String>,

    pub next_evolution_level: Option<u32>,
    pub next_evolution_name: Option<String>,
    pub xp_to_next_evolution: Option<i64>,

    pub leveled_up: bool,
    pub level_before: u32,
    pub level_after: u32,
    pub evolved: bool,
    pub stage_level_before: u32,
    pub stage_level_after: u32,
    pub level_up_flavor: Option<String>,
    pub stage_metadata: Option<serde_json::Value>,
}

impl PetStateUpdate {
    fn from(
        pet: &Pet,
        applied: &AppliedDelta,
        levels: &LevelCurve,
        stages: &[Stage],
    ) -> Self {
        let evolved = applied.evolved();
        let leveled_up = applied.leveled_up();

        let xp_at_level = levels
            .xp_for_level(applied.current_level_after)
            .unwrap_or(0);
        let xp_in_level = applied.xp_after - xp_at_level;
        let xp_for_next_level = levels.xp_for_next_level(applied.current_level_after);

        let next_stage = next_evolution(stages, applied.stage_index_after);
        let next_evolution_level = next_stage.map(|s| s.trigger.min_level_required());
        let next_evolution_name = next_stage.map(|s| s.name.clone());
        let xp_to_next_evolution = next_stage.and_then(|s| {
            levels
                .xp_for_level(s.trigger.min_level_required())
                .map(|target| target - applied.xp_after)
        });

        let cur_stage = stages.get(applied.stage_index_after as usize);

        Self {
            pet_id: pet.id.clone(),
            species_id: pet.template_id.clone(),
            name: pet.name.clone(),
            name_finalized: pet.name_finalized_at.is_some(),
            total_xp: applied.xp_after,
            current_level: applied.current_level_after,
            xp_in_level,
            xp_for_next_level,
            stage_level: applied.stage_level_after,
            stage_name: cur_stage.map(|s| s.name.clone()),
            sprite_key: cur_stage.map(|s| s.id.clone()),
            stage_flavor: cur_stage.and_then(|s| s.flavor.clone()),
            next_evolution_level,
            next_evolution_name,
            xp_to_next_evolution,
            leveled_up,
            level_before: applied.current_level_before,
            level_after: applied.current_level_after,
            evolved,
            stage_level_before: applied.stage_level_before,
            stage_level_after: applied.stage_level_after,
            level_up_flavor: cur_stage
                .filter(|_| evolved)
                .and_then(|s| s.flavor.clone()),
            stage_metadata: if evolved {
                cur_stage.map(|s| {
                    serde_json::json!({
                        "events": s.events,
                        "attributes": s.attributes,
                    })
                })
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct XPEngineSnapshot {
    pub pet: Option<Pet>,
    pub state: Option<XPStateView>,
    pub stage: Option<PetStageRow>,
    pub next_evolution: Option<NextEvolutionView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct XPStateView {
    pub total_xp: i64,
    pub current_level: u32,
    pub xp_in_level: i64,
    pub xp_for_next_level: Option<i64>,
    pub stage_level: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextEvolutionView {
    pub level: u32,
    pub name: String,
    pub sprite_key: String,
    pub xp_to_next: i64,
}

/// In-memory state for the currently active pet: identity row + parsed
/// snapshot (`pet.json`) + a calculator built from that snapshot's rules.
struct ActivePet {
    pet: Pet,
    doc: PetDoc,
    calculator: XPCalculator,
}

impl ActivePet {
    fn levels(&self) -> &LevelCurve {
        &self.doc.levels
    }
    fn stages(&self) -> &[Stage] {
        &self.doc.stages
    }
    fn pet_age_days(&self) -> u32 {
        let now = chrono::Utc::now();
        let dur = now.signed_duration_since(self.doc.born_at);
        dur.num_days().max(0) as u32
    }
}

pub struct XPEngine {
    db: Arc<DbHandle>,
    state: StateManager,
    origin_device_id: String,
    active: RwLock<Option<ActivePet>>,
}

impl XPEngine {
    pub async fn open(db: Arc<DbHandle>) -> Result<Arc<Self>> {
        let origin_device_id = db.ensure_install_id().await?;
        let state = StateManager::new(db.clone());
        let mut engine = Self {
            db,
            state,
            origin_device_id,
            active: RwLock::new(None),
        };
        // Load active pet (if any) from DB + disk
        if let Some(pet) = engine.db.find_active_pet().await.ok().flatten() {
            if let Ok(active) = engine.load_active(pet).await {
                *engine.active.get_mut() = Some(active);
            }
        }
        // Spawn the background registry sync task. The task short-circuits
        // if `PETPET_REGISTRY_SYNC_DISABLED` is set, and any fetch failure
        // is logged + retried after 24h — bundled registry covers us.
        tokio::spawn(crate::xp::registry_sync::run());
        Ok(Arc::new(engine))
    }

    /// Rehydrate ActivePet from a `Pet` row by loading its `pet.json`.
    async fn load_active(&self, pet: Pet) -> Result<ActivePet> {
        let snapshot_path = PathBuf::from(&pet.snapshot_path);
        let doc = tokio::task::spawn_blocking(move || load_pet_doc(&snapshot_path)).await??;
        let rules = RuleCache::from_template_rules(&doc.rules);
        let calculator = XPCalculator::new(rules);
        Ok(ActivePet {
            pet,
            doc,
            calculator,
        })
    }

    pub async fn refresh_active_pet(&self) -> Result<()> {
        let pet = self.db.find_active_pet().await?;
        let new_active = match pet {
            Some(p) => Some(self.load_active(p).await?),
            None => None,
        };
        *self.active.write().await = new_active;
        Ok(())
    }

    /// List every pet on disk, freshest first. Used by the "switch
    /// companion" picker to let the user pick a previously-raised pet
    /// instead of creating a new one from a template.
    pub async fn list_pets(&self) -> Result<Vec<Pet>> {
        self.db.list_pets().await
    }

    /// Build a lightweight summary for every pet — Pokémon-party style:
    /// current level, current stage's name + sprite path. Used by the
    /// switcher UI so each row can show the pet's *current* appearance
    /// and level, not just its template's stage_1 default.
    pub async fn summarize_all(&self) -> Result<Vec<PetSummary>> {
        use crate::xp::state::{build_ctx, stage_index_for};
        use std::collections::HashMap;
        let pets = self.db.list_pets().await?;

        // Pet snapshots store only `pet.json`; the actual stage sprite
        // PNGs live in the template directory (same place the active-
        // -pet renderer reads them). Resolve each pet's template dir
        // up-front via the registry so we can build correct sprite
        // paths below.
        let templates = tokio::task::spawn_blocking(TemplateRegistry::discover).await??;
        let template_dirs: HashMap<String, PathBuf> = templates
            .into_iter()
            .map(|t| (t.template.meta.id.clone(), t.dir))
            .collect();

        let mut out = Vec::with_capacity(pets.len());
        for pet in pets {
            let state_row = self.db.get_pet_state(&pet.id).await?;
            let total_xp = state_row.as_ref().map(|s| s.total_xp).unwrap_or(0);
            let snapshot_path = PathBuf::from(&pet.snapshot_path);
            // load_pet_doc reads `pet.json` synchronously — push to the
            // blocking pool so we don't stall the runtime in a tight
            // loop over many pets.
            let doc_res = tokio::task::spawn_blocking(move || load_pet_doc(&snapshot_path)).await?;
            let doc = match doc_res {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(pet_id = %pet.id, error = %e, "summarize: skip pet — load_pet_doc failed");
                    continue;
                }
            };
            let current_level = doc.levels.current_level(total_xp);
            let pet_age_days = {
                let dur = chrono::Utc::now().signed_duration_since(doc.born_at);
                dur.num_days().max(0) as u32
            };
            let ctx = build_ctx(current_level, total_xp, pet_age_days);
            let stage_index = stage_index_for(&doc.stages, &ctx);
            let stage = doc.stages.get(stage_index as usize);
            let stage_name = stage.map(|s| s.name.clone()).unwrap_or_default();
            let stage_id = stage
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "stage_0".to_string());
            let sprite_path = match template_dirs.get(&pet.template_id) {
                Some(dir) => {
                    // Honest reporting: return the current stage's
                    // sprite path only if it exists on disk. Egg-stage
                    // pets (stage_0) typically ship no PNG, so the
                    // string stays empty and the frontend renders the
                    // procedural egg placeholder. We deliberately do
                    // NOT fall back to stage_1 — that would lie about
                    // an egg's appearance.
                    let primary = dir.join("stages").join(&stage_id).join("sprite.png");
                    if primary.exists() {
                        primary.to_string_lossy().to_string()
                    } else {
                        String::new()
                    }
                }
                None => {
                    tracing::warn!(
                        pet_id = %pet.id,
                        template_id = %pet.template_id,
                        "summarize: template dir not found — sprite_path will be empty",
                    );
                    String::new()
                }
            };
            out.push(PetSummary {
                pet,
                current_level,
                total_xp,
                stage_name,
                stage_id,
                sprite_path,
            });
        }
        Ok(out)
    }

    /// Mark `pet_id` as the only active pet and rehydrate the in-memory
    /// active state. Subsequent `ingest_*` / `snapshot` calls operate
    /// on this pet.
    pub async fn set_active_pet(&self, pet_id: &str) -> Result<()> {
        self.db.set_only_active_pet(pet_id).await?;
        self.refresh_active_pet().await?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn active_pet(&self) -> Option<Pet> {
        self.active.read().await.as_ref().map(|a| a.pet.clone())
    }

    /// Ingest a token-bearing usage event.
    pub async fn ingest_usage(&self, ue: &UsageEvent) -> Result<Option<PetStateUpdate>> {
        let guard = self.active.read().await;
        let Some(active) = guard.as_ref() else { return Ok(None) };
        // Pets don't earn XP from events that occurred before they
        // were born. The live JSONL watcher snaps cursors to EOF on
        // first-seen so this is mostly defensive — but if a backlog
        // ever flushes (clock skew, lag spike, watcher restart after a
        // gap), a freshly-picked pet should NOT pick up a month's
        // worth of XP retroactively. The historical-import lane uses
        // a separate sink that bypasses this entirely.
        if ue.timestamp < active.pet.born_at {
            return Ok(None);
        }
        // Fetch the pet's level BEFORE this event for the algorithm's
        // growth curve. Falls back to 0 for brand-new pets with no
        // state row yet.
        let pet_level = self
            .db
            .get_pet_state(&active.pet.id)
            .await?
            .map(|s| active.levels().current_level(s.total_xp))
            .unwrap_or(0);
        let Some(comp) = active.calculator.score_usage(ue, pet_level) else { return Ok(None) };
        let rec = XpEventRecord::new(
            &active.pet.id,
            XpSourceType::Usage,
            Some(ue.id.to_string()),
            &comp,
            ue.timestamp,
            &self.origin_device_id,
        );
        let inserted = XpEventWriter::write(&self.db, &rec).await?;
        if !inserted {
            return Ok(None);
        }
        let applied = self
            .state
            .apply_delta(
                &active.pet.id,
                active.levels(),
                active.stages(),
                comp.xp_delta,
                ue.timestamp,
                active.pet_age_days(),
            )
            .await?;
        Ok(Some(PetStateUpdate::from(
            &active.pet,
            &applied,
            active.levels(),
            active.stages(),
        )))
    }

    /// Ingest an activity event.
    pub async fn ingest_activity(&self, ae: &ActivityEvent) -> Result<Option<PetStateUpdate>> {
        let guard = self.active.read().await;
        let Some(active) = guard.as_ref() else { return Ok(None) };
        // See `ingest_usage` — pets don't earn XP from activities that
        // happened before they were born.
        if ae.timestamp < active.pet.born_at {
            return Ok(None);
        }
        let Some(comp) = active.calculator.score_activity(ae) else { return Ok(None) };
        let rec = XpEventRecord::new(
            &active.pet.id,
            XpSourceType::Activity,
            Some(ae.id.to_string()),
            &comp,
            ae.timestamp,
            &self.origin_device_id,
        );
        let inserted = XpEventWriter::write(&self.db, &rec).await?;
        if !inserted {
            return Ok(None);
        }
        let applied = self
            .state
            .apply_delta(
                &active.pet.id,
                active.levels(),
                active.stages(),
                comp.xp_delta,
                ae.timestamp,
                active.pet_age_days(),
            )
            .await?;
        Ok(Some(PetStateUpdate::from(
            &active.pet,
            &applied,
            active.levels(),
            active.stages(),
        )))
    }

    pub async fn grant_manual(&self, grant: ManualGrant) -> Result<Option<PetStateUpdate>> {
        let guard = self.active.read().await;
        let Some(active) = guard.as_ref() else { return Ok(None) };
        let Some(comp) = active.calculator.score_manual(&grant) else { return Ok(None) };
        let now = Utc::now();
        let rec = XpEventRecord::new(
            &active.pet.id,
            XpSourceType::Manual,
            Some(grant.ref_id.clone()),
            &comp,
            now,
            &self.origin_device_id,
        );
        let inserted = XpEventWriter::write(&self.db, &rec).await?;
        if !inserted {
            return Ok(None);
        }
        let applied = self
            .state
            .apply_delta(
                &active.pet.id,
                active.levels(),
                active.stages(),
                comp.xp_delta,
                now,
                active.pet_age_days(),
            )
            .await?;
        Ok(Some(PetStateUpdate::from(
            &active.pet,
            &applied,
            active.levels(),
            active.stages(),
        )))
    }

    /// Dev helper: wipe every `xp_event` for the active pet and
    /// recompute pet_state — leaves the pet identity + snapshot intact
    /// but resets level/stage back to 0/egg. Returns the post-reset
    /// `PetStateUpdate` (with `evolved=false`, `leveled_up` true only
    /// if the pet was above level 0 before — same diff semantics as a
    /// real XP delta).
    pub async fn reset_active_xp(&self) -> Result<Option<PetStateUpdate>> {
        let guard = self.active.read().await;
        let Some(active) = guard.as_ref() else { return Ok(None) };
        self.db.delete_xp_events_for_pet(&active.pet.id).await?;
        let applied = self
            .state
            .rebuild(
                &active.pet.id,
                active.levels(),
                active.stages(),
                active.pet_age_days(),
            )
            .await?;
        Ok(Some(PetStateUpdate::from(
            &active.pet,
            &applied,
            active.levels(),
            active.stages(),
        )))
    }

    /// Create a fresh pet from a template id. Snapshots the template's
    /// stages/rules/assets into `~/.petpet/pets/<uuid>/` and inserts a
    /// row in the `pet` table. Becomes the active pet.
    pub async fn pick_template(
        &self,
        template_id: &str,
        name: Option<String>,
    ) -> Result<Pet> {
        let template_id_owned = template_id.to_string();
        let loaded = tokio::task::spawn_blocking(move || TemplateRegistry::find(&template_id_owned))
            .await??
            .ok_or_else(|| anyhow!("unknown template: {}", template_id))?;
        let validated = validate_name(name.as_deref())?;
        // The name typed in the egg picker (if any) is the pet's
        // INITIAL display name only — it never auto-finalizes naming.
        // Prior to this commit we treated picker-supplied names as
        // implicit finalize, which silenced the hatch-ceremony naming
        // popup forever and made the experience inconsistent (a user
        // who typed in the picker never saw the popup; a user who
        // left it blank did). Naming is now ALWAYS finalized through
        // the popup, by either Confirm (keep current / type new) or
        // Skip (leave mutable, popup re-fires next app session).
        let resolved_name = validated.unwrap_or_else(|| default_pet_name(&loaded.template.species));

        let origin_device_id = self.origin_device_id.clone();
        let created = tokio::task::spawn_blocking(move || {
            create_pet_from_template(&loaded, resolved_name, origin_device_id)
        })
        .await??;

        let pet = self
            .db
            .insert_pet(
                &created.doc.id,
                &created.doc.name,
                &created.doc.origin.template_id,
                created.snapshot_dir.to_string_lossy().as_ref(),
                created.doc.born_at,
                true,
                &self.origin_device_id,
            )
            .await?;
        self.db.set_only_active_pet(&pet.id).await?;
        self.db.upsert_pet_state(&pet.id, 0, 0, None).await?;

        let active = self.load_active(pet.clone()).await?;
        *self.active.write().await = Some(active);
        Ok(pet)
    }

    /// One-shot hatch-time naming. Writes the new name (or just locks).
    /// Updates both the DB row and the pet.json on disk.
    pub async fn finalize_naming(
        &self,
        pet_id: &str,
        name: Option<String>,
    ) -> Result<Pet> {
        let validated = validate_name(name.as_deref())?;
        let pet = self.db.finalize_pet_name(pet_id, validated.clone()).await?;

        // Mirror the change into pet.json on disk.
        let snapshot_path = PathBuf::from(&pet.snapshot_path);
        let final_name = pet.name.clone();
        let finalized_at = pet.name_finalized_at;
        let _ = tokio::task::spawn_blocking(move || {
            update_pet_identity(&snapshot_path, Some(final_name), finalized_at)
        })
        .await??;

        // Update active pet cache if needed.
        let mut cache = self.active.write().await;
        if cache.as_ref().map(|a| a.pet.id.as_str()) == Some(pet_id) {
            if let Some(active) = cache.as_mut() {
                active.pet = pet.clone();
                active.doc.name = pet.name.clone();
                active.doc.name_finalized_at = pet.name_finalized_at;
            }
        }
        Ok(pet)
    }

    pub async fn snapshot(&self) -> Result<XPEngineSnapshot> {
        let guard = self.active.read().await;
        let Some(active) = guard.as_ref() else {
            return Ok(XPEngineSnapshot {
                pet: None,
                state: None,
                stage: None,
                next_evolution: None,
            });
        };
        let pet_state = self.db.get_pet_state(&active.pet.id).await?;
        let total_xp = pet_state.as_ref().map(|s| s.total_xp).unwrap_or(0);
        let levels = active.levels();
        let stages = active.stages();
        let pet_age_days = active.pet_age_days();

        let current_level = levels.current_level(total_xp);
        let ctx = build_ctx(current_level, total_xp, pet_age_days);
        let stage_index = stage_index_for(stages, &ctx);

        let xp_at_level = levels.xp_for_level(current_level).unwrap_or(0);
        let xp_in_level = total_xp - xp_at_level;
        let xp_for_next_level = levels.xp_for_next_level(current_level);

        let stage = stages.get(stage_index as usize).map(|s| PetStageRow {
            species_id: active.pet.template_id.clone(),
            level: s.trigger.min_level_required(),
            name: s.name.clone(),
            xp_required: xp_at_level,
            sprite_key: s.id.clone(),
            flavor: s.flavor.clone(),
            metadata: serde_json::json!({
                "events": s.events,
                "attributes": s.attributes,
            }),
        });

        let next_view = next_evolution(stages, stage_index).map(|s| {
            let target_level = s.trigger.min_level_required();
            let target_xp = levels.xp_for_level(target_level).unwrap_or(0);
            NextEvolutionView {
                level: target_level,
                name: s.name.clone(),
                sprite_key: s.id.clone(),
                xp_to_next: target_xp - total_xp,
            }
        });

        let stage_level_now = stages
            .get(stage_index as usize)
            .map(|s| s.trigger.min_level_required())
            .unwrap_or(0);

        Ok(XPEngineSnapshot {
            pet: Some(active.pet.clone()),
            state: Some(XPStateView {
                total_xp,
                current_level,
                xp_in_level,
                xp_for_next_level,
                stage_level: stage_level_now,
            }),
            stage,
            next_evolution: next_view,
        })
    }

    /// Snapshot for any pet by id — mirrors `snapshot()` but loads the
    /// target pet's `pet.json` from disk instead of using the cached
    /// active pet. Read-only: does not change which pet is active.
    ///
    /// Used by the dashboard sidebar so the user can inspect any pet's
    /// stats without first having to set it active.
    ///
    /// Returns `Ok(snapshot_with_pet=None, …)` if no pet with this id
    /// exists; the caller treats that the same way as "no active pet".
    pub async fn snapshot_for_pet(&self, pet_id: &str) -> Result<XPEngineSnapshot> {
        let pet = match self.db.find_pet_by_id(pet_id).await? {
            Some(p) => p,
            None => {
                return Ok(XPEngineSnapshot {
                    pet: None,
                    state: None,
                    stage: None,
                    next_evolution: None,
                });
            }
        };

        // Load this pet's snapshot doc from disk (the cached `active`
        // ActivePet only holds the live pet's doc, so for any other
        // pet we read fresh).
        let snapshot_path = PathBuf::from(&pet.snapshot_path);
        let doc =
            tokio::task::spawn_blocking(move || load_pet_doc(&snapshot_path)).await??;

        let pet_state = self.db.get_pet_state(&pet.id).await?;
        let total_xp = pet_state.as_ref().map(|s| s.total_xp).unwrap_or(0);
        let levels = &doc.levels;
        let stages = &doc.stages;
        let pet_age_days = {
            let dur = chrono::Utc::now().signed_duration_since(doc.born_at);
            dur.num_days().max(0) as u32
        };

        let current_level = levels.current_level(total_xp);
        let ctx = build_ctx(current_level, total_xp, pet_age_days);
        let stage_index = stage_index_for(stages, &ctx);

        let xp_at_level = levels.xp_for_level(current_level).unwrap_or(0);
        let xp_in_level = total_xp - xp_at_level;
        let xp_for_next_level = levels.xp_for_next_level(current_level);

        let stage = stages.get(stage_index as usize).map(|s| PetStageRow {
            species_id: pet.template_id.clone(),
            level: s.trigger.min_level_required(),
            name: s.name.clone(),
            xp_required: xp_at_level,
            sprite_key: s.id.clone(),
            flavor: s.flavor.clone(),
            metadata: serde_json::json!({
                "events": s.events,
                "attributes": s.attributes,
            }),
        });

        let next_view = next_evolution(stages, stage_index).map(|s| {
            let target_level = s.trigger.min_level_required();
            let target_xp = levels.xp_for_level(target_level).unwrap_or(0);
            NextEvolutionView {
                level: target_level,
                name: s.name.clone(),
                sprite_key: s.id.clone(),
                xp_to_next: target_xp - total_xp,
            }
        });

        let stage_level_now = stages
            .get(stage_index as usize)
            .map(|s| s.trigger.min_level_required())
            .unwrap_or(0);

        Ok(XPEngineSnapshot {
            pet: Some(pet),
            state: Some(XPStateView {
                total_xp,
                current_level,
                xp_in_level,
                xp_for_next_level,
                stage_level: stage_level_now,
            }),
            stage,
            next_evolution: next_view,
        })
    }
}

/// Replay a batch of XP events for a pet and recompute its
/// `pet_state` cache row.
///
/// Why this exists: `xp_event` is the source of truth, `pet_state` is
/// the cache. Live ingestion (`grant_manual` / `ingest_usage`) goes
/// through `StateManager::apply_delta` which keeps the cache in sync
/// per event. The archive **importer** doesn't — it bulk-inserts
/// events via `XpEventWriter::replay` and then needs to recompute
/// `pet_state` once at the end, otherwise the snapshot reads
/// `total_xp = 0` and the imported pet appears at Lv.0 despite
/// having all its history in `xp_event`. (That was the regression
/// the user hit — a Lv.20+ pet exported and re-imported as Lv.0.)
///
/// Returns `(inserted, duplicates_or_skipped)`. Duplicates happen
/// when the same archive is imported twice for the same local pet —
/// the `(pet_id, source_type, source_ref)` unique index dedupes them.
///
/// `last_active_at` is derived from the latest event's
/// `occurred_at`, mirroring what `StateManager::apply_delta` would
/// have written had the events been ingested live.
pub async fn replay_events_and_recompute(
    db: &crate::db::DbHandle,
    pet_id: &str,
    pet_doc: &crate::template::types::PetDoc,
    events: &[crate::xp::writer::XpEventInsert],
) -> Result<(usize, usize)> {
    let mut inserted = 0usize;
    let mut skipped = 0usize;
    for ev in events {
        match crate::xp::writer::XpEventWriter::replay(db, ev).await {
            Ok(true) => inserted += 1,
            Ok(false) => skipped += 1,
            Err(_) => skipped += 1,
        }
    }
    let total_xp = db.sum_xp_for_pet(pet_id).await?;
    let current_level = pet_doc.levels.current_level(total_xp);
    let last_active = db.latest_xp_event_time(pet_id).await?;
    db.upsert_pet_state(pet_id, total_xp, current_level, last_active)
        .await?;
    Ok((inserted, skipped))
}

fn default_pet_name(species: &crate::template::types::TemplateSpecies) -> String {
    species
        .default_pet_name
        .clone()
        .unwrap_or_else(|| species.name.clone())
}

const MAX_NAME_CHARS: usize = 20;

pub(crate) fn validate_name(input: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = input else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let len = trimmed.chars().count();
    if len > MAX_NAME_CHARS {
        anyhow::bail!(
            "name too long: max {} characters, got {}",
            MAX_NAME_CHARS,
            len
        );
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xp::env_test_lock;

    #[test]
    fn validate_name_none() {
        assert_eq!(validate_name(None).unwrap(), None);
    }

    #[test]
    fn validate_name_empty_treated_as_none() {
        assert_eq!(validate_name(Some("")).unwrap(), None);
        assert_eq!(validate_name(Some("   ")).unwrap(), None);
    }

    #[test]
    fn validate_name_trims_whitespace() {
        assert_eq!(
            validate_name(Some("  Burny  ")).unwrap(),
            Some("Burny".to_string())
        );
    }

    #[test]
    fn validate_name_accepts_emoji() {
        let pet = "🐱小蛋蛋";
        assert_eq!(validate_name(Some(pet)).unwrap(), Some(pet.to_string()));
    }

    #[test]
    fn validate_name_rejects_overlong() {
        let twenty_one = "x".repeat(21);
        let err = validate_name(Some(&twenty_one)).unwrap_err().to_string();
        assert!(err.contains("max 20"), "got: {}", err);
    }

    #[tokio::test]
    async fn pick_template_creates_pet_with_snapshot() {
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");

        let pet = engine
            .pick_template("sun", Some("TestyMcTest".into()))
            .await
            .expect("pick sun");

        assert_eq!(pet.name, "TestyMcTest");
        assert_eq!(pet.template_id, "sun");
        assert!(!pet.snapshot_path.is_empty());
        // pet.json must exist on disk
        let pet_json = PathBuf::from(&pet.snapshot_path).join("pet.json");
        assert!(pet_json.exists(), "pet.json should exist at {:?}", pet_json);
        // Regression guard: a name supplied in the egg picker is the
        // pet's INITIAL display name only — it must NOT auto-finalize
        // naming, otherwise the hatch-ceremony popup never fires.
        // Naming is locked exclusively through `finalize_naming`
        // (called by the confirm path of `naming_dismiss`).
        assert!(
            pet.name_finalized_at.is_none(),
            "pick_template must not auto-finalize naming when picker supplies a name; \
             got name_finalized_at = {:?}",
            pet.name_finalized_at,
        );
    }

    #[tokio::test]
    async fn pick_template_blank_name_uses_default_and_stays_unfinalized() {
        // Companion to the above — a blank picker name uses the
        // species default and also stays unfinalized.
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");

        let pet = engine
            .pick_template("sun", None)
            .await
            .expect("pick sun");

        assert!(!pet.name.is_empty(), "default name should be applied");
        assert!(
            pet.name_finalized_at.is_none(),
            "blank-picker pets must also stay unfinalized",
        );
    }

    #[tokio::test]
    async fn ingest_usage_skips_events_before_pet_was_born() {
        // Defensive: a freshly-picked pet must not accumulate XP from
        // events whose `timestamp` predates `born_at`. The live
        // JSONL watcher snaps to EOF on first-seen so this is usually
        // a non-issue, but a backlog flush (lag, clock skew, watcher
        // restart) would otherwise dump retroactive XP onto a newborn.
        use crate::event::{EventKind, ProviderId, SourceRef, TokenDelta, UsageEvent};
        use chrono::Duration;
        use uuid::Uuid;

        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");

        let pet = engine
            .pick_template("sun", Some("Birthday".into()))
            .await
            .expect("pick sun");

        // Event timestamp one hour BEFORE the pet was born.
        let prehistoric = pet.born_at - Duration::hours(1);
        let event = UsageEvent {
            id: Uuid::new_v4(),
            provider: ProviderId::ClaudeCode,
            client: None,
            session_id: "s".into(),
            project_path: None,
            git_branch: None,
            model: "claude-opus-4-7".into(),
            timestamp: prehistoric,
            tokens: TokenDelta {
                input: 100_000,
                output: 50_000,
                cache_read: 0,
                cache_creation: 0,
                reasoning: 0,
            },
            kind: EventKind::Turn { stop_reason: None },
            source: SourceRef {
                file: "test".into(),
                byte_offset: 0,
                line: 1,
            },
        };

        let result = engine.ingest_usage(&event).await.expect("ingest");
        assert!(
            result.is_none(),
            "pre-birth event must not produce a state update; \
             got {:?}",
            result.as_ref().map(|u| u.total_xp),
        );

        // Total XP stays 0 — no event was credited.
        let state = db
            .get_pet_state(&pet.id)
            .await
            .expect("state")
            .expect("row");
        assert_eq!(state.total_xp, 0);
    }

    #[tokio::test]
    async fn pick_template_unicorn_loads_and_creates_pet() {
        // Regression guard: no prior test exercised the unicorn
        // template, so a broken levels.json (e.g. non-monotonic
        // xp_required) would silently fail at load time — the picker
        // would just show one fewer template. Sun was the only canary
        // and didn't catch unicorn-specific issues. This test pins
        // that the rebalance keeps unicorn loadable end-to-end.
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");

        let pet = engine
            .pick_template("unicorn", Some("Sparkle".into()))
            .await
            .expect("unicorn should load and snapshot");

        assert_eq!(pet.template_id, "unicorn");
        assert_eq!(pet.name, "Sparkle");
    }

    #[tokio::test]
    async fn pick_template_kingkong_loads_and_creates_pet() {
        // KingKong is the 3rd builtin (hard mode, ~2.5× Sun). Same
        // load-canary as the unicorn test: silently broken levels /
        // stages / rules in this template would drop it from the
        // picker without anything red-flagging.
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");

        let pet = engine
            .pick_template("kingkong", Some("Konga".into()))
            .await
            .expect("kingkong should load and snapshot");

        assert_eq!(pet.template_id, "kingkong");
        assert_eq!(pet.name, "Konga");
    }

    #[tokio::test]
    async fn pick_template_unknown_errors() {
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");
        let err = engine.pick_template("nope", None).await.unwrap_err().to_string();
        assert!(err.contains("unknown template"), "got: {}", err);
    }

    #[tokio::test]
    async fn ingest_usage_emits_evolved_when_crossing_anchor() {
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");
        engine.pick_template("sun", None).await.expect("pick");

        // 200 XP = exactly L1 in sun's curve (same `medium` levels
        // preset as the retired ember used), which is also Stage 1's
        // trigger (`metric=level, value=1`). Anything larger lands in
        // a later stage and the assertion below would have to track
        // the template's curve — keeping this minimal makes the test
        // about the evolution mechanic itself, not curve balance.
        let update = engine
            .grant_manual(crate::xp::types::ManualGrant {
                xp_delta: 200,
                reason: "cross anchor".into(),
                ref_id: "x1".into(),
            })
            .await
            .expect("grant")
            .expect("update");
        assert!(update.evolved);
        assert_eq!(update.stage_name.as_deref(), Some("Stage 1"));
    }

    #[tokio::test]
    async fn finalize_naming_locks_and_updates_pet_json() {
        let _g = env_test_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("PETPET_HOME", dir.path());
        let db = crate::db::DbHandle::open(&dir.path().join("test.db"))
            .await
            .expect("open db");
        let engine = XPEngine::open(db.clone()).await.expect("open engine");
        let pet = engine
            .pick_template("sun", Some("OldName".into()))
            .await
            .expect("pick");
        let finalized = engine
            .finalize_naming(&pet.id, Some("NewName".into()))
            .await
            .expect("finalize");
        assert_eq!(finalized.name, "NewName");
        assert!(finalized.name_finalized_at.is_some());

        // Verify pet.json on disk reflects new name
        let doc = load_pet_doc(&PathBuf::from(&pet.snapshot_path)).expect("load");
        assert_eq!(doc.name, "NewName");
        assert!(doc.name_finalized_at.is_some());
    }
}
