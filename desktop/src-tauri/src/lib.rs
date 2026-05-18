//! petpet desktop entry point.
//!
//! On `run()`:
//! 1. Open SQLite (creates `~/.petpet/petpet.db` if missing).
//! 2. Spawn the writer task draining the unified event channel into SQLite.
//! 3. Spawn each `Provider` in watch mode. Backfill happens internally.
//! 4. Emit a `usage://event` Tauri event for every new `UsageEvent` so the
//!    frontend pet can react in real time.
//! 5. Hand control to the Tauri webview.

use std::sync::{Arc, Mutex};

use petpet::{
    db::{writer::spawn_writer, DbHandle, StatsRow},
    hooks::{ActivitySink, HookServer},
    paths,
    provider::{
        aider::AiderProvider, claude::ClaudeCodeProvider, codex::CodexProvider,
        opencode::OpenCodeProvider, EventSink,
        Provider,
    },
    template::{registry::TemplateRegistry, types::Template},
    xp::{ManualGrant, Pet, PetStateUpdate, PetSummary, XPEngine, XPEngineSnapshot},
    ActivityEvent, UsageEvent,
};

mod archive_cmds;
mod dashboard;
use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, WebviewUrl,
    WebviewWindowBuilder,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

pub struct AppState {
    db: Arc<DbHandle>,
    hook_port: u16,
    xp: Arc<XPEngine>,
    /// Latest notification payload. Stored so a newly-opened
    /// `NotifyView` can pull the message via `notify_current` on mount
    /// (avoiding a race where Rust emits the `notify://message` event
    /// before the React view has registered its listener).
    last_notify: Mutex<Option<NotifyPayload>>,
    /// Latest confirm prompt. Same race-avoidance pattern as
    /// `last_notify` — the `ConfirmView` reads it via `confirm_current`
    /// at mount. Replaced on every new `confirm_show` call.
    last_confirm: Mutex<Option<ConfirmPrompt>>,
    /// Latest naming prompt. Same race-avoidance pattern as
    /// `last_notify` — the `NamingView` reads it via `naming_current`
    /// at mount. The naming popup floats ABOVE the pet (not centered
    /// like the confirm window) because it's the hatching ceremony's
    /// closer — visually anchored to the pet that just hatched.
    last_naming: Mutex<Option<NamingPrompt>>,
}

#[derive(Clone, Serialize)]
struct NotifyPayload {
    text: String,
}

#[derive(Clone, Serialize, serde::Deserialize)]
pub struct ConfirmPrompt {
    pub title: String,
    pub message: String,
    pub options: Vec<ConfirmOption>,
}

/// Payload for the floating naming popup that closes the hatching
/// ceremony. The pet_id is captured at show time so the popup can
/// finalize even if the active pet changes mid-modal (rare but
/// possible if the user opens the pet switcher).
#[derive(Clone, Serialize, serde::Deserialize)]
pub struct NamingPrompt {
    pub pet_id: String,
    pub placeholder: String,
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub cancel_label: String,
}

#[derive(Clone, Serialize, serde::Deserialize)]
pub struct ConfirmOption {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Serialize)]
struct StatsRowDto {
    provider: String,
    model: String,
    events: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    reasoning: u64,
}

impl From<StatsRow> for StatsRowDto {
    fn from(r: StatsRow) -> Self {
        Self {
            provider: r.provider,
            model: r.model,
            events: r.events,
            input: r.input,
            output: r.output,
            cache_read: r.cache_read,
            cache_creation: r.cache_creation,
            reasoning: r.reasoning,
        }
    }
}

