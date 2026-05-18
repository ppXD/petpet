/**
 * Placeholder pet sprite. Renders a stage-aware pixel creature that
 * grows visible features per evolution stage (0..=9). Geometry only —
 * actual species art ships later via `templates/builtin/<id>/stages/
 * stage_N/sprite.png`. This renderer is the fallback when no asset is
 * present, and the dev/test surface for the whole evolution flow.
 *
 * Per-stage growth vocabulary (kept intentionally generic so any
 * template palette swaps cleanly into it):
 *
 *   stage 0  egg (vertical oval, speckled, no face)
 *   stage 1  baby blob, eyes + mouth
 *   stage 2  + antenna nub
 *   stage 3  + side ears
 *   stage 4  + small tail
 *   stage 5  + back-stripe accent  (mid evolution)
 *   stage 6  larger body
 *   stage 7  + floating sparkle above head
 *   stage 8  + halo arc
 *   stage 9  + halo + crown pip + sparkles  (final form)
 */

import { useState, type CSSProperties } from "react";

export type Mood = "idle" | "thinking" | "working" | "eating" | "satisfied";

interface PetProps {
  /** 0 = egg, 1..9 = evolution stages. Anything outside clamps to 1. */
  stageIndex: number;
  mood: Mood;
  /** Optional raster sprite URL for the current stage. When present
   *  and loadable, takes precedence over the placeholder geometry.
   *  If the image fails to load (404, no template asset), Pet falls
   *  back to the procedural SVG below — so this is always safe to
   *  pass speculatively. */
  spriteUrl?: string | null;
  /** Template theme — pulled from `theme.primary / accent / palette`.
   *  All defaulted so a missing template still renders. */
  primary?: string;   // body main
  shadow?: string;    // body shadow
  dark?: string;      // outline / underside
  accent?: string;    // halo / crown / sparkles
}

const DEFAULTS = {
  primary: "#6ee7b7",
  shadow: "#10b981",
  dark: "#047857",
  accent: "#fde047",
};

const EYE = "#0b1020";
const SHEEN = "#ffffff";
const MOUTH = "#0b1020";
const SPARK = "#fde047";
const SWEAT = "#60a5fa";
const BLUSH = "#f5a8c0";

export function Pet({
  stageIndex,
  mood,
  spriteUrl,
  primary = DEFAULTS.primary,
  shadow = DEFAULTS.shadow,
  dark = DEFAULTS.dark,
  accent = DEFAULTS.accent,
}: PetProps) {
  const stage = Math.max(0, Math.min(9, Math.floor(stageIndex)));
  const [imgFailed, setImgFailed] = useState(false);

  // Reset the failure flag whenever the URL changes — a new stage may
  // have a working sprite even if the previous one failed.
  const [lastUrl, setLastUrl] = useState<string | null | undefined>(spriteUrl);
  if (spriteUrl !== lastUrl) {
    setLastUrl(spriteUrl);
    setImgFailed(false);
  }

  const useImage = !!spriteUrl && !imgFailed;

  return (
    <div className={`pet pet-${mood} pet-stage-${stage}`} aria-label="pet">
      {useImage ? (
        <img
          className="pet-sprite-img"
          src={spriteUrl!}
          alt=""
          draggable={false}
          onError={() => setImgFailed(true)}
        />
      ) : (
        <svg
          viewBox="0 0 24 24"
          shapeRendering="crispEdges"
          xmlns="http://www.w3.org/2000/svg"
          className="pet-svg"
        >
          {stage === 0 ? (
            <Egg primary={primary} shadow={shadow} dark={dark} accent={accent} />
          ) : (
            <Creature
              stage={stage}
              mood={mood}
              primary={primary}
              shadow={shadow}
              dark={dark}
              accent={accent}
            />
          )}
        </svg>
      )}
    </div>
  );
}

// ─── Stage 0: Egg ───────────────────────────────────────────────────────────

