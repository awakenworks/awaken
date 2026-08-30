use std::collections::BTreeSet;
use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_background_task::{
    BackgroundInvocation, BackgroundTask, BackgroundTaskCompletion, BackgroundTaskConfig,
    BackgroundTaskEnd, BackgroundTaskId, BackgroundTaskLifecycle, BackgroundTaskOrigin,
    BackgroundTaskPlugin, BackgroundTaskSupervisor, TaskExecutionPolicy, TaskFence,
    task_state_cell,
};
use awaken_runtime_contract::plugin::{
    PhaseContext, PhaseHookPoint, PhaseKind, Plugin, ResolvedExecutionEnv,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolCall, ToolConcurrency, ToolError, ToolExecutionFacts, ToolExecutionFactsResolver,
    ToolOperationContext, ToolRecoveryPolicy, with_tool_execution_facts,
    with_tool_operation_context, with_tool_state_context,
};

struct FixtureExecutionFacts;

impl ToolExecutionFactsResolver for FixtureExecutionFacts {
    fn resolve(&self, call: &ToolCall) -> Result<ToolExecutionFacts, ToolError> {
        (call.tool_id == "bash")
            .then_some(ToolExecutionFacts {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            })
            .ok_or_else(|| ToolError::Unknown(call.tool_id.clone()))
    }
}

fn environment() -> ResolvedExecutionEnv {
    let plugin = BackgroundTaskPlugin::new(BackgroundTaskConfig {
        tools: BTreeSet::from(["bash".into()]),
    });
    ResolvedExecutionEnv::merge(vec![(plugin.manifest(), plugin.resolve())])
        .expect("fixture plugin resolves within its manifest")
}

async fn invoke(
    tool: &dyn RawTool,
    call: ToolCall,
    state: Store,
    operation: &str,
) -> awaken_runtime_contract::tool::ToolOutput {
    with_tool_state_context(
        state,
        with_tool_operation_context(
            ToolOperationContext {
                run_id: Some(RunId("run-1".into())),
                thread_id: Some(ThreadId("thread-1".into())),
                operation_id: operation.into(),
                call_id: Some(call.call_id.clone()),
                execution_scope: None,
            },
            with_tool_execution_facts(Arc::new(FixtureExecutionFacts), tool.invoke(call)),
        ),
    )
    .await
    .expect("fixture tool invocation succeeds")
}

#[tokio::test]
async fn tools_create_list_get_cancel_only_through_runtime_state_commands() {
    // End-to-end cause graph: wrapper invocation -> one StateCommand; Runtime
    // apply/replay -> list/get observe it; cancel -> one replacement command;
    // replaying the same wrapper operation -> same id and no duplicate write.
    // The test deliberately has no repository, SQL connection, or service mock.
    let env = environment();
    let run = env
        .dynamic_tool("run_in_background")
        .expect("configured run tool exists");
    let mut store = Store::new();
    let submitted = invoke(
        run.as_ref(),
        ToolCall {
            call_id: "submit".into(),
            tool_id: "run_in_background".into(),
            arguments: serde_json::json!({"tool":"bash","arguments":{"command":"sleep 1"}}),
        },
        store.clone(),
        "operation-1",
    )
    .await;
    assert_eq!(submitted.state.len(), 1);
    store.apply(&submitted.state[0]);

    let replay = invoke(
        run.as_ref(),
        ToolCall {
            call_id: "submit".into(),
            tool_id: "run_in_background".into(),
            arguments: serde_json::json!({"tool":"bash","arguments":{"command":"sleep 1"}}),
        },
        store.clone(),
        "operation-1",
    )
    .await;
    assert!(
        replay.state.is_empty(),
        "deterministic replay is a read-only receipt"
    );
    assert_eq!(replay.text(), submitted.text());

    let receipt = serde_json::from_str::<serde_json::Value>(&submitted.text())
        .expect("submission receipt is JSON");
    let task_id = receipt["task_id"]
        .as_str()
        .expect("submission receipt contains a task id")
        .to_string();
    let list = invoke(
        env.dynamic_tool("list_background_tasks")
            .expect("configured list tool exists")
            .as_ref(),
        ToolCall {
            call_id: "list".into(),
            tool_id: "list_background_tasks".into(),
            arguments: serde_json::json!({}),
        },
        store.clone(),
        "operation-2",
    )
    .await;
    assert!(list.text().contains(&task_id));
    assert!(
        !list.text().contains("sleep 1"),
        "management views do not leak invocation arguments"
    );

    let get = invoke(
        env.dynamic_tool("get_background_task")
            .expect("configured get tool exists")
            .as_ref(),
        ToolCall {
            call_id: "get".into(),
            tool_id: "get_background_task".into(),
            arguments: serde_json::json!({"task_id":task_id}),
        },
        store.clone(),
        "operation-3",
    )
    .await;
    assert!(get.text().contains("running"));

    let cancel = invoke(
        env.dynamic_tool("cancel_background_task")
            .expect("configured cancel tool exists")
            .as_ref(),
        ToolCall {
            call_id: "cancel".into(),
            tool_id: "cancel_background_task".into(),
            arguments: serde_json::json!({"task_id":task_id}),
        },
        store,
        "operation-4",
    )
    .await;
    assert_eq!(cancel.state.len(), 1);
    assert!(cancel.text().contains("cancelling"));
}

