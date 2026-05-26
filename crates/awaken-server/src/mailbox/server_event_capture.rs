use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, CanonicalEventKind, EventScope, EventVisibility,
};
use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_contract::contract::message::{Message, Role, Visibility};
use awaken_contract::contract::storage::{RunQuery, RunRecord, StorageError};
use awaken_contract::contract::suspension::ToolCallResume;
use serde_json::json;

use super::{Mailbox, MailboxError, dispatch_status_label};

impl Mailbox {
    pub(super) async fn record_mailbox_dispatch_event(
        &self,
        event_kind: &'static str,
        dispatch: &RunDispatch,
    ) {
        self.record_mailbox_dispatch_event_inner(event_kind, dispatch, None, None, None)
            .await;
    }

    pub(super) async fn record_mailbox_timeout(
        &self,
        dispatch: &RunDispatch,
        reason: &'static str,
        timeout_ms: u64,
    ) {
        self.record_mailbox_dispatch_event_inner(
            "MailboxTimeout",
            dispatch,
            Some(reason),
            None,
            Some(timeout_ms),
        )
        .await;
    }

    pub(super) async fn record_mailbox_submit_failed(
        &self,
        dispatch: &RunDispatch,
        error: &StorageError,
    ) {
        self.record_mailbox_dispatch_event_inner(
            "MailboxSubmitFailed",
            dispatch,
            Some("enqueue_failed"),
            Some(error.to_string()),
            None,
        )
        .await;
    }

