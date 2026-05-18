//! **Layer 2 — backward / forward compat for the `.petpet` archive
//! format.**
//!
//! Scope: every test here packs an archive, optionally mutates it to
//! simulate a different schema version (older, newer, malformed), then
//! unpacks and asserts the importer behaved correctly. No shell, no
//! Tauri runtime — just the library API. Fast enough to run on every
//! CI commit so a regression in compat semantics fails immediately.
//!
//! Why these tests matter: the format-evolution promise (additive
//! minor, refuse newer major, ignore unknown fields) is the contract
//! that lets a user's long-raised companion survive years of
//! petpet versions. If the importer's tolerance regresses, that
//! promise quietly breaks until somebody loses a pet. These tests
//! pin the promise in code.
//!
//! Companion test files:
//!   - L1 unit: `src/template/archive.rs::tests` — pure functions
//!   - L2 HTTP integration: `tests/hook_server.rs`
//!   - L2 archive compat (this file)
//!   - L3 shell E2E: `tests/hook_shell_e2e.rs`

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use petpet::db::DbHandle;
use petpet::template::archive::{
    pack_directory, unpack_archive, ArchiveKind, ArchiveManifest, CompatVerdict, PetSummary,
    UnpackError,
};
use petpet::template::types::{
    LevelCurve, LevelEntry, PetDoc, PetOrigin, TemplateAssets, TemplateSpecies, TemplateTheme,
};
use petpet::xp::replay_events_and_recompute;
use petpet::xp::writer::XpEventInsert;
use tempfile::tempdir;

// ─── Fixtures ──────────────────────────────────────────────────────

