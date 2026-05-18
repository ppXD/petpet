//! Dashboard data aggregator.
//!
//! Single Tauri command (`dashboard_data`) that the frontend
//! `DashboardView` (`?view=dashboard`) hits once on mount. It returns
//! everything the trainer-card screen needs:
//!
//!   - identity + level + XP bar progress + sprite path
//!   - days the pet has been raised
//!   - per-provider "type chips": total tokens + total XP + event count
//!   - last N XP events for the "recent moves" battle-log row
//!
//! Kept thin: SQL queries live in `petpet::db`, this module just
//! shapes the result into a frontend-friendly DTO. Per-model
//! drill-down for a provider is a future addition and would land as
//! `dashboard_provider_detail(provider)`.
//!
//! Why aggregate server-side: the dashboard is a one-shot view (open
//! → look → close). Doing N round-trips from the frontend for pet /
//! state / stats / xp would add latency and complicate the cache
//! story. One call is also easier to test.
//!
//! Why bundled as a sibling module instead of inline in `lib.rs`:
//! `lib.rs` is already 600+ lines of mixed concerns; this is the
//! beginning of a "view-data adapters" layer that I'd rather grow
//! out separately.

use std::path::PathBuf;
use std::sync::Arc;

use petpet::db::DbHandle;
use petpet::event::{ProviderId, TokenDelta};
use petpet::model::{ModelIdent, Tier};
use petpet::template::registry::TemplateRegistry;
use petpet::xp::cost_query;
use petpet::xp::heuristic::{fallback_tier, FallbackResult};
use petpet::xp::pricing;
use petpet::xp::registry::Registry;
use petpet::xp::XPEngine;
use serde::Serialize;
use tauri::State;

use crate::AppState;

/// Maximum number of recent XP events surfaced in the dashboard's
/// "recent moves" row. ~20 fits the GBA-style log box vertically
/// without scrolling on most pet windows; the rest is reachable via a
/// scroll bar inside the box.
const RECENT_XP_LIMIT: usize = 20;

/// Sentinel value for `pet_id` indicating "aggregate across every
/// pet in the library" (the ALL PETS sidebar tile). Distinct from
/// `None` which still means "use the currently-active pet". Any real
/// pet id is a UUID, so a literal `__all__` cannot collide.
pub const ALL_PETS_SCOPE: &str = "__all__";

#[derive(Serialize)]
pub struct DashboardData {
    /// Identity of the pet this view is scoped to. `None` ⇒ this is
    /// the "ALL PETS" aggregate view; the frontend renders a different
    /// header in that case and uses `aggregate` for the headline
    /// numbers instead of `level` / `total_xp` / `days_raised`.
    pub pet: Option<PetIdentity>,
    pub level: u32,
    pub total_xp: i64,
    pub xp_in_level: i64,
    pub xp_for_next_level: Option<i64>,
    pub stage_name: Option<String>,
    pub stage_id: Option<String>,
    pub sprite_path: Option<String>,
    pub days_raised: i64,
    pub next_evolution_name: Option<String>,
    pub next_evolution_xp_to: Option<i64>,
    pub providers: Vec<ProviderChip>,
    pub other_xp: i64,
    pub recent: Vec<RecentXp>,
    /// Populated only in the ALL PETS view. `None` for per-pet views.
    pub aggregate: Option<AllPetsAggregate>,
}

/// Library-wide summary surfaced in the ALL PETS identity row.
/// Computed once per `dashboard_data` call when the caller passes
/// the `__all__` sentinel.
#[derive(Serialize)]
pub struct AllPetsAggregate {
    pub pet_count: u64,
    /// Number of days since the oldest pet was born. 0 if no pets
    /// exist yet (fresh install).
    pub oldest_days: i64,
    pub total_xp: i64,
    /// Sum of `cost_usd` across every provider × model × pet.
    pub total_cost_usd: f64,
    /// Sum of every token category across every provider / pet.
    pub total_tokens: u64,
}

#[derive(Serialize)]
pub struct PetIdentity {
    pub id: String,
    pub name: String,
    pub template_id: String,
}

#[derive(Serialize)]
pub struct ProviderChip {
    pub provider: String, // canonical slug — "claude" / "codex" / "opencode"
    pub label: String,    // display name — "Claude" / "Codex" / "OpenCode"
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_creation: u64,
    pub tokens_reasoning: u64,
    pub tokens_total: u64,
    pub xp_total: i64,
    pub events: u64,
}

