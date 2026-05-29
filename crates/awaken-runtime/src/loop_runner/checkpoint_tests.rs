use super::*;
use crate::agent::state::{ToolCallState, ToolCallStatesUpdate};
use awaken_runtime_contract::contract::event_store::{EventReader, EventScope};
use awaken_runtime_contract::contract::storage::{RunStore, ThreadRunStore, ThreadStore};
use awaken_runtime_contract::contract::suspension::ToolCallResumeMode;
use awaken_stores::{
    InMemoryEventStore, InMemoryOutboxStore, InMemoryStore, MemoryCommitCoordinator,
};
use serde_json::json;
use std::sync::Arc;

fn checkpoint_reader(
    store: Arc<InMemoryStore>,
) -> crate::checkpoint_store::ThreadRunCheckpointStore {
    crate::checkpoint_store::ThreadRunCheckpointStore::new(store as Arc<dyn ThreadRunStore>)
}

fn store_with_loop_state() -> crate::state::StateStore {
    let store = crate::state::StateStore::new();
    store
        .install_plugin(crate::loop_runner::LoopStatePlugin)
        .expect("loop state plugin installs");
    store
}

#[test]
fn waiting_state_persists_suspended_tool_tickets() {
    let store = store_with_loop_state();
    commit_update::<ToolCallStates>(
        &store,
        ToolCallStatesUpdate::put(
            ToolCallState::new(
                "call-1",
                "dangerous",
                json!({"path": "/tmp/x"}),
                ToolCallStatus::Suspended,
                123,
            )
            .with_resume_mode(ToolCallResumeMode::UseDecisionAsToolResult)
            .with_suspension(Some("ticket-1".into()), Some("approval".into())),
        ),
    )
    .expect("tool state committed");

    let waiting = waiting_state_from_lifecycle(
        RunStatus::Waiting,
        Some("suspended"),
        Some("dispatch-1".into()),
        waiting_tickets_from_store(&store),
    )
    .expect("waiting state");

    assert_eq!(waiting.reason, WaitingReason::ToolPermission);
    assert_eq!(waiting.ticket_ids, vec!["ticket-1"]);
    assert_eq!(waiting.tickets.len(), 1);
    assert_eq!(waiting.tickets[0].tool_call_id, "call-1");
    assert_eq!(waiting.tickets[0].tool_name, "dangerous");
    assert_eq!(waiting.tickets[0].arguments, json!({"path": "/tmp/x"}));
    assert_eq!(
        waiting.tickets[0].resume_mode,
        ToolCallResumeMode::UseDecisionAsToolResult
    );
    assert_eq!(waiting.tickets[0].reason.as_deref(), Some("approval"));
    assert_eq!(waiting.tickets[0].updated_at, 123);
    assert_eq!(waiting.since_dispatch_id.as_deref(), Some("dispatch-1"));
}

#[test]
fn waiting_ticket_falls_back_to_tool_call_id_without_suspension_id() {
    let store = store_with_loop_state();
    commit_update::<ToolCallStates>(
        &store,
        ToolCallStatesUpdate::put(ToolCallState::new(
            "call-without-ticket",
            "plain_suspend",
            json!({"x": 1}),
            ToolCallStatus::Suspended,
            456,
        )),
    )
    .expect("tool state committed");

    let waiting = waiting_state_from_lifecycle(
        RunStatus::Waiting,
        Some("suspended"),
        None,
        waiting_tickets_from_store(&store),
    )
    .expect("waiting state");

    assert_eq!(waiting.ticket_ids, vec!["call-without-ticket"]);
    assert_eq!(waiting.tickets[0].ticket_id, "call-without-ticket");
    assert_eq!(waiting.tickets[0].tool_call_id, "call-without-ticket");
}

#[test]
fn materialize_message_log_preserves_output_across_same_run_resume() {
    let mut old_output = Message::assistant("before wait");
    old_output.id = Some("m-old-output".into());
    old_output.metadata = Some(
        awaken_runtime_contract::contract::message::MessageMetadata {
            run_id: Some("run-1".into()),
            step_index: Some(0),
        },
    );
    let mut new_output = Message::assistant("after resume");
    new_output.id = Some("m-new-output".into());

    let messages = vec![
        Arc::new(Message::user("first input")),
        Arc::new(old_output),
        Arc::new(Message::user("resume input")),
        Arc::new(new_output),
    ];
    let previous = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        parent_run_id: None,
        resolution_id: None,
        activation: None,
        request: None,
        input: Some(RunMessageInput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(1, 3),
            trigger_message_ids: vec!["resume input".into()],
            selected_message_ids: Vec::new(),
            context_policy: None,
            compacted_snapshot_id: None,
        }),
        output: Some(RunMessageOutput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(2, 2),
            message_ids: vec!["m-old-output".into()],
        }),
        status: RunStatus::Waiting,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 1,
        started_at: None,
        finished_at: None,
        updated_at: 1,
        steps: 1,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );

    let (msgs, _, output) = materialize_message_log(&messages, Some(&previous), &identity, 2, 0);

    let output = output.expect("output should be preserved and extended");
    assert_eq!(
        output.message_ids,
        vec!["m-old-output".to_string(), "m-new-output".to_string()]
    );
    assert_eq!(output.range, MessageSeqRange::new(2, 4));
    assert_eq!(msgs[3].produced_by_run_id(), Some("run-1"));
}