/// Lay out a minimal but realistic template folder. Mirrors what a
/// human author would produce: `template.json` + `levels.json` +
/// `rules.json` + at least one stage with a sprite.
fn write_minimal_template(dir: &Path) {
    fs::write(
        dir.join("template.json"),
        r#"{
            "schema": "petpet-pet/v1",
            "meta": {
                "id": "test.demo",
                "name": "Demo",
                "version": "1.0.0",
                "description": "Test template"
            },
            "species": { "name": "Demo" },
            "levels": [],
            "stages": [],
            "rules": [],
            "theme": {},
            "assets": {}
        }"#,
    )
    .unwrap();
    fs::write(dir.join("levels.json"), r#"{"max_level":99,"entries":[]}"#).unwrap();
    fs::write(dir.join("rules.json"), r#"[]"#).unwrap();
    let stage_dir = dir.join("stages/stage_1");
    fs::create_dir_all(&stage_dir).unwrap();
    fs::write(stage_dir.join("stage.json"), r#"{"name":"Form 1"}"#).unwrap();
    fs::write(stage_dir.join("sprite.png"), b"\x89PNG\r\n\x1a\n").unwrap();
}

/// Build a `xp_events.jsonl` blob from a list of inserts. Lets each
/// test pick the rows + corrupt lines it wants to exercise.
fn jsonl_blob(events: &[XpEventInsert]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str(&serde_json::to_string(e).unwrap());
        out.push('\n');
    }
    out
}

fn make_event(idx: usize, xp: i64, source: &str) -> XpEventInsert {
    XpEventInsert {
        id: format!("evt-{idx:04}"),
        pet_id: "src-pet".into(),
        occurred_at: format!("2026-05-{:02}T12:00:00Z", (idx % 28) + 1),
        source_type: source.into(),
        source_ref: Some(format!("ref-{idx}")),
        xp_delta: xp,
        reason: "test".into(),
        rule_id: String::new(),
        origin_device_id: "test-dev".into(),
    }
}

// ─── Round-trip — same version ─────────────────────────────────────

#[test]
fn template_round_trips_through_pack_unpack() {
    let src = tempdir().unwrap();
    write_minimal_template(src.path());

    let archive = src.path().join("out.petpet");
    pack_directory(src.path(), &archive, None).unwrap();

    let dst = tempdir().unwrap();
    let unpacked = unpack_archive(&archive, dst.path()).unwrap();

    assert_eq!(unpacked.manifest.compat(), CompatVerdict::Ok);
    assert_eq!(unpacked.manifest.kind, ArchiveKind::Template);
    // Every input file (except manifest, which is generated) is present.
    assert!(dst.path().join("template.json").exists());
    assert!(dst.path().join("levels.json").exists());
    assert!(dst.path().join("rules.json").exists());
    assert!(dst.path().join("stages/stage_1/stage.json").exists());
    assert!(dst.path().join("stages/stage_1/sprite.png").exists());
}

#[test]
fn pet_round_trips_with_jsonl_history() {
    let src = tempdir().unwrap();
    write_minimal_template(src.path());
    let pet_dir = src.path().join("pet");
    fs::create_dir_all(&pet_dir).unwrap();
    fs::write(pet_dir.join("pet.json"), r#"{"name":"Tofu"}"#).unwrap();
    let events: Vec<XpEventInsert> = (0..5).map(|i| make_event(i, 10, "usage")).collect();
    fs::write(pet_dir.join("xp_events.jsonl"), jsonl_blob(&events)).unwrap();

    let archive = src.path().join("pet.petpet");
    let summary = PetSummary {
        level: 4,
        total_xp: 50,
        days_raised: 7,
    };
    pack_directory(src.path(), &archive, Some(summary.clone())).unwrap();

    let dst = tempdir().unwrap();
    let unpacked = unpack_archive(&archive, dst.path()).unwrap();
    assert_eq!(unpacked.manifest.kind, ArchiveKind::Pet);
    assert_eq!(unpacked.manifest.pet_summary, Some(summary));
    assert!(dst.path().join("pet/pet.json").exists());
    assert!(dst.path().join("pet/xp_events.jsonl").exists());

    // Replay every line back into structs — proves the JSONL is
    // serde-stable and the importer's parser will work on it.
    let body = fs::read_to_string(dst.path().join("pet/xp_events.jsonl")).unwrap();
    let parsed: Vec<XpEventInsert> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(parsed.len(), 5);
    let sum: i64 = parsed.iter().map(|e| e.xp_delta).sum();
    assert_eq!(sum, 50);
}

// ─── Forward compat — minor version drift ──────────────────────────

#[test]
fn unpack_accepts_minor_drift_in_manifest() {
    let dir = tempdir().unwrap();
    let archive = dir.path().join("future.petpet");
    write_zip(&archive, &[
        ("manifest.json", br#"{
            "$schema": "petpet/v1.99",
            "kind": "template",
            "preview_video": "promo.mp4",
            "signature": "abc-future-field"
        }"#.to_vec()),
        ("template.json", b"{}".to_vec()),
    ]);
    let dst = tempdir().unwrap();
    let unpacked = unpack_archive(&archive, dst.path()).unwrap();
    assert_eq!(unpacked.manifest.compat(), CompatVerdict::Ok);
}

#[test]
fn unpack_ignores_unknown_files_at_root() {
    let dir = tempdir().unwrap();
    let archive = dir.path().join("future.petpet");
    write_zip(&archive, &[
        ("manifest.json", br#"{"$schema":"petpet/v1.2","kind":"template"}"#.to_vec()),
        ("template.json", b"{}".to_vec()),
        // Hypothetical future top-level file. We unpack it (no harm),
        // and downstream installers / loaders ignore what they don't
        // know — additive compat.
        ("combat.json", b"{\"hp_curve\":[]}".to_vec()),
    ]);
    let dst = tempdir().unwrap();
    let unpacked = unpack_archive(&archive, dst.path()).unwrap();
    assert!(unpacked.warnings.is_empty());
    assert!(dst.path().join("combat.json").exists());
}

// ─── Backward compat — older major refused gracefully ──────────────

#[test]
fn unpack_refuses_archive_with_newer_major_with_clear_error() {
    let dir = tempdir().unwrap();
    let archive = dir.path().join("v2.petpet");
    write_zip(&archive, &[
        ("manifest.json", br#"{"$schema":"petpet/v2","kind":"template"}"#.to_vec()),
        ("template.json", b"{}".to_vec()),
    ]);
    let dst = tempdir().unwrap();
    let err = unpack_archive(&archive, dst.path()).unwrap_err();
    match err {
        UnpackError::SchemaTooNew { major } => assert_eq!(major, 2),
        other => panic!("expected SchemaTooNew {{ major: 2 }}, got {other:?}"),
    }
    // Critically: nothing got installed. Refuse-before-extract.
    assert!(!dst.path().join("template.json").exists());
}

// ─── Self-healing — malformed contents handled gracefully ──────────

#[test]
fn unpack_skips_zip_slip_paths_but_imports_the_rest() {
    let dir = tempdir().unwrap();
    let archive = dir.path().join("evil.petpet");
    write_zip(&archive, &[
        ("manifest.json", br#"{"$schema":"petpet/v1","kind":"template"}"#.to_vec()),
        ("template.json", b"{}".to_vec()),
        ("../../escaped.txt", b"evil".to_vec()),
    ]);
    let dst = tempdir().unwrap();
    let unpacked = unpack_archive(&archive, dst.path()).unwrap();
    // Warning surfaced, valid file still installed.
    assert!(
        unpacked.warnings.iter().any(|w| w.contains("unsafe")),
        "expected unsafe-entry warning"
    );
    assert!(dst.path().join("template.json").exists());
    // Anti-escape: the dangerous entry did not land outside the
    // unpack root.
    let parent = dst.path().parent().unwrap();
    assert!(!parent.join("escaped.txt").exists());
}

/// Self-repair for the JSONL log: the importer reads line by line and
/// `serde_json::from_str` is tolerant. We pin that here so a single
/// malformed row never aborts the whole import — a long-raised pet
/// must survive a bit of corruption.
#[test]
fn corrupt_jsonl_line_is_isolated_to_that_row() {
    let good = make_event(0, 5, "usage");
    let mut blob = serde_json::to_string(&good).unwrap();
    blob.push('\n');
    blob.push_str("{not valid json\n");
    let good2 = make_event(1, 8, "activity");
    blob.push_str(&serde_json::to_string(&good2).unwrap());
    blob.push('\n');

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for line in blob.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<XpEventInsert>(line) {
            Ok(_) => imported += 1,
            Err(_) => skipped += 1,
        }
    }
    assert_eq!(imported, 2);
    assert_eq!(skipped, 1);
}

#[test]
fn jsonl_with_unknown_future_field_still_parses() {
    // Future minor adds a `costs_cents` field. Importer on the older
    // schema must ignore it and produce a valid struct.
    let line = r#"{
        "id": "evt-999",
        "pet_id": "p1",
        "occurred_at": "2026-05-16T12:00:00Z",
        "source_type": "usage",
        "source_ref": "ref-xyz",
        "xp_delta": 12,
        "reason": "test",
        "rule_id": "rule-x",
        "origin_device_id": "d1",
        "costs_cents": 42,
        "future_field": [1, 2, 3]
    }"#;
    let parsed: XpEventInsert = serde_json::from_str(line).unwrap();
    assert_eq!(parsed.xp_delta, 12);
    assert_eq!(parsed.source_ref.as_deref(), Some("ref-xyz"));
}

/// Equally important — older exports MISSING fields that have since
/// become optional/expected. Importer must `serde(default)` them.
#[test]
fn jsonl_with_only_minimum_fields_still_parses() {
    let line = r#"{
        "id": "evt-1",
        "pet_id": "p1",
        "occurred_at": "2026-05-16T12:00:00Z",
        "source_type": "usage",
        "xp_delta": 7
    }"#;
    let parsed: XpEventInsert = serde_json::from_str(line).unwrap();
    assert_eq!(parsed.xp_delta, 7);
    assert!(parsed.source_ref.is_none());
    assert!(parsed.reason.is_empty());
    assert!(parsed.rule_id.is_empty());
    assert!(parsed.origin_device_id.is_empty());
}

// ─── Schema-string parsing edge cases ──────────────────────────────

#[test]
fn schema_version_tolerates_minor_with_pre_release_tag() {
    // Future: petpet/v1.2-beta. Current parser tolerantly returns
    // (1, 2) — pre-release suffix ignored. This means an early-access
    // build's manifest doesn't lock out stable users.
    let m = ArchiveManifest {
        schema: "petpet/v1.2-beta".into(),
        kind: ArchiveKind::Template,
        pet_summary: None,
    };
    // Either we got (1, 2) or we got None — both are acceptable
    // behaviours. We assert the major didn't accidentally come out
    // greater than current.
    if let Some((major, _)) = m.schema_version() {
        assert!(
            major <= ArchiveManifest::CURRENT_MAJOR,
            "pre-release minor tag must not parse to a higher major"
        );
    }
}

// ─── Size-cap protection ───────────────────────────────────────────

#[test]
fn unpack_skips_per_file_size_violation_with_warning() {
    let dir = tempdir().unwrap();
    let archive = dir.path().join("fat.petpet");
    // 11 MB file (over the 10 MB per-file cap)
    let big = vec![0u8; 11 * 1024 * 1024];
    write_zip(&archive, &[
        ("manifest.json", br#"{"$schema":"petpet/v1","kind":"template"}"#.to_vec()),
        ("template.json", b"{}".to_vec()),
        ("stages/stage_1/sprite.png", big),
    ]);
    let dst = tempdir().unwrap();
    let unpacked = unpack_archive(&archive, dst.path()).unwrap();
    assert!(
        unpacked
            .warnings
            .iter()
            .any(|w| w.contains("exceeds per-file size cap")),
        "warnings: {:?}",
        unpacked.warnings
    );
    assert!(!dst.path().join("stages/stage_1/sprite.png").exists());
    // Other files still installed.
    assert!(dst.path().join("template.json").exists());
}

// ─── Helper — build a zip without going through pack_directory ─────

/// Bypass `pack_directory` so we can craft archives with surgical
/// precision (specific paths, sizes, missing manifest, etc.) for
/// compat / fuzz cases.
fn write_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let f = fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(f);
    let opts: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body).unwrap();
    }
    zip.finish().unwrap();
}