#[derive(Serialize)]
pub struct RecentXp {
    pub occurred_at: String,
    pub xp_delta: i64,
    pub source_type: String, // "usage" | "activity" | "manual"
    pub reason: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Pet name owning this event. `None` in per-pet views (caller
    /// already knows). `Some(name)` in ALL PETS so the moves log
    /// can label each row.
    pub pet_name: Option<String>,
}

/// Dashboard view for one pet. `pet_id = None` ⇒ the currently-active
/// pet (the historical default the frontend used). `pet_id = Some(id)`
/// is for the dashboard sidebar — selecting a different pet thumb
/// re-fetches with that pet's id, read-only (does not change which
/// pet is the live "active companion").
#[tauri::command]
pub async fn dashboard_data(
    state: State<'_, AppState>,
    pet_id: Option<String>,
) -> Result<Option<DashboardData>, String> {
    build_dashboard(&state.db, &state.xp, pet_id.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct ProviderDetail {
    pub provider: String,
    pub label: String,
    pub tokens_total: u64,
    pub events_total: u64,
    /// All-time spend for this provider × pet, in USD.
    pub cost_usd: f64,
    pub models: Vec<ModelRow>,
    /// Per-local-day breakdown for the last `DAY_BREAKDOWN_DAYS`.
    /// Newest day first. Empty if no activity in the window.
    pub by_day: Vec<DayBreakdownRow>,
    /// First page of recent requests (already deduped, most recent
    /// first). Caller paginates older pages via
    /// `dashboard_provider_requests_page` using the timestamp of the
    /// last row as a cursor.
    pub recent_requests: Vec<RequestRow>,
    /// True if there are likely older requests beyond `recent_requests`.
    /// Set to `true` when the initial page filled exactly to its limit.
    /// Used by the frontend to show / hide the "Load more" button.
    pub has_more_requests: bool,
}

#[derive(Serialize)]
pub struct ModelRow {
    pub model: String,
    pub events: u64,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_creation: u64,
    pub tokens_reasoning: u64,
    pub tokens_total: u64,
    /// All-time spend in USD for this (provider, model).
    pub cost_usd: f64,
}

#[derive(Serialize)]
pub struct RequestRow {
    pub timestamp: String,
    pub model: String,
    pub kind: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_creation: u64,
    pub tokens_reasoning: u64,
    pub tokens_total: u64,
    /// Per-request cost in USD computed via `pricing::compute_cost_usd`.
    /// 0.0 for unknown providers / unpriced models — same fallback the
    /// aggregate cost queries use, so a sum of per-request costs lines
    /// up with the model-row total.
    pub cost_usd: f64,
}

#[derive(Serialize)]
pub struct DayBreakdownRow {
    /// `YYYY-MM-DD` in the user's local timezone.
    pub date_local: String,
    pub cost_usd: f64,
    pub total_tokens: u64,
    pub total_events: u64,
    /// Per-model breakdown for this day, sorted by cost descending.
    /// Surfaces "May 16 — 99% Opus 4.7" without a follow-up query.
    pub by_model: Vec<DayModelRow>,
}

#[derive(Serialize)]
pub struct DayModelRow {
    pub model: String,
    pub events: u64,
    pub tokens_total: u64,
    pub cost_usd: f64,
}

/// Pagination response for "Load more" — same shape as the initial
/// page so the frontend reuses its row component.
#[derive(Serialize)]
pub struct ProviderRequestsPage {
    pub requests: Vec<RequestRow>,
    pub has_more: bool,
}

/// Initial page size in `dashboard_provider_detail` AND each "Load
/// more" click. 30 is a sweet spot: small enough to render fast,
/// large enough that the casual reader rarely has to click more
/// than once.
const REQUESTS_PAGE_SIZE: usize = 30;

/// Window for the BY DAY breakdown — 30 local days mirrors how
/// Anthropic's billing UI scopes "Recent activity". Empty days are
/// dropped server-side so the dashboard list stays short.
const DAY_BREAKDOWN_DAYS: u32 = 30;

/// Per-provider drill-down — clicked from the dashboard's TYPE chip.
/// Returns per-model totals, a per-local-day breakdown (last
/// `DAY_BREAKDOWN_DAYS` days), and the first page of individual
/// requests so the developer can audit specific calls. All numbers
/// pet-scoped via `xp_event` join — switching pets gives the new
/// pet's history, not the global one.
#[tauri::command]
pub async fn dashboard_provider_detail(
    state: State<'_, AppState>,
    provider: String,
    pet_id: Option<String>,
) -> Result<ProviderDetail, String> {
    // ALL PETS aggregate drill-down — use the library-wide query
    // variants (no xp_event join) so the chips, by-day chart, and
    // request list all reflect every pet's activity.
    if pet_id.as_deref() == Some(ALL_PETS_SCOPE) {
        return build_provider_detail_all_pets(&state.db, &provider)
            .await
            .map_err(|e| e.to_string());
    }

    let snap = match pet_id.as_deref() {
        Some(id) => state.xp.snapshot_for_pet(id).await,
        None => state.xp.snapshot().await,
    }
    .map_err(|e| e.to_string())?;
    let Some(pet) = snap.pet else {
        return Ok(empty_detail(&provider));
    };

    let models = load_provider_models(&state.db, &pet.id, &provider)
        .await
        .map_err(|e| e.to_string())?;
    let tokens_total: u64 = models.iter().map(|m| m.tokens_total).sum();
    let events_total: u64 = models.iter().map(|m| m.events).sum();
    let cost_usd: f64 = models.iter().map(|m| m.cost_usd).sum();

    let by_day = load_provider_by_day(&state.db, &pet.id, &provider)
        .await
        .map_err(|e| e.to_string())?;

    let recent_rows = state
        .db
        .recent_usage_for_provider_for_pet_before(&pet.id, &provider, None, REQUESTS_PAGE_SIZE)
        .await
        .map_err(|e| e.to_string())?;
    let has_more_requests = recent_rows.len() == REQUESTS_PAGE_SIZE;
    let recent_requests: Vec<RequestRow> = recent_rows
        .into_iter()
        .map(|r| price_request_row(&provider, r))
        .collect();

    Ok(ProviderDetail {
        provider: provider.clone(),
        label: display_label(&provider).to_string(),
        tokens_total,
        events_total,
        cost_usd,
        models,
        by_day,
        recent_requests,
        has_more_requests,
    })
}

/// ALL PETS provider drill-down. Mirrors `dashboard_provider_detail`
/// but uses the library-wide queries that don't filter on `xp_event.
/// pet_id`. The frontend treats the resulting ProviderDetail the
/// same way as a per-pet one.
async fn build_provider_detail_all_pets(
    db: &Arc<DbHandle>,
    provider: &str,
) -> anyhow::Result<ProviderDetail> {
    let provider_id = provider_id_for_pricing(provider);
    let stats = db.stats_for_provider(provider).await?;
    let mut models: Vec<ModelRow> = stats
        .into_iter()
        .map(|r| {
            let total = r.input + r.output + r.cache_read + r.cache_creation + r.reasoning;
            let tokens = TokenDelta {
                input: r.input,
                output: r.output,
                cache_read: r.cache_read,
                cache_creation: r.cache_creation,
                reasoning: r.reasoning,
            };
            let cost_usd = pricing::compute_cost_usd(provider_id, &r.model, &tokens);
            ModelRow {
                model: r.model,
                events: r.events,
                tokens_input: r.input,
                tokens_output: r.output,
                tokens_cache_read: r.cache_read,
                tokens_cache_creation: r.cache_creation,
                tokens_reasoning: r.reasoning,
                tokens_total: total,
                cost_usd,
            }
        })
        .collect();
    models.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.tokens_total.cmp(&a.tokens_total))
    });
    let tokens_total: u64 = models.iter().map(|m| m.tokens_total).sum();
    let events_total: u64 = models.iter().map(|m| m.events).sum();
    let cost_usd: f64 = models.iter().map(|m| m.cost_usd).sum();

