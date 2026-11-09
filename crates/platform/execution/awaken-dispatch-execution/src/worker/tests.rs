// @formal evidence remote_worker.committed_terminal_recovery_skips_execution_and_settles_dispatch
// @formal evidence remote_worker.execution_requires_claimed_snapshot
// @formal evidence remote_worker.pending_signal_is_consumed_by_commit
// Unit tests for the one DispatchWorker implementation. The containing module
// stays in `worker.rs`, preserving every existing test path and design comment.

use std::sync::Arc;

use super::claim_renewal::RenewalStop;
use super::{
    AttemptCancellation, ClaimedSnapshotAuthority, DispatchWorker, PendingInputAdmission,
    cancellation_uses_tool_interruption, classify_pending_input_admission,
    combine_attempt_cancellation, recovered_attempt_disposition, select_physical_attempt_recovery,
    settle_outcome,
};
use awaken_agent_contract::agent::awaiting::{
    AwaitTarget, PendingTool, RemoteInputReason, ResumeTicket, ToolAwaitReason,
};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunResumeTicket};
use awaken_runtime::Runtime;
use awaken_store_inmem::MemoryCommitCoordinator;

use crate::Error;
use crate::RecoveryProjection;
use crate::dispatch::{DispatchOutcome, PendingInput};
use crate::{FencedStreamCheckpointStore, RunClaim};
use awaken_agent_contract::credential::{
    CandidateFingerprint, CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource,
    CredentialRef, CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
};
use awaken_credential_contract::CredentialRealizationKind;
use awaken_credential_contract::{AttemptCredentialBinding, AttemptCredentialDelivery};
use awaken_dispatch::MemoryDispatchStore;
use awaken_dispatch_contract::{DispatchAdministration as _, DispatchExecution as _};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::cancellation::CancellationCause;
use awaken_runtime_contract::execution::AttemptRecoveryMode;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};

#[path = "tests/claim_path_stack.rs"]
mod claim_path_stack;
#[path = "tests/operation_receipts.rs"]
mod operation_receipts;
#[path = "tests/terminal_pending.rs"]
mod terminal_pending;

#[test]
fn fresh_durable_cancel_consults_the_executor_continuation_authority() {
    // Cause/effect decision table: C1=claim is recovered; C2=claim carries a
    // durable cancellation; C3=the executor recognizes an exact committed
    // continuation. E1=consult the executor and use its decision; E2=require
    // predecessor quiescence without consulting it.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | R1 | no | no | * | E2 |
    // | R2 | yes | * | yes/no | E1 |
    // | R3 | no | yes | yes/no | E1 |
    //
    // The A2A executor's own decision table covers C3 using absent, wrong-
    // endpoint, and exact committed task references. This test owns the Worker
    // gate that used to omit R3 merely because the cancellation claim was fresh.
    use std::cell::Cell;

    for (recovered, cancellation_requested, executor_decision, expected_consulted, expected) in [
        (
            false,
            false,
            AttemptRecoveryMode::ContinueDurableRemoteOperation,
            false,
            AttemptRecoveryMode::RequirePredecessorQuiescence,
        ),
        (
            true,
            false,
            AttemptRecoveryMode::ContinueDurableRemoteOperation,
            true,
            AttemptRecoveryMode::ContinueDurableRemoteOperation,
        ),
        (
            false,
            true,
            AttemptRecoveryMode::ContinueDurableRemoteOperation,
            true,
            AttemptRecoveryMode::ContinueDurableRemoteOperation,
        ),
        (
            false,
            true,
            AttemptRecoveryMode::RequirePredecessorQuiescence,
            true,
            AttemptRecoveryMode::RequirePredecessorQuiescence,
        ),
    ] {
        let consulted = Cell::new(false);
        let outcome = select_physical_attempt_recovery(recovered, cancellation_requested, || {
            consulted.set(true);
            executor_decision
        });
        assert_eq!(consulted.get(), expected_consulted);
        assert_eq!(outcome, expected);
    }
}

