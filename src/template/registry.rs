//! Template registry — discovers and validates templates from
//! filesystem.
//!
//! New layout (post-refactor):
//!
//! ```text
//!   templates/<id>/
//!     template.json    ─ identity, species, labels, theme, assets
//!     levels.json      ─ explicit per-level XP curve
//!     stages/
//!       stage_0/
//!         stage.json   ─ trigger + name + flavor + attributes
//!         on_enter.json ─ optional: transition ceremony
//!         idle.json    ─ optional: idle animation
//!         on_*.json    ─ optional: reactive event ceremonies
//!       stage_1/
//!         ...
//!     rules.json       ─ XP scoring rules (sidecar)
//!     sprite.png       ─ optional shared sheet
//!     thumb.png        ─ catalog thumbnail
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use include_dir::{include_dir, Dir};

use crate::paths;
use crate::template::types::{LevelCurve, Stage, StageAssets, Template, KNOWN_METRICS};

/// Built-in templates compiled into the binary.
static BUILTIN: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/templates/builtin");

#[derive(Debug, Clone)]
pub struct LoadedTemplate {
    pub template: Template,
    pub source: TemplateSource,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSource {
    Builtin,
    Community,
    Custom,
}

impl TemplateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TemplateSource::Builtin => "builtin",
            TemplateSource::Community => "community",
            TemplateSource::Custom => "custom",
        }
    }
}

pub struct TemplateRegistry;

impl TemplateRegistry {
    pub fn ensure_builtins_on_disk() -> Result<PathBuf> {
        let target = paths::builtin_templates_dir();
        std::fs::create_dir_all(&target).with_context(|| {
            format!("creating builtin templates dir {}", target.display())
        })?;
        BUILTIN
            .extract(&target)
            .with_context(|| format!("releasing builtin templates to {}", target.display()))?;
        Ok(target)
    }

    pub fn discover() -> Result<Vec<LoadedTemplate>> {
        let mut found = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        let builtin_dir = Self::ensure_builtins_on_disk()?;
        for entry in fs_dirs(&builtin_dir)? {
            if let Some(loaded) = try_load_dir(&entry, TemplateSource::Builtin)? {
                if seen_ids.insert(loaded.template.meta.id.clone()) {
                    found.push(loaded);
                } else {
                    tracing::warn!(id = %loaded.template.meta.id, "duplicate builtin id, skipping");
                }
            }
        }

        let user_dir = paths::user_templates_dir();
        if user_dir.exists() {
            for entry in fs_dirs(&user_dir)? {
                let source = if entry.join(".custom").exists() {
                    TemplateSource::Custom
                } else {
                    TemplateSource::Community
                };
                if let Some(loaded) = try_load_dir(&entry, source)? {
                    if seen_ids.insert(loaded.template.meta.id.clone()) {
                        found.push(loaded);
                    } else {
                        tracing::warn!(
                            id = %loaded.template.meta.id,
                            dir = %entry.display(),
                            "template id already loaded; skipping duplicate",
                        );
                    }
                }
            }
        }

        Ok(found)
    }

    pub fn find(id: &str) -> Result<Option<LoadedTemplate>> {
        Ok(Self::discover()?
            .into_iter()
            .find(|t| t.template.meta.id == id))
    }
}

