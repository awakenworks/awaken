// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: the one embedded SQLite connection policy shared by persistent store
// @formal reason: adapters. Dynamic path: every file connection receives the same bounded wait and
// @formal reason: foreign-key policy, while a process-serialized one-time WAL transition prevents a
// @formal reason: redundant journal-mode write from racing another store's migration transaction.
// @formal reason: Evidence: store.backends.schema_and_adapter_conformance plus the adjacent
// @formal reason: connection-policy decision-table tests and Session-store concurrent migration test; this
// @formal reason: is a product-requirement classification, not a direct formal-proof claim.
//! Canonical SQLite connection policy for embedded process stores.
//!
//! Store adapters continue to own schemas, migrations, transactions, and domain
//! error mapping. This crate owns only connection-local process policy so two
//! adapters opening the same file cannot silently disagree about locking or
//! foreign-key enforcement.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LockResult, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Semaphore;

const WRITE_WAIT: Duration = Duration::from_secs(30);
static JOURNAL_MODE_CHANGE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq)]
enum SqliteLocation {
    File(PathBuf),
    Memory,
}

/// Reusable factory for connections governed by the embedded-process policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteConnectionFactory {
    location: SqliteLocation,
}

#[derive(Debug, thiserror::Error)]
pub enum SqliteConnectionError {
    #[error("cannot open SQLite connection: {0}")]
    Open(#[source] rusqlite::Error),
    #[error("cannot configure SQLite connection: {0}")]
    Configure(#[source] rusqlite::Error),
}

/// Failure to cross the canonical synchronous-SQLite scheduler boundary.
///
/// Store adapters retain ownership of schema, transactions, and domain error
/// mapping. This error covers only the shared connection lock and Tokio
/// blocking-task lifecycle, so adapters do not duplicate event-loop safety.
#[derive(Debug, thiserror::Error)]
pub enum SqliteTaskError {
    #[error("SQLite connection scheduler was closed")]
    SchedulerClosed,
    #[error("SQLite connection mutex was poisoned")]
    Poisoned,
    #[error("SQLite blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// One shared SQLite connection and its canonical async admission boundary.
///
/// The fair semaphore is acquired before a blocking task is spawned. This
/// prevents a hot background caller from repeatedly reacquiring the synchronous
/// mutex ahead of an already-waiting authority request, and bounds blocking-pool
/// occupancy to the one operation that can actually use this connection.
#[derive(Clone)]
pub struct SharedSqliteConnection {
    connection: Arc<Mutex<Connection>>,
    admission: Arc<Semaphore>,
}

impl SharedSqliteConnection {
    #[must_use]
    pub fn new(connection: Connection) -> Self {
        Self {
            connection: Arc::new(Mutex::new(connection)),
            admission: Arc::new(Semaphore::new(1)),
        }
    }

    /// Synchronous access for construction-time migration and test inspection.
    /// Runtime async adapters must use [`with_connection`].
    pub fn lock(&self) -> LockResult<MutexGuard<'_, Connection>> {
        self.connection.lock()
    }
}

/// Execute one synchronous operation against a shared SQLite connection away
/// from Tokio worker threads.
///
/// `T` may itself be a store-specific `Result`; the helper intentionally does
/// not reinterpret domain or persistence failures. The connection mutex stays
/// the sole embedded single-connection serialization mechanism, while cloud
/// deployments continue to use their PostgreSQL adapters for cross-process
/// concurrency.
pub async fn with_connection<T, F>(
    connection: SharedSqliteConnection,
    operation: F,
) -> Result<T, SqliteTaskError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> T + Send + 'static,
{
    let permit = Arc::clone(&connection.admission)
        .acquire_owned()
        .await
        .map_err(|_| SqliteTaskError::SchedulerClosed)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut connection = connection.lock().map_err(|_| SqliteTaskError::Poisoned)?;
        Ok::<T, SqliteTaskError>(operation(&mut connection))
    })
    .await?
}

impl SqliteConnectionFactory {
    #[must_use]
    pub fn file(path: impl AsRef<Path>) -> Self {
        Self {
            location: SqliteLocation::File(path.as_ref().to_path_buf()),
        }
    }

