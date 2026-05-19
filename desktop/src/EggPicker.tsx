/**
 * EggPicker — first-launch (and on-demand) "choose your companion" UI.
 *
 * Two-step flow:
 *   1. Template list (scrollable, GBA Pokémon menu style).
 *      ▶ select with ↑/↓, mouse hover/click, or j/k.
 *      Enter or click confirms selection — opens NameModal.
 *   2. NameModal — input pet name (optional) + Confirm / Cancel.
 *
 * The picker is fully template-driven:
 *   - preview image  ← template.assets.thumb (or stage 0 sprite fallback)
 *   - labels         ← template.labels  (array of strings)
 *   - description    ← template.species.flavor + template.meta.description
 *   - pet name       ← template.species.default_pet_name (fallback)
 *
 * Templates loaded dynamically via `template_list` Tauri command,
 * which scans both the bundled builtin folder and `~/.petpet/templates/`.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { TemplateCreator } from "./TemplateCreator";

/// Author shape on the wire. Matches the Rust `Author` untagged enum:
///   - bare string: `"author": "petpet-builtin"` (3 builtins, npm-style)
///   - object: `"author": {"name": "Mars", "url": "…"}` (creator-scaffolded
///     templates, leaving room for an optional URL)
/// Both forms ship in the wild — `authorLabel` discriminates by typeof.
type AuthorBlock = string | { name?: string; url?: string };

interface TemplateMeta {
  id: string;
  name: string;
  version: string;
  description?: string;
  /// Optional author block — see `AuthorBlock`. The author's name (in
  /// whichever form) takes precedence over the namespace prefix in
  /// `id` (e.g. "mars" in "mars.drakon") when rendering the "by
  /// <author>" chip in the picker row.
  author?: AuthorBlock;
  /// Display ordering hint for the egg-picker UI. Lower = shown earlier.
  /// Builtin difficulty ladder: unicorn=1, sun=2, kingkong=3.
  /// Templates without an explicit value (user-imported community ones)
  /// sort AFTER explicit-order templates, by name alphabetically.
  display_order?: number;
}

/// Disambiguator label for the egg-picker row. Two templates with the
/// same display name but different ids (e.g. `mars.drakon` and
/// `alice.drakon`) need to be distinguishable at a glance — this
/// resolves a one-word author string with a sensible fallback chain.
function authorLabel(meta: TemplateMeta, source: string): string | null {
  const fromAuthor = readAuthorName(meta.author);
  if (fromAuthor) return fromAuthor;
  // Namespace-style id ("publisher.name") — use the prefix.
  const dot = meta.id.indexOf(".");
  if (dot > 0) {
    return meta.id.slice(0, dot);
  }
  // Built-in templates ship without an author; surface the source.
  if (source === "builtin") return "petpet";
  return null;
}

function readAuthorName(author: AuthorBlock | undefined): string | null {
  if (!author) return null;
  if (typeof author === "string") {
    const t = author.trim();
    return t.length > 0 ? t : null;
  }
  if (author.name && author.name.trim().length > 0) {
    return author.name.trim();
  }
  return null;
}

interface TemplateSpecies {
  name: string;
  description?: string;
  default_pet_name?: string;
  flavor?: string;
}

interface TemplateAssets {
  sheet?: string;
  frames?: string;
  thumb?: string;
}

/**
 * One label. Either a plain string ("Easy") for default uniform style,
 * or an object with custom colours.
 */
type Label =
  | string
  | { text: string; color?: string; fg?: string };

interface Template {
  $schema?: string;
  meta: TemplateMeta;
  species: TemplateSpecies;
  /** Catalog tags shown on each row. Each entry is a string OR an
   *  object with custom background/foreground colours. */
  labels?: Label[];
  assets?: TemplateAssets;
}

interface TemplateInfo {
  template: Template;
  source: string;
  dir: string;
}

