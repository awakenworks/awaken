// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: A typed private persistence operation over the existing Dispatch and Work
// @formal reason: authorities. The pure predicate combines recorded facts; no parser, pool or state is owned here.
//! Claim-fenced Session Work release after a closed, model-only attempt.

use crate::DispatchError;
use crate::dispatch::PhysicalAttempt;
use awaken_worker_contract::RunClaim;

/// Read-only facts captured by the existing backend claim fence. This is not
/// authority after that fence or its caller-owned database transaction releases.
#[derive(Clone)]
pub struct CommitEpochObservation {
    pub request: crate::RunDispatch,
    pub expires_ms: u64,
    pub cancellation_requested: bool,
    pub physical_slot: crate::PhysicalAttemptSlot,
    /// Exact details when readable. Missing or corrupt details prove no parking
    /// permission and do not replace the pre-existing slot admission policy.
    pub physical_attempt: Option<PhysicalAttempt>,
}

/// The one parking rule, shared by independent-store and same-transaction
/// adapters. `model_only` comes only from the existing committed-footprint
/// decoder in Dispatch execution; the parser is not replicated here.
#[must_use]
pub fn session_work_release_is_authorized(
    current: &CommitEpochObservation,
    claim: &RunClaim,
    observed: &PhysicalAttempt,
    model_only: bool,
) -> bool {
    let request = &current.request;
    deferral_parks_session_work(
        request.requires_session_work_authority()
            && request.thread_id() == request.session_thread_id(),
        !matches!(
            current.physical_slot,
            crate::PhysicalAttemptSlot::Empty | crate::PhysicalAttemptSlot::Corrupt
        ),
        observed.closing,
        model_only,
    ) && request.run_id() == &claim.run_id
        && current.physical_attempt.as_ref().is_some_and(|fresh| {
            fresh.closing
                && fresh == observed
                && fresh.claim.run_id == claim.run_id
                && fresh.thread_id.as_ref() == Some(request.thread_id())
        })
}

/// One conditional cross-ledger effect, selected once by process composition.
/// No new execution, registry or durable parking state is introduced.
#[async_trait::async_trait]
pub trait ConditionalSessionWorkRelease: Send + Sync {
    async fn release(
        &self,
        claim: &RunClaim,
        observed: &PhysicalAttempt,
        model_only: bool,
    ) -> Result<bool, DispatchError>;
}

const fn deferral_parks_session_work(
    root_run: bool,
    slot_occupied: bool,
    local_entry_closed: bool,
    entered_only_model_invocations: bool,
) -> bool {
    root_run && slot_occupied && local_entry_closed && entered_only_model_invocations
}
#[cfg(test)]
mod parked_session_work_tests {
    #[test]
    fn only_a_closed_model_only_root_attempt_parks_its_session_work() {
        // Decision table P1-P5 of `deferral_parks_session_work`, exhaustively
        // over its four facts: exactly P1 releases.
        for root_run in [false, true] {
            for slot_occupied in [false, true] {
                for local_entry_closed in [false, true] {
                    for model_only in [false, true] {
                        let rule = match (root_run, slot_occupied, local_entry_closed, model_only) {
                            (false, ..) => "P5",
                            (true, false, ..) => "P4",
                            (true, true, false, _) => "P2",
                            (true, true, true, false) => "P3",
                            (true, true, true, true) => "P1",
                        };
                        assert_eq!(
                            super::deferral_parks_session_work(
                                root_run,
                                slot_occupied,
                                local_entry_closed,
                                model_only,
                            ),
                            rule == "P1",
                            "{rule}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn parking_requires_the_same_closed_physical_record_under_the_current_fence() {
        use super::*;
        use crate::PhysicalAttemptSlot;
        let mut request = crate::dispatch::tests::a_request();
        request.session_thread_id = Some(request.thread_id().clone());
        let current_claim = RunClaim {
            run_id: request.run_id().clone(),
            owner: "successor".into(),
            epoch: 2,
        };
        let observed = PhysicalAttempt {
            claim: RunClaim {
                run_id: request.run_id().clone(),
                owner: "predecessor".into(),
                epoch: 1,
            },
            binding: None,
            thread_id: Some(request.thread_id().clone()),
            session_thread_id: request.session_thread_id.clone(),
            closing: true,
        };
        let base = CommitEpochObservation {
            request,
            expires_ms: 10,
            cancellation_requested: false,
            physical_slot: PhysicalAttemptSlot::OtherClaim,
            physical_attempt: Some(observed.clone()),
        };
        // Causes/effects: the backend has already fenced current owner+epoch;
        // C1 root Work affinity, C2 occupied coherent slot, C3 observed closure,
        // C4 model-only committed footprint, C5 exact physical record equality.
        // R0 all true -> release permission; R1-R11 alter one necessary fact ->
        // keep Work. A predecessor slot is allowed, but a replacement owner,
        // epoch, closure or Thread after the footprint read is never its proof.
        for rule in 0..12 {
            let mut current = base.clone();
            let mut seen = observed.clone();
            let mut claim = current_claim.clone();
            let mut model_only = true;
            match rule {
                0 => {}
                1 => current.request.session_thread_id = None,
                2 => {
                    current.request.session_thread_id =
                        Some(awaken_agent_contract::agent::thread::Id("parent".into()))
                }
                3 => current.physical_slot = PhysicalAttemptSlot::Empty,
                4 => current.physical_slot = PhysicalAttemptSlot::Corrupt,
                5 => seen.closing = false,
                6 => model_only = false,
                7 => current.physical_attempt = None,
                8 => current.physical_attempt.as_mut().unwrap().claim.owner = "replacement".into(),
                9 => {
                    current.physical_attempt.as_mut().unwrap().claim.epoch =
                        awaken_lease_contract::fence::Epoch::from_persisted(observed.claim.epoch)
                            .unwrap()
                            .next()
                            .unwrap()
                            .get()
                }
                10 => current.physical_attempt.as_mut().unwrap().closing = false,
                11 => claim.run_id = awaken_agent_contract::agent::run::Id("another-run".into()),
                _ => unreachable!(),
            }
            assert_eq!(
                session_work_release_is_authorized(&current, &claim, &seen, model_only),
                rule == 0,
                "R{rule}"
            );
        }
    }
}
