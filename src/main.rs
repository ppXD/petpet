//! petpet CLI — backfill / watch / stats.
//!
//! This is the Layer 2 ingestion harness. Tauri scaffolding will wrap the
//! same library later; for now the CLI verifies parsers against real data.

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use petpet::{
    db::{writer::spawn_writer, DbHandle},
    hooks::{ActivitySink, HookServer},
    paths,
    provider::{
        aider::AiderProvider, claude::ClaudeCodeProvider, codex::CodexProvider,
        opencode::OpenCodeProvider, EventSink, Provider,
    },
    event::ActivityEvent,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "petpet", about = "Desktop pet event ingestion daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Catch up from each file's cursor to current EOF (incremental, not
    /// retroactive). First-seen files snap to EOF without emitting — that's
    /// the install-time boundary; the pet only experiences appends from now on.
    Backfill {
        /// Only run a single provider. Repeat or omit to run all.
        #[arg(long, value_enum)]
        only: Option<ProviderArg>,
    },
    /// Backfill, then keep watching forever until Ctrl-C.
    Watch {
        #[arg(long, value_enum)]
        only: Option<ProviderArg>,
    },
    /// Print aggregate token usage grouped by provider+model.
    Stats,
    /// Print where petpet reads from and writes to.
    Where,
    /// Wipe events + cursors. Equivalent to deleting `~/.petpet/petpet.db`.
    /// Use this to start the pet over from zero.
    Reset {
        /// Confirm the destructive action. Required.
        #[arg(long)]
        yes: bool,
    },
    /// Layer 1 hook server operations.
    Hooks {
        #[command(subcommand)]
        cmd: HookCmd,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Run a standalone hook server (no desktop app). Useful for dry-run.
    Serve {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Install petpet hook entries into every supported provider's config.
    /// Idempotent — safe to run repeatedly.
    Install {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Remove petpet-managed hook entries from every provider's config.
    Uninstall,
    /// Report which hook entries are currently installed where.
    Status,
    /// Print the manual snippet for `~/.claude/settings.json` (in case
    /// the user wants to wire by hand instead of `install`).
    Snippet {
        #[arg(long)]
        port: Option<u16>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProviderArg {
    Claude,
    Codex,
    Opencode,
    Aider,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,petpet=debug")))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let db = DbHandle::open(&paths::db_path()).await?;

    match cli.cmd {
        Cmd::Where => {
            println!("app_dir              : {}", paths::app_dir().display());
            println!("db                   : {}", paths::db_path().display());
            println!("claude projects root : {}", paths::claude_projects_root().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()));
            println!("codex sessions root  : {}", paths::codex_sessions_root().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()));
        }
        Cmd::Stats => {
            let rows = db.stats_summary().await?;
            if rows.is_empty() {
                println!("no events yet. run `petpet backfill` first.");
            } else {
                print_stats(&rows);
            }
        }
        Cmd::Backfill { only } => run_backfill(db, only).await?,
        Cmd::Watch { only } => run_watch(db, only).await?,
        Cmd::Reset { yes } => {
            if !yes {
                eprintln!("This will delete all events and cursors. Re-run with --yes to confirm.");
                std::process::exit(1);
            }
            drop(db);
            let path = paths::db_path();
            // Also clean WAL/SHM sidecars so SQLite rebuilds cleanly.
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let p = path.with_file_name(format!(
                    "{}{}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    suffix
                ));
                if p.exists() {
                    std::fs::remove_file(&p)?;
                    println!("removed {}", p.display());
                }
            }
            println!("petpet reset — next run will start the pet from zero.");
        }
        Cmd::Hooks { cmd } => match cmd {
            HookCmd::Serve { port } => run_hook_server(port).await?,
            HookCmd::Snippet { port } => print_hook_snippet(port),
            HookCmd::Install { port } => run_hooks_install(port),
            HookCmd::Uninstall => run_hooks_uninstall(),
            HookCmd::Status => run_hooks_status(),
        },
    }
    Ok(())
}

fn run_hooks_install(port: Option<u16>) {
    let port = port.unwrap_or(petpet::DEFAULT_HOOK_PORT);
    let reports = petpet::hooks::install_all(port);
    for r in reports {
        let path = r
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".into());
        println!(
            "[{}] {}   path: {}   strategy: {}",
            r.provider,
            if r.is_ok() { "ok " } else { "ERR" },
            path,
            r.strategy.as_deref().unwrap_or("—")
        );
        for action in &r.preflight_actions {
            println!("  preflight       : {action}");
        }
        if !r.installed.is_empty() {
            println!("  installed       : {}", r.installed.join(", "));
        }
        if !r.updated.is_empty() {
            println!("  updated         : {}", r.updated.join(", "));
        }
        if !r.already_present.is_empty() {
            println!("  already present : {}", r.already_present.join(", "));
        }
        for b in &r.backups {
            println!("  backup          : {}", b.display());
        }
        for w in &r.warnings {
            println!("  warn            : {w}");
        }
        if let Some(err) = &r.error {
            println!("  error           : {err}");
        }
    }
    println!("\nRestart any running Claude Code / Codex session for hooks to take effect.");
}

fn run_hooks_uninstall() {
    let reports = petpet::hooks::uninstall_all();
    for r in reports {
        let path = r
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".into());
        println!("[{}] path: {}", r.provider, path);
        if r.removed.is_empty() {
            println!("  (nothing to remove)");
        } else {
            println!("  removed: {}", r.removed.join(", "));
        }
        if let Some(err) = &r.error {
            println!("  error  : {err}");
        }
    }
}

fn run_hooks_status() {
    let reports = petpet::hooks::status_all();
    for r in reports {
        let path = r
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".into());
        let port = r
            .installed_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "—".into());
        println!(
            "[{}] config: {}   exists: {}   port: {}   events: [{}]",
            r.provider,
            path,
            r.config_exists,
            port,
            r.installed_events.join(", ")
        );
    }
}

async fn run_hook_server(port: Option<u16>) -> Result<()> {
    if let Some(p) = port {
        std::env::set_var("PETPET_HOOK_PORT", p.to_string());
    }
    let (tx, mut rx) = mpsc::channel::<ActivityEvent>(1024);
    let sink = ActivitySink::new(tx);
    let server = HookServer::new(sink);
    let port = server.port();

    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server_task = tokio::spawn(async move { server.run(server_shutdown).await });

    let print_shutdown = shutdown.clone();
    let print_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = print_shutdown.cancelled() => break,
                Some(ev) = rx.recv() => {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    println!("{json}");
                }
                else => break,
            }
        }
    });

    println!("listening on http://127.0.0.1:{port}  (Ctrl-C to stop)");
    println!("try: curl -fsS -X POST http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit -H 'Content-Type: application/json' -d '{{\"session_id\":\"demo\",\"cwd\":\"/tmp\",\"prompt\":\"hi\"}}'");
    tokio::signal::ctrl_c().await?;
    shutdown.cancel();
    let _ = server_task.await;
    let _ = print_task.await;
    Ok(())
}