#[tauri::command]
async fn stats_summary(state: State<'_, AppState>) -> Result<Vec<StatsRowDto>, String> {
    state
        .db
        .stats_summary()
        .await
        .map(|rows| rows.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn hook_port(state: State<'_, AppState>) -> u16 {
    state.hook_port
}

#[tauri::command]
async fn pet_snapshot(state: State<'_, AppState>) -> Result<XPEngineSnapshot, String> {
    state.xp.snapshot().await.map_err(|e| e.to_string())
}

/// "Feeding bill" — total USD + token + per-model breakdown for one
/// of the canonical ranges. UI hits this for the cost surface on the
/// dashboard. Time windows are anchored on the user's LOCAL timezone
/// (see `xp::cost_query` for rationale).
///
/// `range` accepts: `"today"`, `"week"`, `"month"`, `"lifetime"`.
/// Anything else falls back to `"today"` rather than erroring — the
/// pet UI is meant to be forgiving.
#[tauri::command]
async fn pet_feeding_bill(
    state: State<'_, AppState>,
    range: String,
) -> Result<petpet::xp::cost_query::CostBreakdown, String> {
    let db = &state.db;
    let result = match range.as_str() {
        "week" => petpet::xp::cost_query::cost_this_week_local(db).await,
        "month" => petpet::xp::cost_query::cost_this_month_local(db).await,
        "lifetime" => petpet::xp::cost_query::cost_lifetime(db).await,
        // "today" is the safe default for any unrecognised value.
        _ => petpet::xp::cost_query::cost_today_local(db).await,
    };
    result.map_err(|e| e.to_string())
}

/// Per-day cost series for the chart. `days_back` controls how many
/// local days to include, ending at today (inclusive). The frontend
/// typically asks for 30 days for the monthly bar chart.
#[tauri::command]
async fn pet_feeding_bill_by_day(
    state: State<'_, AppState>,
    days_back: u32,
) -> Result<Vec<petpet::xp::cost_query::DailyCost>, String> {
    let today_local = chrono::Local::now().date_naive();
    let span = days_back.min(365) as i64; // safety: cap at ~1 year
    let start = today_local
        .checked_sub_signed(chrono::Duration::days(span - 1))
        .unwrap_or(today_local);
    petpet::xp::cost_query::cost_by_day_local(&state.db, start, today_local)
        .await
        .map_err(|e| e.to_string())
}

/// List every template the user can pick: built-in + community + custom.
/// Frontend egg-picker calls this on render.
#[tauri::command]
async fn template_list(_state: State<'_, AppState>) -> Result<Vec<TemplateInfo>, String> {
    let loaded = tokio::task::spawn_blocking(TemplateRegistry::discover)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(loaded
        .into_iter()
        .map(|l| TemplateInfo {
            template: l.template,
            source: l.source.as_str().to_string(),
            dir: l.dir.to_string_lossy().to_string(),
        })
        .collect())
}

#[derive(serde::Serialize)]
struct TemplateInfo {
    template: Template,
    source: String,
    dir: String,
}

/// Create a pet from a template id (e.g. "ember"). Snapshots stages +
/// rules + assets into `~/.petpet/pets/<uuid>/`. `pick_template`
/// internally sets the new pet as the active companion, so we emit
/// `pet://active_changed` to mirror the `pet_set_active` flow — the
/// hover bubble / right-click menu / main pet view all listen for
/// that event and re-fetch the snapshot.
#[tauri::command]
async fn pet_pick_template(
    state: State<'_, AppState>,
    app: AppHandle,
    template_id: String,
    name: Option<String>,
) -> Result<Pet, String> {
    let pet = state
        .xp
        .pick_template(&template_id, name)
        .await
        .map_err(|e| e.to_string())?;
    let _ = app.emit("pet://active_changed", &pet.id);
    Ok(pet)
}

/// Resize the main window to `(width, height)` logical pixels and
/// (optionally) recenter it on the monitor it's currently sitting on.
///
/// Used for the compact ↔ picker / switcher mode swap. Doing this in
/// Rust rather than from JS via `win.center()` because that JS call
/// is silently a no-op on a transparent + always-on-top + borderless
/// window under macOS — Tauri's underlying winit call short-circuits
/// for some style-mask combinations. The Rust path computes the target
/// position from the monitor's bounds directly, which works regardless
/// of window style.
#[tauri::command]
async fn main_window_resize(
    app: AppHandle,
    width: f64,
    height: f64,
    center: bool,
) -> Result<(), String> {
    let main = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    main.set_size(LogicalSize::new(width, height))
        .map_err(|e| e.to_string())?;
    if !center {
        return Ok(());
    }
    let mon = match main.current_monitor().map_err(|e| e.to_string())? {
        Some(m) => m,
        None => return Ok(()), // headless / unknown monitor — leave position
    };
    let scale = mon.scale_factor();
    let mon_pos = mon.position();
    let mon_size = mon.size();
    let mon_x = mon_pos.x as f64 / scale;
    let mon_y = mon_pos.y as f64 / scale;
    let mon_w = mon_size.width as f64 / scale;
    let mon_h = mon_size.height as f64 / scale;
    let x = mon_x + (mon_w - width) / 2.0;
    let y = mon_y + (mon_h - height) / 2.0;
    main.set_position(LogicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Dev-mode flag. True in `tauri dev` / `cargo run` (debug build) or
/// when `PETPET_DEV` is set to anything non-empty. Frontend MenuView
/// uses this to gate XP / Reset-XP affordances — production users
/// shouldn't see them, but they're indispensable while building.
#[tauri::command]
fn dev_mode() -> bool {
    if cfg!(debug_assertions) {
        return true;
    }
    matches!(std::env::var("PETPET_DEV"), Ok(v) if !v.is_empty() && v != "0")
}

/// List every previously-created pet. The "switch companion" picker
/// uses this so users can jump back to a pet they've already raised
/// instead of going through the egg-pick / hatch flow again.
#[tauri::command]
async fn pet_list_all(state: State<'_, AppState>) -> Result<Vec<Pet>, String> {
    state.xp.list_pets().await.map_err(|e| e.to_string())
}

/// Pokémon-style party listing — one entry per pet with the CURRENT
/// stage's sprite path, current level, stage name, and total XP.
/// Backs the rich `PetSwitcher` UI so each row shows the companion
/// as it actually looks today.
#[tauri::command]
async fn pet_list_summaries(state: State<'_, AppState>) -> Result<Vec<PetSummary>, String> {
    state.xp.summarize_all().await.map_err(|e| e.to_string())
}

/// Set the currently-active pet to `pet_id`. Refreshes the in-memory
/// active state and emits `pet://state` so all sub-windows re-render
/// with the new pet's data.
#[tauri::command]
async fn pet_set_active(
    state: State<'_, AppState>,
    app: AppHandle,
    pet_id: String,
) -> Result<(), String> {
    state
        .xp
        .set_active_pet(&pet_id)
        .await
        .map_err(|e| e.to_string())?;
    // Emit a "pet changed" signal — the frontend re-fetches snapshot
    // to refresh stage / xp bars / sprite in every window.
    let _ = app.emit("pet://active_changed", &pet_id);
    Ok(())
}

/// Hatch-time naming ceremony. One-shot — second call returns
/// "naming already finalized". Pass `name=None` for the Skip path.
#[tauri::command]
async fn pet_finalize_naming(
    state: State<'_, AppState>,
    pet_id: String,
    name: Option<String>,
) -> Result<Pet, String> {
    state
        .xp
        .finalize_naming(&pet_id, name)
        .await
        .map_err(|e| e.to_string())
}

/// Dev helper: manually grant XP to the active pet. Emits the same
/// `pet://state` / `pet://level_up` events as the real ingestion path
/// so the frontend renders identically. Used by the in-app +XP button
/// for testing the full evolution loop without waiting for real usage.
#[tauri::command]
async fn pet_grant_xp(
    state: State<'_, AppState>,
    app: AppHandle,
    xp_delta: i64,
    reason: Option<String>,
) -> Result<Option<PetStateUpdate>, String> {
    let grant = ManualGrant {
        xp_delta,
        reason: reason.unwrap_or_else(|| "dev grant".to_string()),
        ref_id: format!("dev-{}", chrono::Utc::now().timestamp_millis()),
    };
    let update = state
        .xp
        .grant_manual(grant)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(ref u) = update {
        emit_pet_state(&app, u);
    }
    Ok(update)
}

/// Info window dimensions, in logical pixels. Centralized so the
/// creation path and the reposition path agree on size.
const INFO_W: f64 = 200.0;
const INFO_H: f64 = 100.0;

/// Compute the screen-space logical position the info window should
/// occupy: horizontally centered under the main pet window (using the
/// actual `info_w` — NOT a constant, because the info window is
/// dynamically resized to fit content), vertically 4px below it.
fn info_window_position(main: &tauri::WebviewWindow, info_w: f64) -> Result<(f64, f64), String> {
    let main_pos = main.outer_position().map_err(|e| e.to_string())?;
    let main_size = main.outer_size().map_err(|e| e.to_string())?;
    let scale = main.scale_factor().map_err(|e| e.to_string())?;
    let main_w_logical = main_size.width as f64 / scale;
    let main_h_logical = main_size.height as f64 / scale;
    let main_x_logical = main_pos.x as f64 / scale;
    let main_y_logical = main_pos.y as f64 / scale;
    let x = main_x_logical + (main_w_logical - info_w) / 2.0;
    let y = main_y_logical + main_h_logical + 4.0;
    Ok((x, y))
}


/// Show (creating if necessary) the floating info window adjacent to
/// the main pet window. The info window is a small transparent
/// always-on-top panel that renders the pet's name / level / XP bar /
/// next-evo hint. Frontend invokes this on `mouseenter` over the pet.
#[tauri::command]
async fn info_window_show(app: AppHandle) -> Result<(), String> {
    tracing::info!("info_window_show invoked");
    show_secondary_window(
        &app,
        "info",
        "/?view=info",
        INFO_W,
        INFO_H,
        false, // passive display, don't steal focus
        true,  // always-on-top: info bubble floats above other apps
        |main, w, _h| info_window_position(main, w),
    )
    .await
}

/// Reposition an already-visible info window. Called from the main
/// window's `Moved` event so the info bubble tracks the pet while the
/// user drags it. No-op if the info window doesn't exist or isn't
/// visible — the next `info_window_show` will set the correct position.
fn reposition_pet_anchored_windows_if_visible(app: &AppHandle) {
    let main = match app.get_webview_window("main") {
        Some(w) => w,
        None => return,
    };
    if let Some(info) = app.get_webview_window("info") {
        if info.is_visible().unwrap_or(false) {
            let info_w = window_logical_size(&info).map(|s| s.0).unwrap_or(INFO_W);
            if let Ok((x, y)) = info_window_position(&main, info_w) {
                let _ = info.set_position(LogicalPosition::new(x, y));
            }
        }
    }
    if let Some(notify) = app.get_webview_window("notify") {
        if notify.is_visible().unwrap_or(false) {
            let (nw, nh) = window_logical_size(&notify).unwrap_or((NOTIFY_W, NOTIFY_H));
            if let Ok((x, y)) = notify_window_position(&main, nw, nh) {
                let _ = notify.set_position(LogicalPosition::new(x, y));
            }
        }
    }
    // Naming popup is positioned above the pet just like notify, so
    // tracks the same drag events. Without this it would stay pinned
    // to wherever the pet was when the popup opened.
    if let Some(naming) = app.get_webview_window("naming") {
        if naming.is_visible().unwrap_or(false) {
            let (nw, nh) =
                window_logical_size(&naming).unwrap_or((NAMING_DEFAULT_W, NAMING_DEFAULT_H));
            if let Ok((x, y)) = notify_window_position(&main, nw, nh) {
                let _ = naming.set_position(LogicalPosition::new(x, y));
            }
        }
    }
}

#[tauri::command]
async fn info_window_hide(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("info") {
        let _ = win.hide();
    }
    Ok(())
}

/// Resize the info window to fit its rendered content. JS-driven —
/// `InfoView` measures its panel after each render and invokes this so
/// the bubble auto-fits names + stage strings of varying length without
/// overflowing or leaving empty space.
#[tauri::command]
async fn info_window_resize(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let info = match app.get_webview_window("info") {
        Some(w) => w,
        None => return Ok(()), // hidden / not yet created — next show will size correctly
    };
    let w = width.max(120.0).min(480.0);
    let h = height.max(48.0).min(320.0);
    let _ = info.set_size(LogicalSize::new(w, h));
    if let Some(main) = app.get_webview_window("main") {
        if let Ok((x, y)) = info_window_position(&main, w) {
            let _ = info.set_position(LogicalPosition::new(x, y));
        }
    }
    Ok(())
}

/// Default size of the notify (toast) window. JS resizes it to fit
/// content after first render — these dimensions are just the initial
/// box before measurement.
const NOTIFY_W: f64 = 220.0;
const NOTIFY_H: f64 = 56.0;

/// Position the notify window centered above the main pet window. If
/// it would go off the top of the screen, callers can clamp later —
/// for now we keep it simple and let it spill (multi-monitor + dragged
/// to top is rare).
fn notify_window_position(
    main: &tauri::WebviewWindow,
    notify_w: f64,
    notify_h: f64,
) -> Result<(f64, f64), String> {
    let main_pos = main.outer_position().map_err(|e| e.to_string())?;
    let main_size = main.outer_size().map_err(|e| e.to_string())?;
    let scale = main.scale_factor().map_err(|e| e.to_string())?;
    let main_w_logical = main_size.width as f64 / scale;
    let main_x_logical = main_pos.x as f64 / scale;
    let main_y_logical = main_pos.y as f64 / scale;
    let x = main_x_logical + (main_w_logical - notify_w) / 2.0;
    let y = main_y_logical - notify_h - 6.0;
    Ok((x, y))
}

/// Show a transient notification bubble above the pet. Generic — used
/// for evolution announcements today, hook for level-ups / toasts /
/// reminders later. The actual text reaches the view through:
///   1. `last_notify` mutex (read by `NotifyView` on mount via
///      `notify_current` — avoids the create-window-then-emit race)
///   2. `notify://message` event (drives subsequent updates while the
///      window is already open)
/// Auto-hides after `duration_ms` (default 2500ms).
#[tauri::command]
async fn notify_show(
    state: State<'_, AppState>,
    app: AppHandle,
    text: String,
    duration_ms: Option<u64>,
) -> Result<(), String> {
    tracing::info!(text = %text, duration_ms = ?duration_ms, "notify_show invoked");
    let payload = NotifyPayload { text };
    *state
        .last_notify
        .lock()
        .map_err(|e| format!("last_notify lock poisoned: {e}"))? = Some(payload.clone());
    show_secondary_window(
        &app,
        "notify",
        "/?view=notify",
        NOTIFY_W,
        NOTIFY_H,
        false, // passive display, don't steal focus
        true,  // always-on-top: stage-up toast floats above other apps
        |main, w, h| notify_window_position(main, w, h),
    )
    .await?;
    let _ = app.emit("notify://message", &payload);
    let duration = duration_ms.unwrap_or(5000);
    let app_clone = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(duration)).await;
        if let Some(w) = app_clone.get_webview_window("notify") {
            let _ = w.hide();
        }
    });
    Ok(())
}

#[tauri::command]
fn notify_current(state: State<'_, AppState>) -> Result<Option<NotifyPayload>, String> {
    Ok(state
        .last_notify
        .lock()
        .map_err(|e| format!("last_notify lock poisoned: {e}"))?
        .clone())
}

#[tauri::command]
async fn notify_hide(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("notify") {
        let _ = w.hide();
    }
    Ok(())
}

/// JS-driven dynamic sizing for the notify window. Same pattern as
/// `info_window_resize` — view measures itself with ResizeObserver
/// after each render and asks Rust to size the window to fit.
#[tauri::command]
async fn notify_window_resize(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let notify = match app.get_webview_window("notify") {
        Some(w) => w,
        None => return Ok(()),
    };
    let w = width.max(140.0).min(520.0);
    let h = height.max(40.0).min(200.0);
    let _ = notify.set_size(LogicalSize::new(w, h));
    if let Some(main) = app.get_webview_window("main") {
        if let Ok((x, y)) = notify_window_position(&main, w, h) {
            let _ = notify.set_position(LogicalPosition::new(x, y));
        }
    }
    Ok(())
}

// ─── Confirm sub-window ────────────────────────────────────────────
//
// A standalone always-on-top window for GBA-styled confirm prompts
// (template-replace, pet-id-merge-vs-copy, etc). Lives in its own
// Tauri window so it doesn't have to resize / overlay the main pet
// window — fixes the "pet jumps when I click Import" + "modal hidden
// behind Pets dialog" + "after Cancel something covers the pet"
// problems we had with the in-main-window modal approach.
//
// Flow:
//   1. handleImport invokes `confirm_show(title, message, options)`.
//   2. Backend stores the prompt in AppState, opens (or shows)
//      the `confirm` window centered on screen, focused.
//   3. `ConfirmView` mounts, pulls the prompt via `confirm_current`,
//      renders the GBA dialog.
//   4. User clicks a button → ConfirmView invokes
//      `confirm_dismiss(choice)` → backend emits
//      `confirm://chosen` event and hides the window.
//   5. handleImport's promise (listening for that event) resolves.

const CONFIRM_DEFAULT_W: f64 = 460.0;
const CONFIRM_DEFAULT_H: f64 = 220.0;

#[tauri::command]
async fn confirm_show(
    state: State<'_, AppState>,
    app: AppHandle,
    title: String,
    message: String,
    options: Vec<ConfirmOption>,
) -> Result<(), String> {
    tracing::info!(title = %title, "confirm_show invoked");
    *state
        .last_confirm
        .lock()
        .map_err(|e| format!("last_confirm lock poisoned: {e}"))? =
        Some(ConfirmPrompt {
            title,
            message,
            options,
        });
    show_secondary_window(
        &app,
        "confirm",
        "/?view=confirm",
        CONFIRM_DEFAULT_W,
        CONFIRM_DEFAULT_H,
        true,  // focused so keyboard (Esc) works
        false, // NOT always-on-top: on macOS this combo (focused +
               // always-on-top + transparent + borderless) blocks
               // key-window status — confirm has buttons that need
               // clicks to land
        |main, w, h| centered_confirm_position(main, w, h),
    )
    .await
}

#[tauri::command]
fn confirm_current(state: State<'_, AppState>) -> Result<Option<ConfirmPrompt>, String> {
    Ok(state
        .last_confirm
        .lock()
        .map_err(|e| format!("last_confirm lock poisoned: {e}"))?
        .clone())
}

#[tauri::command]
async fn confirm_dismiss(app: AppHandle, choice: Option<String>) -> Result<(), String> {
    // Always emit FIRST so the awaiter resolves even if the hide
    // races (e.g. user closed the window via OS chrome).
    let payload = choice.unwrap_or_default();
    let _ = app.emit("confirm://chosen", &payload);
    if let Some(w) = app.get_webview_window("confirm") {
        let _ = w.hide();
    }
    Ok(())
}

/// Auto-size the confirm window so the GBA dialog fits its content
/// without scrollbars. ResizeObserver in the React view computes the
/// natural panel size and calls this.
#[tauri::command]
async fn confirm_window_resize(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let win = match app.get_webview_window("confirm") {
        Some(w) => w,
        None => return Ok(()),
    };
    // Generous caps — confirm prompts can carry a couple of lines of
    // body copy on top of a row of buttons.
    let w = width.max(300.0).min(640.0);
    let h = height.max(140.0).min(420.0);
    let _ = win.set_size(LogicalSize::new(w, h));
    if let Some(main) = app.get_webview_window("main") {
        if let Ok((x, y)) = centered_confirm_position(&main, w, h) {
            let _ = win.set_position(LogicalPosition::new(x, y));
        }
    }
    Ok(())
}

// ─── Naming popup window ───────────────────────────────────────────
//
// Hatching's closing modal — "Your egg hatched! Name your companion."
// — used to render INSIDE the floating pet window, which is tiny
// (~140px tall). The form was getting clipped. Solution: open a
// dedicated secondary window above the pet (like notify), big enough
// to fit a title, a body line, an input, and two buttons without
// cramming.
//
// Flow:
//   1. CeremonyPlayer hits the `modal` action with `calls:
//      pet_finalize_naming`, invokes `naming_window_show`.
//   2. Backend stores the prompt, opens the `naming` window above the
//      pet (focused, so the input receives keystrokes).
//   3. NamingView mounts, pulls prompt via `naming_current`, renders
//      the form.
//   4. User submits or skips → NamingView invokes `naming_dismiss`
//      with (name, confirmed). Backend calls `pet_finalize_naming` if
//      confirmed + non-empty, emits `naming://done`, hides window.
//   5. CeremonyPlayer's promise (listening for that event) resolves
//      and marks the modal dismissed → ceremony finishes.

const NAMING_DEFAULT_W: f64 = 300.0;
const NAMING_DEFAULT_H: f64 = 200.0;

#[tauri::command]
async fn naming_window_show(
    state: State<'_, AppState>,
    app: AppHandle,
    pet_id: String,
    placeholder: String,
    title: String,
    body: String,
    confirm_label: String,
    cancel_label: String,
) -> Result<(), String> {
    tracing::info!(pet_id = %pet_id, title = %title, "naming_window_show invoked");
    *state
        .last_naming
        .lock()
        .map_err(|e| format!("last_naming lock poisoned: {e}"))? = Some(NamingPrompt {
        pet_id,
        placeholder,
        title,
        body,
        confirm_label,
        cancel_label,
    });
    show_secondary_window(
        &app,
        "naming",
        "/?view=naming",
        NAMING_DEFAULT_W,
        NAMING_DEFAULT_H,
        true,  // focused so the input field receives keystrokes
        false, // NOT always-on-top — the canonical bug fix. On macOS,
               // a borderless + transparent + always-on-top window
               // cannot become a key window, so the input field
               // never receives keystrokes (and clicks pass through).
               // Dropping always-on-top lets the window be a regular
               // key window. If the user clicks elsewhere it goes
               // behind, but `useNamingPopupSync` will re-show it
               // next time the pet's state is re-evaluated.
        |main, w, h| notify_window_position(main, w, h),
    )
    .await
}

#[tauri::command]
fn naming_current(state: State<'_, AppState>) -> Result<Option<NamingPrompt>, String> {
    Ok(state
        .last_naming
        .lock()
        .map_err(|e| format!("last_naming lock poisoned: {e}"))?
        .clone())
}

/// Called by NamingView when the user submits or skips.
///
/// **Always finalizes** (sets `name_finalized_at`) so the popup
/// doesn't keep reappearing. The distinction is what NAME gets locked:
///   - `confirmed=true` with non-empty input → that name is saved
///   - everything else (Skip, confirmed-with-empty) → keep the
///     template default, lock the timestamp
///
/// The "didn't dismiss" path (user closed the OS window externally,
/// or quit the app entirely) leaves `name_finalized_at` null since
/// this command never runs — `useNamingPopupSync` will re-open the
/// popup next time the pet is active. That's the explicit user
/// spec: "must confirm or skip before it stops popping up".
#[tauri::command]
async fn naming_dismiss(
    state: State<'_, AppState>,
    app: AppHandle,
    pet_id: String,
    name: Option<String>,
    confirmed: bool,
) -> Result<(), String> {
    tracing::info!(pet_id = %pet_id, confirmed, "naming_dismiss invoked");

    // Decide what name to lock in. None = keep current default.
    let final_name: Option<String> = if confirmed {
        name.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    } else {
        None
    };

    state
        .xp
        .finalize_naming(&pet_id, final_name)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "naming_dismiss: finalize_naming failed");
            e.to_string()
        })?;
    // Broadcast so every window re-reads the snapshot — the active pet's
    // `name_finalized_at` is now set, which makes `useNamingPopupSync`
    // hide the popup and stop re-prompting.
    let _ = app.emit("pet://active_changed", &pet_id);
    let _ = app.emit("naming://done", &pet_id);

    if let Some(w) = app.get_webview_window("naming") {
        let _ = w.hide();
    }
    Ok(())
}

