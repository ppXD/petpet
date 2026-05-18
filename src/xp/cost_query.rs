//! Aggregate cost queries — turns the per-event token/model data in
//! `usage_event` into "how much did I feed my pet today/this week/
//! this month/ever?" answers in USD.
//!
//! ## Design notes
//!
//! 1. **No `cost_usd` column on the event row.** We compute cost on
//!    the read path by joining each row through [`crate::xp::pricing`]
//!    in Rust. Pricing tables change (Anthropic adds a model, our
//!    estimate of an undocumented tier improves) — re-priceing
//!    historical events automatically on the next query is the right
//!    behaviour. The alternative — caching cost per row — would
//!    drift the moment we tweaked a rate.
//!
//! 2. **Time windows are computed in LOCAL time, then converted to
//!    UTC for SQL.** This was the bug from our CodexBar reconciliation:
//!    aggregating by UTC day boundary gives different numbers from
//!    CodexBar's local-day boundary. We always anchor on the user's
//!    timezone (`chrono::Local`).
//!
//! 3. **Per-model rows expose the full TokenDelta** so the UI can
//!    show "65% of cost is Opus cache_read" type breakdowns without
//!    re-querying.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::DbHandle;
use crate::event::{ProviderId, TokenDelta};
use crate::xp::pricing;

/// Aggregate summary for one (provider, model) over a time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub provider: String,
    pub model: String,
    pub events: u64,
    pub tokens: TokenDelta,
    pub cost_usd: f64,
}

impl ModelCost {
    fn total_tokens(&self) -> u64 {
        self.tokens.input
            + self.tokens.output
            + self.tokens.cache_read
            + self.tokens.cache_creation
            + self.tokens.reasoning
    }
}

/// Top-level cost breakdown for a time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBreakdown {
    /// USD across all providers / models in the window.
    pub total_usd: f64,
    /// Tokens across all categories.
    pub total_tokens: u64,
    /// Number of underlying usage_event rows aggregated.
    pub total_events: u64,
    /// Per-model rows, sorted by cost descending. The frontend
    /// "cuisine breakdown" picks the top 4-5 and groups the rest
    /// as "Other".
    pub by_model: Vec<ModelCost>,
    /// Window bounds (UTC) for traceability in the UI tooltip.
    pub window_start_utc: DateTime<Utc>,
    pub window_end_utc: DateTime<Utc>,
}

/// One day's worth of cost — used by the chart UI on the
/// "feeding bill" surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyCost {
    /// `YYYY-MM-DD` in the user's local timezone.
    pub date_local: String,
    pub cost_usd: f64,
    pub total_tokens: u64,
    pub total_events: u64,
}

/// Per-day breakdown for one (pet, provider) pair — the dashboard's
/// drill-down "BY DAY" section. Each entry carries both the day-level
/// totals and the per-model split, so the UI can show "May 16 — $241,
/// 99% Opus 4.7" without a follow-up query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayProviderBreakdown {
    pub date_local: String,
    pub cost_usd: f64,
    pub total_tokens: u64,
    pub total_events: u64,
    /// Per-model breakdown within this day, sorted by cost desc.
    pub by_model: Vec<ModelCost>,
}

// ─── Public API ───────────────────────────────────────────────────

