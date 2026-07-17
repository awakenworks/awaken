//! In-memory reference [`WorkQueue`] backend + the shared lease bookkeeping.
//!
//! [`InMemoryWorkQueue`] is the open-tier single-process default the routes wire when
//! no durable backend is configured; [`LeaseBook`] is the process-local reclaim + poll
//! bookkeeping the sqlite/postgres backends share, so the three can't drift. All three
//! backends live in this crate, beside the durable siblings; the neutral port + value
//! objects they operate on live inward in `awaken-session-contract`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_session_contract::work_queue::{
    LeaseReceipt, OBJECT_AT, QueueStats, WorkItem, WorkPayload, WorkQueue, WorkState,
};

/// The lease TTL a heartbeat reports (seconds).
const HEARTBEAT_TTL_SECONDS: u64 = 60;
/// The lease window in ms: an `active` item whose lease last extended more than
/// this ago is no longer held by a live worker and is reclaimable on the next poll.
pub const LEASE_TTL_MS: u64 = HEARTBEAT_TTL_SECONDS * 1000;
/// The liveness window for `workers_polling`: a worker counts as polling if it
/// polled within this many ms of now (the SDK's ~30s window).
pub const POLLER_WINDOW_MS: u64 = 30_000;
/// Ephemeral lease + poll bookkeeping shared by every backend. Leases and poll
/// liveness are inherently short-lived (a lease is held by a heartbeating worker;
/// a poll counts for ~30s) and meaningless after a restart, so they live in
/// process — no durable column — keeping the wire and the schema unchanged. This
/// is the single source of the reclaim + `workers_polling` logic, so the three
/// backends can't drift.
#[derive(Default)]
pub struct LeaseBook {
    /// work_id → lease expiry (ms). Absent ⇒ no live lease (reclaimable).
    leases: Mutex<BTreeMap<String, u64>>,
    /// env_id → (worker_id → last poll ms).
    polls: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
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
            .is_some_and(|exp| *exp > now_ms)
    }
    /// Start/extend `wid`'s lease to `now_ms + LEASE_TTL_MS`.
    pub fn lease(&self, wid: &str, now_ms: u64) {
        self.leases
            .lock()
            .unwrap()
            .insert(wid.to_string(), now_ms + LEASE_TTL_MS);
    }
    /// Drop `wid`'s lease (on stop / reclaim).
    pub fn release(&self, wid: &str) {
        self.leases.lock().unwrap().remove(wid);
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
    async fn enqueue_session(&self, env_id: &str, session_id: &str) -> String {
        let id = self.next_id();
        self.store(
            id.clone(),
            env_id,
            WorkPayload::Session {
                id: session_id.to_string(),
            },
        );
        id
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> String {
        // A healthcheck's inner id is the work id itself (self-reference).
        let id = self.next_id();
        self.store(
            id.clone(),
            env_id,
            WorkPayload::HealthCheck { id: id.clone() },
        );
        id
    }

    async fn list(&self, env_id: &str) -> Vec<WorkItem> {
        self.works
            .lock()
            .unwrap()
            .values()
            .filter(|w| w.environment_id == env_id)
            .cloned()
            .collect()
    }

    async fn get(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        self.with_owned(env_id, wid, |w| w.clone())
    }

    async fn claim(&self, env_id: &str, worker_id: &str, now_ms: u64) -> Option<WorkItem> {
        self.book.record_poll(env_id, worker_id, now_ms);
        let mut works = self.works.lock().unwrap();
        // Reclaim: an `active` item whose lease has lapsed (a worker that stopped
        // heartbeating, e.g. crashed) is no longer held — return it to `queued` so
        // this poll can re-lease it instead of the env blocking forever.
        for (wid, w) in works.iter_mut() {
            if w.environment_id == env_id
                && w.state == WorkState::Active
                && !self.book.is_leased(wid, now_ms)
            {
                w.state = WorkState::Queued;
                self.book.release(wid);
            }
        }
        // Single active lease per environment (the open-tier single-worker cap):
        // after reclaim, only a live-leased item counts.
        if works
            .values()
            .any(|w| w.environment_id == env_id && w.state == WorkState::Active)
        {
            return None;
        }
        // Lease the oldest queued item (ascending id == enqueue order).
        let wid = works
            .iter()
            .filter(|(_, w)| w.environment_id == env_id && w.state == WorkState::Queued)
            .map(|(id, _)| id.clone())
            .min()?;
        let w = works.get_mut(&wid).expect("just found");
        w.state = WorkState::Active;
        w.started_at = Some(OBJECT_AT.to_string());
        self.book.lease(&wid, now_ms);
        Some(w.clone())
    }

    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        self.with_owned(env_id, wid, |w| {
            w.acknowledged_at = Some(OBJECT_AT.to_string());
            if w.state == WorkState::Queued {
                w.state = WorkState::Starting;
            }
            w.clone()
        })
    }

    async fn heartbeat(&self, env_id: &str, wid: &str, now_ms: u64) -> Option<LeaseReceipt> {
        let hb = self.with_owned(env_id, wid, |w| {
            w.latest_heartbeat_at = Some(OBJECT_AT.to_string());
            LeaseReceipt {
                lease_extended: true,
                state: w.state.as_str(),
                ttl_seconds: HEARTBEAT_TTL_SECONDS,
            }
        })?;
        self.book.lease(wid, now_ms); // extend the lease
        Some(hb)
    }

    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        let out = self.with_owned(env_id, wid, |w| {
            w.stop_requested_at = Some(OBJECT_AT.to_string());
            w.stopped_at = Some(OBJECT_AT.to_string());
            w.state = WorkState::Stopped;
            w.clone()
        })?;
        self.book.release(wid);
        Some(out)
    }

    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Option<WorkItem> {
        self.with_owned(env_id, wid, |w| {
            w.metadata.extend(patch);
            w.clone()
        })
    }

    async fn stats(&self, env_id: &str, now_ms: u64) -> QueueStats {
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
        QueueStats {
            depth: queued,
            pending,
            oldest_queued_at: has_unfinished.then(|| OBJECT_AT.to_string()),
            workers_polling,
        }
    }

    async fn remove_env(&self, env_id: &str) {
        let mut works = self.works.lock().unwrap();
        let ids: Vec<String> = works
            .values()
            .filter(|w| w.environment_id == env_id)
            .map(|w| w.id.clone())
            .collect();
        works.retain(|_, w| w.environment_id != env_id);
        self.book.forget_env(env_id, &ids);
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
        let id = q.enqueue_healthcheck("env_a").await;
        let w = q.get("env_a", &id).await.expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkPayload::HealthCheck { id: ref d } if *d == id));
    }

    #[tokio::test]
    async fn claim_leases_oldest_queued_and_caps_at_one_active() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await;
        let _w2 = q.enqueue_session("env_a", "s2").await;
        let leased = q.claim("env_a", "w", 0).await.expect("leases the oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        // A second poll is capped while one is live-leased.
        assert!(
            q.claim("env_a", "w", 0).await.is_none(),
            "single active lease"
        );
        // Stopping the active one frees the lease for the next.
        q.stop("env_a", &w1).await.expect("stop");
        assert!(
            q.claim("env_a", "w", 0).await.is_some(),
            "next lease after stop"
        );
    }

    #[tokio::test]
    async fn an_expired_lease_is_reclaimed_on_the_next_poll() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await;
        // Worker A leases it; while its lease is live, a re-poll is capped.
        assert_eq!(q.claim("env_a", "a", 0).await.expect("lease").id, w1);
        assert!(
            q.claim("env_a", "b", 1_000).await.is_none(),
            "a live lease caps the env"
        );
        // Worker A goes away (no heartbeat). Past the lease TTL, the next poll
        // reclaims the lapsed lease and re-leases the item instead of blocking.
        let reclaimed = q
            .claim("env_a", "b", LEASE_TTL_MS + 1)
            .await
            .expect("expired lease reclaimed");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
        // A heartbeat before expiry keeps the lease alive (no reclaim).
        assert!(
            q.heartbeat("env_a", &w1, LEASE_TTL_MS + 2)
                .await
                .expect("hb")
                .lease_extended
        );
        assert!(
            q.claim("env_a", "c", LEASE_TTL_MS + 3).await.is_none(),
            "the heartbeat kept the lease live"
        );
    }

    /// Cause-effect boundary (BVA on the lease-expiry edge): reclaim fires at
    /// `now_ms >= lease_expiry` because the shared `LeaseBook::is_leased` is a strict
    /// `expiry > now`. So a lapsed worker's item is reclaimable at the *exact* TTL
    /// boundary, not one ms later — pinning the `>`-vs-`>=` off-by-one a refactor of the
    /// LeaseBook shared across all three backends could silently flip. Worker A leases
    /// at now=0, so its lease expires at exactly `LEASE_TTL_MS`.
    #[tokio::test]
    async fn lease_reclaim_is_exact_at_the_ttl_boundary() {
        let q = q();
        let w1 = q.enqueue_session("env_a", "s1").await;
        assert_eq!(q.claim("env_a", "a", 0).await.expect("lease").id, w1);
        // One ms BEFORE expiry: still held (C3 = not-yet-expired → E2 env busy).
        assert!(
            q.claim("env_a", "b", LEASE_TTL_MS - 1).await.is_none(),
            "held until the last ms before expiry"
        );
        // At the EXACT expiry instant: reclaimable (C3 = expired → E1 reclaim + re-lease).
        let reclaimed = q
            .claim("env_a", "b", LEASE_TTL_MS)
            .await
            .expect("reclaimed at the exact TTL boundary");
        assert_eq!(reclaimed.id, w1);
        assert_eq!(reclaimed.state, WorkState::Active);
    }

    #[tokio::test]
    async fn ack_transitions_queued_to_starting_and_stamps() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        let acked = q.ack("env_a", &id).await.expect("acked");
        assert_eq!(acked.state, WorkState::Starting);
        assert!(acked.acknowledged_at.is_some());
    }

    #[tokio::test]
    async fn heartbeat_extends_and_reports_ttl() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        let hb = q.heartbeat("env_a", &id, 0).await.expect("heartbeat");
        assert!(hb.lease_extended);
        assert_eq!(hb.ttl_seconds, HEARTBEAT_TTL_SECONDS);
    }

    #[tokio::test]
    async fn membership_is_enforced_across_environments() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        assert!(q.get("env_b", &id).await.is_none(), "wrong env → none");
        assert!(q.ack("env_b", &id).await.is_none());
        assert!(q.heartbeat("env_b", &id, 0).await.is_none());
        assert!(q.stop("env_b", &id).await.is_none());
    }

    #[tokio::test]
    async fn stats_split_queued_depth_from_pending_and_count_pollers() {
        let q = q();
        q.enqueue_healthcheck("env_a").await;
        let s = q.enqueue_session("env_a", "s1").await;
        // Two queued, no poll yet.
        let st = q.stats("env_a", 0).await;
        assert_eq!(st.depth, 2);
        assert_eq!(st.pending, 0);
        assert_eq!(st.workers_polling, 0);
        assert!(st.oldest_queued_at.is_some());
        // Claim one → depth drops, pending rises, the poller is counted.
        q.claim("env_a", "w1", 0).await;
        let st = q.stats("env_a", 0).await;
        assert_eq!(st.depth, 1);
        assert_eq!(st.pending, 1);
        assert_eq!(st.workers_polling, 1);
        // (touch `s` so the binding is used)
        assert!(q.get("env_a", &s).await.is_some());
    }

    #[tokio::test]
    async fn workers_polling_counts_distinct_workers_within_the_window() {
        let q = q();
        q.enqueue_session("env_a", "s1").await;
        // Two distinct workers poll; the same worker polling twice is not double-counted.
        q.claim("env_a", "w1", 0).await;
        q.claim("env_a", "w2", 100).await;
        q.claim("env_a", "w1", 200).await;
        assert_eq!(q.stats("env_a", 300).await.workers_polling, 2);
        // Past the liveness window (w1 last polled at 200), only w2 (100) is stale too.
        assert_eq!(
            q.stats("env_a", 200 + POLLER_WINDOW_MS + 1)
                .await
                .workers_polling,
            0,
            "pollers age out of the window"
        );
    }

    #[tokio::test]
    async fn oldest_queued_at_persists_while_processing_and_clears_when_drained() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        // Queued: oldest_queued_at is set.
        assert!(q.stats("env_a", 0).await.oldest_queued_at.is_some());
        // Claim the ONLY queued item → depth 0 but it is still being processed, so
        // oldest_queued_at stays set (queued OR processing), not null.
        q.claim("env_a", "w", 0).await.expect("claim");
        let st = q.stats("env_a", 0).await;
        assert_eq!(st.depth, 0, "nothing queued");
        assert_eq!(st.pending, 1, "one processing");
        assert!(
            st.oldest_queued_at.is_some(),
            "oldest_queued_at persists while an item is still processing"
        );
        // Stop it → the queue is fully drained → null.
        q.stop("env_a", &id).await.expect("stop");
        assert!(
            q.stats("env_a", 0).await.oldest_queued_at.is_none(),
            "oldest_queued_at is null only when the queue is fully drained"
        );
    }

    #[tokio::test]
    async fn update_metadata_upserts_and_remove_env_purges() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        let patch = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let up = q
            .update_metadata("env_a", &id, patch)
            .await
            .expect("patched");
        assert_eq!(up.metadata.get("k").map(String::as_str), Some("v"));
        q.remove_env("env_a").await;
        assert!(
            q.get("env_a", &id).await.is_none(),
            "env delete purges work"
        );
    }
}
