use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
use awaken_server_contract::contract::event_sink::EventSink;
use awaken_server_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_server_contract::contract::mailbox::MailboxStore;
use awaken_server_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message, MessageRecord,
    PendingMessageRecord,
};
use awaken_server_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadStore,
};
use awaken_server_contract::contract::suspension::ToolCallResume;
use awaken_server_contract::thread::{Thread, ThreadMetadata};
use awaken_stores::{InMemoryMailboxStore, InMemoryStore, PendingMessageStore};

use super::*;
use crate::mailbox::{
    MailboxConfig, MailboxDispatchStatus, RunDispatchExecutor, live_target_for_run,
};

/// Test store that wraps an [`InMemoryStore`] and forces exactly one
/// `VersionConflict` on the first `freeze_pending_message_records_with_run`
/// call. Before signalling the conflict it retracts a configured pending id
/// from the inner store, so the *next* freeze attempt selects a strictly
/// smaller set. This reproduces the FIX #5 scenario where a retried attempt
/// must re-derive its run input from the originally persisted prior, not from
/// the failed attempt's mutated record (which would otherwise leave a phantom
/// trigger id that was never frozen).
struct ConflictOnceStore {
    inner: Arc<InMemoryStore>,
    freeze_calls: AtomicUsize,
    retract_on_first_conflict: Option<(String, String)>,
}

impl ConflictOnceStore {
    fn new(inner: Arc<InMemoryStore>, retract_on_first_conflict: Option<(String, String)>) -> Self {
        Self {
            inner,
            freeze_calls: AtomicUsize::new(0),
            retract_on_first_conflict,
        }
    }
}

#[async_trait]
impl ThreadStore for ConflictOnceStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        self.inner.load_thread(thread_id).await
    }
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(thread).await
    }
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(thread_id).await
    }
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.inner.list_threads(offset, limit).await
    }
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(thread_id).await
    }
    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_committed_messages(thread_id).await
    }
    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.inner.save_messages(thread_id, messages).await
    }
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(thread_id).await
    }
    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner.update_thread_metadata(id, metadata).await
    }
}

#[async_trait]
impl RunStore for ConflictOnceStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.inner.create_run(record).await
    }
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.load_run(run_id).await
    }
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.latest_run(thread_id).await
    }
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.inner.list_runs(query).await
    }
}

#[async_trait]
impl awaken_server_contract::contract::storage::ThreadRunStore for ConflictOnceStore {
    #[allow(deprecated)]
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.inner.checkpoint(thread_id, messages, run).await
    }
}