function Egg({
  primary,
  shadow,
  dark,
  accent,
}: {
  primary: string;
  shadow: string;
  dark: string;
  accent: string;
}) {
  // 24x24 vertical oval. Built as horizontal strips with gradient shading.
  // top row -> bottom row
  const strips: Array<[number, number, number, string]> = [
    // y, x, width, color
    [2, 10, 4, shadow],
    [3, 9, 6, shadow],
    [4, 8, 8, primary],
    [5, 7, 10, primary],
    [6, 6, 12, primary],
    [7, 5, 14, primary],
    [8, 5, 14, primary],
    [9, 4, 16, primary],
    [10, 4, 16, primary],
    [11, 4, 16, primary],
    [12, 4, 16, primary],
    [13, 4, 16, primary],
    [14, 4, 16, primary],
    [15, 5, 14, primary],
    [16, 5, 14, shadow],
    [17, 6, 12, shadow],
    [18, 7, 10, shadow],
    [19, 8, 8, dark],
    [20, 9, 6, dark],
    [21, 10, 4, dark],
  ];

  return (
    <g>
      {strips.map(([y, x, w, c], i) => (
        <rect key={i} x={x} y={y} width={w} height={1} fill={c} />
      ))}
      {/* top-left highlight glint */}
      <rect x={7} y={5} width={2} height={1} fill={SHEEN} opacity={0.7} />
      <rect x={6} y={6} width={1} height={2} fill={SHEEN} opacity={0.55} />
      {/* speckles in accent color */}
      <rect x={8} y={9} width={2} height={2} fill={accent} opacity={0.85} />
      <rect x={14} y={11} width={2} height={2} fill={accent} opacity={0.85} />
      <rect x={9} y={15} width={2} height={1} fill={accent} opacity={0.85} />
      <rect x={13} y={8} width={1} height={1} fill={accent} opacity={0.85} />
    </g>
  );
}

// ─── Stages 1-9: Creature ──────────────────────────────────────────────────

interface StageGeom {
  /** body bbox: [x, y, w, h] */
  body: [number, number, number, number];
  /** eye pixel anchors (left, right). pupils sit at row+(moodOffset). */
  eyes: [{ x: number; y: number }, { x: number; y: number }];
  /** mouth center row */
  mouthY: number;
  /** mouth center column */
  mouthX: number;
  hasAntenna: boolean;
  hasEars: boolean;
  hasTail: boolean;
  hasStripe: boolean;
  hasSparkle: boolean;
  hasHalo: boolean;
  hasCrown: boolean;
}

const GEOM: Record<number, StageGeom> = {
  1: { body: [9, 10, 7, 7], eyes: [{ x: 10, y: 12 }, { x: 13, y: 12 }], mouthX: 11, mouthY: 14,
       hasAntenna: false, hasEars: false, hasTail: false, hasStripe: false, hasSparkle: false, hasHalo: false, hasCrown: false },
  2: { body: [9, 10, 7, 7], eyes: [{ x: 10, y: 12 }, { x: 13, y: 12 }], mouthX: 11, mouthY: 14,
       hasAntenna: true,  hasEars: false, hasTail: false, hasStripe: false, hasSparkle: false, hasHalo: false, hasCrown: false },
  3: { body: [9, 10, 7, 7], eyes: [{ x: 10, y: 12 }, { x: 13, y: 12 }], mouthX: 11, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: false, hasStripe: false, hasSparkle: false, hasHalo: false, hasCrown: false },
  4: { body: [8, 9, 9, 8],  eyes: [{ x: 10, y: 12 }, { x: 14, y: 12 }], mouthX: 12, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: true,  hasStripe: false, hasSparkle: false, hasHalo: false, hasCrown: false },
  5: { body: [8, 9, 9, 8],  eyes: [{ x: 10, y: 12 }, { x: 14, y: 12 }], mouthX: 12, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: true,  hasStripe: true,  hasSparkle: false, hasHalo: false, hasCrown: false },
  6: { body: [7, 8, 11, 9], eyes: [{ x: 10, y: 12 }, { x: 14, y: 12 }], mouthX: 12, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: true,  hasStripe: true,  hasSparkle: false, hasHalo: false, hasCrown: false },
  7: { body: [7, 8, 11, 9], eyes: [{ x: 10, y: 12 }, { x: 14, y: 12 }], mouthX: 12, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: true,  hasStripe: true,  hasSparkle: true,  hasHalo: false, hasCrown: false },
  8: { body: [6, 8, 13, 9], eyes: [{ x: 10, y: 12 }, { x: 15, y: 12 }], mouthX: 12, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: true,  hasStripe: true,  hasSparkle: true,  hasHalo: true,  hasCrown: false },
  9: { body: [6, 8, 13, 9], eyes: [{ x: 10, y: 12 }, { x: 15, y: 12 }], mouthX: 12, mouthY: 14,
       hasAntenna: true,  hasEars: true,  hasTail: true,  hasStripe: true,  hasSparkle: true,  hasHalo: true,  hasCrown: true },
};

