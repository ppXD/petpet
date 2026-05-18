/**
 * TemplateCreator — GBA-styled scaffold-a-new-template flow.
 *
 * Layout (after the user's "form too long" feedback):
 *
 *   ┌─ CREATE TEMPLATE ──────────────────┐
 *   │ Template name [_______________]    │
 *   │ Your name     [_______________]    │
 *   │ Description   [_______________]    │
 *   │ id: <author>.<name>                │
 *   │                                    │
 *   │ Configuration                      │
 *   │ ┌──────────┐ ┌──────────┐          │
 *   │ │  LEVELS  │ │  STAGES  │          │
 *   │ │  100 lvl │ │ 10 stages│          │
 *   │ │  (Short) │ │(Extended)│          │
 *   │ └──────────┘ └──────────┘          │
 *   │                                    │
 *   │            [Cancel] [Create]       │
 *   └────────────────────────────────────┘
 *
 * Each button pops a modal-on-modal editor — full per-row editing
 * with "quick-fill from system preset" as a one-click shortcut at
 * the top.
 *
 * Architecture:
 *   - TemplateCreator owns the `levels` and `stages` arrays as React
 *     state. Editors receive a draft + onSave/onCancel callbacks; they
 *     hold their own local edit buffer and only emit on Save (Cancel
 *     reverts).
 *   - On Create, we send the full literal `levels` + `stages` to
 *     `template_create` (not just a preset id) — the backend writes
 *     them straight to disk. Snapshot copy, no runtime preset link.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

interface Props {
  /** Snapshot of every installed template (built-in + community).
   *  Reserved for future use; currently unused but kept on the prop
   *  surface so the parent's call site doesn't have to change when
   *  we eventually surface "clone from existing template" affordances. */
  templates: TemplateInfo[];
  onCancel: () => void;
  onCreated: (templateId: string) => void;
}

interface TemplateInfo {
  template: { meta: { id: string; name: string } };
  source: string;
  dir: string;
}

interface CreateResult {
  template_id: string;
  template_dir: string;
}

// ── System preset shapes (mirror of Rust `LevelsPreset` / `StagesPreset`) ──

interface LevelEntry {
  level: number;
  xp_required: number;
}

interface LevelsPreset {
  id: string;
  name: string;
  tagline: string;
  description: string;
  max_level: number;
  entries: LevelEntry[];
}

interface StageStub {
  id: string;
  name: string;
  flavor: string;
  /// Raw trigger JSON — we restrict the editor to the leaf
  /// `{metric: "level", value: N}` shape since that's all the system
  /// presets use and it's what the UI knows how to render. Custom
  /// composite triggers are still loadable via hand-edited stage.json.
  trigger: { metric: string; value: number } | unknown;
  /// Absolute path to a sprite file the user picked in the editor's
  /// per-stage image picker. When set, the backend copies it to
  /// `<template>/stages/stage_N/sprite.png` and skips the linear-
  /// remap fallback. When null/undefined, the cloned base preset's
  /// sprite (remapped to this stage's relative position) is used.
  /// Always undefined on system presets — those carry only structural
  /// data, never per-stage art.
  spritePath?: string | null;
}

interface StagesPreset {
  id: string;
  name: string;
  tagline: string;
  description: string;
  stages: StageStub[];
}

function deriveId(author: string, name: string): string {
  const slug = (s: string) => {
    let out = "";
    let prevDash = false;
    for (const ch of s.toLowerCase()) {
      if (/[a-z0-9]/.test(ch)) {
        out += ch;
        prevDash = false;
      } else if (
        (ch === " " || ch === "_" || ch === "-") &&
        !prevDash &&
        out.length > 0
      ) {
        out += "-";
        prevDash = true;
      }
    }
    while (out.endsWith("-")) out = out.slice(0, -1);
    return out;
  };
  const a = slug(author);
  const n = slug(name);
  if (!a || !n) return "";
  return `${a}.${n}`;
}

/** Default state when no preset has been loaded yet — keeps the form
 *  renderable even before the async preset list resolves. The defaults
 *  match the `unicorn` builtin (easy-difficulty, short levels curve),
 *  so a user who clicks Create immediately gets a sensible starting
 *  shape. Replaced once preset data lands. */
const FALLBACK_LEVELS: LevelEntry[] = [{ level: 0, xp_required: 0 }];
const FALLBACK_STAGES: StageStub[] = [
  {
    id: "stage_0",
    name: "Stage 0",
    flavor: "Starting form.",
    trigger: { metric: "level", value: 0 },
  },
];

