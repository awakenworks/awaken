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

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::types::environment::{Work, WorkData, WorkHeartbeat, WorkQueueStats};

/// The frozen object timestamp the managed wire uses (single-machine builds have
/// no real clock in the projection; timestamps carry presence, not wall time).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// The lease TTL a heartbeat reports.
const HEARTBEAT_TTL_SECONDS: u64 = 60;

/// A work item's lifecycle state. `as_str` is the Anthropic wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Queued,
    Starting,
    Active,
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
    /// Lease the oldest queued item on `env_id` (queued→active) when none is
    /// already active; `None` when the queue is empty or one is active.
    async fn claim(&self, env_id: &str) -> Option<WorkItem>;
    /// Acknowledge receipt (queued→starting), stamping `acknowledged_at`.
    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Record a heartbeat and return the lease's TTL receipt.
    async fn heartbeat(&self, env_id: &str, wid: &str) -> Option<WorkHeartbeat>;
    /// Request a stop (→stopped).
    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Merge a metadata patch (each present key upserts).
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Option<WorkItem>;
    /// Queue stats for `env_id`.
    async fn stats(&self, env_id: &str) -> WorkQueueStats;
    /// Drop all work for `env_id` (on environment delete).
    async fn remove_env(&self, env_id: &str);
}

/// The default single-process work queue: a `BTreeMap` keyed by monotonic work id
/// (ascending id == enqueue order), the exact behavior the routes had inline.
pub struct InMemoryWorkQueue {
    works: Mutex<BTreeMap<String, WorkItem>>,
    seq: AtomicU64,
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
        }
    }

    fn next_id(&self) -> String {
        format!("work_{:016}", self.seq.fetch_add(1, Ordering::SeqCst))
    }

    fn insert(&self, environment_id: &str, data: WorkData) {
        let id = match &data {
            // A healthcheck's inner id is the work id itself.
            WorkData::HealthCheck { id } => id.clone(),
            _ => unreachable!("insert takes an already-identified item"),
        };
        self.store(id, environment_id, data);
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
        let id = self.next_id();
        self.insert(env_id, WorkData::HealthCheck { id: id.clone() });
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

    async fn claim(&self, env_id: &str) -> Option<WorkItem> {
        let mut works = self.works.lock().unwrap();
        // Single active lease per environment (the open-tier single-worker cap).
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

    async fn heartbeat(&self, env_id: &str, wid: &str) -> Option<WorkHeartbeat> {
        self.with_owned(env_id, wid, |w| {
            w.latest_heartbeat_at = Some(OBJECT_AT.to_string());
            WorkHeartbeat {
                object_type: "work_heartbeat",
                last_heartbeat: OBJECT_AT,
                lease_extended: true,
                state: w.state.as_str(),
                ttl_seconds: HEARTBEAT_TTL_SECONDS,
            }
        })
    }

    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
        self.with_owned(env_id, wid, |w| {
            w.stop_requested_at = Some(OBJECT_AT.to_string());
            w.stopped_at = Some(OBJECT_AT.to_string());
            w.state = WorkState::Stopped;
            w.clone()
        })
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

    async fn stats(&self, env_id: &str) -> WorkQueueStats {
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
        let workers_polling = i64::from(in_env.iter().any(|w| w.state == WorkState::Active));
        WorkQueueStats {
            object_type: "work_queue_stats",
            depth: queued,
            pending,
            oldest_queued_at: (queued > 0).then(|| OBJECT_AT.to_string()),
            workers_polling,
        }
    }

    async fn remove_env(&self, env_id: &str) {
        self.works
            .lock()
            .unwrap()
            .retain(|_, w| w.environment_id != env_id);
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
        let leased = q.claim("env_a").await.expect("leases the oldest");
        assert_eq!(leased.id, w1);
        assert_eq!(leased.state, WorkState::Active);
        // A second poll is capped while one is active.
        assert!(q.claim("env_a").await.is_none(), "single active lease");
        // Stopping the active one frees the lease for the next.
        q.stop("env_a", &w1).await.expect("stop");
        assert!(q.claim("env_a").await.is_some(), "next lease after stop");
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
        let hb = q.heartbeat("env_a", &id).await.expect("heartbeat");
        assert!(hb.lease_extended);
        assert_eq!(hb.ttl_seconds, HEARTBEAT_TTL_SECONDS);
    }

    #[tokio::test]
    async fn membership_is_enforced_across_environments() {
        let q = q();
        let id = q.enqueue_session("env_a", "s1").await;
        assert!(q.get("env_b", &id).await.is_none(), "wrong env → none");
        assert!(q.ack("env_b", &id).await.is_none());
        assert!(q.heartbeat("env_b", &id).await.is_none());
        assert!(q.stop("env_b", &id).await.is_none());
    }

    #[tokio::test]
    async fn stats_split_queued_depth_from_pending_and_flag_pollers() {
        let q = q();
        q.enqueue_healthcheck("env_a").await;
        let s = q.enqueue_session("env_a", "s1").await;
        // Two queued, none active.
        let st = q.stats("env_a").await;
        assert_eq!(st.depth, 2);
        assert_eq!(st.pending, 0);
        assert_eq!(st.workers_polling, 0);
        assert!(st.oldest_queued_at.is_some());
        // Claim one → depth drops, pending rises, a poller is flagged.
        q.claim("env_a").await;
        let st = q.stats("env_a").await;
        assert_eq!(st.depth, 1);
        assert_eq!(st.pending, 1);
        assert_eq!(st.workers_polling, 1);
        // (touch `s` so the binding is used)
        assert!(q.get("env_a", &s).await.is_some());
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
