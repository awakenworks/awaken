//! The dispatch ports now live in `awaken-run-ingress-contract` (ADR-0039 2.1),
//! re-exported here so the host's internal `crate::dispatch::*` paths and existing
//! consumers are unchanged.

pub use awaken_run_ingress_contract::dispatch::*;

#[cfg(any(feature = "durable", test, feature = "test-support"))]
use awaken_run_ingress_contract::{DispatchAdmissionShape, DispatchIdentityScope, RunDispatch};

/// Backend-neutral shape read while locking one claimed dispatch epoch. SQLite
/// stores the request as JSON text and PostgreSQL decodes it through `Json<T>`,
/// but both must interpret the remaining fence columns identically.
#[cfg(feature = "durable")]
pub(crate) type ClaimEpochStorageRow<T> = (i64, Option<String>, Option<i64>, T, i64);

/// Persistence-independent evidence available when one caller-owned Run id is
/// admitted. Backends load evidence; this function owns the one identity
/// decision so memory, SQLite, and Postgres cannot drift.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) enum StoredRunIdentity<'a> {
    Absent,
    Live(&'a RunDispatch),
    Completed(Option<&'a str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) enum RunIdentityDecision {
    New,
    Replay,
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn decide_run_identity(
    stored: StoredRunIdentity<'_>,
    incoming: &RunDispatch,
) -> Result<RunIdentityDecision, DispatchError> {
    let replay = match stored {
        StoredRunIdentity::Absent => return Ok(RunIdentityDecision::New),
        StoredRunIdentity::Live(existing) => existing.same_admission_dispatch(incoming),
        StoredRunIdentity::Completed(Some(existing)) => match incoming.identity_scope {
            DispatchIdentityScope::SessionCommand => incoming
                .session_command_fingerprint
                .as_ref()
                .filter(|fingerprint| fingerprint.is_current())
                .is_some_and(|fingerprint| {
                    existing == fingerprint.as_str()
                        || (existing.starts_with("sha256:")
                            && existing == incoming.legacy_session_reservation_fingerprint())
                }),
            DispatchIdentityScope::FullDispatch => existing == incoming.canonical_fingerprint(),
        },
        StoredRunIdentity::Completed(None) => false,
    };
    if replay {
        Ok(RunIdentityDecision::Replay)
    } else {
        Err(DispatchError::Conflict(format!(
            "run id `{}` was reused with another or unverifiable dispatch payload",
            incoming.run_id().0
        )))
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn classify_live_session_run_reservation(
    stored: &RunDispatch,
    state: DispatchState,
    incoming: &RunDispatch,
) -> SessionRunReservationOutcome {
    if decide_run_identity(StoredRunIdentity::Live(stored), incoming).is_err() {
        return SessionRunReservationOutcome::Conflict;
    }
    match state {
        DispatchState::Reserved => SessionRunReservationOutcome::AlreadyReserved,
        DispatchState::ReservationLeased => SessionRunReservationOutcome::RecoveryClaimed,
        _ => stored.session_activity_epoch.map_or(
            SessionRunReservationOutcome::Conflict,
            |session_activity_epoch| SessionRunReservationOutcome::AlreadyActivated {
                session_activity_epoch,
            },
        ),
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn classify_completed_session_run_reservation(
    request_fingerprint: Option<&str>,
    incoming: &RunDispatch,
) -> SessionRunReservationOutcome {
    if decide_run_identity(StoredRunIdentity::Completed(request_fingerprint), incoming).is_ok() {
        SessionRunReservationOutcome::Completed
    } else {
        SessionRunReservationOutcome::Conflict
    }
}

/// Validate the one canonical shape accepted by Session Run reservation and
/// normalize its exclusive admission TTL before any backend transaction.
/// A reservation is always self-affine and cannot already carry the activity
/// coordinate that the later activation/repair transition owns.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_session_run_reservation_request(
    request: RunDispatch,
    reservation_ttl_ms: u64,
) -> Result<(RunDispatch, u64), DispatchError> {
    if !request
        .session_command_fingerprint
        .as_ref()
        .is_some_and(awaken_session_contract::SessionRunCommandFingerprint::is_current)
    {
        return Err(DispatchError::Rejected(
            "Session Run reservation requires a current command fingerprint".to_string(),
        ));
    }
    if request.admission_shape() != DispatchAdmissionShape::SessionRootAwaitingActivity {
        return Err(DispatchError::Rejected(
            "Session Run reservation requires self-affinity and no activity epoch".to_string(),
        ));
    }
    let reservation_ttl_ms = crate::clock::normalize_millis(reservation_ttl_ms);
    if reservation_ttl_ms == 0 {
        return Err(DispatchError::Rejected(
            "Session Run reservation TTL must be nonzero".to_string(),
        ));
    }
    Ok((request.with_session_command_identity(), reservation_ttl_ms))
}

/// A Session replacement may cross only an already-settled Awaiting boundary.
/// Pending/Leased/Reserved/DeadLetter work can still own an open Session
/// activity; replacing it in the queue would strand that aggregate fact.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_session_run_replacement_candidates(
    request: &RunDispatch,
    candidates: impl IntoIterator<Item = DispatchState>,
) -> Result<(), DispatchError> {
    if !request.session_run_replacement.supersedes_prior() {
        return Ok(());
    }
    if candidates
        .into_iter()
        .all(awaken_run_ingress_contract::session_run_replacement_candidate_is_safe)
    {
        Ok(())
    } else {
        Err(DispatchError::Rejected(
            "Session Run replacement requires every prior live dispatch to be Awaiting".to_string(),
        ))
    }
}

/// Reject the one Session-root intent shape that must cross the durable
/// activity-receipt boundary before it is executable. Every ordinary fresh-row
/// path calls this before persistence; child and already activity-bound legacy
/// shapes retain their existing dedicated policy checks.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_executable_dispatch_admission(
    request: &RunDispatch,
) -> Result<(), DispatchError> {
    match request.admission_shape() {
        DispatchAdmissionShape::SessionRootAwaitingActivity => Err(DispatchError::Rejected(
            "Session root awaiting an activity receipt must use Session Run reservation"
                .to_string(),
        )),
        DispatchAdmissionShape::InvalidZeroActivityEpoch => Err(DispatchError::Rejected(
            "Session activity epoch must be nonzero".to_string(),
        )),
        DispatchAdmissionShape::OrdinaryRoot
        | DispatchAdmissionShape::SessionRootWithActivity
        | DispatchAdmissionShape::SessionChild => Ok(()),
    }
}

/// Classify the complete durable evidence for binding one Session activity to
/// an existing Run reservation. `None` is the sole mutation-eligible result:
/// the exact live row is still Reserved, self-affine, and has no activity epoch.
/// Keeping this decision here prevents the memory, SQLite, and PostgreSQL
/// adapters from growing three subtly different crash-window policies.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn classify_session_run_reservation_activation(
    live: Option<(&RunDispatch, DispatchState)>,
    completed: bool,
    session_thread_id: &awaken_agent_contract::agent::thread::Id,
    session_activity_epoch: u64,
) -> Result<Option<SessionRunReservationActivation>, DispatchError> {
    if session_activity_epoch == 0 {
        return Err(DispatchError::Rejected(
            "Session activity epoch must be nonzero".to_string(),
        ));
    }
    let Some((request, state)) = live else {
        return Ok(Some(if completed {
            SessionRunReservationActivation::Completed
        } else {
            SessionRunReservationActivation::MissingOrRejected
        }));
    };
    if request.session_thread_id.as_ref() != Some(session_thread_id)
        || request.thread_id() != session_thread_id
    {
        return Ok(Some(SessionRunReservationActivation::Conflict));
    }
    if request.session_activity_epoch == Some(session_activity_epoch) {
        return Ok(Some(SessionRunReservationActivation::AlreadyActivated {
            session_activity_epoch,
        }));
    }
    if request.session_activity_epoch.is_some() {
        return Ok(Some(SessionRunReservationActivation::Conflict));
    }
    Ok(match state {
        DispatchState::Reserved => None,
        DispatchState::ReservationLeased => Some(SessionRunReservationActivation::RecoveryClaimed),
        DispatchState::Pending
        | DispatchState::Leased
        | DispatchState::Awaiting
        | DispatchState::DeadLetter
        | DispatchState::Superseded => Some(SessionRunReservationActivation::Conflict),
    })
}

/// Validate and normalize one claim-fenced reservation repair decision before
/// any backend mutates its row. The returned value is the only representation
/// storage adapters may interpret, so far-future retry TTLs have identical
/// memory/SQLite/PostgreSQL behavior.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_session_run_reservation_resolution(
    resolution: SessionRunReservationResolution,
) -> Result<SessionRunReservationResolution, DispatchError> {
    match resolution {
        SessionRunReservationResolution::Admitted {
            session_activity_epoch: 0,
        } => Err(DispatchError::Rejected(
            "Session activity epoch must be nonzero".to_string(),
        )),
        SessionRunReservationResolution::Retry { reservation_ttl_ms } => {
            let reservation_ttl_ms = crate::clock::normalize_millis(reservation_ttl_ms);
            if reservation_ttl_ms == 0 {
                return Err(DispatchError::Rejected(
                    "Session Run reservation retry TTL must be nonzero".to_string(),
                ));
            }
            Ok(SessionRunReservationResolution::Retry { reservation_ttl_ms })
        }
        resolution => Ok(resolution),
    }
}

/// Validate the one existing Session-affinity encoding used for coordinated
/// child Runs. A missing parent denotes an ordinary root Run; self-affinity
/// would collapse parent and child lifecycle identities and is therefore not a
/// child admission.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn session_child_parent(
    request: &RunDispatch,
) -> Result<&awaken_agent_contract::agent::thread::Id, DispatchError> {
    let parent = request.session_thread_id.as_ref().ok_or_else(|| {
        DispatchError::Rejected(
            "bounded Session-child admission requires an explicit parent Session Thread"
                .to_string(),
        )
    })?;
    if parent == request.thread_id() {
        return Err(DispatchError::Rejected(
            "a Session child Thread must differ from its parent Session Thread".to_string(),
        ));
    }
    Ok(parent)
}

