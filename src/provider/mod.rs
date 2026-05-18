//! Provider trait + EventSink — the contract between data sources and the
//! rest of the system.
//!
//! Every ingestion path (log watcher, hook server, API proxy) implements
//! [`Provider`] and emits normalized [`UsageEvent`]s through an [`EventSink`].
//! Downstream code never imports anything from `provider::claude` or
//! `provider::codex` directly.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{ProviderId, UsageEvent};
use crate::hooks::ActivitySink;

pub mod aider;
pub mod claude;
pub mod codex;
pub mod jsonl_watcher;
pub mod opencode;

/// Clone-able handle that any producer uses to publish events.
/// Backed by an mpsc channel drained by the SQLite writer task.
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::Sender<UsageEvent>,
}

impl EventSink {
    pub fn new(tx: mpsc::Sender<UsageEvent>) -> Self {
        Self { tx }
    }

    /// Best-effort send. If the writer task has died, log and drop.
    pub async fn emit(&self, ev: UsageEvent) {
        if let Err(e) = self.tx.send(ev).await {
            tracing::error!(error = %e, "event sink closed, dropping event");
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct BackfillStats {
    pub events_emitted: u64,
    pub lines_scanned: u64,
    pub files_scanned: u64,
    pub bytes_scanned: u64,
    pub duration: Duration,
}

impl std::ops::AddAssign for BackfillStats {
    fn add_assign(&mut self, rhs: Self) {
        self.events_emitted += rhs.events_emitted;
        self.lines_scanned += rhs.lines_scanned;
        self.files_scanned += rhs.files_scanned;
        self.bytes_scanned += rhs.bytes_scanned;
        self.duration += rhs.duration;
    }
}

/// All ingestion sources implement this. The orchestrator only knows about
/// `dyn Provider` — adding a new platform is one new file + one registration.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;

    /// One-shot scan of all available historical data, respecting any
    /// per-file cursors persisted from previous runs.
    /// **Usage events only** — activity events are suppressed during
    /// backfill (we'd replay historical animations otherwise).
    async fn backfill(&self, sink: &EventSink) -> Result<BackfillStats>;

    /// Run forever, emitting both **usage** events (for SQLite / pet
    /// growth) and **activity** events (for live frontend animation).
    ///
    /// Activity events are the no-restart-needed fallback: when a user
    /// installs petpet mid-session, their existing Claude/Codex/OpenCode
    /// process won't fire hooks until restart. But log streams keep
    /// flowing, and we derive equivalent activities from them — so the
    /// pet animates instantly, hooks-or-no-hooks.
    async fn watch(
        &self,
        sink: &EventSink,
        activity_sink: &ActivitySink,
        shutdown: CancellationToken,
    ) -> Result<()>;
}