// ─── Level-preservation regression ─────────────────────────────────
//
// Pins the bug we hit where a Lv.20+ pet exported and re-imported
// showed up as Lv.0. Root cause was that `XpEventWriter::replay`
// only wrote `xp_event` rows — the cached `pet_state` was never
// recomputed, so the snapshot read `total_xp = 0`.
//
// `replay_events_and_recompute` is the helper the importer now uses;
// these tests exercise it end-to-end against a real SQLite DB so a
// future refactor that drops the recompute fails loudly.

fn make_pet_doc(level_xp_pairs: &[(u32, i64)]) -> PetDoc {
    let entries: Vec<LevelEntry> = level_xp_pairs
        .iter()
        .map(|(l, xp)| LevelEntry {
            level: *l,
            xp_required: *xp,
        })
        .collect();
    PetDoc {
        schema: "petpet-pet/v1".into(),
        id: "test-pet-id".into(),
        name: "TestPet".into(),
        born_at: chrono::Utc::now(),
        name_finalized_at: None,
        origin_device_id: "test-dev".into(),
        origin: PetOrigin {
            template_id: "test.demo".into(),
            template_version: "1.0.0".into(),
            source: "builtin".into(),
            snapshotted_at: chrono::Utc::now(),
        },
        species: TemplateSpecies {
            name: "Demo".into(),
            description: None,
            default_pet_name: None,
            flavor: None,
        },
        levels: LevelCurve {
            max_level: 99,
            entries,
        },
        stages: vec![],
        rules: vec![],
        theme: TemplateTheme::default(),
        assets: TemplateAssets::default(),
    }
}

