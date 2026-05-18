import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { Pet, type Mood } from "./Pet";
import { StatsPanel, type StatsRow } from "./StatsPanel";
import { EggPicker } from "./EggPicker";
import { PetSwitcher } from "./PetSwitcher";
import { Dashboard } from "./Dashboard";
import { CeremonyPlayer } from "./CeremonyPlayer";
import { ConfirmView } from "./views/ConfirmView";
import { InfoView } from "./views/InfoView";
import { MenuView } from "./views/MenuView";
import { NotifyView } from "./views/NotifyView";
import { NamingView } from "./views/NamingView";
import { awaitConfirm } from "./confirm";
import type { CeremonyAction, PetStateUpdate, UsageEvent } from "./types";
import "./App.css";

interface PetIdentity {
  id: string;
  name: string;
  template_id: string;
  snapshot_path: string;
  /// ISO timestamp when the user finalized their pet's name via the
  /// hatching modal. `null` = still using the template default name AND
  /// the naming prompt is pending. Used on launch to re-prompt naming
  /// if the user closed the app mid-ceremony.
  name_finalized_at?: string | null;
}

interface PetStageRow {
  species_id: string;
  level: number;
  name: string;
  xp_required: number;
  sprite_key: string;
  flavor?: string | null;
  metadata?: Record<string, unknown>;
}

interface XPStateView {
  total_xp: number;
  current_level: number;
  xp_in_level: number;
  xp_for_next_level: number | null;
  stage_level: number;
}

interface NextEvolutionView {
  level: number;
  name: string;
  sprite_key: string;
  xp_to_next: number;
}

interface PetSnapshotResponse {
  pet: PetIdentity | null;
  state: XPStateView | null;
  stage: PetStageRow | null;
  next_evolution: NextEvolutionView | null;
}

/// Mirrors the backend `ImportReport` for type-safety in the
/// handleImport flow. `status` matches the Rust `ImportStatus` enum
/// (serialized as `snake_case`).
interface ImportReport {
  kind: string;
  status:
    | "installed"
    | "already_present"
    | "merged"
    | "needs_version_confirm"
    | "downgrade_blocked"
    | "pet_id_exists"
    | "unknown";
  template_id?: string;
  template_name?: string;
  installed_version?: string;
  incoming_version?: string;
  pet_id?: string;
  pet_name?: string;
  existing_pet?: {
    id: string;
    name: string;
    current_level: number;
    total_xp: number;
    event_count: number;
  };
  xp_events_imported?: number;
  xp_events_skipped?: number;
  warnings?: string[];
  already_present?: boolean;
}

interface TemplateInfo {
  template: {
    meta: { id: string };
    species: { name: string };
    theme?: {
      primary?: string;
      secondary?: string;
      accent?: string;
      palette?: string[];
    };
  };
  source: string;
  dir: string;
}

/** Resize the Tauri window between "compact pet" and "egg-picker"
 *  modes. The compact form (140×140) keeps whatever position the user
 *  has dragged the pet to; picker / switcher forms explicitly center
 *  on the current monitor so the dialog isn't anchored to the corner
 *  of the screen where the pet happened to be sitting. Delegates to
 *  the Rust `main_window_resize` command — JS-side `win.center()` is
 *  silently a no-op for transparent always-on-top borderless windows
 *  on macOS, but the Rust path computes the centered position from
 *  the monitor's bounds directly. */
async function resizeWindow(width: number, height: number, centerOnScreen = false) {
  try {
    await invoke("main_window_resize", { width, height, center: centerOnScreen });
  } catch (e) {
    console.warn("window resize failed", e);
  }
}

const COMPACT_SIZE: [number, number] = [140, 140];
const PICKER_SIZE: [number, number] = [720, 880];

interface ActivityEvent {
  id: string;
  provider: string;
  session_id: string | null;
  project_path: string | null;
  timestamp: string;
  kind: { type: string; [k: string]: unknown };
}

const IDLE_REVERT_MS = 30_000;
const SATISFIED_HOLD_MS = 1500;
const EATING_HOLD_MS = 900;

