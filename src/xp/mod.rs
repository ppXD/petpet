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
