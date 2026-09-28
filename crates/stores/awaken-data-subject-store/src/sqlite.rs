// @formal entrypoint data_subject.revision_is_monotonic
// @formal entrypoint data_subject.stale_writer_never_applies
// @formal entrypoint erasure.checkpoint_never_forgets_completed_targets
// @formal entrypoint erasure.concurrent_replicas_have_one_checkpoint_winner
//! SQLite adapter for the data-subject application, over the crate's own
//! `control_data_subject` migration scope ([`control_data_subject_bundle`]). The subject
//! aggregate serializes into the `data {json}` column; `id`/`org` are keyed
//! columns for lookups.

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{CONTROL_PREFIX, control_data_subject_bundle};
use awaken_data_subject_application::{
    DataSubject, DataSubjectError, DataSubjectId, DataSubjectRepo, ErasureJobRepo, ErasureProgress,
};

/// The component's table namespace (its bundle prefix).
const NS: &str = CONTROL_PREFIX;

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

fn storage(err: impl std::fmt::Display) -> DataSubjectError {
    DataSubjectError::Storage(err.to_string())
}

async fn with_conn<T, F>(
    conn: &awaken_sqlite_pool::SharedSqliteConnection,
    f: F,
) -> Result<T, DataSubjectError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection, &str) -> Result<T, DataSubjectError> + Send + 'static,
{
    awaken_sqlite_pool::with_connection(conn.clone(), move |connection| f(connection, NS))
        .await
        .map_err(storage)?
}

/// A SQLite-backed [`DataSubjectRepo`].
pub struct SqliteDataSubjectRepo {
    conn: awaken_sqlite_pool::SharedSqliteConnection,
}

impl SqliteDataSubjectRepo {
    /// Open (or create) a database file and apply the data-subject migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let store = Self::over(
            awaken_sqlite_pool::SqliteConnectionFactory::file(path)
                .open()
                .map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(
            awaken_sqlite_pool::SqliteConnectionFactory::memory()
                .open()
                .map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `data_subject` scope so this store
    /// shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: awaken_sqlite_pool::SharedSqliteConnection::new(conn),
        }
    }

    /// Apply the `data_subject` scoped migration bundle (idempotent). Optional: skip
    /// it when the schema is owned externally.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("data_subject connection poisoned".to_string()))?;
        let bundle =
            control_data_subject_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl DataSubjectRepo for SqliteDataSubjectRepo {
    async fn create(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let id = subject.id.0.clone();
        let org = subject.org.clone();
        let revision = i64::try_from(subject.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(id.clone()))?;
        let data = serde_json::to_string(&subject).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            let inserted = conn
                .execute(
                    &format!(
                        "INSERT OR IGNORE INTO {p}_subject (id, org, data, revision) \
                     VALUES (?1, ?2, ?3, ?4)"
                    ),
                    params![id, org, data, revision],
                )
                .map_err(storage)?;
            if inserted == 1 {
                Ok(())
            } else {
                Err(DataSubjectError::AlreadyExists(id))
            }
        })
        .await
    }

    async fn compare_and_swap(
        &self,
        expected_revision: u64,
        subject: DataSubject,
    ) -> Result<(), DataSubjectError> {
        let id = subject.id.0.clone();
        let org = subject.org.clone();
        let revision = subject.revision;
        if !subject.is_successor_of(expected_revision) {
            return Err(DataSubjectError::Conflict(id));
        }
        let stored_revision =
            i64::try_from(revision).map_err(|_| DataSubjectError::RevisionExhausted(id.clone()))?;
        let stored_expected = i64::try_from(expected_revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(id.clone()))?;
        let data = serde_json::to_string(&subject).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current: Option<(String, i64)> = tx
                .query_row(
                    &format!("SELECT data, revision FROM {p}_subject WHERE id = ?1"),
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(storage)?;
            let Some((current_data, current_revision)) = current else {
                return Err(DataSubjectError::Conflict(id));
            };
            let current: DataSubject = serde_json::from_str(&current_data).map_err(storage)?;
            if i64::try_from(current.revision).ok() != Some(current_revision) {
                return Err(storage("data subject revision mismatch"));
            }
            if !subject.follows_revision(expected_revision, current.revision) {
                return Err(DataSubjectError::Conflict(id));
            }
            let updated = tx
                .execute(
                    &format!(
                        "UPDATE {p}_subject SET org = ?1, data = ?2, revision = ?3 \
                         WHERE id = ?4 AND revision = ?5"
                    ),
                    params![org, data, stored_revision, id, stored_expected],
                )
                .map_err(storage)?;
            if updated == 1 {
                tx.commit().map_err(storage)?;
                Ok(())
            } else {
                Err(DataSubjectError::Conflict(id))
            }
        })
        .await
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_subject WHERE id = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            let data = data.ok_or_else(|| DataSubjectError::NotFound(id.clone()))?;
            serde_json::from_str(&data).map_err(storage)
        })
        .await
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        let org = org.to_string();
        with_conn(&self.conn, move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT data FROM {p}_subject WHERE org = ?1 ORDER BY rowid"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map(params![org], |r| r.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|data| serde_json::from_str(&data.map_err(storage)?).map_err(storage))
                .collect()
        })
        .await
    }
}

