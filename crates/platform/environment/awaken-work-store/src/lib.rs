// @formal entrypoint session_work.release_requires_exact_owner_and_epoch
// @formal entrypoint work.exact_boundary_reclaim
// @formal entrypoint work.heartbeat_compare_and_set
// @formal entrypoint work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal entrypoint work.single_active_claim
// @formal entrypoint work.terminal_lease_clear
//! Durable Work queue backends: the self-hosted environment work queue over a
//! store, so a session dispatched to a self-hosted environment survives a restart
//! and can be claimed by a worker on any node. SQLite (embedded, single machine)
//! lands here; the Postgres backend (distributed) shares the one portable bundle,
//! exactly like the session/config/catalog stores.
//!
//! Its own scope (`work_queue_item` table + `work_queue_schema_migrations` ledger),
//! a distinct aggregate from the managed session config. The lease is the store's
//! own transaction: SQLite claims under a `BEGIN IMMEDIATE` write lock, so a run is
//! owned by one worker at a time. PostgreSQL locks an environment's work rows so
//! its single-active decision is atomic across processes. A Session claim mints
//! one epoch-bound sessions token and stores only its digest on that same row;
//! list/retrieve projections never expose it again.
//!
//! Work ids are opaque. Existing ids and live leases survive schema upgrades;
//! newly allocated ids use a physical-store generation and a durable counter.
//! Deleting Work rows never resets this authority. Missing/corrupt allocator
//! state fails closed rather than deriving a new counter from surviving rows.
//! The generation only seeds once; it does not change the Managed wire fields.
//!
//! Upgrade all queue writers before resuming delivery: older binaries do not
//! participate in the allocator or Environment revision watermark. Schema
//! upgrade compatibility does not qualify mixed-version writers. Catalog
//! readers must also understand historical registration acknowledgements.
//! Restoring an earlier whole-database backup requires fencing outstanding
//! clients; rollback of the authority itself is outside this store's guarantees.
//!
//! Acknowledgements are committed once per lease generation. Replay checks its
//! original generation before consulting the clock; a fresh acknowledgement
//! checks expiry under the write lock. The append-only migration leaves older
//! acknowledgements without a generation marker: their historical timestamp
//! cannot prove which lease wrote it. Such a lease needs a fresh, live ACK.
//! A stop retains its original owner and generation after clearing the live
//! authority and Session digest. Its successful retry returns the stopped
//! decision without sampling a clock or restoring either grant. Historical
//! rows without a recorded stop subject cannot be attributed retroactively.
//!
//! One lease, one clock: every lease reading — claim, renewal, Session
//! acquisition, bearer authentication, acknowledgement and stop — is a sample
//! of the clock this backend was opened with, taken under its write lock. No
//! operation accepts a caller's time, so a skewed caller can neither mint a
//! deadline that is then judged on another clock nor make a live lease refuse
//! its acknowledgement.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, HeartbeatResult, LeaseHeartbeat, LeaseReceipt, QueueStats,
    SessionAccessAuthenticator, SessionAccessDigest, SessionWorkLeaseControl,
    SessionWorkReleaseAdmission, SessionWorkStore, SessionWorkerLeaseStore, WorkClaim,
    WorkConsumer, WorkItem, WorkMetadataPatch, WorkMutationResult, WorkQueries, WorkQueueError,
    WorkSessionAccess, WorkerLeaseObservation, session_work_release_admission,
};
use awaken_session_types::SessionWorkLease;
#[cfg(all(test, not(feature = "loom")))]
use awaken_time::system_clock;
use awaken_time::{Expiry, WallClock};
fn storage(error: impl std::fmt::Display) -> WorkQueueError {
    WorkQueueError::Storage(error.to_string().into())
}

use awaken_sqlite_pool::EmbeddedPool;
use sqlx::Row;
use sqlx::postgres::PgPool;

// Exact-owner observation is a read of durable claims, with no clock or state
// filter and no reclamation side effect.
const HAS_OWNER_CLAIMS: &str =
    "SELECT EXISTS(SELECT 1 FROM work_queue_item WHERE lease_owner = $1)";

