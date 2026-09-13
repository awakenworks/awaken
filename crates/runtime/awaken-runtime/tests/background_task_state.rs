//! BackgroundTask integration at the Runtime commit boundary.
//!
//! The extension has no service/repository seam. These tests therefore exercise
//! the production agent loop and inspect the ordinary committed Thread state.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Scope, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_background_task::{
    BACKGROUND_TASK_PLUGIN_ID, BACKGROUND_TASK_STATE_PREFIX, BackgroundInvocation, BackgroundTask,
    BackgroundTaskConfig, BackgroundTaskEnd, BackgroundTaskId, BackgroundTaskLifecycle,
    BackgroundTaskOrigin, BackgroundTaskPlugin, TaskClaim, TaskExecutionPolicy, task_state_cell,
};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolConcurrency, ToolOutput, ToolRecoveryCapability, ToolRecoveryMode,
    ToolRecoveryPolicy, ToolResource, ToolResourceAccess,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

struct Bash;

#[async_trait]
impl RawTool for Bash {
    fn id(&self) -> &str {
        "bash"
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::ReplaySafe
    }

    fn concurrency(&self, _arguments: &serde_json::Value) -> ToolConcurrency {
        ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
            "sandbox", "workdir",
        ))])
    }

    async fn invoke(
        &self,
        call: ToolCall,
    ) -> Result<ToolOutput, awaken_runtime_contract::tool::ToolError> {
        Ok(ToolOutput::ok(call.call_id, "unused by submission test"))
    }
}

struct SubmitThenEnd {
    calls: AtomicUsize,
}

struct ReadDirectNotification;

#[async_trait]
impl LlmExecutor for ReadDirectNotification {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let notifications = request.messages.iter().filter(|message| {
            message.content.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("connected-phone-fixture")))
        }).collect::<Vec<_>>();
        assert_eq!(
            notifications.len(),
            1,
            "N1 result must reach actual inference once"
        );
        assert_eq!(
            notifications[0].role,
            Role::User,
            "N1 result is source data"
        );
        Ok(ChatResponse {
            output: AssistantOutput::text("您好，我是知客。请问您想了解什么？"),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn terminal_notification_reaches_inference_and_commits_without_result_lookup() {
    // N1 ordinary submission commit + exact-fence completion -> StepStart
    // folds state and supplies actual result before inference, with zero get
    // tool calls. N2 subsequent Run + durable delivery receipt -> only the
    // original transcript message is visible, no duplicate. This exercises
    // run_phase_hooks and StepLedger rather than merely inspecting HookReaction.
    let supervisor = Arc::new(awaken_ext_background_task::BackgroundTaskSupervisor::new(
        "notification-worker",
    ));
    let plugin = Arc::new(BackgroundTaskPlugin::with_supervisor(
        BackgroundTaskConfig {
            tools: BTreeSet::from(["bash".into()]),
            ..Default::default()
        },
        supervisor.clone(),
    ));
    let commits = Arc::new(MemoryCommitCoordinator::new());
    let runtime = Runtime::new()
        .with_llm(Arc::new(SubmitThenEnd {
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(plugin.clone())
        .with_tool(Arc::new(Bash));
    runtime
        .execute(
            activation(vec![BACKGROUND_TASK_PLUGIN_ID.into()]),
            RuntimeRunContext::new().with_commit(commits.clone()),
        )
        .await
        .unwrap();
    let tasks =
        awaken_ext_background_task::tasks_from_state(&Store::rebuild(&commits.committed().state))
            .unwrap();
    let task = &tasks[0];
    let fence = task.attempt().unwrap().fence();
    assert!(supervisor.register(&task.id, &fence).is_some());
    supervisor.complete(
        task.id.clone(),
        awaken_ext_background_task::BackgroundTaskCompletion {
            fence,
            end: BackgroundTaskEnd::Completed {
                content: vec![ContentBlock::text("connected-phone-fixture")],
                is_error: false,
            },
        },
    );
    let runtime = Runtime::new()
        .with_llm(Arc::new(ReadDirectNotification))
        .with_plugin(plugin)
        .with_tool(Arc::new(Bash));
    for id in ["notification-run", "after-notification-run"] {
        let mut next = activation(vec![BACKGROUND_TASK_PLUGIN_ID.into()]);
        next.run_id = RunId(id.into());
        next.input = vec![Message::text(
            MessageId(format!("input-{id}")),
            Role::System,
            "Continue from received events.",
        )];
        assert_eq!(
            runtime
                .execute(
                    next,
                    RuntimeRunContext::new()
                        .with_commit(commits.clone())
                        .with_reader(commits.clone())
                )
                .await
                .unwrap(),
            RunState::Ended(EndCause::NaturalEnd)
        );
    }
    let committed = commits.committed();
    assert_eq!(
        committed
            .messages
            .iter()
            .filter(|message| message.id.0.starts_with("background-result:"))
            .count(),
        1,
        "N2 committed exactly once"
    );
}

#[async_trait]
impl LlmExecutor for SubmitThenEnd {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            let ids = request
                .tools
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<BTreeSet<_>>();
            assert!(ids.contains("run_in_background"), "C1/E1");
            assert!(ids.contains("list_background_tasks"), "C1/E1");
        }
        Ok(ChatResponse {
            output: if first {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "submit-call".into(),
                    tool_id: "run_in_background".into(),
                    arguments: serde_json::json!({
                        "tool": "bash",
                        "arguments": {"command": "sleep 1"}
                    }),
                }])
            } else {
                AssistantOutput::text("done")
            },
            usage: None,
            stop_reason: None,
        })
    }
}