#[tokio::test]
async fn attempt_cancellation_combines_claim_and_host_without_a_second_authority() {
    // Cause/effect graph: C1=claim cancellation is already/currently signalled;
    // C2=an optional host cancellation is already/currently signalled. Effect
    // E1=Runtime's one combined token is cancelled; E2=an absent host preserves
    // the exact claim token instead of spawning a redundant relay; E3=the
    // combined signal carries the cause its source recorded (the claim's own
    // cell, or the host's recorded cause; an untagged host stays untagged).
    //
    // | Rule | Claim | Host | Effect |
    // | AC1 | live | absent | E2; later claim cancellation -> E1 |
    // | AC2 | live | live | either later cancellation -> E1 + E3 |
    // | AC3 | cancelled | any | immediate E1 + E3 |
    // | AC4 | live | cancelled | immediate E1 + E3 |
    // Constraint: this signal relay owns no lifecycle or persistence state.
    let claim_only = AttemptCancellation::default();
    let (combined, _relay) = combine_attempt_cancellation(claim_only.clone(), None);
    assert_eq!(combined.token, claim_only.token, "AC1/E2");
    claim_only.cancel(CancellationCause::ClaimLost);
    combined.token.cancelled().await;
    assert!(combined.token.is_cancelled(), "AC1/E1");
    assert_eq!(
        combined.cause.recorded(),
        Some(CancellationCause::ClaimLost),
        "AC1/E3"
    );

    for (cancel_claim, host_cause, expected) in [
        (true, None, Some(CancellationCause::ProofExpired)),
        (false, None, None),
        (
            false,
            Some(CancellationCause::WorkerRevoked),
            Some(CancellationCause::WorkerRevoked),
        ),
    ] {
        let claim = AttemptCancellation::default();
        let host = AttemptCancellation::default();
        let (combined, _relay) = combine_attempt_cancellation(claim.clone(), Some(&host));
        if cancel_claim {
            claim.cancel(CancellationCause::ProofExpired);
        } else if let Some(cause) = host_cause {
            host.cancel(cause);
        } else {
            host.token.cancel();
        }
        combined.token.cancelled().await;
        assert!(combined.token.is_cancelled(), "AC2/E1");
        assert_eq!(combined.cause.recorded(), expected, "AC2/E3");
    }

    let cancelled_claim = AttemptCancellation::default();
    cancelled_claim.cancel(CancellationCause::Requested);
    let (combined, _relay) =
        combine_attempt_cancellation(cancelled_claim, Some(&AttemptCancellation::default()));
    assert!(combined.token.is_cancelled(), "AC3/E1");
    assert_eq!(
        combined.cause.recorded(),
        Some(CancellationCause::Requested),
        "AC3/E3"
    );

    let cancelled_host = AttemptCancellation::default();
    cancelled_host.cancel(CancellationCause::HostDrain);
    let (combined, _relay) =
        combine_attempt_cancellation(AttemptCancellation::default(), Some(&cancelled_host));
    assert!(combined.token.is_cancelled(), "AC4/E1");
    assert_eq!(
        combined.cause.recorded(),
        Some(CancellationCause::HostDrain),
        "AC4/E3"
    );
}

#[test]
fn renewal_stop_records_the_cause_that_decides_the_run_effect() {
    // Rows RC1-RC4 of `RenewalStop`: only a durable cancellation is the Run's
    // cancellation authority; every other stop abandons the attempt.
    for (stop, cause, ends_run) in [
        (
            RenewalStop::DurableCancellation,
            CancellationCause::Requested,
            true,
        ),
        (RenewalStop::ClaimLost, CancellationCause::ClaimLost, false),
        (
            RenewalStop::ProofExpired,
            CancellationCause::ProofExpired,
            false,
        ),
        (
            RenewalStop::OwnerDropped,
            CancellationCause::HostDrain,
            false,
        ),
    ] {
        assert_eq!(stop.cause(), cause, "{stop:?}");
        assert_eq!(stop.cause().ends_run(), ends_run, "{stop:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn renewal_tags_durable_cancellation_and_claim_loss_with_their_causes() {
    // Cause/effect table RC: C1 durable cancellation / peer takeover;
    // C2 exact physical slot installed / not started. RC1 C1=cancel+C2=slot
    // => Requested; RC2 cancel+no slot => ClaimLost, because the revoked
    // lease owns no executing effect; RC3/RC4 peer takeover => ClaimLost for
    // either slot state. All rows use the real Dispatch store and renewal loop.
    // First-cause absorption is checked again after dropping the renewal guard.
    // The store authority clock stays independent of the paused Tokio scheduler.
    use awaken_lease_testkit::ManualClock;
    for (durable_cancel, physical_started, expected) in [
        (true, true, CancellationCause::Requested),
        (true, false, CancellationCause::ClaimLost),
        (false, true, CancellationCause::ClaimLost),
        (false, false, CancellationCause::ClaimLost),
    ] {
        let clock = Arc::new(ManualClock::new(0));
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let run_id = RunId(format!("renewal-cause-{durable_cancel}-{physical_started}"));
        store
            .enqueue(crate::RunDispatch::new(RunActivation::new(
                run_id.clone(),
                ThreadId(format!(
                    "renewal-cause-thread-{durable_cancel}-{physical_started}"
                )),
                ExecutableAgentSnapshot::builder("renewal-cause-snapshot")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "test", "native",
                        )
                        .expect("complete model binding"),
                    )
                    .build(),
                Vec::new(),
            )))
            .await
            .expect("enqueue");
        let claimed = store
            .claim("owner-a", 90, &Default::default())
            .await
            .expect("claim")
            .expect("runnable");
        let claim = RunClaim::from(&claimed.lease);
        if physical_started {
            assert_eq!(
                store
                    .begin_attempt(&claim, None)
                    .await
                    .expect("physical admission"),
                awaken_dispatch_contract::AttemptAdmission::Applied,
                "RC1/RC3 install the actual physical slot"
            );
        }
        let renewal = super::renew_claim_while_active(store.clone(), &claim, 90);
        let attempt = renewal.attempt_cancellation();
        if durable_cancel {
            store.cancel(&run_id).await.expect("durable cancellation");
        } else {
            clock.set(91);
            store
                .claim("owner-b", 90, &Default::default())
                .await
                .expect("peer claim")
                .expect("peer replaces the expired claim");
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            attempt.token.cancelled(),
        )
        .await
        .expect("renewal fires the attempt token");
        assert_eq!(attempt.cause.recorded(), Some(expected), "{run_id:?}");
        drop(renewal);
        assert_eq!(
            attempt.cause.recorded(),
            Some(expected),
            "dropping the guard never rewrites the first cause"
        );
    }
}