/// Return the child Thread represented by one live dispatch under `parent`.
/// Lifecycle state is deliberately irrelevant: only committed Thread archive
/// truth releases the long-lived Managed Thread slot.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn session_child_thread<'a>(
    request: &'a RunDispatch,
    parent: &awaken_agent_contract::agent::thread::Id,
) -> Option<&'a awaken_agent_contract::agent::thread::Id> {
    (request.session_thread_id.as_ref() == Some(parent) && request.thread_id() != parent)
        .then(|| request.thread_id())
}

/// Enforce a bound over distinct unarchived child Threads. A follow-up Run on an
/// already-known Thread consumes no new slot; a committed archived Thread may
/// not be revived through Run admission.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn ensure_session_child_capacity(
    request: &RunDispatch,
    admission: &SessionChildAdmission,
    known_threads: impl IntoIterator<Item = awaken_agent_contract::agent::thread::Id>,
) -> Result<(), DispatchError> {
    if admission.archived_threads.contains(request.thread_id()) {
        return Err(DispatchError::Rejected(format!(
            "Session child Thread `{}` is archived",
            request.thread_id().0
        )));
    }
    if admission
        .capacity_exempt_threads
        .contains(request.thread_id())
    {
        return Err(DispatchError::Rejected(format!(
            "capacity-exempt Session Thread `{}` cannot be admitted as an ordinary child",
            request.thread_id().0
        )));
    }
    let mut distinct = Vec::new();
    for thread in known_threads {
        if !admission.archived_threads.contains(&thread)
            && !admission.capacity_exempt_threads.contains(&thread)
            && !distinct.contains(&thread)
        {
            distinct.push(thread);
        }
    }
    if distinct.contains(request.thread_id()) || distinct.len() < admission.max_unarchived_threads {
        return Ok(());
    }
    Err(DispatchError::Rejected(format!(
        "parent Session `{}` already has the maximum {} unarchived child Threads",
        session_child_parent(request)?.0,
        admission.max_unarchived_threads,
    )))
}

