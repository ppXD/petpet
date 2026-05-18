//! Aider provider — reads Aider's local `analytics-log` JSONL.
//!
//! ## How it works
//!
//! Aider supports a local analytics log via the `--analytics-log <file>`
//! CLI flag (or `analytics-log: <file>` in `~/.aider.conf.yml`).  See:
//!   - <https://aider.chat/docs/more/analytics.html>
//!   - <https://github.com/Aider-AI/aider/blob/main/aider/analytics.py>
//!   - <https://github.com/Aider-AI/aider/blob/main/aider/coders/base_coder.py>
//!
//! That log is **independent of Aider's external analytics opt-in** —
//! looking at `Analytics.event()` in `analytics.py`, the `logfile` path
//! is written whenever it's set, even when PostHog/Mixpanel are
//! disabled. So enabling local logging does NOT send any data
//! externally; it just makes Aider write JSONL to a path we choose.
//!
//! ## Auto-config strategy (zero-setup goal)
//!
//! On startup, if Aider appears installed and the user has NOT already
//! configured `analytics-log:` themselves, we append our entry to
//! `~/.aider.conf.yml`. From that point onward every Aider session
//! writes machine-readable usage events that this provider tails.
//!
//! - If the user HAS set `analytics-log:` already, we respect their
//!   path and watch IT instead — never overwrite user config.
//! - The append is annotated with a `# petpet-managed:` comment line
//!   so the user can see what we did and remove it any time.
//! - If Aider isn't installed (no `~/.aider/`, no `~/.aider.conf.yml`,
//!   no binary on PATH), we skip the auto-config and the provider's
//!   watcher becomes a no-op. No file is created speculatively.
//!
//! ## On-disk schema (locked in from Aider source code)
//!
//! Each line is a `{event, properties, user_id, time}` object. We only
//! emit on `event == "message_send"` whose `properties` carry:
//!
//! ```json
//! {
//!   "event": "message_send",
//!   "properties": {
//!     "main_model":       "<redacted model name>",
//!     "weak_model":       "<redacted>",
//!     "editor_model":     "<redacted>",
//!     "edit_format":      "diff-fenced",
//!     "prompt_tokens":    11364,
//!     "completion_tokens":7,
//!     "total_tokens":     11371,
//!     "cost":             0.00011644,
//!     "total_cost":       0.00011644
//!   },
//!   "user_id": "c42c4e6b-…",
//!   "time": 1754761392
//! }
//! ```
//!
//! Aider's analytics has **no cache_read / cache_creation / reasoning
//! token breakdown** — only prompt + completion. We map those to
//! `TokenDelta.input` / `TokenDelta.output` and leave the rest at 0.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::db::DbHandle;
use crate::event::{EventKind, ProviderId, SourceRef, TokenDelta, UsageEvent};
use crate::hooks::ActivitySink;
use crate::provider::jsonl_watcher::{JsonlReader, JsonlWatcher, ParseOutput};
use crate::provider::{BackfillStats, EventSink, Provider};

pub struct AiderProvider {
    /// `None` when auto-config decided not to enable anything (Aider
    /// not installed, or config write failed). The Provider trait
    /// methods short-circuit when this is `None` so we don't pretend
    /// to watch a path that doesn't exist.
    watcher: Option<JsonlWatcher>,
}

impl AiderProvider {
    pub fn new(db: Arc<DbHandle>) -> Self {
        let watcher = match config::ensure_analytics_log_configured() {
            Ok(Some(log_path)) => Some(build_watcher(log_path, db)),
            Ok(None) => {
                tracing::debug!("aider not installed — provider inactive");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "aider auto-config failed; provider inactive");
                None
            }
        };
        Self { watcher }
    }
}

fn build_watcher(log_path: PathBuf, db: Arc<DbHandle>) -> JsonlWatcher {
    // JsonlWatcher discovers via `<root>/<glob>` and filters by `.jsonl`
    // extension. We point root at the log file's parent directory and
    // glob at its exact filename so only the one file we care about
    // gets watched, even if Aider one day writes neighbours.
    let parent = log_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let filename: &'static str = match log_path.file_name().and_then(|s| s.to_str()) {
        // Almost always our default name; if the user pointed Aider at
        // a custom filename we still watch their directory but match
        // any `.jsonl` there (matches_glob also enforces extension).
        Some(name) if name == config::PETPET_LOG_FILENAME => config::PETPET_LOG_FILENAME,
        _ => "*.jsonl",
    };
    JsonlWatcher::new(
        ProviderId::Aider,
        vec![parent],
        filename,
        db,
        Arc::new(|_path| Box::new(AiderLineParser) as Box<dyn JsonlReader>),
    )
}