/// Executor fixture that stops its attempt for a cause that does not end the
/// Run, exactly as Native and ACP do at their terminal commit boundary.
struct AbandoningExecutor(CancellationCause);

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for AbandoningExecutor {
    async fn execute(
        &self,
        _activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        Err(awaken_runtime_contract::cancellation::abandoned_attempt(
            self.0,
        ))
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunAttemptExecutor for AbandoningExecutor {
    async fn resume(
        &self,
        _activation: RunActivation,
        _command: awaken_runtime_contract::resume::ResumeCommand,
        _context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        Err(awaken_runtime_contract::cancellation::abandoned_attempt(
            self.0,
        ))
    }
}

#[tokio::test]
async fn an_abandoned_attempt_settles_nothing_and_keeps_its_retry_budget() {
    // Row W1 of `settle_if_terminal_or_raise`. Cause: the executor stopped for
    // an authority loss (proof expiry, lost claim, Worker revocation, drain).
    // Effects: E1 the drive returns no processed Run; E2 no Thread commit;
    // E3 the row stays Leased to the old claim (no settlement, no release, no
    // spent attempt) so the lease lapses and recovery re-claims it; E4 the
    // returned attempt acknowledged its physical slot on the way out.
    use awaken_dispatch_contract::{DispatchReads as _, DispatchState};
    for cause in [
        CancellationCause::ProofExpired,
        CancellationCause::ClaimLost,
        CancellationCause::WorkerRevoked,
        CancellationCause::HostDrain,
    ] {
        let store = Arc::new(MemoryDispatchStore::new(Arc::new(
            awaken_lease_testkit::ManualClock::new(0),
        )));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let worker = DispatchWorker::new(
            Arc::new(Runtime::new()),
            store.clone(),
            commit.clone(),
            "worker",
        );
        worker.install_attempt_executor(Arc::new(AbandoningExecutor(cause)));
        store
            .enqueue(crate::RunDispatch::new(RunActivation::new(
                RunId("abandoned-run".into()),
                ThreadId("abandoned-thread".into()),
                ExecutableAgentSnapshot::builder("abandoned-snapshot")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "test", "native",
                        )
                        .expect("complete model binding"),
                    )
                    .build(),
                Vec::new(),
            )))
            .await
            .expect("enqueue");
        let claimed = store
            .claim("worker", 60_000, &Default::default())
            .await
            .expect("claim")
            .expect("runnable");

        let driven = worker
            .drive_claimed(claimed, awaken_time::system_clock())
            .await
            .expect("W1 abandonment is not a drive failure");
        assert_eq!(driven, None, "W1/E1 {cause:?}");
        assert!(commit.committed().latest_run.is_none(), "W1/E2 {cause:?}");
        let rows = store.list_dispatches().await.expect("rows");
        assert_eq!(rows.len(), 1, "{cause:?}");
        assert_eq!(rows[0].state, DispatchState::Leased, "W1/E3 {cause:?}");
        assert_eq!(rows[0].attempt_count, 0, "W1/E3 {cause:?}");
        assert!(!rows[0].physical_attempt_active, "W1/E4 {cause:?}");
    }
}

#[test]
fn claimed_snapshot_authority_admits_only_the_exact_attempt_identity() {
    // Cause/effect graph: C1 Run matches; C2 Thread matches; C3 epoch matches.
    // Effect E1 admits the physical attempt only when all causes hold; E2
    // rejects every mismatch. Decision table R1=111=>E1; R2=000..110=>E2.
    // The eight masks exercise the production equality projection rather than
    // only the Boolean Kani reducer.
    let claim = RunClaim {
        run_id: RunId("claimed-run".into()),
        owner: "worker".into(),
        epoch: 7,
    };
    let thread_id = ThreadId("claimed-thread".into());
    let authority = ClaimedSnapshotAuthority::installed(&claim, &thread_id);

    for mismatch_mask in 0..8 {
        let mut candidate = claim.clone();
        let mut candidate_thread = thread_id.clone();
        if mismatch_mask & 1 != 0 {
            candidate.run_id = RunId("other-run".into());
        }
        if mismatch_mask & 2 != 0 {
            candidate_thread = ThreadId("other-thread".into());
        }
        if mismatch_mask & 4 != 0 {
            candidate.epoch += 1;
        }
        assert_eq!(
            authority.admits(&candidate, &candidate_thread),
            mismatch_mask == 0,
            "R1-R2 mask={mismatch_mask}"
        );
    }
}

