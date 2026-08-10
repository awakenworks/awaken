//! Atomic Resource-reference projection for durable Memory extraction work.

use std::sync::Arc;

use awaken_ext_memory::{
    MemoryExtractionError, MemoryExtractionIntent, MemoryExtractionRepository,
    PutMemoryExtractionOutcome,
};
use awaken_resource_contract::{
    ResourceKind, ResourcePurgeError, ResourceReference, ResourceReferenceIndex,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};

pub struct ReferenceIndexedMemoryExtractions {
    inner: Arc<dyn MemoryExtractionRepository>,
    references: Arc<dyn ResourceReferenceIndex>,
}

impl ReferenceIndexedMemoryExtractions {
    #[must_use]
    pub fn new(
        inner: Arc<dyn MemoryExtractionRepository>,
        references: Arc<dyn ResourceReferenceIndex>,
    ) -> Self {
        Self { inner, references }
    }

    fn record(intent: &MemoryExtractionIntent) -> ResourceReferenceRecord {
        ResourceReferenceRecord {
            target: ResourceTarget::new(
                &intent.workspace_id,
                ResourceKind::MemoryStore,
                &intent.memory_store_id,
            ),
            reference: ResourceReference {
                kind: ResourceReferenceKind::ExtractionIntent,
                reference_id: intent.intent_id.clone(),
            },
        }
    }

    /// Rebuild reference rows for intents restored before this decorator was
    /// assembled. The repository's recoverable scan is the authoritative set of
    /// non-terminal work and add is idempotent.
    pub async fn synchronize_recoverable_references(&self) -> Result<(), MemoryExtractionError> {
        for intent in self.inner.recoverable_extractions(usize::MAX).await? {
            self.references
                .add_reference(Self::record(&intent))
                .await
                .map_err(extraction_storage)?;
        }
        Ok(())
    }
}

fn extraction_storage(error: ResourcePurgeError) -> MemoryExtractionError {
    MemoryExtractionError::Storage(error.to_string())
}

#[async_trait::async_trait]
impl MemoryExtractionRepository for ReferenceIndexedMemoryExtractions {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        // Add before exposing durable work. Failure leaves either no intent or a
        // conservative stale row; it can never expose an unprotected intent.
        self.references
            .add_reference(Self::record(&intent))
            .await
            .map_err(extraction_storage)?;
        self.inner.put_extraction_if_absent(intent).await
    }

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        self.inner.get_extraction(intent_id).await
    }

    async fn extraction_cursor(&self, session_id: &str) -> Result<usize, MemoryExtractionError> {
        self.inner.extraction_cursor(session_id).await
    }

    async fn recoverable_extractions(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
        self.inner.recoverable_extractions(limit).await
    }

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        let record = Self::record(&intent);
        if !intent.status.is_terminal() {
            self.references
                .add_reference(record.clone())
                .await
                .map_err(extraction_storage)?;
        }
        self.inner
            .compare_and_swap_extraction(expected_revision, intent.clone())
            .await?;
        if intent.status.is_terminal() {
            // Remove only after the terminal state is visible. A failure leaks a
            // blocker safely and is repaired by idempotent terminal replay.
            self.references
                .remove_reference(&record)
                .await
                .map_err(extraction_storage)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Message, Role};
    use awaken_ext_memory::MemoryExtractorSnapshot;
    use awaken_resource_contract::{ResourceReclamationFence, ResourceReferenceIndex};

    fn intent(id: &str, store: &str) -> MemoryExtractionIntent {
        MemoryExtractionIntent::new_range(
            id,
            format!("session:terminal:{id}"),
            "workspace-a",
            "session-a",
            "terminal-a",
            store,
            1,
            0,
            1,
            vec![Message::text(
                Id("message-a".into()),
                Role::User,
                "remember",
            )],
            MemoryExtractorSnapshot::host_executor("memory-agent", "host", "config-1", "host"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn extraction_reference_lifetime_closes_every_reclamation_window() {
        // FMECA cause/effect graph:
        // C1 Pending intent is inserted; C2 a non-terminal CAS advances it; C3
        // terminal CAS succeeds; C4 target is already reclamation-fenced; C5
        // restart restores a Pending intent before the decorator is assembled.
        // Effects: E1 reference precedes insertion; E2 it survives recoverable
        // transitions; E3 it is removed only after terminal state is durable;
        // E4 fenced insertion fails and creates no intent. Failure "MemoryStore
        // purged while extraction can retry" has S=9,O=4,D=8,RPN=288.
        // Decision rules: X1=C1->E1; X2=C1+C2->E2; X3=C1+C2+C3->E3;
        // X4=C4+C1->E4; X5=C5->rebuild its blocker. Conservative stale rows are
        // safe on partial failure.
        let inner = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let references = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
        let repository = ReferenceIndexedMemoryExtractions::new(inner.clone(), references.clone());
        let target = ResourceTarget::new("workspace-a", ResourceKind::MemoryStore, "memory-a");
        let pending = intent("intent-a", "memory-a");
        repository
            .put_extraction_if_absent(pending.clone())
            .await
            .unwrap();
        assert_eq!(references.references(&target).await.unwrap().len(), 1, "X1");

        let mut claimed = pending;
        let generation = claimed.claim("worker-a", 10, 100).unwrap();
        repository
            .compare_and_swap_extraction(0, claimed.clone())
            .await
            .unwrap();
        assert_eq!(references.references(&target).await.unwrap().len(), 1, "X2");

        claimed
            .terminal_fail("worker-a", generation, 11, "invalid output")
            .unwrap();
        repository
            .compare_and_swap_extraction(1, claimed)
            .await
            .unwrap();
        assert!(
            references.references(&target).await.unwrap().is_empty(),
            "X3"
        );

        let fenced = ResourceTarget::new("workspace-a", ResourceKind::MemoryStore, "memory-fenced");
        assert!(matches!(
            references
                .acquire_reclamation("purge-fenced", &fenced)
                .await
                .unwrap(),
            awaken_resource_contract::AcquireResourceReclamationOutcome::Acquired
        ));
        assert!(
            repository
                .put_extraction_if_absent(intent("intent-fenced", "memory-fenced"))
                .await
                .is_err(),
            "X4"
        );
        assert!(
            inner
                .get_extraction("intent-fenced")
                .await
                .unwrap()
                .is_none(),
            "X4"
        );

        let restored_inner = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        restored_inner
            .put_extraction_if_absent(intent("intent-restored", "memory-restored"))
            .await
            .unwrap();
        let restored_references =
            Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
        let restored =
            ReferenceIndexedMemoryExtractions::new(restored_inner, restored_references.clone());
        restored.synchronize_recoverable_references().await.unwrap();
        assert_eq!(
            restored_references
                .references(&ResourceTarget::new(
                    "workspace-a",
                    ResourceKind::MemoryStore,
                    "memory-restored",
                ))
                .await
                .unwrap()
                .len(),
            1,
            "X5"
        );
    }
}
