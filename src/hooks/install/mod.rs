//! Generic hook-installation framework.
//!
//! Each AI CLI/IDE provider implements [`HookInstaller`]. The desktop app
//! and CLI both call [`install_all`] to opportunistically wire every
//! known provider's hooks to our [`HookServer`]. Idempotent — running
//! repeatedly converges to the same state without duplication.
//!
//! Safety contract (every installer MUST honor):
//!
//! 1. **Create-if-not-exist** for our own marks; never invent a config
//!    file from nothing for a provider the user has not used.
//! 2. **Atomic writes** — write to `<file>.petpet.tmp` then `rename(2)`;
//!    crashed runs never leave a partial config file behind.
//! 3. **Fingerprint marker** in every command we write, so uninstall
//!    can find exactly our entries and skip anything the user added.
//! 4. **Preserve user data** — only append to arrays / insert missing
//!    keys; never overwrite values we did not put there.
//! 5. **Best-effort, never panicking** — partial failure of one provider
//!    must not block the others.

use std::path::PathBuf;

use serde::Serialize;

use crate::event::ProviderId;
use crate::hooks::DEFAULT_HOOK_PORT;

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod util;

/// Shell-comment sentinel appended to every command we write.
/// Survives JSON / TOML round-trip and is grep-able from any format.
/// Fingerprint string used to identify hook commands that petpet
/// installed (vs. unrelated curl commands the user might have added
/// themselves). Embedded in a URL query parameter inside the curl
/// command so it doesn't depend on shell-comment syntax (which differs
/// across `sh` / `cmd.exe` / PowerShell). Detection is a plain
/// `cmd.contains(PETPET_MARKER)` substring match, so the leading-`#`
/// form from old installs is still recognised correctly.
pub const PETPET_MARKER: &str = "petpet-managed";

/// Hook events we install for Claude Code (PascalCase).
///
/// Selection criteria:
///   - generic (no per-file matcher needed)
///   - has gameplay relevance for the pet (animation, XP, achievement)
///   - won't drown the pet in noise (e.g. `InstructionsLoaded` fires many
///     times per session — skipped here, reintroduce later if needed)
///
/// Adding a new event here makes the installer wire it up next run.
/// The hook server already parses any unknown event into `Other`, so
/// new events ship without parser updates.
pub const HOOK_EVENTS: &[&str] = &[
    // Lifecycle
    "SessionStart",
    "SessionEnd",
    // Per-turn
    "UserPromptSubmit",
    "UserPromptExpansion",
    "Stop",
    "StopFailure",
    // Tool loop
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "PermissionRequest",
    "PermissionDenied",
    // Subagent
    "SubagentStart",
    "SubagentStop",
    // Task tracking
    "TaskCreated",
    "TaskCompleted",
    // Compaction
    "PreCompact",
    "PostCompact",
    // Misc
    "Notification",
];

/// Subset of [`HOOK_EVENTS`] that Codex CLI currently dispatches.
/// (Codex's `HOOK_EVENT_NAMES` const lists exactly 8 events as of 2026-05.)
pub const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "PreCompact",
    "PostCompact",
];

/// Gemini CLI uses a smaller native event set. Names mapped directly to
/// the canonical petpet vocabulary in `parsers.rs`.
pub const GEMINI_HOOK_EVENTS: &[&str] = &[
    "BeforeTool",
    "AfterTool",
    "SessionEnd",
];

/// OpenCode plugin subscribes to these native event names. Translation
/// to our canonical vocabulary happens INSIDE the generated JS plugin so
/// the server-side parser stays format-agnostic.
pub const OPENCODE_HOOK_EVENTS: &[&str] = &[
    "tool.execute.before",
    "tool.execute.after",
    "session.created",
    "session.idle",
    "session.error",
    "permission.asked",
];

/// What changed during install for one provider.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub provider: ProviderId,
    pub config_path: Option<PathBuf>,
    /// Events newly written into the config.
    pub installed: Vec<String>,
    /// Events whose petpet entry was already present (no change needed).
    pub already_present: Vec<String>,
    /// Events whose petpet entry existed but pointed to a different port —
    /// we updated them in place.
    pub updated: Vec<String>,
    /// What `preflight()` did before this install (feature flags, etc).
    /// Empty for providers with no preconditions.
    pub preflight_actions: Vec<String>,
    /// Non-fatal warnings (e.g. an experimental strategy that didn't apply).
    pub warnings: Vec<String>,
    /// Hard errors that stopped install for this provider.
    pub error: Option<String>,
    /// Which install strategy succeeded (Codex has several; Claude has one).
    pub strategy: Option<String>,
    /// Timestamped `.bak` files we wrote before mutating user configs.
    /// Caller can show these so the user knows the rollback path.
    pub backups: Vec<PathBuf>,
}

