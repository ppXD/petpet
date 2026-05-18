//! Claude Code provider — reads `~/.claude/projects/**/*.jsonl`.
//!
//! Each JSONL line is one event. We only emit on `type=assistant` entries
//! whose `message.usage` reports non-zero tokens. The `<synthetic>` model
//! marker (system-injected messages) is filtered out — those carry zero
//! tokens anyway but the filter is explicit for clarity.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::db::DbHandle;
use crate::event::{ActivityEvent, ActivityKind, EventKind, ProviderId, SourceRef, TokenDelta, UsageEvent};
use crate::hooks::ActivitySink;
use crate::paths;
use crate::provider::jsonl_watcher::{JsonlReader, JsonlWatcher, ParseOutput};
use crate::provider::{BackfillStats, EventSink, Provider};
use uuid::Uuid;

pub struct ClaudeCodeProvider {
    watcher: JsonlWatcher,
}

impl ClaudeCodeProvider {
    pub fn new(db: Arc<DbHandle>) -> Self {
        let roots: Vec<PathBuf> = paths::claude_projects_root().into_iter().collect();
        let watcher = JsonlWatcher::new(
            ProviderId::ClaudeCode,
            roots,
            "**/*.jsonl",
            db,
            Arc::new(|_path| Box::new(ClaudeLineParser::default()) as Box<dyn JsonlReader>),
        );
        Self { watcher }
    }
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::ClaudeCode
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    async fn backfill(&self, sink: &EventSink) -> Result<BackfillStats> {
        self.watcher.backfill(sink).await
    }

    async fn watch(
        &self,
        sink: &EventSink,
        activity_sink: &ActivitySink,
        shutdown: CancellationToken,
    ) -> Result<()> {
        self.watcher.watch(sink, activity_sink, shutdown).await
    }
}

#[derive(Default)]
struct ClaudeLineParser;

impl JsonlReader for ClaudeLineParser {
    fn parse_line(&mut self, line: &str, source: SourceRef) -> ParseOutput {
        let parsed: ClaudeLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return ParseOutput::default(),
        };
        let mut out = ParseOutput::default();
        let session_id = parsed.session_id.clone();
        let cwd = parsed.cwd.clone();
        let timestamp_for_activity = parsed.timestamp.unwrap_or_else(Utc::now);

        // ─── Activity derivation (no-restart fallback for hooks) ────────
        // We mirror what the hook event would have produced, so frontend
        // gets identical reactions whether or not hooks are installed.
        match parsed.line_type.as_str() {
            "user" => {
                // Skip meta / caveat / synthetic injections — they're not
                // real user prompts and would spam the pet with thinking.
                if !parsed.is_meta.unwrap_or(false) {
                    let prompt_text = parsed.message.as_ref().and_then(|m| match &m.content {
                        Some(ClaudeContent::Text(s)) => Some(s.clone()),
                        Some(ClaudeContent::Blocks(blocks)) => blocks
                            .iter()
                            .filter_map(|b| match b {
                                ClaudeBlock::Text { text } => Some(text.clone()),
                                _ => None,
                            })
                            .next(),
                        None => None,
                    });
                    if let Some(text) = prompt_text {
                        if !text.is_empty() {
                            out.activity.push(ActivityEvent {
                                id: Uuid::new_v4(),
                                provider: ProviderId::ClaudeCode,
                                session_id: session_id.clone(),
                                project_path: cwd.clone(),
                                timestamp: timestamp_for_activity,
                                kind: ActivityKind::UserPromptSubmit {
                                    prompt_preview: Some(truncate(&text, 120)),
                                },
                            });
                        }
                    }
                }
            }
            "assistant" => {
                // Tool uses live in content blocks. Each tool_use block →
                // ToolUseStart. Assistant turn ending without a pending
                // tool_use → AssistantStop.
                let blocks = parsed
                    .message
                    .as_ref()
                    .and_then(|m| match &m.content {
                        Some(ClaudeContent::Blocks(b)) => Some(b.as_slice()),
                        _ => None,
                    })
                    .unwrap_or(&[]);
                let mut had_tool_use = false;
                for b in blocks {
                    if let ClaudeBlock::ToolUse { id: tool_id, name } = b {
                        had_tool_use = true;
                        out.activity.push(ActivityEvent {
                            id: Uuid::new_v4(),
                            provider: ProviderId::ClaudeCode,
                            session_id: session_id.clone(),
                            project_path: cwd.clone(),
                            timestamp: timestamp_for_activity,
                            kind: ActivityKind::ToolUseStart {
                                name: name.clone(),
                                tool_use_id: tool_id.clone(),
                            },
                        });
                    }
                }
                let stop = parsed
                    .message
                    .as_ref()
                    .and_then(|m| m.stop_reason.as_deref());
                if !had_tool_use && matches!(stop, Some("end_turn") | Some("stop_sequence")) {
                    out.activity.push(ActivityEvent {
                        id: Uuid::new_v4(),
                        provider: ProviderId::ClaudeCode,
                        session_id: session_id.clone(),
                        project_path: cwd.clone(),
                        timestamp: timestamp_for_activity,
                        kind: ActivityKind::AssistantStop,
                    });
                }
            }
            _ => {}
        }

