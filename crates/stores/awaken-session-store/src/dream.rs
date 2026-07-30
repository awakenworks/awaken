//! Durable Dream job and Workspace-override adapters.

use awaken_ext_memory::{
    DreamJobRecord, DreamPolicyRecord, DreamRepository, DreamRepositoryError,
    WorkspaceDreamAgentOverride,
};
use rusqlite::{TransactionBehavior, params};
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

fn storage(error: impl std::fmt::Display) -> DreamRepositoryError {
    DreamRepositoryError::Storage(error.to_string())
}

impl DreamRepository for SqliteManagedSessionRepository {
    fn dream_jobs(&self) -> Result<Vec<DreamJobRecord>, DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare("SELECT job_id, data FROM managed_dream ORDER BY job_id")
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(DreamJobRecord {
                    job_id: row.get(0)?,
                    data: row.get(1)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn compare_and_swap_dream_job(
        &self,
        expected: Option<&str>,
        record: DreamJobRecord,
    ) -> Result<bool, DreamRepositoryError> {
        let changed = match expected {
            None => self.conn.lock().map_err(storage)?.execute(
                "INSERT OR IGNORE INTO managed_dream (job_id, data) VALUES (?1, ?2)",
                params![record.job_id, record.data],
            ),
            Some(expected) => self.conn.lock().map_err(storage)?.execute(
                "UPDATE managed_dream SET data=?2 WHERE job_id=?1 AND data=?3",
                params![record.job_id, record.data, expected],
            ),
        }
        .map_err(storage)?;
        Ok(changed == 1)
    }

    fn dream_agent_overrides(
        &self,
    ) -> Result<Vec<WorkspaceDreamAgentOverride>, DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT workspace_id, agent_id FROM managed_dream_agent_override
                 ORDER BY workspace_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(WorkspaceDreamAgentOverride {
                    workspace_id: row.get(0)?,
                    agent_id: row.get(1)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn set_dream_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        match agent_id {
            Some(agent_id) => conn.execute(
                "INSERT INTO managed_dream_agent_override (workspace_id, agent_id)
                 VALUES (?1, ?2)
                 ON CONFLICT(workspace_id) DO UPDATE SET agent_id=excluded.agent_id",
                params![workspace_id, agent_id],
            ),
            None => conn.execute(
                "DELETE FROM managed_dream_agent_override WHERE workspace_id=?1",
                params![workspace_id],
            ),
        }
        .map_err(storage)?;
        Ok(())
    }

    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT workspace_id, memory_store_id, data FROM managed_dream_policy \
                 ORDER BY workspace_id, memory_store_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(DreamPolicyRecord {
                    workspace_id: row.get(0)?,
                    memory_store_id: row.get(1)?,
                    data: row.get(2)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&str>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamRepositoryError> {
        let changed = match expected {
            None => self.conn.lock().map_err(storage)?.execute(
                "INSERT OR IGNORE INTO managed_dream_policy \
                 (workspace_id, memory_store_id, data) VALUES (?1, ?2, ?3)",
                params![record.workspace_id, record.memory_store_id, record.data],
            ),
            Some(expected) => self.conn.lock().map_err(storage)?.execute(
                "UPDATE managed_dream_policy SET data=?3 \
                 WHERE workspace_id=?1 AND memory_store_id=?2 AND data=?4",
                params![
                    record.workspace_id,
                    record.memory_store_id,
                    record.data,
                    expected
                ],
            ),
        }
        .map_err(storage)?;
        Ok(changed == 1)
    }

    fn claim_dream_policy(
        &self,
        expected_policy_data: &str,
        policy: DreamPolicyRecord,
        job: DreamJobRecord,
    ) -> Result<bool, DreamRepositoryError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let changed = tx
            .execute(
                "UPDATE managed_dream_policy SET data=?3 \
                 WHERE workspace_id=?1 AND memory_store_id=?2 AND data=?4",
                params![
                    policy.workspace_id,
                    policy.memory_store_id,
                    policy.data,
                    expected_policy_data
                ],
            )
            .map_err(storage)?;
        if changed != 1 {
            return Ok(false);
        }
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO managed_dream (job_id, data) VALUES (?1, ?2)",
                params![job.job_id, job.data],
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

impl DreamRepository for PostgresManagedSessionRepository {
    fn dream_jobs(&self) -> Result<Vec<DreamJobRecord>, DreamRepositoryError> {
        let pool = self.pool.clone();
        postgres_block(move || {
            Box::pin(async move {
                sqlx::query("SELECT job_id, data FROM managed_dream ORDER BY job_id")
                    .fetch_all(&pool)
                    .await
                    .map_err(storage)?
                    .into_iter()
                    .map(|row| {
                        Ok(DreamJobRecord {
                            job_id: row.try_get(0).map_err(storage)?,
                            data: row.try_get(1).map_err(storage)?,
                        })
                    })
                    .collect()
            })
        })
    }

    fn compare_and_swap_dream_job(
        &self,
        expected: Option<&str>,
        record: DreamJobRecord,
    ) -> Result<bool, DreamRepositoryError> {
        let pool = self.pool.clone();
        let expected = expected.map(str::to_string);
        postgres_block(move || {
            Box::pin(async move {
                let result = match expected {
                None => sqlx::query("INSERT INTO managed_dream (job_id, data) VALUES ($1, $2) ON CONFLICT(job_id) DO NOTHING")
                    .bind(record.job_id).bind(record.data).execute(&pool).await,
                Some(expected) => sqlx::query("UPDATE managed_dream SET data=$2 WHERE job_id=$1 AND data=$3")
                    .bind(record.job_id).bind(record.data).bind(expected).execute(&pool).await,
            }.map_err(storage)?;
                Ok(result.rows_affected() == 1)
            })
        })
    }

    fn dream_agent_overrides(
        &self,
    ) -> Result<Vec<WorkspaceDreamAgentOverride>, DreamRepositoryError> {
        let pool = self.pool.clone();
        postgres_block(move || {
            Box::pin(async move {
                sqlx::query("SELECT workspace_id, agent_id FROM managed_dream_agent_override ORDER BY workspace_id")
                .fetch_all(&pool).await.map_err(storage)?.into_iter()
                .map(|row| Ok(WorkspaceDreamAgentOverride { workspace_id: row.try_get(0).map_err(storage)?, agent_id: row.try_get(1).map_err(storage)? }))
                .collect()
            })
        })
    }

    fn set_dream_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), DreamRepositoryError> {
        let pool = self.pool.clone();
        let workspace_id = workspace_id.to_string();
        let agent_id = agent_id.map(str::to_string);
        postgres_block(move || {
            Box::pin(async move {
                match agent_id {
                Some(agent_id) => sqlx::query("INSERT INTO managed_dream_agent_override (workspace_id, agent_id) VALUES ($1, $2) ON CONFLICT(workspace_id) DO UPDATE SET agent_id=excluded.agent_id")
                    .bind(workspace_id).bind(agent_id).execute(&pool).await,
                None => sqlx::query("DELETE FROM managed_dream_agent_override WHERE workspace_id=$1")
                    .bind(workspace_id).execute(&pool).await,
            }.map_err(storage)?;
                Ok(())
            })
        })
    }

    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamRepositoryError> {
        let pool = self.pool.clone();
        postgres_block(move || {
            Box::pin(async move {
                sqlx::query("SELECT workspace_id, memory_store_id, data FROM managed_dream_policy ORDER BY workspace_id, memory_store_id")
                .fetch_all(&pool).await.map_err(storage)?.into_iter()
                .map(|row| Ok(DreamPolicyRecord { workspace_id: row.try_get(0).map_err(storage)?, memory_store_id: row.try_get(1).map_err(storage)?, data: row.try_get(2).map_err(storage)? }))
                .collect()
            })
        })
    }

    fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&str>,
        record: DreamPolicyRecord,
    ) -> Result<bool, DreamRepositoryError> {
        let pool = self.pool.clone();
        let expected = expected.map(str::to_string);
        postgres_block(move || {
            Box::pin(async move {
                let result = match expected {
                None => sqlx::query("INSERT INTO managed_dream_policy (workspace_id, memory_store_id, data) VALUES ($1, $2, $3) ON CONFLICT(workspace_id, memory_store_id) DO NOTHING")
                    .bind(record.workspace_id).bind(record.memory_store_id).bind(record.data).execute(&pool).await,
                Some(expected) => sqlx::query("UPDATE managed_dream_policy SET data=$3 WHERE workspace_id=$1 AND memory_store_id=$2 AND data=$4")
                    .bind(record.workspace_id).bind(record.memory_store_id).bind(record.data).bind(expected).execute(&pool).await,
            }.map_err(storage)?;
                Ok(result.rows_affected() == 1)
            })
        })
    }

    fn claim_dream_policy(
        &self,
        expected_policy_data: &str,
        policy: DreamPolicyRecord,
        job: DreamJobRecord,
    ) -> Result<bool, DreamRepositoryError> {
        let pool = self.pool.clone();
        let expected_policy_data = expected_policy_data.to_string();
        postgres_block(move || {
            Box::pin(async move {
                let mut tx = pool.begin().await.map_err(storage)?;
                let changed = sqlx::query("UPDATE managed_dream_policy SET data=$3 WHERE workspace_id=$1 AND memory_store_id=$2 AND data=$4")
                .bind(policy.workspace_id).bind(policy.memory_store_id).bind(policy.data).bind(expected_policy_data)
                .execute(&mut *tx).await.map_err(storage)?.rows_affected();
                if changed != 1 {
                    tx.rollback().await.map_err(storage)?;
                    return Ok(false);
                }
                let inserted = sqlx::query("INSERT INTO managed_dream (job_id, data) VALUES ($1, $2) ON CONFLICT(job_id) DO NOTHING")
                .bind(job.job_id).bind(job.data).execute(&mut *tx).await.map_err(storage)?.rows_affected();
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

    #[test]
    fn sqlite_compare_and_swap_and_policy_claim_are_atomic() {
        // Persistence decision table: R1 absent job/policy + insert -> accepted;
        // R2 stale expected JSON -> rejected without mutation; R3 exact policy
        // version + fresh job -> both commit; R4 stale policy + fresh job ->
        // neither commits. These rules are the multi-replica Dream claim fence.
        let repository = SqliteManagedSessionRepository::open_in_memory().unwrap();
        let policy = DreamPolicyRecord {
            workspace_id: "workspace".into(),
            memory_store_id: "store".into(),
            data: "policy-v1".into(),
        };
        assert!(
            repository
                .compare_and_swap_dream_policy(None, policy.clone())
                .unwrap()
        );
        assert!(
            !repository
                .compare_and_swap_dream_policy(
                    Some("stale"),
                    DreamPolicyRecord {
                        data: "policy-bad".into(),
                        ..policy.clone()
                    },
                )
                .unwrap()
        );
        let policy_v2 = DreamPolicyRecord {
            data: "policy-v2".into(),
            ..policy.clone()
        };
        assert!(
            repository
                .claim_dream_policy(
                    "policy-v1",
                    policy_v2.clone(),
                    DreamJobRecord {
                        job_id: "dream-1".into(),
                        data: "job-1".into(),
                    },
                )
                .unwrap()
        );
        assert!(
            !repository
                .claim_dream_policy(
                    "policy-v1",
                    DreamPolicyRecord {
                        data: "policy-v3".into(),
                        ..policy
                    },
                    DreamJobRecord {
                        job_id: "dream-2".into(),
                        data: "job-2".into(),
                    },
                )
                .unwrap()
        );
        assert_eq!(repository.dream_jobs().unwrap().len(), 1);
        assert_eq!(repository.dream_policies().unwrap()[0], policy_v2);
    }
}