    let by_day_raw = cost_query::cost_for_provider_by_day(db, provider, DAY_BREAKDOWN_DAYS).await?;
    let by_day: Vec<DayBreakdownRow> = by_day_raw
        .into_iter()
        .map(|d| DayBreakdownRow {
            date_local: d.date_local,
            cost_usd: d.cost_usd,
            total_tokens: d.total_tokens,
            total_events: d.total_events,
            by_model: d
                .by_model
                .into_iter()
                .map(|m| {
                    let total = m.tokens.input
                        + m.tokens.output
                        + m.tokens.cache_read
                        + m.tokens.cache_creation
                        + m.tokens.reasoning;
                    DayModelRow {
                        model: m.model,
                        events: m.events,
                        tokens_total: total,
                        cost_usd: m.cost_usd,
                    }
                })
                .collect(),
        })
        .collect();

    let recent_rows = db
        .recent_usage_for_provider(provider, REQUESTS_PAGE_SIZE)
        .await?;
    let has_more_requests = recent_rows.len() == REQUESTS_PAGE_SIZE;
    let recent_requests: Vec<RequestRow> = recent_rows
        .into_iter()
        .map(|r| price_request_row(provider, r))
        .collect();

    Ok(ProviderDetail {
        provider: provider.to_string(),
        label: display_label(provider).to_string(),
        tokens_total,
        events_total,
        cost_usd,
        models,
        by_day,
        recent_requests,
        has_more_requests,
    })
}

