//! The durable-ingress store ports and their data.
//!
//! Two aggregates, per the run-ingress DDD split:
//!
//! - [`DispatchQueue`] owns *delivery opportunity* — enqueue, single-owner claim,
//!   lease, settle, and lease-expiry recovery. It never owns message payload
//!   truth or a run's outcome; those are the thread and run aggregates, read back
//!   from committed facts.
//! - [`Inbox`] owns the *thread's pending input* — idempotent append of
//!   delivered-but-unconsumed input, frozen once at a safe run boundary.
//!
//! One concrete store implements both (so a wake can freeze pending input inside
//! the same claim transaction), but the traits stay split so neither aggregate
//! reaches into the other's invariants. A blanket [`Dispatch`] bundles them
//! for the worker and host.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;
use serde::{Deserialize, Serialize};

use crate::request::RunExecutionRequest;

/// A durable-store failure. Commit-time agent truth uses the commit coordinator's
/// own error; this is only the dispatch queue's own storage failure.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("dispatch store rejected: {0}")]
    Rejected(String),
}

/// One unit of durable input delivered to a thread, awaiting consumption. Keyed
/// by a stable `message_id` so an at-least-once delivery appends exactly once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInput {
    pub message_id: String,
    pub run_id: RunId,
    pub thread_id: ThreadId,
    /// The waiting-ticket correlation this input answers. Consumption is keyed to
    /// it: the worker delivers an input only while the committed ticket still
    /// carries the same correlation, so a resume that already committed (and
    /// advanced or cleared the ticket) is never re-applied (ADR-0010).
    pub correlation_id: String,
    /// Earliest delivery time (epoch millis). `None` is deliverable immediately;
    /// a future time schedules the wake — the claim skips it until it is due and
    /// the daemon's poll fires it when the clock reaches it (ADR-0014).
    #[serde(default)]
    pub available_at_ms: Option<u64>,
    /// What this input delivers back into the parked run on resume.
    pub result: ResumeResult,
}

/// A lease over one claimed dispatch: the single owner allowed to execute this
/// run until `expires_ms`. An expired lease is reclaimable (recovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub run_id: RunId,
    pub owner: String,
    pub expires_ms: u64,
    /// Monotonic fence token, bumped on every claim of this run (fresh, wake, or
    /// recovery). The holder passes it back on [`settle`](DispatchQueue::settle) so
    /// a *stale* owner — one whose lease lapsed and was re-claimed by another node
    /// under a higher epoch — cannot settle the dispatch out from under the current
    /// owner: the store rejects a settle whose epoch is not the row's current one.
    /// This is the canonical fencing token (Kleppmann): owner strings can collide
    /// or be reused, but a monotone epoch cannot, so the fence is topology- and
    /// owner-name-independent. A row that has never been claimed has epoch 0.
    #[serde(default)]
    pub epoch: u64,
}

/// A claimed, ready-to-run dispatch and the run's undelivered pending input.
/// `pending` is empty for a fresh run and non-empty for a wake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claimed {
    pub request: RunExecutionRequest,
    pub lease: Lease,
    /// The run's undelivered pending input. The worker decides execute-vs-resume
    /// from committed truth (the waiting ticket), not from this field, and tells
    /// `settle` which inputs it consumed.
    pub pending: Vec<PendingInput>,
    /// The sandbox this run is bound to for its lifetime (B-P3, ADR-0021 §6), as an
    /// **opaque** reference — the dispatch aggregate stays neutral (it names no
    /// provisioning type); the fleet serializes a `SandboxHandle` into it and
    /// parses it back on adoption. `None` until the run is placed on a sandbox.
    /// Durable so crash recovery (`reconcile_adoption`) can re-adopt the same
    /// sandbox instead of leaking it.
    pub sandbox: Option<String>,
}

/// How a claimed attempt resolved. Settled atomically with releasing the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DispatchOutcome {
    /// The run reached a terminus; the dispatch is finished and removed.
    Done,
    /// The run parked on a waiting ticket; keep the dispatch for a later wake.
    Parked,
}

/// Whether a [`settle`](DispatchQueue::settle) was applied or fenced off as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettleOutcome {
    /// The caller still held the current lease (its epoch matched the row's); the
    /// settle was applied.
    Applied,
    /// The caller's lease epoch is stale — the run was re-claimed under a higher
    /// epoch (another node recovered the lapsed lease). NOTHING was changed, so the
    /// current owner's in-flight dispatch is untouched. The stale caller has lost
    /// the lease and must abandon the run.
    Fenced,
}

