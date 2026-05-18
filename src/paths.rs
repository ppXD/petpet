//! Filesystem paths petpet reads / writes.
//!
//! Cross-platform strategy: every external-tool path (Claude Code,
//! Codex, OpenCode) is exposed as both a single canonical path AND a
//! `*_candidates()` list. The single path is used at *install* time
//! (we want a deterministic location to write to even if nothing
//! exists yet). The candidate list is used at *read* time — we try
//! each in priority order until one exists, because the canonical
//! location of these tools on Windows is not always what their docs
//! say (especially OpenCode, where docs claim `~/.config` but the
//! actual implementation often uses `%APPDATA%\opencode`).
//!
//! Probing keeps us robust to:
//!   - User-set `XDG_CONFIG_HOME` / `OPENCODE_CONFIG_DIR` overrides
//!   - Different tool versions writing to different historical paths
//!   - macOS/Windows divergence (Codex sessions, OpenCode data dir)
//!   - Users with WSL who run the tool on both Linux and Windows

use std::path::PathBuf;

/// Return the first candidate that exists on disk, or `None` if none do.
/// Used at *read* time — caller decides what to do with `None` (skip
/// the provider, log, etc.).
pub fn first_existing<I>(candidates: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    candidates.into_iter().find(|p| p.exists())
}

/// Return the first existing candidate, or the first candidate as a
/// fallback default if none exist. Used at *install* time — we want
/// a deterministic location even on a fresh system.
pub fn first_existing_or_default<I>(candidates: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    let v: Vec<PathBuf> = candidates.into_iter().collect();
    if let Some(found) = v.iter().find(|p| p.exists()).cloned() {
        return Some(found);
    }
    v.into_iter().next()
}

/// Resolve the user's home directory, preferring an explicit `HOME`
/// env var if set. Falls back to `dirs::home_dir()` for the
/// platform-native lookup.
///
/// **Why this helper exists**: `dirs::home_dir()` on Windows reads
/// `SHGetKnownFolderPath(FOLDERID_Profile)` from the Windows API and
/// ignores `HOME`, which makes it impossible for tests to sandbox
/// the home directory through an env var. Several real-world tools
/// (msys2, WSL, git-for-windows, JetBrains IDEs, etc.) also expect
/// `HOME` to be honoured on Windows when explicitly set. Preferring
/// `HOME` first lets us write OS-agnostic install paths AND lets
/// the test suite swap `HOME` to a tempdir on every platform with
/// no special-casing.
///
/// On Unix, behaviour is unchanged (HOME is what `dirs::home_dir()`
/// reads anyway). On Windows, behaviour is unchanged for users who
/// don't set `HOME` (falls back to the Win32 lookup).
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs::home_dir()
}

// ─── petpet's own state ────────────────────────────────────────────

/// Where we keep our own state (db, settings).
/// `~/.petpet/` on every platform via `home_dir`.
pub fn app_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PETPET_HOME") {
        return PathBuf::from(p);
    }
    let home = home_dir().expect("could not resolve home directory");
    home.join(".petpet")
}

pub fn db_path() -> PathBuf {
    app_dir().join("petpet.db")
}

/// Built-in templates released from the binary on first launch.
/// Frontend resolves sprite-sheet URLs from here for built-in species.
pub fn builtin_templates_dir() -> PathBuf {
    app_dir().join("builtin_templates")
}

/// System preset library — level curves and stage arcs that ship
/// embedded in the binary and re-extract on every launch. Read-only
/// from the user's perspective: even if they hand-edit a file here,
/// the next launch overwrites it with the canonical copy. The
/// TemplateCreator UI reads from this directory to populate its
/// "system recommended" dropdowns.
pub fn builtin_presets_dir() -> PathBuf {
    app_dir().join("builtin_presets")
}

/// Scratch space for files the user has staged but not yet committed
/// to a template — currently used by the StagesEditor's per-stage
/// sprite picker. When the user picks a file from anywhere on disk
/// (e.g. `~/Pictures/wukong.png`), we copy it here under a UUID
/// filename so that:
///
///   1. The path lives inside `$HOME/.petpet/**` — the asset-protocol
///      scope — so `convertFileSrc` can render the thumbnail preview
///      without widening the scope to expose the user's whole home
///      directory to the renderer.
///
///   2. The file is captured at pick time. If the user moves or
///      deletes the original between picking and clicking Create,
///      the staged copy still exists and the template still builds.
///
/// On successful template creation the staged file gets copied to
/// its final home (`<template>/stages/stage_N/sprite.png`); the
/// staging copy lingers harmlessly until the next reaper pass (not
/// yet scheduled — staging files are tens of KB each, fine to
/// accumulate in low volume).
pub fn template_staging_sprites_dir() -> PathBuf {
    app_dir().join("template-staging").join("sprites")
}