/// Validate the narrow Outbox-to-fresh-Run continuation command. The message is
/// bound to that exact Run before persistence, so two continuations on one Thread
/// cannot be drained by the first claimant as generic ADR-0021 input. Scheduled
/// input and awaiting-ticket correlations retain their other canonical owners.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_outbox_continuation(
    input: &PendingInput,
    request: &RunDispatch,
    admission: &ContinuationAdmission,
) -> Result<(), DispatchError> {
    if input.run_id != *request.run_id()
        || !input.correlation_id.is_empty()
        || input.available_at_ms.is_some()
    {
        return Err(DispatchError::Rejected(
            "atomic outbox continuation requires immediate input bound to its fresh Run"
                .to_string(),
        ));
    }
    if input.thread_id != *request.thread_id() {
        return Err(DispatchError::Rejected(
            "atomic outbox continuation input and root dispatch target different Threads"
                .to_string(),
        ));
    }
    match admission {
        ContinuationAdmission::Root
            if request
                .session_thread_id
                .as_ref()
                .is_some_and(|session| session != request.thread_id()) =>
        {
            return Err(DispatchError::Rejected(
                "a Session-child outbox continuation requires SessionChild admission".to_string(),
            ));
        }
        ContinuationAdmission::SessionChild(_) => {
            session_child_parent(request)?;
        }
        ContinuationAdmission::Root => {}
    }
    if !request.activation.input.is_empty() {
        return Err(DispatchError::Rejected(
            "atomic outbox continuation requires PendingInput to be the sole message owner"
                .to_string(),
        ));
    }
    Ok(())
}

