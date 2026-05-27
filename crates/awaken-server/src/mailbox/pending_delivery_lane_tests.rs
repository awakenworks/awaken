use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message,
};
use awaken_contract::contract::storage::{
    PinnedRegistryManifest, RunRecord, RunStore, ThreadStore,
};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_stores::{InMemoryMailboxStore, InMemoryStore, PendingMessageStore};

use super::*;
use crate::mailbox::{MailboxConfig, RunDispatchExecutor};

struct NoopExecutor;

fn empty_manifest() -> PinnedRegistryManifest {
    PinnedRegistryManifest {
        publication_id: None,
        registry_snapshot_version: None,
        entries: Vec::new(),
    }
}

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
async fn foreground_interrupt_skips_queued_new_run_pending() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    mailbox
        .deliver(
            "thread-foreground-lane",
            &[Message::user("queued future").with_id("queued-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let mut request = RunActivation::new(
        "thread-foreground-lane",
        vec![Message::user("interrupt now").with_id("interrupt-id".to_string())],
    )
    .with_agent_id("agent-1");
    let messages = request.messages().to_vec();

    let run_id = mailbox
        .prepare_run_for_dispatch(&mut request, "thread-foreground-lane", &messages)
        .await
        .unwrap();

    let committed = thread_store
        .load_committed_messages("thread-foreground-lane")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].id.as_deref(), Some("interrupt-id"));
    let pending = thread_store
        .load_pending_message_records("thread-foreground-lane")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pending_id, "queued-id");
    assert_eq!(pending[0].delivery_mode.boundary, DeliveryBoundary::NewRun);
    let run = thread_store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(
        run.activation.unwrap().input.trigger_message_ids,
        vec!["interrupt-id".to_string()]
    );
}

#[tokio::test]
async fn next_step_freeze_skips_queued_new_run_pending() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    thread_store
        .create_run(&created_run_record(
            "thread-next-step-lane",
            "run-next-step-lane",
        ))
        .await
        .unwrap();
    let request = RunActivation::new("thread-next-step-lane", Vec::new())
        .with_run_id_hint("run-next-step-lane");
    let handler = mailbox
        .pending_boundary_handler(&request, "run-next-step-lane", &empty_manifest())
        .expect("handler configured");

    mailbox
        .deliver(
            "thread-next-step-lane",
            &[Message::user("queued future").with_id("queued-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    mailbox
        .deliver(
            "thread-next-step-lane",
            &[Message::user("live steering").with_id("live-id".to_string())],
            DeliveryMode::next_step(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    handler
        .freeze_pending_boundary(DeliveryBoundary::NextStep)
        .await
        .unwrap()
        .expect("live message frozen");

    let committed = thread_store
        .load_committed_messages("thread-next-step-lane")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].id.as_deref(), Some("live-id"));
    let pending = thread_store
        .load_pending_message_records("thread-next-step-lane")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pending_id, "queued-id");
    assert_eq!(pending[0].delivery_mode.boundary, DeliveryBoundary::NewRun);
    let run = thread_store
        .load_run("run-next-step-lane")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run.input.unwrap().trigger_message_ids,
        vec!["live-id".to_string()]
    );
}

#[tokio::test]
async fn targeted_next_step_does_not_fall_through_to_new_run() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    thread_store
        .create_run(&created_run_record("thread-targeted", "run-b"))
        .await
        .unwrap();
    let request = RunActivation::new("thread-targeted", Vec::new()).with_run_id_hint("run-b");
    let handler = mailbox
        .pending_boundary_handler(&request, "run-b", &empty_manifest())
        .expect("handler configured");

    mailbox
        .deliver(
            "thread-targeted",
            &[Message::user("stale steer").with_id("stale-id".to_string())],
            DeliveryMode::next_step(DeliveryGranularity::Batch).targeted_to_run("run-a", false),
        )
        .await
        .unwrap();
    mailbox
        .deliver(
            "thread-targeted",
            &[Message::user("queued").with_id("queued-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    handler
        .freeze_pending_boundary(DeliveryBoundary::NewRun)
        .await
        .unwrap()
        .expect("queued message frozen");

    let committed = thread_store
        .load_committed_messages("thread-targeted")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].id.as_deref(), Some("queued-id"));
    let pending = thread_store
        .load_pending_message_records("thread-targeted")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pending_id, "stale-id");
}

#[tokio::test]
async fn resume_input_does_not_consume_unrelated_queued_new_run() {
    use awaken_contract::contract::tool_intercept::RunMode;

    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    mailbox
        .deliver(
            "thread-resume-lane",
            &[Message::user("later task").with_id("later-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let messages = vec![Message::user("yes").with_id("yes-id".to_string())];
    let mut request =
        RunActivation::new("thread-resume-lane", messages.clone()).with_run_id_hint("run-r");
    request.trace.run_mode = RunMode::Resume;
    let mut record = created_run_record("thread-resume-lane", "run-r");

    mailbox
        .prepare_pending_messages_for_dispatch(
            &request,
            "thread-resume-lane",
            &messages,
            "run-r",
            &mut record,
            &empty_manifest(),
        )
        .await
        .unwrap()
        .expect("resume input frozen");

    let committed = thread_store
        .load_committed_messages("thread-resume-lane")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].id.as_deref(), Some("yes-id"));
    let pending = thread_store
        .load_pending_message_records("thread-resume-lane")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pending_id, "later-id");
    assert_eq!(pending[0].delivery_mode.boundary, DeliveryBoundary::NewRun);
}
