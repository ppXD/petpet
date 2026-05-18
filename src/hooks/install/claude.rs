//! Claude Code hook installer.
//!
//! Patches `~/.claude/settings.json`:
//!
//! ```json
//! {
//!   "hooks": {
//!     "UserPromptSubmit": [
//!       { "matcher": "", "hooks": [
//!         { "type": "command",
//!           "command": "curl ... 127.0.0.1:PORT/hooks/claude/UserPromptSubmit # petpet-managed"
//!         }
//!       ]}
//!     ],
//!     ...
//!   }
//! }
//! ```
//!
//! Discovery rules for "is this entry ours":
//! - any `command` string containing the [`PETPET_MARKER`] is petpet's
//! - port can be re-read from `127.0.0.1:NNNNN/hooks/`
//!
//! Safety:
//! - Skip entirely if `~/.claude/` doesn't exist (user has not used Claude)
//! - Read → parse JSON → mutate in memory → atomic write back
//! - Never touch entries that lack our marker
//! - Preserve unknown top-level keys (we operate on `serde_json::Value`)

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::util::{atomic_write_backed_up, is_managed_command};
use super::{
    build_curl_command, HookInstaller, InstallReport, InstallStatus, UninstallReport,
    HOOK_EVENTS,
};
use crate::event::ProviderId;

pub struct ClaudeHookInstaller;

fn settings_path() -> Option<PathBuf> {
    let home = crate::paths::home_dir()?;
    let claude_dir = home.join(".claude");
    if !claude_dir.exists() {
        return None;
    }
    Some(claude_dir.join("settings.json"))
}

impl HookInstaller for ClaudeHookInstaller {
    fn id(&self) -> ProviderId {
        ProviderId::ClaudeCode
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn install(&self, port: u16) -> InstallReport {
        let mut report = InstallReport::new(ProviderId::ClaudeCode);
        report.strategy = Some("settings.json".into());

        let path = match settings_path() {
            Some(p) => p,
            None => {
                report.warnings.push("~/.claude/ not found — skipping (user has not used Claude Code)".into());
                return report;
            }
        };
        report.config_path = Some(path.clone());

        match install_inner(&path, port) {
            Ok(diff) => {
                report.installed = diff.installed;
                report.updated = diff.updated;
                report.already_present = diff.already_present;
                if let Some(b) = diff.backup {
                    report.backups.push(b);
                }
            }
            Err(e) => report.error = Some(format!("{e:#}")),
        }
        report
    }

    fn uninstall(&self) -> UninstallReport {
        let mut report = UninstallReport::new(ProviderId::ClaudeCode);
        let path = match settings_path() {
            Some(p) => p,
            None => return report,
        };
        if !path.exists() {
            return report;
        }
        report.config_path = Some(path.clone());

        match uninstall_inner(&path) {
            Ok(removed) => report.removed = removed,
            Err(e) => report.error = Some(format!("{e:#}")),
        }
        report
    }

    fn status(&self) -> InstallStatus {
        let mut status = InstallStatus::new(ProviderId::ClaudeCode);
        let Some(path) = settings_path() else { return status };
        status.config_path = Some(path.clone());
        status.config_exists = path.exists();
        if !status.config_exists {
            return status;
        }
        if let Ok(s) = status_inner(&path) {
            status.installed_events = s.events;
            status.installed_port = s.port;
        }
        status
    }
}

#[derive(Debug)]
struct InstallDiff {
    installed: Vec<String>,
    updated: Vec<String>,
    already_present: Vec<String>,
    backup: Option<PathBuf>,
}

fn install_inner(path: &std::path::Path, port: u16) -> Result<InstallDiff> {
    let mut root = read_root(path)?;
    let hooks = root
        .as_object_mut()
        .expect("root is object")
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json \"hooks\" is not an object"))?;

    let mut diff = InstallDiff {
        installed: Vec::new(),
        updated: Vec::new(),
        already_present: Vec::new(),
        backup: None,
    };

    for event in HOOK_EVENTS {
        let event_arr = hooks_obj
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("hooks.{event} is not an array"))?;

        let desired_cmd = build_curl_command("claude", event, port);

        let mut updated_in_place = false;
        let mut already_present = false;

        for matcher_block in event_arr.iter_mut() {
            let inner_hooks = matcher_block
                .as_object_mut()
                .and_then(|m| m.get_mut("hooks"))
                .and_then(|h| h.as_array_mut());
            let Some(inner_hooks) = inner_hooks else { continue };

            for hook in inner_hooks.iter_mut() {
                let cmd_field = hook.as_object_mut().and_then(|o| o.get_mut("command"));
                let Some(Value::String(cmd_str)) = cmd_field else { continue };
                if !is_managed_command(cmd_str) {
                    continue;
                }
                if cmd_str == &desired_cmd {
                    already_present = true;
                } else {
                    *cmd_str = desired_cmd.clone();
                    updated_in_place = true;
                }
            }
        }

        if already_present {
            diff.already_present.push((*event).into());
        } else if updated_in_place {
            diff.updated.push((*event).into());
        } else {
            event_arr.push(json!({
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": desired_cmd
                }]
            }));
            diff.installed.push((*event).into());
        }
    }

    diff.backup = write_root(path, &root)?;
    Ok(diff)
}

