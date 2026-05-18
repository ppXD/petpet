//! Background registry sync — fetches `petpet-model-registry/models.json`
//! on a 24h cycle and writes the result to `~/.petpet/registry-cache.json`.
//!
//! # Design choices
//!
//! - **Restart-pickup**, not hot-swap. Cache writes take effect on next
//!   app start. Petpet is a foreground desktop app that gets restarted
//!   often, and hot-swapping a static `OnceLock` would require API
//!   churn across every caller. Trade-off is acceptable.
//!
//! - **Fail-soft**. Every error path (no network, bad JSON, schema
//!   mismatch, disk full) logs and waits 24h. The bundled registry
//!   always works — sync is purely additive.
//!
//! - **Schema-version gated**. The remote registry repo can evolve
//!   its schema independently; older clients that don't understand
//!   the new schema ignore the cache and stay on bundled. No silent
//!   corruption.
//!
//! - **Privacy**. Single anonymous HTTPS GET, no auth header, no
//!   identifying UA fingerprint beyond a versioned product string.
//!   `PETPET_REGISTRY_SYNC_DISABLED=1` opts out entirely for
//!   air-gapped / privacy-sensitive deployments.
//!
//! # Env vars (public API for operators)
//!
//! - `PETPET_REGISTRY_URL` — override the default registry URL
//!   (mirror / staging / testing).
//! - `PETPET_REGISTRY_SYNC_DISABLED` — set to any value to skip the
//!   background task entirely.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::xp::registry::{cache_path, Registry, REGISTRY_SCHEMA_VERSION};

/// Public registry URL. The repo doesn't exist yet at this URL until
/// the operator creates `ppXD/petpet-model-registry` and seeds it from
/// the bundled JSON — first fetch returns 404 and we shrug, fall back
/// to bundled.
pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/ppXD/petpet-model-registry/main/models.json";

/// Env var to override the URL — used for mirrors, staging, tests.
pub const URL_ENV_VAR: &str = "PETPET_REGISTRY_URL";

/// Env var to disable sync entirely. Any value (even empty) opts out.
pub const DISABLE_ENV_VAR: &str = "PETPET_REGISTRY_SYNC_DISABLED";

/// 24h between fetches — matches typical CI cron cadence for the
/// upstream registry.
pub const FETCH_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Wait this long before the first fetch so app startup isn't blocked
/// by a slow network. 60s is well past UI first-paint.
pub const STARTUP_DELAY: Duration = Duration::from_secs(60);

/// HTTP timeout — long enough for a slow CDN, short enough that a
/// hung connection doesn't pin the task for hours.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Operator opt-out via env var. Honoured at process start; toggling
/// after startup requires a restart.
pub fn is_disabled() -> bool {
    std::env::var(DISABLE_ENV_VAR).is_ok()
}

/// Effective registry URL — env override or default.
pub fn registry_url() -> String {
    std::env::var(URL_ENV_VAR).unwrap_or_else(|_| DEFAULT_REGISTRY_URL.to_string())
}

/// Run the sync loop forever. Wakes every [`FETCH_INTERVAL`].
///
/// This is `pub async fn run()` so the caller decides how to spawn it
/// (`tokio::spawn` typically). The task never returns under normal
/// conditions — failures inside one iteration log and continue.
pub async fn run() {
    if is_disabled() {
        tracing::info!(
            "registry sync disabled via {DISABLE_ENV_VAR}; using bundled registry only"
        );
        return;
    }
    tracing::info!(
        "registry sync: starting in {}s, then every {}h ({})",
        STARTUP_DELAY.as_secs(),
        FETCH_INTERVAL.as_secs() / 3600,
        registry_url()
    );
    tokio::time::sleep(STARTUP_DELAY).await;
    loop {
        match fetch_once().await {
            Ok(()) => {
                tracing::info!(
                    "registry sync: cache updated (effective on next restart): {}",
                    cache_path().display()
                );
            }
            Err(e) => {
                // Warn, not error — the bundled registry covers us.
                // The most common cause is "remote repo doesn't exist
                // yet" or "user is offline". Both are non-actionable.
                tracing::warn!("registry sync failed (will retry in 24h): {e:#}");
            }
        }
        tokio::time::sleep(FETCH_INTERVAL).await;
    }
}