impl InstallReport {
    pub fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            config_path: None,
            installed: Vec::new(),
            already_present: Vec::new(),
            updated: Vec::new(),
            preflight_actions: Vec::new(),
            warnings: Vec::new(),
            error: None,
            strategy: None,
            backups: Vec::new(),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Result of an installer's preflight pass — preconditions necessary
/// for the agent to actually pick up the hooks we're about to write.
#[derive(Debug, Default, Clone, Serialize)]
pub struct PreflightOutcome {
    /// Human-readable description of what we did (or "already satisfied").
    pub actions: Vec<String>,
    /// Backup paths created in the process.
    pub backups: Vec<PathBuf>,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
    /// Hard error — preflight may still let install proceed; hooks just
    /// won't take effect until the user fixes the precondition.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UninstallReport {
    pub provider: ProviderId,
    pub config_path: Option<PathBuf>,
    pub removed: Vec<String>,
    pub error: Option<String>,
}

impl UninstallReport {
    pub fn new(provider: ProviderId) -> Self {
        Self { provider, config_path: None, removed: Vec::new(), error: None }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallStatus {
    pub provider: ProviderId,
    pub config_path: Option<PathBuf>,
    pub config_exists: bool,
    pub installed_events: Vec<String>,
    pub installed_port: Option<u16>,
}

impl InstallStatus {
    pub fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            config_path: None,
            config_exists: false,
            installed_events: Vec::new(),
            installed_port: None,
        }
    }

    pub fn fully_installed_for(&self, port: u16) -> bool {
        self.installed_port == Some(port)
            && HOOK_EVENTS.iter().all(|e| self.installed_events.iter().any(|i| i == *e))
    }
}

pub trait HookInstaller: Send + Sync {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;

    /// Preconditions to satisfy BEFORE writing hook entries. Default is
    /// no-op. Override for agents that gate hook loading behind a
    /// separate feature flag (e.g. Codex `[features].codex_hooks=true`
    /// in `~/.codex/config.toml`).
    fn preflight(&self) -> PreflightOutcome {
        PreflightOutcome::default()
    }

    fn install(&self, port: u16) -> InstallReport;
    fn uninstall(&self) -> UninstallReport;
    fn status(&self) -> InstallStatus;
}

fn installers() -> Vec<Box<dyn HookInstaller>> {
    vec![
        Box::new(claude::ClaudeHookInstaller),
        Box::new(codex::CodexHookInstaller),
        Box::new(gemini::GeminiHookInstaller),
        Box::new(opencode::OpenCodeHookInstaller),
    ]
}

pub fn install_all(port: u16) -> Vec<InstallReport> {
    installers()
        .iter()
        .map(|i| {
            let pre = i.preflight();
            let mut report = i.install(port);
            report.preflight_actions = pre.actions;
            for b in pre.backups {
                if !report.backups.contains(&b) {
                    report.backups.push(b);
                }
            }
            for w in pre.warnings {
                report.warnings.push(w);
            }
            if let Some(e) = pre.error {
                // Preflight error doesn't override install error if install
                // already failed for some other reason.
                if report.error.is_none() {
                    report.error = Some(format!("preflight: {e}"));
                } else {
                    report
                        .warnings
                        .push(format!("preflight also failed: {e}"));
                }
            }
            report
        })
        .collect()
}

pub fn uninstall_all() -> Vec<UninstallReport> {
    installers().iter().map(|i| i.uninstall()).collect()
}

pub fn status_all() -> Vec<InstallStatus> {
    installers().iter().map(|i| i.status()).collect()
}

/// Desktop-app convenience: install only if not already fully installed for
/// the given port. Logs at info level on first install, debug otherwise.
pub fn ensure_installed(port: u16) -> Vec<InstallReport> {
    let mut reports = Vec::new();
    for inst in installers() {
        let status = inst.status();
        if status.fully_installed_for(port) {
            tracing::debug!(provider = %inst.id(), "hooks already installed");
            continue;
        }
        let report = inst.install(port);
        if !report.installed.is_empty() || !report.updated.is_empty() {
            tracing::info!(
                provider = %inst.id(),
                installed = ?report.installed,
                updated = ?report.updated,
                strategy = ?report.strategy,
                "hooks auto-installed"
            );
        }
        if let Some(err) = &report.error {
            tracing::warn!(provider = %inst.id(), error = %err, "hook install failed (non-fatal)");
        }
        reports.push(report);
    }
    reports
}

/// Build the shell command we'll embed in each hook entry.
/// curl reads the hook JSON payload from stdin (Claude/Codex feed it
/// that way) and POSTs it to our endpoint. The `petpet-managed`
/// fingerprint is encoded as a URL query parameter rather than a
/// trailing `#` shell comment, because:
///   - `#` is NOT a comment in Windows `cmd.exe` — cmd would treat
///     the rest of the string as args appended to the URL, breaking
///     the request.
///   - Single quotes aren't recognized by `cmd.exe` either — only
///     double quotes work as string delimiters there.
///
/// Using double quotes for the header and the URL, plus a query-param
/// marker, gives us a command string that runs unmodified under POSIX
/// `sh`, `bash`, `cmd.exe`, and PowerShell. Detection still works via
/// the `cmd.contains(PETPET_MARKER)` substring check in
/// `util::is_managed_command`.
pub fn build_curl_command(provider_slug: &str, event: &str, port: u16) -> String {
    format!(
        "curl -fsS --max-time 1 -X POST -H \"Content-Type: application/json\" --data-binary @- \"http://127.0.0.1:{port}/hooks/{provider_slug}/{event}?_={PETPET_MARKER}\""
    )
}

#[cfg(test)]
mod build_curl_command_tests {
    //! Pin the wire-format of the generated hook command. These
    //! invariants are what make the command cross-platform; a
    //! regression that re-introduces single quotes or `#` markers
    //! would silently break the hooks on Windows.
    use super::{build_curl_command, PETPET_MARKER};

