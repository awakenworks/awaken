// @formal surface exclude=read_model_contract
// @formal reason: Read-only phase projection from original Dispatch records; no transition or execution permission is issued.
use crate::AnyCoordinatorDispatch;
use async_trait::async_trait;
use awaken_agent_contract::{ThreadId, agent::run::Id as RunId};
use awaken_dispatch_contract::DispatchExecutionReads;
#[cfg(test)]
use awaken_dispatch_contract::RunClaim;
use awaken_dispatch_contract::dispatch::PhysicalAttempt;
use awaken_dispatch_contract::{DispatchClaimOwnership, DispatchControlReads, DispatchError};
use awaken_session_application_contract::SessionRunExecutionPhase as Phase;

#[async_trait]
impl DispatchExecutionReads for AnyCoordinatorDispatch {
    async fn observe_execution(
        &self,
        run: &RunId,
        thread: &ThreadId,
    ) -> Result<Phase, DispatchError> {
        let Some(summary) = self.dispatch_summary(run).await? else {
            return Ok(Phase::Unavailable);
        };
        if summary.run_id != *run || summary.thread_id != *thread {
            return Err(DispatchError::Rejected(
                "dispatch observation belongs to another Run or Thread".into(),
            ));
        }
        let Some(physical) = self.load_physical_attempt(run).await? else {
            return Ok(Phase::AdmissionPending);
        };
        let current = self.claim_is_current(&physical.claim).await?;
        let after = self.load_physical_attempt(run).await?;
        physical_phase(run, thread, &physical, current, after.as_ref())
    }
}

fn physical_phase(
    run: &RunId,
    thread: &ThreadId,
    before: &PhysicalAttempt,
    current: bool,
    after: Option<&PhysicalAttempt>,
) -> Result<Phase, DispatchError> {
    if after != Some(before) {
        return Ok(Phase::Unavailable);
    }
    if before.claim.run_id != *run || before.thread_id.as_ref().is_some_and(|id| id != thread) {
        return Err(DispatchError::Rejected(
            "physical observation belongs to another Run or Thread".into(),
        ));
    }
    if before.thread_id.is_none() {
        return Ok(Phase::Unavailable);
    }
    Ok(if before.closing || !current {
        Phase::RecoveryPending
    } else {
        Phase::Admitted
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_projection_requires_stable_exact_and_live_evidence() {
        // Causes C1 first/second physical record equal; C2 exact Run/Thread;
        // C3 Thread evidence present; C4 claim live; C5 entry closing.
        // R1 !C1 -> Unavailable, including ACK/removal or a successor; R2
        // C1+!C2 -> Rejected; R3 C1+C2+!C3 -> Unavailable; R4 C1+C2+C3+
        // (!C4 or C5) -> RecoveryPending; R5 all known/live/open -> Admitted.
        // These are read values only: no row, claim, close or ACK is changed.
        let run = RunId("run".into());
        let thread = ThreadId("thread".into());
        let physical = PhysicalAttempt {
            claim: RunClaim {
                run_id: run.clone(),
                owner: "worker".into(),
                epoch: 1,
            },
            binding: None,
            thread_id: Some(thread.clone()),
            session_thread_id: None,
            closing: false,
        };
        for (live, closing, expected) in [
            (true, false, Phase::Admitted),
            (false, false, Phase::RecoveryPending),
            (true, true, Phase::RecoveryPending),
            (false, true, Phase::RecoveryPending),
        ] {
            let mut value = physical.clone();
            value.closing = closing;
            assert_eq!(
                physical_phase(&run, &thread, &value, live, Some(&value)).unwrap(),
                expected
            );
        }
        let mut successor = physical.clone();
        successor.claim.epoch = 2; // fixed stored test coordinate, not an allocator
        for after in [None, Some(&successor)] {
            assert_eq!(
                physical_phase(&run, &thread, &physical, true, after).unwrap(),
                Phase::Unavailable,
                "R1"
            );
        }
        for axis in 0..3 {
            let mut value = physical.clone();
            match axis {
                0 => value.claim.run_id = RunId("foreign".into()),
                1 => value.thread_id = Some(ThreadId("foreign".into())),
                _ => value.thread_id = None,
            }
            let result = physical_phase(&run, &thread, &value, true, Some(&value));
            if axis < 2 {
                assert!(matches!(result, Err(DispatchError::Rejected(_))), "R2");
            } else {
                assert_eq!(result.unwrap(), Phase::Unavailable, "R3");
            }
        }
    }
}