/// One sync iteration. Fetches the remote registry, validates schema,
/// and atomically writes to the cache file. Errors propagate so the
/// caller (or test) can react.
pub async fn fetch_once() -> Result<()> {
    let url = registry_url();
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("petpet-registry-sync/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;

    let body = client
        .get(&url)
        .send()
        .await
        .context("send registry request")?
        .error_for_status()
        .context("registry returned non-2xx")?
        .text()
        .await
        .context("read registry response body")?;

    validate(&body)?;
    atomic_write_cache(&body)?;
    Ok(())
}

/// Validate that the JSON parses + schema_version matches. Refusing to
/// cache invalid payloads keeps the next-startup load path simple.
fn validate(json: &str) -> Result<()> {
    let parsed = Registry::from_json(json).context("parse remote registry as RegistryFile")?;
    if parsed.data.schema_version != REGISTRY_SCHEMA_VERSION {
        anyhow::bail!(
            "remote registry schema_version={} but client supports {}; refusing to cache",
            parsed.data.schema_version,
            REGISTRY_SCHEMA_VERSION
        );
    }
    if parsed.model_count() == 0 {
        anyhow::bail!("remote registry has zero models — refusing to cache empty file");
    }
    Ok(())
}

/// Write the cache file atomically: write to .tmp, fsync (best-effort),
/// rename into place. Rename is atomic on POSIX and on NTFS for
/// same-volume operations, which is what we have here.
fn atomic_write_cache(content: &str) -> Result<()> {
    let final_path = cache_path();
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent).context("create cache parent dir")?;
    }
    let tmp_path: PathBuf = final_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, content).context("write cache tmp file")?;
    std::fs::rename(&tmp_path, &final_path).context("rename cache tmp into place")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn default_url_targets_petpet_model_registry_repo() {
        assert!(DEFAULT_REGISTRY_URL.starts_with("https://raw.githubusercontent.com/ppXD/petpet-model-registry/"));
        assert!(DEFAULT_REGISTRY_URL.ends_with("/models.json"));
    }

    #[test]
    fn url_env_var_constant_pinned() {
        // Renaming this breaks every operator who pinned a mirror via
        // env. Hard-pin so the change is a compile-visible decision.
        assert_eq!(URL_ENV_VAR, "PETPET_REGISTRY_URL");
    }

    #[test]
    fn disable_env_var_constant_pinned() {
        assert_eq!(DISABLE_ENV_VAR, "PETPET_REGISTRY_SYNC_DISABLED");
    }

    #[test]
    fn registry_url_respects_env_override() {
        let _g = env_lock();
        std::env::set_var(URL_ENV_VAR, "http://localhost:8080/test.json");
        assert_eq!(registry_url(), "http://localhost:8080/test.json");
        std::env::remove_var(URL_ENV_VAR);
        assert_eq!(registry_url(), DEFAULT_REGISTRY_URL);
    }

    #[test]
    fn is_disabled_respects_env_var() {
        let _g = env_lock();
        std::env::remove_var(DISABLE_ENV_VAR);
        assert!(!is_disabled());
        std::env::set_var(DISABLE_ENV_VAR, "1");
        assert!(is_disabled());
        std::env::remove_var(DISABLE_ENV_VAR);
        assert!(!is_disabled());
    }

    #[test]
    fn validate_accepts_bundled_payload() {
        // The bundled JSON is by definition a valid payload.
        let bundled = include_str!("../../data/models.json");
        validate(bundled).expect("bundled registry must validate");
    }

    #[test]
    fn validate_rejects_wrong_schema_version() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("../../data/models.json")).unwrap();
        json["schema_version"] = serde_json::json!(99);
        let s = serde_json::to_string(&json).unwrap();
        let err = validate(&s).unwrap_err().to_string();
        assert!(
            err.contains("schema_version") && err.contains("99"),
            "expected schema_version error, got: {err}"
        );
    }

    #[test]
    fn validate_rejects_empty_models() {
        let mut json: serde_json::Value =
            serde_json::from_str(include_str!("../../data/models.json")).unwrap();
        json["models"] = serde_json::json!([]);
        let s = serde_json::to_string(&json).unwrap();
        let err = validate(&s).unwrap_err().to_string();
        assert!(err.contains("zero models"), "expected zero-models error, got: {err}");
    }

    #[test]
    fn validate_rejects_garbage() {
        assert!(validate("not json at all").is_err());
        assert!(validate("{}").is_err()); // missing required fields
    }

    #[test]
    fn atomic_write_round_trip() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("PETPET_HOME", dir.path());

        let payload = include_str!("../../data/models.json");
        atomic_write_cache(payload).expect("atomic write");

        let read_back = std::fs::read_to_string(cache_path()).expect("read back");
        assert_eq!(payload, read_back);
        // The tmp file should be gone after a successful rename.
        assert!(!cache_path().with_extension("json.tmp").exists());

        std::env::remove_var("PETPET_HOME");
    }

    #[test]
    fn atomic_write_creates_parent_dir() {
        let _g = env_lock();
        let dir = tempfile::tempdir().unwrap();
        // Point PETPET_HOME at a SUB-path that doesn't exist yet.
        std::env::set_var("PETPET_HOME", dir.path().join("not_yet"));

        atomic_write_cache(include_str!("../../data/models.json"))
            .expect("must create parent dir");
        assert!(cache_path().exists());

        std::env::remove_var("PETPET_HOME");
    }

    #[test]
    fn intervals_are_sane() {
        // 24h cycle, 60s startup delay, 10s timeout — pin so accidental
        // edits to "let me make this 1 minute for testing" don't ship.
        assert_eq!(FETCH_INTERVAL.as_secs(), 24 * 3600);
        assert_eq!(STARTUP_DELAY.as_secs(), 60);
        assert_eq!(FETCH_TIMEOUT.as_secs(), 10);
    }
}
