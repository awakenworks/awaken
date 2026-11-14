// The original permission model and one boundary scenario, shared by the
// Memory/SQLite and real shared-PostgreSQL qualification cases.
use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;
use awaken_dispatch_erasure::DispatchBinding;

#[cfg(test)]
#[derive(Default)]
struct PermissionModel {
    calls: AtomicUsize,
}

#[cfg(test)]
#[async_trait::async_trait]
impl LlmExecutor for PermissionModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if request.messages.iter().any(|m| m.role == Role::Tool) {
            AssistantOutput::text("child done")
        } else {
            AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                call_id: "child-write".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({"path":"child.txt","content":"from child"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: Some(awaken_agent_contract::audit::model_request::TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 5,
                ..Default::default()
            }),
            stop_reason: None,
        })
    }
}

#[cfg(test)]
async fn permission_child_round_trip<F, Fut>(
    clock: Arc<dyn awaken_time::WallClock>,
    commit: Arc<awaken_session_run_delivery::SessionCommit>,
    parent_reader: Arc<dyn CommittedThreadView>,
    make_scheduler: F,
) -> awaken_agent_contract::thread::usage::ThreadUsage
where
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = RunScheduler>,
{
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&crate::sandbox_layout::agent_run_sandbox_spec("parent"))
        .await
        .unwrap();
    let model = Arc::new(PermissionModel::default());
    let context = || {
        RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(parent_reader.clone())
    };
    let child_run_id = RunId("child-run-1".into());
    let child_thread = ThreadId(child_run_id.0.clone());
    let origin = DelegationOrigin::root_for_agent(
        RunId("parent-run-1".into()),
        "delegate-call-1",
        "parent-agent",
    );
    let no_delegates = HashSet::new();
    let permission_toolsets = vec![crate::config::test_agent_toolset_permission(
        "write",
        awaken_agent_contract::ToolPermissionRequirement::AlwaysAsk,
    )];
    let execution = |scheduler: RunScheduler| AgentExecution {
        agent_id: "worker",
        model_ref: "default",
        delegates: &no_delegates,
        run_delegation: None,
        context: Some(context()),
        scheduler: Some(scheduler),
        toolsets: &permission_toolsets,
    };
    let request = |seed, resume| ChildRunRequest {
        run_id: child_run_id.clone(),
        origin: origin.clone(),
        seed,
        resume,
        parent_thread_id: ThreadId("parent-thread".into()),
    };
    let scheduler = make_scheduler("replacement-worker").await;
    assert!(matches!(
        awaken_dispatch_execution::DispatchWorker::from_binding(
            Arc::new(awaken_runtime::Runtime::new()), scheduler.binding.clone(),
            scheduler.commit.clone(), scheduler.reader.clone(), "invalid-deployed-worker",
        ),
        Err(error) if error.to_string().contains("requires a committed recovery source")
    ));
    let first = run_agent_until_boundary(
        model.clone(),
        clock.clone(),
        execution(scheduler),
        AgentRunSandbox::SharedLocal(&sandbox),
        request(Some(vec![user("write the file")].into()), None),
    )
    .await
    .unwrap();
    assert!(matches!(first, AgentRunBoundary::Awaiting));
    let (ticket_run, ticket) = commit
        .open_wait_for_thread(&child_thread)
        .await
        .unwrap()
        .expect("child ticket");
    assert_eq!(
        ticket_run, child_run_id,
        "the same exact child owns the durable ticket"
    );
    assert_eq!(
        ticket.reason(),
        awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission
    );
    assert_eq!(ticket.call_id(), Some("child-write"));
    assert_eq!(ticket.delegation_origin.as_ref(), Some(&origin));

    assert!(
        parent_reader.run_state(&child_run_id).is_none(),
        "parent prefix cannot own child state"
    );
    assert!(
        parent_reader.resume_ticket(&child_run_id).is_none(),
        "parent prefix cannot supply child permission"
    );

    // Rebuild the Worker as the original replacement-process fixture did. The
    // SQLite factory reopens its queue; both backends retain the same authority.
    // Only committed truth, not a child Worker/cache, crosses this boundary.
    let replacement = make_scheduler("replacement-worker-2").await;
    let recovered = run_agent_until_boundary(
        model.clone(),
        clock.clone(),
        execution(replacement.clone()),
        AgentRunSandbox::SharedLocal(&sandbox),
        request(Some(vec![user("write the file")].into()), None),
    )
    .await
    .expect("duplicate start reconnects to the awaiting child");
    assert!(matches!(recovered, AgentRunBoundary::Awaiting));
    assert_eq!(
        commit
            .authoritative_committed_messages(&child_thread)
            .await
            .unwrap()
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        1,
        "recovery start does not duplicate the child seed"
    );

    let second = run_agent_until_boundary(
        model.clone(),
        clock.clone(),
        execution(replacement),
        AgentRunSandbox::SharedLocal(&sandbox),
        request(None, Some(ResumeResult::allow())),
    )
    .await
    .unwrap();
    let usage = match second {
        AgentRunBoundary::Ended { text, usage } => {
            assert_eq!(text, "child done");
            assert!(
                !usage.is_empty(),
                "terminal usage comes from the child's committed state"
            );
            usage
        }
        AgentRunBoundary::Awaiting => panic!("approved child reaches its terminal boundary"),
    };
    let before_messages = commit
        .authoritative_committed_messages(&child_thread)
        .await
        .unwrap();
    let before_state = commit.committed_state_view(&child_thread).await.unwrap();
    let before_calls = model.calls.load(Ordering::SeqCst);
    let replay = run_agent_until_boundary(
        model.clone(),
        clock,
        execution(make_scheduler("replacement-worker-3").await),
        AgentRunSandbox::SharedLocal(&sandbox),
        request(None, Some(ResumeResult::allow())),
    )
    .await
    .expect("terminal child replay is idempotent");
    match replay {
        AgentRunBoundary::Ended {
            text,
            usage: replay_usage,
        } => {
            assert_eq!(text, "child done");
            assert_eq!(
                replay_usage, usage,
                "terminal replay retains exact committed usage"
            );
        }
        AgentRunBoundary::Awaiting => panic!("terminal replay cannot reopen permission"),
    }
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        before_calls,
        "terminal replay does not enter the model"
    );
    assert_eq!(
        commit
            .authoritative_committed_messages(&child_thread)
            .await
            .unwrap(),
        before_messages,
        "terminal replay appends no message"
    );
    assert_eq!(
        commit.committed_state_view(&child_thread).await.unwrap(),
        before_state,
        "terminal replay appends no state"
    );
    assert_eq!(
        before_messages
            .iter()
            .filter(|message| message.text_content() == "child done")
            .count(),
        1,
        "replacement resume does not duplicate the child result"
    );
    let committed = commit
        .recovery_snapshot(&child_thread, &child_run_id)
        .await
        .unwrap();
    assert_eq!(
        committed.run_state(&child_run_id),
        Some(RunState::Ended(EndCause::NaturalEnd))
    );
    assert_eq!(
        awaken_agent_contract::thread::usage::ThreadUsage::from_committed_state(&committed.state),
        usage,
        "child committed prefix owns final usage"
    );
    usage
}

