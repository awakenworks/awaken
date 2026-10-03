// @formal entrypoint live_inbox.attempt_scope_is_local_current_capability_gated_and_never_carried_over
// @formal entrypoint remote_worker.execution_requires_claimed_snapshot
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint session_execution.pre_start_cancel_ack_requires_fresh_slot_install
// @formal entrypoint worker_credential.exact_revision_and_current_epoch_required
// @formal entrypoint worker_credential.expired_observation_never_executes
//! Physical model/tool/Sandbox admission for one claimed dispatch.
//!
//! This is part of [`super::DispatchWorker`], not a second executor or state
//! machine. It keeps process-local serialization and the persisted Dispatch
//! attempt slot on the one canonical drive path.

use awaken_dispatch_contract::ControlDispatch;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::{ClaimedSnapshotAuthority, DispatchWorker};
use crate::Error;
use crate::dispatch::{AttemptAdmission, Claimed, DispatchError, RunClaim, SettleOutcome};
use crate::execution_capacity::ExecutionCapacity;
use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::run::RunState;
use awaken_lease_contract::WallClock;
use awaken_lease_contract::WallClockExt;
use awaken_runtime_contract::authority_lease::AuthorityLeaseTiming;
use awaken_runtime_contract::execution::{AttemptRecoveryMode, LiveInput};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

/// An unconfirmed predecessor is a domain outcome for a cancellation request,
/// not a successful cancel or a fabricated physical-return acknowledgement.
pub(super) enum PhysicalAttemptOutcome {
    Returned(Result<RunState, Error>),
    PredecessorUnconfirmed,
    Deferred,
}

pub(super) struct PhysicalAttemptRequest<'a> {
    pub claimed: &'a Claimed,
    pub claimed_snapshot: &'a ClaimedSnapshotAuthority,
    pub context: &'a RuntimeRunContext,
    pub live_input: LiveInput,
    pub recovery_mode: AttemptRecoveryMode,
    pub clock: Arc<dyn WallClock>,
    pub execution_capacity: Option<&'a ExecutionCapacity>,
}