fn activation(plugin_ids: Vec<String>) -> RunActivation {
    let fingerprint = CatalogFingerprint("background-catalog".into());
    RunActivation {
        run_id: RunId("run-1".into()),
        thread_id: ThreadId("thread-1".into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: Some(4),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "backend"),
                ),
                tool_descriptors: vec![
                    ToolDescriptor::pinned(
                        "test",
                        "bash",
                        "Run a command.",
                        serde_json::json!({"type":"object"}),
                    )
                    .with_recovery(ToolRecoveryPolicy::replay_safe()),
                ],
                plugin_ids,
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("input-1".into()),
            role: Role::User,
            content: vec![ContentBlock::text("run it")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

#[tokio::test]
async fn runtime_commits_submission_through_the_ordinary_thread_state_boundary() {
    // Causal graph:
    // C1 configured plugin -> four tools are projected;
    // C2 model submits -> RawTool returns one typed State command;
    // C3 Runtime completes its normal ToolBatch/ThreadCommit -> the task is
    // replayable from committed Thread state. There is no background service,
    // repository mock, database handle, or Runtime special operation in the graph.
    let plugin = Arc::new(BackgroundTaskPlugin::new(BackgroundTaskConfig {
        tools: BTreeSet::from(["bash".into()]),
        ..Default::default()
    }));
    let runtime = Runtime::new()
        .with_llm(Arc::new(SubmitThenEnd {
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(plugin)
        .with_tool(Arc::new(Bash));
    let commits = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(
            activation(vec![BACKGROUND_TASK_PLUGIN_ID.into()]),
            RuntimeRunContext::new().with_commit(commits.clone()),
        )
        .await
        .expect("the state-only plugin must run through the ordinary loop");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "C3/E1");

    let committed = commits.committed();
    let state = Store::rebuild(&committed.state);
    let tasks = state
        .scan_prefix(Scope::Thread, BACKGROUND_TASK_STATE_PREFIX)
        .map(|(_, value)| {
            serde_json::from_value::<BackgroundTask>(value.clone())
                .expect("committed task state must retain its typed shape")
        })
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 1, "C2/E1: exactly one durable submission");
    assert!(matches!(
        tasks[0].lifecycle,
        BackgroundTaskLifecycle::Running { .. }
    ));
    assert_eq!(tasks[0].origin.thread_id, ThreadId("thread-1".into()));
    let attempt = tasks[0]
        .attempt()
        .expect("submission freezes execution facts");
    assert_eq!(attempt.policy.recovery, ToolRecoveryPolicy::replay_safe());
    assert_eq!(
        attempt.policy.concurrency,
        ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
            "sandbox", "workdir"
        ))])
    );
}

