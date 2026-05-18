//! Pet template system.
//!
//! A **template** is a `template.json` file plus assets (sprite sheet,
//! thumbnail) describing how to instantiate a pet. Templates are pure
//! authoring data — no Rust constants, no system-base rules. Three
//! built-in templates (mist/ember/onyx) ship embedded via `include_dir!`
//! and are released to disk on first launch.
//!
//! A **pet** is the result of `template → snapshot`: a `pet.json` file
//! with the merged stages/rules + copies of the template's assets, all
//! frozen at creation time. The pet is fully self-contained — the
//! template can be edited or deleted afterward without affecting it.
//!
//! A **preset** (see [`presets`]) is a generic level curve or stage
//! arc that lives in the system library and never carries assets. The
//! TemplateCreator UI uses presets to scaffold new templates with
//! sensible defaults. Importantly: **the 3 built-in templates ARE
//! equivalent to what the TemplateCreator would emit given the
//! matching preset overrides** (mist=short/extended, ember=medium/
//! extended, onyx=long/extended). They embed snapshot copies of those
//! preset bytes — no runtime dependency on the preset library. See
//! `presets::tests::builtin_templates_match_preset_library` for the
//! pinned-equivalence test that locks in this invariant.
//!
//! ```text
//!   templates/   (filesystem, authoring)        ┌─ id, name
//!     mist/                                     │  born_at
//!       template.json   ──┐                     │  snapshot_path
//!       sheet.png         │  pick_template()    │
//!       thumb.png         ├────────────────►   pet row in DB
//!                         │  (deep copy)        │
//!                         │                     │  + folder on disk:
//!   ~/.petpet/pets/<id>/  │                     │     pet.json (frozen)
//!     pet.json           ◄┘                     │     sheet.png (copy)
//!     sheet.png                                 │     thumb.png (copy)
//!     thumb.png                                 └─
//! ```

pub mod archive;
pub mod presets;
pub mod registry;
pub mod snapshot;
pub mod types;

pub use archive::{
    pack_directory, unpack_archive, ArchiveKind, ArchiveManifest, CompatVerdict, PetSummary,
    UnpackError, Unpacked,
};
pub use presets::{LevelsPreset, PresetRegistry, StageStub, StagesPreset};
pub use registry::{TemplateRegistry, TemplateSource};
pub use snapshot::create_pet_from_template;
pub use types::{
    Author, Label, LevelCurve, LevelEntry, Op, PetDoc, PetOrigin, Predicate, Stage, StageAssets,
    Template, TemplateAssets, TemplateMeta, TemplateRule, TemplateSpecies, TemplateTheme,
    Trigger, TriggerContext, KNOWN_METRICS, METRIC_LEVEL, METRIC_PET_AGE_DAYS, METRIC_XP_TOTAL,
};
