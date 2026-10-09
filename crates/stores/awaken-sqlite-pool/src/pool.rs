// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: the one embedded SQLite pool policy (ADR-0087). Dynamic path: each store
// @formal reason: opens one one-connection pool whose fair acquisition keeps the former FIFO admission,
// @formal reason: with the same busy wait, foreign-key policy and process-serialized one-time WAL switch.
// @formal reason: Evidence: the adjacent policy and admission decision-table tests (S1, F1-F4, H1-H4); this
// @formal reason: is a product-requirement classification, not a direct formal-proof claim.
//! The embedded-process SQLite policy for sqlx pools (ADR-0087).
//!
//! One store opens one pool with exactly one connection. sqlx's pool acquisition
//! is fair (first come, first served), so callers are admitted in the order they
//! asked, as they were behind the former blocking bridge's fair one-permit
//! semaphore. Every connection gets the same busy wait and foreign-key policy,
//! and a file database is switched to WAL once, under a process-wide lock.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Connection as _, TransactionManager as _};

use crate::WRITE_WAIT;
use crate::clock_hold::ClockHold;
use crate::embedded_pool::EmbeddedPool;
use crate::settle::{COLLATION, ConnectedFile, LiveFile, file_key, settled};

/// Admission waits like the former semaphore did: a caller queues until the one
/// connection is free. The bound only keeps a wedged connection from hanging a
/// caller forever; it is far above any write wait.
const ADMISSION_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

static JOURNAL_MODE_CHANGE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, PartialEq, Eq)]
enum Location {
    File(PathBuf),
    ReadOnlyFile(PathBuf),
    Memory,
}

/// Where an embedded store keeps its SQLite database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedSqlite {
    location: Location,
}

