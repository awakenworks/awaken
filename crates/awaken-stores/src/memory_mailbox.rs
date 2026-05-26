//! In-memory implementation of the new lease-based `MailboxStore`.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::{
    LiveCommandReceipt, LiveDeliveryOutcome, LiveRunCommand, LiveRunCommandEntry,
    LiveRunCommandStream, LiveRunTarget, MailboxInterrupt, MailboxInterruptDetails, MailboxStore,
    RunDispatch, RunDispatchResult, RunDispatchStatus,
};
use awaken_contract::contract::storage::StorageError;
use tokio::sync::{RwLock, mpsc, oneshot};
use uuid::Uuid;

use crate::mailbox_state;

/// Per-thread dispatch epoch for interrupt semantics.
struct MailboxState {
    current_dispatch_epoch: u64,
}

/// In-memory `MailboxStore` for testing and local development.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
/// Data lives only in memory and is lost when the store is dropped.
#[derive(Default)]
pub struct InMemoryMailboxStore {
    dispatches: RwLock<HashMap<String, RunDispatch>>,
    state: RwLock<HashMap<String, MailboxState>>,
    /// Single-consumer live-channel: one forwarder per thread at a time.
    /// Re-opening replaces the previous sender so the stale forwarder sees
    /// the channel close.
    live: RwLock<HashMap<String, mpsc::UnboundedSender<LiveRunCommandEntry>>>,
}

/// How long a producer waits for the consumer to ack an in-memory live
/// command before falling back to durable dispatch. Short by design — the
/// in-process forwarder only needs to execute a `try_send` to ack.
const LIVE_ACK_TIMEOUT: Duration = Duration::from_millis(500);

/// Oneshot-backed receipt: `ack` sends `()` on the paired receiver, which
/// unblocks the producer's `deliver_live` await.
struct OneshotReceipt(oneshot::Sender<()>);

impl LiveCommandReceipt for OneshotReceipt {
    fn ack(self: Box<Self>) {
        let _ = self.0.send(());
    }
}

impl InMemoryMailboxStore {
    /// Create a new empty in-memory mailbox store.
    pub fn new() -> Self {
        Self::default()
    }

    fn current_epoch_from_state(state: &HashMap<String, MailboxState>, thread_id: &str) -> u64 {
        state
            .get(thread_id)
            .map(|state| state.current_dispatch_epoch)
            .unwrap_or(0)
    }

    fn live_key_for_thread(thread_id: &str) -> String {
        format!("thread:{thread_id}")
    }

    fn live_key_for_target(target: &LiveRunTarget) -> String {
        match target.dispatch_id.as_deref() {
            Some(dispatch_id) => format!(
                "thread:{}:run:{}:dispatch:{}",
                target.thread_id, target.run_id, dispatch_id
            ),
            None => format!("thread:{}:run:{}", target.thread_id, target.run_id),
        }
    }