/// Hide the naming popup WITHOUT finalizing the pet's name and
/// WITHOUT emitting `naming://done`. Used when the user swaps to a
/// different pet — the popup for the previous pet should disappear
/// (it's no longer the active pet), but we don't want to mark the
/// hatch ceremony as user-dismissed (it stays pending and re-prompts
/// next time that pet is active).
///
/// Callers that DO want to mark naming completed should use
/// `naming_dismiss` instead.
#[tauri::command]
async fn naming_window_hide(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("naming") {
        let _ = w.hide();
    }
    Ok(())
}

/// Self-sizing: NamingView ResizeObserver fits the window to content.
#[tauri::command]
async fn naming_window_resize(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let win = match app.get_webview_window("naming") {
        Some(w) => w,
        None => return Ok(()),
    };
    let w = width.max(240.0).min(420.0);
    let h = height.max(140.0).min(280.0);
    let _ = win.set_size(LogicalSize::new(w, h));
    if let Some(main) = app.get_webview_window("main") {
        if let Ok((x, y)) = notify_window_position(&main, w, h) {
            let _ = win.set_position(LogicalPosition::new(x, y));
        }
    }
    Ok(())
}

/// Center the confirm window on the current monitor (NOT relative to
/// the floating pet — confirm prompts deserve screen-center attention
/// like an OS-native modal).
fn centered_confirm_position(
    main: &tauri::WebviewWindow,
    w: f64,
    h: f64,
) -> Result<(f64, f64), String> {
    let mon = match main.current_monitor().map_err(|e| e.to_string())? {
        Some(m) => m,
        // Fallback to positioning relative to main if no monitor.
        None => {
            let main_pos = main.outer_position().map_err(|e| e.to_string())?;
            let scale = main.scale_factor().map_err(|e| e.to_string())?;
            return Ok((main_pos.x as f64 / scale, main_pos.y as f64 / scale));
        }
    };
    let scale = mon.scale_factor();
    let mon_x = mon.position().x as f64 / scale;
    let mon_y = mon.position().y as f64 / scale;
    let mon_w = mon.size().width as f64 / scale;
    let mon_h = mon.size().height as f64 / scale;
    let x = mon_x + (mon_w - w) / 2.0;
    let y = mon_y + (mon_h - h) / 2.0;
    Ok((x, y))
}