// Poll-liveness bookkeeping is shared by the durable stores. Lease authority remains
// in their rows. The volatile executable specification is never part of a default
// product build.
mod lease_book;
pub use lease_book::{LEASE_TTL_MS, LeaseBook, POLLER_WINDOW_MS};
mod backends;
mod identity;
use backends::pg_row_to_item;
mod mutation;
use mutation::{
    LeaseAuthorityCommand, LeaseAuthorityDecision, LeaseAuthorityTuple, LeaseFence,
    StoredLeaseAuthorityInput, apply_metadata_patch, claim_stored_lease_authority, granted_epoch,
    lease_authority_transition, lease_generation_integrity, reclaimed_lease_authority,
    release_stored_lease_authority, renew_stored_lease_authority, stored_epoch,
    stored_lease_authority, stored_lease_epoch, stored_stop_receipt,
};
#[cfg(any(test, feature = "test-support"))]
mod inmem;
#[cfg(any(test, feature = "test-support"))]
pub use inmem::InMemoryWorkQueue;
mod schema;
#[cfg(all(test, not(feature = "loom")))]
#[path = "../tests/support/work_postgres.rs"]
mod work_postgres_fixture;
use awaken_time::WallClockExt;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use schema::*;
mod session_access;

/// Distinct well-formed Session-access digests for queue fixtures. The queue
/// treats the digest as opaque, so fixtures need no bearer or hash algorithm.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn test_session_access_digest(seed: u64) -> SessionAccessDigest {
    SessionAccessDigest::from_hex(format!("{seed:064x}")).expect("fixture digest is hex")
}

/// SQLite persistence for the environment work queue. Ownership, epoch and expiry
/// are durable because they are safety authority; only poller liveness is ephemeral.
pub struct SqliteWorkQueue {
    pool: EmbeddedPool,
    book: LeaseBook,
    /// The one source of every observation timestamp this backend writes.
    clock: Arc<dyn WallClock>,
}

#[async_trait]
impl WorkQueries for SqliteWorkQueue {
    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        let mut conn = self.connection().await?;
        sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE environment_id = ?1 ORDER BY seq ASC"
        ))
        .bind(env_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(storage)?
        .iter()
        .map(|row| row_to_item(row).map_err(storage))
        .collect()
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        let mut conn = self.connection().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(storage)?;
        Self::owned(&mut tx, env_id, wid).await
    }

    async fn stats(&self, env_id: &str) -> Result<QueueStats, WorkQueueError> {
        let mut conn = self.connection().await?;
        // The oldest unfinished item's own durable acceptance instant.
        // A drained queue has no such row, so the absence is the answer
        // rather than something to stand in for.
        let oldest_queued_at: Option<String> = sqlx::query_scalar(
            "SELECT created_at FROM work_queue_item \
             WHERE environment_id = ?1 \
               AND state IN ('queued', 'starting', 'active', 'stopping') \
             ORDER BY created_at ASC, seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
        let mut count = async |state_clause: &str| -> Result<usize, WorkQueueError> {
            sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM work_queue_item WHERE environment_id = ?1 AND {state_clause}"
            ))
            .bind(env_id)
            .fetch_one(&mut *conn)
            .await
            .map(|count| count as usize)
            .map_err(storage)
        };
        let depth = count("state = 'queued'").await?;
        let pending = count("state IN ('starting', 'active', 'stopping')").await?;
        drop(conn);
        // Parity with the in-memory queue: oldest stays set while an item is still
        // processing (queued OR pending), and pollers are counted from the liveness
        // book, not proxied from the active count.
        Ok(QueueStats {
            depth,
            pending,
            oldest_queued_at,
            workers_polling: self.book.workers_polling(env_id, self.clock.now_ms()),
        })
    }
}

