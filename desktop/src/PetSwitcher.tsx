/**
 * PetSwitcher — Pokémon-party-style picker that lists every
 * previously-raised pet with its CURRENT appearance and level. Lets
 * the user jump back to any companion instead of forcing them through
 * the create-from-template flow each time.
 *
 * Backed by `pet_list_summaries` which returns one entry per pet with:
 *   - current evolution stage's sprite path  (so the row matches what
 *     the user is used to seeing on their desktop, not the egg / first
 *     form);
 *   - current level + total XP;
 *   - stage name.
 *
 * Selection invokes `pet_set_active`, which marks the pet active in
 * SQLite and rehydrates the XP engine's in-memory state.
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Pet } from "./Pet";
import "./PetSwitcher.css";

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

interface Props {
  onPicked: () => void;
  onCancel: () => void;
  /// Called when the user taps the "+ Hatch new" tile. Parent closes
  /// the switcher and opens the template picker.
  onAddNew: () => void;
  /// Called when the user taps the "+ Import" tile (or wants to
  /// import via a different surface). Parent triggers the import
  /// file-picker / drag-drop handler. Placeholder for now.
  onImport: () => void;
  /// Called when the user taps "Export…" with a pet selected. Parent
  /// runs the export flow for that pet id.
  onExport: (petId: string) => void;
}

export function PetSwitcher({
  onPicked,
  onCancel,
  onAddNew,
  onImport,
  onExport,
}: Props) {
  const [summaries, setSummaries] = useState<PetSummary[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Centralised loader so we can call it on mount AND whenever an
  // import (or any other library mutation) lands. Without this, a
  // user importing a `.petpet` while the Pets dialog is open would
  // see their new pet vanish behind a stale list until they close +
  // re-open the dialog.
  const loadSummaries = useCallback(async (preferActive: boolean) => {
    try {
      const list = await invoke<PetSummary[]>("pet_list_summaries");
      setSummaries(list);
      if (preferActive) {
        const active = list.find((s) => s.pet.is_active) ?? list[0];
        if (active) setSelected(active.pet.id);
      } else {
        // After a refresh from a library mutation, keep the user's
        // current row selection if it still exists; otherwise fall
        // back to whichever pet is active.
        setSelected((cur) => {
          if (cur && list.some((s) => s.pet.id === cur)) return cur;
          const active = list.find((s) => s.pet.is_active) ?? list[0];
          return active ? active.pet.id : null;
        });
      }
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    loadSummaries(true);
  }, [loadSummaries]);

  // Refresh on library mutations. The backend emits
  // `pet://library_changed` after an import (without auto-switching);
  // `pet://active_changed` fires when the active pet swaps (so a
  // "Switch" elsewhere is reflected here too).
  useEffect(() => {
    let unLibrary: UnlistenFn | undefined;
    let unActive: UnlistenFn | undefined;
    let cancelled = false;
    listen("pet://library_changed", () => {
      if (!cancelled) void loadSummaries(false);
    }).then((fn) => {
      if (cancelled) fn();
      else unLibrary = fn;
    });
    listen("pet://active_changed", () => {
      if (!cancelled) void loadSummaries(false);
    }).then((fn) => {
      if (cancelled) fn();
      else unActive = fn;
    });
    return () => {
      cancelled = true;
      unLibrary?.();
      unActive?.();
    };
  }, [loadSummaries]);

  const selectedSummary = useMemo(
    () => summaries.find((s) => s.pet.id === selected) ?? null,
    [summaries, selected],
  );

  const confirm = async () => {
    if (!selected || busy) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("pet_set_active", { petId: selected });
      onPicked();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <div className="switcher-overlay">
      <div className="gba-box switcher-box">
        {/* Title row with a corner "+ HATCH" action. The button is
         *  the single entry point to the hatching flow regardless of
         *  party size — discoverable without scrolling past existing
         *  pets, and visually separated from the data grid below. */}
        <div className="gba-title gba-title-row">
          <span className="gba-title-spacer" aria-hidden />
          <span className="gba-title-text">PETS</span>
          <button
            type="button"
            className="gba-title-action"
            onClick={onAddNew}
            title="Create a new companion from a template"
            aria-label="Hatch a new companion"
          >
            +
          </button>
        </div>

        {error && <div className="picker-error">{error}</div>}

        {summaries.length === 0 && !error && (
          // First-visit: no pets yet. Point the user at the corner
          // button rather than rendering an empty grid; the corner
          // "+" is the only path forward and the hint anchors it.
          <div className="switcher-empty">
            <div className="switcher-empty-glyph">↗</div>
            <div className="switcher-empty-text">
              No companions yet — tap <b>+</b> in the top-right to hatch one.
            </div>
          </div>
        )}

        {/* Grid of existing pets. The add-companion affordance lives
         *  in the title row's corner action button (see "+ in title
         *  above), NOT inline with the party — that keeps actions
         *  visually separate from data and the "+" stays discoverable
         *  without scrolling past 20+ pets. Empty state (no pets yet)
         *  is handled below the grid since the title action is the
         *  single entry point regardless of count. */}
        <div className="switcher-grid">
          {summaries.map((s) => {
            const isSelected = selected === s.pet.id;
            return (
              <button
                key={s.pet.id}
                type="button"
                className={`switcher-card ${isSelected ? "selected" : ""} ${s.pet.is_active ? "active" : ""}`}
                onClick={() => setSelected(s.pet.id)}
                onDoubleClick={() => {
                  setSelected(s.pet.id);
                  void confirm();
                }}
              >
                <div className="switcher-card-sprite">
                  {s.sprite_path ? (
                    <img
                      src={convertFileSrc(s.sprite_path)}
                      alt=""
                      className="switcher-card-img"
                      onError={(e) => {
                        (e.target as HTMLImageElement).style.visibility = "hidden";
                      }}
                    />
                  ) : (
                    // Egg-stage / missing-asset fallback — render
                    // the same procedural pet renderer used in the
                    // main window so the avatar at least conveys
                    // "this pet is at egg stage" rather than blank.
                    <div className="switcher-card-svg">
                      <Pet stageIndex={0} mood="idle" />
                    </div>
                  )}
                  {s.pet.is_active && <span className="switcher-card-badge">●</span>}
                </div>
                <div className="switcher-card-name">{s.pet.name}</div>
                <div className="switcher-card-meta">
                  <span className="switcher-card-lvl">Lv.{s.current_level}</span>
                  <span className="switcher-card-stage">{s.stage_name}</span>
                </div>
              </button>
            );
          })}
        </div>

        {selectedSummary && (
          <div className="switcher-detail">
            <div className="switcher-detail-name">{selectedSummary.pet.name}</div>
            <div className="switcher-detail-row">
              <span>{selectedSummary.stage_name}</span>
              <span>Lv.{selectedSummary.current_level}</span>
              <span>{selectedSummary.total_xp.toLocaleString()} XP</span>
            </div>
          </div>
        )}

        <div className="picker-actions">
          <button className="gba-button" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          {/* Import is a box-level action (brings something into the
           *  party) and shares the same handler as the right-click
           *  menu's "Import…" item — so the dialog the user
           *  experiences is identical regardless of entry point. */}
          <button className="gba-button" onClick={onImport} disabled={busy}>
            Import…
          </button>
          {/* Export the selected pet's snapshot + XP history as a
           *  `.petpet` archive. Pokémon's PC-Box "Summary / Move /
           *  Release" idiom — actions live with the selected mon. */}
          <button
            className="gba-button"
            onClick={() => selected && onExport(selected)}
            disabled={!selected || busy || summaries.length === 0}
          >
            Export…
          </button>
          <button
            className="gba-button primary"
            onClick={confirm}
            disabled={!selected || busy || summaries.length === 0}
          >
            {busy ? "Switching…" : "Switch"}
          </button>
        </div>
      </div>
    </div>
  );
}
