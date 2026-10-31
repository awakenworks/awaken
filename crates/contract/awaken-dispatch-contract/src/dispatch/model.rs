use super::*;

/// The Coordinator↔Worker claim lives in `awaken-worker-contract`; dispatch
/// mints it from a [`DispatchLease`] and keeps this path for its callers.
pub use awaken_worker_contract::RunClaim;

impl From<&DispatchLease> for RunClaim {
    fn from(lease: &DispatchLease) -> Self {
        Self {
            run_id: lease.run_id().clone(),
            owner: lease.owner().to_owned(),
            epoch: lease.epoch(),
        }
    }
}

/// One logical Thread commit plus the exact dispatch authority that admits it.
///
/// Worker identity/incarnation is authenticated by the transport/service edge;
/// the durable coordinator receives only `operation`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedCommitCommand {
    pub claim: RunClaim,
    pub operation: awaken_agent_contract::thread::commit::operation::CommitOperation,
}

/// A claimed, ready-to-run dispatch and the run's undelivered pending input.
/// `pending` is normally empty for a fresh Run, but contains an immediate
/// Run-bound input when an Outbox message atomically admitted that continuation.
/// It is non-empty for an ordinary wake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claimed {
    pub request: RunDispatch,
    pub lease: DispatchLease,
    /// Exact subordinate Session Work authority issued by the Coordinator edge
    /// for this claim. Registered remote Session attempts require it before any
    /// committed Thread read; ordinary and colocated attempts omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_work_authority:
        Option<awaken_session_application_contract::SessionWorkAuthorityReceipt>,
    /// Exact credential decisions atomically committed with this lease epoch.
    /// Cancellation carries only the selected authenticated remote A2A
    /// transport binding; anonymous and non-remote cancellation remains empty.
    #[serde(default)]
    pub credential_bindings: Vec<AttemptCredentialBinding>,
    /// Fresh recipient transports issued after this exact claim selected its
    /// Worker. They are response-local and never replace the immutable request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_deliveries: Vec<AttemptCredentialDelivery>,
    /// A durable cancellation intent recorded before any live signal or terminal
    /// commit. The worker drives this through the same claim/epoch fence as normal
    /// execution, so a crash cannot lose a cancellation or resurrect the run.
    #[serde(default)]
    pub cancellation_requested: bool,
    /// The run's undelivered pending input. The worker decides execute-vs-resume
    /// from committed truth (the awaiting ticket), not from this field, and tells
    /// `settle` which inputs it consumed.
    pub pending: Vec<PendingInput>,
    /// Whether this claim reclaimed an expired running lease. Kept explicit so
    /// operations can distinguish crash recovery from an ordinary await/wake
    /// claim (both advance the fencing epoch).
    #[serde(default)]
    pub recovered: bool,
    /// This lease owns an expired Session Run reservation. The Worker must
    /// recover the root activity receipt and atomically resolve the reservation
    /// before it may enter the Run executor.
    #[serde(default)]
    pub session_activity_admission_required: bool,
    /// The sandbox this run is bound to for its lifetime (B-P3, ADR-0021 §6), as an
    /// **opaque** reference — the dispatch aggregate stays neutral (it names no
    /// provisioning type); the fleet serializes a `SandboxHandle` into it and
    /// parses it back on adoption. `None` until the run is placed on a sandbox.
    /// Durable so crash recovery (`reconcile_adoption`) can re-adopt the same
    /// sandbox instead of leaking it.
    pub sandbox: Option<String>,
    /// Worker incarnation selected for this lease epoch. Legacy/local claims omit
    /// it; registered remote claims always persist and return it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<WorkerAssignment>,
}

/// How a claimed attempt resolved. Settled atomically with releasing the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DispatchOutcome {
    /// The run ended; the dispatch is finished and removed.
    Done,
    /// The run awaits external input; keep the dispatch for a later wake.
    Awaiting,
}

/// Whether a [`settle`](DispatchSettlement::settle) was applied or fenced off as stale.
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

/// Authoritative observation returned by one exact-claim lease renewal.
///
/// Renewal is also the existing signal bridge from durable cancellation into
/// an already-running physical attempt. Keeping that distinction typed avoids
/// treating an explicit cancellation as an unexplained ownership loss while
/// still fencing every unrelated owner or epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseRenewalOutcome {
    /// The exact claim remains current and its lease was extended.
    Current,
    /// Durable cancellation revoked the mutation claim, but this exact claim
    /// still owns the physical attempt that must cooperatively stop.
    CancellationRequested,
    /// The claim is neither current nor the cancelled physical participant.
    Lost,
}