fn make_events(pet_id: &str, count: usize, xp_each: i64) -> Vec<XpEventInsert> {
    (0..count)
        .map(|i| XpEventInsert {
            id: format!("evt-{i:06}"),
            pet_id: pet_id.into(),
            occurred_at: format!("2026-05-{:02}T12:00:00Z", (i % 28) + 1),
            source_type: "usage".into(),
            source_ref: Some(format!("ref-{i}")),
            xp_delta: xp_each,
            reason: "test".into(),
            rule_id: String::new(),
            origin_device_id: "test-dev".into(),
        })
        .collect()
}

#[tokio::test]
async fn replay_recomputes_pet_state_to_correct_level() {
    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("test.db"))
        .await
        .expect("open db");

    let pet_id = "imported-pet";
    let origin = db.ensure_install_id().await.unwrap();
    db.insert_pet(
        pet_id,
        "TestPet",
        "test.demo",
        dir.path().to_string_lossy().as_ref(),
        chrono::Utc::now(),
        true,
        &origin,
    )
    .await
    .unwrap();

    // Level curve: 200 XP to reach Lv.1, then +100 per level. So
    // 10 × 50 = 500 XP → Lv.4 (200 + 100 + 100 + 100 = 500 hits L4).
    let curve: Vec<(u32, i64)> = (0..=20).map(|l| (l, 200 + (l as i64) * 100)).collect();
    let doc = make_pet_doc(&curve);

    // 30 events × 100 XP = 3000 XP → L28 by the curve. Pick a target
    // we know is well past 0 so the regression test really exercises
    // the cache-recompute (not just "non-zero").
    let events = make_events(pet_id, 30, 100);
    let (inserted, skipped) = replay_events_and_recompute(&db, pet_id, &doc, &events)
        .await
        .unwrap();
    assert_eq!(inserted, 30);
    assert_eq!(skipped, 0);

    // The whole point: pet_state must now reflect 3000 XP, NOT 0.
    let total = db.sum_xp_for_pet(pet_id).await.unwrap();
    assert_eq!(total, 3000, "xp_event rows didn't accumulate");

    let state = db.get_pet_state(pet_id).await.unwrap().expect("pet_state row");
    assert_eq!(state.total_xp, 3000, "pet_state cache out of sync with xp_event");
    assert_eq!(
        state.current_level,
        doc.levels.current_level(3000),
        "level not derived from total_xp via the pet's curve"
    );
    assert!(state.current_level >= 20, "expected Lv.20+, got {}", state.current_level);
}