#[async_trait]
impl PendingMessageStore for ConflictOnceStore {
    async fn load_pending_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.inner.load_pending_message_records(thread_id).await
    }
    async fn list_threads_with_pending_messages(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<String>, StorageError> {
        self.inner
            .list_threads_with_pending_messages(limit, after)
            .await
    }
    async fn append_pending_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.inner
            .append_pending_message_records(thread_id, messages, delivery_mode)
            .await
    }
    async fn update_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
        message: Message,
    ) -> Result<PendingMessageRecord, StorageError> {
        self.inner
            .update_pending_message_record_checked(
                thread_id,
                pending_id,
                expected_revision,
                message,
            )
            .await
    }
    async fn retract_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<PendingMessageRecord, StorageError> {
        self.inner
            .retract_pending_message_record_checked(thread_id, pending_id, expected_revision)
            .await
    }
    async fn reorder_pending_message_records_checked(
        &self,
        thread_id: &str,
        expected_queue_revision: Option<u64>,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.inner
            .reorder_pending_message_records_checked(
                thread_id,
                expected_queue_revision,
                ordered_pending_ids,
            )
            .await
    }
    async fn freeze_pending_message_records(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        self.inner
            .freeze_pending_message_records(thread_id, boundary, expected_message_version)
            .await
    }
    async fn freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
        expected_pending_ids: &[String],
        run: &RunRecord,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        if self.freeze_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // Mutate pending so the retried attempt selects a smaller set, then
            // signal the conflict so the loop retries.
            if let Some((conflict_thread_id, pending_id)) = &self.retract_on_first_conflict {
                self.inner
                    .retract_pending_message_record(conflict_thread_id, pending_id)
                    .await?;
            }
            return Err(StorageError::VersionConflict {
                expected: expected_message_version.unwrap_or(0),
                actual: expected_message_version.unwrap_or(0).saturating_add(1),
            });
        }
        self.inner
            .freeze_pending_message_records_with_run(
                thread_id,
                boundary,
                expected_message_version,
                expected_pending_ids,
                run,
            )
            .await
    }
    async fn append_and_freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        new_messages: &[Message],
        append_delivery_mode: DeliveryMode,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
        expected_pending_ids: &[String],
        run: &RunRecord,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        if self.freeze_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some((conflict_thread_id, pending_id)) = &self.retract_on_first_conflict {
                self.inner
                    .retract_pending_message_record(conflict_thread_id, pending_id)
                    .await?;
            }
            return Err(StorageError::VersionConflict {
                expected: expected_message_version.unwrap_or(0),
                actual: expected_message_version.unwrap_or(0).saturating_add(1),
            });
        }
        self.inner
            .append_and_freeze_pending_message_records_with_run(
                thread_id,
                new_messages,
                append_delivery_mode,
                boundary,
                expected_message_version,
                expected_pending_ids,
                run,
            )
            .await
    }
}

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
        .update_pending_message_checked(
            "thread-edit-pending",
            &delivered[0].pending_id,
            None,
            Message::user("edited").with_id(delivered[0].pending_id.clone()),
        )
        .await
        .unwrap();
    assert_eq!(edited.message.text(), "edited");

    let reordered = mailbox
        .reorder_pending_messages_checked(
            "thread-edit-pending",
            None,
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
        .retract_pending_message_checked("thread-edit-pending", &delivered[1].pending_id, None)
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
async fn recover_detects_orphaned_pending_thread() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store,
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    // Pending written with no dispatch and no run: a lost consume opportunity.
    mailbox
        .deliver(
            "thread-orphan",
            &[Message::user("stranded").with_id("p1".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    // Reported as an orphan when no queued dispatch covers it.
    let orphaned = mailbox.recover_orphaned_pending_threads(&[]).await.unwrap();
    assert_eq!(orphaned, 1);

    // A thread already covered by a queued dispatch is not reported.
    let covered = mailbox
        .recover_orphaned_pending_threads(&["thread-orphan".to_string()])
        .await
        .unwrap();
    assert_eq!(covered, 0);
}

#[tokio::test]
async fn recover_pages_through_all_orphaned_pending_threads() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Arc::new(Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store,
        "consumer".to_string(),
        MailboxConfig::default(),
    ));
    // More orphaned threads than a single recovery page holds, so detection must
    // advance the cursor across page boundaries to count every one.
    let total = Mailbox::PENDING_RECOVERY_PAGE_SIZE + 5;
    for i in 0..total {
        mailbox
            .deliver(
                &format!("thread-orphan-{i:04}"),
                &[Message::user("stranded").with_id(format!("p{i:04}"))],
                DeliveryMode::new_run(DeliveryGranularity::Batch),
            )
            .await
            .unwrap();
    }

    let orphaned = mailbox.recover_orphaned_pending_threads(&[]).await.unwrap();
    assert_eq!(orphaned, total);
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
        .update_pending_message_checked(
            "thread-edit-consumed",
            &delivered[0].pending_id,
            None,
            Message::user("too late").with_id(delivered[0].pending_id.clone()),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("already consumed"));
}

#[tokio::test]
async fn live_then_queue_stages_remote_running_input_as_next_step_pending() {
    use awaken_server_contract::contract::mailbox::LiveRunCommand;
    use futures::StreamExt;

    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let mut run = created_run_record("thread-live-pending", "run-live-pending");
    run.status = RunStatus::Running;
    run.dispatch_id = Some("dispatch-live-pending".to_string());
    thread_store.create_run(&run).await.unwrap();
    let subscriber = mailbox_store
        .open_live_channel_for(&live_target_for_run(&run))
        .await
        .unwrap();
    let captured = Arc::new(tokio::sync::Mutex::new(Vec::<LiveRunCommand>::new()));
    let captured_clone = captured.clone();
    let _forwarder = tokio::spawn(async move {
        let mut subscriber = subscriber;
        while let Some(entry) = subscriber.next().await {
            captured_clone.lock().await.push(entry.command.clone());
            entry.receipt.ack();
        }
    });
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
    assert!(
        captured
            .lock()
            .await
            .iter()
            .any(|command| matches!(command, LiveRunCommand::PendingBoundaryWake))
    );
    let pending = thread_store
        .load_pending_message_records("thread-live-pending")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].message.text(), "steer");
    assert_eq!(
        pending[0].delivery_mode,
        DeliveryMode::next_step(DeliveryGranularity::Batch)
            .targeted_to_run("run-live-pending", false)
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

#[tokio::test]
async fn resume_with_user_messages_routes_through_pending() {
    use awaken_server_contract::contract::tool_intercept::RunMode;
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
            "resolution-test",
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
    use awaken_server_contract::contract::tool_intercept::RunMode;
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
            "resolution-test",
        )
        .await
        .unwrap();

    assert!(out.is_none(), "internal wake must not stage user pending");
}