/// Decide whether a durable deadline has passed.
///
/// One rule with one name. Lease expiry and reservation expiry are the same
/// question, and before this existed it was answered by four separate Rust
/// spellings --- two of which normalized `now` and two of which did not --- plus
/// one hand-written predicate per SQL query. A rule that must be re-derived
/// correctly at every site is a rule that eventually is not, and only the Rust
/// spellings were ever reachable by a proof.
///
/// This is the one [`awaken_time::Expiry`] law: a deadline has passed
/// once reached (`now >= deadline`; SQL `lease_until <= now`), so a lease minted
/// for a non-zero duration is never already expired when written. An absent
/// deadline never passes, because "this row holds no lease" and "this row's
/// lease ran out" are different facts and only the second may be reclaimed.
///
/// Clamping a millisecond into the range a SQL backend can hold is a storage
/// concern and stays with the stores; this function judges whatever values they
/// hand it, so a caller that normalizes and a caller that does not both keep
/// exactly the behaviour they had.
#[must_use]
pub const fn deadline_has_passed(deadline_ms: Option<u64>, now_ms: u64) -> bool {
    match deadline_ms {
        None => false,
        Some(ms) => Expiry::from_epoch_ms(ms).is_expired_at(UtcTime::from_epoch_ms(now_ms)),
    }
}

#[cfg(test)]
#[test]
fn deadline_has_passed_covers_absent_exact_and_full_wire_boundaries() {
    // Causes: C1 absent/present deadline; C2 now before/at/after it; C3 zero,
    // ordinary and u64::MAX wire values. Effects: E1 absence never expires;
    // E2 present expires exactly at equality and remains expired afterwards;
    // E3 MAX is live at MAX-1 and expired at MAX. Decision rows C1 absent=>E1;
    // present+before=>live; present+at/after=>E2; MAX boundary=>E3.
    for (deadline, now, passed) in [
        (None, 0, false),
        (None, u64::MAX, false),
        (Some(0), 0, true),
        (Some(1), 0, false),
        (Some(1), 1, true),
        (Some(1), 2, true),
        (Some(u64::MAX), u64::MAX - 1, false),
        (Some(u64::MAX), u64::MAX, true),
        (Some(u64::MAX - 1), u64::MAX, true),
    ] {
        assert_eq!(
            deadline_has_passed(deadline, now),
            passed,
            "{deadline:?} at {now}"
        );
    }
}

#[cfg(kani)]
/// @obligation run_ingress.a_deadline_passes_once_it_is_reached
#[kani::proof]
fn absent_deadline_never_passes_for_any_now() {
    // C1 no deadline; C2 arbitrary full-width now. E1 never expired.
    // Rule C1+C2=>E1. No Expiry conversion is needed for an absent lease.
    let now_ms: u64 = kani::any();
    assert!(!deadline_has_passed(None, now_ms), "R1/E1");
}

#[cfg(kani)]
/// @obligation run_ingress.a_deadline_passes_once_it_is_reached
#[kani::proof]
fn bounded_deadline_passes_once_it_is_reached() {
    // Causes: present deadline and independent now/deadline values.
    // Effects: E1 before is live; E2 equal/after has passed. Decision rows
    // deadline>now=>E1, deadline<=now=>E2.
    //
    // Both values are drawn from {0, 1, u64::MAX}, which realises <, = and >
    // at the bottom and at the top of the u64 wire domain. That covers every
    // u64 pair: `deadline_has_passed` compares the Unix-nanosecond images of
    // the two values, and the lease kernel's lemma
    // `unix_nanos_of_epoch_ms_is_strictly_monotone` (awaken-lease-contract)
    // proves that image order equals wire order for every u64, so the
    // verdict, like the oracle, reads only the order of the pair.
    let representatives = [0, 1, u64::MAX];
    let now_index: usize = kani::any();
    let deadline_index: usize = kani::any();
    kani::assume(now_index < representatives.len() && deadline_index < representatives.len());
    let now_ms = representatives[now_index];
    let deadline_ms = representatives[deadline_index];
    let passed = deadline_has_passed(Some(deadline_ms), now_ms);
    assert_eq!(
        passed,
        deadline_ms <= now_ms,
        "present deadline is inclusive"
    );
    kani::cover!(!passed, "a deadline ahead of now is live");
    kani::cover!(
        passed && deadline_ms == now_ms,
        "the deadline instant has passed"
    );
    kani::cover!(
        passed && deadline_ms < now_ms,
        "a deadline behind now has passed"
    );
}

