//! SQLite-backed captured-content store (ADR-0050): the durable counterpart of
//! [`InMemoryCapturedContentStore`](crate::InMemoryCapturedContentStore). Rows
//! are subject-tagged so GDPR erasure is a keyed `DELETE`, and a TTL sweep
//! enforces storage limitation. Implements both [`CaptureSink`] (write) and
//! [`ContentEraser`] (erase) over the crate's `data_subject` migration scope.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_runtime_contract::{CaptureSink, ContentEraser, ContentKind, DataSubjectId, Purpose};
use rusqlite::{Connection, params};

use crate::schema::data_subject_bundle;
use crate::sqlite::StoreError;

const NS: &str = "data_subject";

fn open_migrated(conn: Connection) -> Result<Arc<Mutex<Connection>>, StoreError> {
    let bundle = data_subject_bundle().map_err(|e| StoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|e| StoreError::Migrate(e.to_string()))?
        .run_bundle(&conn, &bundle)
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A SQLite-backed captured-content store.
pub struct SqliteCapturedContentStore {
    conn: Arc<Mutex<Connection>>,
    seq: AtomicU64,
}

impl SqliteCapturedContentStore {
    /// Open (or create) a database file and apply the migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Open(e.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
            seq: AtomicU64::new(0),
        })
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Open(e.to_string()))?;
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
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("cap_{now}_{n:016}");
        let purpose = serde_json::to_string(&purpose).unwrap_or_default();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            &format!(
                "INSERT INTO {NS}_captured (id, subject, purpose, recorded_at, content) \
                 VALUES (?1, ?2, ?3, ?4, ?5)"
            ),
            params![id, subject.0, purpose, now, content],
        );
        id
    }

    /// Remove records older than `ttl_millis` as of `now`; returns count swept.
    pub fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!("DELETE FROM {NS}_captured WHERE ?1 - recorded_at >= ?2"),
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
    ) {
        self.insert(subject, purpose, content, now_millis());
    }
}

#[async_trait]
impl ContentEraser for SqliteCapturedContentStore {
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!("DELETE FROM {NS}_captured WHERE subject = ?1"),
            params![subject.0],
        )
        .unwrap_or(0)
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
        .await;
        assert_eq!(s.len(), 4);

        // Erase subject a → its 3 records gone, b remains.
        assert_eq!(s.erase_subject(&DataSubjectId("a".into())).await, 3);
        assert_eq!(s.len(), 1);

        // TTL sweep removes b (age 150 >= ttl 100 at now=250).
        assert_eq!(s.sweep_expired(100, 250), 1);
        assert!(s.is_empty());
    }
}
