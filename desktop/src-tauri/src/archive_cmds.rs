//! Tauri commands for the `.petpet` archive format.
//!
//! Four operations the frontend invokes:
//!
//! - `template_export(template_id, out_path)` — package an installed
//!   template's folder into a `.petpet` zip
//! - `template_import(zip_path)` — install a template archive into
//!   `~/.petpet/templates/<id>/`
//! - `pet_export(pet_id, out_path)` — bundle template + pet snapshot
//!   + xp_events JSONL into a `.petpet`
//! - `pet_import(zip_path)` — install template (if needed) + create
//!   new local pet + replay xp_events to rebuild state
//!
//! Self-healing import: a malformed `xp_events.jsonl` line gets
//! skipped with a warning rather than aborting the whole import, so
//! a user's long-raised companion never disappears just because one
//! row decoded oddly.

use std::path::{Path, PathBuf};

use petpet::db::DbHandle;
use petpet::paths;
use petpet::template::{
    pack_directory, unpack_archive, ArchiveKind, LevelEntry, LevelsPreset, PetSummary,
    PresetRegistry, StageStub, StagesPreset, TemplateRegistry, UnpackError,
};
use petpet::template::snapshot::load_pet_doc;
use petpet::xp::writer::XpEventInsert;
use petpet::xp::{replay_events_and_recompute, Pet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tauri::{Emitter, State};
use uuid::Uuid;

use crate::AppState;

#[derive(Serialize)]
pub struct ExportReport {
    pub path: String,
    pub bytes: u64,
}

#[derive(Serialize, Default)]
pub struct ImportReport {
    pub kind: String,
    /// Outcome of the import. Frontend branches on this. Terminal
    /// success: `Installed`, `AlreadyPresent`, `Merged`. Interactive
    /// gates that halt before touching disk: `NeedsVersionConfirm`,
    /// `DowngradeBlocked`, `PetIdExists`. Re-invoke with the
    /// appropriate `force` / `pet_action` flag to commit.
    pub status: ImportStatus,
    pub template_id: Option<String>,
    pub template_name: Option<String>,
    /// Version currently on disk (before this import). Populated even
    /// when there's nothing to do — useful for diff display.
    pub installed_version: Option<String>,
    /// Version inside the archive being imported.
    pub incoming_version: Option<String>,
    pub pet_id: Option<String>,
    pub pet_name: Option<String>,
    /// Set when `status == PetIdExists`. Carries the existing local
    /// pet's display details so the frontend can render a
    /// "merge events / create new copy / cancel" prompt with context.
    pub existing_pet: Option<ExistingPetInfo>,
    pub xp_events_imported: usize,
    pub xp_events_skipped: usize,
    pub warnings: Vec<String>,
    /// Legacy field — kept for any older frontend code; equivalent to
    /// `status == AlreadyPresent`.
    pub already_present: bool,
}

#[derive(Serialize)]
pub struct ExistingPetInfo {
    pub id: String,
    pub name: String,
    pub current_level: u32,
    pub total_xp: i64,
    pub event_count: u64,
}

#[derive(Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    /// Template (and pet, if applicable) installed as a brand-new
    /// local entity. Fresh pet id, fresh event ids.
    Installed,
    /// Same template id + version already installed — no changes
    /// made. Matches `npm install` on a satisfied dep.
    AlreadyPresent,
    /// Events were merged into an existing local pet with the same
    /// id (same logical pet, moved between machines). Composite
    /// (pet_id, source_type, source_ref) dedup means re-importing
    /// the same archive twice doesn't double-count.
    Merged,
    /// A different version of this template is installed. Frontend
    /// should prompt the user with a before/after version diff and,
    /// if confirmed, re-invoke with `force: true`.
    NeedsVersionConfirm,
    /// Incoming version is OLDER than what's installed. Default is
    /// refuse to protect the user's current state; `force: true`
    /// overrides.
    DowngradeBlocked,
    /// The archive's pet_id matches a pet already in the local DB —
    /// "this is the same companion you have on another machine".
    /// Frontend prompts: merge events into existing, or create a
    /// fresh-id copy. Re-invoke with `pet_action: "merge"` or
    /// `pet_action: "copy"`.
    PetIdExists,
    #[default]
    Unknown,
}

// ─── Template export ───────────────────────────────────────────────

#[tauri::command]
pub async fn template_export(template_id: String, out_path: String) -> Result<ExportReport, String> {
    let loaded = tokio::task::spawn_blocking(TemplateRegistry::discover)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let tpl = loaded
        .into_iter()
        .find(|t| t.template.meta.id == template_id)
        .ok_or_else(|| format!("template not found: {template_id}"))?;

    let out = PathBuf::from(&out_path);
    let src_dir = tpl.dir.clone();
    let out_clone = out.clone();
    tokio::task::spawn_blocking(move || pack_directory(&src_dir, &out_clone, None))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(ExportReport {
        path: out.to_string_lossy().to_string(),
        bytes,
    })
}

// ─── Pet export ────────────────────────────────────────────────────