function Creature({
  stage,
  mood,
  primary,
  shadow,
  dark,
  accent,
}: {
  stage: number;
  mood: Mood;
  primary: string;
  shadow: string;
  dark: string;
  accent: string;
}) {
  const g = GEOM[stage];
  const [bx, by, bw, bh] = g.body;

  const pupilOffset = mood === "thinking" ? -1 : mood === "working" ? 1 : 0;

  return (
    <g>
      {/* Halo (drawn behind body so head sits in front) */}
      {g.hasHalo && (
        <>
          <rect x={bx + 1} y={by - 3} width={bw - 2} height={1} fill={accent} />
          <rect x={bx + 2} y={by - 4} width={bw - 4} height={1} fill={SHEEN} opacity={0.7} />
        </>
      )}

      {/* Crown pip (final form only) — sits inside the halo */}
      {g.hasCrown && (
        <>
          <rect x={bx + Math.floor(bw / 2)} y={by - 5} width={1} height={1} fill={accent} />
        </>
      )}

      {/* Antenna (single pixel above center) */}
      {g.hasAntenna && (
        <>
          <rect x={bx + Math.floor(bw / 2)} y={by - 1} width={1} height={1} fill={shadow} />
          <rect x={bx + Math.floor(bw / 2)} y={by - 2} width={1} height={1} fill={accent} />
        </>
      )}

      {/* Ears (side bumps on top corners) */}
      {g.hasEars && (
        <>
          <rect x={bx} y={by} width={1} height={2} fill={shadow} />
          <rect x={bx + bw - 1} y={by} width={1} height={2} fill={shadow} />
        </>
      )}

      {/* Tail (small puff to the right) */}
      {g.hasTail && (
        <>
          <rect x={bx + bw} y={by + bh - 3} width={2} height={2} fill={primary} />
          <rect x={bx + bw} y={by + bh - 1} width={2} height={1} fill={shadow} />
          <rect x={bx + bw + 1} y={by + bh - 4} width={1} height={1} fill={accent} />
        </>
      )}

      {/* Body — top shadow band + main fill + bottom shadow */}
      <rect x={bx + 1} y={by} width={bw - 2} height={1} fill={shadow} />
      <rect x={bx} y={by + 1} width={bw} height={bh - 2} fill={primary} />
      <rect x={bx + 1} y={by + bh - 1} width={bw - 2} height={1} fill={dark} />
      {/* Sheen */}
      <rect x={bx + 1} y={by + 1} width={2} height={1} fill={SHEEN} opacity={0.55} />
      <rect x={bx} y={by + 2} width={1} height={2} fill={SHEEN} opacity={0.4} />

      {/* Back stripe (accent line across body) */}
      {g.hasStripe && (
        <rect x={bx + 1} y={by + 2} width={bw - 2} height={1} fill={accent} opacity={0.8} />
      )}

      {/* Eyes — whites */}
      <rect x={g.eyes[0].x} y={g.eyes[0].y - 1} width={1} height={2} fill={SHEEN} />
      <rect x={g.eyes[1].x} y={g.eyes[1].y - 1} width={1} height={2} fill={SHEEN} />
      {/* Eyes — pupils (shift by mood) */}
      <rect x={g.eyes[0].x} y={g.eyes[0].y + pupilOffset} width={1} height={1} fill={EYE} />
      <rect x={g.eyes[1].x} y={g.eyes[1].y + pupilOffset} width={1} height={1} fill={EYE} />

      {/* Blush (tiny pink dots on cheeks, present from stage 2+) */}
      {stage >= 2 && (
        <>
          <rect x={g.eyes[0].x - 1} y={g.eyes[0].y + 1} width={1} height={1} fill={BLUSH} opacity={0.75} />
          <rect x={g.eyes[1].x + 1} y={g.eyes[1].y + 1} width={1} height={1} fill={BLUSH} opacity={0.75} />
        </>
      )}

      {/* Mouth — varies by mood */}
      <Mouth mood={mood} cx={g.mouthX} cy={g.mouthY} />

      {/* Mood-specific decorations */}
      {mood === "working" && (
        <>
          <rect x={bx + bw + 1} y={by + 1} width={1} height={2} fill={SWEAT} />
          <rect x={bx + bw} y={by + 2} width={1} height={1} fill={SWEAT} opacity={0.7} />
        </>
      )}
      {mood === "satisfied" && (
        <>
          <rect x={bx - 2} y={by - 1} width={1} height={1} fill={SPARK} />
          <rect x={bx + bw + 1} y={by - 2} width={1} height={1} fill={SPARK} />
          <rect x={bx - 1} y={by + bh + 1} width={1} height={1} fill={SPARK} />
        </>
      )}

      {/* Floating sparkle (stage 7+) */}
      {g.hasSparkle && (
        <FloatingSparkle x={bx + bw + 2} y={by - 1} color={accent} />
      )}

      {/* Final-form extra sparkles (stage 9) */}
      {stage === 9 && (
        <>
          <rect x={bx - 2} y={by + 3} width={1} height={1} fill={accent} />
          <rect x={bx + bw + 1} y={by + 5} width={1} height={1} fill={accent} />
          <rect x={bx + 1} y={by - 6} width={1} height={1} fill={accent} opacity={0.7} />
        </>
      )}
    </g>
  );
}

