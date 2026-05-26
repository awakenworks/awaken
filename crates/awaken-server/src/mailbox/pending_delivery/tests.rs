use super::*;
use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::{DeliveryGranularity, Message};
use awaken_contract::contract::storage::{RunStore, ThreadStore};
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_stores::{InMemoryMailboxStore, InMemoryStore, PendingMessageStore};

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
async fn deliver_appends_normalized_messages_to_pending_store() {
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
            "thread-deliver",
            &[Message::user("hello").with_id(String::new())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    assert_eq!(delivered.len(), 1);
    assert!(!delivered[0].pending_id.is_empty());
    assert_eq!(delivered[0].message.text(), "hello");
    let pending = thread_store
        .load_pending_message_records("thread-deliver")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].pending_id, delivered[0].pending_id);
}

#[tokio::test]
async fn freeze_pending_commits_delivered_messages() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    );

    mailbox
        .deliver(
            "thread-freeze",
            &[Message::user("queued")],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let frozen = mailbox
        .freeze_pending("thread-freeze", DeliveryBoundary::NewRun, Some(0))
        .await
        .unwrap();

    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen[0].seq, 1);
    assert_eq!(frozen[0].message.text(), "queued");
    assert!(
        thread_store
            .load_pending_message_records("thread-freeze")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cleanup_appended_pending_messages_retracts_unfrozen_append() {
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
            "thread-cleanup",
            &[Message::user("queued").with_id("pending-cleanup".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let pending_ids = delivered
        .iter()
        .map(|record| record.pending_id.clone())
        .collect::<Vec<_>>();
    let store = mailbox.pending_thread_run_store.as_ref().unwrap();

    mailbox
        .cleanup_appended_pending_messages(store, "thread-cleanup", &pending_ids)
        .await;

    assert!(
        thread_store
            .load_pending_message_records("thread-cleanup")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn boundary_freeze_uses_requested_delivery_boundary() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    );
    mailbox
        .deliver(
            "thread-next-step",
            &[
                Message::user("next").with_id("next-id".to_string()),
                Message::user("new").with_id("new-id".to_string()),
            ],
            DeliveryMode::next_step(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    let mut record = created_run_record("thread-next-step", "run-next-step");
    let request =
        RunActivation::new("thread-next-step", Vec::new()).with_run_id_hint("run-next-step");

    let run_id = mailbox
        .prepare_pending_boundary_for_run(
            &request,
            "thread-next-step",
            DeliveryBoundary::NextStep,
            "run-next-step",
            &mut record,
            &empty_manifest(),
        )
        .await
        .unwrap();

    assert_eq!(run_id.as_deref(), Some("run-next-step"));
    let committed = thread_store
        .load_messages("thread-next-step")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 2);
    let run = thread_store
        .load_run("run-next-step")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run.activation.unwrap().input.trigger_message_ids,
        vec!["next-id".to_string(), "new-id".to_string()]
    );
}

#[tokio::test]
async fn runtime_pending_boundary_handler_freezes_next_step_messages() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    thread_store
        .create_run(&created_run_record("thread-handler", "run-handler"))
        .await
        .unwrap();
    mailbox
        .deliver(
            "thread-handler",
            &[Message::user("steer").with_id("steer-id".to_string())],
            DeliveryMode::next_step(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let request = RunActivation::new("thread-handler", Vec::new()).with_run_id_hint("run-handler");
    let handler = mailbox
        .pending_boundary_handler(&request, "run-handler", &empty_manifest())
        .expect("handler configured");
    let frozen = handler
        .freeze_pending_boundary(DeliveryBoundary::NextStep)
        .await
        .unwrap()
        .expect("frozen messages");

    assert_eq!(frozen.messages.len(), 1);
    assert_eq!(frozen.messages[0].text(), "steer");
    let committed = thread_store
        .load_messages("thread-handler")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert!(
        thread_store
            .load_pending_message_records("thread-handler")
            .await
            .unwrap()
            .is_empty()
    );
    let run = thread_store.load_run("run-handler").await.unwrap().unwrap();
    assert_eq!(
        run.activation.unwrap().input.trigger_message_ids,
        vec!["steer-id".to_string()]
    );
}

#[tokio::test]
async fn submit_background_consumes_messages_through_pending_store() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    ));

    let result = mailbox
        .submit_background(RunActivation::new(
            "thread-submit-pending",
            vec![Message::user("queued")],
        ))
        .await
        .unwrap();

    let committed = thread_store
        .load_messages("thread-submit-pending")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].text(), "queued");
    assert!(
        thread_store
            .load_pending_message_records("thread-submit-pending")
            .await
            .unwrap()
            .is_empty()
    );
    let run = thread_store
        .load_run(&result.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.input.unwrap().range.unwrap().to_seq, 1);
    assert_eq!(run.activation.unwrap().input.trigger_message_ids.len(), 1);
}

#[tokio::test]
async fn submit_background_batches_existing_new_run_pending_messages() {
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
            "thread-submit-batch",
            &[Message::user("earlier")],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let result = mailbox
        .submit_background(RunActivation::new(
            "thread-submit-batch",
            vec![Message::user("later")],
        ))
        .await
        .unwrap();

    let committed = thread_store
        .load_messages("thread-submit-batch")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 2);
    assert_eq!(committed[0].text(), "earlier");
    assert_eq!(committed[1].text(), "later");
    let run = thread_store
        .load_run(&result.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.activation.unwrap().input.trigger_message_ids.len(), 2);
}