/// User-installed (community / custom-authored) templates. The
/// egg-picker UI scans this dir alongside `builtin_templates_dir()`.
pub fn user_templates_dir() -> PathBuf {
    app_dir().join("templates")
}

/// Per-pet snapshot folders (`pet.json` + asset copies). Each pet's
/// `snapshot_path` column in the DB points to one of these.
pub fn pets_dir() -> PathBuf {
    app_dir().join("pets")
}

// ─── Claude Code ───────────────────────────────────────────────────
//
// Anthropic documents `~/.claude/` on every platform (macOS:
// `$HOME/.claude/`, Windows: `%USERPROFILE%\.claude\`). Single
// location, no probing required.

pub fn claude_projects_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PETPET_CLAUDE_PROJECTS_ROOT") {
        return Some(PathBuf::from(p));
    }
    Some(home_dir()?.join(".claude").join("projects"))
}

// ─── Codex CLI ─────────────────────────────────────────────────────
//
// OpenAI documents `~/.codex/` for config on every platform. The
// session-rollout JSONL location is NOT documented for Windows, so we
// probe several candidates:
//   1. Env override (`PETPET_CODEX_SESSIONS_ROOT`)
//   2. `~/.codex/sessions/` — Unix canonical; some Windows installs
//      mirror this layout
//   3. `%LOCALAPPDATA%\codex\sessions\` — Windows-native data dir
//   4. `%APPDATA%\codex\sessions\` — Windows roaming alternative

