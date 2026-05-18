//! OpenCode token provider — Layer 2 ingestion from OpenCode's local SQLite.
//!
//! OpenCode doesn't write JSONL sessions like Claude Code / Codex do.
//! Instead it keeps `~/.local/share/opencode/opencode.db` (SQLite, WAL mode).
//! The `message` table stores per-turn assistant rows whose `data` column
//! is a JSON blob carrying token counts in the **same 5-axis shape** we
//! already normalize (`input` / `output` / `reasoning` / `cache.read` /
//! `cache.write`).
//!
//! Sample row JSON:
//! ```json
//! {
//!   "role": "assistant",
//!   "modelID": "qwen3.6-plus-free",
//!   "providerID": "opencode",
//!   "tokens": { "input": 6, "output": 89, "reasoning": 0,
//!               "cache": { "write": 27105, "read": 0 } },
//!   "time": { "created": 1778801220615, "completed": 1778801228700 },
//!   "finish": "stop"
//! }
//! ```
//!
//! We tail the table by `message.time_created` (epoch-millis). Cursor lives
//! in our own `file_cursor` table reusing `byte_offset` as the time
//! threshold — same persistence path as the JSONL providers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use rusqlite::{params, Connection, OpenFlags};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::{Cursor, CursorKind, DbHandle};
use crate::event::{EventKind, ProviderId, SourceRef, TokenDelta, UsageEvent};
use crate::hooks::ActivitySink;
use crate::paths;
use crate::provider::{BackfillStats, EventSink, Provider};

pub struct OpenCodeProvider {
    db: Arc<DbHandle>,
    opencode_db_path: PathBuf,
}

impl OpenCodeProvider {
    pub fn new(db: Arc<DbHandle>) -> Self {
        let opencode_db_path = paths::opencode_db_path().unwrap_or_default();
        Self { db, opencode_db_path }
    }

    fn cursor_key(&self) -> String {
        self.opencode_db_path.to_string_lossy().to_string()
    }

    /// One pass: read messages past our cursor, emit, advance cursor.
    ///
    /// **Cursor by `time_updated`, not `time_created`.** OpenCode inserts
    /// an empty assistant row at turn start and UPDATEs it 10-30s later
    /// with tokens once the model finishes. If we cursored on
    /// `time_created`, a polling tick that lands between the INSERT and
    /// UPDATE would mark the row "seen", advance past it, and never come
    /// back when tokens arrive. `time_updated` bumps on the UPDATE so the
    /// row re-surfaces on the next pass — and we naturally pick up the
    /// finalised token counts.
    async fn drain(&self, sink: &EventSink) -> Result<BackfillStats> {
        let started = Instant::now();
        if !self.opencode_db_path.exists() {
            return Ok(BackfillStats::default());
        }
        let key = self.cursor_key();
        let prior = self.db.get_cursor(ProviderId::OpenCode, &key, CursorKind::Live).await?;
        let prior_ms: i64 = match prior {
            Some(c) => c.byte_offset as i64,
            None => {
                let max_ms = self.read_max_time_updated().unwrap_or(0);
                self.db
                    .set_cursor(
                        ProviderId::OpenCode,
                        &key,
                        CursorKind::Live,
                        Cursor { byte_offset: max_ms as u64, line_index: 0 },
                    )
                    .await?;
                tracing::debug!(
                    provider = %ProviderId::OpenCode,
                    max_ms,
                    "first-seen opencode.db — cursor snapped to current max(message.time_updated)"
                );
                return Ok(BackfillStats { files_scanned: 1, ..Default::default() });
            }
        };

        let rows = self.read_messages_after(prior_ms)?;
        let mut stats = BackfillStats { files_scanned: 1, ..Default::default() };
        let mut latest_ms = prior_ms;
        for row in rows {
            stats.lines_scanned += 1;
            latest_ms = latest_ms.max(row.time_updated);
            if let Some(ev) = row.to_usage_event(&self.opencode_db_path) {
                sink.emit(ev).await;
                stats.events_emitted += 1;
            }
        }
        if latest_ms > prior_ms {
            self.db
                .set_cursor(
                    ProviderId::OpenCode,
                    &key,
                    CursorKind::Live,
                    Cursor { byte_offset: latest_ms as u64, line_index: 0 },
                )
                .await?;
        }
        stats.duration = started.elapsed();
        if stats.events_emitted > 0 {
            tracing::info!(
                provider = %ProviderId::OpenCode,
                events = stats.events_emitted,
                elapsed_ms = stats.duration.as_millis() as u64,
                "drained opencode messages"
            );
        }
        Ok(stats)
    }