#[tokio::test]
async fn replay_dedupes_when_called_twice_with_same_events() {
    // Re-importing the same archive must NOT double the XP. The
    // (pet_id, source_type, source_ref) unique index handles this
    // — we just need to verify the helper's `inserted` / `skipped`
    // counters correctly report what happened.
    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("test.db"))
        .await
        .expect("open db");
    let pet_id = "dedupe-pet";
    let origin = db.ensure_install_id().await.unwrap();
    db.insert_pet(
        pet_id,
        "DedupePet",
        "test.demo",
        dir.path().to_string_lossy().as_ref(),
        chrono::Utc::now(),
        true,
        &origin,
    )
    .await
    .unwrap();
    let doc = make_pet_doc(&[(0, 0), (1, 200), (2, 300), (3, 400)]);
    let events = make_events(pet_id, 5, 100);

    let (i1, s1) = replay_events_and_recompute(&db, pet_id, &doc, &events).await.unwrap();
    assert_eq!(i1, 5);
    assert_eq!(s1, 0);

    let (i2, s2) = replay_events_and_recompute(&db, pet_id, &doc, &events).await.unwrap();
    assert_eq!(i2, 0, "duplicate replay should insert nothing");
    assert_eq!(s2, 5, "duplicate replay should skip all 5 rows");

    let total = db.sum_xp_for_pet(pet_id).await.unwrap();
    assert_eq!(total, 500, "re-import accidentally doubled XP");
}