        // ─── Usage derivation (unchanged — still tied to assistant turn) ─
        if parsed.line_type != "assistant" {
            return out;
        }
        let Some(message) = parsed.message else { return out; };
        let Some(model) = message.model else { return out; };
        if model == "<synthetic>" {
            return out;
        }
        let Some(usage) = message.usage else { return out; };
        let tokens = TokenDelta {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_input_tokens,
            cache_creation: usage.cache_creation_input_tokens,
            reasoning: 0,
        };
        if tokens.is_zero() {
            return out;
        }
        let Some(session_id) = session_id else { return out; };
        let timestamp = parsed.timestamp.unwrap_or_else(Utc::now);

        // Dedup key: prefer Anthropic's `message.id` over file+offset.
        // Claude Code streams one Anthropic response across multiple
        // JSONL lines (thinking / text / tool_use blocks), and each
        // line carries an identical copy of `message.usage`. Counting
        // every line over-counts by 35-110% depending on tool-use
        // intensity. Using `message.id` collapses N lines → 1 id, and
        // the writer's INSERT OR IGNORE drops the duplicates at write
        // time. Fall back to file+offset only when `message.id` is
        // missing (defensive — every real assistant message has one).
        let id = match message.id.as_deref() {
            Some(msg_id) if !msg_id.is_empty() => {
                UsageEvent::external_event_id(ProviderId::ClaudeCode, msg_id)
            }
            _ => UsageEvent::deterministic_id(ProviderId::ClaudeCode, &source),
        };

        out.usage.push(UsageEvent {
            id,
            provider: ProviderId::ClaudeCode,
            client: parsed.entrypoint,
            session_id,
            project_path: cwd,
            git_branch: parsed.git_branch,
            model,
            timestamp,
            tokens,
            kind: EventKind::Turn { stop_reason: message.stop_reason },
            source,
        });
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    line_type: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    message: Option<ClaudeMessage>,
    entrypoint: Option<String>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    /// Anthropic's stable per-message identifier (e.g.
    /// `msg_014CTJaJAiAe87WDUrjgXXka`). Critical for dedup: Claude
    /// Code's streaming writer emits ONE JSONL line per content
    /// block (thinking / text / tool_use), and each line carries
    /// the SAME `usage` payload. Without using `id` as the dedup
    /// key, the same response gets counted 2-3× (35-110% overcount
    /// observed in real data — see tests below).
    id: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    usage: Option<ClaudeUsage>,
    /// Either a plain string ("hi") or an array of content blocks.
    content: Option<ClaudeContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ClaudeContent {
    Text(String),
    Blocks(Vec<ClaudeBlock>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: Option<String>,
        name: String,
    },
    ToolResult {
        #[serde(default)]
        #[allow(dead_code)]
        tool_use_id: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> SourceRef {
        SourceRef { file: "test.jsonl".into(), byte_offset: 0, line: 1 }
    }

    #[test]
    fn ignores_non_assistant_lines() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"user","message":{"content":"hi"}}"#;
        assert!(p.parse_line(line, src()).usage.is_empty());
    }

