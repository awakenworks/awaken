//! The `memoryd` role: the memory-store sidecar (ADR-0053).
//!
//! A pod-local sidecar that projects a durable, path-addressed memory store into a
//! shared volume the agent container reads as plain files. Two realizations, chosen
//! by the typed `--mode` argument:
//!   - `fuse` — a live write-through FUSE mount at the shared path (needs `/dev/fuse`
//!     + `SYS_ADMIN`; the k8s `pod_plan` grants that only to this minimal sidecar,
//!     never the agent). Edits are lazy-read and CAS-flushed back to the store.
//!   - `copy` (default) — materialize the store to files at startup, then harvest the
//!     agent's edits back on teardown. Portable: works on a node without `/dev/fuse`.
//!
//! The store is a store-owned sqlite database under `--store-dir`; mount
//! that dir on a PVC for durability across pod restarts, or leave it pod-local
//! (emptyDir) for an ephemeral scratch store. The Kubernetes plan passes the same
//! explicit arguments as any local supervisor.

use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
use awaken_sandbox_memoryd::{fuse_available, harvest, materialize};

/// Explicit sidecar process configuration.
pub struct MemorydConfig {
    /// The store namespace this sidecar serves.
    pub store_id: String,
    /// The shared volume the store is projected into.
    pub mount_path: PathBuf,
    /// The durable sqlite backing dir; a PVC for
    /// cross-restart durability, or a pod-local emptyDir for an ephemeral store.
    pub store_dir: PathBuf,
    /// Whether a live FUSE mount was requested; the copy
    /// fallback runs when it is unset or when `/dev/fuse` is unavailable.
    pub want_fuse: bool,
}

impl MemorydConfig {
    /// Decode the role's explicit flag/value protocol.
    pub fn from_args(args: &[String]) -> Result<(Self, bool), String> {
        let value = |flag: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
        };
        let store_id = value("--store-id").ok_or("--store-id is required")?;
        let mount_path = value("--mount-path").ok_or("--mount-path is required")?;
        let mode = value("--mode").unwrap_or_else(|| "copy".to_owned());
        if store_id.trim().is_empty() || mount_path.trim().is_empty() {
            return Err("--store-id and --mount-path must be non-empty".to_owned());
        }
        if !matches!(mode.as_str(), "copy" | "fuse") {
            return Err("--mode must be `copy` or `fuse`".to_owned());
        }
        Ok((
            Self {
                store_id,
                mount_path: mount_path.into(),
                store_dir: value("--store-dir")
                    .unwrap_or_else(|| "/var/lib/awaken/memory".to_owned())
                    .into(),
                want_fuse: mode == "fuse",
            },
            args.iter().any(|arg| arg == "--shutdown-on-stdin-eof"),
        ))
    }
}

/// The role entrypoint: decode argv, open the store, and serve until a stop signal.
pub async fn run(_args: &[String]) -> ExitCode {
    let (cfg, shutdown_on_stdin_eof) = match MemorydConfig::from_args(_args) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("awaken-sandbox memoryd: {msg}");
            return ExitCode::from(2);
        }
    };
    match serve(&cfg, shutdown_signal(shutdown_on_stdin_eof)).await {
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
async fn shutdown_signal(shutdown_on_stdin_eof: bool) {
    if shutdown_on_stdin_eof {
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
    use super::{MemorydConfig, should_serve_fuse};

    #[test]
    fn explicit_process_protocol_is_complete_and_fail_closed() {
        // Cause/effect graph: explicit role argv -> one MemorydConfig + shutdown
        // policy. Missing/empty required values or an unknown mode prevents startup;
        // no ambient fallback can change the realized store.
        //
        // | store/mount | mode | EOF flag | result |
        // | present | absent/copy | absent | copy + signal shutdown |
        // | present | fuse | present | fuse + stdin shutdown |
        // | missing/empty | any | any | reject |
        // | present | unknown | any | reject |
        let args = ["--store-id", "s1", "--mount-path", "/memory"].map(str::to_owned);
        let (copy, eof) = MemorydConfig::from_args(&args).unwrap();
        assert_eq!(copy.store_id, "s1");
        assert!(!copy.want_fuse);
        assert!(!eof);

        let args = [
            "--store-id",
            "s1",
            "--mount-path",
            "/memory",
            "--mode",
            "fuse",
            "--shutdown-on-stdin-eof",
        ]
        .map(str::to_owned);
        let (fuse, eof) = MemorydConfig::from_args(&args).unwrap();
        assert!(fuse.want_fuse);
        assert!(eof);

        for args in [
            vec!["--store-id", "s1"],
            vec!["--store-id", "", "--mount-path", "/memory"],
            vec![
                "--store-id",
                "s1",
                "--mount-path",
                "/memory",
                "--mode",
                "other",
            ],
        ] {
            assert!(
                MemorydConfig::from_args(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
                    .is_err()
            );
        }
    }

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
