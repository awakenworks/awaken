//! The `memoryd` role: the memory-store sidecar (ADR-0053).
//!
//! A pod-local sidecar that projects a durable, path-addressed memory store into a
//! shared volume the agent container reads as plain files. Two realizations, chosen
//! by `AWAKEN_MEMORY_MODE`:
//!   - `fuse` — a live write-through FUSE mount at the shared path (needs `/dev/fuse`
//!     + `SYS_ADMIN`; the k8s `pod_plan` grants that only to this minimal sidecar,
//!     never the agent). Edits are lazy-read and CAS-flushed back to the store.
//!   - `copy` (default) — materialize the store to files at startup, then harvest the
//!     agent's edits back on teardown. Portable: works on a node without `/dev/fuse`.
//!
//! The store is a store-owned sqlite database under `AWAKEN_MEMORY_STORE_DIR`; mount
//! that dir on a PVC for durability across pod restarts, or leave it pod-local
//! (emptyDir) for an ephemeral scratch store. The env contract mirrors exactly what
//! `awaken-sandbox-container`'s `pod_plan` sets on the `memoryd-<i>` sidecar:
//! `AWAKEN_MEMORY_STORE_ID`, `AWAKEN_MOUNT_PATH`, `AWAKEN_MEMORY_MODE`.
//! Local process supervisors may additionally set
//! `AWAKEN_MEMORY_SHUTDOWN_ON_STDIN_EOF=1` for a portable graceful-stop channel.

use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
use awaken_sandbox_memoryd::{fuse_available, harvest, materialize};

/// The sidecar configuration read from the pod env (set by the k8s `pod_plan`).
pub struct MemorydConfig {
    /// The store namespace this sidecar serves (`AWAKEN_MEMORY_STORE_ID`).
    pub store_id: String,
    /// The shared volume the store is projected into (`AWAKEN_MOUNT_PATH`).
    pub mount_path: PathBuf,
    /// The durable sqlite backing dir (`AWAKEN_MEMORY_STORE_DIR`); a PVC for
    /// cross-restart durability, or a pod-local emptyDir for an ephemeral store.
    pub store_dir: PathBuf,
    /// Whether a live FUSE mount was requested (`AWAKEN_MEMORY_MODE=fuse`); the copy
    /// fallback runs when it is unset or when `/dev/fuse` is unavailable.
    pub want_fuse: bool,
}

impl MemorydConfig {
    /// Read the sidecar env, failing closed on the two required keys.
    pub fn from_env() -> Result<Self, String> {
        let store_id = std::env::var("AWAKEN_MEMORY_STORE_ID")
            .map_err(|_| "AWAKEN_MEMORY_STORE_ID is required".to_string())?;
        let mount_path = std::env::var("AWAKEN_MOUNT_PATH")
            .map_err(|_| "AWAKEN_MOUNT_PATH is required".to_string())?;
        let store_dir = std::env::var("AWAKEN_MEMORY_STORE_DIR")
            .unwrap_or_else(|_| "/var/lib/awaken/memory".to_string());
        Ok(Self {
            store_id,
            mount_path: mount_path.into(),
            store_dir: store_dir.into(),
            want_fuse: std::env::var("AWAKEN_MEMORY_MODE").as_deref() == Ok("fuse"),
        })
    }
}

/// The role entrypoint: read the env, open the store, and serve until a stop signal.
pub async fn run(_args: &[String]) -> ExitCode {
    let cfg = match MemorydConfig::from_env() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("awaken-sandbox memoryd: {msg}");
            return ExitCode::from(2);
        }
    };
    match serve(&cfg, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("awaken-sandbox memoryd: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Open the store-owned sqlite database, ensure both the backing dir and the mount
/// point exist, then run the requested realization until `shutdown` resolves.
pub async fn serve(cfg: &MemorydConfig, shutdown: impl Future<Output = ()>) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.store_dir)
        .map_err(|e| format!("create store dir {}: {e}", cfg.store_dir.display()))?;
    std::fs::create_dir_all(&cfg.mount_path)
        .map_err(|e| format!("create mount path {}: {e}", cfg.mount_path.display()))?;
    let db = cfg.store_dir.join("memory.db");
    let db = db
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 store path {}", db.display()))?;
    let fs: Arc<dyn MemoryRepository> = Arc::new(
        SqliteMemoryRepository::open(db).map_err(|e| format!("open memory store {db}: {e}"))?,
    );

    // FUSE only when both requested AND the node exposes it; otherwise fall back to the
    // portable copy path (never silently mount nothing, never hard-fail a locked node).
    let fuse_available = fuse_available();
    if cfg.want_fuse && !fuse_available {
        eprintln!(
            "awaken-sandbox memoryd: FUSE requested but /dev/fuse + fusermount are unavailable — \
             falling back to copy mode"
        );
    }
    if should_serve_fuse(cfg.want_fuse, fuse_available) {
        serve_fuse(fs, &cfg.store_id, cfg.mount_path.clone(), shutdown).await
    } else {
        serve_copy(fs.as_ref(), &cfg.store_id, &cfg.mount_path, shutdown).await
    }
}

