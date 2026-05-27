//! In-process retry of checkpoint events that failed to publish after a freeze
//! commit. The freeze transaction is already durable; the canonical/outbox
//! checkpoint events are advisory projections published on a separate path.
//! Rather than waiting for the next process restart (startup
//! `repair_thread_message_checkpoint_events`), a failed publish is queued here
//! and retried on each maintenance sweep. Re-publishing is idempotent — the same
//! invariant startup repair already relies on — so a dropped or retried task
//! cannot duplicate events downstream.

use super::Mailbox;

/// A committed message range whose checkpoint events failed to publish and must
/// be re-published.
#[derive(Debug, Clone)]
pub(crate) struct CheckpointRepairTask {
    pub thread_id: String,
    pub run_id: String,
    pub first_seq: u64,
    pub last_seq: u64,
}

impl Mailbox {
    /// Bounded cap on the retry queue. Overflow drops the oldest task; startup
    /// repair is the durable backstop for anything dropped.
    const CHECKPOINT_REPAIR_QUEUE_CAP: usize = 1024;

    /// Queue a committed range whose checkpoint events failed to publish. No-op
    /// when no publisher is configured.
    pub(super) fn enqueue_checkpoint_repair(&self, task: CheckpointRepairTask) {
        if self.server_event_publisher.is_none() {
            return;
        }
        let mut queue = self
            .checkpoint_repair_queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.len() >= Self::CHECKPOINT_REPAIR_QUEUE_CAP {
            // Overflow: drop the oldest. Startup repair is the durable backstop,
            // but surface the drop so a sustained publisher outage (queue
            // perpetually full) is observable instead of only manifesting later
            // as missing downstream projections.
            if let Some(dropped) = queue.pop_front() {
                crate::metrics::inc_mailbox_operation_by("checkpoint_repair_queue", "dropped", 1);
                tracing::warn!(
                    thread_id = %dropped.thread_id,
                    run_id = %dropped.run_id,
                    cap = Self::CHECKPOINT_REPAIR_QUEUE_CAP,
                    "checkpoint repair queue full; dropped oldest task (startup repair remains backstop)"
                );
            }
        }
        queue.push_back(task);
    }

    /// Retry queued checkpoint-event repairs (called from the maintenance sweep).
    /// A task that still fails is re-queued (bounded) for a later sweep.
    pub(super) async fn drain_checkpoint_repair_queue(&self) {
        if self.server_event_publisher.is_none() {
            return;
        }
        let tasks: Vec<CheckpointRepairTask> = {
            let mut queue = self
                .checkpoint_repair_queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.drain(..).collect()
        };
        for task in tasks {
            let messages = match self.run_store.load_messages(&task.thread_id).await {
                Ok(messages) => messages.unwrap_or_default(),
                Err(error) => {
                    tracing::warn!(
                        thread_id = %task.thread_id,
                        run_id = %task.run_id,
                        error = %error,
                        "checkpoint repair retry could not load messages; re-queued"
                    );
                    self.enqueue_checkpoint_repair(task);
                    continue;
                }
            };
            if let Err(error) = self
                .record_thread_message_checkpoint_events(
                    &task.thread_id,
                    &task.run_id,
                    &messages,
                    task.first_seq,
                    task.last_seq,
                )
                .await
            {
                tracing::warn!(
                    thread_id = %task.thread_id,
                    run_id = %task.run_id,
                    error = %error,
                    "checkpoint repair retry failed; re-queued for next sweep"
                );
                self.enqueue_checkpoint_repair(task);
            }
        }
    }
}
