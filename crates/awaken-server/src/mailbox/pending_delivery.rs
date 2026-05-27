use std::sync::Arc;

use awaken_contract::contract::mailbox::MailboxStore;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryGranularity, DeliveryMode, Message, MessageRecord,
    PendingMessageRecord, select_pending_for_freeze_for_run,
};
use awaken_contract::contract::run::{RunActivationSnapshot, RunInputSnapshot};
use awaken_contract::contract::storage::{
    PinnedRegistryManifest, RunMessageInput, RunRecord, StorageError, ThreadRunStore,
};
use awaken_contract::contract::tool_intercept::RunMode;
use awaken_contract::now_ms;
use awaken_runtime::RunActivation;
use awaken_runtime::loop_runner::{AgentLoopError, PendingBoundaryFreeze, PendingBoundaryHandler};

use super::Mailbox;
use super::helpers::{build_run_input, normalize_message_ids};
use super::{IntoDispatchExecutor, MailboxConfig, MailboxError};

const MAX_PENDING_FREEZE_ATTEMPTS: usize = 8;

fn delivery_mode_for_dispatch(boundary: DeliveryBoundary, run_id: &str) -> DeliveryMode {
    match boundary {
        DeliveryBoundary::ResumeInput => {
            DeliveryMode::resume_input(DeliveryGranularity::Batch, run_id)
        }
        _ => DeliveryMode {
            boundary,
            granularity: DeliveryGranularity::Batch,
            barrier: false,
            target_run_id: None,
            fallback_to_new_run: true,
        },
    }
}

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

    pub(super) async fn deliver(
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

    #[cfg(test)]
    pub(crate) async fn freeze_pending(
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

    pub async fn update_pending_message(
        &self,
        thread_id: &str,
        pending_id: &str,
        message: Message,
    ) -> Result<PendingMessageRecord, MailboxError> {
        self.update_pending_message_checked(thread_id, pending_id, None, message)
            .await
    }

    pub async fn update_pending_message_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
        message: Message,
    ) -> Result<PendingMessageRecord, MailboxError> {
        let store = self.pending_thread_run_store()?;
        Ok(store
            .update_pending_message_record_checked(
                thread_id,
                pending_id,
                expected_revision,
                message,
            )
            .await?)
    }

    pub async fn retract_pending_message(
        &self,
        thread_id: &str,
        pending_id: &str,
    ) -> Result<PendingMessageRecord, MailboxError> {
        self.retract_pending_message_checked(thread_id, pending_id, None)
            .await
    }

    pub async fn retract_pending_message_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<PendingMessageRecord, MailboxError> {
        let store = self.pending_thread_run_store()?;
        Ok(store
            .retract_pending_message_record_checked(thread_id, pending_id, expected_revision)
            .await?)
    }

    pub async fn reorder_pending_messages(
        &self,
        thread_id: &str,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, MailboxError> {
        self.reorder_pending_messages_checked(thread_id, None, ordered_pending_ids)
            .await
    }

    pub async fn reorder_pending_messages_checked(
        &self,
        thread_id: &str,
        expected_queue_revision: Option<u64>,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, MailboxError> {
        let store = self.pending_thread_run_store()?;
        Ok(store
            .reorder_pending_message_records_checked(
                thread_id,
                expected_queue_revision,
                ordered_pending_ids,
            )
            .await?)
    }

    pub(super) async fn prepare_pending_messages_for_dispatch(
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
        if normalized_messages.is_empty() {
            return Ok(None);
        }
        let boundary = match request.trace.run_mode {
            RunMode::Foreground => DeliveryBoundary::Interrupt,
            RunMode::Scheduled => DeliveryBoundary::NewRun,
            // Internal wake carries no user input; never stage pending.
            RunMode::InternalWake => return Ok(None),
            // A genuine HITL decision resume (seeded decisions) carries no fresh
            // user input and must not stage pending. But a fresh user submit that
            // was auto-routed to a reusable waiting run (Resume with no seeded
            // decisions) is user input and must stage through pending so it stays
            // editable/retractable until consumed; it continues the waiting run
            // via a targeted resume boundary so unrelated queued NewRun pending
            // remains available for the next dispatch instead of being folded
            // into the waiting run.
            RunMode::Resume => {
                if request.control.seeded_decisions.is_empty() {
                    DeliveryBoundary::ResumeInput
                } else {
                    return Ok(None);
                }
            }
        };
        let appended = store
            .append_pending_message_records(
                thread_id,
                normalized_messages,
                delivery_mode_for_dispatch(boundary, run_id),
            )
            .await?;
        let appended_pending_ids = appended
            .iter()
            .map(|record| record.pending_id.clone())
            .collect::<Vec<_>>();

        let freeze_result = self
            .prepare_pending_boundary_for_run(
                request, thread_id, boundary, run_id, record, manifest,
            )
            .await;
        match freeze_result {
            Ok(Some(run_id)) => Ok(Some(run_id)),
            Ok(None) => {
                self.cleanup_appended_pending_messages(store, thread_id, &appended_pending_ids)
                    .await;
                Err(MailboxError::Internal(format!(
                    "pending {boundary:?} freeze found no eligible messages for thread '{thread_id}'"
                )))
            }
            Err(error) => {
                self.cleanup_appended_pending_messages(store, thread_id, &appended_pending_ids)
                    .await;
                Err(error)
            }
        }
    }

    pub(super) async fn cleanup_appended_pending_messages(
        &self,
        store: &Arc<dyn awaken_stores::PendingThreadRunStore>,
        thread_id: &str,
        pending_ids: &[String],
    ) {
        for pending_id in pending_ids {
            match store
                .retract_pending_message_record(thread_id, pending_id)
                .await
            {
                Ok(_) => {}
                Err(StorageError::NotFound(_)) => {}
                Err(StorageError::Validation(message)) if message.contains("already consumed") => {}
                Err(error) => {
                    tracing::warn!(
                        thread_id,
                        pending_id,
                        error = %error,
                        "failed to clean up pending message after freeze failure"
                    );
                }
            }
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
        // Capture the originally persisted prior input once, before any attempt
        // mutates `record`. Each retry must merge trigger ids against this
        // original prior — never against a record a failed attempt already
        // mutated — otherwise a VersionConflict retry re-merges the failed
        // attempt's ids as "prior" and accumulates phantom trigger ids that were
        // never frozen (ADR-0042 D4: one run drains pending over several turns,
        // but only successfully frozen ids belong in RunRecord.input).
        let original_prior_trigger_ids = record
            .input
            .as_ref()
            .map(|prior| prior.trigger_message_ids.clone())
            .unwrap_or_default();
        for _ in 0..MAX_PENDING_FREEZE_ATTEMPTS {
            let existing_messages = store
                .load_committed_messages(thread_id)
                .await?
                .unwrap_or_default();
            let expected_version = existing_messages.len() as u64;
            let pending = store.load_pending_message_records(thread_id).await?;
            let selected_indexes =
                select_pending_for_freeze_for_run(&pending, boundary, Some(run_id));
            if selected_indexes.is_empty() {
                return Ok(None);
            }
            let mut selected_pending_ids = Vec::with_capacity(selected_indexes.len());
            let mut trigger_message_ids = Vec::with_capacity(selected_indexes.len());
            for index in selected_indexes {
                let pending_record = &pending[index];
                selected_pending_ids.push(pending_record.pending_id.clone());
                let Some(message_id) = pending_record.message.id.clone() else {
                    return Err(MailboxError::Internal(format!(
                        "pending message '{}' has no message id",
                        pending_record.pending_id
                    )));
                };
                trigger_message_ids.push(message_id);
            }

            let first_new_seq = expected_version + 1;
            let last_new_seq = expected_version + selected_pending_ids.len() as u64;
            let (mut input_snapshot, _) =
                build_run_input(thread_id, last_new_seq, &trigger_message_ids);
            // Accumulate consumed triggers across multiple boundary freezes on the
            // same run: one run may drain pending over several turns (ADR-0042 D4),
            // so RunRecord.input must record the full consumed input, not just the
            // last freeze. The range already spans from seq 1 to the latest seq.
            // Merge against the original prior captured before the loop so a
            // retried attempt does not duplicate ids from a prior failed attempt.
            let mut merged = original_prior_trigger_ids.clone();
            for id in &input_snapshot.trigger_message_ids {
                if !merged.contains(id) {
                    merged.push(id.clone());
                }
            }
            input_snapshot.trigger_message_ids = merged;
            let input = Some(RunMessageInput {
                thread_id: input_snapshot.thread_id.clone(),
                range: input_snapshot.range,
                trigger_message_ids: input_snapshot.trigger_message_ids.clone(),
                selected_message_ids: input_snapshot.selected_message_ids.clone(),
                context_policy: input_snapshot.context_policy.clone(),
                compacted_snapshot_id: input_snapshot.compacted_snapshot_id.clone(),
            });
            let mut snapshot = snapshot_template.clone();
            snapshot.input = input_snapshot;
            // Mutate a throwaway clone, not the caller's record. On
            // VersionConflict the caller's `record` must stay byte-for-byte
            // unchanged so the next attempt re-derives from the original prior.
            let mut attempt_record = record.clone();
            attempt_record.activation = Some(snapshot);
            attempt_record.input = input;
            attempt_record.updated_at = now_ms() / 1000;

            let frozen = match store
                .freeze_pending_message_records_with_run(
                    thread_id,
                    boundary,
                    Some(expected_version),
                    &selected_pending_ids,
                    &attempt_record,
                )
                .await
            {
                Ok(frozen) => frozen,
                Err(
                    StorageError::VersionConflict { .. }
                    | StorageError::PendingSelectionConflict { .. },
                ) => continue,
                Err(error) => return Err(error.into()),
            };
            // Freeze committed durably; only now adopt the attempt's mutations
            // into the caller's record.
            *record = attempt_record;
            let mut appended_messages = existing_messages;
            appended_messages.extend(frozen.iter().map(|record| record.message.clone()));
            // The freeze transaction has already committed messages + run
            // record. Checkpoint events are repairable advisory projections
            // published through a separate outbox path; failing the caller here
            // would report a false negative and invite duplicate user-message
            // retries. Startup repair re-derives missing checkpoint events from
            // committed run records.
            if let Err(error) = self
                .record_thread_message_checkpoint_events(
                    thread_id,
                    run_id,
                    &appended_messages,
                    first_new_seq,
                    last_new_seq,
                )
                .await
            {
                tracing::warn!(
                thread_id,
                run_id,
                error = %error,
                "repairable checkpoint event publish failed after pending freeze commit"
                );
            }
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
    async fn stage_pending_messages(
        &self,
        boundary: DeliveryBoundary,
        messages: Vec<Message>,
    ) -> Result<(), AgentLoopError> {
        if messages.is_empty() {
            return Ok(());
        }
        self.mailbox
            .deliver(
                &self.thread_id,
                &messages,
                DeliveryMode {
                    boundary,
                    granularity: DeliveryGranularity::Batch,
                    barrier: false,
                    target_run_id: Some(self.run_id.clone()),
                    fallback_to_new_run: false,
                },
            )
            .await
            .map_err(|error| AgentLoopError::StorageError(error.to_string()))?;
        Ok(())
    }

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
#[path = "pending_delivery/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pending_delivery_tests.rs"]
mod pending_delivery_tests;

#[cfg(test)]
#[path = "pending_delivery_lane_tests.rs"]
mod pending_delivery_lane_tests;