// Behavior 1: an illegal `Running` executor result fails loudly — the worker
// must NOT settle a live run to Done/Awaiting. `settle_outcome` is the decision
// point `drive_claimed` consults before it calls `store.settle`, so proving it
// errors on `Running` proves the worker never settles a mid-flight run.
#[test]
fn running_state_result_is_rejected_not_settled() {
    let err = settle_outcome(&RunState::Running).expect_err("Running must fail loudly");
    // It is an execution error, not a dispatch/storage error — a broken executor
    // is not a queue fault.
    assert!(
        matches!(err, Error::Execution(_)),
        "a non-settled Running result is an execution error, got {err:?}"
    );
}

#[test]
fn ended_settles_done_and_awaiting_settles_awaiting() {
    assert!(matches!(
        settle_outcome(&RunState::Ended(EndCause::NaturalEnd)).unwrap(),
        DispatchOutcome::Done
    ));
    assert!(matches!(
        settle_outcome(&RunState::Awaiting).unwrap(),
        DispatchOutcome::Awaiting
    ));
}

#[test]
fn managed_cancellation_routes_missing_external_waits_to_batch_recovery() {
    // Cause/effect graph: C1=claim belongs to a managed Session Run; C2=Run is
    // Awaiting vs another state; C3=ResumeTicket is missing, an external-tool
    // reason, or another legal wait. Effect E1=use batch-aware interruption;
    // E2=use ordinary cancellation. The missing partition is intentional: the
    // store isolated a damaged ticket and Runtime must reconstruct/quarantine
    // from durable batch facts.
    //
    // | Rule | Managed | State | Ticket | Effect |
    // | CI1 | no | Awaiting | missing/tool | E2 |
    // | CI2 | yes | Running/Ended/absent | any | E2 |
    // | CI3 | yes | Awaiting | tool/external | E1 |
    // | CI4 | yes | Awaiting | missing | E1 |
    // | CI5 | yes | Awaiting | manual/delegation/scheduled | E2 |
    // Constraint: the queue remains delivery-only; this pure selector neither
    // mutates nor interprets ToolBatch state.
    let run = RunId("interrupt-route-run".into());
    let thread = ThreadId("interrupt-route-thread".into());
    let ticket = |target| {
        ResumeTicket::new(
            "interrupt-route-correlation",
            run.clone(),
            thread.clone(),
            "interrupt-route-snapshot",
            "interrupt-route-catalog",
            target,
        )
    };
    let tool = ticket(AwaitTarget::ToolCall {
        reason: ToolAwaitReason::Permission,
        call_id: "interrupt-route-call".into(),
        tool: PendingTool {
            tool_id: "write".into(),
            arguments: serde_json::json!({}),
        },
        authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
    });
    let external = ticket(AwaitTarget::ToolCall {
        reason: ToolAwaitReason::ClientExecution,
        call_id: "interrupt-route-client-call".into(),
        tool: PendingTool {
            tool_id: "client-tool".into(),
            arguments: serde_json::json!({}),
        },
        authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
    });
    let manual = ticket(AwaitTarget::Pause(
        awaken_agent_contract::agent::awaiting::PauseReason::Manual,
    ));

    assert!(
        !cancellation_uses_tool_interruption(false, Some(&RunState::Awaiting), None),
        "CI1/E2"
    );
    assert!(
        !cancellation_uses_tool_interruption(true, Some(&RunState::Running), Some(&tool)),
        "CI2/E2"
    );
    assert!(
        cancellation_uses_tool_interruption(true, Some(&RunState::Awaiting), Some(&tool)),
        "CI3/E1"
    );
    assert!(
        cancellation_uses_tool_interruption(true, Some(&RunState::Awaiting), Some(&external)),
        "CI3/E1 external result"
    );
    assert!(
        cancellation_uses_tool_interruption(true, Some(&RunState::Awaiting), None),
        "CI4/E1"
    );
    assert!(
        !cancellation_uses_tool_interruption(true, Some(&RunState::Awaiting), Some(&manual)),
        "CI5/E2"
    );
}

#[test]
fn committed_truth_is_the_single_pending_input_admission_rule() {
    // Cause/effect graph: C1 committed state is terminal; C2 it retains a
    // ticket; C3 the input matches that ticket; C4 the input is unbound.
    // Effects: E1 deliver only exact live-ticket or fresh unbound input; E2
    // classify terminal, stale, and orphan-bound input as already consumed by
    // the commit so settle may delete it without re-execution. Constraints:
    // ticket presence makes C4 irrelevant; terminal truth dominates all other
    // causes. The 16 Boolean rows below exhaust the decision table.
    for mask in 0_u8..16 {
        let terminal = mask & 1 != 0;
        let ticket = mask & 2 != 0;
        let matches = mask & 4 != 0;
        let unbound = mask & 8 != 0;
        let expected_deliver = !terminal && ((ticket && matches) || (!ticket && unbound));
        assert_eq!(
            classify_pending_input_admission(terminal, ticket, matches, unbound),
            if expected_deliver {
                PendingInputAdmission::Deliver
            } else {
                PendingInputAdmission::ConsumedByCommittedTruth
            },
            "decision row {mask:04b}"
        );
    }
}

