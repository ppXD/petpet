//! petpet — desktop pet that ingests usage events from multiple AI coding
//! agents into a unified store.
//!
//! Layered architecture:
//!
//! ```text
//!  Layer 1  hook server (HTTP)        ┐
//!  Layer 2  log watcher (this PR)    ─┼─►  Provider trait  ─►  EventSink  ─►  SQLite
//!  Layer 3  OpenAI-compatible proxy   ┘
//! ```
//!
//! Every layer normalizes its inputs into [`event::UsageEvent`]. Downstream
//! consumers (UI, pet state machine, stats) read only `UsageEvent`s.

pub mod db;
pub mod event;
pub mod hooks;
pub mod model;
pub mod paths;
pub mod provider;
pub mod template;
pub mod xp;

pub use event::{ActivityEvent, ActivityKind, EventKind, ProviderId, SourceRef, TokenDelta, UsageEvent};
pub use hooks::{ActivitySink, HookServer, DEFAULT_HOOK_PORT};
pub use provider::{BackfillStats, EventSink, Provider};
