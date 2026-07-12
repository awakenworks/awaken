//! Durable-dispatch backend selection for the host: the process's unique claim
//! owner, the lease-renewal heartbeat cadence, and the shared Postgres pool
//! connected once at startup. This is a distinct responsibility from the run loop
//! in [`crate::host`], and lives here so backend wiring (ADR-0019/0024) stays in
//! one place.

use std::path::Path;
use std::sync::Arc;

use awaken_run_ingress::{AnyDispatchStore, WakeSignal};

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

/// The process-wide cross-node wake for the Postgres backend, built once at startup
/// beside the store when `AWAKEN_DISPATCH_WAKE=pg-notify`. It shares the store's
/// database, so a `pg_notify` fired on enqueue nudges every peer's pool `LISTEN`ing
/// on the channel — no busy-poll, no extra infrastructure (ADR-0019/0024). Absent
/// (the pool falls back to its in-process `LocalWakeSignal` + poll) unless the env
/// selects it, so SQLite and single-node Postgres are unaffected.
static SHARED_PG_WAKE: std::sync::OnceLock<Arc<dyn WakeSignal>> = std::sync::OnceLock::new();

/// The process-wide cross-node wake over NATS, built once at startup beside the
/// Postgres store when `AWAKEN_DISPATCH_WAKE=nats` (NATS url via `AWAKEN_NATS_URL`,
/// subject via `AWAKEN_DISPATCH_WAKE_CHANNEL`). The durable STORE stays Postgres —
/// only the best-effort wake fan-out moves to the NATS broker, for a fleet that runs
/// one already (ADR-0019/0028). Mirrors [`SHARED_PG_WAKE`]; absent unless the env
/// selects nats. Only ever set when the binary is built with `--features nats`.
static SHARED_NATS_WAKE: std::sync::OnceLock<Arc<dyn WakeSignal>> = std::sync::OnceLock::new();

/// The process-wide shared SQLite dispatch store: ONE queue file for the whole
/// process (or one in-memory queue when no store dir), not one per thread. The
/// process-level [`DispatchPool`](awaken_run_ingress::DispatchPool) is the sole
/// claimer of this shared queue and routes each run to its owning session, so a
/// single shared queue is safe — and necessary, since a pool cannot claim across
/// per-thread files.
static SHARED_SQLITE_DISPATCH: std::sync::OnceLock<Arc<AnyDispatchStore>> =
    std::sync::OnceLock::new();

/// A dispatch store injected by a composition root that assembles its own backend
/// (open Gap: horizontal scaling). When set it OUTRANKS the env-selected sqlite /
/// postgres backends, so a closed hosting server can build a `ShardedDispatchQueue`
/// (fan-out over N per-shard Postgres queues), wrap it in an `AnyDispatchStore`, and
/// hand it here — the process pool then claims/drives through the shard fan-out with
/// no change to the neutral run path. Absent on a single-node runtime.
static SHARED_INJECTED_DISPATCH: std::sync::OnceLock<Arc<AnyDispatchStore>> =
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
    // Opt into a cross-node wake with `AWAKEN_DISPATCH_WAKE` (channel/subject via
    // `AWAKEN_DISPATCH_WAKE_CHANNEL`, default `awaken_dispatch_wake`):
    //  - `pg-notify`: fire `pg_notify` on the store's own database — no extra infra.
    //  - `nats`: fan the hint over a NATS broker at `AWAKEN_NATS_URL` (feature `nats`).
    // Either way the durable STORE stays Postgres; the wake is a best-effort hint and
    // the poll fallback stays authoritative. With no selection the pool keeps its
    // in-process `LocalWakeSignal` + poll, so SQLite/single-node Postgres are unaffected.
    let store = match dispatch_wake_kind() {
        DispatchWake::PgNotify => {
            let (store, wake) =
                AnyDispatchStore::connect_postgres_with_wake(url, &dispatch_wake_channel()).await?;
            let _ = SHARED_PG_WAKE.set(wake);
            Arc::new(store)
        }
        DispatchWake::Nats => connect_postgres_with_nats_wake(url).await?,
        DispatchWake::None => Arc::new(AnyDispatchStore::connect_postgres(url).await?),
    };
    let _ = SHARED_POSTGRES_DISPATCH.set(store);
    Ok(())
}

