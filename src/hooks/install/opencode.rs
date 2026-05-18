//! OpenCode plugin installer.
//!
//! OpenCode uses a fundamentally different mechanism from Claude/Codex/Gemini:
//! plugins are **TS/JS modules loaded by the OpenCode runtime**, not JSON
//! entries describing shell commands. So this installer writes a single
//! `.js` plugin file rather than merging into a settings object.
//!
//! Plugin location resolution (matches OpenCode docs):
//! 1. `$OPENCODE_CONFIG_DIR/plugins/petpet.js`
//! 2. `$XDG_CONFIG_HOME/opencode/plugins/petpet.js`
//! 3. `~/.config/opencode/plugins/petpet.js`
//!
//! The plugin code itself maps OpenCode's dot-notation events to our
//! PascalCase canonical names and POSTs to the hook server. The marker
//! `# petpet-managed` is embedded in the JS comment header so we can
//! detect/replace/remove our own file without ever touching other plugins
//! the user installed.

use std::fs;
use std::path::PathBuf;

use super::util::{atomic_write_backed_up, extract_port, is_managed_command};
use super::{HookInstaller, InstallReport, InstallStatus, UninstallReport, OPENCODE_HOOK_EVENTS};
use crate::event::ProviderId;

pub struct OpenCodeHookInstaller;

/// Resolve OpenCode's plugins/ dir. Probes every known location for
/// OpenCode's config (Unix XDG, Windows `%APPDATA%\opencode`, env-var
/// overrides) and returns the first that exists. Returns `None` if
/// the user has never used OpenCode — we don't create a config dir
/// for a tool they don't have.
///
/// See `crate::paths::opencode_config_dir_candidates` for the full
/// search order and per-platform rationale.
fn plugins_dir() -> Option<PathBuf> {
    crate::paths::first_existing(crate::paths::opencode_config_dir_candidates())
        .map(|d| d.join("plugins"))
}

fn plugin_path() -> Option<PathBuf> {
    plugins_dir().map(|d| d.join("petpet.js"))
}

impl HookInstaller for OpenCodeHookInstaller {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCode
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn install(&self, port: u16) -> InstallReport {
        let mut report = InstallReport::new(ProviderId::OpenCode);
        report.strategy = Some("plugins/petpet.js".into());

        let path = match plugin_path() {
            Some(p) => p,
            None => {
                report
                    .warnings
                    .push("OpenCode config dir not found — skipping (user has not used OpenCode)".into());
                return report;
            }
        };
        report.config_path = Some(path.clone());

        // Capture state BEFORE the write — otherwise the post-write file
        // always contains our content and we can't tell first-install from
        // already-present.
        let prior = if path.exists() { fs::read_to_string(&path).ok() } else { None };
        let content = plugin_source(port);
        let was_identical = prior.as_deref() == Some(content.as_str());
        let existed_before = prior.is_some();

        match atomic_write_backed_up(&path, &content) {
            Ok(backup) => {
                let label_events = || OPENCODE_HOOK_EVENTS.iter().map(|s| (*s).to_string());
                if was_identical {
                    report.already_present.extend(label_events());
                } else if existed_before {
                    report.updated.extend(label_events());
                    if let Some(b) = backup {
                        report.backups.push(b);
                    }
                } else {
                    report.installed.extend(label_events());
                }
            }
            Err(e) => report.error = Some(format!("{e:#}")),
        }
        report
    }

    fn uninstall(&self) -> UninstallReport {
        let mut report = UninstallReport::new(ProviderId::OpenCode);
        let Some(path) = plugin_path() else { return report };
        if !path.exists() {
            return report;
        }
        report.config_path = Some(path.clone());

        // Only remove if it's OUR file — recognize the petpet-managed
        // fingerprint in the header so a user-named `petpet.js` of their
        // own (unlikely but possible) stays intact.
        match fs::read_to_string(&path) {
            Ok(contents) if is_managed_command(&contents) => match fs::remove_file(&path) {
                Ok(()) => {
                    report
                        .removed
                        .extend(OPENCODE_HOOK_EVENTS.iter().map(|s| (*s).to_string()));
                }
                Err(e) => report.error = Some(format!("remove plugin: {e:#}")),
            },
            Ok(_) => { /* not ours; leave alone */ }
            Err(e) => report.error = Some(format!("read plugin: {e:#}")),
        }
        report
    }

