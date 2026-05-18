//! Codex CLI hook installer.
//!
//! Codex internally vendors `ClaudeHooksEngine` and reads hook handlers
//! from `~/.codex/hooks.json` with the **same Claude-shaped JSON schema**
//! and the **same PascalCase event names** (`PreToolUse`, `Stop`, etc.).
//! Earlier versions of petpet tried `~/.codex/config.toml [hooks]` with
//! `on_*` snake_case keys — that was wrong; Codex never read them.
//!
//! ## Multi-strategy chain
//!
//! Install runs each registered strategy in priority order. The first one
//! that successfully writes / detects its target wins; subsequent
//! strategies are not attempted. **Uninstall** sweeps every strategy
//! unconditionally so leftover state from older versions of petpet (or
//! older versions of Codex itself) gets cleaned out.
//!
//! Current strategies:
//!
//! | Pri | Target                             | Event names               | Notes                       |
//! |-----|------------------------------------|---------------------------|-----------------------------|
//! |   1 | `~/.codex/hooks.json`              | PascalCase (Claude-style) | Current Codex (≥ 0.115)     |
//! |   2 | `~/.codex/config.toml` `[hooks]`   | snake_case (`pre_tool_use`)| Speculative legacy fallback |
//! |   ∞ | (legacy `on_*` keys in config.toml)| —                         | Uninstall-only sweep        |

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Map, Value};
use toml_edit::{value, DocumentMut, Item, Table};

use super::util::{atomic_write_backed_up, extract_port, is_managed_command};
use super::{
    build_curl_command, HookInstaller, InstallReport, InstallStatus, PreflightOutcome,
    UninstallReport, CODEX_HOOK_EVENTS,
};
use crate::event::ProviderId;

pub struct CodexHookInstaller;

fn codex_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".codex");
    if !dir.exists() {
        return None;
    }
    Some(dir)
}

fn hooks_json_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("hooks.json"))
}

fn config_toml_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("config.toml"))
}

impl HookInstaller for CodexHookInstaller {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    /// Codex only loads `~/.codex/hooks.json` when
    /// `[features].codex_hooks = true` is present in `~/.codex/config.toml`.
    /// We detect, fix, and report — all section-aware via toml_edit so
    /// other user keys and comments survive.
    fn preflight(&self) -> PreflightOutcome {
        let mut out = PreflightOutcome::default();
        let Some(path) = config_toml_path() else { return out };

        match ensure_codex_hooks_feature(&path) {
            Ok(FeatureFlagOutcome::AlreadyEnabled) => {
                out.actions
                    .push("config.toml [features].codex_hooks already true".into());
            }
            Ok(FeatureFlagOutcome::Enabled { backup }) => {
                out.actions.push(
                    "set [features].codex_hooks = true in config.toml (required for Codex to load hooks.json)".into(),
                );
                if let Some(b) = backup {
                    out.backups.push(b);
                }
            }
            Ok(FeatureFlagOutcome::CreatedFile { .. }) => {
                out.actions.push(
                    "created ~/.codex/config.toml with [features].codex_hooks = true".into(),
                );
            }
            Err(e) => out.error = Some(format!("{e:#}")),
        }
        out
    }

    fn install(&self, port: u16) -> InstallReport {
        let mut report = InstallReport::new(ProviderId::Codex);

        if codex_dir().is_none() {
            report
                .warnings
                .push("~/.codex/ not found — skipping (user has not used Codex)".into());
            return report;
        }

        for strat in strategies() {
            match strat.install(port) {
                Ok(outcome)
                    if !outcome.installed.is_empty()
                        || !outcome.updated.is_empty()
                        || !outcome.already_present.is_empty() =>
                {
                    report.config_path = outcome.config_path;
                    report.installed = outcome.installed;
                    report.updated = outcome.updated;
                    report.already_present = outcome.already_present;
                    if let Some(b) = outcome.backup {
                        report.backups.push(b);
                    }
                    report.strategy = Some(strat.name().into());

                    // Always sweep legacy footprints after a successful install
                    // so old wrong-shape entries don't linger.
                    if let Ok(swept) = sweep_legacy_toml_keys() {
                        if !swept.is_empty() {
                            report
                                .warnings
                                .push(format!("cleaned up legacy keys: {}", swept.join(", ")));
                        }
                    }

                    return report;
                }
                Ok(_) => report.warnings.push(format!(
                    "strategy '{}' produced no diff (no Codex events?)",
                    strat.name()
                )),
                Err(e) => report
                    .warnings
                    .push(format!("strategy '{}' failed: {e:#}", strat.name())),
            }
        }
        report.error = Some("all Codex install strategies failed".into());
        report
    }

