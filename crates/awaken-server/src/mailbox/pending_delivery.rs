use std::sync::Arc;

use awaken_contract::contract::mailbox::MailboxStore;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message, MessageRecord,
    PendingMessageRecord, select_pending_for_freeze,
};
use awaken_contract::contract::run::{RunActivationSnapshot, RunInputSnapshot};
use awaken_contract::contract::storage::{
    PinnedRegistryManifest, RunRecord, StorageError, ThreadRunStore,
};
use awaken_contract::contract::tool_intercept::RunMode;
use awaken_contract::now_ms;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, PendingBoundaryFreeze, PendingBoundaryHandler};

use super::Mailbox;
use super::helpers::{build_run_input, normalize_message_ids};
use super::{IntoDispatchExecutor, MailboxConfig, MailboxError};

const MAX_PENDING_FREEZE_ATTEMPTS: usize = 8;

impl Mailbox {
    /// Construct a mailbox whose pending partition is owned by the same
    /// thread/run backend as committed messages and run records.
    #[must_use]
    pub fn new_with_pending_thread_run_store<T>(
        executor: impl IntoDispatchExecutor,
        store: Arc<dyn MailboxStore>,
        thread_run_store: Arc<T>,
        consumer_id: String,
        config: MailboxConfig,
    ) -> Self
    where
        T: awaken_stores::PendingThreadRunStore + 'static,
    {
        let pending_thread_run_store =
            Arc::clone(&thread_run_store) as Arc<dyn awaken_stores::PendingThreadRunStore>;
        let thread_run_store = thread_run_store as Arc<dyn ThreadRunStore>;
        let mut mailbox = Self::new(executor, store, thread_run_store, consumer_id, config);
        mailbox.pending_thread_run_store = Some(pending_thread_run_store);
        mailbox
    }

    fn pending_thread_run_store(
        &self,
    ) -> Result<&Arc<dyn awaken_stores::PendingThreadRunStore>, MailboxError> {
        self.pending_thread_run_store.as_ref().ok_or_else(|| {
            MailboxError::Internal(
                "pending thread-run store is not configured for this mailbox".to_string(),
            )
        })
    }

