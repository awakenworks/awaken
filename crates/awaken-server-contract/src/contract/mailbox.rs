pub use awaken_runtime_contract::contract::mailbox::*;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_runtime_contract::contract::storage::StorageError;

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

#[derive(Clone)]
pub struct ScopedMailboxStore {
    inner: Arc<dyn MailboxStore>,
    scope_id: ScopeId,
}

impl ScopedMailboxStore {
    pub fn new(inner: Arc<dyn MailboxStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn MailboxStore {
        self.inner.as_ref()
    }

    fn scoped(&self, id: &str) -> String {
        scoped_key(&self.scope_id, id)
    }

    fn unscoped<'a>(&self, id: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, id)
    }

    fn encode_dispatch(&self, dispatch: &RunDispatch) -> RunDispatch {
        let mut dispatch = dispatch.clone();
        dispatch.dispatch_id = self.scoped(&dispatch.dispatch_id);
        dispatch.thread_id = self.scoped(&dispatch.thread_id);
        dispatch.run_id = self.scoped(&dispatch.run_id);
        dispatch.dedupe_key = dispatch.dedupe_key.as_deref().map(|key| self.scoped(key));
        dispatch
    }

    fn decode_dispatch(&self, mut dispatch: RunDispatch) -> Option<RunDispatch> {
        dispatch.dispatch_id = self.unscoped(&dispatch.dispatch_id)?.to_string();
        dispatch.thread_id = self.unscoped(&dispatch.thread_id)?.to_string();
        dispatch.run_id = self.unscoped(&dispatch.run_id)?.to_string();
        dispatch.dedupe_key = dispatch
            .dedupe_key
            .as_deref()
            .map(|key| self.unscoped(key).map(str::to_string))
            .unwrap_or(None);
        Some(dispatch)
    }

    fn encode_target(&self, target: &LiveRunTarget) -> LiveRunTarget {
        LiveRunTarget {
            thread_id: self.scoped(&target.thread_id),
            run_id: self.scoped(&target.run_id),
            dispatch_id: target.dispatch_id.as_deref().map(|id| self.scoped(id)),
        }
    }

    fn encode_result(&self, result: &RunDispatchResult) -> RunDispatchResult {
        let mut result = result.clone();
        result.run_id = self.scoped(&result.run_id);
        result
    }
}

#[async_trait]
impl MailboxStore for ScopedMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        self.inner.enqueue(&self.encode_dispatch(dispatch)).await
    }

    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .claim(&self.scoped(thread_id), consumer_id, lease_ms, now, limit)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .claim_dispatch(&self.scoped(dispatch_id), consumer_id, lease_ms, now)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn ack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .ack(&self.scoped(dispatch_id), claim_token, now)
            .await
    }

    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .record_dispatch_start(
                &self.scoped(dispatch_id),
                claim_token,
                dispatch_instance_id,
                now,
            )
            .await
    }

    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .record_run_result(
                &self.scoped(dispatch_id),
                claim_token,
                &self.encode_result(result),
                now,
            )
            .await
    }

    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .nack(&self.scoped(dispatch_id), claim_token, retry_at, error, now)
            .await
    }

    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .dead_letter(&self.scoped(dispatch_id), claim_token, error, now)
            .await
    }

    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .cancel(&self.scoped(dispatch_id), now)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        self.inner
            .extend_lease(&self.scoped(dispatch_id), claim_token, extension_ms, now)
            .await
    }

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        let interrupt = self.inner.interrupt(&self.scoped(thread_id), now).await?;
        Ok(MailboxInterrupt {
            new_dispatch_epoch: interrupt.new_dispatch_epoch,
            active_dispatch: interrupt
                .active_dispatch
                .and_then(|dispatch| self.decode_dispatch(dispatch)),
            superseded_count: interrupt.superseded_count,
        })
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        let details = self
            .inner
            .interrupt_detailed(&self.scoped(thread_id), now)
            .await?;
        let superseded_dispatches: Vec<_> = details
            .superseded_dispatches
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect();
        Ok(MailboxInterruptDetails {
            new_dispatch_epoch: details.new_dispatch_epoch,
            active_dispatch: details
                .active_dispatch
                .and_then(|dispatch| self.decode_dispatch(dispatch)),
            superseded_count: superseded_dispatches.len(),
            superseded_dispatches,
        })
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        self.inner
            .current_dispatch_epoch(&self.scoped(thread_id))
            .await
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .supersede_claimed(&self.scoped(dispatch_id), claim_token, now, reason)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .load_dispatch(&self.scoped(dispatch_id))
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .list_dispatches(&self.scoped(thread_id), status_filter, limit, offset)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        match status {
            RunDispatchStatus::Queued => {
                let mut total = 0;
                for thread_id in self.queued_thread_ids().await? {
                    total += self
                        .list_dispatches(
                            &thread_id,
                            Some(&[RunDispatchStatus::Queued]),
                            usize::MAX,
                            0,
                        )
                        .await?
                        .len();
                }
                Ok(total)
            }
            status if status.is_terminal() => Ok(self
                .list_terminal_dispatches(usize::MAX, 0)
                .await?
                .into_iter()
                .filter(|dispatch| dispatch.status == status)
                .count()),
            _ => Err(StorageError::Io(
                "scoped claimed dispatch count is not supported".into(),
            )),
        }
    }

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let all: Vec<_> = self
            .inner
            .list_terminal_dispatches(usize::MAX, 0)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect();
        Ok(all.into_iter().skip(offset).take(limit).collect())
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .reclaim_expired_leases(now, limit)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn purge_terminal(&self, _older_than: u64) -> Result<usize, StorageError> {
        Err(StorageError::Io(
            "scoped terminal dispatch purge is not supported".into(),
        ))
    }

    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        Ok(self
            .inner
            .queued_thread_ids()
            .await?
            .into_iter()
            .filter_map(|thread_id| self.unscoped(&thread_id).map(str::to_string))
            .collect())
    }

    fn supports_dispatch_signals(&self) -> bool {
        self.inner.supports_dispatch_signals()
    }

    async fn pull_dispatch_signals(
        &self,
        max: usize,
        expires: Duration,
    ) -> Result<Vec<DispatchSignalEntry>, StorageError> {
        Ok(self
            .inner
            .pull_dispatch_signals(max, expires)
            .await?
            .into_iter()
            .filter_map(|entry| {
                Some(DispatchSignalEntry {
                    thread_id: self.unscoped(&entry.thread_id)?.to_string(),
                    dispatch_id: self.unscoped(&entry.dispatch_id)?.to_string(),
                    receipt: entry.receipt,
                })
            })
            .collect())
    }

    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.inner.deliver_live(&self.scoped(thread_id), cmd).await
    }

    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.inner
            .deliver_live_to(&self.encode_target(target), cmd)
            .await
    }

    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.inner.open_live_channel(&self.scoped(thread_id)).await
    }

    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.inner
            .open_live_channel_for(&self.encode_target(target))
            .await
    }
}
