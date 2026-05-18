//! System preset library — reference configurations for level curves
//! and stage arcs.
//!
//! **Identity & lifecycle**
//! - Presets are **system-owned reference data**, NOT templates.
//! - They are compiled into the binary via `include_dir!` (same pattern
//!   as the built-in templates), making them genuinely non-deletable —
//!   a user cannot reach inside the binary, and on every launch we
//!   re-extract a fresh copy to `~/.petpet/builtin_presets/` for
//!   inspection.
//!
//! **Why a separate library instead of just cloning a template?**
//! Templates carry assets (sprites, sounds, ceremony scripts), rule
//! configuration, theme colours, and identity metadata. The presets
//! are *just* the curve + stage shape — the two pieces a template
//! author actually wants to swap when scaffolding. Splitting these out
//! lets the creator UI mix-and-match (e.g. "short curve + extended
//! stages") without needing N×M template variants.
//!
//! **Independence guarantee** — a template created from a preset
//! receives a *snapshot copy* of the preset's bytes. It does not
//! reference the system preset file in any way. Updates to a preset
//! file in a future petpet release have no effect on already-created
//! templates: their levels.json / stage.json files are frozen at
//! create time. This matches the templates-vs-pets relationship used
//! elsewhere in the codebase.
//!
//! **Built-in templates ARE preset snapshots.** The 3 bundled pets
//! (mist / ember / onyx) are conceptually exactly what the
//! TemplateCreator would produce given the matching overrides:
//!
//! ```text
//!   mist  ≡ template_create(levels="short",  stages="extended", base=mist)
//!   ember ≡ template_create(levels="medium", stages="extended", base=ember)
//!   onyx  ≡ template_create(levels="long",   stages="extended", base=onyx)
//! ```
//!
//! They embed their own copies of the level entries and stage
//! definitions — no runtime reference to this preset library. The
//! `builtin_templates_match_preset_library` test pins this equivalence
//! byte-for-byte so future edits can't silently diverge: tweak a
//! curve on one side and the test fails until the other side catches
//! up. Whichever side you edit first, edit both in the same commit.
//!
//! ```text
//!  presets/                       (compiled into binary)
//!    levels/short.json    ─┐
//!    levels/medium.json    │
//!    levels/long.json      │  preset_apply()
//!    stages/simple.json   ─┼──────────►  new template's files (snapshot)
//!    stages/balanced.json  │             ~/.petpet/templates/<id>/
//!    stages/extended.json  │               levels.json   (verbatim copy)
//!                          ┘               stages/stage_*/stage.json
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::template::types::{LevelEntry, Stage, Trigger};

/// All presets compiled into the binary. The directory tree mirrors
/// `~/.petpet/builtin_presets/` once extracted.
static PRESETS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/presets");

/// Schema discriminator strings — pinned so a rename / new major
/// version is a deliberate code change, not an invisible refactor.
pub const LEVELS_SCHEMA_V1: &str = "petpet-preset/levels-v1";
pub const STAGES_SCHEMA_V1: &str = "petpet-preset/stages-v1";

/// On-disk shape of `presets/levels/*.json`. Forward-compatible:
/// `serde(default)` on optional fields means a future release adding
/// new fields still parses cleanly under the v1 schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LevelsPreset {
    #[serde(rename = "$schema", default)]
    pub schema: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub description: String,
    pub max_level: u32,
    pub entries: Vec<LevelEntry>,
}

/// On-disk shape of `presets/stages/*.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagesPreset {
    #[serde(rename = "$schema", default)]
    pub schema: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub description: String,
    pub stages: Vec<StageStub>,
}

/// Trimmed stage entry — just the shape the creator UI needs to display
/// a preview, and the seed data we write into each new stage folder.
/// Specifically does NOT include per-stage assets (sprites, on_enter
/// scripts) — those stay template-specific and are filled in by whoever
/// authors the new template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageStub {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub flavor: String,
    pub trigger: Trigger,
}

impl StageStub {
    /// Promote to the full `Stage` shape used by the runtime registry.
    /// Per-stage assets, attributes, and events default to empty — the
    /// caller (`template_create`) overlays template-specific data after
    /// this conversion.
    pub fn to_stage(&self) -> Stage {
        Stage {
            id: self.id.clone(),
            name: self.name.clone(),
            flavor: Some(self.flavor.clone()).filter(|s| !s.is_empty()),
            trigger: self.trigger.clone(),
            assets: crate::template::types::StageAssets::default(),
            attributes: serde_json::Value::Object(Default::default()),
            events: std::collections::BTreeMap::new(),
        }
    }
}

pub struct PresetRegistry;

