//! Live message delivery + miscellaneous query helpers for `Mailbox`.

use tokio::sync::mpsc;

use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::mailbox::{
    LiveDeliveryOutcome, LiveRunCommand, RunDispatch, RunDispatchStatus,
};
use awaken_server_contract::contract::message::{DeliveryGranularity, DeliveryMode, Message};

use super::{
    Mailbox, MailboxDispatchStatus, MailboxError, MailboxSubmitResult, MailboxWorkerStatus,
    live_target_for_run,
};

impl Mailbox {
    pub(super) async fn try_deliver_live_messages(
        &self,
        thread_id: &str,
        expected_run_id: Option<&str>,
        messages: Vec<Message>,
    ) -> Result<Option<MailboxSubmitResult>, MailboxError> {
        if messages.is_empty() {
            return Ok(None);
        }

        let local_active = {
            let workers = self.workers.read().await;
            workers.get(thread_id).and_then(|worker| {
                let worker = worker.lock();
                match &worker.status {
                    MailboxWorkerStatus::Running {
                        dispatch_id,
                        run_id,
                        ..
                    } => Some((dispatch_id.clone(), run_id.clone())),
                    MailboxWorkerStatus::Idle | MailboxWorkerStatus::Claiming => None,
                }
            })
        };

        if let Some((active_dispatch_id, active_run_id)) = local_active {
            // Race guard against a run that just rolled over.
            if expected_run_id.is_some_and(|expected| expected != active_run_id) {
                return Ok(None);
            }
            if self.pending_thread_run_store.is_some() {
                let appended = self
                    .deliver(
                        thread_id,
                        &messages,
                        DeliveryMode::next_step(DeliveryGranularity::Batch)
                            .targeted_to_run(&active_run_id, false),
                    )
                    .await?;
                if !self.executor.wake_pending_boundary(&active_run_id) {
                    if let Some(store) = self.pending_thread_run_store.as_ref() {
                        let pending_ids = appended
                            .iter()
                            .map(|record| record.pending_id.clone())
                            .collect::<Vec<_>>();
                        self.cleanup_appended_pending_messages(store, thread_id, &pending_ids)
                            .await;
                    }
                    return Ok(None);
                }
                return Ok(Some(MailboxSubmitResult {
                    dispatch_id: active_dispatch_id,
                    run_id: active_run_id,
                    thread_id: thread_id.to_string(),
                    status: MailboxDispatchStatus::Running,
                }));
            }
            // Local fast path: executor has a direct handle to the run's
            // inbox and returns a boolean indicating whether the channel
            // accepted the payload. A `false` here means the local run
            // just ended — fall back to durable dispatch.
            if !self.executor.send_messages(&active_run_id, messages) {
                return Ok(None);
            }
            return Ok(Some(MailboxSubmitResult {
                dispatch_id: active_dispatch_id,
                run_id: active_run_id,
                thread_id: thread_id.to_string(),
                status: MailboxDispatchStatus::Running,
            }));
        }

        // No local worker: check whether another node is running this
        // thread. `ThreadRunStore::latest_run` is the global truth (every
        // node checkpoints to the same store).
        let Some(remote_run) = self.run_store.latest_run(thread_id).await? else {
            return Ok(None);
        };
        if remote_run.status != RunStatus::Running {
            return Ok(None);
        }
        if expected_run_id.is_some_and(|expected| expected != remote_run.run_id) {
            return Ok(None);
        }

        if self.pending_thread_run_store.is_some() {
            let dispatch_id = remote_run
                .dispatch_id
                .clone()
                .unwrap_or_else(|| remote_run.run_id.clone());
            let target = live_target_for_run(&remote_run);
            let run_id = remote_run.run_id.clone();
            let appended = self
                .deliver(
                    thread_id,
                    &messages,
                    DeliveryMode::next_step(DeliveryGranularity::Batch)
                        .targeted_to_run(&run_id, false),
                )
                .await?;
            let outcome = self
                .store
                .deliver_live_to(&target, LiveRunCommand::PendingBoundaryWake)
                .await?;
            if matches!(outcome, LiveDeliveryOutcome::NoSubscriber) {
                if let Some(store) = self.pending_thread_run_store.as_ref() {
                    let pending_ids = appended
                        .iter()
                        .map(|record| record.pending_id.clone())
                        .collect::<Vec<_>>();
                    self.cleanup_appended_pending_messages(store, thread_id, &pending_ids)
                        .await;
                }
                return Ok(None);
            }
            return Ok(Some(MailboxSubmitResult {
                dispatch_id,
                run_id,
                thread_id: thread_id.to_string(),
                status: MailboxDispatchStatus::Running,
            }));
        }

        // Cross-node: ask the store to deliver. If the owning node's
        // forwarder isn't subscribed yet, `deliver_live` reports
        // `NoSubscriber` and we fall through so `submit_live_then_queue`
        // enqueues a durable dispatch instead of silently dropping.
        let outcome = self
            .store
            .deliver_live_to(
                &live_target_for_run(&remote_run),
                LiveRunCommand::Messages(messages),
            )
            .await?;
        match outcome {
            LiveDeliveryOutcome::Delivered => {}
            LiveDeliveryOutcome::NoSubscriber => {
                return Ok(None);
            }
        }

        let dispatch_id = remote_run
            .dispatch_id
            .clone()
            .unwrap_or_else(|| remote_run.run_id.clone());
        Ok(Some(MailboxSubmitResult {
            dispatch_id,
            run_id: remote_run.run_id,
            thread_id: thread_id.to_string(),
            status: MailboxDispatchStatus::Running,
        }))
    }

    /// Reconnect the event sink for an active (suspended) run.
    ///
    /// Replaces the underlying channel sender so subsequent events flow to
    /// `new_tx`. Returns `true` if the thread has an active worker.
    pub async fn reconnect_sink(&self, thread_id: &str, new_tx: mpsc::Sender<AgentEvent>) -> bool {
        let workers = self.workers.read().await;
        let Some(worker) = workers.get(thread_id) else {
            return false;
        };
        let w = worker.lock();
        match &w.status {
            MailboxWorkerStatus::Running { sink, .. } => {
                sink.reconnect(new_tx);
                true
            }
            MailboxWorkerStatus::Idle | MailboxWorkerStatus::Claiming => false,
        }
    }

    pub(super) async fn reusable_waiting_run_id(&self, thread_id: &str) -> Option<String> {
        if let Some(thread) = self.run_store.load_thread(thread_id).await.ok().flatten()
            && let Some(open_run_id) = thread.open_run_id.as_deref()
            && let Some(run) = self.run_store.load_run(open_run_id).await.ok().flatten()
            && run.thread_id == thread_id
            && run.is_resumable_waiting()
        {
            return Some(run.run_id);
        }
        let run = self.run_store.latest_run(thread_id).await.ok().flatten()?;
        run.is_resumable_waiting().then_some(run.run_id)
    }

    // ── Query ────────────────────────────────────────────────────────

    /// List mailbox dispatches for a thread (with optional status filter).
    pub async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, MailboxError> {
        Ok(self
            .store
            .list_dispatches(thread_id, status_filter, limit, offset)
            .await?)
    }

    /// List thread IDs that currently have queued dispatches.
    pub async fn queued_thread_ids(&self) -> Result<Vec<String>, MailboxError> {
        Ok(self.store.queued_thread_ids().await?)
    }

    pub async fn load_dispatch(
        &self,
        dispatch_id: &str,
    ) -> Result<Option<RunDispatch>, MailboxError> {
        Ok(self.store.load_dispatch(dispatch_id).await?)
    }
}