#[test]
fn materialize_checkpoint_append_preserves_concurrent_committed_messages() {
    let input = Message::user("first").with_id("m-input".into());
    let queued = Message::user("queued while running").with_id("m-queued".into());
    let assistant = Message::assistant("done").with_id("m-assistant".into());
    let messages = vec![Arc::new(input.clone()), Arc::new(assistant)];
    let previous = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        input: Some(RunMessageInput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(1, 1),
            trigger_message_ids: vec!["m-input".into()],
            selected_message_ids: Vec::new(),
            context_policy: None,
            compacted_snapshot_id: None,
        }),
        ..Default::default()
    };
    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );

    let (delta, output) = materialize_checkpoint_append(
        &messages,
        &[input, queued],
        Some(&previous),
        &identity,
        1,
        1,
    );

    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].id.as_deref(), Some("m-assistant"));
    assert_eq!(delta[0].produced_by_run_id(), Some("run-1"));
    let output = output.expect("assistant output is recorded");
    assert_eq!(output.range, MessageSeqRange::new(3, 3));
    assert_eq!(output.message_ids, vec!["m-assistant"]);
}

#[test]
fn materialize_checkpoint_append_preserves_committed_output_metadata() {
    let input = Message::user("first").with_id("m-input".into());
    let assistant = Message::assistant("done").with_id("m-assistant".into());
    let mut committed_assistant = assistant.clone();
    committed_assistant.mark_produced_by("run-1", Some(0));
    let previous = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        output: Some(RunMessageOutput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(2, 2),
            message_ids: vec!["m-assistant".into()],
        }),
        ..Default::default()
    };
    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );

    let (delta, output) = materialize_checkpoint_append(
        &[Arc::new(input.clone()), Arc::new(assistant)],
        &[input, committed_assistant],
        Some(&previous),
        &identity,
        2,
        1,
    );

    assert!(
        delta.is_empty(),
        "unmarked in-memory output must not replace committed producer metadata"
    );
    let output = output.expect("existing output relation remains recorded");
    assert_eq!(output.range, MessageSeqRange::new(2, 2));
    assert_eq!(output.message_ids, vec!["m-assistant"]);
}

#[test]
fn materialize_checkpoint_append_backfills_previous_output_metadata() {
    let input = Message::user("first").with_id("m-input".into());
    let assistant = Message::assistant("done").with_id("m-assistant".into());
    let previous = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        output: Some(RunMessageOutput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(2, 2),
            message_ids: vec!["m-assistant".into()],
        }),
        ..Default::default()
    };
    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );

    let (delta, output) = materialize_checkpoint_append(
        &[Arc::new(input.clone()), Arc::new(assistant)],
        &[
            input,
            Message::assistant("done").with_id("m-assistant".into()),
        ],
        Some(&previous),
        &identity,
        2,
        1,
    );

    assert!(
        delta.is_empty(),
        "append mode must not rewrite already-committed output metadata"
    );
    let output = output.expect("existing output relation remains recorded");
    assert_eq!(output.range, MessageSeqRange::new(2, 2));
    assert_eq!(output.message_ids, vec!["m-assistant"]);
}

#[test]
fn materialize_checkpoint_append_does_not_duplicate_committed_message_updates() {
    let input = Message::user("first").with_id("m-input".into());
    let committed_assistant = Message::assistant("done").with_id("m-assistant".into());
    let mut runtime_assistant = committed_assistant.clone();
    runtime_assistant.metadata = Some(
        awaken_runtime_contract::contract::message::MessageMetadata {
            run_id: Some("run-1".into()),
            step_index: Some(0),
        },
    );
    let previous = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        input: Some(RunMessageInput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(1, 1),
            trigger_message_ids: vec!["m-input".into()],
            selected_message_ids: Vec::new(),
            context_policy: None,
            compacted_snapshot_id: None,
        }),
        output: Some(RunMessageOutput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(2, 2),
            message_ids: vec!["m-assistant".into()],
        }),
        ..Default::default()
    };
    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );

    let (delta, output) = materialize_checkpoint_append(
        &[Arc::new(input.clone()), Arc::new(runtime_assistant)],
        &[input, committed_assistant],
        Some(&previous),
        &identity,
        1,
        1,
    );

    assert!(
        delta.is_empty(),
        "committed message id already exists; view/metadata changes are not append deltas"
    );
    let output = output.expect("existing output relation remains recorded");
    assert_eq!(output.range, MessageSeqRange::new(2, 2));
    assert_eq!(output.message_ids, vec!["m-assistant"]);
}