#[async_trait::async_trait]
impl ErasureJobRepo for SqliteDataSubjectRepo {
    async fn load(&self, id: &DataSubjectId) -> Result<Option<ErasureProgress>, DataSubjectError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_erasure_job WHERE subject_id = ?1"),
                    params![id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            data.map(|value| serde_json::from_str(&value).map_err(storage))
                .transpose()
        })
        .await
    }

    async fn compare_and_swap_progress(
        &self,
        id: &DataSubjectId,
        expected_revision: Option<u64>,
        progress: &ErasureProgress,
    ) -> Result<(), DataSubjectError> {
        let id = id.0.clone();
        let progress = progress.clone();
        with_conn(&self.conn, move |conn, p| {
            // IMMEDIATE reserves the writer before reading. The old JSON and
            // revision, admission, and conditional update share this transaction;
            // neither another connection nor this connection can replace the
            // observed checkpoint between admission and publication.
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let current: Option<(String, i64)> = tx
                .query_row(
                    &format!("SELECT data, revision FROM {p}_erasure_job WHERE subject_id = ?1"),
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(storage)?;
            let current = current
                .map(|(data, revision)| {
                    let current: ErasureProgress = serde_json::from_str(&data).map_err(storage)?;
                    if i64::try_from(current.revision).ok() != Some(revision) {
                        return Err(storage("erasure checkpoint revision mismatch"));
                    }
                    Ok(current)
                })
                .transpose()?;
            if !progress.follows(expected_revision, current.as_ref()) {
                return Err(DataSubjectError::Conflict(id));
            }
            let revision = i64::try_from(progress.revision)
                .map_err(|_| DataSubjectError::RevisionExhausted(id.clone()))?;
            let expected = expected_revision
                .map(i64::try_from)
                .transpose()
                .map_err(|_| DataSubjectError::RevisionExhausted(id.clone()))?;
            let data = serde_json::to_string(&progress).map_err(storage)?;
            let changed = match expected {
                None => tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO {p}_erasure_job (subject_id, data, revision) \
                         VALUES (?1, ?2, ?3)"
                    ),
                    params![id, data, revision],
                ),
                Some(expected) => tx.execute(
                    &format!(
                        "UPDATE {p}_erasure_job SET data = ?2, revision = ?3, \
                         updated_at = CURRENT_TIMESTAMP WHERE subject_id = ?1 AND revision = ?4"
                    ),
                    params![id, data, revision, expected],
                ),
            }
            .map_err(storage)?;
            if changed == 1 {
                tx.commit().map_err(storage)?;
                Ok(())
            } else {
                Err(DataSubjectError::Conflict(id))
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The injection seam: a caller supplies its own connection, `over` wraps it
    /// WITHOUT migrating, and `ensure_schema` applies the `data_subject` scope — so a
    /// unified migration pipeline can own the schema and this store shares the
    /// caller's database (mirrors the resource-family stores' `over`/`ensure_schema`).
    #[tokio::test]
    async fn over_then_ensure_schema_round_trips_on_a_caller_owned_connection() {
        let conn = Connection::open_in_memory().unwrap();
        let repo = SqliteDataSubjectRepo::over(conn);
        repo.ensure_schema().unwrap();
        repo.create(DataSubject::new(
            DataSubjectId("dsub_a".into()),
            "org_1",
            100,
        ))
        .await
        .unwrap();
        assert_eq!(
            repo.get(&DataSubjectId("dsub_a".into())).await.unwrap().org,
            "org_1"
        );
    }

    #[tokio::test]
    async fn subject_cas_rejects_revision_drift_and_malformed_truth_without_writes() {
        // Causes: C1 JSON revision disagrees with the indexed revision, or C2
        // the stored aggregate is malformed. Effect: both rules return Storage
        // and preserve the exact bytes/index; CAS never repairs or overwrites an
        // unverified current row. Valid-row admission is covered by repository
        // conformance, so these are the two corrupt-state decision-table rules.
        let old = DataSubject::new(DataSubjectId("corrupt-subject".into()), "org_1", 0);
        for (data, stored_revision) in [
            (serde_json::to_string(&old).unwrap(), 1_i64),
            ("not json".to_string(), 0_i64),
        ] {
            let repo = SqliteDataSubjectRepo::open_in_memory().unwrap();
            repo.conn
                .lock()
                .unwrap()
                .execute(
                    &format!("INSERT INTO {NS}_subject(id,org,data,revision) VALUES (?1,?2,?3,?4)"),
                    params![&old.id.0, &old.org, &data, stored_revision],
                )
                .unwrap();
            let mut candidate = old.clone();
            candidate.revision = 1;
            candidate.name = Some("must-not-apply".into());
            assert!(matches!(
                repo.compare_and_swap(0, candidate).await,
                Err(DataSubjectError::Storage(_))
            ));
            let raw: (String, i64) = repo
                .conn
                .lock()
                .unwrap()
                .query_row(
                    &format!("SELECT data,revision FROM {NS}_subject WHERE id=?1"),
                    params![&old.id.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(raw, (data, stored_revision));
        }
    }

    #[tokio::test]
    async fn erasure_cas_rejects_revision_drift_and_storage_exhaustion_without_writes() {
        // Causes: old JSON revision differs from indexed SQL revision, malformed
        // JSON, or valid signed MAX old revision has no SQL-storable successor.
        // Rules D1/D2 -> Storage, D3 -> RevisionExhausted. Each rejection preserves
        // the exact raw JSON and indexed revision, including corrupt historical
        // bytes; no repair or normalizing rewrite is allowed.
        let old = ErasureProgress {
            revision: 0,
            completed_targets: vec![awaken_data_subject_application::ErasureTarget::Coordinator],
            records_removed: 2,
            accountability_stamped: false,
            complete: false,
        };
        let maximum = ErasureProgress {
            revision: i64::MAX as u64,
            ..old.clone()
        };
        for (data, stored_revision, expected, candidate_revision, exhaustion) in [
            (serde_json::to_string(&old).unwrap(), 1_i64, 0, 1, false),
            ("not json".to_string(), 0_i64, 0, 1, false),
            (
                serde_json::to_string(&maximum).unwrap(),
                i64::MAX,
                maximum.revision,
                maximum.revision + 1,
                true,
            ),
        ] {
            let repo = SqliteDataSubjectRepo::open_in_memory().unwrap();
            let id = DataSubjectId("corrupt-erasure".into());
            repo.conn
                .lock()
                .unwrap()
                .execute(
                    &format!(
                        "INSERT INTO {NS}_erasure_job(subject_id,data,revision) VALUES (?1,?2,?3)"
                    ),
                    params![id.0, data, stored_revision],
                )
                .unwrap();
            let candidate = ErasureProgress {
                revision: candidate_revision,
                completed_targets: vec![
                    awaken_data_subject_application::ErasureTarget::Coordinator,
                ],
                records_removed: 2,
                accountability_stamped: false,
                complete: false,
            };
            let error = repo
                .compare_and_swap_progress(&id, Some(expected), &candidate)
                .await
                .unwrap_err();
            if exhaustion {
                assert!(matches!(error, DataSubjectError::RevisionExhausted(_)));
            } else {
                assert!(matches!(error, DataSubjectError::Storage(_)));
            }
            let raw: (String, i64) = repo
                .conn
                .lock()
                .unwrap()
                .query_row(
                    &format!("SELECT data,revision FROM {NS}_erasure_job WHERE subject_id=?1"),
                    params![id.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(raw, (data, stored_revision));
        }
    }
}