#[async_trait]
impl Provider for AiderProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Aider
    }

    fn display_name(&self) -> &'static str {
        "Aider"
    }

    async fn backfill(&self, sink: &EventSink) -> Result<BackfillStats> {
        match &self.watcher {
            Some(w) => w.backfill(sink).await,
            None => Ok(BackfillStats::default()),
        }
    }

    async fn watch(
        &self,
        sink: &EventSink,
        activity_sink: &ActivitySink,
        shutdown: CancellationToken,
    ) -> Result<()> {
        match &self.watcher {
            Some(w) => w.watch(sink, activity_sink, shutdown).await,
            None => {
                // No-op until shutdown so the provider task doesn't
                // exit immediately and trigger spurious restart logic
                // upstream.
                shutdown.cancelled().await;
                Ok(())
            }
        }
    }
}

// ─── Per-line parser ──────────────────────────────────────────────

struct AiderLineParser;

impl JsonlReader for AiderLineParser {
    fn parse_line(&mut self, line: &str, source: SourceRef) -> ParseOutput {
        let entry: AiderLogEntry = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return ParseOutput::default(),
        };
        // Aider emits MANY event types (launched, exit, model_warning,
        // auto_commits, message_send_starting, message_send_exception,
        // …) — only `message_send` carries the post-call token counts
        // we want for XP accounting.
        if entry.event != MESSAGE_SEND_EVENT {
            return ParseOutput::default();
        }
        let prompt = entry.properties.prompt_tokens.unwrap_or(0).max(0) as u64;
        let completion = entry.properties.completion_tokens.unwrap_or(0).max(0) as u64;
        let tokens = TokenDelta {
            input: prompt,
            output: completion,
            cache_read: 0,
            cache_creation: 0,
            reasoning: 0,
        };
        if tokens.is_zero() {
            return ParseOutput::default();
        }

        // Aider's `time` is Unix seconds (analytics.py emits
        // `int(time.time())`). DateTime::from_timestamp accepts that
        // shape directly. Fallback to "now" if for some reason the
        // value is out of range (would only happen on extreme clock
        // skew; not worth dropping the event).
        let timestamp = DateTime::<Utc>::from_timestamp(entry.time, 0).unwrap_or_else(Utc::now);

        // Aider's log has no per-session id and no per-project path.
        // We synthesize a session id from the stable user-id so the
        // UsageEvent invariant (`session_id: String`, non-optional)
        // is satisfied and downstream queries can still group by
        // "everything from this Aider install".
        let session_id = format!("aider-user-{}", entry.user_id);

        let model = entry
            .properties
            .main_model
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "aider/unknown".to_string());

        let id = UsageEvent::deterministic_id(ProviderId::Aider, &source);

        let mut out = ParseOutput::default();
        out.usage.push(UsageEvent {
            id,
            provider: ProviderId::Aider,
            client: Some("cli".to_string()),
            session_id,
            project_path: None,
            git_branch: None,
            model,
            timestamp,
            tokens,
            // Aider's edit_format isn't a stop reason in the
            // Claude-Code sense; closest match is Turn with no stop.
            kind: EventKind::Turn { stop_reason: None },
            source,
        });
        out
    }
}

const MESSAGE_SEND_EVENT: &str = "message_send";

#[derive(Deserialize)]
struct AiderLogEntry {
    event: String,
    #[serde(default)]
    properties: AiderProperties,
    user_id: String,
    time: i64,
}

#[derive(Default, Deserialize)]
struct AiderProperties {
    #[serde(default)]
    main_model: Option<String>,
    #[serde(default)]
    prompt_tokens: Option<i64>,
    #[serde(default)]
    completion_tokens: Option<i64>,
    // Aider also reports `cost` / `total_cost` per message_send; we
    // don't currently surface USD cost in UsageEvent. Pulling them
    // here is cheap and lets us add the field later non-breakingly.
    #[allow(dead_code)]
    #[serde(default)]
    cost: Option<f64>,
    #[allow(dead_code)]
    #[serde(default)]
    edit_format: Option<String>,
}

// ─── Auto-config of ~/.aider.conf.yml ─────────────────────────────

mod config {
    //! Idempotent installer for `analytics-log:` in `~/.aider.conf.yml`.
    //!
    //! Decision matrix:
    //!
    //!   Aider absent             →  do nothing, return Ok(None)
    //!   Aider present, no setting →  append our line,   return Ok(our_path)
    //!   Aider present, user set   →  leave alone,       return Ok(user_path)
    //!
    //! The "leave alone" branch is critical — a user who already
    //! configured analytics-log themselves (for their own tooling or
    //! analytics pipeline) must not have that overwritten.

    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    use crate::paths;

