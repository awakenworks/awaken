//! SQLite/Postgres adapters for durable Managed Deployments.

use async_trait::async_trait;
use awaken_deployment_contract::{
    DeploymentLifecycleFact, DeploymentRecord, DeploymentRepository, DeploymentRepositoryError,
    DeploymentRunRecord, DeploymentRunView, DeploymentView, DeploymentWriteOutcome,
    MAX_DEPLOYMENT_REVISION, ScheduledRunClaimOutcome,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sqlx::Row;

use crate::{PostgresManagedSessionRepository, SqliteManagedSessionRepository, lifecycle_str};

fn storage(error: impl std::fmt::Display) -> DeploymentRepositoryError {
    DeploymentRepositoryError::Storage(error.to_string())
}

struct StoredDeploymentRow {
    deployment_id: String,
    workspace_id: String,
    revision: u64,
    data: String,
}

impl StoredDeploymentRow {
    fn encode(view: DeploymentView) -> Result<Self, DeploymentRepositoryError> {
        Ok(Self {
            deployment_id: view.id,
            workspace_id: view.record.workspace_id.clone(),
            revision: view.record.revision,
            data: serde_json::to_string(&view.record).map_err(storage)?,
        })
    }

    fn decode(self) -> Result<DeploymentView, DeploymentRepositoryError> {
        let record: DeploymentRecord = serde_json::from_str(&self.data).map_err(storage)?;
        if record.workspace_id != self.workspace_id || record.revision != self.revision {
            return Err(storage(
                "Deployment indexed facts do not match its durable document",
            ));
        }
        Ok(DeploymentView {
            id: self.deployment_id,
            record,
        })
    }
}

struct StoredDeploymentRunRow {
    run_id: String,
    deployment_id: String,
    workspace_id: String,
    data: String,
}

impl StoredDeploymentRunRow {
    fn encode(view: DeploymentRunView) -> Result<Self, DeploymentRepositoryError> {
        Ok(Self {
            run_id: view.id,
            deployment_id: view.record.deployment_id.clone(),
            workspace_id: view.record.workspace_id.clone(),
            data: serde_json::to_string(&view.record).map_err(storage)?,
        })
    }

    fn decode(self) -> Result<DeploymentRunView, DeploymentRepositoryError> {
        let record: DeploymentRunRecord = serde_json::from_str(&self.data).map_err(storage)?;
        if record.deployment_id != self.deployment_id || record.workspace_id != self.workspace_id {
            return Err(storage(
                "DeploymentRun indexed facts do not match its durable document",
            ));
        }
        Ok(DeploymentRunView {
            id: self.run_id,
            record,
        })
    }
}

fn scheduled_live(data: &str) -> Result<bool, DeploymentRepositoryError> {
    let value: serde_json::Value = serde_json::from_str(data).map_err(storage)?;
    Ok(value
        .get("schedule")
        .is_some_and(|schedule| !schedule.is_null())
        && value
            .get("archived_at")
            .is_none_or(serde_json::Value::is_null))
}

fn db_revision(revision: u64) -> Result<i64, DeploymentRepositoryError> {
    i64::try_from(revision)
        .map_err(|_| DeploymentRepositoryError::Storage("Deployment revision exceeds i64".into()))
}

fn is_exact_revision_step(revision: u64, expected_revision: Option<u64>) -> bool {
    if revision > MAX_DEPLOYMENT_REVISION {
        return false;
    }
    match expected_revision {
        None => revision == 0,
        Some(expected) => expected.checked_add(1) == Some(revision),
    }
}

#[async_trait]
impl DeploymentRepository for SqliteManagedSessionRepository {
    async fn deployments(&self) -> Result<Vec<DeploymentView>, DeploymentRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT deployment_id, workspace_id, revision, data FROM managed_deployment \
                 ORDER BY deployment_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(StoredDeploymentRow {
                    deployment_id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    revision: row.get::<_, i64>(2)?.try_into().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Integer,
                            Box::new(error),
                        )
                    })?,
                    data: row.get(3)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(StoredDeploymentRow::decode)
            .collect()
    }

    async fn deployment_runs(&self) -> Result<Vec<DeploymentRunView>, DeploymentRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT run_id, deployment_id, workspace_id, data \
                 FROM managed_deployment_run ORDER BY run_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(StoredDeploymentRunRow {
                    run_id: row.get(0)?,
                    deployment_id: row.get(1)?,
                    workspace_id: row.get(2)?,
                    data: row.get(3)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(StoredDeploymentRunRow::decode)
            .collect()
    }

    async fn write_deployment(
        &self,
        deployment: DeploymentView,
        expected_revision: Option<u64>,
        scheduled_limit: usize,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<DeploymentWriteOutcome, DeploymentRepositoryError> {
        let record = StoredDeploymentRow::encode(deployment)?;
        if !is_exact_revision_step(record.revision, expected_revision) {
            return Ok(DeploymentWriteOutcome::Conflict);
        }
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if scheduled_live(&record.data)? {
            let mut statement = tx
                .prepare("SELECT deployment_id, data FROM managed_deployment")
                .map_err(storage)?;
            let scheduled = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage)?
                .map(|row| row.map_err(storage))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(id, _)| id != &record.deployment_id)
                .map(|(_, data)| scheduled_live(&data))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|scheduled| *scheduled)
                .count();
            drop(statement);
            if scheduled >= scheduled_limit {
                tx.rollback().map_err(storage)?;
                return Ok(DeploymentWriteOutcome::ScheduledCapacityReached);
            }
        }
        let revision = db_revision(record.revision)?;
        let affected = match expected_revision {
            None => tx
                .execute(
                    "INSERT OR IGNORE INTO managed_deployment \
                     (deployment_id, workspace_id, revision, data) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        record.deployment_id,
                        record.workspace_id,
                        revision,
                        record.data
                    ],
                )
                .map_err(storage)?,
            Some(expected) => tx
                .execute(
                    "UPDATE managed_deployment SET workspace_id=?2, revision=?3, data=?4 \
                     WHERE deployment_id=?1 AND revision=?5",
                    params![
                        record.deployment_id,
                        record.workspace_id,
                        revision,
                        record.data,
                        db_revision(expected)?
                    ],
                )
                .map_err(storage)?,
        };
        if affected == 0 {
            tx.rollback().map_err(storage)?;
            return Ok(DeploymentWriteOutcome::Conflict);
        }
        if let Some(fact) = lifecycle {
            tx.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, lifecycle_str(&fact)],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(DeploymentWriteOutcome::Applied)
    }

    async fn upsert_deployment_run(
        &self,
        run: DeploymentRunView,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError> {
        let record = StoredDeploymentRunRow::encode(run)?;
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
                params![fact.id, lifecycle_str(&fact)],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(())
    }

    async fn claim_scheduled_run(
        &self,
        claim_id: &str,
        expected_deployment_revision: u64,
        deployment: DeploymentView,
        run: DeploymentRunView,
        lifecycle: DeploymentLifecycleFact,
    ) -> Result<ScheduledRunClaimOutcome, DeploymentRepositoryError> {
        let deployment = StoredDeploymentRow::encode(deployment)?;
        let run = StoredDeploymentRunRow::encode(run)?;
        if !is_exact_revision_step(deployment.revision, Some(expected_deployment_revision)) {
            return Ok(ScheduledRunClaimOutcome::StaleDeployment);
        }
        let mut conn = self.conn.lock().map_err(storage)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let current_revision: Option<i64> = tx
            .query_row(
                "SELECT revision FROM managed_deployment WHERE deployment_id=?1",
                params![deployment.deployment_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if current_revision != Some(db_revision(expected_deployment_revision)?) {
            tx.rollback().map_err(storage)?;
            return Ok(ScheduledRunClaimOutcome::StaleDeployment);
        }
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
            return Ok(ScheduledRunClaimOutcome::AlreadyClaimed);
        }
        tx.execute(
            "INSERT INTO managed_deployment_run \
             (run_id, deployment_id, workspace_id, data) VALUES (?1, ?2, ?3, ?4)",
            params![&run.run_id, run.deployment_id, run.workspace_id, run.data],
        )
        .map_err(storage)?;
        tx.execute(
            "INSERT INTO managed_deployment_claim (claim_id, run_id) VALUES (?1, ?2)",
            params![claim_id, run.run_id],
        )
        .map_err(storage)?;
        let affected = tx
            .execute(
                "UPDATE managed_deployment SET workspace_id=?2, revision=?3, data=?4 \
             WHERE deployment_id=?1 AND revision=?5",
                params![
                    deployment.deployment_id,
                    deployment.workspace_id,
                    db_revision(deployment.revision)?,
                    deployment.data,
                    db_revision(expected_deployment_revision)?
                ],
            )
            .map_err(storage)?;
        if affected != 1 {
            tx.rollback().map_err(storage)?;
            return Ok(ScheduledRunClaimOutcome::StaleDeployment);
        }
        tx.execute(
            "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            params![lifecycle.id, lifecycle_str(&lifecycle)],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(ScheduledRunClaimOutcome::Claimed)
    }
}

#[async_trait]
impl DeploymentRepository for PostgresManagedSessionRepository {
    async fn deployments(&self) -> Result<Vec<DeploymentView>, DeploymentRepositoryError> {
        sqlx::query(
            "SELECT deployment_id, workspace_id, revision, data FROM managed_deployment \
             ORDER BY deployment_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            StoredDeploymentRow {
                deployment_id: row.try_get(0).map_err(storage)?,
                workspace_id: row.try_get(1).map_err(storage)?,
                revision: row
                    .try_get::<i64, _>(2)
                    .map_err(storage)?
                    .try_into()
                    .map_err(storage)?,
                data: row.try_get(3).map_err(storage)?,
            }
            .decode()
        })
        .collect()
    }

    async fn deployment_runs(&self) -> Result<Vec<DeploymentRunView>, DeploymentRepositoryError> {
        sqlx::query(
            "SELECT run_id, deployment_id, workspace_id, data \
             FROM managed_deployment_run ORDER BY run_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            StoredDeploymentRunRow {
                run_id: row.try_get(0).map_err(storage)?,
                deployment_id: row.try_get(1).map_err(storage)?,
                workspace_id: row.try_get(2).map_err(storage)?,
                data: row.try_get(3).map_err(storage)?,
            }
            .decode()
        })
        .collect()
    }

    async fn write_deployment(
        &self,
        deployment: DeploymentView,
        expected_revision: Option<u64>,
        scheduled_limit: usize,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<DeploymentWriteOutcome, DeploymentRepositoryError> {
        let record = StoredDeploymentRow::encode(deployment)?;
        if !is_exact_revision_step(record.revision, expected_revision) {
            return Ok(DeploymentWriteOutcome::Conflict);
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // One cooperative global transaction lock makes the organization-wide
        // scheduled limit linearizable without teaching the store the aggregate JSON.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(0x4157_4b4e_4445_504c_i64)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        if scheduled_live(&record.data)? {
            let rows = sqlx::query("SELECT deployment_id, data FROM managed_deployment")
                .fetch_all(&mut *tx)
                .await
                .map_err(storage)?;
            let scheduled = rows
                .into_iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0).map_err(storage)?,
                        row.try_get::<String, _>(1).map_err(storage)?,
                    ))
                })
                .collect::<Result<Vec<_>, DeploymentRepositoryError>>()?
                .into_iter()
                .filter(|(id, _)| id != &record.deployment_id)
                .map(|(_, data)| scheduled_live(&data))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|scheduled| *scheduled)
                .count();
            if scheduled >= scheduled_limit {
                tx.rollback().await.map_err(storage)?;
                return Ok(DeploymentWriteOutcome::ScheduledCapacityReached);
            }
        }
        let affected = match expected_revision {
            None => sqlx::query(
                "INSERT INTO managed_deployment (deployment_id, workspace_id, revision, data) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT(deployment_id) DO NOTHING",
            )
            .bind(&record.deployment_id)
            .bind(&record.workspace_id)
            .bind(db_revision(record.revision)?)
            .bind(&record.data)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected(),
            Some(expected) => sqlx::query(
                "UPDATE managed_deployment SET workspace_id=$2, revision=$3, data=$4 \
                 WHERE deployment_id=$1 AND revision=$5",
            )
            .bind(&record.deployment_id)
            .bind(&record.workspace_id)
            .bind(db_revision(record.revision)?)
            .bind(&record.data)
            .bind(db_revision(expected)?)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected(),
        };
        if affected == 0 {
            tx.rollback().await.map_err(storage)?;
            return Ok(DeploymentWriteOutcome::Conflict);
        }
        if let Some(fact) = lifecycle {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2) \
                 ON CONFLICT(fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(DeploymentWriteOutcome::Applied)
    }

    async fn upsert_deployment_run(
        &self,
        run: DeploymentRunView,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentRepositoryError> {
        let record = StoredDeploymentRunRow::encode(run)?;
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
            .bind(lifecycle_str(&fact))
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
        expected_deployment_revision: u64,
        deployment: DeploymentView,
        run: DeploymentRunView,
        lifecycle: DeploymentLifecycleFact,
    ) -> Result<ScheduledRunClaimOutcome, DeploymentRepositoryError> {
        let deployment = StoredDeploymentRow::encode(deployment)?;
        let run = StoredDeploymentRunRow::encode(run)?;
        if !is_exact_revision_step(deployment.revision, Some(expected_deployment_revision)) {
            return Ok(ScheduledRunClaimOutcome::StaleDeployment);
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current_revision = sqlx::query(
            "SELECT revision FROM managed_deployment WHERE deployment_id=$1 FOR UPDATE",
        )
        .bind(&deployment.deployment_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| row.try_get::<i64, _>(0))
        .transpose()
        .map_err(storage)?;
        if current_revision != Some(db_revision(expected_deployment_revision)?) {
            tx.rollback().await.map_err(storage)?;
            return Ok(ScheduledRunClaimOutcome::StaleDeployment);
        }
        sqlx::query(
            "INSERT INTO managed_deployment_run (run_id, deployment_id, workspace_id, data) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&run.run_id)
        .bind(run.deployment_id)
        .bind(run.workspace_id)
        .bind(run.data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let inserted = sqlx::query(
            "INSERT INTO managed_deployment_claim (claim_id, run_id) VALUES ($1, $2) \
             ON CONFLICT(claim_id) DO NOTHING",
        )
        .bind(claim_id)
        .bind(run.run_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected()
            > 0;
        if !inserted {
            tx.rollback().await.map_err(storage)?;
            return Ok(ScheduledRunClaimOutcome::AlreadyClaimed);
        }
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2) \
             ON CONFLICT(fact_id) DO NOTHING",
        )
        .bind(&lifecycle.id)
        .bind(lifecycle_str(&lifecycle))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let affected = sqlx::query(
            "UPDATE managed_deployment SET workspace_id=$2, revision=$3, data=$4 \
             WHERE deployment_id=$1 AND revision=$5",
        )
        .bind(deployment.deployment_id)
        .bind(deployment.workspace_id)
        .bind(db_revision(deployment.revision)?)
        .bind(deployment.data)
        .bind(db_revision(expected_deployment_revision)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if affected != 1 {
            tx.rollback().await.map_err(storage)?;
            return Ok(ScheduledRunClaimOutcome::StaleDeployment);
        }
        tx.commit().await.map_err(storage)?;
        Ok(ScheduledRunClaimOutcome::Claimed)
    }
}