fn uninstall_inner(path: &std::path::Path) -> Result<Vec<String>> {
    let mut root = read_root(path)?;
    let mut removed = Vec::new();
    let Some(root_obj) = root.as_object_mut() else { return Ok(removed) };
    let Some(hooks) = root_obj.get_mut("hooks") else { return Ok(removed) };
    let Some(hooks_obj) = hooks.as_object_mut() else { return Ok(removed) };

    for (event, value) in hooks_obj.iter_mut() {
        let Some(arr) = value.as_array_mut() else { continue };
        let before = arr.len();
        arr.retain_mut(|matcher_block| {
            let Some(map) = matcher_block.as_object_mut() else { return true };
            if let Some(inner) = map.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                inner.retain(|h| {
                    h.as_object()
                        .and_then(|o| o.get("command"))
                        .and_then(|c| c.as_str())
                        .map(|s| !is_managed_command(s))
                        .unwrap_or(true)
                });
                // Drop the matcher block if it became empty
                if inner.is_empty() {
                    return false;
                }
            }
            true
        });
        if arr.len() != before {
            removed.push(event.clone());
        }
    }

    // Drop now-empty event arrays
    hooks_obj.retain(|_, v| !matches!(v, Value::Array(a) if a.is_empty()));
    // Drop the whole "hooks" object if it became empty
    if hooks_obj.is_empty() {
        root_obj.remove("hooks");
    }

    write_root(path, &root)?;
    Ok(removed)
}

struct StatusInner {
    events: Vec<String>,
    port: Option<u16>,
}

