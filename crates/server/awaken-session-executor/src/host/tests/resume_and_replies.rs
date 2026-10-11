//! Resume, client tool results, child replies, and budget resumption.

use super::*;

fn awaiting_tool_batch_state(
    run_id: &RunId,
    ticket: &ResumeTicket,
    wait_kind: awaken_tool_contract::ToolWaitKind,
) -> awaken_agent_contract::agent::state::Command {
    let awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall { call_id, tool, .. } =
        ticket.target()
    else {
        panic!("awaiting tool-batch fixture requires a tool ticket")
    };
    let mut batch = awaken_tool_contract::ToolBatch::for_step(
        run_id.clone(),
        0,
        [(
            awaken_runtime_contract::llm::ToolCall {
                call_id: call_id.clone(),
                tool_id: tool.tool_id.clone(),
                arguments: tool.arguments.clone(),
            },
            awaken_tool_contract::ToolRecoveryPolicy::default(),
        )],
    )
    .expect("valid awaiting tool-batch fixture");
    batch
        .mark_awaiting(call_id, wait_kind, ticket.correlation_id.clone())
        .expect("ticket and tool batch enter one exact wait");
    awaken_tool_contract::ActiveToolBatch::write(&Some(batch))
}

async fn host_requiring_write_confirmation(model: Arc<dyn LlmExecutor>) -> Arc<SharedHost> {
    let host = (crate::HostDependencies::ephemeral(model, "stub")
        .await
        .with_gate_override(write_confirmation_gate()))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);
    host
}

// ---------------------------------------------------------------------------
// run/resume fail-closed boundaries (ADR-0048 gap review)
//
// These guard the double-run / wrong-tool / forged-approval seams: a caller must
// not be able to start a second Run on an Awaiting Thread, resume a Run that was never
// Awaiting, answer the wrong pending tool, or cross the built-in↔client-executed
// binding when resuming. All of them must fail *closed* with a BadRequest and
// leave the run untouched.
// ---------------------------------------------------------------------------

/// Awaits on the Ask-gated built-in `write` until it sees a tool result, then ends.
struct AwaitOnWriteModel;

#[async_trait::async_trait]
impl LlmExecutor for AwaitOnWriteModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let saw_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        let output = if saw_tool {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "note.txt", "content": "x" }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A Thread awaiting on a tool decision must reject a fresh `run`: starting a second
/// Run over an Awaiting Run would double-execute the Awaiting Run's side effects. The
/// guard fails closed with BadRequest and does not touch the await.
#[tokio::test]
async fn run_on_an_awaiting_thread_fails_closed() {
    // Causes: C1 committed Thread truth already contains an Awaiting Run.
    // Constraint/Invariant: a new foreground Run cannot bypass the existing
    // resume ticket or create a second active writer. Decision rule: submit under
    // C1 and require the documented fail-closed effect with no new Run.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel)).await;
    let r1 = host
        .run(None, "t-awaiting", user("hi"))
        .await
        .expect("Run 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "Run awaits on write"
    );

    let err = host
        .run(None, "t-awaiting", user("again"))
        .await
        .err()
        .expect("a second run on an awaiting thread must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("awaiting a tool decision"),
        "message names the await: {}",
        err.message
    );

    // The await still resumes cleanly afterwards — the rejected run was a no-op.
    let r2 = host
        .resume(
            "t-awaiting",
            "w1",
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("resume the untouched await");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// Resuming a thread that has no awaiting run is a caller error, not a panic: there
/// is no run to answer, so it fails closed with BadRequest.
#[tokio::test]
async fn resume_with_no_awaiting_run_fails_closed() {
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel)).await;
    let err = host
        .resume(
            "t-idle",
            "w1",
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .err()
        .expect("resume with nothing awaiting must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("no awaiting run"),
        "message names the missing await: {}",
        err.message
    );
}

/// A resume whose `tool_use_id` does not name the pending tool must be rejected —
/// otherwise a caller could resume the wrong tool. Fails closed with BadRequest and
/// the real await survives.
#[tokio::test]
async fn resume_with_a_wrong_tool_use_id_fails_closed() {
    // Test design. Causes: C1 the committed ticket awaits tool-use id A; C2 resume
    // supplies foreign id B. Effects: E1 C2 is rejected and the Run remains
    // Awaiting. Constraint/Invariant: exact tool-use identity fences replies.
    // Decision rule: exercise B != A and require E1 with no tool result commit.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel)).await;
    let r1 = host
        .run(None, "t-wrongid", user("hi"))
        .await
        .expect("Run 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .resume(
            "t-wrongid",
            "not-the-pending-id",
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .err()
        .expect("a mismatched tool_use_id must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("does not match the pending tool"),
        "message names the mismatch: {}",
        err.message
    );

    // The genuine id still resumes — the mismatch did not consume the await.
    let r2 = host
        .resume(
            "t-wrongid",
            &r1.pending.expect("a pending tool").tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("the real id resumes");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// The built-in↔client binding is enforced on resume: a client-tool *result* may
/// not answer a built-in (Ask-gated) tool. Failing open here would let a caller
/// forge an approval by delivering a fabricated result instead of a decision.
#[tokio::test]
async fn client_result_cannot_answer_a_builtin_tool() {
    // Test design R7. Causes: C1 the pending ticket is a permission target; C2 a
    // custom/generic client result targets it. Effects: E1 C2 is rejected; E2 no
    // reply is committed and the exact ticket remains resumable. Constraint:
    // client results answer only client-owned targets. Decision rule: exercise
    // the mismatch, then resume the same ticket correctly to prove E1+E2.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel)).await;
    let r1 = host.run(None, "t-bind1", user("hi")).await.expect("Run 1");
    let pending = r1.pending.expect("awaiting on the built-in write");
    assert!(!pending.client_executed, "write is a built-in tool");

    let err = host
        .resume(
            "t-bind1",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("forged")],
                is_error: false,
            },
        )
        .await
        .err()
        .expect("a client result must not answer a built-in tool");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("built-in"),
        "message names the binding: {}",
        err.message
    );
    let resumed = host
        .resume(
            "t-bind1",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("R7 rejected mismatch leaves the exact permission await intact");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R7/E2");
}

/// The other direction of the binding: a confirmation may not answer a
/// client-executed tool (which expects a result, not a permission decision).
#[tokio::test]
async fn confirm_cannot_answer_a_client_tool() {
    // Test design R6. Causes: C1 the pending ticket belongs to a client-executed
    // target; C2 a confirm/permission reply targets it. Effects: E1 C2 is
    // rejected; E2 no reply is committed and the exact ticket remains resumable.
    // Constraint: typed reply kind must match target ownership. Decision rule:
    // exercise the inverse mismatch, then answer correctly to prove E1+E2.
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(ClientLookupModel), "stub")
            .await
            .with_client_tools(HashSet::from(["lookup".to_string()])))
        .build()
        .expect("validated Host dependencies"),
    );
    let r1 = host.run(None, "t-bind2", user("hi")).await.expect("Run 1");
    let pending = r1.pending.expect("awaiting on the client tool");
    assert!(pending.client_executed, "lookup is client-executed");

    let err = host
        .resume(
            "t-bind2",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .err()
        .expect("a confirmation must not answer a client tool");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("client-executed"),
        "message names the binding: {}",
        err.message
    );
    let resumed = host
        .resume(
            "t-bind2",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("R6 rejected mismatch leaves the exact client await intact");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R6/E2");
}