/// Apply the one backend-neutral renewal decision to facts read atomically from
/// a Dispatch row. Backends own locking and lease writes; this function owns the
/// meaning of those facts.
#[must_use]
pub const fn decide_lease_renewal(
    current_claim: bool,
    cancelled_physical_attempt: bool,
) -> LeaseRenewalOutcome {
    if current_claim {
        LeaseRenewalOutcome::Current
    } else if cancelled_physical_attempt {
        LeaseRenewalOutcome::CancellationRequested
    } else {
        LeaseRenewalOutcome::Lost
    }
}

#[cfg(kani)]
/// @obligation run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt
#[kani::proof]
fn lease_renewal_observation_preserves_claim_and_attempt_fences() {
    // Cause/effect decision table: C1=exact current mutation claim;
    // C2=durably cancelled exact physical attempt. E1=renew current lease;
    // E2=signal cooperative cancellation; E3=report ownership lost.
    // R1=C1 -> E1 (including a cancellation worker claiming the intent);
    // R2=!C1+C2 -> E2; R3=!C1+!C2 -> E3. This exhausts both causes and
    // prevents cancellation for an unrelated owner/epoch.
    let current_claim: bool = kani::any();
    let cancelled_physical_attempt: bool = kani::any();
    let outcome = decide_lease_renewal(current_claim, cancelled_physical_attempt);
    assert_eq!(outcome == LeaseRenewalOutcome::Current, current_claim);
    assert_eq!(
        outcome == LeaseRenewalOutcome::CancellationRequested,
        !current_claim && cancelled_physical_attempt
    );
    assert_eq!(
        outcome == LeaseRenewalOutcome::Lost,
        !current_claim && !cancelled_physical_attempt
    );
}

/// Admission result for the physical executor slot carried by one Dispatch
/// aggregate. A Run claim authorizes durable mutation, but an expired claim may
/// still have a model/tool/Sandbox future physically unwinding. The successor
/// may hold the new claim while it waits; it cannot cross an external execution
/// boundary until this result records one of the three admitted transitions.
/// Keeping install, exact retry, and predecessor replacement distinct prevents
/// a caller from treating inherited physical responsibility as proof that no
/// effect has started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptAdmission {
    /// This exact claim acquired the previously empty physical slot.
    Applied,
    /// This exact claim already owns the slot; an idempotent retry succeeds.
    AlreadyApplied,
    /// This exact claim replaced a predecessor after the leaf executor proved
    /// it can continue the same durable remote operation without resubmitting
    /// the initiating effect. The predecessor's responsibility is inherited;
    /// this is never evidence that the admitted operation has not started.
    Replaced,
    /// A predecessor claim has not yet acknowledged physical quiescence.
    Blocked,
    /// This claim is no longer the current durable owner.
    Fenced,
}

impl AttemptAdmission {
    /// Whether this admission itself proves that the caller installed an empty
    /// slot and therefore owns no inherited or previously admitted effect.
    ///
    /// This is the only admission provenance that can acknowledge a
    /// cancellation observed before the executor Future is constructed.
    #[must_use]
    pub const fn permits_pre_start_no_effect_ack(self) -> bool {
        matches!(self, Self::Applied)
    }
}

#[cfg(kani)]
/// @obligation session_execution.pre_start_cancel_ack_requires_fresh_slot_install
#[kani::proof]
fn pre_start_no_effect_ack_requires_fresh_install_admission() {
    // Cause/effect decision table: C1 admission installed an empty slot; C2 it
    // stuttered an exact prior admission; C3 it replaced a predecessor; C4 it
    // did not admit. E1 permits a pre-start no-effect ACK; E2 retains the slot.
    // R1=C1 -> E1. R2=C2|C3|C4 -> E2. This proof prevents future enum growth
    // from becoming ACK-authoritative by default.
    let admission = match kani::any::<u8>() % 5 {
        0 => AttemptAdmission::Applied,
        1 => AttemptAdmission::AlreadyApplied,
        2 => AttemptAdmission::Replaced,
        3 => AttemptAdmission::Blocked,
        _ => AttemptAdmission::Fenced,
    };
    assert_eq!(
        admission.permits_pre_start_no_effect_ack(),
        admission == AttemptAdmission::Applied
    );
}