/// Show (creating if necessary) the floating context-menu window at
/// the user's right-click position (window-local logical coords).
#[tauri::command]
async fn menu_window_show(app: AppHandle, x: f64, y: f64) -> Result<(), String> {
    tracing::info!(x, y, "menu_window_show invoked");
    show_secondary_window(
        &app,
        "menu",
        "/?view=menu",
        180.0,
        220.0,
        true, // must be key window so the first click on a menu item fires
        true, // always-on-top: floats above other apps until dismissed
        move |main, _w, _h| {
            // Position menu at right-click point, in screen coords.
            let main_pos = main.outer_position().map_err(|e| e.to_string())?;
            let scale = main.scale_factor().map_err(|e| e.to_string())?;
            let screen_x = main_pos.x as f64 / scale + x;
            let screen_y = main_pos.y as f64 / scale + y;
            Ok((screen_x, screen_y))
        },
    )
    .await
}

#[tauri::command]
async fn menu_window_hide(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("menu") {
        let _ = win.hide();
    }
    Ok(())
}

/// JS-driven dynamic sizing for the right-click menu, same pattern as
/// `info_window_resize` / `notify_window_resize`. The menu's item list
/// varies in height (dev rows hidden in release builds; might add more
/// items later) so a hardcoded size eventually clips the bottom rows.
#[tauri::command]
async fn menu_window_resize(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let menu = match app.get_webview_window("menu") {
        Some(w) => w,
        None => return Ok(()),
    };
    let w = width.max(160.0).min(360.0);
    let h = height.max(80.0).min(560.0);
    let _ = menu.set_size(LogicalSize::new(w, h));
    Ok(())
}