interface Props {
  /** Called after the pet was successfully created. */
  onConfirm: () => void;
  /** Called when user dismisses the picker (only available when an active pet already exists). */
  onCancel?: () => void;
  /** Triggers the parent's import flow (file picker → archive_import).
   *  EggPicker re-fetches its template list when this resolves so a
   *  freshly-imported template appears in the row immediately. */
  onImport?: () => Promise<void> | void;
}

export function EggPicker({ onConfirm, onCancel, onImport }: Props) {
  const [templates, setTemplates] = useState<TemplateInfo[]>([]);
  const [selectedIdx, setSelectedIdx] = useState(0);
  const [namingFor, setNamingFor] = useState<TemplateInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showCreator, setShowCreator] = useState(false);
  const rowRefs = useRef<(HTMLDivElement | null)[]>([]);

  /// Fetch the template list from disk and update local state.
  /// Returns the sorted rows so callers can chain on the *fresh* data
  /// (e.g. auto-select a just-created template by id) without racing
  /// React's batched state updates. Returns null on error — the
  /// `error` state is set separately for the user-facing banner.
  const loadTemplates = useCallback(async (): Promise<TemplateInfo[] | null> => {
    try {
      const rows = await invoke<TemplateInfo[]>("template_list");
      const sourceRank = (s: string) =>
        s === "builtin" ? 0 : s === "community" ? 1 : 2;
      // Templates without `display_order` sort AFTER explicit ones —
      // builtin difficulty ladder (unicorn=1, sun=2, kingkong=3) wins,
      // community/custom imports fall back to name alphabetic at the
      // tail. Using +Infinity keeps the comparator stable and avoids
      // mixing "unordered" templates into the middle of the ladder.
      const orderRank = (m: TemplateMeta) =>
        m.display_order ?? Number.POSITIVE_INFINITY;
      rows.sort(
        (a, b) =>
          sourceRank(a.source) - sourceRank(b.source) ||
          orderRank(a.template.meta) - orderRank(b.template.meta) ||
          a.template.meta.name.localeCompare(b.template.meta.name),
      );
      setTemplates(rows);
      return rows;
    } catch (e) {
      setError(String(e));
      return null;
    }
  }, []);

  // Fetch templates dynamically.
  useEffect(() => {
    loadTemplates();
  }, [loadTemplates]);

  // Keep selected row in view.
  useEffect(() => {
    rowRefs.current[selectedIdx]?.scrollIntoView({
      block: "nearest",
      behavior: "smooth",
    });
  }, [selectedIdx]);

  // Keyboard nav (suspended while name modal is open).
  useEffect(() => {
    if (namingFor) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "ArrowDown" || e.key === "j") {
        setSelectedIdx((i) => Math.min(i + 1, templates.length - 1));
        e.preventDefault();
      } else if (e.key === "ArrowUp" || e.key === "k") {
        setSelectedIdx((i) => Math.max(i - 1, 0));
        e.preventDefault();
      } else if (e.key === "Enter" || e.key === " ") {
        if (templates[selectedIdx]) setNamingFor(templates[selectedIdx]);
        e.preventDefault();
      } else if (e.key === "Escape" && onCancel) {
        onCancel();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [namingFor, templates, selectedIdx, onCancel]);

  const selected = templates[selectedIdx];

  return (
    <div className="egg-picker-overlay" data-tauri-drag-region>
      <div className="gba-box egg-picker">
        {/* Title row with a corner action — the title text stays
         *  visually centered (using `.gba-title-row` flex chrome),
         *  but a small "+ NEW" GBA button sits anchored to the
         *  right edge so the "create template" affordance is
         *  always visible without scrolling the list. Tooltip
         *  spells out the action on hover for first-time users. */}
        <div className="gba-title gba-title-row" data-tauri-drag-region>
          <span className="gba-title-spacer" aria-hidden />
          <span className="gba-title-text">CHOOSE YOUR COMPANION</span>
          <button
            type="button"
            className="gba-title-action"
            onClick={() => setShowCreator(true)}
            title="Create a new template"
            aria-label="Create a new template"
          >
            +
          </button>
        </div>

        <div className="egg-list" role="listbox" aria-label="Pet templates">
          {templates.map((t, i) => (
            <div
              key={t.template.meta.id}
              ref={(el) => {
                rowRefs.current[i] = el;
              }}
              className={`egg-row ${i === selectedIdx ? "selected" : ""}`}
              role="option"
              aria-selected={i === selectedIdx}
              /* Click = select (highlight row + show description /
               * enable bottom action buttons). Double-click = commit
               * (jump to naming) as a power-user shortcut. Hover no
               * longer changes selection — too easy to nudge the
               * cursor and pick a different template by accident. */
              onClick={() => setSelectedIdx(i)}
              onDoubleClick={() => setNamingFor(t)}
            >
              <span className="cursor">{i === selectedIdx ? "▶" : ""}</span>
              <TemplatePreview info={t} size={64} />
              <div className="egg-name-block">
                <span className="egg-name">{t.template.meta.name}</span>
                {(() => {
                  // Author chip — distinguishes two templates with
                  // the same display name but different namespaces
                  // (e.g. `mars.drakon` vs `alice.drakon`). Falls
                  // back through meta.author → id prefix → source.
                  const author = authorLabel(t.template.meta, t.source);
                  return author ? (
                    <span className="egg-author">by {author}</span>
                  ) : null;
                })()}
              </div>
              <Labels labels={t.template.labels ?? []} />
            </div>
          ))}
          {templates.length === 0 && !error && (
            <div className="egg-empty">Loading templates...</div>
          )}
        </div>

        {selected && (
          <div className="gba-box gba-box-inset egg-desc">
            {selected.template.species.flavor && (
              <div className="egg-desc-flavor">
                {selected.template.species.flavor}
              </div>
            )}
            {selected.template.meta.description && (
              <div className="egg-desc-body">
                {selected.template.meta.description}
              </div>
            )}
          </div>
        )}

        {error && <div className="picker-error">⚠ {error}</div>}

        {/* Action row — mirrors the PetSwitcher's layout so the
         *  patterns rhyme: [Back] [Export] [Confirm]. Back hidden on
         *  first-launch (no active pet to return to). Export and
         *  Confirm are gated on a selected template. */}
        <div className="picker-actions">
          {onCancel && (
            <button className="gba-button" onClick={onCancel}>
              Back
            </button>
          )}
          {onImport && (
            <button
              className="gba-button"
              onClick={async () => {
                await onImport();
                // Re-fetch — a freshly-imported template should
                // appear in the row immediately, even if the user
                // doesn't reopen the picker. Awaited so the next
                // render reflects the refreshed list before the
                // user clicks anything else.
                await loadTemplates();
              }}
            >
              Import…
            </button>
          )}
          <button
            className="gba-button"
            onClick={async () => {
              if (!selected) return;
              try {
                const out = await saveDialog({
                  defaultPath: `${selected.template.meta.id}.petpet`,
                  filters: [{ name: "petpet archive", extensions: ["petpet"] }],
                });
                if (!out) return;
                const r = await invoke<{ bytes: number }>("template_export", {
                  templateId: selected.template.meta.id,
                  outPath: out,
                });
                await invoke("notify_show", {
                  text: `Exported "${selected.template.meta.name}" — ${Math.round(r.bytes / 1024)} KB`,
                  durationMs: 5000,
                }).catch(() => {});
              } catch (e) {
                await invoke("notify_show", {
                  text: `Export failed: ${e}`,
                  durationMs: 7000,
                }).catch(() => {});
              }
            }}
            disabled={!selected}
          >
            Export…
          </button>
          <button
            className="gba-button primary"
            onClick={() => selected && setNamingFor(selected)}
            disabled={!selected}
          >
            Confirm
          </button>
        </div>

        {/* Single short keyboard tip. The bottom action row already
         *  explains the click flow visually, so the hint only needs
         *  to surface the keyboard alternative — keeping it on one
         *  line at any window width. */}
        <div className="picker-hint">
          ↑/↓ browse · Enter confirms{onCancel ? " · Esc closes" : ""}
        </div>
      </div>

      {namingFor && (
        <NameModal
          info={namingFor}
          onCancel={() => setNamingFor(null)}
          onSubmit={async (name) => {
            try {
              await invoke("pet_pick_template", {
                templateId: namingFor.template.meta.id,
                name: name.trim() === "" ? null : name.trim(),
              });
              onConfirm();
            } catch (e) {
              setError(String(e));
              setNamingFor(null);
            }
          }}
        />
      )}

      {showCreator && (
        <TemplateCreator
          // Pass the already-loaded template snapshot so the creator
          // can render a live stage / rule preview for whichever
          // preset the user selects (no extra Tauri round-trip).
          templates={templates}
          onCancel={() => setShowCreator(false)}
          onCreated={async (newId) => {
            setShowCreator(false);
            // Re-fetch from disk so the new template appears in the
            // list, then auto-select it via the returned rows (NOT
            // via a follow-up setTemplates updater — that runs in a
            // different microtask and would see the stale pre-fetch
            // state, leaving the new row invisible at the bottom
            // with no selection). Sorting puts custom templates
            // last, so the user wouldn't see it without the scroll
            // that the selectedIdx effect drives.
            const rows = await loadTemplates();
            if (rows) {
              const idx = rows.findIndex((t) => t.template.meta.id === newId);
              if (idx >= 0) {
                setSelectedIdx(idx);
              } else {
                // Most common cause is a stale `tauri dev` binary running
                // a pre-fix validator that drops `<author>.<name>` ids
                // silently. Surface this visibly so the user doesn't
                // think the create silently failed — and console.warn
                // for devs with DevTools open.
                console.warn(
                  `TemplateCreator returned id=${newId} but it isn't in the refreshed ` +
                    `list (${rows.length} rows). Likely cause: backend validation dropped ` +
                    `it on load. Restart the app (Cmd-R in dev) to pick up validator fixes, ` +
                    `or check the Rust tracing logs for "template failed validation".`,
                );
                setError(
                  `Template "${newId}" was written to disk but isn't showing up — ` +
                    `the app may be running an older build. Try closing and reopening petpet, ` +
                    `or restart your tauri dev process.`,
                );
              }
            }
          }}
        />
      )}
    </div>
  );
}

