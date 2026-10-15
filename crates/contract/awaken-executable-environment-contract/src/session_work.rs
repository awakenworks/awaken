// @formal entrypoint session_time.utc_projection_domain_is_total_exact_and_idempotent
// @formal entrypoint session_work.release_requires_exact_owner_and_epoch
// @formal entrypoint work.heartbeat_compare_and_set
// @formal entrypoint work.pre_heartbeat_worker_identity
// @formal entrypoint work.single_active_claim
// @formal entrypoint work.terminal_lease_clear
//! Environment-owned work coordination and its neutral vocabulary (ADR self-hosted work).
//!
//! The port the environments work-queue routes drive, plus the neutral domain shapes
//! in its signatures. The in-memory reference backend and the shared lease
//! bookkeeping live outward in `awaken-work-store`, beside the sqlite/postgres
//! siblings. The Managed wire adapter owns the neutral→wire projection
//! (`WorkItem` → `BetaSelfHostedWork`); this module names no wire type.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_lease_contract::fence::Epoch;
#[cfg(test)]
use awaken_session_application_contract::SessionWorkOwnership;
use awaken_session_types::SessionWorkLease;
use awaken_session_types::{SessionContractErrorDetail, SessionId};

// The UTC calendar codec is owned by the wall-clock port, so a Work receipt and
// an Environment record spell the same reading identically.
#[cfg(kani)]
use awaken_time::utc::bounded_rfc3339_nanos;
use awaken_time::utc::{MAX_RFC3339_NANOS, format_rfc3339_utc_nanos, parse_rfc3339_utc_nanos};
#[cfg(test)]
use awaken_time::utc::{civil_from_days, days_from_civil, days_in_month};

/// Produce the opaque RFC-3339 receipt returned by a successful heartbeat.
///
/// Repeated writes in the same millisecond still receive distinct tokens, and a
/// wall clock that moves backwards cannot repeat an earlier receipt. Keeping this
/// pure conversion in the neutral queue contract lets every backend share the
/// exact compare-and-set vocabulary without importing a clock/date crate.
#[must_use]
pub fn next_heartbeat_receipt(now_ms: u64, previous: Option<&str>) -> String {
    const NANOS_PER_MILLI: i128 = 1_000_000;

    let requested = i128::from(now_ms)
        .saturating_mul(NANOS_PER_MILLI)
        .min(MAX_RFC3339_NANOS);
    let previous = previous.and_then(parse_rfc3339_utc_nanos);
    let next = match previous {
        Some(previous) if requested <= previous => previous.saturating_add(1),
        _ => requested,
    }
    .min(MAX_RFC3339_NANOS);
    format_rfc3339_utc_nanos(next)
}

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

    /// Whether a poll may acquire this item.
    #[must_use]
    pub const fn is_claimable(self) -> bool {
        matches!(self, Self::Queued)
    }

    /// State after acknowledging receipt. An acknowledgement before a poll is a
    /// protocol-compatible `Queued -> Starting` transition; normal workers poll
    /// first, so acknowledging `Active` is an idempotent state no-op.
    #[must_use]
    pub const fn after_ack(self) -> Self {
        if matches!(self, Self::Queued) {
            Self::Starting
        } else {
            self
        }
    }

    /// Only the currently leased state can extend lease authority. A heartbeat
    /// may still return the current queued/starting/stopped state, but it must not
    /// manufacture a lease for an item that was never claimed.
    #[must_use]
    pub const fn can_extend_lease(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Stop is absorbing at the work-item boundary.
    #[must_use]
    pub const fn after_stop(self) -> Self {
        Self::Stopped
    }
}

/// Storage-neutral result of admitting an owner/epoch guarded Work mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkLeaseMutationAdmission {
    Applied,
    NotFound,
    PreconditionFailed,
}