/// Older-than-`before_timestamp` page of recent requests. Drives the
/// frontend "Load more" button. The frontend tracks the timestamp of
/// the last visible row and passes it back as the cursor.
#[tauri::command]
pub async fn dashboard_provider_requests_page(
    state: State<'_, AppState>,
    provider: String,
    before_timestamp: String,
    pet_id: Option<String>,
) -> Result<ProviderRequestsPage, String> {
    // ALL PETS path — page across every pet's usage events, not a
    // single pet's xp_event join.
    if pet_id.as_deref() == Some(ALL_PETS_SCOPE) {
        let rows = state
            .db
            .recent_usage_for_provider_before(
                &provider,
                Some(before_timestamp),
                REQUESTS_PAGE_SIZE,
            )
            .await
            .map_err(|e| e.to_string())?;
        let has_more = rows.len() == REQUESTS_PAGE_SIZE;
        let requests: Vec<RequestRow> = rows
            .into_iter()
            .map(|r| price_request_row(&provider, r))
            .collect();
        return Ok(ProviderRequestsPage { requests, has_more });
    }

    let snap = match pet_id.as_deref() {
        Some(id) => state.xp.snapshot_for_pet(id).await,
        None => state.xp.snapshot().await,
    }
    .map_err(|e| e.to_string())?;
    let Some(pet) = snap.pet else {
        return Ok(ProviderRequestsPage { requests: Vec::new(), has_more: false });
    };

    let rows = state
        .db
        .recent_usage_for_provider_for_pet_before(
            &pet.id,
            &provider,
            Some(before_timestamp),
            REQUESTS_PAGE_SIZE,
        )
        .await
        .map_err(|e| e.to_string())?;
    let has_more = rows.len() == REQUESTS_PAGE_SIZE;
    let requests: Vec<RequestRow> = rows
        .into_iter()
        .map(|r| price_request_row(&provider, r))
        .collect();
    Ok(ProviderRequestsPage { requests, has_more })
}

// ─── Recently identified models ──────────────────────────────────

/// One row for the "Models in this pet's history" panel.
///
/// Surfaces every distinct (provider, model) pair the pet has logged,
/// annotated with the same classification the scorer uses:
///
/// - `tier`       = the tier resolved by `Registry` → `model.rs` fallback
/// - `confidence` = `exact` when Registry recognised it, `heuristic` when
///                  the name's keywords gave a tier signal, `unknown`
///                  when nothing matched (still emits XP at 0.4×)
/// - `in_registry`= whether `data/models.json` carried an explicit entry
///
/// The frontend uses `tier` to pick a chip colour and `confidence`
/// to decide whether to overlay a yellow "guessed" badge.
#[derive(Serialize)]
pub struct RecentModelRow {
    pub provider: String,
    /// The string as it was recorded in `usage_event` (raw, not
    /// normalized). Frontend displays this verbatim so users see the
    /// model id they recognise.
    pub model: String,
    /// Canonical normalized form after `ModelIdent::parse` — dots
    /// collapsed to dashes, vendor prefix stripped, date suffix removed.
    /// Useful for stable React `key`s.
    pub model_normalized: String,
    pub vendor: String,
    pub family: String,
    /// `frontier` / `mid` / `mini` / `unknown` (lowercase, matches
    /// `Tier::as_str()`).
    pub tier: String,
    /// `exact` / `heuristic` / `unknown`. See struct doc.
    pub confidence: String,
    pub in_registry: bool,
    /// `vendor-official` / `third-party-host` / `back-derived` / `convention`
    /// when in registry; `None` otherwise. Lets the UI explain provenance.
    pub registry_source: Option<String>,
    pub events: u64,
    pub tokens_total: u64,
    /// All-time spend for this (provider, model). 0 for unpriced
    /// models — same fallback the rest of the dashboard uses.
    pub cost_usd: f64,
}