    #[test]
    fn ignores_synthetic_model() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"assistant","sessionId":"s","timestamp":"2026-04-17T02:59:04.321Z","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#;
        assert!(p.parse_line(line, src()).usage.is_empty());
    }

    #[test]
    fn extracts_assistant_turn() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"assistant","sessionId":"sess-1","cwd":"/Users/x/p","gitBranch":"main","timestamp":"2026-04-17T02:59:04.321Z","entrypoint":"claude-desktop","message":{"model":"claude-sonnet-4","stop_reason":"end_turn","usage":{"input_tokens":137201,"output_tokens":258,"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}"#;
        let out = p.parse_line(line, src());
        assert_eq!(out.usage.len(), 1);
        let e = &out.usage[0];
        assert_eq!(e.provider, ProviderId::ClaudeCode);
        assert_eq!(e.client.as_deref(), Some("claude-desktop"));
        assert_eq!(e.session_id, "sess-1");
        assert_eq!(e.model, "claude-sonnet-4");
        assert_eq!(e.tokens.input, 137201);
        assert_eq!(e.tokens.output, 258);
        assert_eq!(e.tokens.cache_read, 10);
        assert_eq!(e.tokens.cache_creation, 5);
        assert_eq!(e.tokens.reasoning, 0);
        assert!(matches!(e.kind, EventKind::Turn { ref stop_reason } if stop_reason.as_deref() == Some("end_turn")));
        // Assistant turn with no tool_use blocks AND end_turn stop → AssistantStop activity
        assert!(out.activity.iter().any(|a| matches!(a.kind, ActivityKind::AssistantStop)));
    }

    #[test]
    fn user_prompt_emits_activity() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"user","sessionId":"s","cwd":"/x","timestamp":"2026-04-17T03:00:00Z","message":{"role":"user","content":"please refactor auth"}}"#;
        let out = p.parse_line(line, src());
        assert!(out.usage.is_empty(), "user lines have no token usage");
        assert_eq!(out.activity.len(), 1);
        match &out.activity[0].kind {
            ActivityKind::UserPromptSubmit { prompt_preview } => {
                assert!(prompt_preview.as_ref().unwrap().contains("refactor"));
            }
            _ => panic!("expected UserPromptSubmit"),
        }
    }

    #[test]
    fn meta_user_line_is_silent() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"user","sessionId":"s","cwd":"/x","timestamp":"2026-04-17T03:00:00Z","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>noise</local-command-caveat>"}}"#;
        let out = p.parse_line(line, src());
        assert!(out.usage.is_empty());
        assert!(out.activity.is_empty(), "meta lines must not spam UserPromptSubmit");
    }

    #[test]
    fn assistant_with_tool_use_emits_tool_use_start() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-04-17T03:00:00Z","message":{"model":"claude-opus-4-7","content":[{"type":"text","text":"running"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let out = p.parse_line(line, src());
        let starts: Vec<_> = out.activity.iter().filter_map(|a| match &a.kind {
            ActivityKind::ToolUseStart { name, tool_use_id } => Some((name.clone(), tool_use_id.clone())),
            _ => None,
        }).collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0, "Bash");
        assert_eq!(starts[0].1.as_deref(), Some("toolu_1"));
        // With tool_use present, we should NOT emit AssistantStop
        assert!(!out.activity.iter().any(|a| matches!(a.kind, ActivityKind::AssistantStop)));
        // Usage still flows (tokens > 0)
        assert_eq!(out.usage.len(), 1);
    }

    #[test]
    fn zero_usage_emits_nothing() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"assistant","sessionId":"s","timestamp":"2026-04-17T02:59:04.321Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        assert!(p.parse_line(line, src()).usage.is_empty());
    }

    /// Pin the bug-fix: same `message.id` repeated across N JSONL
    /// lines must produce the SAME UsageEvent id, so the writer's
    /// INSERT OR IGNORE collapses them at write time.
    ///
    /// Observed in real data:
    /// - one Anthropic API response → 3 JSONL lines (thinking / text
    ///   / tool_use blocks), each carrying identical message.usage
    /// - pre-fix petpet counted 3× → 35-110% overcount per day
    /// - post-fix: 3 lines → 1 deterministic id → 1 inserted row
    #[test]
    fn duplicate_message_id_collapses_to_one_id() {
        let mut p = ClaudeLineParser::default();
        // Three lines from the same Anthropic response — the only
        // difference is content (thinking / text / tool_use). usage
        // is identical because Anthropic returns the cumulative
        // total on each streamed block.
        let lines = [
            // (1) thinking block
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-05-17T00:10:31.967Z","requestId":"req_x","uuid":"u1","parentUuid":"p1","message":{"model":"claude-opus-4-7","id":"msg_DUP_TEST","content":[{"type":"thinking","thinking":"…"}],"usage":{"input_tokens":6,"output_tokens":4577,"cache_read_input_tokens":901396,"cache_creation_input_tokens":1004}}}"#,
            // (2) text block — same msg_id, same usage
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-05-17T00:10:34.687Z","requestId":"req_x","uuid":"u2","parentUuid":"u1","message":{"model":"claude-opus-4-7","id":"msg_DUP_TEST","content":[{"type":"text","text":"…"}],"usage":{"input_tokens":6,"output_tokens":4577,"cache_read_input_tokens":901396,"cache_creation_input_tokens":1004}}}"#,
            // (3) tool_use block — same msg_id, same usage
            r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-05-17T00:10:54.774Z","requestId":"req_x","uuid":"u3","parentUuid":"u2","message":{"model":"claude-opus-4-7","id":"msg_DUP_TEST","content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{}}],"usage":{"input_tokens":6,"output_tokens":4577,"cache_read_input_tokens":901396,"cache_creation_input_tokens":1004}}}"#,
        ];

        let mut emitted_ids: Vec<uuid::Uuid> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            // Vary the SourceRef offset so the OLD file+offset dedup
            // logic would have produced 3 different ids. If we still
            // see 3 different ids after the fix, the bug is back.
            let mut s = src();
            s.byte_offset = (i as u64) * 1000;
            let out = p.parse_line(line, s);
            assert_eq!(out.usage.len(), 1, "line {i} should yield one usage event");
            emitted_ids.push(out.usage[0].id);
        }

        assert_eq!(emitted_ids.len(), 3);
        assert_eq!(
            emitted_ids[0], emitted_ids[1],
            "duplicate message.id must produce the SAME id (thinking vs text)"
        );
        assert_eq!(
            emitted_ids[1], emitted_ids[2],
            "duplicate message.id must produce the SAME id (text vs tool_use)"
        );
    }

    /// Different `message.id` → distinct ids, even if file+offset
    /// happens to align. Guards against an over-eager fix that
    /// would collapse legitimately-separate messages.
    #[test]
    fn distinct_message_ids_produce_distinct_event_ids() {
        let mut p = ClaudeLineParser::default();
        let line_a = r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-05-17T00:10:31Z","message":{"model":"claude-opus-4-7","id":"msg_AAA","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let line_b = r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-05-17T00:10:32Z","message":{"model":"claude-opus-4-7","id":"msg_BBB","usage":{"input_tokens":10,"output_tokens":5}}}"#;

        // Same source offset to force any file+offset-based id-derivation
        // path to collide.
        let mut s = src();
        s.byte_offset = 12345;

        let id_a = p.parse_line(line_a, s.clone()).usage[0].id;
        let id_b = p.parse_line(line_b, s).usage[0].id;
        assert_ne!(id_a, id_b, "distinct message.id must NOT collapse");
    }

    /// Defensive fallback: if a malformed line has no message.id at
    /// all, we still derive an id from file+offset rather than
    /// dropping the event. Belt-and-braces — Anthropic has never
    /// emitted an assistant message without id, but tests pin the
    /// behavior so a future "drop on missing id" refactor surfaces.
    #[test]
    fn missing_message_id_falls_back_to_file_offset_id() {
        let mut p = ClaudeLineParser::default();
        let line = r#"{"type":"assistant","sessionId":"s","cwd":"/x","timestamp":"2026-05-17T00:10:31Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let out = p.parse_line(line, src());
        assert_eq!(out.usage.len(), 1, "missing id must NOT drop the event");
    }
}