/// Build the Postgres store paired with a NATS wake (feature `nats`). Reads the NATS
/// url from `AWAKEN_NATS_URL` and the subject from `AWAKEN_DISPATCH_WAKE_CHANNEL`,
/// then publishes the ready wake into [`SHARED_NATS_WAKE`].
#[cfg(feature = "nats")]
async fn connect_postgres_with_nats_wake(url: &str) -> Result<Arc<AnyDispatchStore>, String> {
    let nats_url = std::env::var("AWAKEN_NATS_URL")
        .map_err(|_| "AWAKEN_DISPATCH_WAKE=nats requires AWAKEN_NATS_URL".to_string())?;
    let (store, wake) = AnyDispatchStore::connect_postgres_with_nats_wake(
        url,
        &nats_url,
        &dispatch_wake_channel(),
    )
    .await?;
    let _ = SHARED_NATS_WAKE.set(wake);
    Ok(Arc::new(store))
}

/// When the binary is built WITHOUT `--features nats`, selecting `AWAKEN_DISPATCH_WAKE=nats`
/// is a hard configuration error: fail loudly at startup rather than silently degrade to
/// poll-only (which would look identical to a working wake but never nudge a peer).
#[cfg(not(feature = "nats"))]
async fn connect_postgres_with_nats_wake(_url: &str) -> Result<Arc<AnyDispatchStore>, String> {
    Err("AWAKEN_DISPATCH_WAKE=nats requested but binary built without --features nats \
         (rebuild awaken-server-local with --features nats to enable the NATS wake)"
        .to_string())
}

/// Inject a pre-assembled dispatch store as THE process backend, outranking the
/// env-selected sqlite/postgres backends. Call ONCE at startup (before the pool is
/// spawned) — the neutral seam for a horizontal-scaling composition root (a closed
/// hosting server building a `ShardedDispatchQueue`). Idempotent: a second call keeps
/// the first store.
pub fn init_shared_dispatch_store(store: Arc<AnyDispatchStore>) {
    let _ = SHARED_INJECTED_DISPATCH.set(store);
}

/// The cross-node wake channel/subject for the served pool. Shared by both the
/// `pg_notify` channel and the NATS subject (default `awaken_dispatch_wake`).
fn dispatch_wake_channel() -> String {
    std::env::var("AWAKEN_DISPATCH_WAKE_CHANNEL")
        .unwrap_or_else(|_| "awaken_dispatch_wake".to_string())
}

/// The cross-node wake backend selected by `AWAKEN_DISPATCH_WAKE`, if any.
enum DispatchWake {
    /// No cross-node wake: the pool keeps its in-process `LocalWakeSignal` + poll.
    None,
    /// `pg_notify` on the store's own database (no extra infrastructure).
    PgNotify,
    /// A NATS broker at `AWAKEN_NATS_URL` (requires `--features nats`).
    Nats,
}

/// Which cross-node wake, if any, `AWAKEN_DISPATCH_WAKE` selects for the served pool.
fn dispatch_wake_kind() -> DispatchWake {
    match std::env::var("AWAKEN_DISPATCH_WAKE").as_deref() {
        Ok("pg-notify") => DispatchWake::PgNotify,
        Ok("nats") => DispatchWake::Nats,
        _ => DispatchWake::None,
    }
}

