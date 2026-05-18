/**
 * MenuView — renders in the floating "menu" Tauri window (label="menu").
 * Shows pet status + dev controls + nav actions. Each action invokes
 * its backend command then hides the menu window via `menu_window_hide`.
 *
 * The window is positioned by Rust at the cursor's right-click point.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { PetStateUpdate } from "../types";
import "./MenuView.css";

interface PetSnapshotResponse {
  pet: { id: string; name: string; template_id: string } | null;
  state: { current_level: number; total_xp: number } | null;
  stage: { name: string } | null;
}

interface MenuInfo {
  name: string;
  level: number;
  totalXp: number;
  stageName: string | null;
}

export function MenuView() {
  const [info, setInfo] = useState<MenuInfo | null>(null);
  const [isDev, setIsDev] = useState(false);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const lastSizeRef = useRef<{ w: number; h: number } | null>(null);

  // Auto-size the menu window so every row is visible even when the
  // dev-mode block adds +XP / Reset rows. Same ResizeObserver pattern
  // used by InfoView / NotifyView.
  useEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const sync = () => {
      const r = el.getBoundingClientRect();
      const w = Math.max(el.scrollWidth, Math.ceil(r.width)) + 12;
      const h = Math.max(el.scrollHeight, Math.ceil(r.height)) + 12;
      const last = lastSizeRef.current;
      if (last && last.w === w && last.h === h) return;
      lastSizeRef.current = { w, h };
      invoke("menu_window_resize", { width: w, height: h }).catch((err) =>
        console.warn("menu_window_resize", err),
      );
    };
    sync();
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    if (document.fonts?.ready) {
      document.fonts.ready.then(sync).catch(() => {});
    }
    return () => ro.disconnect();
  }, [isDev, info]);

  // Dev affordances (+XP / Reset) are gated on a Rust-side flag — true
  // for `tauri dev` builds (`cfg!(debug_assertions)`) or when
  // PETPET_DEV is set. Production users won't see them.
  useEffect(() => {
    invoke<boolean>("dev_mode")
      .then(setIsDev)
      .catch(() => setIsDev(false));
  }, []);

  // Centralised refresh — used on mount AND when the active pet
  // changes, so a "switch companion" action immediately reflects the
  // new pet in the menu's status header.
  const refreshFromSnapshot = useCallback(async () => {
    try {
      const snap = await invoke<PetSnapshotResponse>("pet_snapshot");
      if (!snap.pet) return;
      setInfo({
        name: snap.pet.name,
        level: snap.state?.current_level ?? 0,
        totalXp: snap.state?.total_xp ?? 0,
        stageName: snap.stage?.name ?? null,
      });
    } catch {
      /* ignore */
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
      setInfo({
        name: u.name,
        level: u.current_level,
        totalXp: u.total_xp,
        stageName: u.stage_name,
      });
    }).then((fn) => {
      if (cancelled) fn();
      else unState = fn;
    });
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

  // Auto-dismiss on outside click or Escape. Outside click is detected
  // by listening for the menu window losing focus.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        invoke("menu_window_hide").catch(() => {});
      }
    };
    const onBlur = () => {
      // Slight delay so a click inside that triggers blur still runs.
      setTimeout(() => invoke("menu_window_hide").catch(() => {}), 80);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", onBlur);
    };
  }, []);

  const close = useCallback(() => {
    invoke("menu_window_hide").catch(() => {});
  }, []);

  const run = useCallback(
    async (action: () => Promise<unknown>) => {
      try {
        await action();
      } catch (e) {
        console.error("menu action failed", e);
      } finally {
        close();
      }
    },
    [close],
  );

  return (
    <div ref={rootRef} className="menu-root" data-tauri-drag-region={false}>
      {info && (
        <>
          <div className="menu-status">
            <div className="menu-status-row">
              <span className="menu-status-name">{info.name}</span>
              <span className="menu-status-lvl">Lv.{info.level}</span>
            </div>
            <div className="menu-status-sub">
              <span>{info.stageName ?? "—"}</span>
              <span className="menu-status-xp">{info.totalXp} XP</span>
            </div>
          </div>
          <div className="menu-divider" />
        </>
      )}

      {isDev && (
        <>
          <button
            className="menu-item"
            onClick={() => run(() => invoke("pet_grant_xp", { xpDelta: 100, reason: "menu" }))}
          >
            +100 XP
          </button>
          <button
            className="menu-item"
            onClick={() => run(() => invoke("pet_grant_xp", { xpDelta: 1000, reason: "menu" }))}
          >
            +1k XP
          </button>
          <button
            className="menu-item"
            onClick={() => run(() => invoke("pet_grant_xp", { xpDelta: 10000, reason: "menu" }))}
          >
            +10k XP
          </button>

          <div className="menu-divider" />

          <button
            className="menu-item menu-item-warn"
            onClick={() => run(() => invoke("pet_reset_xp"))}
          >
            Reset XP
          </button>

          <div className="menu-divider" />
        </>
      )}

      {/* Trainer-card dashboard — tokens, XP-by-provider, recent
       *  moves. Visible to every user (not gated on dev mode) because
       *  the stats overview IS the app's practical surface. */}
      <button
        className="menu-item"
        onClick={() => run(() => emit("menu:request-dashboard"))}
      >
        Dashboard…
      </button>

      {/* Single entry point — opens the pets box. The box has an "+"
       *  tile at the end for hatching a new pet, so we don't need
       *  separate Switch / New items here. */}
      <button
        className="menu-item"
        onClick={() => run(() => emit("menu:request-switcher"))}
      >
        Pets…
      </button>

      {/* Import a `.petpet` file — equivalent of Pokémon's "Mystery
       *  Gift" menu entry: receive something from outside. Drag-drop
       *  onto the floating pet works too; this menu item is the
       *  discoverable / click-driven path. */}
      <button
        className="menu-item"
        onClick={() => run(() => emit("menu:request-import"))}
      >
        Import…
      </button>
    </div>
  );
}
