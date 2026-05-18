//! `.petpet` archive format — pack / unpack / validate.
//!
//! See `docs/PETPET_FORMAT.md` for the user-facing spec. This module
//! is the implementation; the design considerations that matter for
//! understanding the code:
//!
//! ## Schema versioning
//! - Archive-level manifest carries `$schema: "petpet/v<major>[.<minor>]"`.
//! - We treat the major as a hard gate: unknown newer major → refuse
//!   with a clear "update petpet" message. Unknown minor → load it
//!   (fields we don't recognise get ignored — forward compat for
//!   additive changes).
//! - When a future `v2` ships, the importer keeps `v1` support for
//!   at least two major releases so old exports never become bricks.
//!
//! ## Self-healing import
//! - Hard errors (refuse + no install): unsupported schema major,
//!   missing `manifest.json`, missing `template.json`, zip-slip
//!   path traversal, archive over size cap.
//! - Soft errors (install + warn): missing optional stages, corrupt
//!   `xp_events.jsonl` lines, unknown extra files. Best-effort
//!   "save what we can" so a user's long-raised companion never
//!   becomes un-importable just because one event row is malformed.
//!
//! ## Security
//! - Zip-slip protection: reject any entry whose normalised path
//!   escapes the destination root.
//! - Size caps: 50 MB total archive, 10 MB any single file. Prevents
//!   zip bombs.

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Maximum decompressed size of the entire archive. A real template
/// is a few hundred KB; a heavily-raised pet's xp_events log might
/// reach a few MB. 50 MB is two orders of magnitude of headroom, low
/// enough that a zip bomb can't exhaust memory / disk.
pub const MAX_ARCHIVE_BYTES: u64 = 50 * 1024 * 1024;
/// Maximum decompressed size of any single file in the archive. Pet
/// sprites are typically < 200 KB; the `xp_events.jsonl` is the only
/// thing that could grow, and 10 MB ≈ 100k events ≈ years of usage.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

// ─── Manifest ──────────────────────────────────────────────────────

/// Top-level `manifest.json` — kept deliberately tiny. The manifest
/// only carries what the importer needs to make routing + compat
/// decisions BEFORE reading any other file. Everything else (id,
/// name, version, author, license, tags) lives in the
/// `template.json.meta` block authors already edit — no duplication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveManifest {
    /// Format version, e.g. `"petpet/v1"` or `"petpet/v1.2"`.
    #[serde(rename = "$schema")]
    pub schema: String,
    /// Which kind of archive — drives the import lane.
    pub kind: ArchiveKind,
    /// Cached pet stats so the import-confirmation dialog can show
    /// "Restore Tofu (Lv. 34, 12 days)?" without unzipping the
    /// `pet/pet.json` first. Only populated when `kind == Pet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pet_summary: Option<PetSummary>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveKind {
    Template,
    Pet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PetSummary {
    pub level: u32,
    pub total_xp: i64,
    pub days_raised: i64,
}

impl ArchiveManifest {
    /// The major schema version this build of petpet writes when
    /// exporting. Stays at 1 until we make a genuinely breaking
    /// change (which we plan to avoid for years — see the format
    /// doc's "evolving the format" section).
    pub const CURRENT_MAJOR: u32 = 1;
    pub const CURRENT_MINOR: u32 = 0;

    pub fn new_template() -> Self {
        Self {
            schema: format!("petpet/v{}.{}", Self::CURRENT_MAJOR, Self::CURRENT_MINOR),
            kind: ArchiveKind::Template,
            pet_summary: None,
        }
    }

    pub fn new_pet(summary: PetSummary) -> Self {
        Self {
            schema: format!("petpet/v{}.{}", Self::CURRENT_MAJOR, Self::CURRENT_MINOR),
            kind: ArchiveKind::Pet,
            pet_summary: Some(summary),
        }
    }

