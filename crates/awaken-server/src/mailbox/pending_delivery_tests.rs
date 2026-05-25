use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::mailbox::MailboxStore;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message,
};
use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadStore};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_stores::{InMemoryMailboxStore, InMemoryStore, PendingMessageStore};

use super::*;
use crate::mailbox::{MailboxConfig, MailboxDispatchStatus, RunDispatchExecutor};

struct NoopExecutor;

fn created_run_record(thread_id: &str, run_id: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "agent-1".to_string(),
        status: RunStatus::Created,
        ..Default::default()
    }
}

#[async_trait]
impl RunDispatchExecutor for NoopExecutor {
    async fn run(
        &self,
        activation: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<AgentRunResult, AgentLoopError> {
        Ok(AgentRunResult {
            run_id: activation
                .run_id_hint()
                .unwrap_or("pending-test-run")
                .to_string(),
            response: "ok".to_string(),
            termination: TerminationReason::NaturalEnd,
            steps: 1,
        })
    }

    fn cancel(&self, _id: &str) -> bool {
        false
    }

    async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
        false
    }

    fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
        false
    }
}

#[tokio::test]
async fn pending_messages_can_be_edited_reordered_and_retracted_before_freeze() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    );
    let delivered = mailbox
        .deliver(
            "thread-edit-pending",
            &[
                Message::user("first").with_id("pending-1".to_string()),
                Message::user("second").with_id("pending-2".to_string()),
            ],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let edited = mailbox
        .update_pending_message(
            "thread-edit-pending",
            &delivered[0].pending_id,
            Message::user("edited").with_id(delivered[0].pending_id.clone()),
        )
        .await
        .unwrap();
    assert_eq!(edited.message.text(), "edited");

    let reordered = mailbox
        .reorder_pending_messages(
            "thread-edit-pending",
            &[
                delivered[1].pending_id.clone(),
                delivered[0].pending_id.clone(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(reordered[0].pending_id, delivered[1].pending_id);
    assert_eq!(reordered[1].pending_id, delivered[0].pending_id);

    let retracted = mailbox
        .retract_pending_message("thread-edit-pending", &delivered[1].pending_id)
        .await
        .unwrap();
    assert_eq!(retracted.message.text(), "second");

    let frozen = mailbox
        .freeze_pending("thread-edit-pending", DeliveryBoundary::NewRun, Some(0))
        .await
        .unwrap();
    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen[0].message.text(), "edited");
}

#[tokio::test]
async fn pending_message_edit_after_freeze_returns_consumed_error() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store,
        "consumer".to_string(),
        MailboxConfig::default(),
    );
    let delivered = mailbox
        .deliver(
            "thread-edit-consumed",
            &[Message::user("sent").with_id("sent-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    mailbox
        .freeze_pending("thread-edit-consumed", DeliveryBoundary::NewRun, Some(0))
        .await
        .unwrap();

    let error = mailbox
        .update_pending_message(
            "thread-edit-consumed",
            &delivered[0].pending_id,
            Message::user("too late").with_id(delivered[0].pending_id.clone()),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("already consumed"));
}

#[tokio::test]
async fn live_then_queue_stages_remote_running_input_as_next_step_pending() {
    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let mut run = created_run_record("thread-live-pending", "run-live-pending");
    run.status = RunStatus::Running;
    run.dispatch_id = Some("dispatch-live-pending".to_string());
    thread_store.create_run(&run).await.unwrap();
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        mailbox_store.clone(),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_live_then_queue(
            RunActivation::new("thread-live-pending", vec![Message::user("steer")])
                .with_agent_id("agent-1"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.status, MailboxDispatchStatus::Running);
    assert_eq!(result.run_id, "run-live-pending");
    let pending = thread_store
        .load_pending_message_records("thread-live-pending")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.text(), "steer");
    assert_eq!(
        pending[0].delivery_mode,
        DeliveryMode::next_step(DeliveryGranularity::Batch)
    );
    let dispatches = mailbox_store
        .list_dispatches("thread-live-pending", None, 10, 0)
        .await
        .unwrap();
    assert!(dispatches.is_empty());
}

#[tokio::test]
async fn foreground_prepare_consumes_messages_through_interrupt_boundary() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    let mut request = RunActivation::new(
        "thread-foreground-pending",
        vec![Message::user("interrupt now").with_id("interrupt-id".to_string())],
    )
    .with_agent_id("agent-1");
    let messages = request.messages().to_vec();

    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, "thread-foreground-pending", &messages)
        .await
        .unwrap();

    let committed = thread_store
        .load_messages("thread-foreground-pending")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].text(), "interrupt now");
    assert!(
        thread_store
            .load_pending_message_records("thread-foreground-pending")
            .await
            .unwrap()
            .is_empty()
    );
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(
        run.activation.unwrap().input.trigger_message_ids,
        vec!["interrupt-id".to_string()]
    );
}

