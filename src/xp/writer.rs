//! XpEventWriter: appends to `xp_event` with deterministic UUIDs so
//! re-ingestion of the same source ref dedupes cleanly.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::DbHandle;
use crate::xp::types::{XpComputation, XpSourceType};

pub struct XpEventRecord {
    pub id: Uuid,
    pub pet_id: String,
    pub occurred_at: DateTime<Utc>,
    pub source_type: XpSourceType,
    pub source_ref: Option<String>,
    pub xp_delta: i64,
    pub reason: String,
    pub rule_id: String,
    pub origin_device_id: String,
}

/// Flattened plain-text bag used by `DbHandle::insert_xp_event` so the
/// `spawn_blocking` closure can be `Send + 'static` without holding
/// refs. Also the on-the-wire shape for the `xp_events.jsonl` line
/// format in `.petpet` pet archives — `Serialize + Deserialize` so
/// the exporter can serde-dump it directly and the importer can read
/// it line-by-line. Unknown fields are ignored (forward compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XpEventInsert {
    pub id: String,
    pub pet_id: String,
    pub occurred_at: String,
    pub source_type: String,
    #[serde(default)]
    pub source_ref: Option<String>,
    pub xp_delta: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub rule_id: String,
    #[serde(default)]
    pub origin_device_id: String,
}

impl XpEventRecord {
    pub fn clone_for_insert(&self) -> XpEventInsert {
        XpEventInsert {
            id: self.id.to_string(),
            pet_id: self.pet_id.clone(),
            occurred_at: self.occurred_at.to_rfc3339(),
            source_type: self.source_type.as_str().to_string(),
            source_ref: self.source_ref.clone(),
            xp_delta: self.xp_delta,
            reason: self.reason.clone(),
            rule_id: self.rule_id.clone(),
            origin_device_id: self.origin_device_id.clone(),
        }
    }
}

impl XpEventRecord {
    pub fn new(
        pet_id: &str,
        source_type: XpSourceType,
        source_ref: Option<String>,
        comp: &XpComputation,
        occurred_at: DateTime<Utc>,
        origin_device_id: &str,
    ) -> Self {
        // Deterministic UUID so re-import / re-emit dedupes.
        let key = format!(
            "{}|{}|{}",
            pet_id,
            source_type.as_str(),
            source_ref.as_deref().unwrap_or(comp.rule_id.as_str())
        );
        // Namespace UUID (constant fingerprint for petpet xp events)
        const NS: Uuid = Uuid::from_bytes([
            0xeb, 0x9a, 0x47, 0x00, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0xeb, 0x9a, 0xeb, 0x9a,
            0xeb, 0x9a,
        ]);
        let id = Uuid::new_v5(&NS, key.as_bytes());
        Self {
            id,
            pet_id: pet_id.to_string(),
            occurred_at,
            source_type,
            source_ref,
            xp_delta: comp.xp_delta,
            reason: comp.reason.clone(),
            rule_id: comp.rule_id.clone(),
            origin_device_id: origin_device_id.to_string(),
        }
    }
}

pub struct XpEventWriter;

impl XpEventWriter {
    /// Insert; returns `true` if the row was new (inserted), `false` if it
    /// was a duplicate that got IGNORED.
    pub async fn write(db: &DbHandle, rec: &XpEventRecord) -> Result<bool> {
        db.insert_xp_event(rec).await
    }

    /// Replay an event from a pre-built flat insert payload (i.e.
    /// deserialised from a `.petpet` `xp_events.jsonl` line). Used by
    /// the pet importer — preserves the original event's source_type
    /// and source_ref so dedup still works on subsequent re-imports.
    pub async fn replay(db: &DbHandle, rec: &XpEventInsert) -> Result<bool> {
        db.insert_xp_event_raw(rec).await
    }
}