    pub(super) async fn record_run_errored(&self, dispatch: &RunDispatch, error: &str) {
        let Some(publisher) = &self.server_event_publisher else {
            return;
        };
        let origin = self.server_event_origin.clone();
        let payload = match serde_json::to_value(AgentEvent::RunFinish {
            thread_id: dispatch.thread_id.clone(),
            run_id: dispatch.run_id.clone(),
            identity: None,
            result: None,
            termination: awaken_contract::contract::lifecycle::TerminationReason::Error(
                error.to_string(),
            ),
        }) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(error = %error, dispatch_id = %dispatch.dispatch_id, "invalid run errored event payload");
                return;
            }
        };
        let mut draft = match CanonicalEventDraft::new(
            vec![
                EventScope::thread(dispatch.thread_id.clone()),
                EventScope::run(dispatch.run_id.clone()),
            ],
            match CanonicalEventKind::new("RunErrored") {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(error = %error, "invalid run errored event kind");
                    return;
                }
            },
            payload,
            origin.clone(),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                tracing::error!(error = %error, dispatch_id = %dispatch.dispatch_id, "invalid run errored event draft");
                return;
            }
        };
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(dispatch.dispatch_id.clone());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "RunErrored/{}/{}",
                dispatch.dispatch_id, dispatch.attempt_count
            )),
            expected_prior_cursors: Default::default(),
        };
        if let Err(error) = publisher.publish(draft, options).await {
            tracing::error!(error = %error, dispatch_id = %dispatch.dispatch_id, "failed to record run errored event");
        }
    }

    pub(super) async fn record_mailbox_resume_failed(
        &self,
        dispatch: &RunDispatch,
        decisions: &[(String, ToolCallResume)],
        reason: &'static str,
        error: &StorageError,
    ) {
        let Some(publisher) = &self.server_event_publisher else {
            return;
        };
        let origin = self.server_event_origin.clone();
        let decisions = decisions
            .iter()
            .map(|(tool_call_id, resume)| {
                json!({
                    "tool_call_id": tool_call_id,
                    "decision_id": resume.decision_id,
                    "action": &resume.action,
                    "result": &resume.result,
                    "resume_updated_at": resume.updated_at,
                })
            })
            .collect::<Vec<_>>();
        let payload = json!({
            "thread_id": dispatch.thread_id,
            "run_id": dispatch.run_id,
            "dispatch_id": dispatch.dispatch_id,
            "dispatch_epoch": dispatch.dispatch_epoch,
            "attempt_count": dispatch.attempt_count,
            "status": dispatch_status_label(dispatch.status),
            "reason": reason,
            "error": error.to_string(),
            "decisions": decisions,
        });
        let mut draft = match CanonicalEventDraft::new(
            vec![
                EventScope::thread(dispatch.thread_id.clone()),
                EventScope::run(dispatch.run_id.clone()),
            ],
            match CanonicalEventKind::new("MailboxResumeFailed") {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(error = %error, "invalid mailbox resume failed event kind");
                    return;
                }
            },
            payload,
            origin.clone(),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                tracing::error!(error = %error, dispatch_id = %dispatch.dispatch_id, "invalid mailbox resume failed event draft");
                return;
            }
        };
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(dispatch.dispatch_id.clone());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "MailboxResumeFailed/{}/{}",
                dispatch.dispatch_id, dispatch.attempt_count
            )),
            expected_prior_cursors: Default::default(),
        };
        if let Err(error) = publisher.publish(draft, options).await {
            tracing::error!(error = %error, dispatch_id = %dispatch.dispatch_id, "failed to record mailbox resume failed event");
        }
    }

    pub(super) async fn record_run_rescheduled_dispatch(
        &self,
        dispatch: &RunDispatch,
        reason: &'static str,
    ) {
        if dispatch.status == RunDispatchStatus::Queued {
            self.record_mailbox_dispatch_event_inner(
                "RunRescheduled",
                dispatch,
                Some(reason),
                None,
                None,
            )
            .await;
        }
    }

    pub(super) async fn record_run_rescheduled_dispatch_by_id(
        &self,
        dispatch_id: &str,
        reason: &'static str,
    ) {
        match self.store.load_dispatch(dispatch_id).await {
            Ok(Some(dispatch)) => {
                self.record_run_rescheduled_dispatch(&dispatch, reason)
                    .await;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(dispatch_id, error = %error, "failed to load rescheduled dispatch");
            }
        }
    }

    async fn record_mailbox_dispatch_event_inner(
        &self,
        event_kind: &'static str,
        dispatch: &RunDispatch,
        reason: Option<&'static str>,
        error: Option<String>,
        timeout_ms: Option<u64>,
    ) {
        let Some(publisher) = &self.server_event_publisher else {
            return;
        };
        let origin = self.server_event_origin.clone();
        let mut payload = json!({
            "thread_id": dispatch.thread_id,
            "run_id": dispatch.run_id,
            "dispatch_id": dispatch.dispatch_id,
            "dispatch_epoch": dispatch.dispatch_epoch,
            "attempt_count": dispatch.attempt_count,
            "status": dispatch_status_label(dispatch.status),
            "available_at": dispatch.available_at,
            "created_at": dispatch.created_at,
            "updated_at": dispatch.updated_at,
        });
        if let Some(reason) = reason
            && let Some(payload) = payload.as_object_mut()
        {
            payload.insert("reason".to_string(), json!(reason));
        }
        if let Some(error) = error
            && let Some(payload) = payload.as_object_mut()
        {
            payload.insert("error".to_string(), json!(error));
        }
        if let Some(timeout_ms) = timeout_ms
            && let Some(payload) = payload.as_object_mut()
        {
            payload.insert("timeout_ms".to_string(), json!(timeout_ms));
        }
        let mut draft = match CanonicalEventDraft::new(
            vec![
                EventScope::thread(dispatch.thread_id.clone()),
                EventScope::run(dispatch.run_id.clone()),
            ],
            match CanonicalEventKind::new(event_kind) {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(error = %error, event_kind, "invalid mailbox event kind");
                    return;
                }
            },
            payload,
            origin.clone(),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                tracing::error!(error = %error, event_kind, dispatch_id = %dispatch.dispatch_id, "invalid mailbox event draft");
                return;
            }
        };
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(dispatch.dispatch_id.clone());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "{}/{}/{}",
                event_kind, dispatch.dispatch_id, dispatch.attempt_count
            )),
            expected_prior_cursors: Default::default(),
        };
        if let Err(error) = publisher.publish(draft, options).await {
            tracing::error!(error = %error, event_kind, dispatch_id = %dispatch.dispatch_id, "failed to record mailbox event");
        }
    }

    /// Append the canonical checkpoint events (one `MessageCommitted` per new
    /// seq plus one `ThreadMessagesCheckpointed`) for a freeze/commit.
    ///
    /// ADR-0042 D4 requires messages + run record + canonical events to share a
    /// logical boundary. The freeze transaction commits messages + run
    /// atomically in the store crate, but these events are published through the
    /// advisory outbox publisher, which that transaction cannot reach. Callers
    /// that have already committed state must treat failures here as repairable:
    /// `repair_thread_message_checkpoint_events` re-derives missing events from
    /// committed run records.
    pub(super) async fn record_thread_message_checkpoint_events(
        &self,
        thread_id: &str,
        run_id: &str,
        messages: &[Message],
        first_new_seq: u64,
        last_new_seq: u64,
    ) -> Result<(), MailboxError> {
        if first_new_seq > last_new_seq {
            return Ok(());
        }
        for seq in first_new_seq..=last_new_seq {
            self.record_message_committed(thread_id, run_id, messages, seq)
                .await?;
        }
        self.record_thread_messages_checkpointed(
            thread_id,
            run_id,
            messages,
            first_new_seq,
            last_new_seq,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn repair_thread_message_checkpoint_events(
        &self,
    ) -> Result<usize, MailboxError> {
        if self.server_event_publisher.is_none() {
            return Ok(0);
        }

        let mut offset = 0usize;
        let limit = 100usize;
        let mut repaired = 0usize;
        loop {
            let page = self
                .run_store
                .list_runs(&RunQuery {
                    offset,
                    limit,
                    thread_id: None,
                    status: None,
                })
                .await?;
            for run in &page.items {
                let Some(input) = &run.input else {
                    continue;
                };
                let Some(range) = input.range else {
                    continue;
                };
                let messages = self
                    .run_store
                    .load_messages(&input.thread_id)
                    .await?
                    .unwrap_or_default();
                self.record_thread_message_checkpoint_events(
                    &input.thread_id,
                    &run.run_id,
                    &messages,
                    range.from_seq,
                    range.to_seq,
                )
                .await?;
                repaired += range.len() as usize;
            }
            if !page.has_more {
                break;
            }
            offset += limit;
        }
        Ok(repaired)
    }

    pub(crate) async fn record_mailbox_decision_received_for_run(
        &self,
        run: &RunRecord,
        tool_call_id: &str,
        resume: &ToolCallResume,
        delivery_path: &'static str,
    ) {
        self.record_mailbox_decision_received(
            &run.thread_id,
            &run.run_id,
            run.dispatch_id.as_deref(),
            tool_call_id,
            resume,
            delivery_path,
        )
        .await;
    }

    pub(super) async fn record_mailbox_decision_received_for_dispatch(
        &self,
        dispatch: &RunDispatch,
        tool_call_id: &str,
        resume: &ToolCallResume,
        delivery_path: &'static str,
    ) {
        self.record_mailbox_decision_received(
            &dispatch.thread_id,
            &dispatch.run_id,
            Some(&dispatch.dispatch_id),
            tool_call_id,
            resume,
            delivery_path,
        )
        .await;
    }

    pub(super) async fn record_mailbox_decision_received_for_id(
        &self,
        id: &str,
        tool_call_id: &str,
        resume: &ToolCallResume,
        delivery_path: &'static str,
    ) {
        match self.store.load_dispatch(id).await {
            Ok(Some(dispatch)) => {
                self.record_mailbox_decision_received_for_dispatch(
                    &dispatch,
                    tool_call_id,
                    resume,
                    delivery_path,
                )
                .await;
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(id, error = %error, "failed to load dispatch for mailbox decision event");
            }
        }
        let run = match self.run_store.load_run(id).await {
            Ok(Some(run)) => Some(run),
            Ok(None) => match self.run_store.latest_run(id).await {
                Ok(run) => run,
                Err(error) => {
                    tracing::warn!(id, error = %error, "failed to load latest run for mailbox decision event");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(id, error = %error, "failed to load run for mailbox decision event");
                None
            }
        };
        if let Some(run) = run {
            self.record_mailbox_decision_received_for_run(
                &run,
                tool_call_id,
                resume,
                delivery_path,
            )
            .await;
        }
    }

    async fn record_mailbox_decision_received(
        &self,
        thread_id: &str,
        run_id: &str,
        dispatch_id: Option<&str>,
        tool_call_id: &str,
        resume: &ToolCallResume,
        delivery_path: &'static str,
    ) {
        let Some(publisher) = &self.server_event_publisher else {
            return;
        };
        let origin = self.server_event_origin.clone();
        let payload = json!({
            "thread_id": thread_id,
            "run_id": run_id,
            "dispatch_id": dispatch_id,
            "tool_call_id": tool_call_id,
            "decision_id": resume.decision_id,
            "action": &resume.action,
            "result": &resume.result,
            "reason": &resume.reason,
            "resume_updated_at": resume.updated_at,
            "delivery_path": delivery_path,
        });
        let mut draft = match CanonicalEventDraft::new(
            vec![
                EventScope::thread(thread_id.to_string()),
                EventScope::run(run_id.to_string()),
            ],
            match CanonicalEventKind::new("MailboxDecisionReceived") {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(error = %error, "invalid mailbox decision event kind");
                    return;
                }
            },
            payload,
            origin.clone(),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                tracing::error!(error = %error, run_id, tool_call_id, "invalid mailbox decision event draft");
                return;
            }
        };
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(resume.decision_id.clone());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "MailboxDecisionReceived/{run_id}/{tool_call_id}/{}",
                resume.decision_id
            )),
            expected_prior_cursors: Default::default(),
        };
        if let Err(error) = publisher.publish(draft, options).await {
            tracing::error!(error = %error, run_id, tool_call_id, "failed to record mailbox decision event");
        }
        self.record_tool_permission_resolved(
            thread_id,
            run_id,
            dispatch_id,
            tool_call_id,
            resume,
            delivery_path,
        )
        .await;
    }

    async fn record_tool_permission_resolved(
        &self,
        thread_id: &str,
        run_id: &str,
        dispatch_id: Option<&str>,
        tool_call_id: &str,
        resume: &ToolCallResume,
        delivery_path: &'static str,
    ) {
        let Some(approved) = resume
            .result
            .get("approved")
            .and_then(serde_json::Value::as_bool)
        else {
            return;
        };
        let Some(publisher) = &self.server_event_publisher else {
            return;
        };
        let origin = self.server_event_origin.clone();
        let payload = json!({
            "thread_id": thread_id,
            "run_id": run_id,
            "dispatch_id": dispatch_id,
            "tool_call_id": tool_call_id,
            "decision_id": resume.decision_id,
            "action": &resume.action,
            "approved": approved,
            "result": &resume.result,
            "reason": &resume.reason,
            "resume_updated_at": resume.updated_at,
            "delivery_path": delivery_path,
        });
        let mut draft = match CanonicalEventDraft::new(
            vec![
                EventScope::thread(thread_id.to_string()),
                EventScope::run(run_id.to_string()),
            ],
            match CanonicalEventKind::new("ToolPermissionResolved") {
                Ok(kind) => kind,
                Err(error) => {
                    tracing::error!(error = %error, "invalid tool permission resolved event kind");
                    return;
                }
            },
            payload,
            origin.clone(),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                tracing::error!(error = %error, run_id, tool_call_id, "invalid tool permission resolved event draft");
                return;
            }
        };
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(resume.decision_id.clone());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "ToolPermissionResolved/{run_id}/{tool_call_id}/{}",
                resume.decision_id
            )),
            expected_prior_cursors: Default::default(),
        };
        if let Err(error) = publisher.publish(draft, options).await {
            tracing::error!(error = %error, run_id, tool_call_id, "failed to record tool permission resolved event");
        }
    }

    async fn record_message_committed(
        &self,
        thread_id: &str,
        run_id: &str,
        messages: &[Message],
        seq: u64,
    ) -> Result<(), MailboxError> {
        let Some(publisher) = &self.server_event_publisher else {
            return Ok(());
        };
        let origin = self.server_event_origin.clone();
        let Some(message) = seq
            .checked_sub(1)
            .and_then(|index| messages.get(index as usize))
        else {
            tracing::warn!(
                thread_id,
                run_id,
                seq,
                "message checkpoint event sequence is out of range"
            );
            return Ok(());
        };
        let Some(message_id) = message.id.as_deref().filter(|id| !id.trim().is_empty()) else {
            tracing::warn!(
                thread_id,
                run_id,
                seq,
                "message checkpoint event missing message id"
            );
            return Ok(());
        };
        let parent_message_id = seq
            .checked_sub(2)
            .and_then(|index| messages.get(index as usize))
            .and_then(|message| message.id.clone());
        let payload = json!({
            "thread_id": thread_id,
            "run_id": run_id,
            "message_id": message_id,
            "message_seq": seq,
            "role": message.role,
            "content_blocks": &message.content,
            "message_kind": message_kind(message),
            "parent_message_id": parent_message_id,
        });
        let kind = CanonicalEventKind::new("MessageCommitted").map_err(|error| {
            tracing::error!(error = %error, "invalid message committed event kind");
            MailboxError::Internal(format!("invalid message committed event kind: {error}"))
        })?;
        let mut draft = CanonicalEventDraft::new(
            vec![
                EventScope::thread(thread_id.to_string()),
                EventScope::run(run_id.to_string()),
            ],
            kind,
            payload,
            origin.clone(),
        )
        .map_err(|error| {
            tracing::error!(error = %error, thread_id, run_id, message_id, "invalid message committed event draft");
            MailboxError::Internal(format!("invalid message committed event draft: {error}"))
        })?;
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(run_id.to_string());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "MessageCommitted/{thread_id}/{run_id}/{message_id}"
            )),
            expected_prior_cursors: Default::default(),
        };
        publisher.publish(draft, options).await.map_err(|error| {
            tracing::error!(error = %error, thread_id, run_id, message_id, "failed to record message committed event");
            MailboxError::Internal(format!("failed to record message committed event: {error}"))
        })?;
        Ok(())
    }

    async fn record_thread_messages_checkpointed(
        &self,
        thread_id: &str,
        run_id: &str,
        messages: &[Message],
        first_new_seq: u64,
        last_new_seq: u64,
    ) -> Result<(), MailboxError> {
        let Some(publisher) = &self.server_event_publisher else {
            return Ok(());
        };
        let origin = self.server_event_origin.clone();
        let message_ids = (first_new_seq..=last_new_seq)
            .filter_map(|seq| {
                seq.checked_sub(1)
                    .and_then(|index| messages.get(index as usize))
                    .and_then(|message| message.id.clone())
            })
            .collect::<Vec<_>>();
        let payload = json!({
            "thread_id": thread_id,
            "run_id": run_id,
            "message_seq_start": first_new_seq,
            "message_seq_end": last_new_seq,
            "message_count": message_ids.len(),
            "message_ids": message_ids,
        });
        let kind = CanonicalEventKind::new("ThreadMessagesCheckpointed").map_err(|error| {
            tracing::error!(error = %error, "invalid thread messages checkpoint event kind");
            MailboxError::Internal(format!(
                "invalid thread messages checkpoint event kind: {error}"
            ))
        })?;
        let mut draft = CanonicalEventDraft::new(
            vec![
                EventScope::thread(thread_id.to_string()),
                EventScope::run(run_id.to_string()),
            ],
            kind,
            payload,
            origin.clone(),
        )
        .map_err(|error| {
            tracing::error!(error = %error, thread_id, run_id, "invalid thread messages checkpoint event draft");
            MailboxError::Internal(format!(
                "invalid thread messages checkpoint event draft: {error}"
            ))
        })?;
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = Some(run_id.to_string());
        let options = AppendOptions {
            writer_id: Some("mailbox".to_string()),
            idempotency_key: Some(format!(
                "ThreadMessagesCheckpointed/{thread_id}/{run_id}/{first_new_seq}-{last_new_seq}"
            )),
            expected_prior_cursors: Default::default(),
        };
        publisher.publish(draft, options).await.map_err(|error| {
            tracing::error!(error = %error, thread_id, run_id, "failed to record thread messages checkpoint event");
            MailboxError::Internal(format!(
                "failed to record thread messages checkpoint event: {error}"
            ))
        })?;
        Ok(())
    }
}

fn message_kind(message: &Message) -> &'static str {
    match (message.role, message.visibility) {
        (Role::User, Visibility::All) => "user_input",
        (Role::User, Visibility::Internal) => "internal_user_input",
        (Role::Assistant, _) => "assistant_output",
        (Role::Tool, _) => "tool_result",
        (Role::System, Visibility::All) => "system",
        (Role::System, Visibility::Internal) => "internal_system",
    }
}