/// Process-wide lock serializing secondary-window creation. Without
/// this, two concurrent `show_secondary_window("info", ...)` calls can
/// both observe `get_webview_window("info") == None`, both proceed to
/// `builder.build()`, and Tauri 2 will happily create two windows with
/// the same label — one of which is orphaned (no longer reachable via
/// `get_webview_window`, stuck on screen at its initial position).
static SECONDARY_WINDOW_LOCK: Mutex<()> = Mutex::new(());

/// Logical (width, height) of a window, or `None` if either query
/// failed. Used to compute correct positioning for windows whose actual
/// size diverges from the builder's default (e.g. the info bubble
/// auto-resizes itself to fit content).
fn window_logical_size(win: &tauri::WebviewWindow) -> Option<(f64, f64)> {
    let size = win.outer_size().ok()?;
    let scale = win.scale_factor().ok()?;
    Some((size.width as f64 / scale, size.height as f64 / scale))
}

/// Helper: lazy-create a secondary transparent window with the given
/// label and route, position it via the closure, and show it. If the
/// window already exists, just reposition + show.
///
/// `focused` — whether the new window becomes the key window. Info
/// bubble = false (passive display, don't steal focus), context menu
/// = true (clicks must fire on first try, requires key-window status).
///
/// `always_on_top` — whether the window stays above other application
/// windows. **Critical macOS gotcha**: setting this `true` on a
/// borderless + transparent window prevents the OS from granting it
/// key-window status, even with `focused: true`. The window appears
/// but ignores clicks AND keyboard input. So: pass `false` for any
/// window that contains a text input or needs reliable click
/// interaction (the naming popup is the canonical example). Pass
/// `true` only for purely passive overlays (info / notify) which
/// don't need to receive events.
async fn show_secondary_window<F>(
    app: &AppHandle,
    label: &str,
    route: &str,
    width: f64,
    height: f64,
    focused: bool,
    always_on_top: bool,
    position_fn: F,
) -> Result<(), String>
where
    F: Fn(&tauri::WebviewWindow, f64, f64) -> Result<(f64, f64), String>,
{
    let main = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;

    let _guard = SECONDARY_WINDOW_LOCK
        .lock()
        .map_err(|e| format!("secondary window lock poisoned: {e}"))?;

    if let Some(win) = app.get_webview_window(label) {
        // Reuse path: use the actual current size for position math
        // (the window may have been resized by JS — e.g. the info
        // bubble auto-fits its content) and DON'T set_size here so we
        // don't clobber that resize.
        let (actual_w, actual_h) = window_logical_size(&win).unwrap_or((width, height));
        let (x, y) = position_fn(&main, actual_w, actual_h)?;
        tracing::info!(label, x, y, w = actual_w, "reusing existing secondary window");
        let _ = win.set_position(LogicalPosition::new(x, y));
        let _ = win.show();
        if focused {
            let _ = win.set_focus();
        }
        return Ok(());
    }

    let (x, y) = position_fn(&main, width, height)?;
    tracing::info!(
        label,
        route,
        x,
        y,
        focused,
        always_on_top,
        "creating secondary window"
    );
    let builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App(route.into()))
        .inner_size(width, height)
        .position(x, y)
        .decorations(false)
        .transparent(true)
        .always_on_top(always_on_top)
        .skip_taskbar(true)
        .resizable(false)
        .focused(focused)
        .visible(true)
        .shadow(false);

    builder.build().map_err(|e| {
        tracing::error!(label, error = %e, "WebviewWindowBuilder.build failed");
        e.to_string()
    })?;
    tracing::info!(label, "secondary window built");
    Ok(())
}

