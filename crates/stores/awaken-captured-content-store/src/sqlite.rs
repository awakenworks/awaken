// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This source implements the repository-controlled store product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Coordinator SQLite captured-content store (ADR-0050): the durable counterpart of
//! the test-only in-memory captured-content fixture. Rows
//! are subject-tagged so GDPR erasure is a keyed `DELETE`, and a TTL sweep
//! enforces storage limitation. Implements both [`CaptureSink`] (write) and
//! [`ContentEraser`] (erase) over the Coordinator-owned
//! `coordinator_data_capture` migration scope.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::{
    CaptureError, CaptureSink, ContentEraser, ContentKind, DataSubjectId, Purpose,
};
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;

use crate::StoreError;
use crate::schema::{COORDINATOR_CAPTURE_PREFIX, coordinator_data_capture_bundle};

const NS: &str = COORDINATOR_CAPTURE_PREFIX;

async fn open_migrated(location: EmbeddedSqlite) -> Result<EmbeddedPool, StoreError> {
    let pool = location
        .open()
        .await
        .map_err(|e| StoreError::Open(e.to_string()))?;
    let bundle = coordinator_data_capture_bundle()
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    awaken_sqlite_pool::held(
        SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .map_err(|e| StoreError::Migrate(e.to_string()))?
            .run_bundle(&bundle),
    )
    .await
    .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(pool)
}