    pub const PETPET_LOG_FILENAME: &str = "events.jsonl";
    const CONFIG_FILENAME: &str = ".aider.conf.yml";
    const ANALYTICS_LOG_KEY: &str = "analytics-log";
    const PETPET_MARKER_LINE: &str =
        "# petpet-managed: writes Aider usage events to the path below for local tracking.\n\
         # Remove this line + the analytics-log line below to disable.";

    pub fn ensure_analytics_log_configured() -> Result<Option<PathBuf>> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return Ok(None),
        };
        let config_path = home.join(CONFIG_FILENAME);
        let aider_state_dir = home.join(".aider");

        // Detection: only meddle if Aider has actually been used or
        // installed. We don't want petpet creating ~/.aider.conf.yml
        // out of thin air on machines that don't have Aider.
        let aider_installed = config_path.exists()
            || aider_state_dir.exists()
            || aider_binary_in_path();
        if !aider_installed {
            return Ok(None);
        }

        let our_log = petpet_log_path();
        // Always create the directory we'll watch — both for the
        // "we wrote the config" branch (Aider needs the dir to exist)
        // and the "user has it set" branch (so the watcher's
        // discover_files succeeds before Aider has run for the first
        // time).
        if let Some(parent) = our_log.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
        if let Some(user_path) = extract_yaml_string(&existing, ANALYTICS_LOG_KEY) {
            tracing::info!(
                path = %user_path,
                "aider analytics-log already configured by user; watching their path"
            );
            return Ok(Some(PathBuf::from(user_path)));
        }

        // No existing setting → append ours, preserving any prior
        // content verbatim. We append rather than rewrite so future
        // user edits to other keys are untouched.
        let to_append = build_append_block(&our_log);
        let new_content = match existing.as_str() {
            "" => format!(
                "# Aider config — managed in part by petpet (see comment below)\n{}",
                to_append.trim_start()
            ),
            s if s.ends_with('\n') => format!("{s}{to_append}"),
            s => format!("{s}\n{to_append}"),
        };
        std::fs::write(&config_path, new_content)
            .with_context(|| format!("writing {}", config_path.display()))?;

        tracing::info!(
            log = %our_log.display(),
            config = %config_path.display(),
            "aider analytics-log auto-configured"
        );
        Ok(Some(our_log))
    }

    pub fn petpet_log_path() -> PathBuf {
        paths::app_dir()
            .join("providers")
            .join("aider")
            .join(PETPET_LOG_FILENAME)
    }

    fn build_append_block(log_path: &Path) -> String {
        format!(
            "\n{}\n{}: {}\n",
            PETPET_MARKER_LINE,
            ANALYTICS_LOG_KEY,
            log_path.display(),
        )
    }

    fn aider_binary_in_path() -> bool {
        // PATH lookup — cross-platform via `which` crate would be
        // ideal but adding a dep for a single use isn't worth it.
        // Splitting PATH by `:`/`;` ourselves is fine.
        let path_var = match std::env::var_os("PATH") {
            Some(p) => p,
            None => return false,
        };
        for dir in std::env::split_paths(&path_var) {
            for candidate in ["aider", "aider.exe"] {
                if dir.join(candidate).exists() {
                    return true;
                }
            }
        }
        false
    }

    /// Naive YAML scanner — finds the first uncommented top-level
    /// `<key>: <value>` line and returns the unquoted value. We
    /// deliberately do NOT pull in a full YAML parser for one key:
    ///
    /// - Aider's config file is human-edited and flat (one key per line)
    /// - A full parser would add ~50KB to the binary
    /// - False-negatives (failing to detect an existing setting) are
    ///   acceptable: we'd append a duplicate, Aider takes the last one
    ///   anyway, no data is lost
    /// - False-positives (treating a comment as a value) are avoided
    ///   by the explicit `#` skip
    pub(super) fn extract_yaml_string(content: &str, key: &str) -> Option<String> {
        let prefix = format!("{}:", key);
        for raw in content.lines() {
            let trimmed = raw.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix(&prefix) {
                let v = rest.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SourceRef;

    fn srcref() -> SourceRef {
        SourceRef {
            file: "test.jsonl".to_string(),
            byte_offset: 0,
            line: 0,
        }
    }

    /// Canonical message_send line straight from Aider's published
    /// sample-analytics.jsonl. Tests below pin the parser against
    /// this real shape so a future format change shows up here
    /// before it shows up in production.
    const CANONICAL_MESSAGE_SEND: &str = r#"{"event":"message_send","properties":{"main_model":"gemini/gemini-2.5-flash-lite-preview-06-17","weak_model":"gemini/gemini-2.5-flash","editor_model":"gemini/gemini-2.5-flash-lite-preview-06-17","edit_format":"diff-fenced","prompt_tokens":11364,"completion_tokens":7,"total_tokens":11371,"cost":0.00011644,"total_cost":0.00011644},"user_id":"c42c4e6b-f054-44d7-ae1f-6726cc41da88","time":1754761392}"#;

    #[test]
    fn parses_canonical_message_send_event() {
        let mut p = AiderLineParser;
        let out = p.parse_line(CANONICAL_MESSAGE_SEND, srcref());
        assert_eq!(out.usage.len(), 1, "exactly one usage event expected");
        let ev = &out.usage[0];
        assert_eq!(ev.provider, ProviderId::Aider);
        assert_eq!(ev.tokens.input, 11364, "prompt_tokens → input");
        assert_eq!(ev.tokens.output, 7, "completion_tokens → output");
        // Aider doesn't break down cache/reasoning — must be 0.
        assert_eq!(ev.tokens.cache_read, 0);
        assert_eq!(ev.tokens.cache_creation, 0);
        assert_eq!(ev.tokens.reasoning, 0);
        assert!(ev.model.contains("gemini"), "main_model passed through");
        assert!(
            ev.session_id.starts_with("aider-user-"),
            "session_id derived from user_id"
        );
        assert_eq!(ev.timestamp.timestamp(), 1754761392, "time → DateTime preserved");
    }

    #[test]
    fn ignores_non_message_send_events() {
        let mut p = AiderLineParser;
        // launched, exit, no-repo, auto_commits, model warning,
        // message_send_starting, message_send_exception — none of
        // these carry post-call token counts. Pin a few here.
        let cases = [
            r#"{"event":"launched","properties":{},"user_id":"x","time":1}"#,
            r#"{"event":"exit","properties":{"reason":"Unknown edit format"},"user_id":"x","time":1}"#,
            r#"{"event":"message_send_starting","properties":{},"user_id":"x","time":1}"#,
            r#"{"event":"model warning","properties":{"main_model":"None"},"user_id":"x","time":1}"#,
        ];
        for c in cases {
            let out = p.parse_line(c, srcref());
            assert!(out.usage.is_empty(), "non-message_send line emitted usage: {c}");
        }
    }

    #[test]
    fn drops_message_send_with_zero_tokens() {
        // Defensive: if Aider ever logs a message_send with 0/0
        // (shouldn't normally happen) we drop it so the pet doesn't
        // animate a no-op turn.
        let mut p = AiderLineParser;
        let line = r#"{"event":"message_send","properties":{"main_model":"x","prompt_tokens":0,"completion_tokens":0,"total_tokens":0,"cost":0.0,"total_cost":0.0},"user_id":"x","time":1}"#;
        let out = p.parse_line(line, srcref());
        assert!(out.usage.is_empty());
    }

    #[test]
    fn malformed_json_is_silently_skipped() {
        // Per the JSONL convention used everywhere else in petpet:
        // one bad line never poisons the stream, the watcher keeps
        // tailing past it.
        let mut p = AiderLineParser;
        for junk in ["", "not json", "{", "{\"event\":}", "{\"event\":\"x\""] {
            let out = p.parse_line(junk, srcref());
            assert!(out.usage.is_empty());
            assert!(out.activity.is_empty());
        }
    }

    // ─── config::extract_yaml_string ────────────────────────────────

    #[test]
    fn yaml_extractor_finds_unquoted_value() {
        let cfg = "model: gpt-4\nanalytics-log: /tmp/x.jsonl\nverbose: true\n";
        let got = config::extract_yaml_string(cfg, "analytics-log");
        assert_eq!(got.as_deref(), Some("/tmp/x.jsonl"));
    }

    #[test]
    fn yaml_extractor_finds_quoted_value() {
        for cfg in [
            "analytics-log: \"/tmp/x.jsonl\"",
            "analytics-log: '/tmp/x.jsonl'",
        ] {
            let got = config::extract_yaml_string(cfg, "analytics-log");
            assert_eq!(got.as_deref(), Some("/tmp/x.jsonl"), "input: {cfg}");
        }
    }

    #[test]
    fn yaml_extractor_skips_commented_lines() {
        // A line starting with # is disabled — we must NOT pick it up,
        // otherwise we'd refuse to install our own line and the user
        // would get no Aider tracking.
        let cfg = "# analytics-log: /old/disabled.jsonl\nmodel: gpt-4\n";
        let got = config::extract_yaml_string(cfg, "analytics-log");
        assert_eq!(got, None);
    }

    #[test]
    fn yaml_extractor_returns_none_when_absent() {
        let cfg = "model: gpt-4\nverbose: true\n";
        let got = config::extract_yaml_string(cfg, "analytics-log");
        assert_eq!(got, None);
    }
}