/// Validate the storage-independent half of Session-affine resume staging.
/// State/lease checks remain inside each backend transaction; this kernel keeps
/// Run, Thread, parent affinity, and activity rules identical across adapters.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_session_resume_target(
    request: &RunDispatch,
    input: &PendingInput,
    session_thread_id: &awaken_agent_contract::agent::thread::Id,
    prior_session_activity_epoch: Option<u64>,
    session_activity_epoch: u64,
) -> Result<(), DispatchError> {
    if session_activity_epoch == 0 || prior_session_activity_epoch == Some(0) {
        return Err(DispatchError::Rejected(
            "Session resume requires nonzero activity epochs".to_string(),
        ));
    }
    if input.run_id != *request.run_id() || input.thread_id != *request.thread_id() {
        return Err(DispatchError::Rejected(
            "Session resume does not match its dispatch Run and Thread".to_string(),
        ));
    }
    if request.session_thread_id.as_ref() != Some(session_thread_id) {
        return Err(DispatchError::Rejected(
            "Session resume does not match the dispatch Session affinity".to_string(),
        ));
    }
    if request.session_activity_epoch == Some(0) {
        return Err(DispatchError::Rejected(
            "Session resume has an invalid existing activity coordinate".to_string(),
        ));
    }
    if input.correlation_id.is_empty() || input.available_at_ms.is_some() {
        return Err(DispatchError::Rejected(
            "Session resume requires an immediate bound correlation".to_string(),
        ));
    }
    Ok(())
}

/// Validate the activity half of the atomic Session resume transition.
/// A new stage must still own the exact prior coordinate selected before the
/// Session root CAS. `None` may adopt only an epochless exact Session row (or a
/// row already prebound to this same new epoch); it cannot match another active
/// coordinate. Once the payload is durable, only the exact payload may replay
/// against the already-rotated coordinate.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_session_resume_activity_transition(
    current_session_activity_epoch: Option<u64>,
    prior_session_activity_epoch: Option<u64>,
    session_activity_epoch: u64,
    exact_payload_replay: bool,
) -> Result<bool, DispatchError> {
    if exact_payload_replay {
        return if current_session_activity_epoch == Some(session_activity_epoch) {
            Ok(false)
        } else {
            Err(DispatchError::Rejected(
                "durable Session resume does not match the rotated activity epoch".to_string(),
            ))
        };
    }
    let owns_expected_coordinate = match prior_session_activity_epoch {
        Some(prior) => current_session_activity_epoch == Some(prior),
        None => {
            current_session_activity_epoch.is_none()
                || current_session_activity_epoch == Some(session_activity_epoch)
        }
    };
    if !owns_expected_coordinate {
        return Err(DispatchError::Rejected(
            "Session resume does not match the expected prior activity epoch".to_string(),
        ));
    }
    Ok(true)
}

