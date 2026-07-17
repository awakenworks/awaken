//! The self-hosted environment **work queue** as a port, decoupled from the HTTP
//! routes and from the in-memory store.
//!
//! The routes used to hold both the wire projection and the `Mutex<BTreeMap>`
//! store inline. Extracting the [`WorkQueue`] port keeps the wire shape in the
//! routes while letting a durable backend (sqlite / postgres) back the queue at
//! parity for standalone and distributed deployments. The default
//! [`InMemoryWorkQueue`] preserves the exact single-process behavior the routes
//! had: a fresh environment seeds one `healthcheck`; a session assigned to a
//! self-hosted environment enqueues as `session` work; `claim` leases the oldest
//! queued item only when none is active in the environment (the open-tier
//! single-worker cap).
//!
//! This is deliberately **not** merged with the run-ingress `DispatchQueue`
//! (`awaken-run-ingress-contract`): different bounded contexts. `DispatchQueue`
//! dispatches an internal *run* (`RunExecutionRequest` + pending-input inbox/outbox,
//! sandbox binding, supersede-by-epoch, dead-letters) to a db-less worker we own;
//! this `WorkQueue` assigns a *session* to an external, Anthropic-SDK-compatible
//! self-hosted worker (`poll → ack → heartbeat → stop`). Payloads, lease verbs, and
//! protocol faces don't align, so a shared "leased queue" trait would be speculative
//! abstraction. The redundancy the design targeted — the former in-memory *stub* —
//! is gone, replaced by durable sqlite/postgres backends at parity, not by folding
//! two unlike aggregates into one.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::types::environment::{Work, WorkData};

/// The frozen object timestamp the managed wire uses (single-machine builds have
/// no real clock in the *projection*; wire timestamps carry presence, not wall
/// time). Real wall time enters only as the `now_ms` argument the routes pass to
/// the lease/poll bookkeeping — never onto the wire — so the wire shape is
/// unchanged while leases can expire and pollers can be counted.
pub(crate) const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// The lease TTL a heartbeat reports (seconds).
const HEARTBEAT_TTL_SECONDS: u64 = 60;
/// The lease window in ms: an `active` item whose lease last extended more than
/// this ago is no longer held by a live worker and is reclaimable on the next poll.
pub const LEASE_TTL_MS: u64 = HEARTBEAT_TTL_SECONDS * 1000;
/// The liveness window for `workers_polling`: a worker counts as polling if it
/// polled within this many ms of now (the SDK's ~30s window).
pub const POLLER_WINDOW_MS: u64 = 30_000;

/// A work item's lifecycle state. `as_str` is the Anthropic wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Queued,
    Starting,
    Active,
    /// No transition currently produces `Stopping` (`stop` goes straight to
    /// `Stopped`); it is kept because it is Anthropic wire vocabulary a future
    /// writer could emit, and `state_from_wire` must round-trip a `'stopping'` row.
    Stopping,
    Stopped,
}

impl WorkState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }
}

/// One queued/leased unit of work in an environment's queue (the domain shape;
/// [`WorkItem::project`] renders the `BetaSelfHostedWork` wire object).
#[derive(Clone, Debug)]
pub struct WorkItem {
    pub id: String,
    pub environment_id: String,
    pub data: WorkData,
    pub metadata: BTreeMap<String, String>,
    pub state: WorkState,
    pub acknowledged_at: Option<String>,
    pub latest_heartbeat_at: Option<String>,
    pub started_at: Option<String>,
    pub stop_requested_at: Option<String>,
    pub stopped_at: Option<String>,
}

