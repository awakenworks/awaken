// @formal entrypoint memory_extraction.unknown_execution_never_reenters_retry
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
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

/// The persisted intent and its frozen execution context. Older bare-intent
/// readers refuse this envelope rather than silently dropping routing facts.
const EXTRACTION_FORMAT: &str = "awaken.memory-extraction.v1";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionEnvelope<T> {
    format: String,
    extraction: T,
}

fn encode(intent: &MemoryExtractionIntent) -> Result<String, MemoryExtractionError> {
    intent.validate()?;
    serde_json::to_string(&ExtractionEnvelope {
        format: EXTRACTION_FORMAT.to_string(),
        extraction: intent,
    })
    .map_err(|error| MemoryExtractionError::Storage(error.to_string()))
}

fn decode(data: &str) -> Result<MemoryExtractionIntent, MemoryExtractionError> {
    let decode_error = |error: serde_json::Error| {
        MemoryExtractionError::Storage(format!("decode extraction: {error}"))
    };
    let value: serde_json::Value = serde_json::from_str(data).map_err(decode_error)?;
    let (body, versioned) = if value.get("format").is_some() || value.get("extraction").is_some() {
        let envelope: ExtractionEnvelope<serde_json::Value> =
            serde_json::from_value(value).map_err(decode_error)?;
        if envelope.format != EXTRACTION_FORMAT {
            return Err(MemoryExtractionError::Storage(format!(
                "unsupported Memory extraction format `{}`",
                envelope.format,
            )));
        }
        (envelope.extraction, true)
    } else {
        (value, false)
    };
    // Published bare rows predate execution-context selection. Such facts must
    // carry the envelope that an older reader cannot silently downgrade.
    if !versioned && body.get("execution").is_some() {
        return Err(MemoryExtractionError::Storage(
            "Memory extraction execution context requires an upgraded storage format".into(),
        ));
    }
    let intent: MemoryExtractionIntent = serde_json::from_value(body).map_err(decode_error)?;
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

fn sqlite_storage(error: impl std::fmt::Display) -> MemoryExtractionError {
    MemoryExtractionError::Storage(error.to_string())
}

/// SQLite INTEGER is signed 64-bit; a larger domain value is refused before it
/// reaches storage instead of wrapping.
fn sqlite_integer(value: u64) -> Result<i64, MemoryExtractionError> {
    i64::try_from(value).map_err(sqlite_storage)
}

fn sqlite_optional_integer(value: Option<u64>) -> Result<Option<i64>, MemoryExtractionError> {
    value.map(sqlite_integer).transpose()
}

#[async_trait]
impl MemoryExtractionRepository for SqliteManagedSessionRepository {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let data = encode(&intent)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sqlite_storage)?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT data FROM managed_memory_extraction
                 WHERE idempotency_key = ?1 OR intent_id = ?2
                 ORDER BY CASE WHEN idempotency_key = ?1 THEN 0 ELSE 1 END
                 LIMIT 1",
        )
        .bind(&intent.idempotency_key)
        .bind(&intent.intent_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlite_storage)?;
        if let Some(existing) = existing {
            let existing = decode(&existing)?;
            tx.commit().await.map_err(sqlite_storage)?;
            return if existing.same_request(&intent) {
                Ok(PutMemoryExtractionOutcome::Existing)
            } else {
                Err(MemoryExtractionError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        sqlx::query(
            "INSERT INTO managed_memory_extraction
                (intent_id, idempotency_key, status, revision, lease_expires_at_unix_ms, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(intent.status.storage_name())
        .bind(sqlite_integer(intent.revision)?)
        .bind(sqlite_optional_integer(intent.lease_expires_at_unix_ms)?)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(sqlite_storage)?;
        tx.commit().await.map_err(sqlite_storage)?;
        Ok(PutMemoryExtractionOutcome::Inserted)
    }

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        sqlx::query_scalar::<_, String>(
            "SELECT data FROM managed_memory_extraction WHERE intent_id = ?1",
        )
        .bind(intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlite_storage)?
        .map(|data| decode(&data))
        .transpose()
    }

    async fn extraction_cursor(&self, thread_id: &str) -> Result<usize, MemoryExtractionError> {
        let rows = sqlx::query_scalar::<_, String>("SELECT data FROM managed_memory_extraction")
            .fetch_all(&self.pool)
            .await
            .map_err(sqlite_storage)?;
        let mut cursor = 0;
        for data in rows {
            let intent = decode(&data)?;
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
        let recoverable = MemoryExtractionStatus::recoverable_storage_names();
        sqlx::query_scalar::<_, String>(
            "SELECT data FROM managed_memory_extraction
                 WHERE status IN (?1, ?2, ?3, ?4)
                 ORDER BY created_at, intent_id LIMIT ?5",
        )
        .bind(recoverable[0])
        .bind(recoverable[1])
        .bind(recoverable[2])
        .bind(recoverable[3])
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlite_storage)?
        .iter()
        .map(|data| decode(data))
        .collect()
    }

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        validate_next_revision(expected_revision, &intent)?;
        let data = encode(&intent)?;
        let changed = sqlx::query(
            "UPDATE managed_memory_extraction
                 SET status = ?3, revision = ?4, lease_expires_at_unix_ms = ?5, data = ?6
                 WHERE intent_id = ?1 AND idempotency_key = ?2 AND revision = ?7",
        )
        .bind(&intent.intent_id)
        .bind(&intent.idempotency_key)
        .bind(intent.status.storage_name())
        .bind(sqlite_integer(intent.revision)?)
        .bind(sqlite_optional_integer(intent.lease_expires_at_unix_ms)?)
        .bind(data)
        .bind(sqlite_integer(expected_revision)?)
        .execute(&self.pool)
        .await
        .map_err(sqlite_storage)?
        .rows_affected();
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
        .bind(intent.status.storage_name())
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
        let recoverable = MemoryExtractionStatus::recoverable_storage_names();
        sqlx::query(
            "SELECT data FROM managed_memory_extraction
             WHERE status IN ($1, $2, $3, $4)
             ORDER BY created_at, intent_id LIMIT $5",
        )
        .bind(recoverable[0])
        .bind(recoverable[1])
        .bind(recoverable[2])
        .bind(recoverable[3])
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
        .bind(intent.status.storage_name())
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
mod tests;