impl<S: ControlDispatch + 'static> DispatchWorker<S> {
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
    pub(super) async fn run_physical_attempt<E, F, Fut>(
        &self,
        request: PhysicalAttemptRequest<'_>,
        operation: F,
    ) -> Result<PhysicalAttemptOutcome, Error>
    where
        F: FnOnce(RuntimeRunContext) -> Fut + Send,
        Fut: Future<Output = Result<RunState, E>> + Send,
        E: Send,
        Error: From<E>,
    {
        let PhysicalAttemptRequest {
            claimed,
            claimed_snapshot,
            context,
            live_input,
            recovery_mode,
            clock,
            execution_capacity,
        } = request;
        // One claimed instruction owns both Run identity and Thread serialization.
        let claim = RunClaim::from(&claimed.lease);
        let thread_id = claimed.request.thread_id();
        if !claimed_snapshot.admits(&claim, thread_id) {
            return Err(Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(
                    "physical Run attempt does not match its installed claim snapshot".to_string(),
                ),
            ));
        }
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
        let _thread_execution = self
            .runtime
            .acquire_thread_execution(thread_id, None)
            .await?;
        let timing = AuthorityLeaseTiming::from_ttl_ms(self.lease_ms);
        let admission = match recovery_mode {
            AttemptRecoveryMode::RequirePredecessorQuiescence => {
                self.store
                    .begin_attempt(&claim, self.execution_binding.as_ref())
                    .await?
            }
            AttemptRecoveryMode::ContinueDurableRemoteOperation => {
                self.store
                    .handoff_attempt(&claim, self.execution_binding.as_ref())
                    .await?
            }
        };
        let admission = match admission {
            AttemptAdmission::Applied
            | AttemptAdmission::AlreadyApplied
            | AttemptAdmission::Replaced => admission,
            AttemptAdmission::Blocked => {
                if claimed.cancellation_requested {
                    // R4: cancellation revokes mutation authority but cannot
                    // stop an opaque predecessor. Let the application commit
                    // Indeterminate while retaining this occupied slot.
                    return Ok(PhysicalAttemptOutcome::PredecessorUnconfirmed);
                }
                // The persisted physical slot, not this Tokio task, is the
                // predecessor-exclusion authority. Retain that slot while
                // releasing the mutation claim behind its durable retry
                // deadline so an unprovable predecessor cannot consume one
                // of the Pool's execution permits indefinitely. Share the
                // Pool's exact-claim backoff instead of retrying this occupied
                // slot on every lease-renewal interval. A first cancellation
                // clears a prior deadline; failed cancellation retries do not.
                let retry_after_ms =
                    crate::pool::claim_failure_backoff(claim.epoch).as_millis() as u64;
                crate::pool::defer_failed_claim(self.store.as_ref(), &claim, retry_after_ms)
                    .await?;
                return Ok(PhysicalAttemptOutcome::Deferred);
            }
            AttemptAdmission::Fenced => {
                return Err(Error::Execution(
                    awaken_runtime_contract::execution::Error::Execution(
                        "dispatch claim was fenced before physical attempt admission".to_string(),
                    ),
                ));
            }
        };

        if cancellation.is_cancelled() {
            if !admission.permits_pre_start_no_effect_ack() {
                // Causes: C1 cancellation arrives before this invocation builds
                // the executor Future; C2 admission freshly installed an empty
                // slot, stuttered an earlier admission, or replaced a predecessor.
                // Effects: E1 fresh install may ACK this invocation's known
                // no-effect slot; E2 stutter/replacement retains the inherited
                // responsibility and reports it as unconfirmed. Decision table:
                // R1=C1+Applied -> E1; R2=C1+(AlreadyApplied|Replaced) -> E2.
                // In particular, local "not started" cannot prove that a prior
                // caller or handed-off remote operation did not start.
                return Ok(PhysicalAttemptOutcome::PredecessorUnconfirmed);
            }
            let _ = self.finish_observed_attempt(&claim, &timing).await?;
            return Err(Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(
                    "dispatch authority was lost before physical attempt start".to_string(),
                ),
            ));
        }

        // Worker placement capacity starts at the same boundary as physical
        // execution authority. Claim resolution, predecessor observation and a
        // blocked admission retain their control task/lease but cannot consume a
        // model/tool/Sandbox slot in the Worker heartbeat.
        let _execution_capacity = execution_capacity.map(ExecutionCapacity::enter);
        let mut attempt_context = context.clone();
        attempt_context.physical_quiescence = Default::default();
        // This freshly admitted physical slot may keep observing its own call;
        // the policy alone is never proof that the provider stopped.
        attempt_context.model_cancellation_observation =
            awaken_runtime_contract::runtime_context::ModelCancellationObservation::RetainClaimedUntilReceipt;
        let quiescence = attempt_context.physical_quiescence.clone();
        let attempt = self.runtime.begin_active_attempt(
            &claim.run_id,
            thread_id,
            attempt_context,
            live_input,
        );
        // Erase the owned drive's layout at this existing execution boundary.
        // Otherwise embedding the scheduled loop in the concrete Future pushes
        // the production Worker's cancellation composition over rustc's layout
        // query depth. Ownership/cancellation are unchanged: no spawned task,
        // detached cleanup, or increased recursion limit hides the problem.
        let mut operation: Pin<Box<dyn Future<Output = Result<RunState, Error>> + Send + '_>> =
            Box::pin(async {
                let context = attempt.context().clone();
                let mut state = operation(context.clone()).await.map_err(Error::from)?;
                // A committed ScheduledAction is a continuation of this claimed
                // execution, not a second physical identity. Keep its Thread lock,
                // cancellation carrier and uncertainty latch through every stage;
                // ACK exactly once after the complete local drive returns.
                // Never enter another stage after an unknown effect or cancellation.
                while state == RunState::Awaiting
                    && !claimed.cancellation_requested
                    && !cancellation.is_cancelled()
                    && quiescence.permits_execution()
                {
                    let Some(ticket) = self.reader.resume_ticket(&claim.run_id) else {
                        break;
                    };
                    if ticket.reason() != AwaitReason::ScheduledAction {
                        break;
                    }
                    state = self
                        .perform_scheduled(&claim, clock.now_ms(), context.clone())
                        .await?;
                }
                Ok(state)
            });
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
        if !quiescence.close_and_wait().await {
            if quiescence.local_entry_closed() {
                let _ = self.store.close_attempt(&claim).await?;
            }
            tracing::warn!(run_id = %claim.run_id.0, owner = %claim.owner,
                lease_epoch = claim.epoch, phase = "physical_quiescence",
                "Executor returned without physical termination evidence; retaining attempt slot");
            return Ok(PhysicalAttemptOutcome::Returned(result));
        }
        if self.finish_observed_attempt(&claim, &timing).await? != SettleOutcome::Applied {
            return Err(Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(
                    "physical Run attempt quiescence acknowledgement was fenced".to_string(),
                ),
            ));
        }
        Ok(PhysicalAttemptOutcome::Returned(result))
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