/// One closed authorization table for Work acknowledgement, heartbeat, and
/// stop adapters. A backend obtains all facts from its existing atomic lock or
/// transaction, then this kernel decides whether any write may occur.
#[must_use]
pub const fn work_lease_mutation_admission(
    record_present: bool,
    owner_and_epoch_match: bool,
    command_condition_matches: bool,
) -> WorkLeaseMutationAdmission {
    if !record_present {
        WorkLeaseMutationAdmission::NotFound
    } else if !owner_and_epoch_match || !command_condition_matches {
        WorkLeaseMutationAdmission::PreconditionFailed
    } else {
        WorkLeaseMutationAdmission::Applied
    }
}

/// Storage-neutral plan for settling one exact Session Work lease. Durable
/// adapters obtain these facts from one locked row; the reference adapter
/// obtains owner, epoch, and lease presence from one LeaseBook snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWorkReleaseAdmission {
    Apply,
    AlreadyReleased,
    Reject,
}

#[must_use]
pub const fn session_work_release_admission(
    identity_matches: bool,
    state_is_active: bool,
    state_is_stopped: bool,
    owner_matches: bool,
    epoch_matches: bool,
    authority_is_cleared: bool,
) -> SessionWorkReleaseAdmission {
    if identity_matches && state_is_active && owner_matches && epoch_matches {
        SessionWorkReleaseAdmission::Apply
    } else if identity_matches && state_is_stopped && epoch_matches && authority_is_cleared {
        SessionWorkReleaseAdmission::AlreadyReleased
    } else {
        SessionWorkReleaseAdmission::Reject
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
    /// When the queue accepted this item, read from the store's injected
    /// [`WallClock`](awaken_time::WallClock) inside the enqueue
    /// transaction. It is a durable column, not a projection: the wire value a
    /// client reads is the instant the row was written.
    pub created_at: String,
    pub acknowledged_at: Option<String>,
    pub latest_heartbeat_at: Option<String>,
    pub started_at: Option<String>,
    pub stop_requested_at: Option<String>,
    pub stopped_at: Option<String>,
}

/// Digest of one per-Session Work bearer: 64 lowercase hexadecimal characters.
///
/// The WorkQueue persists and compares only this value. Generating the bearer
/// and deriving its digest belong to the issuing application, so the queue port
/// never handles cleartext credential material.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionAccessDigest(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Session access digest must be 64 lowercase hexadecimal characters")]
pub struct InvalidSessionAccessDigest;

impl SessionAccessDigest {
    pub fn from_hex(hex: impl Into<String>) -> Result<Self, InvalidSessionAccessDigest> {
        let hex = hex.into();
        if hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self(hex))
        } else {
            Err(InvalidSessionAccessDigest)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Queue-side result of a Session-access claim: the leased item and, only when
/// that item is Session work, the pre-minted digest the same transaction bound
/// to the new lease epoch.
#[derive(Clone, Debug)]
pub struct WorkClaim {
    pub item: WorkItem,
    pub session_access: Option<SessionAccessDigest>,
}

/// A freshly claimed Work item plus the one-time per-Session bearer whose
/// digest the authoritative WorkQueue transaction bound to the lease. The
/// cleartext is returned only by `poll`; list/retrieve projections continue to
/// expose `secret: null`.
#[derive(Clone, Debug)]
pub struct ClaimedWork {
    pub item: WorkItem,
    pub sessions_token: Option<RedactedString>,
}

/// Current Work lease recovered from a presented per-Session bearer.
///
/// The token is not a Worker identity or a second lease. This projection
/// carries the exact owner/epoch already persisted by the WorkQueue so protocol
/// adapters can authorize downstream Session calls and map heartbeat/stop back
/// onto the same lease owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkSessionAccess {
    pub work_id: String,
    pub environment_id: String,
    pub session_id: SessionId,
    pub lease_owner: String,
    /// The exact generation the bearer was bound to at claim time.
    pub lease_epoch: Epoch,
    pub expires_at_unix_ms: u64,
}

/// Result of a mutation that only the current work owner may perform.
#[derive(Clone, Debug)]
pub enum WorkMutationResult {
    Accepted(Box<WorkItem>),
    PreconditionFailed,
    NotFound,
}

impl WorkMutationResult {
    #[must_use]
    pub fn accepted(work: WorkItem) -> Self {
        Self::Accepted(Box::new(work))
    }

    #[must_use]
    pub fn into_item(self) -> Option<WorkItem> {
        match self {
            Self::Accepted(item) => Some(*item),
            Self::PreconditionFailed | Self::NotFound => None,
        }
    }

    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }

    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

/// Optimistic concurrency condition supplied by a worker heartbeat.
///
/// The Managed wire uses `NO_HEARTBEAT` for the first heartbeat and then asks
/// the worker to echo the preceding receipt exactly. Keeping that vocabulary at
/// the adapter edge while representing its meaning here makes every store use
/// the same comparison rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeartbeatCondition {
    /// Backward-compatible/manual calls which did not request a conditional write.
    Unconditional,
    /// The item must not have recorded any heartbeat in its current lease.
    First,
    /// The latest heartbeat must equal this opaque receipt exactly.
    Matching(String),
}

