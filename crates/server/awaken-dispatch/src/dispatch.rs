// @formal entrypoint delegation.independent_child_scheduling_lease
// @formal entrypoint remote_worker.execution_requires_claimed_snapshot
// @formal entrypoint session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
// @formal entrypoint session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint session_run.reservation_never_executes
// @formal entrypoint session_run.reservation_precedes_activity
// @formal entrypoint worker.retry_preserves_pinned_binding
// @formal evidence run_ingress.a_deadline_passes_once_it_is_reached
//! The dispatch ports now live in `awaken-dispatch-contract` (ADR-0039 2.1),
//! re-exported here so the host's internal `crate::dispatch::*` paths and existing
//! consumers are unchanged.

pub use awaken_dispatch_contract::dispatch::*;
#[cfg(any(feature = "durable", test, feature = "test-support"))]
use awaken_dispatch_contract::{PhysicalAttemptSlot, PhysicalAttemptTransition};

/// Compile entry inventory from the same locked dispatch row as admission.
/// This retains the original identity and adds no execution/closure authority.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn physical_admission_operation(
    claim: &RunClaim,
    binding: Option<&AttemptExecutionBinding>,
    request: &RunDispatch,
) -> awaken_dispatch_contract::DispatchOperation {
    awaken_dispatch_contract::DispatchOperation::AttemptAdmitted {
        attempt: PhysicalAttempt {
            claim: claim.clone(),
            binding: binding.cloned(),
            thread_id: Some(request.thread_id().clone()),
            session_thread_id: request.session_thread_id.clone(),
            closing: false,
        },
    }
}

/// One private stored alternative in the original operation storage. The public
/// diagnostics enum retains delivery facts and physical admission. No second
/// history or fake delivery transition carries independent closure facts.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum StoredDispatchOperation {
    Diagnostic(awaken_dispatch_contract::DispatchOperation),
    PhysicalFact(AttemptPhysicalFact),
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl From<&awaken_dispatch_contract::DispatchOperation> for StoredDispatchOperation {
    fn from(operation: &awaken_dispatch_contract::DispatchOperation) -> Self {
        Self::Diagnostic(operation.clone())
    }
}
#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl From<awaken_dispatch_contract::DispatchOperation> for StoredDispatchOperation {
    fn from(operation: awaken_dispatch_contract::DispatchOperation) -> Self {
        Self::Diagnostic(operation)
    }
}
#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl From<&AttemptPhysicalFact> for StoredDispatchOperation {
    fn from(fact: &AttemptPhysicalFact) -> Self {
        Self::PhysicalFact(fact.clone())
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl StoredDispatchOperation {
    #[cfg(any(feature = "durable", test))]
    pub(crate) fn decode(encoded: &str) -> Result<Self, DispatchError> {
        let operation: Self = serde_json::from_str(encoded).map_err(DispatchError::unavailable)?;
        if let Self::Diagnostic(awaken_dispatch_contract::DispatchOperation::AttemptAdmitted {
            attempt,
        }) = &operation
        {
            attempt.validate_admission()?;
        }
        if let Some(fact) = operation.physical_fact() {
            fact.validate()?;
        }
        Ok(operation)
    }
    pub(crate) fn run_id(&self) -> &awaken_agent_contract::agent::run::Id {
        match self {
            Self::Diagnostic(operation) => operation.run_id(),
            Self::PhysicalFact(fact) => &fact.attempt().claim.run_id,
        }
    }
    pub(crate) fn physical_fact(&self) -> Option<&AttemptPhysicalFact> {
        match self {
            Self::Diagnostic(_) => None,
            Self::PhysicalFact(fact) => Some(fact),
        }
    }
    pub(crate) fn matches(&self, purpose: OperationReadPurpose<'_>) -> bool {
        match (self, purpose) {
            (Self::Diagnostic(_), OperationReadPurpose::Diagnostics)
            | (Self::PhysicalFact(_), OperationReadPurpose::AttemptClosure) => true,
            (
                Self::Diagnostic(awaken_dispatch_contract::DispatchOperation::AttemptAdmitted {
                    attempt,
                }),
                OperationReadPurpose::Admissions {
                    session_thread,
                    member_threads,
                },
            ) => {
                attempt.session_thread_id.as_ref() == Some(session_thread)
                    || attempt
                        .thread_id
                        .as_ref()
                        .is_some_and(|id| member_threads.contains(id))
            }
            _ => false,
        }
    }
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Clone, Copy)]
pub(crate) enum OperationReadPurpose<'a> {
    Diagnostics,
    AttemptClosure,
    Admissions {
        session_thread: &'a awaken_agent_contract::agent::thread::Id,
        member_threads: &'a [awaken_agent_contract::agent::thread::Id],
    },
}