    async fn deliver_live_key(
        &self,
        key: String,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        // Snapshot the sender without holding the map lock across await.
        let sender = {
            let map = self.live.read().await;
            match map.get(&key) {
                Some(sender) => sender.clone(),
                None => return Ok(LiveDeliveryOutcome::NoSubscriber),
            }
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        let receipt: Box<dyn LiveCommandReceipt> = Box::new(OneshotReceipt(ack_tx));
        if sender
            .send(LiveRunCommandEntry {
                command: cmd,
                receipt,
            })
            .is_err()
        {
            // Receiver dropped — forwarder is gone.
            let mut map = self.live.write().await;
            if let Some(current) = map.get(&key)
                && current.is_closed()
            {
                map.remove(&key);
            }
            return Ok(LiveDeliveryOutcome::NoSubscriber);
        }
        // Wait for the consumer to ack (i.e. to successfully hand the
        // command off to the run). Timeout maps to `NoSubscriber` so the
        // caller falls back to the durable queue.
        match tokio::time::timeout(LIVE_ACK_TIMEOUT, ack_rx).await {
            Ok(Ok(())) => Ok(LiveDeliveryOutcome::Delivered),
            _ => Ok(LiveDeliveryOutcome::NoSubscriber),
        }
    }

    async fn open_live_key(&self, key: String) -> Result<LiveRunCommandStream, StorageError> {
        let (tx, rx) = mpsc::unbounded_channel::<LiveRunCommandEntry>();
        self.live.write().await.insert(key, tx);
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|entry| (entry, rx))
        });
        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl MailboxStore for InMemoryMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let mut state = self.state.write().await;

        // Dedupe check: reject if dedupe_key matches an existing non-terminal dispatch.
        if let Some(ref dk) = dispatch.dedupe_key {
            let duplicate = dispatches.values().any(|j| {
                j.thread_id == dispatch.thread_id
                    && j.dedupe_key.as_deref() == Some(dk)
                    && !j.status.is_terminal()
            });
            if duplicate {
                return Err(StorageError::AlreadyExists(format!("dedupe_key={dk}")));
            }
        }

        // Auto-create MailboxState if needed, get current dispatch epoch.
        let ms = state
            .entry(dispatch.thread_id.clone())
            .or_insert(MailboxState {
                current_dispatch_epoch: 0,
            });

        let mut dispatch = dispatch.clone();
        dispatch.dispatch_epoch = ms.current_dispatch_epoch;
        dispatch.status = RunDispatchStatus::Queued;
        dispatch.dispatch_instance_id = None;
        dispatch.run_status = None;
        dispatch.termination = None;
        dispatch.run_response = None;
        dispatch.run_error = None;
        dispatch.completed_at = None;

        dispatches.insert(dispatch.dispatch_id.clone(), dispatch);
        Ok(())
    }

    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, thread_id)
        };

        for dispatch in dispatches.values_mut() {
            if dispatch.thread_id == thread_id
                && dispatch.status == RunDispatchStatus::Queued
                && dispatch.dispatch_epoch < current_epoch
            {
                dispatch.dispatch_epoch = current_epoch;
                mailbox_state::mark_superseded(
                    dispatch,
                    now,
                    Some(mailbox_state::REASON_QUEUED_SUPERSEDED_BY_EPOCH),
                );
            }
        }

        // Same thread must not have two Claimed dispatches concurrently.
        let has_claimed = dispatches
            .values()
            .any(|j| j.thread_id == thread_id && j.status == RunDispatchStatus::Claimed);
        if has_claimed {
            return Ok(vec![]);
        }

        // Collect eligible dispatch IDs, sorted by (priority ASC, created_at ASC).
        let mut eligible: Vec<&String> = dispatches
            .iter()
            .filter(|(_, j)| {
                j.thread_id == thread_id
                    && j.status == RunDispatchStatus::Queued
                    && j.available_at <= now
            })
            .map(|(id, _)| id)
            .collect();

        // Sort: need to access dispatch data for sorting.
        eligible.sort_by(|a, b| {
            let ja = &dispatches[*a];
            let jb = &dispatches[*b];
            ja.priority
                .cmp(&jb.priority)
                .then(ja.created_at.cmp(&jb.created_at))
        });

        eligible.truncate(limit);
        let ids: Vec<String> = eligible.into_iter().cloned().collect();

        let token = Uuid::now_v7().to_string();
        let mut claimed = Vec::with_capacity(ids.len());

        for id in ids {
            let dispatch = dispatches
                .get_mut(&id)
                .ok_or_else(|| StorageError::NotFound(id.clone()))?;
            dispatch.status = RunDispatchStatus::Claimed;
            dispatch.claim_token = Some(token.clone());
            dispatch.claimed_by = Some(consumer_id.to_string());
            dispatch.lease_until = Some(now + lease_ms);
            dispatch.updated_at = now;
            claimed.push(dispatch.clone());
        }

        Ok(claimed)
    }

    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let thread_id = match dispatches.get(dispatch_id) {
            Some(j) if j.status == RunDispatchStatus::Queued => j.thread_id.clone(),
            _ => return Ok(None),
        };
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &thread_id)
        };
        if let Some(dispatch) = dispatches.get_mut(dispatch_id)
            && dispatch.dispatch_epoch < current_epoch
        {
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_QUEUED_SUPERSEDED_BY_EPOCH),
            );
            return Ok(None);
        }
        let has_other_claimed = dispatches.values().any(|j| {
            j.thread_id == thread_id
                && j.dispatch_id != dispatch_id
                && j.status == RunDispatchStatus::Claimed
        });
        if has_other_claimed {
            return Ok(None);
        }

        // Re-borrow after the shared check above.
        // SAFETY: dispatch_id was already found via `get_mut` above, so this cannot fail.
        let dispatch = dispatches
            .get_mut(dispatch_id)
            .ok_or_else(|| StorageError::Io("dispatch disappeared during claim".into()))?;
        let token = Uuid::now_v7().to_string();
        dispatch.status = RunDispatchStatus::Claimed;
        dispatch.claim_token = Some(token);
        dispatch.claimed_by = Some(consumer_id.to_string());
        dispatch.lease_until = Some(now + lease_ms);
        dispatch.updated_at = now;

        Ok(Some(dispatch.clone()))
    }

    async fn ack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = dispatches
            .get_mut(dispatch_id)
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        if dispatch.dispatch_epoch < current_epoch {
            let stale_epoch = dispatch.dispatch_epoch;
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_CLAIMED_SUPERSEDED_BY_EPOCH),
            );
            return Err(StorageError::VersionConflict {
                expected: stale_epoch,
                actual: current_epoch,
            });
        }

        mailbox_state::mark_acked(dispatch, now);
        Ok(())
    }

    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = dispatches
            .get_mut(dispatch_id)
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.status != RunDispatchStatus::Claimed
            || dispatch.claim_token.as_deref() != Some(claim_token)
        {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        if dispatch.dispatch_epoch < current_epoch {
            let stale_epoch = dispatch.dispatch_epoch;
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_START),
            );
            return Err(StorageError::VersionConflict {
                expected: stale_epoch,
                actual: current_epoch,
            });
        }

        dispatch.dispatch_instance_id = Some(dispatch_instance_id.to_string());
        dispatch.run_status = Some(RunStatus::Running);
        dispatch.termination = None;
        dispatch.run_response = None;
        dispatch.run_error = None;
        dispatch.completed_at = None;
        dispatch.updated_at = now;
        Ok(())
    }

    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = dispatches
            .get_mut(dispatch_id)
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.status != RunDispatchStatus::Claimed
            || dispatch.claim_token.as_deref() != Some(claim_token)
        {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        if dispatch.dispatch_epoch < current_epoch {
            let stale_epoch = dispatch.dispatch_epoch;
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_RESULT),
            );
            return Err(StorageError::VersionConflict {
                expected: stale_epoch,
                actual: current_epoch,
            });
        }

        dispatch.dispatch_instance_id = Some(result.dispatch_instance_id.clone());
        dispatch.run_status = Some(result.status);
        dispatch.termination = result.termination.clone();
        dispatch.run_response = result.response.clone();
        dispatch.run_error = result.error.clone();
        dispatch.completed_at = Some(now);
        dispatch.updated_at = now;
        Ok(())
    }

    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = dispatches
            .get_mut(dispatch_id)
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        if dispatch.dispatch_epoch < current_epoch {
            let stale_epoch = dispatch.dispatch_epoch;
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_NACK),
            );
            return Err(StorageError::VersionConflict {
                expected: stale_epoch,
                actual: current_epoch,
            });
        }

        mailbox_state::mark_nack_result(dispatch, now, retry_at, error);

        Ok(())
    }

    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = dispatches
            .get_mut(dispatch_id)
            .ok_or_else(|| StorageError::NotFound(dispatch_id.to_string()))?;

        if dispatch.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        if dispatch.dispatch_epoch < current_epoch {
            let stale_epoch = dispatch.dispatch_epoch;
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_CLAIMED_SUPERSEDED_BEFORE_DEAD_LETTER),
            );
            return Err(StorageError::VersionConflict {
                expected: stale_epoch,
                actual: current_epoch,
            });
        }

        mailbox_state::mark_dead_letter(dispatch, now, error);
        Ok(())
    }

    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = match dispatches.get_mut(dispatch_id) {
            Some(j) if j.status == RunDispatchStatus::Queued => j,
            _ => return Ok(None),
        };

        mailbox_state::mark_cancelled(dispatch, now);
        Ok(Some(dispatch.clone()))
    }

    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        let mut dispatches = self.dispatches.write().await;

        let dispatch = match dispatches.get_mut(dispatch_id) {
            Some(j)
                if j.status == RunDispatchStatus::Claimed
                    && j.claim_token.as_deref() == Some(claim_token) =>
            {
                j
            }
            _ => return Ok(false),
        };
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        if dispatch.dispatch_epoch < current_epoch {
            dispatch.dispatch_epoch = current_epoch;
            mailbox_state::mark_superseded(
                dispatch,
                now,
                Some(mailbox_state::REASON_CLAIMED_SUPERSEDED_DURING_LEASE_RENEWAL),
            );
            return Ok(false);
        }

        dispatch.lease_until = Some(now + extension_ms);
        dispatch.updated_at = now;
        Ok(true)
    }

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        self.interrupt_detailed(thread_id, now)
            .await
            .map(Into::into)
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let mut state = self.state.write().await;

        let ms = state.entry(thread_id.to_string()).or_insert(MailboxState {
            current_dispatch_epoch: 0,
        });

        let old_dispatch_epoch = ms.current_dispatch_epoch;
        ms.current_dispatch_epoch += 1;
        let new_dispatch_epoch = ms.current_dispatch_epoch;

        let mut superseded_count = 0;
        let mut superseded_dispatches = Vec::new();
        let mut active_dispatch = None;

        for dispatch in dispatches.values_mut() {
            if dispatch.thread_id != thread_id {
                continue;
            }
            match dispatch.status {
                RunDispatchStatus::Queued if dispatch.dispatch_epoch <= old_dispatch_epoch => {
                    mailbox_state::mark_superseded(
                        dispatch,
                        now,
                        Some(mailbox_state::REASON_QUEUED_SUPERSEDED_BY_INTERRUPT),
                    );
                    superseded_count += 1;
                    superseded_dispatches.push(dispatch.clone());
                }
                RunDispatchStatus::Claimed => {
                    active_dispatch = Some(dispatch.clone());
                }
                _ => {}
            }
        }

        Ok(MailboxInterruptDetails {
            new_dispatch_epoch,
            active_dispatch,
            superseded_count,
            superseded_dispatches,
        })
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        let state = self.state.read().await;
        Ok(Self::current_epoch_from_state(&state, thread_id))
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let Some(dispatch) = dispatches.get_mut(dispatch_id) else {
            return Ok(None);
        };
        if dispatch.status != RunDispatchStatus::Claimed {
            return Ok(None);
        }
        if dispatch.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let current_epoch = {
            let state = self.state.read().await;
            Self::current_epoch_from_state(&state, &dispatch.thread_id)
        };
        dispatch.dispatch_epoch = dispatch.dispatch_epoch.max(current_epoch);
        mailbox_state::mark_superseded(dispatch, now, Some(reason));
        Ok(Some(dispatch.clone()))
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        let dispatches = self.dispatches.read().await;
        Ok(dispatches.get(dispatch_id).cloned())
    }

    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let dispatches = self.dispatches.read().await;

        let mut matched: Vec<&RunDispatch> = dispatches
            .values()
            .filter(|j| {
                j.thread_id == thread_id
                    && status_filter
                        .map(|sf| sf.contains(&j.status))
                        .unwrap_or(true)
            })
            .collect();

        matched.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(a.created_at.cmp(&b.created_at))
        });

        Ok(matched
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        let dispatches = self.dispatches.read().await;
        Ok(dispatches
            .values()
            .filter(|dispatch| dispatch.status == status)
            .count())
    }

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let dispatches = self.dispatches.read().await;
        let mut terminal = dispatches
            .values()
            .filter(|dispatch| dispatch.status.is_terminal())
            .cloned()
            .collect::<Vec<_>>();
        terminal.sort_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.dispatch_id.cmp(&b.dispatch_id))
        });
        Ok(terminal.into_iter().skip(offset).take(limit).collect())
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let state = self.state.read().await;

        let expired_ids: Vec<String> = dispatches
            .values()
            .filter(|j| {
                j.status == RunDispatchStatus::Claimed && j.lease_until.is_some_and(|lu| lu < now)
            })
            .take(limit)
            .map(|j| j.dispatch_id.clone())
            .collect();

        let mut reclaimed = Vec::with_capacity(expired_ids.len());

        for id in expired_ids {
            let dispatch = dispatches
                .get_mut(&id)
                .ok_or_else(|| StorageError::NotFound(id.clone()))?;
            let current_epoch = Self::current_epoch_from_state(&state, &dispatch.thread_id);
            if dispatch.dispatch_epoch < current_epoch {
                dispatch.dispatch_epoch = current_epoch;
                mailbox_state::mark_superseded(
                    dispatch,
                    now,
                    Some(mailbox_state::REASON_CLAIMED_LEASE_EXPIRED_AFTER_INTERRUPT),
                );
                continue;
            }
            mailbox_state::mark_expired_lease(dispatch, now);
            reclaimed.push(dispatch.clone());
        }

        Ok(reclaimed)
    }

    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let before = dispatches.len();
        dispatches.retain(|_, j| !(j.status.is_terminal() && j.updated_at < older_than));
        Ok(before - dispatches.len())
    }

    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        let dispatches = self.dispatches.read().await;
        let mut ids: Vec<String> = dispatches
            .values()
            .filter(|j| j.status == RunDispatchStatus::Queued)
            .map(|j| j.thread_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        ids.sort();
        Ok(ids)
    }

    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.deliver_live_key(Self::live_key_for_thread(thread_id), cmd)
            .await
    }

    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.deliver_live_key(Self::live_key_for_target(target), cmd)
            .await
    }

    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        // Single-consumer: a fresh `open_live_channel` replaces any prior
        // forwarder. In production there's one forwarder per thread at a
        // time (enforced by `ActiveRunRegistry`); tests that drive multiple
        // subscribers should be written against `NatsMailboxStore` or an
        // intentional fanout backend.
        self.open_live_key(Self::live_key_for_thread(thread_id))
            .await
    }

    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.open_live_key(Self::live_key_for_target(target)).await
    }
}

#[cfg(test)]
#[path = "memory_mailbox/tests.rs"]
mod tests;
