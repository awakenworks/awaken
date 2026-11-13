// @formal surface obligation=session_work.release_requires_exact_owner_and_epoch
// @formal reason: Existing Work lease SQL moved to caller-borrowed connection primitives;
// @formal reason: both standalone and composed paths still consume the one Work/lease kernels.
//! Session Work SQL on a caller-owned transaction. No pool checkout or close occurs here.
use super::{
    HEARTBEAT_TTL_SECONDS, LeaseFence, SessionWorkLease, SessionWorkReleaseAdmission,
    StoredLeaseAuthorityInput, StoredSessionWorkRow, WallClock, WorkQueueError,
    claim_stored_lease_authority, db_millis, granted_epoch, lease_expiry,
    reclaimed_lease_authority, release_stored_lease_authority, renew_stored_lease_authority,
    session_work_release_admission, storage, stored_lease_epoch,
};
use awaken_time::WallClockExt as _;

/// Preserve the original insertion and lease kernels under the caller's one
/// transaction. `completed=false` is the original early-return rollback path.
pub async fn acquire_postgres_session_work_on(
    connection: &mut sqlx::PgConnection,
    clock: &dyn WallClock,
    env_id: &str,
    session_id: &awaken_session_types::SessionId,
    worker_owner: &str,
) -> Result<(Option<SessionWorkLease>, bool), WorkQueueError> {
    let work_id = super::backends::insert_postgres_work_on(
        connection,
        clock,
        env_id,
        "session",
        Some(session_id.as_str()),
    )
    .await?;
    acquire_existing_on(
        connection,
        clock,
        work_id,
        env_id,
        session_id,
        worker_owner,
        true,
    )
    .await
}

/// Renew an existing live Session Work lease using the caller's transaction.
pub async fn renew_postgres_session_work_on(
    connection: &mut sqlx::PgConnection,
    clock: &dyn WallClock,
    env_id: &str,
    session_id: &awaken_session_types::SessionId,
    worker_owner: &str,
) -> Result<Option<SessionWorkLease>, WorkQueueError> {
    super::backends::lock_postgres_environment(connection, env_id).await?;
    let work_id: Option<String> = sqlx::query_scalar(
        "SELECT work_id FROM work_queue_item WHERE environment_id = $1 AND data_type = 'session' \
         AND data_id = $2 ORDER BY seq ASC LIMIT 1",
    )
    .bind(env_id)
    .bind(session_id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(storage)?;
    let Some(work_id) = work_id else {
        return Ok(None);
    };
    let (lease, _) = acquire_existing_on(
        connection,
        clock,
        work_id,
        env_id,
        session_id,
        worker_owner,
        false,
    )
    .await?;
    Ok(lease)
}

async fn acquire_existing_on(
    connection: &mut sqlx::PgConnection,
    clock: &dyn WallClock,
    work_id: String,
    env_id: &str,
    session_id: &awaken_session_types::SessionId,
    worker_owner: &str,
    may_claim: bool,
) -> Result<(Option<SessionWorkLease>, bool), WorkQueueError> {
    let _: Vec<String> = sqlx::query_scalar(
        "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
    )
    .bind(env_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(storage)?;
    // The one reading this grant is minted and judged on, sampled under the
    // Environment lock so no later writer can commit an earlier deadline.
    let now_ms = clock.now_ms();
    if may_claim {
        let cleared = reclaimed_lease_authority();
        sqlx::query(
                "UPDATE work_queue_item SET state = 'queued', lease_owner = $1, \
                 lease_expires_ms = $2, lease_refreshed_ms = $3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
                 WHERE environment_id = $4 AND state = 'active' \
                 AND (lease_expires_ms IS NULL OR lease_expires_ms <= $5)",
            )
            .bind(cleared.owner)
            .bind(cleared.expires_at)
            .bind(cleared.refreshed_at)
            .bind(env_id)
            .bind(db_millis(now_ms))
            .execute(&mut *connection)
            .await
            .map_err(storage)?;
    }
    let current: (String, Option<String>, i64, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT state, lease_owner, lease_epoch, lease_expires_ms, lease_refreshed_ms FROM work_queue_item \
             WHERE work_id = $1 AND environment_id = $2",
        )
        .bind(&work_id)
        .bind(env_id)
        .fetch_one(&mut *connection)
        .await
        .map_err(storage)?;
    if !may_claim
        && (current.0 != "active" || current.3.is_none_or(|expiry| expiry <= db_millis(now_ms)))
    {
        return Ok((None, false));
    }
    if current.0 == "active" && current.1.as_deref() != Some(worker_owner) {
        let owner = current.1.clone().ok_or_else(|| {
            WorkQueueError::Storage("active Session Work has no lease owner".into())
        })?;
        let epoch = granted_epoch(current.2)?;
        let expires_at_unix_ms = u64::try_from(current.3.unwrap_or_default())
            .map_err(|_| WorkQueueError::Storage("negative Session Work expiry".into()))?;
        return Ok((
            Some(
                SessionWorkLease::from_persisted(
                    work_id,
                    env_id,
                    session_id.clone(),
                    owner,
                    (epoch).get(),
                    expires_at_unix_ms,
                )
                .expect("canonical grant has a nonzero epoch"),
            ),
            false,
        ));
    }
    let epoch = if current.0 == "active" && current.1.as_deref() == Some(worker_owner) {
        let Some(next_authority) = renew_stored_lease_authority(
            StoredLeaseAuthorityInput {
                owner: current.1.clone(),
                epoch: current.2,
                refreshed_at: current.4,
                expires_at: current.3,
            },
            worker_owner.to_string(),
            // The registered-Worker owner is incarnation-qualified and
            // this acquisition carries no generation to compare.
            LeaseFence::OwnerOnly,
            db_millis(now_ms),
            lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
        )?
        else {
            return Err(WorkQueueError::Storage(
                "active Session Work has incomplete lease authority".into(),
            ));
        };
        sqlx::query(
            "UPDATE work_queue_item SET lease_expires_ms = $1, lease_refreshed_ms = $2 \
                 WHERE work_id = $3 AND environment_id = $4 AND lease_owner = $5",
        )
        .bind(next_authority.expires_at)
        .bind(next_authority.refreshed_at)
        .bind(&work_id)
        .bind(env_id)
        .bind(worker_owner)
        .execute(&mut *connection)
        .await
        .map_err(storage)?;
        next_authority.granted_epoch()?
    } else {
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = $1 AND state = 'active'",
        )
        .bind(env_id)
        .fetch_one(&mut *connection)
        .await
        .map_err(storage)?;
        if active > 0 || current.0 != "queued" {
            return Ok((None, false));
        }
        let next_authority = claim_stored_lease_authority(
            current.1.clone(),
            current.2,
            current.4,
            current.3,
            worker_owner.to_string(),
            db_millis(now_ms),
            lease_expiry(now_ms, HEARTBEAT_TTL_SECONDS),
        )?;
        let granted = next_authority.granted_epoch()?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'active', started_at = $1, \
                 lease_owner = $2, lease_epoch = $3, lease_expires_ms = $4, \
                 lease_refreshed_ms = $5, latest_heartbeat_at = NULL WHERE work_id = $6",
        )
        .bind(awaken_time::utc::epoch_millis_to_rfc3339(now_ms))
        .bind(next_authority.owner)
        .bind(stored_lease_epoch(granted)?)
        .bind(next_authority.expires_at)
        .bind(next_authority.refreshed_at)
        .bind(&work_id)
        .execute(&mut *connection)
        .await
        .map_err(storage)?;
        granted
    };
    Ok((
        Some(
            SessionWorkLease::from_persisted(
                work_id,
                env_id,
                session_id.clone(),
                worker_owner,
                (epoch).get(),
                now_ms.saturating_add(HEARTBEAT_TTL_SECONDS * 1_000),
            )
            .expect("canonical grant has a nonzero epoch"),
        ),
        true,
    ))
}