impl SettleOutcome {
    /// Whether the settle was applied (vs. fenced off as a stale owner's).
    pub fn applied(self) -> bool {
        matches!(self, SettleOutcome::Applied)
    }
}

/// The lifecycle status of a dispatch, for the operational query surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchStatus {
    /// Fresh, not yet claimed.
    Pending,
    /// Claimed and executing under a lease.
    Running,
    /// Parked on a committed waiting ticket.
    Parked,
    /// Dead-lettered past its crash-retry budget (ADR-0015).
    DeadLetter,
    /// Superseded by a newer submission on its thread (ADR-0022).
    Superseded,
}

impl DispatchStatus {
    /// Map the stored status text (the SQL backends' `status` column) to the
    /// public status. An unknown value maps to `Pending` (never observed).
    pub fn from_db(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "parked" => Self::Parked,
            "dead_letter" => Self::DeadLetter,
            "superseded" => Self::Superseded,
            _ => Self::Pending,
        }
    }
}

/// An operational view of one dispatch row, for monitoring and maintenance — the
/// `DispatchQueue` query role (ADR-0025). Carries no live handle, just committed
/// queue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchSummary {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    pub status: DispatchStatus,
    /// Consecutive crash-recoveries without a settle.
    pub attempt_count: u64,
}

/// Dispatch-level options for an accepted run. Defaults to ordinary priority and
/// no caller dedupe key — `enqueue` uses these so existing callers are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitOptions {
    /// Higher runs first among fresh (not-yet-started) work; default 0.
    pub priority: i64,
    /// A caller idempotency key. While a dispatch with this key is live, another
    /// enqueue carrying it is a no-op — dedup beyond the run id (e.g. for an
    /// at-least-once producer). Cleared once the run finishes.
    pub dedupe_key: Option<String>,
    /// Supersede the thread's prior pending/parked work: the newest submission
    /// wins, taking the highest epoch; the stale dispatches are marked superseded
    /// and never claimed again (ADR-0022).
    pub supersede: bool,
}

/// Durable run-dispatch queue: activation opportunity, claim, lease, recovery.
#[async_trait]
pub trait DispatchQueue: Send + Sync {
    /// Idempotently record an accepted run at default options. Re-enqueueing the
    /// same run id is a no-op, so an at-least-once submit has an exactly-once
    /// effect per run.
    async fn enqueue(&self, request: RunExecutionRequest) -> Result<(), DispatchError> {
        self.enqueue_with(request, SubmitOptions::default()).await
    }

    /// Record an accepted run with dispatch options (priority, dedupe key). A
    /// dedupe key already live makes this a no-op.
    async fn enqueue_with(
        &self,
        request: RunExecutionRequest,
        options: SubmitOptions,
    ) -> Result<(), DispatchError>;

    /// Claim one runnable dispatch for `owner`, single owner per run: a fresh
    /// `pending` run, a parked run with pending input (a wake), or a running
    /// dispatch whose lease expired (recovery). Returns `None` when nothing is
    /// runnable, and the run's current pending input in the returned [`Claimed`].
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError>;

    /// Extend the lease on a run this `owner` is executing, so a long run is not
    /// reclaimed by another node's recovery while it is still making progress.
    /// Returns `true` if the lease was renewed (the run is still owned by
    /// `owner`); `false` if it was lost (stolen, settled, or unknown) — the holder
    /// should then stop. This is the multi-node liveness knob (ADR-0019).
    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError>;

    /// Renew, to `now_ms + lease_ms`, the lease on every running dispatch owned by
    /// `owner` that is *within half a lease of expiring* — the daemon's heartbeat
    /// that keeps its in-flight runs from being reclaimed while still executing
    /// (ADR-0024). Returns how many leases were renewed.
    ///
    /// Renewing only near-expiry leases (`lease_until < now_ms + lease_ms/2`), not
    /// every running row on every tick, bounds the write amplification of the
    /// heartbeat: with hundreds of thousands of in-flight runs, a blanket renewal
    /// every few seconds is a storm of no-op-equivalent writes. It stays safe as
    /// long as the heartbeat cadence is under half the lease (the ADR-0024
    /// recommendation), so a lease is always caught within the window before it
    /// expires; a fresh claim, whose lease is a full length out, is skipped until
    /// it approaches expiry.
    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError>;