#[tauri::command]
pub async fn pet_export(
    state: State<'_, AppState>,
    pet_id: String,
    out_path: String,
) -> Result<ExportReport, String> {
    let pets = state
        .xp
        .list_pets()
        .await
        .map_err(|e| e.to_string())?;
    let pet = pets
        .into_iter()
        .find(|p| p.id == pet_id)
        .ok_or_else(|| format!("pet not found: {pet_id}"))?;

    // Find the template dir this pet was hatched from.
    let templates = tokio::task::spawn_blocking(TemplateRegistry::discover)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let tpl = templates
        .into_iter()
        .find(|t| t.template.meta.id == pet.template_id)
        .ok_or_else(|| format!("template {} for pet not found locally", pet.template_id))?;

    // Stage the archive contents in a temp dir: template files at
    // root + a `pet/` folder with pet.json + xp_events.jsonl.
    let staging = tempfile::tempdir().map_err(|e| e.to_string())?;
    copy_dir_recursive(&tpl.dir, staging.path())
        .map_err(|e| format!("stage template: {e}"))?;
    let pet_dir = staging.path().join("pet");
    std::fs::create_dir_all(&pet_dir).map_err(|e| e.to_string())?;

    let pet_json = std::fs::read(PathBuf::from(&pet.snapshot_path).join("pet.json"))
        .map_err(|e| format!("read pet.json: {e}"))?;
    std::fs::write(pet_dir.join("pet.json"), &pet_json).map_err(|e| e.to_string())?;

    // Dump every xp_event as a JSONL line. ASC order so the replay
    // happens in the same temporal sequence as the original
    // ingestion — this matters for any future code that reads the
    // log temporally even though `pet_state` is order-invariant.
    let events = state
        .db
        .list_xp_events_for_pet(&pet.id)
        .await
        .map_err(|e| e.to_string())?;
    let mut jsonl = String::new();
    for ev in &events {
        jsonl.push_str(&serde_json::to_string(ev).map_err(|e| e.to_string())?);
        jsonl.push('\n');
    }
    std::fs::write(pet_dir.join("xp_events.jsonl"), jsonl).map_err(|e| e.to_string())?;

    // Compute summary for the manifest's pet_summary block.
    let snap = state.xp.snapshot().await.map_err(|e| e.to_string())?;
    let summary = if snap.pet.as_ref().map(|p| p.id.clone()) == Some(pet.id.clone()) {
        let state_view = snap.state.unwrap_or(petpet::xp::engine::XPStateView {
            total_xp: 0,
            current_level: 0,
            xp_in_level: 0,
            xp_for_next_level: None,
            stage_level: 0,
        });
        let days = chrono::Utc::now()
            .signed_duration_since(pet.born_at)
            .num_days()
            .max(0);
        PetSummary {
            level: state_view.current_level,
            total_xp: state_view.total_xp,
            days_raised: days,
        }
    } else {
        // Pet isn't currently active — derive from events only.
        let total_xp: i64 = events.iter().map(|e| e.xp_delta).sum();
        let days = chrono::Utc::now()
            .signed_duration_since(pet.born_at)
            .num_days()
            .max(0);
        PetSummary {
            level: 0,
            total_xp,
            days_raised: days,
        }
    };

    let out = PathBuf::from(&out_path);
    let staging_path = staging.path().to_path_buf();
    let out_clone = out.clone();
    tokio::task::spawn_blocking(move || pack_directory(&staging_path, &out_clone, Some(summary)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(ExportReport {
        path: out.to_string_lossy().to_string(),
        bytes,
    })
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if s.is_dir() {
            copy_dir_recursive(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

// ─── Template scaffolder ───────────────────────────────────────────
//
// One-shot "Create new template" flow: clones the built-in `mist`
// template as a structural starting point (stages folder layout +
// levels.json + rules.json), then patches `template.json.meta` with
// the user's id / name / author / description. Idiot-proof — only
// requires name + author from the user; everything else is sensible
// defaults the author can refine by editing files on disk.

#[derive(Serialize)]
pub struct TemplateCreateResult {
    pub template_id: String,
    pub template_dir: String,
}

/// The set of built-in templates that can serve as a scaffold preset.
/// Keeping this allowlist explicit (rather than "any installed
/// template") means a user with a broken third-party template can't
/// accidentally fork it as a starting point. Updated when the
/// original mist/ember/onyx were retired in favour of sun + unicorn.
const PRESET_IDS: &[&str] = &["sun", "unicorn"];

/// Wire shape for an inline-supplied level curve. Same shape the
/// existing template loader expects on disk; we just deserialize from
/// the frontend's JSON straight into this.
#[derive(Debug, Deserialize)]
pub struct LevelCurveInput {
    pub max_level: u32,
    pub entries: Vec<LevelEntry>,
}

/// Wire shape for one inline-supplied stage. Frontend's editor sends
/// an array of these; we materialize them to `Stage` via `to_stage()`
/// before serializing to disk.
#[derive(Debug, Deserialize)]
pub struct StageStubInput {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub flavor: String,
    /// Trigger as raw JSON — accepts either a leaf `{metric, value, op?}`
    /// or composite `{all_of|any_of: [...]}`. We round-trip through the
    /// runtime `Trigger` enum during validation to catch malformed
    /// shapes BEFORE writing anything to disk.
    pub trigger: Value,
    /// Optional absolute path to a sprite file the user picked in the
    /// StagesEditor. When set, the file is copied to
    /// `<template>/stages/stage_N/sprite.png` and takes precedence
    /// over the linear-remap fallback (which picks a representative
    /// sprite from the cloned base preset). When `None`, the remap
    /// runs unchanged so unconfigured stages still get *some* art.
    #[serde(default)]
    pub sprite_path: Option<String>,
}

/// Scaffold a new template at `~/.petpet/templates/<id>/`.
///
/// Parameters:
/// - `name`, `author`: identity. Slugged into `<author>.<name>`.
/// - `description`: optional one-line blurb stored in `meta.description`.
/// - `preset`: base clone source — one of `mist | ember | onyx`. The
///   clone provides the sprite sheet, rules.json, theme colours, and
///   ceremony scripts. Defaults to `mist`.
///
/// Two ways to override the cloned base's levels / stages — pick at
/// most one mechanism per axis. Literal data wins when both are set:
/// - `levels`: literal `LevelCurveInput` (full custom curve). When set,
///   replaces the cloned `levels.json`. Takes precedence over `levels_preset`.
/// - `levels_preset`: id from `preset_list_levels` (e.g. `short | medium | long`).
///   When set, replaces the cloned `levels.json` with the chosen
///   preset's entries.
/// - `stages`: literal `[StageStubInput]` array (full custom arc). When
///   set, replaces the cloned `stages/` folder. Sprite art remaps
///   linearly to the new stage count. Takes precedence over `stages_preset`.
/// - `stages_preset`: id from `preset_list_stages` (e.g. `simple | balanced | extended`).
///   When set, replaces the cloned `stages/` folder with the preset arc.
///
/// When neither levels nor levels_preset is set, the base preset's
/// levels.json is inherited unchanged. Same for stages.
#[tauri::command]
pub async fn template_create(
    name: String,
    author: String,
    description: Option<String>,
    preset: Option<String>,
    levels: Option<LevelCurveInput>,
    stages: Option<Vec<StageStubInput>>,
    levels_preset: Option<String>,
    stages_preset: Option<String>,
) -> Result<TemplateCreateResult, String> {
    let name = name.trim().to_string();
    let author = author.trim().to_string();
    if name.is_empty() {
        return Err("Template name is required.".into());
    }
    if author.is_empty() {
        return Err("Author name is required (used to namespace the id).".into());
    }
    // Default scaffold preset — mist is gone, so fall back to sun
    // (the medium-difficulty builtin). Any caller that omits the
    // preset gets the same shape they used to get with mist before
    // the retirement, semantically the closest standalone preset.
    let preset = preset.unwrap_or_else(|| "sun".into());
    if !PRESET_IDS.contains(&preset.as_str()) {
        return Err(format!(
            "Unknown preset '{preset}'. Expected one of: {}",
            PRESET_IDS.join(", ")
        ));
    }

    // Validate inline overrides BEFORE touching disk so a malformed
    // payload surfaces as a clean error rather than a half-cloned
    // template the user has to manually clean up. Literal data wins
    // over preset ids — we ignore the preset id when literal is set
    // rather than treat the combination as an error (frontend may
    // tag both for telemetry / summary display).
    let levels_override = match levels {
        Some(l) => Some(validate_levels_input(l)?),
        None => resolve_levels_preset(levels_preset.as_deref())?
            .map(|p| LevelCurveInput { max_level: p.max_level, entries: p.entries }),
    };
    let stages_override = match stages {
        Some(s) => Some(validate_stages_input(s)?),
        None => resolve_stages_preset(stages_preset.as_deref())?
            .map(|p| p.stages.into_iter().map(|s| StageStubInput {
                id: s.id,
                name: s.name,
                flavor: s.flavor,
                trigger: serde_json::to_value(&s.trigger).unwrap_or(Value::Null),
                // Preset-supplied stages never carry per-stage sprite
                // paths — sprite art comes from the cloned base preset
                // via the remap fallback in apply_stages_to_template.
                sprite_path: None,
            }).collect()),
    };

    let id = derive_template_id(&author, &name);
    if id.is_empty() {
        return Err(
            "Couldn't derive a valid id — try alphanumeric author / name.".into(),
        );
    }

    let dest = paths::user_templates_dir().join(&id);
    if dest.exists() {
        return Err(format!(
            "Template id '{id}' already exists. Pick a different name or author."
        ));
    }

    // Resolve the chosen base preset template — one of the bundled
    // mist / ember / onyx. The clone gives us the sprite sheet, rules,
    // theme, and ceremony scripts; the optional overrides below
    // then swap out the levels curve and/or stage arc in place.
    let src = paths::builtin_templates_dir().join(&preset);
    if !src.exists() {
        return Err(format!(
            "Built-in template '{preset}' missing from disk. Restart petpet to extract it.",
        ));
    }

    let src_clone = src.clone();
    let dest_clone = dest.clone();
    tokio::task::spawn_blocking(move || copy_dir_recursive_inner(&src_clone, &dest_clone))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("clone {preset}: {e}"))?;

    if let Some(lp) = levels_override {
        write_levels_to_template(&dest, &lp)
            .map_err(|e| format!("apply levels: {e}"))?;
    }
    if let Some(sp) = stages_override {
        apply_stages_to_template(&dest, &sp)
            .map_err(|e| format!("apply stages: {e}"))?;
    }

    // Patch the cloned template.json with the new identity. We keep
    // everything else (species, levels, stages, rules, theme) as the
    // base preset's — the author can edit those at their leisure.
    let tpl_path = dest.join("template.json");
    let body = std::fs::read_to_string(&tpl_path).map_err(|e| e.to_string())?;
    let mut v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("parse cloned template.json: {e}"))?;
    if let Some(meta) = v
        .get_mut("meta")
        .and_then(Value::as_object_mut)
    {
        meta.insert("id".to_string(), Value::String(id.clone()));
        meta.insert("name".to_string(), Value::String(name.clone()));
        meta.insert("version".to_string(), Value::String("1.0.0".to_string()));
        if let Some(d) = description {
            let d = d.trim();
            if !d.is_empty() {
                meta.insert("description".to_string(), Value::String(d.to_string()));
            }
        }
        let mut author_obj = serde_json::Map::new();
        author_obj.insert("name".to_string(), Value::String(author));
        meta.insert("author".to_string(), Value::Object(author_obj));
    }
    let pretty = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(&tpl_path, pretty).map_err(|e| e.to_string())?;

    Ok(TemplateCreateResult {
        template_id: id,
        template_dir: dest.to_string_lossy().to_string(),
    })
}

fn resolve_levels_preset(id: Option<&str>) -> Result<Option<LevelsPreset>, String> {
    let Some(id) = id else { return Ok(None) };
    match PresetRegistry::find_levels(id).map_err(|e| e.to_string())? {
        Some(p) => Ok(Some(p)),
        None => Err(format!("Unknown levels preset '{id}'. Available ids come from `preset_list_levels`.")),
    }
}

fn resolve_stages_preset(id: Option<&str>) -> Result<Option<StagesPreset>, String> {
    let Some(id) = id else { return Ok(None) };
    match PresetRegistry::find_stages(id).map_err(|e| e.to_string())? {
        Some(p) => Ok(Some(p)),
        None => Err(format!("Unknown stages preset '{id}'. Available ids come from `preset_list_stages`.")),
    }
}

/// Sanity-check a user-supplied level curve before we trust it on
/// disk. Enforces the same shape the template loader will demand on
/// next startup, so authoring errors surface in the creator dialog
/// instead of as a load-time "template won't appear" mystery.
fn validate_levels_input(input: LevelCurveInput) -> Result<LevelCurveInput, String> {
    if input.entries.is_empty() {
        return Err("Levels curve is empty — add at least one level.".into());
    }
    // First entry MUST be level 0 with 0 XP (the hatch state).
    if input.entries[0].level != 0 {
        return Err(format!(
            "Level curve must start at level 0; got {}.",
            input.entries[0].level
        ));
    }
    if input.entries[0].xp_required != 0 {
        return Err(format!(
            "Level 0 must require 0 XP; got {}.",
            input.entries[0].xp_required
        ));
    }
    // Entries must be densely 0..N contiguous (loader will reject
    // gaps). We auto-trust the frontend's ordering since it always
    // sorts by row index, but verify so a manual API call can't
    // sneak a corrupt payload past us.
    for (i, e) in input.entries.iter().enumerate() {
        if e.level != i as u32 {
            return Err(format!(
                "Levels must be 0-indexed contiguous; entry {i} has level={}.",
                e.level
            ));
        }
        if e.xp_required < 0 {
            return Err(format!(
                "Level {}: xp_required can't be negative ({}).",
                e.level, e.xp_required
            ));
        }
    }
    // max_level must equal the last entry's level. We don't surface a
    // user-visible knob for this in the creator UI — the frontend
    // derives it — but a stale value sneaking through here would
    // crash the template loader, so guard.
    let last = input.entries.last().unwrap().level;
    if input.max_level != last {
        return Err(format!(
            "max_level ({}) must equal the last entry's level ({last}).",
            input.max_level
        ));
    }
    Ok(input)
}

/// Sanity-check a user-supplied stages arc before we trust it on disk.
/// Enforces stage_id ↔ folder-name parity and trigger parseability,
/// plus the "first stage triggers at level 0" rule the UI assumes.
/// Also validates per-stage sprite paths: file exists, is readable,
/// and within size budget. Errors here surface in the editor's
/// inline error banner so the user fixes them BEFORE we touch disk.
const MAX_SPRITE_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB — generous for a PNG sprite
fn validate_stages_input(input: Vec<StageStubInput>) -> Result<Vec<StageStubInput>, String> {
    if input.is_empty() {
        return Err("Stages arc is empty — add at least one stage.".into());
    }
    for (i, s) in input.iter().enumerate() {
        let expected_id = format!("stage_{i}");
        if s.id != expected_id {
            return Err(format!(
                "Stage at row {i} has id='{}'; expected '{expected_id}'. \
                 Stage ids must be stage_0, stage_1, … in order.",
                s.id
            ));
        }
        if s.name.trim().is_empty() {
            return Err(format!("Stage {i} ('{}') needs a name.", s.id));
        }
        // Parse the trigger to catch malformed JSON / unknown metric
        // before we serialize it back to disk. The runtime `Trigger`
        // enum is our source of truth — if it can't parse, the
        // template loader can't either.
        let _: petpet::template::Trigger = serde_json::from_value(s.trigger.clone())
            .map_err(|e| format!("Stage {i} ('{}') has malformed trigger: {e}", s.id))?;

        // Validate per-stage sprite path if the user picked one. We
        // can't open it for read-validity here without risking races
        // (file deleted between validate and apply), so we just
        // check stat() — the apply step handles the read error
        // cleanly with a different, location-specific message.
        if let Some(path) = &s.sprite_path {
            let p = std::path::Path::new(path);
            if !p.is_absolute() {
                return Err(format!(
                    "Stage {i} ('{}') sprite path '{path}' must be absolute.",
                    s.id
                ));
            }
            let meta = std::fs::metadata(p).map_err(|e| format!(
                "Stage {i} ('{}') sprite file '{path}' can't be read: {e}",
                s.id
            ))?;
            if !meta.is_file() {
                return Err(format!(
                    "Stage {i} ('{}') sprite path '{path}' isn't a file.",
                    s.id
                ));
            }
            if meta.len() > MAX_SPRITE_BYTES {
                return Err(format!(
                    "Stage {i} ('{}') sprite is {} bytes; max allowed is {} ({} MiB). \
                     Resize / re-export the image and try again.",
                    s.id,
                    meta.len(),
                    MAX_SPRITE_BYTES,
                    MAX_SPRITE_BYTES / 1024 / 1024,
                ));
            }
        }
    }
    Ok(input)
}

/// Overwrite `<template>/levels.json` with the supplied curve. The
/// resulting file conforms to the template schema (just `max_level`
/// + `entries`), with no link back to whatever preset or editor
/// session produced it.
fn write_levels_to_template(
    dest: &std::path::Path,
    curve: &LevelCurveInput,
) -> Result<(), std::io::Error> {
    let body = serde_json::json!({
        "max_level": curve.max_level,
        "entries": curve.entries,
    });
    let pretty = serde_json::to_string_pretty(&body)?;
    std::fs::write(dest.join("levels.json"), pretty)
}

/// Replace `<template>/stages/` with the supplied stages arc.
///
/// We preserve the cloned template's per-stage sprite art across the
/// swap via linear-interpolated remapping: new stage `N` of `K` total
/// inherits sprite `round(N * (old_count - 1) / (K - 1))`. This means
/// a user customising a Mist clone down to 3 stages gets sprites
/// 0, 5, 9 — egg, mid-form, final — instead of 0, 1, 2 (egg,
/// just-hatched, growing-fast), which would feel like no evolution
/// happened. Sound trade-off for an idiot-proof starter.
fn apply_stages_to_template(
    dest: &std::path::Path,
    stages: &[StageStubInput],
) -> Result<(), std::io::Error> {
    let stages_dir = dest.join("stages");
    if !stages_dir.exists() {
        std::fs::create_dir_all(&stages_dir)?;
    }

    // Snapshot sprite bytes from the cloned base preset, indexed by
    // stage index. Done BEFORE we delete anything so we can remap.
    let existing_sprites = collect_existing_stage_sprites(&stages_dir)?;
    let old_count = existing_sprites.len();

    // Wipe `stage_*` subfolders. We don't recursively wipe the parent
    // because future template formats may grow sibling files we don't
    // want to accidentally erase.
    for ent in std::fs::read_dir(&stages_dir)? {
        let ent = ent?;
        let path = ent.path();
        let is_stage_folder = path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("stage_"))
                .unwrap_or(false);
        if is_stage_folder {
            std::fs::remove_dir_all(&path)?;
        }
    }

    let new_count = stages.len();
    for (i, stub) in stages.iter().enumerate() {
        let folder = stages_dir.join(&stub.id);
        std::fs::create_dir_all(&folder)?;

        // Materialize a full `Stage` from the inline shape, then
        // serialize to disk. Round-trips the trigger through the
        // runtime `Trigger` enum so malformed shapes have already
        // been caught by `validate_stages_input`.
        let stage = StageStub {
            id: stub.id.clone(),
            name: stub.name.clone(),
            flavor: stub.flavor.clone(),
            trigger: serde_json::from_value(stub.trigger.clone())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        }
        .to_stage();
        let body = serde_json::to_string_pretty(&stage)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(folder.join("stage.json"), body)?;

        // Sprite resolution — user-supplied path wins over the remap
        // fallback. Both write to the same `sprite.png` so the
        // template loader doesn't need to know which path produced
        // the bytes.
        let dest_sprite = folder.join("sprite.png");
        if let Some(src_path) = &stub.sprite_path {
            // User explicitly picked a file in the editor → copy it
            // into the template folder as the new stage's sprite.
            // Validation already confirmed the file exists, is
            // readable, and is within the size budget.
            std::fs::copy(src_path, &dest_sprite).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "copying user sprite for {} from '{}' to '{}': {}",
                        stub.id,
                        src_path,
                        dest_sprite.display(),
                        e
                    ),
                )
            })?;
        } else if old_count > 0 {
            // No user pick → fall back to linear remap of the cloned
            // base preset's sprites so even unconfigured stages get
            // SOME art (the egg, the mid-form, the final form, etc.).
            let src_idx = if new_count <= 1 {
                0
            } else {
                let denom = (new_count - 1) as f64;
                let scaled = (i as f64) * ((old_count.saturating_sub(1)) as f64) / denom;
                scaled.round() as usize
            };
            if let Some(sprite_bytes) = existing_sprites.get(src_idx) {
                std::fs::write(&dest_sprite, sprite_bytes)?;
            }
        }
    }

    Ok(())
}

