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
use awaken_contract::contract::commit_coordinator::{
    CheckpointCommitOutcome, CheckpointCommitPlan, CommitCoordinator, CommitError,
    TransactionScopeId,
};

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
        let scope_descriptor = format!(
            "pg::({:p})::{}",
            Arc::as_ptr(&store),
            store.threads_table.as_str()
        );
        let scope = TransactionScopeId::new(scope_descriptor)?;
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

    fn thread_run_store(&self) -> Arc<dyn awaken_contract::contract::storage::ThreadRunStore> {
        Arc::clone(&self.store) as Arc<dyn awaken_contract::contract::storage::ThreadRunStore>
    }

    async fn commit_checkpoint(
        &self,
        plan: CheckpointCommitPlan,
    ) -> Result<CheckpointCommitOutcome, CommitError> {
        plan.validate()?;
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

        let mut canonical_event_ids = Vec::with_capacity(plan.canonical_drafts.len());
        for staged in &plan.canonical_drafts {
            let result = self
                .store
                .append_in_tx(&mut tx, staged.draft.clone(), staged.append_options.clone())
                .await?;
            canonical_event_ids.push(result.event.event_id.as_str().to_string());
        }

        let mut server_event_ids = Vec::with_capacity(plan.server_events.len());
        for event in &plan.server_events {
            let result = self
                .store
                .append_in_tx(&mut tx, event.draft.clone(), event.options.clone())
                .await?;
            server_event_ids.push(result.event.event_id.as_str().to_string());
        }

        let mut additional_outbox_ids = Vec::with_capacity(plan.additional_outbox.len());
        for draft in &plan.additional_outbox {
            let result = enqueue_outbox_in_transaction(&self.store, &mut tx, draft.clone()).await?;
            additional_outbox_ids.push(result.message.outbox_id);
        }

        self.store
            .checkpoint_in_tx(&mut tx, &plan.thread_id, &plan.messages, &plan.run)
            .await?;

        tx.commit()
            .await
            .map_err(|error| CommitError::Commit(error.to_string()))?;

        Ok(CheckpointCommitOutcome {
            canonical_event_ids,
            server_event_ids,
            additional_outbox_ids,
        })
    }
}