    /// Settle a claimed dispatch, fenced by the lease `epoch` the caller holds (from
    /// [`Claimed`]`.lease.epoch`). The settle applies only when `epoch` is still the
    /// row's current epoch; if the run was re-claimed under a higher epoch (a
    /// reclaimer took the lapsed lease), the settle is rejected as
    /// [`SettleOutcome::Fenced`] and NOTHING is changed — a stale owner can never
    /// clobber the current owner's in-flight dispatch (reset its lease, re-park it,
    /// or delete it out from under an active drive).
    ///
    /// When applied: `Done` removes the dispatch and all its pending input; `Parked`
    /// returns it to the waiting state and drops only the `consumed` pending (by
    /// `message_id`), leaving input that arrived mid-attempt for the next wake.
    /// `Parked` also resets the crash-retry budget — a run that reaches a checkpoint
    /// refreshes its attempts.
    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError>;

    /// The run's current lease epoch — the fence token bumped on every claim (see
    /// [`Lease::epoch`]). `Some(epoch)` while a dispatch row exists for the run;
    /// `None` when none does (settled, cancelled, or never enqueued). A caller that
    /// holds a LOWER epoch than this has been superseded: a peer reclaimed the lapsed
    /// lease under a higher epoch, so the caller's in-flight writes must be fenced.
    ///
    /// This is the read the *commit* fence uses, the twin of the epoch [`settle`]
    /// already fences on. It is a REQUIRED method with no default: opting a backend
    /// out of the commit fence must be a conscious choice (return `Ok(None)`, which
    /// makes [`holds_current_epoch`](Self::holds_current_epoch) fail OPEN), never an
    /// inherited default a new backend silently gets. A backend that cannot cheaply
    /// read the fence (e.g. a remote transport that does not proxy it) returns
    /// `Ok(None)` explicitly; every durable store returns the row's `lease_epoch`.
    async fn current_epoch(&self, run_id: &RunId) -> Result<Option<u64>, DispatchError>;

    /// Whether a caller holding `epoch` may still commit for `run_id`: `true` while it
    /// holds the current fence, `false` only when a strictly higher epoch has
    /// superseded it. The commit fence checks this before each durable write so a
    /// slow-but-alive owner cannot double-apply side effects after a peer reclaimed
    /// the run.
    ///
    /// A gone row (`current_epoch` is `None`) is fail-OPEN: a run settles its own row
    /// as its final act, and fencing that would reject the legitimate terminal commit.
    /// A backend that cannot read the epoch is likewise fail-open (preserving prior
    /// behaviour) — the fence only ever *rejects* on a definite, observed supersession.
    async fn holds_current_epoch(&self, run_id: &RunId, epoch: u64) -> Result<bool, DispatchError> {
        Ok(match self.current_epoch(run_id).await? {
            Some(current) => epoch >= current,
            None => true,
        })
    }

    /// Dead-letter every *crashed* dispatch — one whose lease expired without a
    /// settle — that has used up its crash-retry budget (`attempt_count >=
    /// max_attempts`). A dead-lettered dispatch is no longer claimed, so a poison
    /// run cannot be reclaimed forever. Returns how many were dead-lettered
    /// (ADR-0015). The crash-retry count increments only on recovery re-claims, so
    /// a normal park/wake never spends the budget.
    async fn reap(&self, max_attempts: u64, now_ms: u64) -> Result<usize, DispatchError>;

    /// Bind `run_id` to the sandbox it was placed on (B-P3, ADR-0021 §6). The
    /// reference is opaque to the dispatch aggregate (the fleet serializes a
    /// `SandboxHandle` into it). Stored durably so `claim` returns it on recovery
    /// and `reconcile_adoption` can re-adopt the same sandbox. Default is a no-op
    /// for backends that do not persist the binding (the neutral seam).
    async fn bind_sandbox(&self, _run_id: &RunId, _sandbox_ref: &str) -> Result<(), DispatchError> {
        Ok(())
    }

    /// The run ids currently dead-lettered, for operations.
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError>;

    /// Return a dead-lettered run to the queue at a fresh budget. Returns `true`
    /// if a dead-lettered run with that id was requeued.
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError>;

