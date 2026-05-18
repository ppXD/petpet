//! Gemini CLI hook installer.
//!
//! Gemini's hook config lives in `~/.gemini/settings.json` with a Claude-
//! shaped JSON `hooks` block but a smaller native event vocabulary:
//! `BeforeTool` / `AfterTool` / `SessionEnd`. Names get normalized in
//! `parsers.rs` to our canonical `ToolUseStart` / `ToolUseEnd` /
//! `SessionEnd` activities.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::util::{atomic_write_backed_up, is_managed_command};
use super::{
    build_curl_command, HookInstaller, InstallReport, InstallStatus, UninstallReport,
    GEMINI_HOOK_EVENTS,
};
use crate::event::ProviderId;

pub struct GeminiHookInstaller;

fn settings_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".gemini");
    if !dir.exists() {
        return None;
    }
    Some(dir.join("settings.json"))
}

impl HookInstaller for GeminiHookInstaller {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }

    fn display_name(&self) -> &'static str {
        "Gemini CLI"
    }

    fn install(&self, port: u16) -> InstallReport {
        let mut report = InstallReport::new(ProviderId::Gemini);
        report.strategy = Some("settings.json".into());

        let path = match settings_path() {
            Some(p) => p,
            None => {
                report
                    .warnings
                    .push("~/.gemini/ not found — skipping (user has not used Gemini CLI)".into());
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
        let mut report = UninstallReport::new(ProviderId::Gemini);
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
        let mut status = InstallStatus::new(ProviderId::Gemini);
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

    for event in GEMINI_HOOK_EVENTS {
        let event_arr = hooks_obj
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("hooks.{event} is not an array"))?;

        let desired_cmd = build_curl_command("gemini", event, port);

        let mut updated_in_place = false;
        let mut already_present = false;

        for matcher_block in event_arr.iter_mut() {
            let Some(inner_hooks) = matcher_block
                .as_object_mut()
                .and_then(|m| m.get_mut("hooks"))
                .and_then(|h| h.as_array_mut())
            else {
                continue;
            };
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
                "hooks": [{ "type": "command", "command": desired_cmd }]
            }));
            diff.installed.push((*event).into());
        }
    }

    diff.backup = atomic_write_backed_up(path, &serde_json::to_string_pretty(&root)?)?;
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

    hooks_obj.retain(|_, v| !matches!(v, Value::Array(a) if a.is_empty()));
    if hooks_obj.is_empty() {
        root_obj.remove("hooks");
    }
    atomic_write_backed_up(path, &serde_json::to_string_pretty(&root)?)?;
    Ok(removed)
}

struct StatusInner {
    events: Vec<String>,
    port: Option<u16>,
}

fn status_inner(path: &std::path::Path) -> Result<StatusInner> {
    let root = read_root(path)?;
    let mut events = Vec::new();
    let mut port = None;
    if let Some(hooks) = root.get("hooks").and_then(|v| v.as_object()) {
        for (event, value) in hooks {
            let Some(arr) = value.as_array() else { continue };
            for matcher_block in arr {
                let Some(inner) = matcher_block.get("hooks").and_then(|h| h.as_array()) else {
                    continue;
                };
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
        anyhow::bail!("settings.json root is not a JSON object");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn with_test_dir<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = super::super::util::home_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".gemini")).unwrap();
        // `dirs::home_dir()` reads `HOME` on Unix and `USERPROFILE`
        // on Windows — clear both into the tempdir so install paths
        // resolve inside the sandbox on every CI runner.
        let prev_home = std::env::var("HOME").ok();
        let prev_userprofile = std::env::var("USERPROFILE").ok();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("USERPROFILE", dir.path());
        f(dir.path());
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
    }

    #[test]
    fn installs_three_native_events() {
        with_test_dir(|home| {
            let report = GeminiHookInstaller.install(43117);
            assert!(report.is_ok());
            assert_eq!(report.installed.len(), GEMINI_HOOK_EVENTS.len());
            let body = fs::read_to_string(home.join(".gemini/settings.json")).unwrap();
            assert!(body.contains("BeforeTool"));
            assert!(body.contains("AfterTool"));
            assert!(body.contains("SessionEnd"));
            assert!(body.contains(":43117/hooks/gemini/"));
            assert!(body.contains("petpet-managed"));
        });
    }

    #[test]
    fn idempotent_on_repeat_run() {
        with_test_dir(|_| {
            GeminiHookInstaller.install(43117);
            let again = GeminiHookInstaller.install(43117);
            assert!(again.installed.is_empty());
            assert_eq!(again.already_present.len(), GEMINI_HOOK_EVENTS.len());
        });
    }

    #[test]
    fn uninstall_preserves_user_hooks_and_other_settings() {
        with_test_dir(|home| {
            let path = home.join(".gemini/settings.json");
            fs::write(
                &path,
                r#"{
                  "theme": "dark",
                  "hooks": {
                    "BeforeTool": [
                      {"matcher": "", "hooks": [{"type":"command","command":"echo user"}]}
                    ]
                  }
                }"#,
            )
            .unwrap();
            GeminiHookInstaller.install(43117);
            GeminiHookInstaller.uninstall();
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("\"theme\": \"dark\""));
            assert!(body.contains("echo user"));
            assert!(!body.contains("petpet-managed"));
        });
    }
}
