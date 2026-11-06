// @formal surface requirement=product.dream.dream_job
// @formal reason: This source implements the repository-controlled dream product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Durable Dream job and automatic-policy adapters.

use async_trait::async_trait;
use awaken_session_dream_contract::{
    DreamPolicyRecord, DreamProcessRecord, DreamProcessStore, DreamProcessStoreError,
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

fn storage(error: impl std::fmt::Display) -> DreamProcessStoreError {
    DreamProcessStoreError::Storage(error.to_string().into())
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

#[async_trait]
impl DreamProcessStore for SqliteManagedSessionRepository {
    async fn dream_processes(&self) -> Result<Vec<DreamProcessRecord>, DreamProcessStoreError> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT job_id, data FROM managed_dream ORDER BY job_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|(key, data)| decode_process(key, data))
        .collect()
    }

    async fn compare_and_swap_dream_process(
        &self,
        expected: Option<&DreamProcessRecord>,
        record: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let data = encode(&record)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let current =
            sqlx::query_scalar::<_, String>("SELECT data FROM managed_dream WHERE job_id=?1")
                .bind(&record.process_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?
                .map(|data| decode_process(record.process_id.clone(), data))
                .transpose()?;
        if current.as_ref() != expected {
            return Ok(false);
        }
        let changed = match current {
            None => {
                sqlx::query("INSERT OR IGNORE INTO managed_dream (job_id, data) VALUES (?1, ?2)")
                    .bind(&record.process_id)
                    .bind(data)
                    .execute(&mut *tx)
                    .await
            }
            Some(_) => {
                sqlx::query("UPDATE managed_dream SET data=?2 WHERE job_id=?1")
                    .bind(&record.process_id)
                    .bind(data)
                    .execute(&mut *tx)
                    .await
            }
        }
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            return Ok(false);
        }
        tx.commit().await.map_err(storage)?;
        Ok(true)
    }

    async fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamProcessStoreError> {
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT workspace_id, memory_store_id, data FROM managed_dream_policy \
             ORDER BY workspace_id, memory_store_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|(workspace_id, memory_store_id, data)| {
            decode_policy(workspace_id, memory_store_id, data)
        })
        .collect()
    }

    async fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&DreamPolicyRecord>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let data = encode(&record)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let current = sqlx::query_scalar::<_, String>(
            "SELECT data FROM managed_dream_policy WHERE workspace_id=?1 AND memory_store_id=?2",
        )
        .bind(&record.workspace_id)
        .bind(&record.memory_store_id)
        .fetch_optional(&mut *tx)
        .await
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
            None => {
                sqlx::query(
                    "INSERT OR IGNORE INTO managed_dream_policy \
                     (workspace_id, memory_store_id, data) VALUES (?1, ?2, ?3)",
                )
                .bind(&record.workspace_id)
                .bind(&record.memory_store_id)
                .bind(data)
                .execute(&mut *tx)
                .await
            }
            Some(_) => {
                sqlx::query(
                    "UPDATE managed_dream_policy SET data=?3 \
                     WHERE workspace_id=?1 AND memory_store_id=?2",
                )
                .bind(&record.workspace_id)
                .bind(&record.memory_store_id)
                .bind(data)
                .execute(&mut *tx)
                .await
            }
        }
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            return Ok(false);
        }
        tx.commit().await.map_err(storage)?;
        Ok(true)
    }

    async fn claim_dream_policy(
        &self,
        expected_policy: &DreamPolicyRecord,
        policy: DreamPolicyRecord,
        process: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let policy_data = encode(&policy)?;
        let process_data = encode(&process)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let current = sqlx::query_scalar::<_, String>(
            "SELECT data FROM managed_dream_policy WHERE workspace_id=?1 AND memory_store_id=?2",
        )
        .bind(&policy.workspace_id)
        .bind(&policy.memory_store_id)
        .fetch_optional(&mut *tx)
        .await
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
        sqlx::query(
            "UPDATE managed_dream_policy SET data=?3 \
             WHERE workspace_id=?1 AND memory_store_id=?2",
        )
        .bind(&policy.workspace_id)
        .bind(&policy.memory_store_id)
        .bind(policy_data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let inserted =
            sqlx::query("INSERT OR IGNORE INTO managed_dream (job_id, data) VALUES (?1, ?2)")
                .bind(&process.process_id)
                .bind(process_data)
                .execute(&mut *tx)
                .await
                .map_err(storage)?
                .rows_affected();
        if inserted != 1 {
            return Ok(false);
        }
        tx.commit().await.map_err(storage)?;
        Ok(true)
    }
}

#[async_trait]
impl DreamProcessStore for PostgresManagedSessionRepository {
    async fn dream_processes(&self) -> Result<Vec<DreamProcessRecord>, DreamProcessStoreError> {
        sqlx::query("SELECT job_id, data FROM managed_dream ORDER BY job_id")
            .fetch_all(&self.pool)
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
    }

