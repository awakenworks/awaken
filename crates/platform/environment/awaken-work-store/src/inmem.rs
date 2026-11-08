// @formal entrypoint session_work.release_requires_exact_owner_and_epoch
// @formal entrypoint work.heartbeat_compare_and_set
// @formal entrypoint work.lease_authority_snapshot_and_owner_mutations_are_atomic
//! In-memory reference Work queue backend.
//!
//! This executable specification is available only to unit tests and consumers that
//! explicitly enable `test-support`. Product composition must select SQLite or
//! PostgreSQL so accepted work survives process loss.

#[cfg(all(test, feature = "loom"))]
use crate::lease_book::loom_lock::LeafLock;
#[cfg(not(all(test, feature = "loom")))]
use awaken_sync::LeafLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, EnvironmentWorkRevisionOperation as Operation,
    EnvironmentWorkRevisionOutcome as RevisionOutcome, HeartbeatResult, LeaseHeartbeat,
    LeaseReceipt, QueueStats, SessionAccessAuthenticator, SessionAccessDigest,
    SessionWorkLeaseControl, SessionWorkReleaseAdmission, SessionWorkStore,
    SessionWorkerLeaseStore, WorkClaim, WorkConsumer, WorkItem, WorkLeaseMutationAdmission,
    WorkMetadataPatch, WorkMutationResult, WorkPayload, WorkQueries, WorkQueueError,
    WorkSessionAccess, WorkState, WorkerLeaseObservation, admit_environment_work_revision,
    session_work_release_admission, work_lease_mutation_admission,
};
use awaken_session_types::SessionWorkLease;
use awaken_time::WallClock;

use super::mutation::{LeaseFence, StopReceipt};
use super::{LEASE_TTL_MS, LeaseBook, heartbeat_at};
use awaken_time::WallClockExt;

/// The lease TTL a heartbeat reports (seconds).
const HEARTBEAT_TTL_SECONDS: u64 = 60;

/// Test-support single-process reference queue: a `BTreeMap` keyed by monotonic
/// work id (ascending id == enqueue order).
pub struct InMemoryWorkQueue {
    authority: LeafLock<InMemoryQueueAuthority>,
    seq: AtomicU64,
    /// The one source of every observation timestamp this reference backend
    /// writes — the same seam the durable siblings carry, so a conformance test
    /// can drive all three from one [`ManualClock`](awaken_lease_testkit::ManualClock).
    clock: std::sync::Arc<dyn WallClock>,
}

/// All queue state, lease and token accesses share this one commit boundary.
/// The existing LeaseBook remains the sole lease arithmetic implementation.
#[derive(Default)]
struct InMemoryQueueAuthority {
    environment_revisions: BTreeMap<String, (EnvironmentRevision, Operation)>,
    works: BTreeMap<String, WorkItem>,
    ack_epochs: BTreeMap<String, u64>,
    stop_receipts: BTreeMap<String, StopReceipt>,
    session_token_sha256: HashMap<String, SessionAccessDigest>,
    book: LeaseBook,
}

impl InMemoryQueueAuthority {
    fn current_session_lease(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
        now_ms: u64,
    ) -> Option<SessionWorkLease> {
        let work = self
            .works
            .values()
            .find(|work| {
                work.environment_id == env_id
                    && work.state == WorkState::Active
                    && matches!(&work.data, WorkPayload::Session { id } if id == session_id)
            })
            .cloned()?;
        self.book
            .authority(&work.id, now_ms)
            .and_then(|(owner, epoch, expires_at_unix_ms)| {
                SessionWorkLease::from_persisted(
                    work.id,
                    env_id,
                    session_id.clone(),
                    owner,
                    epoch,
                    expires_at_unix_ms,
                )
            })
    }
}

impl InMemoryWorkQueue {
    fn stop_work(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        fence: LeaseFence,
    ) -> WorkMutationResult {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority {
            works,
            book,
            session_token_sha256,
            stop_receipts,
            ..
        } = &mut *authority;
        let Some(work) = works
            .get_mut(wid)
            .filter(|work| work.environment_id == env_id)
        else {
            return WorkMutationResult::NotFound;
        };
        let snapshot = book.snapshot(wid);
        let epoch = snapshot
            .epoch
            .and_then(awaken_lease_contract::fence::Epoch::from_persisted);
        if work.state == WorkState::Stopped {
            return if snapshot.owner.is_none()
                && !snapshot.lease_present
                && work.stopped_at.is_some()
                && work.stop_requested_at.is_some()
                && stop_receipts
                    .get(wid)
                    .is_some_and(|receipt| receipt.matches(worker_id, epoch, fence))
            {
                WorkMutationResult::accepted(work.clone())
            } else {
                WorkMutationResult::PreconditionFailed
            };
        }
        if snapshot.owner.as_deref() != Some(worker_id) || !fence.admits(epoch) {
            return WorkMutationResult::PreconditionFailed;
        }
        let Some(epoch) = epoch else {
            return WorkMutationResult::PreconditionFailed;
        };
        let stamp = self.clock.observed_at();
        work.stop_requested_at = Some(stamp.clone());
        work.stopped_at = Some(stamp);
        work.state = work.state.after_stop();
        book.release(wid);
        session_token_sha256.remove(wid);
        stop_receipts.insert(
            wid.to_owned(),
            StopReceipt {
                owner: worker_id.to_owned(),
                epoch,
            },
        );
        WorkMutationResult::accepted(work.clone())
    }

