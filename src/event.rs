//! Unified domain event that every provider normalizes to.
//!
//! Downstream consumers (db writer, pet state machine, stats, tasks) read
//! `UsageEvent` and never look at the original provider payloads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable, lowercase identifier for the source provider.
///
/// Adding a new provider = add a variant + namespace UUID + implement
/// `Provider` trait. String form is what gets written to SQLite
/// (`provider` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    ClaudeCode,
    Codex,
    Gemini,
    OpenCode,
    Aider,
    CustomApi,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::ClaudeCode => "claude_code",
            ProviderId::Codex => "codex",
            ProviderId::Gemini => "gemini",
            ProviderId::OpenCode => "opencode",
            ProviderId::Aider => "aider",
            ProviderId::CustomApi => "custom_api",
        }
    }

    /// URL slug used in `POST /hooks/{slug}/{event}`. Shorter than `as_str`
    /// and stable across underscores.
    pub fn slug(self) -> &'static str {
        match self {
            ProviderId::ClaudeCode => "claude",
            ProviderId::Codex => "codex",
            ProviderId::Gemini => "gemini",
            ProviderId::OpenCode => "opencode",
            ProviderId::Aider => "aider",
            ProviderId::CustomApi => "custom",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        Some(match slug {
            "claude" => ProviderId::ClaudeCode,
            "codex" => ProviderId::Codex,
            "gemini" => ProviderId::Gemini,
            "opencode" => ProviderId::OpenCode,
            "aider" => ProviderId::Aider,
            "custom" => ProviderId::CustomApi,
            _ => return None,
        })
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Token counts for a single normalized event.
///
/// All values are **uncached** counts — providers that bundle cache hits into
/// `input` must subtract them before construction (see `provider/codex.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenDelta {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub reasoning: u64,
}

impl TokenDelta {
    pub fn is_zero(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_creation == 0
            && self.reasoning == 0
    }

    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation + self.reasoning
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionStart { source: Option<String> },
    SessionEnd,
    Turn { stop_reason: Option<String> },
    ToolCall { name: String },
    ToolResult { name: String, exit_code: Option<i32> },
}

impl EventKind {
    pub fn tag(&self) -> &'static str {
        match self {
            EventKind::SessionStart { .. } => "session_start",
            EventKind::SessionEnd => "session_end",
            EventKind::Turn { .. } => "turn",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::ToolResult { .. } => "tool_result",
        }
    }
}

/// Reference back to where this event was derived from.
/// Enables forensic queries and idempotent re-ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub file: String,
    pub byte_offset: u64,
    pub line: u64,
}

/// The unified event every consumer sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: Uuid,
    pub provider: ProviderId,
    /// Specific client variant inside the provider, when known.
    /// Examples: `"cli"`, `"claude-desktop"` (Claude); `"Codex Desktop"` (Codex).
    /// `None` if the source line did not carry an identifier.
    pub client: Option<String>,
    pub session_id: String,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub model: String,
    pub timestamp: DateTime<Utc>,
    pub tokens: TokenDelta,
    pub kind: EventKind,
    pub source: SourceRef,
}

/// Interaction signal emitted by Layer 1 (hook server). Decoupled from
/// `UsageEvent` because hooks fire on event boundaries that carry no token
/// usage — they exist purely to drive instant pet animation / micro-states.
///
/// Not persisted: ActivityEvent is fire-and-forget. If the app isn't open
/// when a hook arrives, the moment is lost (and that's fine — we wouldn't
/// have animated it anyway). Token-accountable growth flows through
/// `UsageEvent` → DB only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: Uuid,
    pub provider: ProviderId,
    pub session_id: Option<String>,
    pub project_path: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub kind: ActivityKind,
}

/// Normalized interaction signal. Covers every Claude Code / Codex hook
/// event we care about, with one [`ActivityKind::Other`] catchall for
/// events the host added but we haven't typed yet — so the frontend can
/// still react instead of dropping them.
///
/// Tag = `type` (snake_case). Add new variants here; frontend then matches
/// `ev.kind.type` and renders the right animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActivityKind {
    // ── Lifecycle ─────────────────────────────────────────
    SessionStart { source: Option<String> },
    SessionEnd { reason: Option<String> },

    // ── Per-turn ──────────────────────────────────────────
    UserPromptSubmit { prompt_preview: Option<String> },
    UserPromptExpansion { command_name: Option<String> },
    AssistantStop,
    AssistantStopFailure { reason: Option<String> },

    // ── Tool loop ─────────────────────────────────────────
    ToolUseStart { name: String, tool_use_id: Option<String> },
    ToolUseEnd { name: String, success: bool, tool_use_id: Option<String> },
    ToolBatchEnd { count: usize },
    PermissionRequest { tool_name: String },
    PermissionDenied { tool_name: String, reason: Option<String> },

    // ── Subagent ──────────────────────────────────────────
    SubagentStart { agent_type: String },
    SubagentStop { agent_type: String },

    // ── Task tracking ─────────────────────────────────────
    TaskCreated { title: String },
    TaskCompleted { title: String },

    // ── Compaction ────────────────────────────────────────
    PreCompact { trigger: Option<String> },
    PostCompact { trigger: Option<String> },

    // ── Misc ──────────────────────────────────────────────
    Notification { message: String, notification_type: Option<String> },

    // ── Catchall ──────────────────────────────────────────
    Other { event_name: String },
}

