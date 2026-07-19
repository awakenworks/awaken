//! The environment work-queue port + its neutral vocabulary (ADR self-hosted work).
//!
//! The port the environments work-queue routes drive, plus the neutral domain shapes
//! in its signatures. The in-memory reference backend and the shared lease
//! bookkeeping live outward in `awaken-work-store`, beside the sqlite/postgres
//! siblings. The Managed wire adapter owns the neutral→wire projection
//! (`WorkItem` → `BetaSelfHostedWork`); this crate names no wire type.

use std::collections::BTreeMap;

use async_trait::async_trait;

/// The frozen object timestamp the managed wire uses (single-machine builds have
/// no real clock in the *projection*; wire timestamps carry presence, not wall
/// time). Real wall time enters only as the `now_ms` argument the routes pass to
/// the lease/poll bookkeeping — never onto the wire — so the wire shape is
/// unchanged while leases can expire and pollers can be counted.
pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// A work item's payload (the domain shape). The Managed wire adapter maps this onto
/// the tagged `BetaSelfHostedWork.data`; here it names no wire vocabulary. An
/// environment is seeded with a `HealthCheck`; a session assigned to a self-hosted
/// environment is enqueued as `Session` work (its inner `id` is the session id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkPayload {
    HealthCheck { id: String },
    Session { id: String },
}

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

    /// Parse the wire/persisted string back to a state — the exact inverse of
    /// [`as_str`](Self::as_str), co-located here so the two CANNOT drift (adding a variant
    /// forces `as_str` to grow an arm, and this round-trips it). Returns `None` for an
    /// UNRECOGNIZED string rather than silently defaulting: a durable backend reading a
    /// corrupt or newer-schema state must fail CLOSED (treat it as terminal / not
    /// re-dispatchable), because silently mapping an unknown state to `Queued` would invite
    /// a re-claim and double execution — the exactly-once violation this inverse exists to
    /// prevent. See the round-trip property test in this crate.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "starting" => Some(Self::Starting),
            "active" => Some(Self::Active),
            "stopping" => Some(Self::Stopping),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

/// One queued/leased unit of work in an environment's queue (the domain shape; the
/// Managed adapter renders the `BetaSelfHostedWork` wire object from it).
#[derive(Clone, Debug)]
pub struct WorkItem {
    pub id: String,
    pub environment_id: String,
    pub data: WorkPayload,
    pub metadata: BTreeMap<String, String>,
    pub state: WorkState,
    pub acknowledged_at: Option<String>,
    pub latest_heartbeat_at: Option<String>,
    pub started_at: Option<String>,
    pub stop_requested_at: Option<String>,
    pub stopped_at: Option<String>,
}

/// The neutral heartbeat receipt (the port's shape): the lease was extended and
/// its TTL. The route projects this to the wire `WorkHeartbeat` (adding the
/// `object_type` tag).
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
