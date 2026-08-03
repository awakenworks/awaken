//! In-memory reference [`WorkQueue`] backend + the shared lease bookkeeping.
//!
//! [`InMemoryWorkQueue`] is the open-tier single-process default the routes wire when
//! no durable backend is configured; [`LeaseBook`] is its process-local lease model and
//! the poll-liveness bookkeeping used by the durable stores. Durable stores keep lease
//! safety authority in their rows. All three
//! backends live in this crate, beside the durable siblings; the neutral port + value
//! objects they operate on live inward in `awaken-session-contract`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_session_contract::work_queue::{
    HeartbeatResult, LeaseHeartbeat, LeaseReceipt, OBJECT_AT, QueueStats, WorkItem, WorkPayload,
    WorkQueue, WorkQueueError, WorkState,
};

use super::heartbeat_at;

/// The lease TTL a heartbeat reports (seconds).
const HEARTBEAT_TTL_SECONDS: u64 = 60;
/// The lease window in ms: an `active` item whose lease last extended more than
/// this ago is no longer held by a live worker and is reclaimable on the next poll.
pub const LEASE_TTL_MS: u64 = HEARTBEAT_TTL_SECONDS * 1000;
/// The liveness window for `workers_polling`: a worker counts as polling if it
/// polled within this many ms of now (the SDK's ~30s window).
pub const POLLER_WINDOW_MS: u64 = 30_000;
/// Process-local bookkeeping. The in-memory backend uses both maps; durable stores
/// use only `polls`, because their lease owner/epoch/expiry must survive restarts and
/// coordinate across processes in the database.
#[derive(Default)]
pub struct LeaseBook {
    /// work_id → lease refresh/expiry window. Absent ⇒ no live lease.
    leases: Mutex<BTreeMap<String, LeaseWindow>>,
    /// work_id → worker identity that owns the current lease.
    owners: Mutex<BTreeMap<String, String>>,
    /// env_id → (worker_id → last poll ms).
    polls: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
}

#[derive(Clone, Copy)]
struct LeaseWindow {
    refreshed_at_ms: u64,
    expires_at_ms: u64,
}

impl LeaseBook {
    /// Record that `worker_id` polled `env_id` at `now_ms`.
    pub fn record_poll(&self, env_id: &str, worker_id: &str, now_ms: u64) {
        self.polls
            .lock()
            .unwrap()
            .entry(env_id.to_string())
            .or_default()
            .insert(worker_id.to_string(), now_ms);
    }
    /// True while `wid` holds a lease that has not expired as of `now_ms`.
    pub fn is_leased(&self, wid: &str, now_ms: u64) -> bool {
        self.leases
            .lock()
            .unwrap()
            .get(wid)
            .is_some_and(|lease| lease.expires_at_ms > now_ms)
    }
    pub fn is_leased_with_reclaim_age(&self, wid: &str, now_ms: u64, age_ms: u64) -> bool {
        self.leases
            .lock()
            .unwrap()
            .get(wid)
            .is_some_and(|lease| now_ms < lease.refreshed_at_ms.saturating_add(age_ms))
    }
    /// Start/extend `wid`'s lease to `now_ms + LEASE_TTL_MS`.
    pub fn lease(&self, wid: &str, now_ms: u64) {
        self.lease_for(wid, now_ms, LEASE_TTL_MS);
    }

    /// Bind the current lease to the worker that claimed it.
    pub fn own(&self, wid: &str, worker_id: &str) {
        self.owners
            .lock()
            .unwrap()
            .insert(wid.to_string(), worker_id.to_string());
    }

    /// Whether `worker_id` owns `wid`'s current lease.
    pub fn is_owned_by(&self, wid: &str, worker_id: &str) -> bool {
        self.owners
            .lock()
            .unwrap()
            .get(wid)
            .is_some_and(|owner| owner == worker_id)
    }

    /// Start/extend `wid`'s lease by the requested duration.
    pub fn lease_for(&self, wid: &str, now_ms: u64, ttl_ms: u64) {
        self.leases.lock().unwrap().insert(
            wid.to_string(),
            LeaseWindow {
                refreshed_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
        );
    }
    /// Drop `wid`'s lease (on stop / reclaim).
    pub fn release(&self, wid: &str) {
        self.leases.lock().unwrap().remove(wid);
        self.owners.lock().unwrap().remove(wid);
    }
    /// Distinct workers that polled `env_id` within `POLLER_WINDOW_MS` of `now_ms`.
    pub fn workers_polling(&self, env_id: &str, now_ms: u64) -> i64 {
        self.polls
            .lock()
            .unwrap()
            .get(env_id)
            .map(|m| {
                m.values()
                    .filter(|&&t| t + POLLER_WINDOW_MS > now_ms)
                    .count() as i64
            })
            .unwrap_or(0)
    }
    /// Forget an environment's leases + polls (on `remove_env`).
    pub fn forget_env(&self, env_id: &str, work_ids: &[String]) {
        let mut leases = self.leases.lock().unwrap();
        for wid in work_ids {
            leases.remove(wid);
        }
        let mut owners = self.owners.lock().unwrap();
        for wid in work_ids {
            owners.remove(wid);
        }
        self.polls.lock().unwrap().remove(env_id);
    }
}

/// The default single-process work queue: a `BTreeMap` keyed by monotonic work id
/// (ascending id == enqueue order), the exact behavior the routes had inline.
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
}

#[async_trait]
impl WorkQueue for InMemoryWorkQueue {
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        let id = self.next_id();
        self.store(
            id.clone(),
            env_id,
            WorkPayload::Session {
                id: session_id.to_string(),
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
        let w = works.get_mut(&wid).expect("just found");
        w.state = WorkState::Active;
        w.started_at = Some(OBJECT_AT.to_string());
        w.latest_heartbeat_at = None;
        self.book.lease(&wid, now_ms);
        self.book.own(&wid, worker_id);
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
    async fn ack(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        Ok(self.with_owned(env_id, wid, |w| {
            w.acknowledged_at = Some(OBJECT_AT.to_string());
            w.state = w.state.after_ack();
            w.clone()
        }))
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

    async fn stop(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError> {
        let Some(out) = self.with_owned(env_id, wid, |w| {
            w.stop_requested_at = Some(OBJECT_AT.to_string());
            w.stopped_at = Some(OBJECT_AT.to_string());
            w.state = w.state.after_stop();
            w.clone()
        }) else {
            return Ok(None);
        };
        self.book.release(wid);
        Ok(Some(out))
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
        let q = q();
        let id = q.enqueue_healthcheck("env_a").await.expect("enqueue");
        let w = q.get("env_a", &id).await.expect("get").expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkPayload::HealthCheck { id: ref d } if *d == id));
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
        q.stop("env_a", &w1)
            .await
            .expect("stop query")
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
    async fn ack_transitions_queued_to_starting_and_stamps() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await.expect("enqueue");
        let acked = q
            .ack("env_a", &id)
            .await
            .expect("ack query")
            .expect("acked");
        assert_eq!(acked.state, WorkState::Starting);
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
        assert!(q.ack("env_b", &id).await.expect("ack").is_none());
        assert!(
            q.heartbeat("env_b", &id, "worker", 0, LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .is_not_found()
        );
        assert!(q.stop("env_b", &id).await.expect("stop").is_none());
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
        q.stop("env_a", &id)
            .await
            .expect("stop query")
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
