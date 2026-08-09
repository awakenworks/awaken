//! Durable Dream job and automatic-policy adapters.

use awaken_session_contract::{
    DreamPolicyRecord, DreamProcessRecord, DreamProcessStore, DreamProcessStoreError,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

fn storage(error: impl std::fmt::Display) -> DreamProcessStoreError {
    DreamProcessStoreError::Storage(error.to_string())
}

fn encode(record: &impl Serialize) -> Result<String, DreamProcessStoreError> {
    serde_json::to_string(record).map_err(storage)
}

fn decode<T: DeserializeOwned>(data: &str) -> Result<T, DreamProcessStoreError> {
    serde_json::from_str(data).map_err(storage)
}

fn decode_process(key: String, data: String) -> Result<DreamProcessRecord, DreamProcessStoreError> {
    let record: DreamProcessRecord = decode(&data)?;
    if record.process_id != key {
        return Err(storage("Dream process record identity mismatch"));
    }
    Ok(record)
}

fn decode_policy(
    workspace_id: String,
    memory_store_id: String,
    data: String,
) -> Result<DreamPolicyRecord, DreamProcessStoreError> {
    let record: DreamPolicyRecord = decode(&data)?;
    if record.workspace_id != workspace_id || record.memory_store_id != memory_store_id {
        return Err(storage("Dream policy record identity mismatch"));
    }
    Ok(record)
}

impl DreamProcessStore for SqliteManagedSessionRepository {
    fn dream_processes(&self) -> Result<Vec<DreamProcessRecord>, DreamProcessStoreError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare("SELECT job_id, data FROM managed_dream ORDER BY job_id")
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage)?;
        rows.map(|row| {
            let (key, data) = row.map_err(storage)?;
            decode_process(key, data)
        })
        .collect()
    }

    fn compare_and_swap_dream_process(
        &self,
        expected: Option<&DreamProcessRecord>,
        record: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let data = encode(&record)?;
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let current = tx
            .query_row(
                "SELECT data FROM managed_dream WHERE job_id=?1",
                params![record.process_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|data| decode_process(record.process_id.clone(), data))
            .transpose()?;
        if current.as_ref() != expected {
            return Ok(false);
        }
        let changed = match current {
            None => tx.execute(
                "INSERT OR IGNORE INTO managed_dream (job_id, data) VALUES (?1, ?2)",
                params![record.process_id, data],
            ),
            Some(_) => tx.execute(
                "UPDATE managed_dream SET data=?2 WHERE job_id=?1",
                params![record.process_id, data],
            ),
        }
        .map_err(storage)?;
        if changed != 1 {
            return Ok(false);
        }
        tx.commit().map_err(storage)?;
        Ok(true)
    }

    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamProcessStoreError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT workspace_id, memory_store_id, data FROM managed_dream_policy \
                 ORDER BY workspace_id, memory_store_id",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(storage)?;
        rows.map(|row| {
            let (workspace_id, memory_store_id, data) = row.map_err(storage)?;
            decode_policy(workspace_id, memory_store_id, data)
        })
        .collect()
    }

    fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&DreamPolicyRecord>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let data = encode(&record)?;
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let current = tx
            .query_row(
                "SELECT data FROM managed_dream_policy WHERE workspace_id=?1 AND memory_store_id=?2",
                params![record.workspace_id, record.memory_store_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|data| {
                decode_policy(
                    record.workspace_id.clone(),
                    record.memory_store_id.clone(),
                    data,
                )
            })
            .transpose()?;
        if current.as_ref() != expected {
            return Ok(false);
        }
        let changed = match current {
            None => tx.execute(
                "INSERT OR IGNORE INTO managed_dream_policy \
                 (workspace_id, memory_store_id, data) VALUES (?1, ?2, ?3)",
                params![record.workspace_id, record.memory_store_id, data],
            ),
            Some(_) => tx.execute(
                "UPDATE managed_dream_policy SET data=?3 \
                 WHERE workspace_id=?1 AND memory_store_id=?2",
                params![record.workspace_id, record.memory_store_id, data],
            ),
        }
        .map_err(storage)?;
        if changed != 1 {
            return Ok(false);
        }
        tx.commit().map_err(storage)?;
        Ok(true)
    }

    fn claim_dream_policy(
        &self,
        expected_policy: &DreamPolicyRecord,
        policy: DreamPolicyRecord,
        process: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let policy_data = encode(&policy)?;
        let process_data = encode(&process)?;
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let current = tx
            .query_row(
                "SELECT data FROM managed_dream_policy WHERE workspace_id=?1 AND memory_store_id=?2",
                params![policy.workspace_id, policy.memory_store_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?;
        let current = current
            .map(|data| {
                decode_policy(
                    policy.workspace_id.clone(),
                    policy.memory_store_id.clone(),
                    data,
                )
            })
            .transpose()?;
        if current.as_ref() != Some(expected_policy) {
            return Ok(false);
        }
        tx.execute(
            "UPDATE managed_dream_policy SET data=?3 \
             WHERE workspace_id=?1 AND memory_store_id=?2",
            params![policy.workspace_id, policy.memory_store_id, policy_data],
        )
        .map_err(storage)?;
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO managed_dream (job_id, data) VALUES (?1, ?2)",
                params![process.process_id, process_data],
            )
            .map_err(storage)?;
        if inserted != 1 {
            return Ok(false);
        }
        tx.commit().map_err(storage)?;
        Ok(true)
    }
}