// Query exclusions, not another codec: unrecognized tags still reach decode_row.
// Missing a future non-admission tag merely makes it an extra decoded raw row.
#[cfg(feature = "durable")]
pub(crate) const NON_ADMISSION_OPERATION_TAGS: &[&str] = &[
    "claimed",
    "lease_lost",
    "reclaimed",
    "settled",
    "dead_lettered",
    "attempt_acknowledgement",
    "attempt_closure",
];

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_admission_scope(
    root: &awaken_agent_contract::agent::thread::Id,
    members: &[awaken_agent_contract::agent::thread::Id],
) -> Result<(), DispatchError> {
    if root.0.trim().is_empty()
        || !members.contains(root)
        || members.iter().any(|id| id.0.trim().is_empty())
    {
        return Err(DispatchError::Rejected(
            "admission scope must contain its exact nonempty root and members".into(),
        ));
    }
    Ok(())
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn diagnostic_page(
    cursor: awaken_dispatch_contract::DispatchCursor,
    stored: Vec<StoredDispatchEvent>,
) -> awaken_dispatch_contract::DispatchPage {
    let events = stored
        .into_iter()
        .filter_map(StoredDispatchEvent::diagnostic)
        .collect::<Vec<_>>();
    let next_cursor = events.last().map_or(cursor, |event| event.cursor);
    awaken_dispatch_contract::DispatchPage {
        events,
        next_cursor,
    }
}

/// The sole Memory operation element, and the canonical decoded SQL element.
/// Public/internal pages are projections, never mirrored persistence.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredDispatchEvent {
    pub(crate) cursor: awaken_dispatch_contract::DispatchCursor,
    pub(crate) recorded_at_ms: Option<u64>,
    pub(crate) operation: StoredDispatchOperation,
}
#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl StoredDispatchEvent {
    /// One stored-row law for both native adapters. Negative coordinates and
    /// a different encoded subject fail before either purpose can project a
    /// value; nullable historical time remains explicit None.
    #[cfg(any(feature = "durable", test))]
    pub(crate) fn decode_row(
        sequence: i64,
        recorded_at_ms: Option<i64>,
        run_id: &str,
        encoded: &str,
    ) -> Result<Self, DispatchError> {
        let cursor =
            awaken_dispatch_contract::DispatchCursor(u64::try_from(sequence).map_err(|_| {
                DispatchError::Rejected("persisted dispatch operation sequence is negative".into())
            })?);
        let operation = StoredDispatchOperation::decode(encoded)?;
        if operation.run_id().0 != run_id {
            return Err(DispatchError::Rejected(
                "operation storage subject differs from its original Run".into(),
            ));
        }
        let recorded_at_ms = recorded_at_ms.map(u64::try_from).transpose().map_err(|_| {
            DispatchError::Rejected("persisted dispatch operation time is negative".into())
        })?;
        Ok(Self {
            cursor,
            recorded_at_ms,
            operation,
        })
    }

    pub(crate) fn diagnostic(self) -> Option<awaken_dispatch_contract::DispatchOperationalEvent> {
        match self.operation {
            StoredDispatchOperation::Diagnostic(operation) => {
                Some(awaken_dispatch_contract::DispatchOperationalEvent {
                    cursor: self.cursor,
                    recorded_at_ms: self.recorded_at_ms,
                    operation,
                })
            }
            StoredDispatchOperation::PhysicalFact(_) => None,
        }
    }
    pub(crate) fn attempt_closure(self) -> Option<AttemptClosureEvent> {
        match self.operation {
            StoredDispatchOperation::PhysicalFact(record) => Some(AttemptClosureEvent {
                cursor: self.cursor,
                recorded_at_ms: self.recorded_at_ms,
                record,
            }),
            StoredDispatchOperation::Diagnostic(_) => None,
        }
    }
}