/** Derive a 0-based stage index from a sprite_key like "stage_5". */
function parseStageIndex(spriteKey: string | null | undefined): number {
  if (!spriteKey) return 0;
  const m = /^stage_(\d+)$/.exec(spriteKey);
  return m ? parseInt(m[1], 10) : 0;
}

/**
 * State-driven naming popup synchroniser.
 *
 * Treats the naming window as a function of pet state:
 *   needsNaming = active pet exists
 *               ∧ pet.name_finalized_at is null
 *               ∧ pet has hatched (current_level >= 1)
 *
 * The hook reconciles the Tauri window to match: if `needsNaming` and
 * we aren't already showing for this pet, invoke `naming_window_show`.
 * If `needsNaming` is false (pet swapped to a named one, user just
 * confirmed, pet not yet hatched), invoke `naming_window_hide`.
 *
 * Why declarative rather than imperative show/hide scattered around:
 * the previous design fired `naming_window_show` from three places
 * (initial mount, active_changed, ceremony bridge). When the user
 * reset XP and re-hatched, all three fired in sequence for the same
 * logical event and the popup got into a half-shown state where input
 * wasn't focused. One effect, one window, one truth.
 *
 * Listens to `naming://done` to bump a sync counter — when the user
 * finalizes, the backend emits this; we re-evaluate (the snapshot
 * will already reflect the new `name_finalized_at` once the active-
 * changed event arrives from `naming_dismiss`).
 */
function useNamingPopupSync(
  pet: PetIdentity | null | undefined,
  petState: { currentLevel: number } | null,
): void {
  // Track what we last asked the window to show. `null` means hidden.
  // Compared against the desired state on each render — only fire the
  // Tauri command when the two diverge.
  const lastShownForRef = useRef<string | null>(null);

  useEffect(() => {
    const needsNaming =
      !!pet &&
      !pet.name_finalized_at &&
      (petState?.currentLevel ?? 0) >= 1;

    const desiredPetId = needsNaming ? pet!.id : null;
    if (desiredPetId === lastShownForRef.current) return;

    if (desiredPetId) {
      lastShownForRef.current = desiredPetId;
      invoke("naming_window_show", {
        petId: desiredPetId,
        placeholder: pet!.name,
        title: "Name your companion",
        body: `Pick a name to make ${pet!.name} truly yours, or skip to keep the default.`,
        confirmLabel: "Confirm",
        cancelLabel: "Skip",
      }).catch((err) => {
        // Failure to show isn't fatal — clear the latch so the next
        // state change can retry.
        console.warn("naming_window_show failed", err);
        lastShownForRef.current = null;
      });
    } else {
      lastShownForRef.current = null;
      invoke("naming_window_hide").catch((err) =>
        console.warn("naming_window_hide failed", err),
      );
    }
  }, [pet, petState?.currentLevel]);
}

/** Top-level router. Each Tauri window loads the same React bundle —
 *  the URL query param `?view=info` / `?view=menu` selects which view
 *  to render. Main window has no view param. */
export default function App() {
  const view = new URLSearchParams(window.location.search).get("view");
  if (view === "info") return <InfoView />;
  if (view === "menu") return <MenuView />;
  if (view === "notify") return <NotifyView />;
  if (view === "confirm") return <ConfirmView />;
  if (view === "naming") return <NamingView />;
  return <MainView />;
}

