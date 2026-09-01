//! Coordinator SQLite captured-content store (ADR-0050): the durable counterpart of
//! the test-only in-memory captured-content fixture. Rows
//! are subject-tagged so GDPR erasure is a keyed `DELETE`, and a TTL sweep
//! enforces storage limitation. Implements both [`CaptureSink`] (write) and
//! [`ContentEraser`] (erase) over the Coordinator-owned
//! `coordinator_data_capture` migration scope.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_runtime_contract::{
    CaptureError, CaptureSink, ContentEraser, ContentKind, DataSubjectId, Purpose,
};
use awaken_sqlite_runtime::{SharedSqliteConnection, SqliteConnectionFactory};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::StoreError;
use crate::schema::{COORDINATOR_CAPTURE_PREFIX, coordinator_data_capture_bundle};

const NS: &str = COORDINATOR_CAPTURE_PREFIX;

fn open_migrated(conn: Connection) -> Result<SharedSqliteConnection, StoreError> {
    let bundle = coordinator_data_capture_bundle()
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|e| StoreError::Migrate(e.to_string()))?
        .run_bundle(&conn, &bundle)
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(SharedSqliteConnection::new(conn))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A SQLite-backed captured-content store.
pub struct SqliteCapturedContentStore {
    conn: SharedSqliteConnection,
    seq: AtomicU64,
}

impl SqliteCapturedContentStore {
    /// Open (or create) a database file and apply the migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = SqliteConnectionFactory::file(path)
            .open()
            .map_err(|e| StoreError::Open(e.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
            seq: AtomicU64::new(0),
        })
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = SqliteConnectionFactory::memory()
            .open()
            .map_err(|e| StoreError::Open(e.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
            seq: AtomicU64::new(0),
        })
    }

    /// Insert a captured item at an explicit time; returns its `cap_…` id.
    pub fn insert(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
        now: i64,
    ) -> String {
        self.try_insert(subject, purpose, content, now)
            .expect("insert captured content")
    }

    fn try_insert(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
        now: i64,
    ) -> Result<String, CaptureError> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let mut conn = self
            .conn
            .lock()
            .map_err(|error| CaptureError::Store(error.to_string()))?;
        Self::try_insert_on(&mut conn, n, subject, purpose, content, now)
    }

    fn try_insert_on(
        conn: &mut Connection,
        sequence: u64,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
        now: i64,
    ) -> Result<String, CaptureError> {
        let id = format!("cap_{now}_{sequence:016}");
        let purpose = serde_json::to_string(&purpose).unwrap_or_default();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| CaptureError::Store(error.to_string()))?;
        let fenced = tx
            .query_row(
                &format!("SELECT subject FROM {NS}_fence WHERE subject = ?1"),
                params![subject.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| CaptureError::Store(error.to_string()))?
            .is_some();
        if fenced {
            return Err(CaptureError::SubjectErased);
        }
        tx.execute(
            &format!(
                "INSERT INTO {NS}_captured (id, subject, purpose, recorded_at, content, restricted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)"
            ),
            params![id, subject.0, purpose, now, content],
        )
        .map_err(|error| CaptureError::Store(error.to_string()))?;
        tx.commit()
            .map_err(|error| CaptureError::Store(error.to_string()))?;
        Ok(id)
    }

    /// Restrict a subject's records (GDPR Art. 18); returns the number restricted.
    pub fn restrict(&self, subject: &DataSubjectId) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!(
                "UPDATE {NS}_captured SET restricted = 1 WHERE subject = ?1 AND restricted = 0"
            ),
            params![subject.0],
        )
        .unwrap_or(0)
    }

    /// Lift the restriction on a subject's records (Art. 18(3)); returns released.
    pub fn release(&self, subject: &DataSubjectId) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!(
                "UPDATE {NS}_captured SET restricted = 0 WHERE subject = ?1 AND restricted = 1"
            ),
            params![subject.0],
        )
        .unwrap_or(0)
    }

    /// Remove records older than `ttl_millis` as of `now`; returns count swept.
    /// Restricted (Art. 18) records are exempt.
    pub fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!("DELETE FROM {NS}_captured WHERE restricted = 0 AND ?1 - recorded_at >= ?2"),
            params![now, ttl_millis],
        )
        .unwrap_or(0)
    }

    /// Current record count.
    #[must_use]
    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {NS}_captured"), [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl CaptureSink for SqliteCapturedContentStore {
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        _kind: ContentKind,
        content: &str,
    ) -> Result<(), CaptureError> {
        let sequence = self.seq.fetch_add(1, Ordering::SeqCst);
        let subject = subject.clone();
        let content = content.to_string();
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
            Self::try_insert_on(conn, sequence, &subject, purpose, &content, now_millis())
                .map(|_| ())
        })
        .await
        .map_err(|error| CaptureError::Store(error.to_string()))?
    }
}