/// Classify durable reply evidence already present in the Outbox or Inbox.
/// One awaiting correlation accepts exactly one payload. The exact payload is
/// a replay; another message or a reused id with another payload is corruption.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_session_resume_evidence<'a>(
    input: &PendingInput,
    existing: impl IntoIterator<Item = &'a PendingInput>,
) -> Result<bool, DispatchError> {
    let mut exact = false;
    for candidate in existing {
        if candidate.message_id == input.message_id {
            if candidate != input {
                return Err(DispatchError::Conflict(format!(
                    "idempotency key `{}` was reused with another Session resume payload",
                    input.message_id
                )));
            }
            exact = true;
        } else if candidate.run_id == input.run_id
            && candidate.correlation_id == input.correlation_id
        {
            return Err(DispatchError::Conflict(format!(
                "Run `{}` correlation `{}` already has another durable reply",
                input.run_id.0, input.correlation_id
            )));
        }
    }
    Ok(exact)
}

#[cfg(feature = "durable")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactClaimMode {
    Runnable,
    ReservationRecovery,
    TerminalRecovery,
    RetryExhausted { max_attempts: u64 },
}

#[cfg(feature = "durable")]
impl ExactClaimMode {
    pub(crate) fn bypasses_execution_admission(self) -> bool {
        !matches!(self, Self::Runnable)
    }

    pub(crate) fn retry_limit(self) -> Option<u64> {
        match self {
            Self::RetryExhausted { max_attempts } => Some(max_attempts),
            Self::Runnable | Self::ReservationRecovery | Self::TerminalRecovery => None,
        }
    }
}

/// Select the only exact-claim mode that may own a Session reservation.
///
/// The deadline is an exclusive window for the original admitting caller.
/// Cancellation records intent but cannot shorten that window: doing so could
/// let RecoverOnly delete the row before the caller's Session CAS and leave an
/// orphan activity. A cancelled recovery lease is first returned to Reserved
/// with an already-expired deadline by the backend, so it still repairs without
/// waiting for another full lease.
#[cfg(feature = "durable")]
pub(crate) fn classify_exact_claim_mode(
    state: DispatchState,
    lease_or_reservation_deadline_ms: Option<u64>,
    now_ms: u64,
) -> ExactClaimMode {
    let expired = lease_or_reservation_deadline_ms
        .is_some_and(|deadline| deadline < crate::clock::normalize_millis(now_ms));
    if expired
        && matches!(
            state,
            DispatchState::Reserved | DispatchState::ReservationLeased
        )
    {
        ExactClaimMode::ReservationRecovery
    } else {
        ExactClaimMode::Runnable
    }
}

/// Decode SQL evidence once, then apply the storage-neutral retry policy. Query
/// filters are advisory; SQLite and PostgreSQL both call this after their
/// transactional exact-row read before advancing the claim epoch.
#[cfg(feature = "durable")]
pub(crate) fn retry_exhaustion_evidence_is_eligible(
    status: &str,
    lease_until: Option<i64>,
    attempt_count: i64,
    max_attempts: u64,
    now_ms: u64,
) -> Result<bool, DispatchError> {
    let state = DispatchState::from_db(status).ok_or_else(|| {
        DispatchError::Rejected(format!("unknown persisted dispatch state {status}"))
    })?;
    let lease_until = lease_until
        .map(crate::clock::millis_from_db)
        .transpose()
        .map_err(|error| DispatchError::Rejected(error.to_string()))?;
    let attempt_count = crate::durable_u64("dispatch attempt count", attempt_count)?;
    Ok(awaken_run_ingress_contract::retry_exhaustion_eligible(
        state,
        lease_until,
        attempt_count,
        max_attempts,
        now_ms,
    ))
}

/// Canonical host-side normalization for every pending/outbox ingress path.
/// Persistent backends store signed BIGINT values, so all implementations use
/// the same bounded representation before idempotency comparisons.
#[cfg(feature = "durable")]
pub(crate) fn normalize_pending_millis(mut input: PendingInput) -> PendingInput {
    input.available_at_ms = input.available_at_ms.map(crate::clock::normalize_millis);
    input
}

#[cfg(feature = "durable")]
pub(crate) fn installed_worker_credential_capabilities(
    worker: &crate::WorkerSnapshot,
) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, DispatchError> {
    awaken_run_ingress_contract::worker_credential_realization_capabilities(worker).map_err(
        |error| DispatchError::Rejected(format!("Worker capability evidence is invalid: {error}")),
    )
}