    fn acknowledge(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        fence: LeaseFence,
    ) -> WorkMutationResult {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority {
            works,
            book,
            ack_epochs,
            ..
        } = &mut *authority;
        let Some(work) = works
            .get_mut(wid)
            .filter(|work| work.environment_id == env_id)
        else {
            return WorkMutationResult::NotFound;
        };
        let owns = match fence {
            LeaseFence::OwnerOnly => book.is_owned_by(wid, worker_id),
            LeaseFence::Exact(epoch) => book.is_owned_at_epoch(wid, worker_id, epoch.get()),
        };
        if !owns {
            return WorkMutationResult::PreconditionFailed;
        }
        let Some(epoch) = book.snapshot(wid).epoch else {
            return WorkMutationResult::PreconditionFailed;
        };
        if work.acknowledged_at.is_none() || ack_epochs.get(wid) != Some(&epoch) {
            let now = self.clock.now();
            if !book.is_leased(wid, now.as_epoch_ms()) {
                return WorkMutationResult::PreconditionFailed;
            }
            work.acknowledged_at = Some(now.to_rfc3339());
            work.state = work.state.after_ack();
            ack_epochs.insert(wid.to_owned(), epoch);
        }
        WorkMutationResult::accepted(work.clone())
    }

    /// The reference queue stamping and judging with `clock`, the composing
    /// process's (or test's) one wall clock.
    #[must_use]
    pub fn new(clock: std::sync::Arc<dyn WallClock>) -> Self {
        Self {
            authority: LeafLock::new(InMemoryQueueAuthority::default()),
            seq: AtomicU64::new(0),
            clock,
        }
    }

