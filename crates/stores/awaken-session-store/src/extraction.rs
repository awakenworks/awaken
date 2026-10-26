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
mod tests {
    use super::*;

    #[tokio::test]
    async fn extraction_execution_selection_and_first_claim_share_one_revision_fence() {
        // Cause/effect table: C1 both independent handles read a virgin legacy
        // Pending revision 0; C2 selection wins / claim wins / writes race.
        // S1 selection first => exactly the selected context persists, old claim
        // rejects; S2 claim first => original context persists, relocation rejects;
        // S3 concurrent => exactly one next revision and the full winner, never a
        // mixed scope/claim. The test uses the existing JSON codec and SQL CAS;
        // neither operation runs a model or creates another job identity.
        use awaken_ext_memory::{MemoryExtractionError, MemoryExtractionRepository};
        for order in ["selection-first", "claim-first", "concurrent"] {
            let (_dir, path) = crate::tests::temp_database("extraction-selection.db");
            let first = SqliteManagedSessionRepository::open(&path).await.unwrap();
            let second = SqliteManagedSessionRepository::open(&path).await.unwrap();
            let initial = crate::tests::extraction("selection-race", order);
            first
                .put_extraction_if_absent(initial.clone())
                .await
                .unwrap();
            let selected = first
                .get_extraction(&initial.intent_id)
                .await
                .unwrap()
                .unwrap()
                .with_execution(awaken_ext_memory::MemoryExtractionExecution {
                    commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::AuxiliaryThread,
                    data_subject_id: None,
                })
                .unwrap();
            let mut claimed = second
                .get_extraction(&initial.intent_id)
                .await
                .unwrap()
                .unwrap();
            claimed.claim("old-extractor", 100, 50).unwrap();
            assert_eq!((selected.revision, claimed.revision), (1, 1));
            let (selection, claim) = match order {
                "selection-first" => {
                    let selection = first.compare_and_swap_extraction(0, selected.clone()).await;
                    (
                        selection,
                        second.compare_and_swap_extraction(0, claimed.clone()).await,
                    )
                }
                "claim-first" => {
                    let claim = second.compare_and_swap_extraction(0, claimed.clone()).await;
                    (
                        first.compare_and_swap_extraction(0, selected.clone()).await,
                        claim,
                    )
                }
                "concurrent" => tokio::join!(
                    first.compare_and_swap_extraction(0, selected.clone()),
                    second.compare_and_swap_extraction(0, claimed.clone()),
                ),
                _ => unreachable!(),
            };
            assert_ne!(selection.is_ok(), claim.is_ok(), "{order}");
            let (winner, loser) = if selection.is_ok() {
                (selected, claim)
            } else {
                (claimed, selection)
            };
            assert!(
                matches!(loser, Err(MemoryExtractionError::RevisionConflict(_))),
                "{order}"
            );
            let stored = first
                .get_extraction(&initial.intent_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored, winner, "{order}: one complete winner");
            assert!(
                stored.with_execution(Default::default()).is_err(),
                "{order}: first admission stays frozen"
            );
        }
    }

    #[test]
    fn extraction_codec_accepts_legacy_and_one_versioned_authority() {
        // R1 the published bare intent => exact legacy read, no guessed facts;
        // R2 canonical v1, with Session/Auxiliary output and attribution => exact roundtrip through the same
        // codec used by SQLite and PostgreSQL. R3 the old bare-intent serde
        // reader cannot interpret the new envelope as a legacy job. Serialization
        // changes only representation, preserving identities, state and lease.
        let intent = crate::tests::extraction("codec-intent", "codec-key");
        let legacy = serde_json::to_string(&intent).unwrap();
        assert_eq!(decode(&legacy).unwrap(), intent, "R1");
        for execution in [
            awaken_ext_memory::MemoryExtractionExecution::default(),
            awaken_ext_memory::MemoryExtractionExecution {
                commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::Session,
                data_subject_id: Some(awaken_runtime_contract::DataSubjectId("subject-a".into())),
            },
            awaken_ext_memory::MemoryExtractionExecution {
                commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::AuxiliaryThread,
                data_subject_id: Some(awaken_runtime_contract::DataSubjectId("subject-b".into())),
            },
        ] {
            let selected = intent.clone().with_execution(execution).unwrap();
            let canonical = encode(&selected).unwrap();
            assert_eq!(decode(&canonical).unwrap(), selected, "R2");
            let envelope: serde_json::Value = serde_json::from_str(&canonical).unwrap();
            assert_eq!(envelope["format"], EXTRACTION_FORMAT, "R2");
            assert_eq!(
                envelope["extraction"],
                serde_json::to_value(&selected).unwrap(),
                "R2"
            );
            assert!(
                serde_json::from_str::<MemoryExtractionIntent>(&canonical).is_err(),
                "R3"
            );
        }
    }

    #[test]
    fn extraction_codec_refuses_unknown_partial_and_downgraded_formats() {
        // F1 unsupported version; F2 missing/non-object body; F3 envelope plus
        // competing legacy top-level identity; F4 unversioned or unknown
        // execution context; F5 unknown bare intent field. Every row must refuse
        // before a claim; no fallback to legacy or dropped routing metadata.
        let intent = crate::tests::extraction("codec-invalid", "codec-key");
        let bare = serde_json::to_value(&intent).unwrap();
        let canonical: serde_json::Value = serde_json::from_str(&encode(&intent).unwrap()).unwrap();
        let mut unknown = canonical.clone();
        unknown["format"] = serde_json::json!("awaken.memory-extraction.v999");
        let mut competing = canonical.clone();
        competing["intent_id"] = serde_json::json!("another-intent");
        let mut future_bare = bare.clone();
        future_bare["execution"] = serde_json::json!({"commit_scope":"auxiliary_thread"});
        let mut future_envelope = canonical.clone();
        future_envelope["extraction"]["execution"] = serde_json::json!({"commit_scope":"unknown"});
        let mut unknown_field = bare;
        unknown_field["future_authority"] = serde_json::json!(true);
        for encoded in [
            unknown,
            serde_json::json!({"format":EXTRACTION_FORMAT}),
            serde_json::json!({"format":EXTRACTION_FORMAT,"extraction":42}),
            competing,
            future_bare,
            future_envelope,
            unknown_field,
        ] {
            assert!(decode(&encoded.to_string()).is_err(), "{encoded}");
        }
        assert!(decode("not json").is_err());
    }

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