impl HeartbeatCondition {
    /// Whether `actual` authorizes this heartbeat write.
    #[must_use]
    pub fn permits(&self, actual: Option<&str>) -> bool {
        match self {
            Self::Unconditional => true,
            Self::First => actual.is_none(),
            Self::Matching(expected) => actual == Some(expected.as_str()),
        }
    }
}

/// One heartbeat mutation request. `desired_ttl_seconds` is optional on the
/// Managed wire; the store applies its documented default when absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHeartbeat {
    pub condition: HeartbeatCondition,
    pub desired_ttl_seconds: Option<u64>,
}

impl LeaseHeartbeat {
    #[must_use]
    pub const fn unconditional() -> Self {
        Self {
            condition: HeartbeatCondition::Unconditional,
            desired_ttl_seconds: None,
        }
    }
}

/// The neutral heartbeat receipt (the port's shape): the lease was extended and
/// its TTL. The route projects this to the wire `WorkHeartbeat` (adding the
/// `object_type` tag).
#[derive(Debug, Clone)]
pub struct LeaseReceipt {
    /// Opaque RFC-3339 compare token to echo on the next heartbeat.
    pub last_heartbeat: String,
    pub lease_extended: bool,
    pub state: WorkState,
    pub ttl_seconds: u64,
}

/// Result of an atomic heartbeat compare-and-extend operation.
#[derive(Debug, Clone)]
pub enum HeartbeatResult {
    Accepted(LeaseReceipt),
    PreconditionFailed,
    NotFound,
}

impl HeartbeatResult {
    #[must_use]
    pub fn into_receipt(self) -> Option<LeaseReceipt> {
        match self {
            Self::Accepted(receipt) => Some(receipt),
            Self::PreconditionFailed | Self::NotFound => None,
        }
    }

    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }
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

/// Storage failures stay in Environment; Session ports translate at their application adapter.
///
/// ```compile_fail,E0277
/// use awaken_executable_environment_contract::WorkQueueError;
/// fn no_implicit_session_conversion(error: WorkQueueError) -> awaken_session_types::RunError {
///     error.into()
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkQueueError {
    #[error("work queue storage failure: {0}")]
    Storage(SessionContractErrorDetail),
}

/// Read-only Work items and queue observations. No operation grants or renews authority.
#[async_trait]
pub trait WorkQueries: Send + Sync {
    /// All work items in `env_id`, ascending by id (enqueue order).
    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError>;
    /// The work item under `wid` when it belongs to `env_id`.
    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Queue stats for `env_id` as of `now_ms` (for the `workers_polling` window).
    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError>;
}