    /// Durably cancel a *not-running* dispatch (pending or parked): remove it and
    /// its pending input so it never runs or resumes. Returns the run's thread id
    /// when cancelled (the host then commits a terminal `Cancelled` fact), or
    /// `None` if the run is currently running (use live cancel), already
    /// dead-lettered, or unknown.
    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError>;

    /// The run currently parked on a thread, if any. A thread is the stable
    /// addressable unit (a run is one ephemeral execution); this resolves a
    /// thread-addressed delivery to the run waiting on it.
    async fn parked_run(&self, thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError>;

    /// Remove every dead-lettered dispatch (and its pending input) — operator GC.
    /// Returns how many were purged.
    async fn purge_dead_letters(&self) -> Result<usize, DispatchError>;

    /// Remove dead-lettered dispatches whose dead-letter time is at or before
    /// `cutoff_ms` (and their pending input) — time-windowed GC the daemon runs on
    /// a cadence (ADR-0023). Returns how many were purged.
    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError>;

    /// The run ids superseded by a newer submission on their thread (ADR-0022),
    /// for operations — the mirror of [`dead_letters`](Self::dead_letters).
    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError>;

    /// Every dispatch row's operational summary, in enqueue order — the query
    /// surface for monitoring and maintenance (ADR-0025).
    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError>;
}

/// A pending input as stored, with its optimistic-concurrency `revision`. The
/// revision is store-assigned (1 on append, bumped on edit), so it is surfaced
/// on reads — not part of [`PendingInput`], which is the caller's append payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRecord {
    pub input: PendingInput,
    pub revision: u64,
}

/// The result of a revision-guarded pending edit/retract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    /// The expected revision matched; the change was applied.
    Applied,
    /// The record exists but at a different revision (a concurrent change);
    /// the operation is rejected, fail closed.
    RevisionMismatch,
    /// No pending record with that id (already consumed or never appended).
    NotFound,
}

/// Durable pending-input intake and the thread-message operations over it.
///
/// `append` is the delivery path; `list`/`retract`/`edit` are the thread-message
/// operations surface (run-ingress design: these are NOT `RunIngress` routes).
/// Edit and retract are optimistic: they check the record's `revision` and fail
/// closed on a mismatch, so a concurrent change is never silently overwritten.
/// Records are mutable only before the worker consumes them.
#[async_trait]
pub trait Inbox: Send + Sync {
    /// Idempotently append one pending input at revision 1. Returns `true` if
    /// newly stored, `false` if the `message_id` was already present.
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError>;

    /// The thread's undelivered pending input, in arrival order, with revisions.
    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError>;

    /// Retract one pending record if it is still at `expected_revision`.
    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError>;

    /// Replace a pending record's result if it is still at `expected_revision`,
    /// bumping the revision on success.
    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError>;
}

/// Durable cross-thread delivery via a transactional outbox.
///
/// A run on one thread stages a delivery to *another* thread's pending input;
/// the outbox holds it until a relay moves it. The relay is idempotent without a
/// `delivered` flag or two-phase commit: in one store transaction it appends the
/// payload to the target pending input (idempotent by `message_id`) and deletes
/// the outbox row. A crash between the two leaves the outbox row, so the next
/// relay re-appends (a no-op) and deletes — at-least-once with an exactly-once
/// effect (run-ingress design: cross-thread uses outbox + idempotent target
/// append, never 2PC).
#[async_trait]
pub trait Outbox: Send + Sync {
    /// Idempotently stage a cross-thread delivery. The payload carries the target
    /// run/thread it is destined for; the same `message_id` keys both the outbox
    /// row and the eventual pending append.
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError>;

    /// Relay every staged delivery to its target thread's pending input, one
    /// store transaction per message. Returns how many were relayed.
    async fn relay(&self) -> Result<usize, DispatchError>;
}

/// The combined durable-ingress store. One object implements all aggregates so a
/// wake can freeze pending input within a claim and a relay can move outbox to
/// pending in one transaction; the worker and host depend on this bundle, not on
/// a concrete store.
pub trait Dispatch: DispatchQueue + Inbox + Outbox {}

