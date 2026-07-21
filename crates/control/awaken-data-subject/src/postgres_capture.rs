//! Postgres-backed captured-content store (feature `postgres`, ADR-0050 D7): the
//! network-DB sibling of
//! [`SqliteCapturedContentStore`](crate::SqliteCapturedContentStore). Rows are
//! subject-tagged so GDPR erasure is a keyed `DELETE`, a TTL sweep enforces storage
//! limitation, and an Art. 18 `restricted` flag exempts a row from both. Implements
//! [`CaptureSink`] (write) and [`ContentEraser`] (erase) over the crate's
//! `data_subject` migration scope — the same portable bundle as sqlite.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_runtime_contract::{CaptureSink, ContentEraser, ContentKind, DataSubjectId, Purpose};
use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::postgres::{PgStoreError, connect_migrated, pool_migrated};

const NS: &str = "data_subject";

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A Postgres-backed captured-content store.
pub struct PgCapturedContentStore {
    pool: PgPool,
    seq: AtomicU64,
}

impl PgCapturedContentStore {
    /// Connect and apply the data-subject migrations under the `data_subject` namespace.
    pub async fn connect(url: &str) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: connect_migrated(url).await?,
            seq: AtomicU64::new(0),
        })
    }

    /// Build from an existing pool: apply the data-subject migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: pool_migrated(pool).await?,
            seq: AtomicU64::new(0),
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
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("cap_{now}_{n:016}");
        let purpose = serde_json::to_string(&purpose).unwrap_or_default();
        let _ = sqlx::query(&format!(
            "INSERT INTO {NS}_captured (id, subject, purpose, recorded_at, content, restricted) \
             VALUES ($1, $2, $3, $4, $5, 0)"
        ))
        .bind(&id)
        .bind(&subject.0)
        .bind(&purpose)
        .bind(now)
        .bind(content)
        .execute(&self.pool)
        .await;
        id
    }

    /// Restrict a subject's records (GDPR Art. 18); returns the number restricted.
    pub async fn restrict(&self, subject: &DataSubjectId) -> usize {
        sqlx::query(&format!(
            "UPDATE {NS}_captured SET restricted = 1 WHERE subject = $1 AND restricted = 0"
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
            "UPDATE {NS}_captured SET restricted = 0 WHERE subject = $1 AND restricted = 1"
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
            "DELETE FROM {NS}_captured WHERE restricted = 0 AND $1 - recorded_at >= $2"
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
        sqlx::query(&format!("SELECT COUNT(*) AS n FROM {NS}_captured"))
            .fetch_one(&self.pool)
            .await
            .and_then(|row| row.try_get::<i64, _>("n"))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Whether the capture repository currently contains no records.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[async_trait]
impl CaptureSink for PgCapturedContentStore {
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        _kind: ContentKind,
        content: &str,
    ) {
        self.insert(subject, purpose, content, now_millis()).await;
    }
}

#[async_trait]
impl ContentEraser for PgCapturedContentStore {
    async fn erase_subject(
        &self,
        subject: &DataSubjectId,
    ) -> Result<usize, awaken_runtime_contract::ErasureError> {
        // Restricted (Art. 18) rows survive erasure until released. A DELETE that
        // errors is surfaced (fail-closed) rather than swallowed to a `0` count.
        sqlx::query(&format!(
            "DELETE FROM {NS}_captured WHERE subject = $1 AND restricted = 0"
        ))
        .bind(&subject.0)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected() as usize)
        .map_err(|e| awaken_runtime_contract::ErasureError(e.to_string()))
    }
}