#[async_trait]
impl WorkConsumer for SqliteWorkQueue {
    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.claim_with_reclaim(env_id, worker_id, worker_id, None)
            .await
    }

    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        age_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.book
            .record_poll(env_id, poller_id, self.clock.now_ms());
        let mut conn = self.connection().await?;
        Self::claim_inner(
            &mut conn,
            env_id,
            lease_owner,
            self.clock.as_ref(),
            age_ms,
            None,
        )
        .await
        .map(|claim| claim.map(|claim| claim.item))
    }

    async fn claim_with_session_access(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        reclaim_older_than_ms: Option<u64>,
        session_access: &SessionAccessDigest,
    ) -> Result<Option<WorkClaim>, WorkQueueError> {
        self.book
            .record_poll(env_id, poller_id, self.clock.now_ms());
        let mut conn = self.connection().await?;
        Self::claim_inner(
            &mut conn,
            env_id,
            lease_owner,
            self.clock.as_ref(),
            reclaim_older_than_ms,
            Some(session_access.clone()),
        )
        .await
    }

    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut conn = self.connection().await?;
        Self::ack_inner(
            &mut conn,
            env_id,
            wid,
            worker_id,
            LeaseFence::OwnerOnly,
            self.clock.as_ref(),
        )
        .await
    }

    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut conn = self.connection().await?;
        Self::heartbeat_inner(
            &mut conn,
            env_id,
            wid,
            worker_id,
            LeaseFence::OwnerOnly,
            self.clock.as_ref(),
            heartbeat,
        )
        .await
    }

    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut conn = self.connection().await?;
        Self::stop_inner(
            &mut conn,
            env_id,
            wid,
            worker_id,
            LeaseFence::OwnerOnly,
            self.clock.as_ref(),
        )
        .await
    }
}

#[async_trait]
impl SessionAccessAuthenticator for SqliteWorkQueue {
    async fn authenticate_session_access(
        &self,
        presented: &SessionAccessDigest,
    ) -> Result<Option<WorkSessionAccess>, WorkQueueError> {
        let digest = presented.as_str();
        let mut conn = self.connection().await?;
        let row: Option<(String, String, String, String, i64, u64)> = sqlx::query_as(
            "SELECT work_id, environment_id, data_id, lease_owner, lease_epoch, lease_expires_ms \
             FROM work_queue_item WHERE data_type = 'session' AND state = 'active' \
               AND session_token_sha256 = ?1 AND lease_expires_ms IS NOT NULL \
             ORDER BY lease_expires_ms DESC LIMIT 1",
        )
        .bind(digest)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
        // Subject fencing precedes the one clock sample: liveness is judged on
        // the clock that granted and renewed the lease, and only when a bound
        // lease exists to judge.
        let row = row.filter(|row| Expiry::from_epoch_ms(row.5).is_live_at(self.clock.now()));
        row.map(
            |(work_id, environment_id, session, lease_owner, epoch, expires_at_unix_ms)| {
                Ok(WorkSessionAccess {
                    work_id,
                    environment_id,
                    session_id: awaken_session_types::SessionId::from(session),
                    lease_owner,
                    lease_epoch: granted_epoch(epoch)?,
                    expires_at_unix_ms,
                })
            },
        )
        .transpose()
    }
}

#[async_trait]
impl SessionWorkLeaseControl for SqliteWorkQueue {
    async fn ack_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut conn = self.connection().await?;
        Self::ack_inner(
            &mut conn,
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
            self.clock.as_ref(),
        )
        .await
    }

    async fn heartbeat_with_session_access(
        &self,
        access: &WorkSessionAccess,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        let mut conn = self.connection().await?;
        Self::heartbeat_inner(
            &mut conn,
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
            self.clock.as_ref(),
            heartbeat,
        )
        .await
    }

    async fn stop_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        let mut conn = self.connection().await?;
        Self::stop_inner(
            &mut conn,
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
            self.clock.as_ref(),
        )
        .await
    }
}