#[test]
fn only_a_recovered_claim_that_will_execute_records_a_reschedule() {
    // Causes: C1 the claim is a durable lease recovery; C2 committed state
    // is absent/Running, Awaiting, or Ended; C3 Awaiting has an exact ticket;
    // C4 that ticket is scheduled, matched by delivered input, or unmatched.
    // Effects: E1 preserve Running/Awaiting disposition for a real retry; E2
    // omit fresh, terminal, and settlement-only claims; E3 reject impossible
    // state/ticket products before execution.
    //
    // | Rule | C1 | State | Ticket/input | Effect |
    // |---|---|---|---|---|
    // | R1 | F | any | any | E2 |
    // | R2 | T | Running/absent | none | E1 Running |
    // | R3 | T | Awaiting | scheduled | E1 Awaiting |
    // | R4 | T | Awaiting | matching | E1 Awaiting |
    // | R5 | T | Awaiting | unmatched | E2 |
    // | R6 | T | Ended | none | E2 |
    // | R7 | T | Awaiting | missing | E3 |
    // Constraint/Invariant: recovery records a reschedule only for an
    // attempt that will execute or resume. Decision rule: R1-R7 partition
    // freshness, committed state, ticket presence, and input match.
    let run_id = RunId("run".into());
    let thread_id = ThreadId("thread".into());
    let ticket = |target, correlation: &str| {
        ResumeTicket::new(
            correlation,
            run_id.clone(),
            thread_id.clone(),
            "snapshot",
            "catalog",
            target,
        )
    };
    let pending = PendingInput {
        message_id: "message".into(),
        run_id: run_id.clone(),
        thread_id: thread_id.clone(),
        correlation_id: "match".into(),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("continue".into()),
    };

    assert!(
        recovered_attempt_disposition(false, &run_id, None, None, &[])
            .unwrap()
            .is_none(),
        "R1/E2"
    );
    assert!(
        matches!(
            recovered_attempt_disposition(true, &run_id, Some(&RunState::Running), None, &[],)
                .unwrap(),
            Some(RunDisposition::Running { .. })
        ),
        "R2/E1"
    );
    assert!(
        matches!(
            recovered_attempt_disposition(
                true,
                &run_id,
                Some(&RunState::Awaiting),
                Some(ticket(
                    AwaitTarget::ToolCall {
                        reason: ToolAwaitReason::ScheduledAction,
                        call_id: "scheduled".into(),
                        tool: PendingTool {
                            tool_id: "scheduled-action".into(),
                            arguments: serde_json::Value::Null,
                        },
                        authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                    },
                    "scheduled",
                )),
                &[],
            )
            .unwrap(),
            Some(RunDisposition::Awaiting(_))
        ),
        "R3/E1"
    );
    assert!(
        matches!(
            recovered_attempt_disposition(
                true,
                &run_id,
                Some(&RunState::Awaiting),
                Some(ticket(
                    AwaitTarget::RemoteInput {
                        reason: RemoteInputReason::UserInput,
                        call_id: "match".into(),
                    },
                    "match",
                )),
                std::slice::from_ref(&pending),
            )
            .unwrap(),
            Some(RunDisposition::Awaiting(_))
        ),
        "R4/E1"
    );
    assert!(
        recovered_attempt_disposition(
            true,
            &run_id,
            Some(&RunState::Awaiting),
            Some(ticket(
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::UserInput,
                    call_id: "other".into(),
                },
                "other",
            )),
            std::slice::from_ref(&pending),
        )
        .unwrap()
        .is_none(),
        "R5/E2"
    );
    assert!(
        recovered_attempt_disposition(
            true,
            &run_id,
            Some(&RunState::Ended(EndCause::NaturalEnd)),
            None,
            &[],
        )
        .unwrap()
        .is_none(),
        "R6/E2"
    );
    assert!(
        recovered_attempt_disposition(true, &run_id, Some(&RunState::Awaiting), None, &[],)
            .is_err(),
        "R7/E3"
    );
}

