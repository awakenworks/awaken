//! Physical model/tool/Sandbox admission for one claimed dispatch.
//!
//! This is part of [`super::DispatchWorker`], not a second executor or state
//! machine. It keeps process-local serialization and the persisted Dispatch
//! attempt slot on the one canonical drive path.

use std::future::Future;
use std::sync::Arc;

use super::DispatchWorker;
use crate::Error;
use crate::clock::Clock;
use crate::dispatch::{
    AttemptAdmission, Claimed, Dispatch, DispatchError, RunClaim, SettleOutcome,
};
use awaken_runtime_contract::authority_lease::AuthorityLeaseTiming;
use awaken_runtime_contract::execution::{AttemptRecoveryMode, LiveInput};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

pub(super) struct PhysicalAttemptRequest<'a> {
    pub claimed: &'a Claimed,
    pub context: &'a RuntimeRunContext,
    pub live_input: LiveInput,
    pub recovery_mode: AttemptRecoveryMode,
    pub clock: Arc<dyn Clock>,
}

impl<S: Dispatch + 'static> DispatchWorker<S> {
    /// Cross exactly one physical model/tool/Sandbox attempt boundary.
    ///
    /// The durable claim remains the mutation fence. Its embedded physical slot
    /// adds only the missing quiescence fact: a successor claim may exist, but it
    /// waits here until the predecessor future has actually returned. If
    /// cooperative cancellation exceeds the lease request budget, this drive
    /// keeps observing the same future with the slot occupied. Only actual
    /// return without nested uncertainty acknowledges quiescence; a request
    /// timeout is not a cleanup deadline. An authoritative
    /// provider terminal receipt may prove a remote model request stopped;
    /// process/Pod termination proves only process-bound tool/Sandbox work.
    pub(super) async fn run_physical_attempt<T, E, F, Fut>(
        &self,
        request: PhysicalAttemptRequest<'_>,
        operation: F,
    ) -> Result<Result<T, E>, Error>
    where
        F: FnOnce(RuntimeRunContext) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let PhysicalAttemptRequest {
            claimed,
            context,
            live_input,
            recovery_mode,
            clock,
        } = request;
        // One claimed instruction owns both Run identity and Thread serialization.
        let claim = RunClaim::from(&claimed.lease);
        let thread_id = claimed.request.thread_id();
        // Cause L1: one Runtime receives the same exact claimed dispatch twice.
        // Cause L2: two different Runtimes own predecessor/successor claims.
        // Effect E1: L1 is serialized here before durable admission, so the
        // duplicate cannot interpret `AlreadyApplied` as concurrent permission.
        // Effect E2: L2 is serialized by the persisted physical-attempt slot.
        // This is one layered authority, not two competing durable truths.
        // `execution_context_with` already installs the exact attempt token in
        // the Runtime context. Reusing it here keeps one cancellation carrier
        // across admission and execution instead of accepting two handles that
        // callers could accidentally bind to different attempts.
        let cancellation = context.cancellation.as_ref().ok_or_else(|| {
            Error::Execution(awaken_runtime_contract::execution::Error::Execution(
                "physical Run attempt is missing its cancellation authority".to_string(),
            ))
        })?;
        let _thread_execution = self.runtime.acquire_thread_execution(thread_id).await;
        let timing = AuthorityLeaseTiming::from_ttl_ms(self.lease_ms);
        loop {
            let admission = match recovery_mode {
                AttemptRecoveryMode::RequirePredecessorQuiescence => {
                    self.store.begin_attempt(&claim, clock.now_ms()).await?
                }
                AttemptRecoveryMode::ContinueDurableRemoteOperation => {
                    self.store.handoff_attempt(&claim, clock.now_ms()).await?
                }
            };
            match admission {
                AttemptAdmission::Applied | AttemptAdmission::AlreadyApplied => break,
                AttemptAdmission::Blocked => {
                    tokio::select! {
                        _ = cancellation.cancelled() => {
                            return Err(Error::Execution(
                                awaken_runtime_contract::execution::Error::Execution(
                                    "dispatch authority was lost while waiting for predecessor quiescence".to_string(),
                                ),
                            ));
                        }
                        _ = tokio::time::sleep(timing.retry_delay()) => {}
                    }
                }
                AttemptAdmission::Fenced => {
                    return Err(Error::Execution(
                        awaken_runtime_contract::execution::Error::Execution(
                            "dispatch claim was fenced before physical attempt admission"
                                .to_string(),
                        ),
                    ));
                }
            }
        }

        if cancellation.is_cancelled() {
            let _ = self.finish_observed_attempt(&claim, &timing).await?;
            return Err(Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(
                    "dispatch authority was lost before physical attempt start".to_string(),
                ),
            ));
        }

        let mut attempt_context = context.clone();
        attempt_context.physical_quiescence = Default::default();
        let quiescence = attempt_context.physical_quiescence.clone();
        let attempt = self.runtime.begin_active_attempt(
            &claim.run_id,
            thread_id,
            attempt_context,
            live_input,
        );
        let operation = operation(attempt.context().clone());
        tokio::pin!(operation);
        let result = tokio::select! {
            result = &mut operation => result,
            _ = cancellation.cancelled() => {
                match tokio::time::timeout(timing.request_timeout(), &mut operation).await {
                    Ok(result) => result,
                    Err(_) => {
                        // The executor owns process termination and its grace
                        // periods. Dropping it here could preempt TERM -> KILL
                        // and lose the only late-return ACK observer. Retain this
                        // structured drive (and Thread lock), never spawn an
                        // untracked cleanup task or manufacture a receipt.
                        tracing::warn!(run_id = %claim.run_id.0, owner = %claim.owner,
                            lease_epoch = claim.epoch, phase = "physical_quiescence",
                            "Physical attempt cleanup exceeded lease request budget; awaiting actual return");
                        operation.await
                    }
                }
            }
        };
        if !quiescence.can_acknowledge_return() {
            tracing::warn!(run_id = %claim.run_id.0, owner = %claim.owner,
                lease_epoch = claim.epoch, phase = "physical_quiescence",
                "Executor returned without physical termination evidence; retaining attempt slot");
            return Ok(result);
        }
        if self.finish_observed_attempt(&claim, &timing).await? != SettleOutcome::Applied {
            return Err(Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(
                    "physical Run attempt quiescence acknowledgement was fenced".to_string(),
                ),
            ));
        }
        Ok(result)
    }

    async fn finish_observed_attempt(
        &self,
        claim: &RunClaim,
        timing: &AuthorityLeaseTiming,
    ) -> Result<SettleOutcome, Error> {
        loop {
            match tokio::time::timeout(timing.request_timeout(), self.store.finish_attempt(claim))
                .await
            {
                Ok(Ok(outcome)) => return Ok(outcome),
                Ok(Err(DispatchError::Unavailable(_))) | Err(_) => {
                    // Actual return is already known; retry only its exact,
                    // idempotent ACK. A lost response may have cleared the slot.
                    // Claim loss cannot invalidate evidence about this old slot,
                    // and a successor slot is fenced by the existing store.
                    tracing::warn!(run_id = %claim.run_id.0, owner = %claim.owner,
                        lease_epoch = claim.epoch, phase = "physical_quiescence_ack",
                        "Physical attempt ACK unavailable; retaining observed return for retry");
                    tokio::time::sleep(timing.retry_delay()).await;
                }
                Ok(Err(error)) => return Err(error.into()),
            }
        }
    }
}