    fn status(&self) -> InstallStatus {
        let mut status = InstallStatus::new(ProviderId::OpenCode);
        let Some(path) = plugin_path() else { return status };
        status.config_path = Some(path.clone());
        status.config_exists = path.exists();
        if !status.config_exists {
            return status;
        }
        if let Ok(contents) = fs::read_to_string(&path) {
            if is_managed_command(&contents) {
                status.installed_port = extract_port(&contents);
                status
                    .installed_events
                    .extend(OPENCODE_HOOK_EVENTS.iter().map(|s| (*s).to_string()));
            }
        }
        status
    }
}

/// The JS plugin source we emit. Includes:
/// - `# petpet-managed` fingerprint (in JS comment),
/// - the `127.0.0.1:PORT/hooks/opencode/` URL (port extraction works),
/// - event-name normalization from OpenCode's dot-notation to our
///   PascalCase canonical vocabulary (server parsers stay format-agnostic).
fn plugin_source(port: u16) -> String {
    let url = format!("http://127.0.0.1:{port}/hooks/opencode");
    format!(
        r#"// petpet — OpenCode plugin (auto-generated).
// {marker}
// Forwards OpenCode runtime events to petpet's hook server at {url}.
// Re-running `petpet hooks install` overwrites this file. Edit by hand only
// if you understand the petpet-managed marker is what makes uninstall work.

const ENDPOINT = "{url}";

async function forward(eventName, body) {{
  try {{
    await fetch(`${{ENDPOINT}}/${{eventName}}`, {{
      method: "POST",
      headers: {{ "Content-Type": "application/json" }},
      body: JSON.stringify(body ?? {{}}),
      signal: AbortSignal.timeout(800),
    }});
  }} catch {{
    // hook server offline or slow — agent shouldn't notice
  }}
}}

const SESSION_EVENT_MAP = {{
  "session.created": "SessionStart",
  "session.idle": "Stop",
  "session.error": "StopFailure",
  "permission.asked": "PermissionRequest",
}};

const hooks = {{
  "tool.execute.before": async (input, output) => {{
    await forward("PreToolUse", {{
      tool_name: input?.tool,
      tool_input: output?.args ?? input?.args,
      tool_use_id: input?.callID ?? input?.id,
    }});
  }},
  "tool.execute.after": async (input) => {{
    const ok = !input?.output?.error;
    await forward(ok ? "PostToolUse" : "PostToolUseFailure", {{
      tool_name: input?.tool,
      tool_input: input?.args,
      tool_response: {{ is_error: !ok, output: input?.output }},
      tool_use_id: input?.callID ?? input?.id,
    }});
  }},
  event: async ({{ event }}) => {{
    const mapped = SESSION_EVENT_MAP[event?.type];
    if (mapped) await forward(mapped, event ?? {{}});
  }},
}};

const PetpetPlugin = {{
  id: "petpet",
  server: async () => hooks,
}};

export default PetpetPlugin;
export {{ PetpetPlugin }};
"#,
        marker = super::PETPET_MARKER,
        url = url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tempfile::tempdir;

    fn with_test_dir<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = super::super::util::home_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        let plugins = dir.path().join(".config/opencode/plugins");
        fs::create_dir_all(&plugins).unwrap();
        // Sandbox HOME (and the higher-priority opencode-specific +
        // XDG overrides) into the tempdir. We deliberately do NOT
        // mutate `APPDATA` / `LOCALAPPDATA`: those are read by
        // `paths::tests::windows_candidates_include_appdata_and_localappdata`
        // running in parallel under its own lock, and clearing them
        // here would race that test on windows-latest. The Unix-style
        // `~/.config/opencode/plugins` candidate (under our sandboxed
        // HOME) exists before install runs, and `first_existing` picks
        // it as the highest-priority match, so the Windows APPDATA
        // probes don't matter for correctness here.
        let prev_home = env::var("HOME").ok();
        let prev_xdg = env::var("XDG_CONFIG_HOME").ok();
        let prev_oc = env::var("OPENCODE_CONFIG_DIR").ok();
        env::set_var("HOME", dir.path());
        env::remove_var("XDG_CONFIG_HOME");
        env::remove_var("OPENCODE_CONFIG_DIR");
        f(dir.path());
        match prev_home {
            Some(h) => env::set_var("HOME", h),
            None => env::remove_var("HOME"),
        }
        if let Some(v) = prev_xdg {
            env::set_var("XDG_CONFIG_HOME", v);
        }
        if let Some(v) = prev_oc {
            env::set_var("OPENCODE_CONFIG_DIR", v);
        }
    }

    #[test]
    fn install_writes_plugin_file() {
        with_test_dir(|home| {
            let report = OpenCodeHookInstaller.install(43117);
            assert!(report.is_ok());
            assert_eq!(report.installed.len(), OPENCODE_HOOK_EVENTS.len());
            let path = home.join(".config/opencode/plugins/petpet.js");
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("petpet-managed"));
            assert!(body.contains(":43117/hooks/opencode"));
            assert!(body.contains("tool.execute.before"));
            assert!(body.contains("PreToolUse"));
            // Map keys present
            assert!(body.contains("session.idle"));
            assert!(body.contains("PostToolUseFailure"));
        });
    }

    #[test]
    fn install_is_idempotent_when_content_matches() {
        with_test_dir(|_| {
            OpenCodeHookInstaller.install(43117);
            let again = OpenCodeHookInstaller.install(43117);
            assert!(again.installed.is_empty());
            assert!(again.updated.is_empty());
            assert_eq!(again.already_present.len(), OPENCODE_HOOK_EVENTS.len());
            assert!(again.backups.is_empty());
        });
    }

    #[test]
    fn install_updates_when_port_changes_and_backs_up() {
        with_test_dir(|home| {
            OpenCodeHookInstaller.install(43117);
            let r = OpenCodeHookInstaller.install(9999);
            assert!(r.installed.is_empty());
            assert_eq!(r.updated.len(), OPENCODE_HOOK_EVENTS.len());
            assert_eq!(r.backups.len(), 1);
            let body = fs::read_to_string(home.join(".config/opencode/plugins/petpet.js")).unwrap();
            assert!(body.contains(":9999/"));
            assert!(!body.contains(":43117/"));
        });
    }

    #[test]
    fn uninstall_removes_only_our_plugin() {
        with_test_dir(|home| {
            OpenCodeHookInstaller.install(43117);
            let r = OpenCodeHookInstaller.uninstall();
            assert_eq!(r.removed.len(), OPENCODE_HOOK_EVENTS.len());
            assert!(!home.join(".config/opencode/plugins/petpet.js").exists());
        });
    }

    #[test]
    fn uninstall_leaves_foreign_petpet_js_intact() {
        with_test_dir(|home| {
            let path = home.join(".config/opencode/plugins/petpet.js");
            fs::write(&path, "// user-authored, no marker\nconst x = 1;\n").unwrap();
            let r = OpenCodeHookInstaller.uninstall();
            assert!(r.removed.is_empty());
            assert!(path.exists(), "must not touch unmanaged file");
        });
    }

    #[test]
    fn status_reports_install_state_correctly() {
        with_test_dir(|_| {
            let empty = OpenCodeHookInstaller.status();
            assert!(empty.installed_events.is_empty());
            OpenCodeHookInstaller.install(43117);
            let after = OpenCodeHookInstaller.status();
            assert_eq!(after.installed_port, Some(43117));
            assert_eq!(after.installed_events.len(), OPENCODE_HOOK_EVENTS.len());
        });
    }
}