/// The two existing finish purposes share one storage plan. Ordinary local
/// observations never satisfy the independent closing ACK gate.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
#[derive(Clone, Copy)]
pub(crate) enum AttemptFinish<'a> {
    Ordinary(Option<AttemptReturnCause>),
    Independent(&'a AttemptClosureRecord),
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
impl AttemptFinish<'_> {
    pub(crate) fn requires_fact(self) -> bool {
        !matches!(self, Self::Ordinary(None))
    }
}

/// Canonical cause/identity/replay law around the original finish kernel.
/// Adapters supply their locked original row and original operation prefix;
/// only the returned Clear plan may atomically append and remove that slot.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn plan_physical_attempt_finish<'a>(
    claim: &RunClaim,
    purpose: AttemptFinish<'_>,
    slot: PhysicalAttemptSlot,
    closing: bool,
    original: Option<&PhysicalAttempt>,
    prior: impl IntoIterator<Item = &'a AttemptPhysicalFact>,
    row_exists: bool,
) -> Result<
    (
        PhysicalAttemptTransition,
        SettleOutcome,
        Option<AttemptPhysicalFact>,
    ),
    DispatchError,
> {
    if let AttemptFinish::Independent(record) = purpose {
        record.validate()?;
    }
    let (transition, mut outcome) = if row_exists {
        decide_physical_attempt_finish(
            slot,
            closing,
            matches!(purpose, AttemptFinish::Independent(_)),
        )?
    } else {
        (PhysicalAttemptTransition::Stutter, SettleOutcome::Fenced)
    };
    if !purpose.requires_fact() || transition == PhysicalAttemptTransition::Fenced {
        return Ok((transition, outcome, None));
    }
    let mut found = None;
    for previous in prior {
        previous.validate()?;
        if previous.attempt().claim == *claim {
            if found.is_some() {
                return Err(DispatchError::Conflict(
                    "duplicate physical attempt fact subjects".into(),
                ));
            }
            found = Some(previous);
        }
    }
    if transition == PhysicalAttemptTransition::Stutter {
        outcome = match (purpose, found) {
            (
                AttemptFinish::Independent(expected),
                Some(AttemptPhysicalFact::IndependentClosure { record: previous }),
            ) if previous == expected => SettleOutcome::Applied,
            (
                AttemptFinish::Ordinary(Some(cause)),
                Some(AttemptPhysicalFact::Acknowledgement { record: previous }),
            ) if previous.cause() == cause => SettleOutcome::Applied,
            (_, Some(_)) => return Err(DispatchError::Conflict(
                "physical attempt replay changed its original purpose, cause, subject or reference"
                    .into(),
            )),
            (_, None) => SettleOutcome::Fenced,
        };
        return Ok((transition, outcome, None));
    }
    if found.is_some() {
        return Err(DispatchError::Conflict(
            "a retired fact subject cannot be asserted for another active entry".into(),
        ));
    }
    let original = original.ok_or_else(corrupt_physical_attempt)?;
    if original.claim != *claim {
        return Err(DispatchError::Conflict(
            "finish fact differs from the original physical claim".into(),
        ));
    }
    let fact = match purpose {
        AttemptFinish::Independent(record) => {
            if original != record.attempt() {
                return Err(DispatchError::Conflict(
                    "closure subject differs from the frozen physical attempt".into(),
                ));
            }
            AttemptPhysicalFact::IndependentClosure {
                record: record.clone(),
            }
        }
        AttemptFinish::Ordinary(Some(cause)) => AttemptPhysicalFact::Acknowledgement {
            record: AttemptAcknowledgementRecord::try_new(original.clone(), cause)?,
        },
        AttemptFinish::Ordinary(None) => unreachable!("legacy ACK has no fact"),
    };
    Ok((transition, outcome, Some(fact)))
}

// SQL projection of the one Thread-peer exclusion rule. SQLite and PostgreSQL
// use these exact expressions inside their existing transactions; adapter lock
// and clock ownership are unchanged. Cancellation excludes only Running peers,
// while ordinary claims also exclude Awaiting peers, always excluding the
// candidate itself. The ControlDispatch conformance decision table owns the
// executable assertions rather than matching SQL strings in another test.
#[cfg(feature = "durable")]
pub(crate) fn thread_available_for_claim(prefix: &str) -> String {
    let cancellation_available = no_running_peer(prefix);
    format!(
        "((d.cancel_requested = 1 AND {cancellation_available}) \
         OR (d.cancel_requested = 0 AND NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.run_id <> d.run_id \
         AND r.status IN ('running', 'reservation_running', 'awaiting'))))"
    )
}

