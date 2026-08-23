//! PostgreSQL implementation of the canonical environment WorkQueue.
//!
//! The contract and backend-independent conformance tests remain owned by the
//! parent module. This module owns only distributed SQL transaction mechanics.

use super::*;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

fn pg_row_to_item(row: &PgRow) -> WorkItem {
    let metadata_json: String = row.get("metadata_json");
    build_item(
        row.get("work_id"),
        row.get("environment_id"),
        &row.get::<String, _>("data_type"),
        row.get("data_id"),
        &metadata_json,
        &row.get::<String, _>("state"),
        row.get("acknowledged_at"),
        row.get("latest_heartbeat_at"),
        row.get("started_at"),
        row.get("stop_requested_at"),
        row.get("stopped_at"),
    )
}

/// A Postgres-backed [`WorkQueue`] — the network-DB sibling over the same
/// `work_queue` migration scope, for distributed deployments. Claims run in a
/// transaction that mirrors the SQLite semantics (the single-active-per-env cap);
/// the transaction locks the environment rows before the single-active decision.
pub struct PostgresWorkQueue {
    pool: PgPool,
    book: LeaseBook,
}

impl PostgresWorkQueue {
    /// Connect and apply the work-queue migrations under the `work_queue` namespace.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the work-queue migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = work_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
        })
    }

    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        let bundle = work_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            book: LeaseBook::default(),
        })
    }

    async fn insert(
        &self,
        env_id: &str,
        data_type: &str,
        session_id: Option<&str>,
    ) -> Result<String, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Serialize the portable MAX(seq)+1 allocator. This is infrequent control
        // plane work and avoids a backend-specific sequence while remaining safe
        // across processes.
        sqlx::query("LOCK TABLE work_queue_item IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        if let Some(session_id) = session_id
            && let Some(existing) = sqlx::query_scalar::<_, String>(
                "SELECT work_id FROM work_queue_item \
                 WHERE environment_id = $1 AND data_type = 'session' AND data_id = $2 \
                 ORDER BY seq ASC LIMIT 1",
            )
            .bind(env_id)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
        {
            tx.commit().await.map_err(storage)?;
            return Ok(existing);
        }
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        let data_id = session_id.unwrap_or(&work_id).to_string();
        sqlx::query(
            "INSERT INTO work_queue_item \
                (work_id, seq, environment_id, data_type, data_id, metadata_json, state) \
             VALUES ($1, $2, $3, $4, $5, '{}', 'queued')",
        )
        .bind(&work_id)
        .bind(next)
        .bind(env_id)
        .bind(data_type)
        .bind(&data_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(work_id)
    }

    async fn fetch_owned(
        &self,
        env_id: &str,
        wid: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE work_id = $1 AND environment_id = $2"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?
        .map(|r| pg_row_to_item(&r)))
    }
}