#[async_trait]
impl SessionWorkStore for SqliteWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, WorkQueueError> {
        let created_at = self.clock.observed_at();
        let mut conn = self.connection().await?;
        Self::insert(
            &mut conn,
            env_id,
            "session",
            Some(session_id.as_str()),
            created_at,
        )
        .await
    }

    async fn wake_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, WorkQueueError> {
        let session_id = session_id.as_str();
        let created_at = self.clock.observed_at();
        let mut conn = self.connection().await?;
        let work_id =
            Self::insert(&mut conn, env_id, "session", Some(session_id), created_at).await?;
        let mut tx = backends::immediate(&mut conn).await?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', acknowledged_at = NULL, \
             latest_heartbeat_at = NULL, started_at = NULL, stop_requested_at = NULL, \
             stopped_at = NULL, lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL, session_token_sha256 = NULL WHERE work_id = ?1 AND environment_id = ?2 \
             AND data_type = 'session' AND data_id = ?3 AND state = 'stopped'",
        )
        .bind(&work_id)
        .bind(env_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(work_id)
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let stopped_at = self.clock.observed_at();
        let mut conn = self.connection().await?;
        let mut tx = backends::immediate(&mut conn).await?;
        let work_id: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = ?1 \
             AND data_type = 'session' AND data_id = ?2 ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .bind(session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(work_id) = work_id else {
            return Ok(None);
        };
        let cleared = reclaimed_lease_authority();
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = ?2, lease_expires_ms = ?3, \
             lease_refreshed_ms = ?4, session_token_sha256 = NULL WHERE work_id = ?5",
        )
        .bind(stopped_at)
        .bind(cleared.owner)
        .bind(cleared.expires_at)
        .bind(cleared.refreshed_at)
        .bind(&work_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let item = Self::owned(&mut tx, env_id, &work_id).await?;
        tx.commit().await.map_err(storage)?;
        Ok(item)
    }
}

#[async_trait]
impl SessionWorkerLeaseStore for SqliteWorkQueue {
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        let mut conn = self.connection().await?;
        let cleared = reclaimed_lease_authority();
        let released = sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = NULL, stopped_at = NULL, \
             state = 'queued', lease_owner = ?1, lease_expires_ms = ?2, \
             lease_refreshed_ms = ?3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
             WHERE data_type = 'session' AND state = 'active' AND lease_owner = ?4",
        )
        .bind(cleared.owner)
        .bind(cleared.expires_at)
        .bind(cleared.refreshed_at)
        .bind(worker_owner)
        .execute(&mut *conn)
        .await
        .map_err(storage)?
        .rows_affected();
        usize::try_from(released).map_err(storage)
    }

    async fn release_session(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError> {
        let stopped_at = self.clock.observed_at();
        let mut conn = self.connection().await?;
        let epoch = i64::try_from(lease.epoch()).map_err(storage)?;
        let mut tx = backends::immediate(&mut conn).await?;
        let current: Option<(String, String, String, Option<String>, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT data_type, data_id, state, lease_owner, lease_epoch, lease_expires_ms, lease_refreshed_ms \
             FROM work_queue_item WHERE work_id = ?1 AND environment_id = ?2",
        )
        .bind(lease.work_id())
        .bind(lease.environment_id())
        .fetch_optional(&mut *tx)
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
            "UPDATE work_queue_item SET stop_requested_at = ?1, stopped_at = ?1, \
             state = 'stopped', lease_owner = ?2, lease_expires_ms = ?3, \
             lease_refreshed_ms = ?4, session_token_sha256 = NULL \
             WHERE work_id = ?5 AND environment_id = ?6",
        )
        .bind(stopped_at)
        .bind(next_authority.owner)
        .bind(next_authority.expires_at)
        .bind(next_authority.refreshed_at)
        .bind(lease.work_id())
        .bind(lease.environment_id())
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        tx.commit().await.map_err(storage)?;
        Ok(changed == 1)
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        self.acquire_session_inner(env_id, session_id, worker_owner)
            .await
    }
}

#[async_trait]
impl WorkerLeaseObservation for SqliteWorkQueue {
    async fn has_owner_claims(&self, worker_owner: &str) -> Result<bool, WorkQueueError> {
        let mut conn = self.connection().await?;
        sqlx::query_scalar::<_, bool>(HAS_OWNER_CLAIMS)
            .bind(worker_owner)
            .fetch_one(&mut *conn)
            .await
            .map_err(storage)
    }
}