    fn enqueue_session_inner(
        &self,
        works: &mut BTreeMap<String, WorkItem>,
        env_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        if let Some(existing) = works
            .values()
            .find(|work| {
                work.environment_id == env_id
                    && matches!(&work.data, WorkPayload::Session { id } if id == session_id)
            })
            .map(|work| work.id.clone())
        {
            return Ok(existing);
        }
        let id = self.next_id();
        works.insert(
            id.clone(),
            WorkItem {
                id: id.clone(),
                environment_id: env_id.to_string(),
                data: WorkPayload::Session {
                    id: session_id.to_string(),
                },
                metadata: BTreeMap::new(),
                state: WorkState::Queued,
                created_at: self.clock.observed_at(),
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
        Ok(id)
    }

    fn next_id(&self) -> String {
        format!("work_{:016}", self.seq.fetch_add(1, Ordering::SeqCst))
    }

    fn insert_healthcheck(&self, authority: &mut InMemoryQueueAuthority, env_id: &str) -> String {
        let id = self.next_id();
        authority.works.insert(
            id.clone(),
            WorkItem {
                id: id.clone(),
                environment_id: env_id.into(),
                data: WorkPayload::HealthCheck { id: id.clone() },
                metadata: BTreeMap::new(),
                state: WorkState::Queued,
                created_at: self.clock.observed_at(),
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
        id
    }

    fn ensure_healthcheck_locked(
        &self,
        authority: &mut InMemoryQueueAuthority,
        env_id: &str,
    ) -> String {
        authority
            .works
            .values()
            .find(|item| {
                item.environment_id == env_id
                    && matches!(item.data, WorkPayload::HealthCheck { .. })
            })
            .map(|item| item.id.clone())
            .unwrap_or_else(|| self.insert_healthcheck(authority, env_id))
    }

    fn remove_work_locked(
        authority: &mut InMemoryQueueAuthority,
        env_id: &str,
        healthchecks_only: bool,
    ) {
        let ids: Vec<_> = authority
            .works
            .values()
            .filter(|item| {
                item.environment_id == env_id
                    && (!healthchecks_only || matches!(item.data, WorkPayload::HealthCheck { .. }))
            })
            .map(|item| item.id.clone())
            .collect();
        authority.works.retain(|id, _| !ids.contains(id));
        authority.ack_epochs.retain(|id, _| !ids.contains(id));
        authority.stop_receipts.retain(|id, _| !ids.contains(id));
        authority.book.forget_env(env_id, &ids);
        authority
            .session_token_sha256
            .retain(|id, _| !ids.contains(id));
    }

    fn require_healthcheck_locked(
        authority: &InMemoryQueueAuthority,
        env_id: &str,
    ) -> Result<(), WorkQueueError> {
        if authority
            .environment_revisions
            .get(env_id)
            .is_some_and(|(_, operation)| {
                !matches!(operation, Operation::Register { healthcheck: true })
            })
        {
            Err(super::storage(
                "unversioned healthcheck cannot bypass the Environment registration watermark",
            ))
        } else {
            Ok(())
        }
    }

    /// Run `f` on the item under `wid` only when it belongs to `env_id`.
    fn with_owned<R>(
        &self,
        env_id: &str,
        wid: &str,
        f: impl FnOnce(&mut WorkItem, &LeaseBook) -> R,
    ) -> Option<R> {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority { works, book, .. } = &mut *authority;
        match works.get_mut(wid) {
            Some(w) if w.environment_id == env_id => Some(f(w, book)),
            _ => None,
        }
    }

    fn claim_inner(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        reclaim_older_than_ms: Option<u64>,
        session_access: Option<&SessionAccessDigest>,
    ) -> Result<Option<WorkClaim>, WorkQueueError> {
        let mut authority = self.authority.lock();
        // The one store-clock sample, under the authority lock: reclaim, the
        // new deadline and `started_at` are this reading, and acknowledgement,
        // authentication and stop judge the lease on the same clock.
        let now_ms = self.clock.now_ms();
        let InMemoryQueueAuthority {
            works,
            book,
            session_token_sha256,
            ..
        } = &mut *authority;
        book.record_poll(env_id, poller_id, now_ms);
        for (wid, work) in works.iter_mut() {
            if work.environment_id == env_id
                && work.state == WorkState::Active
                && reclaim_older_than_ms.map_or_else(
                    || !book.is_leased(wid, now_ms),
                    |age| !book.is_leased_with_reclaim_age(wid, now_ms, age),
                )
            {
                work.state = WorkState::Queued;
                work.latest_heartbeat_at = None;
                book.release(wid);
                session_token_sha256.remove(wid);
            }
        }
        if works
            .values()
            .any(|work| work.environment_id == env_id && work.state == WorkState::Active)
        {
            return Ok(None);
        }
        // The head of the queue is the oldest claimable item. One whose lease
        // generation is untrustworthy is stopped here, permanently, and the
        // item behind it becomes the head in this same claim.
        let wid = loop {
            let Some(wid) = works
                .iter()
                .filter(|(_, work)| work.environment_id == env_id && work.state.is_claimable())
                .map(|(id, _)| id.clone())
                .min()
            else {
                return Ok(None);
            };
            match book.claim_for(&wid, lease_owner, now_ms, LEASE_TTL_MS) {
                Ok(_) => break wid,
                Err(WorkQueueError::LeaseGenerationIntegrity(violation)) => {
                    let work = works.get_mut(&wid).expect("just found");
                    let stopped_at = awaken_time::utc::epoch_millis_to_rfc3339(now_ms);
                    work.stop_requested_at = Some(stopped_at.clone());
                    work.stopped_at = Some(stopped_at);
                    work.state = WorkState::Stopped;
                    session_token_sha256.remove(&wid);
                    crate::session_access::observe_untrustworthy_work(env_id, &wid, violation);
                }
                Err(error) => return Err(error),
            }
        };
        let work = works.get_mut(&wid).expect("just found");
        work.state = WorkState::Active;
        work.started_at = Some(awaken_time::utc::epoch_millis_to_rfc3339(now_ms));
        work.latest_heartbeat_at = None;
        let session_access = session_access
            .filter(|_| matches!(work.data, WorkPayload::Session { .. }))
            .cloned();
        if let Some(digest) = session_access.as_ref() {
            session_token_sha256.insert(wid, digest.clone());
        }
        Ok(Some(WorkClaim {
            item: work.clone(),
            session_access,
        }))
    }
}

#[async_trait]
impl WorkQueries for InMemoryWorkQueue {
    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        Ok(self
            .authority
            .lock()
            .works
            .values()
            .filter(|w| w.environment_id == env_id)
            .cloned()
            .collect())
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self.with_owned(env_id, wid, |w, _| w.clone()))
    }

    async fn stats(&self, env_id: &str) -> Result<QueueStats, WorkQueueError> {
        let mut authority = self.authority.lock();
        let now_ms = self.clock.now_ms();
        let InMemoryQueueAuthority { works, book, .. } = &mut *authority;
        let in_env: Vec<&WorkItem> = works
            .values()
            .filter(|w| w.environment_id == env_id)
            .collect();
        // `depth` = items waiting to be claimed; `pending` = items a worker has
        // claimed and is currently processing (the SDK's queue-stats semantics).
        let queued = in_env
            .iter()
            .filter(|w| w.state == WorkState::Queued)
            .count();
        let pending = in_env
            .iter()
            .filter(|w| {
                matches!(
                    w.state,
                    WorkState::Starting | WorkState::Active | WorkState::Stopping
                )
            })
            .count();
        // `workers_polling` = distinct workers that polled within the liveness window
        // (the SDK's ~30s semantics), tracked from the `worker_id` each poll carries —
        // not the 0/1 active-item proxy the clockless build was limited to.
        let workers_polling = book.workers_polling(env_id, now_ms);
        // `oldest_queued_at` is the acceptance instant of the oldest item still
        // QUEUED or being PROCESSED (the SDK's semantics), so it stays set once a
        // worker claims the last queued item — not just while `depth > 0`. Only a
        // fully drained queue (all stopped) has no such row, and then the absence
        // is the answer.
        let oldest_queued_at = in_env
            .iter()
            .filter(|w| {
                matches!(
                    w.state,
                    WorkState::Queued
                        | WorkState::Starting
                        | WorkState::Active
                        | WorkState::Stopping
                )
            })
            .min_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|w| w.created_at.clone());
        Ok(QueueStats {
            depth: queued,
            pending,
            oldest_queued_at,
            workers_polling,
        })
    }
}

#[async_trait]
impl WorkConsumer for InMemoryWorkQueue {
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
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self
            .claim_inner(env_id, lease_owner, poller_id, reclaim_older_than_ms, None)?
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
            Some(session_access),
        )
    }

    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        Ok(self.acknowledge(env_id, wid, worker_id, LeaseFence::OwnerOnly))
    }

    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        Ok(self
            .with_owned(env_id, wid, |w, book| {
                if !book.is_owned_by(wid, worker_id) {
                    return HeartbeatResult::PreconditionFailed;
                }
                if !heartbeat
                    .condition
                    .permits(w.latest_heartbeat_at.as_deref())
                {
                    return HeartbeatResult::PreconditionFailed;
                }
                // Admission precedes the one store-clock sample the extension
                // starts from.
                let now_ms = self.clock.now_ms();
                let extended = w.state.can_extend_lease();
                let last_heartbeat = heartbeat_at(now_ms, w.latest_heartbeat_at.as_deref());
                if extended {
                    w.latest_heartbeat_at = Some(last_heartbeat.clone());
                    let ttl = heartbeat
                        .desired_ttl_seconds
                        .unwrap_or(HEARTBEAT_TTL_SECONDS)
                        .max(1);
                    book.lease_for(wid, now_ms, ttl.saturating_mul(1000));
                }
                HeartbeatResult::Accepted(LeaseReceipt {
                    last_heartbeat,
                    lease_extended: extended,
                    state: w.state,
                    ttl_seconds: heartbeat
                        .desired_ttl_seconds
                        .unwrap_or(HEARTBEAT_TTL_SECONDS)
                        .max(1),
                })
            })
            .unwrap_or(HeartbeatResult::NotFound))
    }

    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        Ok(self.stop_work(env_id, wid, worker_id, LeaseFence::OwnerOnly))
    }
}

