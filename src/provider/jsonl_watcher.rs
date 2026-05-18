//! Generic JSONL tail-watcher shared by every file-based provider.
//!
//! Pattern of use (see `provider/claude.rs`):
//!
//! ```ignore
//! let watcher = JsonlWatcher::new(
//!     ProviderId::ClaudeCode,
//!     vec![claude_projects_root()],
//!     "**/*.jsonl",
//!     db.clone(),
//!     Arc::new(|_path| Box::new(ClaudeLineParser::default()) as Box<dyn JsonlReader>),
//! );
//! watcher.backfill(&sink).await?;
//! watcher.watch(&sink, shutdown).await?;
//! ```
//!
//! Responsibilities:
//! - Expand glob over configured roots → ordered file list.
//! - For each file, resume from cursor (byte offset persisted in SQLite).
//! - Read newly-appended bytes line by line, hand each to a `JsonlReader`.
//! - Push every emitted `UsageEvent` to the sink, then advance the cursor.
//! - In `watch` mode: backfill first, then use `notify` to react to writes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::{Cursor, DbHandle};
use crate::event::{ActivityEvent, ProviderId, SourceRef, UsageEvent};
use crate::hooks::ActivitySink;
use crate::provider::{BackfillStats, EventSink};

/// What a single line yielded after parsing.
///
/// - `usage` flows to SQLite (token accounting, pet growth)
/// - `activity` flows to the Tauri event bus (pet animation, UI reactions)
///
/// Same line CAN produce both — e.g. a Codex `task_complete` event yields
/// an `AssistantStop` activity AND, separately, the next `token_count`
/// event yields a `UsageEvent`. Parsers decide independently.
#[derive(Default)]
pub struct ParseOutput {
    pub usage: Vec<UsageEvent>,
    pub activity: Vec<ActivityEvent>,
}

/// Per-line JSONL parser. Holds whatever per-file state the provider needs
/// (e.g. Codex tracks `current_model` across lines).
pub trait JsonlReader: Send {
    fn parse_line(&mut self, line: &str, source: SourceRef) -> ParseOutput;
}

pub type ReaderFactory = Arc<dyn Fn(&Path) -> Box<dyn JsonlReader> + Send + Sync>;

pub struct JsonlWatcher {
    provider_id: ProviderId,
    roots: Vec<PathBuf>,
    glob: &'static str,
    db: Arc<DbHandle>,
    make_reader: ReaderFactory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmitMode {
    /// Backfill / catchup — emit usage events only. Activity events are
    /// **suppressed** because we'd replay hours of historical animations.
    UsageOnly,
    /// Live watch — emit both usage AND activity. Activity drives the
    /// frontend pet's real-time reactions.
    Live,
}

impl JsonlWatcher {
    pub fn new(
        provider_id: ProviderId,
        roots: Vec<PathBuf>,
        glob: &'static str,
        db: Arc<DbHandle>,
        make_reader: ReaderFactory,
    ) -> Self {
        Self { provider_id, roots, glob, db, make_reader }
    }

    /// One-shot scan: catch up from each file's cursor to EOF.
    /// **Usage-only**: activity events are suppressed during backfill so
    /// the pet doesn't replay hours of historical animations.
    pub async fn backfill(&self, sink: &EventSink) -> Result<BackfillStats> {
        let started = Instant::now();
        let files = self.discover_files()?;
        let mut stats = BackfillStats::default();

        for path in files {
            let per_file = self.drain_file(&path, sink, None, EmitMode::UsageOnly).await?;
            stats += per_file;
        }
        stats.duration = started.elapsed();
        tracing::info!(
            provider = %self.provider_id,
            events = stats.events_emitted,
            files = stats.files_scanned,
            elapsed_ms = stats.duration.as_millis() as u64,
            "backfill complete"
        );
        Ok(stats)
    }