/// Environment-credential Worker polling and owner mutations. Session access is bound atomically at claim.
#[async_trait]
pub trait WorkConsumer: Send + Sync {
    /// Poll as `worker_id` at wall time `now_ms`: first reclaim any `active` item
    /// whose lease has lapsed (its worker went away), then lease the oldest queued
    /// item (queued→active) when none is actively leased. `None` when the queue is
    /// empty or one is still live-leased. The poll is recorded for `workers_polling`.
    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Claim with an optional caller-requested reclaim age, recording the poll
    /// under `poller_id`.
    ///
    /// Required, with no default. The default this replaced dropped both
    /// qualifiers and delegated to [`Self::claim`]: a backend that did not
    /// override it accepted a reclaim age and never applied it, and never
    /// recorded the poll, so `workers_polling` undercounted and a lapsed lease
    /// outlived the age its caller asked for — while the call reported success.
    /// Every backend implements it, so there is nothing for a default to buy.
    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Claim through the public self-hosted Worker protocol. When the selected
    /// item is Session work, the caller's pre-minted `session_access` digest is
    /// persisted in the same transaction that writes the new lease epoch and is
    /// echoed in [`WorkClaim::session_access`]; otherwise it is discarded. The
    /// queue never generates or sees the bearer itself.
    ///
    /// Required, without a default: a backend must atomically bind the digest
    /// or explicitly refuse the claim. The historical default dropped `session_access`
    /// and returned the claim anyway: the caller had already minted a bearer and
    /// was told the claim succeeded, while the digest it must be compared
    /// against was never persisted — every later request under that bearer then
    /// failed authentication against a lease that looked healthy. An
    /// implementation that cannot bind the digest must say so, not hand back a
    /// lease that silently has none.
    async fn claim_with_session_access(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
        session_access: &SessionAccessDigest,
    ) -> Result<Option<WorkClaim>, WorkQueueError>;
    /// Acknowledge receipt (queued→starting), stamping `acknowledged_at`.
    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError>;
    /// Atomically compare the preceding heartbeat and, when it matches, record a
    /// new heartbeat at `now_ms` and extend the lease.
    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError>;
    /// Request a stop (→stopped).
    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError>;
}

/// Resolve a bearer digest against a live lease. This port grants no mutation capability.
#[async_trait]
pub trait SessionAccessAuthenticator: Send + Sync {
    /// Resolve one bearer digest against the current live Work lease. Expired,
    /// reclaimed, stopped, retired, or replaced leases return `None`.
    async fn authenticate_session_access(
        &self,
        _presented: &SessionAccessDigest,
        _now_ms: u64,
    ) -> Result<Option<WorkSessionAccess>, WorkQueueError>;
}