#[async_trait]
impl SessionAccessAuthenticator for InMemoryWorkQueue {
    async fn authenticate_session_access(
        &self,
        presented: &SessionAccessDigest,
    ) -> Result<Option<WorkSessionAccess>, WorkQueueError> {
        let authority = self.authority.lock();
        let Some(work_id) = authority
            .session_token_sha256
            .iter()
            .find_map(|(id, stored)| (stored == presented).then_some(id))
        else {
            return Ok(None);
        };
        let Some(work) = authority
            .works
            .get(work_id)
            .filter(|work| work.state == WorkState::Active)
        else {
            return Ok(None);
        };
        let WorkPayload::Session { id: session_id } = &work.data else {
            return Ok(None);
        };
        // Subject fencing precedes the one clock sample: liveness is judged on
        // the clock that granted and renewed the lease.
        let now_ms = self.clock.now_ms();
        Ok(authority.book.authority(work_id, now_ms).and_then(
            |(lease_owner, lease_epoch, expires_at_unix_ms)| {
                Some(WorkSessionAccess {
                    work_id: work_id.clone(),
                    environment_id: work.environment_id.clone(),
                    session_id: awaken_session_types::SessionId::from(session_id),
                    lease_owner,
                    lease_epoch: awaken_lease_contract::fence::Epoch::from_persisted(lease_epoch)?,
                    expires_at_unix_ms,
                })
            },
        ))
    }
}

#[async_trait]
impl SessionWorkLeaseControl for InMemoryWorkQueue {
    async fn ack_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        Ok(self.acknowledge(
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
        ))
    }

    async fn heartbeat_with_session_access(
        &self,
        access: &WorkSessionAccess,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        Ok(self
            .with_owned(&access.environment_id, &access.work_id, |work, book| {
                let admission = work_lease_mutation_admission(
                    true,
                    book.is_owned_at_epoch(
                        &access.work_id,
                        &access.lease_owner,
                        access.lease_epoch.get(),
                    ),
                    heartbeat
                        .condition
                        .permits(work.latest_heartbeat_at.as_deref()),
                );
                if admission != WorkLeaseMutationAdmission::Applied {
                    return HeartbeatResult::PreconditionFailed;
                }
                // Admission precedes the one store-clock sample the extension
                // starts from.
                let now_ms = self.clock.now_ms();
                let extended = work.state.can_extend_lease();
                let last_heartbeat = heartbeat_at(now_ms, work.latest_heartbeat_at.as_deref());
                let ttl_seconds = heartbeat
                    .desired_ttl_seconds
                    .unwrap_or(HEARTBEAT_TTL_SECONDS)
                    .max(1);
                if extended {
                    work.latest_heartbeat_at = Some(last_heartbeat.clone());
                    book.lease_for(&access.work_id, now_ms, ttl_seconds.saturating_mul(1000));
                }
                HeartbeatResult::Accepted(LeaseReceipt {
                    last_heartbeat,
                    lease_extended: extended,
                    state: work.state,
                    ttl_seconds,
                })
            })
            .unwrap_or(HeartbeatResult::NotFound))
    }

    async fn stop_with_session_access(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        Ok(self.stop_work(
            &access.environment_id,
            &access.work_id,
            &access.lease_owner,
            LeaseFence::Exact(access.lease_epoch),
        ))
    }
}