fn print_hook_snippet(port: Option<u16>) {
    let port = port.unwrap_or(petpet::DEFAULT_HOOK_PORT);
    let snippet = format!(r#"{{
  "hooks": {{
    "UserPromptSubmit": [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/UserPromptSubmit"}}]}}],
    "PreToolUse":       [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/PreToolUse"}}]}}],
    "PostToolUse":      [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/PostToolUse"}}]}}],
    "Stop":             [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/Stop"}}]}}],
    "SessionStart":     [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/SessionStart"}}]}}],
    "SessionEnd":       [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/SessionEnd"}}]}}],
    "Notification":     [{{"matcher":"","hooks":[{{"type":"command","command":"curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @- http://127.0.0.1:{port}/hooks/claude/Notification"}}]}}]
  }}
}}"#);
    println!("# Merge this into ~/.claude/settings.json (top-level \"hooks\" key).");
    println!("# Then restart any Claude Code session to load.");
    println!();
    println!("{snippet}");
}

fn build_providers(db: Arc<DbHandle>, only: Option<ProviderArg>) -> Vec<Box<dyn Provider>> {
    let mut out: Vec<Box<dyn Provider>> = Vec::new();
    if matches!(only, None | Some(ProviderArg::Claude)) {
        out.push(Box::new(ClaudeCodeProvider::new(db.clone())));
    }
    if matches!(only, None | Some(ProviderArg::Codex)) {
        out.push(Box::new(CodexProvider::new(db.clone())));
    }
    if matches!(only, None | Some(ProviderArg::Opencode)) {
        out.push(Box::new(OpenCodeProvider::new(db.clone())));
    }
    if matches!(only, None | Some(ProviderArg::Aider)) {
        out.push(Box::new(AiderProvider::new(db.clone())));
    }
    out
}

async fn run_backfill(db: Arc<DbHandle>, only: Option<ProviderArg>) -> Result<()> {
    let (tx, rx) = mpsc::channel(8192);
    let sink = EventSink::new(tx);
    let writer_shutdown = CancellationToken::new();
    let writer = spawn_writer(db.clone(), rx, writer_shutdown.clone());

    let providers = build_providers(db.clone(), only);
    for p in &providers {
        tracing::info!(provider = %p.id(), "starting backfill");
        let stats = p.backfill(&sink).await?;
        println!(
            "[{}] events={} lines={} files={} bytes={} elapsed={}ms",
            p.display_name(),
            stats.events_emitted,
            stats.lines_scanned,
            stats.files_scanned,
            stats.bytes_scanned,
            stats.duration.as_millis()
        );
    }
    drop(sink);
    let writer_stats = writer.await??;
    println!(
        "writer: inserted={} deduped={} failed={}",
        writer_stats.inserted, writer_stats.deduped, writer_stats.failed
    );

    let rows = db.stats_summary().await?;
    print_stats(&rows);
    Ok(())
}

async fn run_watch(db: Arc<DbHandle>, only: Option<ProviderArg>) -> Result<()> {
    let (tx, rx) = mpsc::channel(8192);
    let sink = EventSink::new(tx);
    let (act_tx, mut act_rx) = mpsc::channel::<ActivityEvent>(1024);
    let activity_sink = ActivitySink::new(act_tx);
    let shutdown = CancellationToken::new();
    let writer = spawn_writer(db.clone(), rx, shutdown.clone());

    // CLI mode: just print activity events to stdout for visibility.
    let act_shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = act_shutdown.cancelled() => break,
                Some(ev) = act_rx.recv() => {
                    if let Ok(json) = serde_json::to_string(&ev) {
                        println!("activity: {json}");
                    }
                }
                else => break,
            }
        }
    });

    let providers = build_providers(db.clone(), only);
    let mut handles = Vec::new();
    for p in providers {
        let sink = sink.clone();
        let act_sink = activity_sink.clone();
        let token = shutdown.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = p.watch(&sink, &act_sink, token).await {
                tracing::error!(provider = %p.id(), error = %e, "watch terminated");
            }
        }));
    }

    drop(sink);
    drop(activity_sink);
    tokio::signal::ctrl_c().await?;
    tracing::info!("ctrl-c received, shutting down");
    shutdown.cancel();
    for h in handles {
        let _ = h.await;
    }
    let writer_stats = writer.await??;
    println!(
        "writer: inserted={} deduped={} failed={}",
        writer_stats.inserted, writer_stats.deduped, writer_stats.failed
    );
    Ok(())
}

fn print_stats(rows: &[petpet::db::StatsRow]) {
    println!(
        "\n{:<14} {:<28} {:>10} {:>12} {:>10} {:>12} {:>14} {:>12}",
        "provider", "model", "events", "input", "output", "cache_read", "cache_create", "reasoning"
    );
    println!("{}", "-".repeat(112));
    for r in rows {
        println!(
            "{:<14} {:<28} {:>10} {:>12} {:>10} {:>12} {:>14} {:>12}",
            r.provider, r.model, r.events, r.input, r.output, r.cache_read, r.cache_creation, r.reasoning
        );
    }
}