#[tokio::test]
async fn persist_checkpoint_preserves_existing_registry_manifest() {
    let state_store = store_with_loop_state();
    commit_update::<RunLifecycle>(
        &state_store,
        RunLifecycleUpdate::Start {
            run_id: "run-1".into(),
            updated_at: 1_000,
        },
    )
    .expect("lifecycle starts");

    let checkpoint_store = Arc::new(InMemoryStore::new());
    let coordinator = MemoryCommitCoordinator::wrap(Arc::clone(&checkpoint_store));
    checkpoint_store
        .create_run(&RunRecord {
            run_id: "run-1".into(),
            thread_id: "thread-1".into(),
            agent_id: "agent".into(),
            parent_run_id: None,
            resolution_id: Some("resolution-9".to_string()),
            activation: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Running,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 1,
            started_at: None,
            finished_at: None,
            updated_at: 1,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        })
        .await
        .expect("seed run");

    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );
    let messages = vec![Arc::new(Message::user("hello"))];
    let reader = checkpoint_reader(checkpoint_store.clone());

    persist_checkpoint(CheckpointPersist {
        store: &state_store,
        checkpoint_store: Some(&reader),
        commit: crate::loop_runner::CommitWiring::new(Some(&*coordinator)),
        messages: &messages,
        input_message_count: 1,
        run_identity: &identity,
        run_created_at: 1_000,
        total_input_tokens: 2,
        total_output_tokens: 3,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        thread_ctx: None,
    })
    .await
    .expect("checkpoint persists");

    let loaded = checkpoint_store
        .load_run("run-1")
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(loaded.resolution_id, Some("resolution-9".to_string()));
    assert_eq!(loaded.input_tokens, 2);
    assert_eq!(loaded.output_tokens, 3);
}