#[tokio::test]
async fn durable_child_permission_recovers_and_duplicate_resume_is_idempotent() {
    // Original cause/effect rules: explicit write=always_ask + Session authority
    // + parent prefix without child + new/Awaiting/resume/terminal. R1 new =>
    // Awaiting; R2 duplicate => one seed; R3 exact resume => one result; R4 terminal
    // replay => same text/usage, no model or append. This SAME model/round-trip
    // runs against PG below, without copying the permission scenario. The SQLite
    // queue reopens between boundaries; ordinary stack and owned task stay exact.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp
        .path()
        .join("child-dispatch.db")
        .to_string_lossy()
        .to_string();
    let clock = awaken_time::system_clock();
    let memory = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let local = Arc::new(crate::LocalCommitAdapter::projected_shared(memory.clone()));
    let commit = Arc::new(awaken_session_run_delivery::SessionCommit::Local(
        local.clone(),
    ));
    let parent_reader: Arc<dyn CommittedThreadView> =
        Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let make_scheduler = |owner: &str| {
        let owner = owner.to_owned();
        let clock = clock.clone();
        let path = path.clone();
        let commit = commit.clone();
        let local = local.clone();
        let reader = memory.clone();
        async move {
            let dispatch = Arc::new(
                awaken_dispatch::open_sqlite_dispatch_store(&path, clock.clone())
                    .await
                    .expect("reopen original child dispatch"),
            );
            let authority = Arc::new(
                awaken_dispatch_erasure::ReferenceAuthority::with_dispatch(clock.clone(), dispatch)
                    .with_commit("parent-thread", local),
            );
            RunScheduler {
                // None keeps R1-R4 on the original cold committed-read authority.
                recovery_history: None,
                clock,
                binding: DispatchBinding::coordinator(authority),
                commit,
                reader,
                owner,
                session_resources: None,
                publication_source: None,
                claimed_model_request_gate: None,
            }
        }
    };
    permission_child_round_trip(clock.clone(), commit.clone(), parent_reader, make_scheduler).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicitly admitted PostgreSQL"]
async fn local_postgres_child_permission_recovers_with_its_own_prefix() {
    // P1 the public Coordinator composition selects one process PG pool capped
    // at one for Dispatch + global claimed commits. Fresh child Workers keep
    // main's required per-attempt RecoveryProjection; no parent prefix supplies
    // a child ticket/state and no nested second checkout can make progress.
    // P2 the SAME R1-R4 permission fixture proves one seed/result, exact resumed
    // text/usage, no model/append on terminal replay. P3 read through the original
    // physical parent Session after execution => child SQL truth, while the
    // parent's synchronous prefix remains empty for that child. No paired
    // writer/cache advancement, extra pool, retry or new production inlet.
    let url = awaken_reliability_testkit::postgres_fixture_url()
        .expect("the live child case requires an admitted PostgreSQL fixture");
    let clock = awaken_time::system_clock();
    let storage = awaken_deployment_config::CoordinatorStorage {
        storage_dir: None,
        store: awaken_deployment_config::StoreKind::Postgres,
        dispatch_backend: awaken_deployment_config::DispatchBackend::Postgres,
        wake: awaken_deployment_config::Wake::None,
        wake_channel: "awaken_dispatch_wake".into(),
        nats_url: None,
        database_url: Some(url),
        postgres_max_connections: std::num::NonZeroU32::new(1).unwrap(),
        postgres_min_connections: 0,
    };
    let persistence = awaken_coordinator::open_coordinator_persistence(&storage, clock.clone())
        .await
        .unwrap();
    let pool = persistence.postgres_pool.as_ref().unwrap();
    let authority = persistence.runtime_authority.clone();
    let parent_read = authority.open_commit("parent-thread").await.unwrap();
    let parent_reader: Arc<dyn CommittedThreadView> = parent_read.clone();
    let commit = Arc::new(awaken_session_run_delivery::SessionCommit::Local(
        parent_read.clone(),
    ));
    let make_scheduler = |owner: &str| {
        let owner = owner.to_owned();
        let clock = clock.clone();
        let authority = authority.clone();
        let commit = commit.clone();
        let reader = parent_reader.clone();
        async move {
            RunScheduler {
                // None keeps R1-R4 on the original cold committed-read authority.
                recovery_history: None,
                clock,
                binding: DispatchBinding::coordinator(authority),
                commit,
                reader,
                owner,
                session_resources: None,
                publication_source: None,
                claimed_model_request_gate: None,
            }
        }
    };
    let usage = permission_child_round_trip(
        clock.clone(),
        commit.clone(),
        parent_reader.clone(),
        make_scheduler,
    )
    .await;
    let run = RunId("child-run-1".into());
    assert_eq!(
        parent_read.run_state(&run),
        None,
        "P3 parent prefix cannot own child state"
    );
    assert!(
        parent_read.resume_ticket(&run).is_none(),
        "P3 parent prefix cannot supply child permission"
    );
    let child = commit
        .recovery_snapshot(&ThreadId(run.0.clone()), &run)
        .await
        .unwrap();
    assert_eq!(
        child.run_state(&run),
        Some(RunState::Ended(EndCause::NaturalEnd))
    );
    assert_eq!(
        awaken_agent_contract::thread::usage::ThreadUsage::from_committed_state(&child.state),
        usage,
        "P3 durable child usage through original physical Session"
    );
    assert_eq!(
        pool.options().get_max_connections(),
        1,
        "P1 unchanged original cap"
    );
    pool.close().await;
}

#[cfg(test)]
#[tokio::test]
async fn child_boundary_read_failure_is_not_absence_or_success() {
    // R1 the original asynchronous observation fails before any boundary =>
    // preserve that runtime error; never poll it as None or report a result.
    let error =
        await_committed_child_boundary(&RunId("child-read-failure".into()), None, || async {
            Err(child_read_error("original committed read is unavailable"))
        })
        .await
        .expect_err("failed read is not unknown absence");
    assert!(matches!(error, crate::agent_runner::AgentRunError::Runtime(
        awaken_runtime_contract::execution::Error::Execution(ref message))
        if message == "original committed read is unavailable"));
}

#[cfg(test)]
#[test]
fn child_resume_keeps_the_answered_ticket_when_a_later_wait_is_observed() {
    // R1 an answer has frozen exact Run+Thread+correlation in ResumeCommand;
    // R2 a later Awaiting ticket for the SAME child has another correlation.
    // C3 the command carries nonempty accepted context and a denial.
    // R1+R2+C3 => enqueue its exact IDs/context/result. The existing Worker will fence
    // that old correlation if R2 is now current; approval cannot retarget it.
    let run = RunId("child-frozen-ticket".into());
    let thread = ThreadId("answered-child-thread".into());
    let ticket = ResumeTicket::new(
        "answered-wait",
        run.clone(),
        thread.clone(),
        "snapshot",
        "fingerprint",
        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
            reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
            call_id: "answered-call".into(),
            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                tool_id: "write".into(),
                arguments: serde_json::json!({}),
            },
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        },
    );
    let result = ResumeResult::deny(Some("original decision".into()));
    let context = vec![Message::text(
        awaken_agent_contract::agent::message::Id("frozen-context".into()),
        Role::System,
        "original accepted context",
    )];
    let frozen =
        awaken_runtime_contract::resume::ResumeCommand::from_ticket(&ticket, result.clone(), 0)
            .with_context_messages(context.clone());
    let mut latest = ticket.clone();
    latest.correlation_id = "later-wait".into();
    let input = child_resume_input(frozen);
    assert_eq!(input.run_id, run);
    assert_eq!(input.thread_id, thread);
    assert_eq!(
        input.context_messages, context,
        "accepted context is not discarded"
    );
    assert_eq!(input.result, result, "answer A is not rebuilt from wait B");
    assert_eq!(input.correlation_id, ticket.correlation_id);
    assert_ne!(input.correlation_id, latest.correlation_id);
    assert_eq!(
        input.message_id,
        "child-resume:child-frozen-ticket:answered-wait"
    );
}