fn empty_manifest() -> awaken_contract::contract::storage::PinnedRegistryManifest {
    awaken_contract::contract::storage::PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: Vec::new(),
    }
}

#[tokio::test]
async fn resume_with_user_messages_routes_through_pending() {
    use awaken_contract::contract::tool_intercept::RunMode;
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    let mut record = created_run_record("thread-resume-user", "run-resume-user");
    let messages = vec![Message::user("steer me").with_id("u-resume".to_string())];
    let mut request = RunActivation::new("thread-resume-user", messages.clone())
        .with_run_id_hint("run-resume-user");
    // A reusable waiting run is auto-converted to Resume in prepare_run_for_dispatch.
    request.trace.run_mode = RunMode::Resume;

    let out = mailbox
        .prepare_pending_messages_for_dispatch(
            &request,
            "thread-resume-user",
            &messages,
            "run-resume-user",
            &mut record,
            &empty_manifest(),
        )
        .await
        .unwrap();

    assert_eq!(
        out.as_deref(),
        Some("run-resume-user"),
        "user input auto-routed to a waiting run must stage through pending, not direct-append"
    );
}

#[tokio::test]
async fn internal_wake_skips_pending() {
    use awaken_contract::contract::tool_intercept::RunMode;
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    let mut record = created_run_record("thread-wake", "run-wake");
    let messages = vec![Message::user("wake").with_id("u-wake".to_string())];
    let mut request =
        RunActivation::new("thread-wake", messages.clone()).with_run_id_hint("run-wake");
    request.trace.run_mode = RunMode::InternalWake;

    let out = mailbox
        .prepare_pending_messages_for_dispatch(
            &request,
            "thread-wake",
            &messages,
            "run-wake",
            &mut record,
            &empty_manifest(),
        )
        .await
        .unwrap();

    assert!(out.is_none(), "internal wake must not stage user pending");
}

#[tokio::test]
async fn boundary_freeze_accumulates_run_input_across_freezes() {
    use awaken_runtime::loop_runner::PendingBoundaryHandler;
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    thread_store
        .create_run(&created_run_record("thread-acc", "run-acc"))
        .await
        .unwrap();
    let request = RunActivation::new("thread-acc", Vec::new()).with_run_id_hint("run-acc");
    let handler = mailbox
        .pending_boundary_handler(&request, "run-acc", &empty_manifest())
        .expect("handler configured");

    mailbox
        .deliver(
            "thread-acc",
            &[Message::user("a").with_id("a-id".to_string())],
            DeliveryMode::next_step(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    handler
        .freeze_pending_boundary(DeliveryBoundary::NextStep)
        .await
        .unwrap()
        .expect("frozen a");

    mailbox
        .deliver(
            "thread-acc",
            &[Message::user("b").with_id("b-id".to_string())],
            DeliveryMode::next_step(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    handler
        .freeze_pending_boundary(DeliveryBoundary::NextStep)
        .await
        .unwrap()
        .expect("frozen b");

    let run = thread_store.load_run("run-acc").await.unwrap().unwrap();
    assert_eq!(
        run.input.unwrap().trigger_message_ids,
        vec!["a-id".to_string(), "b-id".to_string()],
        "run input must accumulate consumed triggers across boundary freezes"
    );
}
