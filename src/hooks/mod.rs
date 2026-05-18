//! Layer 1 — HTTP hook server.
//!
//! Local-only HTTP receiver that AI CLIs POST to from their hook scripts.
//! Each request becomes an [`ActivityEvent`] flowing through [`ActivitySink`]
//! into the desktop app's Tauri event bus (no DB persistence — see module
//! doc in `event.rs`).
//!
//! Endpoints:
//!   POST /hooks/claude/{event}   ← Claude Code hook schema
//!   POST /hooks/codex/{event}    ← Codex CLI hook schema
//!   GET  /healthz                ← readiness probe (returns "ok")
//!
//! Port defaults to 43117. Override with `PETPET_HOOK_PORT`.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{ActivityEvent, ProviderId};

pub mod install;
pub mod parsers;

pub use install::{
    ensure_installed, install_all, status_all, uninstall_all, HookInstaller, InstallReport,
    InstallStatus, UninstallReport,
};

pub const DEFAULT_HOOK_PORT: u16 = 43117;

#[derive(Clone)]
pub struct ActivitySink {
    tx: mpsc::Sender<ActivityEvent>,
}

impl ActivitySink {
    pub fn new(tx: mpsc::Sender<ActivityEvent>) -> Self {
        Self { tx }
    }

    pub async fn emit(&self, ev: ActivityEvent) {
        if let Err(e) = self.tx.send(ev).await {
            tracing::warn!(error = %e, "activity sink closed");
        }
    }
}

pub struct HookServer {
    port: u16,
    sink: ActivitySink,
}

impl HookServer {
    pub fn new(sink: ActivitySink) -> Self {
        let port = std::env::var("PETPET_HOOK_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_HOOK_PORT);
        Self { port, sink }
    }

    /// Explicit-port variant — used by integration tests where each
    /// test needs its own listener and can't rely on the process-wide
    /// `PETPET_HOOK_PORT` env var (cargo runs tests in parallel; env
    /// is shared mutable state).
    pub fn with_port(port: u16, sink: ActivitySink) -> Self {
        Self { port, sink }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Bind 127.0.0.1:port and serve until `shutdown` fires.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let state = Arc::new(self.sink);

        let app = Router::new()
            .route("/healthz", get(healthz))
            // Generic route — provider slug is `claude` / `codex` / `gemini` / etc.
            // New providers automatically work by adding a `ProviderId` variant
            // and slug; no route table changes needed.
            .route("/hooks/:provider/:event", post(hook))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding hook server to {addr}"))?;
        tracing::info!(addr = %addr, "hook server listening");

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .context("hook server crashed")
    }
}

async fn healthz() -> &'static str {
    "ok"
}

/// Generic hook receiver — works for any provider whose ID maps to a slug.
/// Logs each accepted request at INFO so it shows up in `tauri dev` console
/// (visible diagnostic that hooks are actually reaching us).
async fn hook(
    State(sink): State<Arc<ActivitySink>>,
    Path((provider_slug, event)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    let Some(provider) = ProviderId::from_slug(&provider_slug) else {
        tracing::warn!(slug = %provider_slug, event = %event, "unknown provider slug — 404");
        return StatusCode::NOT_FOUND;
    };
    let ev = parsers::parse(provider, &event, &body);
    let kind = ev.kind.clone();
    tracing::info!(
        provider = %provider,
        event = %event,
        kind = ?kind,
        "hook received"
    );
    sink.emit(ev).await;
    StatusCode::OK
}
