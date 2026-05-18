//! Hook payload (JSON) → unified [`ActivityEvent`].
//!
//! Claude Code and Codex CLI ship the **same** PascalCase event names and
//! near-identical payload schemas (Codex literally vendors `ClaudeHooksEngine`).
//! One parser handles both; the URL routing tells us the provider.
//!
//! Unknown event names map to [`ActivityKind::Other`] so the frontend can
//! still react (or ignore) — beats silently dropping.

use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use crate::event::{ActivityEvent, ActivityKind, ProviderId};

const PROMPT_PREVIEW_CHARS: usize = 120;

pub fn parse(provider: ProviderId, event_name: &str, body: &Value) -> ActivityEvent {
    let kind = parse_kind(event_name, body);
    ActivityEvent {
        id: Uuid::new_v4(),
        provider,
        session_id: body.get("session_id").and_then(|v| v.as_str()).map(String::from),
        project_path: body.get("cwd").and_then(|v| v.as_str()).map(String::from),
        timestamp: Utc::now(),
        kind,
    }
}

fn parse_kind(event: &str, body: &Value) -> ActivityKind {
    match event {
        // ── Lifecycle ──
        "SessionStart" => ActivityKind::SessionStart {
            source: body.get("source").and_then(|v| v.as_str()).map(String::from),
        },
        "SessionEnd" => ActivityKind::SessionEnd {
            reason: body.get("reason").and_then(|v| v.as_str()).map(String::from),
        },

        // ── Gemini aliases ──
        // Gemini's native event vocabulary maps onto our canonical tool-use
        // axis. Treating them as Claude PreToolUse/PostToolUse here means
        // the rest of the system (animations, achievements) never has to
        // know which CLI fired the event.
        "BeforeTool" => ActivityKind::ToolUseStart {
            name: tool_name(body),
            tool_use_id: body.get("tool_use_id").and_then(|v| v.as_str()).map(String::from),
        },
        "AfterTool" => ActivityKind::ToolUseEnd {
            name: tool_name(body),
            success: !body
                .get("tool_response")
                .and_then(|r| r.get("is_error"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tool_use_id: body.get("tool_use_id").and_then(|v| v.as_str()).map(String::from),
        },

        // ── Per-turn ──
        "UserPromptSubmit" => ActivityKind::UserPromptSubmit {
            prompt_preview: body
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(truncate_preview),
        },
        "UserPromptExpansion" => ActivityKind::UserPromptExpansion {
            command_name: body
                .get("command_name")
                .and_then(|v| v.as_str())
                .map(String::from),
        },
        "Stop" => ActivityKind::AssistantStop,
        "StopFailure" => ActivityKind::AssistantStopFailure {
            reason: body
                .get("hook_event_name") // matcher-as-reason fallback
                .and_then(|v| v.as_str())
                .map(String::from),
        },

        // ── Tool loop ──
        "PreToolUse" => ActivityKind::ToolUseStart {
            name: tool_name(body),
            tool_use_id: body.get("tool_use_id").and_then(|v| v.as_str()).map(String::from),
        },
        "PostToolUse" => ActivityKind::ToolUseEnd {
            name: tool_name(body),
            success: !body
                .get("tool_response")
                .and_then(|r| r.get("is_error"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tool_use_id: body.get("tool_use_id").and_then(|v| v.as_str()).map(String::from),
        },
        "PostToolUseFailure" => ActivityKind::ToolUseEnd {
            name: tool_name(body),
            success: false,
            tool_use_id: body.get("tool_use_id").and_then(|v| v.as_str()).map(String::from),
        },
        "PostToolBatch" => ActivityKind::ToolBatchEnd {
            count: body
                .get("tool_uses")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        },
        "PermissionRequest" => ActivityKind::PermissionRequest {
            tool_name: tool_name(body),
        },
        "PermissionDenied" => ActivityKind::PermissionDenied {
            tool_name: tool_name(body),
            reason: body
                .get("denial_reason")
                .and_then(|v| v.as_str())
                .map(String::from),
        },

        // ── Subagent ──
        "SubagentStart" => ActivityKind::SubagentStart {
            agent_type: body
                .get("agent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        },
        "SubagentStop" => ActivityKind::SubagentStop {
            agent_type: body
                .get("agent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        },

        // ── Task tracking ──
        "TaskCreated" => ActivityKind::TaskCreated {
            title: body
                .get("task_title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "TaskCompleted" => ActivityKind::TaskCompleted {
            title: body
                .get("task_title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },

        // ── Compaction ──
        "PreCompact" => ActivityKind::PreCompact {
            trigger: body.get("trigger").and_then(|v| v.as_str()).map(String::from),
        },
        "PostCompact" => ActivityKind::PostCompact {
            trigger: body.get("trigger").and_then(|v| v.as_str()).map(String::from),
        },

        // ── Misc ──
        "Notification" => ActivityKind::Notification {
            message: body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            notification_type: body
                .get("notification_type")
                .and_then(|v| v.as_str())
                .map(String::from),
        },

        // ── Unknown ──
        other => ActivityKind::Other { event_name: other.to_string() },
    }
}

fn tool_name(body: &Value) -> String {
    body.get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn truncate_preview(s: &str) -> String {
    let mut out: String = s.chars().take(PROMPT_PREVIEW_CHARS).collect();
    if s.chars().count() > PROMPT_PREVIEW_CHARS {
        out.push('…');
    }
    out
}

// Backwards-compatible API. Tauri-facing routes call `claude()` / `codex()`
// which are now thin wrappers over the unified parser.
pub fn claude(event: &str, body: &Value) -> Option<ActivityEvent> {
    Some(parse(ProviderId::ClaudeCode, event, body))
}

pub fn codex(event: &str, body: &Value) -> Option<ActivityEvent> {
    Some(parse(ProviderId::Codex, event, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_prompt_submit_captures_preview() {
        let ev = parse(
            ProviderId::ClaudeCode,
            "UserPromptSubmit",
            &json!({"session_id":"a","cwd":"/x","prompt":"refactor auth to scrypt"}),
        );
        match ev.kind {
            ActivityKind::UserPromptSubmit { prompt_preview } => {
                assert!(prompt_preview.unwrap().contains("scrypt"));
            }
            _ => panic!("wrong kind"),
        }
        assert_eq!(ev.provider, ProviderId::ClaudeCode);
        assert_eq!(ev.session_id.as_deref(), Some("a"));
    }

    #[test]
    fn pre_post_tool_use_pair() {
        let pre = parse(
            ProviderId::ClaudeCode,
            "PreToolUse",
            &json!({"session_id":"a","tool_name":"Bash","tool_use_id":"u1"}),
        );
        assert!(matches!(
            pre.kind,
            ActivityKind::ToolUseStart { ref name, ref tool_use_id }
                if name == "Bash" && tool_use_id.as_deref() == Some("u1")
        ));

        let post_ok = parse(
            ProviderId::ClaudeCode,
            "PostToolUse",
            &json!({"tool_name":"Bash","tool_response":{}}),
        );
        assert!(matches!(
            post_ok.kind,
            ActivityKind::ToolUseEnd { success: true, .. }
        ));

        let post_err = parse(
            ProviderId::ClaudeCode,
            "PostToolUse",
            &json!({"tool_name":"Bash","tool_response":{"is_error":true}}),
        );
        assert!(matches!(
            post_err.kind,
            ActivityKind::ToolUseEnd { success: false, .. }
        ));
    }

    #[test]
    fn post_tool_use_failure_event_maps_to_failed_end() {
        let ev = parse(
            ProviderId::ClaudeCode,
            "PostToolUseFailure",
            &json!({"tool_name":"Bash","tool_error":"oh no"}),
        );
        assert!(matches!(
            ev.kind,
            ActivityKind::ToolUseEnd { success: false, ref name, .. } if name == "Bash"
        ));
    }

    #[test]
    fn post_tool_batch_counts_uses() {
        let ev = parse(
            ProviderId::ClaudeCode,
            "PostToolBatch",
            &json!({"tool_uses":[{"name":"Read"},{"name":"Edit"},{"name":"Bash"}]}),
        );
        assert!(matches!(ev.kind, ActivityKind::ToolBatchEnd { count: 3 }));
    }

    #[test]
    fn permission_request_captures_tool_name() {
        let ev = parse(
            ProviderId::ClaudeCode,
            "PermissionRequest",
            &json!({"tool_name":"Bash"}),
        );
        assert!(matches!(
            ev.kind,
            ActivityKind::PermissionRequest { ref tool_name } if tool_name == "Bash"
        ));
    }

    #[test]
    fn subagent_start_stop_with_type() {
        let start = parse(
            ProviderId::ClaudeCode,
            "SubagentStart",
            &json!({"agent_type":"Explore","agent_id":"id1"}),
        );
        assert!(matches!(
            start.kind,
            ActivityKind::SubagentStart { ref agent_type } if agent_type == "Explore"
        ));
        let stop = parse(
            ProviderId::ClaudeCode,
            "SubagentStop",
            &json!({"agent_type":"Plan"}),
        );
        assert!(matches!(
            stop.kind,
            ActivityKind::SubagentStop { ref agent_type } if agent_type == "Plan"
        ));
    }

    #[test]
    fn task_lifecycle_captures_title() {
        let created = parse(
            ProviderId::ClaudeCode,
            "TaskCreated",
            &json!({"task_title":"Refactor auth"}),
        );
        assert!(matches!(
            created.kind,
            ActivityKind::TaskCreated { ref title } if title == "Refactor auth"
        ));
        let done = parse(
            ProviderId::ClaudeCode,
            "TaskCompleted",
            &json!({"task_id":"t1","task_title":"Refactor auth"}),
        );
        assert!(matches!(
            done.kind,
            ActivityKind::TaskCompleted { ref title } if title == "Refactor auth"
        ));
    }

    #[test]
    fn compaction_events_pass_trigger() {
        let pre = parse(
            ProviderId::ClaudeCode,
            "PreCompact",
            &json!({"trigger":"auto"}),
        );
        assert!(matches!(
            pre.kind,
            ActivityKind::PreCompact { trigger: Some(ref t) } if t == "auto"
        ));
        let post = parse(
            ProviderId::ClaudeCode,
            "PostCompact",
            &json!({"trigger":"manual"}),
        );
        assert!(matches!(
            post.kind,
            ActivityKind::PostCompact { trigger: Some(ref t) } if t == "manual"
        ));
    }

    #[test]
    fn unknown_event_falls_through_to_other() {
        let ev = parse(
            ProviderId::ClaudeCode,
            "SomeFutureEvent",
            &json!({"session_id":"x"}),
        );
        assert!(matches!(
            ev.kind,
            ActivityKind::Other { ref event_name } if event_name == "SomeFutureEvent"
        ));
    }

    #[test]
    fn codex_uses_same_schema_as_claude() {
        let body = json!({
            "session_id": "codex-019",
            "cwd": "/Users/x",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "hello"
        });
        let ev = parse(ProviderId::Codex, "UserPromptSubmit", &body);
        assert_eq!(ev.provider, ProviderId::Codex);
        assert!(matches!(ev.kind, ActivityKind::UserPromptSubmit { .. }));
    }

    #[test]
    fn long_prompt_truncates_with_ellipsis() {
        let body = json!({"prompt": "x".repeat(500)});
        let ev = parse(ProviderId::ClaudeCode, "UserPromptSubmit", &body);
        match ev.kind {
            ActivityKind::UserPromptSubmit { prompt_preview } => {
                let p = prompt_preview.unwrap();
                assert!(p.ends_with('…'));
                assert_eq!(p.chars().count(), PROMPT_PREVIEW_CHARS + 1);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn notification_carries_type_and_message() {
        let ev = parse(
            ProviderId::ClaudeCode,
            "Notification",
            &json!({"message":"hi","notification_type":"idle_prompt"}),
        );
        match ev.kind {
            ActivityKind::Notification { message, notification_type } => {
                assert_eq!(message, "hi");
                assert_eq!(notification_type.as_deref(), Some("idle_prompt"));
            }
            _ => panic!(),
        }
    }
}