fn postgres_block<T: Send + 'static>(
    make: impl FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>
    + Send
    + 'static,
) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => std::thread::spawn(move || handle.block_on(make()))
            .join()
            .expect("Dream Postgres bridge thread panicked"),
        Err(_) => tokio::runtime::Runtime::new()
            .expect("create Dream Postgres bridge runtime")
            .block_on(make()),
    }
}

impl DreamProcessStore for PostgresManagedSessionRepository {
    fn dream_processes(&self) -> Result<Vec<DreamProcessRecord>, DreamProcessStoreError> {
        let pool = self.pool.clone();
        postgres_block(move || {
            Box::pin(async move {
                sqlx::query("SELECT job_id, data FROM managed_dream ORDER BY job_id")
                    .fetch_all(&pool)
                    .await
                    .map_err(storage)?
                    .into_iter()
                    .map(|row| {
                        decode_process(
                            row.try_get(0).map_err(storage)?,
                            row.try_get(1).map_err(storage)?,
                        )
                    })
                    .collect()
            })
        })
    }

    fn compare_and_swap_dream_process(
        &self,
        expected: Option<&DreamProcessRecord>,
        record: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let pool = self.pool.clone();
        let expected = expected.cloned();
        let data = encode(&record)?;
        postgres_block(move || {
            Box::pin(async move {
                let mut tx = pool.begin().await.map_err(storage)?;
                let current =
                    sqlx::query("SELECT data FROM managed_dream WHERE job_id=$1 FOR UPDATE")
                        .bind(&record.process_id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(storage)?
                        .map(|row| {
                            decode_process(
                                record.process_id.clone(),
                                row.try_get(0).map_err(storage)?,
                            )
                        })
                        .transpose()?;
                if current.as_ref() != expected.as_ref() {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                let result = match current {
                    None => sqlx::query("INSERT INTO managed_dream (job_id, data) VALUES ($1, $2) ON CONFLICT(job_id) DO NOTHING")
                        .bind(record.process_id).bind(data).execute(&mut *tx).await,
                    Some(_) => sqlx::query("UPDATE managed_dream SET data=$2 WHERE job_id=$1")
                        .bind(record.process_id).bind(data).execute(&mut *tx).await,
                }.map_err(storage)?;
                if result.rows_affected() != 1 {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                tx.commit().await.map_err(storage)?;
                Ok(true)
            })
        })
    }

    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamProcessStoreError> {
        let pool = self.pool.clone();
        postgres_block(move || {
            Box::pin(async move {
                sqlx::query("SELECT workspace_id, memory_store_id, data FROM managed_dream_policy ORDER BY workspace_id, memory_store_id")
                .fetch_all(&pool).await.map_err(storage)?.into_iter()
                .map(|row| decode_policy(
                    row.try_get(0).map_err(storage)?,
                    row.try_get(1).map_err(storage)?,
                    row.try_get(2).map_err(storage)?,
                ))
                .collect()
            })
        })
    }

    fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&DreamPolicyRecord>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let pool = self.pool.clone();
        let expected = expected.cloned();
        let data = encode(&record)?;
        postgres_block(move || {
            Box::pin(async move {
                let mut tx = pool.begin().await.map_err(storage)?;
                let current = sqlx::query("SELECT data FROM managed_dream_policy WHERE workspace_id=$1 AND memory_store_id=$2 FOR UPDATE")
                    .bind(&record.workspace_id).bind(&record.memory_store_id)
                    .fetch_optional(&mut *tx).await.map_err(storage)?
                    .map(|row| decode_policy(
                        record.workspace_id.clone(),
                        record.memory_store_id.clone(),
                        row.try_get(0).map_err(storage)?,
                    ))
                    .transpose()?;
                if current.as_ref() != expected.as_ref() {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                let result = match current {
                    None => sqlx::query("INSERT INTO managed_dream_policy (workspace_id, memory_store_id, data) VALUES ($1, $2, $3) ON CONFLICT(workspace_id, memory_store_id) DO NOTHING")
                        .bind(record.workspace_id).bind(record.memory_store_id).bind(data).execute(&mut *tx).await,
                    Some(_) => sqlx::query("UPDATE managed_dream_policy SET data=$3 WHERE workspace_id=$1 AND memory_store_id=$2")
                        .bind(record.workspace_id).bind(record.memory_store_id).bind(data).execute(&mut *tx).await,
                }.map_err(storage)?;
                if result.rows_affected() != 1 {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                tx.commit().await.map_err(storage)?;
                Ok(true)
            })
        })
    }

    fn claim_dream_policy(
        &self,
        expected_policy: &DreamPolicyRecord,
        policy: DreamPolicyRecord,
        process: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let pool = self.pool.clone();
        let expected_policy = expected_policy.clone();
        let policy_data = encode(&policy)?;
        let process_data = encode(&process)?;
        postgres_block(move || {
            Box::pin(async move {
                let mut tx = pool.begin().await.map_err(storage)?;
                let current = sqlx::query("SELECT data FROM managed_dream_policy WHERE workspace_id=$1 AND memory_store_id=$2 FOR UPDATE")
                    .bind(&policy.workspace_id).bind(&policy.memory_store_id)
                    .fetch_optional(&mut *tx).await.map_err(storage)?
                    .map(|row| decode_policy(
                        policy.workspace_id.clone(),
                        policy.memory_store_id.clone(),
                        row.try_get(0).map_err(storage)?,
                    ))
                    .transpose()?;
                if current.as_ref() != Some(&expected_policy) {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                sqlx::query("UPDATE managed_dream_policy SET data=$3 WHERE workspace_id=$1 AND memory_store_id=$2")
                    .bind(&policy.workspace_id).bind(&policy.memory_store_id).bind(policy_data)
                    .execute(&mut *tx).await.map_err(storage)?;
                let inserted = sqlx::query("INSERT INTO managed_dream (job_id, data) VALUES ($1, $2) ON CONFLICT(job_id) DO NOTHING")
                .bind(process.process_id).bind(process_data).execute(&mut *tx).await.map_err(storage)?.rows_affected();
                if inserted != 1 {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                tx.commit().await.map_err(storage)?;
                Ok(true)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dream_policy(next_due_ms: u64) -> DreamPolicyRecord {
        DreamPolicyRecord {
            workspace_id: "workspace".into(),
            memory_store_id: "store".into(),
            config: awaken_session_contract::DreamPolicyConfig::default(),
            next_due_ms,
            last_completed_cutoff_ms: 0,
        }
    }

    fn process(process_id: &str) -> DreamProcessRecord {
        DreamProcessRecord {
            process_id: process_id.into(),
            workspace_id: "workspace".into(),
            status: awaken_session_contract::DreamStatus::Pending,
            source_memory_store_id: "store".into(),
            session_ids: vec!["session".into()],
            model: awaken_session_contract::DreamModelConfig {
                id: "claude-sonnet-5".into(),
                speed: None,
            },
            request_guidance: None,
            agent_id: "agent".into(),
            result_memory_store_id: None,
            session_id: None,
            transcript_file_ids: Vec::new(),
            cleanup_pending: false,
            created_at: 1,
            ended_at: None,
            archived_at: None,
            error: None,
            policy_key: None,
        }
    }

    #[test]
    fn sqlite_compare_and_swap_and_policy_claim_are_atomic() {
        // Persistence decision table: R1 absent job/policy + insert -> accepted;
        // R2 stale expected typed value -> rejected without mutation; R3 exact policy
        // version + fresh job -> both commit; R4 stale policy + fresh job ->
        // neither commits. These rules are the multi-replica Dream claim fence.
        let repository = SqliteManagedSessionRepository::open_in_memory().unwrap();
        let policy = dream_policy(1);
        assert!(
            repository
                .compare_and_swap_dream_policy(None, policy.clone())
                .unwrap()
        );
        assert!(
            !repository
                .compare_and_swap_dream_policy(
                    Some(&dream_policy(0)),
                    DreamPolicyRecord {
                        next_due_ms: 999,
                        ..policy.clone()
                    },
                )
                .unwrap()
        );
        let policy_v2 = DreamPolicyRecord {
            next_due_ms: 2,
            ..policy.clone()
        };
        assert!(
            repository
                .claim_dream_policy(&policy, policy_v2.clone(), process("dream-1"),)
                .unwrap()
        );
        assert!(
            !repository
                .claim_dream_policy(
                    &policy,
                    DreamPolicyRecord {
                        next_due_ms: 3,
                        ..policy.clone()
                    },
                    process("dream-2"),
                )
                .unwrap()
        );
        assert_eq!(repository.dream_processes().unwrap().len(), 1);
        assert_eq!(repository.dream_policies().unwrap()[0], policy_v2);
    }

    #[test]
    fn sqlite_cas_normalizes_a_legacy_dream_record_through_the_typed_path() {
        // Compatibility cause/effect rules: C1 a retained row uses legacy `id`
        // and duplicated `usage` fields -> E1 it decodes to the canonical typed
        // process; C2 that exact typed value is the CAS expectation -> E2 the
        // transition commits and rewrites one canonical payload without `usage`.
        // R1=C1, R2=C1+C2 prove compatibility is an adapter concern, not a
        // parallel application persistence path.
        let repository = SqliteManagedSessionRepository::open_in_memory().unwrap();
        let original = process("dream-legacy");
        let mut legacy = serde_json::to_value(&original).unwrap();
        let fields = legacy.as_object_mut().unwrap();
        let id = fields.remove("process_id").unwrap();
        fields.insert("id".into(), id);
        fields.insert(
            "usage".into(),
            serde_json::json!({"input_tokens": 42, "output_tokens": 7}),
        );
        repository
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO managed_dream (job_id, data) VALUES (?1, ?2)",
                params!["dream-legacy", legacy.to_string()],
            )
            .unwrap();

        let loaded = repository.dream_processes().unwrap().remove(0);
        assert_eq!(loaded, original, "R1");
        let candidate = DreamProcessRecord {
            status: awaken_session_contract::DreamStatus::Running,
            ..loaded.clone()
        };
        assert!(
            repository
                .compare_and_swap_dream_process(Some(&loaded), candidate.clone())
                .unwrap(),
            "R2"
        );
        assert_eq!(repository.dream_processes().unwrap(), vec![candidate]);
        let raw: String = repository
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT data FROM managed_dream WHERE job_id=?1",
                params!["dream-legacy"],
                |row| row.get(0),
            )
            .unwrap();
        let normalized: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(normalized.get("process_id").is_some(), "R2");
        assert!(normalized.get("id").is_none(), "R2");
        assert!(normalized.get("usage").is_none(), "R2");
    }
}
