/**
 * NamingView — renders in the floating "naming" Tauri window
 * (label="naming"). The hatching ceremony's closing modal — "Your
 * egg hatched! Name your companion." — used to render INSIDE the
 * tiny pet window and got clipped. Moving it to its own window above
 * the pet (same position as the stage-up notify bubble) gives the
 * form the room it needs.
 *
 * Backend wiring:
 *   1. `naming_current` snapshot read on mount (race-safe for the
 *      create-window-then-emit gap).
 *   2. `naming_dismiss(pet_id, name, confirmed)` invoked on submit
 *      or skip — backend finalizes the name then emits `naming://done`
 *      so the CeremonyPlayer can resolve.
 *
 * Window auto-sizes via ResizeObserver, same pattern as InfoView /
 * NotifyView.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./NamingView.css";

interface NamingPrompt {
  pet_id: string;
  placeholder: string;
  title: string;
  body: string;
  confirm_label: string;
  cancel_label: string;
}

export function NamingView() {
  const [prompt, setPrompt] = useState<NamingPrompt | null>(null);
  const [name, setName] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const panelRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const lastSizeRef = useRef<{ w: number; h: number } | null>(null);

  // Pull the current prompt on mount AND on every `naming://refresh`
  // emitted by `naming_window_show`. The Tauri naming window is
  // created once and reused via .hide()/.show() across hatching
  // events — so this React component is the SAME instance for every
  // subsequent popup. Without the listener, the prompt + form state
  // would freeze at whatever the first hatching set (stale pet name,
  // and worse, `submitting=true` carried over from the previous
  // dismiss would lock out the input + buttons).
  const refreshFromBackend = useCallback(() => {
    invoke<NamingPrompt | null>("naming_current")
      .then((p) => {
        if (p) {
          setPrompt(p);
          setName("");           // clear stale input value
          setSubmitting(false);  // unstick the previous dismiss
          setError(null);
        }
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    refreshFromBackend();
    const unlistenP = listen("naming://refresh", () => refreshFromBackend());
    return () => {
      unlistenP.then((unlisten) => unlisten()).catch(() => {});
    };
  }, [refreshFromBackend]);

  // Focus the input once the prompt has rendered.
  useEffect(() => {
    if (prompt) inputRef.current?.focus();
  }, [prompt]);

  // Auto-fit the Tauri window to the rendered panel size. Same pattern
  // as InfoView / NotifyView — clamps applied server-side so we just
  // measure and forward.
  useEffect(() => {
    const el = panelRef.current;
    if (!el) return;
    const sync = () => {
      const r = el.getBoundingClientRect();
      const w = Math.max(el.scrollWidth, Math.ceil(r.width)) + 16;
      const h = Math.max(el.scrollHeight, Math.ceil(r.height)) + 16;
      const last = lastSizeRef.current;
      if (last && last.w === w && last.h === h) return;
      lastSizeRef.current = { w, h };
      invoke("naming_window_resize", { width: w, height: h }).catch((err) =>
        console.warn("naming_window_resize", err),
      );
    };
    sync();
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    if (document.fonts?.ready) {
      document.fonts.ready.then(sync).catch(() => {});
    }
    return () => ro.disconnect();
  }, [prompt, error]);

  const dismiss = async (confirmed: boolean) => {
    if (submitting || !prompt) return;
    setSubmitting(true);
    setError(null);
    try {
      await invoke("naming_dismiss", {
        petId: prompt.pet_id,
        name: confirmed && name.trim() !== "" ? name.trim() : null,
        confirmed,
      });
      // Reset `submitting` immediately. The window is about to hide,
      // but the React instance lives on (Tauri reuses the window) —
      // leaving `submitting=true` would lock out the buttons on the
      // NEXT hatching event before our naming://refresh listener
      // has a chance to fire. Defense in depth.
      setSubmitting(false);
    } catch (e) {
      setError(String(e));
      setSubmitting(false);
    }
  };

  if (!prompt) return null;

  return (
    <div
      ref={panelRef}
      className="gba-box naming-panel"
      role="dialog"
      aria-modal="true"
    >
      {/* No `data-tauri-drag-region` — this is a focused modal, not
       *  a draggable companion bubble. Drag handles intercept clicks
       *  on some Tauri/macOS combos and were a candidate cause of the
       *  "can't click, can't type" symptom. */}
      <div className="gba-title naming-title">
        {prompt.title.toUpperCase()}
      </div>
      <form
        className="naming-body"
        onSubmit={(e) => {
          e.preventDefault();
          dismiss(true);
        }}
      >
        {prompt.body && <div className="naming-text">{prompt.body}</div>}
        <input
          ref={inputRef}
          type="text"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={prompt.placeholder}
          maxLength={20}
          disabled={submitting}
          autoFocus
        />
        {error && <div className="picker-error">⚠ {error}</div>}
        <div className="picker-actions naming-actions">
          <button
            type="button"
            className="gba-button"
            onClick={() => dismiss(false)}
            disabled={submitting}
          >
            {prompt.cancel_label.toUpperCase()}
          </button>
          <button
            type="submit"
            className="gba-button primary"
            disabled={submitting}
          >
            {prompt.confirm_label.toUpperCase()}
          </button>
        </div>
      </form>
    </div>
  );
}