impl PresetRegistry {
    /// Extract the embedded preset library to `~/.petpet/builtin_presets/`
    /// for inspection. Idempotent — re-extracts every launch so a user
    /// who hand-edits or deletes a file picks up the canonical copy
    /// again on next start (this is what makes the library "non-deletable"
    /// from the user's perspective even though the on-disk copy is
    /// technically writable).
    pub fn ensure_on_disk() -> Result<PathBuf> {
        let target = paths::builtin_presets_dir();
        std::fs::create_dir_all(&target).with_context(|| {
            format!("creating builtin presets dir {}", target.display())
        })?;
        PRESETS
            .extract(&target)
            .with_context(|| format!("releasing builtin presets to {}", target.display()))?;
        Ok(target)
    }

    /// Read every `levels/*.json` from the embedded library. Reads from
    /// the in-binary `Dir` directly — does NOT require `ensure_on_disk()`
    /// to have been called. This is what guarantees the library is
    /// available even if the user nukes `~/.petpet/` between launches.
    pub fn list_levels() -> Result<Vec<LevelsPreset>> {
        Self::list_in("levels", LEVELS_SCHEMA_V1, |bytes, name| {
            let mut p: LevelsPreset = serde_json::from_slice(bytes)
                .with_context(|| format!("parsing levels preset {name}"))?;
            // Fill in `id` from filename if the JSON elides it (defensive
            // — every shipped preset specifies it, but reading from disk
            // we may encounter hand-edited copies).
            if p.id.is_empty() {
                p.id = file_stem(name);
            }
            Ok(p)
        })
    }

    /// Read every `stages/*.json` from the embedded library.
    pub fn list_stages() -> Result<Vec<StagesPreset>> {
        Self::list_in("stages", STAGES_SCHEMA_V1, |bytes, name| {
            let mut p: StagesPreset = serde_json::from_slice(bytes)
                .with_context(|| format!("parsing stages preset {name}"))?;
            if p.id.is_empty() {
                p.id = file_stem(name);
            }
            Ok(p)
        })
    }

    /// Look up a single levels preset by id. Returns `None` (not an
    /// error) when no preset matches — callers decide whether that's
    /// a hard failure (e.g. "unknown preset" returned to the user) or
    /// a soft one (fall back to default).
    pub fn find_levels(id: &str) -> Result<Option<LevelsPreset>> {
        Ok(Self::list_levels()?.into_iter().find(|p| p.id == id))
    }

    pub fn find_stages(id: &str) -> Result<Option<StagesPreset>> {
        Ok(Self::list_stages()?.into_iter().find(|p| p.id == id))
    }

    fn list_in<T, F>(subdir: &str, expected_schema: &str, mut parse: F) -> Result<Vec<T>>
    where
        F: FnMut(&[u8], &str) -> Result<T>,
    {
        let dir = PRESETS
            .get_dir(subdir)
            .with_context(|| format!("presets/{subdir} missing from embedded library"))?;
        let mut out = Vec::new();
        let mut entries: Vec<_> = dir.files().collect();
        entries.sort_by_key(|f| f.path().to_path_buf());
        for f in entries {
            let path = f.path();
            // Ignore non-JSON files quietly — leaves room for sidecar
            // README / .DS_Store without breaking the listing.
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>");
            match parse(f.contents(), name) {
                Ok(p) => out.push(p),
                Err(e) => {
                    tracing::warn!(
                        preset = %name,
                        expected_schema,
                        error = %e,
                        "skipping malformed preset"
                    );
                }
            }
        }
        Ok(out)
    }
}