fn fs_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for ent in std::fs::read_dir(root)
        .with_context(|| format!("reading templates dir {}", root.display()))?
    {
        let ent = ent?;
        let p = ent.path();
        if p.is_dir() {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn try_load_dir(dir: &Path, source: TemplateSource) -> Result<Option<LoadedTemplate>> {
    let manifest = dir.join("template.json");
    if !manifest.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let mut template: Template = match serde_json::from_str(&raw) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(dir = %dir.display(), error = %e, "invalid template.json — skipping");
            return Ok(None);
        }
    };

    // Merge sidecars in order: levels → stages → rules.
    if let Err(e) = merge_sidecar_levels(dir, &mut template) {
        tracing::error!(dir = %dir.display(), error = %e, "failed loading levels.json — skipping");
        return Ok(None);
    }
    if let Err(e) = merge_sidecar_stages(dir, &mut template) {
        tracing::error!(dir = %dir.display(), error = %e, "failed loading stages — skipping");
        return Ok(None);
    }
    if let Err(e) = merge_sidecar_rules(dir, &mut template) {
        tracing::error!(dir = %dir.display(), error = %e, "failed loading rules.json — skipping");
        return Ok(None);
    }

    if let Err(e) = validate(&template) {
        tracing::error!(
            id = %template.meta.id,
            dir = %dir.display(),
            error = %e,
            "template failed validation — skipping",
        );
        return Ok(None);
    }

    Ok(Some(LoadedTemplate {
        template,
        source,
        dir: dir.to_path_buf(),
    }))
}

/// Load `levels.json` if the template didn't inline `levels`.
fn merge_sidecar_levels(dir: &Path, template: &mut Template) -> Result<()> {
    if !template.levels.entries.is_empty() {
        return Ok(());
    }
    let sidecar = dir.join("levels.json");
    if !sidecar.exists() {
        return Err(anyhow!("no levels in template.json and no levels.json sidecar"));
    }
    let raw = std::fs::read_to_string(&sidecar)
        .with_context(|| format!("reading {}", sidecar.display()))?;
    template.levels = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", sidecar.display()))?;
    Ok(())
}

/// Scan `stages/stage_N/` subdirectories. Each folder contains:
///   - stage.json (required)
///   - on_enter.json, idle.json, on_*.json (optional event ceremonies)
///   - attributes.json (optional — merged into stage.attributes)
fn merge_sidecar_stages(dir: &Path, template: &mut Template) -> Result<()> {
    if !template.stages.is_empty() {
        return Ok(());
    }
    let stages_dir = dir.join("stages");
    if !stages_dir.exists() {
        return Err(anyhow!("no stages in template.json and no stages/ folder"));
    }

    let mut stages: Vec<(u32, Stage)> = Vec::new();
    for entry in std::fs::read_dir(&stages_dir)
        .with_context(|| format!("reading stages dir {}", stages_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("invalid stage folder name at {}", path.display()))?;
        // Folder must match ^stage_(\d+)$
        let index: u32 = folder_name
            .strip_prefix("stage_")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                anyhow!(
                    "stage folder name '{}' must match 'stage_<N>' format",
                    folder_name
                )
            })?;

        let stage = load_stage_folder(&path, folder_name)?;
        stages.push((index, stage));
    }
    if stages.is_empty() {
        return Err(anyhow!("stages/ folder is empty — at least stage_0 required"));
    }

    // Sort by index.
    stages.sort_by_key(|(i, _)| *i);

    // Validate: must be 0-indexed contiguous.
    for (pos, (idx, _)) in stages.iter().enumerate() {
        if *idx != pos as u32 {
            return Err(anyhow!(
                "stage indices must be 0-indexed contiguous; expected stage_{} but got stage_{}",
                pos,
                idx
            ));
        }
    }

    template.stages = stages.into_iter().map(|(_, s)| s).collect();
    Ok(())
}