interface NameModalProps {
  info: TemplateInfo;
  onCancel: () => void;
  onSubmit: (name: string) => Promise<void>;
}

function NameModal({ info, onCancel, onSubmit }: NameModalProps) {
  const [name, setName] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const handleConfirm = async () => {
    if (submitting) return;
    setSubmitting(true);
    await onSubmit(name);
  };

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

  const speciesName = info.template.species.name;
  const defaultName = info.template.species.default_pet_name ?? speciesName;

  return (
    <div className="name-modal-backdrop" onClick={onCancel} data-tauri-drag-region>
      <div
        className="gba-box name-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <div className="gba-title" data-tauri-drag-region>
          NAME YOUR {speciesName.toUpperCase()}
        </div>
        <div className="name-modal-body">
          <div className="name-modal-egg">
            <TemplatePreview info={info} size={112} />
          </div>
          <div className="name-modal-prompt">
            What will you call this companion?
          </div>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              handleConfirm();
            }}
          >
            <input
              ref={inputRef}
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={defaultName}
              maxLength={20}
              disabled={submitting}
            />
          </form>
          <div className="name-modal-hint">
            Optional · up to 20 chars · emoji ok
          </div>
        </div>
        <div className="picker-actions">
          <button
            type="button"
            className="gba-button"
            onClick={onCancel}
            disabled={submitting}
          >
            CANCEL
          </button>
          <button
            type="button"
            className="gba-button primary"
            onClick={handleConfirm}
            disabled={submitting}
          >
            {submitting ? "HATCHING..." : "CONFIRM ▶"}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Template preview image with a 3-step fallback chain:
 *
 *   1. `template.assets.thumb`           — explicit thumbnail (preferred)
 *   2. `stages/stage_0/sprite.png`       — the "egg" stage sprite, used
 *                                          when the template ships per-
 *                                          stage sprites but no dedicated
 *                                          thumb
 *   3. generic monochrome placeholder    — last resort; explicitly NOT
 *                                          theme-coloured, so the picker
 *                                          stays neutral about the "egg"
 *                                          concept
 *
 * Each load failure (file missing, corrupt, etc.) bumps the attempt
 * counter and re-renders with the next source. Templates that ship a
 * full stage tree (e.g. the built-in `sun`) get a real preview even
 * if `thumb.png` isn't declared.
 */
