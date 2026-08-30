// Unit tests for the one DispatchWorker implementation. The containing module
// stays in `worker.rs`, preserving every existing test path and design comment.
use std::sync::Arc;

use super::{
    DispatchWorker, cancellation_uses_tool_interruption, combine_attempt_cancellation,
    recovered_attempt_disposition, settle_outcome,
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
use crate::{DispatchQueue, FencedStreamCheckpointStore, MemoryDispatchStore, RunClaim};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn attempt_cancellation_combines_claim_and_host_without_a_second_authority() {
    // Cause/effect graph: C1=claim cancellation is already/currently signalled;
    // C2=an optional host cancellation is already/currently signalled. Effect
    // E1=Runtime's one combined token is cancelled; E2=an absent host preserves
    // the exact claim token instead of spawning a redundant relay.
    //
    // | Rule | Claim | Host | Effect |
    // | AC1 | live | absent | E2; later claim cancellation -> E1 |
    // | AC2 | live | live | either later cancellation -> E1 |
    // | AC3 | cancelled | any | immediate E1 |
    // | AC4 | live | cancelled | immediate E1 |
    // Constraint: this signal relay owns no lifecycle or persistence state.
    let claim_only = CancellationToken::new();
    let (combined, _relay) = combine_attempt_cancellation(claim_only.clone(), None);
    assert_eq!(combined, claim_only, "AC1/E2");
    claim_only.cancel();
    combined.cancelled().await;
    assert!(combined.is_cancelled(), "AC1/E1");

    for cancel_claim in [true, false] {
        let claim = CancellationToken::new();
        let host = CancellationToken::new();
        let (combined, _relay) = combine_attempt_cancellation(claim.clone(), Some(&host));
        if cancel_claim {
            claim.cancel();
        } else {
            host.cancel();
        }
        combined.cancelled().await;
        assert!(combined.is_cancelled(), "AC2/E1");
    }

    let cancelled_claim = CancellationToken::new();
    cancelled_claim.cancel();
    let (combined, _relay) =
        combine_attempt_cancellation(cancelled_claim, Some(&CancellationToken::new()));
    assert!(combined.is_cancelled(), "AC3/E1");

    let cancelled_host = CancellationToken::new();
    cancelled_host.cancel();
    let (combined, _relay) =
        combine_attempt_cancellation(CancellationToken::new(), Some(&cancelled_host));
    assert!(combined.is_cancelled(), "AC4/E1");
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
    });
    let external = ticket(AwaitTarget::ToolCall {
        reason: ToolAwaitReason::ClientExecution,
        call_id: "interrupt-route-client-call".into(),
        tool: PendingTool {
            tool_id: "client-tool".into(),
            arguments: serde_json::json!({}),
        },
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
    // Cause/effect graph: C1 `new` starts with a provisional local projection;
    // C2 a database-independent Worker installs its transport-backed
    // projection; C3 that snapshot says the claimed Run is Awaiting and owns
    // a resume ticket. Effects: E1 the old projection/source are released;
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
    let worker = DispatchWorker::new(
        Arc::new(Runtime::new()),
        Arc::new(MemoryDispatchStore::new()),
        Arc::new(MemoryCommitCoordinator::new()),
        "worker",
    );
    assert!(worker.recovery_source.is_some());
    let provisional = Arc::downgrade(
        worker
            .recovery_projection
            .as_ref()
            .expect("constructor projection"),
    );

    let projection = Arc::new(RecoveryProjection::new());
    let projection_reader: Arc<dyn CommittedThreadView> = projection.clone();
    let worker = worker.with_recovery_projection(projection.clone());

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
            RunRecoverySnapshot {
                thread_id: thread_id.clone(),
                claimed_run_id: run_id.clone(),
                runs: vec![RunRecord {
                    id: run_id.clone(),
                    thread_id,
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
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
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
            0,
            &Default::default(),
        )
        .await
        .expect("R1 parent admission")
        .expect("R1 parent claim");
    let parent_claim = RunClaim::from(&parent.lease);
    let dispatch: Arc<dyn DispatchQueue> = store.clone();
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
            0,
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
            RunRecoverySnapshot {
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
    let worker = DispatchWorker::from_parts(
        Arc::new(Runtime::new()),
        store,
        commit.clone(),
        commit,
        "worker",
    )
    .with_context(parent_context)
    .with_recovery_projection(projection.clone());
    let context = worker.execution_context_with(
        &child_claim,
        None,
        &None,
        &[],
        Arc::new(crate::ManualClock::new(0)),
        None,
    );

    assert!(
        context
            .stream_checkpoint
            .as_ref()
            .expect("R1/E1 child checkpoint")
            .get(&child_run.0)
            .await
            .expect("R1/E1 checkpoint accepts child")
            .is_none(),
        "R1/E1"
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