/// A SQLite-backed captured-content store.
pub struct SqliteCapturedContentStore {
    pool: EmbeddedPool,
    seq: AtomicU64,
    /// The wall clock capture records and erasure fences are stamped with.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl SqliteCapturedContentStore {
    /// Open (or create) a database file and apply the migrations.
    pub async fn open(
        path: &str,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            pool: open_migrated(EmbeddedSqlite::file(path)).await?,
            seq: AtomicU64::new(0),
            clock,
        })
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory(
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            pool: open_migrated(EmbeddedSqlite::memory()).await?,
            seq: AtomicU64::new(0),
            clock,
        })
    }

    /// Insert a captured item at an explicit time; returns its `cap_…` id.
    pub async fn insert(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
        now: i64,
    ) -> String {
        let sequence = self.seq.fetch_add(1, Ordering::SeqCst);
        self.try_insert(sequence, subject, purpose, content, now)
            .await
            .expect("insert captured content")
    }

    async fn try_insert(
        &self,
        sequence: u64,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
        now: i64,
    ) -> Result<String, CaptureError> {
        let id = format!("cap_{now}_{sequence:016}");
        let purpose = serde_json::to_string(&purpose).unwrap_or_default();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| CaptureError::Store(error.to_string()))?;
        let fenced = sqlx::query_scalar::<_, String>(&format!(
            "SELECT subject FROM {NS}_fence WHERE subject = ?1"
        ))
        .bind(&subject.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| CaptureError::Store(error.to_string()))?
        .is_some();
        if fenced {
            return Err(CaptureError::SubjectErased);
        }
        sqlx::query(&format!(
            "INSERT INTO {NS}_captured (id, subject, purpose, recorded_at, content, restricted) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0)"
        ))
        .bind(&id)
        .bind(&subject.0)
        .bind(&purpose)
        .bind(now)
        .bind(content)
        .execute(&mut *tx)
        .await
        .map_err(|error| CaptureError::Store(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| CaptureError::Store(error.to_string()))?;
        Ok(id)
    }

    /// Restrict a subject's records (GDPR Art. 18); returns the number restricted.
    pub async fn restrict(&self, subject: &DataSubjectId) -> usize {
        sqlx::query(&format!(
            "UPDATE {NS}_captured SET restricted = 1 WHERE subject = ?1 AND restricted = 0"
        ))
        .bind(&subject.0)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected() as usize)
        .unwrap_or(0)
    }

    /// Lift the restriction on a subject's records (Art. 18(3)); returns released.
    pub async fn release(&self, subject: &DataSubjectId) -> usize {
        sqlx::query(&format!(
            "UPDATE {NS}_captured SET restricted = 0 WHERE subject = ?1 AND restricted = 1"
        ))
        .bind(&subject.0)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected() as usize)
        .unwrap_or(0)
    }

    /// Remove records older than `ttl_millis` as of `now`; returns count swept.
    /// Restricted (Art. 18) records are exempt.
    pub async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        sqlx::query(&format!(
            "DELETE FROM {NS}_captured WHERE restricted = 0 AND ?1 - recorded_at >= ?2"
        ))
        .bind(now)
        .bind(ttl_millis)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected() as usize)
        .unwrap_or(0)
    }

    /// Current record count.
    pub async fn len(&self) -> usize {
        sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {NS}_captured"))
            .fetch_one(&self.pool)
            .await
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Whether the store is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
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
        let now = self.clock.now().as_epoch_ms_i64();
        self.try_insert(sequence, subject, purpose, content, now)
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl ContentEraser for SqliteCapturedContentStore {
    async fn erase_subject(
        &self,
        subject: &DataSubjectId,
    ) -> Result<usize, awaken_runtime_contract::ErasureError> {
        let erased_at = self.clock.now().as_epoch_ms_i64();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
        let previous = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT records_removed FROM {NS}_fence WHERE subject = ?1"
        ))
        .bind(&subject.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?
        .unwrap_or(0);
        // Restricted (Art. 18) rows survive erasure until released. A DELETE that
        // errors is surfaced (fail-closed) rather than swallowed to a `0` count.
        let removed = sqlx::query(&format!(
            "DELETE FROM {NS}_captured WHERE subject = ?1 AND restricted = 0"
        ))
        .bind(&subject.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| awaken_runtime_contract::ErasureError(e.to_string()))?
        .rows_affected();
        let receipt = previous.checked_add(removed as i64).ok_or_else(|| {
            awaken_runtime_contract::ErasureError("erasure receipt overflow".into())
        })?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_fence (subject, erased_at, records_removed) VALUES (?1, ?2, ?3) \
             ON CONFLICT(subject) DO UPDATE SET records_removed = excluded.records_removed"
        ))
        .bind(&subject.0)
        .bind(erased_at)
        .bind(receipt)
        .execute(&mut *tx)
        .await
        .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
        usize::try_from(receipt).map_err(|error| {
            awaken_runtime_contract::ErasureError(format!("invalid erasure receipt: {error}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn erase_and_ttl_over_sqlite() {
        let s = SqliteCapturedContentStore::open_in_memory(awaken_time::system_clock())
            .await
            .unwrap();
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x1",
            100,
        )
        .await;
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x2",
            100,
        )
        .await;
        s.insert(
            &DataSubjectId("b".into()),
            Purpose::EvalRecording,
            "y1",
            100,
        )
        .await;
        assert_eq!(s.len().await, 3);

        // Sink write goes to the same table.
        s.record(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            ContentKind::OutputMessages,
            "x3",
        )
        .await
        .unwrap();
        assert_eq!(s.len().await, 4);

        // Erase subject a → its 3 records gone, b remains.
        assert_eq!(
            s.erase_subject(&DataSubjectId("a".into())).await.unwrap(),
            3
        );
        assert_eq!(s.len().await, 1);

        // TTL sweep removes b (age 150 >= ttl 100 at now=250).
        assert_eq!(s.sweep_expired(100, 250).await, 1);
        assert!(s.is_empty().await);
    }

    #[tokio::test]
    async fn restricted_rows_survive_erasure_and_ttl_over_sqlite() {
        let s = SqliteCapturedContentStore::open_in_memory(awaken_time::system_clock())
            .await
            .unwrap();
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x",
            100,
        )
        .await;
        s.insert(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "y",
            100,
        )
        .await;
        assert_eq!(s.restrict(&DataSubjectId("a".into())).await, 2);

        // Erasure + TTL both skip restricted rows.
        assert_eq!(
            s.erase_subject(&DataSubjectId("a".into())).await.unwrap(),
            0
        );
        assert_eq!(s.sweep_expired(1, 10_000).await, 0);
        assert_eq!(s.len().await, 2);

        // Release, then erase works.
        assert_eq!(s.release(&DataSubjectId("a".into())).await, 2);
        assert_eq!(
            s.erase_subject(&DataSubjectId("a".into())).await.unwrap(),
            2
        );
        assert!(s.is_empty().await);
    }

    #[tokio::test]
    async fn sqlite_erasure_transaction_persists_the_capture_fence() {
        // Cause/effect decision table: R1 erase existing subject -> fence,
        // delete, and stable receipt commit atomically; R2 retry -> same receipt;
        // R3 later capture for it -> SubjectErased; R4 another subject ->
        // unaffected. Immediate transactions serialize write/erase without a
        // check-then-insert race.
        let store = SqliteCapturedContentStore::open_in_memory(awaken_time::system_clock())
            .await
            .unwrap();
        let erased = DataSubjectId("erased".into());
        store
            .insert(&erased, Purpose::TelemetryContent, "old", 1)
            .await;
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
        assert_eq!(store.len().await, 1, "R4");
    }
}
