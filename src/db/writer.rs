//! Drains the event mpsc channel into SQLite.
//!
//! One task owns all writes. Providers (potentially many) push through the
//! same `EventSink` (mpsc::Sender). The writer batches inserts in a single
//! transaction every `BATCH_FLUSH_MS` or once `BATCH_MAX` events are queued,
//! which keeps SQLite fsyncs amortised even during backfill bursts.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::DbHandle;
use crate::event::UsageEvent;

const BATCH_MAX: usize = 256;
const BATCH_FLUSH_MS: u64 = 200;
const HEARTBEAT_INTERVAL_SECS: u64 = 30;

pub fn spawn_writer(
    db: Arc<DbHandle>,
    mut rx: mpsc::Receiver<UsageEvent>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<Result<WriterStats>> {
    tokio::spawn(async move {
        let mut stats = WriterStats::default();
        let mut batch: Vec<UsageEvent> = Vec::with_capacity(BATCH_MAX);
        let flush_dur = Duration::from_millis(BATCH_FLUSH_MS);

        // Initial heartbeat marks "petpet is alive starting now". Strict
        // mode at next startup compares (now - last_alive) to decide
        // whether to snap cursors forward or drain normally.
        let _ = db.touch_heartbeat().await;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    drain(&mut rx, &mut batch, &mut stats, &db).await;
                    flush(&db, &mut batch, &mut stats).await;
                    return Ok(stats);
                }
                maybe = rx.recv() => match maybe {
                    Some(ev) => {
                        batch.push(ev);
                        if batch.len() >= BATCH_MAX {
                            flush(&db, &mut batch, &mut stats).await;
                        }
                    }
                    None => {
                        flush(&db, &mut batch, &mut stats).await;
                        return Ok(stats);
                    }
                },
                _ = tokio::time::sleep(flush_dur), if !batch.is_empty() => {
                    flush(&db, &mut batch, &mut stats).await;
                }
                _ = heartbeat.tick() => {
                    if let Err(e) = db.touch_heartbeat().await {
                        tracing::warn!(error = %e, "heartbeat write failed");
                    }
                }
            }
        }
    })
}

async fn drain(
    rx: &mut mpsc::Receiver<UsageEvent>,
    batch: &mut Vec<UsageEvent>,
    stats: &mut WriterStats,
    db: &Arc<DbHandle>,
) {
    while let Ok(ev) = rx.try_recv() {
        batch.push(ev);
        if batch.len() >= BATCH_MAX {
            flush(db, batch, stats).await;
        }
    }
}

async fn flush(db: &Arc<DbHandle>, batch: &mut Vec<UsageEvent>, stats: &mut WriterStats) {
    if batch.is_empty() {
        return;
    }
    for ev in batch.drain(..) {
        match db.insert_event(&ev).await {
            Ok(true) => stats.inserted += 1,
            Ok(false) => stats.deduped += 1,
            Err(e) => {
                tracing::warn!(error = %e, "insert_event failed");
                stats.failed += 1;
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct WriterStats {
    pub inserted: u64,
    pub deduped: u64,
    pub failed: u64,
}
