//! Codex provider — reads `~/.codex/sessions/**/*.jsonl`.
//!
//! Codex's JSONL is event-stream-shaped: session_meta opens, turn_context
//! lines announce the model for upcoming turns, and `event_msg.token_count`
//! lines carry per-emission token deltas in `info.last_token_usage`.
//!
//! Critical normalization rule: Codex's `input_tokens` already **includes**
//! `cached_input_tokens`. We subtract before assigning to `TokenDelta.input`
//! so cache hits aren't double-counted across providers.

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

pub struct CodexProvider {
    watcher: JsonlWatcher,
}

impl CodexProvider {
    pub fn new(db: Arc<DbHandle>) -> Self {
        let roots: Vec<PathBuf> = paths::codex_sessions_root().into_iter().collect();
        let watcher = JsonlWatcher::new(
            ProviderId::Codex,
            roots,
            "**/*.jsonl",
            db,
            Arc::new(|_path| Box::new(CodexLineParser::default()) as Box<dyn JsonlReader>),
        );
        Self { watcher }
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn display_name(&self) -> &'static str {
        "Codex"
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

/// Per-file mutable state — Codex requires correlating `token_count` events
/// with the most recent `turn_context.model` and `session_meta` envelope.
#[derive(Default)]
struct CodexLineParser {
    session_id: Option<String>,
    cwd: Option<String>,
    current_model: Option<String>,
    originator: Option<String>,
}

impl JsonlReader for CodexLineParser {
    fn parse_line(&mut self, line: &str, source: SourceRef) -> ParseOutput {
        let Ok(env): Result<CodexEnvelope, _> = serde_json::from_str(line) else {
            return ParseOutput::default();
        };

        let mut out = ParseOutput::default();
        let ts = env.timestamp.unwrap_or_else(Utc::now);

        match env.line_type.as_str() {
            "session_meta" => {
                if let Some(payload) = env.payload {
                    let meta: SessionMetaPayload = match serde_json::from_value(payload) {
                        Ok(v) => v,
                        Err(_) => return out,
                    };
                    self.session_id = Some(meta.id);
                    self.cwd = meta.cwd;
                    self.originator = meta.originator;
                    // session_meta is the canonical session start signal
                    out.activity.push(self.make_activity(ts, ActivityKind::SessionStart {
                        source: self.originator.clone(),
                    }));
                }
            }
            "turn_context" => {
                if let Some(payload) = env.payload {
                    let turn: TurnContextPayload = match serde_json::from_value(payload) {
                        Ok(v) => v,
                        Err(_) => return out,
                    };
                    if let Some(m) = turn.model {
                        self.current_model = Some(m);
                    }
                    if let Some(c) = turn.cwd {
                        self.cwd = Some(c);
                    }
                }
            }
            "event_msg" => {
                self.handle_event_msg(env.payload.clone(), env.timestamp, source, &mut out);
            }
            _ => {}
        }
        out
    }
}

impl CodexLineParser {
    fn make_activity(&self, ts: DateTime<Utc>, kind: ActivityKind) -> ActivityEvent {
        ActivityEvent {
            id: Uuid::new_v4(),
            provider: ProviderId::Codex,
            session_id: self.session_id.clone(),
            project_path: self.cwd.clone(),
            timestamp: ts,
            kind,
        }
    }
}

impl CodexLineParser {
    fn handle_event_msg(
        &mut self,
        payload: Option<serde_json::Value>,
        timestamp: Option<DateTime<Utc>>,
        source: SourceRef,
        out: &mut ParseOutput,
    ) {
        let Some(payload) = payload else { return };
        let Some(payload_type) = payload.get("type").and_then(|v| v.as_str()) else {
            return;
        };
        let ts = timestamp.unwrap_or_else(Utc::now);

        // ─── Activity derivation ───────────────────────────────────────
        // Mirror the hook events Codex would have fired, so the pet
        // reacts even when hooks aren't loaded yet (no-restart fallback).
        match payload_type {
            // Only `user_message` carries the actual prompt content.
            // `task_started` is just the turn boundary — emitting
            // UserPromptSubmit with None preview was creating phantom
            // "empty prompts" in the activity stream.
            "user_message" => {
                let preview = payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(|s| {
                        let mut t: String = s.chars().take(120).collect();
                        if s.chars().count() > 120 {
                            t.push('…');
                        }
                        t
                    });
                if preview.is_some() {
                    out.activity.push(self.make_activity(
                        ts,
                        ActivityKind::UserPromptSubmit { prompt_preview: preview },
                    ));
                }
            }
            "exec_command_start" => {
                let cmd_name = payload
                    .get("parsed_cmd")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        payload
                            .get("command")
                            .and_then(|v| v.as_array())
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_str())
                    })
                    .unwrap_or("Bash")
                    .to_string();
                let tool_use_id = payload.get("call_id").and_then(|v| v.as_str()).map(String::from);
                out.activity.push(self.make_activity(
                    ts,
                    ActivityKind::ToolUseStart { name: cmd_name, tool_use_id },
                ));
            }
            "exec_command_end" => {
                let cmd_name = payload
                    .get("parsed_cmd")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or("Bash")
                    .to_string();
                let exit = payload.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
                let tool_use_id = payload.get("call_id").and_then(|v| v.as_str()).map(String::from);
                out.activity.push(self.make_activity(
                    ts,
                    ActivityKind::ToolUseEnd { name: cmd_name, success: exit == 0, tool_use_id },
                ));
            }
            "mcp_tool_call_end" => {
                let invocation = payload.get("invocation");
                let name = invocation
                    .and_then(|v| v.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("mcp")
                    .to_string();
                out.activity.push(self.make_activity(
                    ts,
                    ActivityKind::ToolUseEnd { name, success: true, tool_use_id: None },
                ));
            }
            "task_complete" => {
                out.activity.push(self.make_activity(ts, ActivityKind::AssistantStop));
            }
            _ => {}
        }

        // ─── Usage (token_count) — unchanged ───────────────────────────
        if payload_type != "token_count" {
            return;
        }
        let tc: TokenCountPayload = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Some(info) = tc.info else { return };
        let Some(last) = info.last_token_usage else { return };

        // Codex's input_tokens bundles cache hits; uncached input is the
        // remainder. Use saturating_sub to tolerate any future schema drift.
        let cache_read = last.cached_input_tokens;
        let input = last.input_tokens.saturating_sub(cache_read);

        let tokens = TokenDelta {
            input,
            output: last.output_tokens,
            cache_read,
            cache_creation: 0,
            reasoning: last.reasoning_output_tokens,
        };
        if tokens.is_zero() {
            return;
        }

        let Some(session_id) = self.session_id.clone() else {
            tracing::trace!(file = %source.file, "token_count before session_meta — skipping");
            return;
        };
        let model = self.current_model.clone().unwrap_or_else(|| "unknown".to_string());
        let id = UsageEvent::deterministic_id(ProviderId::Codex, &source);

        out.usage.push(UsageEvent {
            id,
            provider: ProviderId::Codex,
            client: self.originator.clone(),
            session_id,
            project_path: self.cwd.clone(),
            git_branch: None,
            model,
            timestamp: ts,
            tokens,
            kind: EventKind::Turn { stop_reason: None },
            source,
        });
    }
}