/// Release through the exact original Work owner/epoch predicate.
pub async fn release_postgres_session_work_on(
    connection: &mut sqlx::PgConnection,
    clock: &dyn WallClock,
    lease: &SessionWorkLease,
) -> Result<bool, WorkQueueError> {
    let epoch = i64::try_from(lease.epoch()).map_err(storage)?;
    let current: Option<StoredSessionWorkRow> = sqlx::query_as(
            "SELECT data_type, data_id, state, lease_owner, lease_epoch, lease_expires_ms, lease_refreshed_ms \
             FROM work_queue_item WHERE work_id = $1 AND environment_id = $2 FOR UPDATE",
        )
        .bind(lease.work_id())
        .bind(lease.environment_id())
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage)?;
    let Some((data_type, data_id, state, owner, current_epoch, expires, refreshed)) = current
    else {
        return Ok(false);
    };
    let admission = session_work_release_admission(
        data_type == "session" && data_id == lease.session_id(),
        state == "active",
        state == "stopped",
        owner.as_deref() == Some(lease.owner()),
        current_epoch == epoch,
        owner.is_none() && expires.is_none() && refreshed.is_none(),
    );
    match admission {
        SessionWorkReleaseAdmission::Reject => return Ok(false),
        SessionWorkReleaseAdmission::AlreadyReleased => return Ok(true),
        SessionWorkReleaseAdmission::Apply => {}
    }
    let Some(next_authority) = release_stored_lease_authority(
        StoredLeaseAuthorityInput {
            owner,
            epoch: current_epoch,
            refreshed_at: refreshed,
            expires_at: expires,
        },
        lease.owner().to_owned(),
        LeaseFence::Exact(
            awaken_lease_contract::fence::Epoch::from_persisted(lease.epoch())
                .expect("transfer epoch is nonzero"),
        ),
    )?
    else {
        return Ok(false);
    };
    let changed = sqlx::query(
        "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = $2, lease_expires_ms = $3, \
             lease_refreshed_ms = $4, session_token_sha256 = NULL \
             WHERE work_id = $5 AND environment_id = $6",
    )
    .bind(clock.observed_at())
    .bind(next_authority.owner)
    .bind(next_authority.expires_at)
    .bind(next_authority.refreshed_at)
    .bind(lease.work_id())
    .bind(lease.environment_id())
    .execute(&mut *connection)
    .await
    .map_err(storage)?
    .rows_affected();
    Ok(changed == 1)
}