#[derive(Debug, thiserror::Error)]
pub enum SqlitePoolError {
    #[error("cannot open SQLite pool: {0}")]
    Open(#[source] sqlx::Error),
    #[error("cannot configure SQLite pool: {0}")]
    Configure(#[source] sqlx::Error),
}

impl EmbeddedSqlite {
    /// A database file, created if missing.
    #[must_use]
    pub fn file(path: impl AsRef<Path>) -> Self {
        Self {
            location: Location::File(path.as_ref().to_path_buf()),
        }
    }

    /// An existing database owned by someone else, opened read-only. Nothing is
    /// created and the journal mode is never changed, so reading it cannot write
    /// to the file.
    #[must_use]
    pub fn read_only(path: impl AsRef<Path>) -> Self {
        Self {
            location: Location::ReadOnlyFile(path.as_ref().to_path_buf()),
        }
    }

    /// A private in-memory database that lives as long as the returned pool.
    #[must_use]
    pub const fn memory() -> Self {
        Self {
            location: Location::Memory,
        }
    }

    fn file_path(&self) -> Option<&Path> {
        match &self.location {
            Location::File(path) | Location::ReadOnlyFile(path) => Some(path),
            Location::Memory => None,
        }
    }

    /// Wait until every pool this process dropped on the file has finished
    /// closing. sqlx closes a dropped pool's connection later, on its own thread,
    /// and closing the last connection checkpoints the WAL into the main file, so
    /// a caller that inspects the files after dropping a store waits here to see
    /// them at rest, as it did when dropping a synchronous connection closed it.
    /// Pools still in use do not delay it.
    pub async fn settled(&self) {
        if let Some(path) = self.file_path() {
            settled(path).await;
        }
    }

    /// Open the store's one-connection pool under the embedded policy.
    pub async fn open(&self) -> Result<EmbeddedPool, SqlitePoolError> {
        let options = match &self.location {
            Location::File(path) => SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true),
            Location::ReadOnlyFile(path) => {
                SqliteConnectOptions::new().filename(path).read_only(true)
            }
            Location::Memory => SqliteConnectOptions::new().in_memory(true),
        }
        .busy_timeout(WRITE_WAIT)
        .foreign_keys(true);
        let file = self.file_path().map(file_key);
        let options = match &file {
            Some(file) => {
                let connected = ConnectedFile::new(file.clone());
                options.collation(COLLATION, move |left: &str, right: &str| {
                    let _connected = &connected;
                    left.cmp(right)
                })
            }
            None => options,
        };
        let _opening = ClockHold::take();
        // `min_connections(1)` with no idle or lifetime limit keeps the single
        // connection open, so an in-memory database is never dropped and
        // recreated empty behind the store's back.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(ADMISSION_WAIT)
            .test_before_acquire(false)
            .after_release(|connection, _| {
                Box::pin(async move {
                    // sqlx's custom BEGIN validates its handle after the worker
                    // acknowledged BEGIN but before a Transaction exists. A
                    // cancelled constructor can therefore leave depth behind
                    // without a Transaction Drop to enqueue rollback. Return
                    // the same connection only after the driver's own rollback
                    // settles every remaining level; sqlx retires it on error.
                    while connection.is_in_transaction() {
                        sqlx::sqlite::SqliteTransactionManager::rollback(connection).await?;
                    }
                    Ok(true)
                })
            })
            .connect_with(options)
            .await
            .map_err(SqlitePoolError::Open)?;
        if matches!(self.location, Location::File(_)) {
            ensure_wal(&pool).await?;
        }
        Ok(EmbeddedPool::new(pool, file.map(LiveFile::new)))
    }
}

/// Changing the journal mode is a database write. Serialize only that one-time
/// transition and never rewrite WAL on an open: another freshly opened store may
/// already be running its migration on the same file.
async fn ensure_wal(pool: &SqlitePool) -> Result<(), SqlitePoolError> {
    let _change = JOURNAL_MODE_CHANGE.lock().await;
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(pool)
        .await
        .map_err(SqlitePoolError::Configure)?;
    if !journal.eq_ignore_ascii_case("wal") {
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(pool)
            .await
            .map_err(SqlitePoolError::Configure)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt as _;

    use super::*;

    async fn pragma_text(pool: &EmbeddedPool, pragma: &str) -> String {
        sqlx::query_scalar::<_, String>(&format!("PRAGMA {pragma}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn pragma_int(pool: &EmbeddedPool, pragma: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(&format!("PRAGMA {pragma}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Connection-policy decision table:
    ///
    /// | Rule | Shared file | Effect |
    /// |---|---|---|
    /// | P1 | yes, two pools | WAL, foreign keys on, 30 s busy wait on both |
    /// | P2 | memory | foreign keys on, 30 s busy wait; journal stays `memory` |
    /// | P3 | yes, another pool mid-transaction | WAL, no journal-mode write attempted |
    #[tokio::test]
    async fn every_pool_receives_the_same_process_policy() {
        let directory = tempfile::tempdir().unwrap();
        let store = EmbeddedSqlite::file(directory.path().join("process.db"));
        let first = store.open().await.unwrap();
        let second = store.open().await.unwrap();
        for pool in [&first, &second] {
            assert_eq!(
                pragma_text(pool, "journal_mode").await.to_ascii_lowercase(),
                "wal",
                "P1"
            );
            assert_eq!(pragma_int(pool, "foreign_keys").await, 1, "P1");
            assert_eq!(pragma_int(pool, "busy_timeout").await, 30_000, "P1");
        }

        let memory = EmbeddedSqlite::memory().open().await.unwrap();
        assert_eq!(
            pragma_text(&memory, "journal_mode")
                .await
                .to_ascii_lowercase(),
            "memory",
            "P2"
        );
        assert_eq!(pragma_int(&memory, "foreign_keys").await, 1, "P2");
        assert_eq!(pragma_int(&memory, "busy_timeout").await, 30_000, "P2");
    }

    #[tokio::test]
    async fn opening_an_existing_wal_file_does_not_rewrite_journal_mode() {
        let directory = tempfile::tempdir().unwrap();
        let store = EmbeddedSqlite::file(directory.path().join("writer.db"));
        let writer = store.open().await.unwrap();
        let mut transaction = writer.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("CREATE TABLE authority (id INTEGER PRIMARY KEY)")
            .execute(&mut *transaction)
            .await
            .unwrap();

        let observer = store
            .open()
            .await
            .expect("P3 observer opens while a writer holds the lock");
        assert_eq!(
            pragma_text(&observer, "journal_mode")
                .await
                .to_ascii_lowercase(),
            "wal",
            "P3"
        );
        transaction.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn a_read_only_database_is_neither_written_nor_switched_to_wal() {
        // R1 an existing rollback-journal file opened read-only keeps its journal
        // mode; R2 a write through the pool fails; R3 a missing file is not created.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("foreign.db");
        {
            let owner = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(&path)
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            sqlx::query("CREATE TABLE t (id INTEGER)")
                .execute(&owner)
                .await
                .unwrap();
            owner.close().await;
        }
        let reader = EmbeddedSqlite::read_only(&path).open().await.unwrap();
        assert_eq!(
            pragma_text(&reader, "journal_mode")
                .await
                .to_ascii_lowercase(),
            "delete",
            "R1"
        );
        assert!(
            sqlx::query("INSERT INTO t VALUES (1)")
                .execute(&reader)
                .await
                .is_err(),
            "R2"
        );
        let missing = directory.path().join("missing.db");
        assert!(
            EmbeddedSqlite::read_only(&missing).open().await.is_err(),
            "R3"
        );
        assert!(!missing.exists(), "R3");
    }

    /// Paused-clock decision table (a current-thread runtime with paused time):
    ///
    /// | Rule | SQLite work | Pending timer | Effect |
    /// |---|---|---|---|
    /// | C1 | opening the pool | the 24 h admission bound | open succeeds |
    /// | C2 | one statement | a 1 s timer | the statement finishes first |
    /// | C3 | back-to-back callers on the one connection | admission bounds | every caller is admitted |
    #[tokio::test(start_paused = true)]
    async fn sqlite_work_holds_a_paused_clock_still() {
        let pool = EmbeddedSqlite::memory().open().await.expect("C1");
        sqlx::query("CREATE TABLE fact (value INTEGER)")
            .execute(&pool)
            .await
            .unwrap();

        let started = tokio::time::Instant::now();
        let timer = tokio::spawn(tokio::time::sleep(Duration::from_secs(1)));
        for value in 0..50 {
            sqlx::query("INSERT INTO fact (value) VALUES (?1)")
                .bind(value)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert_eq!(started.elapsed(), Duration::ZERO, "C2");
        assert!(!timer.is_finished(), "C2");

        let callers = (0..20)
            .map(|value| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    sqlx::query("INSERT INTO fact (value) VALUES (?1)")
                        .bind(value)
                        .execute(&pool)
                        .await
                })
            })
            .collect::<Vec<_>>();
        for caller in callers {
            caller.await.unwrap().expect("C3");
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fact")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 70, "C3");
        timer.await.unwrap();
    }

    /// Settle decision table:
    ///
    /// | Rule | Pools on the file | Effect of `settled` |
    /// |---|---|---|
    /// | L1 | one in use | resolves at once |
    /// | L2 | one in use, one dropped | resolves once the dropped one has closed; the other still works |
    /// | L3 | all dropped | resolves once closed; the last close checkpointed and removed the WAL |
    /// | L4 | memory, or an open that failed | resolves at once |
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settled_waits_only_for_dropped_pools_to_finish_closing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settled.db");
        let store = EmbeddedSqlite::file(&path);
        let first = store.open().await.unwrap();
        sqlx::query("CREATE TABLE fact (value INTEGER)")
            .execute(&first)
            .await
            .unwrap();
        assert!(store.settled().now_or_never().is_some(), "L1");

        let second = EmbeddedSqlite::file(directory.path().join(".").join("settled.db"))
            .open()
            .await
            .unwrap();
        drop(first);
        tokio::time::timeout(Duration::from_secs(10), store.settled())
            .await
            .expect("L2 the dropped pool closes");
        sqlx::query("INSERT INTO fact (value) VALUES (1)")
            .execute(&second)
            .await
            .expect("L2");

        drop(second);
        tokio::time::timeout(Duration::from_secs(10), store.settled())
            .await
            .expect("L3 the dropped pools close");
        assert!(!directory.path().join("settled.db-wal").exists(), "L3");

        assert!(
            EmbeddedSqlite::memory().settled().now_or_never().is_some(),
            "L4"
        );
        let missing = EmbeddedSqlite::read_only(directory.path().join("missing.db"));
        assert!(missing.open().await.is_err(), "L4");
        assert!(missing.settled().now_or_never().is_some(), "L4");
    }

    #[tokio::test]
    async fn an_in_memory_database_persists_across_operations() {
        // The pool keeps its single connection, so data written by one operation
        // is visible to the next; a recycled connection would see an empty
        // database.
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        sqlx::query("CREATE TABLE kept (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE name = 'kept'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    // ---- Admission semantics carried over from the former blocking bridge ----
    //
    // The bridge's decision tables (S1, F1-F4, H1-H4) are asserted here against
    // the one-connection pool, which is now their only owner. F4 differs by
    // design: a panic drops its transaction, which sqlx rolls back on the
    // connection before handing it out, so the next caller gets a clean
    // connection instead of a permanently poisoned one.

    use std::sync::{Arc, Mutex};

    type Order = Arc<Mutex<Vec<&'static str>>>;

    async fn memory_pool() -> EmbeddedPool {
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        sqlx::query("CREATE TABLE evidence (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    /// Queue one caller behind the current owner *now*, in program order, by
    /// polling its admission once in place before spawning it (a spawned task
    /// might otherwise register after the next caller).
    async fn enqueue(
        pool: &EmbeddedPool,
        order: &Order,
        name: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let (pool, order) = (pool.clone(), Arc::clone(order));
        let mut admission = Box::pin(async move {
            let _connection = pool.acquire().await.unwrap();
            order.lock().unwrap().push(name);
        });
        std::future::poll_fn(|cx| {
            assert!(
                admission.as_mut().poll(cx).is_pending(),
                "a caller queued behind an owner must wait for admission"
            );
            std::task::Poll::Ready(())
        })
        .await;
        tokio::spawn(admission)
    }

    /// Hold the one connection until the returned sender fires.
    async fn owner(
        pool: &EmbeddedPool,
        order: &Order,
        name: &'static str,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (pool, order) = (pool.clone(), Arc::clone(order));
        let task = tokio::spawn(async move {
            let _connection = pool.acquire().await.unwrap();
            order.lock().unwrap().push(name);
            entered_tx.send(()).unwrap();
            let _ = release_rx.await;
        });
        entered_rx.await.unwrap();
        (task, release_tx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_contention_never_blocks_authority_timers() {
        // S1: callers queued behind a held connection do not occupy runtime
        // workers; an authority timer stays schedulable and both finish later.
        let pool = memory_pool().await;
        let order = Order::default();
        let (holder, release) = owner(&pool, &order, "owner").await;
        let left = enqueue(&pool, &order, "left").await;
        let right = enqueue(&pool, &order, "right").await;
        let started = std::time::Instant::now();
        // awaken-wait: S1 measures the timer itself; its prompt firing proves runtime workers are not blocked.
        let authority_timer = tokio::time::sleep(Duration::from_millis(10));
        tokio::time::timeout(Duration::from_millis(100), authority_timer)
            .await
            .expect("S1 authority timer remains schedulable");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "S1 runtime workers were blocked"
        );
        release.send(()).unwrap();
        for task in [holder, left, right] {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["owner", "left", "right"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_foreground_control_cannot_be_starved_by_hot_background_reentry() {
        // F1: a requeued background caller cannot overtake a foreground caller
        // that queued before it.
        let pool = memory_pool().await;
        let order = Order::default();
        let (background, release) = owner(&pool, &order, "background-1").await;
        let foreground = enqueue(&pool, &order, "foreground").await;
        let reentry = enqueue(&pool, &order, "background-2").await;
        release.send(()).unwrap();
        for task in [background, foreground, reentry] {
            task.await.unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            ["background-1", "foreground", "background-2"],
            "F1"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_queued_callers_enter_in_exactly_the_order_they_queued() {
        // F2
        let pool = memory_pool().await;
        let order = Order::default();
        let (owner_task, release) = owner(&pool, &order, "owner").await;
        let first = enqueue(&pool, &order, "first").await;
        let second = enqueue(&pool, &order, "second").await;
        let third = enqueue(&pool, &order, "third").await;
        release.send(()).unwrap();
        for task in [owner_task, first, second, third] {
            task.await.unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            ["owner", "first", "second", "third"],
            "F2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_cancelled_waiter_never_runs_and_never_blocks_the_queue() {
        // F3
        let pool = memory_pool().await;
        let order = Order::default();
        let (owner_task, release) = owner(&pool, &order, "owner").await;
        let cancelled = enqueue(&pool, &order, "cancelled").await;
        let live = enqueue(&pool, &order, "live").await;
        cancelled.abort();
        assert!(cancelled.await.expect_err("F3 cancelled").is_cancelled());
        release.send(()).unwrap();
        owner_task.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), live)
            .await
            .expect("F3 the live caller is not stuck behind the cancelled one")
            .unwrap();
        assert_eq!(*order.lock().unwrap(), ["owner", "live"], "F3");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_panicking_operation_rolls_back_and_releases_admission() {
        // F4': an operation that panics inside an open write transaction releases
        // admission; its transaction is rolled back before the next caller, which
        // sees a clean connection and none of the uncommitted write.
        let pool = memory_pool().await;
        let panicking = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
                sqlx::query("INSERT INTO evidence VALUES (1)")
                    .execute(&mut *transaction)
                    .await
                    .unwrap();
                panic!("F4 operation fails mid-flight");
            })
        };
        assert!(
            panicking
                .await
                .expect_err("F4 the caller sees a failure")
                .is_panic()
        );
        let count: i64 = tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query_scalar("SELECT count(*) FROM evidence").fetch_one(&pool),
        )
        .await
        .expect("F4 admission was released, not deadlocked")
        .unwrap();
        assert_eq!(count, 0, "F4 the uncommitted write was rolled back");
        sqlx::query("INSERT INTO evidence VALUES (2)")
            .execute(&pool)
            .await
            .expect("F4 the next caller can write: no transaction was left open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_held_transactions_release_or_drop_without_committing_and_preserve_fifo() {
        // H1 explicit rollback, H2 dropped transaction, H3 domain error then
        // rollback: none commits, and callers queued behind the held transaction
        // enter in FIFO order afterwards while the runtime stays schedulable.
        for rule in ["release", "drop", "domain-error"] {
            let pool = memory_pool().await;
            let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
            sqlx::query("INSERT INTO evidence VALUES (1)")
                .execute(&mut *transaction)
                .await
                .unwrap();
            let prepared: Result<(), &str> = if rule == "domain-error" {
                Err("domain")
            } else {
                Ok(())
            };
            assert_eq!(prepared.is_err(), rule == "domain-error");
            let order = Order::default();
            let first = enqueue(&pool, &order, "first").await;
            let second = enqueue(&pool, &order, "second").await;
            tokio::time::timeout(Duration::from_secs(1), tokio::spawn(async { 7 }))
                .await
                .expect("authority runtime remains schedulable")
                .unwrap();
            if rule == "drop" {
                drop(transaction);
            } else {
                transaction.rollback().await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                first.await.unwrap();
                second.await.unwrap();
            })
            .await
            .expect("released admission");
            assert_eq!(*order.lock().unwrap(), ["first", "second"]);
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{rule} cannot commit the prepared transaction");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_cancelling_during_held_prepare_rolls_back_before_next_admission() {
        // H4: the caller disappears while its transaction is open; the rollback
        // runs on the connection before the next caller uses it.
        let pool = memory_pool().await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let preparing = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
                sqlx::query("INSERT INTO evidence VALUES (1)")
                    .execute(&mut *transaction)
                    .await
                    .unwrap();
                entered_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            })
        };
        entered_rx.await.unwrap();
        preparing.abort();
        assert!(preparing.await.unwrap_err().is_cancelled());
        let count: i64 = tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query_scalar("SELECT count(*) FROM evidence").fetch_one(&pool),
        )
        .await
        .expect("H4 admission released")
        .unwrap();
        assert_eq!(count, 0, "H4 rollback precedes successor access");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_cancelled_custom_begin_never_leaks_transaction_depth() {
        use std::future::{Future as _, poll_fn};
        use std::task::Poll;

        // H5 cancellation at the first driver suspension (BEGIN acknowledgement),
        // H6 cancellation at the second suspension (custom-BEGIN handle validation)
        // -> E1 the same pool admits the next BEGIN IMMEDIATE at depth zero.
        // H7 a BEGIN completes before that suspension -> dropping its actual
        // Transaction has the same E1. E2 the private memory table survives,
        // proving rollback reused the connection instead of losing the database.
        // Poll the real public sqlx future, not a
        // model of its worker; dropping this acquired connection matches the
        // pool-owned connection dropped by a cancelled Transaction constructor.
        let pool = memory_pool().await;
        for boundary in [1, 2] {
            let mut cancellations = 0;
            for attempt in 0..32 {
                let mut connection = pool.acquire().await.unwrap();
                let cancelled = {
                    let mut beginning = std::pin::pin!(sqlx::Connection::begin_with(
                        &mut *connection,
                        "BEGIN IMMEDIATE"
                    ));
                    let mut pending = 0;
                    poll_fn(|context| match beginning.as_mut().poll(context) {
                        Poll::Pending => {
                            pending += 1;
                            if pending == boundary {
                                Poll::Ready(true)
                            } else {
                                Poll::Pending
                            }
                        }
                        Poll::Ready(result) => {
                            drop(result.unwrap());
                            Poll::Ready(false)
                        }
                    })
                    .await
                };
                cancellations += usize::from(cancelled);
                drop(connection);
                let next = tokio::time::timeout(
                    Duration::from_secs(5),
                    pool.begin_with("BEGIN IMMEDIATE"),
                )
                .await
                .expect("H5-H7 next admission must not remain blocked")
                .unwrap_or_else(|error| panic!("boundary {boundary}, attempt {attempt}: {error}"));
                next.rollback().await.unwrap();
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(count, 0, "E2 retain the same in-memory database");
            }
            assert!(cancellations > 0, "boundary {boundary} actually cancelled");
        }
    }

    #[tokio::test]
    async fn pool_release_settles_nested_depth_or_retires_failed_rollback() {
        // Release decision table: R1 orphan depth>1 + rollback succeeds =>
        // rollback every level, retain the memory DB, discard uncommitted write;
        // R2 tracked depth>0 + SQLite already rolled back => rollback error,
        // retire that connection and preserve the file's committed authority.
        // R0 depth=0 reuse is covered by the process-policy/admission cases.
        // These use the actual driver transaction manager and SQL engine, not
        // a mock depth counter or a second pool implementation.
        // R3 a leaked admission/rollback wait => bounded diagnostic failure,
        // never the pool's production 24-hour admission wait.
        tokio::time::timeout(Duration::from_secs(5), async {
            let memory = memory_pool().await;
            let mut connection = memory.acquire().await.unwrap();
            for _ in 0..2 {
                sqlx::sqlite::SqliteTransactionManager::begin(&mut connection, None)
                    .await
                    .unwrap();
            }
            sqlx::query("INSERT INTO evidence VALUES (1)")
                .execute(&mut *connection)
                .await
                .unwrap();
            drop(connection);
            let next = memory.begin_with("BEGIN IMMEDIATE").await.unwrap();
            next.rollback().await.unwrap();
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM evidence")
                .fetch_one(&memory)
                .await
                .unwrap();
            assert_eq!(count, 0, "R1 all nested uncommitted writes rolled back");

            let directory = tempfile::tempdir().unwrap();
            let store = EmbeddedSqlite::file(directory.path().join("rollback.db"));
            let file = store.open().await.unwrap();
            sqlx::query("CREATE TABLE evidence (id INTEGER); INSERT INTO evidence VALUES (7)")
                .execute(&file)
                .await
                .unwrap();
            let mut connection = file.acquire().await.unwrap();
            sqlx::sqlite::SqliteTransactionManager::begin(&mut connection, None)
                .await
                .unwrap();
            sqlx::query("ROLLBACK")
                .execute(&mut *connection)
                .await
                .unwrap();
            assert!(
                connection.is_in_transaction(),
                "R2 driver still records depth"
            );
            drop(connection);
            let next = file.begin_with("BEGIN IMMEDIATE").await.unwrap();
            next.rollback().await.unwrap();
            let value: i64 = sqlx::query_scalar("SELECT id FROM evidence")
                .fetch_one(&file)
                .await
                .unwrap();
            assert_eq!(value, 7, "R2 committed file authority survives retirement");
            file.close().await;
            store.settled().await;
        })
        .await
        .expect("R1/R2 release must settle or retire the connection within 5 seconds");
    }
}