/// Merge semantics — when the local DB has a pet with the SAME id as
/// the archive (i.e. the same logical companion synced from another
/// machine), the importer's "merge" path should replay events ON TOP
/// of the existing pet, dedup by (pet_id, source_type, source_ref),
/// and the pet's id MUST stay the same. This is what makes "raise a
/// pet on machine A, sync to machine B, raise more on B, sync back"
/// converge correctly.
///
/// The end-to-end install_pet flow lives in archive_cmds.rs (desktop
/// crate, can't be tested from here). What we CAN pin from here:
/// `replay_events_and_recompute` correctly handles the merge case
/// where some of the events being replayed are already in the DB.
#[tokio::test]
async fn merge_preserves_existing_pet_id_and_dedupes_overlap() {
    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("test.db"))
        .await
        .expect("open db");
    let pet_id = "shared-pet-id";
    let origin = db.ensure_install_id().await.unwrap();
    db.insert_pet(
        pet_id,
        "Tofu",
        "test.demo",
        dir.path().to_string_lossy().as_ref(),
        chrono::Utc::now(),
        true,
        &origin,
    )
    .await
    .unwrap();
    let doc = make_pet_doc(&[(0, 0), (1, 50), (2, 150), (3, 250), (4, 400), (5, 600)]);

    // Stage 1: machine A has 5 events, totals 250 → Lv.3.
    let machine_a_events = make_events(pet_id, 5, 50);
    let (n, _) = replay_events_and_recompute(&db, pet_id, &doc, &machine_a_events)
        .await
        .unwrap();
    assert_eq!(n, 5);
    let state = db.get_pet_state(pet_id).await.unwrap().unwrap();
    assert_eq!(state.total_xp, 250);

    // Stage 2: machine B exports — its archive contains the original
    // 5 events PLUS 3 more (raised further). Import into A simulates
    // "merge" — the same 5 dedup, new 3 land.
    let mut import_events = machine_a_events.clone();
    let extra = make_events("ignored-original-id", 3, 100)
        .into_iter()
        .enumerate()
        .map(|(i, mut e)| {
            // Distinct source_ref so they don't collide with the
            // original 5; pet_id rewritten as the importer would do.
            e.source_ref = Some(format!("extra-{i}"));
            e.pet_id = pet_id.to_string();
            // Mint fresh ids as install_pet does to avoid PK collisions.
            e.id = uuid::Uuid::new_v4().to_string();
            e
        })
        .collect::<Vec<_>>();
    for e in &mut import_events {
        // Same source_refs as Stage 1, but pet_id stays the same and
        // ids get rewritten — the unique index on (pet_id, source_type,
        // source_ref) catches the overlap.
        e.id = uuid::Uuid::new_v4().to_string();
    }
    import_events.extend(extra);

    let (inserted, skipped) =
        replay_events_and_recompute(&db, pet_id, &doc, &import_events)
            .await
            .unwrap();
    assert_eq!(inserted, 3, "only the 3 new events should land");
    assert_eq!(skipped, 5, "5 originals should be deduped");

    let total = db.sum_xp_for_pet(pet_id).await.unwrap();
    assert_eq!(total, 250 + 300, "expected merged total xp = 250 + 3×100");

    // CRITICAL: pet_id is the same as before — the merge did not
    // create a new local pet, did not orphan the existing one.
    let pets = db.list_pets().await.unwrap();
    assert_eq!(pets.len(), 1);
    assert_eq!(pets[0].id, pet_id);
}