#[async_trait]
impl WorkQueue for PostgresWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        self.insert(env_id, "session", Some(session_id)).await
    }

    async fn wake_session(&self, env_id: &str, session_id: &str) -> Result<String, WorkQueueError> {
        let work_id = self.insert(env_id, "session", Some(session_id)).await?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', acknowledged_at = NULL, \
             latest_heartbeat_at = NULL, started_at = NULL, stop_requested_at = NULL, \
             stopped_at = NULL, lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE work_id = $1 AND environment_id = $2 \
             AND data_type = 'session' AND data_id = $3 AND state = 'stopped'",
        )
        .bind(&work_id)
        .bind(env_id)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(work_id)
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        self.insert(env_id, "healthcheck", None).await
    }

    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("LOCK TABLE work_queue_item IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        if let Some(id) = sqlx::query_scalar::<_, String>(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 AND data_type = 'healthcheck' ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        {
            tx.commit().await.map_err(storage)?;
            return Ok(id);
        }
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM work_queue_item")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        let work_id = format!("work_{next:016}");
        sqlx::query("INSERT INTO work_queue_item (work_id, seq, environment_id, data_type, data_id, metadata_json, state) VALUES ($1, $2, $3, 'healthcheck', $1, '{}', 'queued')")
            .bind(&work_id).bind(next).bind(env_id)
            .execute(&mut *tx).await.map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(work_id)
    }

    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        Ok(sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE environment_id = $1 ORDER BY seq ASC"
        ))
        .bind(env_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .iter()
        .map(pg_row_to_item)
        .collect())
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        self.fetch_owned(env_id, wid).await
    }

    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.claim_with_reclaim(env_id, worker_id, worker_id, now_ms, None)
            .await
            .map(|claimed| claimed.map(ClaimedWork::into_item))
    }

    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        age_ms: Option<u64>,
    ) -> Result<Option<ClaimedWork>, WorkQueueError> {
        self.book.record_poll(env_id, poller_id, now_ms);
        if let Some(age) = age_ms.filter(|age| *age <= now_ms) {
            let cutoff = db_millis(now_ms - age);
            sqlx::query(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
                 WHERE environment_id = $1 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= $2",
            )
            .bind(env_id)
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Lock the environment's rows before count/select so concurrent pollers
        // cannot both observe zero active work and lease different rows.
        let _: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, \
                 lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE environment_id = $1 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= $2)",
        )
        .bind(env_id)
        .bind(db_millis(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        // Single active lease per environment (the open-tier single-worker cap).
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
        )
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if active > 0 {
            return Ok(None);
        }
        // Lease the oldest queued item, locking the chosen row.
        let wid: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item \
             WHERE environment_id = $1 AND state = 'queued' \
             ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(wid) = wid else {
            return Ok(None);
        };
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = $1, lease_owner = $2, \
                 lease_epoch = lease_epoch + 1, lease_expires_ms = $3, \
                 lease_refreshed_ms = $4, latest_heartbeat_at = NULL \
             WHERE work_id = $5",
        )
        .bind(OBJECT_AT)
        .bind(lease_owner)
        .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
        .bind(db_millis(now_ms))
        .bind(&wid)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT {COLS}, lease_epoch, lease_expires_ms FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2"
        ))
        .bind(&wid)
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let item = pg_row_to_item(&row);
        let session_lease = match &item.data {
            WorkPayload::Session { id } => {
                let (_, epoch) = lease_epoch(row.get("lease_epoch"), false)?;
                let expires: i64 = row.try_get("lease_expires_ms").map_err(storage)?;
                Some(SessionWorkLease {
                    work_id: wid,
                    environment_id: env_id.to_string(),
                    session_id: id.clone(),
                    owner: lease_owner.to_string(),
                    epoch,
                    expires_at_unix_ms: u64::try_from(expires).map_err(storage)?,
                })
            }
            WorkPayload::HealthCheck { .. } => None,
        };
        tx.commit().await.map_err(storage)?;
        Ok(Some(ClaimedWork {
            item,
            session_lease,
        }))
    }

    async fn current_session_lease(
        &self,
        env_id: &str,
        session_id: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let row: Option<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT work_id, lease_owner, lease_epoch, lease_expires_ms \
             FROM work_queue_item WHERE environment_id = $1 AND data_type = 'session' \
             AND data_id = $2 AND state = 'active' AND lease_expires_ms > $3",
        )
        .bind(env_id)
        .bind(session_id)
        .bind(db_millis(now_ms))
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|(work_id, owner, epoch, expires)| {
            let (_, epoch) = lease_epoch(epoch, false)?;
            Ok(SessionWorkLease {
                work_id,
                environment_id: env_id.to_string(),
                session_id: session_id.to_string(),
                owner,
                epoch,
                expires_at_unix_ms: u64::try_from(expires).map_err(storage)?,
            })
        })
        .transpose()
    }

    async fn release_claim(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError> {
        let epoch = i64::try_from(lease.epoch).map_err(storage)?;
        let changed = sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, \
             lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE work_id = $1 AND environment_id = $2 AND data_type = 'session' \
             AND data_id = $3 AND state = 'active' AND lease_owner = $4 AND lease_epoch = $5",
        )
        .bind(&lease.work_id)
        .bind(&lease.environment_id)
        .bind(&lease.session_id)
        .bind(&lease.owner)
        .bind(epoch)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected();
        Ok(changed == 1)
    }

    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT state, lease_owner FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((state, owner)) = current else {
            return Ok(WorkMutationResult::NotFound);
        };
        if owner.as_deref() != Some(worker_id) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let next = awaken_session_contract::work_queue::WorkState::from_wire(&state)
            .unwrap_or(awaken_session_contract::work_queue::WorkState::Stopped)
            .after_ack()
            .as_str();
        sqlx::query(
            "UPDATE work_queue_item SET acknowledged_at = $1, state = $2 \
             WHERE work_id = $3 AND environment_id = $4",
        )
        .bind(OBJECT_AT)
        .bind(next)
        .bind(wid)
        .bind(env_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(self
            .fetch_owned(env_id, wid)
            .await?
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current = sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2 FOR UPDATE"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| pg_row_to_item(&row));
        let Some(current) = current else {
            return Ok(HeartbeatResult::NotFound);
        };
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT lease_owner FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if owner.as_deref() != Some(worker_id) {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        if !heartbeat
            .condition
            .permits(current.latest_heartbeat_at.as_deref())
        {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        let extended = current.state.can_extend_lease();
        let ttl_seconds = effective_ttl_seconds(heartbeat.desired_ttl_seconds);
        let last_heartbeat = heartbeat_at(now_ms, current.latest_heartbeat_at.as_deref());
        if extended {
            sqlx::query(
                "UPDATE work_queue_item SET latest_heartbeat_at = $1, lease_expires_ms = $2, lease_refreshed_ms = $3 \
                 WHERE work_id = $4 AND environment_id = $5 AND state = 'active' \
                   AND lease_owner = $6",
            )
            .bind(&last_heartbeat)
            .bind(lease_expiry(now_ms, ttl_seconds))
            .bind(db_millis(now_ms))
            .bind(wid)
            .bind(env_id)
            .bind(worker_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(HeartbeatResult::Accepted(LeaseReceipt {
            last_heartbeat,
            lease_extended: extended,
            state: current.state,
            ttl_seconds,
        }))
    }

    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let owner: Option<Option<String>> = sqlx::query_scalar(
            "SELECT lease_owner FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(owner) = owner else {
            return Ok(WorkMutationResult::NotFound);
        };
        if owner.as_deref() != Some(worker_id) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL \
             WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(wid)
        .bind(env_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(self
            .fetch_owned(env_id, wid)
            .await?
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        let released = sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = NULL, stopped_at = NULL, \
             state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE data_type = 'session' AND state = 'active' AND lease_owner = $1",
        )
        .bind(worker_owner)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected();
        usize::try_from(released)
            .map_err(|_| WorkQueueError::Storage("released row count overflow".into()))
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let work_id: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 \
             AND data_type = 'session' AND data_id = $2 ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some(work_id) = work_id else {
            return Ok(None);
        };
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(&work_id)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        self.fetch_owned(env_id, &work_id).await
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let work_id = self.insert(env_id, "session", Some(session_id)).await?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let _: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, \
             lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL \
             WHERE environment_id = $1 AND state = 'active' \
             AND (lease_expires_ms IS NULL OR lease_expires_ms <= $2)",
        )
        .bind(env_id)
        .bind(db_millis(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let current: (String, Option<String>, i64, Option<i64>) = sqlx::query_as(
            "SELECT state, lease_owner, lease_epoch, lease_expires_ms FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2",
        )
        .bind(&work_id)
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if current.0 == "active" && current.1.as_deref() != Some(worker_owner) {
            let owner = current.1.clone().ok_or_else(|| {
                WorkQueueError::Storage("active Session Work has no lease owner".into())
            })?;
            let (_, epoch) = lease_epoch(current.2, false)?;
            let expires_at_unix_ms = u64::try_from(current.3.unwrap_or_default())
                .map_err(|_| WorkQueueError::Storage("negative Session Work expiry".into()))?;
            return Ok(Some(SessionWorkLease {
                work_id,
                environment_id: env_id.to_string(),
                session_id: session_id.to_string(),
                owner,
                epoch,
                expires_at_unix_ms,
            }));
        }
        let epoch = if current.0 == "active" && current.1.as_deref() == Some(worker_owner) {
            let (_, epoch) = lease_epoch(current.2, false)?;
            sqlx::query(
                "UPDATE work_queue_item SET lease_expires_ms = $1, lease_refreshed_ms = $2 \
                 WHERE work_id = $3 AND environment_id = $4 AND lease_owner = $5",
            )
            .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
            .bind(db_millis(now_ms))
            .bind(&work_id)
            .bind(env_id)
            .bind(worker_owner)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            epoch
        } else {
            let active: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
            )
            .bind(env_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
            if active > 0 || current.0 != "queued" {
                return Ok(None);
            }
            let (epoch_db, epoch) = lease_epoch(current.2, true)?;
            sqlx::query(
                "UPDATE work_queue_item SET state = 'active', started_at = $1, \
                 lease_owner = $2, lease_epoch = $3, lease_expires_ms = $4, \
                 lease_refreshed_ms = $5, latest_heartbeat_at = NULL WHERE work_id = $6",
            )
            .bind(OBJECT_AT)
            .bind(worker_owner)
            .bind(epoch_db)
            .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
            .bind(db_millis(now_ms))
            .bind(&work_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            epoch
        };
        tx.commit().await.map_err(storage)?;
        Ok(Some(SessionWorkLease {
            work_id,
            environment_id: env_id.to_string(),
            session_id: session_id.to_string(),
            owner: worker_owner.to_string(),
            epoch,
            expires_at_unix_ms: now_ms.saturating_add(HEARTBEAT_TTL_SECONDS * 1_000),
        }))
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let Some(mut current) = self.fetch_owned(env_id, wid).await? else {
            return Ok(None);
        };
        apply_metadata_patch(&mut current.metadata, patch);
        let metadata_json = metadata_str(&current.metadata);
        sqlx::query(
            "UPDATE work_queue_item SET metadata_json = $1 WHERE work_id = $2 AND environment_id = $3",
        )
        .bind(metadata_json)
        .bind(wid)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        self.fetch_owned(env_id, wid).await
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError> {
        let count = |clause: &'static str| {
            let pool = self.pool.clone();
            let env = env_id.to_string();
            async move {
                sqlx::query_scalar::<_, i64>(&format!(
                    "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND {clause}"
                ))
                .bind(env)
                .fetch_one(&pool)
                .await
                .map_err(storage)
                .map(|count| count as usize)
            }
        };
        let depth = count("state = 'queued'").await?;
        let pending = count("state IN ('starting', 'active', 'stopping')").await?;
        // Parity with the in-memory queue: oldest persists while processing, and
        // pollers come from the liveness book (not the active-count proxy).
        let has_unfinished = depth > 0 || pending > 0;
        Ok(QueueStats {
            depth,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling: self.book.workers_polling(env_id, now_ms),
        })
    }

    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("LOCK TABLE work_queue_item IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query("DELETE FROM work_queue_item WHERE environment_id = $1")
            .bind(env_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        self.book.forget_env(env_id, &ids);
        Ok(())
    }
}
