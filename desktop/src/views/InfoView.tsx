/**
 * InfoView — renders in the floating "info" Tauri window (label="info").
 * Shows pet name / level / xp bar / next-evolution hint. Listens for
 * `pet://state` events so the panel stays live as XP accumulates.
 *
 * This window is shown on `mouseenter` over the pet sprite in the main
 * window and hidden on `mouseleave`. Position is computed in Rust based
 * on the main window's location.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { PetStateUpdate } from "../types";
import "./InfoView.css";

interface PetIdentity {
  id: string;
  name: string;
  template_id: string;
}

interface PetStageRow {
  level: number;
  name: string;
  xp_required: number;
  sprite_key: string;
  flavor?: string | null;
}

interface XPStateView {
  total_xp: number;
  current_level: number;
  xp_in_level: number;
  xp_for_next_level: number | null;
  stage_level: number;
}

interface NextEvolutionView {
  level: number;
  name: string;
  sprite_key: string;
  xp_to_next: number;
}

interface PetSnapshotResponse {
  pet: PetIdentity | null;
  state: XPStateView | null;
  stage: PetStageRow | null;
  next_evolution: NextEvolutionView | null;
}

interface FlatPetState {
  name: string;
  currentLevel: number;
  totalXp: number;
  xpInLevel: number;
  xpForNextLevel: number | null;
  stageName: string | null;
  stageFlavor: string | null;
  nextEvoName: string | null;
  xpToNextEvo: number | null;
}

function parseStageIndex(spriteKey: string | null | undefined): number {
  if (!spriteKey) return 0;
  const m = /^stage_(\d+)$/.exec(spriteKey);
  return m ? parseInt(m[1], 10) : 0;
}

export function InfoView() {
  const [state, setState] = useState<FlatPetState | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  // Track the last reported size so we only fire the resize invoke when
  // dimensions actually change — avoids a Rust round-trip on every
  // unrelated re-render (e.g. when listen callbacks fire identical
  // state).
  const lastSizeRef = useRef<{ w: number; h: number } | null>(null);

  // Centralised snapshot refresh — used on mount AND whenever the
  // active pet changes (so a "switch companion" action immediately
  // reflects the new pet in the hover bubble).
  const refreshFromSnapshot = useCallback(async () => {
    try {
      const snap = await invoke<PetSnapshotResponse>("pet_snapshot");
      if (!snap.pet) {
        setState(null);
        return;
      }
      setState({
        name: snap.pet.name,
        currentLevel: snap.state?.current_level ?? 0,
        totalXp: snap.state?.total_xp ?? 0,
        xpInLevel: snap.state?.xp_in_level ?? 0,
        xpForNextLevel: snap.state?.xp_for_next_level ?? null,
        stageName: snap.stage?.name ?? null,
        stageFlavor: snap.stage?.flavor ?? null,
        nextEvoName: snap.next_evolution?.name ?? null,
        xpToNextEvo: snap.next_evolution?.xp_to_next ?? null,
      });
    } catch {
      /* ignore — view stays blank until next event */
    }
  }, []);

  useEffect(() => {
    refreshFromSnapshot();
  }, [refreshFromSnapshot]);

  useEffect(() => {
    let unState: UnlistenFn | undefined;
    let unActive: UnlistenFn | undefined;
    let cancelled = false;
    listen<PetStateUpdate>("pet://state", (ev) => {
      const u = ev.payload;
      setState({
        name: u.name,
        currentLevel: u.current_level,
        totalXp: u.total_xp,
        xpInLevel: u.xp_in_level,
        xpForNextLevel: u.xp_for_next_level,
        stageName: u.stage_name,
        stageFlavor: u.stage_flavor,
        nextEvoName: u.next_evolution_name,
        xpToNextEvo: u.xp_to_next_evolution,
      });
    }).then((fn) => {
      if (cancelled) fn();
      else unState = fn;
    });
    // Pull a fresh snapshot whenever the active companion changes —
    // the new pet has different name / level / stage and the cached
    // state above would otherwise show the previous pet's data.
    listen("pet://active_changed", () => {
      void refreshFromSnapshot();
    }).then((fn) => {
      if (cancelled) fn();
      else unActive = fn;
    });
    return () => {
      cancelled = true;
      unState?.();
      unActive?.();
    };
  }, [refreshFromSnapshot]);

  const xpProgress = useMemo(() => {
    if (!state || state.xpForNextLevel === null) return 1;
    const total = state.xpInLevel + state.xpForNextLevel;
    return total > 0 ? state.xpInLevel / total : 0;
  }, [state]);

  const nextLevelTotalXp = useMemo(() => {
    if (!state || state.xpForNextLevel === null) return null;
    return state.totalXp - state.xpInLevel + state.xpForNextLevel;
  }, [state]);

  // void this for now — stage_index parsing is left as a hook for future
  // sprite-based rendering inside the info window.
  void parseStageIndex;

  // Keep the Tauri info window sized to the panel's actual rendered
  // box. Use a ResizeObserver instead of a one-shot useLayoutEffect
  // because the panel reflows after Press Start 2P (loaded async from
  // Google Fonts) replaces the fallback font, and we need to re-resize
  // at that point too — otherwise the window stays at the fallback-
  // -font width and the pixel-font content overflows visually.
  useEffect(() => {
    const el = panelRef.current;
    if (!el) return;
    const sync = () => {
      const r = el.getBoundingClientRect();
      // Use scrollWidth / scrollHeight so any content that overflows
      // the panel's max-width (e.g. a long pet name with white-space:
      // nowrap on the head row) still counts toward the window size.
      // Without this, the window resizes to the panel box but
      // overflowing text gets clipped by html's overflow: hidden.
      // CSS gives the panel 6px margin on each side; +4px slack for
      // sub-pixel rounding.
      const w = Math.max(el.scrollWidth, Math.ceil(r.width)) + 16;
      const h = Math.max(el.scrollHeight, Math.ceil(r.height)) + 16;
      const last = lastSizeRef.current;
      if (last && last.w === w && last.h === h) return;
      lastSizeRef.current = { w, h };
      invoke("info_window_resize", { width: w, height: h }).catch((e) =>
        console.warn("info_window_resize", e),
      );
    };
    sync();
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    // Belt-and-suspenders: webfonts load after the first paint, so
    // fire one more sync once they're ready.
    if (document.fonts?.ready) {
      document.fonts.ready.then(sync).catch(() => {});
    }
    return () => ro.disconnect();
  }, [state]);

  if (!state) return null;

  return (
    <div ref={panelRef} className="info-panel" title={state.stageFlavor ?? undefined}>
      <div className="info-row info-head">
        <span className="info-name">{state.name}</span>
        <span className="info-lvl">Lv.{state.currentLevel}</span>
      </div>

      <div className="info-xp-row">
        <div className="info-xp-bar">
          <div
            className="info-xp-bar-fill"
            style={{ width: `${Math.min(100, xpProgress * 100)}%` }}
          />
        </div>
        <span className="info-xp-numbers">
          {state.xpForNextLevel === null
            ? "MAX"
            : `${state.totalXp}/${nextLevelTotalXp ?? 0}`}
        </span>
      </div>

      <div className="info-row info-foot">
        <span className="info-stage">{state.stageName ?? "—"}</span>
        <span className="info-next">
          {state.nextEvoName && state.xpToNextEvo !== null ? (
            <>
              → <b>{state.nextEvoName}</b> · {state.xpToNextEvo} XP
            </>
          ) : (
            <>
              → <b>Final form</b>
            </>
          )}
        </span>
      </div>
    </div>
  );
}