/// Recently-seen models for a pet (or library-wide), classified the
/// same way the scorer classifies them. Drives the dashboard's
/// "Models" panel so users can see which models the pet recognises
/// (Exact, green) versus which it's guessing about (Heuristic, yellow),
/// versus which it couldn't classify at all (Unknown).
///
/// Ordered by tokens_total DESC — most-used model first. Limit caps
/// the result so a verbose pet with 50+ historical models doesn't
/// dump the entire list on first paint.
#[tauri::command]
pub async fn recent_models(
    state: State<'_, AppState>,
    pet_id: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<RecentModelRow>, String> {
    let cap = limit.unwrap_or(20).clamp(1, 200);
    build_recent_models(&state.db, &state.xp, pet_id.as_deref(), cap)
        .await
        .map_err(|e| e.to_string())
}

async fn build_recent_models(
    db: &Arc<DbHandle>,
    xp: &Arc<XPEngine>,
    pet_id: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<RecentModelRow>> {
    // ALL PETS scope: aggregate across the library. The pet-scoped
    // `stats_summary_for_pet` joins through xp_event for accuracy.
    let stats = if pet_id == Some(ALL_PETS_SCOPE) {
        db.stats_summary().await?
    } else {
        let resolved_pet_id = match pet_id {
            Some(id) => Some(id.to_string()),
            None => xp.snapshot().await?.pet.map(|p| p.id),
        };
        match resolved_pet_id {
            Some(id) => db.stats_summary_for_pet(&id).await?,
            None => Vec::new(),
        }
    };

    let mut rows: Vec<RecentModelRow> = stats.into_iter().map(classify_stats_row).collect();
    // Sort by tokens_total DESC so the most-used model leads. Stable
    // tiebreaker on event count keeps the order deterministic across
    // refreshes.
    rows.sort_by(|a, b| {
        b.tokens_total
            .cmp(&a.tokens_total)
            .then_with(|| b.events.cmp(&a.events))
    });
    rows.truncate(limit);
    Ok(rows)
}

/// Convert a raw stats row into a classified registry-aware row.
/// This is the single place that mirrors the scorer's classification
/// logic, so the UI always shows the same `(tier, confidence)` the
/// algorithm would apply to the next event for this model.
fn classify_stats_row(s: petpet::db::StatsRow) -> RecentModelRow {
    let ident = ModelIdent::parse(&s.model);
    let registry_hit = Registry::bundled().lookup(&s.model);

    // (Tier, Confidence) — identical decision tree to `scorer::usage::classify`.
    let (tier, confidence) = if ident.tier != Tier::Unknown {
        (ident.tier, "exact")
    } else {
        match fallback_tier(&ident.model) {
            FallbackResult::Confident(t) => (t, "heuristic"),
            FallbackResult::Default => (Tier::Mid, "unknown"),
        }
    };

    let tokens = TokenDelta {
        input: s.input,
        output: s.output,
        cache_read: s.cache_read,
        cache_creation: s.cache_creation,
        reasoning: s.reasoning,
    };
    let provider_id = provider_id_for_pricing(&s.provider);
    let cost_usd = pricing::compute_cost_usd(provider_id, &s.model, &tokens);
    let tokens_total = s.input + s.output + s.cache_read + s.cache_creation + s.reasoning;

    let (in_registry, registry_source) = match registry_hit.as_ref() {
        Some(entry) => (true, Some(entry.source.source_type.to_string())),
        None => (false, None),
    };

    RecentModelRow {
        provider: s.provider,
        model: s.model,
        model_normalized: ident.model,
        vendor: ident.vendor.as_str().to_string(),
        family: ident.family,
        tier: tier.as_str().to_string(),
        confidence: confidence.to_string(),
        in_registry,
        registry_source,
        events: s.events,
        tokens_total,
        cost_usd,
    }
}

// ─── Helpers ─────────────────────────────────────────────────────

fn empty_detail(provider: &str) -> ProviderDetail {
    ProviderDetail {
        provider: provider.to_string(),
        label: display_label(provider).to_string(),
        tokens_total: 0,
        events_total: 0,
        cost_usd: 0.0,
        models: Vec::new(),
        by_day: Vec::new(),
        recent_requests: Vec::new(),
        has_more_requests: false,
    }
}

/// Map a slug to ProviderId for pricing lookup, with fallback to
/// CustomApi for unknown slugs. Mirrors `cost_query`'s fallback —
/// keeps per-request cost and aggregate cost on the same code path so
/// the sums reconcile.
fn provider_id_for_pricing(slug: &str) -> ProviderId {
    ProviderId::from_slug(slug)
        .or_else(|| match slug {
            "claude_code" => Some(ProviderId::ClaudeCode),
            "codex" => Some(ProviderId::Codex),
            "gemini" => Some(ProviderId::Gemini),
            "opencode" => Some(ProviderId::OpenCode),
            "aider" => Some(ProviderId::Aider),
            "custom_api" => Some(ProviderId::CustomApi),
            _ => None,
        })
        .unwrap_or(ProviderId::CustomApi)
}

fn price_request_row(provider: &str, r: petpet::db::RecentUsageRow) -> RequestRow {
    let total = r.tokens_input
        + r.tokens_output
        + r.tokens_cache_read
        + r.tokens_cache_creation
        + r.tokens_reasoning;
    let tokens = TokenDelta {
        input: r.tokens_input,
        output: r.tokens_output,
        cache_read: r.tokens_cache_read,
        cache_creation: r.tokens_cache_creation,
        reasoning: r.tokens_reasoning,
    };
    let cost_usd = pricing::compute_cost_usd(provider_id_for_pricing(provider), &r.model, &tokens);
    RequestRow {
        timestamp: r.timestamp,
        model: r.model,
        kind: r.kind,
        tokens_input: r.tokens_input,
        tokens_output: r.tokens_output,
        tokens_cache_read: r.tokens_cache_read,
        tokens_cache_creation: r.tokens_cache_creation,
        tokens_reasoning: r.tokens_reasoning,
        tokens_total: total,
        cost_usd,
    }
}

async fn load_provider_models(
    db: &Arc<DbHandle>,
    pet_id: &str,
    provider: &str,
) -> anyhow::Result<Vec<ModelRow>> {
    let stats = db.stats_for_provider_for_pet(pet_id, provider).await?;
    let provider_id = provider_id_for_pricing(provider);
    let models: Vec<ModelRow> = stats
        .into_iter()
        .map(|r| {
            let total = r.input + r.output + r.cache_read + r.cache_creation + r.reasoning;
            let tokens = TokenDelta {
                input: r.input,
                output: r.output,
                cache_read: r.cache_read,
                cache_creation: r.cache_creation,
                reasoning: r.reasoning,
            };
            let cost_usd = pricing::compute_cost_usd(provider_id, &r.model, &tokens);
            ModelRow {
                model: r.model,
                events: r.events,
                tokens_input: r.input,
                tokens_output: r.output,
                tokens_cache_read: r.cache_read,
                tokens_cache_creation: r.cache_creation,
                tokens_reasoning: r.reasoning,
                tokens_total: total,
                cost_usd,
            }
        })
        .collect();
    // Stats already sorted by total tokens DESC at the SQL layer;
    // re-sort by cost so the table mirrors the BY DAY ordering.
    let mut models = models;
    models.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.tokens_total.cmp(&a.tokens_total))
    });
    Ok(models)
}