#[async_trait]
impl SessionWorkStore for InMemoryWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, WorkQueueError> {
        let mut authority = self.authority.lock();
        self.enqueue_session_inner(&mut authority.works, env_id, session_id.as_str())
    }

    async fn wake_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, WorkQueueError> {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority {
            works,
            book,
            session_token_sha256,
            ..
        } = &mut *authority;
        if let Some(existing) = works.values_mut().find(|work| {
            work.environment_id == env_id
                && matches!(&work.data, WorkPayload::Session { id } if id == session_id)
        }) {
            if existing.state == WorkState::Stopped {
                existing.state = WorkState::Queued;
                existing.acknowledged_at = None;
                existing.latest_heartbeat_at = None;
                existing.started_at = None;
                existing.stop_requested_at = None;
                existing.stopped_at = None;
                book.release(&existing.id);
                session_token_sha256.remove(&existing.id);
            }
            return Ok(existing.id.clone());
        }
        let id = self.next_id();
        works.insert(
            id.clone(),
            WorkItem {
                id: id.clone(),
                environment_id: env_id.to_string(),
                data: WorkPayload::Session {
                    id: session_id.to_string(),
                },
                metadata: BTreeMap::new(),
                state: WorkState::Queued,
                created_at: self.clock.observed_at(),
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
        Ok(id)
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority {
            works,
            book,
            session_token_sha256,
            ..
        } = &mut *authority;
        let Some((wid, work)) = works.iter_mut().find(|(_, work)| {
            work.environment_id == env_id
                && matches!(&work.data, WorkPayload::Session { id } if id == session_id)
        }) else {
            return Ok(None);
        };
        let stopped_at = self.clock.observed_at();
        work.stop_requested_at = Some(stopped_at.clone());
        work.stopped_at = Some(stopped_at);
        work.state = WorkState::Stopped;
        let item = work.clone();
        book.release(wid);
        session_token_sha256.remove(wid);
        Ok(Some(item))
    }
}

#[async_trait]
impl SessionWorkerLeaseStore for InMemoryWorkQueue {
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority {
            works,
            book,
            session_token_sha256,
            ..
        } = &mut *authority;
        let mut released = 0;
        for (id, work) in works.iter_mut() {
            if work.state == WorkState::Active
                && matches!(&work.data, WorkPayload::Session { .. })
                && book.is_owned_by(id, worker_owner)
            {
                work.stop_requested_at = None;
                work.stopped_at = None;
                work.latest_heartbeat_at = None;
                work.state = WorkState::Queued;
                book.release(id);
                session_token_sha256.remove(id);
                released += 1;
            }
        }
        Ok(released)
    }

    async fn release_session(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError> {
        let mut authority = self.authority.lock();
        let InMemoryQueueAuthority {
            works,
            book,
            session_token_sha256,
            ..
        } = &mut *authority;
        let Some(work) = works.get_mut(lease.work_id()) else {
            return Ok(false);
        };
        let authority = book.snapshot(lease.work_id());
        let admission = session_work_release_admission(
            work.environment_id == lease.environment_id()
                && matches!(&work.data, WorkPayload::Session { id } if id == lease.session_id()),
            work.state == WorkState::Active,
            work.state == WorkState::Stopped,
            authority.owner.as_deref() == Some(lease.owner()),
            authority.epoch == Some(lease.epoch()),
            authority.owner.is_none() && !authority.lease_present,
        );
        match admission {
            SessionWorkReleaseAdmission::AlreadyReleased => return Ok(true),
            SessionWorkReleaseAdmission::Reject => return Ok(false),
            SessionWorkReleaseAdmission::Apply => {}
        }
        let stopped_at = self.clock.observed_at();
        work.stop_requested_at = Some(stopped_at.clone());
        work.stopped_at = Some(stopped_at);
        work.state = WorkState::Stopped;
        book.release(lease.work_id());
        session_token_sha256.remove(lease.work_id());
        Ok(true)
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        let mut authority = self.authority.lock();
        // The one reading this grant is minted and judged on.
        let now_ms = self.clock.now_ms();
        if let Some(lease) = authority.current_session_lease(env_id, session_id, now_ms) {
            if lease.owner() != worker_owner {
                return Ok(Some(lease));
            }
            authority.book.lease(lease.work_id(), now_ms);
            return Ok(authority.current_session_lease(env_id, session_id, now_ms));
        }
        let work_id =
            self.enqueue_session_inner(&mut authority.works, env_id, session_id.as_str())?;
        {
            let InMemoryQueueAuthority {
                works,
                book,
                session_token_sha256,
                ..
            } = &mut *authority;
            for (id, work) in works.iter_mut() {
                if work.environment_id == env_id
                    && work.state == WorkState::Active
                    && !book.is_leased(id, now_ms)
                {
                    work.state = WorkState::Queued;
                    work.latest_heartbeat_at = None;
                    book.release(id);
                    session_token_sha256.remove(id);
                }
            }
            if works
                .values()
                .any(|work| work.environment_id == env_id && work.state == WorkState::Active)
            {
                return Ok(None);
            }
            let Some(work) = works.get_mut(&work_id) else {
                return Ok(None);
            };
            if !work.state.is_claimable() {
                return Ok(None);
            }
            book.claim_for(&work_id, worker_owner, now_ms, LEASE_TTL_MS)?;
            work.state = WorkState::Active;
            work.started_at = Some(awaken_time::utc::epoch_millis_to_rfc3339(now_ms));
            work.latest_heartbeat_at = None;
        }
        Ok(authority.current_session_lease(env_id, session_id, now_ms))
    }
}

#[async_trait]
impl WorkerLeaseObservation for InMemoryWorkQueue {
    async fn has_owner_claims(&self, worker_owner: &str) -> Result<bool, WorkQueueError> {
        let authority = self.authority.lock();
        Ok(authority
            .works
            .keys()
            .any(|id| authority.book.is_owned_by(id, worker_owner)))
    }
}

#[async_trait]
impl WorkMetadataPatch for InMemoryWorkQueue {
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self.with_owned(env_id, wid, |w, _| {
            for (key, value) in patch {
                match value {
                    Some(value) => {
                        w.metadata.insert(key, value);
                    }
                    None => {
                        w.metadata.remove(&key);
                    }
                }
            }
            w.clone()
        }))
    }
}