    pub async fn deliver(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, MailboxError> {
        let store = self.pending_thread_run_store()?;
        let normalized = normalize_message_ids(messages);
        Ok(store
            .append_pending_message_records(thread_id, &normalized, delivery_mode)
            .await?)
    }

    pub async fn freeze_pending(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, MailboxError> {
        let store = self.pending_thread_run_store()?;
        Ok(store
            .freeze_pending_message_records(thread_id, boundary, expected_message_version)
            .await?)
    }

    pub(super) async fn prepare_pending_new_run_for_dispatch(
        &self,
        request: &RunActivation,
        thread_id: &str,
        normalized_messages: &[Message],
        run_id: &str,
        record: &mut RunRecord,
        manifest: &PinnedRegistryManifest,
    ) -> Result<Option<String>, MailboxError> {
        let Some(store) = self.pending_thread_run_store.as_ref() else {
            return Ok(None);
        };
        if normalized_messages.is_empty() || request.trace.run_mode != RunMode::Scheduled {
            return Ok(None);
        }
        store
            .append_pending_message_records(
                thread_id,
                normalized_messages,
                DeliveryMode::new_run(DeliveryGranularity::Batch),
            )
            .await?;

        match self
            .prepare_pending_boundary_for_run(
                request,
                thread_id,
                DeliveryBoundary::NewRun,
                run_id,
                record,
                manifest,
            )
            .await?
        {
            Some(run_id) => Ok(Some(run_id)),
            None => Err(MailboxError::Internal(format!(
                "pending NewRun freeze found no eligible messages for thread '{thread_id}'"
            ))),
        }
    }

    pub(super) async fn prepare_pending_boundary_for_run(
        &self,
        request: &RunActivation,
        thread_id: &str,
        boundary: DeliveryBoundary,
        run_id: &str,
        record: &mut RunRecord,
        manifest: &PinnedRegistryManifest,
    ) -> Result<Option<String>, MailboxError> {
        let snapshot_template = request.snapshot(RunInputSnapshot::default(), manifest.clone());
        self.prepare_pending_boundary_snapshot_for_run(
            &snapshot_template,
            thread_id,
            boundary,
            run_id,
            record,
        )
        .await
        .map(|frozen| frozen.map(|_| run_id.to_string()))
    }

    pub(super) async fn prepare_pending_boundary_snapshot_for_run(
        &self,
        snapshot_template: &RunActivationSnapshot,
        thread_id: &str,
        boundary: DeliveryBoundary,
        run_id: &str,
        record: &mut RunRecord,
    ) -> Result<Option<Vec<MessageRecord>>, MailboxError> {
        let Some(store) = self.pending_thread_run_store.as_ref() else {
            return Ok(None);
        };
        for _ in 0..MAX_PENDING_FREEZE_ATTEMPTS {
            let existing_messages = store.load_messages(thread_id).await?.unwrap_or_default();
            let expected_version = existing_messages.len() as u64;
            let pending = store.load_pending_message_records(thread_id).await?;
            let selected_indexes = select_pending_for_freeze(&pending, boundary);
            if selected_indexes.is_empty() {
                return Ok(None);
            }
            let mut selected_pending_ids = Vec::with_capacity(selected_indexes.len());
            let mut trigger_message_ids = Vec::with_capacity(selected_indexes.len());
            for index in selected_indexes {
                let record = &pending[index];
                selected_pending_ids.push(record.pending_id.clone());
                let Some(message_id) = record.message.id.clone() else {
                    return Err(MailboxError::Internal(format!(
                        "pending message '{}' has no message id",
                        record.pending_id
                    )));
                };
                trigger_message_ids.push(message_id);
            }

            let first_new_seq = expected_version + 1;
            let last_new_seq = expected_version + selected_pending_ids.len() as u64;
            let (input_snapshot, input) =
                build_run_input(thread_id, last_new_seq, &trigger_message_ids);
            let mut snapshot = snapshot_template.clone();
            snapshot.input = input_snapshot;
            record.activation = Some(snapshot);
            record.input = input;
            record.updated_at = now_ms() / 1000;

            let frozen = match store
                .freeze_pending_message_records_with_run(
                    thread_id,
                    boundary,
                    Some(expected_version),
                    &selected_pending_ids,
                    record,
                )
                .await
            {
                Ok(frozen) => frozen,
                Err(StorageError::VersionConflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            let mut appended_messages = existing_messages;
            appended_messages.extend(frozen.iter().map(|record| record.message.clone()));
            self.record_thread_message_checkpoint_events(
                thread_id,
                run_id,
                &appended_messages,
                first_new_seq,
                last_new_seq,
            )
            .await;
            self.refresh_worker_checkpoint_cache(thread_id, &appended_messages, record)
                .await;
            return Ok(Some(frozen));
        }

        Err(MailboxError::Internal(format!(
            "pending {boundary:?} freeze exhausted {MAX_PENDING_FREEZE_ATTEMPTS} retries under version conflict for thread '{thread_id}'"
        )))
    }

    pub(super) fn pending_boundary_handler(
        self: &Arc<Self>,
        request: &RunActivation,
        run_id: &str,
        manifest: &PinnedRegistryManifest,
    ) -> Option<Arc<dyn PendingBoundaryHandler>> {
        self.pending_thread_run_store.as_ref()?;
        let snapshot_template = request.snapshot(RunInputSnapshot::default(), manifest.clone());
        Some(Arc::new(MailboxPendingBoundaryHandler {
            mailbox: Arc::clone(self),
            thread_id: request.thread_id().to_string(),
            run_id: run_id.to_string(),
            snapshot_template,
        }))
    }
}

struct MailboxPendingBoundaryHandler {
    mailbox: Arc<Mailbox>,
    thread_id: String,
    run_id: String,
    snapshot_template: RunActivationSnapshot,
}

#[async_trait::async_trait]
impl PendingBoundaryHandler for MailboxPendingBoundaryHandler {
    async fn freeze_pending_boundary(
        &self,
        boundary: DeliveryBoundary,
    ) -> Result<Option<PendingBoundaryFreeze>, AgentLoopError> {
        let mut record = self
            .mailbox
            .thread_run_store()
            .load_run(&self.run_id)
            .await
            .map_err(|error| AgentLoopError::StorageError(error.to_string()))?
            .ok_or_else(|| {
                AgentLoopError::StorageError(format!(
                    "run '{}' not found while freezing pending {boundary:?}",
                    self.run_id
                ))
            })?;
        let frozen = self
            .mailbox
            .prepare_pending_boundary_snapshot_for_run(
                &self.snapshot_template,
                &self.thread_id,
                boundary,
                &self.run_id,
                &mut record,
            )
            .await
            .map_err(|error| AgentLoopError::StorageError(error.to_string()))?;
        Ok(frozen.map(|records| PendingBoundaryFreeze {
            messages: records.into_iter().map(|record| record.message).collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
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

        let request =
            RunActivation::new("thread-handler", Vec::new()).with_run_id_hint("run-handler");
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

        assert_eq!(
            result.status,
            crate::mailbox::MailboxDispatchStatus::Running
        );
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
}