fn status_inner(path: &std::path::Path) -> Result<StatusInner> {
    let root = read_root(path)?;
    let mut events = Vec::new();
    let mut port: Option<u16> = None;
    let hooks = root.get("hooks").and_then(|v| v.as_object());
    if let Some(hooks) = hooks {
        for (event, value) in hooks {
            let Some(arr) = value.as_array() else { continue };
            for matcher_block in arr {
                let inner = matcher_block
                    .get("hooks")
                    .and_then(|h| h.as_array());
                let Some(inner) = inner else { continue };
                for h in inner {
                    if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                        if is_managed_command(cmd) {
                            if !events.contains(event) {
                                events.push(event.clone());
                            }
                            if port.is_none() {
                                port = super::util::extract_port(cmd);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(StatusInner { events, port })
}

fn read_root(path: &std::path::Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let v: Value = serde_json::from_str(&raw)?;
    if !v.is_object() {
        anyhow::bail!("settings.json root is not an object");
    }
    Ok(v)
}

fn write_root(path: &std::path::Path, root: &Value) -> Result<Option<PathBuf>> {
    let pretty = serde_json::to_string_pretty(root)?;
    atomic_write_backed_up(path, &pretty)
}

/// Test override: replace `~/.claude` with an arbitrary dir.
#[cfg(test)]
fn settings_path_for_test(home: &std::path::Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

#[cfg(test)]
fn run_install(home: &std::path::Path, port: u16) -> Result<InstallDiff> {
    let path = settings_path_for_test(home);
    fs::create_dir_all(path.parent().unwrap())?;
    install_inner(&path, port)
}

#[cfg(test)]
fn run_uninstall(home: &std::path::Path) -> Result<Vec<String>> {
    let path = settings_path_for_test(home);
    uninstall_inner(&path)
}

#[cfg(test)]
fn run_status(home: &std::path::Path) -> Result<StatusInner> {
    status_inner(&settings_path_for_test(home))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn read(path: &std::path::Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn creates_settings_when_missing() {
        let dir = tempdir().unwrap();
        let diff = run_install(dir.path(), 43117).unwrap();
        assert_eq!(diff.installed.len(), HOOK_EVENTS.len());
        assert!(diff.updated.is_empty());
        assert!(diff.already_present.is_empty());

        let v = read(&settings_path_for_test(dir.path()));
        assert!(v.get("hooks").unwrap().get("UserPromptSubmit").is_some());
    }

    #[test]
    fn second_run_is_idempotent() {
        let dir = tempdir().unwrap();
        run_install(dir.path(), 43117).unwrap();
        let diff = run_install(dir.path(), 43117).unwrap();
        assert!(diff.installed.is_empty());
        assert!(diff.updated.is_empty());
        assert_eq!(diff.already_present.len(), HOOK_EVENTS.len());
    }

    #[test]
    fn updates_port_in_place() {
        let dir = tempdir().unwrap();
        run_install(dir.path(), 43117).unwrap();
        let diff = run_install(dir.path(), 9999).unwrap();
        assert_eq!(diff.updated.len(), HOOK_EVENTS.len());

        let v = read(&settings_path_for_test(dir.path()));
        let cmd = v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains(":9999/"));
    }

    #[test]
    fn preserves_user_hooks_and_other_settings() {
        let dir = tempdir().unwrap();
        let path = settings_path_for_test(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
              "theme": "dark",
              "hooks": {
                "UserPromptSubmit": [
                  {"matcher": "", "hooks": [{"type":"command","command":"echo user-hook"}]}
                ]
              }
            }"#,
        )
        .unwrap();

        run_install(dir.path(), 43117).unwrap();
        let v = read(&path);
        assert_eq!(v["theme"], "dark");

        // UserPromptSubmit array now has 2 matcher blocks: user's + ours.
        let arr = v["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["hooks"][0]["command"], "echo user-hook");
        assert!(arr[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("petpet-managed"));
    }

    #[test]
    fn uninstall_removes_only_managed_entries() {
        let dir = tempdir().unwrap();
        let path = settings_path_for_test(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
              "theme": "light",
              "hooks": {
                "UserPromptSubmit": [
                  {"matcher": "", "hooks": [{"type":"command","command":"echo user-hook"}]}
                ]
              }
            }"#,
        )
        .unwrap();

        run_install(dir.path(), 43117).unwrap();
        let removed = run_uninstall(dir.path()).unwrap();
        assert!(removed.contains(&"UserPromptSubmit".to_string()));

        let v = read(&path);
        assert_eq!(v["theme"], "light");
        let arr = v["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["hooks"][0]["command"], "echo user-hook");
    }

    #[test]
    fn uninstall_drops_empty_hooks_object() {
        let dir = tempdir().unwrap();
        run_install(dir.path(), 43117).unwrap();
        run_uninstall(dir.path()).unwrap();
        let v = read(&settings_path_for_test(dir.path()));
        assert!(v.get("hooks").is_none(), "empty hooks {{}} should be removed");
    }

    #[test]
    fn status_after_install_reports_correctly() {
        let dir = tempdir().unwrap();
        run_install(dir.path(), 43117).unwrap();
        let s = run_status(dir.path()).unwrap();
        assert_eq!(s.port, Some(43117));
        let mut events = s.events;
        events.sort();
        let mut expected: Vec<String> = HOOK_EVENTS.iter().map(|e| (*e).into()).collect();
        expected.sort();
        assert_eq!(events, expected);
    }

    #[test]
    fn malformed_existing_settings_returns_error() {
        let dir = tempdir().unwrap();
        let path = settings_path_for_test(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not valid json {").unwrap();
        let err = run_install(dir.path(), 43117).unwrap_err();
        assert!(format!("{err:#}").contains("expected"));
    }
}