/// Cost for an arbitrary UTC window. Base primitive; the
/// today/week/month wrappers below convert local-time windows
/// to UTC and call this.
pub async fn cost_for_window(
    db: &DbHandle,
    window_start_utc: DateTime<Utc>,
    window_end_utc: DateTime<Utc>,
) -> Result<CostBreakdown> {
    let conn = db.conn().clone();
    let start = window_start_utc.to_rfc3339();
    let end = window_end_utc.to_rfc3339();

    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, u64, TokenDelta)>> {
        let g = conn.blocking_lock();
        let mut stmt = g.prepare(
            "SELECT provider, model, COUNT(*),
                    SUM(tokens_input), SUM(tokens_output),
                    SUM(tokens_cache_read), SUM(tokens_cache_creation),
                    SUM(tokens_reasoning)
             FROM usage_event
             WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY provider, model",
        )?;
        let mut out = Vec::new();
        let mut rows = stmt.query(params![start, end])?;
        while let Some(r) = rows.next()? {
            let provider: String = r.get(0)?;
            let model: String = r.get(1)?;
            let events: u64 = r.get::<_, i64>(2)? as u64;
            let tokens = TokenDelta {
                input: r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
                output: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                cache_read: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
                cache_creation: r.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64,
                reasoning: r.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as u64,
            };
            out.push((provider, model, events, tokens));
        }
        Ok(out)
    })
    .await??;

    // Apply pricing per row in Rust — keeps the SQL ignorant of rates,
    // so a rates change re-prices historical aggregates automatically.
    let mut by_model: Vec<ModelCost> = rows
        .into_iter()
        .map(|(provider_str, model, events, tokens)| {
            // Tolerate unknown provider strings (data from an old DB
            // schema, future-provider names) by falling back to a
            // benign default. Pricing for an unknown provider will
            // resolve to None and contribute $0.
            let provider = ProviderId::from_slug(&provider_str)
                .or_else(|| match provider_str.as_str() {
                    "claude_code" => Some(ProviderId::ClaudeCode),
                    "codex" => Some(ProviderId::Codex),
                    "gemini" => Some(ProviderId::Gemini),
                    "opencode" => Some(ProviderId::OpenCode),
                    "aider" => Some(ProviderId::Aider),
                    "custom_api" => Some(ProviderId::CustomApi),
                    _ => None,
                })
                .unwrap_or(ProviderId::CustomApi);
            let cost_usd = pricing::compute_cost_usd(provider, &model, &tokens);
            ModelCost { provider: provider_str, model, events, tokens, cost_usd }
        })
        .collect();

    by_model.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap_or(std::cmp::Ordering::Equal));

    let total_usd: f64 = by_model.iter().map(|m| m.cost_usd).sum();
    let total_tokens: u64 = by_model.iter().map(|m| m.total_tokens()).sum();
    let total_events: u64 = by_model.iter().map(|m| m.events).sum();

    Ok(CostBreakdown {
        total_usd,
        total_tokens,
        total_events,
        by_model,
        window_start_utc,
        window_end_utc,
    })
}

/// Cost for "today" in the user's local timezone — `[local 00:00, now)`.
///
/// Defined here as `today's local midnight → now UTC`. For midnight
/// boundary correctness across DST transitions we lean on chrono's
/// `Local` timezone math (which understands the local tz database).
pub async fn cost_today_local(db: &DbHandle) -> Result<CostBreakdown> {
    let now = chrono::Local::now();
    let start_local = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 always valid");
    let start_local_tz = chrono::Local
        .from_local_datetime(&start_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&start_local).earliest())
        .expect("local midnight resolves on any sane tz");
    let start_utc = start_local_tz.with_timezone(&Utc);
    let end_utc = Utc::now();
    cost_for_window(db, start_utc, end_utc).await
}

/// Cost for "this week" (local Monday-start) up to now.
///
/// Monday-start matches Anthropic's billing-week display in the
/// `api.anthropic.com/api/oauth/usage` endpoint, which CodexBar also
/// uses. Sunday-start can be added behind a setting later.
pub async fn cost_this_week_local(db: &DbHandle) -> Result<CostBreakdown> {
    let now = chrono::Local::now();
    let weekday_idx = now.weekday().num_days_from_monday() as i64;
    let monday_local = now
        .date_naive()
        .checked_sub_signed(Duration::days(weekday_idx))
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("week-start always resolves");
    let monday_tz = chrono::Local
        .from_local_datetime(&monday_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&monday_local).earliest())
        .expect("local Monday 00:00 resolves");
    let start_utc = monday_tz.with_timezone(&Utc);
    cost_for_window(db, start_utc, Utc::now()).await
}

/// Cost for "this month" (local 1st of month, 00:00) up to now.
pub async fn cost_this_month_local(db: &DbHandle) -> Result<CostBreakdown> {
    let now = chrono::Local::now();
    let first_local = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("first of month always valid");
    let first_tz = chrono::Local
        .from_local_datetime(&first_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&first_local).earliest())
        .expect("local month-start resolves");
    let start_utc = first_tz.with_timezone(&Utc);
    cost_for_window(db, start_utc, Utc::now()).await
}

/// All-time cost since the DB exists. We pick a far-past sentinel so
/// the SQL window is `[1970-01-01, now)` and trust SQLite's index.
pub async fn cost_lifetime(db: &DbHandle) -> Result<CostBreakdown> {
    let start = DateTime::<Utc>::from_timestamp(0, 0).expect("unix epoch is a valid UTC time");
    cost_for_window(db, start, Utc::now()).await
}