    fn uninstall(&self) -> UninstallReport {
        let mut report = UninstallReport::new(ProviderId::Codex);
        if codex_dir().is_none() {
            return report;
        }

        let mut removed = Vec::new();
        let mut latest_err: Option<String> = None;

        // Sweep every strategy + legacy paths regardless. Best-effort: we
        // don't abort on per-strategy failure.
        for strat in strategies() {
            match strat.uninstall() {
                Ok(mut r) => {
                    if !r.is_empty() {
                        report.config_path = strat.target_path();
                    }
                    removed.append(&mut r);
                }
                Err(e) => latest_err = Some(format!("{e:#}")),
            }
        }
        if let Ok(mut legacy) = sweep_legacy_toml_keys() {
            removed.append(&mut legacy);
        }

        removed.sort();
        removed.dedup();
        report.removed = removed;
        report.error = latest_err;
        report
    }

    fn status(&self) -> InstallStatus {
        let mut status = InstallStatus::new(ProviderId::Codex);
        if codex_dir().is_none() {
            return status;
        }
        for strat in strategies() {
            if let Ok(s) = strat.status() {
                if !s.events.is_empty() {
                    status.config_path = strat.target_path();
                    status.config_exists = true;
                    status.installed_events = s.events;
                    status.installed_port = s.port;
                    return status; // first strategy with hits wins
                }
            }
        }
        // Nothing installed — still report paths so callers see where we'd land.
        status.config_path = hooks_json_path();
        status.config_exists = status
            .config_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false);
        status
    }
}

trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn target_path(&self) -> Option<PathBuf>;
    fn install(&self, port: u16) -> Result<StrategyOutcome>;
    fn uninstall(&self) -> Result<Vec<String>>;
    fn status(&self) -> Result<StrategyStatus>;
}

struct StrategyOutcome {
    config_path: Option<PathBuf>,
    installed: Vec<String>,
    updated: Vec<String>,
    already_present: Vec<String>,
    backup: Option<PathBuf>,
}

struct StrategyStatus {
    events: Vec<String>,
    port: Option<u16>,
}

fn strategies() -> Vec<Box<dyn Strategy>> {
    vec![
        Box::new(HooksJsonStrategy),
        Box::new(ConfigTomlHooksTableStrategy),
    ]
}

// ═════════════════════════════════════════════════════════════════════════
// Strategy 1 (primary): ~/.codex/hooks.json  PascalCase Claude shape
// ═════════════════════════════════════════════════════════════════════════

struct HooksJsonStrategy;

