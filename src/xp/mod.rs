//! Petpet growth/XP system — event sourcing over usage / activity events.
//!
//! Layered structure:
//!
//! ```text
//! UsageEvent / ActivityEvent / ManualGrant
//!         │
//!         ▼
//!     ┌──────────────────────────────────────────┐
//!     │   XPEngine                                │
//!     │     ├─ XPCalculator                       │
//!     │     │    ├─ RuleResolver (loads xp_rule)  │
//!     │     │    └─ Scorers: usage / activity / .. │
//!     │     ├─ XpEventWriter (INSERT OR IGNORE)   │
//!     │     └─ StateManager (pet_state, level up) │
//!     └──────────────────────────────────────────┘
//!         │
//!         ▼ emits
//!     PetStateUpdate { pet, total_xp, level, sprite_key, leveled_up }
//! ```
//!
//! Persistence: `xp_event` is append-only, deterministically keyed by
//! (pet_id, source_type, source_ref) so re-import / re-emission cannot
//! double-count.

pub mod algorithm;
pub mod calculator;
pub mod classification;
pub mod cost_query;
pub mod engine;
pub mod heuristic;
pub mod pricing;
pub mod registry;
pub mod registry_sync;
pub mod resolver;
pub mod scorer;
pub mod state;
pub mod types;
pub mod writer;

pub use engine::{
    replay_events_and_recompute, PetStateUpdate, PetSummary, XPEngine, XPEngineSnapshot,
};
pub use types::{
    ActivityInput, ManualGrant, MatchContext, Pet, PetStageRow, RuleId, XpComputation,
    XpSource, XpSourceType,
};

// ─── Shared test-only env-var lock ──────────────────────────────────
// Tests across `xp::engine` and `xp::registry_sync` both mutate the
// global `PETPET_HOME` env var to point at a per-test tempdir. Without
// a shared lock, two test modules can clobber each other's env state
// mid-run — manifesting as ~10% flake on `pick_template_creates_pet_*`
// (the engine test sees PETPET_HOME pointing at a non-existent
// registry_sync tempdir → "unknown template: sun").
//
// One mutex, both modules. Production code never touches it.
#[cfg(test)]
pub(crate) fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
