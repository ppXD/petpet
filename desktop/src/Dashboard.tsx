/**
 * Dashboard — "trainer card" view of the active pet's stats.
 *
 * Sections (top to bottom, mirrors the GBA trainer-card structure):
 *
 *   1. Identity  — sprite + name + Lv.N + stage + XP bar + days raised
 *   2. Types     — one chip per provider (Claude / Codex / OpenCode)
 *                  showing total tokens + XP earned from that provider.
 *                  "Other XP" chip on the right covers activity-hook
 *                  XP and manual grants (no provider attribution).
 *   3. Moves     — recent N XP events as a battle-log scroll.
 *
 * Lives in the main window (takeover mode, like the PetSwitcher and
 * EggPicker). Open via right-click menu → "Dashboard…" → main window
 * resizes to picker size, centred on screen. Back button returns to
 * the compact floating pet.
 */

import { useEffect, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { Pet } from "./Pet";
import "./Dashboard.css";

/// Sentinel `pet_id` value sent to the backend to request the
/// library-wide aggregate dashboard (the "ALL PETS" sidebar tile).
/// Matches `ALL_PETS_SCOPE` in `desktop/src-tauri/src/dashboard.rs`.
const ALL_PETS_SCOPE = "__all__";

interface DashboardData {
  /// `null` ⇒ this is the ALL PETS aggregate view; the frontend
  /// renders `AllPetsIdentitySection` instead of the per-pet
  /// `IdentitySection`, and uses `aggregate` for the headline numbers.
  pet: { id: string; name: string; template_id: string } | null;
  level: number;
  total_xp: number;
  xp_in_level: number;
  xp_for_next_level: number | null;
  stage_name: string | null;
  stage_id: string | null;
  sprite_path: string | null;
  days_raised: number;
  next_evolution_name: string | null;
  next_evolution_xp_to: number | null;
  providers: ProviderChip[];
  other_xp: number;
  recent: RecentXp[];
  /// Only present in ALL PETS mode.
  aggregate: AllPetsAggregate | null;
}

interface AllPetsAggregate {
  pet_count: number;
  oldest_days: number;
  total_xp: number;
  total_cost_usd: number;
  total_tokens: number;
}

interface ProviderChip {
  provider: string;
  label: string;
  tokens_input: number;
  tokens_output: number;
  tokens_cache_read: number;
  tokens_cache_creation: number;
  tokens_reasoning: number;
  tokens_total: number;
  xp_total: number;
  events: number;
}

interface RecentXp {
  occurred_at: string;
  xp_delta: number;
  source_type: string;
  reason: string | null;
  provider: string | null;
  model: string | null;
  /// Set only in ALL PETS mode. Lets the moves log label each row
  /// with which pet earned the XP.
  pet_name: string | null;
}

interface Props {
  onClose: () => void;
}

interface ProviderDetail {
  provider: string;
  label: string;
  tokens_total: number;
  events_total: number;
  cost_usd: number;
  models: ModelRow[];
  by_day: DayBreakdownRow[];
  recent_requests: RequestRow[];
  has_more_requests: boolean;
}
interface ModelRow {
  model: string;
  events: number;
  tokens_input: number;
  tokens_output: number;
  tokens_cache_read: number;
  tokens_cache_creation: number;
  tokens_reasoning: number;
  tokens_total: number;
  cost_usd: number;
}
interface RequestRow {
  timestamp: string;
  model: string;
  kind: string;
  tokens_input: number;
  tokens_output: number;
  tokens_cache_read: number;
  tokens_cache_creation: number;
  tokens_reasoning: number;
  tokens_total: number;
  cost_usd: number;
}
interface DayBreakdownRow {
  date_local: string; // YYYY-MM-DD (local tz)
  cost_usd: number;
  total_tokens: number;
  total_events: number;
  by_model: DayModelRow[];
}
interface DayModelRow {
  model: string;
  events: number;
  tokens_total: number;
  cost_usd: number;
}
/// One row of the "Models" panel — mirrors `dashboard.rs::RecentModelRow`.
/// The `confidence` field drives the yellow "guessed" badge; `in_registry`
/// is currently unused by the UI but kept around for a future "this is
/// from data/models.json" tooltip.
interface RecentModelRow {
  provider: string;
  model: string;
  model_normalized: string;
  vendor: string;
  family: string;
  tier: "frontier" | "mid" | "mini" | "unknown";
  confidence: "exact" | "heuristic" | "unknown";
  in_registry: boolean;
  registry_source: string | null;
  events: number;
  tokens_total: number;
  cost_usd: number;
}
interface ProviderRequestsPage {
  requests: RequestRow[];
  has_more: boolean;
}

type View =
  | { kind: "overview" }
  | { kind: "provider"; slug: string }
  | {
      kind: "requests";
      slug: string;
      label: string;
      initialRows: RequestRow[];
      initialHasMore: boolean;
      eventsTotal: number;
    };

/// Sidebar entry — one per pet. Same data shape that PetSwitcher uses
/// via `pet_list_summaries`. Re-declared locally so the Dashboard's
/// type chain doesn't reach into PetSwitcher.
interface PetSummary {
  pet: {
    id: string;
    name: string;
    template_id: string;
    snapshot_path: string;
    is_active: boolean;
    born_at: string;
  };
  current_level: number;
  total_xp: number;
  stage_name: string;
  stage_id: string;
  sprite_path: string;
}

export function Dashboard({ onClose }: Props) {
  const [summaries, setSummaries] = useState<PetSummary[] | null>(null);
  // Currently-selected pet for the sidebar. `null` = "use the active
  // pet" (default on first load; resolved after summaries arrive).
  const [selectedPetId, setSelectedPetId] = useState<string | null>(null);
  const [data, setData] = useState<DashboardData | null>(null);
  const [models, setModels] = useState<RecentModelRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Two-level navigation stack: overview ↔ provider drill-down. Back
  // button context-sensitive — drill-down's Back returns to overview,
  // overview's Back closes the dashboard entirely.
  const [view, setView] = useState<View>({ kind: "overview" });

  // 1. Load the pet sidebar (one tile per pet). The pet flagged as
  //    `is_active` becomes the default selection so the dashboard
  //    opens to the user's live companion.
  useEffect(() => {
    let cancelled = false;
    invoke<PetSummary[]>("pet_list_summaries")
      .then((list) => {
        if (cancelled) return;
        setSummaries(list);
        if (selectedPetId === null) {
          const active = list.find((s) => s.pet.is_active);
          setSelectedPetId(active?.pet.id ?? list[0]?.pet.id ?? null);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
    // Only refetch when the user explicitly returns to the dashboard
    // (parent remounts this component on each open).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 2. Whenever the selected pet changes, fetch THAT pet's dashboard.
  //    Reset any in-flight drill-down — provider / requests views
  //    were scoped to the previous pet's data.
  useEffect(() => {
    if (!selectedPetId) return;
    let cancelled = false;
    setData(null);
    setModels(null);
    setView({ kind: "overview" });
    invoke<DashboardData | null>("dashboard_data", { petId: selectedPetId })
      .then((d) => {
        if (cancelled) return;
        if (!d) setError("Pet has no snapshot yet — try hatching first.");
        else {
          setError(null);
          setData(d);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    // Recent models in parallel — the panel renders independently so a
    // slow models query doesn't block the trainer card. limit=20 covers
    // a verbose pet's full unique-model set without overflow.
    invoke<RecentModelRow[]>("recent_models", { petId: selectedPetId, limit: 20 })
      .then((rows) => {
        if (!cancelled) setModels(rows);
      })
      .catch(() => {
        // Non-fatal — the dashboard works without this panel.
        if (!cancelled) setModels([]);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedPetId]);

  // Error pane — keep the sidebar visible so the user can switch to
  // a working pet rather than dead-ending into a generic error page.
  if (error && !summaries) {
    return (
      <div className="dash-overlay">
        <div className="gba-box dash-box">
          <div className="picker-error">⚠ {error}</div>
          <div className="picker-actions">
            <button className="gba-button" onClick={onClose}>
              Back
            </button>
          </div>
        </div>
      </div>
    );
  }

  if (view.kind === "provider") {
    return (
      <ProviderDetailView
        slug={view.slug}
        petId={selectedPetId}
        onBack={() => setView({ kind: "overview" })}
        onShowRequests={(label, initialRows, initialHasMore, eventsTotal) =>
          setView({
            kind: "requests",
            slug: view.slug,
            label,
            initialRows,
            initialHasMore,
            eventsTotal,
          })
        }
      />
    );
  }

  if (view.kind === "requests") {
    return (
      <RequestsPopupView
        slug={view.slug}
        label={view.label}
        eventsTotal={view.eventsTotal}
        petId={selectedPetId}
        initialRows={view.initialRows}
        initialHasMore={view.initialHasMore}
        onBack={() => setView({ kind: "provider", slug: view.slug })}
      />
    );
  }

  return (
    <div className="dash-overlay">
      {/* One unified GBA-box frame around BOTH sidebar and main pane.
       *  No explicit divider line — the sidebar's parchment tint
       *  sitting against the main pane's white background does the
       *  separation work, matching the Octopus-Deploy-style nav
       *  pattern the user referenced. Tile cards on the parchment
       *  add the "colored block" visual structure. */}
      <div className="gba-box dash-shell">
        <DashboardSidebar
          summaries={summaries}
          selectedPetId={selectedPetId}
          onSelect={setSelectedPetId}
        />
        <div className="dash-main">
          <div className="gba-title">TRAINER CARD</div>

          {error && <div className="picker-error">⚠ {error}</div>}
          {!data && !error && <div className="dash-loading">Loading…</div>}

          {data && (
            <>
              {data.aggregate ? (
                <AllPetsIdentitySection aggregate={data.aggregate} />
              ) : (
                <IdentitySection data={data} />
              )}
              <TypesSection
                data={data}
                onChipClick={(slug) => setView({ kind: "provider", slug })}
              />
              <MovesSection recent={data.recent} />
              {models && models.length > 0 && <ModelsSection models={models} />}
            </>
          )}

          <div className="picker-actions dash-actions">
            <button className="gba-button" onClick={onClose}>
              Back
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/// Vertical strip of pet thumbnails on the left side of the dashboard.
/// First tile is always the "ALL PETS" aggregate tile (clicking it
/// switches the main pane to library-wide stats). Subsequent tiles
/// are individual pets, active-first.
///
/// Selecting a tile re-fetches the dashboard for that scope without
/// changing which pet is the live "active companion" — purely
/// read-only inspection.
function DashboardSidebar({
  summaries,
  selectedPetId,
  onSelect,
}: {
  summaries: PetSummary[] | null;
  selectedPetId: string | null;
  onSelect: (petId: string) => void;
}) {
  if (!summaries) {
    return <div className="dash-sidebar dash-sidebar-loading">…</div>;
  }
  // Active pet first, then everything else in original order. Stable
  // across re-renders because we filter+concat instead of sorting in
  // place.
  const ordered = (() => {
    const active = summaries.find((s) => s.pet.is_active);
    const others = summaries.filter((s) => !s.pet.is_active);
    return active ? [active, ...others] : summaries;
  })();
  return (
    <div className="dash-sidebar">
      <SidebarAllTile
        selected={selectedPetId === ALL_PETS_SCOPE}
        onClick={() => onSelect(ALL_PETS_SCOPE)}
        petCount={summaries.length}
      />
      {ordered.map((s) => (
        <SidebarPetTile
          key={s.pet.id}
          summary={s}
          selected={s.pet.id === selectedPetId}
          onClick={() => onSelect(s.pet.id)}
        />
      ))}
    </div>
  );
}

/// "ALL PETS" sidebar tile — special row at the top of the sidebar
/// that triggers the library-wide aggregate view. Visually distinct
/// from pet tiles (no sprite; a stacked-tiles glyph + count) so the
/// user understands this isn't a real pet.
function SidebarAllTile({
  selected,
  onClick,
  petCount,
}: {
  selected: boolean;
  onClick: () => void;
  petCount: number;
}) {
  return (
    <button
      type="button"
      className={`dash-sidebar-tile dash-sidebar-tile-all${
        selected ? " is-selected" : ""
      }`}
      onClick={onClick}
      title={`Aggregate stats across all ${petCount} ${petCount === 1 ? "pet" : "pets"}`}
    >
      <div className="dash-sidebar-tile-sprite dash-sidebar-tile-all-icon">
        {/* Three stacked offset squares: visually says "all of them".
         *  Pure SVG so it scales with the tile sprite slot. */}
        <svg
          viewBox="0 0 24 24"
          width="36"
          height="36"
          shapeRendering="crispEdges"
          aria-hidden="true"
        >
          <rect x="2" y="8" width="13" height="13" fill="#c8a8ff" stroke="#1a1a1a" strokeWidth="1.5" />
          <rect x="6" y="5" width="13" height="13" fill="#ffc850" stroke="#1a1a1a" strokeWidth="1.5" />
          <rect x="9" y="2" width="13" height="13" fill="#2e8b57" stroke="#1a1a1a" strokeWidth="1.5" />
        </svg>
      </div>
      <div className="dash-sidebar-tile-name">All</div>
      <div className="dash-sidebar-tile-lvl">
        {petCount} {petCount === 1 ? "pet" : "pets"}
      </div>
    </button>
  );
}

function SidebarPetTile({
  summary,
  selected,
  onClick,
}: {
  summary: PetSummary;
  selected: boolean;
  onClick: () => void;
}) {
  // Show the real sprite whenever the backend gave us a path —
  // procedural <Pet> only kicks in as a fallback when no sprite is
  // available (legacy templates that ship no stage_0 art). The
  // previous heuristic was `stage_id === "stage_0" || !sprite_path`,
  // which assumed stage_0 always meant "use the procedural egg" —
  // true for the retired mist/ember/onyx but wrong for sun and
  // unicorn, which both ship a real stage_0 PNG. Mirrors PetSwitcher's
  // simpler check.
  const usePng = !!summary.sprite_path;
  return (
    <button
      type="button"
      className={`dash-sidebar-tile${selected ? " is-selected" : ""}${
        summary.pet.is_active ? " is-active" : ""
      }`}
      onClick={onClick}
      title={`${summary.pet.name} · Lv.${summary.current_level} · ${summary.stage_name}${
        summary.pet.is_active ? " · active" : ""
      }`}
    >
      <div className="dash-sidebar-tile-sprite">
        {usePng ? (
          <img
            src={convertFileSrc(summary.sprite_path)}
            alt=""
            className="dash-sidebar-tile-img"
            onError={(e) => {
              (e.target as HTMLImageElement).style.visibility = "hidden";
            }}
          />
        ) : (
          <Pet stageIndex={0} mood="idle" />
        )}
      </div>
      <div className="dash-sidebar-tile-name" title={summary.pet.name}>
        {summary.pet.name}
      </div>
      <div className="dash-sidebar-tile-lvl">Lv.{summary.current_level}</div>
      {summary.pet.is_active && (
        <span className="dash-sidebar-tile-dot" aria-hidden="true" />
      )}
    </button>
  );
}

// ─── Identity ──────────────────────────────────────────────────────

/// Replaces `IdentitySection` when the dashboard is in ALL PETS mode.
/// Shows four library-wide aggregate stats — total spend, total XP,
/// pet count + days, total tokens — in a compact grid. No sprite,
/// no XP-to-next bar (those are pet-specific concepts that don't
/// apply at the library level).
function AllPetsIdentitySection({
  aggregate,
}: {
  aggregate: AllPetsAggregate;
}) {
  const days = aggregate.oldest_days;
  return (
    <div className="dash-identity dash-identity-all">
      <div className="dash-identity-all-header">
        <span className="dash-identity-all-title">ALL PETS</span>
        <span className="dash-identity-all-sub">
          {aggregate.pet_count} {aggregate.pet_count === 1 ? "pet" : "pets"}
          {" · "}
          oldest raised {days} {days === 1 ? "day" : "days"}
        </span>
      </div>
      <div className="dash-identity-all-grid">
        <div className="dash-identity-all-stat">
          <span className="dash-identity-all-stat-label">Spend</span>
          <span
            className="dash-identity-all-stat-value dash-identity-all-stat-cost"
            title={`$${aggregate.total_cost_usd.toFixed(4)} across all pets`}
          >
            ${aggregate.total_cost_usd.toFixed(2)}
          </span>
        </div>
        <div className="dash-identity-all-stat">
          <span className="dash-identity-all-stat-label">Total XP</span>
          <span
            className="dash-identity-all-stat-value"
            title={aggregate.total_xp.toLocaleString()}
          >
            {fmtCompactInt(aggregate.total_xp)}
          </span>
        </div>
        <div className="dash-identity-all-stat">
          <span className="dash-identity-all-stat-label">Tokens</span>
          <span
            className="dash-identity-all-stat-value"
            title={aggregate.total_tokens.toLocaleString()}
          >
            {fmtTokens(aggregate.total_tokens)}
          </span>
        </div>
      </div>
    </div>
  );
}

function IdentitySection({ data }: { data: DashboardData }) {
  const xpProgress =
    data.xp_for_next_level === null
      ? 1
      : data.xp_in_level / (data.xp_in_level + data.xp_for_next_level);

  return (
    <div className="dash-identity">
      <div className="dash-sprite">
        {data.sprite_path ? (
          <img
            src={convertFileSrc(data.sprite_path)}
            alt=""
            className="dash-sprite-img"
            onError={(e) => {
              (e.target as HTMLImageElement).style.visibility = "hidden";
            }}
          />
        ) : (
          // Egg-stage or missing-asset fallback — same approach as
          // PetSwitcher: render the procedural egg via the Pet
          // component so the avatar isn't blank.
          <div className="dash-sprite-svg">
            <Pet stageIndex={0} mood="idle" />
          </div>
        )}
      </div>

      <div className="dash-id-text">
        <div className="dash-name-row">
          {/* `data.pet` is non-null in per-pet mode (the only code
           *  path that renders IdentitySection — the parent guards
           *  on `data.aggregate`). Defensive `??` fallback in case
           *  the contract ever changes. */}
          <span className="dash-name">{data.pet?.name ?? "—"}</span>
          <span className="dash-lvl">Lv.{data.level}</span>
        </div>
        <div className="dash-stage">{data.stage_name ?? "—"}</div>
        <div className="dash-xp-bar">
          <div
            className="dash-xp-bar-fill"
            style={{ width: `${Math.min(100, xpProgress * 100)}%` }}
          />
        </div>
        <div className="dash-xp-numbers" title={`${data.total_xp.toLocaleString()} XP total`}>
          {data.xp_for_next_level === null ? (
            <span>MAX · {fmtCompactInt(data.total_xp)} XP total</span>
          ) : (
            <span>
              {fmtCompactInt(data.xp_in_level)} /{" "}
              {fmtCompactInt(data.xp_in_level + data.xp_for_next_level)} XP
              {data.next_evolution_name && data.next_evolution_xp_to !== null && (
                <>
                  {" · "}
                  → <b>{data.next_evolution_name}</b> in{" "}
                  {fmtCompactInt(data.next_evolution_xp_to)} XP
                </>
              )}
            </span>
          )}
        </div>
        <div
          className="dash-raised"
          title={`${data.total_xp.toLocaleString()} XP total`}
        >
          raised {data.days_raised} {data.days_raised === 1 ? "day" : "days"} · total{" "}
          {fmtCompactInt(data.total_xp)} XP
        </div>
      </div>
    </div>
  );
}

// ─── Types (provider chips) ────────────────────────────────────────

function TypesSection({
  data,
  onChipClick,
}: {
  data: DashboardData;
  onChipClick: (slug: string) => void;
}) {
  if (data.providers.length === 0 && data.other_xp === 0) {
    return (
      <div className="dash-section">
        <div className="dash-section-head">TYPES</div>
        <div className="dash-empty">
          No usage recorded yet. Use Claude Code / Codex / OpenCode and
          your pet starts feeding.
        </div>
      </div>
    );
  }

  return (
    <div className="dash-section">
      <div className="dash-section-head">TYPES — tap a chip to inspect</div>
      <div className="dash-chips">
        {data.providers.map((p) => (
          <ProviderChipCard
            key={p.provider}
            chip={p}
            onClick={() => onChipClick(p.provider)}
          />
        ))}
        {data.other_xp > 0 && (
          // Non-clickable — no provider to drill into. Tooltip on the
          // chip carries the explanation that used to be the visible
          // hint line ("activity hooks + manual grants"), keeping the
          // chip body single-line and the overview consistent with the
          // other provider chips' simplified format.
          <div
            className="dash-chip dash-chip-other"
            title="XP from activity hooks and manual grants — no token-bearing usage event"
          >
            <div className="dash-chip-label">Interactions</div>
            <div className="dash-chip-stat">
              <span className="dash-chip-stat-label">XP earned</span>
              <span
                className="dash-chip-stat-value dash-chip-stat-value-xp"
                title={data.other_xp.toLocaleString()}
              >
                +{fmtCompactInt(data.other_xp)}
              </span>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function ProviderChipCard({
  chip,
  onClick,
}: {
  chip: ProviderChip;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`dash-chip dash-chip-clickable dash-chip-${chip.provider}`}
      onClick={onClick}
      title={`Inspect ${chip.label} — per-model breakdown and individual requests`}
    >
      <div className="dash-chip-label">{chip.label}</div>
      <div className="dash-chip-stat">
        <span className="dash-chip-stat-label">Tokens used</span>
        <span
          className="dash-chip-stat-value"
          title={chip.tokens_total.toLocaleString()}
        >
          {fmtTokens(chip.tokens_total)}
        </span>
      </div>
      <div className="dash-chip-stat">
        <span className="dash-chip-stat-label">XP earned</span>
        <span
          className="dash-chip-stat-value dash-chip-stat-value-xp"
          title={chip.xp_total.toLocaleString()}
        >
          +{fmtCompactInt(chip.xp_total)}
        </span>
      </div>
      {/* in/out/cache breakdown intentionally omitted on the overview
       *  chip — keeps the card clean (label + two stat lines only).
       *  The full breakdown lives one tap away in the per-provider
       *  drill-down's MODELS table. */}
    </button>
  );
}

/// Compact token count formatter that scales up to billions.
/// Examples:
///   827            → "827"
///   12_345         → "12.3k"
///   1_478_900      → "1.48M"
///   1_478_900_000  → "1.48B"
///   2_300_000_000_000 → "2.3T"
/// Capped at 5 chars + suffix so even a wildly large value (multi-
/// trillion tokens) fits inside a 180-px chip without overflowing.
function fmtTokens(n: number): string {
  if (n >= 1e12) return `${(n / 1e12).toFixed(2)}T`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return n.toString();
}

/// Compact integer formatter for XP-style numbers. Keeps full
/// precision (with thousands separators) up to 99,999 so daily-XP
/// readings stay exact, then switches to k/M/B suffixes the same way
/// `fmtTokens` does. Used in the chip "XP earned" row and the
/// identity-line totals so the dashboard doesn't blow out when
/// somebody hits multi-million XP.
function fmtCompactInt(n: number): string {
  const abs = Math.abs(n);
  if (abs >= 1e9) return `${(n / 1e9).toFixed(2)}B`;
  if (abs >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  if (abs >= 1e5) return `${(n / 1e3).toFixed(1)}k`;
  return n.toLocaleString();
}

/// Tight XP-delta formatter for the moves-log row. The cell is only
/// 80 px wide AND has to fit the trailing " XP" label — so we
/// compact at 1,000 (one decimal of "kilo") instead of waiting for
/// 100k like `fmtCompactInt`. Trailing `.0` is stripped after
/// rounding so e.g. `9999` renders as `10k`, not `10.0k`.
///
/// Examples (suffix " XP" added by caller):
///   50          → "50"        → "+50 XP"
///   250         → "250"       → "+250 XP"
///   1_000       → "1k"        → "+1k XP"
///   1_500       → "1.5k"      → "+1.5k XP"
///   9_999       → "10k"       → "+10k XP"
///   15_000      → "15k"       → "+15k XP"
///   99_999      → "100k"      → "+100k XP"
///   1_500_000   → "1.5M"      → "+1.5M XP"
function fmtXpCompact(n: number): string {
  const abs = Math.abs(n);
  const fmtUnit = (value: number, suffix: string): string => {
    const rounded = value.toFixed(1);
    // Strip a trailing ".0" so whole-unit values lose the redundant
    // decimal — saves one character in the cell. `.5`, `.7` etc.
    // stay as-is for precision.
    const trimmed = rounded.endsWith(".0") ? rounded.slice(0, -2) : rounded;
    return `${trimmed}${suffix}`;
  };
  if (abs >= 1e6) return fmtUnit(n / 1e6, "M");
  if (abs >= 1e3) return fmtUnit(n / 1e3, "k");
  return n.toString();
}

// ─── Moves (recent activity battle-log) ────────────────────────────

function MovesSection({ recent }: { recent: RecentXp[] }) {
  // `dash-section-grow` makes this section the flex stretcher inside
  // the dialog — fills whatever vertical space the identity + types
  // rows leave behind. The inner log then scrolls if there are more
  // events than fit.
  //
  // ALL PETS mode adds a "pet name" column to each row, so the
  // backend stamps `pet_name` on the rows. We detect that once at
  // the section level and switch the row layout to 4-column instead
  // of 3-column; per-pet mode keeps the original layout.
  const hasPetColumn = recent.some((r) => r.pet_name);
  return (
    <div className="dash-section dash-section-grow">
      <div className="dash-section-head">RECENT MOVES</div>
      <div className={`dash-log${hasPetColumn ? " dash-log-with-pet" : ""}`}>
        {recent.length === 0 ? (
          <div className="dash-empty">No moves yet — XP events will appear here.</div>
        ) : (
          recent.map((r, i) => (
            <MoveRow key={i} row={r} showPet={hasPetColumn} />
          ))
        )}
      </div>
    </div>
  );
}

function MoveRow({
  row,
  showPet,
}: {
  row: RecentXp;
  /// True when the parent MovesSection has chosen the 4-column
  /// "with pet" layout (ALL PETS mode). Every row in that layout
  /// must include the pet cell — even if `row.pet_name` is null —
  /// so the columns stay aligned across rows.
  showPet: boolean;
}) {
  const t = formatTime(row.occurred_at);
  const sign = row.xp_delta >= 0 ? "+" : "";
  const provider = row.provider ? displayLabel(row.provider) : null;
  const detail = (() => {
    if (row.source_type === "usage") {
      if (provider && row.model) return `${provider} · ${row.model}`;
      if (provider) return provider;
      return "usage";
    }
    if (row.source_type === "activity") return row.reason ?? "interaction";
    if (row.source_type === "manual") return row.reason ?? "manual grant";
    return row.source_type;
  })();
  return (
    <div className="dash-log-row">
      <span className="dash-log-time">{t}</span>
      {showPet && (
        <span
          className="dash-log-pet"
          title={row.pet_name ? `Earned by ${row.pet_name}` : ""}
        >
          {row.pet_name ?? "—"}
        </span>
      )}
      <span
        className={`dash-log-delta ${row.xp_delta < 0 ? "neg" : ""}`}
        title={`${sign}${row.xp_delta.toLocaleString()} XP`}
      >
        {/* `fmtXpCompact` switches to "k"/"M" suffixes at 1,000 (not
         *  at 100k like `fmtCompactInt`) so values like 1,000 render
         *  as "1k XP" and stay on one line inside the tight 80 px
         *  cell. Tooltip carries the exact integer for audit. */}
        {sign}
        {fmtXpCompact(row.xp_delta)} XP
      </span>
      <span className="dash-log-detail">{detail}</span>
    </div>
  );
}

// ─── Models (registry-classified history) ──────────────────────────

/// "Models" panel — every distinct model the pet has logged, annotated
/// with the same tier + confidence the XP scorer would apply to its
/// next event. Color-coded tier badges give a quick read on the pet's
/// usage profile; a yellow "guessed" overlay on heuristic / unknown
/// rows signals "this model isn't in our registry yet — ping the
/// petpet-model-registry repo if you want precise scoring".
function ModelsSection({ models }: { models: RecentModelRow[] }) {
  return (
    <div className="dash-section dash-models">
      <div className="dash-section-head">MODELS</div>
      <div className="dash-models-grid">
        {models.map((m) => (
          <ModelChip key={`${m.provider}::${m.model_normalized}`} m={m} />
        ))}
      </div>
    </div>
  );
}

function ModelChip({ m }: { m: RecentModelRow }) {
  // Confidence drives the "guessed" overlay: `exact` is silent (the
  // registry knows this model), `heuristic` / `unknown` add a yellow
  // badge so the user can spot models that might benefit from a
  // registry update.
  const guessed = m.confidence !== "exact";
  return (
    <div className={`model-chip tier-${m.tier} confidence-${m.confidence}`}>
      <div className="model-chip-head">
        <span className={`tier-badge tier-badge-${m.tier}`}>
          {m.tier.toUpperCase()}
        </span>
        <span className="model-chip-name" title={`${m.vendor} · ${m.family}`}>
          {m.model}
        </span>
        {guessed && (
          <span
            className="model-chip-guessed"
            title={
              m.confidence === "heuristic"
                ? "Tier inferred from model name — not in the registry yet."
                : "No tier signal; treated as Mid at reduced confidence."
            }
          >
            guessed
          </span>
        )}
      </div>
      <div className="model-chip-stats">
        <span>{m.events.toLocaleString()} events</span>
        <span>{fmtTokens(m.tokens_total)} tok</span>
        {m.cost_usd > 0 && <span>${m.cost_usd.toFixed(2)}</span>}
      </div>
    </div>
  );
}

function displayLabel(provider: string): string {
  switch (provider) {
    case "claude":
    case "claude_code":
      return "Claude";
    case "codex":
      return "Codex";
    case "opencode":
      return "OpenCode";
    case "gemini":
      return "Gemini";
    default:
      return provider;
  }
}

/// Format an ISO 8601 / RFC 3339 timestamp into the local clock time
/// `HH:MM` (matching the battle-log feel). Falls back to the raw
/// string if Date parsing fails so we never render `Invalid Date`.
function formatTime(iso: string): string {
  try {
    const d = new Date(iso.endsWith("Z") || iso.includes("+") ? iso : `${iso}Z`);
    if (isNaN(d.getTime())) return iso;
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
  } catch {
    return iso;
  }
}

// (Replaced by `formatClock` + `formatDateLabel` lower down — request
// rows now show clock only with the date in a sticky group header.)

// ─── Drill-down: per-provider detail ───────────────────────────────

function ProviderDetailView({
  slug,
  petId,
  onBack,
  onShowRequests,
}: {
  slug: string;
  /// Scope the provider's stats to this pet. `null` falls back to the
  /// active pet (legacy behaviour) — the sidebar always passes the
  /// currently-selected pet so the drill-down matches the overview.
  petId: string | null;
  onBack: () => void;
  /// Called when the user taps the long "VIEW … REQUESTS →" button.
  /// We hand the parent the already-loaded first page + has-more
  /// flag so the popup opens instantly without re-fetching.
  onShowRequests: (
    label: string,
    initialRows: RequestRow[],
    initialHasMore: boolean,
    eventsTotal: number,
  ) => void;
}) {
  const [detail, setDetail] = useState<ProviderDetail | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<ProviderDetail>("dashboard_provider_detail", {
      provider: slug,
      petId,
    })
      .then((d) => {
        if (!cancelled) setDetail(d);
      })
      .catch((e) => {
        if (!cancelled) setError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [slug, petId]);

  return (
    <div className="dash-overlay">
      <div className="gba-box dash-box dash-box-detail">
        <div className="gba-title dash-detail-title">
          {detail?.label ? `${detail.label.toUpperCase()} · INSPECT` : "INSPECT"}
        </div>

        <div className="dash-detail-scroll">
          {error && <div className="picker-error">⚠ {error}</div>}
          {!detail && !error && <div className="dash-loading">Loading…</div>}

          {detail && (
            <>
              <div className="dash-detail-summary">
                <div className="dash-detail-stat">
                  <span className="dash-chip-stat-label">Spend</span>
                  <span
                    className="dash-chip-stat-value dash-chip-stat-value-cost"
                    title={`$${detail.cost_usd.toFixed(2)} all-time`}
                  >
                    {fmtUsd(detail.cost_usd)}
                  </span>
                </div>
                <div className="dash-detail-stat">
                  <span className="dash-chip-stat-label">Tokens</span>
                  <span
                    className="dash-chip-stat-value"
                    title={detail.tokens_total.toLocaleString()}
                  >
                    {fmtTokens(detail.tokens_total)}
                  </span>
                </div>
                <div className="dash-detail-stat">
                  <span className="dash-chip-stat-label">Requests</span>
                  <span
                    className="dash-chip-stat-value"
                    title={detail.events_total.toLocaleString()}
                  >
                    {fmtCompactInt(detail.events_total)}
                  </span>
                </div>
              </div>

              <div className="dash-detail-note">
                Input = full input load billed (fresh + cache reads + cache
                writes). Cached % is share served from prompt cache.
                Spend is an estimate from published per-1M-token rates.
              </div>

              <ModelsTable models={detail.models} />
              <ByDayChart days={detail.by_day} />
            </>
          )}
        </div>

        {/* Long primary-action button — opens the dedicated REQUESTS
         *  popup. Hands the already-loaded first page to the popup so
         *  it opens instantly. Hidden until detail loads since we
         *  don't know the request count yet. */}
        {detail && (
          <button
            type="button"
            className="dash-view-requests-btn"
            onClick={() =>
              onShowRequests(
                detail.label,
                detail.recent_requests,
                detail.has_more_requests,
                detail.events_total,
              )
            }
            disabled={detail.events_total === 0}
          >
            <span className="dash-view-requests-btn-label">
              {detail.events_total === 0
                ? "NO REQUESTS YET"
                : `VIEW ${detail.events_total.toLocaleString()} ${
                    detail.events_total === 1 ? "REQUEST" : "REQUESTS"
                  }`}
            </span>
            <span className="dash-view-requests-btn-arrow" aria-hidden="true">
              →
            </span>
          </button>
        )}

        <div className="picker-actions dash-actions dash-actions-detail">
          <button className="gba-button" onClick={onBack}>
            Back
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── Requests popup view ───────────────────────────────────────────

/// Standalone takeover view dedicated to the paginated request list.
/// Opened from the long button at the bottom of the provider detail.
/// Receives the already-loaded first page as a prop so the popup
/// renders instantly; pagination from there uses
/// `dashboard_provider_requests_page` (oldest cursor = timestamp of
/// the last visible row).
function RequestsPopupView({
  slug,
  label,
  eventsTotal,
  petId,
  initialRows,
  initialHasMore,
  onBack,
}: {
  slug: string;
  label: string;
  eventsTotal: number;
  /// Scope pagination to this pet — same value the provider-detail
  /// view used to fetch the first page. Null falls back to the active
  /// pet, matching backend semantics.
  petId: string | null;
  initialRows: RequestRow[];
  initialHasMore: boolean;
  onBack: () => void;
}) {
  const [rows, setRows] = useState<RequestRow[]>(initialRows);
  const [hasMore, setHasMore] = useState(initialHasMore);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadMore = async () => {
    if (loading || rows.length === 0) return;
    setLoading(true);
    try {
      const cursor = rows[rows.length - 1].timestamp;
      const page = await invoke<ProviderRequestsPage>(
        "dashboard_provider_requests_page",
        { provider: slug, beforeTimestamp: cursor, petId },
      );
      setRows((prev) => [...prev, ...page.requests]);
      setHasMore(page.has_more);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  const grouped = groupByLocalDate(rows);

  return (
    <div className="dash-overlay">
      <div className="gba-box dash-box dash-box-detail">
        <div className="gba-title dash-detail-title">
          {label.toUpperCase()} · REQUESTS
        </div>

        <div className="dash-requests-popup">
          {error && <div className="picker-error">⚠ {error}</div>}
          <div className="dash-requests-popup-meta">
            Showing {rows.length.toLocaleString()} of{" "}
            {eventsTotal.toLocaleString()}
            {hasMore ? " · more available" : " · all loaded"}
          </div>
          <div className="dash-table dash-table-requests">
            {rows.length === 0 ? (
              <div className="dash-empty">No requests recorded.</div>
            ) : (
              <>
                <div className="dash-table-head dash-request-row">
                  <span>Time</span>
                  <span>Model</span>
                  <span
                    className="ralign"
                    title="Total input billed — fresh + cache reads + cache writes."
                  >
                    Input
                  </span>
                  <span className="ralign">Output</span>
                  <span
                    className="ralign"
                    title="Share of this request's input served from prompt cache."
                  >
                    Cached %
                  </span>
                  <span className="ralign">Total</span>
                  <span
                    className="ralign"
                    title="Estimated cost for this single request."
                  >
                    Cost
                  </span>
                </div>
                <div className="dash-table-rows">
                  {grouped.map((group) => (
                    <div key={group.dateKey}>
                      <div className="dash-day-header">
                        <span>{formatDateLabel(group.dateKey)}</span>
                        <span className="dash-day-header-sub">
                          {group.rows.length}{" "}
                          {group.rows.length === 1 ? "request" : "requests"} · $
                          {group.totalCost.toFixed(2)}
                        </span>
                      </div>
                      {group.rows.map((r, i) => {
                        const inputTotal =
                          r.tokens_input +
                          r.tokens_cache_read +
                          r.tokens_cache_creation;
                        const cachedPct =
                          inputTotal > 0
                            ? (r.tokens_cache_read / inputTotal) * 100
                            : 0;
                        return (
                          <div
                            key={`${group.dateKey}-${i}`}
                            className="dash-table-row dash-request-row"
                          >
                            <span className="dash-cell-time" title={r.timestamp}>
                              {formatClock(r.timestamp)}
                            </span>
                            <span className="dash-cell-name" title={r.model}>
                              {r.model}
                            </span>
                            <span
                              className="ralign"
                              title={`fresh ${r.tokens_input.toLocaleString()} + cache reads ${r.tokens_cache_read.toLocaleString()} + cache writes ${r.tokens_cache_creation.toLocaleString()} = ${inputTotal.toLocaleString()}`}
                            >
                              {fmtTokens(inputTotal)}
                            </span>
                            <span
                              className="ralign"
                              title={r.tokens_output.toLocaleString()}
                            >
                              {fmtTokens(r.tokens_output)}
                            </span>
                            <span
                              className="ralign"
                              title={`${r.tokens_cache_read.toLocaleString()} of ${inputTotal.toLocaleString()} input tokens served from cache`}
                            >
                              {inputTotal > 0 ? `${cachedPct.toFixed(0)}%` : "—"}
                            </span>
                            <span
                              className="ralign dash-cell-total"
                              title={r.tokens_total.toLocaleString()}
                            >
                              {fmtTokens(r.tokens_total)}
                            </span>
                            <span
                              className="ralign dash-cell-cost"
                              title={`$${r.cost_usd.toFixed(4)}`}
                            >
                              {fmtUsd(r.cost_usd)}
                            </span>
                          </div>
                        );
                      })}
                    </div>
                  ))}
                </div>
                <div className="dash-loadmore">
                  {hasMore ? (
                    <button
                      type="button"
                      className="dash-loadmore-link"
                      onClick={loadMore}
                      disabled={loading}
                    >
                      {loading ? "loading…" : "load more ↓"}
                    </button>
                  ) : (
                    <span className="dash-loadmore-end">
                      — end of history —
                    </span>
                  )}
                </div>
              </>
            )}
          </div>
        </div>

        <div className="picker-actions dash-actions dash-actions-detail">
          <button className="gba-button" onClick={onBack}>
            Back
          </button>
        </div>
      </div>
    </div>
  );
}

function ModelsTable({ models }: { models: ModelRow[] }) {
  if (models.length === 0) {
    return (
      <div className="dash-section dash-section-models">
        <div className="dash-section-head">MODELS</div>
        <div className="dash-empty">No model usage recorded.</div>
      </div>
    );
  }
  return (
    <div className="dash-section dash-section-models">
      <div className="dash-section-head">MODELS — ALL-TIME</div>
      <div className="dash-table">
        <div className="dash-table-head dash-model-row">
          <span>Model</span>
          <span className="ralign">Requests</span>
          <span
            className="ralign"
            title="Total input tokens billed — fresh + cache reads + cache writes."
          >
            Input
          </span>
          <span className="ralign">Output</span>
          <span
            className="ralign"
            title="Share of input served from prompt cache. High = caching working."
          >
            Cached %
          </span>
          <span className="ralign">Total</span>
          <span
            className="ralign"
            title="Estimated spend in USD using published per-1M-token rates. Some models (e.g. opus-4-7) use back-derived rates calibrated against billing receipts."
          >
            Cost
          </span>
        </div>
        <div className="dash-table-rows">
          {models.map((m) => {
            const inputTotal =
              m.tokens_input + m.tokens_cache_read + m.tokens_cache_creation;
            const cachedPct =
              inputTotal > 0 ? (m.tokens_cache_read / inputTotal) * 100 : 0;
            return (
              <div key={m.model} className="dash-table-row dash-model-row">
                <span className="dash-cell-name" title={m.model}>
                  {m.model}
                </span>
                <span className="ralign" title={m.events.toLocaleString()}>
                  {fmtCompactInt(m.events)}
                </span>
                <span
                  className="ralign"
                  title={`fresh ${m.tokens_input.toLocaleString()} + cache reads ${m.tokens_cache_read.toLocaleString()} + cache writes ${m.tokens_cache_creation.toLocaleString()} = ${inputTotal.toLocaleString()}`}
                >
                  {fmtTokens(inputTotal)}
                </span>
                <span className="ralign" title={m.tokens_output.toLocaleString()}>
                  {fmtTokens(m.tokens_output)}
                </span>
                <span
                  className="ralign"
                  title={`${m.tokens_cache_read.toLocaleString()} of ${inputTotal.toLocaleString()} input tokens served from cache`}
                >
                  {inputTotal > 0 ? `${cachedPct.toFixed(0)}%` : "—"}
                </span>
                <span
                  className="ralign dash-cell-total"
                  title={m.tokens_total.toLocaleString()}
                >
                  {fmtTokens(m.tokens_total)}
                </span>
                <span
                  className="ralign dash-cell-cost"
                  title={`$${m.cost_usd.toFixed(4)}`}
                >
                  {fmtUsd(m.cost_usd)}
                </span>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

/// Backend window for BY DAY data. Must match the Rust constant
/// `DAY_BREAKDOWN_DAYS` in `dashboard.rs`. We backfill empty days on
/// the frontend so the chart x-axis is contiguous — sparse data
/// (e.g. Codex with 2 days of activity in a month) renders as 28
/// zero bars + 2 real bars, preserving the calendar.
const BYDAY_WINDOW = 30;

/// Build a contiguous newest-first array of `BYDAY_WINDOW` days. The
/// backend returns only days with activity; we synthesise zero rows
/// for empty days so the chart's bar positions correspond to real
/// calendar slots.
function backfillDays(days: DayBreakdownRow[]): DayBreakdownRow[] {
  const byDate = new Map(days.map((d) => [d.date_local, d]));
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const out: DayBreakdownRow[] = [];
  for (let i = 0; i < BYDAY_WINDOW; i++) {
    const dt = new Date(today);
    dt.setDate(dt.getDate() - i);
    const y = dt.getFullYear();
    const m = String(dt.getMonth() + 1).padStart(2, "0");
    const dd = String(dt.getDate()).padStart(2, "0");
    const key = `${y}-${m}-${dd}`;
    const existing = byDate.get(key);
    out.push(
      existing ?? {
        date_local: key,
        cost_usd: 0,
        total_tokens: 0,
        total_events: 0,
        by_model: [],
      },
    );
  }
  return out;
}

function ByDayChart({ days }: { days: DayBreakdownRow[] }) {
  const [hovered, setHovered] = useState<number | null>(null);

  const backfilled = backfillDays(days);
  // Oldest → newest so the chart reads left-to-right like a calendar.
  const asc = [...backfilled].reverse();
  const hasAny = asc.some((d) => d.cost_usd > 0 || d.total_events > 0);

  // Default the detail panel to the most recent day with activity, so
  // even at rest the chart conveys "today's spend" at a glance.
  const defaultIdx = (() => {
    for (let i = asc.length - 1; i >= 0; i--) {
      if (asc[i].cost_usd > 0 || asc[i].total_events > 0) return i;
    }
    return asc.length - 1;
  })();
  const focusIdx = hovered ?? defaultIdx;
  const focus = asc[focusIdx];
  // 30-day window total — matches CodexBar's bottom-line "Est. total".
  const windowTotal = asc.reduce((sum, d) => sum + d.cost_usd, 0);

  if (!hasAny) {
    return (
      <div className="dash-section dash-section-byday">
        <div className="dash-section-head">BY DAY — LAST 30 DAYS</div>
        <div className="dash-byday-empty">
          No activity in the last 30 days.
        </div>
      </div>
    );
  }

  // SVG coordinate space — unitless viewBox so bars scale naturally
  // with the container width via preserveAspectRatio="none".
  const W = 100;
  const H = 40;
  const maxCost = Math.max(...asc.map((d) => d.cost_usd), 0.0001);
  const barSlot = W / asc.length;
  // 75% bar, 25% gap — visually crisp without feeling crammed.
  const barW = barSlot * 0.75;
  const barOffset = (barSlot - barW) / 2;
  // Floor zero-bars at 0.4 of a viewBox unit so empty days are still
  // visible as a faint baseline tick.
  const ZERO_FLOOR = 0.4;

  return (
    <div className="dash-section dash-section-byday">
      <div className="dash-section-head">
        BY DAY — LAST {BYDAY_WINDOW} DAYS (LOCAL)
      </div>
      <div className="dash-byday-chart">
        <svg
          viewBox={`0 0 ${W} ${H}`}
          className="dash-byday-chart-svg"
          preserveAspectRatio="none"
          onMouseLeave={() => setHovered(null)}
          role="img"
          aria-label="Daily cost over the last 30 days"
        >
          {asc.map((d, i) => {
            // `isFocus` ≠ `isHover`: focus follows hover OR defaults
            // to the most recent active day, so today's bar is
            // pre-highlighted at rest (matches CodexBar's "you are
            // here" cue).
            const isFocus = focusIdx === i;
            const isActive = d.cost_usd > 0;
            const h = isActive
              ? Math.max((d.cost_usd / maxCost) * H, 1)
              : ZERO_FLOOR;
            return (
              <rect
                key={d.date_local}
                x={i * barSlot + barOffset}
                y={H - h}
                width={barW}
                height={h}
                className={`dash-byday-bar ${
                  isFocus ? "is-focus" : ""
                } ${isActive ? "" : "is-empty"}`}
                onMouseEnter={() => setHovered(isActive ? i : null)}
              >
                <title>
                  {d.date_local} — {fmtUsd(d.cost_usd)} ·{" "}
                  {fmtTokens(d.total_tokens)} tokens · {d.total_events}{" "}
                  {d.total_events === 1 ? "request" : "requests"}
                </title>
              </rect>
            );
          })}
        </svg>
        <div className="dash-byday-axis">
          <span>{formatDateLabel(asc[0].date_local)}</span>
          <span>{formatDateLabel(asc[asc.length - 1].date_local)}</span>
        </div>

        {/* Three-region detail panel (mirrors CodexBar):
         *   1. selected-day header — date + cost + tokens
         *   2. per-model rows — one per model that day, with colored
         *      indicator stripe, model name, cost, tokens
         *   3. window total — total spend over the 30-day window */}
        <div className="dash-byday-panel">
          <div className="dash-byday-day-header">
            <span className="dash-byday-day-date">
              {formatDateLabel(focus.date_local)}:
            </span>
            <span
              className="dash-byday-day-cost"
              title={`$${focus.cost_usd.toFixed(4)}`}
            >
              {fmtUsd(focus.cost_usd)}
            </span>
            <span className="dash-byday-sep">·</span>
            <span title={focus.total_tokens.toLocaleString()}>
              {fmtTokens(focus.total_tokens)} tokens
            </span>
          </div>

          {focus.by_model.length === 0 ? (
            <div className="dash-byday-models-empty">
              No requests this day.
            </div>
          ) : (
            <div className="dash-byday-models">
              {focus.by_model.map((m) => (
                <div className="dash-byday-model" key={m.model}>
                  <span
                    className="dash-byday-model-bar"
                    aria-hidden="true"
                  />
                  <div className="dash-byday-model-info">
                    <div className="dash-byday-model-name" title={m.model}>
                      {m.model}
                    </div>
                    <div className="dash-byday-model-stats">
                      <span
                        className="dash-byday-model-cost"
                        title={`$${m.cost_usd.toFixed(4)}`}
                      >
                        {fmtUsd(m.cost_usd)}
                      </span>
                      <span className="dash-byday-sep">·</span>
                      <span title={m.tokens_total.toLocaleString()}>
                        {fmtTokens(m.tokens_total)}
                      </span>
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}

          <div
            className="dash-byday-total"
            title={`$${windowTotal.toFixed(2)} across the last ${BYDAY_WINDOW} days`}
          >
            Est. total ({BYDAY_WINDOW}d):{" "}
            <span className="dash-byday-total-cost">{fmtUsd(windowTotal)}</span>
          </div>
        </div>
      </div>
    </div>
  );
}

// `RequestsTable` was removed — REQUESTS now lives in its own popup
// view (`RequestsPopupView` above) opened from the long "VIEW N
// REQUESTS →" button at the bottom of the provider detail. The
// `groupByLocalDate` helper + DayGroup interface below are still
// used by the popup.

interface DayGroup {
  dateKey: string; // YYYY-MM-DD in local tz
  rows: RequestRow[];
  totalCost: number;
}

/// Bucket consecutive request rows by their local-date string. Rows
/// already arrive newest-first from the backend; we preserve that
/// order so each group's first row is its latest request.
function groupByLocalDate(rows: RequestRow[]): DayGroup[] {
  const out: DayGroup[] = [];
  for (const r of rows) {
    const key = localDateKey(r.timestamp);
    const last = out[out.length - 1];
    if (last && last.dateKey === key) {
      last.rows.push(r);
      last.totalCost += r.cost_usd;
    } else {
      out.push({ dateKey: key, rows: [r], totalCost: r.cost_usd });
    }
  }
  return out;
}

function localDateKey(iso: string): string {
  try {
    const d = new Date(iso.endsWith("Z") || iso.includes("+") ? iso : `${iso}Z`);
    if (isNaN(d.getTime())) return iso.slice(0, 10);
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    return `${y}-${m}-${day}`;
  } catch {
    return iso.slice(0, 10);
  }
}

/// Date label for the per-day grouping header. Just the localised
/// short date — earlier iterations prefixed "Today · " / "Yesterday ·
/// " / weekday, but the user found the mix of relative tag + date
/// noisy. Stripped to the plain date so every row reads the same way
/// regardless of recency.
function formatDateLabel(dateLocal: string): string {
  const parts = dateLocal.split("-");
  if (parts.length !== 3) return dateLocal;
  const d = new Date(
    parseInt(parts[0], 10),
    parseInt(parts[1], 10) - 1,
    parseInt(parts[2], 10),
  );
  if (isNaN(d.getTime())) return dateLocal;
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

/// Just the clock portion (`HH:MM`) — used inside REQUESTS rows now
/// that the date lives in the group header. Previously each row had
/// `MM/DD HH:MM`; splitting saves a column-worth of space and removes
/// the visual repetition.
function formatClock(iso: string): string {
  try {
    const d = new Date(iso.endsWith("Z") || iso.includes("+") ? iso : `${iso}Z`);
    if (isNaN(d.getTime())) return iso;
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
  } catch {
    return iso;
  }
}

/// USD formatter — chooses precision based on magnitude so a $0.0023
/// request doesn't render as "$0.00" while a $241.89 day doesn't
/// render as "$241.890000". Matches CodexBar's display granularity.
function fmtUsd(n: number): string {
  if (!isFinite(n) || n === 0) return "$0";
  const abs = Math.abs(n);
  if (abs >= 100) return `$${n.toFixed(0)}`;
  if (abs >= 10) return `$${n.toFixed(1)}`;
  if (abs >= 1) return `$${n.toFixed(2)}`;
  if (abs >= 0.01) return `$${n.toFixed(3)}`;
  return `$${n.toFixed(4)}`;
}