#[async_trait]
impl WorkMetadataPatch for SqliteWorkQueue {
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let mut conn = self.connection().await?;
        let mut tx = backends::immediate(&mut conn).await?;
        let Some(mut current) = Self::owned(&mut tx, env_id, wid).await? else {
            return Ok(None);
        };
        apply_metadata_patch(&mut current.metadata, patch);
        let metadata_json = metadata_str(&current.metadata);
        sqlx::query("UPDATE work_queue_item SET metadata_json = ?1 WHERE work_id = ?2")
            .bind(metadata_json)
            .bind(wid)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let item = Self::owned(&mut tx, env_id, wid).await?;
        tx.commit().await.map_err(storage)?;
        Ok(item)
    }
}

/// A Postgres-backed Work queue — the network-DB sibling over the same
/// `work_queue` migration scope, for distributed deployments. Claims run in a
/// transaction that mirrors the SQLite semantics (the single-active-per-env cap);
/// the transaction locks the environment rows before the single-active decision.
pub struct PostgresWorkQueue {
    pool: PgPool,
    book: LeaseBook,
    /// The one source of every observation timestamp this backend writes.
    clock: Arc<dyn WallClock>,
}

#[async_trait]
impl WorkQueries for PostgresWorkQueue {
    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM work_queue_item WHERE environment_id = $1 ORDER BY seq ASC"
        ))
        .bind(env_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .iter()
        .map(pg_row_to_item)
        .collect()
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        self.fetch_owned(env_id, wid).await
    }

    async fn stats(&self, env_id: &str) -> Result<QueueStats, WorkQueueError> {
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
        // Parity with the in-memory queue: oldest is the oldest unfinished item's
        // own durable acceptance instant, and pollers come from the liveness book
        // (not the active-count proxy).
        let oldest_queued_at: Option<String> = sqlx::query_scalar(
            "SELECT created_at FROM work_queue_item \
             WHERE environment_id = $1 \
               AND state IN ('queued', 'starting', 'active', 'stopping') \
             ORDER BY created_at ASC, seq ASC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        Ok(QueueStats {
            depth,
            pending,
            oldest_queued_at,
            workers_polling: self.book.workers_polling(env_id, self.clock.now_ms()),
        })
    }
}

#[async_trait]
impl WorkConsumer for PostgresWorkQueue {
    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.claim_with_reclaim(env_id, worker_id, worker_id, None)
            .await
    }

    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        age_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self
            .claim_inner(env_id, lease_owner, poller_id, age_ms, None)
            .await?
            .map(|claim| claim.item))
    }

    async fn claim_with_session_access(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        reclaim_older_than_ms: Option<u64>,
        session_access: &SessionAccessDigest,
    ) -> Result<Option<WorkClaim>, WorkQueueError> {
        self.claim_inner(
            env_id,
            lease_owner,
            poller_id,
            reclaim_older_than_ms,
            Some(session_access.clone()),
        )
        .await
    }

    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        self.ack_inner(env_id, wid, worker_id, LeaseFence::OwnerOnly)
            .await
    }

    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        self.heartbeat_inner(env_id, wid, worker_id, LeaseFence::OwnerOnly, heartbeat)
            .await
    }

    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        self.stop_inner(env_id, wid, worker_id, LeaseFence::OwnerOnly)
            .await
    }
}