#[test]
fn remote_projection_is_the_single_awaiting_resume_reader() {
    // Cause/effect graph: C1 remote construction installs a provisional projection;
    // C2 a database-independent Worker installs its transport-backed
    // projection; C3 that snapshot says the claimed Run is Awaiting and owns
    // a resume ticket. Effects: E1 the old projection is released and RPC retained;
    // E2 snapshot install, dispatch selection, and Runtime context share one
    // Arc; E3 the Awaiting ticket is visible at the exact decision point that
    // chooses `resume`, so the original activation cannot be executed fresh.
    //
    // | Rule | Topology | Snapshot | Reader ownership | Effect |
    // |---|---|---|---|---|
    // | P1 | local | any | provisional projection + local source | ordinary local load |
    // | P2 | remote | Awaiting + ticket | one installed Arc | E1+E2+E3 resume |
    // | P3 | remote | no ticket | one installed Arc | fresh/terminal decision from same prefix |
    // Constraint/Invariant: remote snapshot install, dispatch selection, and
    // Runtime resume must share one reader Arc. Decision rule: P1-P3 cover
    // local ownership and both remote ticket partitions.
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let identity = awaken_worker_contract::WorkerIdentity::new("worker", "boot", 1);
    let binding = awaken_dispatch_erasure::DispatchBinding::remote(
        Arc::new(awaken_coordinator_client::HttpDispatchQueue::new(
            "http://127.0.0.1:9",
            identity.clone(),
        )),
        Arc::new(awaken_coordinator_client::RemoteClaimedRunCommit::new(
            "http://127.0.0.1:9",
            identity,
        )),
    );
    let worker = DispatchWorker::from_binding(
        Arc::new(Runtime::new()),
        binding,
        commit.clone(),
        commit,
        "worker",
    )
    .unwrap();
    assert!(
        worker.recovery_source.is_none(),
        "remote load authority is required RPC"
    );
    let provisional = Arc::downgrade(
        worker
            .recovery_projection
            .as_ref()
            .expect("constructor projection"),
    );

    let projection = Arc::new(RecoveryProjection::new());
    let projection_reader: Arc<dyn CommittedThreadView> = projection.clone();
    let worker = worker
        .with_recovery_projection(projection.clone())
        .expect("required remote recovery RPC");

    assert!(
        worker.recovery_source.is_none(),
        "P2/E1 transport owns load"
    );
    assert!(provisional.upgrade().is_none(), "P2/E1 no parallel P1");
    assert!(
        Arc::ptr_eq(
            worker.recovery_projection.as_ref().expect("install target"),
            &projection,
        ),
        "P2/E2 install target"
    );
    assert!(
        Arc::ptr_eq(&worker.reader, &projection_reader),
        "P2/E2 dispatch reader"
    );
    let runtime_reader = worker
        .execution_context()
        .reader
        .expect("Runtime committed reader");
    assert!(
        Arc::ptr_eq(&runtime_reader, &projection_reader),
        "P2/E2 Runtime reader"
    );

    let run_id = RunId("remote-awaiting".into());
    let thread_id = ThreadId("remote-thread".into());
    let ticket = ResumeTicket::new(
        "remote-reply",
        run_id.clone(),
        thread_id.clone(),
        "snapshot",
        "catalog",
        AwaitTarget::RemoteInput {
            reason: RemoteInputReason::UserInput,
            call_id: "remote-reply".into(),
        },
    );
    projection
        .install(
            &run_id,
            &thread_id,
            RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: thread_id.clone(),
                claimed_run_id: run_id.clone(),
                runs: vec![RunRecord {
                    id: run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: RunState::Awaiting,
                }],
                latest_run_id: Some(run_id.clone()),
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: vec![RunResumeTicket {
                    run_id: run_id.clone(),
                    ticket: ticket.clone(),
                }],
                thread_version: 1,
                store_cursor: 1,
                next_commit_ordinal: 1,
            },
        )
        .expect("install claim-fenced snapshot");
    assert_eq!(worker.reader.run_state(&run_id), Some(RunState::Awaiting));
    assert_eq!(worker.reader.resume_ticket(&run_id), Some(ticket.clone()));
    assert_eq!(runtime_reader.resume_ticket(&run_id), Some(ticket), "P2/E3");
}

struct TerminalView(RunRecord);

impl CommittedThreadView for TerminalView {
    fn committed_messages(
        &self,
        _thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::message::Message> {
        Vec::new()
    }

    fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
        Some(self.0.clone())
    }

    fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
        Some(self.0.clone())
    }

    fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
        None
    }

    fn open_wait_for_thread(&self, _thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        None
    }
}

#[tokio::test]
async fn terminal_recovery_rejects_conflicting_committed_identity_before_settlement() {
    // Cause/effect graph: C1 the dispatch claim names Run R and Thread T; C2
    // committed terminal truth reports R vs another Run; C3 it reports T vs
    // another Thread. Effects: E1 exact identity may enter terminal settlement;
    // E2 either identity conflict is rejected; E3 rejection leaves the durable
    // dispatch untouched and cannot enter an executor.
    //
    // | Rule | C2 Run | C3 Thread | Effect |
    // |---|---|---|---|
    // | R1 | foreign | exact | E2 + E3 |
    // | R2 | exact | foreign | E2 + E3 |
    //
    // The exact R/T/Ended success partition is owned by the existing terminal
    // recovery integration tests. These two rules cover the newly exposed
    // identity-conflict mapping at the Worker boundary.
    let run_id = RunId("terminal-identity-run".into());
    let thread_id = ThreadId("terminal-identity-thread".into());
    let activation = RunActivation::new(
        run_id.clone(),
        thread_id.clone(),
        ExecutableAgentSnapshot::builder("terminal-identity-snapshot")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "test", "native",
            ))
            .build(),
        Vec::new(),
    );
    // The store's own clock is the dispatch authority now; freeze it at the
    // same t=0 this fixture always used.
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(
        awaken_lease_testkit::ManualClock::new(0),
    )));
    store
        .enqueue(crate::RunDispatch::new(activation))
        .await
        .expect("enqueue terminal identity fixture");
    let claimed = store
        .claim("worker", 1_000, &Default::default())
        .await
        .expect("claim terminal identity fixture")
        .expect("terminal identity fixture is runnable");

    for (record, rule) in [
        (
            RunRecord {
                id: RunId("foreign-run".into()),
                thread_id: thread_id.clone(),
                state: RunState::Ended(EndCause::NaturalEnd),
            },
            "R1",
        ),
        (
            RunRecord {
                id: run_id.clone(),
                thread_id: ThreadId("foreign-thread".into()),
                state: RunState::Ended(EndCause::NaturalEnd),
            },
            "R2",
        ),
    ] {
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let reader: Arc<dyn CommittedThreadView> = Arc::new(TerminalView(record));
        let worker = DispatchWorker::from_parts(
            Arc::new(Runtime::new()),
            store.clone(),
            commit,
            reader,
            "worker",
        );
        assert!(
            matches!(
                worker.committed_terminal_for_claim(&claimed),
                Err(Error::Dispatch(crate::DispatchError::Rejected(_)))
            ),
            "{rule}/E2"
        );
        assert_eq!(store.dispatch_count(), 1, "{rule}/E3");
    }
}