impl<T: DispatchQueue + Inbox + Outbox> Dispatch for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resume::ResumeResult;
    use std::sync::Mutex;

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn a_request() -> RunExecutionRequest {
        let activation = RunActivation::new(
            RunId("run-1".into()),
            ThreadId("thrd-1".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snap".into()),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: "be helpful".into(),
                    max_steps: 8,
                    model_binding: ModelBinding::new("prov", "model", "acp:test"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        );
        RunExecutionRequest::new(activation)
    }

    /// The SQL backends' `status` column maps to the public enum, and any value
    /// outside the known set (a typo, a future status this build predates) falls
    /// back to `Pending` rather than panicking or mis-rendering the monitor.
    #[test]
    fn dispatch_status_from_db_maps_known_values_and_falls_back() {
        assert_eq!(DispatchStatus::from_db("running"), DispatchStatus::Running);
        assert_eq!(DispatchStatus::from_db("parked"), DispatchStatus::Parked);
        assert_eq!(
            DispatchStatus::from_db("dead_letter"),
            DispatchStatus::DeadLetter
        );
        assert_eq!(
            DispatchStatus::from_db("superseded"),
            DispatchStatus::Superseded
        );
        // "pending" is explicit; an unknown token and the empty string both fall back.
        assert_eq!(DispatchStatus::from_db("pending"), DispatchStatus::Pending);
        assert_eq!(DispatchStatus::from_db("Running"), DispatchStatus::Pending); // case-sensitive
        assert_eq!(DispatchStatus::from_db("bogus"), DispatchStatus::Pending);
        assert_eq!(DispatchStatus::from_db(""), DispatchStatus::Pending);
    }

    /// The neutral submit default every existing caller inherits: ordinary priority,
    /// no dedupe key, no supersede — so `enqueue` is behavior-unchanged.
    #[test]
    fn submit_options_default_is_ordinary_priority_no_dedupe_no_supersede() {
        let o = SubmitOptions::default();
        assert_eq!(o.priority, 0);
        assert!(o.dedupe_key.is_none());
        assert!(!o.supersede);
    }

    fn pending() -> PendingInput {
        PendingInput {
            message_id: "m1".into(),
            run_id: RunId("run-1".into()),
            thread_id: ThreadId("thrd-1".into()),
            correlation_id: "corr-1".into(),
            available_at_ms: Some(1_234),
            result: ResumeResult::allow(),
        }
    }

    /// A pending-input row round-trips through JSON unchanged — the durable delivery
    /// payload the inbox persists.
    #[test]
    fn pending_input_round_trips_through_serde() {
        let p = pending();
        let json = serde_json::to_string(&p).expect("serializes");
        let back: PendingInput = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, p);
    }

    /// ADR-0014 added `available_at_ms` with `#[serde(default)]`: a row written
    /// before it (no such key) must deserialize as `None`, not fail — otherwise a
    /// pre-existing pending input would be dropped (a no-data-loss invariant). Built
    /// by stripping the key from a real row, so it never hardcodes `ResumeResult`'s
    /// wire shape.
    #[test]
    fn a_pending_row_without_available_at_ms_loads_as_none() {
        let mut v = serde_json::to_value(pending()).expect("to value");
        v.as_object_mut().expect("object").remove("available_at_ms");
        let back: PendingInput = serde_json::from_value(v).expect("legacy row loads");
        assert_eq!(back.available_at_ms, None);
    }

    /// The claim/settle wire payloads round-trip through JSON — the lease a worker
    /// holds and the outcome it settles with must survive the HTTP dispatch
    /// transport (a cross-node worker claims/settles over the wire, not the DB).
    #[test]
    fn lease_and_outcome_round_trip_through_serde() {
        let lease = Lease {
            run_id: RunId("run-1".into()),
            owner: "host-7-42".into(),
            expires_ms: 9_999,
            epoch: 3,
        };
        let back: Lease = serde_json::from_str(&serde_json::to_string(&lease).expect("serializes"))
            .expect("deserializes");
        assert_eq!(back, lease);

        // A pre-fence lease row (no `epoch` key) loads with epoch 0 — the neutral
        // value a never-claimed row carries, so an old persisted/wire lease is not
        // rejected (the fence only fires when a HIGHER epoch supersedes it).
        let mut v = serde_json::to_value(&lease).expect("to value");
        v.as_object_mut().expect("object").remove("epoch");
        let legacy: Lease = serde_json::from_value(v).expect("legacy lease loads");
        assert_eq!(legacy.epoch, 0);

        for outcome in [DispatchOutcome::Done, DispatchOutcome::Parked] {
            let back: DispatchOutcome =
                serde_json::from_str(&serde_json::to_string(&outcome).expect("serializes"))
                    .expect("deserializes");
            assert_eq!(back, outcome);
        }
    }

    /// `applied()` is the single predicate the worker branches on after a settle: it
    /// is true only for `Applied`, so a `Fenced` (stale-owner) settle never reads as
    /// success and the stale owner abandons the run.
    #[test]
    fn settle_outcome_applied_is_true_only_when_applied() {
        assert!(SettleOutcome::Applied.applied());
        assert!(!SettleOutcome::Fenced.applied());
    }

    /// `SettleOutcome` is a settle *response*; a cross-node worker settles over the
    /// wire, so both variants must survive a JSON round-trip intact.
    #[test]
    fn settle_outcome_round_trips_through_serde() {
        for o in [SettleOutcome::Applied, SettleOutcome::Fenced] {
            let back: SettleOutcome =
                serde_json::from_str(&serde_json::to_string(&o).expect("serializes"))
                    .expect("deserializes");
            assert_eq!(back, o);
        }
    }

    /// The full claim payload — request + lease + pending + a bound sandbox ref —
    /// round-trips through JSON, since a cross-node worker receives `Claimed` over the
    /// dispatch transport, not out of the DB.
    #[test]
    fn claimed_round_trips_through_serde_including_sandbox_binding() {
        let claimed = Claimed {
            request: a_request(),
            lease: Lease {
                run_id: RunId("run-1".into()),
                owner: "host-7".into(),
                expires_ms: 5_000,
                epoch: 2,
            },
            pending: vec![pending()],
            sandbox: Some("sbx-opaque-ref".into()),
        };
        let back: Claimed =
            serde_json::from_str(&serde_json::to_string(&claimed).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back, claimed);
        assert_eq!(back.sandbox.as_deref(), Some("sbx-opaque-ref"));

        // An unplaced run (no sandbox yet) round-trips with `sandbox: None`.
        let unplaced = Claimed {
            sandbox: None,
            ..claimed
        };
        let back: Claimed =
            serde_json::from_str(&serde_json::to_string(&unplaced).expect("serializes"))
                .expect("deserializes");
        assert_eq!(back.sandbox, None);
    }

    /// A `DispatchQueue` that records the options its `enqueue_with` was called with,
    /// to prove the default `enqueue` delegates at `SubmitOptions::default()` and that
    /// `bind_sandbox` defaults to a no-op `Ok(())`. Every other method is out of scope
    /// for this test and left `unimplemented!()`.
    #[derive(Default)]
    struct CapturingQueue {
        last_options: Mutex<Option<SubmitOptions>>,
    }

    #[async_trait]
    impl DispatchQueue for CapturingQueue {
        async fn enqueue_with(
            &self,
            _request: RunExecutionRequest,
            options: SubmitOptions,
        ) -> Result<(), DispatchError> {
            *self.last_options.lock().unwrap() = Some(options);
            Ok(())
        }
        async fn claim(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            unimplemented!()
        }
        async fn renew_lease(
            &self,
            _run_id: &RunId,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<bool, DispatchError> {
            unimplemented!()
        }
        async fn renew_owned_leases(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn settle(
            &self,
            _run_id: &RunId,
            _epoch: u64,
            _outcome: DispatchOutcome,
            _consumed: &[String],
        ) -> Result<SettleOutcome, DispatchError> {
            unimplemented!()
        }
        async fn current_epoch(&self, _run_id: &RunId) -> Result<Option<u64>, DispatchError> {
            // A submit-only capture double: no fence, fail-open by explicit choice.
            Ok(None)
        }
        async fn reap(&self, _max_attempts: u64, _now_ms: u64) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
            unimplemented!()
        }
        async fn requeue(&self, _run_id: &RunId) -> Result<bool, DispatchError> {
            unimplemented!()
        }
        async fn cancel(&self, _run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
            unimplemented!()
        }
        async fn parked_run(&self, _thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
            unimplemented!()
        }
        async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn purge_dead_letters_before(&self, _cutoff_ms: u64) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
            unimplemented!()
        }
        async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn enqueue_delegates_to_enqueue_with_at_default_options() {
        let q = CapturingQueue::default();
        q.enqueue(a_request()).await.expect("enqueue ok");
        assert_eq!(
            q.last_options.lock().unwrap().clone(),
            Some(SubmitOptions::default()),
            "the convenience enqueue must submit at default priority/dedupe/supersede"
        );
    }

    #[tokio::test]
    async fn bind_sandbox_defaults_to_a_no_op_ok() {
        // Backends that do not persist the binding inherit the neutral no-op seam.
        let q = CapturingQueue::default();
        assert!(
            q.bind_sandbox(&RunId("run-1".into()), "sbx-ref")
                .await
                .is_ok()
        );
    }

    /// A `DispatchQueue` whose only wired read is `current_epoch`, returning a fixed
    /// value, so the *default* `holds_current_epoch` fence logic can be exercised in
    /// isolation. Every other method is out of scope and left `unimplemented!()`.
    struct FixedEpochQueue(Option<u64>);

    #[async_trait]
    impl DispatchQueue for FixedEpochQueue {
        async fn current_epoch(&self, _run_id: &RunId) -> Result<Option<u64>, DispatchError> {
            Ok(self.0)
        }
        async fn enqueue_with(
            &self,
            _request: RunExecutionRequest,
            _options: SubmitOptions,
        ) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn claim(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<Option<Claimed>, DispatchError> {
            unimplemented!()
        }
        async fn renew_lease(
            &self,
            _run_id: &RunId,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<bool, DispatchError> {
            unimplemented!()
        }
        async fn renew_owned_leases(
            &self,
            _owner: &str,
            _lease_ms: u64,
            _now_ms: u64,
        ) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn settle(
            &self,
            _run_id: &RunId,
            _epoch: u64,
            _outcome: DispatchOutcome,
            _consumed: &[String],
        ) -> Result<SettleOutcome, DispatchError> {
            unimplemented!()
        }
        async fn reap(&self, _max_attempts: u64, _now_ms: u64) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
            unimplemented!()
        }
        async fn requeue(&self, _run_id: &RunId) -> Result<bool, DispatchError> {
            unimplemented!()
        }
        async fn cancel(&self, _run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
            unimplemented!()
        }
        async fn parked_run(&self, _thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
            unimplemented!()
        }
        async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn purge_dead_letters_before(&self, _cutoff_ms: u64) -> Result<usize, DispatchError> {
            unimplemented!()
        }
        async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
            unimplemented!()
        }
        async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
            unimplemented!()
        }
    }

    /// The commit fence's default `holds_current_epoch` is a safety invariant that
    /// lives entirely in this contract layer, yet it was never asserted directly.
    /// Pin all three arms of the default:
    ///
    /// - A GONE row (`current_epoch == None`) is fail-OPEN → `true`. This is
    ///   deliberate (documented on the trait): a run settles its own dispatch row as
    ///   its final act, so fencing a gone row would reject the legitimate terminal
    ///   commit. NOT a bug — the fence only ever *rejects* on an observed, strictly
    ///   higher epoch, never on absence.
    /// - Equal or higher held epoch → `true` (the caller still holds the fence).
    /// - A strictly HIGHER current epoch → `false` (a peer reclaimed the lapsed
    ///   lease; the slow-but-alive owner is fenced off).
    #[tokio::test]
    async fn holds_current_epoch_fails_open_on_a_gone_row_and_fences_only_a_higher_epoch() {
        let run = RunId("run-1".into());

        // Gone row: fail-open regardless of the epoch the caller holds.
        let gone = FixedEpochQueue(None);
        assert!(
            gone.holds_current_epoch(&run, 0).await.unwrap(),
            "a gone row lets a terminal commit through (fail-open by design)"
        );
        assert!(
            gone.holds_current_epoch(&run, 99).await.unwrap(),
            "fail-open holds for any caller epoch on a gone row"
        );

        // Live row at epoch 5: equal or higher held epoch still holds the fence.
        let live = FixedEpochQueue(Some(5));
        assert!(
            live.holds_current_epoch(&run, 5).await.unwrap(),
            "the current holder (equal epoch) still commits"
        );
        assert!(
            live.holds_current_epoch(&run, 6).await.unwrap(),
            "a caller at a higher epoch holds the fence"
        );
        // Strictly higher CURRENT epoch supersedes the caller → fenced off.
        assert!(
            !live.holds_current_epoch(&run, 4).await.unwrap(),
            "a strictly-higher current epoch fences the stale owner"
        );
    }
}
