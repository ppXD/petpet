//! **Layer 3 — Shell-level end-to-end** for the hook ingestion path.
//!
//! Scope of this file (strict): every test in here invokes the *local
//! OS shell* to execute the curl hook command. The point is to prove
//! that `build_curl_command`'s output parses correctly under whichever
//! shell the host CLI (Claude Code, Codex, OpenCode) will hand it
//! to — `/bin/sh` on Unix, `cmd.exe` on Windows. Anything provable
//! without the shell lives elsewhere:
//!
//! | layer | file | what it proves |
//! |-------|------|----------------|
//! | L1 unit | `src/hooks/install/mod.rs::build_curl_command_tests` | command-string invariants |
//! | L2 HTTP integration | `tests/hook_server.rs` | the axum server's routing, parsing, concurrency |
//! | **L3 shell E2E** | this file | the OS shell parses + executes our installed command |
//!
//! Cross-platform value: the same test source runs on macOS, Linux,
//! AND Windows via the CI matrix in `.github/workflows/test.yml`. If
//! the cross-platform refactor (single→double quotes, `#`→URL marker)
//! regressed under `cmd.exe`, every test in this file would fail on
//! the Windows runner.
//!
//! Requires `curl` on PATH. macOS, Linux, and Windows 10 1803+ all
//! ship `curl` natively; the three GitHub-Actions runners we target
//! (`ubuntu-latest`, `macos-latest`, `windows-latest`) all have it.

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use petpet::event::{ActivityEvent, ActivityKind, ProviderId};
use petpet::hooks::{install::build_curl_command, ActivitySink, HookServer};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ─── Test fixtures ─────────────────────────────────────────────────

/// Bind an ephemeral localhost port, then drop the listener so the
/// HookServer can re-bind. There's a tiny race window (TIME_WAIT etc.)
/// where the port could be snatched — acceptable for local CI runs;
/// surfaces clearly as "address already in use".
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Spawn the real HookServer on `port`. Returns: shutdown token, task
/// join handle, mpsc receiver for emitted ActivityEvents.
///
/// Uses `HookServer::with_port` (not `new`) because cargo runs
/// integration tests in parallel and `PETPET_HOOK_PORT` is
/// process-global — relying on it caused tests to bind each other's
/// ports.
async fn spawn_hook_server(
    port: u16,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<ActivityEvent>,
) {
    let (tx, rx) = mpsc::channel(32);
    let sink = ActivitySink::new(tx);
    let server = HookServer::with_port(port, sink);
    assert_eq!(server.port(), port);

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = server.run(shutdown_clone).await {
            eprintln!("hook server exited: {e}");
        }
    });

    // Wait for /healthz before letting tests fire requests. Without
    // this, the first hook would race the bind and time out.
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

/// Run `cmd` through the local OS shell, piping `stdin_body` to it.
/// On Unix this is `/bin/sh -c <cmd>`; on Windows it's `cmd /c <cmd>`.
/// **This is the entire point of the L3 layer** — every other test
/// tier short-circuits the shell.
fn exec_through_shell(cmd: &str, stdin_body: &[u8]) -> std::io::Result<std::process::Output> {
    #[cfg(unix)]
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .arg("/c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    use std::io::Write;
    child
        .stdin
        .as_mut()
        .expect("child stdin should be piped")
        .write_all(stdin_body)
        .expect("write stdin");
    // Drop stdin to signal EOF — curl waits on it before sending.
    drop(child.stdin.take());
    child.wait_with_output()
}

/// Recv next event or panic with diagnostics. Better than
/// `rx.recv().await.unwrap()` because timeout failures here usually
/// mean the shell didn't parse the curl command at all (the
/// cross-platform regression we're guarding against).
async fn recv_event(rx: &mut mpsc::Receiver<ActivityEvent>, deadline: Duration) -> ActivityEvent {
    match tokio::time::timeout(deadline, rx.recv()).await {
        Ok(Some(ev)) => ev,
        Ok(None) => panic!("activity channel closed before any event arrived"),
        Err(_) => panic!(
            "no ActivityEvent received within {:?} — the curl command likely failed under \
             the local shell. Run with --nocapture to inspect curl's stderr.",
            deadline
        ),
    }
}

/// Wrap a curl invocation in the assertion that it succeeded. Used
/// everywhere a test runs the shell; centralises the diagnostic
/// formatting.
fn run_curl_or_fail(cmd: &str, body: &[u8]) {
    let out = exec_through_shell(cmd, body).expect("spawn shell");
    assert!(
        out.status.success(),
        "curl exited non-zero:\nCMD: {cmd}\n--stdout--\n{}\n--stderr--\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ─── Tests ──────────────────────────────────────────────────────────
//
// `flavor = "multi_thread"` is mandatory on every test: the test body
// blocks on `child.wait_with_output()` (synchronous std::process), and
// under the default `current_thread` runtime that would freeze the
// HookServer task → curl timeout.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_user_prompt_submit_round_trips_through_shell() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let cmd = build_curl_command("claude", "UserPromptSubmit", port);
    run_curl_or_fail(&cmd, br#"{"prompt":"hello"}"#);

    let ev = recv_event(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(ev.provider, ProviderId::ClaudeCode);
    assert!(
        matches!(ev.kind, ActivityKind::UserPromptSubmit { .. }),
        "expected UserPromptSubmit, got: {:?}",
        ev.kind
    );

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_pre_tool_use_round_trips_through_shell() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let cmd = build_curl_command("codex", "PreToolUse", port);
    run_curl_or_fail(&cmd, br#"{"tool_name":"bash"}"#);

    let ev = recv_event(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(ev.provider, ProviderId::Codex);

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_session_start_round_trips_through_shell() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    let cmd = build_curl_command("opencode", "SessionStart", port);
    run_curl_or_fail(&cmd, br#"{"source":"e2e"}"#);

    let ev = recv_event(&mut rx, Duration::from_secs(2)).await;
    assert_eq!(ev.provider, ProviderId::OpenCode);

    shutdown.cancel();
    let _ = handle.await;
}

/// Real Claude Code / Codex sessions fire several hooks per second
/// (e.g. `PreToolUse` + `PostToolUse` around each tool call). Verify
/// the shell-issued path holds up under back-to-back invocations and
/// every event lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_shell_invocations_all_land() {
    let port = pick_free_port();
    let (shutdown, handle, mut rx) = spawn_hook_server(port).await;

    const N: usize = 5;
    // Use distinct events so a missed delivery shows up as a missing
    // kind rather than a count discrepancy alone.
    let events = ["UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SessionStart"];
    for ev in &events[..N] {
        let cmd = build_curl_command("claude", ev, port);
        run_curl_or_fail(&cmd, br#"{}"#);
    }

    let mut received = Vec::with_capacity(N);
    for _ in 0..N {
        received.push(recv_event(&mut rx, Duration::from_secs(2)).await);
    }
    assert_eq!(received.len(), N, "expected {N} events");
    for ev in &received {
        assert_eq!(ev.provider, ProviderId::ClaudeCode);
    }

    shutdown.cancel();
    let _ = handle.await;
}