#[tokio::test]
async fn child_attempt_rebinds_every_parent_run_authority_to_its_claim() {
    // Cause/effect graph: C1 the inherited parent context already contains a
    // parent-claim-fenced checkpoint; C2 the child Worker owns a distinct claim
    // and isolated recovery projection. Effects: E1 the final checkpoint accepts
    // the child Run id (no nested parent fence); E2 the Runtime reader is the
    // child's projection; E3 a child commit validates and advances that same
    // projection; E4 ownership verifies the child claim. Constraint: all four
    // ports are assembled only by `execution_context_with`, after durable claim.
    //
    // | Rule | Parent fence | Child claim/projection | Effects |
    // |---|---|---|---|
    // | R1 | present | present and current | E1,E2,E3,E4 |
    // Decision rule: R1 is the only valid inherited-parent/claimed-child
    // product; assert every authority port resolves to the child claim.
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(
        awaken_lease_testkit::ManualClock::new(0),
    )));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let activation = |run: &str, thread: &str| {
        RunActivation::new(
            RunId(run.into()),
            ThreadId(thread.into()),
            ExecutableAgentSnapshot::builder(format!("snapshot-{run}"))
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "test", "native",
                ))
                .build(),
            Vec::new(),
        )
    };
    let parent = store
        .claim_new_run(
            crate::RunDispatch::new(activation("parent-run", "parent-thread")),
            "worker",
            1_000,
            &Default::default(),
        )
        .await
        .expect("R1 parent admission")
        .expect("R1 parent claim");
    let parent_claim = RunClaim::from(&parent.lease);
    let dispatch: Arc<dyn awaken_dispatch_contract::DispatchCommitAuthority> = store.clone();
    let parent_checkpoint: Arc<dyn StreamCheckpointStore> = Arc::new(
        FencedStreamCheckpointStore::new(None, dispatch, parent_claim),
    );
    let parent_context = RuntimeRunContext::new().with_stream_checkpoint(parent_checkpoint);

    let child_run = RunId("child-run".into());
    let child_thread = ThreadId("child-thread".into());
    let child = store
        .claim_new_run(
            crate::RunDispatch::new(activation(&child_run.0, &child_thread.0)),
            "worker",
            1_000,
            &Default::default(),
        )
        .await
        .expect("R1 child admission")
        .expect("R1 child claim");
    let child_claim = RunClaim::from(&child.lease);
    let projection = Arc::new(RecoveryProjection::new());
    projection
        .install(
            &child_run,
            &child_thread,
            RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: child_thread.clone(),
                claimed_run_id: child_run.clone(),
                runs: Vec::new(),
                latest_run_id: None,
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 0,
                store_cursor: 0,
                next_commit_ordinal: 0,
            },
        )
        .expect("R1 child projection install");
    let projection_reader: Arc<dyn CommittedThreadView> = projection.clone();
    let worker = DispatchWorker::new(Arc::new(Runtime::new()), store, commit, "worker")
        .with_context(parent_context)
        .with_recovery_projection(projection.clone())
        .expect("required local recovery source");
    let context = worker
        .execution_context_with(&child_claim, None, &None, &[], &[], None)
        .expect("R1 exact child context");

    assert_eq!(
        context
            .stream_checkpoint
            .as_ref()
            .expect("R1/E1 child checkpoint")
            .get(&child_run.0)
            .await,
        awaken_agent_contract::observation::Observed::Absent,
        "R1/E1 the child's fenced store answers, and answers that nothing is stored"
    );
    assert!(
        Arc::ptr_eq(
            context.reader.as_ref().expect("R1/E2 child reader"),
            &projection_reader,
        ),
        "R1/E2"
    );
    context
        .commit
        .as_ref()
        .expect("R1/E3 child commit")
        .commit(ThreadCommit::assemble(
            child_thread,
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R1/E3 commit matches child projection");
    assert_eq!(
        projection.run_state(&child_run),
        Some(RunState::Running),
        "R1/E3"
    );
    context
        .ownership
        .as_ref()
        .expect("R1/E4 child ownership")
        .verify_current()
        .await
        .expect("R1/E4 child claim is current");
}

#[tokio::test]
async fn worker_rejects_unmatched_claim_delivery_without_panicking() {
    // Cause/effect graph: C1 a claimed binding exists; C2 a transported delivery
    // names that exact candidate and epoch. E1 construct the claim-fenced
    // Runtime context; E2 return a normal execution error before model or tool
    // entry. Constraint: transport validation is defense in depth; the Runtime
    // contract remains the sole delivery-admission authority.
    //
    // | Rule | C1 | C2 | Effect |
    // |---|---|---|---|
    // | R1 | yes | yes | E1 (covered by contract and HTTP integration tests) |
    // | R2 | yes | no  | E2 |
    let store = Arc::new(MemoryDispatchStore::new(Arc::new(
        awaken_lease_testkit::ManualClock::new(0),
    )));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let claimed = store
        .claim_new_run(
            crate::RunDispatch::new(RunActivation::new(
                RunId("delivery-run".into()),
                ThreadId("delivery-thread".into()),
                ExecutableAgentSnapshot::builder("delivery-snapshot")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "test", "native",
                        )
                        .expect("R2 complete model binding"),
                    )
                    .build(),
                Vec::new(),
            )),
            "worker",
            1_000,
            &Default::default(),
        )
        .await
        .expect("R2 fixture admission")
        .expect("R2 fixture claim");
    let claim = RunClaim::from(&claimed.lease);
    let worker = DispatchWorker::new(Arc::new(Runtime::new()), store, commit, "worker");
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let credential = CredentialRef {
        id: "credential-a".into(),
        revision: 1,
    };
    let binding = AttemptCredentialBinding {
        candidate_fingerprint: CandidateFingerprint("candidate-a".into()),
        credential: credential.clone(),
        selected_plaintext_holder: holder.clone(),
        selected_realization_kind: CredentialRealizationKind::WorkerProviderAdapter,
        claim_epoch: claim.epoch,
    };
    let delivery = AttemptCredentialDelivery {
        candidate_fingerprint: CandidateFingerprint("candidate-b".into()),
        access: CredentialAccess::new(
            credential,
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::exact(holder, ModelExposurePolicy::Forbidden),
        ),
        claim_epoch: claim.epoch,
    };

    let result = worker.execution_context_with(&claim, None, &None, &[binding], &[delivery], None);
    assert!(
        matches!(result, Err(Error::Execution(_))),
        "R2/E2 invalid claim delivery must fail closed instead of panicking"
    );
}