/// Load one `stages/stage_N/` folder into a Stage struct.
/// stage.json provides identity + trigger + attributes;
/// other .json files in the same folder become entries in `events`.
fn load_stage_folder(dir: &Path, folder_name: &str) -> Result<Stage> {
    let manifest = dir.join("stage.json");
    if !manifest.exists() {
        return Err(anyhow!("missing stage.json in {}", dir.display()));
    }
    let raw = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let mut stage: Stage = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", manifest.display()))?;

    // Enforce: folder name === stage.json.id
    if stage.id != folder_name {
        return Err(anyhow!(
            "stage.json id '{}' must match folder name '{}'",
            stage.id,
            folder_name
        ));
    }

    // Auto-discover event files: on_*.json + idle.json
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if fname == "stage.json" || !fname.ends_with(".json") {
            continue;
        }

        let event_name = if fname == "idle.json" {
            "idle".to_string()
        } else if let Some(rest) = fname.strip_prefix("on_").and_then(|r| r.strip_suffix(".json")) {
            format!("on_{}", rest)
        } else if fname == "attributes.json" {
            // attributes.json → merge into stage.attributes
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let attrs: serde_json::Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if stage.attributes.is_null() || stage.attributes.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                stage.attributes = attrs;
            }
            continue;
        } else {
            // Unknown JSON file — log + skip
            tracing::debug!(file = %path.display(), "unknown stage file (not on_*.json/idle.json/attributes.json), ignoring");
            continue;
        };

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let ceremonies: Vec<serde_json::Value> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        stage.events.insert(event_name, ceremonies);
    }

    // Apply default StageAssets from sprite.png / sprite.json sibling
    // files when stage.json didn't specify.
    apply_default_stage_assets(dir, &mut stage);

    Ok(stage)
}

fn apply_default_stage_assets(dir: &Path, stage: &mut Stage) {
    if stage.assets.sprite.is_none() && dir.join("sprite.png").exists() {
        stage.assets = StageAssets {
            sprite: Some("sprite.png".to_string()),
            frames: if dir.join("sprite.json").exists() {
                Some("sprite.json".to_string())
            } else {
                None
            },
        };
    }
}

fn merge_sidecar_rules(dir: &Path, template: &mut Template) -> Result<()> {
    if !template.rules.is_empty() {
        return Ok(());
    }
    let sidecar = dir.join("rules.json");
    if sidecar.exists() {
        let raw = std::fs::read_to_string(&sidecar)
            .with_context(|| format!("reading {}", sidecar.display()))?;
        template.rules = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", sidecar.display()))?;
    }
    Ok(())
}

/// All validation rules — invalid templates are skipped at load time.
pub fn validate(t: &Template) -> Result<()> {
    // ID format. Allows lowercase, digits, hyphen, AND period.
    //
    // Period: the canonical separator the TemplateCreator uses to
    // namespace user templates as `<author>.<name>` (matching the
    // `mars.drakon` convention shown in the creator's id-hint).
    //
    // Digit-leading: an author who enters `123` as their name produces
    // ids like `123.test` — that's an idiot-proof slug, not malicious
    // input, and the id is opaque to code paths (it's a directory name
    // and a database key, never parsed). Rejecting it would silently
    // drop the user's template with no on-screen explanation. So
    // leading char may be a lowercase letter OR a digit; only `.` and
    // `-` are disallowed at the start (they'd produce surprising path
    // semantics: `.foo` becomes hidden on Unix, `-foo` looks like a
    // CLI flag).
    //
    // We don't formally enforce that periods can't be leading /
    // trailing / repeated — a hand-edited template.json with `..` or
    // trailing `.` is still allowed at load. The creator's slugger
    // never emits those, and a user hand-editing JSON can fix at
    // their leisure.
    let id = &t.meta.id;
    let valid_chars = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    let valid_lead = id
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .unwrap_or(false);
    if !valid_chars || id.len() < 3 || id.len() > 40 || !valid_lead {
        return Err(anyhow!(
            "meta.id '{}' must match ^[a-z0-9][a-z0-9.-]{{2,39}}$",
            id
        ));
    }

    // Levels must be non-empty + dense + monotonic.
    validate_levels(&t.levels)?;

    // Stages must exist + be 0-indexed contiguous + triggers monotonic.
    validate_stages(&t.stages)?;

    // Rule IDs unique.
    let mut rule_ids = std::collections::HashSet::new();
    for r in &t.rules {
        if !rule_ids.insert(&r.id) {
            return Err(anyhow!("duplicate rule id '{}'", r.id));
        }
    }

    Ok(())
}