fn collect_existing_stage_sprites(
    stages_dir: &std::path::Path,
) -> Result<Vec<Vec<u8>>, std::io::Error> {
    let mut by_idx: Vec<(u32, Vec<u8>)> = Vec::new();
    for ent in std::fs::read_dir(stages_dir)? {
        let ent = ent?;
        let path = ent.path();
        if !path.is_dir() {
            continue;
        }
        let idx = match path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("stage_"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            Some(i) => i,
            None => continue,
        };
        let sprite = path.join("sprite.png");
        if sprite.exists() {
            by_idx.push((idx, std::fs::read(&sprite)?));
        }
    }
    by_idx.sort_by_key(|(i, _)| *i);
    Ok(by_idx.into_iter().map(|(_, b)| b).collect())
}

// ─── Sprite staging ─────────────────────────────────────────────────
//
// The StagesEditor lets the user pick a sprite from anywhere on disk
// (~/Pictures, ~/Desktop, an iCloud drive, …). Two problems with
// using that raw path directly:
//
//   1. Tauri's asset-protocol scope only allows requests for paths
//      under `$HOME/.petpet/**`, so the React thumbnail preview
//      (which uses `convertFileSrc`) can't load the user's pick
//      from outside that scope.
//
//   2. The user may move / delete / rename the source file between
//      picking it and clicking Create — by then the path is stale
//      and template_create fails with a confusing "file not found".
//
// Both go away if we copy the picked file into a known scope-safe
// staging dir at pick time. The frontend stores the staging path,
// previews via convertFileSrc (works — staging is in scope), and
// passes the staging path through to template_create at commit time.