/// One durable observation that a dispatch applied [`DispatchOutcome::Done`].
///
/// This is delivery truth only: the committed run fact remains authoritative for
/// the run's terminal state and cause. `sequence` is a store-assigned cursor;
/// consumers checkpoint it independently and may replay pages idempotently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchCompletion {
    pub sequence: u64,
    pub run_id: RunId,
    /// Logical Thread and optional Session routing affinity retained from the
    /// accepted dispatch. A Session root is self-affine; a coordinated child is
    /// affine to its parent. Historical rows may omit both. Admission uses the
    /// pair only to enforce the Thread/archive limit, never as a second
    /// relationship or archive source of truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_thread_id: Option<ThreadId>,
    /// Canonical identity of the accepted dispatch. Historical tombstones that
    /// predate collision detection retain `None` and therefore cannot authorize
    /// a caller-owned Run-id replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    /// Whether a durable cancel request was retained on the dispatch row when
    /// it settled. The committed Run state stays the terminal authority; this
    /// keeps the cancel fact an interrupted Run may not have been able to
    /// commit (its cancellation claim found an unconfirmed predecessor and it
    /// ended Error/Indeterminate). Tombstones written before this fact existed
    /// decode as `false`, and `false` is omitted on the wire so older peers
    /// read and write identical bytes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancellation_requested: bool,
}

impl SettleOutcome {
    /// Whether the settle was applied (vs. fenced off as a stale owner's).
    pub fn applied(self) -> bool {
        matches!(self, SettleOutcome::Applied)
    }
}

/// The lifecycle state of a dispatch, for the operational query surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchState {
    /// Complete Session Run intent awaiting its root activity admission. It is
    /// invisible to ordinary claims until its reservation deadline expires.
    Reserved,
    /// An exclusive recovery claim is repairing the Reserved intent's Session
    /// activity admission. The Run executor has not started.
    ReservationLeased,
    /// Fresh, not yet claimed.
    Pending,
    /// Claimed and executing under a lease.
    Leased,
    /// Awaiting on a committed resume ticket.
    Awaiting,
    /// Dead-lettered past its crash-retry budget (ADR-0015).
    DeadLetter,
    /// Superseded by a newer submission on its thread (ADR-0022).
    Superseded,
}

impl DispatchState {
    /// A runnable or reserved row still requires cancellation/settlement before
    /// source access. Physical occupancy remains an independent observation.
    #[must_use]
    pub const fn remains_unsettled(self) -> bool {
        matches!(
            self,
            Self::Reserved
                | Self::ReservationLeased
                | Self::Pending
                | Self::Leased
                | Self::Awaiting
        )
    }

    /// Map the stored status text (the SQL backends' `status` column) to the
    /// public status. Unknown durable vocabulary is rejected rather than turned
    /// into claimable `Pending` work.
    pub fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "reserved" => Self::Reserved,
            "reservation_running" => Self::ReservationLeased,
            "pending" => Self::Pending,
            "running" => Self::Leased,
            "awaiting" => Self::Awaiting,
            "dead_letter" => Self::DeadLetter,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// Whether a prior Session dispatch has crossed a boundary at which queue
/// replacement cannot strand an open Session activity.
#[must_use]
pub const fn session_run_replacement_candidate_is_safe(state: DispatchState) -> bool {
    matches!(state, DispatchState::Awaiting | DispatchState::Superseded)
}

#[cfg(kani)]
mod session_replacement_proofs {
    use super::{DispatchState, session_run_replacement_candidate_is_safe};

    /// @obligation run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
    /// @proof-input-debt tautology session_run_replacement_candidate_is_safe
    #[kani::proof]
    fn only_awaiting_or_already_superseded_is_replacement_safe() {
        let tag: u8 = kani::any();
        kani::assume(tag < 7);
        let state = match tag {
            0 => DispatchState::Reserved,
            1 => DispatchState::ReservationLeased,
            2 => DispatchState::Pending,
            3 => DispatchState::Leased,
            4 => DispatchState::Awaiting,
            5 => DispatchState::DeadLetter,
            _ => DispatchState::Superseded,
        };
        assert_eq!(
            session_run_replacement_candidate_is_safe(state),
            matches!(state, DispatchState::Awaiting | DispatchState::Superseded)
        );
    }
}

/// An operational view of one dispatch row, for monitoring and maintenance — the
/// `DispatchReads` query role (ADR-0025). Carries no live handle, just committed
/// queue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchSummary {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    /// Existing Session routing affinity. `None` denotes an ordinary root Run,
    /// self-affinity denotes a Session root, and a different Thread denotes a
    /// coordinated child. Operational cleanup can use this projection to find
    /// every child without a second relationship registry.
    pub session_thread_id: Option<ThreadId>,
    /// Durable Session activity coordinate currently carried by this row. It
    /// lets the Session root atomically transfer a committed continuation away
    /// from the finishing attempt before the outbox rotates the row.
    pub session_activity_epoch: Option<u64>,
    /// Expiry of the exclusive admission owner while `state == Reserved`.
    /// Other states report `None`.
    pub reservation_deadline_ms: Option<u64>,
    pub state: DispatchState,
    /// Terminal control has been accepted and is awaiting/under a fenced claim.
    pub cancellation_requested: bool,
    /// Consecutive crash-recoveries without a settle.
    pub attempt_count: u64,
    /// Whether an opaque Sandbox handle has been durably bound. The monitoring
    /// view deliberately exposes no provider-specific handle material.
    pub sandbox_bound: bool,
    /// Whether a model/tool/Sandbox future still owns the exact physical slot.
    /// The owner/epoch remain private fencing material; operators need only the
    /// stuck-quiescence signal.
    pub physical_attempt_active: bool,
}