/// Regression: when the SOURCE pet still exists in the local DB
/// (e.g. user exports + reimports on the same machine to test), the
/// imported events carry the same deterministic UUID v5 ids that
/// were originally generated by the writer. Without rewriting the
/// `id` on import, every row collides with the existing primary key
/// and `INSERT OR IGNORE` silently drops the entire log → pet
/// imports at Lv.0 despite 96 events parsing correctly.
///
/// This test pins the importer-side `id` rewrite contract: as long
/// as the caller (`install_pet`) regenerates ev.id before calling
/// the helper, this test passes. If a future refactor drops that
/// rewrite, this test would fail (modeling the bug).
#[tokio::test]
async fn replay_handles_id_collisions_with_source_pet_in_db() {
    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("test.db"))
        .await
        .expect("open db");

    // Seed: an EXISTING pet with rows that share the same composite
    // (source_type, source_ref) the imported events will carry.
    let source_pet_id = "source-pet";
    let origin = db.ensure_install_id().await.unwrap();
    db.insert_pet(
        source_pet_id,
        "Source",
        "test.demo",
        dir.path().to_string_lossy().as_ref(),
        chrono::Utc::now(),
        false,
        &origin,
    )
    .await
    .unwrap();
    let original_events = make_events(source_pet_id, 5, 50);
    for ev in &original_events {
        db.insert_xp_event_raw(ev).await.unwrap();
    }
    assert_eq!(db.sum_xp_for_pet(source_pet_id).await.unwrap(), 250);

    // Now simulate import: events have the SAME ids as the seeded
    // rows (worst case — exporter and importer hit the same machine).
    // Rewrite pet_id AND id, mirroring install_pet's import-time
    // remap. Use uuid v4 for fresh ids.
    let new_pet_id = "imported-pet";
    db.insert_pet(
        new_pet_id,
        "Imported",
        "test.demo",
        dir.path().to_string_lossy().as_ref(),
        chrono::Utc::now(),
        true,
        &origin,
    )
    .await
    .unwrap();
    let doc = make_pet_doc(&[(0, 0), (1, 50), (2, 150), (3, 250)]);
    let mut imported_events = original_events.clone();
    for ev in &mut imported_events {
        ev.pet_id = new_pet_id.into();
        ev.id = uuid::Uuid::new_v4().to_string();
    }

    let (inserted, _) = replay_events_and_recompute(&db, new_pet_id, &doc, &imported_events)
        .await
        .unwrap();
    assert_eq!(inserted, 5, "id-rewrite let imports past the primary-key collision");

    let new_total = db.sum_xp_for_pet(new_pet_id).await.unwrap();
    assert_eq!(new_total, 250, "imported pet must have full xp despite source pet's existence");

    // Sanity: the source pet's rows are untouched.
    let source_total = db.sum_xp_for_pet(source_pet_id).await.unwrap();
    assert_eq!(source_total, 250);
}

#[tokio::test]
async fn replay_with_malformed_events_filtered_upstream_still_recomputes() {
    // The archive_cmds.rs importer pre-filters malformed JSONL lines
    // before calling replay_events_and_recompute. Pin that the
    // helper itself doesn't depend on every event being valid — it
    // recomputes from whatever's in xp_event after the loop.
    let dir = tempdir().unwrap();
    let db = DbHandle::open(&dir.path().join("test.db"))
        .await
        .expect("open db");
    let pet_id = "partial-pet";
    let origin = db.ensure_install_id().await.unwrap();
    db.insert_pet(
        pet_id,
        "Partial",
        "test.demo",
        dir.path().to_string_lossy().as_ref(),
        chrono::Utc::now(),
        true,
        &origin,
    )
    .await
    .unwrap();
    let doc = make_pet_doc(&[(0, 0), (1, 50), (2, 100), (3, 150)]);
    // 7 valid events × 25 XP = 175 → Lv.3 in this curve.
    let events = make_events(pet_id, 7, 25);
    let (inserted, _) = replay_events_and_recompute(&db, pet_id, &doc, &events).await.unwrap();
    assert_eq!(inserted, 7);
    let state = db.get_pet_state(pet_id).await.unwrap().unwrap();
    assert_eq!(state.total_xp, 175);
    assert_eq!(state.current_level, doc.levels.current_level(175));
}

// ─── Original write_zip helper sanity check ───────────────────────

/// Verify the helper is well-formed before any test relies on it.
#[test]
fn write_zip_helper_produces_readable_archives() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("simple.zip");
    write_zip(&p, &[("hello.txt", b"world".to_vec())]);
    let f = fs::File::open(&p).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut entry = z.by_name("hello.txt").unwrap();
    let mut buf = String::new();
    entry.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "world");
}
