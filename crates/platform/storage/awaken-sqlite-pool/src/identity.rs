// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: embedded SQLite opened-database identity (ADR-0087).
// @formal reason: Dynamic path: a paired owner may transfer its original transaction only when the
// @formal reason: observed main file, retained native identity and current filename agree. Independent
// @formal reason: memory/file databases stay distinct; replacement and journal aliases fail closed.
// @formal reason: Evidence: adjacent native identity tests and guarded admission conformance. This
// @formal reason: is a requirement classification, not a formal-proof claim.
//! An observation of the opened main database, never a caller-supplied identity.

use std::path::PathBuf;

use same_file::Handle;
use sqlx::SqlitePool;

pub(crate) struct DatabaseFile {
    path: PathBuf,
    opened: Handle,
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
        let opened = Handle::from_path(&path)?;
        Ok(Some(Self { path, opened }))
    }

    pub(crate) fn same_current_file(&self, other: &Self) -> Result<bool, sqlx::Error> {
        // Journal identity follows the opened canonical filename. Hard-link
        // aliases with different journals must never transfer a transaction.
        // Retained native handles also detect a removed/replaced file: matching
        // path strings alone cannot make a foreign pool a transaction owner.
        let current = Handle::from_path(&self.path)?;
        let other_current = Handle::from_path(&other.path)?;
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
}