#[async_trait]
impl EnvironmentWorkLifecycle for InMemoryWorkQueue {
    async fn apply_environment_revision(
        &self,
        env_id: &str,
        revision: EnvironmentRevision,
        operation: Operation,
    ) -> Result<RevisionOutcome, awaken_executable_environment_contract::EnvironmentWorkRevisionError>
    {
        let mut authority = self.authority.lock();
        let outcome = admit_environment_work_revision(
            env_id,
            revision,
            operation,
            authority.environment_revisions.get(env_id).copied(),
        )?;
        if outcome != RevisionOutcome::Applied {
            return Ok(outcome);
        }
        match operation {
            Operation::Register { healthcheck: true } => {
                self.ensure_healthcheck_locked(&mut authority, env_id);
            }
            Operation::Register { healthcheck: false } => {
                Self::remove_work_locked(&mut authority, env_id, true)
            }
            Operation::Withdraw => Self::remove_work_locked(&mut authority, env_id, true),
        }
        authority
            .environment_revisions
            .insert(env_id.into(), (revision, operation));
        Ok(outcome)
    }
    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        let mut authority = self.authority.lock();
        Self::require_healthcheck_locked(&authority, env_id)?;
        Ok(self.insert_healthcheck(&mut authority, env_id))
    }
    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        let mut authority = self.authority.lock();
        Self::require_healthcheck_locked(&authority, env_id)?;
        Ok(self.ensure_healthcheck_locked(&mut authority, env_id))
    }
    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
        let mut authority = self.authority.lock();
        Self::remove_work_locked(&mut authority, env_id, false);
        Ok(())
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use crate::{LEASE_TTL_MS, POLLER_WINDOW_MS, test_session_access_digest};

    fn q() -> InMemoryWorkQueue {
        clocked().0
    }

    /// The queue and its store clock, on the logical epoch-zero timeline these
    /// decision tables advance.
    fn clocked() -> (
        InMemoryWorkQueue,
        std::sync::Arc<awaken_lease_testkit::ManualClock>,
    ) {
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        (InMemoryWorkQueue::new(clock.clone()), clock)
    }

    /// Decision table W1-W4: `crate::tests::assert_untrustworthy_head_is_stopped`.
    #[cfg(not(feature = "loom"))]
    #[tokio::test]
    async fn untrustworthy_head_is_stopped_and_the_queue_continues() {
        let queue = q();
        crate::tests::assert_untrustworthy_head_is_stopped(&queue, |work| {
            queue.authority.lock().book.corrupt_epoch_for_test(
                &work,
                awaken_lease_contract::fence::Epoch::from_persisted(u64::MAX).unwrap(),
            );
            std::future::ready(())
        })
        .await;
    }

    // Pin the Anthropic wire vocabulary each `WorkState` renders to. This is the
    // wire-facing contract the durable work-store's `state_from_wire` (in
    // `awaken-work-store`) must round-trip against — including `Stopping`, which no
    // transition currently emits but is kept as wire vocabulary a future writer
    // could persist. No inverse fn lives in THIS crate (the parse is in the store),
    // so we pin `as_str` only.
    #[test]
    fn work_state_as_str_pins_the_wire_vocabulary() {
        assert_eq!(WorkState::Queued.as_str(), "queued");
        assert_eq!(WorkState::Starting.as_str(), "starting");
        assert_eq!(WorkState::Active.as_str(), "active");
        assert_eq!(WorkState::Stopping.as_str(), "stopping");
        assert_eq!(WorkState::Stopped.as_str(), "stopped");
    }

    #[tokio::test]
    async fn healthcheck_seed_carries_its_own_id_and_is_queued() {
        // Cause/effect graph: C1 queue starts empty; C2 enqueue kind is
        // healthcheck/session; C3 enqueue is repeated. Effects: E1 one monotonic id
        // is consumed per accepted item; E2 a healthcheck data id equals its work id;
        // E3 list order equals enqueue order.
        //
        // | Rule | first kind | second kind | ids                | health self-ref | order |
        // | T1   | health     | session     | work_0, work_1     | yes             | H,S   |
        // | T2   | session    | health      | work_0, work_1     | yes             | S,H   |
        let queue = q();
        let id = queue.enqueue_healthcheck("env_a").await.expect("enqueue");
        let w = queue.get("env_a", &id).await.expect("get").expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkPayload::HealthCheck { id: ref d } if *d == id));
        let session = queue
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("session_a"))
            .await
            .expect("enqueue");
        assert_eq!(id, "work_0000000000000000", "T1/E1");
        assert_eq!(session, "work_0000000000000001", "T1/E1");
        assert_eq!(
            queue
                .list("env_a")
                .await
                .expect("list")
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![id, session],
            "T1/E3"
        );

        let reversed = q();
        let session = reversed
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("session_a"))
            .await
            .expect("T2 session");
        let health = reversed
            .enqueue_healthcheck("env_a")
            .await
            .expect("T2 health");
        assert_eq!(session, "work_0000000000000000", "T2/E1");
        assert_eq!(health, "work_0000000000000001", "T2/E1");
        let item = reversed
            .get("env_a", &health)
            .await
            .expect("T2 get")
            .expect("T2 item");
        assert!(
            matches!(item.data, WorkPayload::HealthCheck { id } if id == health),
            "T2/E2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_healthcheck_convergence_has_one_canonical_work_item() {
        // Cause/effect graph: C1 the Environment is registered concurrently; C2
        // no healthcheck exists initially; C3 every caller uses ensure rather
        // than unconditional enqueue. Effects: E1 every caller receives the same
        // identity and E2 the queue contains exactly one probe.
        //
        // | Rule | existing | concurrent ensures | identities | queue rows |
        // | T1 | no | 32 | one canonical id | 1 |
        // | T2 | yes | replay | same canonical id | 1 |
        let queue = std::sync::Arc::new(q());
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let queue = queue.clone();
            tasks.push(tokio::spawn(async move {
                queue.ensure_healthcheck("env_a").await.expect("T1")
            }));
        }
        let mut ids = Vec::new();
        for task in tasks {
            ids.push(task.await.expect("join T1"));
        }
        assert!(ids.iter().all(|id| id == &ids[0]), "T1");
        assert_eq!(queue.list("env_a").await.expect("list").len(), 1, "T1");
        assert_eq!(
            queue.ensure_healthcheck("env_a").await.expect("T2"),
            ids[0],
            "T2"
        );
        assert_eq!(queue.list("env_a").await.expect("list").len(), 1, "T2");
    }

    #[tokio::test]
    async fn claim_leases_oldest_queued_and_caps_at_one_active() {
        let q = q();
        let w1 = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        let _w2 = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s2"))
            .await
            .expect("enqueue");
        let leased = q
            .claim("env_a", "w")
            .await
            .expect("claim query")
            .expect("leases the oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        // A second poll is capped while one is live-leased.
        assert!(
            q.claim("env_a", "w").await.expect("claim").is_none(),
            "single active lease"
        );
        // Stopping the active one frees the lease for the next.
        q.stop("env_a", &w1, "w")
            .await
            .expect("stop query")
            .into_item()
            .expect("stop");
        assert!(
            q.claim("env_a", "w").await.expect("claim").is_some(),
            "next lease after stop"
        );
    }

    #[tokio::test]
    async fn an_expired_lease_is_reclaimed_on_the_next_poll() {
        let (q, clock) = clocked();
        let w1 = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        // Worker A leases it; while its lease is live, a re-poll is capped.
        assert_eq!(
            q.claim("env_a", "a")
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            w1
        );
        clock.set(1_000);
        assert!(
            q.claim("env_a", "b").await.expect("claim").is_none(),
            "a live lease caps the env"
        );
        clock.set(LEASE_TTL_MS + 1);
        // Worker A goes away (no heartbeat). Past the lease TTL, the next poll
        // reclaims the lapsed lease and re-leases the item instead of blocking.
        let reclaimed = q
            .claim("env_a", "b")
            .await
            .expect("claim query")
            .expect("expired lease reclaimed");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
        clock.set(LEASE_TTL_MS + 2);
        // A heartbeat before expiry keeps the lease alive (no reclaim).
        assert!(
            q.heartbeat("env_a", &w1, "b", LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .into_receipt()
                .expect("hb")
                .lease_extended
        );
        clock.set(LEASE_TTL_MS + 3);
        assert!(
            q.claim("env_a", "c").await.expect("claim").is_none(),
            "the heartbeat kept the lease live"
        );
    }

    /// Cause-effect boundary (BVA on the lease-expiry edge): reclaim fires at
    /// `now_ms >= lease_expiry` because `LeaseBook::is_leased` is a strict
    /// `expiry > now`. So a lapsed worker's item is reclaimable at the *exact* TTL
    /// boundary, not one ms later — pinning the `>`-vs-`>=` off-by-one a refactor of the
    /// in-memory lease kernel could silently flip. Worker A leases
    /// at now=0, so its lease expires at exactly `LEASE_TTL_MS`.
    #[tokio::test]
    async fn lease_reclaim_is_exact_at_the_ttl_boundary() {
        let (q, clock) = clocked();
        let w1 = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        assert_eq!(
            q.claim("env_a", "a")
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            w1
        );
        clock.set(LEASE_TTL_MS - 1);
        // One ms BEFORE expiry: still held (C3 = not-yet-expired → E2 env busy).
        assert!(
            q.claim("env_a", "b").await.expect("claim").is_none(),
            "held until the last ms before expiry"
        );
        clock.set(LEASE_TTL_MS);
        // At the EXACT expiry instant: reclaimable (C3 = expired → E1 reclaim + re-lease).
        let reclaimed = q
            .claim("env_a", "b")
            .await
            .expect("claim query")
            .expect("reclaimed at the exact TTL boundary");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
    }

    #[tokio::test]
    async fn ack_is_fenced_to_the_worker_that_claimed_the_item() {
        /* Cause/effect graph: C1 item is actively leased; C2 caller identity
         * equals the lease owner. Effects: E1 exact owner acknowledgement is
         * accepted and stamped; E2 a different owner is rejected without a
         * state mutation. Rules A1=C1+C2->E1, A2=C1+!C2->E2. */
        let q = q();
        let id = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        q.claim("env_a", "worker-a")
            .await
            .expect("claim query")
            .expect("claim");
        assert!(matches!(
            q.ack("env_a", &id, "worker-b").await.expect("A2"),
            WorkMutationResult::PreconditionFailed
        ));
        let acked = q
            .ack("env_a", &id, "worker-a")
            .await
            .expect("ack query")
            .into_item()
            .expect("acked");
        assert_eq!(acked.state, WorkState::Active);
        assert!(acked.acknowledged_at.is_some());
    }

    #[tokio::test]
    async fn reclaimed_item_rejects_the_preceding_owner_mutations() {
        // R1 old owner + epoch 1 expires; R2 worker-b reclaims under epoch 2.
        // Delayed acknowledgement/stop calls from worker-a must both stutter,
        // leaving the replacement claim active and unacknowledged.
        let (q, clock) = clocked();
        let id = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        q.claim("env_a", "worker-a")
            .await
            .expect("first claim")
            .expect("first work");
        clock.set(LEASE_TTL_MS);
        q.claim("env_a", "worker-b")
            .await
            .expect("replacement claim")
            .expect("replacement work");

        assert!(matches!(
            q.ack("env_a", &id, "worker-a").await.expect("stale ack"),
            WorkMutationResult::PreconditionFailed
        ));
        assert!(matches!(
            q.stop("env_a", &id, "worker-a").await.expect("stale stop"),
            WorkMutationResult::PreconditionFailed
        ));
        let current = q.get("env_a", &id).await.expect("get").expect("work");
        assert_eq!(current.state, WorkState::Active);
        assert!(current.acknowledged_at.is_none());
    }

    #[tokio::test]
    async fn heartbeat_extends_and_reports_ttl() {
        let q = q();
        let id = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        q.claim("env_a", "worker")
            .await
            .expect("claim query")
            .expect("claim");
        let hb = q
            .heartbeat("env_a", &id, "worker", LeaseHeartbeat::unconditional())
            .await
            .expect("heartbeat")
            .into_receipt()
            .expect("heartbeat");
        assert!(hb.lease_extended);
        assert_eq!(hb.ttl_seconds, HEARTBEAT_TTL_SECONDS);
    }

    #[tokio::test]
    async fn membership_is_enforced_across_environments() {
        let q = q();
        let id = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        assert!(
            q.get("env_b", &id).await.expect("get").is_none(),
            "wrong env → none"
        );
        assert!(
            q.ack("env_b", &id, "worker")
                .await
                .expect("ack")
                .is_not_found()
        );
        assert!(
            q.heartbeat("env_b", &id, "worker", LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .is_not_found()
        );
        assert!(
            q.stop("env_b", &id, "worker")
                .await
                .expect("stop")
                .is_not_found()
        );
    }

    #[tokio::test]
    async fn stats_split_queued_depth_from_pending_and_count_pollers() {
        let q = q();
        q.enqueue_healthcheck("env_a").await.expect("enqueue");
        let s = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        // Two queued, no poll yet.
        let st = q.stats("env_a").await.expect("stats");
        assert_eq!(st.depth, 2);
        assert_eq!(st.pending, 0);
        assert_eq!(st.workers_polling, 0);
        assert!(st.oldest_queued_at.is_some());
        // Claim one → depth drops, pending rises, the poller is counted.
        q.claim("env_a", "w1").await.expect("claim");
        let st = q.stats("env_a").await.expect("stats");
        assert_eq!(st.depth, 1);
        assert_eq!(st.pending, 1);
        assert_eq!(st.workers_polling, 1);
        // (touch `s` so the binding is used)
        assert!(q.get("env_a", &s).await.expect("get").is_some());
    }

    #[tokio::test]
    async fn workers_polling_counts_distinct_workers_within_the_window() {
        let (q, clock) = clocked();
        q.enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        // Two distinct workers poll; the same worker polling twice is not double-counted.
        q.claim("env_a", "w1").await.expect("claim");
        clock.set(100);
        q.claim("env_a", "w2").await.expect("claim");
        clock.set(200);
        q.claim("env_a", "w1").await.expect("claim");
        clock.set(300);
        assert_eq!(q.stats("env_a").await.expect("stats").workers_polling, 2);
        clock.set(200 + POLLER_WINDOW_MS + 1);
        // Past the liveness window (w1 last polled at 200), only w2 (100) is stale too.
        assert_eq!(
            q.stats("env_a").await.expect("stats").workers_polling,
            0,
            "pollers age out of the window"
        );
    }

    #[tokio::test]
    async fn oldest_queued_at_persists_while_processing_and_clears_when_drained() {
        let q = q();
        let id = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        // Queued: oldest_queued_at is set.
        assert!(
            q.stats("env_a")
                .await
                .expect("stats")
                .oldest_queued_at
                .is_some()
        );
        // Claim the ONLY queued item → depth 0 but it is still being processed, so
        // oldest_queued_at stays set (queued OR processing), not null.
        q.claim("env_a", "w")
            .await
            .expect("claim query")
            .expect("claim");
        let st = q.stats("env_a").await.expect("stats");
        assert_eq!(st.depth, 0, "nothing queued");
        assert_eq!(st.pending, 1, "one processing");
        assert!(
            st.oldest_queued_at.is_some(),
            "oldest_queued_at persists while an item is still processing"
        );
        // Stop it → the queue is fully drained → null.
        q.stop("env_a", &id, "w")
            .await
            .expect("stop query")
            .into_item()
            .expect("stop");
        assert!(
            q.stats("env_a")
                .await
                .expect("stats")
                .oldest_queued_at
                .is_none(),
            "oldest_queued_at is null only when the queue is fully drained"
        );
    }

    #[tokio::test]
    async fn update_metadata_upserts_and_remove_env_purges() {
        let q = q();
        let id = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        let patch = BTreeMap::from([("k".to_string(), Some("v".to_string()))]);
        let up = q
            .update_metadata("env_a", &id, patch)
            .await
            .expect("update query")
            .expect("patched");
        assert_eq!(up.metadata.get("k").map(String::as_str), Some("v"));
        let deleted = q
            .update_metadata("env_a", &id, BTreeMap::from([("k".to_string(), None)]))
            .await
            .expect("delete query")
            .expect("deleted");
        assert!(!deleted.metadata.contains_key("k"));
        q.remove_env("env_a").await.unwrap();
        assert!(
            q.get("env_a", &id).await.expect("get").is_none(),
            "env delete purges work"
        );
    }

    #[tokio::test]
    async fn session_token_tracks_the_current_in_memory_lease_only() {
        // Cause/effect graph: C1 a Session is claimed under epoch one; C2 the
        // presented token is current/wrong; C3 the lease expires and another
        // owner reclaims. Effects: E1 the current token resolves the exact Work,
        // Session, owner and epoch; E2 wrong tokens fail; E3 reclaim rotates the
        // token and makes epoch one unusable; C4 an epoch-one mutation races the
        // replacement claim. Constraint K1 the queue aggregate locks Work,
        // its existing LeaseBook and token digest together; the digest is only
        // a secret verifier and cannot supply independent lease authority.
        // Decision rows I1=C1+C2(current)->E1, I2=C2(wrong)->E2,
        // I3=C3->E3, I4=C4->PreconditionFailed and epoch two stays live.
        let (q, clock) = clocked();
        let id = q
            .enqueue_session("env", &awaken_session_types::SessionId::from("sesn_1"))
            .await
            .unwrap();
        let first = q
            .claim_with_session_access(
                "env",
                "owner-a",
                "poller",
                None,
                &test_session_access_digest(1),
            )
            .await
            .unwrap()
            .unwrap();
        let first_token = first.session_access.expect("I1 token");
        clock.set(1);
        let first_access = q
            .authenticate_session_access(&first_token)
            .await
            .unwrap()
            .expect("I1 current");
        assert_eq!(first_access.work_id, id);
        assert_eq!(first_access.session_id, "sesn_1");
        assert_eq!(first_access.lease_owner, "owner-a");
        assert!(
            q.authenticate_session_access(&test_session_access_digest(u64::MAX))
                .await
                .unwrap()
                .is_none(),
            "I2"
        );
        clock.set(LEASE_TTL_MS);
        let second = q
            .claim_with_session_access(
                "env",
                "owner-b",
                "poller-b",
                None,
                &test_session_access_digest(2),
            )
            .await
            .unwrap()
            .unwrap();
        let second_token = second.session_access.expect("I3 token");
        assert!(
            q.authenticate_session_access(&first_token)
                .await
                .unwrap()
                .is_none(),
            "I3 stale"
        );
        let second_access = q
            .authenticate_session_access(&second_token)
            .await
            .unwrap()
            .expect("I3 replacement");
        assert_eq!(second_access.lease_epoch.get(), 2);
        assert!(
            matches!(
                q.stop_with_session_access(&first_access).await.unwrap(),
                WorkMutationResult::PreconditionFailed
            ),
            "I4 stale epoch"
        );
        clock.set(LEASE_TTL_MS + 1);
        assert!(
            q.heartbeat_with_session_access(&second_access, LeaseHeartbeat::unconditional())
                .await
                .unwrap()
                .into_receipt()
                .is_some(),
            "I4 current epoch remains live"
        );
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests;

#[cfg(all(test, not(feature = "loom")))]
mod registration_thread_races {
    use super::*;
    use std::sync::{Arc, Barrier};
    #[test]
    fn independent_threads_cannot_publish_an_older_registration_after_withdrawal() {
        for _ in 0..24 {
            let q = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
            let start = Arc::new(Barrier::new(2));
            let tasks: Vec<_> = [false, true]
                .into_iter()
                .map(|newest| {
                    let q = q.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        start.wait();
                        runtime
                            .block_on(q.apply_environment_revision(
                                "thread-race",
                                EnvironmentRevision(if newest { 2 } else { 1 }),
                                if newest {
                                    Operation::Withdraw
                                } else {
                                    Operation::Register { healthcheck: true }
                                },
                            ))
                            .unwrap()
                    })
                })
                .collect();
            for task in tasks {
                task.join().unwrap();
            }
            let authority = q.authority.lock();
            assert!(authority.works.is_empty());
            assert_eq!(
                authority.environment_revisions.get("thread-race"),
                Some(&(EnvironmentRevision(2), Operation::Withdraw))
            );
        }
    }
}