#[derive(Serialize)]
pub struct StagedSprite {
    pub staged_path: String,
}

#[tauri::command]
pub async fn sprite_stage_for_picker(src_path: String) -> Result<StagedSprite, String> {
    tokio::task::spawn_blocking(move || stage_sprite_blocking(&src_path))
        .await
        .map_err(|e| e.to_string())?
}

fn stage_sprite_blocking(src_path: &str) -> Result<StagedSprite, String> {
    let src = std::path::Path::new(src_path);
    if !src.is_absolute() {
        return Err(format!("Sprite path '{src_path}' must be absolute."));
    }
    let meta = std::fs::metadata(src)
        .map_err(|e| format!("Can't read '{src_path}': {e}"))?;
    if !meta.is_file() {
        return Err(format!("'{src_path}' isn't a file."));
    }
    if meta.len() > MAX_SPRITE_BYTES {
        return Err(format!(
            "Sprite is {} bytes; max allowed is {} ({} MiB). Resize and try again.",
            meta.len(),
            MAX_SPRITE_BYTES,
            MAX_SPRITE_BYTES / 1024 / 1024,
        ));
    }

    let staging_dir = paths::template_staging_sprites_dir();
    std::fs::create_dir_all(&staging_dir).map_err(|e| {
        format!("creating staging dir {}: {e}", staging_dir.display())
    })?;

    // Preserve the source extension — the asset protocol uses MIME
    // sniffing that benefits from a known extension (png/jpg/gif/
    // webp) for the preview, and the rest of the pipeline doesn't
    // care about the on-disk filename.
    let ext = src
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let staged = staging_dir.join(format!("{}.{}", Uuid::new_v4(), ext));
    std::fs::copy(src, &staged)
        .map_err(|e| format!("copying '{src_path}' to staging: {e}"))?;

    Ok(StagedSprite {
        staged_path: staged.to_string_lossy().to_string(),
    })
}