#[async_trait]
impl SessionAccessAuthenticator for PostgresWorkQueue {
    async fn authenticate_session_access(
        &self,
        presented: &SessionAccessDigest,
    ) -> Result<Option<WorkSessionAccess>, WorkQueueError> {
        let digest = presented.as_str();
        let row = sqlx::query(
            "SELECT work_id, environment_id, data_id, lease_owner, lease_epoch, lease_expires_ms \
             FROM work_queue_item WHERE data_type = 'session' AND state = 'active' \
               AND session_token_sha256 = $1 AND lease_expires_ms IS NOT NULL \
             ORDER BY lease_expires_ms DESC LIMIT 1",
        )
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let expires_at_unix_ms =
            u64::try_from(row.try_get::<i64, _>(5).map_err(storage)?).map_err(storage)?;
        // Subject fencing precedes the one clock sample: liveness is judged on
        // the clock that granted and renewed the lease.
        if Expiry::from_epoch_ms(expires_at_unix_ms).is_expired_at(self.clock.now()) {
            return Ok(None);
        }
        Ok(Some(WorkSessionAccess {
            work_id: row.try_get(0).map_err(storage)?,
            environment_id: row.try_get(1).map_err(storage)?,
            session_id: awaken_session_types::SessionId::from(
                row.try_get::<String, _>(2).map_err(storage)?,
            ),
            lease_owner: row.try_get(3).map_err(storage)?,
            lease_epoch: granted_epoch(row.try_get::<i64, _>(4).map_err(storage)?)?,
            expires_at_unix_ms,
        }))
    }
}

#[async_trait]
impl SessionWorkLeaseControl for PostgresWorkQueue {
    async fn ack_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        self.ack_inner(
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
        )
        .await
    }

    async fn heartbeat_with_session_access(
        &self,
        access: &WorkSessionAccess,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        self.heartbeat_inner(
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
            heartbeat,
        )
        .await
    }

    async fn stop_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        self.stop_inner(
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
        )
        .await
    }
}

#[async_trait]
impl SessionWorkStore for PostgresWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, WorkQueueError> {
        self.insert(env_id, "session", Some(session_id.as_str()))
            .await
    }

    async fn wake_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, WorkQueueError> {
        let work_id = self
            .insert(env_id, "session", Some(session_id.as_str()))
            .await?;
        sqlx::query(
            "UPDATE work_queue_item SET state = 'queued', acknowledged_at = NULL, \
             latest_heartbeat_at = NULL, started_at = NULL, stop_requested_at = NULL, \
             stopped_at = NULL, lease_owner = NULL, lease_expires_ms = NULL, \
             lease_refreshed_ms = NULL, session_token_sha256 = NULL WHERE work_id = $1 AND environment_id = $2 \
             AND data_type = 'session' AND data_id = $3 AND state = 'stopped'",
        )
        .bind(&work_id)
        .bind(env_id)
        .bind(session_id.as_str())
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(work_id)
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let work_id: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 \
             AND data_type = 'session' AND data_id = $2 ORDER BY seq ASC LIMIT 1",
        )
        .bind(env_id)
        .bind(session_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some(work_id) = work_id else {
            return Ok(None);
        };
        let cleared = reclaimed_lease_authority();
        sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = $1, stopped_at = $1, \
             state = 'stopped', lease_owner = $2, lease_expires_ms = $3, \
             lease_refreshed_ms = $4, session_token_sha256 = NULL WHERE work_id = $5 AND environment_id = $6",
        )
        .bind(self.clock.observed_at())
        .bind(cleared.owner)
        .bind(cleared.expires_at)
        .bind(cleared.refreshed_at)
        .bind(&work_id)
        .bind(env_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        self.fetch_owned(env_id, &work_id).await
    }
}

