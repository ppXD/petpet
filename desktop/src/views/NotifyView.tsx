/**
 * NotifyView — renders in the floating "notify" Tauri window
 * (label="notify"). Generic toast bubble above the pet, used today for
 * evolution announcements and reusable for any short-form notice.
 *
 * Text is set by the Rust `notify_show` command via two paths:
 *   1. A `notify_current` snapshot read on mount — survives the
 *      create-window-then-emit race for the first notification.
 *   2. `notify://message` Tauri events for subsequent updates while
 *      the window is already open.
 *
 * The window's size is driven by a ResizeObserver on the panel: any
 * layout change (state update, webfont load) invokes
 * `notify_window_resize` so the bubble's Tauri window fits the
 * rendered content exactly.
 */

import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import "./NotifyView.css";

interface NotifyPayload {
  text: string;
}

export function NotifyView() {
  const [text, setText] = useState<string | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const lastSizeRef = useRef<{ w: number; h: number } | null>(null);

  // Pull the latest message snapshot on mount and subscribe to live
  // updates. `cancelled` closes the StrictMode race where the effect's
  // cleanup runs before `listen()`'s promise resolves.
  useEffect(() => {
    let cancelled = false;
    let unlisten: UnlistenFn | undefined;
    invoke<NotifyPayload | null>("notify_current")
      .then((p) => {
        if (!cancelled && p) setText(p.text);
      })
      .catch(() => {});
    listen<NotifyPayload>("notify://message", (e) => {
      if (!cancelled) setText(e.payload.text);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Auto-fit the Tauri window to the rendered panel size. Same pattern
  // as InfoView — scrollWidth so any non-wrapping overflow still counts
  // toward the window size, document.fonts.ready for the pixel font
  // load reflow.
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
      invoke("notify_window_resize", { width: w, height: h }).catch((err) =>
        console.warn("notify_window_resize", err),
      );
    };
    sync();
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    if (document.fonts?.ready) {
      document.fonts.ready.then(sync).catch(() => {});
    }
    return () => ro.disconnect();
  }, [text]);

  if (!text) return null;
  return (
    <div ref={panelRef} className="notify-panel">
      {text}
    </div>
  );
}