function TemplatePreview({
  info,
  size = 64,
}: {
  info: TemplateInfo;
  size?: number;
}) {
  const dir = info.dir.replace(/\/+$/, "");
  const thumb = info.template.assets?.thumb;
  // Attempt chain — 0 = declared thumb, 1 = stage_0/sprite.png,
  // 2 = placeholder. Lazy initializer skips step 0 entirely when no
  // thumb is declared (avoids a wasted 404 on every render).
  const [attempt, setAttempt] = useState<0 | 1 | 2>(() => (thumb ? 0 : 1));

  const src: string | null = (() => {
    if (attempt === 0 && thumb) {
      return convertFileSrc(`${dir}/${thumb}`);
    }
    if (attempt === 1) {
      return convertFileSrc(`${dir}/stages/stage_0/sprite.png`);
    }
    return null;
  })();

  if (src === null) {
    return <PreviewPlaceholder size={size} />;
  }

  return (
    <img
      className="template-preview"
      src={src}
      width={size}
      height={size}
      alt=""
      onError={() => setAttempt((n) => (n < 2 ? ((n + 1) as 0 | 1 | 2) : n))}
      draggable={false}
    />
  );
}

/**
 * Generic placeholder — a simple monochrome silhouette. NOT
 * tied to any template's theme. Used until the template ships an
 * actual sprite asset.
 */