/// Dev helper: zero out the active pet's XP (wipes xp_event rows +
/// recomputes pet_state to 0). Pet identity / snapshot remain. Emits
/// `pet://state` so the frontend resets visuals to stage_0.
#[tauri::command]
async fn pet_reset_xp(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Option<PetStateUpdate>, String> {
    tracing::info!("pet_reset_xp invoked");
    // Reload pet doc from disk first so any out-of-band edits to
    // pet.json (e.g. ceremony text tweaks during dev) are picked up.
    // Reset is the natural "back to fresh" affordance — refreshing
    // the in-memory cache matches that semantic.
    state
        .xp
        .refresh_active_pet()
        .await
        .map_err(|e| e.to_string())?;
    let update = state.xp.reset_active_xp().await.map_err(|e| {
        tracing::error!(error = %e, "pet_reset_xp failed");
        e.to_string()
    })?;
    match &update {
        Some(u) => {
            tracing::info!(
                pet_id = %u.pet_id,
                level_before = u.level_before,
                level_after = u.level_after,
                xp_after = u.total_xp,
                "pet_reset_xp ok",
            );
            emit_pet_state(&app, u);
        }
        None => tracing::warn!("pet_reset_xp: no active pet"),
    }
    Ok(update)
}

/// macOS-only NSWindow tweaks required for the floating pet window:
///
/// * `setAcceptsMouseMovedEvents:YES` — without this, AppKit only delivers
///   mouse-moved events to the key window. Our pet window is always-on-top
///   but typically NOT the key window (user's editor / terminal stays
///   focused). Without this flag, `mouseenter` / `mouseleave` and JS
///   `mousemove` never fire when the user hovers the pet.
///
/// * `setMovableByWindowBackground:YES` — lets a left-mousedown on any
///   opaque pixel start a native window drag, even when the window is not
///   the key window. Without this, the first click on an inactive window
///   is consumed by AppKit for activation, breaking drag-to-move.
#[cfg(target_os = "macos")]
fn configure_macos_main_window(window: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    let ns_window_ptr = match window.ns_window() {
        Ok(p) if !p.is_null() => p,
        Ok(_) => {
            tracing::warn!("ns_window() returned null pointer");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "ns_window() failed");
            return;
        }
    };
    unsafe {
        let window_obj: *mut AnyObject = ns_window_ptr.cast();
        let _: () = msg_send![window_obj, setAcceptsMouseMovedEvents: true];
        let _: () = msg_send![window_obj, setMovableByWindowBackground: true];
    }
    tracing::info!(
        "NSWindow configured: acceptsMouseMovedEvents=YES, movableByWindowBackground=YES"
    );
}