    #[test]
    fn produces_curl_with_double_quoted_header() {
        let cmd = build_curl_command("claude", "Stop", 43117);
        // Header must use double quotes (cmd.exe doesn't grok `'`).
        assert!(
            cmd.contains("-H \"Content-Type: application/json\""),
            "expected double-quoted Content-Type header, got: {cmd}"
        );
    }

    #[test]
    fn contains_no_single_quotes() {
        let cmd = build_curl_command("claude", "Stop", 43117);
        assert!(
            !cmd.contains('\''),
            "single quotes are not portable to cmd.exe — got: {cmd}"
        );
    }

    #[test]
    fn contains_no_hash_shell_comment_marker() {
        // The marker MUST be URL-encoded, not a `#` shell comment —
        // cmd.exe doesn't recognise `#` so the rest of the line would
        // be appended as args to the URL and break the request.
        let cmd = build_curl_command("claude", "Stop", 43117);
        assert!(
            !cmd.contains("# petpet"),
            "found legacy shell-comment marker — got: {cmd}"
        );
    }

    #[test]
    fn embeds_marker_in_url_for_detection() {
        let cmd = build_curl_command("codex", "PreToolUse", 9999);
        // Detection in `util::is_managed_command` is a substring match
        // on PETPET_MARKER; the URL must therefore carry it.
        assert!(cmd.contains(PETPET_MARKER));
        assert!(cmd.contains("?_=petpet-managed"));
    }

    #[test]
    fn url_includes_provider_slug_event_and_port() {
        let cmd = build_curl_command("opencode", "SessionStart", 12345);
        assert!(cmd.contains("http://127.0.0.1:12345/hooks/opencode/SessionStart"));
    }

    #[test]
    fn url_is_double_quoted_so_query_chars_dont_break_cmd() {
        // cmd.exe treats `?` and `&` as plain chars only inside
        // quotes; unquoted URLs with `?` work by accident but lose
        // robustness against future curl flags. Pinning this here.
        let cmd = build_curl_command("claude", "Stop", 43117);
        assert!(
            cmd.contains("\"http://127.0.0.1:43117/hooks/claude/Stop?_=petpet-managed\""),
            "URL must be wrapped in double quotes: {cmd}"
        );
    }

    #[test]
    fn reads_stdin_via_data_binary_dash() {
        // Claude/Codex feed the hook payload on stdin — losing
        // `--data-binary @-` would drop the JSON body and our handler
        // would see empty requests.
        let cmd = build_curl_command("claude", "Stop", 43117);
        assert!(
            cmd.contains("--data-binary @-"),
            "stdin → body wiring missing: {cmd}"
        );
    }
}

/// Re-exported sentinel port for callers that want the default.
pub fn default_port() -> u16 {
    DEFAULT_HOOK_PORT
}