#[derive(Deserialize)]
struct CodexEnvelope {
    timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "type")]
    line_type: String,
    payload: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: Option<String>,
    originator: Option<String>,
}

#[derive(Deserialize)]
struct TurnContextPayload {
    cwd: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct TokenCountPayload {
    info: Option<TokenCountInfo>,
}

#[derive(Deserialize)]
struct TokenCountInfo {
    last_token_usage: Option<TokenCountValues>,
}

#[derive(Deserialize)]
struct TokenCountValues {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(offset: u64) -> SourceRef {
        SourceRef { file: "test.jsonl".into(), byte_offset: offset, line: offset / 10 + 1 }
    }

    #[test]
    fn token_count_before_session_meta_is_ignored() {
        let mut p = CodexLineParser::default();
        let line = r#"{"timestamp":"2026-04-17T00:44:33Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0}}}}"#;
        assert!(p.parse_line(line, src(0)).usage.is_empty());
    }

    #[test]
    fn full_session_flow() {
        let mut p = CodexLineParser::default();

        let meta = r#"{"timestamp":"2026-04-17T00:44:32Z","type":"session_meta","payload":{"id":"019d","cwd":"/Users/x","originator":"Codex Desktop"}}"#;
        let meta_out = p.parse_line(meta, src(0));
        assert!(meta_out.usage.is_empty());
        // session_meta now produces a SessionStart activity
        assert_eq!(meta_out.activity.len(), 1);
        assert!(matches!(meta_out.activity[0].kind, ActivityKind::SessionStart { .. }));

        let turn = r#"{"timestamp":"2026-04-17T00:44:33Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/Users/x","model":"gpt-5.3-codex","approval_policy":"x","sandbox_policy":"y","summary":""}}"#;
        assert!(p.parse_line(turn, src(100)).usage.is_empty());

        let tok = r#"{"timestamp":"2026-04-17T00:44:50Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":19619,"cached_input_tokens":18432,"output_tokens":338,"reasoning_output_tokens":83},"total_token_usage":{"input_tokens":19619,"cached_input_tokens":18432,"output_tokens":338,"reasoning_output_tokens":83,"total_tokens":19957},"model_context_window":258400}}}"#;
        let out = p.parse_line(tok, src(200));
        assert_eq!(out.usage.len(), 1);
        let e = &out.usage[0];
        assert_eq!(e.provider, ProviderId::Codex);
        assert_eq!(e.client.as_deref(), Some("Codex Desktop"));
        assert_eq!(e.session_id, "019d");
        assert_eq!(e.project_path.as_deref(), Some("/Users/x"));
        assert_eq!(e.model, "gpt-5.3-codex");
        // input_tokens(19619) - cached(18432) = 1187 uncached
        assert_eq!(e.tokens.input, 1187);
        assert_eq!(e.tokens.cache_read, 18432);
        assert_eq!(e.tokens.output, 338);
        assert_eq!(e.tokens.reasoning, 83);
        assert_eq!(e.tokens.cache_creation, 0);
    }