#[tokio::test]
async fn boundary_freeze_accumulates_run_input_across_freezes() {
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
        .pending_boundary_handler(&request, "run-acc", "resolution-test")
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

// FIX #5: a freeze retry after a VersionConflict must derive its run input from
// the originally persisted prior, not from the failed attempt's mutated record.
// Here the first freeze attempt selects two pending messages, conflicts, and the
// store retracts one of them; the second attempt selects only the survivor. The
// final run input must contain exactly the frozen id with no phantom id from the
// failed attempt.
#[tokio::test]
async fn freeze_retry_after_conflict_does_not_leave_phantom_trigger_ids() {
    let inner = Arc::new(InMemoryStore::new());
    let store = Arc::new(ConflictOnceStore::new(
        inner.clone(),
        Some(("thread-conflict".to_string(), "b2-id".to_string())),
    ));
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        store,
        "consumer".to_string(),
        MailboxConfig::default(),
    );
    mailbox
        .deliver(
            "thread-conflict",
            &[
                Message::user("b1").with_id("b1-id".to_string()),
                Message::user("b2").with_id("b2-id".to_string()),
            ],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let mut record = created_run_record("thread-conflict", "run-conflict");
    let request =
        RunActivation::new("thread-conflict", Vec::new()).with_run_id_hint("run-conflict");
    let run_id = mailbox
        .prepare_pending_boundary_for_run(
            &request,
            "thread-conflict",
            DeliveryBoundary::NewRun,
            "run-conflict",
            &mut record,
            "resolution-test",
            None,
        )
        .await
        .unwrap();

    assert_eq!(run_id.as_deref(), Some("run-conflict"));
    // Only "b1" was actually frozen; "b2" was retracted on the conflicting
    // attempt and must not appear as a phantom trigger id.
    assert_eq!(
        record.input.as_ref().unwrap().trigger_message_ids,
        vec!["b1-id".to_string()],
        "retry must not carry the failed attempt's trigger ids forward"
    );
    let committed = inner
        .load_committed_messages("thread-conflict")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].text(), "b1");
    // Each id appears exactly once (no duplication).
    let triggers = &record.input.as_ref().unwrap().trigger_message_ids;
    let unique = triggers.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), triggers.len(), "trigger ids must be unique");
}

struct FailingEventPublisher;