impl Strategy for HooksJsonStrategy {
    fn name(&self) -> &'static str {
        "hooks.json"
    }

    fn target_path(&self) -> Option<PathBuf> {
        hooks_json_path()
    }

    fn install(&self, port: u16) -> Result<StrategyOutcome> {
        let Some(path) = hooks_json_path() else {
            return Ok(empty(path_none()));
        };
        let mut root = read_json(&path)?;
        let mut installed = Vec::new();
        let mut updated = Vec::new();
        let mut already_present = Vec::new();

        let obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("hooks.json root is not an object"))?;

        for event in CODEX_HOOK_EVENTS {
            let arr = obj
                .entry((*event).to_string())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("hooks.json {event} is not an array"))?;

            let desired_cmd = build_curl_command("codex", event, port);

            let mut updated_in_place = false;
            let mut present = false;

            for matcher_block in arr.iter_mut() {
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
                        present = true;
                    } else {
                        *cmd_str = desired_cmd.clone();
                        updated_in_place = true;
                    }
                }
            }

            if present {
                already_present.push((*event).into());
            } else if updated_in_place {
                updated.push((*event).into());
            } else {
                arr.push(json!({
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": desired_cmd }]
                }));
                installed.push((*event).into());
            }
        }

        let backup = atomic_write_backed_up(&path, &serde_json::to_string_pretty(&root)?)?;
        Ok(StrategyOutcome {
            config_path: Some(path),
            installed,
            updated,
            already_present,
            backup,
        })
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        let Some(path) = hooks_json_path() else { return Ok(Vec::new()) };
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut root = read_json(&path)?;
        let Some(obj) = root.as_object_mut() else { return Ok(Vec::new()) };
        let mut removed = Vec::new();

        for event_key in CODEX_HOOK_EVENTS.iter().chain(super::HOOK_EVENTS.iter()) {
            if let Some(Value::Array(arr)) = obj.get_mut(*event_key) {
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
                    removed.push((*event_key).to_string());
                }
            }
        }

        obj.retain(|_, v| !matches!(v, Value::Array(a) if a.is_empty()));

        if obj.is_empty() {
            // Remove the file entirely if we've emptied it.
            let _ = fs::remove_file(&path);
        } else {
            atomic_write_backed_up(&path, &serde_json::to_string_pretty(&root)?)?;
        }

        removed.sort();
        removed.dedup();
        Ok(removed)
    }

    fn status(&self) -> Result<StrategyStatus> {
        let Some(path) = hooks_json_path() else {
            return Ok(StrategyStatus { events: Vec::new(), port: None });
        };
        if !path.exists() {
            return Ok(StrategyStatus { events: Vec::new(), port: None });
        }
        let root = read_json(&path)?;
        let mut events = Vec::new();
        let mut port = None;
        if let Value::Object(obj) = &root {
            for (event_name, value) in obj {
                let Some(arr) = value.as_array() else { continue };
                for matcher_block in arr {
                    let Some(inner) = matcher_block.get("hooks").and_then(|h| h.as_array()) else {
                        continue;
                    };
                    for h in inner {
                        if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                            if is_managed_command(cmd) {
                                if !events.contains(event_name) {
                                    events.push(event_name.clone());
                                }
                                if port.is_none() {
                                    port = extract_port(cmd);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(StrategyStatus { events, port })
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Strategy 2 (fallback / future): ~/.codex/config.toml  [hooks]
// PascalCase keys (mirrors Claude's settings.json hooks block).
// ═════════════════════════════════════════════════════════════════════════

struct ConfigTomlHooksTableStrategy;

impl Strategy for ConfigTomlHooksTableStrategy {
    fn name(&self) -> &'static str {
        "config.toml [hooks]"
    }

    fn target_path(&self) -> Option<PathBuf> {
        config_toml_path()
    }

    fn install(&self, _port: u16) -> Result<StrategyOutcome> {
        // Speculative fallback path that we DON'T proactively write to.
        // If primary `hooks.json` strategy succeeds (it always should for
        // recent Codex), we never reach here. Future Codex schema change
        // can light this up by editing this block.
        Ok(empty(config_toml_path()))
    }

    fn uninstall(&self) -> Result<Vec<String>> {
        let Some(path) = config_toml_path() else { return Ok(Vec::new()) };
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut doc = read_toml(&path)?;
        let mut removed = Vec::new();
        if let Some(toml_edit::Item::Table(hooks)) = doc.get_mut("hooks") {
            let keys: Vec<String> = hooks.iter().map(|(k, _)| k.to_string()).collect();
            for k in keys {
                if let Some(v) = hooks.get(&k).and_then(|i| i.as_str()) {
                    if is_managed_command(v) {
                        hooks.remove(&k);
                        removed.push(k);
                    }
                }
            }
            if hooks.is_empty() {
                doc.remove("hooks");
            }
        }
        if !removed.is_empty() {
            atomic_write_backed_up(&path, doc.to_string().as_str())?;
        }
        Ok(removed)
    }

    fn status(&self) -> Result<StrategyStatus> {
        let Some(path) = config_toml_path() else {
            return Ok(StrategyStatus { events: Vec::new(), port: None });
        };
        if !path.exists() {
            return Ok(StrategyStatus { events: Vec::new(), port: None });
        }
        let doc = read_toml(&path)?;
        let mut events = Vec::new();
        let mut port = None;
        if let Some(toml_edit::Item::Table(hooks)) = doc.get("hooks") {
            for (k, v) in hooks.iter() {
                if let Some(s) = v.as_str() {
                    if is_managed_command(s) {
                        events.push(k.to_string());
                        if port.is_none() {
                            port = extract_port(s);
                        }
                    }
                }
            }
        }
        Ok(StrategyStatus { events, port })
    }
}

// ═════════════════════════════════════════════════════════════════════════
// Legacy `on_*` snake_case keys (petpet wrote these before 2026-05-15).
// Pure cleanup — never installed again.
// ═════════════════════════════════════════════════════════════════════════

const LEGACY_KEYS: &[&str] = &[
    "on_user_message",
    "on_tool_call_start",
    "on_tool_call_end",
    "on_task_complete",
    "on_subagent_complete",
    "on_session_start",
    "on_session_end",
    "on_notification",
];

fn sweep_legacy_toml_keys() -> Result<Vec<String>> {
    let Some(path) = config_toml_path() else { return Ok(Vec::new()) };
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut doc = read_toml(&path)?;
    let mut removed = Vec::new();
    if let Some(toml_edit::Item::Table(hooks)) = doc.get_mut("hooks") {
        for legacy in LEGACY_KEYS {
            if let Some(v) = hooks.get(legacy).and_then(|i| i.as_str()) {
                if is_managed_command(v) {
                    hooks.remove(legacy);
                    removed.push((*legacy).into());
                }
            }
        }
        if hooks.is_empty() {
            doc.remove("hooks");
        }
    }
    if !removed.is_empty() {
        atomic_write_backed_up(&path, doc.to_string().as_str())?;
    }
    Ok(removed)
}

// ═════════════════════════════════════════════════════════════════════════
// Codex preflight: [features].codex_hooks = true
// ═════════════════════════════════════════════════════════════════════════

/// What `ensure_codex_hooks_feature` did to the TOML file.
#[derive(Debug)]
enum FeatureFlagOutcome {
    /// Already had the right value — nothing changed.
    AlreadyEnabled,
    /// Found the [features] section and inserted/updated the key.
    Enabled { backup: Option<PathBuf> },
    /// `config.toml` didn't exist — we created it with just the feature.
    #[allow(dead_code)]
    CreatedFile { backup: Option<PathBuf> },
}

/// Section-aware: only touches `[features].codex_hooks`. A top-level
/// `codex_hooks =` or one under `[other_section]` does not satisfy the
/// check (because Codex CLI specifically looks under `[features]`).
fn ensure_codex_hooks_feature(path: &std::path::Path) -> Result<FeatureFlagOutcome> {
    if !path.exists() {
        atomic_write_backed_up(path, "[features]\ncodex_hooks = true\n")?;
        return Ok(FeatureFlagOutcome::CreatedFile { backup: None });
    }

    let raw = fs::read_to_string(path)?;
    let mut doc: DocumentMut = raw.parse()?;

    // Walk down to features.codex_hooks specifically — don't be fooled by
    // a same-named key elsewhere.
    let already = doc
        .get("features")
        .and_then(|i| i.as_table())
        .and_then(|t| t.get("codex_hooks"))
        .and_then(|i| i.as_bool());

    if already == Some(true) {
        return Ok(FeatureFlagOutcome::AlreadyEnabled);
    }

    // Need to insert or update. Ensure [features] exists as a table.
    if !doc.contains_key("features") {
        doc["features"] = Item::Table(Table::new());
    }
    let features = doc["features"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[features] is not a TOML table"))?;
    features.insert("codex_hooks", value(true));

    let new_content = doc.to_string();
    let backup = atomic_write_backed_up(path, &new_content)?;
    Ok(FeatureFlagOutcome::Enabled { backup })
}

// ── helpers ──────────────────────────────────────────────────────────────

fn read_json(path: &std::path::Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let v: Value = serde_json::from_str(&raw)?;
    if !v.is_object() {
        anyhow::bail!("{} root is not a JSON object", path.display());
    }
    Ok(v)
}

fn read_toml(path: &std::path::Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = fs::read_to_string(path)?;
    let doc: DocumentMut = raw.parse()?;
    Ok(doc)
}

fn empty(path: Option<PathBuf>) -> StrategyOutcome {
    StrategyOutcome {
        config_path: path,
        installed: Vec::new(),
        updated: Vec::new(),
        already_present: Vec::new(),
        backup: None,
    }
}

fn path_none() -> Option<PathBuf> {
    None
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
        let codex = dir.path().join(".codex");
        fs::create_dir_all(&codex).unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());
        f(dir.path());
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn json_strategy_creates_hooks_json_with_pascal_case() {
        with_test_dir(|home| {
            let outcome = HooksJsonStrategy.install(43117).unwrap();
            assert_eq!(outcome.installed.len(), CODEX_HOOK_EVENTS.len());
            let body = fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
            assert!(body.contains("\"PreToolUse\""));
            assert!(body.contains("\"UserPromptSubmit\""));
            assert!(body.contains(":43117/hooks/codex/UserPromptSubmit"));
            assert!(body.contains("petpet-managed"));
            assert!(!body.contains("on_user_message"));
        });
    }

    #[test]
    fn json_strategy_is_idempotent() {
        with_test_dir(|_| {
            HooksJsonStrategy.install(43117).unwrap();
            let again = HooksJsonStrategy.install(43117).unwrap();
            assert!(again.installed.is_empty());
            assert!(again.updated.is_empty());
            assert_eq!(again.already_present.len(), CODEX_HOOK_EVENTS.len());
        });
    }

    #[test]
    fn json_strategy_updates_port_in_place() {
        with_test_dir(|home| {
            HooksJsonStrategy.install(43117).unwrap();
            let again = HooksJsonStrategy.install(9999).unwrap();
            assert_eq!(again.updated.len(), CODEX_HOOK_EVENTS.len());
            let body = fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
            assert!(body.contains(":9999/"));
            assert!(!body.contains(":43117/"));
        });
    }

    #[test]
    fn json_strategy_preserves_user_hooks_alongside_ours() {
        with_test_dir(|home| {
            let path = home.join(".codex/hooks.json");
            fs::write(
                &path,
                r#"{
                  "PreToolUse": [
                    {"matcher":"","hooks":[{"type":"command","command":"echo user-hook"}]}
                  ]
                }"#,
            )
            .unwrap();
            HooksJsonStrategy.install(43117).unwrap();
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("echo user-hook"));
            assert!(body.contains("petpet-managed"));
        });
    }

    #[test]
    fn uninstall_sweeps_json_only_managed_entries() {
        with_test_dir(|home| {
            let path = home.join(".codex/hooks.json");
            fs::write(
                &path,
                r#"{
                  "PreToolUse": [
                    {"matcher":"","hooks":[{"type":"command","command":"echo user-hook"}]}
                  ]
                }"#,
            )
            .unwrap();
            HooksJsonStrategy.install(43117).unwrap();
            let removed = HooksJsonStrategy.uninstall().unwrap();
            assert!(removed.contains(&"PreToolUse".into()));
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("echo user-hook"));
            assert!(!body.contains("petpet-managed"));
        });
    }

    #[test]
    fn legacy_toml_keys_are_swept_after_install() {
        with_test_dir(|home| {
            let toml_path = home.join(".codex/config.toml");
            fs::write(
                &toml_path,
                r#"
model = "gpt-5"

[hooks]
on_user_message = "curl http://127.0.0.1:43117/hooks/codex/UserPromptSubmit # petpet-managed"
on_tool_call_start = "curl http://127.0.0.1:43117/hooks/codex/PreToolUse # petpet-managed"
"#,
            )
            .unwrap();

            // Full install (calls primary JSON + sweep_legacy).
            let report = CodexHookInstaller.install(43117);
            assert!(report.is_ok());
            let body = fs::read_to_string(&toml_path).unwrap();
            assert!(body.contains(r#"model = "gpt-5""#)); // user config preserved
            assert!(!body.contains("on_user_message"));
            assert!(!body.contains("petpet-managed"));
        });
    }

    #[test]
    fn full_install_then_uninstall_leaves_no_artifacts() {
        with_test_dir(|home| {
            // Pre-existing user content in both files
            let toml_path = home.join(".codex/config.toml");
            fs::write(&toml_path, "model = \"gpt-5\"\n").unwrap();
            let json_path = home.join(".codex/hooks.json");
            fs::write(
                &json_path,
                r#"{"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"echo keep"}]}]}"#,
            )
            .unwrap();

            CodexHookInstaller.install(43117);
            CodexHookInstaller.uninstall();

            let toml_body = fs::read_to_string(&toml_path).unwrap();
            assert!(toml_body.contains(r#"model = "gpt-5""#));
            let json_body = fs::read_to_string(&json_path).unwrap();
            assert!(json_body.contains("echo keep"));
            assert!(!json_body.contains("petpet-managed"));
            assert!(!toml_body.contains("petpet-managed"));
        });
    }

    #[test]
    fn preflight_creates_features_section_when_missing() {
        with_test_dir(|home| {
            let toml_path = home.join(".codex/config.toml");
            fs::write(&toml_path, "model = \"gpt-5\"\n").unwrap();
            let out = CodexHookInstaller.preflight();
            assert!(out.error.is_none());
            assert!(!out.actions.is_empty());
            let body = fs::read_to_string(&toml_path).unwrap();
            assert!(body.contains("model = \"gpt-5\""));
            assert!(body.contains("[features]"));
            assert!(body.contains("codex_hooks = true"));
        });
    }

    #[test]
    fn preflight_idempotent_when_already_enabled() {
        with_test_dir(|home| {
            let toml_path = home.join(".codex/config.toml");
            fs::write(&toml_path, "[features]\ncodex_hooks = true\n").unwrap();
            let out = CodexHookInstaller.preflight();
            assert!(out.error.is_none());
            assert_eq!(out.actions.len(), 1);
            assert!(out.actions[0].contains("already"));
            assert!(out.backups.is_empty());
        });
    }

    #[test]
    fn preflight_flips_false_to_true_and_backs_up() {
        with_test_dir(|home| {
            let toml_path = home.join(".codex/config.toml");
            fs::write(
                &toml_path,
                "model = \"gpt-5\"\n\n[features]\ncodex_hooks = false\nother = true\n",
            )
            .unwrap();
            let out = CodexHookInstaller.preflight();
            assert!(out.error.is_none());
            assert_eq!(out.backups.len(), 1, "should produce a .bak");
            let body = fs::read_to_string(&toml_path).unwrap();
            assert!(body.contains("codex_hooks = true"));
            assert!(body.contains("other = true"));
            assert!(body.contains("model = \"gpt-5\""));
        });
    }

    #[test]
    fn preflight_ignores_codex_hooks_in_wrong_section() {
        with_test_dir(|home| {
            let toml_path = home.join(".codex/config.toml");
            fs::write(
                &toml_path,
                "codex_hooks = true\n\n[unrelated]\ncodex_hooks = true\n",
            )
            .unwrap();
            let out = CodexHookInstaller.preflight();
            assert!(out.error.is_none());
            let body = fs::read_to_string(&toml_path).unwrap();
            assert!(body.contains("[features]"));
            assert!(body.contains("[unrelated]"));
            // The wrong-section codex_hooks key is left intact
            assert_eq!(body.matches("codex_hooks = true").count(), 3);
        });
    }

    #[test]
    fn status_reports_install_state_correctly() {
        with_test_dir(|_| {
            let empty_status = CodexHookInstaller.status();
            assert!(empty_status.installed_events.is_empty());
            assert!(empty_status.installed_port.is_none());

            CodexHookInstaller.install(43117);
            let s = CodexHookInstaller.status();
            assert_eq!(s.installed_port, Some(43117));
            let mut have = s.installed_events;
            have.sort();
            let mut want: Vec<String> = CODEX_HOOK_EVENTS.iter().map(|s| (*s).into()).collect();
            want.sort();
            assert_eq!(have, want);
        });
    }
}