function Mouth({ mood, cx, cy }: { mood: Mood; cx: number; cy: number }) {
  switch (mood) {
    case "eating":
      return <rect x={cx} y={cy} width={2} height={2} fill={MOUTH} />;
    case "thinking":
      return <rect x={cx} y={cy + 1} width={2} height={1} fill={MOUTH} />;
    case "working":
      return <rect x={cx - 1} y={cy + 1} width={4} height={1} fill={MOUTH} />;
    case "satisfied":
      return (
        <g>
          <rect x={cx - 1} y={cy} width={1} height={1} fill={MOUTH} />
          <rect x={cx + 2} y={cy} width={1} height={1} fill={MOUTH} />
          <rect x={cx} y={cy + 1} width={2} height={1} fill={MOUTH} />
        </g>
      );
    case "idle":
    default:
      return (
        <g>
          <rect x={cx - 1} y={cy} width={1} height={1} fill={MOUTH} />
          <rect x={cx} y={cy + 1} width={2} height={1} fill={MOUTH} />
          <rect x={cx + 2} y={cy} width={1} height={1} fill={MOUTH} />
        </g>
      );
  }
}

function FloatingSparkle({ x, y, color }: { x: number; y: number; color: string }) {
  const style: CSSProperties = { transformOrigin: `${x + 0.5}px ${y + 0.5}px` };
  return (
    <g className="float-sparkle" style={style}>
      <rect x={x} y={y - 1} width={1} height={3} fill={color} />
      <rect x={x - 1} y={y} width={3} height={1} fill={color} />
    </g>
  );
}