/// One coherent, data-only observation of the original admission storage.
/// Missing grants no reservation or execution authority. Live carries the
/// original request even when its row is reserved, awaiting or quarantined;
/// Completed retains delivery evidence only, never a reconstructed input.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionRunAdmissionObservation {
    Missing,
    Live(Box<RunDispatch>),
    Completed(DispatchCompletion),
}

/// Claim-fenced resolution of one expired Session Run reservation. This mutates
/// the existing dispatch row only; it is not a second command or activity log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunReservationResolution {
    /// The canonical Session activity receipt committed. Bind it to the row and
    /// publish Pending; a later ordinary claim remains the only executor entry.
    Admitted { session_activity_epoch: u64 },
    /// Session policy definitively rejected the still-unstarted intent. Remove
    /// the reservation without writing a Run completion tombstone.
    Rejected,
    /// Admission authority was unavailable. Return the exact claim to Reserved
    /// for this store-clock TTL, retaining the same Run identity.
    Retry { reservation_ttl_ms: u64 },
}

/// Closed evidence returned when a caller binds a committed Session activity to
/// its one durable Run reservation. Callers use this instead of guessing whether
/// a false CAS result means replay, recovery ownership, completion, or absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRunReservationActivation {
    Activated,
    AlreadyActivated { session_activity_epoch: u64 },
    RecoveryClaimed,
    Completed,
    MissingOrRejected,
    Conflict,
}

/// Closed evidence returned before a caller opens a Session activity. This
/// prevents a completion or conflicting replay from creating an epoch that
/// would then require compensating recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunReservationOutcome {
    Reserved,
    AlreadyReserved,
    RecoveryClaimed,
    AlreadyActivated { session_activity_epoch: u64 },
    Completed,
    Conflict,
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
    /// Supersede the thread's prior pending/awaiting work: the newest submission
    /// wins, taking the highest epoch; the stale dispatches are marked superseded
    /// and never claimed again (ADR-0022).
    pub supersede: bool,
}

/// Trusted policy and committed archive evidence for one Session-child
/// admission.
///
/// `archived_threads` is a caller-supplied projection of authoritative committed
/// [`ThreadDisposition`](awaken_agent_contract::ThreadDisposition) state. The
/// queue never persists archive flags. It combines this evidence with its live
/// dispatches and completion tombstones under one admission transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChildAdmission {
    pub max_unarchived_threads: usize,
    pub archived_threads: Vec<ThreadId>,
    /// Derived child Threads that share the Session partition but do not belong
    /// to this Managed capacity class (currently publication-pinned advisor
    /// consultations). This is transient authoritative evidence, not a stored
    /// flag or relationship registry.
    pub capacity_exempt_threads: Vec<ThreadId>,
}

impl SessionChildAdmission {
    #[must_use]
    pub fn new(max_unarchived_threads: usize, archived_threads: Vec<ThreadId>) -> Self {
        Self {
            max_unarchived_threads,
            archived_threads,
            capacity_exempt_threads: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_capacity_exempt_threads(mut self, threads: Vec<ThreadId>) -> Self {
        self.capacity_exempt_threads = threads;
        self
    }
}

/// Admission authority for a fresh Run activated by one Outbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationAdmission {
    /// A root Run. Ordinary roots carry no Session affinity; a Session root
    /// carries the existing canonical self-affinity (`session_thread_id` equals
    /// its own Thread). A foreign parent affinity requires [`SessionChild`](Self::SessionChild).
    Root,
    /// Existing/new Session-child Thread under trusted archive evidence.
    SessionChild(SessionChildAdmission),
}
