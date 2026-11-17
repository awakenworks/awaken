// @formal surface obligation=session_work.release_requires_exact_owner_and_epoch
// @formal reason: A Run-free preparation's hand-back is the exact owner/epoch release
// @formal reason: narrowed further by the preparation's recorded hold, on the same row lock.
//! The Session Work a Run-free preparation's claim holds (ADR-0106).
//!
//! A preparation claims the Session's Work like a Run, and the granted epoch is
//! recorded as the preparation's hold. Any later Run acquisition by the same
//! owner renews that one lease and clears the hold in the same statement. The
//! hand-back reads the hold under the lock every acquisition takes and releases
//! only when the hold still equals the current lease epoch, so Work a Run took
//! between the Session root's decision and the hand-back is never released.

use awaken_executable_environment_contract::preparation_work_release_admission;
use awaken_session_types::SessionId;
use awaken_time::WallClock;
use awaken_time::WallClockExt as _;

use super::{
    LeaseFence, SqliteWorkQueue, StoredLeaseAuthorityInput, WorkQueueError, backends,
    release_stored_lease_authority, storage,
};

/// Which acquisition claims Session Work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionWorkClaimant {
    Run,
    Preparation,
}

impl SessionWorkClaimant {
    /// A Run acquiring Work (fresh or as a renewal) ends any preparation hold;
    /// a preparation renewing its own lease, or a plain renewal, keeps it.
    pub(crate) fn clears_preparation_hold(claimant: Option<Self>) -> bool {
        claimant == Some(Self::Run)
    }

    /// The hold a fresh claim records: the granted epoch for a preparation,
    /// none for a Run.
    pub(crate) fn held_epoch(claimant: Option<Self>, granted: i64) -> Option<i64> {
        (claimant == Some(Self::Preparation)).then_some(granted)
    }
}

type HeldRow = (
    String,
    String,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// The release columns for an admitted hand-back, or `None` to keep the Work.
fn admitted_release(
    row: &HeldRow,
    worker_owner: &str,
) -> Result<Option<super::LeaseAuthorityTuple<String, i64>>, WorkQueueError> {
    let (_, state, owner, epoch, expires, refreshed, held) = row;
    if !preparation_work_release_admission(
        true,
        state == "active",
        owner.as_deref() == Some(worker_owner),
        *held == Some(*epoch),
    ) {
        return Ok(None);
    }
    let Some(granted) = u64::try_from(*epoch)
        .ok()
        .and_then(awaken_lease_contract::fence::Epoch::from_persisted)
    else {
        return Ok(None);
    };
    release_stored_lease_authority(
        StoredLeaseAuthorityInput {
            owner: owner.clone(),
            epoch: *epoch,
            refreshed_at: *refreshed,
            expires_at: *expires,
        },
        worker_owner.to_owned(),
        LeaseFence::Exact(granted),
    )
}

const SELECT_HELD_SQLITE: &str = "SELECT work_id, state, lease_owner, lease_epoch, \
     lease_expires_ms, lease_refreshed_ms, preparation_lease_epoch FROM work_queue_item \
     WHERE environment_id = ?1 AND data_type = 'session' AND data_id = ?2 \
     ORDER BY seq ASC LIMIT 1";

const SELECT_HELD_POSTGRES: &str = "SELECT work_id, state, lease_owner, lease_epoch, \
     lease_expires_ms, lease_refreshed_ms, preparation_lease_epoch FROM work_queue_item \
     WHERE environment_id = $1 AND data_type = 'session' AND data_id = $2 \
     ORDER BY seq ASC LIMIT 1 FOR UPDATE";

impl SqliteWorkQueue {
    pub(super) async fn release_preparation_session_inner(
        &self,
        env_id: &str,
        session_id: &SessionId,
        worker_owner: &str,
    ) -> Result<bool, WorkQueueError> {
        let stopped_at = self.clock.observed_at();
        let mut conn = self.connection().await?;
        let mut tx = backends::immediate(&mut conn).await?;
        let row: Option<HeldRow> = sqlx::query_as(SELECT_HELD_SQLITE)
            .bind(env_id)
            .bind(session_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        let Some(row) = row else {
            return Ok(false);
        };
        let Some(next) = admitted_release(&row, worker_owner)? else {
            return Ok(false);
        };
        let changed = sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = ?2, lease_expires_ms = ?3, \
             lease_refreshed_ms = ?4, session_token_sha256 = NULL, \
             preparation_lease_epoch = NULL \
             WHERE work_id = ?5 AND environment_id = ?6 AND lease_epoch = ?7 \
             AND preparation_lease_epoch = lease_epoch",
        )
        .bind(stopped_at)
        .bind(next.owner)
        .bind(next.expires_at)
        .bind(next.refreshed_at)
        .bind(&row.0)
        .bind(env_id)
        .bind(row.3)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        tx.commit().await.map_err(storage)?;
        Ok(changed == 1)
    }
}

/// Hand back a Run-free preparation's Session Work inside the caller's
/// transaction, under the row lock every acquisition takes.
pub async fn release_postgres_preparation_work_on(
    connection: &mut sqlx::PgConnection,
    clock: &dyn WallClock,
    env_id: &str,
    session_id: &SessionId,
    worker_owner: &str,
) -> Result<bool, WorkQueueError> {
    let row: Option<HeldRow> = sqlx::query_as(SELECT_HELD_POSTGRES)
        .bind(env_id)
        .bind(session_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(storage)?;
    let Some(row) = row else {
        return Ok(false);
    };
    let Some(next) = admitted_release(&row, worker_owner)? else {
        return Ok(false);
    };
    let changed = sqlx::query(
        "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
         state = 'stopped', lease_owner = $2, lease_expires_ms = $3, \
         lease_refreshed_ms = $4, session_token_sha256 = NULL, \
         preparation_lease_epoch = NULL \
         WHERE work_id = $5 AND environment_id = $6 AND lease_epoch = $7 \
         AND preparation_lease_epoch = lease_epoch",
    )
    .bind(clock.observed_at())
    .bind(next.owner)
    .bind(next.expires_at)
    .bind(next.refreshed_at)
    .bind(&row.0)
    .bind(env_id)
    .bind(row.3)
    .execute(&mut *connection)
    .await
    .map_err(storage)?
    .rows_affected();
    Ok(changed == 1)
}
