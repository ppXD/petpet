//! Atomic file write + backup + petpet-managed substring detection helpers.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use super::PETPET_MARKER;

/// Write `content` to `path` atomically (write to sibling .tmp, then rename).
/// Creates parent directories if missing. **Does not** create a backup —
/// use [`atomic_write_backed_up`] when touching user configs.
pub fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("no parent dir for {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut tmp = parent.to_path_buf();
    let file_name = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("petpet_write");
    tmp.push(format!(".{file_name}.petpet.tmp"));
    fs::write(&tmp, content).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Atomic write that ALSO drops a timestamped `.bak` next to the original
/// when the file already exists AND the new content differs. Skips the
/// backup if content is unchanged so repeated idempotent runs don't
/// accumulate dozens of identical bak files.
///
/// Returns the path of the backup file we wrote, or `None` if no backup
/// was needed (file missing, or content unchanged).
///
/// Backup filename format: `<original>.<UTC-ISO8601>.bak`, e.g.
/// `settings.json.2026-05-15T10-30-45Z.bak`.
pub fn atomic_write_backed_up(path: &Path, content: &str) -> Result<Option<PathBuf>> {
    let backup = if path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing == content {
            None
        } else {
            let ts = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
            let file_name = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("petpet_backup");
            let mut bak = path.to_path_buf();
            bak.set_file_name(format!("{file_name}.{ts}.bak"));
            fs::copy(path, &bak)
                .with_context(|| format!("backup {} → {}", path.display(), bak.display()))?;
            Some(bak)
        }
    } else {
        None
    };
    atomic_write(path, content)?;
    Ok(backup)
}

/// True if a command string was placed by petpet (looks for our fingerprint).
pub fn is_managed_command(cmd: &str) -> bool {
    cmd.contains(PETPET_MARKER)
}

/// Cross-module mutex for tests that mutate the process-global `$HOME` env
/// var. Without one shared lock, parallel test runs in different installer
/// modules trample each other. Used by `claude::tests`, `codex::tests`,
/// `gemini::tests` via `pub(crate)`.
#[cfg(test)]
pub(crate) fn home_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Best-effort port extraction from one of our managed commands.
/// Looks for `127.0.0.1:NNNNN/hooks/`.
pub fn extract_port(cmd: &str) -> Option<u16> {
    let idx = cmd.find("127.0.0.1:")?;
    let after = &cmd[idx + "127.0.0.1:".len()..];
    let end = after.find('/')?;
    after[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_creates_parents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        atomic_write(&path, "hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.txt");
        atomic_write(&path, "first").unwrap();
        atomic_write(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn backed_up_write_creates_bak_when_content_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        atomic_write(&path, "{\"first\":true}").unwrap();
        let bak = atomic_write_backed_up(&path, "{\"second\":true}").unwrap();
        assert!(bak.is_some(), "expected a backup when content changes");
        let bak_path = bak.unwrap();
        assert!(bak_path.exists());
        let bak_content = fs::read_to_string(&bak_path).unwrap();
        assert_eq!(bak_content, "{\"first\":true}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"second\":true}");
        let name = bak_path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("settings.json."));
        assert!(name.ends_with(".bak"));
    }

    #[test]
    fn backed_up_write_skips_bak_when_content_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        atomic_write(&path, "same").unwrap();
        let bak = atomic_write_backed_up(&path, "same").unwrap();
        assert!(bak.is_none(), "no backup when content matches");
    }

    #[test]
    fn backed_up_write_skips_bak_when_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.json");
        let bak = atomic_write_backed_up(&path, "fresh").unwrap();
        assert!(bak.is_none(), "no backup for brand new file");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh");
    }

    #[test]
    fn extract_port_finds_default() {
        let cmd = "curl ... http://127.0.0.1:43117/hooks/claude/Stop # petpet-managed";
        assert_eq!(extract_port(cmd), Some(43117));
    }

    #[test]
    fn extract_port_finds_custom() {
        let cmd = "curl http://127.0.0.1:9999/hooks/codex/x # petpet-managed";
        assert_eq!(extract_port(cmd), Some(9999));
    }

    #[test]
    fn extract_port_returns_none_for_unmanaged() {
        let cmd = "echo hi";
        assert_eq!(extract_port(cmd), None);
    }

    #[test]
    fn is_managed_command_detects_marker() {
        assert!(is_managed_command("anything # petpet-managed trailing"));
        assert!(!is_managed_command("anything else"));
    }

    /// Detection must also recognise the new URL-query form used by
    /// `build_curl_command` (post cross-platform refactor), because
    /// Windows `cmd.exe` doesn't treat `#` as a comment so we moved
    /// the fingerprint into the URL.
    #[test]
    fn is_managed_command_detects_url_query_marker() {
        let cmd = r#"curl -fsS -X POST "http://127.0.0.1:43117/hooks/claude/Stop?_=petpet-managed""#;
        assert!(is_managed_command(cmd));
    }

    #[test]
    fn extract_port_handles_url_query_marker() {
        let cmd = r#"curl -fsS -X POST "http://127.0.0.1:43117/hooks/claude/Stop?_=petpet-managed""#;
        assert_eq!(extract_port(cmd), Some(43117));
    }
}