    #[test]
    fn null_info_is_skipped() {
        let mut p = CodexLineParser::default();
        let meta = r#"{"type":"session_meta","payload":{"id":"x"}}"#;
        p.parse_line(meta, src(0));
        let line = r#"{"timestamp":"2026-04-17T00:44:50Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{}}}"#;
        let out = p.parse_line(line, src(50));
        assert!(out.usage.is_empty());
        assert!(out.activity.is_empty());
    }

    #[test]
    fn model_changes_mid_session() {
        let mut p = CodexLineParser::default();
        p.parse_line(r#"{"type":"session_meta","payload":{"id":"s"}}"#, src(0));
        p.parse_line(r#"{"type":"turn_context","payload":{"model":"gpt-5.3-codex"}}"#, src(50));
        let tok = r#"{"timestamp":"2026-04-17T00:44:50Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0}}}}"#;
        let e1 = p.parse_line(tok, src(100));
        assert_eq!(e1.usage[0].model, "gpt-5.3-codex");

        p.parse_line(r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#, src(150));
        let e2 = p.parse_line(tok, src(200));
        assert_eq!(e2.usage[0].model, "gpt-5.5");
    }

    #[test]
    fn exec_command_start_emits_tool_use_start() {
        let mut p = CodexLineParser::default();
        p.parse_line(r#"{"type":"session_meta","payload":{"id":"s"}}"#, src(0));
        let line = r#"{"timestamp":"2026-04-17T00:44:50Z","type":"event_msg","payload":{"type":"exec_command_start","call_id":"c1","command":["ls","-la"],"parsed_cmd":["ls"]}}"#;
        let out = p.parse_line(line, src(100));
        let starts: Vec<_> = out.activity.iter().filter_map(|a| match &a.kind {
            ActivityKind::ToolUseStart { name, tool_use_id } => Some((name.clone(), tool_use_id.clone())),
            _ => None,
        }).collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0, "ls");
        assert_eq!(starts[0].1.as_deref(), Some("c1"));
    }

    #[test]
    fn task_complete_emits_assistant_stop() {
        let mut p = CodexLineParser::default();
        p.parse_line(r#"{"type":"session_meta","payload":{"id":"s"}}"#, src(0));
        let line = r#"{"timestamp":"2026-04-17T00:45:00Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","completed_at":1,"duration_ms":1234}}"#;
        let out = p.parse_line(line, src(100));
        assert!(out.activity.iter().any(|a| matches!(a.kind, ActivityKind::AssistantStop)));
    }
}
