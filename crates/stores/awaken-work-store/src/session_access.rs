// @formal entrypoint work.exact_boundary_reclaim
// @formal entrypoint work.heartbeat_compare_and_set
// @formal entrypoint work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal entrypoint work.persisted_epoch_is_exact_nonnegative_and_nonwrapping
// @formal entrypoint work.single_active_claim
// @formal entrypoint work.terminal_lease_clear
//! Session-access digest binding and epoch-fenced lease mutations.
//!
//! The bearer is minted by the issuing application; this store only persists
//! the pre-minted digest in the same statement that writes the lease epoch.

use super::*;
use awaken_session_work_contract::SessionAccessDigest;
use awaken_time::WallClockExt;

// SQL adapters share these exact column shapes. Keeping the aliases beside the
// conversion boundary prevents SQLite and PostgreSQL from independently
// restating lease identity/order. There is no runtime decision to unit-test;
// the existing cross-adapter claim/release decision tables exercise every
// field, while Clippy/type checking verifies both queries retain this shape.
type StoredClaimCandidate = (
    String,
    String,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
);
type StoredLeaseAuthority = (Option<String>, i64, Option<i64>, Option<i64>);

impl SqliteWorkQueue {
    pub(super) fn claim_inner(
        conn: &mut Connection,
        env_id: &str,
        lease_owner: &str,
        now_ms: u64,
        age_ms: Option<u64>,
        session_access: Option<SessionAccessDigest>,
    ) -> Result<Option<WorkClaim>, WorkQueueError> {
        // `started_at` is the very reading the lease arithmetic below uses, so a
        // claim's durable start and its lease window cannot disagree.
        let started_at = awaken_time::utc::epoch_millis_to_rfc3339(now_ms);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let cleared = reclaimed_lease_authority();
        if let Some(cutoff) = reclaim_cutoff(now_ms, age_ms) {
            tx.execute(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = ?1, lease_expires_ms = ?2, lease_refreshed_ms = ?3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
                 WHERE environment_id = ?4 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= ?5",
                params![cleared.owner.as_deref(), cleared.expires_at, cleared.refreshed_at, env_id, cutoff],
            )
            .map_err(storage)?;
        }
        Self::reclaim_lapsed(&tx, env_id, now_ms)?;
        let active: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
                params![env_id],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if active > 0 {
            tx.commit().map_err(storage)?;
            return Ok(None);
        }
        let selected: Option<StoredClaimCandidate> = tx
            .query_row(
                "SELECT work_id, data_type, lease_owner, lease_epoch, lease_refreshed_ms, lease_expires_ms FROM work_queue_item \
                 WHERE environment_id = ?1 AND state = 'queued' ORDER BY seq ASC LIMIT 1",
                params![env_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()
            .map_err(storage)?;
        let Some((wid, data_type, owner, current_epoch, refreshed, expires)) = selected else {
            tx.commit().map_err(storage)?;
            return Ok(None);
        };
        let LeaseAuthorityDecision::Apply(next_authority) = lease_authority_transition(
            stored_lease_authority(owner, current_epoch, refreshed, expires)?,
            LeaseAuthorityCommand::Claim {
                owner: lease_owner.to_string(),
                refreshed_at: db_millis(now_ms),
                expires_at: lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
            },
        ) else {
            return Err(WorkQueueError::Storage("work lease epoch exhausted".into()));
        };
        let next_epoch = stored_lease_epoch(next_authority.granted_epoch()?)?;
        let session_access = session_access.filter(|_| data_type == "session");
        let token_hash = session_access.as_ref().map(SessionAccessDigest::as_str);
        tx.execute(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = ?1, lease_owner = ?2, \
                 lease_epoch = ?7, lease_expires_ms = ?3, \
                 lease_refreshed_ms = ?4, latest_heartbeat_at = NULL, \
                 session_token_sha256 = ?5 WHERE work_id = ?6",
            params![
                started_at,
                next_authority.owner.as_deref(),
                next_authority.expires_at,
                next_authority.refreshed_at,
                token_hash,
                wid,
                next_epoch,
            ],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, &wid)?;
        tx.commit().map_err(storage)?;
        Ok(item.map(|item| WorkClaim {
            item,
            session_access,
        }))
    }

    fn lease_matches(
        tx: &Transaction<'_>,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        fence: LeaseFence,
    ) -> Result<bool, WorkQueueError> {
        let authority = Self::lease_authority(tx, env_id, wid)?;
        Ok(authority.owner.as_deref() == Some(worker_id) && fence.admits(authority.epoch))
    }

    fn lease_authority(
        tx: &Transaction<'_>,
        env_id: &str,
        wid: &str,
    ) -> Result<LeaseAuthorityTuple<String, i64>, WorkQueueError> {
        let authority: StoredLeaseAuthority = tx
            .query_row(
                "SELECT lease_owner, lease_epoch, lease_refreshed_ms, lease_expires_ms FROM work_queue_item \
                 WHERE work_id = ?1 AND environment_id = ?2",
                params![wid, env_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(storage)?;
        stored_lease_authority(authority.0, authority.1, authority.2, authority.3)
    }

    pub(super) fn ack_inner(
        conn: &mut Connection,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        fence: LeaseFence,
        acknowledged_at: String,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(WorkMutationResult::NotFound);
        };
        if !Self::lease_matches(&tx, env_id, wid, worker_id, fence)? {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let next = ack_next_state(&current);
        tx.execute(
            "UPDATE work_queue_item SET acknowledged_at = ?1, state = ?2 WHERE work_id = ?3",
            params![acknowledged_at, next, wid],
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
        fence: LeaseFence,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let Some(current) = Self::owned(&tx, env_id, wid)? else {
            return Ok(HeartbeatResult::NotFound);
        };
        let admission = awaken_session_work_contract::work_lease_mutation_admission(
            true,
            Self::lease_matches(&tx, env_id, wid, worker_id, fence)?,
            heartbeat
                .condition
                .permits(current.latest_heartbeat_at.as_deref()),
        );
        if admission != awaken_session_work_contract::WorkLeaseMutationAdmission::Applied {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        let extended = current.state.can_extend_lease();
        let ttl_seconds = effective_ttl_seconds(heartbeat.desired_ttl_seconds);
        let last_heartbeat = heartbeat_at(now_ms, current.latest_heartbeat_at.as_deref());
        if extended {
            let LeaseAuthorityDecision::Apply(next_authority) = lease_authority_transition(
                Self::lease_authority(&tx, env_id, wid)?,
                LeaseAuthorityCommand::Renew {
                    owner: worker_id.to_string(),
                    fence,
                    refreshed_at: db_millis(now_ms),
                    expires_at: lease_expiry(now_ms, ttl_seconds),
                },
            ) else {
                return Ok(HeartbeatResult::PreconditionFailed);
            };
            tx.execute(
                "UPDATE work_queue_item \
                 SET latest_heartbeat_at = ?1, lease_expires_ms = ?2, lease_refreshed_ms = ?3 \
                 WHERE work_id = ?4 AND environment_id = ?5 AND state = 'active' \
                   AND lease_owner = ?6",
                params![
                    &last_heartbeat,
                    next_authority.expires_at,
                    next_authority.refreshed_at,
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
        fence: LeaseFence,
        stopped_at: String,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if Self::owned(&tx, env_id, wid)?.is_none() {
            return Ok(WorkMutationResult::NotFound);
        }
        let authority = Self::lease_authority(&tx, env_id, wid)?;
        if authority.owner.as_deref() != Some(worker_id) || !fence.admits(authority.epoch) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let LeaseAuthorityDecision::Apply(next_authority) = lease_authority_transition(
            authority,
            LeaseAuthorityCommand::Release {
                owner: worker_id.to_string(),
                fence,
            },
        ) else {
            return Ok(WorkMutationResult::PreconditionFailed);
        };
        tx.execute(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = ?2, lease_expires_ms = ?3, \
             lease_refreshed_ms = ?4, session_token_sha256 = NULL \
             WHERE work_id = ?5",
            params![
                stopped_at,
                next_authority.owner,
                next_authority.expires_at,
                next_authority.refreshed_at,
                wid
            ],
        )
        .map_err(storage)?;
        let item = Self::owned(&tx, env_id, wid)?;
        tx.commit().map_err(storage)?;
        Ok(item
            .map(WorkMutationResult::accepted)
            .unwrap_or(WorkMutationResult::NotFound))
    }

    pub(super) async fn acquire_session_inner(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let env_id = env_id.to_string();
        let session_id = session_id.to_string();
        let worker_owner = worker_owner.to_string();
        let created_at = self.clock.observed_at();
        let started_at = awaken_time::utc::epoch_millis_to_rfc3339(now_ms);
        self.with_connection(move |conn| {
            let work_id = Self::insert(conn, &env_id, "session", Some(&session_id), created_at)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            Self::reclaim_lapsed(&tx, &env_id, now_ms)?;
            let current: (String, Option<String>, i64, Option<i64>, Option<i64>) = tx
                .query_row(
                    "SELECT state, lease_owner, lease_epoch, lease_expires_ms, lease_refreshed_ms FROM work_queue_item \
                     WHERE work_id = ?1 AND environment_id = ?2",
                    params![work_id, env_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .map_err(storage)?;
            if current.0 == "active" && current.1.as_deref() != Some(worker_owner.as_str()) {
                let owner = current.1.clone().ok_or_else(|| {
                    WorkQueueError::Storage("active Session Work has no lease owner".into())
                })?;
                let epoch = stored_epoch(current.2)?.ok_or_else(|| {
                    WorkQueueError::Storage("active Session Work has no lease epoch".into())
                })?;
                let expires_at_unix_ms = u64::try_from(current.3.unwrap_or_default())
                    .map_err(|_| WorkQueueError::Storage("negative Session Work expiry".into()))?;
                return Ok(Some(SessionWorkLease::granted(
                    work_id, env_id, awaken_session_types::SessionId::from(&session_id), owner, epoch,
                    Expiry::from_epoch_ms(expires_at_unix_ms),
                )));
            }
            let epoch = if current.0 == "active"
                && current.1.as_deref() == Some(worker_owner.as_str())
            {
                let Some(next) = renew_stored_lease_authority(
                    StoredLeaseAuthorityInput { owner: current.1.clone(), epoch: current.2, refreshed_at: current.4, expires_at: current.3 },
                    worker_owner.clone(), LeaseFence::OwnerOnly, db_millis(now_ms), lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
                )? else {
                    return Err(WorkQueueError::Storage("active Session Work has incomplete lease authority".into()));
                };
                tx.execute(
                    "UPDATE work_queue_item SET lease_expires_ms = ?1, lease_refreshed_ms = ?2 \
                     WHERE work_id = ?3 AND environment_id = ?4 AND lease_owner = ?5",
                    params![next.expires_at, next.refreshed_at, work_id, env_id, worker_owner],
                ).map_err(storage)?;
                next.granted_epoch()?
            } else {
                let active: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND state = 'active'",
                    params![env_id], |row| row.get(0),
                ).map_err(storage)?;
                if active > 0 || current.0 != "queued" { return Ok(None); }
                let next = claim_stored_lease_authority(
                    current.1.clone(), current.2, current.4, current.3, worker_owner.clone(),
                    db_millis(now_ms), lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
                )?;
                let epoch_db = stored_lease_epoch(next.granted_epoch()?)?;
                tx.execute(
                    "UPDATE work_queue_item SET state = 'active', started_at = ?1, \
                     lease_owner = ?2, lease_epoch = ?3, lease_expires_ms = ?4, \
                     lease_refreshed_ms = ?5, latest_heartbeat_at = NULL WHERE work_id = ?6",
                    params![started_at, next.owner, epoch_db, next.expires_at, next.refreshed_at, work_id],
                ).map_err(storage)?;
                next.granted_epoch()?
            };
            tx.commit().map_err(storage)?;
            Ok(Some(SessionWorkLease::granted(
                work_id, env_id, awaken_session_types::SessionId::from(&session_id), worker_owner, epoch,
                Expiry::from_epoch_ms(now_ms.saturating_add(HEARTBEAT_TTL_SECONDS * 1_000)),
            )))
        }).await
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
        session_access: Option<SessionAccessDigest>,
    ) -> Result<Option<WorkClaim>, WorkQueueError> {
        self.book.record_poll(env_id, poller_id, now_ms);
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let cleared = reclaimed_lease_authority();
        let _: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(cutoff) = reclaim_cutoff(now_ms, age_ms) {
            sqlx::query(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = $1, lease_expires_ms = $2, lease_refreshed_ms = $3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
                 WHERE environment_id = $4 AND state = 'active' AND lease_refreshed_ms IS NOT NULL AND lease_refreshed_ms <= $5",
            )
            .bind(cleared.owner.as_deref())
            .bind(cleared.expires_at)
            .bind(cleared.refreshed_at)
            .bind(env_id)
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'queued', lease_owner = $1, lease_expires_ms = $2, \
                 lease_refreshed_ms = $3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
             WHERE environment_id = $4 AND state = 'active' \
               AND (lease_expires_ms IS NULL OR lease_expires_ms <= $5)",
        )
        .bind(cleared.owner.as_deref())
        .bind(cleared.expires_at)
        .bind(cleared.refreshed_at)
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
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        }
        let selected: Option<StoredClaimCandidate> = sqlx::query_as(
            "SELECT work_id, data_type, lease_owner, lease_epoch, lease_refreshed_ms, lease_expires_ms FROM work_queue_item \
             WHERE environment_id = $1 AND state = 'queued' ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((wid, data_type, owner, current_epoch, refreshed, expires)) = selected else {
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        };
        let LeaseAuthorityDecision::Apply(next_authority) = lease_authority_transition(
            stored_lease_authority(owner, current_epoch, refreshed, expires)?,
            LeaseAuthorityCommand::Claim {
                owner: lease_owner.to_string(),
                refreshed_at: db_millis(now_ms),
                expires_at: lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
            },
        ) else {
            return Err(WorkQueueError::Storage("work lease epoch exhausted".into()));
        };
        let next_epoch = stored_lease_epoch(next_authority.granted_epoch()?)?;
        let session_access = session_access.filter(|_| data_type == "session");
        let token_hash = session_access.as_ref().map(SessionAccessDigest::as_str);
        sqlx::query(
            "UPDATE work_queue_item \
             SET state = 'active', started_at = $1, lease_owner = $2, \
                 lease_epoch = $7, lease_expires_ms = $3, \
                 lease_refreshed_ms = $4, latest_heartbeat_at = NULL, \
                 session_token_sha256 = $5 WHERE work_id = $6",
        )
        .bind(awaken_time::utc::epoch_millis_to_rfc3339(now_ms))
        .bind(next_authority.owner)
        .bind(next_authority.expires_at)
        .bind(next_authority.refreshed_at)
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
        Ok(Some(WorkClaim {
            item,
            session_access,
        }))
    }

    pub(super) async fn ack_inner(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        fence: LeaseFence,
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
        let epoch = stored_epoch(epoch)?;
        if owner.as_deref() != Some(worker_id) || !fence.admits(epoch) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let next = awaken_session_work_contract::WorkState::from_wire(&state)
            .unwrap_or(awaken_session_work_contract::WorkState::Stopped)
            .after_ack()
            .as_str();
        sqlx::query(
            "UPDATE work_queue_item SET acknowledged_at = $1, state = $2 \
             WHERE work_id = $3 AND environment_id = $4",
        )
        .bind(self.clock.observed_at())
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
        fence: LeaseFence,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT {COLS}, lease_owner, lease_epoch, lease_refreshed_ms, lease_expires_ms FROM work_queue_item \
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
        // By name, not by position: the projection's column list grows, and an
        // ordinal that silently shifts decodes the wrong column.
        let owner: Option<String> = row.try_get("lease_owner").map_err(storage)?;
        let epoch = stored_epoch(row.try_get::<i64, _>("lease_epoch").map_err(storage)?)?;
        let refreshed: Option<i64> = row.try_get("lease_refreshed_ms").map_err(storage)?;
        let expires: Option<i64> = row.try_get("lease_expires_ms").map_err(storage)?;
        let admission = awaken_session_work_contract::work_lease_mutation_admission(
            true,
            owner.as_deref() == Some(worker_id) && fence.admits(epoch),
            heartbeat
                .condition
                .permits(current.latest_heartbeat_at.as_deref()),
        );
        if admission != awaken_session_work_contract::WorkLeaseMutationAdmission::Applied {
            return Ok(HeartbeatResult::PreconditionFailed);
        }
        let extended = current.state.can_extend_lease();
        let ttl_seconds = effective_ttl_seconds(heartbeat.desired_ttl_seconds);
        let last_heartbeat = heartbeat_at(now_ms, current.latest_heartbeat_at.as_deref());
        if extended {
            let LeaseAuthorityDecision::Apply(next_authority) = lease_authority_transition(
                LeaseAuthorityTuple {
                    owner,
                    epoch,
                    refreshed_at: refreshed,
                    expires_at: expires,
                },
                LeaseAuthorityCommand::Renew {
                    owner: worker_id.to_string(),
                    fence,
                    refreshed_at: db_millis(now_ms),
                    expires_at: lease_expiry(now_ms, ttl_seconds),
                },
            ) else {
                return Ok(HeartbeatResult::PreconditionFailed);
            };
            sqlx::query(
                "UPDATE work_queue_item SET latest_heartbeat_at = $1, lease_expires_ms = $2, lease_refreshed_ms = $3 \
                 WHERE work_id = $4 AND environment_id = $5 AND state = 'active' \
                   AND lease_owner = $6",
            )
            .bind(&last_heartbeat)
            .bind(next_authority.expires_at)
            .bind(next_authority.refreshed_at)
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
        fence: LeaseFence,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let authority: Option<StoredLeaseAuthority> = sqlx::query_as(
            "SELECT lease_owner, lease_epoch, lease_refreshed_ms, lease_expires_ms FROM work_queue_item WHERE work_id = $1 \
             AND environment_id = $2 FOR UPDATE",
        )
        .bind(wid)
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some((owner, epoch, refreshed, expires)) = authority else {
            return Ok(WorkMutationResult::NotFound);
        };
        let epoch = stored_epoch(epoch)?;
        if owner.as_deref() != Some(worker_id) || !fence.admits(epoch) {
            return Ok(WorkMutationResult::PreconditionFailed);
        }
        let LeaseAuthorityDecision::Apply(next_authority) = lease_authority_transition(
            LeaseAuthorityTuple {
                owner,
                epoch,
                refreshed_at: refreshed,
                expires_at: expires,
            },
            LeaseAuthorityCommand::Release {
                owner: worker_id.to_string(),
                fence,
            },
        ) else {
            return Ok(WorkMutationResult::PreconditionFailed);
        };
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = $2, lease_expires_ms = $3, \
             lease_refreshed_ms = $4, session_token_sha256 = NULL \
             WHERE work_id = $5 AND environment_id = $6",
        )
        .bind(self.clock.observed_at())
        .bind(next_authority.owner)
        .bind(next_authority.expires_at)
        .bind(next_authority.refreshed_at)
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

// Build-selection decision table: C1 ordinary test build, C2 Loom mutex
// substitution. E1 C1+!C2 runs the real SQLite epoch boundary cases below;
// E2 C2 reserves the dedicated `loom_tests` entrypoint for concurrency proof.
// This matches the crate's other ordinary fixtures in lib.rs and inmem.rs.
#[cfg(all(test, not(feature = "loom")))]
mod epoch_tests {
    use super::*;

    // E2 queued/MAX -> Storage, no REAL promotion/wrap;
    // E3 MAX-1 -> exact MAX and one token. Negative epochs are forbidden by the
    // real schema and covered as corrupt numeric inputs by mutation's proof.
    // Exercise public claim_with_session_access, not just the numeric helper.
    // Rejected transactions retain queued state, epoch, and absent bearer hash.
    #[tokio::test]
    async fn sqlite_session_claim_fences_epoch_exhaustion() {
        let queue = SqliteWorkQueue::open_in_memory(system_clock()).unwrap();
        for epoch in [i64::MAX, i64::MAX - 1] {
            let env = format!("epoch-{epoch}");
            let work = queue
                .enqueue_session(&env, &awaken_session_types::SessionId::from("session"))
                .await
                .unwrap();
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
                .claim_with_session_access(
                    &env,
                    "worker",
                    "poller",
                    0,
                    None,
                    &test_session_access_digest(1),
                )
                .await;
            if epoch == i64::MAX - 1 {
                assert!(outcome.unwrap().unwrap().session_access.is_some(), "E3");
            } else {
                assert!(matches!(outcome, Err(WorkQueueError::Storage(_))), "E1/E2");
            }
            let row: (String, i64, Option<String>) = queue.with_connection(move |conn| {
                conn.query_row("SELECT state, lease_epoch, session_token_sha256 FROM work_queue_item WHERE work_id = ?1",
                    [work], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(storage)
            }).await.unwrap();
            if epoch == i64::MAX - 1 {
                assert_eq!((row.0.as_str(), row.1), ("active", i64::MAX));
                assert_eq!(
                    row.2.as_deref(),
                    Some(test_session_access_digest(1).as_str()),
                    "E3 exact pre-minted digest bound with the lease"
                );
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
        let queue = PostgresWorkQueue::with_pool(pool.clone(), system_clock())
            .await
            .unwrap();
        for epoch in [i64::MAX, i64::MAX - 1] {
            let env = format!("epoch-{epoch}");
            let work = queue
                .enqueue_session(&env, &awaken_session_types::SessionId::from("session"))
                .await
                .unwrap();
            sqlx::query("UPDATE work_queue_item SET lease_epoch = $1 WHERE work_id = $2")
                .bind(epoch)
                .bind(&work)
                .execute(&pool)
                .await
                .unwrap();
            let outcome = queue
                .claim_with_session_access(
                    &env,
                    "worker",
                    "poller",
                    0,
                    None,
                    &test_session_access_digest(2),
                )
                .await;
            if epoch == i64::MAX - 1 {
                assert!(outcome.unwrap().unwrap().session_access.is_some(), "E3");
            } else {
                assert!(matches!(outcome, Err(WorkQueueError::Storage(_))), "E1/E2");
            }
            let row: (String, i64, Option<String>) = sqlx::query_as("SELECT state, lease_epoch, session_token_sha256 FROM work_queue_item WHERE work_id = $1").bind(&work).fetch_one(&pool).await.unwrap();
            if epoch == i64::MAX - 1 {
                assert_eq!((row.0.as_str(), row.1), ("active", i64::MAX));
                assert_eq!(
                    row.2.as_deref(),
                    Some(test_session_access_digest(2).as_str()),
                    "E3 exact pre-minted digest bound with the lease"
                );
            } else {
                assert_eq!(row, ("queued".into(), epoch, None));
            }
        }
    }
}
