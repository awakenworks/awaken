//! Durable Memory extraction repository adapters.
//!
//! The table is an application-work outbox beside the Managed Session aggregate.
//! It persists the secret-free intent as JSON while indexing only scheduling
//! fields. Authorization remains outside this repository; `workspace_id` is an
//! intrinsic routing/ownership fact revalidated by the resource edge on use.

use async_trait::async_trait;
use awaken_ext_memory::{
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

fn postgres_integer(value: u64, field: &str) -> Result<i64, MemoryExtractionError> {
    i64::try_from(value).map_err(|_| {
        MemoryExtractionError::Storage(format!(
            "Memory extraction {field} exceeds Postgres BIGINT storage"
        ))
    })
}

fn postgres_optional_integer(
    value: Option<u64>,
    field: &str,
) -> Result<Option<i64>, MemoryExtractionError> {
    value
        .map(|value| postgres_integer(value, field))
        .transpose()
}

fn validate_next_revision(
    expected_revision: u64,
    intent: &MemoryExtractionIntent,
) -> Result<(), MemoryExtractionError> {
    let next_revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| MemoryExtractionError::RevisionConflict(intent.intent_id.clone()))?;
    if intent.revision != next_revision {
        return Err(MemoryExtractionError::RevisionConflict(
            intent.intent_id.clone(),
        ));
    }
    Ok(())
}

#[async_trait]
impl MemoryExtractionRepository for SqliteManagedSessionRepository {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let data = encode(&intent)?;
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
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
                return if existing.same_request(&intent) {
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
        })
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
    }

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        let intent_id = intent_id.to_string();
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
            conn.query_row(
                "SELECT data FROM managed_memory_extraction WHERE intent_id = ?1",
                params![intent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
            .map(|data| decode(&data))
            .transpose()
        })
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
    }

    async fn extraction_cursor(&self, thread_id: &str) -> Result<usize, MemoryExtractionError> {
        let thread_id = thread_id.to_string();
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
            let mut statement = conn
                .prepare("SELECT data FROM managed_memory_extraction")
                .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
            let mut cursor = 0;
            for row in rows {
                let data =
                    row.map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
                let intent = decode(&data)?;
                if intent.logical_thread_id() == thread_id {
                    cursor = cursor.max(intent.transcript_cursor());
                }
            }
            Ok(cursor)
        })
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
    }

    async fn recoverable_extractions(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
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
        })
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
    }

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        validate_next_revision(expected_revision, &intent)?;
        let data = encode(&intent)?;
        awaken_sqlite_runtime::with_connection(self.conn.clone(), move |conn| {
            let changed = conn
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
        })
        .await
        .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
    }
}

#[async_trait]
impl MemoryExtractionRepository for PostgresManagedSessionRepository {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let data = encode(&intent)?;
        let revision = postgres_integer(intent.revision, "revision")?;
        let lease_expires_at_unix_ms =
            postgres_optional_integer(intent.lease_expires_at_unix_ms, "lease_expires_at_unix_ms")?;
        let inserted = sqlx::query(
            "INSERT INTO managed_memory_extraction
                (intent_id, idempotency_key, status, revision, lease_expires_at_unix_ms, data)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT DO NOTHING",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(status_name(intent.status))
        .bind(revision)
        .bind(lease_expires_at_unix_ms)
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
        if existing.same_request(&intent) {
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

    async fn extraction_cursor(&self, thread_id: &str) -> Result<usize, MemoryExtractionError> {
        let rows = sqlx::query("SELECT data FROM managed_memory_extraction")
            .fetch_all(&self.pool)
            .await
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        let mut cursor = 0;
        for row in rows {
            let intent = decode(row.get("data"))?;
            if intent.logical_thread_id() == thread_id {
                cursor = cursor.max(intent.transcript_cursor());
            }
        }
        Ok(cursor)
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
        validate_next_revision(expected_revision, &intent)?;
        let data = encode(&intent)?;
        let revision = postgres_integer(intent.revision, "revision")?;
        let expected_revision = postgres_integer(expected_revision, "expected_revision")?;
        let lease_expires_at_unix_ms =
            postgres_optional_integer(intent.lease_expires_at_unix_ms, "lease_expires_at_unix_ms")?;
        let changed = sqlx::query(
            "UPDATE managed_memory_extraction
             SET status = $3, revision = $4, lease_expires_at_unix_ms = $5, data = $6
             WHERE intent_id = $1 AND idempotency_key = $2 AND revision = $7",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(status_name(intent.status))
        .bind(revision)
        .bind(lease_expires_at_unix_ms)
        .bind(data)
        .bind(expected_revision)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_integer_conversion_rejects_clamping_and_lease_erasure() {
        assert!(matches!(
            postgres_integer(u64::MAX, "revision"),
            Err(MemoryExtractionError::Storage(message))
                if message.contains("revision") && message.contains("BIGINT")
        ));
        assert!(matches!(
            postgres_optional_integer(Some(u64::MAX), "lease"),
            Err(MemoryExtractionError::Storage(message))
                if message.contains("lease") && message.contains("BIGINT")
        ));
    }
}