function PreviewPlaceholder({ size }: { size: number }) {
  return (
    <svg
      className="template-preview"
      width={size}
      height={size}
      viewBox="0 0 16 16"
      shapeRendering="crispEdges"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden
    >
      <rect x="0" y="0" width="16" height="16" fill="#e8e8e8" />
      <text
        x="8"
        y="11"
        fontSize="9"
        fontFamily="ui-monospace, monospace"
        textAnchor="middle"
        fill="#999"
      >
        ?
      </text>
    </svg>
  );
}

/**
 * Renders the template's `labels` array. Each label is either a
 * plain string (uniform default style) or an object with custom
 * `{color, fg}`. Author-driven — no theme inference.
 */
function Labels({ labels }: { labels: Label[] }) {
  if (!labels || labels.length === 0) return null;
  return (
    <span className="egg-labels">
      {labels.map((label, i) => {
        const text = typeof label === "string" ? label : label.text;
        const customBg = typeof label === "string" ? undefined : label.color;
        const customFg = typeof label === "string" ? undefined : label.fg;
        const style: React.CSSProperties = {};
        if (customBg) {
          style.background = customBg;
          // Auto-pick contrast foreground only when bg is set but
          // fg is not. If author specifies fg, use it verbatim.
          style.color = customFg ?? (isLight(customBg) ? "#1a1a1a" : "#ffffff");
        } else if (customFg) {
          style.color = customFg;
        }
        return (
          <span
            key={`${i}-${text}`}
            className="egg-label"
            style={style}
            title={text}
          >
            {text.toUpperCase()}
          </span>
        );
      })}
    </span>
  );
}

function isLight(hex: string): boolean {
  const m = /^#([0-9a-f]{6})$/i.exec(hex);
  if (!m) return true;
  const v = parseInt(m[1], 16);
  const r = (v >> 16) & 0xff;
  const g = (v >> 8) & 0xff;
  const b = v & 0xff;
  // Perceived luminance (Rec. 709)
  const lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return lum > 140;
}