/// The happy path for the client-executed binding: a `ClientResult` delivers the
/// caller-Run tool's output, it reaches the model's next inference, and the Run
/// ends. This is the direct-ingress row of the resume-delivery decision table;
/// `durable_client_result_settles_the_authoritative_dispatch` covers the durable
/// row against the same model and protocol-neutral Host API.
#[tokio::test]
async fn client_result_delivers_a_client_tool_result_and_ends_the_run() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 direct ingress; C2 valid committed client-tool
    // ticket; C3 exact ClientResult. Effects: E1 resume executes inline once;
    // E2 result reaches the next inference; E3 Run ends. Constraint: no durable
    // dispatch is authored in direct mode.
    //
    // | Rule | ingress | ticket | answer | Effects |
    // | R1 | direct | valid client tool | exact result | E1+E2+E3 |
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(ClientLookupModel), "stub")
            .await
            .with_client_tools(HashSet::from(["lookup".to_string()])))
        .build()
        .expect("validated Host dependencies"),
    );
    let r1 = host.run(None, "t-client", user("hi")).await.expect("Run 1");
    let pending = r1.pending.expect("awaiting on the client tool");

    let r2 = host
        .resume(
            "t-client",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("client result resumes");
    assert!(matches!(r2.state, RunState::Ended(_)), "the Run ends");
    let reply = r2
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "result was sunny",
        "the delivered client result reached the model's next inference"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_foreground_run_relays_live_progress_before_committed_completion() {
    // Cause/effect graph: C1 the Host uses durable dispatch with a local pool;
    // C2 a foreground run supplies a StreamSink; C3 the model emits a live text
    // delta; C4 the run commits its terminal state. Effects: E1 C3 reaches the
    // caller's exact sink while its run-id registration is active; E2 C4 returns
    // the same authoritative terminal result as the non-streaming durable path;
    // E3 the registration is removed at settlement (owned by the registry unit
    // test). Constraint: background runs
    // and remote workers without this process-local registration still use the
    // committed projection and never create a durable delta source of truth.
    //
    // | Rule | durable | foreground sink | local worker | Effects |
    // | R1 | yes | yes | yes | E1+E2+E3 |
    // | R2 | yes | no | yes | E2 (existing durable tests) |
    // | R3 | yes | yes | remote | E2 fallback; no false replay guarantee |
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("in-memory dispatch"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();
    let sink = Arc::new(awaken_store_inmem::MemoryStreamSink::new());

    let outcome = host
        .run_streaming(
            None,
            "t-durable-live",
            user("stream this"),
            sink.clone() as Arc<dyn awaken_agent_contract::stream::sink::Sink>,
        )
        .await
        .expect("durable streaming run");

    assert!(matches!(outcome.state, RunState::Ended(_)), "R1/E2");
    assert!(!sink.events().is_empty(), "R1/E1");
}

#[tokio::test]
async fn terminal_child_report_atomically_admits_one_deterministic_primary_run() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 child boundary is terminal; C2 report provenance
    // matches the exact child Run; C3 exact delivery is retried; C4 the child
    // activity epoch is still active. Effects: E1 freeze the exact report into
    // the deterministic primary activation; E2 enqueue one deterministic primary
    // Run carrying C4; E3 retry is an exact no-op; E4 no Outbox/Inbox copy exists.
    // Awaiting is intentionally absent: Managed projects its child lifecycle
    // directly and SessionApplication never invokes this command.
    //
    // | Rule | State | Provenance | Replay | Epoch | Effect |
    // | R1 | Ended | exact | no | active | E1+E2+E4 |
    // | R2 | Ended | exact | yes | same | E3 |
    // | R3 | Ended | wrong | any | any | reject before mutation |
    // | R4 | Ended | exact | any | zero | reject before mutation |
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("in-memory dispatch"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);
    let child_run = RunId("child-report-run".into());
    let command = awaken_session_execution_contract::SessionAgentReportContinuation {
        session_id: "primary-report".into(),
        source_thread_id: ThreadId("child-report-thread".into()),
        source_run_id: child_run.clone(),
        session_activity_epoch: 23,
        message: Message::text(
            MessageId::agent_thread_report(&child_run),
            Role::User,
            "Message from agent Researcher (thread child-report-thread):\nanswer",
        ),
    };

    host.continue_session_agent_report(command.clone())
        .await
        .expect("R1 terminal continuation");
    host.continue_session_agent_report(command)
        .await
        .expect("R2 exact replay");
    let rows = dispatch.list_dispatches().await.expect("R1 dispatch");
    assert_eq!(rows.len(), 1, "R1/R2 one deterministic root");
    assert_eq!(rows[0].thread_id, ThreadId("primary-report".into()));
    let inbox = dispatch
        .list(&ThreadId("primary-report".into()))
        .await
        .expect("R1 primary Inbox");
    assert!(inbox.is_empty(), "R1/E4 no parallel Inbox message");

    let bad_run = RunId("other-child".into());
    let rejected = host
        .continue_session_agent_report(
            awaken_session_execution_contract::SessionAgentReportContinuation {
                session_id: "primary-report".into(),
                source_thread_id: ThreadId("child-report-thread".into()),
                source_run_id: bad_run,
                session_activity_epoch: 23,
                message: Message::text(
                    MessageId::agent_thread_report(&child_run),
                    Role::User,
                    "forged",
                ),
            },
        )
        .await;
    assert!(rejected.is_err(), "R3");
    assert_eq!(dispatch.list_dispatches().await.unwrap().len(), 1, "R3");
    let zero_epoch = host
        .continue_session_agent_report(
            awaken_session_execution_contract::SessionAgentReportContinuation {
                session_id: "primary-report".into(),
                source_thread_id: ThreadId("child-report-thread".into()),
                source_run_id: child_run.clone(),
                session_activity_epoch: 0,
                message: Message::text(
                    MessageId::agent_thread_report(&child_run),
                    Role::User,
                    "missing activity",
                ),
            },
        )
        .await;
    assert!(zero_epoch.is_err(), "R4");
    assert_eq!(dispatch.list_dispatches().await.unwrap().len(), 1, "R4");
    clock.set(1);
    let claimed = dispatch
        .claim("report-epoch-inspector", 30_000, &Default::default())
        .await
        .expect("R1 claim")
        .expect("R1 report root remains runnable");
    assert_eq!(
        claimed.request.session_activity_epoch,
        Some(23),
        "R1 activity handoff is owned by the canonical dispatch, not its operational summary"
    );
    assert_eq!(
        claimed.request.activation.input.len(),
        1,
        "R1/E1 exact input"
    );
    assert_eq!(
        claimed.request.activation.input[0].id,
        MessageId::agent_thread_report(&child_run),
        "R1/E1 typed provenance stays inside the target Run"
    );
}

#[tokio::test]
async fn coordinated_child_reply_rotates_activity_at_the_parent_affined_outbox_boundary() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 parent-partition committed ticket matches the
    // child/tool; C2 dispatch is the exact unleased Awaiting child; C3 reply is
    // fresh/exact/conflicting; C4 Session activity epoch is the new trusted
    // coordinate; C5 the preflight Run/correlation fence is current/stale; C6
    // an ordinary ExistingThread follow-up is already bound to a deterministic
    // fresh Run; C7 that same operation is retried exactly or with changed
    // content; C8 accompanying System is exact/changed; C9 a later await reuses
    // the provider Run/correlation/call id under a new public Event occurrence.
    // Effects: E1 one Outbox delivery is relayed; E2 dispatch epoch rotates
    // before it becomes runnable; E3 exact retry creates no second input; E4 a
    // conflicting reply is rejected and cannot rotate again; E5 a concurrent
    // await change is rejected before any Outbox or dispatch mutation; E6 C6
    // stays queued while the old Awaiting Run owns the Thread; E7 the typed reply
    // wakes only that old Run, then its settlement releases the fresh follow-up;
    // E8 exact operation retry keeps one Run/message and changed content fails
    // without another effect; E9 a changed System payload conflicts just like a
    // changed reply and cannot create another durable delivery; E10 a delayed
    // old occurrence cannot answer the later await; E11 the newly admitted
    // occurrence remains independently deliverable.
    //
    // | Rule | Ticket/affinity | Dispatch | Reply | Effect |
    // |---|---|---|---|---|
    // | CR1 | exact | Awaiting | fresh | E1+E2 |
    // | CR2 | exact | Running with exact evidence | exact | E3 |
    // | CR3 | exact | Running with exact evidence | conflict | E4 |
    // | CR4 | stale fence | Awaiting | fresh | E5 |
    // | CR5 | exact + fresh follow-up | Awaiting | fresh/exact reply | E6+E7 |
    // | CR6 | exact + fresh follow-up | Awaiting | exact/changed follow-up retry | E8 |
    // | CR7 | exact + changed System | Running with evidence | fresh | E9 |
    // | CR10 | later identical await + old Thread version | exact old receipt | E10 |
    // | CR11 | later identical await + current version/new Event id | active | E11 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PendingTool, ToolAwaitReason};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch::{
        ContinuationAdmission, DispatchOutcome, Outbox as _, SessionChildAdmission,
    };

    let parent = ThreadId("coordinated-reply-parent".into());
    let child = ThreadId("coordinated-reply-child".into());
    let child_run = RunId("coordinated-reply-run".into());
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("coordinated reply dispatch"),
    );
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
            .await
            .with_ephemeral_dispatch(dispatch.clone()))
        .build()
        .expect("validated Host dependencies"),
    );
    let ctx = host
        .ctx_for(&parent.0, None)
        .await
        .expect("parent Session context");
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("parent commit partition");
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("commit child Running");
    let ticket = ResumeTicket::new(
        "coordinated-reply-correlation",
        child_run.clone(),
        child.clone(),
        "coordinated-reply-snapshot",
        "coordinated-reply-catalog",
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "coordinated-reply-tool-use".into(),
            tool: PendingTool {
                tool_id: "write".into(),
                arguments: serde_json::json!({"path": "answer.txt"}),
            },
            authority: ToolCallAuthority::RuntimeToolBatch,
        },
    );
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            vec![awaiting_tool_batch_state(
                &child_run,
                &ticket,
                awaken_tool_contract::ToolWaitKind::ToolPermission,
            )],
            Vec::new(),
        ))
        .await
        .expect("commit child Awaiting ticket");
    let admitted_thread_version = commit
        .recovery_snapshot(&child, &child_run)
        .await
        .expect("read admitted reply coordinate")
        .thread_version;

    let initial_epoch = 41;
    let resumed_epoch = 42;
    let request = RunDispatch::new(RunActivation::new(
        child_run.clone(),
        child.clone(),
        ctx.config.clone(),
        Vec::new(),
    ))
    .for_session(parent.clone())
    .with_session_activity_epoch(initial_epoch);
    dispatch
        .enqueue(request)
        .await
        .expect("enqueue child dispatch");
    clock.set(0);
    let claimed = dispatch
        .claim_run(
            &child_run,
            "coordinated-reply-owner",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("claim child dispatch")
        .expect("child dispatch is runnable");
    assert_eq!(
        dispatch
            .settle(
                &child_run,
                claimed.lease.epoch(),
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("settle child Awaiting"),
        awaken_dispatch_contract::SettleOutcome::Applied
    );

    let follow_up_run = RunId("coordinated-reply-follow-up".into());
    let follow_up_request = RunDispatch::new(RunActivation::new(
        follow_up_run.clone(),
        child.clone(),
        ctx.config.clone(),
        Vec::new(),
    ))
    .for_session(parent.clone())
    .with_session_activity_epoch(43);
    let follow_up_input = awaken_dispatch_contract::PendingInput {
        message_id: "coordinated-reply-follow-up-message".into(),
        run_id: follow_up_run.clone(),
        thread_id: child.clone(),
        correlation_id: String::new(),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: awaken_runtime_contract::resume::ResumeResult::Input(
            "continue after the pending tool".into(),
        ),
    };
    let follow_up_admission =
        || ContinuationAdmission::SessionChild(SessionChildAdmission::new(24, Vec::new()));
    dispatch
        .relay_and_enqueue(
            follow_up_input.clone(),
            follow_up_request.clone(),
            follow_up_admission(),
        )
        .await
        .expect("CR5 admit fresh ExistingThread follow-up");
    dispatch
        .relay_and_enqueue(
            follow_up_input.clone(),
            follow_up_request.clone(),
            follow_up_admission(),
        )
        .await
        .expect("CR6 exact follow-up retry");
    let changed_follow_up = awaken_dispatch_contract::PendingInput {
        context_messages: Vec::new(),
        result: awaken_runtime_contract::resume::ResumeResult::Input("changed intent".into()),
        ..follow_up_input.clone()
    };
    assert!(
        dispatch
            .relay_and_enqueue(
                changed_follow_up,
                follow_up_request.clone(),
                follow_up_admission(),
            )
            .await
            .is_err(),
        "CR6/E8 one operation cannot change its message"
    );
    assert_eq!(
        dispatch.list(&child).await.expect("CR6 child Inbox"),
        vec![awaken_dispatch_contract::PendingRecord {
            input: follow_up_input.clone(),
            revision: 1,
        }],
        "CR6/E8 one fresh Run owns one follow-up message"
    );
    clock.set(1);
    assert!(
        dispatch
            .claim_run(
                &follow_up_run,
                "coordinated-reply-owner",
                DEFAULT_LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("CR5 inspect queued follow-up")
            .is_none(),
        "CR5/E6 the old Awaiting Run remains the open writer"
    );

    let command = awaken_session_execution_contract::SessionThreadToolReplyCommand {
        session_id: SessionId::from(&parent.0),
        tool_request_event_id: Some("evt-coordinated-reply-tool-use-1".into()),
        expected_thread_version: Some(admitted_thread_version),
        target: awaken_session_execution_contract::SessionThreadTarget::Child(child.clone()),
        expected_run_id: child_run.clone(),
        expected_correlation_id: ticket.correlation_id.clone(),
        tool_use_id: "coordinated-reply-tool-use".into(),
        reply: awaken_session_execution_contract::SessionThreadToolReply::Confirm(
            PermissionDecision::Allow { note: None },
        ),
        accompanying_system: Some(awaken_session_event_contract::SessionUserRunSystemInput {
            operation_id: "coordinated-reply-system".into(),
            content: vec![ContentBlock::text("reply context")],
        }),
    };
    let fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("trusted Awaiting fence");
    let mut stale_fence = fence.clone();
    if let awaken_session_execution_contract::SessionThreadToolReplyActivity::Session {
        prior_epoch: Some(epoch),
    } = &mut stale_fence.activity
    {
        *epoch += 1;
    } else {
        panic!("this fixture starts from a coordinated activity");
    }
    assert!(
        host.reply_session_thread_tool(
            awaken_session_execution_contract::SessionThreadToolReplyDelivery {
                command: command.clone(),
                fence: stale_fence,
                session_activity_epoch: Some(resumed_epoch),
            }
        )
        .await
        .is_err(),
        "CR4/E5"
    );
    assert_eq!(
        dispatch.relay().await.expect("CR4 no delivery"),
        0,
        "CR4/E5"
    );
    host.reply_session_thread_tool(
        awaken_session_execution_contract::SessionThreadToolReplyDelivery {
            command: command.clone(),
            fence: fence.clone(),
            session_activity_epoch: Some(resumed_epoch),
        },
    )
    .await
    .expect("CR1 stage coordinated reply");
    clock.set(1);
    let resumed = dispatch
        .claim_run(
            &child_run,
            "coordinated-reply-owner",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("CR1 claim resumed child")
        .expect("CR1 relayed reply makes child runnable");
    assert_eq!(
        resumed.request.session_activity_epoch,
        Some(resumed_epoch),
        "CR1/E2"
    );
    assert_eq!(resumed.pending.len(), 1, "CR1/E1");
    assert_eq!(
        resumed.pending[0].correlation_id, ticket.correlation_id,
        "CR1/E1"
    );
    assert_eq!(
        resumed.pending[0].run_id, child_run,
        "CR5/E7 the exact typed reply wakes the old Run, not the follow-up"
    );
    assert_eq!(
        resumed.pending[0]
            .context_messages
            .iter()
            .map(|message| (&message.role, message.text_content()))
            .collect::<Vec<_>>(),
        vec![(&Role::System, "reply context".to_string())],
        "CR1/E1 the stable accompanying System is frozen in the same PendingInput"
    );
    clock.set(2);
    assert!(
        dispatch
            .claim_run(
                &follow_up_run,
                "coordinated-reply-owner",
                DEFAULT_LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("CR5 inspect follow-up while old Run is resumed")
            .is_none(),
        "CR5/E6 the resumed old Run still owns the Thread"
    );

    host.reply_session_thread_tool(
        awaken_session_execution_contract::SessionThreadToolReplyDelivery {
            command: command.clone(),
            fence: fence.clone(),
            session_activity_epoch: Some(resumed_epoch),
        },
    )
    .await
    .expect("CR2 exact reply replay");
    let retried_inbox = dispatch.list(&child).await.expect("CR2 Inbox");
    assert_eq!(
        retried_inbox.len(),
        2,
        "CR2/E3 includes the queued follow-up"
    );
    assert_eq!(
        retried_inbox
            .iter()
            .filter(|record| record.input.run_id == child_run)
            .count(),
        1,
        "CR2/E3 the exact typed reply is not duplicated"
    );
    assert_eq!(
        retried_inbox
            .iter()
            .filter(|record| record.input.run_id == follow_up_run)
            .count(),
        1,
        "CR6/E8 the follow-up remains one message"
    );

    // Model the Runtime's single successful resume commit: Running consumes
    // the active ticket while ResumeApplied remains as the durable ingress
    // receipt. This is the response-loss partition that the old implementation
    // could not distinguish from an unresolved/stale reply.
    //
    // | Rule | Active ticket | Receipt | Retried payload | Effect |
    // |---|---|---|---|---|
    // | CR8 | absent | exact O1/correlation | exact O1 | already applied/no-op |
    // | CR9 | absent | O1/correlation | changed O2 | conflict/no mutation |
    // Constraint/invariant: ticket removal and receipt creation share the
    // Runtime ThreadCommit; neither Session nor dispatch owns a shadow receipt.
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            vec![
                awaken_agent_contract::audit::run_event::RunEvent::ResumeApplied {
                    operation_id: command.delivery_operation_id(),
                    correlation_id: ticket.correlation_id.clone(),
                }
                .into(),
            ],
        ))
        .await
        .expect("commit the accepted reply receipt while consuming its ticket");
    let replay_fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("CR8 receipt replaces the consumed ticket for exact retry");
    assert!(
        replay_fence.already_applied,
        "CR8 exact receipt is terminal"
    );
    host.reply_session_thread_tool(
        awaken_session_execution_contract::SessionThreadToolReplyDelivery {
            command: command.clone(),
            fence: replay_fence,
            session_activity_epoch: Some(resumed_epoch),
        },
    )
    .await
    .expect("CR8 exact response-loss retry is a no-op");
    assert_eq!(
        dispatch
            .list(&child)
            .await
            .expect("CR8 unchanged Inbox")
            .len(),
        2,
        "CR8 does not stage a second input"
    );

    let mut conflicting_system = command.clone();
    conflicting_system
        .accompanying_system
        .as_mut()
        .expect("CR7 System")
        .content = vec![ContentBlock::text("changed reply context")];
    assert!(
        host.reply_session_thread_tool(
            awaken_session_execution_contract::SessionThreadToolReplyDelivery {
                command: conflicting_system,
                fence: fence.clone(),
                session_activity_epoch: Some(resumed_epoch + 1),
            }
        )
        .await
        .is_err(),
        "CR7/E9"
    );
    assert_eq!(
        dispatch.relay().await.expect("CR7 no staged conflict"),
        0,
        "CR7/E9"
    );

    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("commit a later await that reuses provider coordinates");
    let later_thread_version = commit
        .recovery_snapshot(&child, &child_run)
        .await
        .expect("read later reply coordinate")
        .thread_version;
    let delayed_old_fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("CR10 old receipt absorbs a delayed old occurrence");
    assert!(delayed_old_fence.already_applied, "CR10/E10");
    let mut later_occurrence = command.clone();
    later_occurrence.tool_request_event_id = Some("evt-coordinated-reply-tool-use-2".into());
    later_occurrence.expected_thread_version = Some(later_thread_version);
    assert_ne!(
        later_occurrence.delivery_operation_id(),
        command.delivery_operation_id(),
        "CR11 each public Event occurrence owns a distinct durable operation"
    );
    let later_fence = host
        .session_thread_tool_reply_fence(&later_occurrence)
        .await
        .expect("CR11 current occurrence targets the later active await");
    assert!(!later_fence.already_applied, "CR11/E11");

    let mut conflicting = command;
    conflicting.reply = awaken_session_execution_contract::SessionThreadToolReply::Confirm(
        PermissionDecision::Deny {
            reason: Some("changed".into()),
        },
    );
    assert!(
        host.reply_session_thread_tool(
            awaken_session_execution_contract::SessionThreadToolReplyDelivery {
                command: conflicting,
                fence,
                session_activity_epoch: Some(resumed_epoch + 1),
            },
        )
        .await
        .is_err(),
        "CR3/E4"
    );
    assert_eq!(
        dispatch.relay().await.expect("CR3 no staged conflict"),
        0,
        "CR3/E4"
    );
    assert_eq!(
        dispatch
            .settle(
                &child_run,
                resumed.lease.epoch(),
                DispatchOutcome::Done,
                &[resumed.pending[0].message_id.clone()],
            )
            .await
            .expect("CR5 settle the replied-to old Run"),
        awaken_dispatch_contract::SettleOutcome::Applied,
        "CR5/E7"
    );
    clock.set(3);
    let follow_up = dispatch
        .claim_run(
            &follow_up_run,
            "coordinated-reply-owner",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("CR5 claim released follow-up")
        .expect("CR5 old Run settlement releases the follow-up");
    assert_eq!(follow_up.pending, vec![follow_up_input], "CR5/E7");
}

#[test]
fn tool_reply_authority_decision_table_preserves_runtime_and_executor_owners() {
    // Causes: C1 authority is Runtime/Executor/Legacy; C2 an exact Runtime
    // ToolBatch is present/absent; C3 the legacy reason is permission/client.
    // Effects: E1 Runtime requires and validates its exact batch; E2 Executor
    // ignores any stale Runtime cell and keeps its closed ticket authoritative;
    // E3 legacy exact batch is validated; E4 legacy permission without a batch
    // remains upgrade-resumable; E5 ambiguous legacy client waits fail closed.
    //
    // | Rule | Authority | Batch | Reason | Effect |
    // |---|---|---|---|---|
    // | A1 | Runtime | exact | permission | E1 accept |
    // | A2 | Runtime | absent | permission | E1 reject |
    // | A3 | Executor | absent/stale | permission | E2 accept |
    // | A4 | Legacy | exact | permission | E3 accept |
    // | A5 | Legacy | absent | permission | E4 accept |
    // | A6 | Legacy | absent | client | E5 reject |
    use awaken_agent_contract::agent::awaiting::{PendingTool, ToolAwaitReason};

    let thread = ThreadId("authority-thread".into());
    let run = RunId("authority-run".into());
    let ticket = |authority, reason| {
        ResumeTicket::new(
            "authority-correlation",
            run.clone(),
            thread.clone(),
            "authority-snapshot",
            "authority-catalog",
            AwaitTarget::ToolCall {
                reason,
                call_id: "authority-call".into(),
                tool: PendingTool {
                    tool_id: "bash".into(),
                    arguments: serde_json::json!({"command": "true"}),
                },
                authority,
            },
        )
    };
    let runtime = ticket(
        ToolCallAuthority::RuntimeToolBatch,
        ToolAwaitReason::Permission,
    );
    let exact_state = [awaiting_tool_batch_state(
        &run,
        &runtime,
        awaken_tool_contract::ToolWaitKind::ToolPermission,
    )];
    assert!(
        crate::host::coordination::validate_awaiting_tool_authority(
            &thread,
            &runtime,
            &exact_state
        )
        .is_ok(),
        "A1/E1"
    );
    assert!(
        crate::host::coordination::validate_awaiting_tool_authority(&thread, &runtime, &[])
            .is_err(),
        "A2/E1"
    );

    let executor = ticket(
        ToolCallAuthority::RunAttemptExecutor,
        ToolAwaitReason::Permission,
    );
    for (rule, state) in [("A3a", &[][..]), ("A3b", &exact_state[..])] {
        assert!(
            crate::host::coordination::validate_awaiting_tool_authority(&thread, &executor, state)
                .is_ok(),
            "{rule}/E2"
        );
    }
    let legacy_permission = ticket(
        ToolCallAuthority::LegacyUnspecified,
        ToolAwaitReason::Permission,
    );
    assert!(
        crate::host::coordination::validate_awaiting_tool_authority(
            &thread,
            &legacy_permission,
            &exact_state,
        )
        .is_ok(),
        "A4/E3"
    );
    assert!(
        crate::host::coordination::validate_awaiting_tool_authority(
            &thread,
            &legacy_permission,
            &[]
        )
        .is_ok(),
        "A5/E4"
    );
    let legacy_client = ticket(
        ToolCallAuthority::LegacyUnspecified,
        ToolAwaitReason::ClientExecution,
    );
    assert!(
        crate::host::coordination::validate_awaiting_tool_authority(&thread, &legacy_client, &[])
            .is_err(),
        "A6/E5"
    );
}

#[tokio::test]
async fn remote_input_reply_uses_its_ticket_without_inventing_a_tool_batch() {
    // Cause C1: AwaitTarget is a local ToolCall or protocol-owned RemoteInput.
    // Cause C2: the public reply call id and client-result kind match the ticket.
    // Effect E1: ToolCall additionally requires an exact ActiveToolBatch; E2:
    // RemoteInput succeeds from its closed ticket and no parallel batch exists;
    // E3: a mismatched remote call id is rejected.
    //
    // | Rule | C1 | C2 | Durable batch | Effect |
    // |---|---|---|---|---|
    // | R1 | ToolCall | yes | absent | reject (E1) |
    // | R2 | RemoteInput | yes | absent by design | accept (E2) |
    // | R3 | RemoteInput | no | absent by design | reject (E3) |
    use awaken_agent_contract::agent::awaiting::{
        AwaitTarget, PendingTool, RemoteInputReason, ResumeTicket, ToolAwaitReason,
    };
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};

    let thread = ThreadId("remote-input-reply-session".into());
    let run_id = RunId("remote-input-reply-run".into());
    let host = (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let commit = host
        .commit_for_read(&thread.0)
        .await
        .expect("remote input commit partition");
    commit
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::running(run_id.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("remote input Running");
    let ticket = ResumeTicket::new(
        "remote-input-correlation",
        run_id.clone(),
        thread.clone(),
        "remote-input-snapshot",
        "remote-input-catalog",
        AwaitTarget::RemoteInput {
            reason: RemoteInputReason::UserInput,
            call_id: "remote-task-7".into(),
        },
    );
    commit
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R2 RemoteInput without ToolBatch");
    let command = awaken_session_execution_contract::SessionThreadToolReplyCommand {
        session_id: SessionId::from(&thread.0),
        tool_request_event_id: Some("remote-input-event".into()),
        expected_thread_version: None,
        target: awaken_session_execution_contract::SessionThreadTarget::Primary,
        expected_run_id: run_id,
        expected_correlation_id: ticket.correlation_id.clone(),
        tool_use_id: "remote-task-7".into(),
        reply: awaken_session_execution_contract::SessionThreadToolReply::Result {
            content: vec![ContentBlock::text("README.md")],
            is_error: false,
        },
        accompanying_system: None,
    };
    assert!(
        host.validated_session_thread_tool_reply(&command)
            .await
            .is_ok(),
        "R2/E2"
    );
    let mut stale = command;
    stale.tool_use_id = "another-task".into();
    assert!(
        host.validated_session_thread_tool_reply(&stale)
            .await
            .is_err(),
        "R3/E3"
    );

    let tool_thread = ThreadId("missing-tool-batch-session".into());
    let tool_run = RunId("missing-tool-batch-run".into());
    let tool_host = (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let tool_commit = tool_host
        .commit_for_read(&tool_thread.0)
        .await
        .expect("tool commit partition");
    tool_commit
        .commit(ThreadCommit::assemble(
            tool_thread.clone(),
            RunDisposition::running(tool_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("tool Running");
    let tool_ticket = ResumeTicket::new(
        "missing-tool-batch-correlation",
        tool_run.clone(),
        tool_thread.clone(),
        "missing-tool-batch-snapshot",
        "missing-tool-batch-catalog",
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::ClientExecution,
            call_id: "client-call".into(),
            tool: PendingTool {
                tool_id: "client-tool".into(),
                arguments: serde_json::json!({}),
            },
            authority: ToolCallAuthority::RuntimeToolBatch,
        },
    );
    tool_commit
        .commit(ThreadCommit::assemble(
            tool_thread.clone(),
            RunDisposition::awaiting(tool_ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R1 ToolCall without ToolBatch");
    let tool_command = awaken_session_execution_contract::SessionThreadToolReplyCommand {
        session_id: SessionId::from(&tool_thread.0),
        tool_request_event_id: Some("missing-tool-batch-event".into()),
        expected_thread_version: None,
        target: awaken_session_execution_contract::SessionThreadTarget::Primary,
        expected_run_id: tool_run,
        expected_correlation_id: tool_ticket.correlation_id,
        tool_use_id: "client-call".into(),
        reply: awaken_session_execution_contract::SessionThreadToolReply::Result {
            content: vec![ContentBlock::text("result")],
            is_error: false,
        },
        accompanying_system: None,
    };
    assert!(
        tool_host
            .validated_session_thread_tool_reply(&tool_command)
            .await
            .is_err(),
        "R1/E1"
    );
}

#[tokio::test]
async fn primary_generic_tool_result_uses_the_same_fenced_durable_reply_path() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 target is Primary; C2 committed ticket is the exact
    // ExternalEvent Run/correlation/tool; C3 the generic result is normal/error;
    // C4 expected Run/correlation is exact/stale; C5 the committed ToolBatch
    // payload agrees/conflicts with the ticket. Effects: E1 the existing root
    // dispatch activity rotates and one client ToolOutput is staged; E2 normal
    // content and is_error survive unchanged; E3 stale admission coordinates
    // fail before Outbox mutation; E4 a C5 conflict also fails before Outbox
    // mutation. Child confirmation coverage lives in the
    // sibling decision table above; both targets use this same Host boundary.
    //
    // | Rule | Target | Ticket | Result | Effect |
    // |---|---|---|---|---|
    // | PR1 | Primary | exact | normal generic | E1+E2 |
    // | PR2 | Primary | stale Run/correlation | normal generic | E3 |
    // | PR3 | Primary | exact ticket/conflicting batch | normal generic | E4 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PendingTool, ToolAwaitReason};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch::{DispatchOutcome, Outbox as _, RunDispatch};

    // PR4 repeats PR1-PR3 for an extension-owned Run: same ticket/result checks,
    // no Session epoch, unchanged ordinary affinity. This is the real Outcome tool boundary.
    for extension in [false, true] {
        let parent = ThreadId(format!("primary-reply-session-{extension}"));
        let run_id = RunId("primary-reply-run".into());
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("primary reply dispatch"),
        );
        let (host, _managed) = dispatch_test_host(
            (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
                .await
                .with_ephemeral_dispatch(dispatch.clone()))
            .build()
            .expect("validated Host dependencies"),
        );
        let ctx = host
            .ctx_for(&parent.0, None)
            .await
            .expect("primary Session context");
        let commit = host
            .commit_for_read(&parent.0)
            .await
            .expect("primary commit partition");
        commit
            .commit(ThreadCommit::assemble(
                parent.clone(),
                RunDisposition::running(run_id.clone()),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("commit primary Running");
        let ticket = ResumeTicket::new(
            "primary-reply-correlation",
            run_id.clone(),
            parent.clone(),
            "primary-reply-snapshot",
            "primary-reply-catalog",
            AwaitTarget::ToolCall {
                reason: ToolAwaitReason::ClientExecution,
                call_id: "primary-reply-tool-use".into(),
                tool: PendingTool {
                    tool_id: "client_lookup".into(),
                    arguments: serde_json::json!({"query": "answer"}),
                },
                authority: ToolCallAuthority::RuntimeToolBatch,
            },
        );
        commit
            .commit(ThreadCommit::assemble(
                parent.clone(),
                RunDisposition::awaiting(ticket.clone()),
                true,
                Vec::new(),
                vec![awaiting_tool_batch_state(
                    &run_id,
                    &ticket,
                    awaken_tool_contract::ToolWaitKind::ExternalResult,
                )],
                Vec::new(),
            ))
            .await
            .expect("commit primary Awaiting ticket");

        let initial_epoch = 51;
        let resumed_epoch = 52;
        let ordinary = RunDispatch::new(RunActivation::new(
            run_id.clone(),
            parent.clone(),
            ctx.config.clone(),
            Vec::new(),
        ));
        let request = if extension {
            ordinary
        } else {
            ordinary
                .for_session(parent.clone())
                .with_session_activity_epoch(initial_epoch)
        };
        dispatch
            .enqueue(request)
            .await
            .expect("enqueue primary dispatch");
        clock.set(0);
        let claimed = dispatch
            .claim_run(
                &run_id,
                "primary-reply-owner",
                DEFAULT_LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("claim primary dispatch")
            .expect("primary dispatch runnable");
        assert_eq!(
            dispatch
                .settle(
                    &run_id,
                    claimed.lease.epoch(),
                    DispatchOutcome::Awaiting,
                    &[],
                )
                .await
                .expect("settle primary Awaiting"),
            awaken_dispatch_contract::SettleOutcome::Applied
        );

        let command = awaken_session_execution_contract::SessionThreadToolReplyCommand {
            session_id: SessionId::from(&parent.0),
            tool_request_event_id: Some("evt-primary-reply-tool-use-1".into()),
            expected_thread_version: None,
            target: awaken_session_execution_contract::SessionThreadTarget::Primary,
            expected_run_id: run_id.clone(),
            expected_correlation_id: ticket.correlation_id.clone(),
            tool_use_id: "primary-reply-tool-use".into(),
            reply: awaken_session_execution_contract::SessionThreadToolReply::Result {
                content: vec![ContentBlock::text("client answer")],
                is_error: false,
            },
            accompanying_system: None,
        };
        let mut stale = command.clone();
        stale.expected_run_id = RunId("later-primary-run".into());
        assert!(
            host.session_thread_tool_reply_fence(&stale).await.is_err(),
            "PR2/E3"
        );
        assert_eq!(
            dispatch.relay().await.expect("PR2 no delivery"),
            0,
            "PR2/E3"
        );

        let mut conflicting_batch = awaken_tool_contract::ToolBatch::for_step(
            run_id.clone(),
            0,
            [(
                awaken_runtime_contract::llm::ToolCall {
                    call_id: "primary-reply-tool-use".into(),
                    tool_id: "client_lookup".into(),
                    arguments: serde_json::json!({"query": "another request"}),
                },
                awaken_tool_contract::ToolRecoveryPolicy::default(),
            )],
        )
        .expect("PR3 internally valid conflicting batch");
        conflicting_batch
            .mark_awaiting(
                "primary-reply-tool-use",
                awaken_tool_contract::ToolWaitKind::ExternalResult,
                ticket.correlation_id.clone(),
            )
            .expect("PR3 conflicting batch awaits");
        commit
            .commit(ThreadCommit::assemble(
                parent.clone(),
                RunDisposition::awaiting(ticket.clone()),
                true,
                Vec::new(),
                vec![awaken_tool_contract::ActiveToolBatch::write(&Some(
                    conflicting_batch,
                ))],
                Vec::new(),
            ))
            .await
            .expect("PR3 commit conflicting batch projection");
        assert!(
            host.session_thread_tool_reply_fence(&command)
                .await
                .is_err(),
            "PR3/E4"
        );
        assert_eq!(
            dispatch.relay().await.expect("PR3 no delivery"),
            0,
            "PR3/E4"
        );
        commit
            .commit(ThreadCommit::assemble(
                parent.clone(),
                RunDisposition::awaiting(ticket.clone()),
                true,
                Vec::new(),
                vec![awaiting_tool_batch_state(
                    &run_id,
                    &ticket,
                    awaken_tool_contract::ToolWaitKind::ExternalResult,
                )],
                Vec::new(),
            ))
            .await
            .expect("restore exact PR1 batch fixture");

        let fence = host
            .session_thread_tool_reply_fence(&command)
            .await
            .expect("PR1 exact primary fence");
        host.reply_session_thread_tool(
            awaken_session_execution_contract::SessionThreadToolReplyDelivery {
                command,
                fence,
                session_activity_epoch: (!extension).then_some(resumed_epoch),
            },
        )
        .await
        .expect("PR1 stage primary result");
        clock.set(1);
        let resumed = dispatch
            .claim_run(
                &run_id,
                "primary-reply-owner",
                DEFAULT_LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("claim resumed primary")
            .expect("primary result makes dispatch runnable");
        assert_eq!(
            resumed.request.session_activity_epoch,
            (!extension).then_some(resumed_epoch),
            "PR1/E1"
        );
        assert_eq!(resumed.pending.len(), 1, "PR1/E1");
        match &resumed.pending[0].result {
            awaken_runtime_contract::resume::ResumeResult::ToolResult(output) => {
                assert_eq!(output.call_id, "primary-reply-tool-use", "PR1/E2");
                assert_eq!(output.text(), "client answer", "PR1/E2");
                assert!(!output.is_error, "PR1/E2");
            }
            other => panic!("PR1 expected ToolResult, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_resume_reuses_the_exact_run_and_durable_dispatch_generation() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 committed BudgetReached truth belongs to a root or
    // Session child Run; C2 a durable child row retains its prior activity; C3
    // exact deliveries race/replay; C4 the same Run later reaches another budget
    // pause; C5 a stale generation or ToolPermission ticket is presented.
    // Effects: E1 root handoff creates one existing-Run Dispatch; E2 child resume
    // atomically rotates its existing row; E3 exact replay has one pending input;
    // E4 C4 has a larger commit-derived generation and a distinct input while
    // retaining Run identity; E5 C5 is a no-op. Commit recovery and the existing
    // Dispatch row remain the only pause/delivery authorities.
    //
    // | Rule | Owner | Ticket/generation | Delivery | Effect |
    // |---|---|---|---|---|
    // | B1 | root | BudgetReached/current | fresh | E1 |
    // | B2 | child | BudgetReached/current | concurrent exact | E2+E3 |
    // | B3 | same child Run | second BudgetReached/new generation | fresh | E4 |
    // | B4 | child | old generation | stale | E5 |
    // | B5 | child | ToolPermission | budget discovery | E5 |
    use awaken_agent_contract::agent::awaiting::{
        AwaitTarget, PauseReason, PendingTool, ResumeTicket, ToolAwaitReason,
    };
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch::{DispatchOutcome, RunDispatch};
    use awaken_session_application_contract::{
        SessionBudgetResumeDelivery, SessionBudgetResumeDisposition,
    };

    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("budget resume dispatch"),
    );
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
            .await
            .with_ephemeral_dispatch(dispatch.clone()))
        .build()
        .expect("validated Host dependencies"),
    );

    let root = ThreadId("budget-resume-root".into());
    let root_run = RunId("budget-resume-root-run".into());
    let root_ctx = host.ctx_for(&root.0, None).await.expect("B1 root context");
    let snapshot_id = root_ctx.config.id.0.clone();
    let catalog_fingerprint = root_ctx.config.resolved_spec.catalog_fingerprint.0.clone();
    let budget_ticket = |thread: &ThreadId, run: &RunId| {
        ResumeTicket::new(
            run.0.clone(),
            run.clone(),
            thread.clone(),
            snapshot_id.clone(),
            catalog_fingerprint.clone(),
            AwaitTarget::Pause(PauseReason::BudgetReached),
        )
    };
    let root_commit = host.commit_for_read(&root.0).await.expect("B1 root commit");
    root_commit
        .commit(ThreadCommit::assemble(
            root.clone(),
            RunDisposition::running(root_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B1 root Running");
    root_commit
        .commit(ThreadCommit::assemble(
            root.clone(),
            RunDisposition::awaiting(budget_ticket(&root, &root_run)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B1 root budget pause");
    let root_pause = host
        .session_budget_resume_tickets(&root.0)
        .await
        .expect("B1 discover root pause")
        .into_iter()
        .next()
        .expect("B1 root ticket");
    assert_eq!(root_pause.ticket.run_id, root_run, "B1/E1");
    assert_eq!(root_pause.prior_session_activity_epoch, None, "B1/E1");
    let root_delivery = SessionBudgetResumeDelivery {
        session_id: SessionId::from(&root.0),
        thread_id: root.clone(),
        run_id: root_run.clone(),
        correlation_id: root_pause.ticket.correlation_id,
        pause_generation: root_pause.pause_generation,
        prior_session_activity_epoch: None,
        session_activity_epoch: 31,
    };
    let mut root_stale = root_delivery.clone();
    root_stale.pause_generation = root_stale.pause_generation.saturating_add(1);
    assert_eq!(
        host.resume_budget_reached(root_stale).await.unwrap(),
        SessionBudgetResumeDisposition::Stale,
        "B4/E5"
    );
    assert!(
        dispatch.list_dispatches().await.unwrap().is_empty(),
        "B4/E5"
    );
    assert_eq!(
        host.resume_budget_reached(root_delivery.clone())
            .await
            .unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B1/E1"
    );
    assert_eq!(
        host.resume_budget_reached(root_delivery).await.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B1 exact replay remains successful"
    );
    let root_rows = dispatch.list_dispatches().await.unwrap();
    assert_eq!(root_rows.len(), 1, "B1/E1 one handoff row");
    assert_eq!(root_rows[0].run_id, root_run, "B1/E1 same Run");
    assert_eq!(root_rows[0].session_activity_epoch, Some(31), "B1/E1");
    let root_pending = dispatch.list(&root).await.unwrap();
    assert_eq!(root_pending.len(), 1, "B1 exact replay has one input");
    assert_eq!(root_pending[0].input.run_id, root_run, "B1/E1");

    let parent = ThreadId("budget-resume-child-parent".into());
    let child = ThreadId("budget-resume-child".into());
    let child_run = RunId("budget-resume-child-run".into());
    host.ctx_for(&parent.0, None)
        .await
        .expect("B2 parent context");
    let child_commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("B2 parent commit partition");
    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B2 child Running");
    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(budget_ticket(&child, &child_run)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B2 child budget pause");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &child.0,
                &child_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(41),
        )
        .await
        .expect("B2 child dispatch");
    clock.set(0);
    let initial_child_claim = dispatch
        .claim_run(
            &child_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("B2 initial claim")
        .expect("B2 initial child work");
    dispatch
        .settle(
            &child_run,
            initial_child_claim.lease.epoch(),
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("B2 initial Awaiting settlement");
    let first_pause = host
        .session_budget_resume_tickets(&parent.0)
        .await
        .expect("B2 discover child pause")
        .into_iter()
        .find(|pause| pause.ticket.run_id == child_run)
        .expect("B2 child ticket");
    assert_eq!(first_pause.prior_session_activity_epoch, Some(41), "B2/E2");
    let first_delivery = SessionBudgetResumeDelivery {
        session_id: SessionId::from(&parent.0),
        thread_id: child.clone(),
        run_id: child_run.clone(),
        correlation_id: first_pause.ticket.correlation_id.clone(),
        pause_generation: first_pause.pause_generation,
        prior_session_activity_epoch: Some(41),
        session_activity_epoch: 42,
    };
    let (first, replay) = tokio::join!(
        host.resume_budget_reached(first_delivery.clone()),
        host.resume_budget_reached(first_delivery.clone()),
    );
    assert_eq!(
        first.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B2"
    );
    assert_eq!(
        replay.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B2 exact concurrent replay"
    );
    clock.set(1);
    let resumed = dispatch
        .claim_run(
            &child_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("B2 resumed claim")
        .expect("B2 resumed child");
    assert_eq!(resumed.request.session_activity_epoch, Some(42), "B2/E2");
    assert_eq!(resumed.pending.len(), 1, "B2/E3");
    assert_eq!(resumed.pending[0].run_id, child_run, "B2/E2 same Run");
    let first_message_id = resumed.pending[0].message_id.clone();

    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B3 same Run resumes");
    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(budget_ticket(&child, &child_run)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B3 same Run pauses again");
    dispatch
        .settle(
            &child_run,
            resumed.lease.epoch(),
            DispatchOutcome::Awaiting,
            std::slice::from_ref(&first_message_id),
        )
        .await
        .expect("B3 settle second pause");
    let second_pause = host
        .session_budget_resume_tickets(&parent.0)
        .await
        .expect("B3 discover second pause")
        .into_iter()
        .find(|pause| pause.ticket.run_id == child_run)
        .expect("B3 second child ticket");
    assert_eq!(second_pause.ticket.run_id, child_run, "B3/E4 same Run");
    assert_ne!(
        second_pause.pause_generation, first_pause.pause_generation,
        "B3/E4 commit-derived generations differ"
    );
    assert_eq!(
        host.resume_budget_reached(first_delivery).await.unwrap(),
        SessionBudgetResumeDisposition::Stale,
        "B4/E5 old generation"
    );
    let after_stale = dispatch
        .list_dispatches()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.run_id == child_run)
        .expect("B4 child row");
    assert_eq!(after_stale.session_activity_epoch, Some(42), "B4/E5");
    assert!(dispatch.list(&child).await.unwrap().is_empty(), "B4/E5");
    let second_delivery = SessionBudgetResumeDelivery {
        session_id: SessionId::from(&parent.0),
        thread_id: child.clone(),
        run_id: child_run.clone(),
        correlation_id: second_pause.ticket.correlation_id,
        pause_generation: second_pause.pause_generation,
        prior_session_activity_epoch: Some(42),
        session_activity_epoch: 43,
    };
    assert_eq!(
        host.resume_budget_reached(second_delivery).await.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B3/E4"
    );
    clock.set(2);
    let resumed_again = dispatch
        .claim_run(
            &child_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("B3 second resumed claim")
        .expect("B3 second resumed child");
    assert_eq!(resumed_again.pending.len(), 1, "B3/E4");
    assert_eq!(resumed_again.pending[0].run_id, child_run, "B3/E4");
    assert_ne!(
        resumed_again.pending[0].message_id, first_message_id,
        "B3/E4"
    );

    let action_parent = ThreadId("budget-action-parent".into());
    let action_child = ThreadId("budget-action-child".into());
    let action_run = RunId("budget-action-run".into());
    let action_commit = host
        .commit_for_read(&action_parent.0)
        .await
        .expect("B5 parent partition");
    action_commit
        .commit(ThreadCommit::assemble(
            action_child.clone(),
            RunDisposition::running(action_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B5 Running");
    let action_ticket = ResumeTicket::new(
        action_run.0.clone(),
        action_run.clone(),
        action_child.clone(),
        snapshot_id.clone(),
        catalog_fingerprint.clone(),
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "budget-action-call".into(),
            tool: PendingTool {
                tool_id: "write".into(),
                arguments: serde_json::json!({"path":"requires-action.txt"}),
            },
            authority: ToolCallAuthority::RuntimeToolBatch,
        },
    );
    action_commit
        .commit(ThreadCommit::assemble(
            action_child.clone(),
            RunDisposition::awaiting(action_ticket),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B5 ToolPermission pause");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &action_child.0,
                &action_run.0,
            ))
            .for_session(action_parent.clone())
            .with_session_activity_epoch(51),
        )
        .await
        .expect("B5 action dispatch");
    clock.set(3);
    let action_claim = dispatch
        .claim_run(
            &action_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("B5 action claim");
    dispatch
        .settle(
            &action_run,
            action_claim.lease.epoch(),
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("B5 action Awaiting settlement");
    assert!(
        host.session_budget_resume_tickets(&action_parent.0)
            .await
            .expect("B5 discovery")
            .is_empty(),
        "B5/E5 ToolPermission remains the higher-priority required action"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_client_result_settles_the_authoritative_dispatch() {
    async fn bounded_stage<T>(
        stage: &'static str,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        match tokio::time::timeout(std::time::Duration::from_secs(10), future).await {
            Ok(output) => output,
            Err(_) => panic!("R2/E6 durable client-result test timed out at {stage}"),
        }
    }

    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 durable ingress with a local pool; C2 a claimed Run settles Awaiting
    // on a client-tool ticket; C3 the exact ClientResult arrives; C4 resumed work
    // ends; C5 resumed work awaits on a new ticket. Effects: E1 input is appended
    // to the canonical durable Inbox; E2 the Worker alone claims/resumes/settles;
    // E3 Done removes the dispatch row; E4 Awaiting retains exactly one row; E5
    // committed result reaches the model; E6 any missing dispatch settlement
    // fails at its exact bounded stage instead of hanging the suite. Constraints:
    // direct ingress remains the R1 path above; one ticket accepts one
    // idempotency identity.
    //
    // | Rule | ingress | initial state | resume result | Effect |
    // | R1 | direct | Awaiting | Ended | inline E1/E2 not applicable (sibling test) |
    // | R2 | durable/local pool | Awaiting(old ticket) | Ended | E1+E2+E3+E5 |
    // | R3 | durable | Awaiting(old ticket) | Awaiting(new ticket) | E1+E2+E4 |
    // | R4 | durable | Awaiting(old ticket) | exact retry | one Inbox identity |
    // R3 is owned by run-ingress worker settle tests; R4 by the resume-identity
    // test plus each Inbox backend's idempotency-conflict conformance suite.
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("in-memory dispatch"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(ClientLookupModel), "stub")
        .await
        .with_client_tools(HashSet::from(["lookup".to_string()]))
        .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();

    let first = bounded_stage(
        "initial host.run settlement",
        host.run(None, "t-durable-client", user("hi")),
    )
    .await
    .expect("durable Run awaits");
    assert!(matches!(first.state, RunState::Awaiting), "R2 precondition");
    let pending = first.pending.expect("client tool ticket");
    let awaiting = bounded_stage(
        "first dispatch list after initial Awaiting",
        dispatch.list_dispatches(),
    )
    .await
    .expect("list awaiting dispatch");
    assert_eq!(awaiting.len(), 1, "one durable dispatch owns the wait");
    assert_eq!(awaiting[0].run_id, first.run_id);
    assert_eq!(
        awaiting[0].state,
        awaken_dispatch_contract::DispatchState::Awaiting
    );

    let resumed = bounded_stage(
        "host.resume client-result settlement",
        host.resume(
            "t-durable-client",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        ),
    )
    .await
    .expect("durable worker resumes the client result");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R2/E5");
    let settled_dispatches = bounded_stage(
        "final dispatch list after terminal settlement",
        dispatch.list_dispatches(),
    )
    .await
    .expect("list settled dispatches");
    assert!(
        settled_dispatches
            .iter()
            .all(|summary| summary.run_id != resumed.run_id),
        "R2/E3: terminal settlement removes the authoritative dispatch row"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn coordinator_only_foreground_admission_survives_request_drop_without_a_pool() {
    // Causes: C1 durable dispatch is installed; C2 this Coordinator owns no
    // local pool; C3 the foreground request is dropped before a remote Worker
    // claims. Effects: E1 one row is committed to the same DispatchStore;
    // E2 no local physical attempt starts; E3 dropping the waiter does not
    // retract that row. The sibling durable-client test covers local pool
    // claim, resume and settlement through the same store.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | Q1 | yes | no  | no  | local claim and settle (sibling) |
    // | Q2 | yes | yes | yes | E1+E2+E3 |
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("in-memory dispatch"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(ClientLookupModel), "stub")
        .await
        .with_client_tools(HashSet::from(["lookup".to_string()]))
        .with_ephemeral_coordinator_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();
    assert!(host.dispatch.dispatch_pool.get().is_none(), "Q2/E2");

    let request = host.clone();
    let foreground =
        tokio::spawn(async move { request.run(None, "remote-dispatch", user("hi")).await });
    let row = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(row) = dispatch
                .list_dispatches()
                .await
                .expect("dispatch rows")
                .into_iter()
                .next()
            {
                break row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Q2/E1 bounded admission");
    assert_eq!(row.thread_id, ThreadId("remote-dispatch".into()), "Q2/E1");
    assert!(!row.physical_attempt_active, "Q2/E2");

    foreground.abort();
    assert!(
        matches!(foreground.await, Err(error) if error.is_cancelled()),
        "Q2/E3",
    );
    assert_eq!(
        dispatch
            .list_dispatches()
            .await
            .expect("retained dispatch")
            .len(),
        1,
        "Q2/E3",
    );
}

#[tokio::test]
async fn pending_client_tool_query_uses_committed_ticket_during_projection_gap() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1=the Runtime has atomically committed a client-tool
    // call and its Awaiting ticket; C2=the foreground protocol has not yet
    // published its process-local step projection; C3=a peer protocol queries the
    // pending tool; C4=a fresh user Run races that wait; C5=the exact client
    // result arrives. E1=the exact committed call is returned as client-executed;
    // E2=no pending tool is fabricated when committed truth has no open wait;
    // E3=the fresh Run is rejected; E4=the original Run resumes and ends.
    // Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | R1   | T  | T  | T  | F  | F  | E1     |
    // | R2   | T  | T  | F  | T  | F  | E3     |
    // | R3   | T  | T  | F  | F  | T  | E4     |
    // | R4   | F  | T/F| T  | F  | F  | E2 (resume_with_no_awaiting_run) |
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(ClientLookupModel), "stub")
            .await
            .with_client_tools(HashSet::from(["lookup".to_string()])))
        .build()
        .expect("validated Host dependencies"),
    );
    let first = host
        .run(None, "t-cross-protocol-pending", user("hi"))
        .await
        .expect("client tool awaits");
    let expected = first.pending.expect("run exposes the pending client tool");

    // The query has no mutable process-local awaiting position to corrupt or
    // synchronize; committed Thread truth is its only input.
    let observed = host
        .pending_tool("t-cross-protocol-pending")
        .await
        .expect("committed ticket read succeeds")
        .expect("committed ticket remains queryable");
    assert_eq!(observed.tool_use_id, expected.tool_use_id);
    assert_eq!(observed.name, "lookup");
    assert_eq!(observed.input, serde_json::json!({ "q": "weather" }));
    assert!(observed.client_executed);
    assert!(host.is_awaiting("t-cross-protocol-pending").await);

    let error = match host
        .run(
            None,
            "t-cross-protocol-pending",
            user("do not overtake the wait"),
        )
        .await
    {
        Ok(_) => panic!("committed wait must reject a competing user Run"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::BadRequest);

    let resumed = host
        .resume(
            "t-cross-protocol-pending",
            &observed.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("committed wait resumes without the disposable position");
    assert!(matches!(resumed.state, RunState::Ended(_)));
    assert!(!host.is_awaiting("t-cross-protocol-pending").await);
}

/// Superseding a run requires durable ingress; a default (direct-ingress) host
/// must fail closed rather than silently behave like a plain run.
#[tokio::test]
async fn supersede_run_without_durable_ingress_fails_closed() {
    // Test design. Causes: C1 supersede is requested while no durable ingress
    // authority is configured. Effects: E1 the command fails without cancelling
    // or starting any Run. Constraint/Invariant: supersession is a durable queue
    // mutation and has no in-memory fallback. Decision rule: execute C1 and
    // require fail-closed zero side effects.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel)).await;
    // Await first so the supersede path is not short-circuited by the awaiting guard
    // (supersede is allowed on an awaiting thread; the durable check is what must fire).
    let r1 = host.run(None, "t-sup", user("hi")).await.expect("Run 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .supersede_run(None, "t-sup", user("newest wins"))
        .await
        .err()
        .expect("supersede without durable ingress must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("durable ingress"),
        "message names the requirement: {}",
        err.message
    );
}

#[tokio::test]
async fn managed_session_root_rejects_legacy_host_run_ingress() {
    // Cause/effect graph: C1 the Thread is marked as a Managed Session root;
    // C2 a caller selects ordinary or superseding Host Run ingress. Effects:
    // E1 both commands fail as bad requests before context realization; E2 the
    // dispatch authority remains empty. Decision rules: M1 C1+ordinary=>E1+E2;
    // M2 C1+supersede=>E1+E2. Constraint: Session Run reservation and its
    // activity receipt remain the sole Managed-root ingress; no Host fallback.
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("managed ingress fence dispatch"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    host.installed_sessions
        .update_run("managed-root", |run| run.session_dispatch = true);

    for result in [
        host.run(None, "managed-root", user("ordinary bypass"))
            .await,
        host.supersede_run(None, "managed-root", user("supersede bypass"))
            .await,
    ] {
        let error = match result {
            Ok(_) => panic!("Managed root must reject legacy Host ingress"),
            Err(error) => error,
        };
        assert_eq!(error.kind, HostErrorKind::BadRequest, "E1");
        assert!(error.message.contains("Session-owned Run ingress"), "E1");
    }
    assert!(
        dispatch
            .list_dispatches()
            .await
            .expect("E2 inspect dispatch authority")
            .is_empty(),
        "E2"
    );
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchReads as _, DispatchSettlement as _,
    InboxRead as _, SessionContinuationOutbox as _,
};