    #[must_use]
    pub const fn memory() -> Self {
        Self {
            location: SqliteLocation::Memory,
        }
    }

    pub fn open(&self) -> Result<Connection, SqliteConnectionError> {
        let connection = match &self.location {
            SqliteLocation::File(path) => Connection::open(path),
            SqliteLocation::Memory => Connection::open_in_memory(),
        }
        .map_err(SqliteConnectionError::Open)?;
        configure(
            &connection,
            matches!(&self.location, SqliteLocation::File(_)),
        )?;
        Ok(connection)
    }
}

fn configure(connection: &Connection, file_backed: bool) -> Result<(), SqliteConnectionError> {
    connection
        .busy_timeout(WRITE_WAIT)
        .map_err(SqliteConnectionError::Configure)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(SqliteConnectionError::Configure)?;
    if file_backed {
        // Changing journal mode is a database write. Serialize only that
        // one-time transition and do not rewrite WAL on every connection:
        // another freshly opened store may already be running its migration.
        let _change = JOURNAL_MODE_CHANGE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let journal: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(SqliteConnectionError::Configure)?;
        if !journal.eq_ignore_ascii_case("wal") {
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(SqliteConnectionError::Configure)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connection-policy cause/effect graph:
    /// C1 two independently constructed stores open the same file; C2 SQLite
    /// settings are connection-local; C3 the factory is their sole constructor;
    /// C4 the file is already WAL and another connection holds a write transaction.
    /// Effects are E1 both connections use WAL, E2 both enforce foreign keys,
    /// E3 both wait for a transient writer for the same bounded duration, and E4
    /// opening another connection does not attempt a redundant journal-mode write.
    ///
    /// | Rule | Shared file | Factory | Effect |
    /// |---|---|---|---|
    /// | P1 | yes | both | E1 + E2 + E3 equal policy |
    /// | P2 | memory | one | E2 + E3; journal remains SQLite `memory` |
    /// | P3 | yes + active writer | second | E1 + E4 without lock conflict |
    #[test]
    fn every_connection_receives_the_same_process_policy() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let factory = SqliteConnectionFactory::file(directory.path().join("process.db"));
        let first = factory.open().expect("first connection");
        let second = factory.open().expect("second connection");

        for connection in [&first, &second] {
            let journal: String = connection
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .expect("journal mode");
            let foreign_keys: i64 = connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                .expect("foreign keys");
            let busy_timeout_ms: i64 = connection
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .expect("busy timeout");
            assert_eq!(journal.to_ascii_lowercase(), "wal", "P1/E1");
            assert_eq!(foreign_keys, 1, "P1/E2");
            assert_eq!(busy_timeout_ms, 30_000, "P1/E3");
        }

        let memory = SqliteConnectionFactory::memory()
            .open()
            .expect("memory connection");
        let journal: String = memory
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("memory journal mode");
        let foreign_keys: i64 = memory
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("memory foreign keys");
        let busy_timeout_ms: i64 = memory
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("memory busy timeout");
        assert_eq!(journal.to_ascii_lowercase(), "memory", "P2");
        assert_eq!(foreign_keys, 1, "P2/E2");
        assert_eq!(busy_timeout_ms, 30_000, "P2/E3");
    }

    #[test]
    fn opening_an_existing_wal_file_does_not_rewrite_journal_mode() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let factory = SqliteConnectionFactory::file(directory.path().join("writer.db"));
        let mut writer = factory.open().expect("writer connection");
        let transaction = writer.transaction().expect("active writer");
        transaction
            .execute("CREATE TABLE authority (id INTEGER PRIMARY KEY)", [])
            .expect("writer owns the database");

        let observer = factory.open().expect("P3/E4 observer connection");
        let journal: String = observer
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("P3 journal mode");
        assert_eq!(journal.to_ascii_lowercase(), "wal", "P3/E1");
    }

    /// Scheduler-isolation cause/effect graph: C1 one synchronous SQLite
    /// operation owns the connection; C2 two async callers queue behind it on
    /// a two-thread Tokio runtime. Effects: E1 waiters consume blocking-pool
    /// capacity, not runtime workers; E2 an authority timer remains schedulable;
    /// E3 both database operations finish after the owner releases the lock.
    /// Decision rule S1=C1+C2=>E1+E2+E3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_contention_never_blocks_authority_timers() {
        let connection =
            SharedSqliteConnection::new(Connection::open_in_memory().expect("connection"));
        let held = connection.clone();
        let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().expect("S1 connection lock");
            held_tx.send(()).expect("S1 announce held connection");
            std::thread::sleep(Duration::from_millis(250));
        });
        held_rx.recv().expect("S1 connection held");

        let started = std::time::Instant::now();
        let authority_timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            started.elapsed()
        });
        let left = tokio::spawn(with_connection(connection.clone(), |_| 1_u8));
        let right = tokio::spawn(with_connection(connection.clone(), |_| 2_u8));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let timer_elapsed = tokio::time::timeout(Duration::from_millis(100), authority_timer)
            .await
            .expect("S1/E1-E2 authority timer remains schedulable")
            .expect("S1 authority timer task");
        assert!(
            timer_elapsed < Duration::from_millis(100),
            "S1/E2 timer fired after {timer_elapsed:?}; runtime workers were blocked"
        );

        holder.join().expect("S1 release connection");
        assert_eq!(left.await.expect("left task").expect("S1/E3"), 1);
        assert_eq!(right.await.expect("right task").expect("S1/E3"), 2);
    }

    /// Queue one caller behind the current owner *now*, in program order.
    ///
    /// Spawning a task and yielding does not establish queue order on a
    /// multi-thread runtime: under load the spawned task may not be polled
    /// before the next caller is spawned, so a fairness assertion would then
    /// test the scheduler instead of admission. Polling the admission future
    /// once in place registers it with the FIFO semaphore before this function
    /// returns, and the boxed future keeps that waiter when it is spawned.
    async fn enqueue<T, F>(
        connection: &SharedSqliteConnection,
        operation: F,
    ) -> tokio::task::JoinHandle<Result<T, SqliteTaskError>>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> T + Send + 'static,
    {
        let mut admission = Box::pin(with_connection(connection.clone(), operation));
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

    /// Hold the connection until the returned sender fires, reporting entry.
    async fn owner(
        connection: &SharedSqliteConnection,
        order: &Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
    ) -> (
        tokio::task::JoinHandle<Result<(), SqliteTaskError>>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let order = Arc::clone(order);
        let task = tokio::spawn(with_connection(connection.clone(), move |_| {
            order.lock().expect("owner order").push(name);
            entered_tx.send(()).expect("announce owner");
            release_rx.recv().expect("release owner");
        }));
        entered_rx.await.expect("owner entered");
        (task, release_tx)
    }

    fn record(
        order: &Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
    ) -> impl FnOnce(&mut Connection) + Send + 'static {
        let order = Arc::clone(order);
        move |_| order.lock().expect("order").push(name)
    }

    /// Fair-admission cause/effect graph. Causes: C1 one background operation
    /// owns the connection; C2 other callers are queued behind it in a known
    /// order (established by [`enqueue`], not by scheduler timing); C3 the same
    /// background loop requests again after them; C4 a queued caller is
    /// cancelled before admission; C5 the owning operation panics. Effects:
    /// E1 queued callers enter in queue order and a requeued background caller
    /// cannot overtake them; E2 every admitted operation runs exactly once;
    /// E3 a cancelled caller never runs and never blocks the callers behind
    /// it; E4 a panic releases admission instead of deadlocking, the panicking
    /// caller sees a join failure, and the connection then fails closed as
    /// poisoned for every later caller.
    ///
    /// | Rule | Owner | Queue | Effect |
    /// |---|---|---|---|
    /// | F1 | background | foreground, then background requeue | E1 + E2 |
    /// | F2 | background | three callers | E1 + E2 in exact queue order |
    /// | F3 | background | one cancelled caller, then one live | E3 + E2 |
    /// | F4 | panicking | one caller | E4 |
    ///
    /// Before `enqueue` existed, F1 spawned the foreground caller and yielded
    /// once; under full CPU saturation that let the requeue register first in
    /// 2 of 400 runs, so the rule tested scheduling luck rather than C2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_control_cannot_be_starved_by_hot_background_reentry() {
        let connection =
            SharedSqliteConnection::new(Connection::open_in_memory().expect("connection"));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (first_background, release) = owner(&connection, &order, "background-1").await;
        let foreground = enqueue(&connection, record(&order, "foreground")).await;
        let background_reentry = enqueue(&connection, record(&order, "background-2")).await;
        release.send(()).expect("F1 release first background");

        first_background.await.expect("F1 task").expect("F1/E2");
        foreground.await.expect("F1 task").expect("F1/E2");
        background_reentry.await.expect("F1 task").expect("F1/E2");
        assert_eq!(
            *order.lock().expect("F1 order"),
            ["background-1", "foreground", "background-2"],
            "F1/E1-E2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_callers_enter_in_exactly_the_order_they_queued() {
        let connection =
            SharedSqliteConnection::new(Connection::open_in_memory().expect("connection"));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (owner_task, release) = owner(&connection, &order, "owner").await;
        let first = enqueue(&connection, record(&order, "first")).await;
        let second = enqueue(&connection, record(&order, "second")).await;
        let third = enqueue(&connection, record(&order, "third")).await;
        release.send(()).expect("F2 release owner");

        for task in [owner_task, first, second, third] {
            task.await.expect("F2 task").expect("F2/E2");
        }
        assert_eq!(
            *order.lock().expect("F2 order"),
            ["owner", "first", "second", "third"],
            "F2/E1-E2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_waiter_never_runs_and_never_blocks_the_queue() {
        let connection =
            SharedSqliteConnection::new(Connection::open_in_memory().expect("connection"));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (owner_task, release) = owner(&connection, &order, "owner").await;
        let cancelled = enqueue(&connection, record(&order, "cancelled")).await;
        let live = enqueue(&connection, record(&order, "live")).await;
        cancelled.abort();
        assert!(
            cancelled.await.expect_err("F3 cancelled").is_cancelled(),
            "F3 the waiter was cancelled before admission"
        );
        release.send(()).expect("F3 release owner");

        owner_task.await.expect("F3 task").expect("F3/E2");
        tokio::time::timeout(Duration::from_secs(5), live)
            .await
            .expect("F3/E3 the live caller is not stuck behind the cancelled one")
            .expect("F3 task")
            .expect("F3/E2");
        assert_eq!(
            *order.lock().expect("F3 order"),
            ["owner", "live"],
            "F3/E3 the cancelled operation never ran"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_operation_releases_admission_and_fails_closed() {
        let connection =
            SharedSqliteConnection::new(Connection::open_in_memory().expect("connection"));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let panicking = tokio::spawn(with_connection(connection.clone(), move |_| {
            entered_tx.send(()).expect("F4 announce owner");
            release_rx.recv().expect("F4 release owner");
            panic!("F4 operation fails mid-flight");
        }));
        entered_rx.await.expect("F4 owner entered");
        let waiter = enqueue(&connection, |_| ()).await;
        release_tx.send(()).expect("F4 release owner");

        let failure = panicking
            .await
            .expect("F4 task")
            .expect_err("F4/E4 the panicking caller sees a failure");
        assert!(
            matches!(failure, SqliteTaskError::Join(ref join) if join.is_panic()),
            "F4/E4 a panic is reported as a join failure, got {failure:?}"
        );
        let waiter = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("F4/E4 admission was released, not deadlocked")
            .expect("F4 task");
        assert!(
            matches!(waiter, Err(SqliteTaskError::Poisoned)),
            "F4/E4 a connection a panic left mid-operation is not handed out again"
        );
        let later = with_connection(connection, |_| ()).await;
        assert!(
            matches!(later, Err(SqliteTaskError::Poisoned)),
            "F4/E4 the connection stays fail-closed for every later caller"
        );
    }
}