#[async_trait]
impl ContentEraser for SqliteCapturedContentStore {
    async fn erase_subject(
        &self,
        subject: &DataSubjectId,
    ) -> Result<usize, awaken_runtime_contract::ErasureError> {
        let subject = subject.clone();
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
            let previous = tx
                .query_row(
                    &format!("SELECT records_removed FROM {NS}_fence WHERE subject = ?1"),
                    params![subject.0],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?
                .unwrap_or(0);
            // Restricted (Art. 18) rows survive erasure until released. A DELETE that
            // errors is surfaced (fail-closed) rather than swallowed to a `0` count.
            let removed = tx
                .execute(
                    &format!("DELETE FROM {NS}_captured WHERE subject = ?1 AND restricted = 0"),
                    params![subject.0],
                )
                .map_err(|e| awaken_runtime_contract::ErasureError(e.to_string()))?;
            let receipt = previous.checked_add(removed as i64).ok_or_else(|| {
                awaken_runtime_contract::ErasureError("erasure receipt overflow".into())
            })?;
            tx.execute(
            &format!(
                "INSERT INTO {NS}_fence (subject, erased_at, records_removed) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(subject) DO UPDATE SET records_removed = excluded.records_removed"
            ),
            params![subject.0, now_millis(), receipt],
        )
        .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
            tx.commit()
                .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
            usize::try_from(receipt).map_err(|error| {
                awaken_runtime_contract::ErasureError(format!("invalid erasure receipt: {error}"))
            })
        })
        .await
        .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn erase_and_ttl_over_sqlite() {
        let s = SqliteCapturedContentStore::open_in_memory().unwrap();
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x1",
            100,
        );
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x2",
            100,
        );
        s.insert(
            &DataSubjectId("b".into()),
            Purpose::EvalRecording,
            "y1",
            100,
        );
        assert_eq!(s.len(), 3);

        // Sink write goes to the same table.
        s.record(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            ContentKind::OutputMessages,
            "x3",
        )
        .await
        .unwrap();
        assert_eq!(s.len(), 4);

        // Erase subject a → its 3 records gone, b remains.
        assert_eq!(
            s.erase_subject(&DataSubjectId("a".into())).await.unwrap(),
            3
        );
        assert_eq!(s.len(), 1);

        // TTL sweep removes b (age 150 >= ttl 100 at now=250).
        assert_eq!(s.sweep_expired(100, 250), 1);
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn restricted_rows_survive_erasure_and_ttl_over_sqlite() {
        let s = SqliteCapturedContentStore::open_in_memory().unwrap();
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x",
            100,
        );
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "y",
            100,
        );
        assert_eq!(s.restrict(&DataSubjectId("a".into())), 2);

        // Erasure + TTL both skip restricted rows.
        assert_eq!(
            s.erase_subject(&DataSubjectId("a".into())).await.unwrap(),
            0
        );
        assert_eq!(s.sweep_expired(1, 10_000), 0);
        assert_eq!(s.len(), 2);

        // Release, then erase works.
        assert_eq!(s.release(&DataSubjectId("a".into())), 2);
        assert_eq!(
            s.erase_subject(&DataSubjectId("a".into())).await.unwrap(),
            2
        );
        assert!(s.is_empty());
    }

    #[tokio::test]
    async fn sqlite_erasure_transaction_persists_the_capture_fence() {
        // Cause/effect decision table: R1 erase existing subject -> fence,
        // delete, and stable receipt commit atomically; R2 retry -> same receipt;
        // R3 later capture for it -> SubjectErased; R4 another subject ->
        // unaffected. Immediate transactions serialize write/erase without a
        // check-then-insert race.
        let store = SqliteCapturedContentStore::open_in_memory().unwrap();
        let erased = DataSubjectId("erased".into());
        store.insert(&erased, Purpose::TelemetryContent, "old", 1);
        assert_eq!(store.erase_subject(&erased).await.unwrap(), 1, "R1");
        assert_eq!(store.erase_subject(&erased).await.unwrap(), 1, "R2");
        assert_eq!(
            store
                .record(
                    &erased,
                    Purpose::TelemetryContent,
                    ContentKind::InputMessages,
                    "late",
                )
                .await,
            Err(CaptureError::SubjectErased),
            "R3"
        );
        store
            .record(
                &DataSubjectId("other".into()),
                Purpose::TelemetryContent,
                ContentKind::InputMessages,
                "allowed",
            )
            .await
            .unwrap();
        assert_eq!(store.len(), 1, "R4");
    }
}
