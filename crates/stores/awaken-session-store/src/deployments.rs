//! SQLite/Postgres adapters for durable Managed Deployments.

use async_trait::async_trait;
use awaken_deployment_contract::{
    DeploymentLifecycleFact, DeploymentRecord, DeploymentRepository, DeploymentRepositoryError,
    DeploymentRunRecord,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};

fn storage(error: impl std::fmt::Display) -> DeploymentRepositoryError {
    DeploymentRepositoryError::Storage(error.to_string())
}

fn deployment_lifecycle_str(fact: &DeploymentLifecycleFact) -> String {
    serde_json::to_string(fact).expect("Deployment lifecycle fact serializes")
}

#[async_trait]
impl DeploymentRepository for SqliteManagedSessionRepository {
    async fn deployments(&self) -> Result<Vec<DeploymentRecord>, DeploymentRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT deployment_id, workspace_id, data FROM managed_deployment \
                 ORDER BY deployment_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(DeploymentRecord {
                    deployment_id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    data: row.get(2)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    async fn deployment_runs(&self) -> Result<Vec<DeploymentRunRecord>, DeploymentRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT run_id, deployment_id, workspace_id, data \
                 FROM managed_deployment_run ORDER BY run_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(DeploymentRunRecord {
                    run_id: row.get(0)?,
                    deployment_id: row.get(1)?,
                    workspace_id: row.get(2)?,
                    data: row.get(3)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    async fn upsert_deployment(
        &self,
        record: DeploymentRecord,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        tx.execute(
            "INSERT INTO managed_deployment (deployment_id, workspace_id, data) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(deployment_id) DO UPDATE SET \
                 workspace_id=excluded.workspace_id, data=excluded.data",
            params![record.deployment_id, record.workspace_id, record.data],
        )
        .map_err(storage)?;
        if let Some(fact) = lifecycle {
            tx.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, deployment_lifecycle_str(&fact)],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(())
    }

    async fn upsert_deployment_run(
        &self,
        record: DeploymentRunRecord,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn.transaction().map_err(storage)?;
        tx.execute(
            "INSERT INTO managed_deployment_run \
                 (run_id, deployment_id, workspace_id, data) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(run_id) DO UPDATE SET data=excluded.data",
            params![
                record.run_id,
                record.deployment_id,
                record.workspace_id,
                record.data
            ],
        )
        .map_err(storage)?;
        if let Some(fact) = lifecycle {
            tx.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, deployment_lifecycle_str(&fact)],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(())
    }

    async fn claim_scheduled_run(
        &self,
        claim_id: &str,
        deployment: DeploymentRecord,
        run: DeploymentRunRecord,
        lifecycle: DeploymentLifecycleFact,
    ) -> Result<bool, DeploymentRepositoryError> {
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT run_id FROM managed_deployment_claim WHERE claim_id=?1",
                params![claim_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if existing.is_some() {
            tx.commit().map_err(storage)?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO managed_deployment_claim (claim_id, run_id) VALUES (?1, ?2)",
            params![claim_id, run.run_id],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT INTO managed_deployment_run \
             (run_id, deployment_id, workspace_id, data) VALUES (?1, ?2, ?3, ?4)",
            params![run.run_id, run.deployment_id, run.workspace_id, run.data],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT INTO managed_deployment (deployment_id, workspace_id, data) \
             VALUES (?1, ?2, ?3) ON CONFLICT(deployment_id) DO UPDATE SET \
             workspace_id=excluded.workspace_id, data=excluded.data",
            params![
                deployment.deployment_id,
                deployment.workspace_id,
                deployment.data
            ],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            params![lifecycle.id, deployment_lifecycle_str(&lifecycle)],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(true)
    }
}

#[async_trait]
impl DeploymentRepository for PostgresManagedSessionRepository {
    async fn deployments(&self) -> Result<Vec<DeploymentRecord>, DeploymentRepositoryError> {
        sqlx::query(
            "SELECT deployment_id, workspace_id, data FROM managed_deployment \
             ORDER BY deployment_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            Ok(DeploymentRecord {
                deployment_id: row.try_get(0).map_err(storage)?,
                workspace_id: row.try_get(1).map_err(storage)?,
                data: row.try_get(2).map_err(storage)?,
            })
        })
        .collect()
    }

    async fn deployment_runs(&self) -> Result<Vec<DeploymentRunRecord>, DeploymentRepositoryError> {
        sqlx::query(
            "SELECT run_id, deployment_id, workspace_id, data \
             FROM managed_deployment_run ORDER BY run_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            Ok(DeploymentRunRecord {
                run_id: row.try_get(0).map_err(storage)?,
                deployment_id: row.try_get(1).map_err(storage)?,
                workspace_id: row.try_get(2).map_err(storage)?,
                data: row.try_get(3).map_err(storage)?,
            })
        })
        .collect()
    }

    async fn upsert_deployment(
        &self,
        record: DeploymentRecord,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query(
            "INSERT INTO managed_deployment (deployment_id, workspace_id, data) \
             VALUES ($1, $2, $3) ON CONFLICT(deployment_id) DO UPDATE SET \
             workspace_id=excluded.workspace_id, data=excluded.data",
        )
        .bind(record.deployment_id)
        .bind(record.workspace_id)
        .bind(record.data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(fact) = lifecycle {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2) \
                 ON CONFLICT(fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(deployment_lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn upsert_deployment_run(
        &self,
        record: DeploymentRunRecord,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query(
            "INSERT INTO managed_deployment_run (run_id, deployment_id, workspace_id, data) \
             VALUES ($1, $2, $3, $4) ON CONFLICT(run_id) DO UPDATE SET data=excluded.data",
        )
        .bind(record.run_id)
        .bind(record.deployment_id)
        .bind(record.workspace_id)
        .bind(record.data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(fact) = lifecycle {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2) \
                 ON CONFLICT(fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(deployment_lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn claim_scheduled_run(
        &self,
        claim_id: &str,
        deployment: DeploymentRecord,
        run: DeploymentRunRecord,
        lifecycle: DeploymentLifecycleFact,
    ) -> Result<bool, DeploymentRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let inserted = sqlx::query(
            "INSERT INTO managed_deployment_claim (claim_id, run_id) VALUES ($1, $2) \
             ON CONFLICT(claim_id) DO NOTHING",
        )
        .bind(claim_id)
        .bind(&run.run_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected()
            > 0;
        if !inserted {
            tx.rollback().await.map_err(storage)?;
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO managed_deployment_run (run_id, deployment_id, workspace_id, data) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(run.run_id)
        .bind(run.deployment_id)
        .bind(run.workspace_id)
        .bind(run.data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2) \
             ON CONFLICT(fact_id) DO NOTHING",
        )
        .bind(&lifecycle.id)
        .bind(deployment_lifecycle_str(&lifecycle))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query(
            "INSERT INTO managed_deployment (deployment_id, workspace_id, data) \
             VALUES ($1, $2, $3) ON CONFLICT(deployment_id) DO UPDATE SET \
             workspace_id=excluded.workspace_id, data=excluded.data",
        )
        .bind(deployment.deployment_id)
        .bind(deployment.workspace_id)
        .bind(deployment.data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(true)
    }
}
