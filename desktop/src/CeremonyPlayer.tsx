/**
 * CeremonyPlayer — plays a JSON-authored sequence of visual effects
 * over the pet. Each `CeremonyAction` has `after` (delay), `for`
 * (duration), and a `kind` that selects which effect renders.
 *
 * Effects:
 *   shake      → emits onShakeChange(true) for `for`. Parent applies
 *                CSS shake to .pet-wrapper.
 *   flash      → full-screen white veil that fades.
 *   crack      → angled crack lines (used on the egg before hatch).
 *   burst      → radial particle burst from the pet's center.
 *   ring       → expanding ring outline.
 *   sparkle    → scattered sparkles around the pet area.
 *   bubble     → small text bubble near the pet (supports
 *                {{pet.name}} / {{stage.name}} / {{stage.flavor}}).
 *   confetti   → falling colored shards.
 *   modal      → blocking dialog. Optional `calls` invokes a Tauri
 *                command (currently `pet_finalize_naming`) on confirm.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { CeremonyAction, CeremonyKind } from "./types";

export interface CeremonyContext {
  petId?: string;
  petName?: string;
  stageName?: string | null;
  stageFlavor?: string | null;
}

interface Props {
  ceremony: CeremonyAction[];
  context: CeremonyContext;
  onShakeChange?: (shaking: boolean) => void;
  onComplete: () => void;
}

const SAFETY_TAIL_MS = 600;

export function CeremonyPlayer({
  ceremony,
  context,
  onShakeChange,
  onComplete,
}: Props) {
  const [now, setNow] = useState(0);
  const [modalDismissed, setModalDismissed] = useState(false);
  const startedAt = useRef(performance.now());

  // When all non-modal effects (shake/flash/burst/ring/sparkle/etc.)
  // have finished playing. Modal waits for this — users see all the
  // visual celebration first, then the dialog appears at the end.
  const nonModalEndMs = useMemo(() => {
    let max = 0;
    for (const a of ceremony) {
      if (a.kind === "modal") continue;
      const end = parseDur(a.after) + Math.max(parseDur(a.for), 200);
      if (end > max) max = end;
    }
    return max;
  }, [ceremony]);

  const totalMs = useMemo(() => {
    // Total ceremony time: non-modal end, plus any modal's own duration
    // (modals usually have no `for` so this just equals nonModalEndMs).
    let max = nonModalEndMs;
    for (const a of ceremony) {
      if (a.kind !== "modal") continue;
      const end = nonModalEndMs + Math.max(parseDur(a.for), 0);
      if (end > max) max = end;
    }
    return max;
  }, [ceremony, nonModalEndMs]);

  // RAF tick.
  useEffect(() => {
    let raf = 0;
    let cancelled = false;
    const tick = () => {
      if (cancelled) return;
      const elapsed = performance.now() - startedAt.current;
      setNow(elapsed);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => {
      cancelled = true;
      cancelAnimationFrame(raf);
    };
  }, []);

  // Determine if any shake action is currently active.
  const shaking = useMemo(() => {
    return ceremony.some(
      (a) =>
        a.kind === "shake" &&
        now >= parseDur(a.after) &&
        now < parseDur(a.after) + parseDur(a.for),
    );
  }, [ceremony, now]);

  useEffect(() => {
    onShakeChange?.(shaking);
    return () => onShakeChange?.(false);
  }, [shaking, onShakeChange]);

  // `bubble` ceremony actions render in the separate floating `notify`
  // Tauri window above the pet — the main pet window is 140×140 and
  // can't fit text bubbles for long stage names. Schedule each bubble
  // via setTimeout at its `after` offset; auto-hide is handled by Rust
  // using `for` (parsed) as `duration_ms`.
  useEffect(() => {
    const timers: number[] = [];
    for (const a of ceremony) {
      if (a.kind !== "bubble") continue;
      const text = substituteTokens(a.text ?? a.body ?? "", context);
      if (!text) continue;
      const after = parseDur(a.after);
      // Floor at 5s — the JSON's `for` was tuned for an inline 1-line
      // bubble inside the 140px pet window; the floating notify window
      // can hold more text and warrants a longer dwell so the user has
      // time to read the evolution announcement.
      const duration = Math.max(parseDur(a.for), 5000);
      timers.push(
        window.setTimeout(() => {
          invoke("notify_show", { text, durationMs: duration }).catch((err) =>
            console.warn("notify_show", err),
          );
        }, after),
      );
    }
    return () => {
      timers.forEach((t) => clearTimeout(t));
      // Best-effort hide if the user dismisses the ceremony early.
      invoke("notify_hide").catch(() => {});
    };
  }, [ceremony, context]);

  // Find a pending modal action. Trigger only after all non-modal effects
  // have finished — the modal is the celebratory closer, not interruption.
  //
  // Naming modals (`calls: "pet_finalize_naming"`) are deliberately
  // SKIPPED here. The naming popup is no longer ceremony-coupled —
  // App.tsx's `useNamingPopupSync` opens/closes it based on the pet's
  // `name_finalized_at` field. The ceremony just plays the visuals
  // (shake / crack / burst / flash / ring / sparkle / bubble) and
  // completes; the popup appears as a side effect of the level-up
  // state change reaching the parent.
  const modalAction = useMemo(
    () =>
      ceremony.find(
        (a) =>
          a.kind === "modal" &&
          a.calls !== "pet_finalize_naming" &&
          now >= nonModalEndMs &&
          !modalDismissed,
      ),
    [ceremony, now, nonModalEndMs, modalDismissed],
  );

  // Completion: when total time elapsed AND no modal pending.
  useEffect(() => {
    if (now > totalMs + SAFETY_TAIL_MS && !modalAction) {
      onComplete();
    }
  }, [now, totalMs, modalAction, onComplete]);

  return (
    <div className="ceremony-layer">
      {ceremony.map((action, i) => {
        if (action.kind === "shake") return null;       // handled via onShakeChange
        if (action.kind === "modal") return null;       // handled below
        const start = parseDur(action.after);
        const dur = parseDur(action.for) || 1500;
        const active = now >= start && now < start + dur;
        if (!active) return null;
        return (
          <Effect
            key={i}
            kind={action.kind}
            action={action}
            elapsedInAction={now - start}
            totalDur={dur}
            context={context}
          />
        );
      })}

      {modalAction && (
        <CeremonyModal
          action={modalAction}
          context={context}
          onDismiss={() => setModalDismissed(true)}
        />
      )}
    </div>
  );
}

// ─── Per-effect renderer ────────────────────────────────────────────────────

function Effect({
  kind,
  action,
  elapsedInAction,
  totalDur,
  // `context` is accepted on the type so call sites pass a uniform
  // bundle, but no effect case currently needs it. Rename-to-underscore
  // is the standard TS escape from `noUnusedParameters` without losing
  // the prop on the public surface.
  context: _context,
}: {
  kind: CeremonyKind;
  action: CeremonyAction;
  elapsedInAction: number;
  totalDur: number;
  context: CeremonyContext;
}) {
  const t = elapsedInAction / totalDur;     // 0..1 progress
  switch (kind) {
    case "flash":
      return (
        <div
          className="fx-flash"
          style={{
            opacity: 1 - t,
            background: action.color ?? "#ffffff",
          }}
        />
      );
    case "ring": {
      const scale = 0.2 + t * 1.6;
      const opacity = Math.max(0, 1 - t);
      return (
        <div
          className="fx-ring"
          style={{
            transform: `translate(-50%, -50%) scale(${scale})`,
            opacity,
            borderColor: action.color ?? "#7eb0e8",
          }}
        />
      );
    }
    case "burst":
      return <Burst count={action.count ?? 16} colors={action.colors} t={t} />;
    case "sparkle":
      return <Sparkles count={action.count ?? 10} colors={action.colors} t={t} />;
    case "confetti":
      return <Confetti count={action.count ?? 24} colors={action.colors} t={t} />;
    case "crack":
      return <Crack t={t} />;
    case "bubble":
      // Handled by the `notify_show` scheduler in CeremonyPlayer's
      // useEffect — the bubble renders in the separate `notify`
      // window so long evolution text isn't clipped by the 140-px
      // main pet window.
      return null;
    default:
      return null;
  }
}

function Burst({
  count,
  colors,
  t,
}: {
  count: number;
  colors?: string[];
  t: number;
}) {
  const palette = colors && colors.length > 0 ? colors : ["#fde047", "#7eb0e8", "#a78bfa"];
  const items = useMemo(() => {
    const out = [];
    for (let i = 0; i < count; i++) {
      const angle = (i / count) * Math.PI * 2 + Math.random() * 0.2;
      out.push({
        angle,
        color: palette[i % palette.length],
        size: 4 + Math.random() * 4,
      });
    }
    return out;
  }, [count, palette.join(",")]);

  const distance = t * 120;
  const opacity = Math.max(0, 1 - t * 1.2);

  return (
    <div className="fx-burst">
      {items.map((p, i) => (
        <span
          key={i}
          style={{
            background: p.color,
            width: p.size,
            height: p.size,
            transform: `translate(-50%, -50%) translate(${Math.cos(p.angle) * distance}px, ${Math.sin(p.angle) * distance}px)`,
            opacity,
          }}
        />
      ))}
    </div>
  );
}

function Sparkles({
  count,
  colors,
  t,
}: {
  count: number;
  colors?: string[];
  t: number;
}) {
  const palette = colors && colors.length > 0 ? colors : ["#fde047", "#ffffff", "#a78bfa"];
  const items = useMemo(() => {
    const out = [];
    for (let i = 0; i < count; i++) {
      out.push({
        x: 10 + Math.random() * 80,    // % of layer width
        y: 10 + Math.random() * 80,
        delay: Math.random(),
        color: palette[i % palette.length],
        size: 3 + Math.random() * 4,
      });
    }
    return out;
  }, [count, palette.join(",")]);

  return (
    <div className="fx-sparkles">
      {items.map((s, i) => {
        const localT = (t + s.delay) % 1;
        const fade = Math.sin(localT * Math.PI);
        return (
          <span
            key={i}
            style={{
              left: `${s.x}%`,
              top: `${s.y}%`,
              width: s.size,
              height: s.size,
              background: s.color,
              opacity: fade,
              transform: `translate(-50%, -50%) rotate(${localT * 360}deg)`,
            }}
          />
        );
      })}
    </div>
  );
}

function Confetti({
  count,
  colors,
  t,
}: {
  count: number;
  colors?: string[];
  t: number;
}) {
  const palette = colors && colors.length > 0 ? colors : ["#fde047", "#7eb0e8", "#a78bfa", "#f5a8c0", "#ffffff"];
  const items = useMemo(() => {
    const out = [];
    for (let i = 0; i < count; i++) {
      out.push({
        x: Math.random() * 100,
        delay: Math.random() * 0.4,
        speed: 0.8 + Math.random() * 0.6,
        rot: Math.random() * 360,
        color: palette[i % palette.length],
      });
    }
    return out;
  }, [count, palette.join(",")]);

  return (
    <div className="fx-confetti">
      {items.map((c, i) => {
        const localT = Math.max(0, t * c.speed - c.delay);
        const fallY = localT * 280;       // px from top
        const opacity = Math.max(0, 1 - localT);
        return (
          <span
            key={i}
            style={{
              left: `${c.x}%`,
              top: `${fallY}px`,
              background: c.color,
              opacity,
              transform: `rotate(${c.rot + localT * 540}deg)`,
            }}
          />
        );
      })}
    </div>
  );
}

function Crack({ t }: { t: number }) {
  // Two crack lines that grow over the duration.
  const len = Math.min(1, t * 1.8);
  return (
    <svg className="fx-crack" viewBox="0 0 100 100" preserveAspectRatio="xMidYMid meet">
      <polyline
        points={`50,30 ${50 - 8 * len},${40 + 8 * len} ${50 + 4 * len},${52 + 6 * len} ${50 - 6 * len},${65 + 5 * len}`}
        stroke="#1a1a1a"
        strokeWidth="1.4"
        fill="none"
        strokeLinejoin="miter"
      />
      <polyline
        points={`50,30 ${50 + 6 * len},${42 + 6 * len} ${50 - 3 * len},${56 + 8 * len}`}
        stroke="#1a1a1a"
        strokeWidth="1"
        fill="none"
      />
    </svg>
  );
}

// ─── Modal ──────────────────────────────────────────────────────────────────

function CeremonyModal({
  action,
  context,
  onDismiss,
}: {
  action: CeremonyAction;
  context: CeremonyContext;
  onDismiss: () => void;
}) {
  const wantsName = action.calls === "pet_finalize_naming";
  const [name, setName] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (wantsName) inputRef.current?.focus();
  }, [wantsName]);

  const title = substituteTokens(action.title ?? "", context);
  const body = substituteTokens(action.body ?? "", context);
  const confirmLabel = action.confirm_label ?? "OK";
  const cancelLabel = action.cancel_label ?? "Skip";

  const handle = async (confirmed: boolean) => {
    if (submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      if (action.calls && context.petId) {
        const args: Record<string, unknown> = { petId: context.petId };
        if (wantsName) {
          args.name = confirmed && name.trim() !== "" ? name.trim() : null;
        }
        await invoke(action.calls, args);
      }
      onDismiss();
    } catch (e) {
      setError(String(e));
      setSubmitting(false);
    }
  };

  return (
    <div className="ceremony-modal-backdrop" data-tauri-drag-region>
      <div
        className="gba-box ceremony-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        {title && (
          <div className="gba-title" data-tauri-drag-region>
            {title.toUpperCase()}
          </div>
        )}
        <div className="ceremony-modal-body">
          {body && <div className="ceremony-modal-text">{body}</div>}
          {wantsName && (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                handle(true);
              }}
            >
              <input
                ref={inputRef}
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder={context.petName ?? ""}
                maxLength={20}
                disabled={submitting}
              />
            </form>
          )}
          {error && <div className="picker-error">⚠ {error}</div>}
        </div>
        <div className="picker-actions">
          <button
            type="button"
            className="gba-button"
            onClick={() => handle(false)}
            disabled={submitting}
          >
            {cancelLabel.toUpperCase()}
          </button>
          <button
            type="button"
            className="gba-button primary"
            onClick={() => handle(true)}
            disabled={submitting}
          >
            {submitting ? "..." : confirmLabel.toUpperCase()}
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── helpers ────────────────────────────────────────────────────────────────

function parseDur(s?: string): number {
  if (!s) return 0;
  const m = /^([\d.]+)\s*(ms|s)?$/.exec(s);
  if (!m) return 0;
  const val = parseFloat(m[1]);
  return m[2] === "ms" ? val : val * 1000;
}

function substituteTokens(s: string, ctx: CeremonyContext): string {
  return s
    .replace(/\{\{pet\.name\}\}/g, ctx.petName ?? "")
    .replace(/\{\{stage\.name\}\}/g, ctx.stageName ?? "")
    .replace(/\{\{stage\.flavor\}\}/g, ctx.stageFlavor ?? "");
}