/// Mutate only the exact epoch authenticated by a Session bearer. Never fall back to an owner-only check.
#[async_trait]
pub trait SessionWorkLeaseControl: Send + Sync {
    /// Apply an acknowledgement only while the exact lease epoch recovered
    /// from a sessions token is still current. Each backend must implement
    /// this exact-epoch fence explicitly.
    async fn ack_with_session_access(
        &self,
        _access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError>;
    /// Extend only the exact lease epoch recovered from a sessions token.
    async fn heartbeat_with_session_access(
        &self,
        _access: &WorkSessionAccess,
        _now_ms: u64,
        _heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError>;
    /// Stop only the exact lease epoch recovered from a sessions token.
    async fn stop_with_session_access(
        &self,
        _access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError>;
}

/// Session lifecycle projection into its Environment queue: enqueue, explicitly wake, or retire.
#[async_trait]
pub trait SessionWorkStore: Send + Sync {
    /// Ensure one `session` work item exists for `(env_id, session_id)` and
    /// return its canonical work id. Replays are idempotent so a durable Session
    /// reconciliation loop can safely close a commit-before-dispatch crash gap.
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
    ) -> Result<String, WorkQueueError>;
    /// Explicitly wake an existing stopped Session work item, or create it when
    /// absent. Unlike reconciliation's [`Self::enqueue_session`], this command
    /// is driven only by a new admitted Session event, so a completed idle item
    /// cannot be continuously resurrected by the background supervisor.
    async fn wake_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
    ) -> Result<String, WorkQueueError>;
    /// Coordinator-owned terminal projection. This is not a Worker mutation:
    /// it retires the canonical item after the Session terminal fence and
    /// therefore intentionally does not require the former lease owner.
    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
}

/// Registered Worker lease storage. The Session application alone decides whether Work is required or may revive.
#[async_trait]
pub trait SessionWorkerLeaseStore: Send + Sync {
    /// Registered-Worker authority teardown. Only active Session items with the
    /// exact incarnation-qualified owner are returned to the queue, so a live
    /// non-terminal Session can be adopted by a fresh incarnation.
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError>;
    /// Registered-Worker settlement transition. Retire only the exact current
    /// Session Work owner and lease epoch in one store compare-and-set; a stale
    /// request must never retire a successor lease from the same incarnation.
    async fn release_session(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError>;
    /// Private registered-Worker adapter over the same WorkQueue authority.
    /// It claims the exact queued Session or renews that exact current owner.
    /// When the exact Session is actively held by another owner, return that
    /// current lease so the caller can distinguish stale ownership from a
    /// retired or capacity-blocked item. The Environment single-active cap and
    /// non-claimable states otherwise fail closed with `None`.
    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError>;
}

/// Read-only persisted owner occupancy, including expired but unreleased claims.
#[async_trait]
pub trait WorkerLeaseObservation: Send + Sync {
    /// Whether any persisted Work lease still names this exact owner. Expiry is
    /// not release: this read never reclaims, clears or extends lease authority.
    /// It is occupancy evidence only, never process-exit or replay permission.
    async fn has_owner_claims(&self, worker_owner: &str) -> Result<bool, WorkQueueError>;
}

/// Patch Work metadata: Some(value) upserts, None removes. Lease and lifecycle fields are outside this capability.
#[async_trait]
pub trait WorkMetadataPatch: Send + Sync {
    /// Merge a metadata patch (a string upserts and None deletes the key).
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
}

/// Environment registration convergence and withdrawal cleanup. Kept out of Worker and query roles.
#[async_trait]
pub trait EnvironmentWorkLifecycle: Send + Sync {
    /// Seed a `healthcheck` work item (its inner id is the work id); returns it.
    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError>;
    /// Return the one healthcheck for an Environment, creating it when absent.
    /// This convergence operation is safe to replay after an Environment create
    /// committed but the caller lost the response.
    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError>;
    /// Drop all work for `env_id` (on environment delete).
    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError>;
}

#[cfg(test)]
mod tests {
    use super::{
        SessionWorkLease, SessionWorkOwnership, SessionWorkReleaseAdmission,
        WorkLeaseMutationAdmission, next_heartbeat_receipt, session_work_release_admission,
        work_lease_mutation_admission,
    };