// ─── Preset library exposure ───────────────────────────────────────
//
// Read-only listings of the embedded preset library. The TemplateCreator
// UI calls these on open to populate its "system recommended" dropdowns.
// Each list call re-extracts the embedded library to
// `~/.petpet/builtin_presets/` so a curious user can inspect the JSON
// (the canonical bytes always come from the binary — disk edits are
// overwritten on next list, by design).

#[tauri::command]
pub async fn preset_list_levels() -> Result<Vec<LevelsPreset>, String> {
    tokio::task::spawn_blocking(|| {
        // Best-effort disk extraction so the user can browse the files.
        // A failure here doesn't block the listing — the data the UI
        // needs lives in the embedded `Dir`, not on disk.
        if let Err(e) = PresetRegistry::ensure_on_disk() {
            tracing::warn!(error = %e, "couldn't extract presets to disk; continuing with embedded copy");
        }
        PresetRegistry::list_levels().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn preset_list_stages() -> Result<Vec<StagesPreset>, String> {
    tokio::task::spawn_blocking(|| {
        if let Err(e) = PresetRegistry::ensure_on_disk() {
            tracing::warn!(error = %e, "couldn't extract presets to disk; continuing with embedded copy");
        }
        PresetRegistry::list_stages().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Derive a namespaced template id from `<author>.<name>`. Lowercased,
/// alphanumerics + hyphens only. Returns empty string if either
/// component slugs to nothing (caller treats that as an error).
fn derive_template_id(author: &str, name: &str) -> String {
    fn slug(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_dash = false;
        for ch in s.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() {
                out.push(lower);
                prev_dash = false;
            } else if (lower == ' ' || lower == '_' || lower == '-') && !prev_dash && !out.is_empty() {
                out.push('-');
                prev_dash = true;
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        out
    }
    let a = slug(author);
    let n = slug(name);
    if a.is_empty() || n.is_empty() {
        return String::new();
    }
    format!("{a}.{n}")
}

// ─── Unified import ────────────────────────────────────────────────
//
// Both template and pet archives flow through this one command. The
// archive's manifest tells the importer which lane to take, so the
// frontend doesn't have to pre-flight which kind the file is — drop
// any .petpet on the window or pick any .petpet via the file dialog
// and this handles it. Always has AppState available, so a pet
// archive coming in via a "template-y" surface (e.g. egg-picker's
// Import button) still imports correctly.

/// `force=true` bypasses the version-conflict gates (DowngradeBlocked /
/// NeedsVersionConfirm). `pet_action` resolves the PetIdExists gate
/// when the local DB already has a pet with the same id ("this is the
/// same companion you have on another machine"):
///   - `"merge"` → keep the existing pet, replay new events on top.
///     The composite (pet_id, source_type, source_ref) dedup index
///     means re-importing the same archive twice is a safe no-op.
///   - `"copy"` → mint a fresh local pet id (the current behaviour
///     before this flag existed). Useful when the user wants two
///     separate copies of the same pet evolving independently.
#[tauri::command]
pub async fn archive_import(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    zip_path: String,
    force: Option<bool>,
    pet_action: Option<String>,
) -> Result<ImportReport, String> {
    do_import(
        PathBuf::from(zip_path),
        state,
        app,
        force.unwrap_or(false),
        pet_action,
    )
    .await
}

async fn do_import(
    zip_path: PathBuf,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    force: bool,
    pet_action: Option<String>,
) -> Result<ImportReport, String> {
    let staging = tempfile::tempdir().map_err(|e| e.to_string())?;
    let staging_path = staging.path().to_path_buf();
    let zip_clone = zip_path.clone();
    let unpacked =
        tokio::task::spawn_blocking(move || unpack_archive(&zip_clone, &staging_path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format_unpack_err(&e))?;

    let mut report = ImportReport::default();
    report.warnings = unpacked.warnings;
    report.kind = match unpacked.manifest.kind {
        ArchiveKind::Template => "template".into(),
        ArchiveKind::Pet => "pet".into(),
    };

    // Read template.json so we know the id / version to install at.
    let tpl_path = unpacked.root.join("template.json");
    if !tpl_path.exists() {
        return Err("archive is missing template.json".into());
    }
    let tpl_body = std::fs::read_to_string(&tpl_path).map_err(|e| e.to_string())?;
    let tpl_meta = parse_template_meta(&tpl_body)?;
    report.template_id = Some(tpl_meta.id.clone());
    report.template_name = Some(tpl_meta.name.clone());
    report.incoming_version = Some(tpl_meta.version.clone());

    // Install template into ~/.petpet/templates/<id>/. Resolve
    // version conflicts before touching anything on disk:
    //   - same version           → no-op (npm-style)
    //   - newer incoming version → halt + ask user (unless force)
    //   - older incoming version → halt + warn (unless force)
    //   - no existing version    → fresh install
    let dest = paths::user_templates_dir().join(&tpl_meta.id);
    let existing_version = read_existing_template_version(&dest);
    report.installed_version = existing_version.clone();

    let needs_install = match (&existing_version, force) {
        (Some(v), _) if v == &tpl_meta.version => {
            report.status = ImportStatus::AlreadyPresent;
            report.already_present = true;
            // Still proceed for pet-kind archives — the template is
            // already in place but we still need to install the pet.
            false
        }
        (Some(v), false) => {
            match version_compare(&tpl_meta.version, v) {
                std::cmp::Ordering::Less => {
                    // Incoming is older. Refuse without force.
                    report.status = ImportStatus::DowngradeBlocked;
                    return Ok(report);
                }
                _ => {
                    // Incoming is newer. Halt and ask the user.
                    report.status = ImportStatus::NeedsVersionConfirm;
                    return Ok(report);
                }
            }
        }
        _ => true,
    };

    if needs_install {
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
        }
        copy_template_payload(&unpacked.root, &dest).map_err(|e| e.to_string())?;
        report.status = ImportStatus::Installed;
    }

    // For pet kind, also create the local pet + replay events.
    if unpacked.manifest.kind == ArchiveKind::Pet {
        let outcome = install_pet(
            &unpacked.root,
            &state.db,
            &tpl_meta.id,
            pet_action.as_deref(),
            &mut report,
        )
        .await?;
        let InstallPetOutcome { pet_id, pet_name, merged } = outcome;
        if report.status == ImportStatus::Unknown {
            // install_pet didn't override status — set it based on
            // whether we merged or freshly installed.
            report.status = if merged {
                ImportStatus::Merged
            } else {
                ImportStatus::Installed
            };
        }
        report.pet_id = Some(pet_id.clone());
        report.pet_name = Some(pet_name);
        // Note: we deliberately DO NOT call `set_active_pet` here.
        // Importing a pet adds it to the user's box; switching to it
        // is a separate, explicit action via the Pets dialog. This
        // matches Pokémon's "you caught a new mon, it goes into the
        // PC, your active team only changes if you swap." Prevents
        // the surprising "I imported a backup and my active pet
        // suddenly changed mid-conversation" effect.
        let _ = app.emit("pet://library_changed", &pet_id);
    }

    Ok(report)
}

fn format_unpack_err(e: &UnpackError) -> String {
    match e {
        UnpackError::SchemaTooNew { major } => format!(
            "This .petpet was made by a newer version of petpet (format v{major}). \
             Please update petpet to import it."
        ),
        UnpackError::NotPetpet => {
            "This file isn't a petpet archive (no manifest.json found).".into()
        }
        UnpackError::SchemaMalformed(s) => format!("Archive schema malformed: {s:?}."),
        UnpackError::MissingRequired(f) => format!("Archive is missing required field: {f}"),
        UnpackError::TooLarge(n) => format!("Archive is too large ({n} bytes > 50 MB)."),
        UnpackError::UnsafePath(p) => format!("Archive contains unsafe path: {p:?}"),
        other => other.to_string(),
    }
}

struct TplMeta {
    id: String,
    name: String,
    version: String,
}

fn parse_template_meta(body: &str) -> Result<TplMeta, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("template.json: {e}"))?;
    let id = v
        .get("meta")
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "template.json missing meta.id".to_string())?
        .to_string();
    let name = v
        .get("meta")
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let version = v
        .get("meta")
        .and_then(|m| m.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("0.0.0")
        .to_string();
    Ok(TplMeta { id, name, version })
}

/// Component-wise numeric comparison for version strings. Handles
/// semver-shape strings (`"1.0.0"`, `"2.3"`, `"1.10.0"`) plus the
/// case where the field is missing or `"0.0.0"`. Used for template
/// upgrade / downgrade detection on import.
fn version_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let parts_a: Vec<u32> = a.split('.').filter_map(|p| p.parse().ok()).collect();
    let parts_b: Vec<u32> = b.split('.').filter_map(|p| p.parse().ok()).collect();
    let n = parts_a.len().max(parts_b.len());
    for i in 0..n {
        let pa = parts_a.get(i).copied().unwrap_or(0);
        let pb = parts_b.get(i).copied().unwrap_or(0);
        match pa.cmp(&pb) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

fn read_existing_template_version(dir: &Path) -> Option<String> {
    let body = std::fs::read_to_string(dir.join("template.json")).ok()?;
    parse_template_meta(&body).ok().map(|m| m.version)
}

fn copy_template_payload(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip archive-level artefacts. `manifest.json` belongs to
        // the archive shell, not the template definition; `pet/` is
        // the pet-archive payload handled separately.
        if name_str == "manifest.json" || name_str == "pet" {
            continue;
        }
        let s = entry.path();
        let d = dst.join(&name);
        if s.is_dir() {
            copy_dir_recursive_inner(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

fn copy_dir_recursive_inner(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if s.is_dir() {
            copy_dir_recursive_inner(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

struct InstallPetOutcome {
    pet_id: String,
    pet_name: String,
    /// True when the import merged into an existing local pet (same
    /// id, `pet_action == Some("merge")`). False when a fresh local
    /// pet row was created.
    merged: bool,
}

async fn install_pet(
    archive_root: &Path,
    db: &Arc<DbHandle>,
    template_id: &str,
    pet_action: Option<&str>,
    report: &mut ImportReport,
) -> Result<InstallPetOutcome, String> {
    let pet_json_path = archive_root.join("pet/pet.json");
    if !pet_json_path.exists() {
        return Err("pet archive missing pet/pet.json".into());
    }
    let pet_body = std::fs::read_to_string(&pet_json_path).map_err(|e| e.to_string())?;
    let pet_v: Value = serde_json::from_str(&pet_body)
        .map_err(|e| format!("pet.json: {e}"))?;
    let pet_name = pet_v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Unnamed")
        .to_string();
    let archive_pet_id = pet_v
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let name_finalized_at = pet_v
        .get("name_finalized_at")
        .and_then(Value::as_str)
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .flatten()
        .map(|d| d.with_timezone(&chrono::Utc));
    let born_at = pet_v
        .get("born_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);

    // Check for an existing local pet with the same id — "this is
    // the same companion you already have on another machine".
    // Resolution depends on the caller-supplied pet_action:
    //   - None     → halt + return PetIdExists for frontend prompt
    //   - "merge"  → keep existing pet, replay events on top
    //   - "copy"   → mint fresh local id (cloned independent pet)
    let existing_pet_with_same_id: Option<Pet> = if let Some(aid) = &archive_pet_id {
        db.list_pets()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|p| &p.id == aid)
    } else {
        None
    };

    let mut merged = false;
    let (new_pet_id, snapshot_dir) = if let Some(existing) = existing_pet_with_same_id {
        match pet_action {
            None => {
                // No decision yet — surface the collision to the
                // frontend with enough context to render a useful
                // prompt. Bail before touching anything.
                let state_row = db.get_pet_state(&existing.id).await.ok().flatten();
                let event_count = db
                    .list_xp_events_for_pet(&existing.id)
                    .await
                    .map(|v| v.len() as u64)
                    .unwrap_or(0);
                report.existing_pet = Some(ExistingPetInfo {
                    id: existing.id.clone(),
                    name: existing.name.clone(),
                    current_level: state_row.as_ref().map(|s| s.current_level).unwrap_or(0),
                    total_xp: state_row.as_ref().map(|s| s.total_xp).unwrap_or(0),
                    event_count,
                });
                report.status = ImportStatus::PetIdExists;
                return Ok(InstallPetOutcome {
                    pet_id: existing.id,
                    pet_name: existing.name,
                    merged: false,
                });
            }
            Some("merge") => {
                // Reuse existing pet id + snapshot dir. New events
                // get appended; composite dedup handles overlap.
                merged = true;
                let dir = PathBuf::from(&existing.snapshot_path);
                (existing.id.clone(), dir)
            }
            Some("copy") => {
                // Fall through to fresh-id branch below.
                let new_id = Uuid::new_v4().to_string();
                let dir = paths::pets_dir().join(&new_id);
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                write_rewritten_pet_json(&dir, &pet_v, &new_id, &pet_body)?;
                let origin = db.ensure_install_id().await.map_err(|e| e.to_string())?;
                db.insert_pet(
                    &new_id,
                    &pet_name,
                    template_id,
                    dir.to_string_lossy().as_ref(),
                    born_at,
                    false,
                    &origin,
                )
                .await
                .map_err(|e| e.to_string())?;
                if name_finalized_at.is_some() {
                    db.finalize_pet_name(&new_id, Some(pet_name.clone()))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                (new_id, dir)
            }
            Some(other) => {
                return Err(format!(
                    "unknown pet_action {other:?} — expected \"merge\" or \"copy\""
                ));
            }
        }
    } else {
        // No collision — install fresh, mint new id.
        let new_id = Uuid::new_v4().to_string();
        let dir = paths::pets_dir().join(&new_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        write_rewritten_pet_json(&dir, &pet_v, &new_id, &pet_body)?;
        let origin = db.ensure_install_id().await.map_err(|e| e.to_string())?;
        db.insert_pet(
            &new_id,
            &pet_name,
            template_id,
            dir.to_string_lossy().as_ref(),
            born_at,
            false,
            &origin,
        )
        .await
        .map_err(|e| e.to_string())?;
        if name_finalized_at.is_some() {
            db.finalize_pet_name(&new_id, Some(pet_name.clone()))
                .await
                .map_err(|e| e.to_string())?;
        }
        (new_id, dir)
    };

    // Parse xp_events.jsonl line by line. Malformed lines get
    // skipped with a warning rather than aborting the whole import —
    // a single bad row should not prevent a years-raised pet from
    // moving between machines.
    let log_path = archive_root.join("pet").join("xp_events.jsonl");
    tracing::info!(
        archive_root = %archive_root.display(),
        log_path = %log_path.display(),
        log_exists = log_path.exists(),
        "install_pet: probing xp_events.jsonl",
    );
    let mut parsed_events: Vec<XpEventInsert> = Vec::new();
    let mut skipped_malformed = 0usize;
    if log_path.exists() {
        let body = std::fs::read_to_string(&log_path).map_err(|e| e.to_string())?;
        tracing::info!(
            bytes = body.len(),
            line_count = body.lines().count(),
            "install_pet: read xp_events.jsonl",
        );
        for (lineno, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<XpEventInsert>(line) {
                Ok(mut ev) => {
                    // Rewrite pet_id to the freshly allocated local
                    // one so events attach to THIS install's pet row,
                    // not the exporter's.
                    ev.pet_id = new_pet_id.clone();
                    // Regenerate the primary-key `id`. The source
                    // archive carried deterministic UUID v5 ids hashed
                    // from the EXPORTER's pet_id + source_type +
                    // source_ref. When the exporter's pet still lives
                    // in our DB (e.g. user exports + imports on the
                    // same machine to test), those ids collide with
                    // existing rows and `INSERT OR IGNORE` silently
                    // skips all 96 events — the pet imports as Lv.0
                    // despite the JSONL parsing correctly. A fresh
                    // v4 id is unique by construction; the (pet_id,
                    // source_type, source_ref) unique index still
                    // dedupes if the user re-imports the same archive
                    // into the same local pet.
                    ev.id = Uuid::new_v4().to_string();
                    parsed_events.push(ev);
                }
                Err(e) => {
                    skipped_malformed += 1;
                    report.warnings.push(format!(
                        "xp_events line {} malformed ({e}) — skipped",
                        lineno + 1
                    ));
                }
            }
        }
    }

    // Replay events AND recompute `pet_state` in one helper. The
    // recompute step is what fixes the "import a Lv.20+ pet, it
    // shows as Lv.0" bug — without it, every xp_event row lands
    // correctly but the cached `pet_state.total_xp` stays at 0 and
    // the snapshot reads 0.
    let pet_doc = {
        let dir = snapshot_dir.clone();
        tokio::task::spawn_blocking(move || load_pet_doc(&dir))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("load pet.json: {e}"))?
    };
    tracing::info!(
        events_to_replay = parsed_events.len(),
        new_pet_id = %new_pet_id,
        "install_pet: calling replay_events_and_recompute",
    );
    let (inserted, dedup_skipped) =
        replay_events_and_recompute(db, &new_pet_id, &pet_doc, &parsed_events)
            .await
            .map_err(|e| format!("replay: {e}"))?;
    tracing::info!(
        inserted, dedup_skipped, skipped_malformed,
        "install_pet: replay complete",
    );
    report.xp_events_imported = inserted;
    report.xp_events_skipped = dedup_skipped + skipped_malformed;
    Ok(InstallPetOutcome {
        pet_id: new_pet_id,
        pet_name,
        merged,
    })
}

/// Rewrite pet.json's `id` field in-place to `new_id` and write the
/// result to `dir/pet.json`. Extracted so both the fresh-install and
/// "copy" branches can reuse it.
fn write_rewritten_pet_json(
    dir: &Path,
    pet_v: &Value,
    new_id: &str,
    original_body_fallback: &str,
) -> Result<(), String> {
    let mut pet_v_mut = pet_v.clone();
    if let Some(obj) = pet_v_mut.as_object_mut() {
        obj.insert("id".to_string(), Value::String(new_id.to_string()));
    }
    let body =
        serde_json::to_string_pretty(&pet_v_mut).unwrap_or_else(|_| original_body_fallback.into());
    std::fs::write(dir.join("pet.json"), body).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// Build a stage input where the trigger is the canonical leaf
    /// `{metric:"level", value:N}`. Used by tests below — keeps each
    /// test focused on the property under test, not the JSON ceremony.
    fn stage_input(idx: u32, level: f64, sprite_path: Option<String>) -> StageStubInput {
        StageStubInput {
            id: format!("stage_{idx}"),
            name: format!("Stage {idx}"),
            flavor: String::new(),
            trigger: json!({"metric": "level", "value": level}),
            sprite_path,
        }
    }

    /// Seed `dest/stages/stage_N/sprite.png` with distinguishable
    /// bytes so the remap-vs-user-pick assertions below can tell
    /// which path produced the final file at each stage index.
    fn seed_existing_stages_with_sprites(dest: &std::path::Path, sprite_per_idx: &[(u32, Vec<u8>)]) {
        let stages_dir = dest.join("stages");
        std::fs::create_dir_all(&stages_dir).unwrap();
        for (idx, bytes) in sprite_per_idx {
            let folder = stages_dir.join(format!("stage_{idx}"));
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("sprite.png"), bytes).unwrap();
            // A stage.json must exist for the FOLDER to count as a
            // valid stage when the template loader scans later; the
            // apply step under test wipes these so the seed content
            // doesn't matter, but presence is good hygiene for the
            // realism of the test setup.
            std::fs::write(
                folder.join("stage.json"),
                r#"{"id":"placeholder","name":"placeholder","trigger":{"metric":"level","value":0}}"#,
            )
            .unwrap();
        }
    }

    /// `sprite_path` set on a stage MUST result in that file's bytes
    /// living at `<template>/stages/stage_N/sprite.png`. This is the
    /// contract the StagesEditor relies on when the user picks a
    /// per-stage image — anything else (silent fallback to remap,
    /// wrong stage folder, byte drift) is a bug that would ship a
    /// broken template to the user.
    #[test]
    fn apply_stages_writes_user_picked_sprite_to_correct_stage_folder() {
        let dest = tempdir().unwrap();
        // Mimic the cloned-base state: 10 existing stages, each with
        // a different sprite byte so the remap path is observable.
        let seed: Vec<(u32, Vec<u8>)> = (0..10)
            .map(|i| (i as u32, vec![b'A' + (i as u8); 16]))
            .collect();
        seed_existing_stages_with_sprites(dest.path(), &seed);

        // User picks a custom sprite for stage_1 only. The bytes are
        // a distinctive marker we can grep for at the end.
        let user_sprite = tempdir().unwrap();
        let user_path = user_sprite.path().join("wukong.png");
        let user_bytes: &[u8] = b"USER_PICKED_SPRITE_BYTES_12345";
        std::fs::write(&user_path, user_bytes).unwrap();

        let stages = vec![
            stage_input(0, 0.0, None),
            stage_input(1, 5.0, Some(user_path.to_string_lossy().to_string())),
            stage_input(2, 30.0, None),
        ];
        apply_stages_to_template(dest.path(), &stages).expect("apply succeeded");

        // 1. User pick lands at the exact stage they picked it for.
        let stage_1_sprite = dest.path().join("stages/stage_1/sprite.png");
        assert!(stage_1_sprite.exists(), "stage_1 sprite must exist");
        assert_eq!(
            std::fs::read(&stage_1_sprite).unwrap(),
            user_bytes,
            "stage_1 should carry the user-picked bytes verbatim"
        );

        // 2. Non-picked stages fall back to the linear remap. With
        //    new_count=3 and old_count=10, the formula
        //    round(i * (old-1) / (new-1)) = round(i * 9 / 2) gives
        //    stage_0 → old[0], stage_2 → old[9].
        let stage_0_sprite = dest.path().join("stages/stage_0/sprite.png");
        let stage_2_sprite = dest.path().join("stages/stage_2/sprite.png");
        assert_eq!(
            std::fs::read(&stage_0_sprite).unwrap(),
            seed[0].1.clone(),
            "stage_0 should remap from old stage_0"
        );
        assert_eq!(
            std::fs::read(&stage_2_sprite).unwrap(),
            seed[9].1.clone(),
            "stage_2 should remap from old stage_9 (last)"
        );

        // 3. stage.json is created per stage with the right id and
        //    parseable structure. We don't deep-check every field —
        //    that's covered by the loader's own validation — but a
        //    smoke check catches the case where the writer fails to
        //    produce template-loader-valid JSON.
        for i in 0..3 {
            let stage_json = dest.path().join(format!("stages/stage_{i}/stage.json"));
            let body = std::fs::read_to_string(&stage_json).unwrap();
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["id"], format!("stage_{i}"));
            assert_eq!(v["trigger"]["metric"], "level");
        }

        // 4. Old surplus stage folders (3..9) are wiped — otherwise
        //    the loader would try to read them and fail contiguity.
        for i in 3..10 {
            let leftover = dest.path().join(format!("stages/stage_{i}"));
            assert!(
                !leftover.exists(),
                "stage_{i} should have been removed when stages count shrank to 3",
            );
        }
    }

    /// When ALL stages have a user-picked sprite, no remap should
    /// occur — every final stage carries the user's bytes. Guards
    /// against an off-by-one in the precedence logic that would let
    /// the remap overwrite a user pick.
    #[test]
    fn apply_stages_user_picks_take_precedence_over_remap() {
        let dest = tempdir().unwrap();
        seed_existing_stages_with_sprites(
            dest.path(),
            &[
                (0, b"OLD_A".to_vec()),
                (1, b"OLD_B".to_vec()),
                (2, b"OLD_C".to_vec()),
            ],
        );

        let user_dir = tempdir().unwrap();
        let mut stages = Vec::new();
        let mut expected_bytes: Vec<Vec<u8>> = Vec::new();
        for i in 0..3 {
            let path = user_dir.path().join(format!("custom_{i}.png"));
            let bytes = format!("NEW_USER_BYTES_{i}").into_bytes();
            std::fs::write(&path, &bytes).unwrap();
            stages.push(stage_input(
                i,
                (i * 10) as f64,
                Some(path.to_string_lossy().to_string()),
            ));
            expected_bytes.push(bytes);
        }

        apply_stages_to_template(dest.path(), &stages).expect("apply succeeded");

        for (i, want) in expected_bytes.iter().enumerate() {
            let got = std::fs::read(dest.path().join(format!("stages/stage_{i}/sprite.png")))
                .unwrap();
            assert_eq!(&got, want, "stage_{i} must have user bytes, not remap");
        }
    }

    /// Sprite_path validation rejects bad inputs BEFORE
    /// apply_stages_to_template runs (the actual UI surface) so the
    /// user sees a clean error and the template dir is never created.
    #[test]
    fn validate_stages_rejects_missing_or_oversized_sprite_paths() {
        // Non-existent file.
        let bad = vec![stage_input(
            0,
            0.0,
            Some("/nonexistent/path/to/wukong.png".into()),
        )];
        let err = validate_stages_input(bad).unwrap_err();
        assert!(err.contains("can't be read"), "got: {err}");

        // Relative path (must be absolute).
        let rel = vec![stage_input(0, 0.0, Some("relative/path.png".into()))];
        let err = validate_stages_input(rel).unwrap_err();
        assert!(err.contains("absolute"), "got: {err}");

        // Oversized file.
        let big_src = tempdir().unwrap();
        let big_path = big_src.path().join("huge.png");
        std::fs::write(&big_path, vec![0u8; (MAX_SPRITE_BYTES + 1) as usize]).unwrap();
        let big = vec![stage_input(0, 0.0, Some(big_path.to_string_lossy().to_string()))];
        let err = validate_stages_input(big).unwrap_err();
        assert!(err.contains("max allowed"), "got: {err}");
    }
}
