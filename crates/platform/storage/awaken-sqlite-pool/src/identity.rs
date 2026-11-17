// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: embedded SQLite opened-database identity (ADR-0087).
// @formal reason: Dynamic path: a paired owner may transfer its original transaction only when the
// @formal reason: observed main file, retained native identity and current filename agree. Independent
// @formal reason: memory/file databases stay distinct; replacement and journal aliases fail closed.
// @formal reason: Evidence: adjacent native identity tests and guarded admission conformance. This
// @formal reason: is a requirement classification, not a formal-proof claim.
//! An observation of the opened main database, never a caller-supplied identity.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

/// Which file a path names. On Unix this is the device and inode from `stat`,
/// read without opening the file: closing any descriptor on a file drops every
/// POSIX lock this process holds on it, including the locks SQLite's open
/// connections rely on. Another process could then checkpoint and delete the
/// WAL those connections keep writing to. The pool's own connection stays open
/// for the pool's lifetime, so the inode cannot be reused while it is observed.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path)?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Elsewhere locks belong to the handle that took them, so an opened handle
/// is a safe identity.
#[cfg(not(unix))]
type FileIdentity = same_file::Handle;

#[cfg(not(unix))]
fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    same_file::Handle::from_path(path)
}

pub(crate) struct DatabaseFile {
    path: PathBuf,
    opened: FileIdentity,
}

impl DatabaseFile {
    pub(crate) async fn observe(pool: &SqlitePool) -> Result<Option<Self>, sqlx::Error> {
        let databases: Vec<(i64, String, String)> = sqlx::query_as("PRAGMA database_list")
            .fetch_all(pool)
            .await?;
        let filename = databases
            .into_iter()
            .find(|(_, name, _)| name == "main")
            .ok_or_else(|| sqlx::Error::Protocol("SQLite main database is absent".into()))?
            .2;
        // SQLite reports an empty filename for a private memory database.
        if filename.is_empty() {
            return Ok(None);
        }
        let path = std::fs::canonicalize(filename)?;
        let opened = file_identity(&path)?;
        Ok(Some(Self { path, opened }))
    }

    pub(crate) fn same_current_file(&self, other: &Self) -> Result<bool, sqlx::Error> {
        // Journal identity follows the opened canonical filename. Hard-link
        // aliases with different journals must never transfer a transaction.
        // The retained identity also detects a removed/replaced file: matching
        // path strings alone cannot make a foreign pool a transaction owner.
        let current = file_identity(&self.path)?;
        let other_current = file_identity(&other.path)?;
        if current != self.opened || other_current != other.opened {
            return Err(sqlx::Error::Protocol(
                "SQLite database file was replaced".into(),
            ));
        }
        if self.opened == other.opened && self.path != other.path {
            return Err(sqlx::Error::Protocol(
                "SQLite database has distinct journal aliases".into(),
            ));
        }
        Ok(self.opened == other.opened)
    }
}

#[cfg(test)]
mod tests {
    use crate::EmbeddedSqlite;

    #[tokio::test]
    async fn identity_comes_from_opened_database_without_acquiring_its_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("process.db");
        let first = EmbeddedSqlite::file(&file).open().await.unwrap();
        let second = EmbeddedSqlite::file(&file).open().await.unwrap();
        let distinct = EmbeddedSqlite::file(directory.path().join("other.db"))
            .open()
            .await
            .unwrap();
        let tx = first.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(first.shares_database(&first.clone()).unwrap());
        assert!(first.shares_database(&second).unwrap());
        assert!(!first.shares_database(&distinct).unwrap());
        tx.rollback().await.unwrap();
        let memory = EmbeddedSqlite::memory().open().await.unwrap();
        let other_memory = EmbeddedSqlite::memory().open().await.unwrap();
        assert!(memory.shares_database(&memory.clone()).unwrap());
        assert!(!memory.shares_database(&other_memory).unwrap());
        assert!(!memory.shares_database(&first).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_link_alias_and_replaced_filename_do_not_prove_transaction_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("process.db");
        let first = EmbeddedSqlite::file(&file).open().await.unwrap();
        let alias = directory.path().join("alias.db");
        std::fs::hard_link(&file, &alias).unwrap();
        let aliased = EmbeddedSqlite::file(&alias).open().await.unwrap();
        assert!(first.shares_database(&aliased).is_err());
        // Keep the old file handles alive while replacing the directory entry.
        std::fs::rename(&file, directory.path().join("old.db")).unwrap();
        std::fs::write(&file, []).unwrap();
        assert!(first.shares_database(&first.clone()).is_err());
        assert!(first.shares_database(&aliased).is_err());
    }

    /// Set in the child process of the case below: it is the other process.
    #[cfg(unix)]
    const OTHER_PROCESS: &str = "AWAKEN_SQLITE_IDENTITY_OTHER_PROCESS";
    #[cfg(unix)]
    const HELD_DATABASE_CASE: &str =
        "identity::tests::identity_check_keeps_the_locks_that_hold_the_database_open";

    /// Another process asks SQLite for the whole database without waiting.
    /// While any connection of this process has it open, SQLite must refuse.
    #[cfg(unix)]
    fn another_process_is_refused_the_database(file: &std::path::Path, rule: &str) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", HELD_DATABASE_CASE, "--nocapture"])
            .env(OTHER_PROCESS, file)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "{rule}: the other process took the database: {status}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_check_keeps_the_locks_that_hold_the_database_open() {
        // Causes: C1 this process holds a WAL database open through its pools;
        // C2 it asks whether two pools share that database; C3 another process
        // asks SQLite for exclusive access without waiting. Effect E1: C3 is
        // refused, and the WAL stays. R0 C1+C3 => E1 (control); R1 C1+C2+C3
        // => E1. Opening and closing a descriptor on the database file in C2
        // drops every POSIX lock this process holds on it: C3 is then granted
        // the database, and any other process that closes it checkpoints and
        // deletes the WAL our connections keep writing to, so their later
        // commits are invisible to every other process and lost if this one
        // dies.
        use sqlx::Connection as _;
        if let Some(file) = std::env::var_os(OTHER_PROCESS) {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(file)
                .busy_timeout(std::time::Duration::ZERO)
                .locking_mode(sqlx::sqlite::SqliteLockingMode::Exclusive);
            let refusal = match sqlx::sqlite::SqliteConnection::connect_with(&options).await {
                Err(error) => error.to_string(),
                Ok(mut other) => match sqlx::query_scalar::<_, i64>("SELECT count(*) FROM fact")
                    .fetch_one(&mut other)
                    .await
                {
                    Err(error) => error.to_string(),
                    Ok(_) => panic!("another process was granted the open database"),
                },
            };
            assert!(refusal.contains("locked"), "{refusal}");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("process.db");
        let wal = directory.path().join("process.db-wal");
        let first = EmbeddedSqlite::file(&file).open().await.unwrap();
        let second = EmbeddedSqlite::file(&file).open().await.unwrap();
        sqlx::query("CREATE TABLE fact (id INTEGER)")
            .execute(&first)
            .await
            .unwrap();
        sqlx::query("INSERT INTO fact VALUES (1)")
            .execute(&first)
            .await
            .unwrap();
        another_process_is_refused_the_database(&file, "R0");
        assert!(first.shares_database(&second).unwrap());
        another_process_is_refused_the_database(&file, "R1");
        assert!(wal.exists(), "R1 the WAL our connections write to stays");
    }
}