/// Composition-root guard: a **durable** ingress must be backed by a **persistent**
/// queue, or it is a durable ingress in name only. `shared_durable_store` falls back
/// to an in-memory SQLite queue when the sqlite backend has no store dir
/// (`open_sqlite_in_memory`, above) — convenient for tests, but in a real boot it
/// would silently drop on restart exactly the runs the durable path exists to
/// protect (crash-recovery, dead-letter, scheduled actions). That violates the
/// no-data-loss invariant, so refuse to start instead of degrading silently.
///
/// Persistent backings that satisfy the guard:
/// - `AWAKEN_DISPATCH_BACKEND=postgres` (the shared cross-node queue), or
/// - a set `AWAKEN_STORAGE_DIR` (the on-disk `dispatch.db` SQLite queue), or
/// - an injected backend (a closed composition root already assembled a durable
///   `ShardedDispatchQueue`; it owns its own durability contract).
///
/// A pure decision so it is unit-tested without touching process env; the public
/// [`ensure_durable_backend`] wraps it over the environment.
fn durable_backend_persisted(
    durable: bool,
    postgres_backend: bool,
    has_storage_dir: bool,
    injected: bool,
) -> Result<(), &'static str> {
    if durable && !postgres_backend && !has_storage_dir && !injected {
        return Err(
            "AWAKEN_INGRESS=durable needs a persistent dispatch queue, but none is \
             configured: the default SQLite backend has no AWAKEN_STORAGE_DIR, so the \
             queue would be in-memory and a restart would silently drop every queued, \
             crashed, dead-lettered, and scheduled run. Set AWAKEN_STORAGE_DIR for the \
             durable on-disk queue (<dir>/dispatch.db), or AWAKEN_DISPATCH_BACKEND=postgres \
             with AWAKEN_DATABASE_URL for the shared queue. Refusing to serve a 'durable' \
             ingress on a volatile queue.",
        );
    }
    Ok(())
}

/// Fail fast at startup when `AWAKEN_INGRESS=durable` would resolve to a volatile
/// in-memory queue (see [`durable_backend_persisted`]). Call this in the composition
/// root before serving — every open boot path (`awaken-server-local`,
/// `awaken-standalone`) does. A no-op unless durable ingress is enabled.
pub fn ensure_durable_backend() -> Result<(), String> {
    let durable = std::env::var("AWAKEN_INGRESS").as_deref() == Ok("durable");
    let postgres_backend = std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() == Ok("postgres");
    let has_storage_dir = std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .is_some_and(|value| !value.is_empty());
    let injected = SHARED_INJECTED_DISPATCH.get().is_some();
    durable_backend_persisted(durable, postgres_backend, has_storage_dir, injected)
        .map_err(str::to_string)
}

/// The cross-node wake to spawn the served pool with, if one was built at startup
/// (`AWAKEN_DISPATCH_WAKE=pg-notify` or `=nats` on the Postgres backend). `None` keeps
/// the pool on its default in-process `LocalWakeSignal`. At most one is ever set —
/// `AWAKEN_DISPATCH_WAKE` picks a single backend — so prefer whichever was built.
pub(crate) fn shared_dispatch_wake() -> Option<Arc<dyn WakeSignal>> {
    SHARED_PG_WAKE
        .get()
        .or_else(|| SHARED_NATS_WAKE.get())
        .cloned()
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
    // An injected backend (a horizontal-scaling shard fan-out) outranks every
    // env-selected one: a closed composition root already assembled the queue.
    if let Some(store) = SHARED_INJECTED_DISPATCH.get() {
        return Ok(store.clone());
    }
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

#[cfg(test)]
mod tests {
    use super::durable_backend_persisted;

    #[test]
    fn direct_ingress_never_requires_persistence() {
        // Not durable → the queue is irrelevant, any combination is fine.
        assert!(durable_backend_persisted(false, false, false, false).is_ok());
    }

    #[test]
    fn durable_sqlite_without_storage_dir_is_rejected() {
        // The footgun: `AWAKEN_INGRESS=durable` on the default sqlite backend with no
        // `AWAKEN_STORAGE_DIR` would give a volatile in-memory queue — refuse it.
        let err = durable_backend_persisted(true, false, false, false).unwrap_err();
        assert!(err.contains("AWAKEN_STORAGE_DIR"), "{err}");
        assert!(err.contains("durable"), "{err}");
    }

    #[test]
    fn durable_is_accepted_on_any_persistent_backing() {
        // A storage dir (on-disk dispatch.db), Postgres (shared queue), or an injected
        // backend each satisfy the durability contract.
        assert!(durable_backend_persisted(true, false, true, false).is_ok());
        assert!(durable_backend_persisted(true, true, false, false).is_ok());
        assert!(durable_backend_persisted(true, false, false, true).is_ok());
    }
}