    /// Strict-mode startup snap, gated by heartbeat (same rule as
    /// `JsonlWatcher`). Only snaps forward when we've actually been gone
    /// for `STRICT_SNAP_THRESHOLD_SECS`+ — brief restarts let the normal
    /// drain path catch up.
    async fn snap_cursor_to_max(&self) -> Result<()> {
        const STRICT_SNAP_THRESHOLD_SECS: i64 = 120;
        if !self.opencode_db_path.exists() {
            return Ok(());
        }
        let key = self.cursor_key();
        let prior = self.db.get_cursor(ProviderId::OpenCode, &key, CursorKind::Live).await?;
        let Some(prior) = prior else { return Ok(()) };

        let age = self.db.heartbeat_age_secs().await.unwrap_or(None);
        match age {
            None => return Ok(()),
            Some(s) if s < STRICT_SNAP_THRESHOLD_SECS => {
                tracing::debug!(
                    provider = %ProviderId::OpenCode,
                    heartbeat_age_s = s,
                    "skipping strict snap (recent heartbeat — brief restart)"
                );
                return Ok(());
            }
            _ => {}
        }

        let max_ms = self.read_max_time_updated().unwrap_or(0);
        if (max_ms as u64) > prior.byte_offset {
            self.db
                .set_cursor(
                    ProviderId::OpenCode,
                    &key,
                    CursorKind::Live,
                    Cursor { byte_offset: max_ms as u64, line_index: 0 },
                )
                .await?;
            tracing::info!(
                provider = %ProviderId::OpenCode,
                from = prior.byte_offset,
                to = max_ms,
                heartbeat_age_s = age.unwrap_or(-1),
                "strict mode: snapped opencode cursor forward to current max(time_updated)"
            );
        }
        Ok(())
    }

    fn read_max_time_updated(&self) -> Result<i64> {
        let conn = self.open_ro()?;
        let max: Option<i64> = conn
            .query_row("SELECT MAX(time_updated) FROM message", [], |r| r.get(0))
            .ok();
        Ok(max.unwrap_or(0))
    }

