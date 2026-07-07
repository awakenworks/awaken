//! Durable-dispatch backend selection for the host: the process's unique claim
//! owner, the lease-renewal heartbeat cadence, and the shared Postgres pool
//! connected once at startup. This is a distinct responsibility from the run loop
//! in [`crate::host`], and lives here so backend wiring (ADR-0019/0024) stays in
//! one place.

use std::path::Path;
use std::sync::Arc;

use awaken_run_ingress::AnyDispatchStore;

use crate::host::HostError;
use crate::store::sanitize_thread;

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
/// make the loop's future non-`Send`; [`open_durable_store`] only clones the handle.
static SHARED_POSTGRES_DISPATCH: std::sync::OnceLock<Arc<AnyDispatchStore>> =
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

/// Open the durable-dispatch store for `thread`, selected by
/// `AWAKEN_DISPATCH_BACKEND` (default `sqlite`). `postgres` shares ONE queue
/// across processes: `FOR UPDATE SKIP LOCKED` gives distinct-claim, so N hosts
/// against one `AWAKEN_DATABASE_URL` drain the same queue (ADR-0019) — the pool is
/// the one connected at startup. `sqlite` is a per-thread file queue under
/// `store_dir` (survives a restart), or a private in-memory queue when no store
/// dir is set. Either way the concrete type is `AnyDispatchStore`, so the ingress
/// keeps its operational verbs (recover / reap / supersede) reachable.
pub(crate) fn open_durable_store(
    store_dir: Option<&Path>,
    thread: &str,
) -> Result<Arc<AnyDispatchStore>, HostError> {
    match std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() {
        Ok("postgres") => SHARED_POSTGRES_DISPATCH.get().cloned().ok_or_else(|| {
            HostError::internal(
                "AWAKEN_DISPATCH_BACKEND=postgres requires init_shared_postgres_dispatch() \
                 at process startup (with AWAKEN_DATABASE_URL)",
            )
        }),
        _ => Ok(Arc::new(match store_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|e| HostError::internal(e.to_string()))?;
                let path = dir.join(format!("{}-dispatch.db", sanitize_thread(thread)));
                AnyDispatchStore::open_sqlite(&path.to_string_lossy())
                    .map_err(HostError::internal)?
            }
            None => AnyDispatchStore::open_sqlite_in_memory().map_err(HostError::internal)?,
        })),
    }
}