#[cfg(feature = "durable")]
pub(crate) fn no_running_peer(prefix: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM {prefix}_dispatch r \
         WHERE r.thread_id = d.thread_id AND r.run_id <> d.run_id \
         AND r.status IN ('running', 'reservation_running'))"
    )
}

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

/// Validate retained physical facts before either summary filtering or an
/// exact read. Only a complete absence is empty; a torn pair, orphan binding
/// or closing-only residue is unknown/corrupt, never absence evidence.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn observed_physical_attempt_is_present(
    owner_present: bool,
    epoch_present: bool,
    binding_present: bool,
    closing: bool,
) -> Result<bool, DispatchError> {
    use awaken_dispatch_contract::PhysicalAttemptSlot;
    match PhysicalAttemptSlot::from_facts(owner_present, epoch_present, false, false) {
        PhysicalAttemptSlot::Empty if !binding_present && !closing => Ok(false),
        PhysicalAttemptSlot::Empty | PhysicalAttemptSlot::Corrupt => {
            Err(corrupt_physical_attempt())
        }
        PhysicalAttemptSlot::ExactClaim | PhysicalAttemptSlot::OtherClaim => Ok(true),
    }
}

/// One native row projection used by exact physical reads and retirement.
/// Column order is original owner, epoch, binding, request, closing. Request is
/// adapter encoding (SQLite text or PostgreSQL Json), not another domain state;
/// the shared physical decoder remains the only retained-facts law.
#[cfg(feature = "durable")]
pub(crate) type PhysicalAttemptStorageRow<Request> =
    (Option<String>, Option<i64>, Option<String>, Request, bool);

#[cfg(feature = "durable")]
pub(crate) fn decode_physical_attempt(
    run_id: &awaken_agent_contract::agent::run::Id,
    owner: Option<String>,
    epoch: Option<u64>,
    binding: Option<String>,
    closing: bool,
) -> Result<Option<PhysicalAttempt>, DispatchError> {
    observed_physical_attempt_is_present(
        owner.is_some(),
        epoch.is_some(),
        binding.is_some(),
        closing,
    )?;
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
                closing,
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
    closing: bool,
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
        handoff_physical_attempt(current, slot, thread_peer_occupied, closing)
    } else {
        begin_physical_attempt(current, slot, thread_peer_occupied, closing)
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
    closing: bool,
    closure_evidence: bool,
) -> Result<
    (
        awaken_dispatch_contract::PhysicalAttemptTransition,
        SettleOutcome,
    ),
    DispatchError,