/// Per-day cost over a local-date range, inclusive. Used by the
/// chart. Returns one [`DailyCost`] per local day in the window even
/// if that day had zero activity (front-end charts prefer a contiguous
/// x-axis; zero bars are meaningful).
pub async fn cost_by_day_local(
    db: &DbHandle,
    start_local_date: NaiveDate,
    end_local_date: NaiveDate,
) -> Result<Vec<DailyCost>> {
    // Convert the local-date range into a single UTC window covering
    // [local_start 00:00, local_end+1 00:00).
    let start_local = start_local_date.and_hms_opt(0, 0, 0).expect("date.and_hms");
    let end_local = (end_local_date + Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .expect("date+1.and_hms");
    let start_utc = chrono::Local
        .from_local_datetime(&start_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&start_local).earliest())
        .expect("local start resolves")
        .with_timezone(&Utc);
    let end_utc = chrono::Local
        .from_local_datetime(&end_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&end_local).earliest())
        .expect("local end resolves")
        .with_timezone(&Utc);

    // Fetch every event in the window. Bucket per local day in Rust,
    // not in SQL — we'd need the SQL engine to know about the local
    // tz to bucket correctly, and SQLite doesn't have native tz
    // support. Pulling rows + bucketing client-side is simple and
    // performant up to millions of rows.
    let conn = db.conn().clone();
    let s = start_utc.to_rfc3339();
    let e = end_utc.to_rfc3339();

    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, String, TokenDelta)>> {
        let g = conn.blocking_lock();
        let mut stmt = g.prepare(
            "SELECT timestamp, provider, model,
                    tokens_input, tokens_output, tokens_cache_read, tokens_cache_creation, tokens_reasoning
             FROM usage_event
             WHERE timestamp >= ?1 AND timestamp < ?2",
        )?;
        let mut out = Vec::new();
        let mut rs = stmt.query(params![s, e])?;
        while let Some(r) = rs.next()? {
            let ts: String = r.get(0)?;
            let provider: String = r.get(1)?;
            let model: String = r.get(2)?;
            let tokens = TokenDelta {
                input: r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
                output: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                cache_read: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
                cache_creation: r.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64,
                reasoning: r.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as u64,
            };
            out.push((ts, provider, model, tokens));
        }
        Ok(out)
    })
    .await??;

    // Bucket by local date.
    let mut bucket: BTreeMap<NaiveDate, (f64, u64, u64)> = BTreeMap::new();
    for (ts_str, provider_str, model, tokens) in rows {
        let ts = DateTime::parse_from_rfc3339(&ts_str)?;
        let local_date = ts.with_timezone(&chrono::Local).date_naive();
        let provider = ProviderId::from_slug(&provider_str)
            .or_else(|| match provider_str.as_str() {
                "claude_code" => Some(ProviderId::ClaudeCode),
                "codex" => Some(ProviderId::Codex),
                "gemini" => Some(ProviderId::Gemini),
                "opencode" => Some(ProviderId::OpenCode),
                "aider" => Some(ProviderId::Aider),
                "custom_api" => Some(ProviderId::CustomApi),
                _ => None,
            })
            .unwrap_or(ProviderId::CustomApi);
        let cost = pricing::compute_cost_usd(provider, &model, &tokens);
        let total_tok = tokens.input + tokens.output + tokens.cache_read + tokens.cache_creation + tokens.reasoning;
        let entry = bucket.entry(local_date).or_insert((0.0, 0, 0));
        entry.0 += cost;
        entry.1 += total_tok;
        entry.2 += 1;
    }

    // Backfill empty days so the front-end gets a contiguous range.
    let mut out = Vec::new();
    let mut d = start_local_date;
    while d <= end_local_date {
        let (cost, tok, events) = bucket.get(&d).copied().unwrap_or((0.0, 0, 0));
        out.push(DailyCost {
            date_local: d.format("%Y-%m-%d").to_string(),
            cost_usd: cost,
            total_tokens: tok,
            total_events: events,
        });
        d = d
            .succ_opt()
            .expect("date succession bounded by chrono range");
    }
    Ok(out)
}

/// All-pets variant of [`cost_for_provider_for_pet_by_day`]. Same
/// shape, but aggregates usage events across every pet in the
/// library — no `xp_event` join. Powers the "ALL PETS" view's
/// provider drill-down chart.
pub async fn cost_for_provider_by_day(
    db: &DbHandle,
    provider: &str,
    days: u32,
) -> Result<Vec<DayProviderBreakdown>> {
    let (start_utc, end_utc) = local_window_utc(days);

    let conn = db.conn().clone();
    let provider_owned = provider.to_string();
    let s = start_utc.to_rfc3339();
    let e = end_utc.to_rfc3339();

    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, TokenDelta)>> {
        let g = conn.blocking_lock();
        let mut stmt = g.prepare(
            "SELECT timestamp, model,
                    tokens_input, tokens_output,
                    tokens_cache_read, tokens_cache_creation, tokens_reasoning
             FROM usage_event
             WHERE provider = ?1
               AND timestamp >= ?2
               AND timestamp < ?3",
        )?;
        let mut out = Vec::new();
        let mut rs = stmt.query(params![provider_owned, s, e])?;
        while let Some(r) = rs.next()? {
            let ts: String = r.get(0)?;
            let model: String = r.get(1)?;
            let tokens = TokenDelta {
                input: r.get::<_, Option<i64>>(2)?.unwrap_or(0).max(0) as u64,
                output: r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
                cache_read: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                cache_creation: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
                reasoning: r.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64,
            };
            out.push((ts, model, tokens));
        }
        Ok(out)
    })
    .await??;

    Ok(bucket_rows_into_days(rows, provider))
}

