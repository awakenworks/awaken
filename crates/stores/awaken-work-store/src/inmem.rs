//! In-memory reference [`WorkQueue`] backend.
//!
//! This executable specification is available only to unit tests and consumers that
//! explicitly enable `test-support`. Product composition must select SQLite or
//! PostgreSQL so accepted work survives process loss.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_session_contract::work_queue::{
    HeartbeatResult, LeaseHeartbeat, LeaseReceipt, OBJECT_AT, QueueStats, SessionWorkLease,
    WorkItem, WorkMutationResult, WorkPayload, WorkQueue, WorkQueueError, WorkState,
};

use super::{LeaseBook, heartbeat_at};

/// The lease TTL a heartbeat reports (seconds).
const HEARTBEAT_TTL_SECONDS: u64 = 60;

/// Test-support single-process reference queue: a `BTreeMap` keyed by monotonic
/// work id (ascending id == enqueue order).
pub struct InMemoryWorkQueue {
    works: Mutex<BTreeMap<String, WorkItem>>,
    seq: AtomicU64,
    book: LeaseBook,
}

impl Default for InMemoryWorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryWorkQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            works: Mutex::new(BTreeMap::new()),
            seq: AtomicU64::new(0),
            book: LeaseBook::default(),
        }
    }

    fn next_id(&self) -> String {
        format!("work_{:016}", self.seq.fetch_add(1, Ordering::SeqCst))
    }

    fn store(&self, id: String, environment_id: &str, data: WorkPayload) {
        self.works.lock().unwrap().insert(
            id.clone(),
            WorkItem {
                id,
                environment_id: environment_id.to_string(),
                data,
                metadata: BTreeMap::new(),
                state: WorkState::Queued,
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
    }

    /// Run `f` on the item under `wid` only when it belongs to `env_id`.
    fn with_owned<R>(
        &self,
        env_id: &str,
        wid: &str,
        f: impl FnOnce(&mut WorkItem) -> R,
    ) -> Option<R> {
        let mut works = self.works.lock().unwrap();
        match works.get_mut(wid) {
            Some(w) if w.environment_id == env_id => Some(f(w)),
            _ => None,
        }
    }

    fn current_session_lease(
        &self,
        env_id: &str,
        session_id: &str,
        now_ms: u64,
    ) -> Option<SessionWorkLease> {
        let work = self
            .works
            .lock()
            .unwrap()
            .values()
            .find(|work| {
                work.environment_id == env_id
                    && work.state == WorkState::Active
                    && matches!(&work.data, WorkPayload::Session { id } if id == session_id)
            })
            .cloned()?;
        self.book
            .authority(&work.id, now_ms)
            .map(|(owner, epoch, expires_at_unix_ms)| SessionWorkLease {
                work_id: work.id,
                environment_id: env_id.to_string(),
                session_id: session_id.to_string(),
                owner,
                epoch,
                expires_at_unix_ms,
            })
    }
}

#[async_trait]
impl WorkQueue for InMemoryWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        let mut works = self.works.lock().unwrap();
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
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
        Ok(id)
    }

    async fn wake_session(&self, env_id: &str, session_id: &str) -> Result<String, WorkQueueError> {
        let mut works = self.works.lock().unwrap();
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
                self.book.release(&existing.id);
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
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
        Ok(id)
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        // A healthcheck's inner id is the work id itself (self-reference).
        let id = self.next_id();
        self.store(
            id.clone(),
            env_id,
            WorkPayload::HealthCheck { id: id.clone() },
        );
        Ok(id)
    }

    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
        let mut works = self.works.lock().unwrap();
        if let Some(existing) = works
            .values()
            .find(|work| {
                work.environment_id == env_id
                    && matches!(work.data, WorkPayload::HealthCheck { .. })
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
                data: WorkPayload::HealthCheck { id: id.clone() },
                metadata: BTreeMap::new(),
                state: WorkState::Queued,
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            },
        );
        Ok(id)
    }

    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
        Ok(self
            .works
            .lock()
            .unwrap()
            .values()
            .filter(|w| w.environment_id == env_id)
            .cloned()
            .collect())
    }

    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self.with_owned(env_id, wid, |w| w.clone()))
    }

    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.book.record_poll(env_id, worker_id, now_ms);
        let mut works = self.works.lock().unwrap();
        for (wid, w) in works.iter_mut() {
            if w.environment_id == env_id
                && w.state == WorkState::Active
                && !self.book.is_leased(wid, now_ms)
            {
                w.state = WorkState::Queued;
                w.latest_heartbeat_at = None;
                self.book.release(wid);
            }
        }
        if works
            .values()
            .any(|w| w.environment_id == env_id && w.state == WorkState::Active)
        {
            return Ok(None);
        }
        let Some(wid) = works
            .iter()
            .filter(|(_, w)| w.environment_id == env_id && w.state.is_claimable())
            .map(|(id, _)| id.clone())
            .min()
        else {
            return Ok(None);
        };
        self.book
            .own(&wid, worker_id)
            .map_err(|error| WorkQueueError::Storage(error.into()))?;
        let w = works.get_mut(&wid).expect("just found");
        w.state = WorkState::Active;
        w.started_at = Some(OBJECT_AT.to_string());
        w.latest_heartbeat_at = None;
        self.book.lease(&wid, now_ms);
        Ok(Some(w.clone()))
    }

    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        if let Some(age) = reclaim_older_than_ms {
            let mut works = self.works.lock().unwrap();
            for (wid, w) in works.iter_mut() {
                if w.environment_id == env_id
                    && w.state == WorkState::Active
                    && !self.book.is_leased_with_reclaim_age(wid, now_ms, age)
                {
                    w.state = WorkState::Queued;
                    w.latest_heartbeat_at = None;
                    self.book.release(wid);
                }
            }
        }
        self.claim(env_id, worker_id, now_ms).await
    }
    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        if !self.book.is_owned_by(wid, worker_id) {
            return Ok(if self.with_owned(env_id, wid, |_| ()).is_some() {
                WorkMutationResult::PreconditionFailed
            } else {
                WorkMutationResult::NotFound
            });
        }
        Ok(self
            .with_owned(env_id, wid, |w| {
                w.acknowledged_at = Some(OBJECT_AT.to_string());
                w.state = w.state.after_ack();
                WorkMutationResult::accepted(w.clone())
            })
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
        Ok(self
            .with_owned(env_id, wid, |w| {
                if !self.book.is_owned_by(wid, worker_id) {
                    return HeartbeatResult::PreconditionFailed;
                }
                if !heartbeat
                    .condition
                    .permits(w.latest_heartbeat_at.as_deref())
                {
                    return HeartbeatResult::PreconditionFailed;
                }
                let extended = w.state.can_extend_lease();
                let last_heartbeat = heartbeat_at(now_ms, w.latest_heartbeat_at.as_deref());
                if extended {
                    w.latest_heartbeat_at = Some(last_heartbeat.clone());
                    let ttl = heartbeat
                        .desired_ttl_seconds
                        .unwrap_or(HEARTBEAT_TTL_SECONDS)
                        .max(1);
                    self.book.lease_for(wid, now_ms, ttl.saturating_mul(1000));
                }
                HeartbeatResult::Accepted(LeaseReceipt {
                    last_heartbeat,
                    lease_extended: extended,
                    state: w.state.as_str(),
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
        if !self.book.is_owned_by(wid, worker_id) {
            return Ok(if self.with_owned(env_id, wid, |_| ()).is_some() {
                WorkMutationResult::PreconditionFailed
            } else {
                WorkMutationResult::NotFound
            });
        }
        let Some(out) = self.with_owned(env_id, wid, |w| {
            w.stop_requested_at = Some(OBJECT_AT.to_string());
            w.stopped_at = Some(OBJECT_AT.to_string());
            w.state = w.state.after_stop();
            w.clone()
        }) else {
            return Ok(WorkMutationResult::NotFound);
        };
        self.book.release(wid);
        Ok(WorkMutationResult::accepted(out))
    }

    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let wid = self
            .works
            .lock()
            .unwrap()
            .values()
            .find(|work| {
                work.environment_id == env_id
                    && matches!(&work.data, WorkPayload::Session { id } if id == session_id)
            })
            .map(|work| work.id.clone());
        let Some(wid) = wid else {
            return Ok(None);
        };
        let item = self.with_owned(env_id, &wid, |work| {
            work.stop_requested_at = Some(OBJECT_AT.to_string());
            work.stopped_at = Some(OBJECT_AT.to_string());
            work.state = WorkState::Stopped;
            work.clone()
        });
        self.book.release(&wid);
        Ok(item)
    }

    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError> {
        if let Some(lease) = self.current_session_lease(env_id, session_id, now_ms) {
            if lease.owner != worker_owner {
                return Ok(None);
            }
            self.book.lease(&lease.work_id, now_ms);
            return Ok(self.current_session_lease(env_id, session_id, now_ms));
        }
        let work_id = self.enqueue_session(env_id, session_id).await?;
        {
            let mut works = self.works.lock().unwrap();
            for (id, work) in works.iter_mut() {
                if work.environment_id == env_id
                    && work.state == WorkState::Active
                    && !self.book.is_leased(id, now_ms)
                {
                    work.state = WorkState::Queued;
                    work.latest_heartbeat_at = None;
                    self.book.release(id);
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
            self.book
                .own(&work_id, worker_owner)
                .map_err(|error| WorkQueueError::Storage(error.into()))?;
            work.state = WorkState::Active;
            work.started_at = Some(OBJECT_AT.to_string());
            work.latest_heartbeat_at = None;
        }
        self.book.lease(&work_id, now_ms);
        Ok(self.current_session_lease(env_id, session_id, now_ms))
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self.with_owned(env_id, wid, |w| {
            w.metadata.extend(patch);
            w.clone()
        }))
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError> {
        let works = self.works.lock().unwrap();
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
        let workers_polling = self.book.workers_polling(env_id, now_ms);
        // `oldest_queued_at` is the oldest item still QUEUED or being PROCESSED (the
        // SDK's semantics), so it stays set once a worker claims the last queued item —
        // not just while `depth > 0`. Only a fully drained queue (all stopped) is null.
        let has_unfinished = queued > 0 || pending > 0;
        Ok(QueueStats {
            depth: queued,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling,
        })
    }

    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
        let mut works = self.works.lock().unwrap();
        let ids: Vec<String> = works
            .values()
            .filter(|w| w.environment_id == env_id)
            .map(|w| w.id.clone())
            .collect();
        works.retain(|_, w| w.environment_id != env_id);
        self.book.forget_env(env_id, &ids);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LEASE_TTL_MS, POLLER_WINDOW_MS};

    fn q() -> InMemoryWorkQueue {
        InMemoryWorkQueue::new()
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
            .enqueue_session("env_a", "session_a")
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
            .enqueue_session("env_a", "session_a")
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
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let _w2 = q.enqueue_session("env_a", "s2").await.expect("enqueue");
        let leased = q
            .claim("env_a", "w", 0)
            .await
            .expect("claim query")
            .expect("leases the oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        // A second poll is capped while one is live-leased.
        assert!(
            q.claim("env_a", "w", 0).await.expect("claim").is_none(),
            "single active lease"
        );
        // Stopping the active one frees the lease for the next.
        q.stop("env_a", &w1, "w")
            .await
            .expect("stop query")
            .into_item()
            .expect("stop");
        assert!(
            q.claim("env_a", "w", 0).await.expect("claim").is_some(),
            "next lease after stop"
        );
    }

    #[tokio::test]
    async fn an_expired_lease_is_reclaimed_on_the_next_poll() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        // Worker A leases it; while its lease is live, a re-poll is capped.
        assert_eq!(
            q.claim("env_a", "a", 0)
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            w1
        );
        assert!(
            q.claim("env_a", "b", 1_000).await.expect("claim").is_none(),
            "a live lease caps the env"
        );
        // Worker A goes away (no heartbeat). Past the lease TTL, the next poll
        // reclaims the lapsed lease and re-leases the item instead of blocking.
        let reclaimed = q
            .claim("env_a", "b", LEASE_TTL_MS + 1)
            .await
            .expect("claim query")
            .expect("expired lease reclaimed");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
        // A heartbeat before expiry keeps the lease alive (no reclaim).
        assert!(
            q.heartbeat(
                "env_a",
                &w1,
                "b",
                LEASE_TTL_MS + 2,
                LeaseHeartbeat::unconditional(),
            )
            .await
            .expect("heartbeat")
            .into_receipt()
            .expect("hb")
            .lease_extended
        );
        assert!(
            q.claim("env_a", "c", LEASE_TTL_MS + 3)
                .await
                .expect("claim")
                .is_none(),
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
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        assert_eq!(
            q.claim("env_a", "a", 0)
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            w1
        );
        // One ms BEFORE expiry: still held (C3 = not-yet-expired → E2 env busy).
        assert!(
            q.claim("env_a", "b", LEASE_TTL_MS - 1)
                .await
                .expect("claim")
                .is_none(),
            "held until the last ms before expiry"
        );
        // At the EXACT expiry instant: reclaimable (C3 = expired → E1 reclaim + re-lease).
        let reclaimed = q
            .claim("env_a", "b", LEASE_TTL_MS)
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
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        q.claim("env_a", "worker-a", 0)
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
    async fn heartbeat_extends_and_reports_ttl() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        q.claim("env_a", "worker", 0)
            .await
            .expect("claim query")
            .expect("claim");
        let hb = q
            .heartbeat("env_a", &id, "worker", 0, LeaseHeartbeat::unconditional())
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
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
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
            q.heartbeat("env_b", &id, "worker", 0, LeaseHeartbeat::unconditional())
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
        let s = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        // Two queued, no poll yet.
        let st = q.stats("env_a", 0).await.expect("stats");
        assert_eq!(st.depth, 2);
        assert_eq!(st.pending, 0);
        assert_eq!(st.workers_polling, 0);
        assert!(st.oldest_queued_at.is_some());
        // Claim one → depth drops, pending rises, the poller is counted.
        q.claim("env_a", "w1", 0).await.expect("claim");
        let st = q.stats("env_a", 0).await.expect("stats");
        assert_eq!(st.depth, 1);
        assert_eq!(st.pending, 1);
        assert_eq!(st.workers_polling, 1);
        // (touch `s` so the binding is used)
        assert!(q.get("env_a", &s).await.expect("get").is_some());
    }

    #[tokio::test]
    async fn workers_polling_counts_distinct_workers_within_the_window() {
        let q = q();
        q.enqueue_session("env_a", "s1").await.expect("enqueue");
        // Two distinct workers poll; the same worker polling twice is not double-counted.
        q.claim("env_a", "w1", 0).await.expect("claim");
        q.claim("env_a", "w2", 100).await.expect("claim");
        q.claim("env_a", "w1", 200).await.expect("claim");
        assert_eq!(
            q.stats("env_a", 300).await.expect("stats").workers_polling,
            2
        );
        // Past the liveness window (w1 last polled at 200), only w2 (100) is stale too.
        assert_eq!(
            q.stats("env_a", 200 + POLLER_WINDOW_MS + 1)
                .await
                .expect("stats")
                .workers_polling,
            0,
            "pollers age out of the window"
        );
    }

    #[tokio::test]
    async fn oldest_queued_at_persists_while_processing_and_clears_when_drained() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        // Queued: oldest_queued_at is set.
        assert!(
            q.stats("env_a", 0)
                .await
                .expect("stats")
                .oldest_queued_at
                .is_some()
        );
        // Claim the ONLY queued item → depth 0 but it is still being processed, so
        // oldest_queued_at stays set (queued OR processing), not null.
        q.claim("env_a", "w", 0)
            .await
            .expect("claim query")
            .expect("claim");
        let st = q.stats("env_a", 0).await.expect("stats");
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
            q.stats("env_a", 0)
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
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let patch = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let up = q
            .update_metadata("env_a", &id, patch)
            .await
            .expect("update query")
            .expect("patched");
        assert_eq!(up.metadata.get("k").map(String::as_str), Some("v"));
        q.remove_env("env_a").await.unwrap();
        assert!(
            q.get("env_a", &id).await.expect("get").is_none(),
            "env delete purges work"
        );
    }
}