fn should_serve_fuse(want_fuse: bool, fuse_available: bool) -> bool {
    want_fuse && fuse_available
}

/// FUSE realization: mount the store live at `mount_path`, hold until `shutdown`,
/// then drain open fds and unmount.
#[cfg(target_os = "linux")]
async fn serve_fuse(
    fs: Arc<dyn MemoryRepository>,
    store_id: &str,
    mount_path: PathBuf,
    shutdown: impl Future<Output = ()>,
) -> Result<(), String> {
    let handle = awaken_sandbox_memoryd::spawn_mount(fs, store_id.to_string(), mount_path.clone())
        .map_err(|e| {
            format!(
                "FUSE-mount store {store_id} at {}: {e}",
                mount_path.display()
            )
        })?;
    eprintln!(
        "awaken-sandbox memoryd: FUSE-mounted store {store_id} at {} (write-through)",
        mount_path.display()
    );
    shutdown.await;
    // Drains open fds up to a bounded timeout, then unmounts.
    handle.unmount();
    eprintln!("awaken-sandbox memoryd: unmounted store {store_id}");
    Ok(())
}

// The copy realization is deliberately portable. Keep a non-Linux definition so
// enabling the `memoryd` feature never requires Linux-only exports merely to build;
// `fuse_available()` makes this branch unreachable through `serve`.
#[cfg(not(target_os = "linux"))]
async fn serve_fuse(
    _fs: Arc<dyn MemoryRepository>,
    _store_id: &str,
    _mount_path: PathBuf,
    _shutdown: impl Future<Output = ()>,
) -> Result<(), String> {
    Err("FUSE realization is unavailable on this platform".to_string())
}

/// Copy realization: materialize the store to files, hold until `shutdown`, then
/// harvest the agent's edits back. `pub` so the round-trip is unit-testable without a
/// process or `/dev/fuse`.
pub async fn serve_copy(
    fs: &dyn MemoryRepository,
    store_id: &str,
    mount_path: &std::path::Path,
    shutdown: impl Future<Output = ()>,
) -> Result<(), String> {
    let mut snapshot = materialize(fs, store_id, mount_path)
        .await
        .map_err(|e| format!("materialize store {store_id}: {e}"))?;
    let written = snapshot.len();
    eprintln!(
        "awaken-sandbox memoryd: materialized {written} memories of store {store_id} to {} (copy mode)",
        mount_path.display()
    );
    shutdown.await;
    let report = harvest(fs, store_id, mount_path, &mut snapshot)
        .await
        .map_err(|e| format!("harvest store {store_id}: {e}"))?;
    eprintln!(
        "awaken-sandbox memoryd: harvested {} changed memories back to store {store_id}",
        report.changed
    );
    if !report.conflicts.is_empty() {
        eprintln!(
            "awaken-sandbox memoryd: preserved {} concurrent durable heads: {:?}",
            report.conflicts.len(),
            report.conflicts
        );
    }
    Ok(())
}

/// Resolve on SIGTERM (the orchestrator's graceful stop, sent before SIGKILL) or
/// SIGINT (a developer's foreground stop), so the copy harvest / FUSE unmount runs
/// before exit rather than being lost to an abrupt kill.
async fn shutdown_signal() {
    if std::env::var("AWAKEN_MEMORY_SHUTDOWN_ON_STDIN_EOF").as_deref() == Ok("1") {
        use tokio::io::{AsyncReadExt, stdin};

        let mut byte = [0_u8; 1];
        let _ = stdin().read(&mut byte).await;
        return;
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::should_serve_fuse;

    #[test]
    fn fuse_selection_cause_graph_decision_table() {
        // Cause graph: C1 FUSE requested, C2 FUSE available. Only C1 AND C2
        // selects the live mount; every other rule must select portable copy.
        //
        // | Rule | C1 requested | C2 available | Result |
        // |---|---|---|---|
        // | M1 | F | F | copy |
        // | M2 | F | T | copy |
        // | M3 | T | F | copy + fallback diagnostic |
        // | M4 | T | T | FUSE |
        assert!(!should_serve_fuse(false, false), "M1");
        assert!(!should_serve_fuse(false, true), "M2");
        assert!(!should_serve_fuse(true, false), "M3");
        assert!(should_serve_fuse(true, true), "M4");
    }
}
