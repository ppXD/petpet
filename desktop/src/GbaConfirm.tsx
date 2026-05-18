/**
 * GbaConfirm — promise-driven modal that matches the existing GBA
 * Pokémon-dialog look (`.gba-box`, `.gba-button`, Press Start 2P
 * font). Replaces the OS-native `@tauri-apps/plugin-dialog` `ask` /
 * `confirm` for in-app decisions where keeping the aesthetic
 * matters more than getting the OS chrome.
 *
 * Usage from a parent component:
 *
 *     const [ask, askElement] = useGbaConfirm();
 *
 *     async function doStuff() {
 *       const choice = await ask({
 *         title: "Replace template?",
 *         message: "Drakon 1.0.0 already installed. Replace with 1.1.0?",
 *         options: [
 *           { label: "Replace", value: "replace", primary: true },
 *           { label: "Cancel",  value: null },
 *         ],
 *       });
 *       if (choice === "replace") { ... }
 *     }
 *
 *     return (<>
 *       ...
 *       {askElement}
 *     </>);
 *
 * The `null` value is reserved for "user dismissed / cancelled" and
 * is also what the modal resolves to if the user hits Escape.
 */

import { useCallback, useEffect, useRef, useState } from "react";

export interface GbaConfirmOption<V> {
  label: string;
  value: V;
  /** Renders with the primary (blue) button style. Default: false. */
  primary?: boolean;
  /** Renders with the warn (red-ish) button style for destructive
   *  actions. Currently styled the same as default but reserved for
   *  future use. */
  warn?: boolean;
}

export interface GbaConfirmOpts<V> {
  title: string;
  /** Body copy. Newlines render as line breaks. */
  message: string;
  options: GbaConfirmOption<V>[];
}

interface OpenState<V> {
  opts: GbaConfirmOpts<V>;
  resolve: (value: V | null) => void;
}

/// Custom hook returning [askFn, modalElement]. Mount the element
/// inside the component tree; askFn returns a promise that resolves
/// when the user picks an option (or null if they hit Escape /
/// closed the dialog).
export function useGbaConfirm<V = string | null>(): [
  (opts: GbaConfirmOpts<V>) => Promise<V | null>,
  React.ReactNode,
] {
  const [open, setOpen] = useState<OpenState<V> | null>(null);
  // React 19 made `useRef<T>()` require an initial-value argument.
  // The ref is populated synchronously below before any consumer can
  // call `ask`, so seeding with `undefined` is safe.
  const askRef = useRef<
    ((opts: GbaConfirmOpts<V>) => Promise<V | null>) | undefined
  >(undefined);

  askRef.current = (opts: GbaConfirmOpts<V>) =>
    new Promise<V | null>((resolve) => {
      setOpen({ opts, resolve });
    });

  const ask = useCallback((opts: GbaConfirmOpts<V>) => askRef.current!(opts), []);

  // Esc cancels the dialog when one is open.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        open.resolve(null);
        setOpen(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  const pick = useCallback(
    (value: V | null) => {
      if (!open) return;
      open.resolve(value);
      setOpen(null);
    },
    [open],
  );

  const element = open ? (
    <div className="gba-confirm-backdrop" onClick={() => pick(null)}>
      <div className="gba-box gba-confirm-box" onClick={(e) => e.stopPropagation()}>
        <div className="gba-title">{open.opts.title}</div>
        <div className="gba-box gba-box-inset gba-confirm-body">
          {open.opts.message.split("\n").map((line, i) => (
            <p key={i}>{line}</p>
          ))}
        </div>
        <div className="picker-actions gba-confirm-actions">
          {open.opts.options.map((opt, i) => (
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
  ) : null;

  return [ask, element];
}