#[async_trait]
impl awaken_server_contract::contract::commit_coordinator::OutboxServerEventPublisher
    for FailingEventPublisher
{
    async fn publish(
        &self,
        _draft: awaken_server_contract::contract::event_store::CanonicalEventDraft,
        _options: awaken_server_contract::contract::event_store::AppendOptions,
    ) -> Result<
        awaken_server_contract::contract::commit_coordinator::ServerEventPublishOutcome,
        awaken_server_contract::contract::commit_coordinator::EventPublishError,
    > {
        Err(
            awaken_server_contract::contract::commit_coordinator::EventPublishError::Enqueue(
                awaken_server_contract::contract::outbox::OutboxError::Io(
                    "event publisher unavailable".to_string(),
                ),
            ),
        )
    }
}

// After a successful freeze the canonical checkpoint events go through the
// advisory outbox publisher, which is outside the freeze transaction. A publish
// failure must not turn the already-committed freeze into a caller-visible
// failure, because that false negative can make clients retry and duplicate
// user input. Startup repair re-derives the missing checkpoint events.
#[tokio::test]
async fn freeze_event_publish_failure_is_repairable_success() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store.clone(),
        "consumer".to_string(),
        MailboxConfig::default(),
    )
    .with_server_event_publisher(Arc::new(FailingEventPublisher), "server")
    .unwrap();
    mailbox
        .deliver(
            "thread-event-fail",
            &[Message::user("queued").with_id("evt-id".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();

    let mut record = created_run_record("thread-event-fail", "run-event-fail");
    let request =
        RunActivation::new("thread-event-fail", Vec::new()).with_run_id_hint("run-event-fail");
    let frozen = mailbox
        .prepare_pending_boundary_for_run(
            &request,
            "thread-event-fail",
            DeliveryBoundary::NewRun,
            "run-event-fail",
            &mut record,
            "resolution-test",
            None,
        )
        .await
        .expect("event publish failure after freeze commit is repairable");

    assert_eq!(frozen, Some("run-event-fail".to_string()));
    let committed = thread_store
        .load_committed_messages("thread-event-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].id.as_deref(), Some("evt-id"));
    assert!(
        thread_store
            .load_pending_message_records("thread-event-fail")
            .await
            .unwrap()
            .is_empty(),
        "successful freeze must consume pending instead of cleanup/retry"
    );
}

#[tokio::test]
async fn preflight_foreground_blocked_by_leading_barrier() {
    // ADR-0042 D6 / Major 2: a barrier ahead in pending blocks a foreground
    // interrupt. The preflight must report it BEFORE any interrupt/cancel side
    // effect, so the active run is never cancelled only to then fail Internal.
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store,
        "consumer".to_string(),
        MailboxConfig::default(),
    );
    let mut barrier = DeliveryMode::new_run(DeliveryGranularity::Batch);
    barrier.barrier = true;
    mailbox
        .deliver(
            "t-barrier",
            &[Message::user("queued").with_id("p-barrier".to_string())],
            barrier,
        )
        .await
        .unwrap();

    let err = mailbox
        .preflight_foreground_pending("t-barrier")
        .await
        .unwrap_err();
    match err {
        crate::mailbox::MailboxError::DeliveryBlockedByBarrier {
            blocking_pending_id,
        } => {
            assert_eq!(blocking_pending_id, "p-barrier");
        }
        other => panic!("expected DeliveryBlockedByBarrier, got {other:?}"),
    }
}

#[tokio::test]
async fn preflight_foreground_allows_empty_and_skippable_pending() {
    let thread_store = Arc::new(InMemoryStore::new());
    let mailbox = Mailbox::new_with_pending_thread_run_store(
        Arc::new(NoopExecutor),
        Arc::new(InMemoryMailboxStore::new()),
        thread_store,
        "consumer".to_string(),
        MailboxConfig::default(),
    );
    // Empty pending: nothing blocks a foreground interrupt.
    mailbox
        .preflight_foreground_pending("t-empty")
        .await
        .unwrap();

    // A non-barrier queued NewRun is skippable for an Interrupt boundary, so the
    // foreground message would still be eligible — not blocked.
    mailbox
        .deliver(
            "t-skip",
            &[Message::user("queued").with_id("p1".to_string())],
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        )
        .await
        .unwrap();
    mailbox
        .preflight_foreground_pending("t-skip")
        .await
        .unwrap();
}