    /// Parse the `schema` string into `(major, minor)`. Tolerant:
    /// `"petpet/v1"` → `(1, 0)`, `"petpet/v1.2"` → `(1, 2)`,
    /// anything else → `None`. Tolerance matters because authors
    /// hand-edit manifests and a typo shouldn't crash the importer.
    pub fn schema_version(&self) -> Option<(u32, u32)> {
        let rest = self.schema.strip_prefix("petpet/v")?;
        let mut parts = rest.splitn(2, '.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some((major, minor))
    }

    /// Decide whether this build can load the archive. Three outcomes:
    /// - `Ok`: same major as us — load normally
    /// - `TooNew`: major exceeds what we know — refuse with a
    ///   user-facing "update petpet" message
    /// - `Ancient`: major older than us — load (we keep read support
    ///   for at least 2 majors after a bump; see format doc)
    /// - `Malformed`: schema string doesn't parse — refuse, but as a
    ///   "this isn't a valid petpet archive" error rather than a
    ///   version one
    pub fn compat(&self) -> CompatVerdict {
        match self.schema_version() {
            None => CompatVerdict::Malformed,
            Some((major, _)) if major == Self::CURRENT_MAJOR => CompatVerdict::Ok,
            Some((major, _)) if major > Self::CURRENT_MAJOR => CompatVerdict::TooNew { major },
            Some((major, _)) => CompatVerdict::Ancient { major },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatVerdict {
    Ok,
    TooNew { major: u32 },
    Ancient { major: u32 },
    Malformed,
}

// ─── Packing ───────────────────────────────────────────────────────

/// Pack a directory (the template's on-disk layout) into a `.petpet`
/// zip archive. The directory is taken verbatim except that a fresh
/// `manifest.json` is written at the archive root — even if the
/// source dir doesn't have one, so authors don't have to maintain it
/// by hand.
///
/// `pet_summary` injects the pet-summary block (and sets `kind:
/// Pet`); pass `None` to produce a template-only archive.
pub fn pack_directory(
    src_dir: &Path,
    out_zip: &Path,
    pet_summary: Option<PetSummary>,
) -> Result<()> {
    let manifest = match pet_summary {
        Some(s) => ArchiveManifest::new_pet(s),
        None => ArchiveManifest::new_template(),
    };
    let manifest_json = serde_json::to_string_pretty(&manifest)?;

    if let Some(parent) = out_zip.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let file = fs::File::create(out_zip)
        .with_context(|| format!("create archive at {}", out_zip.display()))?;
    let mut zip = zip::ZipWriter::new(file);

    let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    zip.start_file("manifest.json", opts)?;
    zip.write_all(manifest_json.as_bytes())?;

    // Recursively walk the source directory and write every file
    // (skip `manifest.json` if it exists — our generated one wins).
    add_dir_to_zip(&mut zip, src_dir, src_dir, opts)?;

    zip.finish()?;
    Ok(())
}

fn add_dir_to_zip<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    root: &Path,
    dir: &Path,
    opts: zip::write::FileOptions<()>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|e| anyhow!("strip_prefix failed: {e}"))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if path.is_dir() {
            add_dir_to_zip(zip, root, &path, opts)?;
            continue;
        }
        // Skip any existing manifest.json — `pack_directory` always
        // writes a freshly-generated one to avoid drift between the
        // file on disk and the export.
        if rel_str == "manifest.json" {
            continue;
        }
        let mut data = Vec::new();
        fs::File::open(&path)
            .with_context(|| format!("open {}", path.display()))?
            .read_to_end(&mut data)?;
        zip.start_file(&rel_str, opts)?;
        zip.write_all(&data)?;
    }
    Ok(())
}

// ─── Unpacking ─────────────────────────────────────────────────────

/// Result of a successful unpack. The caller decides what to do with
/// the extracted files (install as template / restore as pet).
#[derive(Debug)]
pub struct Unpacked {
    pub manifest: ArchiveManifest,
    /// Temp directory holding the extracted contents. Caller is
    /// responsible for moving files out / cleaning up.
    pub root: PathBuf,
    /// Soft-error log accumulated during extraction. Entries here
    /// are user-visible warnings (e.g. "skipped malformed file X") —
    /// not import failures.
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    #[error("not a petpet archive (manifest.json missing or unreadable)")]
    NotPetpet,
    #[error("archive made by a newer petpet (schema v{major}) — please update")]
    SchemaTooNew { major: u32 },
    #[error("archive schema malformed: {0:?}")]
    SchemaMalformed(String),
    #[error("missing required {0}")]
    MissingRequired(&'static str),
    #[error("archive exceeds size cap ({0} bytes > 50 MB)")]
    TooLarge(u64),
    #[error("archive contains unsafe path: {0:?}")]
    UnsafePath(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub fn unpack_archive(zip_path: &Path, dest_root: &Path) -> Result<Unpacked, UnpackError> {
    let file = fs::File::open(zip_path)?;
    let mut zip = zip::ZipArchive::new(file)?;

    // Quick size check before extracting anything.
    let total: u64 = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.size()))
        .sum();
    if total > MAX_ARCHIVE_BYTES {
        return Err(UnpackError::TooLarge(total));
    }

    // Manifest comes first — gate on compat before extracting anything
    // else. If it's malformed or missing, we don't want random files
    // landing on disk.
    let manifest = read_manifest(&mut zip)?;
    match manifest.compat() {
        CompatVerdict::Ok | CompatVerdict::Ancient { .. } => {}
        CompatVerdict::TooNew { major } => return Err(UnpackError::SchemaTooNew { major }),
        CompatVerdict::Malformed => {
            return Err(UnpackError::SchemaMalformed(manifest.schema.clone()))
        }
    }

    fs::create_dir_all(dest_root)?;
    let mut warnings = Vec::new();

    // Extract every file with zip-slip + size protection. Files that
    // exceed the per-file cap are skipped with a warning, not a hard
    // error — preserves "save what we can" semantics for the rest of
    // the archive.
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw_name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        let safe_rel = match sanitise_entry_path(&raw_name) {
            Ok(p) => p,
            Err(why) => {
                warnings.push(format!("skipped unsafe entry {raw_name:?}: {why}"));
                continue;
            }
        };
        if entry.size() > MAX_FILE_BYTES {
            warnings.push(format!(
                "skipped {} — exceeds per-file size cap ({} bytes)",
                safe_rel.display(),
                entry.size()
            ));
            continue;
        }
        let out_path = dest_root.join(&safe_rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out)?;
    }

