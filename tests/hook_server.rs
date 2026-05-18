//! **Layer 2 — HTTP integration** for `HookServer`.
//!
//! Scope of this file (strict): every test here exercises the real
//! axum `HookServer` over HTTP, using `reqwest` as the client. No
//! shell, no `curl`, no platform-specific spawning. This tier:
//!
//! 1. Catches server-routing / parsing / concurrency regressions
//!    fast — no shell-fork overhead, runs in milliseconds.
//! 2. Tests behaviours that the shell E2E doesn't bother with —
//!    404s on unknown providers, malformed bodies, query-param
//!    survival, healthz, concurrent deliveries.
//! 3. Decouples server validation from shell quoting. If a future
//!    refactor breaks the HTTP layer, *this* tier flags it cleanly
//!    instead of the L3 shell tier failing with a confusing curl
//!    error.
//!
//! Companion tiers:
//!   - L1 unit: `src/hooks/install/mod.rs::build_curl_command_tests`
//!     and `src/hooks/install/util.rs::tests` — pure-function pinning.
//!   - L3 shell E2E: `tests/hook_shell_e2e.rs` — invokes the local OS
//!     shell so we know `cmd.exe` / `sh` actually parses our
//!     `build_curl_command` output.

use std::net::TcpListener;
use std::time::Duration;

use petpet::event::{ActivityEvent, ActivityKind, ProviderId};
use petpet::hooks::install::PETPET_MARKER;
use petpet::hooks::{ActivitySink, HookServer};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ─── Fixtures ──────────────────────────────────────────────────────

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

async fn spawn_hook_server(
    port: u16,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<ActivityEvent>,
) {
    let (tx, rx) = mpsc::channel(64);
    let sink = ActivitySink::new(tx);
    let server = HookServer::with_port(port, sink);

    let shutdown = CancellationToken::new();
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            if let Err(e) = server.run(shutdown).await {
                eprintln!("hook server exited: {e}");
            }
        }
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    for _ in 0..40 {
        if client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return (shutdown, handle, rx);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("hook server did not become ready in time");
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

async fn recv_event(rx: &mut mpsc::Receiver<ActivityEvent>, deadline: Duration) -> ActivityEvent {
    tokio::time::timeout(deadline, rx.recv())
        .await
        .unwrap_or_else(|_| panic!("no event within {deadline:?}"))
        .expect("activity channel closed")
}

// ─── Routing tests ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_returns_ok() {
    let port = pick_free_port();
    let (shutdown, handle, _rx) = spawn_hook_server(port).await;

    let body = client()
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "ok");

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_routing_produces_claude_provider() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let resp = client()
        .post(format!("http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit"))
        .header("content-type", "application/json")
        .body(r#"{"prompt":"hi"}"#)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let ev = recv_event(&mut rx, Duration::from_secs(1)).await;
    assert_eq!(ev.provider, ProviderId::ClaudeCode);
    assert!(matches!(ev.kind, ActivityKind::UserPromptSubmit { .. }));

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_routing_produces_codex_provider() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let resp = client()
        .post(format!("http://127.0.0.1:{port}/hooks/codex/PreToolUse"))
        .header("content-type", "application/json")
        .body(r#"{"tool_name":"bash"}"#)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let ev = recv_event(&mut rx, Duration::from_secs(1)).await;
    assert_eq!(ev.provider, ProviderId::Codex);

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_routing_produces_opencode_provider() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let resp = client()
        .post(format!("http://127.0.0.1:{port}/hooks/opencode/SessionStart"))
        .header("content-type", "application/json")
        .body(r#"{"source":"x"}"#)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let ev = recv_event(&mut rx, Duration::from_secs(1)).await;
    assert_eq!(ev.provider, ProviderId::OpenCode);

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_provider_returns_404() {
    let port = pick_free_port();
    let (shutdown, handle, _rx) = spawn_hook_server(port).await;

    let resp = client()
        .post(format!("http://127.0.0.1:{port}/hooks/notarealprovider/Foo"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    shutdown.cancel();
    let _ = handle.await;
}

// ─── Marker / port compatibility ───────────────────────────────────

/// The marker arrives encoded as a query param in real installs
/// (`?_=petpet-managed`). Axum's path extractor must not be tripped
/// up by that. Without this, the server might 404 valid hook requests
/// from a recent install.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_marker_query_param_survives_routing() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let url = format!(
        "http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit?_={PETPET_MARKER}"
    );
    let resp = client()
        .post(&url)
        .header("content-type", "application/json")
        .body(r#"{"prompt":"with marker"}"#)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "got {}", resp.status());

    let ev = recv_event(&mut rx, Duration::from_secs(1)).await;
    assert_eq!(ev.provider, ProviderId::ClaudeCode);

    shutdown.cancel();
    let _ = handle.await;
}

// ─── Robustness — malformed inputs ─────────────────────────────────

/// A malformed JSON body should not crash the server. axum's Json
/// extractor rejects with 4xx; the connection must stay healthy for
/// the next valid hook. Real hosts have been observed to send empty
/// bodies on synthetic / cancelled events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_body_returns_4xx_and_server_still_serves() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let bad = client()
        .post(format!("http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit"))
        .header("content-type", "application/json")
        .body("not json at all")
        .send()
        .await
        .unwrap();
    assert!(
        bad.status().is_client_error(),
        "expected 4xx for malformed body, got {}",
        bad.status()
    );

    // Now the server must still accept a valid request.
    let good = client()
        .post(format!("http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit"))
        .header("content-type", "application/json")
        .body(r#"{"prompt":"recovery"}"#)
        .send()
        .await
        .unwrap();
    assert!(good.status().is_success());

    let ev = recv_event(&mut rx, Duration::from_secs(1)).await;
    assert_eq!(ev.provider, ProviderId::ClaudeCode);

    shutdown.cancel();
    let _ = handle.await;
}

// ─── Concurrency ───────────────────────────────────────────────────

/// Real Claude Code sessions can fire several hooks per second
/// (PreToolUse + PostToolUse + Stop in tight loops). Verify the server
/// handles concurrent deliveries without dropping any.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_hooks_all_delivered() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    const N: usize = 20;
    let client = client();
    let mut sends = Vec::with_capacity(N);
    for i in 0..N {
        let client = client.clone();
        let url = format!("http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit");
        let body = format!(r#"{{"prompt":"req-{i}"}}"#);
        sends.push(tokio::spawn(async move {
            client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
        }));
    }
    for s in sends {
        let resp = s.await.unwrap().unwrap();
        assert!(resp.status().is_success());
    }

    // All N events must arrive — channel capacity is 64, well above N,
    // so nothing should be dropped.
    let mut received = Vec::with_capacity(N);
    for _ in 0..N {
        received.push(recv_event(&mut rx, Duration::from_secs(2)).await);
    }
    assert_eq!(received.len(), N);
    for ev in &received {
        assert_eq!(ev.provider, ProviderId::ClaudeCode);
        assert!(matches!(ev.kind, ActivityKind::UserPromptSubmit { .. }));
    }

    shutdown.cancel();
    let _ = handle.await;
}
