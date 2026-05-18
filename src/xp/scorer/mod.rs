//! Scorer modules: each turns one kind of input + its rule config into XP.

pub mod activity;
pub mod manual;
pub mod usage;

pub use activity::ActivityScorer;
pub use manual::ManualScorer;
pub use usage::UsageScorer;