#[tokio::test]
async fn persist_checkpoint_appends_delta_after_concurrent_committed_message() {
    let state_store = store_with_loop_state();
    commit_update::<RunLifecycle>(
        &state_store,
        RunLifecycleUpdate::Start {
            run_id: "run-1".into(),
            updated_at: 1_000,
        },
    )
    .expect("lifecycle starts");
    commit_update::<RunLifecycle>(
        &state_store,
        RunLifecycleUpdate::StepCompleted { updated_at: 1_500 },
    )
    .expect("step completes");

    let checkpoint_store = Arc::new(InMemoryStore::new());
    let coordinator = MemoryCommitCoordinator::wrap(Arc::clone(&checkpoint_store));
    let input = Message::user("first").with_id("m-input".into());
    let queued = Message::user("queued while running").with_id("m-queued".into());
    let assistant = Message::assistant("done").with_id("m-assistant".into());
    let previous = RunRecord {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        agent_id: "agent".into(),
        input: Some(RunMessageInput {
            thread_id: "thread-1".into(),
            range: MessageSeqRange::new(1, 1),
            trigger_message_ids: vec!["m-input".into()],
            selected_message_ids: Vec::new(),
            context_policy: None,
            compacted_snapshot_id: None,
        }),
        status: RunStatus::Created,
        ..Default::default()
    };
    checkpoint_store
        .checkpoint_append("thread-1", std::slice::from_ref(&input), Some(0), &previous)
        .await
        .expect("seed input");
    checkpoint_store
        .checkpoint_append(
            "thread-1",
            std::slice::from_ref(&queued),
            Some(1),
            &RunRecord {
                run_id: "run-queued".into(),
                thread_id: "thread-1".into(),
                agent_id: "agent".into(),
                status: RunStatus::Created,
                ..Default::default()
            },
        )
        .await
        .expect("concurrent append");

    let identity = RunIdentity::new(
        "thread-1".into(),
        None,
        "run-1".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );
    let messages = vec![Arc::new(input), Arc::new(assistant)];
    let reader = checkpoint_reader(checkpoint_store.clone());

    persist_checkpoint(CheckpointPersist {
        store: &state_store,
        checkpoint_store: Some(&reader),
        commit: crate::loop_runner::CommitWiring::new(Some(&*coordinator)),
        messages: &messages,
        input_message_count: 1,
        run_identity: &identity,
        run_created_at: 1_000,
        total_input_tokens: 2,
        total_output_tokens: 3,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        thread_ctx: None,
    })
    .await
    .expect("checkpoint persists");

    let committed = checkpoint_store
        .load_messages("thread-1")
        .await
        .expect("load messages")
        .expect("messages exist");
    let ids: Vec<_> = committed
        .iter()
        .map(|message| message.id.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(ids, vec!["m-input", "m-queued", "m-assistant"]);
    assert_eq!(committed[2].produced_by_run_id(), Some("run-1"));

    let loaded = checkpoint_store
        .load_run("run-1")
        .await
        .expect("load run")
        .expect("run exists");
    let output = loaded.output.expect("output persisted");
    assert_eq!(output.range, MessageSeqRange::new(3, 3));
    assert_eq!(output.message_ids, vec!["m-assistant"]);
}

#[tokio::test]
async fn persist_checkpoint_uses_commit_seed_when_no_previous_record() {
    // ADR-0035 D9: when persist_checkpoint runs without a previous
    // RunRecord (direct runtime.run path), the manifest seed carried by
    // CommitWiring must populate the new RunRecord so
    // resume can later verify pinned versions.
    let state_store = store_with_loop_state();
    commit_update::<RunLifecycle>(
        &state_store,
        RunLifecycleUpdate::Start {
            run_id: "run-seed".into(),
            updated_at: 1_000,
        },
    )
    .expect("lifecycle starts");

    let checkpoint_store = Arc::new(InMemoryStore::new());
    let coordinator = MemoryCommitCoordinator::wrap(Arc::clone(&checkpoint_store));
    let identity = RunIdentity::new(
        "thread-seed".into(),
        None,
        "run-seed".into(),
        None,
        "agent-seed".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );
    let messages = vec![Arc::new(Message::user("hi"))];
    let reader = checkpoint_reader(checkpoint_store.clone());

    persist_checkpoint(CheckpointPersist {
        store: &state_store,
        checkpoint_store: Some(&reader),
        commit: crate::loop_runner::CommitWiring::new(Some(&*coordinator))
            .with_resolution_id_seed(Some("resolution-2")),
        messages: &messages,
        input_message_count: 1,
        run_identity: &identity,
        run_created_at: 1_000,
        total_input_tokens: 0,
        total_output_tokens: 0,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        thread_ctx: None,
    })
    .await
    .expect("checkpoint persists with seed");

    let loaded = checkpoint_store
        .load_run("run-seed")
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(loaded.resolution_id, Some("resolution-2".to_string()));
}

#[tokio::test]
async fn persist_checkpoint_routes_through_commit_coordinator() {
    // ADR-0036 D1+D2: when a coordinator is wired, the checkpoint commits
    // through the coordinator instead of `ThreadRunStore::checkpoint`. The
    // runtime stages no canonical drafts itself — that is the server staging
    // coordinator's job — so the plan carries an empty draft list here.
    let state_store = store_with_loop_state();
    let checkpoint_store = Arc::new(InMemoryStore::new());
    let event_store = Arc::new(InMemoryEventStore::new());
    let outbox_store = Arc::new(InMemoryOutboxStore::new());
    let coordinator = MemoryCommitCoordinator::new(
        Arc::clone(&checkpoint_store),
        Arc::clone(&event_store),
        Arc::clone(&outbox_store),
    )
    .expect("memory coordinator builds");

    let identity = RunIdentity::new(
        "thread-c".into(),
        None,
        "run-c".into(),
        None,
        "agent".into(),
        awaken_runtime_contract::contract::identity::RunOrigin::User,
    );
    let messages = vec![Arc::new(Message::user("hello"))];
    let reader = checkpoint_reader(checkpoint_store.clone());

    persist_checkpoint(CheckpointPersist {
        store: &state_store,
        checkpoint_store: Some(&reader),
        commit: CommitWiring::new(Some(&coordinator)),
        messages: &messages,
        input_message_count: 1,
        run_identity: &identity,
        run_created_at: 1_000,
        total_input_tokens: 0,
        total_output_tokens: 0,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        thread_ctx: None,
    })
    .await
    .expect("coordinator commit succeeds");

    // Thread checkpoint persisted (via coordinator path, not legacy
    // ThreadRunStore.checkpoint()).
    let loaded = checkpoint_store
        .load_run("run-c")
        .await
        .expect("load run")
        .expect("run persisted by coordinator");
    assert_eq!(loaded.thread_id, "thread-c");

    // The runtime stages no canonical events; that responsibility moved to the
    // server staging coordinator (exercised in its own unit tests).
    let count = event_store
        .count(EventScope::run("run-c"))
        .await
        .expect("count canonical events");
    assert_eq!(
        count, 0,
        "runtime no longer stages canonical events directly"
    );
}