impl WorkItem {
    /// Project to the official `BetaSelfHostedWork` wire shape. `secret` is always
    /// `null` (no per-lease token is minted here).
    #[must_use]
    pub fn project(&self) -> Work {
        Work {
            id: self.id.clone(),
            object_type: "work",
            environment_id: self.environment_id.clone(),
            data: self.data.clone(),
            metadata: self.metadata.clone(),
            state: self.state.as_str(),
            secret: None,
            acknowledged_at: self.acknowledged_at.clone(),
            latest_heartbeat_at: self.latest_heartbeat_at.clone(),
            created_at: OBJECT_AT.to_string(),
            started_at: self.started_at.clone(),
            stop_requested_at: self.stop_requested_at.clone(),
            stopped_at: self.stopped_at.clone(),
        }
    }
}

/// The neutral heartbeat receipt (the port's shape): the lease was extended and
/// its TTL. The route projects this to the wire `WorkHeartbeat` (adding the
/// `object_type` tag), mirroring [`WorkItem::project`].
#[derive(Debug, Clone)]
pub struct LeaseReceipt {
    pub lease_extended: bool,
    pub state: &'static str,
    pub ttl_seconds: u64,
}

/// The neutral queue statistics (the port's shape): depth, in-flight count, the
/// oldest unfinished item's timestamp, and the live poller count. The route
/// projects this to the wire `WorkQueueStats`.
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub depth: usize,
    pub pending: usize,
    pub oldest_queued_at: Option<String>,
    pub workers_polling: i64,
}

/// The port the environments work-queue routes drive. In-memory by default; a
/// durable impl (sqlite / postgres) backs it at parity. Membership is enforced by
/// the port: an operation on a `wid` that does not belong to `env_id` returns
/// `None`, which the route maps to a `work not found` 404.
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Enqueue a `session` work item; returns the new work id.
    async fn enqueue_session(&self, env_id: &str, session_id: &str) -> String;
    /// Seed a `healthcheck` work item (its inner id is the work id); returns it.
    async fn enqueue_healthcheck(&self, env_id: &str) -> String;
    /// All work items in `env_id`, ascending by id (enqueue order).
    async fn list(&self, env_id: &str) -> Vec<WorkItem>;
    /// The work item under `wid` when it belongs to `env_id`.
    async fn get(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Poll as `worker_id` at wall time `now_ms`: first reclaim any `active` item
    /// whose lease has lapsed (its worker went away), then lease the oldest queued
    /// item (queued→active) when none is actively leased. `None` when the queue is
    /// empty or one is still live-leased. The poll is recorded for `workers_polling`.
    async fn claim(&self, env_id: &str, worker_id: &str, now_ms: u64) -> Option<WorkItem>;
    /// Acknowledge receipt (queued→starting), stamping `acknowledged_at`.
    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Record a heartbeat at `now_ms` (extending the lease) and return the TTL receipt.
    async fn heartbeat(&self, env_id: &str, wid: &str, now_ms: u64) -> Option<LeaseReceipt>;
    /// Request a stop (→stopped).
    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Merge a metadata patch (each present key upserts).
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Option<WorkItem>;
    /// Queue stats for `env_id` as of `now_ms` (for the `workers_polling` window).
    async fn stats(&self, env_id: &str, now_ms: u64) -> QueueStats;
    /// Drop all work for `env_id` (on environment delete).
    async fn remove_env(&self, env_id: &str);
}

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

    fn store(&self, id: String, environment_id: &str, data: WorkData) {
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
            WorkData::Session {
                id: session_id.to_string(),
            },
        );
        id
    }

    async fn enqueue_healthcheck(&self, env_id: &str) -> String {
        // A healthcheck's inner id is the work id itself (self-reference).
        let id = self.next_id();
        self.store(id.clone(), env_id, WorkData::HealthCheck { id: id.clone() });
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

    #[tokio::test]
    async fn healthcheck_seed_carries_its_own_id_and_is_queued() {
        let q = q();
        let id = q.enqueue_healthcheck("env_a").await;
        let w = q.get("env_a", &id).await.expect("seeded");
        assert_eq!(w.state, WorkState::Queued);
        assert!(matches!(w.data, WorkData::HealthCheck { id: ref d } if *d == id));
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
