//! In-memory implementation of the new lease-based `MailboxStore`.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::{
    LiveCommandReceipt, LiveDeliveryOutcome, LiveRunCommand, LiveRunCommandEntry,
    LiveRunCommandStream, LiveRunTarget, MailboxInterrupt, MailboxStore, RunDispatch,
    RunDispatchResult, RunDispatchStatus,
};
use awaken_contract::contract::storage::StorageError;
use tokio::sync::{RwLock, mpsc, oneshot};
use uuid::Uuid;

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

        dispatch.status = RunDispatchStatus::Acked;
        dispatch.updated_at = now;
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

        dispatch.attempt_count += 1;
        dispatch.last_error = Some(error.to_string());
        dispatch.updated_at = now;

        if dispatch.attempt_count >= dispatch.max_attempts {
            dispatch.status = RunDispatchStatus::DeadLetter;
        } else {
            dispatch.status = RunDispatchStatus::Queued;
            dispatch.available_at = retry_at;
            dispatch.claim_token = None;
            dispatch.claimed_by = None;
            dispatch.lease_until = None;
        }

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

        dispatch.status = RunDispatchStatus::DeadLetter;
        dispatch.last_error = Some(error.to_string());
        dispatch.updated_at = now;
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

        dispatch.status = RunDispatchStatus::Cancelled;
        dispatch.updated_at = now;
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

        dispatch.lease_until = Some(now + extension_ms);
        dispatch.updated_at = now;
        Ok(true)
    }

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        let mut dispatches = self.dispatches.write().await;
        let mut state = self.state.write().await;

        let ms = state.entry(thread_id.to_string()).or_insert(MailboxState {
            current_dispatch_epoch: 0,
        });

        let old_dispatch_epoch = ms.current_dispatch_epoch;
        ms.current_dispatch_epoch += 1;
        let new_dispatch_epoch = ms.current_dispatch_epoch;

        let mut superseded_count = 0;
        let mut active_dispatch = None;

        for dispatch in dispatches.values_mut() {
            if dispatch.thread_id != thread_id {
                continue;
            }
            match dispatch.status {
                RunDispatchStatus::Queued if dispatch.dispatch_epoch <= old_dispatch_epoch => {
                    dispatch.status = RunDispatchStatus::Superseded;
                    dispatch.updated_at = now;
                    superseded_count += 1;
                }
                RunDispatchStatus::Claimed => {
                    active_dispatch = Some(dispatch.clone());
                }
                _ => {}
            }
        }

        Ok(MailboxInterrupt {
            new_dispatch_epoch,
            active_dispatch,
            superseded_count,
        })
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

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let mut dispatches = self.dispatches.write().await;

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
            dispatch.attempt_count += 1;
            dispatch.updated_at = now;

            if dispatch.attempt_count >= dispatch.max_attempts {
                dispatch.status = RunDispatchStatus::DeadLetter;
            } else {
                dispatch.status = RunDispatchStatus::Queued;
                dispatch.claim_token = None;
                dispatch.claimed_by = None;
                dispatch.lease_until = None;
            }
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
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_dispatch(thread_id: &str, agent_id: &str) -> RunDispatch {
        RunDispatch {
            dispatch_id: Uuid::now_v7().to_string(),
            thread_id: thread_id.to_string(),
            run_id: format!("run-{agent_id}"),
            priority: 128,
            dedupe_key: None,
            dispatch_epoch: 0,
            status: RunDispatchStatus::Queued,
            available_at: 1000,
            attempt_count: 0,
            max_attempts: 5,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            dispatch_instance_id: None,
            run_status: None,
            termination: None,
            run_response: None,
            run_error: None,
            completed_at: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[tokio::test]
    async fn enqueue_and_list() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        store.enqueue(&dispatch).await.unwrap();

        let listed = store.list_dispatches("m-1", None, 100, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, RunDispatchStatus::Queued);
    }

    #[tokio::test]
    async fn claim_returns_queued_dispatch() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].dispatch_id, dispatch_id);
        assert_eq!(claimed[0].status, RunDispatchStatus::Claimed);
        assert!(claimed[0].claim_token.is_some());
    }

    #[tokio::test]
    async fn claim_respects_available_at() {
        let store = InMemoryMailboxStore::new();
        let mut dispatch = make_dispatch("m-1", "agent-1");
        dispatch.available_at = 5000; // future
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 10)
            .await
            .unwrap();
        assert!(claimed.is_empty());

        // Now advance time past available_at.
        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 5000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
    }

    #[tokio::test]
    async fn claim_limit() {
        let store = InMemoryMailboxStore::new();
        for _ in 0..3 {
            store
                .enqueue(&make_dispatch("m-1", "agent-1"))
                .await
                .unwrap();
        }

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
    }

    #[tokio::test]
    async fn claim_honors_batch_limit_without_active_claim() {
        let store = InMemoryMailboxStore::new();
        for _ in 0..3 {
            store
                .enqueue(&make_dispatch("m-1", "agent-1"))
                .await
                .unwrap();
        }

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 2)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(
            claimed
                .iter()
                .all(|dispatch| dispatch.status == RunDispatchStatus::Claimed)
        );
    }

    #[tokio::test]
    async fn claim_priority_ordering() {
        let store = InMemoryMailboxStore::new();

        let mut low = make_dispatch("m-1", "agent-1");
        low.priority = 200;
        low.created_at = 900;
        store.enqueue(&low).await.unwrap();

        let mut high = make_dispatch("m-1", "agent-1");
        high.priority = 10;
        high.created_at = 1000;
        store.enqueue(&high).await.unwrap();

        let mut mid = make_dispatch("m-1", "agent-1");
        mid.priority = 128;
        mid.created_at = 950;
        store.enqueue(&mid).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].priority, 10);
        let token = claimed[0].claim_token.clone().unwrap();
        store
            .ack(&claimed[0].dispatch_id, &token, 1100)
            .await
            .unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1200, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].priority, 128);
        let token = claimed[0].claim_token.clone().unwrap();
        store
            .ack(&claimed[0].dispatch_id, &token, 1300)
            .await
            .unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1400, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].priority, 200);
    }

    #[tokio::test]
    async fn ack_transitions_to_acked() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store.ack(&dispatch_id, &token, 2000).await.unwrap();

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunDispatchStatus::Acked);
    }

    #[tokio::test]
    async fn ack_rejects_wrong_claim_token() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        let result = store.ack(&dispatch_id, "wrong-token", 2000).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn records_dispatch_start_and_run_result_separately_from_ack() {
        use awaken_contract::contract::lifecycle::TerminationReason;

        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .record_dispatch_start(&dispatch_id, &token, "dispatch-1", 1500)
            .await
            .unwrap();
        let running = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(running.status, RunDispatchStatus::Claimed);
        assert_eq!(running.run_id, dispatch.run_id);
        assert_eq!(running.dispatch_instance_id.as_deref(), Some("dispatch-1"));
        assert_eq!(running.run_status, Some(RunStatus::Running));
        assert!(running.termination.is_none());
        assert!(running.completed_at.is_none());

        let result = RunDispatchResult {
            run_id: "run-1".into(),
            dispatch_instance_id: "dispatch-1".into(),
            status: RunStatus::Done,
            termination: Some(TerminationReason::NaturalEnd),
            response: Some("done".into()),
            error: None,
        };
        store
            .record_run_result(&dispatch_id, &token, &result, 1800)
            .await
            .unwrap();
        let completed = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(completed.status, RunDispatchStatus::Claimed);
        assert_eq!(completed.run_status, Some(RunStatus::Done));
        assert_eq!(completed.termination, Some(TerminationReason::NaturalEnd));
        assert_eq!(completed.run_response.as_deref(), Some("done"));
        assert_eq!(completed.completed_at, Some(1800));

        store.ack(&dispatch_id, &token, 2000).await.unwrap();
        let acked = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(acked.status, RunDispatchStatus::Acked);
        assert_eq!(acked.run_status, Some(RunStatus::Done));
        assert_eq!(acked.run_response.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn record_run_result_rejects_stale_claim_token() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        let result = RunDispatchResult {
            run_id: "run-1".into(),
            dispatch_instance_id: "dispatch-1".into(),
            status: RunStatus::Done,
            termination: None,
            response: None,
            error: Some("wrong token".into()),
        };
        assert!(
            store
                .record_run_result(&dispatch_id, "wrong-token", &result, 2000)
                .await
                .is_err()
        );

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert!(loaded.run_status.is_none());
        assert!(loaded.run_error.is_none());
    }

    #[tokio::test]
    async fn nack_returns_to_queued() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .nack(&dispatch_id, &token, 3000, "transient error", 2000)
            .await
            .unwrap();

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunDispatchStatus::Queued);
        assert_eq!(loaded.attempt_count, 1);
        assert_eq!(loaded.available_at, 3000);
        assert!(loaded.claim_token.is_none());
    }

    #[tokio::test]
    async fn nack_dead_letters_after_max_attempts() {
        let store = InMemoryMailboxStore::new();
        let mut dispatch = make_dispatch("m-1", "agent-1");
        dispatch.max_attempts = 1;
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .nack(&dispatch_id, &token, 3000, "final error", 2000)
            .await
            .unwrap();

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunDispatchStatus::DeadLetter);
    }

    #[tokio::test]
    async fn dead_letter_is_terminal() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .dead_letter(&dispatch_id, &token, "permanent failure", 2000)
            .await
            .unwrap();

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunDispatchStatus::DeadLetter);
        assert!(loaded.status.is_terminal());
    }

    #[tokio::test]
    async fn cancel_queued_dispatch() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let cancelled = store.cancel(&dispatch_id, 2000).await.unwrap();
        assert!(cancelled.is_some());
        assert_eq!(cancelled.unwrap().status, RunDispatchStatus::Cancelled);

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunDispatchStatus::Cancelled);
    }

    #[tokio::test]
    async fn extend_lease_success() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        let ok = store
            .extend_lease(&dispatch_id, &token, 60_000, 15_000)
            .await
            .unwrap();
        assert!(ok);

        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.lease_until, Some(75_000));
    }

    #[tokio::test]
    async fn extend_lease_wrong_token() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        let ok = store
            .extend_lease(&dispatch_id, "wrong-token", 60_000, 15_000)
            .await
            .unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn interrupt_supersedes_queued() {
        let store = InMemoryMailboxStore::new();
        store
            .enqueue(&make_dispatch("m-1", "agent-1"))
            .await
            .unwrap();
        store
            .enqueue(&make_dispatch("m-1", "agent-1"))
            .await
            .unwrap();

        let result = store.interrupt("m-1", 2000).await.unwrap();
        assert_eq!(result.new_dispatch_epoch, 1);
        assert_eq!(result.superseded_count, 2);
        assert!(result.active_dispatch.is_none());

        let listed = store
            .list_dispatches("m-1", Some(&[RunDispatchStatus::Superseded]), 100, 0)
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn interrupt_returns_active_claimed() {
        let store = InMemoryMailboxStore::new();
        let dispatch1 = make_dispatch("m-1", "agent-1");
        store.enqueue(&dispatch1).await.unwrap();

        // Claim the first dispatch.
        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        // Enqueue another.
        store
            .enqueue(&make_dispatch("m-1", "agent-1"))
            .await
            .unwrap();

        let result = store.interrupt("m-1", 2000).await.unwrap();
        assert!(result.active_dispatch.is_some());
        assert_eq!(
            result.active_dispatch.unwrap().status,
            RunDispatchStatus::Claimed
        );
        // The second (Queued) dispatch should be superseded.
        assert_eq!(result.superseded_count, 1);
    }

    #[tokio::test]
    async fn dedupe_key_rejects_duplicate() {
        let store = InMemoryMailboxStore::new();
        let mut dispatch1 = make_dispatch("m-1", "agent-1");
        dispatch1.dedupe_key = Some("unique-key".to_string());
        store.enqueue(&dispatch1).await.unwrap();

        let mut dispatch2 = make_dispatch("m-1", "agent-1");
        dispatch2.dedupe_key = Some("unique-key".to_string());
        let result = store.enqueue(&dispatch2).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reclaim_expired_leases() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        // Claim with a short lease.
        store
            .claim("m-1", "consumer-1", 100, 1000, 1)
            .await
            .unwrap();

        // Advance time past lease expiry (lease_until = 1100).
        let reclaimed = store.reclaim_expired_leases(2000, 10).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].dispatch_id, dispatch_id);
        assert_eq!(reclaimed[0].status, RunDispatchStatus::Queued);
        assert_eq!(reclaimed[0].attempt_count, 1);
    }

    #[tokio::test]
    async fn purge_terminal() {
        let store = InMemoryMailboxStore::new();

        // Create a dispatch, claim, and ack it (terminal).
        let dispatch = make_dispatch("m-1", "agent-1");
        store.enqueue(&dispatch).await.unwrap();
        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();
        store
            .ack(&claimed[0].dispatch_id, &token, 1500)
            .await
            .unwrap();

        // Create another non-terminal dispatch.
        store
            .enqueue(&make_dispatch("m-1", "agent-1"))
            .await
            .unwrap();

        // Purge terminal dispatches older than 2000.
        let purged = store.purge_terminal(2000).await.unwrap();
        assert_eq!(purged, 1);

        // The non-terminal dispatch should remain.
        let listed = store.list_dispatches("m-1", None, 100, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, RunDispatchStatus::Queued);
    }

    #[tokio::test]
    async fn queued_thread_ids() {
        let store = InMemoryMailboxStore::new();
        store
            .enqueue(&make_dispatch("m-1", "agent-1"))
            .await
            .unwrap();
        store
            .enqueue(&make_dispatch("m-2", "agent-1"))
            .await
            .unwrap();
        store
            .enqueue(&make_dispatch("m-3", "agent-1"))
            .await
            .unwrap();

        let ids = store.queued_thread_ids().await.unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"m-1".to_string()));
        assert!(ids.contains(&"m-2".to_string()));
        assert!(ids.contains(&"m-3".to_string()));
    }

    #[tokio::test]
    async fn claim_dispatch_by_id() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        let claimed = store
            .claim_dispatch(&dispatch_id, "consumer-1", 30_000, 1000)
            .await
            .unwrap();
        assert!(claimed.is_some());
        let claimed = claimed.unwrap();
        assert_eq!(claimed.dispatch_id, dispatch_id);
        assert_eq!(claimed.status, RunDispatchStatus::Claimed);
        assert!(claimed.claim_token.is_some());
    }

    #[tokio::test]
    async fn claim_skips_if_thread_already_has_claimed() {
        let store = InMemoryMailboxStore::new();
        let dispatch1 = make_dispatch("m-1", "agent-1");
        let dispatch2 = make_dispatch("m-1", "agent-1");
        store.enqueue(&dispatch1).await.unwrap();
        store.enqueue(&dispatch2).await.unwrap();

        // Claim first dispatch.
        let claimed = store.claim("m-1", "c-1", 30_000, 1000, 1).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // Second claim() should return empty — same thread already has Claimed.
        let claimed2 = store.claim("m-1", "c-1", 30_000, 1000, 1).await.unwrap();
        assert!(claimed2.is_empty());
    }

    #[tokio::test]
    async fn claim_dispatch_rejects_if_thread_already_has_claimed() {
        let store = InMemoryMailboxStore::new();
        let dispatch1 = make_dispatch("m-1", "agent-1");
        let dispatch2 = make_dispatch("m-1", "agent-1");
        let id1 = dispatch1.dispatch_id.clone();
        let id2 = dispatch2.dispatch_id.clone();
        store.enqueue(&dispatch1).await.unwrap();
        store.enqueue(&dispatch2).await.unwrap();

        // Claim first by ID.
        let claimed = store
            .claim_dispatch(&id1, "c-1", 30_000, 1000)
            .await
            .unwrap();
        assert!(claimed.is_some());

        // claim_dispatch for second should fail — same thread already has Claimed.
        let claimed2 = store
            .claim_dispatch(&id2, "c-1", 30_000, 1000)
            .await
            .unwrap();
        assert!(claimed2.is_none());
    }

    #[tokio::test]
    async fn claim_resumes_after_ack() {
        let store = InMemoryMailboxStore::new();
        let dispatch1 = make_dispatch("m-1", "agent-1");
        let dispatch2 = make_dispatch("m-1", "agent-1");
        store.enqueue(&dispatch1).await.unwrap();
        store.enqueue(&dispatch2).await.unwrap();

        // Claim first (whichever the store picks).
        let claimed = store.claim("m-1", "c-1", 30_000, 1000, 1).await.unwrap();
        assert_eq!(claimed.len(), 1);
        let claimed_id = claimed[0].dispatch_id.clone();
        let claimed_token = claimed[0].claim_token.clone().unwrap();

        // Ack the claimed dispatch → Acked.
        store.ack(&claimed_id, &claimed_token, 2000).await.unwrap();

        // Now claim should succeed for the other dispatch.
        let claimed2 = store.claim("m-1", "c-1", 30_000, 2000, 1).await.unwrap();
        assert_eq!(claimed2.len(), 1);
        assert_ne!(claimed2[0].dispatch_id, claimed_id);
    }

    // ── Concurrency & parallelism tests ─────────────────────────────

    #[tokio::test]
    async fn fifo_ordering_within_same_priority() {
        let store = InMemoryMailboxStore::new();

        // Enqueue 5 dispatches with identical priority but incrementing created_at.
        let mut dispatch_ids = Vec::new();
        for i in 0u64..5 {
            let mut dispatch = make_dispatch("thread-1", "agent-1");
            dispatch.priority = 0;
            dispatch.created_at = 1000 + i;
            dispatch.available_at = 1000;
            dispatch_ids.push(dispatch.dispatch_id.clone());
            store.enqueue(&dispatch).await.unwrap();
        }

        // Claim them one-by-one and verify FIFO order.
        let mut claimed_order = Vec::new();
        for _ in 0..5 {
            let claimed = store
                .claim("thread-1", "consumer-1", 30_000, 1000, 1)
                .await
                .unwrap();
            assert_eq!(claimed.len(), 1, "expected exactly 1 dispatch per claim");
            let dispatch = &claimed[0];
            claimed_order.push(dispatch.dispatch_id.clone());
            // Ack so it becomes terminal and won't be claimed again.
            store
                .ack(
                    &dispatch.dispatch_id,
                    dispatch.claim_token.as_ref().unwrap(),
                    2000,
                )
                .await
                .unwrap();
        }

        assert_eq!(
            claimed_order, dispatch_ids,
            "dispatches must be claimed in FIFO order"
        );
    }

    #[tokio::test]
    async fn concurrent_enqueue_no_lost_dispatches() {
        let store = std::sync::Arc::new(InMemoryMailboxStore::new());
        let mut handles = Vec::new();

        for i in 0..10 {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let mut dispatch = make_dispatch("thread-1", "agent-1");
                dispatch.dedupe_key = Some(format!("dedupe-{i}"));
                store.enqueue(&dispatch).await.unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let listed = store
            .list_dispatches("thread-1", None, 100, 0)
            .await
            .unwrap();
        assert_eq!(
            listed.len(),
            10,
            "all 10 concurrently enqueued dispatches must be present"
        );
    }

    #[tokio::test]
    async fn concurrent_claim_only_one_wins() {
        let store = std::sync::Arc::new(InMemoryMailboxStore::new());

        // Enqueue exactly 1 dispatch.
        let dispatch = make_dispatch("thread-1", "agent-1");
        let dispatch_id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        // Use a barrier so all tasks start claiming at roughly the same time.
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(10));
        let mut handles = Vec::new();

        for i in 0..10 {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .claim("thread-1", &format!("consumer-{i}"), 30_000, 1000, 1)
                    .await
                    .unwrap()
            }));
        }

        let mut winners = 0;
        let mut losers = 0;
        for h in handles {
            let claimed = h.await.unwrap();
            if claimed.is_empty() {
                losers += 1;
            } else {
                winners += 1;
                assert_eq!(claimed.len(), 1);
                assert_eq!(claimed[0].dispatch_id, dispatch_id);
            }
        }

        assert_eq!(winners, 1, "exactly one consumer must win the claim");
        assert_eq!(losers, 9, "the other 9 must get empty results");

        // Verify the dispatch has a single claim_token.
        let loaded = store.load_dispatch(&dispatch_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, RunDispatchStatus::Claimed);
        assert!(loaded.claim_token.is_some());
    }

    #[tokio::test]
    async fn claim_respects_per_thread_isolation() {
        let store = InMemoryMailboxStore::new();

        let dispatch1 = make_dispatch("thread-1", "agent-1");
        let dispatch1_id = dispatch1.dispatch_id.clone();
        store.enqueue(&dispatch1).await.unwrap();

        let dispatch2 = make_dispatch("thread-2", "agent-1");
        let dispatch2_id = dispatch2.dispatch_id.clone();
        store.enqueue(&dispatch2).await.unwrap();

        // Claim from thread-1.
        let claimed1 = store
            .claim("thread-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed1.len(), 1);
        assert_eq!(claimed1[0].dispatch_id, dispatch1_id);

        // Claim from thread-2 should succeed independently.
        let claimed2 = store
            .claim("thread-2", "consumer-2", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed2.len(), 1);
        assert_eq!(claimed2[0].dispatch_id, dispatch2_id);

        // Both are independently Claimed.
        let loaded1 = store.load_dispatch(&dispatch1_id).await.unwrap().unwrap();
        let loaded2 = store.load_dispatch(&dispatch2_id).await.unwrap().unwrap();
        assert_eq!(loaded1.status, RunDispatchStatus::Claimed);
        assert_eq!(loaded2.status, RunDispatchStatus::Claimed);
        assert_ne!(
            loaded1.claim_token, loaded2.claim_token,
            "each thread should get its own claim token"
        );
    }

    #[tokio::test]
    async fn claim_returns_only_one_per_call_with_limit_1() {
        let store = InMemoryMailboxStore::new();

        for _ in 0..3 {
            store
                .enqueue(&make_dispatch("thread-1", "agent-1"))
                .await
                .unwrap();
        }

        let claimed = store
            .claim("thread-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "limit=1 must return exactly 1 dispatch");

        // Verify remaining 2 are still Queued.
        let queued = store
            .list_dispatches("thread-1", Some(&[RunDispatchStatus::Queued]), 100, 0)
            .await
            .unwrap();
        assert_eq!(
            queued.len(),
            2,
            "remaining 2 dispatches must still be Queued"
        );
    }

    #[tokio::test]
    async fn concurrent_claim_dispatch_only_one_wins() {
        let inner = InMemoryMailboxStore::new();
        let dispatch1 = make_dispatch("m-1", "agent-1");
        let dispatch2 = make_dispatch("m-1", "agent-1");
        let id1 = dispatch1.dispatch_id.clone();
        let id2 = dispatch2.dispatch_id.clone();
        inner.enqueue(&dispatch1).await.unwrap();
        inner.enqueue(&dispatch2).await.unwrap();

        let store = Arc::new(inner);

        // Try to claim both by ID concurrently.
        let s1 = Arc::clone(&store);
        let s2 = Arc::clone(&store);
        let i1 = id1.clone();
        let i2 = id2.clone();
        let (r1, r2): (
            Result<Option<RunDispatch>, _>,
            Result<Option<RunDispatch>, _>,
        ) = tokio::join!(
            s1.claim_dispatch(&i1, "c-1", 30_000, 1000),
            s2.claim_dispatch(&i2, "c-1", 30_000, 1000),
        );

        let claimed_count = [r1.unwrap(), r2.unwrap()]
            .iter()
            .filter(|r| r.is_some())
            .count();
        assert_eq!(
            claimed_count, 1,
            "only one claim_dispatch should succeed for same thread"
        );
    }

    #[tokio::test]
    async fn claim_dispatch_different_thread_both_succeed() {
        let store = InMemoryMailboxStore::new();
        let dispatch1 = make_dispatch("m-1", "agent-1");
        let dispatch2 = make_dispatch("m-2", "agent-1");
        let id1 = dispatch1.dispatch_id.clone();
        let id2 = dispatch2.dispatch_id.clone();
        store.enqueue(&dispatch1).await.unwrap();
        store.enqueue(&dispatch2).await.unwrap();

        let r1 = store
            .claim_dispatch(&id1, "c-1", 30_000, 1000)
            .await
            .unwrap();
        let r2 = store
            .claim_dispatch(&id2, "c-1", 30_000, 1000)
            .await
            .unwrap();
        assert!(r1.is_some(), "different thread should succeed");
        assert!(r2.is_some(), "different thread should succeed");
    }

    #[tokio::test]
    async fn claim_after_nack_works() {
        let store = InMemoryMailboxStore::new();
        let dispatch = make_dispatch("m-1", "agent-1");
        let id = dispatch.dispatch_id.clone();
        store.enqueue(&dispatch).await.unwrap();

        // Claim then nack.
        let claimed = store
            .claim_dispatch(&id, "c-1", 30_000, 1000)
            .await
            .unwrap()
            .unwrap();
        let token = claimed.claim_token.unwrap();
        store.nack(&id, &token, 1000, "retry", 2000).await.unwrap();

        // Should be claimable again.
        let reclaimed = store.claim("m-1", "c-1", 30_000, 2000, 1).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
    }

    // ── Live-channel tests ──

    mod live_channel {
        use super::*;
        use awaken_contract::contract::mailbox::{LiveDeliveryOutcome, LiveRunCommand};
        use awaken_contract::contract::message::Message;
        use futures::StreamExt;
        use std::time::Duration;
        use tokio::time::timeout;

        /// Spawn a consumer task that drains the stream and auto-acks each
        /// entry, capturing the commands for assertions.
        fn spawn_auto_ack_consumer(
            mut stream: LiveRunCommandStream,
        ) -> (
            tokio::task::JoinHandle<()>,
            std::sync::Arc<tokio::sync::Mutex<Vec<LiveRunCommand>>>,
        ) {
            let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let captured_clone = captured.clone();
            let handle = tokio::spawn(async move {
                while let Some(entry) = stream.next().await {
                    captured_clone.lock().await.push(entry.command.clone());
                    entry.receipt.ack();
                }
            });
            (handle, captured)
        }

        #[tokio::test]
        async fn publish_reaches_subscriber_and_delivered_requires_ack() {
            let store = InMemoryMailboxStore::new();
            let stream = store.open_live_channel("t-1").await.unwrap();
            let (_consumer, captured) = spawn_auto_ack_consumer(stream);

            let outcome = store
                .deliver_live("t-1", LiveRunCommand::Messages(vec![Message::user("hi")]))
                .await
                .unwrap();
            assert_eq!(outcome, LiveDeliveryOutcome::Delivered);

            let commands = captured.lock().await.clone();
            assert_eq!(commands.len(), 1);
            match &commands[0] {
                LiveRunCommand::Messages(msgs) => assert_eq!(msgs[0].text(), "hi"),
                other => panic!("expected Messages, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn publish_without_subscriber_reports_no_subscriber() {
            let store = InMemoryMailboxStore::new();
            let outcome = store
                .deliver_live("t-2", LiveRunCommand::Cancel)
                .await
                .expect("deliver_live is infallible here");
            assert_eq!(outcome, LiveDeliveryOutcome::NoSubscriber);
        }

        #[tokio::test]
        async fn publish_after_subscriber_drop_reports_no_subscriber() {
            let store = InMemoryMailboxStore::new();
            {
                let _stream = store.open_live_channel("t-drop").await.unwrap();
                // subscriber dropped at scope exit
            }
            let outcome = store
                .deliver_live("t-drop", LiveRunCommand::Cancel)
                .await
                .expect("deliver_live is infallible here");
            assert_eq!(outcome, LiveDeliveryOutcome::NoSubscriber);
        }

        #[tokio::test]
        async fn consumer_that_drops_receipt_triggers_no_subscriber() {
            // Regression for issue #2: ack-after-forward guarantees that a
            // consumer which fails to hand off the command (drops receipt)
            // causes the producer to report NoSubscriber.
            let store = InMemoryMailboxStore::new();
            let mut stream = store.open_live_channel("t-nof").await.unwrap();

            let producer = tokio::spawn({
                let store = std::sync::Arc::new(store);
                let s = store.clone();
                async move {
                    s.deliver_live("t-nof", LiveRunCommand::Cancel)
                        .await
                        .unwrap()
                }
            });

            // Receive the entry, DO NOT ack — drop the receipt.
            let entry = timeout(Duration::from_millis(200), stream.next())
                .await
                .unwrap()
                .unwrap();
            drop(entry.receipt);

            let outcome = producer.await.unwrap();
            assert_eq!(outcome, LiveDeliveryOutcome::NoSubscriber);
        }

        #[tokio::test]
        async fn different_threads_isolated() {
            let store = InMemoryMailboxStore::new();
            let stream_a = store.open_live_channel("t-a").await.unwrap();
            let mut stream_b = store.open_live_channel("t-b").await.unwrap();
            let (_consumer_a, captured_a) = spawn_auto_ack_consumer(stream_a);

            store
                .deliver_live("t-a", LiveRunCommand::Cancel)
                .await
                .unwrap();
            assert_eq!(captured_a.lock().await.len(), 1);

            let got_b = timeout(Duration::from_millis(100), stream_b.next()).await;
            assert!(got_b.is_err(), "t-b must not receive t-a's command");
        }

        #[tokio::test]
        async fn reopen_replaces_previous_subscriber() {
            // Single-consumer semantics: opening a second channel on the
            // same thread invalidates the prior forwarder (its stream ends).
            let store = InMemoryMailboxStore::new();
            let mut old_stream = store.open_live_channel("t-replace").await.unwrap();
            let new_stream = store.open_live_channel("t-replace").await.unwrap();
            let (_consumer, captured) = spawn_auto_ack_consumer(new_stream);

            store
                .deliver_live("t-replace", LiveRunCommand::Cancel)
                .await
                .unwrap();

            // Old stream should be closed (sender replaced).
            let old = timeout(Duration::from_millis(100), old_stream.next()).await;
            assert!(
                matches!(old, Ok(None)),
                "old stream must close, got {old:?}"
            );
            assert_eq!(captured.lock().await.len(), 1);
        }

        #[tokio::test]
        async fn order_preserved_for_single_subscriber() {
            let store = InMemoryMailboxStore::new();
            let stream = store.open_live_channel("t-ord").await.unwrap();
            let (_consumer, captured) = spawn_auto_ack_consumer(stream);

            for i in 0..5 {
                store
                    .deliver_live(
                        "t-ord",
                        LiveRunCommand::Messages(vec![Message::user(format!("m-{i}"))]),
                    )
                    .await
                    .unwrap();
            }

            let captured = captured.lock().await;
            for (i, cmd) in captured.iter().enumerate() {
                match cmd {
                    LiveRunCommand::Messages(msgs) => {
                        assert_eq!(msgs[0].text(), format!("m-{i}"))
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        }

        #[tokio::test]
        async fn cmd_variants_all_delivered() {
            let store = InMemoryMailboxStore::new();
            let stream = store.open_live_channel("t-var").await.unwrap();
            let (_consumer, captured) = spawn_auto_ack_consumer(stream);

            store
                .deliver_live("t-var", LiveRunCommand::Messages(vec![Message::user("x")]))
                .await
                .unwrap();
            store
                .deliver_live("t-var", LiveRunCommand::Cancel)
                .await
                .unwrap();
            store
                .deliver_live("t-var", LiveRunCommand::Decision(vec![]))
                .await
                .unwrap();

            let captured = captured.lock().await;
            assert!(matches!(captured[0], LiveRunCommand::Messages(_)));
            assert!(matches!(captured[1], LiveRunCommand::Cancel));
            assert!(matches!(captured[2], LiveRunCommand::Decision(_)));
        }
    }

    // ── Property-based tests ──

    mod proptest_memory_mailbox {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn concurrent_claim_at_most_one_winner(
                num_claimers in 2usize..20,
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let store = Arc::new(InMemoryMailboxStore::new());
                    let dispatch = make_dispatch("test-thread", "agent-prop");
                    store.enqueue(&dispatch).await.unwrap();

                    let mut handles = vec![];
                    for i in 0..num_claimers {
                        let store = store.clone();
                        handles.push(tokio::spawn(async move {
                            store
                                .claim(
                                    "test-thread",
                                    &format!("consumer-{i}"),
                                    30_000,
                                    1000,
                                    1,
                                )
                                .await
                        }));
                    }

                    let results = futures::future::join_all(handles).await;
                    let winners: usize = results
                        .iter()
                        .filter(|r| {
                            r.as_ref()
                                .ok()
                                .and_then(|inner| inner.as_ref().ok())
                                .is_some_and(|dispatches| !dispatches.is_empty())
                        })
                        .count();
                    // Exactly one claimer should win.
                    assert_eq!(winners, 1, "expected exactly 1 winner, got {winners}");
                });
            }

            #[test]
            fn enqueue_then_claim_preserves_dispatch_data(
                priority in 0u8..=255u8,
                max_attempts in 1u32..20,
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let store = InMemoryMailboxStore::new();
                    let mut dispatch = make_dispatch("m-prop", "agent-prop");
                    dispatch.priority = priority;
                    dispatch.max_attempts = max_attempts;
                    store.enqueue(&dispatch).await.unwrap();

                    let claimed = store.claim("m-prop", "consumer-1", 30_000, 1000, 1).await.unwrap();
                    assert_eq!(claimed.len(), 1);
                    let cj = &claimed[0];
                    assert_eq!(cj.priority, priority);
                    assert_eq!(cj.max_attempts, max_attempts);
                    assert_eq!(cj.status, RunDispatchStatus::Claimed);
                    assert!(cj.claim_token.is_some());
                    assert_eq!(cj.claimed_by.as_deref(), Some("consumer-1"));
                });
            }
        }
    }
}
