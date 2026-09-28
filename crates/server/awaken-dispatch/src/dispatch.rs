//! The dispatch ports now live in `awaken-dispatch-contract` (ADR-0039 2.1),
//! re-exported here so the host's internal `crate::dispatch::*` paths and existing
//! consumers are unchanged.

pub use awaken_dispatch_contract::dispatch::*;

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn verify_physical_attempt_binding(
    transition: awaken_dispatch_contract::PhysicalAttemptTransition,
    claim: &RunClaim,
    assignment: Option<&crate::WorkerAssignment>,
    requested: Option<&AttemptExecutionBinding>,
    stored: Option<&AttemptExecutionBinding>,
) -> Result<(), DispatchError> {
    use awaken_dispatch_contract::PhysicalAttemptTransition as T;
    if !matches!(transition, T::Install | T::Replace | T::Stutter) {
        return Ok(());
    }
    if let Some(binding) = requested {
        binding.validate_for(claim)?;
        // Never recover a typed Worker identity by splitting the lease-owner
        // string. The exact registered assignment is the pre-entry authority.
        if assignment.map(|a| &a.identity) != Some(binding.worker()) {
            return Err(DispatchError::Rejected(
                "execution binding requires its exact registered Worker assignment".into(),
            ));
        }
    }
    if !attempt_binding_retry_is_exact(transition == T::Stutter, requested == stored) {
        return Err(DispatchError::Conflict(
            "physical attempt execution binding is immutable after admission".into(),
        ));
    }
    Ok(())
}

#[cfg(feature = "durable")]
pub(crate) fn decode_physical_attempt(
    run_id: &awaken_agent_contract::agent::run::Id,
    owner: Option<String>,
    epoch: Option<u64>,
    binding: Option<String>,
) -> Result<Option<PhysicalAttempt>, DispatchError> {
    match (owner, epoch, binding) {
        (None, None, None) => Ok(None),
        (Some(owner), Some(epoch), binding) => {
            let claim = RunClaim {
                run_id: run_id.clone(),
                owner,
                epoch,
            };
            let binding: Option<AttemptExecutionBinding> = binding
                .map(|json| serde_json::from_str(&json).map_err(DispatchError::unavailable))
                .transpose()?;
            if let Some(binding) = &binding {
                binding.validate_for(&claim)?;
            }
            Ok(Some(PhysicalAttempt {
                claim,
                binding,
                thread_id: None,
                session_thread_id: None,
            }))
        }
        _ => Err(corrupt_physical_attempt()),
    }
}

/// Translate the existing physical-attempt kernel once for every backend.
/// Stores provide one locked snapshot and apply only the returned mutation;
/// neither the result/error policy nor the handoff exception belongs to SQL.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn decide_physical_attempt_admission(
    current: bool,
    slot: awaken_dispatch_contract::PhysicalAttemptSlot,
    handoff: bool,
    thread_peer_occupied: bool,
) -> Result<
    (
        awaken_dispatch_contract::PhysicalAttemptTransition,
        AttemptAdmission,
    ),
    DispatchError,
> {
    use awaken_dispatch_contract::{
        PhysicalAttemptTransition as T, begin_physical_attempt, handoff_physical_attempt,
    };
    let transition = if handoff {
        handoff_physical_attempt(current, slot, thread_peer_occupied)
    } else {
        begin_physical_attempt(current, slot, thread_peer_occupied)
    };
    let outcome = match transition {
        T::Install => AttemptAdmission::Applied,
        T::Replace => AttemptAdmission::Replaced,
        T::Stutter => AttemptAdmission::AlreadyApplied,
        T::Blocked => AttemptAdmission::Blocked,
        T::Fenced => AttemptAdmission::Fenced,
        T::Corrupt => return Err(corrupt_physical_attempt()),
        T::Clear => unreachable!("attempt admission never clears a slot"),
    };
    Ok((transition, outcome))
}