#[tokio::test]
async fn configuration_schema_and_persisted_state_fail_closed() {
    // Partitions: configured target is admitted; unknown target is rejected;
    // unknown config fields fail plugin resolution; malformed persisted JSON
    // fails list rather than disappearing as an empty task set.
    let plugin = BackgroundTaskPlugin::new(Default::default());
    assert!(
        plugin
            .resolve_configured(Some(&serde_json::json!({"tools":["bash"],"extra":true})))
            .is_err()
    );
    let env = environment();
    let run = env
        .dynamic_tool("run_in_background")
        .expect("configured run tool exists");
    let error = with_tool_state_context(
        Store::new(),
        with_tool_operation_context(
            ToolOperationContext {
                run_id: Some(RunId("run".into())),
                thread_id: Some(ThreadId("thread".into())),
                operation_id: "op".into(),
                call_id: Some("call".into()),
                execution_scope: None,
            },
            run.invoke(ToolCall {
                call_id: "call".into(),
                tool_id: "run_in_background".into(),
                arguments: serde_json::json!({"tool":"unknown","arguments":{}}),
            }),
        ),
    )
    .await
    .expect_err("unconfigured target must fail closed");
    assert!(error.to_string().contains("not configured"));

    let mut malformed = Store::new();
    malformed.apply(&Command::set(
        Scope::Thread,
        MergePolicy::Exclusive,
        "background_task/bad",
        serde_json::json!({"state":"wrong"}),
    ));
    let list = env
        .dynamic_tool("list_background_tasks")
        .expect("configured list tool exists");
    let error = with_tool_state_context(
        malformed,
        list.invoke(ToolCall {
            call_id: "list".into(),
            tool_id: "list_background_tasks".into(),
            arguments: serde_json::json!({}),
        }),
    )
    .await
    .expect_err("malformed committed state must fail closed");
    assert!(error.to_string().contains("malformed"));
}

#[test]
fn plugin_manifest_has_no_persistence_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in ["sqlx", "rusqlite", "migration", "background-task-store"] {
        assert!(
            !manifest.contains(forbidden),
            "plugin must not depend on {forbidden}"
        );
    }
}

#[test]
fn model_schemas_are_derived_from_typed_arguments_then_narrowed_by_configuration() {
    // Cause graph: Rust argument structs -> generated closed object schemas;
    // external allowed-tool configuration -> only the dynamic enum refinement.
    // Effects: field additions cannot drift from parsing, unknown keys are
    // rejected by both schema and Serde, and the model cannot select an
    // unconfigured target.
    let env = environment();
    let descriptors = env.dynamic_descriptors();
    let run = descriptors
        .iter()
        .find(|descriptor| descriptor.id == "run_in_background")
        .expect("configured wrapper descriptor");
    let schema = run.model_parameters();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["tool"]["enum"],
        serde_json::json!(["bash"])
    );
    assert_eq!(schema["properties"]["arguments"]["type"], "object");

    let get = descriptors
        .iter()
        .find(|descriptor| descriptor.id == "get_background_task")
        .expect("get descriptor");
    let schema = get.model_parameters();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["task_id"]["minLength"], 1);
}