pub fn codex_sessions_root_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("PETPET_CODEX_SESSIONS_ROOT") {
        v.push(PathBuf::from(p));
    }
    if let Some(home) = home_dir() {
        v.push(home.join(".codex").join("sessions"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = dirs::data_local_dir() {
            v.push(local.join("codex").join("sessions"));
        }
        if let Some(roaming) = dirs::data_dir() {
            v.push(roaming.join("codex").join("sessions"));
        }
    }
    v
}

/// Backwards-compat single-path accessor — returns the first existing
/// session root, or the canonical default if none exist (so install /
/// preflight code still works on a fresh system).
pub fn codex_sessions_root() -> Option<PathBuf> {
    first_existing_or_default(codex_sessions_root_candidates())
}

/// Where to install the Codex `config.toml` / `hooks.json` (always
/// `~/.codex/` per docs).
pub fn codex_config_dir() -> Option<PathBuf> {
    Some(home_dir()?.join(".codex"))
}

// ─── OpenCode ──────────────────────────────────────────────────────
//
// OpenCode's location is the trickiest of the three because its docs
// and its actual Windows behaviour disagree:
//   - Docs say `~/.config/opencode/` everywhere (XDG-style).
//   - Multiple users on GitHub report Windows installs writing to
//     `%APPDATA%\opencode\` instead.
//   - `OPENCODE_CONFIG_DIR` env var overrides everything.
//
// So we probe a list. Install picks the first existing OR the
// canonical default; read picks only existing.

pub fn opencode_config_dir_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("OPENCODE_CONFIG_DIR") {
        v.push(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        v.push(PathBuf::from(p).join("opencode"));
    }
    if let Some(home) = home_dir() {
        v.push(home.join(".config").join("opencode"));
    }
    #[cfg(target_os = "windows")]
    {
        // %APPDATA% — what OpenCode's Windows builds actually use
        // (per multiple GitHub-issue reports, despite the docs).
        if let Some(roaming) = dirs::data_dir() {
            v.push(roaming.join("opencode"));
        }
        // %LOCALAPPDATA% — sometimes used by tools that prefer
        // non-roaming state.
        if let Some(local) = dirs::data_local_dir() {
            v.push(local.join("opencode"));
        }
    }
    v
}

/// Where to install our OpenCode JS plugin — the highest-priority
/// candidate that exists, or the canonical default. Falling back to
/// the canonical default ensures fresh systems still get the install.
pub fn opencode_config_dir_for_install() -> Option<PathBuf> {
    first_existing_or_default(opencode_config_dir_candidates())
}

/// OpenCode persists per-message rollups (incl. token tallies) into a
/// SQLite DB. The canonical Unix path is
/// `~/.local/share/opencode/opencode.db`; Windows may store the same
/// DB under `%LOCALAPPDATA%` or `%APPDATA%`. We probe all of them.

pub fn opencode_data_dir_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("OPENCODE_DATA_DIR") {
        v.push(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("XDG_DATA_HOME") {
        v.push(PathBuf::from(p).join("opencode"));
    }
    if let Some(home) = home_dir() {
        v.push(home.join(".local").join("share").join("opencode"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = dirs::data_local_dir() {
            v.push(local.join("opencode"));
        }
        if let Some(roaming) = dirs::data_dir() {
            v.push(roaming.join("opencode"));
        }
    }
    v
}

/// Resolve the OpenCode SQLite DB path. Probes every data-dir
/// candidate, returning the first that actually contains an
/// `opencode.db`.
pub fn opencode_db_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PETPET_OPENCODE_DB_PATH") {
        return Some(PathBuf::from(p));
    }
    let dbs: Vec<PathBuf> = opencode_data_dir_candidates()
        .into_iter()
        .map(|d| d.join("opencode.db"))
        .collect();
    if let Some(found) = first_existing(dbs.clone()) {
        return Some(found);
    }
    // No DB yet — return the canonical default so the provider can
    // watch the parent dir for it to appear.
    dbs.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // The path-probing tests mutate process-global env vars and
    // create / delete tempdirs. Serialise them under a shared mutex
    // so parallel test runs don't trample each other's `OPENCODE_*` /
    // `XDG_*` env state.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Take a snapshot of an env var, optionally set it, and restore on
    /// drop. RAII so a panicking test doesn't poison the next one.
    struct EnvGuard {
        key: String,
        prior: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &str, value: Option<&std::path::Path>) -> Self {
            let prior = std::env::var(key).ok();
            match value {
                Some(p) => std::env::set_var(key, p),
                None => std::env::remove_var(key),
            }
            Self {
                key: key.to_string(),
                prior,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }

    #[test]
    fn first_existing_returns_first_present() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let a = dir.path().join("absent_a");
        let b = dir.path().join("present_b");
        let c = dir.path().join("present_c");
        std::fs::write(&b, "").unwrap();
        std::fs::write(&c, "").unwrap();
        assert_eq!(first_existing([a, b.clone(), c]).as_deref(), Some(b.as_path()));
    }

    #[test]
    fn first_existing_returns_none_when_all_absent() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let a = dir.path().join("absent_a");
        let b = dir.path().join("absent_b");
        assert!(first_existing([a, b]).is_none());
    }

    #[test]
    fn first_existing_or_default_falls_back_to_first_candidate() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let canonical = dir.path().join("canonical_first");
        let other = dir.path().join("never_existed");
        // Neither exists → returns the *first* (canonical) candidate
        // so install code knows where to create the dir.
        assert_eq!(
            first_existing_or_default([canonical.clone(), other]).as_deref(),
            Some(canonical.as_path())
        );
    }

    #[test]
    fn opencode_config_dir_env_override_wins() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom_opencode");
        std::fs::create_dir_all(&custom).unwrap();
        let _override = EnvGuard::set("OPENCODE_CONFIG_DIR", Some(&custom));
        let candidates = opencode_config_dir_candidates();
        // First candidate must be the env-var override.
        assert_eq!(candidates.first(), Some(&custom));
    }

    #[test]
    fn opencode_config_dir_xdg_override_second_priority() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let xdg = dir.path().join("xdg_config_home");
        let _clear_oc = EnvGuard::set("OPENCODE_CONFIG_DIR", None);
        let _override = EnvGuard::set("XDG_CONFIG_HOME", Some(&xdg));
        let candidates = opencode_config_dir_candidates();
        assert_eq!(candidates.first(), Some(&xdg.join("opencode")));
    }

    #[test]
    fn opencode_data_dir_env_override_wins() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom_data");
        let _clear_xdg = EnvGuard::set("XDG_DATA_HOME", None);
        let _override = EnvGuard::set("OPENCODE_DATA_DIR", Some(&custom));
        let candidates = opencode_data_dir_candidates();
        assert_eq!(candidates.first(), Some(&custom));
    }

    #[test]
    fn opencode_db_path_picks_existing_over_default() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let custom_data = dir.path().join("custom_data");
        std::fs::create_dir_all(&custom_data).unwrap();
        let db = custom_data.join("opencode.db");
        std::fs::write(&db, "").unwrap();
        let _clear_xdg = EnvGuard::set("XDG_DATA_HOME", None);
        let _clear_dbpath = EnvGuard::set("PETPET_OPENCODE_DB_PATH", None);
        let _override = EnvGuard::set("OPENCODE_DATA_DIR", Some(&custom_data));
        assert_eq!(opencode_db_path().as_deref(), Some(db.as_path()));
    }

    #[test]
    fn codex_sessions_candidates_includes_home_codex() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _clear_override = EnvGuard::set("PETPET_CODEX_SESSIONS_ROOT", None);
        let candidates = codex_sessions_root_candidates();
        assert!(
            candidates
                .iter()
                .any(|p| p.ends_with(".codex/sessions") || p.ends_with(".codex\\sessions")),
            "expected ~/.codex/sessions in candidates, got: {:?}",
            candidates
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_candidates_include_appdata_and_localappdata() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _clear = EnvGuard::set("OPENCODE_CONFIG_DIR", None);
        let _clear_xdg = EnvGuard::set("XDG_CONFIG_HOME", None);
        let candidates = opencode_config_dir_candidates();
        // On Windows the list must include at least one path under
        // %APPDATA% or %LOCALAPPDATA% so we catch tools that don't
        // respect XDG conventions.
        let has_appdata = candidates.iter().any(|p| {
            p.to_string_lossy().contains("AppData") || p.to_string_lossy().contains("appdata")
        });
        assert!(
            has_appdata,
            "Windows candidates missing AppData entry: {:?}",
            candidates
        );
    }

    // ─── Hardening — invariants every candidate list must satisfy ──

    /// On any platform, every candidate function must return a
    /// non-empty list when no env-var overrides are set, OR explicitly
    /// `None` when the home directory can't be resolved (which doesn't
    /// happen in practice on hosted CI). This guards against a
    /// refactor that silently drops every entry on a particular OS
    /// (e.g. forgetting a `cfg(...)` clause).
    #[test]
    fn opencode_config_candidates_non_empty_on_default_env() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _a = EnvGuard::set("OPENCODE_CONFIG_DIR", None);
        let _b = EnvGuard::set("XDG_CONFIG_HOME", None);
        assert!(
            !opencode_config_dir_candidates().is_empty(),
            "OpenCode config candidates must include at least the home-relative fallback"
        );
    }

    #[test]
    fn opencode_data_candidates_non_empty_on_default_env() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _a = EnvGuard::set("OPENCODE_DATA_DIR", None);
        let _b = EnvGuard::set("XDG_DATA_HOME", None);
        assert!(!opencode_data_dir_candidates().is_empty());
    }

    #[test]
    fn codex_session_candidates_non_empty_on_default_env() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _a = EnvGuard::set("PETPET_CODEX_SESSIONS_ROOT", None);
        assert!(!codex_sessions_root_candidates().is_empty());
    }

    /// `codex_sessions_root()` returns the canonical fallback path
    /// even on a fresh system with no `~/.codex` — install / preflight
    /// needs a concrete location to point at. This guards against a
    /// regression where `first_existing_or_default` silently returns
    /// `None`.
    #[test]
    fn codex_sessions_root_is_some_on_fresh_system() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _a = EnvGuard::set("PETPET_CODEX_SESSIONS_ROOT", None);
        assert!(codex_sessions_root().is_some());
    }

    /// Same invariant for OpenCode's install path resolver — must
    /// always return *somewhere* to install, even if OpenCode has
    /// never run on this machine.
    #[test]
    fn opencode_config_dir_for_install_is_some_on_fresh_system() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _a = EnvGuard::set("OPENCODE_CONFIG_DIR", None);
        let _b = EnvGuard::set("XDG_CONFIG_HOME", None);
        assert!(opencode_config_dir_for_install().is_some());
    }

    /// Multi-platform candidate ordering must not include duplicates.
    /// A duplicate at install time could cause us to write the plugin
    /// twice; at read time it's harmless but wasteful.
    #[test]
    fn opencode_config_candidates_have_no_duplicates() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        // Clear all env vars so the candidate list is purely the
        // platform default — overrides could legitimately duplicate.
        let _a = EnvGuard::set("OPENCODE_CONFIG_DIR", None);
        let _b = EnvGuard::set("XDG_CONFIG_HOME", None);
        let v = opencode_config_dir_candidates();
        let unique: std::collections::BTreeSet<_> = v.iter().collect();
        assert_eq!(
            v.len(),
            unique.len(),
            "duplicate candidate in OpenCode config list: {v:?}"
        );
    }

    // (Not testing `app_dir()` with `PETPET_HOME` here — that env
    // var is also mutated by `xp::engine::tests` under a different
    // mutex, and we'd risk cross-module interference. The xp engine
    // tests cover `app_dir()` indirectly via `pick_template` flows.)
}