/// The same exact-old-slot ACK decision for Memory, SQLite and PostgreSQL.
/// The caller may have lost its lease: finishing grants no new execution or
/// commit authority. Read failures must be returned before calling this helper.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn decide_physical_attempt_finish(
    slot: awaken_dispatch_contract::PhysicalAttemptSlot,
) -> Result<
    (
        awaken_dispatch_contract::PhysicalAttemptTransition,
        SettleOutcome,
    ),
    DispatchError,
> {
    use awaken_dispatch_contract::{PhysicalAttemptTransition as T, finish_physical_attempt};
    let transition = finish_physical_attempt(slot);
    let outcome = match transition {
        T::Clear | T::Stutter => SettleOutcome::Applied,
        T::Fenced => SettleOutcome::Fenced,
        T::Corrupt => return Err(corrupt_physical_attempt()),
        T::Install | T::Replace | T::Blocked => unreachable!("finish never admits an attempt"),
    };
    Ok((transition, outcome))
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
fn corrupt_physical_attempt() -> DispatchError {
    DispatchError::Rejected(
        "persisted physical attempt slot is not an owner/epoch pair".to_string(),
    )
}

#[cfg(test)]
mod physical_attempt_tests {
    use super::*;
    use awaken_dispatch_contract::{PhysicalAttemptSlot as S, PhysicalAttemptTransition as T};

    #[test]
    fn every_backend_uses_the_same_admission_and_finish_result_policy() {
        // Causes: C1 current/stale claim; C2 empty/exact/other/torn slot;
        // C3 ordinary admission/verified continuation handoff.
        // Effects and decision table (all 16 admission combinations):
        // R1 stale + any + any -> Fenced without inspecting corrupt state.
        // R2 current + empty + any -> Install / Applied.
        // R3 current + exact + any -> Stutter / AlreadyApplied.
        // R4 current + other + ordinary -> Blocked / Blocked.
        // R5 current + other + handoff -> Replace / Replaced.
        // R6 current + torn + any -> Rejected; never silently clear the slot.
        // Finish has no lease precondition: R7 empty -> Stutter / Applied;
        // R8 exact -> Clear / Applied; R9 other -> Fenced; R10 torn -> Rejected.
        // The explicit table is independent of the production kernel; testing
        // against another call to that kernel would merely repeat its verdict.
        for current in [false, true] {
            for handoff in [false, true] {
                for slot in [S::Empty, S::ExactClaim, S::OtherClaim, S::Corrupt] {
                    let expected = match (current, slot, handoff) {
                        (false, _, _) => Some((T::Fenced, AttemptAdmission::Fenced)),
                        (true, S::Empty, _) => Some((T::Install, AttemptAdmission::Applied)),
                        (true, S::ExactClaim, _) => {
                            Some((T::Stutter, AttemptAdmission::AlreadyApplied))
                        }
                        (true, S::OtherClaim, false) => {
                            Some((T::Blocked, AttemptAdmission::Blocked))
                        }
                        (true, S::OtherClaim, true) => {
                            Some((T::Replace, AttemptAdmission::Replaced))
                        }
                        (true, S::Corrupt, _) => None,
                    };
                    // P1 peer absent => base table; P2 occupied peer cannot
                    // grant Install/Replace/Stutter, even outside Running.
                    let occupied = decide_physical_attempt_admission(current, slot, handoff, true);
                    let peer_expected = match (current, slot) {
                        (false, _) => Some((T::Fenced, AttemptAdmission::Fenced)),
                        (true, S::Corrupt) => None,
                        (true, _) => Some((T::Blocked, AttemptAdmission::Blocked)),
                    };
                    match peer_expected {
                        Some(expected) => assert_eq!(occupied.unwrap(), expected, "P2"),
                        None => assert!(matches!(occupied, Err(DispatchError::Rejected(_))), "P2"),
                    }
                    let actual = decide_physical_attempt_admission(current, slot, handoff, false);
                    match expected {
                        Some(expected) => {
                            let actual = actual.unwrap();
                            assert_eq!(actual, expected);
                            assert_eq!(
                                actual.1.permits_pre_start_no_effect_ack(),
                                actual.0 == T::Install,
                                "only the fresh-install provenance can certify no pre-start effect"
                            );
                        }
                        None => assert!(matches!(actual, Err(DispatchError::Rejected(_)))),
                    }
                }
            }
        }
        for (slot, expected) in [
            (S::Empty, Some((T::Stutter, SettleOutcome::Applied))),
            (S::ExactClaim, Some((T::Clear, SettleOutcome::Applied))),
            (S::OtherClaim, Some((T::Fenced, SettleOutcome::Fenced))),
            (S::Corrupt, None),
        ] {
            let actual = decide_physical_attempt_finish(slot);
            match expected {
                Some(expected) => assert_eq!(actual.unwrap(), expected),
                None => assert!(matches!(actual, Err(DispatchError::Rejected(_)))),
            }
        }
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
use awaken_dispatch_contract::{DispatchAdmissionShape, DispatchIdentityScope, RunDispatch};

/// Backend-neutral shape read while locking one claimed dispatch epoch. SQLite
/// stores the request as JSON text and PostgreSQL decodes it through `Json<T>`,
/// but both must interpret the remaining fence columns identically.
#[cfg(feature = "durable")]
pub(crate) type ClaimEpochStorageRow<T> = (
    i64,
    Option<String>,
    Option<i64>,
    T,
    i64,
    Option<String>,
    Option<i64>,
);

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

/// Incoming identity views compared with one permanent completion tombstone.
/// The generic value is the exact canonical byte/fingerprint value selected by
/// [`RunDispatch`]; storage adapters never supply a precomputed match bit.
#[cfg(any(feature = "durable", test, feature = "test-support", kani))]
enum CompletedIncomingIdentity<'a, T: ?Sized> {
    SessionCommand {
        current: Option<&'a T>,
        legacy: &'a T,
    },
    FullDispatch(&'a T),
}

/// One completed-row replay decision shared by memory, SQLite and PostgreSQL.
/// A missing tombstone fingerprint or any changed exact identity conflicts.
#[cfg(any(feature = "durable", test, feature = "test-support", kani))]
fn completed_run_identity_replays<T: Eq + ?Sized>(
    stored: Option<&T>,
    incoming: CompletedIncomingIdentity<'_, T>,
) -> bool {
    let Some(stored) = stored else {
        return false;
    };
    match incoming {
        CompletedIncomingIdentity::SessionCommand { current, legacy } => {
            current.is_some_and(|current| stored == current || stored == legacy)
        }
        CompletedIncomingIdentity::FullDispatch(canonical) => stored == canonical,
    }
}

#[cfg(test)]
mod completed_identity_tests {
    use super::{CompletedIncomingIdentity, completed_run_identity_replays};

    #[test]
    fn completion_tombstone_exact_identity_table_is_closed() {
        // Causes: C1 the permanent completion row has/has-not a fingerprint;
        // C2 a full child dispatch has exact/changed canonical identity; C3 a
        // Session command has a current exact/changed fingerprint or only
        // legacy bytes. Effects: E1 exact full/current/legacy retry reuses the
        // tombstone; E2 missing, changed, or unverifiable identity conflicts and
        // cannot insert work. The adapters consume this owner under their
        // existing transaction; this test adds no parallel receipt.
        //
        // | Rule | Stored | Incoming scope/identity | Effect |
        // |---|---|---|---|
        // | T1 | exact | full exact | E1 replay |
        // | T2 | exact | full changed | E2 conflict |
        // | T3 | exact | current exact | E1 replay |
        // | T4 | legacy | current present + legacy exact | E1 replay |
        // | T5 | exact | current absent | E2 conflict |
        // | T6 | absent | any | E2 conflict |
        let exact = b"exact".as_slice();
        let changed = b"changed".as_slice();
        assert!(completed_run_identity_replays(
            Some(exact),
            CompletedIncomingIdentity::FullDispatch(exact),
        ));
        assert!(!completed_run_identity_replays(
            Some(exact),
            CompletedIncomingIdentity::FullDispatch(changed),
        ));
        assert!(completed_run_identity_replays(
            Some(exact),
            CompletedIncomingIdentity::SessionCommand {
                current: Some(exact),
                legacy: changed,
            },
        ));
        assert!(completed_run_identity_replays(
            Some(exact),
            CompletedIncomingIdentity::SessionCommand {
                current: Some(changed),
                legacy: exact,
            },
        ));
        assert!(!completed_run_identity_replays(
            Some(exact),
            CompletedIncomingIdentity::SessionCommand {
                current: None,
                legacy: exact,
            },
        ));
        assert!(!completed_run_identity_replays(
            None,
            CompletedIncomingIdentity::FullDispatch(exact),
        ));
    }
}

#[cfg(kani)]
mod completed_identity_proofs {
    use super::{CompletedIncomingIdentity, completed_run_identity_replays};

    /// @obligation session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
    /// @obligation session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
    /// @kani default-features=false
    #[kani::proof]
    fn agent_message_completed_tombstone_replays_only_exact_dispatch_identity() {
        // Kani exhausts two-byte exact identity values, tombstone presence, the
        // actual full-dispatch versus Session-command identity variants, and
        // current-fingerprint presence. The production helper compares values,
        // not caller-supplied match Booleans. Exact completed replay is the only
        // successful child path; changed or unverifiable payload stutters the
        // store through the surrounding conflict result. Hash/serialization and
        // adapter transactions remain explicit TCB, not claims of this harness.
        let stored = [kani::any::<u8>(), kani::any::<u8>()];
        let canonical = [kani::any::<u8>(), kani::any::<u8>()];
        let current = [kani::any::<u8>(), kani::any::<u8>()];
        let legacy = [kani::any::<u8>(), kani::any::<u8>()];
        let stored_present = kani::any::<bool>();
        let current_present = kani::any::<bool>();
        let session_command = kani::any::<bool>();
        let stored_ref = stored_present.then_some(&stored);
        let incoming = if session_command {
            CompletedIncomingIdentity::SessionCommand {
                current: current_present.then_some(&current),
                legacy: &legacy,
            }
        } else {
            CompletedIncomingIdentity::FullDispatch(&canonical)
        };
        let replay = completed_run_identity_replays(stored_ref, incoming);
        let expected = stored_present
            && if session_command {
                current_present && (stored == current || stored == legacy)
            } else {
                stored == canonical
            };
        assert_eq!(replay, expected);
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn decide_run_identity(
    stored: StoredRunIdentity<'_>,
    incoming: &RunDispatch,
) -> Result<RunIdentityDecision, DispatchError> {
    let replay = match stored {
        StoredRunIdentity::Absent => return Ok(RunIdentityDecision::New),
        StoredRunIdentity::Live(existing) => existing.same_admission_dispatch(incoming),
        StoredRunIdentity::Completed(existing) => match incoming.identity_scope {
            DispatchIdentityScope::SessionCommand => {
                let legacy = incoming.legacy_session_reservation_fingerprint();
                completed_run_identity_replays(
                    existing,
                    CompletedIncomingIdentity::SessionCommand {
                        current: incoming
                            .session_command_fingerprint
                            .as_ref()
                            .filter(|fingerprint| fingerprint.is_current())
                            .map(|fingerprint| fingerprint.as_str()),
                        legacy: legacy.as_str(),
                    },
                )
            }
            DispatchIdentityScope::FullDispatch => {
                let canonical = incoming.canonical_fingerprint();
                completed_run_identity_replays(
                    existing,
                    CompletedIncomingIdentity::FullDispatch(canonical.as_str()),
                )
            }
        },
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
        .is_some_and(awaken_session_execution_contract::SessionRunCommandFingerprint::is_current)
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
        .all(awaken_dispatch_contract::session_run_replacement_candidate_is_safe)
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
    session_activity_epoch: Option<u64>,
) -> Result<(), DispatchError> {
    if session_activity_epoch == Some(0) || prior_session_activity_epoch == Some(0) {
        return Err(DispatchError::Rejected(
            "Session resume requires nonzero activity epochs".to_string(),
        ));
    }
    if input.run_id != *request.run_id() || input.thread_id != *request.thread_id() {
        return Err(DispatchError::Rejected(
            "Session resume does not match its dispatch Run and Thread".to_string(),
        ));
    }
    let activity = awaken_session_execution_contract::SessionThreadToolReplyActivity::from_dispatch(
        request.thread_id(),
        request.session_thread_id.as_ref(),
        request.session_activity_epoch,
        session_thread_id,
    );
    let ownership_matches = match activity {
        Some(awaken_session_execution_contract::SessionThreadToolReplyActivity::Session {
            ..
        }) => session_activity_epoch.is_some(),
        Some(awaken_session_execution_contract::SessionThreadToolReplyActivity::ThreadExtension) => {
            session_activity_epoch.is_none() && prior_session_activity_epoch.is_none()
        }
        None => false,
    };
    if !ownership_matches {
        return Err(DispatchError::Rejected(
            "Session resume does not match the dispatch Session affinity or ownership".to_string(),
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
    session_activity_epoch: Option<u64>,
    exact_payload_replay: bool,
) -> Result<bool, DispatchError> {
    if exact_payload_replay {
        return if current_session_activity_epoch == session_activity_epoch {
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
                || current_session_activity_epoch == session_activity_epoch
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

#[cfg(any(feature = "durable", test, kani))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactClaimMode {
    Runnable,
    ReservationRecovery,
    TerminalRecovery,
    RetryExhausted { max_attempts: u64 },
}

#[cfg(any(feature = "durable", test, kani))]
impl ExactClaimMode {
    #[cfg(any(feature = "durable", test))]
    pub(crate) fn bypasses_execution_admission(self) -> bool {
        !matches!(self, Self::Runnable)
    }

    #[cfg(feature = "durable")]
    pub(crate) fn retry_limit(self) -> Option<u64> {
        match self {
            Self::RetryExhausted { max_attempts } => Some(max_attempts),
            Self::Runnable | Self::ReservationRecovery | Self::TerminalRecovery => None,
        }
    }
}

/// Advance one claim while moving its already-persisted request through the
/// canonical transition unchanged. The mode comes from locked row evidence;
/// this function accepts no newly resolved request that could replace the
/// dispatch's frozen execution pin.
#[cfg(any(feature = "durable", test, kani))]
pub(crate) fn claim_with_frozen<T>(
    current: awaken_dispatch_contract::DispatchTransition,
    frozen: T,
    mode: ExactClaimMode,
) -> Result<
    Option<(awaken_dispatch_contract::DispatchTransition, T)>,
    awaken_dispatch_contract::DispatchTransitionError,
> {
    let claimed = if mode == ExactClaimMode::ReservationRecovery {
        current.recover_reservation()
    } else {
        current.claim()
    }?;
    Ok(claimed.map(|transition| (transition, frozen)))
}

/// Select the only exact-claim mode that may own a Session reservation.
///
/// The deadline is an exclusive window for the original admitting caller.
/// Cancellation records intent but cannot shorten that window: doing so could
/// let RecoverOnly delete the row before the caller's Session CAS and leave an
/// orphan activity. A cancelled recovery lease is first returned to Reserved
/// with an already-expired deadline by the backend, so it still repairs without
/// waiting for another full lease.
#[cfg(any(feature = "durable", test))]
pub(crate) fn classify_exact_claim_mode(
    state: DispatchState,
    lease_or_reservation_deadline_ms: Option<u64>,
    now_ms: u64,
) -> ExactClaimMode {
    let expired = crate::clock::deadline_has_passed(
        lease_or_reservation_deadline_ms,
        crate::clock::normalize_millis(now_ms),
    );
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
    Ok(awaken_dispatch_contract::retry_exhaustion_eligible(
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
#[cfg(any(feature = "durable", test))]
pub(crate) fn normalize_pending_millis(mut input: PendingInput) -> PendingInput {
    input.available_at_ms = input.available_at_ms.map(crate::clock::normalize_millis);
    input
}

#[cfg(any(feature = "durable", test))]
pub(crate) fn installed_worker_credential_capabilities(
    worker: &crate::WorkerSnapshot,
) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, DispatchError> {
    awaken_dispatch_contract::worker_credential_realization_capabilities(worker).map_err(
        |error| DispatchError::Rejected(format!("Worker capability evidence is invalid: {error}")),
    )
}

#[cfg(kani)]
mod claim_binding_proofs {
    use super::*;

    fn arbitrary_state() -> DispatchState {
        match kani::any::<u8>() % 7 {
            0 => DispatchState::Reserved,
            1 => DispatchState::ReservationLeased,
            2 => DispatchState::Pending,
            3 => DispatchState::Leased,
            4 => DispatchState::Awaiting,
            5 => DispatchState::DeadLetter,
            _ => DispatchState::Superseded,
        }
    }

    fn arbitrary_mode() -> ExactClaimMode {
        match kani::any::<u8>() % 4 {
            0 => ExactClaimMode::Runnable,
            1 => ExactClaimMode::ReservationRecovery,
            2 => ExactClaimMode::TerminalRecovery,
            _ => ExactClaimMode::RetryExhausted {
                max_attempts: kani::any(),
            },
        }
    }

    /// @obligation delegation.independent_child_scheduling_lease
    /// @obligation remote_worker.execution_requires_claimed_snapshot
    /// @obligation session_run.recovery_claim_is_admission_only
    /// @obligation worker.retry_preserves_pinned_binding
    /// @kani default-features=false
    #[kani::proof]
    fn dispatch_claim_preserves_every_frozen_binding() {
        // Cause/effect graph: C1 selects ordinary, terminal, exhausted, or
        // reservation recovery; C2 the row is valid or invalid for that path;
        // C3 the epoch is incrementable or exhausted; C4 the frozen binding has
        // arbitrary primary, provider, route, and fallback-order coordinates.
        // Effects: E1 a successful claim returns every C4 coordinate exactly;
        // E2 its phase is Leased or ReservationLeased according to C1; E3 its
        // epoch is exactly old+1 and cancellation is unchanged; E4 an invalid
        // phase is a no-op; E5 an exhausted valid phase fails closed.
        //
        // | Rule | Path | Phase valid | Epoch | Effects |
        // | R1 | ordinary/terminal/exhausted | yes | open | E1+E2+E3 |
        // | R2 | reservation | yes | open | E1+E2+E3 |
        // | R3 | any | no | any | E4 |
        // | R4 | any | yes | max | E5 |
        //
        // Constraint: the generic production kernel cannot inspect or rebuild
        // `T`; this four-axis witness makes a mixed or reordered binding visible
        // while the Rust move preserves the complete `RunDispatch` in callers.
        let state = awaken_dispatch_contract::DispatchTransition {
            state: arbitrary_state(),
            lease_epoch: kani::any(),
            cancellation_requested: kani::any(),
        };
        let mode = arbitrary_mode();
        let frozen = [
            kani::any::<u8>(),
            kani::any::<u8>(),
            kani::any::<u8>(),
            kani::any::<u8>(),
        ];
        let result = claim_with_frozen(state, frozen, mode);
        let reservation_recovery = mode == ExactClaimMode::ReservationRecovery;
        let valid = if reservation_recovery {
            matches!(
                state.state,
                DispatchState::Reserved | DispatchState::ReservationLeased
            )
        } else {
            matches!(
                state.state,
                DispatchState::Pending | DispatchState::Leased | DispatchState::Awaiting
            )
        };

        if !valid {
            assert_eq!(result, Ok(None), "R3/E4");
        } else if state.lease_epoch == u64::MAX {
            assert_eq!(
                result,
                Err(awaken_dispatch_contract::DispatchTransitionError::LeaseEpochExhausted),
                "R4/E5"
            );
        } else {
            let (claimed, returned) = result.unwrap().expect("R1-R2 successful claim");
            assert_eq!(returned, frozen, "R1-R2/E1");
            assert_eq!(
                claimed.state,
                if reservation_recovery {
                    DispatchState::ReservationLeased
                } else {
                    DispatchState::Leased
                },
                "R1-R2/E2"
            );
            assert_eq!(claimed.lease_epoch, state.lease_epoch + 1, "R1-R2/E3");
            assert_eq!(
                claimed.cancellation_requested, state.cancellation_requested,
                "R1-R2/E3"
            );
        }
    }
}
