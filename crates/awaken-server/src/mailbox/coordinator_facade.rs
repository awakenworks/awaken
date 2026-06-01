//! Mailbox-side facade types that adapt executor inputs to the single
//! `CommitCoordinator` field on [`crate::mailbox::Mailbox`].
//!
//! - [`IntoDispatchExecutor`] erases either `Arc<R>` or `Arc<dyn ...>` into
//!   the trait object the mailbox stores internally so `Mailbox::new`
//!   accepts both shapes through one signature.
//! - [`MailboxRunStoreCoordinator`] is the implicit coordinator the mailbox
//!   builds only in unit tests when the supplied executor exposes none. It
//!   delegates straight to `ThreadRunStore::checkpoint`; non-test callers wire
//!   a full coordinator
//!   (`MemoryCommitCoordinator` / `FileCommitCoordinator` /
//!   `PgCommitCoordinator`) through the runtime instead.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::commit_coordinator::{
    CommitCoordinator, CommitError, ThreadCommit, ThreadCommitOutcome, TransactionScopeId,
};
use awaken_server_contract::contract::storage::{
    RuntimeCheckpointStore, ThreadRunCheckpointStore, ThreadRunStore,
};

use super::RunDispatchExecutor;

/// Erase any `Arc<R>` (or `Arc<dyn RunDispatchExecutor>`) into the single
/// trait object the mailbox holds internally. Lets `Mailbox::new` accept
/// both concrete and pre-erased executors through one signature.
pub trait IntoDispatchExecutor {
    fn into_dispatch_executor(self) -> Arc<dyn RunDispatchExecutor>;
}

impl<R: RunDispatchExecutor + 'static> IntoDispatchExecutor for Arc<R> {
    fn into_dispatch_executor(self) -> Arc<dyn RunDispatchExecutor> {
        self
    }
}

impl IntoDispatchExecutor for Arc<dyn RunDispatchExecutor> {
    fn into_dispatch_executor(self) -> Arc<dyn RunDispatchExecutor> {
        self
    }
}

/// Minimal [`CommitCoordinator`] that delegates straight to
/// [`ThreadRunStore::checkpoint`]. Used by `Mailbox::new` only in unit tests
/// where the runtime never carries persistence wiring. It does not publish
/// canonical events or outbox drafts — only checkpoint commits — so it is
/// **not** a replacement for `MemoryCommitCoordinator` /
/// `FileCommitCoordinator` / `PgCommitCoordinator` in deployments that need
/// full transactional event capture.
pub(super) struct MailboxRunStoreCoordinator {
    store: Arc<dyn ThreadRunStore>,
    scope: TransactionScopeId,
}

impl MailboxRunStoreCoordinator {
    pub(super) fn new(store: Arc<dyn ThreadRunStore>) -> Self {
        let scope = TransactionScopeId::new(format!("mailbox-implicit::{:p}", Arc::as_ptr(&store)))
            .expect("mailbox scope id is non-empty");
        Self { store, scope }
    }
}

#[async_trait]
impl CommitCoordinator for MailboxRunStoreCoordinator {
    fn scope(&self) -> TransactionScopeId {
        self.scope.clone()
    }

    fn reader(&self) -> Arc<dyn RuntimeCheckpointStore> {
        Arc::new(ThreadRunCheckpointStore::new(Arc::clone(&self.store)))
    }

    async fn commit_checkpoint(
        &self,
        plan: ThreadCommit,
    ) -> Result<ThreadCommitOutcome, CommitError> {
        plan.validate()?;
        self.store
            .checkpoint_append(
                &plan.thread_id,
                &plan.message_delta,
                plan.expected_message_count,
                &plan.run_projection,
            )
            .await
            .map_err(|error| {
                CommitError::StoreWrite(error).reclassify_append_conflict(&plan.thread_id)
            })?;
        Ok(ThreadCommitOutcome)
    }
}