#[tokio::test]
async fn terminal_reclaim_rejects_then_recovers_from_committed_thread_truth() {
    // Cause/effect decision table:
    // C1 an expired local attempt is NeverReplay or replayable with epoch equal
    // to its maximum; C2 its complete terminal State command is rejected by a
    // real read-only SQLite transaction or accepted under a fresh Run; C3 the
    // process-owned store is dropped and reopened after either outcome.
    // R1 NeverReplay -> Indeterminate with no replacement attempt; R2 exhausted
    // replay -> Failed with no replacement attempt; R3 R1/R2 + rejection + C3
    // -> only the prior Running aggregate is recovered; R4 R3 + recompute +
    // accepted fresh-Run commit + C3 -> the exact terminal aggregate is
    // recovered. Rejection never publishes a partial terminal value and retry
    // never replays the uncertain tool effect. SQLite/serde/async correctness
    // remain adapter evidence, not part of the scalar Kani proof.
    for (case, mode, maximum, expected_claim, indeterminate) in [
        (
            "never-replay",
            ToolRecoveryMode::NeverReplay,
            2,
            TaskClaim::EndedIndeterminate,
            true,
        ),
        (
            "attempts-exhausted",
            ToolRecoveryMode::ReplaySafe,
            1,
            TaskClaim::EndedAttemptsExhausted,
            false,
        ),
    ] {
        let directory = tempfile::tempdir().expect("temporary SQLite directory");
        let database = directory.path().join(format!("{case}.db"));
        let database = database.to_str().expect("temporary path is UTF-8");
        let thread_id = ThreadId(format!("thread-{case}"));
        let origin_run = RunId(format!("origin-{case}"));
        let retry_run = RunId(format!("retry-{case}"));
        let mut running = BackgroundTask::requested(
            BackgroundTaskId::new(format!("task-{case}")).expect("valid task id"),
            BackgroundTaskOrigin {
                thread_id: thread_id.clone(),
                run_id: origin_run.clone(),
                operation_id: format!("operation-{case}"),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: format!("call-{case}"),
                    tool_id: "bash".into(),
                    arguments: serde_json::json!({"command":"do not replay"}),
                },
            },
        );
        running
            .start(
                "worker-old",
                0,
                1,
                TaskExecutionPolicy {
                    recovery: ToolRecoveryPolicy::try_new(mode, maximum)
                        .expect("positive attempt budget"),
                    concurrency: ToolConcurrency::Parallel,
                },
            )
            .expect("requested task starts");

        let store = SqliteCommitCoordinator::open(database).expect("open SQLite commit store");
        store
            .commit(ThreadCommit {
                thread_id: thread_id.clone(),
                run: RunDisposition::ended(origin_run, EndCause::NaturalEnd),
                messages: Vec::new(),
                state: vec![
                    task_state_cell(&running.id)
                        .write(&running)
                        .expect("serialize running aggregate"),
                ],
                events: Vec::new(),
            })
            .await
            .expect("initial Running aggregate commits");

        let mut terminal = running.clone();
        assert_eq!(
            terminal.reclaim("worker-new", 1, 1),
            Ok(expected_claim.clone()),
            "{case}: R1/R2"
        );
        assert!(
            terminal.attempt().is_none(),
            "{case}: no replacement attempt"
        );
        assert!(
            matches!(
                (&terminal.lifecycle, indeterminate),
                (
                    BackgroundTaskLifecycle::Ended {
                        end: BackgroundTaskEnd::Indeterminate { .. }
                    },
                    true
                ) | (
                    BackgroundTaskLifecycle::Ended {
                        end: BackgroundTaskEnd::Failed { .. }
                    },
                    false
                )
            ),
            "{case}: exact terminal class"
        );

        store
            .make_read_only_for_test()
            .expect("enable deterministic SQLite rejection");
        assert!(
            store
                .commit(ThreadCommit {
                    thread_id: thread_id.clone(),
                    run: RunDisposition::ended(retry_run.clone(), EndCause::NaturalEnd),
                    messages: Vec::new(),
                    state: vec![
                        task_state_cell(&terminal.id)
                            .write(&terminal)
                            .expect("serialize terminal aggregate")
                    ],
                    events: Vec::new(),
                })
                .await
                .is_err(),
            "{case}: R3 rejects the complete terminal commit"
        );
        drop(store);

        let store = SqliteCommitCoordinator::open(database).expect("reopen after rejection");
        let recovered_state =
            Store::rebuild(&CommittedThreadView::committed_state(&store, &thread_id));
        let mut recovered = task_state_cell(&running.id)
            .load(&recovered_state)
            .expect("decode committed aggregate")
            .expect("prior aggregate remains present");
        assert_eq!(recovered, running, "{case}: R3 rejection stutters");
        assert_eq!(
            recovered.reclaim("worker-new", 1, 1),
            Ok(expected_claim),
            "{case}: R4 recomputes the same terminal decision"
        );
        assert_eq!(recovered, terminal, "{case}: R4 exact terminal aggregate");
        store
            .commit(ThreadCommit {
                thread_id: thread_id.clone(),
                run: RunDisposition::ended(retry_run, EndCause::NaturalEnd),
                messages: Vec::new(),
                state: vec![
                    task_state_cell(&recovered.id)
                        .write(&recovered)
                        .expect("serialize recovered terminal aggregate"),
                ],
                events: Vec::new(),
            })
            .await
            .expect("fresh-Run terminal retry commits");
        drop(store);

        let reopened = SqliteCommitCoordinator::open(database).expect("reopen after success");
        let final_state =
            Store::rebuild(&CommittedThreadView::committed_state(&reopened, &thread_id));
        let final_task = task_state_cell(&terminal.id)
            .load(&final_state)
            .expect("decode terminal aggregate after restart")
            .expect("terminal aggregate remains present");
        assert_eq!(final_task, terminal, "{case}: R4 durable terminal truth");
        assert!(final_task.attempt().is_none(), "{case}: R4 cannot replay");
    }
}

#[test]
fn an_unconfigured_plugin_is_inert() {
    // Boundary partition: registration is not enablement. Empty external
    // configuration contributes no tool and no state namespace, so a product
    // may register the extension globally without advertising unavailable work.
    use awaken_runtime_contract::plugin::Plugin;

    let contributions = BackgroundTaskPlugin::new(Default::default()).resolve();
    assert!(contributions.dynamic_tools.is_empty());
    assert!(contributions.state_keys.is_empty());
}