    async fn compare_and_swap_dream_process(
        &self,
        expected: Option<&DreamProcessRecord>,
        record: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let data = encode(&record)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current = sqlx::query("SELECT data FROM managed_dream WHERE job_id=$1 FOR UPDATE")
            .bind(&record.process_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
            .map(|row| decode_process(record.process_id.clone(), row.try_get(0).map_err(storage)?))
            .transpose()?;
        if current.as_ref() != expected {
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
    }

    async fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamProcessStoreError> {
        sqlx::query("SELECT workspace_id, memory_store_id, data FROM managed_dream_policy ORDER BY workspace_id, memory_store_id")
            .fetch_all(&self.pool).await.map_err(storage)?.into_iter()
            .map(|row| decode_policy(
                row.try_get(0).map_err(storage)?,
                row.try_get(1).map_err(storage)?,
                row.try_get(2).map_err(storage)?,
            ))
            .collect()
    }

    async fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&DreamPolicyRecord>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let data = encode(&record)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current = sqlx::query("SELECT data FROM managed_dream_policy WHERE workspace_id=$1 AND memory_store_id=$2 FOR UPDATE")
            .bind(&record.workspace_id).bind(&record.memory_store_id)
            .fetch_optional(&mut *tx).await.map_err(storage)?
            .map(|row| decode_policy(
                record.workspace_id.clone(),
                record.memory_store_id.clone(),
                row.try_get(0).map_err(storage)?,
            ))
            .transpose()?;
        if current.as_ref() != expected {
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
    }

    async fn claim_dream_policy(
        &self,
        expected_policy: &DreamPolicyRecord,
        policy: DreamPolicyRecord,
        process: DreamProcessRecord,
    ) -> Result<bool, DreamProcessStoreError> {
        let policy_data = encode(&policy)?;
        let process_data = encode(&process)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current = sqlx::query("SELECT data FROM managed_dream_policy WHERE workspace_id=$1 AND memory_store_id=$2 FOR UPDATE")
            .bind(&policy.workspace_id).bind(&policy.memory_store_id)
            .fetch_optional(&mut *tx).await.map_err(storage)?
            .map(|row| decode_policy(
                policy.workspace_id.clone(),
                policy.memory_store_id.clone(),
                row.try_get(0).map_err(storage)?,
            ))
            .transpose()?;
        if current.as_ref() != Some(expected_policy) {
            tx.rollback().await.map_err(storage)?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE managed_dream_policy SET data=$3 WHERE workspace_id=$1 AND memory_store_id=$2",
        )
        .bind(&policy.workspace_id)
        .bind(&policy.memory_store_id)
        .bind(policy_data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let inserted = sqlx::query("INSERT INTO managed_dream (job_id, data) VALUES ($1, $2) ON CONFLICT(job_id) DO NOTHING")
        .bind(process.process_id).bind(process_data).execute(&mut *tx).await.map_err(storage)?.rows_affected();
        if inserted != 1 {
            tx.rollback().await.map_err(storage)?;
            return Ok(false);
        }
        tx.commit().await.map_err(storage)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_store_conformance::dream_store::{dream_store_conformance, process};

    #[tokio::test]
    async fn sqlite_compare_and_swap_and_policy_claim_are_atomic() {
        dream_store_conformance(
            &SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_dream_store_conforms_and_reopens() {
        // R1-R6 execute the same suite against real PostgreSQL. R7 reopen
        // preserves the complete process/policy frames. The port is async, so
        // the PostgreSQL adapter calls sqlx directly without a runtime bridge.
        let Some((url, admin)) = crate::tests::postgres_test_admin().await else {
            return;
        };
        let schema = format!("t_dream_conformance_{}", std::process::id());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let scoped = crate::tests::scoped_postgres_url(&url, &schema);
        let repository = PostgresManagedSessionRepository::connect(&scoped)
            .await
            .unwrap();
        dream_store_conformance(&repository).await;
        let jobs = repository.dream_processes().await.unwrap();
        let policies = repository.dream_policies().await.unwrap();
        drop(repository);
        let reopened = PostgresManagedSessionRepository::connect(&scoped)
            .await
            .unwrap();
        assert_eq!(reopened.dream_processes().await.unwrap(), jobs);
        assert_eq!(reopened.dream_policies().await.unwrap(), policies);
        drop(reopened);
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }

    #[tokio::test]
    async fn sqlite_cas_normalizes_a_legacy_dream_record_through_the_typed_path() {
        // Compatibility cause/effect rules: C1 a retained row uses legacy `id`
        // and duplicated `usage` fields -> E1 it decodes to the canonical typed
        // process; C2 that exact typed value is the CAS expectation -> E2 the
        // transition commits and rewrites one canonical payload without `usage`.
        // R1=C1, R2=C1+C2 prove compatibility is an adapter concern, not a
        // parallel application persistence path.
        let repository = SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap();
        let original = process("dream-legacy");
        let mut legacy = serde_json::to_value(&original).unwrap();
        let fields = legacy.as_object_mut().unwrap();
        let id = fields.remove("process_id").unwrap();
        fields.insert("id".into(), id);
        fields.insert(
            "usage".into(),
            serde_json::json!({"input_tokens": 42, "output_tokens": 7}),
        );
        sqlx::query("INSERT INTO managed_dream (job_id, data) VALUES (?1, ?2)")
            .bind("dream-legacy")
            .bind(legacy.to_string())
            .execute(&repository.pool)
            .await
            .unwrap();

        let loaded = repository.dream_processes().await.unwrap().remove(0);
        assert_eq!(loaded, original, "R1");
        let candidate = DreamProcessRecord {
            status: awaken_session_dream_contract::DreamStatus::Running,
            ..loaded.clone()
        };
        assert!(
            repository
                .compare_and_swap_dream_process(Some(&loaded), candidate.clone())
                .await
                .unwrap(),
            "R2"
        );
        assert_eq!(repository.dream_processes().await.unwrap(), vec![candidate]);
        let raw: String = sqlx::query_scalar("SELECT data FROM managed_dream WHERE job_id=?1")
            .bind("dream-legacy")
            .fetch_one(&repository.pool)
            .await
            .unwrap();
        let normalized: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(normalized.get("process_id").is_some(), "R2");
        assert!(normalized.get("id").is_none(), "R2");
        assert!(normalized.get("usage").is_none(), "R2");
    }
}
