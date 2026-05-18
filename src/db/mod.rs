//! SQLite handle: a single Mutex<Connection> reached through async wrappers.
//!
//! rusqlite is sync; we wrap each call in `spawn_blocking`. Throughput is
//! plenty for ingestion (events arrive at most a few per second from CLIs).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};

use crate::event::{EventKind, ProviderId, UsageEvent};
use crate::xp::types::Pet;
use crate::xp::writer::XpEventRecord;

pub mod writer;

const SCHEMA_TABLES_SQL: &str = include_str!("schema.sql");
const SCHEMA_INDEXES_SQL: &str = include_str!("schema_indexes.sql");

pub struct DbHandle {
    conn: Arc<Mutex<Connection>>,
}

impl DbHandle {
    /// Borrow the shared connection handle. Intended for read-only
    /// query helpers in submodules (e.g. `xp::cost_query`) that need
    /// to run their own SQL without re-implementing the open/parent-
    /// dir/PRAGMA bootstrap. Callers MUST keep the same
    /// `spawn_blocking + blocking_lock` discipline as the methods
    /// here — synchronous rusqlite access from inside the async
    /// runtime is a guaranteed deadlock without that pattern.
    pub fn conn(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    pub async fn open(path: &Path) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("creating db parent dir {}", parent.display())
            })?;
        }
        let path_owned = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let c = Connection::open(&path_owned)
                .with_context(|| format!("opening {}", path_owned.display()))?;
            // Stage 1: tables (no-op if they already exist).
            c.execute_batch(SCHEMA_TABLES_SQL).context("applying schema (tables)")?;
            // Stage 2: column migrations for DBs that pre-date a column.
            ensure_column(&c, "usage_event", "client", "TEXT").context("migrate: client")?;
            ensure_column(&c, "pet", "name_finalized_at", "TEXT")
                .context("migrate: pet.name_finalized_at")?;
            ensure_column(&c, "pet", "template_id", "TEXT NOT NULL DEFAULT 'unknown'")
                .context("migrate: pet.template_id")?;
            ensure_column(&c, "pet", "snapshot_path", "TEXT NOT NULL DEFAULT ''")
                .context("migrate: pet.snapshot_path")?;
            // Drop legacy columns from the pre-snapshot schema. Stale dev
            // DBs still carry `species_id NOT NULL` which blocks inserts
            // (the new code never writes it).
            drop_column_if_present(&c, "pet", "species_id")
                .context("migrate: drop legacy pet.species_id")?;
            // Stage 3: indexes — safe now that every referenced column exists.
            c.execute_batch(SCHEMA_INDEXES_SQL).context("applying schema (indexes)")?;
            Ok(c)
        })
        .await??;
        Ok(Arc::new(Self { conn: Arc::new(Mutex::new(conn)) }))
    }

    /// Returns the persisted cursor for a file, or `None` if we have never
    /// seen it before. The `None` case is meaningful: at first-discovery we
    /// snap the cursor to current EOF so historical data stays in the source
    /// files but never enters our DB. This way the pet starts from zero on
    /// every fresh install, regardless of how much history exists on disk.
    pub async fn get_cursor(&self, provider: ProviderId, file: &str) -> Result<Option<Cursor>> {
        let conn = self.conn.clone();
        let file = file.to_string();
        let cur = tokio::task::spawn_blocking(move || -> Result<Option<Cursor>> {
            let g = conn.blocking_lock();
            let row: Option<(i64, i64)> = g
                .query_row(
                    "SELECT byte_offset, line_index FROM file_cursor WHERE provider = ?1 AND file_path = ?2",
                    params![provider.as_str(), file],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok(row.map(|(b, l)| Cursor { byte_offset: b as u64, line_index: l as u64 }))
        })
        .await??;
        Ok(cur)
    }

    pub async fn set_cursor(
        &self,
        provider: ProviderId,
        file: &str,
        cursor: Cursor,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let file = file.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let g = conn.blocking_lock();
            g.execute(
                "INSERT INTO file_cursor (provider, file_path, byte_offset, line_index, updated_at)
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))
                 ON CONFLICT (provider, file_path) DO UPDATE SET
                    byte_offset = excluded.byte_offset,
                    line_index = excluded.line_index,
                    updated_at = excluded.updated_at",
                params![provider.as_str(), file, cursor.byte_offset as i64, cursor.line_index as i64],
            )?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    pub async fn insert_event(&self, e: &UsageEvent) -> Result<bool> {
        let conn = self.conn.clone();
        let e = e.clone();
        let inserted = tokio::task::spawn_blocking(move || -> Result<bool> {
            let g = conn.blocking_lock();
            let (stop_reason, tool_name, tool_exit_code) = match &e.kind {
                EventKind::Turn { stop_reason } => (stop_reason.clone(), None, None),
                EventKind::ToolCall { name } => (None, Some(name.clone()), None),
                EventKind::ToolResult { name, exit_code } => {
                    (None, Some(name.clone()), *exit_code)
                }
                _ => (None, None, None),
            };
            let n = g.execute(
                "INSERT OR IGNORE INTO usage_event
                 (id, provider, client, session_id, project_path, git_branch, model, timestamp, kind,
                  stop_reason, tool_name, tool_exit_code,
                  tokens_input, tokens_output, tokens_cache_read, tokens_cache_creation, tokens_reasoning,
                  source_file, source_offset, source_line)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                         ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16, ?17,
                         ?18, ?19, ?20)",
                params![
                    e.id.to_string(),
                    e.provider.as_str(),
                    e.client,
                    e.session_id,
                    e.project_path,
                    e.git_branch,
                    e.model,
                    e.timestamp.to_rfc3339(),
                    e.kind.tag(),
                    stop_reason,
                    tool_name,
                    tool_exit_code,
                    e.tokens.input as i64,
                    e.tokens.output as i64,
                    e.tokens.cache_read as i64,
                    e.tokens.cache_creation as i64,
                    e.tokens.reasoning as i64,
                    e.source.file,
                    e.source.byte_offset as i64,
                    e.source.line as i64,
                ],
            )?;
            Ok(n == 1)
        })
        .await??;
        Ok(inserted)
    }

    /// Record / refresh the "petpet is alive right now" heartbeat. Cheap:
    /// one upsert, called every ~30s by the writer task.
    pub async fn touch_heartbeat(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let g = conn.blocking_lock();
            g.execute(
                "INSERT INTO app_heartbeat (id, last_alive)
                 VALUES (1, datetime('now'))
                 ON CONFLICT (id) DO UPDATE SET last_alive = excluded.last_alive",
                [],
            )?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// Seconds since the heartbeat was last refreshed. `None` if we've
    /// never run before (fresh install) — caller treats that as "this is
    /// the first launch, snap cursors to EOF" (install-time semantic).
    pub async fn heartbeat_age_secs(&self) -> Result<Option<i64>> {
        let conn = self.conn.clone();
        let age = tokio::task::spawn_blocking(move || -> Result<Option<i64>> {
            let g = conn.blocking_lock();
            let v: Option<i64> = g
                .query_row(
                    "SELECT CAST((julianday('now') - julianday(last_alive)) * 86400 AS INTEGER)
                     FROM app_heartbeat WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(v)
        })
        .await??;
        Ok(age)
    }

    // ═══════════════════════════════════════════════════════════════
    // Growth system DAOs
    // ═══════════════════════════════════════════════════════════════

    pub async fn ensure_install_id(&self) -> Result<String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let g = conn.blocking_lock();
            let existing: Option<String> = g
                .query_row(
                    "SELECT id FROM petpet_install ORDER BY first_seen_at ASC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok(id);
            }
            let new_id = uuid::Uuid::new_v4().to_string();
            g.execute(
                "INSERT INTO petpet_install (id, first_seen_at, schema_version)
                 VALUES (?1, datetime('now'), 1)",
                params![new_id],
            )?;
            Ok(new_id)
        })
        .await?
    }

    /// Look up a single pet by id. Used by the dashboard sidebar to
    /// inspect pets other than the active one (selecting a different
    /// sidebar thumbnail fetches that pet's stats without changing
    /// which pet is the live "active companion").
    pub async fn find_pet_by_id(&self, pet_id: &str) -> Result<Option<Pet>> {
        let conn = self.conn.clone();
        let pet_id_owned = pet_id.to_string();
        let pet = tokio::task::spawn_blocking(move || -> Result<Option<Pet>> {
            let g = conn.blocking_lock();
            let row: Option<(String, String, String, String, String, i64, String, Option<String>)> = g
                .query_row(
                    "SELECT id, name, template_id, snapshot_path, born_at, is_active, origin_device_id, name_finalized_at
                     FROM pet WHERE id = ?1 LIMIT 1",
                    params![pet_id_owned],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7).ok(),
                        ))
                    },
                )
                .optional()?;
            Ok(row.map(|(id, name, template_id, snapshot_path, born_at, is_active, origin, finalized)| {
                Pet {
                    id,
                    name,
                    template_id,
                    snapshot_path,
                    born_at: parse_dt(&born_at),
                    is_active: is_active != 0,
                    origin_device_id: origin,
                    name_finalized_at: finalized.as_deref().map(parse_dt),
                }
            }))
        })
        .await??;
        Ok(pet)
    }

    pub async fn find_active_pet(&self) -> Result<Option<Pet>> {
        let conn = self.conn.clone();
        let pet = tokio::task::spawn_blocking(move || -> Result<Option<Pet>> {
            let g = conn.blocking_lock();
            let row: Option<(String, String, String, String, String, i64, String, Option<String>)> = g
                .query_row(
                    "SELECT id, name, template_id, snapshot_path, born_at, is_active, origin_device_id, name_finalized_at
                     FROM pet WHERE is_active = 1 ORDER BY updated_at DESC LIMIT 1",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7).ok(),
                        ))
                    },
                )
                .optional()?;
            Ok(row.map(|(id, name, template_id, snapshot_path, born_at, is_active, origin, finalized)| {
                Pet {
                    id,
                    name,
                    template_id,
                    snapshot_path,
                    born_at: parse_dt(&born_at),
                    is_active: is_active != 0,
                    origin_device_id: origin,
                    name_finalized_at: finalized.as_deref().map(parse_dt),
                }
            }))
        })
        .await??;
        Ok(pet)
    }

    pub async fn insert_pet(
        &self,
        id: &str,
        name: &str,
        template_id: &str,
        snapshot_path: &str,
        born_at: DateTime<Utc>,
        is_active: bool,
        origin_device_id: &str,
    ) -> Result<Pet> {
        let conn = self.conn.clone();
        let id_s = id.to_string();
        let name_s = name.to_string();
        let template_s = template_id.to_string();
        let snapshot_s = snapshot_path.to_string();
        let origin_s = origin_device_id.to_string();
        let born_s = born_at.to_rfc3339();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let g = conn.blocking_lock();
            g.execute(
                "INSERT INTO pet (id, name, template_id, snapshot_path, born_at, is_active, origin_device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id_s, name_s, template_s, snapshot_s, born_s, is_active as i64, origin_s],
            )?;
            Ok(())
        })
        .await??;
        Ok(Pet {
            id: id.to_string(),
            name: name.to_string(),
            template_id: template_id.to_string(),
            snapshot_path: snapshot_path.to_string(),
            born_at,
            is_active,
            origin_device_id: origin_device_id.to_string(),
            name_finalized_at: None,
        })
    }

    /// Finalize a pet's name at the hatch-time ceremony. Idempotent:
    /// repeated calls are allowed (e.g. hatch ceremony replays after a
    /// dev XP reset, or the user wants to rename via the same flow).
    /// If `new_name` is `None`, the current name is preserved (user chose
    /// Skip). `name_finalized_at` is refreshed on every successful call.
    pub async fn finalize_pet_name(
        &self,
        pet_id: &str,
        new_name: Option<String>,
    ) -> Result<Pet> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let now_dt = Utc::now();
        let now_s = now_dt.to_rfc3339();
        tokio::task::spawn_blocking(move || -> Result<Pet> {
            let g = conn.blocking_lock();
            let row: Option<(String, String, String, String, i64, String)> = g
                .query_row(
                    "SELECT name, template_id, snapshot_path, born_at, is_active, origin_device_id
                     FROM pet WHERE id = ?1",
                    params![pet_id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((curr_name, template_id, snapshot_path, born_at, is_active, origin)) = row
            else {
                anyhow::bail!("pet not found: {}", pet_id);
            };
            let final_name = new_name.unwrap_or_else(|| curr_name.clone());
            g.execute(
                "UPDATE pet
                    SET name = ?1, name_finalized_at = ?2, updated_at = datetime('now')
                  WHERE id = ?3",
                params![final_name, now_s, pet_id],
            )?;
            Ok(Pet {
                id: pet_id,
                name: final_name,
                template_id,
                snapshot_path,
                born_at: parse_dt(&born_at),
                is_active: is_active != 0,
                origin_device_id: origin,
                name_finalized_at: Some(now_dt),
            })
        })
        .await?
    }

    /// List every pet row, freshest first. Used by the "switch
    /// companion" UI; cheap query, ordering by `updated_at` puts the
    /// most-recently-active companions at the top.
    pub async fn list_pets(&self) -> Result<Vec<Pet>> {
        let conn = self.conn.clone();
        let pets = tokio::task::spawn_blocking(move || -> Result<Vec<Pet>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT id, name, template_id, snapshot_path, born_at, is_active, origin_device_id, name_finalized_at
                 FROM pet ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7).ok().flatten(),
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, name, template_id, snapshot_path, born_at, is_active, origin, finalized) =
                    row?;
                out.push(Pet {
                    id,
                    name,
                    template_id,
                    snapshot_path,
                    born_at: parse_dt(&born_at),
                    is_active: is_active != 0,
                    origin_device_id: origin,
                    name_finalized_at: finalized.as_deref().map(parse_dt),
                });
            }
            Ok(out)
        })
        .await??;
        Ok(pets)
    }

    pub async fn set_only_active_pet(&self, pet_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let g = conn.blocking_lock();
            g.execute("UPDATE pet SET is_active = 0", [])?;
            g.execute(
                "UPDATE pet SET is_active = 1, updated_at = datetime('now') WHERE id = ?1",
                params![pet_id],
            )?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    pub async fn get_pet_state(&self, pet_id: &str) -> Result<Option<PetStateRow>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let row = tokio::task::spawn_blocking(move || -> Result<Option<PetStateRow>> {
            let g = conn.blocking_lock();
            let row: Option<(i64, i64, Option<String>)> = g
                .query_row(
                    "SELECT total_xp, current_level, last_active_at FROM pet_state WHERE pet_id = ?1",
                    params![pet_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2).ok())),
                )
                .optional()?;
            Ok(row.map(|(total, lvl, last)| PetStateRow {
                total_xp: total,
                current_level: lvl as u32,
                last_active_at: last.as_deref().map(parse_dt),
            }))
        })
        .await??;
        Ok(row)
    }

    pub async fn upsert_pet_state(
        &self,
        pet_id: &str,
        total_xp: i64,
        current_level: u32,
        last_active_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let last_s = last_active_at.map(|d| d.to_rfc3339());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let g = conn.blocking_lock();
            g.execute(
                "INSERT INTO pet_state (pet_id, total_xp, current_level, last_active_at, recomputed_at)
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))
                 ON CONFLICT (pet_id) DO UPDATE SET
                    total_xp = excluded.total_xp,
                    current_level = excluded.current_level,
                    last_active_at = COALESCE(excluded.last_active_at, pet_state.last_active_at),
                    recomputed_at = excluded.recomputed_at",
                params![pet_id, total_xp, current_level as i64, last_s],
            )?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    pub async fn insert_xp_event(&self, rec: &XpEventRecord) -> Result<bool> {
        let conn = self.conn.clone();
        let rec = rec.clone_for_insert();
        let inserted = tokio::task::spawn_blocking(move || -> Result<bool> {
            let g = conn.blocking_lock();
            let n = g.execute(
                "INSERT OR IGNORE INTO xp_event
                 (id, pet_id, occurred_at, source_type, source_ref, xp_delta, reason, rule_id, origin_device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rec.id,
                    rec.pet_id,
                    rec.occurred_at,
                    rec.source_type,
                    rec.source_ref,
                    rec.xp_delta,
                    rec.reason,
                    rec.rule_id,
                    rec.origin_device_id,
                ],
            )?;
            Ok(n == 1)
        })
        .await??;
        Ok(inserted)
    }

    /// Insert a pre-built `XpEventInsert` directly (used by the pet
    /// importer when replaying `xp_events.jsonl`). Same SQL as
    /// `insert_xp_event` but skips the deterministic UUID step —
    /// preserves whatever id the source archive carried so dedup
    /// works on re-imports.
    pub async fn insert_xp_event_raw(
        &self,
        rec: &crate::xp::writer::XpEventInsert,
    ) -> Result<bool> {
        let conn = self.conn.clone();
        let rec = rec.clone();
        let inserted = tokio::task::spawn_blocking(move || -> Result<bool> {
            let g = conn.blocking_lock();
            let n = g.execute(
                "INSERT OR IGNORE INTO xp_event
                 (id, pet_id, occurred_at, source_type, source_ref, xp_delta, reason, rule_id, origin_device_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rec.id,
                    rec.pet_id,
                    rec.occurred_at,
                    rec.source_type,
                    rec.source_ref,
                    rec.xp_delta,
                    rec.reason,
                    rec.rule_id,
                    rec.origin_device_id,
                ],
            )?;
            Ok(n == 1)
        })
        .await??;
        Ok(inserted)
    }

    /// Wipe every `xp_event` row for one pet. Used by the dev "reset XP"
    /// flow — after this, a `rebuild` call will produce 0 total_xp.
    /// Stream every `xp_event` row for `pet_id` in chronological
    /// order. Used by the archive exporter (`pet_export`) to dump
    /// the full XP history as JSONL so the destination machine can
    /// replay them and reconstruct the pet's exact state.
    pub async fn list_xp_events_for_pet(
        &self,
        pet_id: &str,
    ) -> Result<Vec<crate::xp::writer::XpEventInsert>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let rows = tokio::task::spawn_blocking(
            move || -> Result<Vec<crate::xp::writer::XpEventInsert>> {
                let g = conn.blocking_lock();
                let mut stmt = g.prepare(
                    "SELECT id, pet_id, occurred_at, source_type, source_ref,
                            xp_delta, reason, rule_id, origin_device_id
                     FROM xp_event
                     WHERE pet_id = ?1
                     ORDER BY occurred_at ASC",
                )?;
                let rows = stmt
                    .query_map(params![pet_id], |r| {
                        Ok(crate::xp::writer::XpEventInsert {
                            id: r.get(0)?,
                            pet_id: r.get(1)?,
                            occurred_at: r.get(2)?,
                            source_type: r.get(3)?,
                            source_ref: r.get::<_, Option<String>>(4)?,
                            xp_delta: r.get(5)?,
                            reason: r.get(6).unwrap_or_default(),
                            rule_id: r.get(7).unwrap_or_default(),
                            origin_device_id: r.get(8).unwrap_or_default(),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            },
        )
        .await??;
        Ok(rows)
    }

    pub async fn delete_xp_events_for_pet(&self, pet_id: &str) -> Result<usize> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let n = tokio::task::spawn_blocking(move || -> Result<usize> {
            let g = conn.blocking_lock();
            let n = g.execute(
                "DELETE FROM xp_event WHERE pet_id = ?1",
                params![pet_id],
            )?;
            Ok(n)
        })
        .await??;
        Ok(n)
    }

    pub async fn sum_xp_for_pet(&self, pet_id: &str) -> Result<i64> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let s = tokio::task::spawn_blocking(move || -> Result<i64> {
            let g = conn.blocking_lock();
            // COALESCE so a pet with zero xp_event rows returns 0
            // instead of NULL (which fails i64 deserialization).
            let s: i64 = g.query_row(
                "SELECT COALESCE(SUM(xp_delta), 0) FROM xp_event WHERE pet_id = ?1",
                params![pet_id],
                |r| r.get(0),
            )?;
            Ok(s)
        })
        .await??;
        Ok(s)
    }

    pub async fn latest_xp_event_time(&self, pet_id: &str) -> Result<Option<DateTime<Utc>>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let s = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let g = conn.blocking_lock();
            // Column comes back NULL when the pet has zero xp_event rows
            // (e.g. right after a reset). Use Option<String> for the column
            // type, then flatten the outer Option<row>.
            let s: Option<Option<String>> = g
                .query_row(
                    "SELECT MAX(occurred_at) FROM xp_event WHERE pet_id = ?1",
                    params![pet_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?;
            Ok(s.flatten())
        })
        .await??;
        Ok(s.as_deref().map(parse_dt))
    }

    /// Per-provider XP totals for one pet. Joins `xp_event` →
    /// `usage_event` so we can attribute usage-sourced XP back to its
    /// provider (Claude / Codex / OpenCode / ...). Activity-sourced
    /// and manual XP land in the `provider IS NULL` bucket — callers
    /// surface that as "interaction XP".
    pub async fn xp_by_provider(&self, pet_id: &str) -> Result<Vec<XpByProviderRow>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<XpByProviderRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT u.provider AS provider,
                        COALESCE(SUM(x.xp_delta), 0) AS xp_total,
                        COUNT(x.id) AS events,
                        COALESCE(SUM(x.source_type = 'usage'), 0) AS usage_events
                 FROM xp_event x
                 LEFT JOIN usage_event u
                   ON x.source_type = 'usage' AND x.source_ref = u.id
                 WHERE x.pet_id = ?1
                 GROUP BY u.provider
                 ORDER BY xp_total DESC",
            )?;
            let rows = stmt
                .query_map(params![pet_id], |r| {
                    Ok(XpByProviderRow {
                        provider: r.get::<_, Option<String>>(0)?,
                        xp_total: r.get(1)?,
                        events: r.get::<_, i64>(2)? as u64,
                        usage_events: r.get::<_, i64>(3)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Pet-scoped variant of `stats_summary`. Only counts usage
    /// events that were credited to this pet's XP — joins `xp_event`
    /// on `source_type='usage'` so a usage event that landed before
    /// the user switched companions is attributed to whichever pet
    /// was active at the time.
    ///
    /// Without this filter the dashboard would show "all tokens you
    /// ever sent through Claude" alongside "this pet's XP", which is
    /// semantically inconsistent — heavy historical use under a
    /// different pet would inflate the new pet's token totals.
    pub async fn stats_summary_for_pet(&self, pet_id: &str) -> Result<Vec<StatsRow>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<StatsRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT u.provider, u.model,
                        COUNT(*) AS events,
                        SUM(u.tokens_input) AS input,
                        SUM(u.tokens_output) AS output,
                        SUM(u.tokens_cache_read) AS cache_read,
                        SUM(u.tokens_cache_creation) AS cache_creation,
                        SUM(u.tokens_reasoning) AS reasoning
                 FROM usage_event u
                 INNER JOIN xp_event x
                   ON x.source_type = 'usage' AND x.source_ref = u.id
                 WHERE x.pet_id = ?1
                 GROUP BY u.provider, u.model
                 ORDER BY u.provider, u.model",
            )?;
            let rows = stmt
                .query_map(params![pet_id], |r| {
                    Ok(StatsRow {
                        provider: r.get(0)?,
                        model: r.get(1)?,
                        events: r.get::<_, i64>(2)? as u64,
                        input: r.get::<_, i64>(3)? as u64,
                        output: r.get::<_, i64>(4)? as u64,
                        cache_read: r.get::<_, i64>(5)? as u64,
                        cache_creation: r.get::<_, i64>(6)? as u64,
                        reasoning: r.get::<_, i64>(7)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Pet-scoped variant of `stats_for_provider`. Same join semantics
    /// as `stats_summary_for_pet` — only counts events the active pet
    /// actually earned XP from.
    pub async fn stats_for_provider_for_pet(
        &self,
        pet_id: &str,
        provider: &str,
    ) -> Result<Vec<StatsRow>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let provider = provider.to_string();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<StatsRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT u.provider, u.model,
                        COUNT(*) AS events,
                        SUM(u.tokens_input) AS input,
                        SUM(u.tokens_output) AS output,
                        SUM(u.tokens_cache_read) AS cache_read,
                        SUM(u.tokens_cache_creation) AS cache_creation,
                        SUM(u.tokens_reasoning) AS reasoning
                 FROM usage_event u
                 INNER JOIN xp_event x
                   ON x.source_type = 'usage' AND x.source_ref = u.id
                 WHERE x.pet_id = ?1 AND u.provider = ?2
                 GROUP BY u.provider, u.model
                 ORDER BY (input + output + cache_read + cache_creation + reasoning) DESC",
            )?;
            let rows = stmt
                .query_map(params![pet_id, provider], |r| {
                    Ok(StatsRow {
                        provider: r.get(0)?,
                        model: r.get(1)?,
                        events: r.get::<_, i64>(2)? as u64,
                        input: r.get::<_, i64>(3)? as u64,
                        output: r.get::<_, i64>(4)? as u64,
                        cache_read: r.get::<_, i64>(5)? as u64,
                        cache_creation: r.get::<_, i64>(6)? as u64,
                        reasoning: r.get::<_, i64>(7)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Pet-scoped variant of `recent_usage_for_provider`. Same join.
    pub async fn recent_usage_for_provider_for_pet(
        &self,
        pet_id: &str,
        provider: &str,
        limit: usize,
    ) -> Result<Vec<RecentUsageRow>> {
        self.recent_usage_for_provider_for_pet_before(pet_id, provider, None, limit)
            .await
    }

    /// Keyset-paginated variant of `recent_usage_for_provider_for_pet`.
    ///
    /// Pagination is by `timestamp` rather than offset because new
    /// events get appended live (the watcher is always running) — an
    /// offset cursor would shift under the user. Keyset on the same
    /// column we ORDER BY is stable across writes.
    ///
    /// `before_timestamp = None` → fetch the freshest page.
    /// `before_timestamp = Some(t)` → fetch the page strictly older
    /// than `t` (exclusive boundary so the last row of the previous
    /// page doesn't repeat).
    pub async fn recent_usage_for_provider_for_pet_before(
        &self,
        pet_id: &str,
        provider: &str,
        before_timestamp: Option<String>,
        limit: usize,
    ) -> Result<Vec<RecentUsageRow>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let provider = provider.to_string();
        let limit_i = limit as i64;
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<RecentUsageRow>> {
            let g = conn.blocking_lock();
            // Two query shapes — with vs. without cursor. Sticking the
            // cursor into a single prepared query via `?3 IS NULL OR
            // u.timestamp < ?3` works but defeats the index on
            // `timestamp`. Two paths is verbose but each plan is
            // optimal. Each branch binds `stmt` first then materialises
            // the iterator into a Vec in the same scope (rusqlite's
            // `MappedRows` borrows `stmt`, so they must drop together).
            let rows: Vec<RecentUsageRow> = if let Some(ref ts) = before_timestamp {
                let mut stmt = g.prepare(
                    "SELECT u.timestamp, u.model, u.kind,
                            u.tokens_input, u.tokens_output,
                            u.tokens_cache_read, u.tokens_cache_creation, u.tokens_reasoning
                     FROM usage_event u
                     INNER JOIN xp_event x
                       ON x.source_type = 'usage' AND x.source_ref = u.id
                     WHERE x.pet_id = ?1 AND u.provider = ?2 AND u.timestamp < ?3
                     ORDER BY u.timestamp DESC
                     LIMIT ?4",
                )?;
                let mapped = stmt
                    .query_map(params![pet_id, provider, ts, limit_i], map_recent_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                mapped
            } else {
                let mut stmt = g.prepare(
                    "SELECT u.timestamp, u.model, u.kind,
                            u.tokens_input, u.tokens_output,
                            u.tokens_cache_read, u.tokens_cache_creation, u.tokens_reasoning
                     FROM usage_event u
                     INNER JOIN xp_event x
                       ON x.source_type = 'usage' AND x.source_ref = u.id
                     WHERE x.pet_id = ?1 AND u.provider = ?2
                     ORDER BY u.timestamp DESC
                     LIMIT ?3",
                )?;
                let mapped = stmt
                    .query_map(params![pet_id, provider, limit_i], map_recent_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                mapped
            };
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Per-(provider, model) token totals scoped to a single provider.
    /// Used by the dashboard's drill-down view — same shape as
    /// `stats_summary` but filtered, so the frontend doesn't have to
    /// re-aggregate.
    pub async fn stats_for_provider(&self, provider: &str) -> Result<Vec<StatsRow>> {
        let conn = self.conn.clone();
        let provider = provider.to_string();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<StatsRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT provider, model,
                        COUNT(*) AS events,
                        SUM(tokens_input) AS input,
                        SUM(tokens_output) AS output,
                        SUM(tokens_cache_read) AS cache_read,
                        SUM(tokens_cache_creation) AS cache_creation,
                        SUM(tokens_reasoning) AS reasoning
                 FROM usage_event
                 WHERE provider = ?1
                 GROUP BY provider, model
                 ORDER BY (input + output + cache_read + cache_creation + reasoning) DESC",
            )?;
            let rows = stmt
                .query_map(params![provider], |r| {
                    Ok(StatsRow {
                        provider: r.get(0)?,
                        model: r.get(1)?,
                        events: r.get::<_, i64>(2)? as u64,
                        input: r.get::<_, i64>(3)? as u64,
                        output: r.get::<_, i64>(4)? as u64,
                        cache_read: r.get::<_, i64>(5)? as u64,
                        cache_creation: r.get::<_, i64>(6)? as u64,
                        reasoning: r.get::<_, i64>(7)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Most recent `limit` usage events for one provider, freshest
    /// first. Backs the "REQUESTS" sub-log in the dashboard's
    /// per-provider drill-down: a developer can audit individual
    /// requests and see exactly which call drove the largest spike.
    pub async fn recent_usage_for_provider(
        &self,
        provider: &str,
        limit: usize,
    ) -> Result<Vec<RecentUsageRow>> {
        self.recent_usage_for_provider_before(provider, None, limit)
            .await
    }

    /// Keyset-paginated variant of `recent_usage_for_provider`. Same
    /// shape and semantics as `recent_usage_for_provider_for_pet_before`
    /// but without the `xp_event` join — surfaces every usage event
    /// in the library, used by the dashboard's ALL PETS view.
    pub async fn recent_usage_for_provider_before(
        &self,
        provider: &str,
        before_timestamp: Option<String>,
        limit: usize,
    ) -> Result<Vec<RecentUsageRow>> {
        let conn = self.conn.clone();
        let provider = provider.to_string();
        let limit_i = limit as i64;
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<RecentUsageRow>> {
            let g = conn.blocking_lock();
            let rows: Vec<RecentUsageRow> = if let Some(ref ts) = before_timestamp {
                let mut stmt = g.prepare(
                    "SELECT timestamp, model, kind,
                            tokens_input, tokens_output,
                            tokens_cache_read, tokens_cache_creation, tokens_reasoning
                     FROM usage_event
                     WHERE provider = ?1 AND timestamp < ?2
                     ORDER BY timestamp DESC
                     LIMIT ?3",
                )?;
                let mapped = stmt
                    .query_map(params![provider, ts, limit_i], map_recent_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                mapped
            } else {
                let mut stmt = g.prepare(
                    "SELECT timestamp, model, kind,
                            tokens_input, tokens_output,
                            tokens_cache_read, tokens_cache_creation, tokens_reasoning
                     FROM usage_event
                     WHERE provider = ?1
                     ORDER BY timestamp DESC
                     LIMIT ?2",
                )?;
                let mapped = stmt
                    .query_map(params![provider, limit_i], map_recent_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                mapped
            };
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Most recent `limit` XP events for one pet, freshest first. Used
    /// by the dashboard's "recent moves" battle-log section.
    pub async fn recent_xp_events(&self, pet_id: &str, limit: usize) -> Result<Vec<RecentXpRow>> {
        let conn = self.conn.clone();
        let pet_id = pet_id.to_string();
        let limit_i = limit as i64;
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<RecentXpRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT x.occurred_at, x.xp_delta, x.source_type, x.reason,
                        u.provider, u.model
                 FROM xp_event x
                 LEFT JOIN usage_event u
                   ON x.source_type = 'usage' AND x.source_ref = u.id
                 WHERE x.pet_id = ?1
                 ORDER BY x.occurred_at DESC
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![pet_id, limit_i], |r| {
                    Ok(RecentXpRow {
                        occurred_at: r.get::<_, String>(0)?,
                        xp_delta: r.get(1)?,
                        source_type: r.get(2)?,
                        reason: r.get::<_, Option<String>>(3)?,
                        provider: r.get::<_, Option<String>>(4)?,
                        model: r.get::<_, Option<String>>(5)?,
                        // Per-pet path: caller already knows the
                        // pet name, leave the row tag empty.
                        pet_name: None,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// All-pets variant of `recent_xp_events` — freshest events from
    /// every pet, with each row tagged by its pet name so the moves
    /// log can label which pet earned what. Backs the dashboard's
    /// "ALL PETS" sidebar view.
    pub async fn recent_xp_events_all(&self, limit: usize) -> Result<Vec<RecentXpRow>> {
        let conn = self.conn.clone();
        let limit_i = limit as i64;
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<RecentXpRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT x.occurred_at, x.xp_delta, x.source_type, x.reason,
                        u.provider, u.model, p.name
                 FROM xp_event x
                 LEFT JOIN usage_event u
                   ON x.source_type = 'usage' AND x.source_ref = u.id
                 LEFT JOIN pet p
                   ON p.id = x.pet_id
                 ORDER BY x.occurred_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit_i], |r| {
                    Ok(RecentXpRow {
                        occurred_at: r.get::<_, String>(0)?,
                        xp_delta: r.get(1)?,
                        source_type: r.get(2)?,
                        reason: r.get::<_, Option<String>>(3)?,
                        provider: r.get::<_, Option<String>>(4)?,
                        model: r.get::<_, Option<String>>(5)?,
                        pet_name: r.get::<_, Option<String>>(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// All-pets variant of `xp_by_provider`. Sums XP across every
    /// pet, grouped by provider — used by the dashboard's "ALL PETS"
    /// view to build provider chips that aggregate the whole library.
    pub async fn xp_by_provider_all(&self) -> Result<Vec<XpByProviderRow>> {
        let conn = self.conn.clone();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<XpByProviderRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT u.provider AS provider,
                        COALESCE(SUM(x.xp_delta), 0) AS xp_total,
                        COUNT(x.id) AS events,
                        COALESCE(SUM(x.source_type = 'usage'), 0) AS usage_events
                 FROM xp_event x
                 LEFT JOIN usage_event u
                   ON x.source_type = 'usage' AND x.source_ref = u.id
                 GROUP BY u.provider
                 ORDER BY xp_total DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(XpByProviderRow {
                        provider: r.get::<_, Option<String>>(0)?,
                        xp_total: r.get(1)?,
                        events: r.get::<_, i64>(2)? as u64,
                        usage_events: r.get::<_, i64>(3)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }

    /// Aggregate metadata about the entire pet library. Used by the
    /// dashboard's "ALL PETS" identity row — pet count, oldest pet's
    /// age in days, and total XP across every pet's `pet_state` row.
    pub async fn pet_library_aggregates(&self) -> Result<PetLibraryAggregates> {
        let conn = self.conn.clone();
        let agg = tokio::task::spawn_blocking(move || -> Result<PetLibraryAggregates> {
            let g = conn.blocking_lock();
            // SUM total_xp from pet_state — that's the per-pet cached
            // running total, kept in sync by the XP engine.
            let total_xp: i64 = g
                .query_row(
                    "SELECT COALESCE(SUM(total_xp), 0) FROM pet_state",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let (pet_count, oldest_born_at): (i64, Option<String>) = g
                .query_row(
                    "SELECT COUNT(*), MIN(born_at) FROM pet",
                    [],
                    |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap_or((0, None));
            Ok(PetLibraryAggregates {
                pet_count: pet_count as u64,
                oldest_born_at: oldest_born_at.as_deref().map(parse_dt),
                total_xp,
            })
        })
        .await??;
        Ok(agg)
    }

    pub async fn stats_summary(&self) -> Result<Vec<StatsRow>> {
        let conn = self.conn.clone();
        let rows = tokio::task::spawn_blocking(move || -> Result<Vec<StatsRow>> {
            let g = conn.blocking_lock();
            let mut stmt = g.prepare(
                "SELECT provider, model,
                        COUNT(*) AS events,
                        SUM(tokens_input) AS input,
                        SUM(tokens_output) AS output,
                        SUM(tokens_cache_read) AS cache_read,
                        SUM(tokens_cache_creation) AS cache_creation,
                        SUM(tokens_reasoning) AS reasoning
                 FROM usage_event
                 GROUP BY provider, model
                 ORDER BY provider, model",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(StatsRow {
                        provider: r.get(0)?,
                        model: r.get(1)?,
                        events: r.get::<_, i64>(2)? as u64,
                        input: r.get::<_, i64>(3)? as u64,
                        output: r.get::<_, i64>(4)? as u64,
                        cache_read: r.get::<_, i64>(5)? as u64,
                        cache_creation: r.get::<_, i64>(6)? as u64,
                        reasoning: r.get::<_, i64>(7)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await??;
        Ok(rows)
    }
}

/// Idempotent `ALTER TABLE ... ADD COLUMN` — no-op when the column already
/// exists. Used to bring older databases forward without losing rows.
fn ensure_column(c: &Connection, table: &str, col: &str, sql_type: &str) -> Result<()> {
    let exists: Option<String> = c
        .query_row(
            &format!("SELECT name FROM pragma_table_info('{table}') WHERE name = ?1"),
            params![col],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        c.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {col} {sql_type}"),
            [],
        )?;
    }
    Ok(())
}

/// Idempotent `ALTER TABLE ... DROP COLUMN` — no-op when the column does
/// not exist. Used to retire legacy columns from earlier schema versions.
/// Requires SQLite >= 3.35.0 (rusqlite bundled is newer than that).
///
/// SQLite refuses to drop a column while an index references it, so we
/// first sweep any indexes whose `index_info` mentions the target column
/// and drop them. Schema-defined indexes will be re-created on the next
/// `apply schema (indexes)` pass.
fn drop_column_if_present(c: &Connection, table: &str, col: &str) -> Result<()> {
    let exists: Option<String> = c
        .query_row(
            &format!("SELECT name FROM pragma_table_info('{table}') WHERE name = ?1"),
            params![col],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(());
    }

    // Discover indexes on this table.
    let index_names: Vec<String> = {
        let mut stmt = c.prepare(&format!("PRAGMA index_list('{table}')"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // For each index, check if any of its columns matches the target.
    let mut to_drop = Vec::new();
    for idx in &index_names {
        let mut info = c.prepare(&format!("PRAGMA index_info('{idx}')"))?;
        let cols = info.query_map([], |r| r.get::<_, String>(2))?;
        for col_name in cols {
            if col_name? == col {
                to_drop.push(idx.clone());
                break;
            }
        }
    }
    for idx in to_drop {
        c.execute(&format!("DROP INDEX IF EXISTS {idx}"), [])?;
    }

    c.execute(&format!("ALTER TABLE {table} DROP COLUMN {col}"), [])?;
    Ok(())
}

/// Persisted resume position for one file under one provider.
#[derive(Debug, Clone, Copy)]
pub struct Cursor {
    pub byte_offset: u64,
    pub line_index: u64,
}

/// Row shape used internally by `get_pet_state`.
#[derive(Debug, Clone)]
pub struct PetStateRow {
    pub total_xp: i64,
    pub current_level: u32,
    pub last_active_at: Option<DateTime<Utc>>,
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| {
            // SQLite's `datetime('now')` produces "YYYY-MM-DD HH:MM:SS" (no TZ).
            // Treat that as UTC.
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .map(|nd| Utc.from_utc_datetime(&nd))
                .unwrap_or_else(|_| Utc::now())
        })
}

#[derive(Debug)]
pub struct StatsRow {
    pub provider: String,
    pub model: String,
    pub events: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub reasoning: u64,
}

/// One row of "how much XP did this provider earn for me". `provider`
/// is `None` for XP that wasn't sourced from a token-bearing usage
/// event (i.e. activity hooks + manual dev grants) — the dashboard
/// surfaces this as "interactions" / "other".
#[derive(Debug, Clone)]
pub struct XpByProviderRow {
    pub provider: Option<String>,
    pub xp_total: i64,
    pub events: u64,
    pub usage_events: u64,
}

#[derive(Debug, Clone)]
pub struct RecentXpRow {
    pub occurred_at: String,
    pub xp_delta: i64,
    pub source_type: String,
    pub reason: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Name of the pet that earned this XP. `None` in per-pet
    /// queries (where the caller already knows the pet's name);
    /// `Some(name)` in the all-pets aggregate so the moves log can
    /// label each row with "which pet earned this".
    pub pet_name: Option<String>,
}

/// Library-wide pet aggregates surfaced in the dashboard's "ALL
/// PETS" identity row.
#[derive(Debug, Clone)]
pub struct PetLibraryAggregates {
    pub pet_count: u64,
    /// `born_at` of the oldest pet in the library. `None` if no pets
    /// exist yet (fresh install).
    pub oldest_born_at: Option<DateTime<Utc>>,
    /// Sum of `pet_state.total_xp` across every pet.
    pub total_xp: i64,
}

/// One usage_event row, denormalised for direct display in the
/// dashboard's per-provider drill-down "REQUESTS" sub-log. Tokens
/// kept split so the frontend can colour-code in / out / cache
/// distinctly if it wants to.
#[derive(Debug, Clone)]
pub struct RecentUsageRow {
    pub timestamp: String,
    pub model: String,
    pub kind: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_creation: u64,
    pub tokens_reasoning: u64,
}

/// Shared row mapper for the two `recent_usage_for_provider_for_pet*`
/// query shapes. Keeps both code paths in sync — adding a column means
/// editing both SELECT lists AND this mapper, but never one without
/// the other.
fn map_recent_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecentUsageRow> {
    Ok(RecentUsageRow {
        timestamp: r.get(0)?,
        model: r.get(1)?,
        kind: r.get(2)?,
        tokens_input: r.get::<_, i64>(3)? as u64,
        tokens_output: r.get::<_, i64>(4)? as u64,
        tokens_cache_read: r.get::<_, i64>(5)? as u64,
        tokens_cache_creation: r.get::<_, i64>(6)? as u64,
        tokens_reasoning: r.get::<_, i64>(7)? as u64,
    })
}

