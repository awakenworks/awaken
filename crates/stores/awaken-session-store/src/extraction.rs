//! Durable Memory extraction repository adapters.
//!
//! The table is an application-work outbox beside the Managed Session aggregate.
//! It persists the secret-free intent as JSON while indexing only scheduling
//! fields. Authorization remains outside this repository; `workspace_id` is an
//! intrinsic routing/ownership fact revalidated by the resource edge on use.

use async_trait::async_trait;
use awaken_session_contract::{
    MemoryExtractionError, MemoryExtractionIntent, MemoryExtractionRepository,
    MemoryExtractionStatus, PutMemoryExtractionOutcome,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

fn status_name(status: MemoryExtractionStatus) -> &'static str {
    match status {
        MemoryExtractionStatus::Pending => "pending",
        MemoryExtractionStatus::Claimed => "claimed",
        MemoryExtractionStatus::Extracted => "extracted",
        MemoryExtractionStatus::Stored => "stored",
        MemoryExtractionStatus::Completed => "completed",
        MemoryExtractionStatus::TerminalFailed => "terminal_failed",
    }
}

fn encode(intent: &MemoryExtractionIntent) -> Result<String, MemoryExtractionError> {
    intent.validate()?;
    serde_json::to_string(intent).map_err(|error| MemoryExtractionError::Storage(error.to_string()))
}

fn decode(data: &str) -> Result<MemoryExtractionIntent, MemoryExtractionError> {
    let intent: MemoryExtractionIntent = serde_json::from_str(data)
        .map_err(|error| MemoryExtractionError::Storage(format!("decode extraction: {error}")))?;
    intent.validate()?;
    Ok(intent)
}

#[async_trait]
impl MemoryExtractionRepository for SqliteManagedSessionRepository {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let data = encode(&intent)?;
        let mut conn = self.conn.lock().map_err(|error| {
            MemoryExtractionError::Storage(format!("session repository lock: {error}"))
        })?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        let existing = tx
            .query_row(
                "SELECT data FROM managed_memory_extraction
                 WHERE idempotency_key = ?1 OR intent_id = ?2
                 ORDER BY CASE WHEN idempotency_key = ?1 THEN 0 ELSE 1 END
                 LIMIT 1",
                params![intent.idempotency_key, intent.intent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        if let Some(existing) = existing {
            let existing = decode(&existing)?;
            tx.commit()
                .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
            return if existing == intent {
                Ok(PutMemoryExtractionOutcome::Existing)
            } else {
                Err(MemoryExtractionError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        tx.execute(
            "INSERT INTO managed_memory_extraction
                (intent_id, idempotency_key, status, revision, lease_expires_at_unix_ms, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                intent.intent_id,
                intent.idempotency_key,
                status_name(intent.status),
                intent.revision,
                intent.lease_expires_at_unix_ms,
                data,
            ],
        )
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        tx.commit()
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        Ok(PutMemoryExtractionOutcome::Inserted)
    }

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        let conn = self.conn.lock().map_err(|error| {
            MemoryExtractionError::Storage(format!("session repository lock: {error}"))
        })?;
        conn.query_row(
            "SELECT data FROM managed_memory_extraction WHERE intent_id = ?1",
            params![intent_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
        .map(|data| decode(&data))
        .transpose()
    }

    async fn recoverable_extractions(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn.lock().map_err(|error| {
            MemoryExtractionError::Storage(format!("session repository lock: {error}"))
        })?;
        let mut statement = conn
            .prepare(
                "SELECT data FROM managed_memory_extraction
                 WHERE status NOT IN ('completed', 'terminal_failed')
                 ORDER BY created_at, intent_id LIMIT ?1",
            )
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        statement
            .query_map(params![limit], |row| row.get::<_, String>(0))
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
            .map(|row| {
                row.map_err(|error| MemoryExtractionError::Storage(error.to_string()))
                    .and_then(|data| decode(&data))
            })
            .collect()
    }

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        if intent.revision != expected_revision.saturating_add(1) {
            return Err(MemoryExtractionError::RevisionConflict(intent.intent_id));
        }
        let data = encode(&intent)?;
        let changed = self
            .conn
            .lock()
            .map_err(|error| {
                MemoryExtractionError::Storage(format!("session repository lock: {error}"))
            })?
            .execute(
                "UPDATE managed_memory_extraction
                 SET status = ?3, revision = ?4, lease_expires_at_unix_ms = ?5, data = ?6
                 WHERE intent_id = ?1 AND idempotency_key = ?2 AND revision = ?7",
                params![
                    intent.intent_id,
                    intent.idempotency_key,
                    status_name(intent.status),
                    intent.revision,
                    intent.lease_expires_at_unix_ms,
                    data,
                    expected_revision,
                ],
            )
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        if changed == 1 {
            Ok(())
        } else {
            Err(MemoryExtractionError::RevisionConflict(intent.intent_id))
        }
    }
}

#[async_trait]
impl MemoryExtractionRepository for PostgresManagedSessionRepository {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let data = encode(&intent)?;
        let inserted = sqlx::query(
            "INSERT INTO managed_memory_extraction
                (intent_id, idempotency_key, status, revision, lease_expires_at_unix_ms, data)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT DO NOTHING",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(status_name(intent.status))
        .bind(i64::try_from(intent.revision).unwrap_or(i64::MAX))
        .bind(
            intent
                .lease_expires_at_unix_ms
                .and_then(|value| i64::try_from(value).ok()),
        )
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        if inserted.rows_affected() == 1 {
            return Ok(PutMemoryExtractionOutcome::Inserted);
        }
        let row = sqlx::query(
            "SELECT data FROM managed_memory_extraction
             WHERE idempotency_key = $1 OR intent_id = $2
             ORDER BY CASE WHEN idempotency_key = $1 THEN 0 ELSE 1 END
             LIMIT 1",
        )
        .bind(&intent.idempotency_key)
        .bind(&intent.intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
        .ok_or_else(|| {
            MemoryExtractionError::Storage("conflicting extraction disappeared".into())
        })?;
        let existing = decode(row.get("data"))?;
        if existing == intent {
            Ok(PutMemoryExtractionOutcome::Existing)
        } else {
            Err(MemoryExtractionError::IdempotencyConflict(
                intent.idempotency_key,
            ))
        }
    }

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        sqlx::query("SELECT data FROM managed_memory_extraction WHERE intent_id = $1")
            .bind(intent_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
            .map(|row| decode(row.get("data")))
            .transpose()
    }

    async fn recoverable_extractions(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        sqlx::query(
            "SELECT data FROM managed_memory_extraction
             WHERE status NOT IN ('completed', 'terminal_failed')
             ORDER BY created_at, intent_id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
        .into_iter()
        .map(|row| decode(row.get("data")))
        .collect()
    }

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        if intent.revision != expected_revision.saturating_add(1) {
            return Err(MemoryExtractionError::RevisionConflict(intent.intent_id));
        }
        let data = encode(&intent)?;
        let changed = sqlx::query(
            "UPDATE managed_memory_extraction
             SET status = $3, revision = $4, lease_expires_at_unix_ms = $5, data = $6
             WHERE intent_id = $1 AND idempotency_key = $2 AND revision = $7",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(status_name(intent.status))
        .bind(i64::try_from(intent.revision).unwrap_or(i64::MAX))
        .bind(
            intent
                .lease_expires_at_unix_ms
                .and_then(|value| i64::try_from(value).ok()),
        )
        .bind(data)
        .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        if changed.rows_affected() == 1 {
            Ok(())
        } else {
            Err(MemoryExtractionError::RevisionConflict(intent.intent_id))
        }
    }
}
