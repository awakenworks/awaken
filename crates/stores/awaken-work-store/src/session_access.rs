//! Session-scoped bearer issuance and epoch-fenced lease mutations.

use super::*;
use awaken_agent_contract::RedactedString;
use awaken_iam_core::{EntropySource, OsEntropy};
use base64::Engine as _;
use sha2::{Digest, Sha256};

const SESSION_TOKEN_BYTES: usize = 32;

pub(crate) fn issue_session_token() -> RedactedString {
    let mut bytes = [0_u8; SESSION_TOKEN_BYTES];
    OsEntropy.fill_bytes(&mut bytes);
    RedactedString::new(format!(
        "sk-ant-req-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

pub(crate) fn session_token_sha256(token: &RedactedString) -> String {
    let mut digest = Sha256::new();
    digest.update(b"awaken-work-session-token-v1\0");
    digest.update(token.expose_secret().as_bytes());
    format!("{:x}", digest.finalize())
}

impl SqliteWorkQueue {
    pub(super) fn claim_inner(
        conn: &mut Connection,
        env_id: &str,
        lease_owner: &str,
        now_ms: u64,
        age_ms: Option<u64>,
        mint_session_access: bool,
    ) -> Result<Option<ClaimedWork>, WorkQueueError> {
        if let Some(cutoff) = reclaim_cutoff(now_ms, age_ms) {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            tx.execute(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
                 WHERE environment_id = ?1 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= ?2",
                params![env_id, cutoff],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)?;
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        Self::reclaim_lapsed(&tx, env_id, now_ms)?;
        let active: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
                params![env_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if active > 0 {
            return Ok(None);
        }
        let selected: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT work_id, data_type, lease_epoch FROM work_queue_item \
                 WHERE environment_id = ?1 AND state = 'queued' ORDER BY seq ASC LIMIT 1",
                params![env_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(storage)?;
        let Some((wid, data_type, current_epoch)) = selected else {
            return Ok(None);
        };
        let (next_epoch, _) = lease_epoch(current_epoch, true)?;
        let sessions_token =
            (mint_session_access && data_type == "session").then(issue_session_token);
        let token_hash = sessions_token.as_ref().map(session_token_sha256);
        tx.execute(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = ?1, lease_owner = ?2, \
                 lease_epoch = ?7, lease_expires_ms = ?3, \
                 lease_refreshed_ms = ?4, latest_heartbeat_at = NULL, \
                 session_token_sha256 = ?5 WHERE work_id = ?6",
            params![
                OBJECT_AT,
                lease_owner,
                lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
                db_millis(now_ms),
                token_hash,
                wid,
                next_epoch,
            ],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, &wid)?;
        tx.commit().map_err(storage)?;
        Ok(item.map(|item| ClaimedWork {
            item,
            sessions_token,
        }))
    }

    fn lease_matches(
        tx: &Transaction<'_>,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
    ) -> Result<bool, WorkQueueError> {
        let authority: (Option<String>, i64) = tx
            .query_row(
                "SELECT lease_owner, lease_epoch FROM work_queue_item \
                 WHERE work_id = ?1 AND environment_id = ?2",
                params![wid, env_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(storage)?;
        let epoch = u64::try_from(authority.1).map_err(storage)?;
        Ok(authority.0.as_deref() == Some(worker_id)
            && expected_epoch.is_none_or(|expected| expected == epoch))
    }

    pub(super) fn ack_inner(
        conn: &mut Connection,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(WorkMutationResult::NotFound);
        };
        if !Self::lease_matches(&tx, env_id, wid, worker_id, expected_epoch)? {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let next = ack_next_state(&current);
        tx.execute(
            "UPDATE work_queue_item SET acknowledged_at = ?1, state = ?2 WHERE work_id = ?3",
            params![OBJECT_AT, next, wid],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, wid)?;
        tx.commit().map_err(storage)?;
        Ok(item
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    pub(super) fn heartbeat_inner(
        conn: &mut Connection,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(HeartbeatResult::NotFound);
        };
        let admission = awaken_session_contract::work_queue::work_lease_mutation_admission(
            true,
            Self::lease_matches(&tx, env_id, wid, worker_id, expected_epoch)?,
            heartbeat
                .condition
                .permits(current.latest_heartbeat_at.as_deref()),
        );
        if admission != awaken_session_contract::work_queue::WorkLeaseMutationAdmission::Applied {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        let extended = current.state.can_extend_lease();
        let ttl_seconds = effective_ttl_seconds(heartbeat.desired_ttl_seconds);
        let last_heartbeat = heartbeat_at(now_ms, current.latest_heartbeat_at.as_deref());
        if extended {
            tx.execute(
                "UPDATE work_queue_item \
                 SET latest_heartbeat_at = ?1, lease_expires_ms = ?2, lease_refreshed_ms = ?3 \
                 WHERE work_id = ?4 AND environment_id = ?5 AND state = 'active' \
                   AND lease_owner = ?6",
                params![
                    &last_heartbeat,
                    lease_expiry(now_ms, ttl_seconds),
                    db_millis(now_ms),
                    wid,
                    env_id,
                    worker_id
                ],
            )
            .map_err(storage)?;
        }
        tx.commit().map_err(storage)?;
        Ok(HeartbeatResult::Accepted(LeaseReceipt {
            last_heartbeat,
            lease_extended: extended,
            state: current.state,
            ttl_seconds,
        }))
    }

    pub(super) fn stop_inner(
        conn: &mut Connection,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if Self::owned(&tx, env_id, wid)?.is_none() {
            return Ok(WorkMutationResult::NotFound);
        }
        if !Self::lease_matches(&tx, env_id, wid, worker_id, expected_epoch)? {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        tx.execute(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL, session_token_sha256 = NULL \
             WHERE work_id = ?2",
            params![OBJECT_AT, wid],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, wid)?;
        tx.commit().map_err(storage)?;
        Ok(item
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }
}

impl PostgresWorkQueue {
    pub(super) async fn claim_inner(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        age_ms: Option<u64>,
        mint_session_access: bool,
    ) -> Result<Option<ClaimedWork>, WorkQueueError> {
        self.book.record_poll(env_id, poller_id, now_ms);
        if let Some(cutoff) = reclaim_cutoff(now_ms, age_ms) {
            sqlx::query(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = NULL, lease_expires_ms = NULL, lease_refreshed_ms = NULL, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
                 WHERE environment_id = $1 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= $2",
            )
            .bind(env_id)
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
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
                 lease_refreshed_ms = NULL, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
             WHERE environment_id = $1 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= $2)",
        )
        .bind(env_id)
        .bind(db_millis(now_ms))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
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
        let selected: Option<(String, String, i64)> = sqlx::query_as(
            "SELECT work_id, data_type, lease_epoch FROM work_queue_item \
             WHERE environment_id = $1 AND state = 'queued' ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((wid, data_type, current_epoch)) = selected else {
            return Ok(None);
        };
        let (next_epoch, _) = lease_epoch(current_epoch, true)?;
        let sessions_token =
            (mint_session_access && data_type == "session").then(issue_session_token);
        let token_hash = sessions_token.as_ref().map(session_token_sha256);
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = $1, lease_owner = $2, \
                 lease_epoch = $7, lease_expires_ms = $3, \
                 lease_refreshed_ms = $4, latest_heartbeat_at = NULL, \
                 session_token_sha256 = $5 WHERE work_id = $6",
        )
        .bind(OBJECT_AT)
        .bind(lease_owner)
        .bind(lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS))
        .bind(db_millis(now_ms))
        .bind(token_hash)
        .bind(&wid)
        .bind(next_epoch)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE work_id = $1 AND environment_id = $2"
        ))
        .bind(&wid)
        .bind(env_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let item = pg_row_to_item(&row)?;
        tx.commit().await.map_err(storage)?;
        Ok(Some(ClaimedWork {
            item,
            sessions_token,
        }))
    }

    pub(super) async fn ack_inner(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current: Option<(String, Option<String>, i64)> = sqlx::query_as(
            "SELECT state, lease_owner, lease_epoch FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((state, owner, epoch)) = current else {
            return Ok(WorkMutationResult::NotFound);
        };
        let epoch = u64::try_from(epoch).map_err(storage)?;
        if owner.as_deref() != Some(worker_id)
            || expected_epoch.is_some_and(|expected| expected != epoch)
        {
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

    pub(super) async fn heartbeat_inner(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT {COLS}, lease_owner, lease_epoch FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2 FOR UPDATE"
        ))
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(row) = row else {
            return Ok(HeartbeatResult::NotFound);
        };
        let current = pg_row_to_item(&row)?;
        let owner: Option<String> = row.try_get(11).map_err(storage)?;
        let epoch = u64::try_from(row.try_get::<i64, _>(12).map_err(storage)?).map_err(storage)?;
        let admission = awaken_session_contract::work_queue::work_lease_mutation_admission(
            true,
            owner.as_deref() == Some(worker_id)
                && expected_epoch.is_none_or(|expected| expected == epoch),
            heartbeat
                .condition
                .permits(current.latest_heartbeat_at.as_deref()),
        );
        if admission != awaken_session_contract::work_queue::WorkLeaseMutationAdmission::Applied {
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

    pub(super) async fn stop_inner(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        expected_epoch: Option<u64>,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let authority: Option<(Option<String>, i64)> = sqlx::query_as(
            "SELECT lease_owner, lease_epoch FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((owner, epoch)) = authority else {
            return Ok(WorkMutationResult::NotFound);
        };
        let epoch = u64::try_from(epoch).map_err(storage)?;
        if owner.as_deref() != Some(worker_id)
            || expected_epoch.is_some_and(|expected| expected != epoch)
        {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL, session_token_sha256 = NULL \
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
}

#[cfg(test)]
mod epoch_tests {
    use super::*;

    // E2 queued/MAX -> Storage, no REAL promotion/wrap;
    // E3 MAX-1 -> exact MAX and one token. Negative epochs are forbidden by the
    // real schema and covered as corrupt numeric inputs by mutation's proof.
    // Exercise public claim_with_session_access, not just the numeric helper.
    // Rejected transactions retain queued state, epoch, and absent bearer hash.
    #[tokio::test]
    async fn sqlite_session_claim_fences_epoch_exhaustion() {
        let queue = SqliteWorkQueue::open_in_memory().unwrap();
        for epoch in [i64::MAX, i64::MAX - 1] {
            let env = format!("epoch-{epoch}");
            let work = queue.enqueue_session(&env, "session").await.unwrap();
            let work_for_sql = work.clone();
            queue
                .with_connection(move |conn| {
                    conn.execute(
                        "UPDATE work_queue_item SET lease_epoch = ?1 WHERE work_id = ?2",
                        params![epoch, work_for_sql],
                    )
                    .map_err(storage)?;
                    Ok(())
                })
                .await
                .unwrap();
            let outcome = queue
                .claim_with_session_access(&env, "worker", "poller", 0, None)
                .await;
            if epoch == i64::MAX - 1 {
                assert!(outcome.unwrap().unwrap().sessions_token.is_some(), "E3");
            } else {
                assert!(matches!(outcome, Err(WorkQueueError::Storage(_))), "E1/E2");
            }
            let row: (String, i64, Option<String>) = queue.with_connection(move |conn| {
                conn.query_row("SELECT state, lease_epoch, session_token_sha256 FROM work_queue_item WHERE work_id = ?1",
                    [work], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(storage)
            }).await.unwrap();
            if epoch == i64::MAX - 1 {
                assert_eq!((row.0.as_str(), row.1), ("active", i64::MAX));
                assert!(row.2.is_some());
            } else {
                assert_eq!(row, ("queued".into(), epoch, None));
            }
        }
    }

    #[tokio::test]
    async fn postgres_session_claim_fences_epoch_exhaustion() {
        use sqlx::Executor;
        let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
            return;
        };
        let admin = PgPool::connect(&url)
            .await
            .expect("explicit database must be reachable");
        admin.execute("DROP SCHEMA IF EXISTS t_work_epoch_admission CASCADE; CREATE SCHEMA t_work_epoch_admission").await.unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET search_path = t_work_epoch_admission")
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let queue = PostgresWorkQueue::with_pool(pool.clone()).await.unwrap();
        for epoch in [i64::MAX, i64::MAX - 1] {
            let env = format!("epoch-{epoch}");
            let work = queue.enqueue_session(&env, "session").await.unwrap();
            sqlx::query("UPDATE work_queue_item SET lease_epoch = $1 WHERE work_id = $2")
                .bind(epoch)
                .bind(&work)
                .execute(&pool)
                .await
                .unwrap();
            let outcome = queue
                .claim_with_session_access(&env, "worker", "poller", 0, None)
                .await;
            if epoch == i64::MAX - 1 {
                assert!(outcome.unwrap().unwrap().sessions_token.is_some(), "E3");
            } else {
                assert!(matches!(outcome, Err(WorkQueueError::Storage(_))), "E1/E2");
            }
            let row: (String, i64, Option<String>) = sqlx::query_as("SELECT state, lease_epoch, session_token_sha256 FROM work_queue_item WHERE work_id = $1").bind(&work).fetch_one(&pool).await.unwrap();
            if epoch == i64::MAX - 1 {
                assert_eq!((row.0.as_str(), row.1), ("active", i64::MAX));
                assert!(row.2.is_some());
            } else {
                assert_eq!(row, ("queued".into(), epoch, None));
            }
        }
    }
}
