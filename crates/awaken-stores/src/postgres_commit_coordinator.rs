//! PostgreSQL [`CommitCoordinator`] implementation (ADR-0036).
//!
//! Opens one `sqlx::Transaction` per checkpoint commit and drives
//! `ThreadRunStore`, `EventStore`, and `OutboxStore` writes through that
//! transaction. The canonical outbox row produced by each event append is
//! inserted by `append_in_tx` (ADR-0034 D9). The inline-writer outbox rows
//! attached to the plan are inserted via [`enqueue_outbox_in_transaction`]
//! in the same transaction.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::commit_coordinator::{
    CommitCoordinator, CommitError, ThreadCommit, ThreadCommitOutcome, TransactionScopeId,
};
use awaken_server_contract::contract::staged_commit::{
    StagedCommitCoordinator, ThreadCommitStagedOutcome, ThreadCommitStagedWrites,
};
use awaken_server_contract::contract::storage::StorageError;

use crate::postgres::PostgresStore;
use crate::postgres_outbox::enqueue_outbox_in_transaction;

/// Coordinator that drives [`PostgresStore`] through one Postgres
/// transaction per checkpoint commit.
#[derive(Clone)]
pub struct PgCommitCoordinator {
    store: Arc<PostgresStore>,
    scope: TransactionScopeId,
}

impl std::fmt::Debug for PgCommitCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCommitCoordinator")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl PgCommitCoordinator {
    /// Construct a coordinator from a shared [`PostgresStore`]. The store
    /// supplies the connection pool, schema prefix, and ensures schema
    /// readiness; the coordinator only manages the transaction boundary.
    pub fn new(store: Arc<PostgresStore>) -> Result<Self, CommitError> {
        let scope = TransactionScopeId::new(store.transaction_scope_descriptor())?;
        Ok(Self { store, scope })
    }

    /// Borrow the underlying store (escape hatch for callers that already
    /// hold an `Arc<PostgresStore>` and need to wire reads against it).
    #[must_use]
    pub fn store(&self) -> Arc<PostgresStore> {
        Arc::clone(&self.store)
    }
}

#[async_trait]
impl CommitCoordinator for PgCommitCoordinator {
    fn scope(&self) -> TransactionScopeId {
        self.scope.clone()
    }

    fn reader(&self) -> Arc<dyn awaken_server_contract::contract::storage::RuntimeCheckpointStore> {
        Arc::new(
            awaken_server_contract::contract::storage::ThreadRunCheckpointStore::new(Arc::clone(
                &self.store,
            )
                as Arc<dyn awaken_server_contract::contract::storage::ThreadRunStore>),
        )
    }

    async fn commit_checkpoint(
        &self,
        plan: ThreadCommit,
    ) -> Result<ThreadCommitOutcome, CommitError> {
        self.commit_checkpoint_staged(plan, ThreadCommitStagedWrites::default())
            .await?;
        Ok(ThreadCommitOutcome)
    }
}

#[async_trait]
impl StagedCommitCoordinator for PgCommitCoordinator {
    async fn commit_checkpoint_staged(
        &self,
        plan: ThreadCommit,
        staged: ThreadCommitStagedWrites,
    ) -> Result<ThreadCommitStagedOutcome, CommitError> {
        plan.validate()?;
        staged.validate(&plan.thread_id, &plan.run_projection.run_id)?;
        self.store
            .ensure_schema()
            .await
            .map_err(CommitError::StoreWrite)?;

        let mut tx = self
            .store
            .pool
            .begin()
            .await
            .map_err(|error| CommitError::Commit(error.to_string()))?;

        let mut canonical_event_ids = Vec::with_capacity(staged.canonical_drafts.len());
        for staged_event in &staged.canonical_drafts {
            let result = self
                .store
                .append_in_tx(
                    &mut tx,
                    staged_event.draft.clone(),
                    staged_event.append_options.clone(),
                )
                .await?;
            canonical_event_ids.push(result.event.event_id.as_str().to_string());
        }

        let mut server_event_ids = Vec::with_capacity(staged.server_events.len());
        for event in &staged.server_events {
            let result = self
                .store
                .append_in_tx(&mut tx, event.draft.clone(), event.options.clone())
                .await?;
            server_event_ids.push(result.event.event_id.as_str().to_string());
        }

        let mut additional_outbox_ids = Vec::with_capacity(staged.additional_outbox.len());
        for draft in &staged.additional_outbox {
            let result = enqueue_outbox_in_transaction(&self.store, &mut tx, draft.clone()).await?;
            additional_outbox_ids.push(result.message.outbox_id);
        }

        self.store
            .checkpoint_append_in_tx(
                &mut tx,
                &plan.thread_id,
                &plan.message_delta,
                plan.expected_message_count,
                &plan.run_projection,
            )
            .await
            .map_err(|error| match error {
                StorageError::VersionConflict { expected, actual } => {
                    CommitError::MessageVersionConflict {
                        thread_id: plan.thread_id.clone(),
                        expected,
                        actual,
                    }
                }
                other => CommitError::StoreWrite(other),
            })?;

        if let Some(thread_state) = &plan.thread_state_snapshot {
            self.store
                .save_thread_state_tx(&mut tx, &plan.thread_id, thread_state)
                .await
                .map_err(CommitError::StoreWrite)?;
        }

        tx.commit()
            .await
            .map_err(|error| CommitError::Commit(error.to_string()))?;

        Ok(ThreadCommitStagedOutcome {
            canonical_event_ids,
            server_event_ids,
            additional_outbox_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    fn lazy_pool(url: &str) -> PgPool {
        PgPool::connect_lazy(url).expect("lazy pg pool")
    }

    #[tokio::test]
    async fn scope_is_stable_for_equivalent_store_instances() {
        let left = Arc::new(PostgresStore::with_prefix(
            lazy_pool("postgres://user@localhost/awaken_test"),
            "awaken",
        ));
        let right = Arc::new(PostgresStore::with_prefix(
            lazy_pool("postgres://user@localhost/awaken_test"),
            "awaken",
        ));

        let left_scope = PgCommitCoordinator::new(left).unwrap().scope();
        let right_scope = PgCommitCoordinator::new(right).unwrap().scope();

        assert_eq!(left_scope, right_scope);
        assert!(!left_scope.as_str().contains("0x"));
    }

    #[tokio::test]
    async fn scope_differs_for_distinct_database_or_tables() {
        let base = Arc::new(PostgresStore::with_prefix(
            lazy_pool("postgres://user@localhost/awaken_test"),
            "awaken",
        ));
        let other_db = Arc::new(PostgresStore::with_prefix(
            lazy_pool("postgres://user@localhost/other_test"),
            "awaken",
        ));
        let other_prefix = Arc::new(PostgresStore::with_prefix(
            lazy_pool("postgres://user@localhost/awaken_test"),
            "other",
        ));

        let base_scope = PgCommitCoordinator::new(base).unwrap().scope();
        assert_ne!(
            base_scope,
            PgCommitCoordinator::new(other_db).unwrap().scope()
        );
        assert_ne!(
            base_scope,
            PgCommitCoordinator::new(other_prefix).unwrap().scope()
        );
    }
}