function MainView() {
  const [stats, setStats] = useState<StatsRow[]>([]);
  const [mood, setMood] = useState<Mood>("idle");
  const [showPanel, setShowPanel] = useState(false);
  const [lastEvent, setLastEvent] = useState<UsageEvent | null>(null);
  const [lastActivity, setLastActivity] = useState<ActivityEvent | null>(null);

  // Pet identity: null = unknown / loading, undefined object = no active pet.
  const [activePet, setActivePet] = useState<PetIdentity | null | undefined>(undefined);
  // Live pet state — null until first snapshot or pet://state event arrives.
  const [petState, setPetState] = useState<{
    stageIndex: number;
    stageName: string | null;
    stageFlavor: string | null;
    currentLevel: number;
    totalXp: number;
    xpInLevel: number;
    xpForNextLevel: number | null;
    nextEvoLevel: number | null;
    nextEvoName: string | null;
    xpToNextEvo: number | null;
  } | null>(null);

  // Cache: template_id → template (for palette lookups). Fetched once at mount.
  const [templatesById, setTemplatesById] = useState<Record<string, TemplateInfo>>({});

  // Pending ceremony (hatch or evolution). Player fires onComplete to clear.
  const [ceremony, setCeremony] = useState<CeremonyAction[] | null>(null);
  const [shakeActive, setShakeActive] = useState(false);

  const [showPicker, setShowPicker] = useState(false);
  const [showSwitcher, setShowSwitcher] = useState(false);
  const [showDashboard, setShowDashboard] = useState(false);

  // `awaitConfirm` lives in src/confirm.ts so EggPicker /
  // TemplateCreator / Dashboard can all share the same GBA dialog
  // helper. Imported below at module top.

  const moodTimer = useRef<number | null>(null);
  const idleTimer = useRef<number | null>(null);

  // State-driven naming popup. Opens whenever the active pet is
  // hatched (level >= 1) but `name_finalized_at` is still null;
  // closes once finalized OR the user swaps to a different pet that
  // doesn't need naming. See the hook for the full reconciliation
  // policy.
  useNamingPopupSync(activePet ?? null, petState);

  // Convert backend snapshot row → flat petState shape.
  const applyStageRow = useCallback(
    (
      stage: PetStageRow | null | undefined,
      state: XPStateView | null | undefined,
      next: NextEvolutionView | null | undefined,
    ) => {
      setPetState({
        stageIndex: parseStageIndex(stage?.sprite_key),
        stageName: stage?.name ?? null,
        stageFlavor: stage?.flavor ?? null,
        currentLevel: state?.current_level ?? 0,
        totalXp: state?.total_xp ?? 0,
        xpInLevel: state?.xp_in_level ?? 0,
        xpForNextLevel: state?.xp_for_next_level ?? null,
        nextEvoLevel: next?.level ?? null,
        nextEvoName: next?.name ?? null,
        xpToNextEvo: next?.xp_to_next ?? null,
      });
    },
    [],
  );

  // Apply a pet://state event update.
  const applyUpdate = useCallback((u: PetStateUpdate) => {
    setPetState({
      stageIndex: parseStageIndex(u.sprite_key),
      stageName: u.stage_name,
      stageFlavor: u.stage_flavor,
      currentLevel: u.current_level,
      totalXp: u.total_xp,
      xpInLevel: u.xp_in_level,
      xpForNextLevel: u.xp_for_next_level,
      nextEvoLevel: u.next_evolution_level,
      nextEvoName: u.next_evolution_name,
      xpToNextEvo: u.xp_to_next_evolution,
    });
  }, []);

  // Decide initial UI: load active pet, then open picker if none.
  useEffect(() => {
    let cancelled = false;
    invoke<PetSnapshotResponse>("pet_snapshot")
      .then((snap) => {
        if (cancelled) return;
        const pet = snap.pet ?? null;
        setActivePet(pet);
        if (pet) {
          applyStageRow(snap.stage, snap.state, snap.next_evolution);
          resizeWindow(COMPACT_SIZE[0], COMPACT_SIZE[1]);
        } else {
          setShowPicker(true);
          resizeWindow(PICKER_SIZE[0], PICKER_SIZE[1], true);
        }
      })
      .catch((e) => {
        console.error("pet_snapshot failed", e);
        if (!cancelled) {
          setActivePet(null);
          setShowPicker(true);
          resizeWindow(PICKER_SIZE[0], PICKER_SIZE[1], true);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [applyStageRow]);

  // Templates → id map. Mount-time fetch plus an explicit refresh
  // helper used after picker confirmation: if the user created their
  // pet from a template that wasn't in the original mount-time list
  // (newly-imported community template, fresh custom template via
  // TemplateCreator, or just a race where `template_list` resolved
  // after the picker did), the floating pet's `spriteUrl` would
  // resolve to `null` and fall back to the procedural SVG egg —
  // re-fetching after every pick guarantees the new pet's template
  // is in the map by the time we render its sprite.
  const refreshTemplates = useCallback(async () => {
    try {
      const list = await invoke<TemplateInfo[]>("template_list");
      const map: Record<string, TemplateInfo> = {};
      for (const t of list) map[t.template.meta.id] = t;
      setTemplatesById(map);
    } catch (e) {
      console.warn("template_list failed", e);
    }
  }, []);

  useEffect(() => {
    refreshTemplates();
  }, [refreshTemplates]);

  // Listen for live pet state updates from backend.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<PetStateUpdate>("pet://state", (ev) => {
      applyUpdate(ev.payload);
    }).then((fn) => (unlisten = fn));
    return () => {
      unlisten?.();
    };
  }, [applyUpdate]);

  // Active companion changed (Rust side `pet_set_active` emitted this).
  // Re-fetch the full snapshot so identity, sprite path, level, and
  // stage all flip to the newly-active pet — this is the canonical
  // refresh path; `pet://state` is per-XP-delta and doesn't fire here.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let cancelled = false;
    listen("pet://active_changed", async () => {
      try {
        // Cancel any in-flight hatch ceremony — it was scoped to the
        // previous pet and the visuals shouldn't bleed into the new
        // one. Naming popup teardown is automatic: `useNamingPopupSync`
        // sees activePet flip and reconciles the window to match.
        setCeremony(null);

        const snap = await invoke<PetSnapshotResponse>("pet_snapshot");
        if (cancelled) return;
        const pet = snap.pet ?? null;
        setActivePet(pet);
        applyStageRow(snap.stage, snap.state, snap.next_evolution);
      } catch (e) {
        console.warn("active_changed snapshot failed", e);
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [applyStageRow]);

  // Listen for level-up events. If the pet *evolved* (stage changed),
  // play that stage's on_enter ceremony.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<PetStateUpdate>("pet://level_up", (ev) => {
      const u = ev.payload;
      if (!u.evolved) return;
      const events = (u.stage_metadata?.events ?? {}) as Record<string, CeremonyAction[]>;
      const onEnter = events.on_enter;
      if (onEnter && onEnter.length > 0) {
        setCeremony(onEnter);
      }
    }).then((fn) => (unlisten = fn));
    return () => {
      unlisten?.();
    };
  }, []);

  const handlePickerConfirmed = useCallback(async () => {
    try {
      // Refresh templates FIRST so the new pet's `template_id` is
      // guaranteed to resolve to a `tpl.dir` when the sprite-url
      // useMemo runs on the next render. Without this, freshly-created
      // pets from a template the mount-time fetch missed (newly added
      // builtin, custom-created template, race condition) show the
      // procedural fallback egg instead of their actual sprite.
      await refreshTemplates();
      const snap = await invoke<PetSnapshotResponse>("pet_snapshot");
      setActivePet(snap.pet ?? null);
      applyStageRow(snap.stage, snap.state, snap.next_evolution);
    } catch (e) {
      console.error("post-pick snapshot failed", e);
    }
    setShowPicker(false);
    resizeWindow(COMPACT_SIZE[0], COMPACT_SIZE[1]);
  }, [applyStageRow, refreshTemplates]);

  const handlePickerCancelled = useCallback(() => {
    // The Back button in the egg picker is only shown when an active
    // pet exists (first-launch is modal). In that case the user got
    // here from the Pets box → "+ Hatch new" — return them there
    // instead of dropping straight to the floating compact pet. The
    // Pets box uses the same window size so there's no resize flicker.
    setShowPicker(false);
    setShowSwitcher(true);
  }, []);

  const handleOpenPicker = useCallback(() => {
    setShowPicker(true);
    resizeWindow(PICKER_SIZE[0], PICKER_SIZE[1], true);
  }, []);

  // Menu window asks main to open the egg picker (via global event).
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<void>("menu:request-picker", () => {
      handleOpenPicker();
    }).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, [handleOpenPicker]);

  // Switcher path — list existing pets and let the user pick one.
  const handleOpenSwitcher = useCallback(() => {
    setShowSwitcher(true);
    resizeWindow(PICKER_SIZE[0], PICKER_SIZE[1], true);
  }, []);
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<void>("menu:request-switcher", () => {
      handleOpenSwitcher();
    }).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, [handleOpenSwitcher]);

  const handleSwitcherPicked = useCallback(async () => {
    setShowSwitcher(false);
    resizeWindow(COMPACT_SIZE[0], COMPACT_SIZE[1]);
    // Refresh main from disk so sprite / stage / xp reflect the
    // newly-active pet. The `pet://active_changed` event from Rust
    // also fires but doing it explicitly here keeps the transition
    // synchronous from the user's POV.
    try {
      const snap = await invoke<PetSnapshotResponse>("pet_snapshot");
      setActivePet(snap.pet ?? null);
      applyStageRow(snap.stage, snap.state, snap.next_evolution);
    } catch (e) {
      console.error("post-switch snapshot failed", e);
    }
  }, [applyStageRow]);
  const handleSwitcherCancelled = useCallback(() => {
    setShowSwitcher(false);
    resizeWindow(COMPACT_SIZE[0], COMPACT_SIZE[1]);
  }, []);

  // Unified import handler — wired to:
  //   (1) the right-click menu's "Import…" item (menu:request-import)
  //   (2) the PetSwitcher's "Import…" button
  //   (3) drag-drop of a `.petpet` file onto the window
  // When `sourceFilePath` is undefined, opens an OS file picker.
  // Routing (template vs pet) happens server-side based on the
  // archive's manifest — same code path either way, so a friend
  // sending us a "pet archive" via Discord works identically to a
  // template archive.
  const handleImport = useCallback(async (sourceFilePath?: string) => {
    // No main-window resize. The confirm dialog lives in its own
    // Tauri sub-window (`?view=confirm`) so the floating pet stays
    // exactly where it was — no jumping, no visual flash, and the
    // confirm renders independently of whatever mode main is in.
    try {
      let path = sourceFilePath;
      if (!path) {
        const picked = await openDialog({
          multiple: false,
          filters: [{ name: "petpet archive", extensions: ["petpet"] }],
        });
        if (!picked || typeof picked !== "string") return;
        path = picked;
      }
      // First call: no force. Backend may return a "needs confirm"
      // status if the template version differs from what's installed.
      // The same call covers template- and pet-kind archives — the
      // manifest tells Rust which lane to take.
      let report = await invoke<ImportReport>("archive_import", {
        zipPath: path,
      });

      // Carry the user's prior decisions across re-invokes if multiple
      // conflict gates fire in sequence.
      let force = false;
      let petAction: "merge" | "copy" | undefined;

      // Loop until the backend either succeeds, signals a no-op, or
      // surfaces a fresh gate we haven't answered yet.
      while (
        report.status === "needs_version_confirm" ||
        report.status === "downgrade_blocked" ||
        report.status === "pet_id_exists"
      ) {
        if (
          report.status === "needs_version_confirm" ||
          report.status === "downgrade_blocked"
        ) {
          const tplName =
            report.template_name ?? report.template_id ?? "this template";
          const installed = report.installed_version ?? "(unknown)";
          const incoming = report.incoming_version ?? "(unknown)";
          const message =
            report.status === "downgrade_blocked"
              ? `"${tplName}" version ${installed} is already installed.\nThe file you're importing is ${incoming} (older).\nReplace anyway?`
              : `"${tplName}" version ${installed} is already installed.\nReplace with ${incoming}?`;
          const choice = await awaitConfirm({
            title: "REPLACE TEMPLATE?",
            message,
            options: [
              { label: "Cancel", value: "cancel" },
              { label: "Replace", value: "replace", primary: true },
            ],
          });
          if (choice !== "replace") {
            // Silent cancel — no notify toast, since the toast can
            // overlap the floating pet when the import was triggered
            // from compact mode. The dismissed confirm window is the
            // visual feedback that the action ended.
            return;
          }
          force = true;
        } else {
          // pet_id_exists — the local DB already has a pet with this
          // archive's id. Single three-way GBA dialog: merge, copy,
          // or cancel.
          const ex = report.existing_pet;
          const existingDesc = ex
            ? `"${ex.name}" (Lv.${ex.current_level}, ${ex.event_count} events)`
            : "this pet";
          const choice = await awaitConfirm({
            title: "SAME COMPANION FOUND",
            message:
              `You already have ${existingDesc} with the same id.\n\n` +
              `MERGE — add new events into the existing pet (sync).\n` +
              `COPY — create a separate pet with a fresh id (clone).`,
            options: [
              { label: "Cancel", value: "cancel" },
              { label: "Copy", value: "copy" },
              { label: "Merge", value: "merge", primary: true },
            ],
          });
          if (choice === "merge") {
            petAction = "merge";
          } else if (choice === "copy") {
            petAction = "copy";
          } else {
            return;
          }
        }
        report = await invoke<ImportReport>("archive_import", {
          zipPath: path,
          force,
          petAction,
        });
      }

      const msg =
        report.kind === "pet"
          ? report.status === "merged"
            ? `Merged "${report.pet_name ?? "pet"}" — ${
                report.xp_events_imported ?? 0
              } new events added${
                report.xp_events_skipped
                  ? ` (${report.xp_events_skipped} duplicates skipped)`
                  : ""
              }.`
            : `Restored "${report.pet_name ?? "pet"}" — ${
                report.xp_events_imported ?? 0
              } XP events replayed${
                report.xp_events_skipped
                  ? ` (${report.xp_events_skipped} skipped)`
                  : ""
              }.`
          : report.status === "already_present"
            ? `Template "${report.template_id}" already installed (no changes).`
            : `Imported template "${report.template_id}".`;
      await invoke("notify_show", { text: msg, durationMs: 5500 }).catch(() => {});
    } catch (e) {
      await invoke("notify_show", {
        text: `Import failed: ${e}`,
        durationMs: 7000,
      }).catch(() => {});
    }
  }, []);

  const handleExportPet = useCallback(async (petId: string) => {
    try {
      const out = await saveDialog({
        defaultPath: `pet-${petId.slice(0, 8)}.petpet`,
        filters: [{ name: "petpet archive", extensions: ["petpet"] }],
      });
      if (!out) return;
      const report = await invoke<{ path: string; bytes: number }>("pet_export", {
        petId,
        outPath: out,
      });
      await invoke("notify_show", {
        text: `Exported pet — ${Math.round(report.bytes / 1024)} KB`,
        durationMs: 5000,
      }).catch(() => {});
    } catch (e) {
      await invoke("notify_show", {
        text: `Export failed: ${e}`,
        durationMs: 7000,
      }).catch(() => {});
    }
  }, []);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<void>("menu:request-import", () => {
      handleImport();
    }).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, [handleImport]);

  // Native drag-drop of files onto the main pet window. Tauri 2 emits
  // `tauri://drag-drop` with the list of paths the user dropped.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<{ paths?: string[]; payload?: { paths?: string[] } }>(
      "tauri://drag-drop",
      (ev) => {
        // Schema in Tauri 2 nests paths under `payload` for some
        // versions and at the top level in others — accept both.
        const paths =
          (ev.payload && (ev.payload as { paths?: string[] }).paths) ||
          (ev as unknown as { paths?: string[] }).paths ||
          [];
        const petpet = paths.find((p) => p.toLowerCase().endsWith(".petpet"));
        if (petpet) handleImport(petpet);
      },
    ).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, [handleImport]);

  // Dashboard ("trainer card") — takeover mode like picker/switcher.
  const handleOpenDashboard = useCallback(() => {
    setShowDashboard(true);
    resizeWindow(PICKER_SIZE[0], PICKER_SIZE[1], true);
  }, []);
  const handleCloseDashboard = useCallback(() => {
    setShowDashboard(false);
    resizeWindow(COMPACT_SIZE[0], COMPACT_SIZE[1]);
  }, []);
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<void>("menu:request-dashboard", () => {
      handleOpenDashboard();
    }).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, [handleOpenDashboard]);

  // Suppress hover triggers while a modal mode (picker / switcher /
  // ceremony / stats panel) takes over the main window. A ref instead
  // of state so the latest value is visible inside the long-lived
  // cursor:// listener closures below (no need to re-register).
  const suppressHoverRef = useRef(false);

  // Pet hover info popup is driven by `cursor://pet_enter` /
  // `cursor://pet_leave` events emitted from a Rust-side cursor poller.
  // macOS does not deliver mouseMoved to non-key always-on-top
  // transparent windows even with acceptsMouseMovedEvents:YES — polling
  // is the only reliable signal here.
  //
  // The `cancelled` flag closes a race where React StrictMode mounts
  // → unmounts → re-mounts before `listen()` resolves: without it, the
  // promise resolution from the first mount would silently register a
  // second listener, double-invoking `info_window_show` and creating
  // a duplicate info sub-window.
  useEffect(() => {
    let unEnter: UnlistenFn | undefined;
    let unLeave: UnlistenFn | undefined;
    let cancelled = false;
    listen("cursor://pet_enter", () => {
      if (suppressHoverRef.current) return;
      invoke("info_window_show").catch((e) =>
        console.warn("info_window_show", e),
      );
    }).then((fn) => {
      if (cancelled) fn();
      else unEnter = fn;
    });
    listen("cursor://pet_leave", () => {
      invoke("info_window_hide").catch(() => {});
    }).then((fn) => {
      if (cancelled) fn();
      else unLeave = fn;
    });
    return () => {
      cancelled = true;
      unEnter?.();
      unLeave?.();
    };
  }, []);

  // Right-click anywhere in the (small, pet-shaped) main window →
  // floating context-menu sub-window at the cursor. Bound at the
  // window level via a useEffect rather than as a React JSX
  // `onContextMenu` prop, because the React synthetic handler on
  // `.pet-wrapper` never fired on macOS — WKWebView's default
  // context menu was running instead. Capturing at the document gives
  // us first dibs on the event so we can preventDefault() reliably.
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault();
      e.stopPropagation();
      invoke("menu_window_show", { x: e.clientX, y: e.clientY }).catch((err) =>
        console.warn("menu_window_show", err),
      );
    };
    window.addEventListener("contextmenu", onContextMenu, { capture: true });
    return () =>
      window.removeEventListener("contextmenu", onContextMenu, { capture: true });
  }, []);

  // Left-mousedown on pet → start native window drag. Done explicitly
  // rather than via `data-tauri-drag-region` because the shim is flaky
  // on macOS transparent always-on-top windows where the first click
  // would otherwise be consumed for window activation.
  const handlePetMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button !== 0) return;
    getCurrentWebviewWindow()
      .startDragging()
      .catch((err) => console.warn("startDragging", err));
  }, []);

  const setTempMood = (next: Mood, holdMs?: number) => {
    if (moodTimer.current) window.clearTimeout(moodTimer.current);
    if (idleTimer.current) window.clearTimeout(idleTimer.current);
    setMood(next);
    if (holdMs !== undefined) {
      moodTimer.current = window.setTimeout(() => setMood("idle"), holdMs);
    } else {
      idleTimer.current = window.setTimeout(() => setMood("idle"), IDLE_REVERT_MS);
    }
  };

  // Initial stats + 5s polling.
  useEffect(() => {
    let cancelled = false;
    const refresh = () =>
      invoke<StatsRow[]>("stats_summary").then((rows) => {
        if (!cancelled) setStats(rows);
      });
    refresh();
    const t = window.setInterval(refresh, 5000);
    return () => {
      cancelled = true;
      window.clearInterval(t);
    };
  }, []);

  // Layer 2: token-bearing usage events. Brief eating animation.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<UsageEvent>("usage://event", (ev) => {
      setLastEvent(ev.payload);
      setTempMood("eating", EATING_HOLD_MS);
    }).then((fn) => (unlisten = fn));
    return () => {
      unlisten?.();
    };
  }, []);

  // Layer 1: live interaction hooks. Drives mood transitions.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<ActivityEvent>("activity://event", (ev) => {
      setLastActivity(ev.payload);
      const kind = ev.payload.kind.type;
      switch (kind) {
        case "user_prompt_submit":
          setTempMood("thinking");
          break;
        case "tool_use_start":
          setTempMood("working");
          break;
        case "tool_use_end":
          setTempMood("working");
          break;
        case "assistant_stop":
          setTempMood("satisfied", SATISFIED_HOLD_MS);
          break;
        case "session_start":
          setTempMood("thinking", 600);
          break;
        case "session_end":
          setMood("idle");
          break;
      }
    }).then((fn) => (unlisten = fn));
    return () => {
      unlisten?.();
      if (moodTimer.current) window.clearTimeout(moodTimer.current);
      if (idleTimer.current) window.clearTimeout(idleTimer.current);
    };
  }, []);

  // Resolve palette from the active pet's template (cached).
  const palette = useMemo(() => {
    const tpl = activePet ? templatesById[activePet.template_id] : null;
    const theme = tpl?.template.theme ?? {};
    const fallback = ["#6ee7b7", "#10b981", "#047857", "#fde047"];
    const pal = (theme.palette && theme.palette.length >= 2) ? theme.palette : fallback;
    return {
      primary: theme.primary ?? pal[1] ?? fallback[0],
      shadow: pal[2] ?? theme.secondary ?? fallback[1],
      dark: pal[0] ?? fallback[2],
      accent: theme.accent ?? pal[pal.length - 1] ?? fallback[3],
    };
  }, [activePet, templatesById]);

  // Resolve `<template_dir>/stages/stage_<N>/sprite.png` via Tauri's asset
  // protocol. Pet.tsx falls back to the placeholder SVG on load error.
  const spriteUrl = useMemo(() => {
    if (!activePet || !petState) return null;
    const tpl = templatesById[activePet.template_id];
    if (!tpl) return null;
    const stageId = `stage_${petState.stageIndex}`;
    const path = `${tpl.dir.replace(/\/+$/, "")}/stages/${stageId}/sprite.png`;
    return convertFileSrc(path);
  }, [activePet, templatesById, petState]);

  // Hide the info / menu / notify sub-windows whenever a ceremony /
  // picker / switcher / dashboard / stats panel takes over the main
  // window — they'd otherwise float distractingly over those modes.
  // Also flip the hover-suppression ref so the cursor poller can't
  // immediately re-show the info bubble on the next tick.
  useEffect(() => {
    const modalActive =
      showPicker || showSwitcher || showDashboard || showPanel || ceremony !== null;
    suppressHoverRef.current = modalActive;
    if (modalActive) {
      invoke("info_window_hide").catch(() => {});
      invoke("menu_window_hide").catch(() => {});
      invoke("notify_hide").catch(() => {});
    }
  }, [showPicker, showSwitcher, showDashboard, showPanel, ceremony]);

  if (activePet === undefined) {
    return null;
  }

  if (showPicker) {
    return (
      <EggPicker
        onConfirm={handlePickerConfirmed}
        onCancel={activePet ? handlePickerCancelled : undefined}
        // Same handler everywhere — the egg picker's Import…, the
        // Pets-box Import…, the right-click menu Import…, and the
        // drag-drop listener all route through this. Format detection
        // (template vs pet) is server-side.
        onImport={() => handleImport()}
      />
    );
  }

  if (showSwitcher) {
    return (
      <PetSwitcher
        onPicked={handleSwitcherPicked}
        onCancel={handleSwitcherCancelled}
        // "+ Hatch new" tile flips straight into the template picker
        // without bouncing the user back to the menu first. Window
        // stays at picker size so no resize flicker.
        onAddNew={() => {
          setShowSwitcher(false);
          setShowPicker(true);
        }}
        onImport={() => handleImport()}
        onExport={(petId) => handleExportPet(petId)}
      />
    );
  }

  if (showDashboard) {
    return <Dashboard onClose={handleCloseDashboard} />;
  }

  const stageIdx = petState?.stageIndex ?? 0;

  return (
    <div className="app">
      <div className="pet-area">
        <div
          className={`pet-wrapper ${shakeActive ? "pet-shaking" : ""}`}
          onMouseDown={handlePetMouseDown}
        >
          <Pet
            stageIndex={stageIdx}
            mood={mood}
            spriteUrl={spriteUrl}
            primary={palette.primary}
            shadow={palette.shadow}
            dark={palette.dark}
            accent={palette.accent}
          />
        </div>
      </div>

      {showPanel && (
        <StatsPanel
          rows={stats}
          lastEvent={lastEvent}
          lastActivity={lastActivity}
          onClose={() => setShowPanel(false)}
        />
      )}

      {ceremony && (
        <CeremonyPlayer
          ceremony={ceremony}
          context={{
            petId: activePet?.id,
            petName: activePet?.name,
            stageName: petState?.stageName ?? null,
            stageFlavor: petState?.stageFlavor ?? null,
          }}
          onShakeChange={setShakeActive}
          onComplete={() => setCeremony(null)}
        />
      )}

    </div>
  );
}
