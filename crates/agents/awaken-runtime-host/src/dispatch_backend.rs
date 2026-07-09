//! Durable-dispatch backend selection for the host: the process's unique claim
//! owner, the lease-renewal heartbeat cadence, and the shared Postgres pool
//! connected once at startup. This is a distinct responsibility from the run loop
//! in [`crate::host`], and lives here so backend wiring (ADR-0019/0024) stays in
//! one place.

use std::path::Path;
use std::sync::Arc;

use awaken_run_ingress::AnyDispatchStore;

use crate::host::HostError;

/// The dispatch claim owner for this process. Must be unique per process across a
/// fleet sharing one Postgres queue: the lease is owner-scoped, so a shared owner
/// lets peers renew each other's leases and breaks single-owner-per-run
/// (ADR-0019/0024). `AWAKEN_DISPATCH_OWNER` overrides; the default
/// `<hostname>-<pid>` is distinct per process and per node.
pub(crate) fn dispatch_owner() -> String {
    std::env::var("AWAKEN_DISPATCH_OWNER").unwrap_or_else(|_| {
        let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string());
        format!("{host}-{}", std::process::id())
    })
}

/// The lease-renewal heartbeat cadence for the standing daemon: a third of the
/// 30s default lease, so a renewal always lands before expiry (ADR-0024). Without
/// it a long run in a multi-node fleet would be reclaimed by a peer mid-flight.
pub(crate) const LEASE_RENEWAL: std::time::Duration = std::time::Duration::from_secs(10);

/// The process-wide shared Postgres dispatch backend, connected once at startup.
/// The pool is shared by every thread's durable ingress (one queue per process,
/// ADR-0019). It lives here — not built per-thread in the run path — because the
/// sqlx connect future is not `Send`, so awaiting it inside the run loop would
/// make the loop's future non-`Send`; [`shared_durable_store`] only clones the handle.
static SHARED_POSTGRES_DISPATCH: std::sync::OnceLock<Arc<AnyDispatchStore>> =
    std::sync::OnceLock::new();

/// The process-wide shared SQLite dispatch store: ONE queue file for the whole
/// process (or one in-memory queue when no store dir), not one per thread. The
/// process-level [`DispatchPool`](awaken_run_ingress::DispatchPool) is the sole
/// claimer of this shared queue and routes each run to its owning session, so a
/// single shared queue is safe — and necessary, since a pool cannot claim across
/// per-thread files.
static SHARED_SQLITE_DISPATCH: std::sync::OnceLock<Arc<AnyDispatchStore>> =
    std::sync::OnceLock::new();

/// Connect the process-wide Postgres dispatch pool at `url` and publish it for
/// `AWAKEN_DISPATCH_BACKEND=postgres`. Call this ONCE at startup (the server does
/// so before it serves) — it must run here, not in the per-thread run path, so the
/// non-`Send` sqlx connect future never enters the run loop's future. Idempotent:
/// a second call keeps the first pool.
pub async fn init_shared_postgres_dispatch(url: &str) -> Result<(), String> {
    if SHARED_POSTGRES_DISPATCH.get().is_some() {
        return Ok(());
    }
    let store = Arc::new(AnyDispatchStore::connect_postgres(url).await?);
    let _ = SHARED_POSTGRES_DISPATCH.set(store);
    Ok(())
}

/// Open the ONE process-shared durable-dispatch store, selected by
/// `AWAKEN_DISPATCH_BACKEND` (default `sqlite`). Every session's worker and the
/// process-level [`DispatchPool`](awaken_run_ingress::DispatchPool) share this one
/// queue: the pool is its sole claimer and routes each claimed run to its owning
/// session, so a single shared queue is both safe and required (a pool cannot
/// claim across per-thread files).
///
/// `postgres` shares one queue across processes too: `FOR UPDATE SKIP LOCKED`
/// gives distinct-claim, so N hosts against one `AWAKEN_DATABASE_URL` drain the
/// same queue (ADR-0019) — the pool is the one connected at startup. `sqlite` is a
/// single queue file `store_dir/dispatch.db` (survives a restart), or a private
/// in-memory queue when no store dir is set. Either way the concrete type is
/// `AnyDispatchStore`, so the ingress keeps its operational verbs reachable.
pub(crate) fn shared_durable_store(
    store_dir: Option<&Path>,
) -> Result<Arc<AnyDispatchStore>, HostError> {
    match std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() {
        Ok("postgres") => SHARED_POSTGRES_DISPATCH.get().cloned().ok_or_else(|| {
            HostError::internal(
                "AWAKEN_DISPATCH_BACKEND=postgres requires init_shared_postgres_dispatch() \
                 at process startup (with AWAKEN_DATABASE_URL)",
            )
        }),
        _ => {
            if let Some(store) = SHARED_SQLITE_DISPATCH.get() {
                return Ok(store.clone());
            }
            let store = Arc::new(match store_dir {
                Some(dir) => {
                    std::fs::create_dir_all(dir).map_err(|e| HostError::internal(e.to_string()))?;
                    let path = dir.join("dispatch.db");
                    AnyDispatchStore::open_sqlite(&path.to_string_lossy())
                        .map_err(HostError::internal)?
                }
                None => AnyDispatchStore::open_sqlite_in_memory().map_err(HostError::internal)?,
            });
            // First writer wins; a racing opener re-reads the winner and drops its own.
            let _ = SHARED_SQLITE_DISPATCH.set(store);
            Ok(SHARED_SQLITE_DISPATCH
                .get()
                .cloned()
                .expect("shared sqlite dispatch store set"))
        }
    }
}
