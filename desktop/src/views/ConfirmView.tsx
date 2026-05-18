/**
 * ConfirmView — renders inside the floating "confirm" Tauri window
 * (label="confirm"). GBA-styled dialog for in-app yes/no/multi-choice
 * prompts (template-replace, pet-id-merge-vs-copy, etc).
 *
 * Lives in its OWN window rather than as an overlay inside the main
 * pet window — this is what stops the floating pet from jumping or
 * being covered when a confirm fires from compact mode.
 *
 * Flow (server-driven):
 *   1. handleImport (main window) invokes `confirm_show(title, ...)`.
 *   2. Rust opens this sub-window centered on screen, focused.
 *   3. ConfirmView mounts, pulls the prompt via `confirm_current`.
 *   4. User clicks a button → invoke `confirm_dismiss(choice)`.
 *   5. Rust emits `confirm://chosen` event with the choice and hides
 *      the window.
 *   6. handleImport's awaiting promise resolves with the choice.
 *
 * Escape / window-blur both resolve to a `null` choice (cancel), so
 * the import flow's awaiter doesn't hang if the user dismisses
 * unconventionally.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./ConfirmView.css";

interface ConfirmOption {
  label: string;
  value: string;
  primary?: boolean;
}

interface ConfirmPrompt {
  title: string;
  message: string;
  options: ConfirmOption[];
}

export function ConfirmView() {
  const [prompt, setPrompt] = useState<ConfirmPrompt | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const lastSizeRef = useRef<{ w: number; h: number } | null>(null);

  // Pull the current prompt on mount. The store is set by
  // `confirm_show` before the window is shown, so this is always
  // populated by the time we render.
  useEffect(() => {
    let cancelled = false;
    invoke<ConfirmPrompt | null>("confirm_current")
      .then((p) => {
        if (!cancelled) setPrompt(p);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  // Auto-size the Tauri window to fit the rendered panel.
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
      invoke("confirm_window_resize", { width: w, height: h }).catch(() => {});
    };
    sync();
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    if (document.fonts?.ready) {
      document.fonts.ready.then(sync).catch(() => {});
    }
    return () => ro.disconnect();
  }, [prompt]);

  const pick = useCallback((value: string | null) => {
    invoke("confirm_dismiss", { choice: value }).catch(() => {});
  }, []);

  // Escape dismisses to cancel.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") pick(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pick]);

  // Window blur dismisses to cancel — matches the "click outside to
  // close" UX of macOS / Windows native dialogs without requiring a
  // backdrop element.
  useEffect(() => {
    const onBlur = () => {
      // Tiny delay so a button-click that triggers blur still runs
      // its onClick first (same trick MenuView uses).
      setTimeout(() => pick(null), 80);
    };
    window.addEventListener("blur", onBlur);
    return () => window.removeEventListener("blur", onBlur);
  }, [pick]);

  if (!prompt) return null;

  return (
    <div className="confirm-root" ref={panelRef}>
      <div className="gba-box confirm-box">
        <div className="gba-title confirm-title">{prompt.title}</div>
        <div className="gba-box gba-box-inset confirm-body">
          {prompt.message.split("\n").map((line, i) =>
            line.trim() === "" ? (
              <p key={i} className="confirm-spacer">
                &nbsp;
              </p>
            ) : (
              <p key={i}>{line}</p>
            ),
          )}
        </div>
        <div className="picker-actions confirm-actions">
          {prompt.options.map((opt, i) => (
            <button
              key={i}
              className={`gba-button${opt.primary ? " primary" : ""}`}
              onClick={() => pick(opt.value)}
            >
              {opt.label}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