fn validate_levels(levels: &LevelCurve) -> Result<()> {
    if levels.entries.is_empty() {
        return Err(anyhow!("levels must have at least one entry (level 0)"));
    }
    // Must start at level 0 with xp_required = 0
    if levels.entries[0].level != 0 || levels.entries[0].xp_required != 0 {
        return Err(anyhow!(
            "first level entry must be level=0, xp_required=0; got level={}, xp_required={}",
            levels.entries[0].level,
            levels.entries[0].xp_required
        ));
    }
    // Dense + monotonic
    for (i, e) in levels.entries.iter().enumerate() {
        if e.level != i as u32 {
            return Err(anyhow!(
                "level entries must be dense from 0..=max_level; expected level={} but got level={}",
                i,
                e.level
            ));
        }
        if i > 0 && e.xp_required <= levels.entries[i - 1].xp_required {
            return Err(anyhow!(
                "level xp_required must be strictly increasing; L{}={} <= L{}={}",
                e.level,
                e.xp_required,
                levels.entries[i - 1].level,
                levels.entries[i - 1].xp_required
            ));
        }
    }
    let last = &levels.entries[levels.entries.len() - 1];
    if last.level != levels.max_level {
        return Err(anyhow!(
            "max_level={} but last entry level={}",
            levels.max_level,
            last.level
        ));
    }
    Ok(())
}

