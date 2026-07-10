//! Shared Postgres commit-coordinator backend (ADR-0022 D6): the process-wide
//! thread-history store, connected once at startup. Unlike the per-thread SQLite
//! files (which pin a thread to a node), this is one coordinator keyed by thread
//! internally, so any node serves any thread's committed history — the prerequisite
//! for warm-reloading any thread on any Brain.

use std::sync::Arc;

use awaken_store_postgres::PostgresCommitCoordinator;

/// The process-wide shared Postgres commit coordinator, connected once at startup.
/// It lives here — not built per-thread in the run path — because the sqlx connect
/// future is not `Send`; awaiting it inside the run loop would make that future
/// non-`Send`. [`shared_postgres_commit`] only clones the handle.
static SHARED_POSTGRES_COMMIT: std::sync::OnceLock<Arc<PostgresCommitCoordinator>> =
    std::sync::OnceLock::new();

/// Connect the process-wide Postgres commit coordinator at `url` and publish it for
/// `AWAKEN_STORE=postgres`. Call this ONCE at startup (the server does so before it
/// serves) — it must run here, not in the per-thread run path. Idempotent: a second
/// call keeps the first coordinator.
pub async fn init_shared_postgres_commit(url: &str) -> Result<(), String> {
    if SHARED_POSTGRES_COMMIT.get().is_some() {
        return Ok(());
    }
    let coord = Arc::new(
        PostgresCommitCoordinator::connect(url)
            .await
            .map_err(|e| e.to_string())?,
    );
    let _ = SHARED_POSTGRES_COMMIT.set(coord);
    Ok(())
}

/// The shared Postgres commit coordinator, or `None` when `AWAKEN_STORE=postgres`
/// was not selected / [`init_shared_postgres_commit`] was not called.
pub(crate) fn shared_postgres_commit() -> Option<Arc<PostgresCommitCoordinator>> {
    SHARED_POSTGRES_COMMIT.get().cloned()
}