    /// A backend that does not bind the digest refuses the claim.
    ///
    /// Decision table for an explicit `claim_with_session_access` refusal. Causes:
    /// D1 an adapter that cannot bind a digest; D2 an unqualified call on that
    /// implementation that binds the digest. Effects: E1 a refusal naming the
    /// missing capability; E2 a claim whose `session_access` carries the exact
    /// digest presented. Rules D1 => E1, D2 => E2.
    ///
    /// E1 is the conversion: the replaced default answered `Ok(Some(claim))`
    /// with `session_access: None`, so a caller that had already minted a bearer
    /// was told its claim succeeded while nothing to compare that bearer against
    /// had been written.
    #[tokio::test]
    async fn a_backend_that_cannot_bind_the_digest_refuses_the_claim() {
        use super::*;
        use std::collections::BTreeMap;

        struct Unbinding;

        #[async_trait::async_trait]
        impl WorkConsumer for Unbinding {
            async fn claim_with_session_access(
                &self,
                env_id: &str,
                lease_owner: &str,
                poller_id: &str,
                now_ms: u64,
                reclaim_older_than_ms: Option<u64>,
                session_access: &SessionAccessDigest,
            ) -> Result<Option<WorkClaim>, WorkQueueError> {
                let _ = (
                    env_id,
                    lease_owner,
                    poller_id,
                    now_ms,
                    reclaim_older_than_ms,
                    session_access,
                );
                Err(WorkQueueError::Storage(
                    "this WorkQueue backend cannot bind a Session-access digest to a claim".into(),
                ))
            }

            async fn claim(
                &self,
                _: &str,
                _: &str,
                _: u64,
            ) -> Result<Option<WorkItem>, WorkQueueError> {
                Ok(Some(item()))
            }
            async fn claim_with_reclaim(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: u64,
                _: Option<u64>,
            ) -> Result<Option<WorkItem>, WorkQueueError> {
                Ok(Some(item()))
            }
            async fn ack(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<WorkMutationResult, WorkQueueError> {
                Ok(WorkMutationResult::NotFound)
            }
            async fn heartbeat(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: u64,
                _: LeaseHeartbeat,
            ) -> Result<HeartbeatResult, WorkQueueError> {
                Ok(HeartbeatResult::NotFound)
            }
            async fn stop(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<WorkMutationResult, WorkQueueError> {
                Ok(WorkMutationResult::NotFound)
            }
        }

        fn item() -> WorkItem {
            WorkItem {
                id: "work_1".into(),
                environment_id: "env".into(),
                data: WorkPayload::Session {
                    id: "sesn_1".into(),
                },
                metadata: BTreeMap::new(),
                state: WorkState::Active,
                created_at: "2026-02-03T04:05:06.007Z".into(),
                acknowledged_at: None,
                latest_heartbeat_at: None,
                started_at: None,
                stop_requested_at: None,
                stopped_at: None,
            }
        }

        let digest = SessionAccessDigest::from_hex("a".repeat(64)).expect("D1 digest");
        let refused = Unbinding
            .claim_with_session_access("env", "owner", "poller", 0, None, &digest)
            .await;
        let Err(WorkQueueError::Storage(detail)) = refused else {
            panic!("D1/E1: the adapter must refuse, not hand back a digest-less claim");
        };
        assert!(
            detail.to_string().contains("Session-access digest"),
            "D1/E1 the refusal names the missing capability, got {detail:?}"
        );
        // D2/E2: the unqualified sibling still answers, so the refusal is about
        // the qualifier and not about claiming at all.
        assert!(
            Unbinding
                .claim_with_reclaim("env", "owner", "poller", 0, None)
                .await
                .expect("D2")
                .is_some(),
            "D2/E2"
        );
    }

    #[test]
    fn session_work_effect_admission_follows_the_complete_authority_table() {
        // Cause/effect graph: C1 the frozen Session requires no external Work;
        // C2 required Work is absent/retired or leased; C3 the observed lease
        // names the authenticated Worker. Effects: E1 permit C1 and exact
        // C2+C3; E2 deny absent Work and a foreign owner. The four rows cover
        // every semantic observation produced by SessionWorkerLeaseAuthority;
        // lease identity/epoch validity remains inside that authoritative
        // queue read.
        //
        // | Rule | Work observation | Owner | Permitted |
        // |---|---|---|---|
        // | A1 | NotRequired | any | yes |
        // | A2 | Unowned | any | no |
        // | A3 | Leased | exact | yes |
        // | A4 | Leased | foreign | no |
        let lease = |owner: &str| {
            SessionWorkLease::from_persisted("work", "environment", "session".into(), owner, 7, 11)
                .unwrap()
        };
        for (rule, ownership, expected) in [
            ("A1", SessionWorkOwnership::NotRequired, true),
            ("A2", SessionWorkOwnership::Unowned, false),
            ("A3", SessionWorkOwnership::Leased(lease("worker")), true),
            ("A4", SessionWorkOwnership::Leased(lease("foreign")), false),
        ] {
            assert_eq!(ownership.permits_effect_for("worker"), expected, "{rule}");
        }
    }

    #[test]
    fn utc_calendar_codec_covers_every_representable_day() {
        // D6 exhaustive 1970..9999 day-domain coverage. Check both calendar
        // directions, real month lengths, and dense successive dates. Golden
        // Unix date anchors live in run_application::tests to avoid a solely
        // self-consistent round-trip oracle.
        for day in 0..=2_932_896 {
            let (year, month, date) = super::civil_from_days(day);
            assert!((1970..=9999).contains(&year));
            assert!((1..=12).contains(&month));
            assert!((1..=super::days_in_month(year, month)).contains(&date));
            assert_eq!(super::days_from_civil(year, month, date), day);
        }
        assert_eq!(super::civil_from_days(2_932_896), (9999, 12, 31));
    }

    #[test]
    fn heartbeat_receipts_are_rfc3339_and_advance_within_one_millisecond() {
        let first = next_heartbeat_receipt(0, None);
        let second = next_heartbeat_receipt(0, Some(&first));
        assert_eq!(first, "1970-01-01T00:00:00.000000000Z");
        assert_eq!(second, "1970-01-01T00:00:00.000000001Z");
    }

    #[test]
    fn heartbeat_receipts_do_not_go_backwards_with_the_wall_clock() {
        let previous = "2026-01-01T00:00:00.999999999Z";
        assert_eq!(
            next_heartbeat_receipt(0, Some(previous)),
            "2026-01-01T00:00:01.000000000Z"
        );
    }

    #[test]
    fn heartbeat_receipts_accept_the_old_seconds_only_shape() {
        assert_eq!(
            next_heartbeat_receipt(0, Some("2026-01-01T00:00:00Z")),
            "2026-01-01T00:00:00.000000001Z"
        );
    }

    #[test]
    fn lease_mutation_admission_follows_the_complete_decision_table() {
        // Causes: C1 the Work row exists; C2 exact owner+epoch matches; C3 the
        // command CAS condition matches. Effects: E1 only C1+C2+C3 mutates;
        // E2 absence is distinguishable; E3 every stale fact fails unchanged.
        // Decision rules are the complete 2^3 input table below.
        for present in [false, true] {
            for authority in [false, true] {
                for condition in [false, true] {
                    let actual = work_lease_mutation_admission(present, authority, condition);
                    let expected = if !present {
                        WorkLeaseMutationAdmission::NotFound
                    } else if authority && condition {
                        WorkLeaseMutationAdmission::Applied
                    } else {
                        WorkLeaseMutationAdmission::PreconditionFailed
                    };
                    assert_eq!(
                        actual, expected,
                        "C1={present} C2={authority} C3={condition}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_work_release_follows_the_complete_authority_table() {
        // Causes: C1 exact Work/Environment/Session identity; C2 Active or
        // Stopped state; C3 exact owner; C4 exact epoch; C5 authority cleared.
        // Effects: E1 Apply only for exact active owner+epoch; E2 idempotent
        // success only for exact stopped epoch with no owner/lease; E3 every
        // other combination rejects unchanged. This exhausts all 2^6 rules.
        for identity in [false, true] {
            for active in [false, true] {
                for stopped in [false, true] {
                    for owner in [false, true] {
                        for epoch in [false, true] {
                            for cleared in [false, true] {
                                let actual = session_work_release_admission(
                                    identity, active, stopped, owner, epoch, cleared,
                                );
                                let expected = if identity && active && owner && epoch {
                                    SessionWorkReleaseAdmission::Apply
                                } else if identity && stopped && epoch && cleared {
                                    SessionWorkReleaseAdmission::AlreadyReleased
                                } else {
                                    SessionWorkReleaseAdmission::Reject
                                };
                                assert_eq!(actual, expected);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_state(tag: u8) -> WorkState {
        match tag % 5 {
            0 => WorkState::Queued,
            1 => WorkState::Starting,
            2 => WorkState::Active,
            3 => WorkState::Stopping,
            _ => WorkState::Stopped,
        }
    }

    /// @obligation work.single_active_claim
    #[kani::proof]
    fn only_queued_work_is_claimable() {
        let state = symbolic_state(kani::any());
        assert_eq!(state.is_claimable(), state == WorkState::Queued);
    }

    /// @obligation work.heartbeat_compare_and_set
    #[kani::proof]
    fn only_active_work_accepts_lease_extension() {
        let state = symbolic_state(kani::any());
        assert_eq!(state.can_extend_lease(), state == WorkState::Active);
    }

    /// @obligation work.heartbeat_compare_and_set
    /// @proof-input-debt free_symbol work_lease_mutation_admission owner_and_epoch_match
    #[kani::proof]
    fn work_lease_mutation_requires_present_exact_authority_and_matching_condition() {
        let present = kani::any::<bool>();
        let authority = kani::any::<bool>();
        let condition = kani::any::<bool>();
        let decision = work_lease_mutation_admission(present, authority, condition);
        assert_eq!(
            decision == WorkLeaseMutationAdmission::Applied,
            present && authority && condition
        );
        assert_eq!(decision == WorkLeaseMutationAdmission::NotFound, !present);
    }

    /// @obligation session_work.release_requires_exact_owner_and_epoch
    /// @proof-input-debt free_symbol session_work_release_admission authority_is_cleared
    /// @proof-input-debt free_symbol session_work_release_admission epoch_matches
    /// @proof-input-debt free_symbol session_work_release_admission identity_matches
    /// @proof-input-debt free_symbol session_work_release_admission owner_matches
    /// @proof-input-debt free_symbol session_work_release_admission state_is_active
    /// @proof-input-debt free_symbol session_work_release_admission state_is_stopped
    #[kani::proof]
    fn session_work_release_requires_exact_owner_and_epoch() {
        // Same complete C1-C5/E1-E3 table as the native test. The asserted
        // formula is independent of the production decision implementation.
        let identity: bool = kani::any();
        let active: bool = kani::any();
        let stopped: bool = kani::any();
        let owner: bool = kani::any();
        let epoch: bool = kani::any();
        let cleared: bool = kani::any();
        kani::assume(!(active && stopped));
        let decision =
            session_work_release_admission(identity, active, stopped, owner, epoch, cleared);
        assert_eq!(
            decision == SessionWorkReleaseAdmission::Apply,
            identity && active && owner && epoch
        );
        assert_eq!(
            decision == SessionWorkReleaseAdmission::AlreadyReleased,
            identity && stopped && epoch && cleared
        );
    }

    /// @obligation work.terminal_lease_clear
    #[kani::proof]
    fn stop_is_absorbing_for_every_work_state() {
        let state = symbolic_state(kani::any());
        assert_eq!(state.after_stop(), WorkState::Stopped);
        assert_eq!(state.after_stop().after_stop(), WorkState::Stopped);
    }

    /// @obligation work.pre_heartbeat_worker_identity
    #[kani::proof]
    fn first_heartbeat_is_authorized_exactly_once() {
        let first = HeartbeatCondition::First;
        assert!(first.permits(None));
        assert!(!first.permits(Some("already-recorded")));
    }

    /// @obligation work.heartbeat_compare_and_set
    #[kani::proof]
    fn matching_heartbeat_rejects_every_other_receipt() {
        let matching = HeartbeatCondition::Matching("expected".into());
        assert!(matching.permits(Some("expected")));
        assert!(!matching.permits(None));
        assert!(!matching.permits(Some("stale")));
    }
}

#[cfg(kani)]
/// @obligation session_time.utc_projection_domain_is_total_exact_and_idempotent
#[kani::proof]
fn session_utc_projection_domain_is_total_exact_and_idempotent() {
    // D1 negative -> epoch; D2 legal -> exact; D3 overflow -> last UTC instant.
    // Arbitrary i128 exhausts both boundaries. This proves the production
    // projection clamp, not the OS clock, calendar codec, or database adapter.
    let input: i128 = kani::any();
    let value = bounded_rfc3339_nanos(input);
    assert!((0..=MAX_RFC3339_NANOS).contains(&value));
    assert_eq!(bounded_rfc3339_nanos(value), value);
    if input < 0 {
        assert_eq!(value, 0);
    } else if input > MAX_RFC3339_NANOS {
        assert_eq!(value, MAX_RFC3339_NANOS);
    } else {
        assert_eq!(value, input);
    }
}