#[test]
fn claim_refresh_requires_complete_control_delivery_and_transport() {
    /* Claim-refresh capability table. C1 bindings are absent/present; C2
     * response-local deliveries are absent, complete, or partial; C3 the
     * Worker has the authenticated redelivery transport. E1 only the complete
     * C1+C2+C3 row installs live refresh; E2 every other row keeps the claim's
     * original published/cached envelope path. This reproduces the distributed
     * k3d deployment whose Control intentionally has no delivery source. */
    for (bindings, deliveries, transport, expected) in [
        (0, 0, false, false),
        (0, 0, true, false),
        (1, 0, true, false),
        (1, 1, false, false),
        (2, 1, true, false),
        (1, 1, true, true),
        (2, 2, true, true),
    ] {
        assert_eq!(
            super::claim_credential_refresh_is_available(bindings, deliveries, transport),
            expected,
            "bindings={bindings} deliveries={deliveries} transport={transport}",
        );
    }
}

#[test]
fn remote_constructors_install_one_required_snapshot_target() {
    // C1 erased construction; C2 combined construction. Both must retain one
    // cache shared by snapshot installation, dispatch selection and Runtime.
    // A provisional committed reader cannot bypass the required remote load.
    for combined in [false, true] {
        let identity = awaken_worker_contract::WorkerIdentity::new("worker", "boot", 1);
        let binding = awaken_dispatch_erasure::DispatchBinding::remote(
            Arc::new(awaken_coordinator_client::HttpDispatchQueue::new(
                "http://127.0.0.1:9",
                identity.clone(),
            )),
            Arc::new(awaken_coordinator_client::RemoteClaimedRunCommit::new(
                "http://127.0.0.1:9",
                identity,
            )),
        );
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let worker = if combined {
            DispatchWorker::new_bound(Arc::new(Runtime::new()), binding, commit, "worker")
        } else {
            DispatchWorker::from_binding(
                Arc::new(Runtime::new()),
                binding,
                commit.clone(),
                commit,
                "worker",
            )
            .unwrap()
        };
        let projection: Arc<dyn CommittedThreadView> = worker
            .recovery_projection
            .as_ref()
            .expect("required target")
            .clone();
        assert!(
            Arc::ptr_eq(&worker.reader, &projection),
            "combined={combined}"
        );
        assert!(Arc::ptr_eq(
            &worker.execution_context().reader.expect("Runtime reader"),
            &projection
        ));
        assert!(
            worker.recovery_source.is_none(),
            "remote construction retains RPC authority"
        );
    }
}
