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

    /// Fair-admission cause/effect graph: C1 one background operation owns the
    /// connection; C2 a foreground Control operation queues next; C3 the same
    /// background loop immediately requests another operation. Effects: E1 the
    /// foreground operation enters before the requeued background operation;
    /// E2 every operation runs exactly once; E3 no second blocking waiter is
    /// created while the first owns the connection.
    ///
    /// | Rule | Owner | Next waiter | Requeue | Effect |
    /// |---|---|---|---|---|
    /// | F1 | background | foreground | background | E1 + E2 + E3 |
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreground_control_cannot_be_starved_by_hot_background_reentry() {
        let connection =
            SharedSqliteConnection::new(Connection::open_in_memory().expect("connection"));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);

        let background_order = Arc::clone(&order);
        let first_background = tokio::spawn(with_connection(connection.clone(), move |_| {
            background_order
                .lock()
                .expect("F1 order")
                .push("background-1");
            entered_tx.send(()).expect("F1 announce owner");
            release_rx.recv().expect("F1 release owner");
        }));
        entered_rx.await.expect("F1 background entered");

        let foreground_order = Arc::clone(&order);
        let foreground = tokio::spawn(with_connection(connection.clone(), move |_| {
            foreground_order
                .lock()
                .expect("F1 order")
                .push("foreground");
        }));
        tokio::task::yield_now().await;

        let reentry_order = Arc::clone(&order);
        let background_reentry = tokio::spawn(with_connection(connection, move |_| {
            reentry_order.lock().expect("F1 order").push("background-2");
        }));
        release_tx.send(()).expect("F1 release first background");

        first_background.await.expect("F1 task").expect("F1/E2");
        foreground.await.expect("F1 task").expect("F1/E2");
        background_reentry.await.expect("F1 task").expect("F1/E2");
        assert_eq!(
            *order.lock().expect("F1 order"),
            ["background-1", "foreground", "background-2"],
            "F1/E1-E3"
        );
    }
}