    fn read_messages_after(&self, cursor_ms: i64) -> Result<Vec<OpenCodeRow>> {
        let conn = self.open_ro()?;
        let mut stmt = conn.prepare(
            "SELECT m.id,
                    m.session_id,
                    m.time_created,
                    m.time_updated,
                    m.data,
                    s.directory
             FROM message m
             LEFT JOIN session s ON s.id = m.session_id
             WHERE m.time_updated > ?
             ORDER BY m.time_updated ASC",
        )?;
        let rows = stmt
            .query_map(params![cursor_ms], |r| {
                Ok(OpenCodeRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    time_created: r.get(2)?,
                    time_updated: r.get(3)?,
                    data: r.get(4)?,
                    directory: r.get(5).ok(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn open_ro(&self) -> Result<Connection> {
        Connection::open_with_flags(
            &self.opencode_db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open RO {}", self.opencode_db_path.display()))
    }
}

#[async_trait]
impl Provider for OpenCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCode
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    async fn backfill(&self, sink: &EventSink) -> Result<BackfillStats> {
        self.drain(sink).await
    }

    async fn import_historical(&self, sink: &EventSink) -> Result<BackfillStats> {
        // OpenCode's `drain` reads a single JSON state file (not a
        // line-oriented JSONL log), so there's no historical-vs-live
        // distinction to make per-line. The state file always reflects
        // the union of all sessions OpenCode has ever recorded, so
        // calling `drain` covers the same ground as `import_historical`
        // semantically. XP isolation comes from the SINK the caller
        // passes — `lib.rs::spawn_ingestion` routes history through a
        // DB-only sink, so this stays display-only by construction.
        self.drain(sink).await
    }

    async fn watch(
        &self,
        sink: &EventSink,
        _activity_sink: &ActivitySink,
        shutdown: CancellationToken,
    ) -> Result<()> {
        // OpenCode's primary activity signal is its hook plugin (Layer 1).
        // Deriving activities from the SQLite tail is plausible but lower
        // priority — the plugin already covers ToolUseStart/End/SessionEnd
        // in real time with no restart needed (plugin is auto-loaded by
        // OpenCode on launch). We accept `_activity_sink` so the trait
        // stays uniform and a future enhancement can wire it.

        // Strict mode: snap cursor forward to current max(time_updated) so
        // any messages OpenCode wrote while petpet was closed don't count
        // toward the pet.
        self.snap_cursor_to_max().await?;
        let _ = self.drain(sink).await?;

        let Some(watch_dir) = self.opencode_db_path.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        if !watch_dir.exists() {
            tracing::debug!(provider = %ProviderId::OpenCode, "opencode dir missing — watcher idle");
            shutdown.cancelled().await;
            return Ok(());
        }

        // Hybrid trigger: notify-debouncer-full for low-latency reaction
        // AND a 2-second polling tick as a SQLite-WAL safety net.
        //
        // macOS FSEvents doesn't reliably surface modifications to
        // `opencode.db-wal` (the WAL file SQLite actually writes to in
        // WAL mode). Without the polling fallback we miss whole
        // conversations: notify never fires, drain never runs.
        // Polling every 2s costs a one-row `SELECT MAX` + a cursor
        // compare; cheap enough to leave on always.
        let (tx, mut rx) = mpsc::unbounded_channel::<&'static str>();
        let notify_tx = tx.clone();
        let _debouncer = match new_debouncer(
            Duration::from_millis(500),
            None,
            move |res: DebounceEventResult| {
                if res.is_ok() {
                    let _ = notify_tx.send("notify");
                }
            },
        ) {
            Ok(mut d) => match d.watcher().watch(&watch_dir, RecursiveMode::NonRecursive) {
                Ok(_) => Some(d),
                Err(e) => {
                    tracing::warn!(
                        provider = %ProviderId::OpenCode,
                        error = %e,
                        "fs notify watch failed — falling back to pure polling"
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(
                    provider = %ProviderId::OpenCode,
                    error = %e,
                    "fs notify init failed — falling back to pure polling"
                );
                None
            }
        };

        let mut poll = tokio::time::interval(Duration::from_secs(2));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                Some(_) = rx.recv() => {
                    if let Err(e) = self.drain(sink).await {
                        tracing::warn!(provider = %ProviderId::OpenCode, error = %e, "drain failed (notify)");
                    }
                }
                _ = poll.tick() => {
                    if let Err(e) = self.drain(sink).await {
                        tracing::warn!(provider = %ProviderId::OpenCode, error = %e, "drain failed (poll)");
                    }
                }
            }
        }
    }
}

// ── DB row + JSON parsing ────────────────────────────────────────────────

struct OpenCodeRow {
    id: String,
    session_id: String,
    time_created: i64, // epoch ms — used for event timestamp display
    time_updated: i64, // epoch ms — used for cursor advancement
    data: String,
    directory: Option<String>,
}

#[derive(Deserialize)]
struct MessageData {
    role: Option<String>,
    tokens: Option<MessageTokens>,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
    finish: Option<String>,
}

#[derive(Deserialize)]
struct MessageTokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    reasoning: u64,
    #[serde(default)]
    cache: Option<MessageCache>,
}

#[derive(Deserialize)]
struct MessageCache {
    #[serde(default)]
    read: u64,
    #[serde(default)]
    write: u64,
}

impl OpenCodeRow {
    fn to_usage_event(&self, db_path: &Path) -> Option<UsageEvent> {
        let parsed: MessageData = serde_json::from_str(&self.data).ok()?;
        if parsed.role.as_deref() != Some("assistant") {
            return None;
        }
        let tokens = parsed.tokens?;
        let token_delta = TokenDelta {
            input: tokens.input,
            output: tokens.output,
            cache_read: tokens.cache.as_ref().map(|c| c.read).unwrap_or(0),
            cache_creation: tokens.cache.as_ref().map(|c| c.write).unwrap_or(0),
            reasoning: tokens.reasoning,
        };
        if token_delta.is_zero() {
            return None;
        }
        let model = parsed.model_id.unwrap_or_else(|| "unknown".to_string());
        let timestamp: DateTime<Utc> = Utc
            .timestamp_millis_opt(self.time_created)
            .single()
            .unwrap_or_else(Utc::now);

        // UUID derives from message-id only (byte_offset fixed at 0).
        // OpenCode UPDATEs the same row multiple times during a turn
        // (placeholder → tokens), and our drain may see it more than once.
        // Stable UUID + INSERT OR IGNORE means the first complete emission
        // wins; later sees become no-ops. `time_created` flows through
        // as the visible timestamp, but does NOT affect identity.
        let source = SourceRef {
            file: format!("{}#{}", db_path.display(), self.id),
            byte_offset: 0,
            line: 0,
        };
        let id = UsageEvent::deterministic_id(ProviderId::OpenCode, &source);

        Some(UsageEvent {
            id,
            provider: ProviderId::OpenCode,
            // OpenCode's `providerID` field describes the upstream model
            // service (e.g. "opencode", "anthropic"). Stash as our `client`
            // tag so the stats UI can distinguish gateway routing.
            client: parsed.provider_id,
            session_id: self.session_id.clone(),
            project_path: self.directory.clone(),
            git_branch: None,
            model,
            timestamp,
            tokens: token_delta,
            kind: EventKind::Turn { stop_reason: parsed.finish },
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_row_emits_usage_event_with_5_axis_tokens() {
        let row = OpenCodeRow {
            id: "msg-1".into(),
            session_id: "ses-1".into(),
            time_created: 1_778_801_220_615,
            time_updated: 1_778_801_240_615,
            directory: Some("/Users/mars/work".into()),
            data: r#"{
                "role": "assistant",
                "modelID": "qwen3.6-plus-free",
                "providerID": "opencode",
                "tokens": {"input":6,"output":89,"reasoning":0,"cache":{"write":27105,"read":12}},
                "finish": "stop"
            }"#
            .into(),
        };
        let ev = row.to_usage_event(Path::new("/tmp/opencode.db")).unwrap();
        assert_eq!(ev.provider, ProviderId::OpenCode);
        assert_eq!(ev.session_id, "ses-1");
        assert_eq!(ev.project_path.as_deref(), Some("/Users/mars/work"));
        assert_eq!(ev.model, "qwen3.6-plus-free");
        assert_eq!(ev.client.as_deref(), Some("opencode"));
        assert_eq!(ev.tokens.input, 6);
        assert_eq!(ev.tokens.output, 89);
        assert_eq!(ev.tokens.cache_read, 12);
        assert_eq!(ev.tokens.cache_creation, 27105);
        assert_eq!(ev.tokens.reasoning, 0);
        assert!(matches!(ev.kind, EventKind::Turn { ref stop_reason } if stop_reason.as_deref() == Some("stop")));
    }

    #[test]
    fn non_assistant_row_returns_none() {
        let row = OpenCodeRow {
            id: "msg-2".into(),
            session_id: "ses-1".into(),
            time_created: 0,
            time_updated: 0,
            directory: None,
            data: r#"{"role":"user","content":"hi"}"#.into(),
        };
        assert!(row.to_usage_event(Path::new("/tmp/opencode.db")).is_none());
    }

    #[test]
    fn zero_tokens_returns_none() {
        let row = OpenCodeRow {
            id: "msg-3".into(),
            session_id: "ses-1".into(),
            time_created: 0,
            time_updated: 0,
            directory: None,
            data: r#"{"role":"assistant","modelID":"x","tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#.into(),
        };
        assert!(row.to_usage_event(Path::new("/tmp/opencode.db")).is_none());
    }

    #[test]
    fn missing_cache_block_is_safe() {
        let row = OpenCodeRow {
            id: "msg-4".into(),
            session_id: "ses-1".into(),
            time_created: 1,
            time_updated: 1,
            directory: None,
            data: r#"{"role":"assistant","modelID":"x","tokens":{"input":1,"output":2,"reasoning":3}}"#.into(),
        };
        let ev = row.to_usage_event(Path::new("/tmp/opencode.db")).unwrap();
        assert_eq!(ev.tokens.cache_read, 0);
        assert_eq!(ev.tokens.cache_creation, 0);
        assert_eq!(ev.tokens.reasoning, 3);
    }
}