impl UsageEvent {
    /// Deterministic id derived from provider + a **provider-supplied
    /// external id** (e.g. Anthropic's `message.id`). Use this in
    /// preference to [`deterministic_id`] whenever the source format
    /// gives us a stable per-event identifier — file+offset dedup is
    /// idempotent on cursor edge cases, but it cannot collapse the
    /// case where one logical event is split into multiple JSONL
    /// lines (Claude Code's content-block streaming writes the same
    /// `message.usage` to a `thinking` line, a `text` line, and a
    /// `tool_use` line — file+offset thinks they're three events).
    ///
    /// Naming inputs:
    ///   - `provider` selects the namespace (same as `deterministic_id`)
    ///   - `external_id` is whatever the provider calls "this event"
    ///     (e.g. `msg_014CTJaJAiAe87WDUrjgXXka` for Anthropic).
    ///
    /// Result is stable across re-ingestions: re-reading the same
    /// `message.id` produces the same Uuid, so `INSERT OR IGNORE`
    /// against the `id` primary key collapses duplicates at write
    /// time without any extra index.
    pub fn external_event_id(provider: ProviderId, external_id: &str) -> Uuid {
        let ns = Self::provider_namespace(provider);
        Uuid::new_v5(&ns, external_id.as_bytes())
    }

    /// Deterministic id derived from provider+file+offset.
    /// Re-ingesting the same line yields the same id → INSERT OR IGNORE
    /// makes the writer idempotent on cursor edge cases.
    ///
    /// Use [`external_event_id`] in preference when the source format
    /// supplies a stable per-event id (e.g. Anthropic `message.id`) —
    /// this function cannot collapse N JSONL lines that share one
    /// logical event into a single id.
    pub fn deterministic_id(provider: ProviderId, source: &SourceRef) -> Uuid {
        let ns = Self::provider_namespace(provider);
        let name = format!("{}:{}", source.file, source.byte_offset);
        Uuid::new_v5(&ns, name.as_bytes())
    }

    /// Per-provider UUIDv5 namespace. Each provider gets a distinct
    /// 128-bit constant so deterministic ids from different providers
    /// cannot collide even when they derive from the same external id
    /// or file+offset. The byte patterns spell the provider name in
    /// hex-ish form (e.g. `a1de…` for Aider) so logs are debug-friendly.
    fn provider_namespace(provider: ProviderId) -> Uuid {
        match provider {
            ProviderId::ClaudeCode => Uuid::from_bytes([
                0xc1, 0x4d, 0xc0, 0xde, 0x00, 0x00, 0x40, 0x00,
                0x80, 0x00, 0xc1, 0xa1, 0x1d, 0xe0, 0xcd, 0xe0,
            ]),
            ProviderId::Codex => Uuid::from_bytes([
                0xc0, 0xde, 0xcf, 0x00, 0x00, 0x00, 0x40, 0x00,
                0x80, 0x00, 0xc0, 0xde, 0xc0, 0xde, 0xc0, 0xde,
            ]),
            ProviderId::Gemini => Uuid::from_bytes([
                0x9e, 0x71, 0x10, 0x00, 0x00, 0x00, 0x40, 0x00,
                0x80, 0x00, 0x9e, 0x71, 0x9e, 0x71, 0x9e, 0x71,
            ]),
            ProviderId::OpenCode => Uuid::from_bytes([
                0x09, 0xec, 0x10, 0x00, 0x00, 0x00, 0x40, 0x00,
                0x80, 0x00, 0x09, 0xec, 0x09, 0xec, 0x09, 0xec,
            ]),
            ProviderId::Aider => Uuid::from_bytes([
                0xa1, 0xde, 0x10, 0x00, 0x00, 0x00, 0x40, 0x00,
                0x80, 0x00, 0xa1, 0xde, 0xa1, 0xde, 0xa1, 0xde,
            ]),
            ProviderId::CustomApi => Uuid::from_bytes([
                0xcf, 0xa9, 0x10, 0x00, 0x00, 0x00, 0x40, 0x00,
                0x80, 0x00, 0xcf, 0xa9, 0xcf, 0xa9, 0xcf, 0xa9,
            ]),
        }
    }
}