#[cfg(not(target_os = "macos"))]
fn configure_macos_main_window(_window: &tauri::WebviewWindow) {}

/// Background task: poll the global cursor position every 60ms and emit
/// `cursor://pet_enter` / `cursor://pet_leave` Tauri events when the
/// cursor crosses the main window's bounding box.
///
/// Why polling instead of native `mouseenter` / `mouseleave`: on macOS,
/// a transparent, always-on-top window that is NOT the current key
/// window does not receive `mouseMoved` events even with
/// `acceptsMouseMovedEvents:YES` — AppKit only forwards them through a
/// chain of conditions WKWebView doesn't satisfy here. Polling
/// `cursor_position()` works regardless of focus / window level / level
/// mask, at a flat ~20 Hz CPU cost.
fn spawn_cursor_poller(app: AppHandle) {
    tokio::spawn(async move {
        let mut was_inside = false;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let inside = match cursor_inside_main(&app) {
                Some(b) => b,
                None => continue, // window missing or transient query failure
            };
            if inside != was_inside {
                was_inside = inside;
                let topic = if inside { "cursor://pet_enter" } else { "cursor://pet_leave" };
                if let Err(e) = app.emit(topic, ()) {
                    tracing::warn!(error = %e, topic, "emit cursor event failed");
                }
            }
            // Keep the info bubble pinned under the pet while it's
            // moving (drag). `WindowEvent::Moved` does not fire
            // continuously through a native `startDragging()` operation
            // on macOS, so we sample position here instead. No-op when
            // the info window is hidden.
            reposition_pet_anchored_windows_if_visible(&app);
        }
    });
}