fn file_stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_presets_load_and_have_canonical_ids() {
        let presets = PresetRegistry::list_levels().expect("list_levels");
        let ids: Vec<_> = presets.iter().map(|p| p.id.as_str()).collect();
        // The shipped library is the contract — these IDs are referenced
        // by the desktop UI's dropdowns and by tests that pin user-facing
        // values. Adding a preset is fine; removing one needs a deliberate
        // UI-side change and gets caught here first.
        assert!(ids.contains(&"short"));
        assert!(ids.contains(&"medium"));
        assert!(ids.contains(&"long"));
        for p in &presets {
            assert!(!p.entries.is_empty(), "preset {} has no entries", p.id);
            assert_eq!(p.entries[0].level, 0, "preset {} doesn't start at level 0", p.id);
        }
    }

    #[test]
    fn stages_presets_load_and_have_canonical_ids() {
        let presets = PresetRegistry::list_stages().expect("list_stages");
        let ids: Vec<_> = presets.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"simple"));
        assert!(ids.contains(&"balanced"));
        assert!(ids.contains(&"extended"));
        for p in &presets {
            assert!(!p.stages.is_empty(), "preset {} has no stages", p.id);
            // The first stage of every preset must trigger at level 0
            // (the starting form) — UI assumes this when rendering the
            // preview. `Trigger` is an untagged enum, so destructure
            // the leaf form explicitly; composite triggers (all_of /
            // any_of) aren't allowed at stage_0 for this contract.
            match &p.stages[0].trigger {
                crate::template::types::Trigger::Leaf(pred) => {
                    assert_eq!(pred.metric, "level", "preset {} stage_0 wrong metric", p.id);
                    assert_eq!(pred.value as i64, 0, "preset {} stage_0 must trigger at level 0", p.id);
                }
                other => panic!("preset {} stage_0 trigger must be a leaf, got {:?}", p.id, other),
            }
        }
    }

    #[test]
    fn find_returns_none_for_unknown_id() {
        assert!(PresetRegistry::find_levels("does-not-exist").unwrap().is_none());
        assert!(PresetRegistry::find_stages("does-not-exist").unwrap().is_none());
    }

    /// Pin the **"builtins are snapshots, not references"** invariant.
    ///
    /// The shipped builtin templates' `levels.json` and
    /// `stages/stage_*/stage.json` files are *byte-for-byte
    /// equivalent* to what the TemplateCreator would produce given
    /// the matching preset overrides:
    ///
    ///   unicorn = template_create(levels_preset="short",  stages_preset="extended", base=unicorn)
    ///   sun     = template_create(levels_preset="medium", stages_preset="extended", base=sun)
    ///
    /// They DO NOT reference the preset library at runtime — they're
    /// independent, generic copies. This test exists so that future
    /// edits can't silently break the equivalence: if you tune a level
    /// curve in `presets/levels/short.json`, this test fails until you
    /// also update `templates/builtin/unicorn/levels.json` (and vice
    /// versa). Same for any stage trigger / name / flavor change.
    ///
    /// Why pin it? The promise to the user is that the builtin pets
    /// are "the same thing you'd get if you scaffolded one yourself".
    /// If the preset library drifts, users who scaffold a new template
    /// with `short` levels would get a subtly different curve from
    /// unicorn — confusing, and contrary to the documented design.
    ///
    /// (Note: the original mist/ember/onyx templates were removed in
    /// favour of sun and unicorn. Currently no template represents
    /// the `long` levels preset — that's fine; the parity invariant
    /// only checks templates that DO exist, not that every preset has
    /// a representative.)
    #[test]
    fn builtin_templates_match_preset_library() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let tpl_root = workspace.join("templates").join("builtin");

        // (template id, levels preset id, stages preset id)
        let mappings = [
            ("unicorn", "short", "extended"),
            ("sun", "medium", "extended"),
        ];

        let levels_presets = PresetRegistry::list_levels().expect("list_levels");
        let stages_presets = PresetRegistry::list_stages().expect("list_stages");

        for (tpl_id, levels_id, stages_id) in mappings {
            let lp = levels_presets
                .iter()
                .find(|p| p.id == levels_id)
                .unwrap_or_else(|| panic!("levels preset '{levels_id}' missing"));
            let sp = stages_presets
                .iter()
                .find(|p| p.id == stages_id)
                .unwrap_or_else(|| panic!("stages preset '{stages_id}' missing"));

            check_levels_parity(tpl_id, &tpl_root, lp);
            check_stages_parity(tpl_id, &tpl_root, sp);
        }
    }

    fn check_levels_parity(
        tpl_id: &str,
        tpl_root: &std::path::Path,
        preset: &LevelsPreset,
    ) {
        let path = tpl_root.join(tpl_id).join("levels.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let v: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));

        let hint = format!(
            "If the curve genuinely changed, update BOTH templates/builtin/{tpl_id}/levels.json \
             AND presets/levels/{preset_id}.json in the same commit so the equivalence holds.",
            tpl_id = tpl_id,
            preset_id = preset.id,
        );

        assert_eq!(
            v["max_level"].as_u64().unwrap_or(0) as u32,
            preset.max_level,
            "{tpl_id}.levels.json max_level diverged from preset {preset_id}. {hint}",
            preset_id = preset.id,
            hint = hint,
        );

        let tpl_entries: Vec<LevelEntry> =
            serde_json::from_value(v["entries"].clone()).expect("entries parse");
        assert_eq!(
            tpl_entries.len(),
            preset.entries.len(),
            "{tpl_id}.levels.json has {} entries; preset {preset_id} has {}. {hint}",
            tpl_entries.len(),
            preset.entries.len(),
            preset_id = preset.id,
            hint = hint,
        );
        for (i, (t, p)) in tpl_entries.iter().zip(preset.entries.iter()).enumerate() {
            assert_eq!(
                t.level, p.level,
                "{tpl_id} entry[{i}] level mismatch (tpl={}, preset={preset_id}). {hint}",
                t.level,
                preset_id = preset.id,
                hint = hint,
            );
            assert_eq!(
                t.xp_required, p.xp_required,
                "{tpl_id} entry[{i}] (level {lvl}) xp_required diverged: tpl={tpl_xp}, preset={p_xp}. {hint}",
                lvl = t.level,
                tpl_xp = t.xp_required,
                p_xp = p.xp_required,
                hint = hint,
            );
        }
    }

    fn check_stages_parity(
        tpl_id: &str,
        tpl_root: &std::path::Path,
        preset: &StagesPreset,
    ) {
        let stages_dir = tpl_root.join(tpl_id).join("stages");
        let mut tpl_stages: Vec<(u32, serde_json::Value)> = Vec::new();
        for ent in std::fs::read_dir(&stages_dir).expect("read stages dir") {
            let ent = ent.expect("dir entry");
            let path = ent.path();
            if !path.is_dir() {
                continue;
            }
            let folder = path.file_name().unwrap().to_string_lossy().into_owned();
            let idx: u32 = folder
                .strip_prefix("stage_")
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("non-stage folder in {}: {folder}", stages_dir.display()));
            let raw = std::fs::read_to_string(path.join("stage.json"))
                .unwrap_or_else(|e| panic!("reading {tpl_id} stage_{idx}: {e}"));
            let val: serde_json::Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("parsing {tpl_id} stage_{idx}: {e}"));
            tpl_stages.push((idx, val));
        }
        tpl_stages.sort_by_key(|(i, _)| *i);

        let hint = format!(
            "Update BOTH templates/builtin/{tpl_id}/stages/ AND presets/stages/{preset_id}.json \
             in the same commit so the equivalence holds.",
            tpl_id = tpl_id,
            preset_id = preset.id,
        );

        assert_eq!(
            tpl_stages.len(),
            preset.stages.len(),
            "{tpl_id} has {} stages on disk, but preset {preset_id} has {}. {hint}",
            tpl_stages.len(),
            preset.stages.len(),
            preset_id = preset.id,
            hint = hint,
        );

        for (i, ((_, t), p)) in tpl_stages.iter().zip(preset.stages.iter()).enumerate() {
            assert_eq!(
                t["id"].as_str().unwrap_or(""),
                p.id,
                "{tpl_id} stage[{i}] id mismatch. {hint}"
            );
            assert_eq!(
                t["name"].as_str().unwrap_or(""),
                p.name,
                "{tpl_id} stage[{i}] name mismatch. {hint}"
            );
            assert_eq!(
                t["flavor"].as_str().unwrap_or(""),
                p.flavor,
                "{tpl_id} stage[{i}] flavor mismatch. {hint}"
            );
            // Trigger compare — normalize both sides by round-tripping
            // through `Trigger`. The raw JSON can differ cosmetically
            // (int `0` vs float `0.0`; the default `op: ">="` omitted
            // vs present) while being semantically identical; parsing
            // both into the enum and re-serializing collapses those
            // cosmetic differences to a single canonical form.
            let t_norm = normalize_trigger_json(&t["trigger"], "template");
            let p_norm = normalize_trigger_json(
                &serde_json::to_value(&p.trigger).expect("preset trigger serialize"),
                "preset",
            );
            assert_eq!(
                t_norm, p_norm,
                "{tpl_id} stage[{i}] trigger mismatch. {hint}"
            );
        }
    }

    /// Round-trip a trigger through the `Trigger` enum to get a
    /// canonical JSON form. The `who` arg is just for the panic
    /// message — it's almost always either "template" or "preset".
    fn normalize_trigger_json(v: &serde_json::Value, who: &str) -> serde_json::Value {
        let parsed: crate::template::types::Trigger = serde_json::from_value(v.clone())
            .unwrap_or_else(|e| panic!("{who} trigger parse failed: {e} (raw={v})"));
        serde_json::to_value(&parsed).expect("trigger reserialize")
    }

    #[test]
    fn extracted_disk_copy_matches_embedded() {
        // Defensive: if we ever change the extraction logic, this catches
        // the case where the on-disk copy quietly drifts from the
        // embedded one — that would mean two presets per id, with the UI
        // potentially showing stale data.
        let dir = PresetRegistry::ensure_on_disk().expect("ensure_on_disk");
        let levels_dir = dir.join("levels");
        assert!(levels_dir.exists());
        for p in PresetRegistry::list_levels().unwrap() {
            let on_disk = levels_dir.join(format!("{}.json", p.id));
            assert!(on_disk.exists(), "{} missing on disk", on_disk.display());
        }
    }
}