#[tokio::test]
async fn step_start_folds_completion_and_reclaims_only_after_lease_expiry() {
    // Cause graph / decision table:
    // C1 matching process completion + current fence -> E1 Ended in one StateCommand;
    // C2 replacement worker before lease expiry -> E2 no command;
    // C3 replacement worker at expiry + replay-safe policy -> E3 epoch increments
    // and ownership moves in one command; C4 an old local completion observes
    // the new durable fence -> E4 it is retired and cannot suppress relaunch.
    // This is the persisted recovery edge;
    // the product observer may execute only after E1/E3's enclosing Run commits.
    let supervisor = Arc::new(BackgroundTaskSupervisor::new("worker-new"));
    let plugin = BackgroundTaskPlugin::with_supervisor(
        BackgroundTaskConfig {
            tools: BTreeSet::from(["bash".into()]),
        },
        supervisor.clone(),
    );
    let env = ResolvedExecutionEnv::merge(vec![(plugin.manifest(), plugin.resolve())])
        .expect("plugin environment");
    let hook = env
        .hooks_for(PhaseHookPoint::StepStart)
        .into_iter()
        .next()
        .expect("reconciliation hook");
    let ctx = PhaseContext {
        run_id: RunId("reconcile".into()),
        step: 0,
        kind: PhaseKind::StepStart,
    };

    let make_task = |id: &str| {
        BackgroundTask::requested(
            BackgroundTaskId::new(id).expect("task id"),
            BackgroundTaskOrigin {
                thread_id: ThreadId("thread".into()),
                run_id: RunId("origin".into()),
                operation_id: format!("operation-{id}"),
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id: format!("call-{id}"),
                    tool_id: "bash".into(),
                    arguments: serde_json::json!({}),
                },
            },
        )
    };

    let mut completed = make_task("completed");
    let completed_fence = completed
        .start(
            supervisor.worker_id(),
            BackgroundTaskSupervisor::now_ms(),
            BackgroundTaskSupervisor::lease_ms(),
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("completed claim");
    assert!(
        supervisor.register(&completed.id).is_some(),
        "C1 completion must follow local launch registration"
    );
    supervisor.complete(
        completed.id.clone(),
        BackgroundTaskCompletion {
            fence: completed_fence,
            end: BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        },
    );
    let mut completed_state = Store::new();
    completed_state.apply(
        &task_state_cell(&completed.id)
            .write(&completed)
            .expect("completed state"),
    );
    let reaction = hook.on_phase(&ctx, &[], &completed_state).await;
    assert_eq!(reaction.state.len(), 1, "C1/E1");
    completed_state.apply(&reaction.state[0]);
    let folded = task_state_cell(&completed.id)
        .load(&completed_state)
        .expect("typed completion")
        .expect("completion remains present");
    assert!(matches!(
        folded.lifecycle,
        BackgroundTaskLifecycle::Ended { .. }
    ));

    let now = BackgroundTaskSupervisor::now_ms();
    let mut leased = make_task("leased");
    leased
        .start(
            "worker-old",
            now,
            BackgroundTaskSupervisor::lease_ms(),
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::replay_safe(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("old claim");
    let mut leased_state = Store::new();
    leased_state.apply(
        &task_state_cell(&leased.id)
            .write(&leased)
            .expect("leased state"),
    );
    assert!(
        hook.on_phase(&ctx, &[], &leased_state)
            .await
            .state
            .is_empty(),
        "C2/E2"
    );

    let mut replacement = make_task("replacement");
    replacement
        .start(
            "worker-old",
            now.saturating_sub(2),
            1,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::replay_safe(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("expired claim");
    let mut replacement_state = Store::new();
    replacement_state.apply(
        &task_state_cell(&replacement.id)
            .write(&replacement)
            .expect("replacement state"),
    );
    let reaction = hook.on_phase(&ctx, &[], &replacement_state).await;
    assert_eq!(reaction.state.len(), 1, "C3/E3");
    replacement_state.apply(&reaction.state[0]);
    let reclaimed = task_state_cell(&replacement.id)
        .load(&replacement_state)
        .expect("typed reclaim")
        .expect("reclaimed task");
    let attempt = reclaimed.attempt().expect("reclaimed attempt");
    assert_eq!(attempt.worker_id, supervisor.worker_id());
    assert_eq!(attempt.epoch, 2);

    assert!(supervisor.register(&replacement.id).is_some(), "C4 setup");
    supervisor.complete(
        replacement.id.clone(),
        BackgroundTaskCompletion {
            fence: TaskFence {
                worker_id: "worker-old".into(),
                epoch: 1,
            },
            end: BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        },
    );
    assert!(supervisor.completion(&replacement.id).is_some(), "C4 setup");
    assert!(
        hook.on_phase(&ctx, &[], &replacement_state)
            .await
            .state
            .is_empty(),
        "C4/E4 stale completion cannot mutate the current attempt"
    );
    assert!(supervisor.completion(&replacement.id).is_none(), "C4/E4");
    assert!(
        supervisor.register(&replacement.id).is_some(),
        "C4/E4 the stale guard no longer suppresses post-commit launch"
    );
}