const DEFAULT_LEVELS_PRESET = "short";
const DEFAULT_STAGES_PRESET = "extended";

export function TemplateCreator({ templates: _templates, onCancel, onCreated }: Props) {
  const [name, setName] = useState("");
  const [author, setAuthor] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const nameRef = useRef<HTMLInputElement>(null);

  const [levelsPresets, setLevelsPresets] = useState<LevelsPreset[]>([]);
  const [stagesPresets, setStagesPresets] = useState<StagesPreset[]>([]);
  const [levels, setLevels] = useState<LevelEntry[]>(FALLBACK_LEVELS);
  const [stages, setStages] = useState<StageStub[]>(FALLBACK_STAGES);
  /// Track which preset (if any) the current `levels` / `stages` data
  /// last matched. Used only for the summary chip label — "(Short)" /
  /// "(custom)". Editing manually flips this to null until the user
  /// re-applies a preset.
  const [levelsPresetTag, setLevelsPresetTag] = useState<string | null>(null);
  const [stagesPresetTag, setStagesPresetTag] = useState<string | null>(null);
  const [presetsErr, setPresetsErr] = useState<string | null>(null);

  /// Which (if any) sub-editor is open. The two modal editors render
  /// as siblings to the main form so their backdrop overlays the
  /// creator without unmounting the form state behind it.
  const [editing, setEditing] = useState<null | "levels" | "stages">(null);

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  // Load both preset lists on mount, then seed the local draft state
  // with the defaults. After that, the user owns the data — preset
  // selections become "quick-fill" shortcuts inside the editors.
  useEffect(() => {
    let cancelled = false;
    Promise.all([
      invoke<LevelsPreset[]>("preset_list_levels"),
      invoke<StagesPreset[]>("preset_list_stages"),
    ])
      .then(([lvls, stgs]) => {
        if (cancelled) return;
        setLevelsPresets(lvls);
        setStagesPresets(stgs);
        const lp = lvls.find((p) => p.id === DEFAULT_LEVELS_PRESET) ?? lvls[0];
        const sp = stgs.find((p) => p.id === DEFAULT_STAGES_PRESET) ?? stgs[0];
        if (lp) {
          setLevels(lp.entries);
          setLevelsPresetTag(lp.id);
        }
        if (sp) {
          setStages(sp.stages);
          setStagesPresetTag(sp.id);
        }
      })
      .catch((e) => {
        if (cancelled) return;
        setPresetsErr(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const id = useMemo(() => deriveId(author, name), [author, name]);
  const canCreate = id.length > 0 && !busy && !presetsErr && levels.length > 0 && stages.length > 0;

  // `max_level` of the curve is just the last entry's level (entries
  // are 0-indexed contiguous, validated on Save inside the editor).
  // The summary chip already conveys "100 entries (Short)" — the
  // earlier verbose "Lv.0 → Lv.99, 12,045 XP total" detail line went
  // away with the card slim-down, so we no longer need maxXp here.
  const maxLevel = levels.length > 0 ? levels[levels.length - 1].level : 0;

  // Human-readable summary chips for the two configuration buttons.
  const levelsChip =
    levelsPresetTag !== null
      ? `${levels.length} levels (${prettyPresetName(levelsPresets, levelsPresetTag)})`
      : `${levels.length} levels (custom)`;
  const stagesChip =
    stagesPresetTag !== null
      ? `${stages.length} stages (${prettyPresetName(stagesPresets, stagesPresetTag)})`
      : `${stages.length} stages (custom)`;

  const create = async () => {
    if (!canCreate) return;
    setBusy(true);
    setError(null);
    try {
      // Send the literal levels + stages — the backend writes them
      // straight to levels.json + stages/stage_N/stage.json. We don't
      // bother forwarding the preset tag since the literal data is
      // the source of truth and the tag is purely UI bookkeeping.
      const result = await invoke<CreateResult>("template_create", {
        name: name.trim(),
        author: author.trim(),
        description: description.trim() || null,
        levels: { max_level: maxLevel, entries: levels },
        stages: stages.map((s) => ({
          id: s.id,
          name: s.name,
          flavor: s.flavor,
          trigger: s.trigger,
          // Forward the per-stage sprite pick (may be null → backend
          // falls back to the linear-remap of the cloned base's
          // sprites). Snake_case to match the Rust struct field
          // serde expects inside the nested object.
          sprite_path: s.spritePath ?? null,
        })),
      });
      await invoke("notify_show", {
        text: `Template "${name.trim()}" created.`,
        durationMs: 4500,
      }).catch(() => {});
      onCreated(result.template_id);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  return (
    <>
      <div className="creator-overlay">
        <div className="gba-box creator-box">
          <div className="gba-title creator-title">CREATE TEMPLATE</div>

          <div className="creator-form">
            <label className="creator-label">Template name</label>
            <input
              ref={nameRef}
              className="creator-input"
              type="text"
              value={name}
              placeholder="e.g. Drakon"
              onChange={(e) => setName(e.target.value)}
              disabled={busy}
              maxLength={48}
            />

            <label className="creator-label">Your name</label>
            <input
              className="creator-input"
              type="text"
              value={author}
              placeholder="e.g. Mars"
              onChange={(e) => setAuthor(e.target.value)}
              disabled={busy}
              maxLength={48}
            />

            <label className="creator-label">Description (optional)</label>
            <textarea
              className="creator-input creator-textarea"
              value={description}
              placeholder="A companion that grows with every prompt."
              onChange={(e) => setDescription(e.target.value)}
              disabled={busy}
              rows={2}
              maxLength={200}
            />

            <div className="creator-id-hint">
              id: <code>{id || "(fill in both fields above)"}</code>
            </div>

            {presetsErr && (
              <div className="picker-error">
                ⚠ Couldn't load preset library: {presetsErr}
              </div>
            )}

            {/* ── Configuration: two buttons + summary chip each ── */}
            <label className="creator-label creator-section-label">
              Configuration
            </label>
            {/* Single-line config rows — title on the left, one-line
             *  summary in the middle, chevron on the right. Replaces
             *  the previous three-line vertical cards because the
             *  extra rows added visual noise without adding info the
             *  user couldn't see by clicking through to the editor. */}
            <div className="creator-config-row">
              <button
                type="button"
                className="creator-config-card"
                onClick={() => setEditing("levels")}
                disabled={busy || presetsErr !== null}
              >
                <span className="creator-config-card-title">LEVELS</span>
                <span className="creator-config-card-sub">{levelsChip}</span>
                <span className="creator-config-card-chevron" aria-hidden>▸</span>
              </button>
              <button
                type="button"
                className="creator-config-card"
                onClick={() => setEditing("stages")}
                disabled={busy || presetsErr !== null}
              >
                <span className="creator-config-card-title">STAGES</span>
                <span className="creator-config-card-sub">{stagesChip}</span>
                <span className="creator-config-card-chevron" aria-hidden>▸</span>
              </button>
            </div>

            <div className="creator-explainer">
              Sprite art, XP rules and ceremony scripts come from the
              bundled <b>Mist</b> template — replace them later by editing
              files in your template's folder. Share via the <b>Export…</b>
              button; friends receive a <code>.petpet</code> file.
            </div>
          </div>

          {error && <div className="picker-error">⚠ {error}</div>}

          <div className="picker-actions">
            <button className="gba-button" onClick={onCancel} disabled={busy}>
              Cancel
            </button>
            <button
              className="gba-button primary"
              onClick={create}
              disabled={!canCreate}
            >
              {busy ? "Creating…" : "Create"}
            </button>
          </div>
        </div>
      </div>

      {editing === "levels" && (
        <LevelsEditor
          initial={levels}
          presets={levelsPresets}
          activePresetId={levelsPresetTag}
          onCancel={() => setEditing(null)}
          onSave={(next, presetTag) => {
            setLevels(next);
            setLevelsPresetTag(presetTag);
            setEditing(null);
          }}
        />
      )}

      {editing === "stages" && (
        <StagesEditor
          initial={stages}
          presets={stagesPresets}
          activePresetId={stagesPresetTag}
          onCancel={() => setEditing(null)}
          onSave={(next, presetTag) => {
            setStages(next);
            setStagesPresetTag(presetTag);
            setEditing(null);
          }}
        />
      )}
    </>
  );
}

function prettyPresetName<T extends { id: string; name: string }>(
  list: T[],
  id: string | null,
): string {
  if (!id) return "custom";
  return list.find((p) => p.id === id)?.name ?? id;
}

// ─── LevelsEditor ──────────────────────────────────────────────────

/// Default XP step the +Add button appends. Picked to roughly match
/// the slope of the "short" preset's middle-band so a hand-added row
/// doesn't feel like a cliff. The user can edit immediately.
const DEFAULT_XP_STEP = 100;

function LevelsEditor({
  initial,
  presets,
  activePresetId,
  onCancel,
  onSave,
}: {
  initial: LevelEntry[];
  presets: LevelsPreset[];
  activePresetId: string | null;
  onCancel: () => void;
  onSave: (next: LevelEntry[], presetTag: string | null) => void;
}) {
  const [draft, setDraft] = useState<LevelEntry[]>(() => initial.map((e) => ({ ...e })));
  const [presetSel, setPresetSel] = useState<string>(activePresetId ?? "");
  const [presetTag, setPresetTag] = useState<string | null>(activePresetId);
  const [err, setErr] = useState<string | null>(null);

  // Esc cancels; Enter on a row commits the input (default behaviour).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);

  const applyPreset = () => {
    const p = presets.find((p) => p.id === presetSel);
    if (!p) return;
    setDraft(p.entries.map((e) => ({ ...e })));
    setPresetTag(p.id);
    setErr(null);
  };

  const updateXp = (rowIdx: number, val: string) => {
    const n = parseInt(val, 10);
    if (Number.isNaN(n) || n < 0) return;
    setDraft((cur) => {
      const next = cur.map((e) => ({ ...e }));
      next[rowIdx] = { level: rowIdx, xp_required: n };
      return next;
    });
    setPresetTag(null); // edits invalidate the preset tag
  };

  const addLevel = () => {
    setDraft((cur) => {
      const last = cur[cur.length - 1];
      const next = [...cur, { level: cur.length, xp_required: last.xp_required + DEFAULT_XP_STEP }];
      return next;
    });
    setPresetTag(null);
  };

  const removeLevel = (rowIdx: number) => {
    if (rowIdx === 0) return; // Lv.0 can't be removed
    setDraft((cur) => {
      const out: LevelEntry[] = [];
      for (let i = 0; i < cur.length; i++) {
        if (i === rowIdx) continue;
        // Renumber so levels stay 0..N contiguous after the splice.
        out.push({ level: out.length, xp_required: cur[i].xp_required });
      }
      return out;
    });
    setPresetTag(null);
  };

  const commit = () => {
    // Defensive validation mirroring the backend's checks — surface
    // user-fixable errors here, before the round-trip.
    if (draft.length === 0) {
      setErr("At least one level is required.");
      return;
    }
    if (draft[0].xp_required !== 0) {
      setErr("Level 0 must have 0 XP required.");
      return;
    }
    for (let i = 1; i < draft.length; i++) {
      if (draft[i].xp_required < draft[i - 1].xp_required) {
        setErr(
          `Lv.${i} requires ${draft[i].xp_required} XP, less than Lv.${i - 1} (${draft[i - 1].xp_required}). XP must increase per level.`,
        );
        return;
      }
    }
    onSave(draft, presetTag);
  };

  const maxXp = draft[draft.length - 1]?.xp_required ?? 0;

  return (
    <div className="editor-overlay" onClick={onCancel}>
      <div
        className="gba-box editor-box"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <div className="gba-title editor-title">CONFIGURE LEVELS</div>

        <div className="editor-quickfill">
          <span className="editor-quickfill-label">Quick-fill from preset:</span>
          <select
            className="creator-select editor-quickfill-select"
            value={presetSel}
            onChange={(e) => setPresetSel(e.target.value)}
          >
            <option value="">— pick one —</option>
            {presets.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name} — {p.tagline}
              </option>
            ))}
          </select>
          <button
            type="button"
            className="gba-button editor-quickfill-btn"
            onClick={applyPreset}
            disabled={!presetSel}
          >
            Apply
          </button>
        </div>

        <div className="editor-summary">
          {draft.length} entries · Lv.0 → Lv.{Math.max(0, draft.length - 1)} ·
          {" "}{maxXp.toLocaleString()} XP total
        </div>

        <div className="editor-rows">
          {draft.map((entry, i) => (
            <div key={i} className="editor-row">
              <span className="editor-row-idx">Lv.{i}</span>
              <input
                className="editor-row-input"
                type="number"
                min={0}
                step={50}
                value={entry.xp_required}
                disabled={i === 0}
                onChange={(e) => updateXp(i, e.target.value)}
                aria-label={`XP required for level ${i}`}
              />
              <span className="editor-row-unit">XP</span>
              <button
                type="button"
                className="editor-row-del"
                onClick={() => removeLevel(i)}
                disabled={i === 0}
                title={i === 0 ? "Lv.0 is required" : "Delete this level"}
                aria-label={`Delete level ${i}`}
              >
                ×
              </button>
            </div>
          ))}
        </div>

        <button
          type="button"
          className="editor-add-row"
          onClick={addLevel}
        >
          + Add level (Lv.{draft.length})
        </button>

        {err && <div className="picker-error">⚠ {err}</div>}

        <div className="picker-actions">
          <button className="gba-button" onClick={onCancel}>
            Cancel
          </button>
          <button className="gba-button primary" onClick={commit}>
            Save
          </button>
        </div>
      </div>
    </div>
  );
}

// ─── StagesEditor ──────────────────────────────────────────────────

const DEFAULT_STAGE_LEVEL_STEP = 5;

function StagesEditor({
  initial,
  presets,
  activePresetId,
  onCancel,
  onSave,
}: {
  initial: StageStub[];
  presets: StagesPreset[];
  activePresetId: string | null;
  onCancel: () => void;
  onSave: (next: StageStub[], presetTag: string | null) => void;
}) {
  const [draft, setDraft] = useState<StageStub[]>(() =>
    initial.map((s) => ({
      ...s,
      // Defensive clone of trigger so editor mutations don't affect parent state.
      trigger: { ...((s.trigger as object) ?? {}) } as StageStub["trigger"],
      spritePath: s.spritePath ?? null,
    })),
  );
  const [presetSel, setPresetSel] = useState<string>(activePresetId ?? "");
  const [presetTag, setPresetTag] = useState<string | null>(activePresetId);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);

  const applyPreset = () => {
    const p = presets.find((p) => p.id === presetSel);
    if (!p) return;
    setDraft(
      p.stages.map((s, i) => ({
        // Re-id so stage_0..stage_N stays contiguous even if a future
        // preset ever ships gaps.
        id: `stage_${i}`,
        name: s.name,
        flavor: s.flavor,
        trigger: { ...((s.trigger as object) ?? {}) } as StageStub["trigger"],
        // Applying a preset wipes per-stage sprite picks — the user
        // is starting over with this preset's structure, and the
        // backend's sprite-remap fallback will fill in art from the
        // cloned base template.
        spritePath: null,
      })),
    );
    setPresetTag(p.id);
    setErr(null);
  };

  /// Open a native file picker, COPY the chosen file into petpet's
  /// staging dir, and store the staging path against the given stage
  /// row. The copy serves two purposes:
  ///   1. The staging dir lives under `~/.petpet/`, which is in the
  ///      Tauri asset-protocol scope — so `convertFileSrc(stagingPath)`
  ///      can render the thumbnail preview. The original `~/Pictures/…`
  ///      path is out of scope and would fail silently.
  ///   2. The file is "captured" at pick time. If the user moves /
  ///      deletes the original before clicking Create, the staged
  ///      copy still exists and template_create can complete.
  /// Filter restricts to common image extensions so the user doesn't
  /// accidentally pick a .docx or whatever.
  const pickSprite = async (rowIdx: number) => {
    try {
      const picked = await openDialog({
        multiple: false,
        directory: false,
        filters: [
          { name: "Image", extensions: ["png", "jpg", "jpeg", "gif", "webp"] },
        ],
      });
      // `open` returns `string | null` for single-file mode. The Tauri
      // type system says `string | string[] | null`, but multiple:false
      // narrows to single; we defensively handle both.
      const rawPath = Array.isArray(picked) ? picked[0] ?? null : picked;
      if (!rawPath) return; // user cancelled
      // Stage the file. Backend validates size + readability, copies
      // to ~/.petpet/template-staging/sprites/<uuid>.<ext>, returns
      // the staging path. Any "file too big / unreadable" error
      // surfaces as a single inline message rather than confusing
      // the user later at Create time.
      const { staged_path } = await invoke<{ staged_path: string }>(
        "sprite_stage_for_picker",
        { srcPath: rawPath },
      );
      setDraft((cur) => {
        const next = cur.map((s) => ({
          ...s,
          trigger: { ...((s.trigger as object) ?? {}) } as StageStub["trigger"],
        }));
        next[rowIdx] = { ...next[rowIdx], spritePath: staged_path };
        return next;
      });
      setPresetTag(null);
      setErr(null);
    } catch (e) {
      setErr(`Couldn't use that image: ${e}`);
    }
  };

  const clearSprite = (rowIdx: number) => {
    setDraft((cur) => {
      const next = cur.map((s) => ({
        ...s,
        trigger: { ...((s.trigger as object) ?? {}) } as StageStub["trigger"],
      }));
      next[rowIdx] = { ...next[rowIdx], spritePath: null };
      return next;
    });
    setPresetTag(null);
  };

  /// Get the level value from a stage's trigger, defaulting to 0 when
  /// the trigger isn't a simple leaf `{metric: "level", value: N}`
  /// (e.g. a composite trigger from a hand-edited template). The
  /// editor only supports leaf triggers — composite ones survive but
  /// render as level 0.
  const getTriggerLevel = (s: StageStub): number => {
    const t = s.trigger as { metric?: string; value?: number } | undefined;
    return t && t.metric === "level" && typeof t.value === "number" ? t.value : 0;
  };

  const updateField = (
    rowIdx: number,
    field: "name" | "flavor",
    val: string,
  ) => {
    setDraft((cur) => {
      const next = cur.map((s) => ({
        ...s,
        trigger: { ...((s.trigger as object) ?? {}) } as StageStub["trigger"],
      }));
      next[rowIdx] = { ...next[rowIdx], [field]: val };
      return next;
    });
    setPresetTag(null);
  };

  const updateTriggerLevel = (rowIdx: number, val: string) => {
    const n = parseInt(val, 10);
    if (Number.isNaN(n) || n < 0) return;
    setDraft((cur) => {
      const next = cur.map((s) => ({
        ...s,
        trigger: { ...((s.trigger as object) ?? {}) } as StageStub["trigger"],
      }));
      next[rowIdx] = {
        ...next[rowIdx],
        trigger: { metric: "level", value: n },
      };
      return next;
    });
    setPresetTag(null);
  };

  const addStage = () => {
    setDraft((cur) => {
      const last = cur[cur.length - 1];
      const lastLevel = last ? getTriggerLevel(last) : 0;
      return [
        ...cur,
        {
          id: `stage_${cur.length}`,
          name: `Stage ${cur.length}`,
          flavor: "",
          trigger: { metric: "level", value: lastLevel + DEFAULT_STAGE_LEVEL_STEP },
          // New stages have no custom sprite yet — backend remap
          // fallback picks a representative sprite from the cloned
          // base. User can override via the picker on this row.
          spritePath: null,
        },
      ];
    });
    setPresetTag(null);
  };

  const removeStage = (rowIdx: number) => {
    if (rowIdx === 0) return; // stage_0 must exist (the starting form at Lv.0)
    setDraft((cur) => {
      const out: StageStub[] = [];
      for (let i = 0; i < cur.length; i++) {
        if (i === rowIdx) continue;
        out.push({
          ...cur[i],
          id: `stage_${out.length}`, // renumber to stay 0-contiguous
          trigger: { ...((cur[i].trigger as object) ?? {}) } as StageStub["trigger"],
          // Preserve the per-stage sprite pick across renumbering —
          // the user picked it for THIS stage's content, not for
          // its index. Index changes, content (and its sprite) stays.
          spritePath: cur[i].spritePath ?? null,
        });
      }
      return out;
    });
    setPresetTag(null);
  };

  const commit = () => {
    if (draft.length === 0) {
      setErr("At least one stage is required.");
      return;
    }
    if (getTriggerLevel(draft[0]) !== 0) {
      setErr("The first stage must trigger at level 0 (it's the starting form).");
      return;
    }
    for (let i = 0; i < draft.length; i++) {
      if (draft[i].name.trim() === "") {
        setErr(`Stage ${i} needs a name.`);
        return;
      }
    }
    for (let i = 1; i < draft.length; i++) {
      if (getTriggerLevel(draft[i]) <= getTriggerLevel(draft[i - 1])) {
        setErr(
          `Stage ${i} ("${draft[i].name}") triggers at Lv.${getTriggerLevel(
            draft[i],
          )}, not after stage ${i - 1} (Lv.${getTriggerLevel(draft[i - 1])}). Stage triggers must increase.`,
        );
        return;
      }
    }
    onSave(draft, presetTag);
  };

  return (
    <div className="editor-overlay" onClick={onCancel}>
      <div
        className="gba-box editor-box editor-box-wide"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <div className="gba-title editor-title">CONFIGURE STAGES</div>

        <div className="editor-quickfill">
          <span className="editor-quickfill-label">Quick-fill from preset:</span>
          <select
            className="creator-select editor-quickfill-select"
            value={presetSel}
            onChange={(e) => setPresetSel(e.target.value)}
          >
            <option value="">— pick one —</option>
            {presets.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name} — {p.tagline}
              </option>
            ))}
          </select>
          <button
            type="button"
            className="gba-button editor-quickfill-btn"
            onClick={applyPreset}
            disabled={!presetSel}
          >
            Apply
          </button>
        </div>

        <div className="editor-summary">
          {draft.length} stages · per-stage sprite optional (defaults remap from base)
        </div>

        <div className="editor-rows">
          {draft.map((s, i) => (
            <div key={i} className="editor-row editor-stage-row">
              <span className="editor-row-idx">{s.id}</span>

              {/* Sprite column — thumbnail (or placeholder) + pick/
               *  change/clear controls. Clicking the thumb itself
               *  also opens the picker; the explicit button below
               *  is for users who don't realise the thumb is
               *  clickable. */}
              <div className="editor-stage-sprite">
                <button
                  type="button"
                  className={`editor-sprite-thumb ${s.spritePath ? "has-image" : "is-empty"}`}
                  onClick={() => pickSprite(i)}
                  title={s.spritePath ? "Click to choose a different image" : "Click to choose a sprite for this stage"}
                  aria-label={`Sprite for stage ${i}`}
                >
                  {s.spritePath ? (
                    <img
                      src={convertFileSrc(s.spritePath)}
                      alt=""
                      draggable={false}
                      onError={(e) => {
                        // File moved / deleted between pick and render
                        // — fall back to a visible "broken" indicator
                        // so the user knows to re-pick.
                        (e.target as HTMLImageElement).style.visibility = "hidden";
                      }}
                    />
                  ) : (
                    <span className="editor-sprite-glyph">+</span>
                  )}
                </button>
                <button
                  type="button"
                  className="editor-sprite-pick"
                  onClick={() => pickSprite(i)}
                >
                  {s.spritePath ? "Change" : "Pick"}
                </button>
                {s.spritePath && (
                  <button
                    type="button"
                    className="editor-sprite-clear"
                    onClick={() => clearSprite(i)}
                    title="Revert to inherited sprite from base preset"
                  >
                    Default
                  </button>
                )}
              </div>

              <div className="editor-stage-fields">
                <input
                  className="editor-row-input editor-stage-name"
                  type="text"
                  value={s.name}
                  placeholder="Stage name (e.g. Starting form)"
                  onChange={(e) => updateField(i, "name", e.target.value)}
                  maxLength={32}
                  aria-label={`Stage ${i} name`}
                />
                <input
                  className="editor-row-input editor-stage-flavor"
                  type="text"
                  value={s.flavor}
                  placeholder="Flavour line (optional)"
                  onChange={(e) => updateField(i, "flavor", e.target.value)}
                  maxLength={80}
                  aria-label={`Stage ${i} flavor`}
                />
                <div className="editor-stage-trigger">
                  <span className="editor-row-unit">at Lv.</span>
                  <input
                    className="editor-row-input editor-stage-level"
                    type="number"
                    min={0}
                    step={1}
                    value={getTriggerLevel(s)}
                    disabled={i === 0}
                    onChange={(e) => updateTriggerLevel(i, e.target.value)}
                    aria-label={`Stage ${i} trigger level`}
                  />
                </div>
              </div>
              <button
                type="button"
                className="editor-row-del"
                onClick={() => removeStage(i)}
                disabled={i === 0}
                title={i === 0 ? "stage_0 is required (the starting form at Lv.0)" : "Delete this stage"}
                aria-label={`Delete stage ${i}`}
              >
                ×
              </button>
            </div>
          ))}
        </div>

        <button
          type="button"
          className="editor-add-row"
          onClick={addStage}
        >
          + Add stage (stage_{draft.length})
        </button>

        {err && <div className="picker-error">⚠ {err}</div>}

        <div className="picker-actions">
          <button className="gba-button" onClick={onCancel}>
            Cancel
          </button>
          <button className="gba-button primary" onClick={commit}>
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