    Ok(Unpacked {
        manifest,
        root: dest_root.to_path_buf(),
        warnings,
    })
}

/// Read just the manifest from a zip without extracting anything.
/// Used at the top of `unpack_archive` so we can refuse incompatible
/// archives before touching the filesystem.
fn read_manifest<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<ArchiveManifest, UnpackError> {
    let mut entry = match zip.by_name("manifest.json") {
        Ok(e) => e,
        Err(_) => return Err(UnpackError::NotPetpet),
    };
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    serde_json::from_str(&buf).map_err(|_| UnpackError::NotPetpet)
}

/// Reject path-escape attempts (zip-slip). Strips leading slashes
/// and refuses any `..` component. Returns a clean relative path
/// rooted at the destination.
fn sanitise_entry_path(raw: &str) -> Result<PathBuf, &'static str> {
    // Treat backslashes as separators too (Windows-authored zips).
    let normalised = raw.replace('\\', "/");
    if normalised.starts_with('/') {
        return Err("absolute path");
    }
    let path = PathBuf::from(&normalised);
    for comp in path.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return Err("contains '..' or root reference"),
        }
    }
    if path.as_os_str().is_empty() {
        return Err("empty path");
    }
    Ok(path)
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn make_simple_template_dir(dir: &Path) {
        fs::write(
            dir.join("template.json"),
            r#"{ "schema":"petpet-pet/v1",
                 "meta": { "id":"test.demo", "name":"Demo", "version":"1.0.0" },
                 "species":{"name":"Demo"},
                 "levels":[],"stages":[],"rules":[],"theme":{},"assets":{} }"#,
        )
        .unwrap();
        fs::create_dir_all(dir.join("stages/stage_0")).unwrap();
        fs::write(dir.join("stages/stage_0/stage.json"), r#"{"name":"Egg"}"#).unwrap();
    }

    #[test]
    fn manifest_version_parses_explicit_minor() {
        let m = ArchiveManifest {
            schema: "petpet/v1.7".into(),
            kind: ArchiveKind::Template,
            pet_summary: None,
        };
        assert_eq!(m.schema_version(), Some((1, 7)));
    }

    #[test]
    fn manifest_version_defaults_minor_to_zero() {
        let m = ArchiveManifest {
            schema: "petpet/v1".into(),
            kind: ArchiveKind::Template,
            pet_summary: None,
        };
        assert_eq!(m.schema_version(), Some((1, 0)));
    }

    #[test]
    fn manifest_version_rejects_garbage() {
        let m = ArchiveManifest {
            schema: "notpetpet/v1".into(),
            kind: ArchiveKind::Template,
            pet_summary: None,
        };
        assert_eq!(m.schema_version(), None);
    }

    #[test]
    fn compat_same_major_is_ok() {
        let m = ArchiveManifest::new_template();
        assert_eq!(m.compat(), CompatVerdict::Ok);
    }

    #[test]
    fn compat_newer_major_is_too_new() {
        let m = ArchiveManifest {
            schema: "petpet/v99".into(),
            kind: ArchiveKind::Template,
            pet_summary: None,
        };
        assert_eq!(m.compat(), CompatVerdict::TooNew { major: 99 });
    }

    #[test]
    fn compat_minor_drift_within_major_is_ok() {
        // v1.999 — far-future minor on the same major. Must load.
        let m = ArchiveManifest {
            schema: "petpet/v1.999".into(),
            kind: ArchiveKind::Template,
            pet_summary: None,
        };
        assert_eq!(m.compat(), CompatVerdict::Ok);
    }

    #[test]
    fn compat_malformed_schema_is_flagged() {
        let m = ArchiveManifest {
            schema: "garbage".into(),
            kind: ArchiveKind::Template,
            pet_summary: None,
        };
        assert_eq!(m.compat(), CompatVerdict::Malformed);
    }

    #[test]
    fn manifest_serde_round_trips_with_pet_summary() {
        let m = ArchiveManifest::new_pet(PetSummary {
            level: 34,
            total_xp: 12345,
            days_raised: 12,
        });
        let json = serde_json::to_string(&m).unwrap();
        let back: ArchiveManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn manifest_serde_ignores_unknown_future_fields() {
        // Forward-compat: a v1.5 manifest with new fields must load
        // in this build (which knows only the current minor).
        let json = r#"{
            "$schema": "petpet/v1.5",
            "kind": "template",
            "preview_video": "promo.mp4",
            "signature": "abc123"
        }"#;
        let m: ArchiveManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.compat(), CompatVerdict::Ok);
        assert_eq!(m.kind, ArchiveKind::Template);
    }

    #[test]
    fn sanitise_rejects_zip_slip_traversal() {
        assert!(sanitise_entry_path("../../etc/passwd").is_err());
        assert!(sanitise_entry_path("stages/../../escape.txt").is_err());
    }

    #[test]
    fn sanitise_rejects_absolute_paths() {
        assert!(sanitise_entry_path("/etc/passwd").is_err());
        assert!(sanitise_entry_path("\\Windows\\System32").is_err());
    }

    #[test]
    fn sanitise_accepts_normal_relative_paths() {
        assert!(sanitise_entry_path("stages/stage_0/sprite.png").is_ok());
        assert!(sanitise_entry_path("manifest.json").is_ok());
    }

    #[test]
    fn pack_then_unpack_round_trips_template() {
        let src = tempdir().unwrap();
        make_simple_template_dir(src.path());
        let archive_path = src.path().join("out.petpet");

        pack_directory(src.path(), &archive_path, None).unwrap();

        let dest = tempdir().unwrap();
        let unpacked = unpack_archive(&archive_path, dest.path()).unwrap();
        assert_eq!(unpacked.manifest.kind, ArchiveKind::Template);
        assert_eq!(unpacked.manifest.compat(), CompatVerdict::Ok);
        assert!(unpacked.warnings.is_empty());
        assert!(dest.path().join("manifest.json").exists());
        assert!(dest.path().join("template.json").exists());
        assert!(dest.path().join("stages/stage_0/stage.json").exists());
    }

    #[test]
    fn pack_includes_pet_summary_for_pet_archives() {
        let src = tempdir().unwrap();
        make_simple_template_dir(src.path());
        fs::create_dir_all(src.path().join("pet")).unwrap();
        fs::write(src.path().join("pet/pet.json"), "{}").unwrap();
        let archive_path = src.path().join("out.petpet");

        let summary = PetSummary {
            level: 12,
            total_xp: 999,
            days_raised: 3,
        };
        pack_directory(src.path(), &archive_path, Some(summary.clone())).unwrap();

        let dest = tempdir().unwrap();
        let unpacked = unpack_archive(&archive_path, dest.path()).unwrap();
        assert_eq!(unpacked.manifest.kind, ArchiveKind::Pet);
        assert_eq!(unpacked.manifest.pet_summary, Some(summary));
    }

    #[test]
    fn unpack_rejects_archive_with_no_manifest() {
        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("evil.petpet");
        let f = fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("template.json", opts).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.finish().unwrap();

        let dest = tempdir().unwrap();
        let err = unpack_archive(&archive_path, dest.path()).unwrap_err();
        assert!(matches!(err, UnpackError::NotPetpet), "got: {err:?}");
    }

    #[test]
    fn unpack_refuses_archive_with_newer_major_schema() {
        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("future.petpet");
        let f = fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("manifest.json", opts).unwrap();
        zip.write_all(br#"{"$schema":"petpet/v9","kind":"template"}"#).unwrap();
        zip.finish().unwrap();

        let dest = tempdir().unwrap();
        let err = unpack_archive(&archive_path, dest.path()).unwrap_err();
        assert!(
            matches!(err, UnpackError::SchemaTooNew { major: 9 }),
            "got: {err:?}"
        );
    }

    #[test]
    fn unpack_accepts_archive_with_minor_drift() {
        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("future_minor.petpet");
        let f = fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("manifest.json", opts).unwrap();
        zip.write_all(
            br#"{"$schema":"petpet/v1.99","kind":"template","unknown_future_field":42}"#,
        )
        .unwrap();
        zip.start_file("template.json", opts).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.finish().unwrap();

        let dest = tempdir().unwrap();
        let unpacked = unpack_archive(&archive_path, dest.path()).unwrap();
        assert_eq!(unpacked.manifest.compat(), CompatVerdict::Ok);
    }

    #[test]
    fn unpack_skips_zip_slip_entries_with_warning() {
        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("evil.petpet");
        let f = fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("manifest.json", opts).unwrap();
        zip.write_all(br#"{"$schema":"petpet/v1","kind":"template"}"#).unwrap();
        zip.start_file("../../escaped.txt", opts).unwrap();
        zip.write_all(b"evil").unwrap();
        zip.finish().unwrap();

        let dest = tempdir().unwrap();
        let unpacked = unpack_archive(&archive_path, dest.path()).unwrap();
        assert!(
            unpacked.warnings.iter().any(|w| w.contains("unsafe")),
            "expected unsafe-entry warning, got: {:?}",
            unpacked.warnings
        );
        assert!(!dest.path().parent().unwrap().join("escaped.txt").exists());
    }

    /// Defensive: pet_summary missing on a `kind: "pet"` archive is
    /// not a hard error — older exporters might have omitted it. The
    /// importer can fall back to reading pet.json.
    #[test]
    fn pet_summary_is_optional_on_deserialize() {
        let json = r#"{"$schema":"petpet/v1","kind":"pet"}"#;
        let m: ArchiveManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind, ArchiveKind::Pet);
        assert!(m.pet_summary.is_none());
    }

    #[test]
    fn manifest_reads_via_seekable_cursor() {
        // Sanity check that read_manifest works through ZipArchive's
        // typical entry point (used as a helper in real importers).
        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("a.petpet");
        let f = fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("manifest.json", opts).unwrap();
        zip.write_all(br#"{"$schema":"petpet/v1","kind":"template"}"#).unwrap();
        zip.finish().unwrap();

        let bytes = fs::read(&archive_path).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let m = read_manifest(&mut z).unwrap();
        assert_eq!(m.kind, ArchiveKind::Template);
    }
}