> {
    use awaken_dispatch_contract::{PhysicalAttemptTransition as T, finish_physical_attempt};
    let transition = finish_physical_attempt(slot, closing, closure_evidence);
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

#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn decide_physical_attempt_close(
    slot: awaken_dispatch_contract::PhysicalAttemptSlot,
) -> Result<SettleOutcome, DispatchError> {
    if slot == awaken_dispatch_contract::PhysicalAttemptSlot::Corrupt {
        return Err(corrupt_physical_attempt());
    }
    Ok(
        if awaken_dispatch_contract::can_close_physical_attempt(slot) {
            SettleOutcome::Applied
        } else {
            SettleOutcome::Fenced
        },
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
        // Retained-read decision table: V1 all four facts absent => false;
        // V2 complete owner+epoch with any binding/closing => true; V3 a half
        // pair or binding/closing residue on an empty pair => error. The
        // literal 16-row table covers every combination, independently of the
        // adapter projection; corrupt facts must never enter the false branch.
        for (owner, epoch, binding, closing, expected) in [
            (false, false, false, false, Some(false)),
            (false, false, false, true, None),
            (false, false, true, false, None),
            (false, false, true, true, None),
            (false, true, false, false, None),
            (false, true, false, true, None),
            (false, true, true, false, None),
            (false, true, true, true, None),
            (true, false, false, false, None),
            (true, false, false, true, None),
            (true, false, true, false, None),
            (true, false, true, true, None),
            (true, true, false, false, Some(true)),
            (true, true, false, true, Some(true)),
            (true, true, true, false, Some(true)),
            (true, true, true, true, Some(true)),
        ] {
            let actual = observed_physical_attempt_is_present(owner, epoch, binding, closing);
            match expected {
                Some(expected) => assert_eq!(actual.unwrap(), expected, "V1/V2"),
                None => assert!(actual.is_err(), "V3"),
            }
        }
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
                    let occupied =
                        decide_physical_attempt_admission(current, slot, handoff, true, false);
                    let peer_expected = match (current, slot) {
                        (false, _) => Some((T::Fenced, AttemptAdmission::Fenced)),
                        (true, S::Corrupt) => None,
                        (true, _) => Some((T::Blocked, AttemptAdmission::Blocked)),
                    };
                    match peer_expected {
                        Some(expected) => assert_eq!(occupied.unwrap(), expected, "P2"),
                        None => assert!(matches!(occupied, Err(DispatchError::Rejected(_))), "P2"),
                    }
                    let actual =
                        decide_physical_attempt_admission(current, slot, handoff, false, false);
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
            let actual = decide_physical_attempt_finish(slot, false, false);
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

/// An observation must not return another Run/Thread hidden under this key.
/// This storage-integrity check grants no replay or execution authority.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_observed_dispatch_key(
    run_id: &awaken_agent_contract::agent::run::Id,
    thread_id: &awaken_agent_contract::agent::thread::Id,
    request: &RunDispatch,
) -> Result<(), DispatchError> {
    if request.run_id() != run_id || request.thread_id() != thread_id {
        return Err(DispatchError::unavailable(
            "persisted dispatch request does not match its Run/Thread storage key",
        ));
    }
    Ok(())
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
            DispatchIdentityScope::PreparedSessionCommand => incoming
                .session_command_fingerprint
                .as_ref()
                .filter(|fingerprint| fingerprint.is_current())
                .is_some_and(|fingerprint| existing == Some(fingerprint.as_str())),
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
                ) || incoming
                    .pre_mcp_requirement_canonical_fingerprint()
                    .is_some_and(|legacy| {
                        completed_run_identity_replays(
                            existing,
                            CompletedIncomingIdentity::FullDispatch(legacy.as_str()),
                        )
                    })
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
    validate_dispatch_worker_codecs(&request)?;
    if !request
        .session_command_fingerprint
        .as_ref()
        .is_some_and(awaken_session_types::SessionRunCommandFingerprint::is_current)
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
    let request = if request.identity_scope == DispatchIdentityScope::PreparedSessionCommand {
        request
    } else {
        request.with_session_command_identity()
    };
    Ok((request, reservation_ttl_ms))
}

#[cfg(any(feature = "durable", test, feature = "test-support"))]
fn legacy_unbound_session_run(request: &RunDispatch) -> bool {
    request.identity_scope == DispatchIdentityScope::FullDispatch
        && request.session_thread_id.is_none()
        && request.session_activity_epoch.is_none()
        && request.session_command_fingerprint.is_none()
}

/// Validate only trusted adoption input before taking a backend transaction.
/// The later locked classifier still rechecks every original stored fact.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn validate_pending_session_run_adoption(
    expected_original: &RunDispatch,
    command_fingerprint: &awaken_session_types::SessionRunCommandFingerprint,
    owner: &str,
    lease_ms: u64,
) -> Result<u64, DispatchError> {
    if !legacy_unbound_session_run(expected_original) || !command_fingerprint.is_current() {
        return Err(DispatchError::Rejected(
            "Session Run adoption requires an original unbound FullDispatch and current fingerprint"
                .to_string(),
        ));
    }
    let lease_ms = crate::clock::normalize_millis(lease_ms);
    if owner.trim().is_empty() || lease_ms == 0 {
        return Err(DispatchError::Rejected(
            "Session Run adoption requires an owner and nonzero lease TTL".to_string(),
        ));
    }
    Ok(lease_ms)
}