async fn load_provider_by_day(
    db: &Arc<DbHandle>,
    pet_id: &str,
    provider: &str,
) -> anyhow::Result<Vec<DayBreakdownRow>> {
    let days = cost_query::cost_for_provider_for_pet_by_day(db, pet_id, provider, DAY_BREAKDOWN_DAYS).await?;
    Ok(days
        .into_iter()
        .map(|d| DayBreakdownRow {
            date_local: d.date_local,
            cost_usd: d.cost_usd,
            total_tokens: d.total_tokens,
            total_events: d.total_events,
            by_model: d
                .by_model
                .into_iter()
                .map(|m| {
                    let total = m.tokens.input
                        + m.tokens.output
                        + m.tokens.cache_read
                        + m.tokens.cache_creation
                        + m.tokens.reasoning;
                    DayModelRow {
                        model: m.model,
                        events: m.events,
                        tokens_total: total,
                        cost_usd: m.cost_usd,
                    }
                })
                .collect(),
        })
        .collect())
}

async fn build_dashboard(
    db: &Arc<DbHandle>,
    xp: &Arc<XPEngine>,
    pet_id: Option<&str>,
) -> anyhow::Result<Option<DashboardData>> {
    // ALL PETS aggregate path — sidebar's special tile passes the
    // `__all__` sentinel; we short-circuit to a library-wide build
    // that doesn't reference any single pet.
    if pet_id == Some(ALL_PETS_SCOPE) {
        return build_dashboard_all_pets(db).await.map(Some);
    }

    // Snapshot is the single source of truth for level / XP / stage —
    // identical to what the hover bubble shows so the dashboard
    // doesn't disagree with it. When `pet_id` is supplied, fetch THAT
    // pet's snapshot (read-only); otherwise default to whichever pet
    // is currently active.
    let snap = match pet_id {
        Some(id) => xp.snapshot_for_pet(id).await?,
        None => xp.snapshot().await?,
    };
    let Some(pet) = snap.pet else {
        return Ok(None); // no such pet (or no active pet)
    };
    let state = snap.state.unwrap_or_else(|| petpet::xp::engine::XPStateView {
        total_xp: 0,
        current_level: 0,
        xp_in_level: 0,
        xp_for_next_level: None,
        stage_level: 0,
    });

    // Resolve the current sprite path. The template registry tells us
    // where the bundled stages live on disk; the snapshot tells us
    // which stage_id the pet is currently at. Egg stage has no PNG;
    // we report `None` and the frontend falls back to a procedural
    // SVG.
    let stage_id = snap.stage.as_ref().map(|s| s.sprite_key.clone());
    let stage_name = snap.stage.as_ref().map(|s| s.name.clone());
    let sprite_path = sprite_path_for(&pet.template_id, stage_id.as_deref())?;

    // Per-provider chips: join pet-scoped stats (tokens) with
    // xp_by_provider (xp). We use `stats_summary_for_pet` (NOT the
    // global `stats_summary`) so the token totals match the XP totals
    // semantically — both are "what this pet earned XP from", not
    // "everything you've ever sent through Claude". Otherwise the
    // dashboard would inflate a newly-hatched pet's token counts
    // with history from a previously-active pet.
    let stats = db.stats_summary_for_pet(&pet.id).await?;
    let mut by_provider: std::collections::BTreeMap<String, ProviderChip> =
        std::collections::BTreeMap::new();
    for row in stats {
        let entry = by_provider
            .entry(row.provider.clone())
            .or_insert_with(|| ProviderChip {
                provider: row.provider.clone(),
                label: display_label(&row.provider).to_string(),
                tokens_input: 0,
                tokens_output: 0,
                tokens_cache_read: 0,
                tokens_cache_creation: 0,
                tokens_reasoning: 0,
                tokens_total: 0,
                xp_total: 0,
                events: 0,
            });
        entry.tokens_input += row.input;
        entry.tokens_output += row.output;
        entry.tokens_cache_read += row.cache_read;
        entry.tokens_cache_creation += row.cache_creation;
        entry.tokens_reasoning += row.reasoning;
        entry.events += row.events;
    }
    for c in by_provider.values_mut() {
        c.tokens_total = c.tokens_input
            + c.tokens_output
            + c.tokens_cache_read
            + c.tokens_cache_creation
            + c.tokens_reasoning;
    }

    let xp_rows = db.xp_by_provider(&pet.id).await?;
    let mut other_xp: i64 = 0;
    for row in xp_rows {
        match row.provider {
            Some(p) => {
                let entry = by_provider.entry(p.clone()).or_insert_with(|| ProviderChip {
                    provider: p.clone(),
                    label: display_label(&p).to_string(),
                    tokens_input: 0,
                    tokens_output: 0,
                    tokens_cache_read: 0,
                    tokens_cache_creation: 0,
                    tokens_reasoning: 0,
                    tokens_total: 0,
                    xp_total: 0,
                    events: 0,
                });
                entry.xp_total += row.xp_total;
            }
            None => {
                // Activity-sourced or manual XP — surfaced separately.
                other_xp += row.xp_total;
            }
        }
    }

    // Sort chips by xp_total desc, then by tokens_total desc, so the
    // most-used provider lands leftmost.
    let mut providers: Vec<ProviderChip> = by_provider.into_values().collect();
    providers.sort_by(|a, b| {
        b.xp_total
            .cmp(&a.xp_total)
            .then_with(|| b.tokens_total.cmp(&a.tokens_total))
    });

    // Days raised — clamp to >=0 so a born_at slightly in the future
    // (clock skew) doesn't print "-1 days".
    let days_raised = {
        let dur = chrono::Utc::now().signed_duration_since(pet.born_at);
        dur.num_days().max(0)
    };

    let recent = db
        .recent_xp_events(&pet.id, RECENT_XP_LIMIT)
        .await?
        .into_iter()
        .map(|r| RecentXp {
            occurred_at: r.occurred_at,
            xp_delta: r.xp_delta,
            source_type: r.source_type,
            reason: r.reason,
            provider: r.provider,
            model: r.model,
            pet_name: r.pet_name, // None in per-pet path
        })
        .collect();

    Ok(Some(DashboardData {
        pet: Some(PetIdentity {
            id: pet.id,
            name: pet.name,
            template_id: pet.template_id,
        }),
        level: state.current_level,
        total_xp: state.total_xp,
        xp_in_level: state.xp_in_level,
        xp_for_next_level: state.xp_for_next_level,
        stage_name,
        stage_id,
        sprite_path,
        days_raised,
        next_evolution_name: snap.next_evolution.as_ref().map(|n| n.name.clone()),
        next_evolution_xp_to: snap.next_evolution.as_ref().map(|n| n.xp_to_next),
        providers,
        other_xp,
        recent,
        aggregate: None,
    }))
}

