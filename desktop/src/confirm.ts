/**
 * Promise wrapper around the `confirm` Tauri sub-window.
 *
 * Use from anywhere in the frontend that needs a GBA-styled dialog
 * without rendering it inside its own component tree. Each call:
 *   1. invokes `confirm_show(title, message, options)` — Rust stores
 *      the prompt and opens the centered `?view=confirm` window
 *   2. awaits one `confirm://chosen` event
 *   3. resolves with the picked value, or `null` if the user
 *      dismissed (Esc, click-outside, blur)
 *
 * Centralised here so `App.tsx`, `EggPicker`, `TemplateCreator`,
 * `Dashboard`, and future surfaces all hit the same code path —
 * keeps the GBA dialog visually consistent across the app.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ConfirmOption {
  label: string;
  value: string;
  primary?: boolean;
}

export interface ConfirmOpts {
  title: string;
  message: string;
  options: ConfirmOption[];
}

export async function awaitConfirm(opts: ConfirmOpts): Promise<string | null> {
  let unlisten: UnlistenFn | undefined;
  try {
    const result = await new Promise<string | null>((resolve) => {
      let settled = false;
      const settle = (v: string | null) => {
        if (settled) return;
        settled = true;
        resolve(v);
      };
      listen<string>("confirm://chosen", (e) => {
        const v = e.payload;
        settle(v && v.length > 0 ? v : null);
      })
        .then((fn) => {
          unlisten = fn;
          // If we already settled (shouldn't happen — listen resolves
          // before the first event ever fires — but defensive), tear
          // down immediately.
          if (settled) fn();
          // Tauri's `invoke` types require `InvokeArgs` (index-signature
          // record). Spread the strongly-typed `opts` into a fresh object
          // so we keep the typed boundary at our callers without losing
          // type-safety on the wire shape.
          return invoke("confirm_show", {
            title: opts.title,
            message: opts.message,
            options: opts.options,
          });
        })
        .catch((err) => {
          console.warn("confirm_show / listen failed", err);
          settle(null);
        });
    });
    return result;
  } finally {
    unlisten?.();
  }
}