#[tokio::test]
async fn child_async_read_cancellation_preserves_ready_truth_and_original_deadline() {
    // Decision table: C1 the authoritative read is Ready/Pending; C2 parent
    // cancellation is ready/absent; C3 Ready contains terminal truth/read Err.
    // R1 Pending+cancel => existing cancellation error immediately, no extra
    // timer. R2 terminal+cancel => original committed natural end wins.
    // R3 read Err+cancel => retain the original typed read failure, not success
    // or cancellation. R4 Pending+no cancel => the ORIGINAL 60s enclosing
    // deadline, not an unbounded I/O wait. Paused Tokio time proves causality
    // without sleeping, retries, or increasing either production budget.
    tokio::time::pause();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let child = RunId("pending-child-read".into());
    let began = tokio::time::Instant::now();
    let error = await_committed_child_boundary(&child, Some(&cancellation), || {
        std::future::pending::<Result<Option<RunState>, crate::agent_runner::AgentRunError>>()
    })
    .await
    .expect_err("R1 pending read cannot hide cancellation");
    assert!(
        error.to_string().contains("was cancelled"),
        "R1 original cancellation: {error}"
    );
    assert!(began.elapsed().is_zero(), "R1 no deadline advance needed");
    let terminal = RunState::Ended(EndCause::NaturalEnd);
    let observed = await_committed_child_boundary(&child, Some(&cancellation), || {
        std::future::ready(Ok(Some(terminal.clone())))
    })
    .await
    .expect("R2 ready committed truth wins");
    assert_eq!(observed, terminal);
    let error = await_committed_child_boundary(&child, Some(&cancellation), || async {
        Err(crate::agent_runner::AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(
                "original authoritative read failure".into(),
            ),
        ))
    })
    .await
    .expect_err("R3 ready read failure remains an error");
    assert!(
        matches!(error, crate::agent_runner::AgentRunError::Runtime(
        awaken_runtime_contract::execution::Error::Execution(ref detail)
    ) if detail == "original authoritative read failure"),
        "R3 original typed failure: {error}"
    );
    let began = tokio::time::Instant::now();
    // Tokio's timer wheel quantizes both 60s deadlines to the same tick. Use
    // an independent literal contract deadline on this SAME paused clock;
    // exact elapsed==60s would reject that legal quantization. No slack is
    // added: changing the helper to 59s or 61s yields distinct finish instants.
    let ((error, helper_finished), reference_finished) = tokio::join!(
        async {
            let error =
                await_committed_child_boundary(&child, None, || {
                    std::future::pending::<
                        Result<Option<RunState>, crate::agent_runner::AgentRunError>,
                    >()
                })
                .await
                .expect_err("R4 original enclosing deadline remains bounded");
            (error, tokio::time::Instant::now())
        },
        async {
            tokio::time::sleep_until(began + std::time::Duration::from_secs(60)).await;
            tokio::time::Instant::now()
        },
    );
    assert!(
        error
            .to_string()
            .contains("did not reach a committed boundary"),
        "R4 original deadline: {error}"
    );
    assert_eq!(
        helper_finished, reference_finished,
        "R4 unchanged exact 60s contract"
    );
}