/// ALL PETS aggregate build — fired when the caller passes
/// `ALL_PETS_SCOPE`. Produces the same `DashboardData` shape with
/// `pet: None` and the `aggregate` field populated; the frontend
/// renders an alternative identity row keyed on `pet.is_none()`.
///
/// Reuses the library-wide DB queries:
///   - `stats_summary` for provider × model token totals
///   - `xp_by_provider_all` for provider XP totals + "other XP"
///   - `recent_xp_events_all` for the moves log (with pet_name)
///   - `pet_library_aggregates` for the headline numbers
async fn build_dashboard_all_pets(db: &Arc<DbHandle>) -> anyhow::Result<DashboardData> {
    let stats = db.stats_summary().await?;
    let mut by_provider: std::collections::BTreeMap<String, ProviderChip> =
        std::collections::BTreeMap::new();
    for row in stats {
        let entry = by_provider
            .entry(row.provider.clone())
            .or_insert_with(|| ProviderChip {
                provider: row.provider.clone(),
                label: display_label(&row.provider).to_string(),
                tokens_input: 0,
                tokens_output: 0,
                tokens_cache_read: 0,
                tokens_cache_creation: 0,
                tokens_reasoning: 0,
                tokens_total: 0,
                xp_total: 0,
                events: 0,
            });
        entry.tokens_input += row.input;
        entry.tokens_output += row.output;
        entry.tokens_cache_read += row.cache_read;
        entry.tokens_cache_creation += row.cache_creation;
        entry.tokens_reasoning += row.reasoning;
        entry.events += row.events;
    }
    for c in by_provider.values_mut() {
        c.tokens_total = c.tokens_input
            + c.tokens_output
            + c.tokens_cache_read
            + c.tokens_cache_creation
            + c.tokens_reasoning;
    }

    let xp_rows = db.xp_by_provider_all().await?;
    let mut other_xp: i64 = 0;
    for row in xp_rows {
        match row.provider {
            Some(p) => {
                let entry = by_provider.entry(p.clone()).or_insert_with(|| ProviderChip {
                    provider: p.clone(),
                    label: display_label(&p).to_string(),
                    tokens_input: 0,
                    tokens_output: 0,
                    tokens_cache_read: 0,
                    tokens_cache_creation: 0,
                    tokens_reasoning: 0,
                    tokens_total: 0,
                    xp_total: 0,
                    events: 0,
                });
                entry.xp_total += row.xp_total;
            }
            None => other_xp += row.xp_total,
        }
    }

    let mut providers: Vec<ProviderChip> = by_provider.into_values().collect();
    providers.sort_by(|a, b| {
        b.xp_total
            .cmp(&a.xp_total)
            .then_with(|| b.tokens_total.cmp(&a.tokens_total))
    });

    // Aggregate headline numbers for the identity row. Cost is the
    // sum of per-provider × per-model pricings — same code path the
    // per-pet dashboard takes for its provider drill-downs, just
    // run against the global `stats_for_provider` (no xp_event join).
    // Pricing on the read path means a future rate change re-prices
    // history automatically.
    let lib = db.pet_library_aggregates().await?;
    let oldest_days = lib
        .oldest_born_at
        .map(|d| chrono::Utc::now().signed_duration_since(d).num_days().max(0))
        .unwrap_or(0);

    let mut total_cost_usd: f64 = 0.0;
    for chip in &providers {
        let provider_id = provider_id_for_pricing(&chip.provider);
        let model_rows = db.stats_for_provider(&chip.provider).await?;
        for r in model_rows {
            let tokens = TokenDelta {
                input: r.input,
                output: r.output,
                cache_read: r.cache_read,
                cache_creation: r.cache_creation,
                reasoning: r.reasoning,
            };
            total_cost_usd += pricing::compute_cost_usd(provider_id, &r.model, &tokens);
        }
    }

    let total_tokens: u64 = providers.iter().map(|p| p.tokens_total).sum();

    let recent = db
        .recent_xp_events_all(RECENT_XP_LIMIT)
        .await?
        .into_iter()
        .map(|r| RecentXp {
            occurred_at: r.occurred_at,
            xp_delta: r.xp_delta,
            source_type: r.source_type,
            reason: r.reason,
            provider: r.provider,
            model: r.model,
            pet_name: r.pet_name,
        })
        .collect();

    Ok(DashboardData {
        pet: None,
        // No single-pet level/stage in aggregate mode; frontend keys
        // off `pet.is_none()` and uses the `aggregate` block instead.
        level: 0,
        total_xp: lib.total_xp,
        xp_in_level: 0,
        xp_for_next_level: None,
        stage_name: None,
        stage_id: None,
        sprite_path: None,
        days_raised: oldest_days,
        next_evolution_name: None,
        next_evolution_xp_to: None,
        providers,
        other_xp,
        recent,
        aggregate: Some(AllPetsAggregate {
            pet_count: lib.pet_count,
            oldest_days,
            total_xp: lib.total_xp,
            total_cost_usd,
            total_tokens,
        }),
    })
}

/// Map a provider slug to a friendly display label. Centralised here
/// so the dashboard's chip text matches the rest of the UI (info
/// bubble, ceremony bubble, etc.) and a future label rename is one
/// place to change.
fn display_label(slug: &str) -> &'static str {
    match slug {
        "claude_code" | "claude" => "Claude",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "gemini" => "Gemini",
        "custom_api" | "custom" => "Custom",
        _ => "Other",
    }
}

/// Resolve the absolute on-disk path to the current stage's
/// `sprite.png`. Returns `None` if the file doesn't exist (notably
/// stage_0 which ships no PNG) — the frontend handles that branch by
/// rendering the procedural egg SVG.
fn sprite_path_for(template_id: &str, stage_id: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(stage_id) = stage_id else { return Ok(None) };
    let templates = TemplateRegistry::discover()?;
    let dir: Option<PathBuf> = templates
        .into_iter()
        .find(|t| t.template.meta.id == template_id)
        .map(|t| t.dir);
    let Some(dir) = dir else { return Ok(None) };
    let p = dir.join("stages").join(stage_id).join("sprite.png");
    if p.exists() {
        Ok(Some(p.to_string_lossy().to_string()))
    } else {
        Ok(None)
    }
}