#[async_trait]
impl SessionWorkerLeaseStore for PostgresWorkQueue {
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        let cleared = reclaimed_lease_authority();
        let released = sqlx::query(
            "UPDATE work_queue_item SET stop_requested_at = NULL, stopped_at = NULL, \
             state = 'queued', lease_owner = $1, lease_expires_ms = $2, \
             lease_refreshed_ms = $3, latest_heartbeat_at = NULL, session_token_sha256 = NULL \
             WHERE data_type = 'session' AND state = 'active' AND lease_owner = $4",
        )
        .bind(cleared.owner)
        .bind(cleared.expires_at)
        .bind(cleared.refreshed_at)
        .bind(worker_owner)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected();
        usize::try_from(released)
            .map_err(|_| WorkQueueError::Storage("released row count overflow".into()))
    }

    async fn release_session(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError> {
        let epoch = i64::try_from(lease.epoch()).map_err(storage)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current: Option<(String, String, String, Option<String>, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT data_type, data_id, state, lease_owner, lease_epoch, lease_expires_ms, lease_refreshed_ms \
             FROM work_queue_item WHERE work_id = $1 AND environment_id = $2 FOR UPDATE",
        )
        .bind(lease.work_id())
        .bind(lease.environment_id())
        .fetch_optional(&mut *tx)
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
        .bind(self.clock.observed_at())
        .bind(next_authority.owner)
        .bind(next_authority.expires_at)
        .bind(next_authority.refreshed_at)
        .bind(lease.work_id())
        .bind(lease.environment_id())
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        tx.commit().await.map_err(storage)?;
        Ok(changed == 1)
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let work_id = self
            .insert(env_id, "session", Some(session_id.as_str()))
            .await?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let _: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_queue_item WHERE environment_id = $1 FOR UPDATE",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        // The one reading this grant is minted and judged on, sampled under the
        // Environment lock so no later writer can commit an earlier deadline.
        let now_ms = self.clock.now_ms();
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
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let current: (String, Option<String>, i64, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT state, lease_owner, lease_epoch, lease_expires_ms, lease_refreshed_ms FROM work_queue_item \
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
            let epoch = granted_epoch(current.2)?;
            let expires_at_unix_ms = u64::try_from(current.3.unwrap_or_default())
                .map_err(|_| WorkQueueError::Storage("negative Session Work expiry".into()))?;
            return Ok(Some(
                SessionWorkLease::from_persisted(
                    work_id,
                    env_id,
                    session_id.clone(),
                    owner,
                    (epoch).get(),
                    expires_at_unix_ms,
                )
                .expect("canonical grant has a nonzero epoch"),
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
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            next_authority.granted_epoch()?
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
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            granted
        };
        tx.commit().await.map_err(storage)?;
        Ok(Some(
            SessionWorkLease::from_persisted(
                work_id,
                env_id,
                session_id.clone(),
                worker_owner,
                (epoch).get(),
                now_ms.saturating_add(HEARTBEAT_TTL_SECONDS * 1_000),
            )
            .expect("canonical grant has a nonzero epoch"),
        ))
    }
}

#[async_trait]
impl WorkerLeaseObservation for PostgresWorkQueue {
    async fn has_owner_claims(&self, worker_owner: &str) -> Result<bool, WorkQueueError> {
        sqlx::query_scalar::<_, bool>(HAS_OWNER_CLAIMS)
            .bind(worker_owner)
            .fetch_one(&self.pool)
            .await
            .map_err(storage)
    }
}

#[async_trait]
impl WorkMetadataPatch for PostgresWorkQueue {
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
}

#[cfg(all(test, not(feature = "loom")))]
mod tests;

#[cfg(test)]
mod product_readiness_tests {
    #[test]
    fn volatile_queue_is_opt_in_and_its_export_is_feature_gated() {
        // Cause/effect graph: C1 the product uses default features; C2 test-support
        // is explicitly enabled. Effects: E1 the volatile backend is unreachable;
        // E2 the reference backend is available to conformance tests. Constraint:
        // C1 and C2 are mutually exclusive build selections for this boundary.
        //
        // | Rule | default product | test-support | InMemoryWorkQueue export |
        // | T1   | yes             | no           | absent                   |
        // | T2   | no              | yes          | present                  |
        //
        // T1 is completed by the default-feature `cargo check`; T2 is completed by
        // the all-features conformance suite. This source-level fitness assertion
        // prevents either selector from being silently removed or made default.
        let manifest = include_str!("../Cargo.toml");
        let source = include_str!("lib.rs").replace("\r\n", "\n");
        assert!(manifest.contains("test-support = []"), "T2 selector");
        assert!(
            !manifest.contains("default = [\"test-support\"]"),
            "T1 must remain the default"
        );
        assert!(
            source.contains("#[cfg(any(test, feature = \"test-support\"))]\nmod inmem;")
                && source.contains(
                    "#[cfg(any(test, feature = \"test-support\"))]\npub use inmem::InMemoryWorkQueue;"
                ),
            "T1/T2 export gate"
        );
    }
}