fn cursor_inside_main(app: &AppHandle) -> Option<bool> {
    let main = app.get_webview_window("main")?;
    let pos = app.cursor_position().ok()?;
    let win_pos = main.outer_position().ok()?;
    let win_size = main.outer_size().ok()?;
    let x_in = pos.x >= win_pos.x as f64 && pos.x < (win_pos.x + win_size.width as i32) as f64;
    let y_in = pos.y >= win_pos.y as f64 && pos.y < (win_pos.y + win_size.height as i32) as f64;
    Some(x_in && y_in)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,petpet=debug,petpet_desktop_lib=debug")))
        .try_init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let db = rt.block_on(async { DbHandle::open(&paths::db_path()).await }).expect("open db");
    let xp_engine = rt
        .block_on(async { XPEngine::open(db.clone()).await })
        .expect("open xp engine");
    let shutdown = CancellationToken::new();
    let port = std::env::var("PETPET_HOOK_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(petpet::DEFAULT_HOOK_PORT);

    // Auto-install hooks into every detected provider's config.
    // Idempotent — repeated launches converge without duplication.
    let _ = petpet::hooks::ensure_installed(port);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            db: db.clone(),
            hook_port: port,
            xp: xp_engine.clone(),
            last_notify: Mutex::new(None),
            last_confirm: Mutex::new(None),
            last_naming: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            stats_summary,
            hook_port,
            pet_snapshot,
            pet_feeding_bill,
            pet_feeding_bill_by_day,
            template_list,
            pet_pick_template,
            pet_finalize_naming,
            pet_grant_xp,
            pet_reset_xp,
            dev_mode,
            main_window_resize,
            pet_list_all,
            pet_list_summaries,
            pet_set_active,
            dashboard::dashboard_data,
            dashboard::dashboard_provider_detail,
            dashboard::dashboard_provider_requests_page,
            archive_cmds::template_export,
            archive_cmds::pet_export,
            archive_cmds::archive_import,
            archive_cmds::template_create,
            archive_cmds::preset_list_levels,
            archive_cmds::preset_list_stages,
            archive_cmds::sprite_stage_for_picker,
            info_window_show,
            info_window_hide,
            info_window_resize,
            menu_window_show,
            menu_window_hide,
            menu_window_resize,
            notify_show,
            notify_hide,
            notify_current,
            notify_window_resize,
            confirm_show,
            confirm_dismiss,
            confirm_current,
            confirm_window_resize,
            naming_window_show,
            naming_current,
            naming_dismiss,
            naming_window_hide,
            naming_window_resize
        ])
        .setup({
            let db = db.clone();
            let xp = xp_engine.clone();
            let shutdown = shutdown.clone();
            move |app| {
                // Apply macOS-specific NSWindow tweaks to the main pet
                // window. Required for first-click drag to work on a
                // transparent, non-key, always-on-top window. See
                // `configure_macos_main_window`. (Mouse-moved delivery
                // still doesn't work in this configuration, so hover is
                // driven by `spawn_cursor_poller` below instead.)
                if let Some(main) = app.get_webview_window("main") {
                    configure_macos_main_window(&main);
                    // While the user drags the pet, keep the info bubble
                    // pinned just below it. The cursor poller can't
                    // catch this — drag holds the cursor "inside"
                    // throughout, so no transition fires.
                    let app_for_move = app.handle().clone();
                    main.on_window_event(move |ev| {
                        if let tauri::WindowEvent::Moved(_) = ev {
                            reposition_pet_anchored_windows_if_visible(&app_for_move);
                        }
                    });
                } else {
                    tracing::warn!("main window not yet available in setup");
                }

                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    rt.block_on(spawn_ingestion(handle, db, xp, shutdown));
                });
                Ok(())
            }
        })
        .on_window_event({
            let shutdown = shutdown.clone();
            move |_window, event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    shutdown.cancel();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn spawn_ingestion(
    app: AppHandle,
    db: Arc<DbHandle>,
    xp: Arc<XPEngine>,
    shutdown: CancellationToken,
) {
    // ─── Cursor poller (drives hover info popup) ────────────────────────
    spawn_cursor_poller(app.clone());

    // ─── Usage ingestion: TWO independent lanes ─────────────────────────
    //
    // Phase 2 introduces dual ingestion lanes that route to the same DB
    // writer but differ on what else they trigger downstream:
    //
    //   live_tx    (this PR — renamed from `raw_tx`) — full fan-out:
    //              ① emit "usage://event" to frontend  (live UI react)
    //              ② xp.ingest_usage  (grants XP to active pet)
    //              ③ db_tx → writer   (persists usage_event)
    //
    //   history_tx (B3 will add) — DB-only relay:
    //              ③ db_tx → writer
    //              Used for historical import flowing into the
    //              Dashboard's "All" view, never granting XP. The
    //              isolation lets `Provider::import_historical()` emit
    //              events without retroactively crediting any pet.
    //
    // Two senders share one db_writer (cloned `db_tx`) so the DB layer
    // stays single-writer + serialized via WAL.
    let (live_tx, mut live_rx) = mpsc::channel::<UsageEvent>(8192);
    let (db_tx, db_rx) = mpsc::channel::<UsageEvent>(8192);
    let writer = spawn_writer(db.clone(), db_rx, shutdown.clone());

    // Live fan-out: events from `Provider::watch()` → XP + DB + frontend.
    // History lane (B3) will share `db_tx` via a separate relay task.
    let fan_app = app.clone();
    let fan_shutdown = shutdown.clone();
    let fan_xp = xp.clone();
    let live_db_tx = db_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = fan_shutdown.cancelled() => break,
                maybe = live_rx.recv() => match maybe {
                    Some(ev) => {
                        let _ = fan_app.emit("usage://event", &ev);
                        // Feed XP engine — token-based growth, gated on
                        // active pet (XPEngine::ingest_usage returns
                        // None when no active pet, so this is safe even
                        // before the user has hatched).
                        match fan_xp.ingest_usage(&ev).await {
                            Ok(Some(update)) => emit_pet_state(&fan_app, &update),
                            Ok(None) => {}
                            Err(e) => tracing::warn!(error = %e, "xp ingest_usage failed"),
                        }
                        if live_db_tx.send(ev).await.is_err() {
                            tracing::error!("live → db channel closed");
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    });

    // `db_tx` (the original) lives until spawn_ingestion returns;
    // B3 will add `let history_db_tx = db_tx.clone()` before that
    // point to give the history relay its own sender clone.

    let live_sink = EventSink::new(live_tx);
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(ClaudeCodeProvider::new(db.clone())),
        Box::new(CodexProvider::new(db.clone())),
        Box::new(OpenCodeProvider::new(db.clone())),
        // Aider: zero-setup if Aider is detected (auto-writes
        // analytics-log into ~/.aider.conf.yml). Inactive no-op
        // otherwise, no files created speculatively. See provider/aider.rs.
        Box::new(AiderProvider::new(db.clone())),
    ];

    // Build the ActivitySink that we'll route into for log-derived events.
    // We construct it INSIDE the provider loop so providers feed the same
    // sink as the hook server below — frontend can't tell which path an
    // event came from, which is the whole point of the fallback.
    let (provider_act_tx, mut provider_act_rx) = mpsc::channel::<ActivityEvent>(1024);
    let provider_activity_sink = ActivitySink::new(provider_act_tx);
    let act_relay_app = app.clone();
    let act_relay_shutdown = shutdown.clone();
    let act_relay_xp = xp.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = act_relay_shutdown.cancelled() => break,
                Some(ev) = provider_act_rx.recv() => {
                    tracing::info!(
                        provider = %ev.provider,
                        kind = ?ev.kind,
                        "log-derived activity"
                    );
                    let _ = act_relay_app.emit("activity://event", &ev);
                    match act_relay_xp.ingest_activity(&ev).await {
                        Ok(Some(update)) => emit_pet_state(&act_relay_app, &update),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "xp ingest_activity failed"),
                    }
                }
                else => break,
            }
        }
    });

    let mut handles = Vec::new();
    for p in providers {
        let sink = live_sink.clone();
        let act_sink = provider_activity_sink.clone();
        let token = shutdown.clone();
        handles.push(tokio::spawn(async move {
            tracing::info!(provider = %p.id(), "starting provider watch");
            if let Err(e) = p.watch(&sink, &act_sink, token).await {
                tracing::error!(provider = %p.id(), error = %e, "watch terminated");
            }
        }));
    }
    drop(live_sink);
    drop(provider_activity_sink);

    // ─── Activity path: Layer 1 hook server → frontend (no DB) ──────────
    let (act_tx, mut act_rx) = mpsc::channel::<ActivityEvent>(1024);
    let act_sink = ActivitySink::new(act_tx);
    let act_app = app.clone();
    let act_shutdown = shutdown.clone();
    let act_xp = xp.clone();
    handles.push(tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = act_shutdown.cancelled() => break,
                maybe = act_rx.recv() => match maybe {
                    Some(ev) => {
                        let _ = act_app.emit("activity://event", &ev);
                        match act_xp.ingest_activity(&ev).await {
                            Ok(Some(update)) => emit_pet_state(&act_app, &update),
                            Ok(None) => {}
                            Err(e) => tracing::warn!(error = %e, "xp ingest_activity failed"),
                        }
                    }
                    None => break,
                }
            }
        }
    }));

    let hook_server = HookServer::new(act_sink);
    let hs_shutdown = shutdown.clone();
    handles.push(tokio::spawn(async move {
        if let Err(e) = hook_server.run(hs_shutdown).await {
            tracing::error!(error = %e, "hook server terminated");
        }
    }));

    shutdown.cancelled().await;
    for h in handles {
        let _ = h.await;
    }
    if let Ok(Ok(s)) = writer.await {
        tracing::info!(inserted = s.inserted, deduped = s.deduped, failed = s.failed, "writer drained");
    }
}

/// Emit every state change as `pet://state`. Emit `pet://level_up` as a
/// second event so the frontend can trigger a celebratory animation
/// without diffing prev/next state itself.
fn emit_pet_state(app: &AppHandle, update: &PetStateUpdate) {
    let _ = app.emit("pet://state", update);
    if update.leveled_up {
        let _ = app.emit("pet://level_up", update);
    }
}