    /// Long-lived watch with **two triggers**:
    ///
    /// 1. **notify** (low latency, gives ~ms reaction to file changes)
    /// 2. **3-second polling tick** as the **mandatory safety net** —
    ///    macOS FSEvents has shown itself unreliable for certain deeply
    ///    nested paths (observed for `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`).
    ///    Without polling, notify silently drops append events and the pet
    ///    misses whole conversations.
    ///
    /// Polling is cheap: per tick we run a glob + `stat()` per file + a
    /// short-circuit if `file_len <= cursor`. For 1361 files this is <10ms.
    ///
    /// Strict mode: at startup we snap known cursors to current EOF so
    /// the pet never gets credit for activity that happened while petpet
    /// was closed. `petpet backfill` (CLI) is the explicit opt-in catchup.
    pub async fn watch(
        &self,
        sink: &EventSink,
        activity_sink: &ActivitySink,
        shutdown: CancellationToken,
    ) -> Result<()> {
        self.snap_known_cursors_to_eof().await?;
        self.backfill(sink).await?;

        let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
        let _debouncer = self.spawn_fs_watcher(tx).ok();
        let mut poll = tokio::time::interval(Duration::from_secs(3));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        poll.tick().await; // skip the immediate first tick — backfill just ran

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!(provider = %self.provider_id, "watch cancelled");
                    return Ok(());
                }
                Some(path) = rx.recv() => {
                    if !self.matches_glob(&path) {
                        continue;
                    }
                    if let Err(e) = self.drain_file(&path, sink, Some(activity_sink), EmitMode::Live).await {
                        tracing::warn!(provider = %self.provider_id, path = %path.display(), error = %e, "drain_file failed (notify)");
                    }
                }
                _ = poll.tick() => {
                    if let Err(e) = self.poll_drain(sink, activity_sink).await {
                        tracing::warn!(provider = %self.provider_id, error = %e, "poll drain failed");
                    }
                }
            }
        }
    }

    /// Polling-tick fallback: walk every discoverable file and let
    /// `drain_file` short-circuit when there's nothing new.
    async fn poll_drain(&self, sink: &EventSink, activity_sink: &ActivitySink) -> Result<()> {
        for path in self.discover_files()? {
            if let Err(e) = self.drain_file(&path, sink, Some(activity_sink), EmitMode::Live).await {
                tracing::trace!(
                    provider = %self.provider_id,
                    path = %path.display(),
                    error = %e,
                    "drain_file failed (poll)"
                );
            }
        }
        Ok(())
    }

    /// Strict-mode helper, GATED BY HEARTBEAT.
    ///
    /// We only snap cursors forward if `last_alive` heartbeat is older
    /// than `STRICT_SNAP_THRESHOLD_SECS` — that's the signal of a real
    /// offline gap. For brief restarts (tauri-dev rebuild, app crash,
    /// kernel sleep < 2min) we DO NOT snap, so notify/polling catches up
    /// any events that were appended while petpet was briefly down.
    ///
    /// This fixes the pathological case where notify-rs missed events,
    /// petpet briefly restarted, and strict-mode snap unfairly skipped
    /// past those events forever.
    async fn snap_known_cursors_to_eof(&self) -> Result<()> {
        const STRICT_SNAP_THRESHOLD_SECS: i64 = 120;

        let age = self.db.heartbeat_age_secs().await.unwrap_or(None);
        match age {
            None => {
                // No prior heartbeat — fresh install. First-seen semantic in
                // drain_file already handles snap-to-EOF for new files.
                return Ok(());
            }
            Some(secs) if secs < STRICT_SNAP_THRESHOLD_SECS => {
                // Brief restart. Let drain_file catch up.
                tracing::debug!(
                    provider = %self.provider_id,
                    heartbeat_age_s = secs,
                    "skipping strict snap (recent heartbeat — treating as brief restart)"
                );
                return Ok(());
            }
            _ => {}
        }

        let files = self.discover_files()?;
        let mut snapped = 0u32;
        for path in files {
            let path_str = path.to_string_lossy().to_string();
            let prior = match self.db.get_cursor(self.provider_id, &path_str).await? {
                Some(c) => c,
                None => continue,
            };
            let file_len = match tokio::fs::metadata(&path).await {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if file_len > prior.byte_offset {
                self.db
                    .set_cursor(
                        self.provider_id,
                        &path_str,
                        Cursor { byte_offset: file_len, line_index: prior.line_index },
                    )
                    .await?;
                snapped += 1;
            }
        }
        if snapped > 0 {
            tracing::info!(
                provider = %self.provider_id,
                snapped,
                heartbeat_age_s = age.unwrap_or(-1),
                "strict mode: skipped offline activity by snapping cursors to current EOF"
            );
        }
        Ok(())
    }

    fn discover_files(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                tracing::debug!(root = %root.display(), "provider root absent — skipping");
                continue;
            }
            let pattern = format!("{}/{}", root.display(), self.glob);
            for entry in glob::glob(&pattern).context("invalid glob")? {
                match entry {
                    Ok(p) if p.is_file() => out.push(p),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "glob entry error"),
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn matches_glob(&self, path: &Path) -> bool {
        for root in &self.roots {
            if let Ok(rel) = path.strip_prefix(root) {
                if rel.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    return true;
                }
            }
        }
        false
    }

    fn spawn_fs_watcher(
        &self,
        tx: mpsc::UnboundedSender<PathBuf>,
    ) -> Result<notify_debouncer_full::Debouncer<notify::RecommendedWatcher, notify_debouncer_full::FileIdMap>> {
        let mut debouncer = new_debouncer(
            Duration::from_millis(250),
            None,
            move |res: DebounceEventResult| match res {
                Ok(events) => {
                    for ev in events {
                        for p in ev.paths.iter() {
                            let _ = tx.send(p.clone());
                        }
                    }
                }
                Err(errs) => {
                    for e in errs {
                        tracing::warn!(error = %e, "notify error");
                    }
                }
            },
        )?;
        for root in &self.roots {
            if root.exists() {
                debouncer
                    .watcher()
                    .watch(root, RecursiveMode::Recursive)
                    .with_context(|| format!("watch root {}", root.display()))?;
            }
        }
        Ok(debouncer)
    }

    /// Read the file, prime per-file parser state by scanning every line,
    /// but only **emit** events for lines whose byte offset is past our
    /// previous cursor. This serves two needs at once:
    ///
    /// 1. **Install-time boundary** — first time we see a file, the prior
    ///    cursor is `None` which we treat as `file_len`: scan everything to
    ///    initialise parser state, emit nothing.
    /// 2. **Stateful parsers** (Codex) — its `session_meta` lives on line 1.
    ///    Subsequent `token_count` lines need `session_id` from that meta.
    ///    Re-scanning from byte 0 guarantees the parser is always primed,
    ///    so events past the cursor get correctly attributed.
    ///
    /// Cost: one full read per notify fire. For typical Claude/Codex session
    /// files (<50 MB) this is well under 100 ms; for the rare 200 MB file,
    /// still under a second. We trade that for stateless watcher code and
    /// no per-file in-memory parser cache.
    async fn drain_file(
        &self,
        path: &Path,
        sink: &EventSink,
        activity_sink: Option<&ActivitySink>,
        mode: EmitMode,
    ) -> Result<BackfillStats> {
        let path_str = path.to_string_lossy().to_string();
        let metadata = tokio::fs::metadata(path).await?;
        let file_len = metadata.len();

        let prior = self.db.get_cursor(self.provider_id, &path_str).await?;
        let first_seen = prior.is_none();

        // Emit threshold: byte offset above which lines are "new" and worth
        // surfacing to the sink. On first-see we set it to `file_len` so
        // pre-existing content gets parsed for state only, not emitted.
        let mut emit_threshold = prior.map(|c| c.byte_offset).unwrap_or(file_len);

        // File rotation / truncation: persisted cursor points past EOF.
        // Reset; deterministic UUIDs keep re-insertion idempotent.
        if let Some(c) = prior {
            if file_len < c.byte_offset {
                tracing::warn!(
                    provider = %self.provider_id,
                    path = %path.display(),
                    file_len,
                    cursor_byte = c.byte_offset,
                    "file is smaller than recorded cursor — resetting and re-scanning from start"
                );
                emit_threshold = 0;
                self.db
                    .set_cursor(self.provider_id, &path_str, Cursor { byte_offset: 0, line_index: 0 })
                    .await?;
            }
        }

        // Subsequent call but nothing new — short circuit BEFORE the read.
        if !first_seen && file_len <= emit_threshold {
            return Ok(BackfillStats { files_scanned: 1, ..Default::default() });
        }

        let mut file = File::open(path).await.with_context(|| format!("open {}", path.display()))?;
        let mut buf = Vec::with_capacity(file_len as usize);
        file.read_to_end(&mut buf).await?;

        let mut reader = (self.make_reader)(path);
        let mut stats = BackfillStats { files_scanned: 1, ..Default::default() };
        let mut line_offset: u64 = 0;
        let mut line_index: u64 = 0;
        let mut last_complete_offset: u64 = 0;

        for raw in buf.split_inclusive(|&b| b == b'\n') {
            let len = raw.len() as u64;
            if !raw.ends_with(b"\n") {
                break; // partial trailing line: re-read next pass
            }
            let line = std::str::from_utf8(&raw[..raw.len() - 1]).unwrap_or("").trim_end_matches('\r');
            line_index += 1;
            if !line.is_empty() {
                let src = SourceRef {
                    file: path_str.clone(),
                    byte_offset: line_offset,
                    line: line_index,
                };
                let parsed = reader.parse_line(line, src);
                if line_offset >= emit_threshold {
                    for ev in parsed.usage {
                        sink.emit(ev).await;
                        stats.events_emitted += 1;
                    }
                    if mode == EmitMode::Live {
                        if let Some(act_sink) = activity_sink {
                            for act in parsed.activity {
                                act_sink.emit(act).await;
                            }
                        }
                    }
                }
            }
            line_offset += len;
            stats.lines_scanned += 1;
            last_complete_offset = line_offset;
        }
        stats.bytes_scanned = last_complete_offset;

        // Persist cursor at the last fully-read line boundary so the next
        // pass picks up cleanly after any partial trailing line.
        self.db
            .set_cursor(
                self.provider_id,
                &path_str,
                Cursor { byte_offset: last_complete_offset, line_index },
            )
            .await?;

        if first_seen {
            tracing::debug!(
                provider = %self.provider_id,
                path = %path.display(),
                file_len,
                lines = line_index,
                "first-seen file — parser state primed, cursor at EOF (install-time boundary)"
            );
        }
        Ok(stats)
    }
}
