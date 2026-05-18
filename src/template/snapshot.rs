//! Pet snapshot creation.
//!
//! `create_pet_from_template(template, name, origin_device_id)` does the
//! whole template → pet ceremony:
//!   1. Allocate a new UUID for the pet.
//!   2. Create `~/.petpet/pets/<uuid>/`.
//!   3. Deep-copy every field of the template into a `PetDoc`, plus
//!      pet identity (id, name, born_at, origin).
//!   4. Write `pet.json`.
//!   5. Copy assets (sheet.png / sheet.json / thumb.png if present).
//!   6. Return the populated `PetDoc` (caller inserts the DB row).

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::paths;
use crate::template::registry::{LoadedTemplate, TemplateSource};
use crate::template::types::{PetDoc, PetOrigin};

pub struct CreatedPet {
    pub doc: PetDoc,
    pub snapshot_dir: PathBuf,
}

pub fn create_pet_from_template(
    loaded: &LoadedTemplate,
    name: String,
    origin_device_id: String,
) -> Result<CreatedPet> {
    let id = Uuid::new_v4().to_string();
    let pet_dir = paths::pets_dir().join(&id);
    std::fs::create_dir_all(&pet_dir)
        .with_context(|| format!("creating pet dir {}", pet_dir.display()))?;

    let now = Utc::now();
    let doc = PetDoc {
        schema: "petpet-pet/v1".to_string(),
        id: id.clone(),
        name,
        born_at: now,
        name_finalized_at: None,
        origin_device_id,
        origin: PetOrigin {
            template_id: loaded.template.meta.id.clone(),
            template_version: loaded.template.meta.version.clone(),
            source: loaded.source.as_str().to_string(),
            snapshotted_at: now,
        },
        species: loaded.template.species.clone(),
        levels: loaded.template.levels.clone(),
        stages: loaded.template.stages.clone(),
        rules: loaded.template.rules.clone(),
        theme: loaded.template.theme.clone(),
        assets: loaded.template.assets.clone(),
    };

    let pet_json_path = pet_dir.join("pet.json");
    let serialized = serde_json::to_string_pretty(&doc)?;
    std::fs::write(&pet_json_path, serialized)
        .with_context(|| format!("writing {}", pet_json_path.display()))?;

    // Copy assets — best-effort. Missing assets log a warning but
    // do not fail snapshot creation (frontend renders placeholders).
    for asset_name in [
        loaded.template.assets.sheet.as_deref(),
        loaded.template.assets.frames.as_deref(),
        loaded.template.assets.thumb.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let src = loaded.dir.join(asset_name);
        let dst = pet_dir.join(asset_name);
        if src.exists() {
            if let Err(e) = std::fs::copy(&src, &dst) {
                tracing::warn!(
                    src = %src.display(),
                    dst = %dst.display(),
                    error = %e,
                    "asset copy failed — pet will render with placeholder",
                );
            }
        } else {
            tracing::debug!(asset = %src.display(), "template asset missing — skipping");
        }
    }

    let _ = TemplateSource::Builtin; // ensure import is live
    Ok(CreatedPet {
        doc,
        snapshot_dir: pet_dir,
    })
}

/// Load an existing pet.json from disk. Used by XPEngine to hydrate
/// per-pet stages + rules at runtime.
pub fn load_pet_doc(pet_dir: &std::path::Path) -> Result<PetDoc> {
    let manifest = pet_dir.join("pet.json");
    let raw = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let doc: PetDoc = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", manifest.display()))?;
    Ok(doc)
}

/// Update only `name` / `name_finalized_at` in pet.json — preserves
/// every other field. Used by finalize_naming and rename operations.
pub fn update_pet_identity(
    pet_dir: &std::path::Path,
    name: Option<String>,
    name_finalized_at: Option<chrono::DateTime<Utc>>,
) -> Result<PetDoc> {
    let mut doc = load_pet_doc(pet_dir)?;
    if let Some(n) = name {
        doc.name = n;
    }
    if let Some(ts) = name_finalized_at {
        doc.name_finalized_at = Some(ts);
    }
    let manifest = pet_dir.join("pet.json");
    let serialized = serde_json::to_string_pretty(&doc)?;
    std::fs::write(&manifest, serialized)?;
    Ok(doc)
}