fn validate_stages(stages: &[Stage]) -> Result<()> {
    if stages.is_empty() {
        return Err(anyhow!("template has no stages"));
    }

    // ID format + contiguity already enforced by folder scanner, but
    // re-check here for inlined-stages case (templates that didn't use
    // stages/ folder).
    let mut ids = std::collections::HashSet::new();
    for (i, s) in stages.iter().enumerate() {
        let expected = format!("stage_{}", i);
        if s.id != expected {
            return Err(anyhow!(
                "stage at index {} must have id '{}', got '{}'",
                i,
                expected,
                s.id
            ));
        }
        if !ids.insert(&s.id) {
            return Err(anyhow!("duplicate stage id '{}'", s.id));
        }
        // Depth limit on triggers (prevent runaway nesting)
        if s.trigger.depth() > 5 {
            return Err(anyhow!(
                "stage '{}' trigger nesting depth ({}) exceeds maximum 5",
                s.id,
                s.trigger.depth()
            ));
        }
        // Every metric a predicate references must be one the engine
        // populates. Catches typos like {"metric": "lvl", value: 10}.
        let mut metrics = Vec::new();
        s.trigger.collect_metrics(&mut metrics);
        for m in metrics {
            if !KNOWN_METRICS.contains(&m) {
                return Err(anyhow!(
                    "stage '{}' references unknown metric '{}'; known metrics: {:?}",
                    s.id,
                    m,
                    KNOWN_METRICS
                ));
            }
        }
    }

    // stage_0 must have an "always true" trigger (effective min level 0).
    let s0 = &stages[0];
    if s0.trigger.min_level_required() != 0 {
        return Err(anyhow!(
            "stage_0 trigger must have min_level_required = 0, got {}",
            s0.trigger.min_level_required()
        ));
    }

    // Subsequent stages must have strictly increasing min_level.
    let mut prev_min = 0u32;
    for (i, s) in stages.iter().enumerate() {
        let cur = s.trigger.min_level_required();
        if i == 0 {
            prev_min = cur;
            continue;
        }
        if cur <= prev_min {
            return Err(anyhow!(
                "stage_{} trigger min_level ({}) must be strictly greater than stage_{} ({})",
                i,
                cur,
                i - 1,
                prev_min
            ));
        }
        prev_min = cur;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::types::{
        LevelEntry, Predicate, TemplateMeta, TemplateSpecies, Trigger,
    };
    use std::collections::BTreeMap;

    #[test]
    fn builtin_dirs_exist() {
        // Smoke-check that `include_dir!` picked up the shipped
        // builtins. Original mist/ember/onyx were retired in favour
        // of sun + unicorn; if either of these goes missing the
        // picker / pet creation would silently break.
        assert!(BUILTIN.get_dir("sun").is_some());
        assert!(BUILTIN.get_dir("unicorn").is_some());
        assert!(BUILTIN.get_dir("kingkong").is_some());
    }

    fn stage_at(idx: u32, trigger: Trigger) -> Stage {
        Stage {
            id: format!("stage_{idx}"),
            name: format!("Stage {idx}"),
            flavor: None,
            trigger,
            assets: Default::default(),
            attributes: serde_json::json!({}),
            events: BTreeMap::new(),
        }
    }

    fn leaf(metric: &str, value: f64) -> Trigger {
        Trigger::Leaf(Predicate {
            metric: metric.to_string(),
            op: Default::default(),
            value,
        })
    }

    #[test]
    fn validate_stages_rejects_unknown_metric() {
        let stages = vec![
            stage_at(0, leaf("level", 0.0)),
            stage_at(1, leaf("focus_streak", 3.0)),
        ];
        let err = validate_stages(&stages).unwrap_err().to_string();
        assert!(
            err.contains("unknown metric 'focus_streak'"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_stages_accepts_all_known_metrics() {
        // Each stage's min_level is strictly increasing; composite stage
        // uses AllOf so min_level = max of children = the level term.
        let stages = vec![
            stage_at(0, leaf("level", 0.0)),
            stage_at(1, leaf("level", 5.0)),
            stage_at(
                2,
                Trigger::AllOf {
                    all_of: vec![leaf("level", 10.0), leaf("xp_total", 5000.0)],
                },
            ),
            stage_at(
                3,
                Trigger::AllOf {
                    all_of: vec![leaf("level", 30.0), leaf("pet_age_days", 14.0)],
                },
            ),
        ];
        validate_stages(&stages).expect("all metrics known");
    }

    /// Build a minimally-valid template, optionally with a custom id.
    /// Used by the id-format tests below — keeps each test focused on
    /// the assertion that matters without duplicating the boilerplate
    /// every Template construction needs.
    fn tpl_with_id(id: &str) -> Template {
        Template {
            schema: "petpet-template/v1".into(),
            meta: TemplateMeta {
                id: id.into(),
                name: "Test".into(),
                version: "1.0.0".into(),
                author: None,
                license: None,
                description: None,
                source_url: None,
                display_order: None,
            },
            species: TemplateSpecies {
                name: "Test".into(),
                description: None,
                default_pet_name: None,
                flavor: None,
            },
            labels: vec![],
            theme: Default::default(),
            assets: Default::default(),
            levels: LevelCurve {
                max_level: 0,
                entries: vec![LevelEntry { level: 0, xp_required: 0 }],
            },
            stages: vec![stage_at(0, leaf("level", 0.0))],
            rules: vec![],
        }
    }

    /// Pin the id-format contract: namespaced ids like the
    /// TemplateCreator emits (`<author>.<name>`) MUST validate.
    ///
    /// Earlier this regression suite caught two silent-drop bugs:
    ///   1. validator's regex was `[a-z0-9-]` — no `.` → `mars.drakon`
    ///      was rejected on load
    ///   2. validator required a leading lowercase LETTER → a user
    ///      with author "123" got `123.test`, which still failed
    ///
    /// Both fixed; this test holds the new contract. The user-visible
    /// impact was the same in both cases: create succeeds, picker
    /// stays empty, no error banner. Hence the strong test surface.
    #[test]
    fn validate_accepts_namespaced_ids_with_periods() {
        // Canonical creator-emitted shape.
        validate(&tpl_with_id("mars.drakon")).expect("mars.drakon should validate");
        // Multi-segment / digit-bearing variants the slugger can emit.
        validate(&tpl_with_id("mars.sun-wukong"))
            .expect("hyphenated namespaced id should validate");
        validate(&tpl_with_id("alice-2.dragon-1"))
            .expect("digit-bearing namespaced id should validate");
        // Built-in style — no period — still works.
        validate(&tpl_with_id("sun")).expect("bare lowercase id should validate");
    }

    /// Authors who type a numeric name ("123") produce digit-leading
    /// ids. These are valid (the id is an opaque key, not a parsed
    /// expression) and must NOT be silently dropped.
    #[test]
    fn validate_accepts_digit_leading_ids() {
        validate(&tpl_with_id("123.test"))
            .expect("digit-leading id from numeric author should validate");
        validate(&tpl_with_id("123.456")).expect("all-numeric id should validate");
        validate(&tpl_with_id("9-lives")).expect("digit-then-hyphen id should validate");
    }

    /// Pin the dual-shape author contract.
    ///
    /// On-disk we accept either `"author": "Mars"` (string, what the
    /// 3 builtins ship) OR `"author": {"name": "Mars", "url": "…"}`
    /// (object, what `template_create` writes). Before this enum
    /// existed, the schema was `Option<String>` and the object form
    /// failed to deserialize at load — `template_list` silently
    /// dropped every user-scaffolded template with no error banner.
    /// This test catches a regression to either single-shape schema.
    #[test]
    fn template_meta_accepts_both_author_shapes() {
        use crate::template::types::Author;

        let with_string = r#"{
            "id": "test.string",
            "name": "Test",
            "version": "1.0.0",
            "author": "Mars"
        }"#;
        let m: TemplateMeta =
            serde_json::from_str(with_string).expect("string author should parse");
        assert!(matches!(m.author, Some(Author::Simple(ref s)) if s == "Mars"));
        assert_eq!(m.author.as_ref().and_then(|a| a.name()), Some("Mars"));

        let with_object = r#"{
            "id": "test.obj",
            "name": "Test",
            "version": "1.0.0",
            "author": {"name": "Mars", "url": "https://example.com"}
        }"#;
        let m: TemplateMeta =
            serde_json::from_str(with_object).expect("object author should parse");
        assert!(matches!(m.author, Some(Author::Detailed { .. })));
        assert_eq!(m.author.as_ref().and_then(|a| a.name()), Some("Mars"));

        let with_partial_object = r#"{
            "id": "test.partial",
            "name": "Test",
            "version": "1.0.0",
            "author": {"name": "OnlyName"}
        }"#;
        let m: TemplateMeta = serde_json::from_str(with_partial_object)
            .expect("object author with only name should parse");
        assert_eq!(m.author.as_ref().and_then(|a| a.name()), Some("OnlyName"));

        let without_author = r#"{
            "id": "test.none",
            "name": "Test",
            "version": "1.0.0"
        }"#;
        let m: TemplateMeta =
            serde_json::from_str(without_author).expect("missing author should parse");
        assert!(m.author.is_none());
    }

    #[test]
    fn validate_rejects_disallowed_id_characters() {
        // Uppercase, spaces, slashes, underscore, leading `.` / `-` —
        // none in the allowed set. Digit-leading is INTENTIONALLY
        // permitted (see `validate_accepts_digit_leading_ids`), so
        // historical-test cases like "1foo" no longer belong here.
        for bad in [
            "Foo.bar",   // uppercase
            "foo bar",   // space
            "foo/bar",   // slash
            "foo_bar",   // underscore
            ".foo",      // leading period (hidden on Unix)
            "-foo",      // leading hyphen (CLI-flag-ish)
            "ab",        // too short (< 3 chars)
        ] {
            assert!(
                validate(&tpl_with_id(bad)).is_err(),
                "expected '{bad}' to be rejected by validate()",
            );
        }
    }
}