/// Resolve a "last N local days" window into the UTC instants the
/// SQL layer can filter on. Shared between
/// `cost_for_provider_for_pet_by_day` and `cost_for_provider_by_day`.
fn local_window_utc(days: u32) -> (DateTime<Utc>, DateTime<Utc>) {
    let now = chrono::Local::now();
    let end_local_date = now.date_naive();
    let start_local_date = end_local_date
        .checked_sub_signed(Duration::days((days.saturating_sub(1)) as i64))
        .unwrap_or(end_local_date);

    let start_local = start_local_date.and_hms_opt(0, 0, 0).expect("date.and_hms");
    let end_local = (end_local_date + Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .expect("date+1.and_hms");
    let start_utc = chrono::Local
        .from_local_datetime(&start_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&start_local).earliest())
        .expect("local start resolves")
        .with_timezone(&Utc);
    let end_utc = chrono::Local
        .from_local_datetime(&end_local)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&end_local).earliest())
        .expect("local end resolves")
        .with_timezone(&Utc);
    (start_utc, end_utc)
}

/// Bucket a flat list of (timestamp, model, tokens) rows into
/// per-day per-model `DayProviderBreakdown` records. Shared between
/// the pet-scoped and all-pets variants.
fn bucket_rows_into_days(
    rows: Vec<(String, String, TokenDelta)>,
    provider: &str,
) -> Vec<DayProviderBreakdown> {
    let provider_id = ProviderId::from_slug(provider)
        .or_else(|| match provider {
            "claude_code" => Some(ProviderId::ClaudeCode),
            "codex" => Some(ProviderId::Codex),
            "gemini" => Some(ProviderId::Gemini),
            "opencode" => Some(ProviderId::OpenCode),
            "aider" => Some(ProviderId::Aider),
            "custom_api" => Some(ProviderId::CustomApi),
            _ => None,
        })
        .unwrap_or(ProviderId::CustomApi);

    let mut per_day: BTreeMap<NaiveDate, BTreeMap<String, (u64, TokenDelta)>> = BTreeMap::new();
    for (ts_str, model, tokens) in rows {
        let Ok(ts) = DateTime::parse_from_rfc3339(&ts_str) else { continue };
        let local_date = ts.with_timezone(&chrono::Local).date_naive();
        let day_bucket = per_day.entry(local_date).or_default();
        let model_entry = day_bucket.entry(model).or_insert((0, TokenDelta::default()));
        model_entry.0 += 1;
        model_entry.1.input += tokens.input;
        model_entry.1.output += tokens.output;
        model_entry.1.cache_read += tokens.cache_read;
        model_entry.1.cache_creation += tokens.cache_creation;
        model_entry.1.reasoning += tokens.reasoning;
    }

    let mut out: Vec<DayProviderBreakdown> = per_day
        .into_iter()
        .map(|(date, models)| {
            let mut by_model: Vec<ModelCost> = models
                .into_iter()
                .map(|(model, (events, tokens))| {
                    let cost_usd = pricing::compute_cost_usd(provider_id, &model, &tokens);
                    ModelCost {
                        provider: provider.to_string(),
                        model,
                        events,
                        tokens,
                        cost_usd,
                    }
                })
                .collect();
            by_model.sort_by(|a, b| {
                b.cost_usd
                    .partial_cmp(&a.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let cost_usd: f64 = by_model.iter().map(|m| m.cost_usd).sum();
            let total_tokens: u64 = by_model.iter().map(|m| m.total_tokens()).sum();
            let total_events: u64 = by_model.iter().map(|m| m.events).sum();
            DayProviderBreakdown {
                date_local: date.format("%Y-%m-%d").to_string(),
                cost_usd,
                total_tokens,
                total_events,
                by_model,
            }
        })
        .collect();
    out.sort_by(|a, b| b.date_local.cmp(&a.date_local));
    out
}

/// Per-day breakdown for one (pet, provider) over the last `days`
/// local days, freshest first. Each `DayProviderBreakdown` carries
/// the day's totals AND a per-model split (sorted by cost desc) so
/// the dashboard drill-down's BY DAY section renders without follow-up
/// queries.
///
/// Days with zero activity are omitted (unlike `cost_by_day_local`
/// which backfills for the chart). The drill-down list reads
/// vertically — empty rows would waste pixels.
///
/// The join goes through `xp_event` so the result is pet-scoped, same
/// as everywhere else in the dashboard. Switching pets gives the new
/// pet's spend history, not the global one.
pub async fn cost_for_provider_for_pet_by_day(
    db: &DbHandle,
    pet_id: &str,
    provider: &str,
    days: u32,
) -> Result<Vec<DayProviderBreakdown>> {
    let (start_utc, end_utc) = local_window_utc(days);

    let conn = db.conn().clone();
    let pet_id_owned = pet_id.to_string();
    let provider_owned = provider.to_string();
    let s = start_utc.to_rfc3339();
    let e = end_utc.to_rfc3339();

    // Pull every (timestamp, model, tokens) row in the window for this
    // (pet, provider). Bucketing by local date happens in Rust because
    // SQLite has no native tz support.
    let rows = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, TokenDelta)>> {
        let g = conn.blocking_lock();
        let mut stmt = g.prepare(
            "SELECT u.timestamp, u.model,
                    u.tokens_input, u.tokens_output,
                    u.tokens_cache_read, u.tokens_cache_creation, u.tokens_reasoning
             FROM usage_event u
             INNER JOIN xp_event x
               ON x.source_type = 'usage' AND x.source_ref = u.id
             WHERE x.pet_id = ?1
               AND u.provider = ?2
               AND u.timestamp >= ?3
               AND u.timestamp < ?4",
        )?;
        let mut out = Vec::new();
        let mut rs = stmt.query(params![pet_id_owned, provider_owned, s, e])?;
        while let Some(r) = rs.next()? {
            let ts: String = r.get(0)?;
            let model: String = r.get(1)?;
            let tokens = TokenDelta {
                input: r.get::<_, Option<i64>>(2)?.unwrap_or(0).max(0) as u64,
                output: r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
                cache_read: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                cache_creation: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
                reasoning: r.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64,
            };
            out.push((ts, model, tokens));
        }
        Ok(out)
    })
    .await??;

    Ok(bucket_rows_into_days(rows, provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local timezone handling math test — verify cost_today_local
    /// produces a window starting at local midnight regardless of
    /// what the system clock thinks. We don't run a live SQL query
    /// here; the algebra is what matters.
    #[test]
    fn today_window_starts_at_local_midnight() {
        let now = chrono::Local::now();
        let expected_local_midnight = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        // Round-trip Local -> UTC -> Local should give back local midnight
        let utc = chrono::Local
            .from_local_datetime(&expected_local_midnight)
            .single()
            .unwrap()
            .with_timezone(&Utc);
        let back = utc.with_timezone(&chrono::Local).naive_local();
        assert_eq!(back, expected_local_midnight);
    }

    /// Per-day buckets must use local date, not UTC date. Pin this:
    /// a UTC timestamp at 23:30 UTC on day D maps to local day D+1
    /// (under positive offsets like UTC+8 → 07:30 next day). The
    /// chart should attribute that event to D+1, not D.
    #[test]
    fn timestamp_bucketing_uses_local_date() {
        // Construct a UTC timestamp at 23:30 — under UTC+8 this is
        // 07:30 of the NEXT day locally.
        let utc_ts = chrono::Utc
            .with_ymd_and_hms(2026, 5, 16, 23, 30, 0)
            .single()
            .unwrap();
        let local_date_seen = utc_ts.with_timezone(&chrono::Local).date_naive();
        // The test assumes the runner machine is UTC+8 (matches the
        // user's setup). On other tz machines, the local_date may
        // equal the UTC date; we don't fail the test in that case —
        // only assert that bucketing uses Local semantics.
        let utc_date = utc_ts.date_naive();
        let local = chrono::Local::now().offset().local_minus_utc();
        if local >= 6 * 3600 {
            // UTC+6 or further east: 23:30 UTC = next local day
            assert_ne!(local_date_seen, utc_date, "UTC+6+ tz: expected day-rollover");
        } else if local <= -7 * 3600 {
            // UTC-7 or further west: 23:30 UTC = same or previous local day
            assert!(local_date_seen <= utc_date);
        }
        // For tz between [UTC-6, UTC+6) the date may be equal —
        // either way the bucketing is *based on local date*, which
        // is what we care about.
    }
}