/// One locked adoption decision shared by Memory, SQLite and PostgreSQL. The
/// original full identity is strict: even the ordinary old-MCP comparison is
/// unavailable here. Only admission metadata changes; all execution inputs
/// remain the exact original request.
#[cfg(any(feature = "durable", test, feature = "test-support"))]
pub(crate) fn pending_session_run_adoption(
    stored: &RunDispatch,
    expected_original: &RunDispatch,
    current: awaken_dispatch_contract::DispatchTransition,
    authority_empty: bool,
    physical_slot: PhysicalAttemptSlot,
    closing: bool,
    command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
) -> Result<Option<(RunDispatch, awaken_dispatch_contract::DispatchTransition)>, DispatchError> {
    let next = current
        .adopt_pending_reservation(
            stored.same_original_dispatch(expected_original),
            legacy_unbound_session_run(stored),
            authority_empty,
            physical_slot,
            closing,
        )
        .map_err(crate::transition_error)?;
    Ok(next.map(|next| {
        (
            stored
                .clone()
                .for_session(stored.thread_id().clone())
                .with_session_command_fingerprint(command_fingerprint)
                .with_prepared_session_command_identity(),
            next,
        )
    }))
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
    validate_dispatch_worker_codecs(request)?;
    if request.identity_scope == DispatchIdentityScope::PreparedSessionCommand
        && (!request
            .session_command_fingerprint
            .as_ref()
            .is_some_and(awaken_session_types::SessionRunCommandFingerprint::is_current)
            || !matches!(
                request.admission_shape(),
                DispatchAdmissionShape::SessionRootAwaitingActivity
                    | DispatchAdmissionShape::SessionRootWithActivity
            ))
    {
        return Err(DispatchError::Rejected(
            "Prepared Session Run requires self-affinity and its current complete command fingerprint".to_string(),
        ));
    }
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

#[cfg(any(feature = "durable", test, feature = "test-support"))]
fn validate_dispatch_worker_codecs(request: &RunDispatch) -> Result<(), DispatchError> {
    request
        .validate_worker_codec_requirements()
        .map_err(|error| {
            DispatchError::Rejected(format!(
                "dispatch omits a mandatory Worker codec: {error:?}"
            ))
        })
}

#[cfg(test)]
mod codec_rollout_tests {
    use super::*;

    #[test]
    fn historical_unmarked_pending_adoption_adds_only_intrinsic_codec_narrowing() {
        use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
        use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
        // C1 retained pre-codec FullDispatch Pending has no raw/prepared
        // capability metadata; C2 original full identity is exact and authority
        // vacant. E1 the shared production reducer adds ONLY needed codec
        // narrowing plus existing Session admission metadata/tenure; E2 original
        // activation, environment, tools and all other frozen input stay exact.
        // This is retained storage input, not a fresh enqueue that could bypass
        // current admission. Three-backend conformance owns transaction use of
        // this same reducer; neither test claims Thread/SQL refinement.
        let original = RunDispatch::new(RunActivation::new(
            RunId("historical-codec-run".into()),
            ThreadId("historical-codec-thread".into()),
            ExecutableAgentSnapshot::builder("worker")
                .model(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "provider", "model", "native",
                    )
                    .unwrap(),
                )
                .build(),
            Vec::new(),
        ))
        .with_placement(awaken_dispatch_contract::PlacementRequirements::remote_preferred());
        let fingerprint =
            awaken_session_types::SessionRunCommandFingerprint::from_sha256_hex(&"a".repeat(64))
                .unwrap();
        assert!(
            original.placement.required_capabilities.is_empty(),
            "C1 exact old metadata absence"
        );
        let (mut adopted, next) = pending_session_run_adoption(
            &original,
            &original,
            awaken_dispatch_contract::DispatchTransition {
                state: DispatchState::Pending,
                lease_epoch: 0,
                cancellation_requested: false,
            },
            true,
            PhysicalAttemptSlot::Empty,
            false,
            fingerprint,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            next.state,
            DispatchState::ReservationLeased,
            "E1 admission only"
        );
        assert_eq!(
            adopted.placement.location, original.placement.location,
            "E1 local/remote topology unchanged"
        );
        assert_eq!(
            adopted.placement.required_capabilities,
            [
                awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY.to_owned(),
                awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_owned()
            ]
            .into_iter()
            .collect(),
            "E1 only intrinsic narrowing"
        );
        adopted.identity_scope = original.identity_scope;
        adopted.session_thread_id = original.session_thread_id.clone();
        adopted.session_command_fingerprint = original.session_command_fingerprint.clone();
        adopted.placement.required_capabilities = original.placement.required_capabilities.clone();
        assert_eq!(
            adopted, original,
            "E2 all original execution-bearing fields retained"
        );
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
        Some(
            awaken_session_execution_contract::SessionThreadToolReplyActivity::ThreadExtension,
        ) => session_activity_epoch.is_none() && prior_session_activity_epoch.is_none(),
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
) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, DispatchError> {
    awaken_dispatch_contract::worker_credential_realization_capabilities(worker).map_err(|error| {
        DispatchError::Rejected(format!("Worker capability evidence is invalid: {error}"))
    })
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

#[cfg(test)]
mod operation_storage_tests {
    use super::*;

    #[test]
    fn ordinary_ack_codec_keeps_its_original_subject_without_independent_closure_fields() {
        // O1 either local cause + original open subject -> exact private
        // roundtrip; O2 closing/foreign-shaped independent fields/unknown cause
        // -> reject, not an empty fact. O3 public diagnostics never project
        // these entries. All/Controlled golden remains the existing W2 case.
        for cause in [
            AttemptReturnCause::OperationFutureNotEntered,
            AttemptReturnCause::StructuredReturnObserved,
        ] {
            let original = PhysicalAttempt {
                claim: RunClaim {
                    run_id: awaken_agent_contract::RunId("ack".into()),
                    owner: "w".into(),
                    epoch: 1,
                },
                binding: None,
                thread_id: Some(awaken_agent_contract::ThreadId("t".into())),
                session_thread_id: None,
                closing: false,
            };
            let fact = AttemptPhysicalFact::Acknowledgement {
                record: AttemptAcknowledgementRecord::try_new(original, cause).unwrap(),
            };
            let stored = StoredDispatchOperation::from(&fact);
            let wire = serde_json::to_value(&stored).unwrap();
            assert_eq!(
                wire["type"], "attempt_acknowledgement",
                "O1 original operation family"
            );
            assert!(
                wire["record"].get("evidence_ref").is_none(),
                "O1 no fabricated Provider reference"
            );
            let decoded = StoredDispatchOperation::decode(&wire.to_string()).unwrap();
            assert_eq!(decoded.physical_fact(), Some(&fact));
            assert!(
                !decoded.matches(OperationReadPurpose::Diagnostics),
                "O3 no public wire expansion"
            );
            for (field, value) in [
                ("closing", serde_json::json!(true)),
                ("unknown", serde_json::json!(1)),
            ] {
                let mut bad = wire.clone();
                bad["record"]["attempt"][field] = value;
                assert!(
                    StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                    "O2 subject"
                );
            }
            let mut bad = wire.clone();
            bad["record"]["cause"] = serde_json::json!("all_execution_stopped");
            assert!(
                StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                "O2 Worker cause cannot upgrade to All"
            );
            bad = wire;
            bad["record"]["evidence_ref"] = serde_json::json!("not-a-provider-proof");
            assert!(
                StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                "O2 no sibling proof fields"
            );
        }
    }

    #[test]
    fn private_closure_codec_preserves_public_wire_and_rejects_unknown_facts() {
        // Codec decision table: W1 one of the five original public JSON shapes
        // -> unchanged serialization and diagnostics projection; W2 either
        // genuine closure kind -> internal projection only; W3 bad/unknown tag,
        // blank reference or incomplete closed subject -> error, never a
        // missing/public fallback. No deserialized record grants a stop role.
        // Row decision table W4: nonnegative sequence/time + exact row Run
        // -> same original coordinates; historical None time remains None.
        // W5 negative sequence or time, or encoded Run != stored Run -> error
        // for either purpose, before filtering or any cursor publication.
        // Feature graph W6: neutral production has no physical store consumer
        // and excludes this private stored block. Durable SQL and test-support
        // Memory retain the same block; cfg(test) retains this real codec case.
        // The crate neutral/store compile checks cover those partitions, rather
        // than inventing a neutral encode/decode call to suppress dead code.
        let claimed = r#"{"type":"claimed","claim":{"run_id":"r","owner":"w","epoch":1}}"#;
        for (sequence, time, row_run, valid) in [
            (1, Some(0), "r", true),
            (1, None, "r", true),
            (-1, Some(0), "r", false),
            (1, Some(-1), "r", false),
            (1, Some(0), "other", false),
        ] {
            let actual = StoredDispatchEvent::decode_row(sequence, time, row_run, claimed);
            if valid {
                let event = actual.unwrap();
                assert_eq!(event.cursor.0, sequence as u64, "W4 original cursor");
                assert_eq!(
                    event.recorded_at_ms,
                    time.map(|time| time as u64),
                    "W4 historical time"
                );
            } else {
                assert!(actual.is_err(), "W5 row coordinates/subject");
            }
        }
        for encoded in [
            r#"{"type":"claimed","claim":{"run_id":"r","owner":"w","epoch":1}}"#,
            r#"{"type":"lease_lost","claim":{"run_id":"r","owner":"w","epoch":1},"reason":"expired"}"#,
            r#"{"type":"reclaimed","previous":{"run_id":"r","owner":"w","epoch":1},"claim":{"run_id":"r","owner":"next","epoch":2}}"#,
            r#"{"type":"settled","claim":{"run_id":"r","owner":"w","epoch":1},"outcome":"Done"}"#,
            r#"{"type":"dead_lettered","claim":{"run_id":"r","owner":"w","epoch":1},"attempt_count":3}"#,
        ] {
            let decoded = StoredDispatchOperation::decode(encoded).unwrap();
            assert!(decoded.matches(OperationReadPurpose::Diagnostics), "W1");
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::from_str::<serde_json::Value>(encoded).unwrap(),
                "W1 original wire Golden"
            );
        }
        for kind in [
            AttemptClosureKind::AllExecutionStopped,
            AttemptClosureKind::ControlledExecutionClosed,
        ] {
            let record = AttemptClosureRecord::try_new(
                PhysicalAttempt {
                    claim: RunClaim {
                        run_id: awaken_agent_contract::agent::run::Id("r".into()),
                        owner: "w".into(),
                        epoch: 1,
                    },
                    binding: None,
                    thread_id: Some(awaken_agent_contract::agent::thread::Id("t".into())),
                    session_thread_id: None,
                    closing: true,
                },
                kind,
                "codec-storage-reference",
            )
            .unwrap();
            let fact = AttemptPhysicalFact::IndependentClosure {
                record: record.clone(),
            };
            let encoded = serde_json::to_string(&StoredDispatchOperation::from(&fact)).unwrap();
            let kind_tag = match kind {
                AttemptClosureKind::AllExecutionStopped => "all_execution_stopped",
                AttemptClosureKind::ControlledExecutionClosed => "controlled_execution_closed",
            };
            // W2 the unique contract sum is also the stored codec: retire the
            // former private duplicate without changing either old tag/byte.
            assert_eq!(
                encoded,
                format!(
                    r#"{{"type":"attempt_closure","record":{{"attempt":{{"claim":{{"run_id":"r","owner":"w","epoch":1}},"binding":null,"thread_id":"t","session_thread_id":null,"closing":true}},"kind":"{kind_tag}","evidence_ref":"codec-storage-reference"}}}}"#
                )
            );
            let decoded = StoredDispatchOperation::decode(&encoded).unwrap();
            assert_eq!(
                decoded.physical_fact(),
                Some(&fact),
                "W2 exact typed payload"
            );
            assert!(
                !decoded.matches(OperationReadPurpose::Diagnostics),
                "W2 no public variant"
            );
            let mut bad: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            bad["record"]["evidence_ref"] = "".into();
            assert!(
                StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                "W3 ref"
            );
            bad["record"]["evidence_ref"] = "ref".into();
            bad["record"]["attempt"]["closing"] = false.into();
            assert!(
                StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                "W3 open subject"
            );
            let mut bad: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            bad["type"] = "unknown_physical_fact".into();
            assert!(
                StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                "W3 unknown tag cannot fall back to diagnostics"
            );
            bad["type"] = "attempt_closure".into();
            bad["unknown"] = true.into();
            assert!(
                StoredDispatchOperation::decode(&bad.to_string()).is_err(),
                "W3 unknown outer field cannot fall back to diagnostics"
            );
        }
        for bad in [
            r#"{"type":"unknown_old_operation"}"#,
            r#"{"type":"attempt_closure"}"#,
            "null",
        ] {
            assert!(
                StoredDispatchOperation::decode(bad).is_err(),
                "W3 unknown/missing is error"
            );
        }
    }
}
