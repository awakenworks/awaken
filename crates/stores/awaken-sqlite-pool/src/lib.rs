//! Canonical SQLite connection policy for embedded process stores.
//!
//! Store adapters continue to own schemas, migrations, transactions, and domain
//! error mapping. This crate owns only connection-local process policy so two
//! adapters opening the same file cannot silently disagree about locking or
//! foreign-key enforcement.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;

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
}
