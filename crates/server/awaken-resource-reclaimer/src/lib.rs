//! Authorization-agnostic resource reclamation application service.
//!
//! Logical delete is authorized and committed before this service is invoked.
//! The reclaimer consumes only intrinsic resource lifecycle/reference facts and
//! drives an idempotent physical adapter behind a fenced durable intent. Local
//! embedding and cloud deployment therefore share the same state machine while
//! supplying different repository/guard/reclaimer adapters.

use std::sync::Arc;

use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourcePhysicalReclaimer,
    ResourcePurgeError, ResourcePurgeGuard, ResourcePurgeIntent, ResourcePurgeReceipt,
    ResourcePurgeRepository,
};

/// Outcome of one reconciliation scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    pub completed: usize,
    pub deferred: usize,
    pub retryable_failures: usize,
    pub conflicts: usize,
}

/// Durable, crash-recoverable coordinator for physical resource cleanup.
pub struct ResourceReclaimer {
    owner: String,
    lease_ms: u64,
    repository: Arc<dyn ResourcePurgeRepository>,
    guards: Vec<Arc<dyn ResourcePurgeGuard>>,
    physical: Arc<dyn ResourcePhysicalReclaimer>,
}

impl ResourceReclaimer {
    pub fn new(
        owner: impl Into<String>,
        lease_ms: u64,
        repository: Arc<dyn ResourcePurgeRepository>,
        physical: Arc<dyn ResourcePhysicalReclaimer>,
    ) -> Result<Self, ResourcePurgeError> {
        let owner = owner.into();
        if owner.trim().is_empty() || lease_ms == 0 {
            return Err(ResourcePurgeError::Invalid(
                "reclaimer owner and positive lease are required".into(),
            ));
        }
        Ok(Self {
            owner,
            lease_ms,
            repository,
            guards: Vec::new(),
            physical,
        })
    }

    /// Add an independently owned safety source. All guards must report no
    /// blockers before physical deletion is attempted.
    #[must_use]
    pub fn with_guard(mut self, guard: Arc<dyn ResourcePurgeGuard>) -> Self {
        self.guards.push(guard);
        self
    }

    pub async fn enqueue(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        self.repository.put(intent).await
    }

    pub async fn reconcile(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<ReconcileSummary, ResourcePurgeError> {
        if limit == 0 {
            return Ok(ReconcileSummary::default());
        }
        let candidates = self.repository.recoverable(now_unix_ms, limit).await?;
        let mut summary = ReconcileSummary::default();
        for mut intent in candidates {
            let expected_revision = intent.revision;
            let generation = match intent.claim(&self.owner, now_unix_ms, self.lease_ms) {
                Ok(generation) => generation,
                Err(ResourcePurgeError::LeaseHeld { .. })
                | Err(ResourcePurgeError::RetentionHeld { .. }) => {
                    summary.conflicts += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = self
                .repository
                .save(expected_revision, intent.clone())
                .await
            {
                if matches!(error, ResourcePurgeError::RevisionConflict(_)) {
                    summary.conflicts += 1;
                    continue;
                }
                return Err(error);
            }

            let mut blockers = Vec::new();
            let mut guard_error = None;
            for guard in &self.guards {
                match guard
                    .blockers(&intent.target, intent.config_version, now_unix_ms)
                    .await
                {
                    Ok(mut found) => blockers.append(&mut found),
                    Err(error) => {
                        guard_error = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = guard_error {
                let claimed_revision = intent.revision;
                intent.retry(&self.owner, generation, now_unix_ms, error.to_string())?;
                self.repository.save(claimed_revision, intent).await?;
                summary.retryable_failures += 1;
                continue;
            }
            if !blockers.is_empty() {
                let claimed_revision = intent.revision;
                intent.defer(&self.owner, generation, now_unix_ms, blockers)?;
                self.repository.save(claimed_revision, intent).await?;
                summary.deferred += 1;
                continue;
            }

            match self
                .repository
                .acquire_reclamation(&intent.intent_id, &intent.target)
                .await?
            {
                AcquireResourceReclamationOutcome::Acquired
                | AcquireResourceReclamationOutcome::AlreadyOwned => {}
                AcquireResourceReclamationOutcome::Blocked(records) => {
                    let claimed_revision = intent.revision;
                    intent.defer(
                        &self.owner,
                        generation,
                        now_unix_ms,
                        records.into_iter().map(|record| record.reference).collect(),
                    )?;
                    self.repository.save(claimed_revision, intent).await?;
                    summary.deferred += 1;
                    continue;
                }
                AcquireResourceReclamationOutcome::Contended => {
                    let claimed_revision = intent.revision;
                    intent.retry(
                        &self.owner,
                        generation,
                        now_unix_ms,
                        "physical resource is fenced by another reclamation intent",
                    )?;
                    self.repository.save(claimed_revision, intent).await?;
                    summary.conflicts += 1;
                    continue;
                }
            }

            // Close the interval between the first guard scan and durable fence
            // acquisition. Writers represented by the reference index now fail
            // closed; independently owned guard sources get one final check.
            blockers.clear();
            guard_error = None;
            for guard in &self.guards {
                match guard
                    .blockers(&intent.target, intent.config_version, now_unix_ms)
                    .await
                {
                    Ok(mut found) => blockers.append(&mut found),
                    Err(error) => {
                        guard_error = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = guard_error {
                let release_error = self
                    .repository
                    .release_reclamation(&intent.intent_id, &intent.target)
                    .await
                    .err();
                let claimed_revision = intent.revision;
                intent.retry(
                    &self.owner,
                    generation,
                    now_unix_ms,
                    combine_errors(error, release_error),
                )?;
                self.repository.save(claimed_revision, intent).await?;
                summary.retryable_failures += 1;
                continue;
            }
            if !blockers.is_empty() {
                self.repository
                    .release_reclamation(&intent.intent_id, &intent.target)
                    .await?;
                let claimed_revision = intent.revision;
                intent.defer(&self.owner, generation, now_unix_ms, blockers)?;
                self.repository.save(claimed_revision, intent).await?;
                summary.deferred += 1;
                continue;
            }

            match self
                .physical
                .purge(&intent.target, intent.config_version)
                .await
            {
                Ok(evidence) => {
                    if let Err(error) = self
                        .repository
                        .release_reclamation(&intent.intent_id, &intent.target)
                        .await
                    {
                        let claimed_revision = intent.revision;
                        intent.retry(&self.owner, generation, now_unix_ms, error.to_string())?;
                        self.repository.save(claimed_revision, intent).await?;
                        summary.retryable_failures += 1;
                        continue;
                    }
                    let claimed_revision = intent.revision;
                    intent.complete(
                        &self.owner,
                        generation,
                        now_unix_ms,
                        ResourcePurgeReceipt {
                            purged_at_unix_ms: now_unix_ms,
                            evidence,
                        },
                    )?;
                    self.repository.save(claimed_revision, intent).await?;
                    summary.completed += 1;
                }
                Err(error) => {
                    let release_error = self
                        .repository
                        .release_reclamation(&intent.intent_id, &intent.target)
                        .await
                        .err();
                    let claimed_revision = intent.revision;
                    intent.retry(
                        &self.owner,
                        generation,
                        now_unix_ms,
                        combine_errors(error, release_error),
                    )?;
                    self.repository.save(claimed_revision, intent).await?;
                    summary.retryable_failures += 1;
                }
            }
        }
        Ok(summary)
    }
}

fn combine_errors(primary: ResourcePurgeError, release: Option<ResourcePurgeError>) -> String {
    release.map_or_else(
        || primary.to_string(),
        |release| format!("{primary}; failed to release reclamation fence: {release}"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use awaken_resource_contract::{
        ResourceKind, ResourcePurgeEvidence, ResourcePurgeStatus, ResourceReclamationFence,
        ResourceReference, ResourceReferenceKind, ResourceTarget,
    };

    use super::*;

    #[derive(Default)]
    struct MemoryRepository {
        intents: Mutex<BTreeMap<String, ResourcePurgeIntent>>,
        fences: Mutex<BTreeMap<(ResourceKind, String), String>>,
    }

    #[async_trait]
    impl ResourceReclamationFence for MemoryRepository {
        async fn acquire_reclamation(
            &self,
            intent_id: &str,
            target: &ResourceTarget,
        ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
            let mut fences = self.fences.lock().unwrap();
            let key = (target.kind, target.resource_id.clone());
            match fences.get(&key) {
                Some(owner) if owner == intent_id => {
                    Ok(AcquireResourceReclamationOutcome::AlreadyOwned)
                }
                Some(_) => Ok(AcquireResourceReclamationOutcome::Contended),
                None => {
                    fences.insert(key, intent_id.into());
                    Ok(AcquireResourceReclamationOutcome::Acquired)
                }
            }
        }

        async fn release_reclamation(
            &self,
            intent_id: &str,
            target: &ResourceTarget,
        ) -> Result<bool, ResourcePurgeError> {
            let mut fences = self.fences.lock().unwrap();
            let key = (target.kind, target.resource_id.clone());
            match fences.get(&key) {
                Some(owner) if owner == intent_id => Ok(fences.remove(&key).is_some()),
                Some(_) => Err(ResourcePurgeError::StaleReclamationFence),
                None => Ok(false),
            }
        }
    }

    #[async_trait]
    impl ResourcePurgeRepository for MemoryRepository {
        async fn put(
            &self,
            intent: ResourcePurgeIntent,
        ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
            let mut rows = self.intents.lock().unwrap();
            if let Some(existing) = rows.get(&intent.intent_id) {
                return if existing.same_request(&intent) {
                    Ok(PutResourcePurgeOutcome::Existing)
                } else {
                    Err(ResourcePurgeError::IdempotencyConflict(
                        intent.idempotency_key,
                    ))
                };
            }
            rows.insert(intent.intent_id.clone(), intent);
            Ok(PutResourcePurgeOutcome::Inserted)
        }

        async fn get(
            &self,
            intent_id: &str,
        ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
            Ok(self.intents.lock().unwrap().get(intent_id).cloned())
        }

        async fn recoverable(
            &self,
            now_unix_ms: u64,
            limit: usize,
        ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
            Ok(self
                .intents
                .lock()
                .unwrap()
                .values()
                .filter(|intent| {
                    !intent.status.is_terminal()
                        && intent.not_before_unix_ms <= now_unix_ms
                        && intent
                            .lease_expires_at_unix_ms
                            .is_none_or(|expires| expires <= now_unix_ms)
                })
                .take(limit)
                .cloned()
                .collect())
        }

        async fn save(
            &self,
            expected_revision: u64,
            intent: ResourcePurgeIntent,
        ) -> Result<(), ResourcePurgeError> {
            let mut rows = self.intents.lock().unwrap();
            let current = rows
                .get(&intent.intent_id)
                .ok_or_else(|| ResourcePurgeError::NotFound(intent.intent_id.clone()))?;
            if current.revision != expected_revision {
                return Err(ResourcePurgeError::RevisionConflict(intent.intent_id));
            }
            rows.insert(intent.intent_id.clone(), intent);
            Ok(())
        }
    }

    struct Guard(Mutex<Vec<ResourceReference>>);

    #[async_trait]
    impl ResourcePurgeGuard for Guard {
        async fn blockers(
            &self,
            _target: &ResourceTarget,
            _config_version: Option<u64>,
            _now_unix_ms: u64,
        ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[derive(Default)]
    struct Physical(Mutex<usize>);

    #[async_trait]
    impl ResourcePhysicalReclaimer for Physical {
        async fn purge(
            &self,
            target: &ResourceTarget,
            _config_version: Option<u64>,
        ) -> Result<ResourcePurgeEvidence, ResourcePurgeError> {
            *self.0.lock().unwrap() += 1;
            match target.kind {
                ResourceKind::File => Ok(ResourcePurgeEvidence::File { blob_deleted: true }),
                _ => unreachable!(),
            }
        }
    }

    fn intent() -> ResourcePurgeIntent {
        ResourcePurgeIntent::new(
            "purge-1",
            "file:ws-a:file-1",
            ResourceTarget::new("ws-a", ResourceKind::File, "file-1"),
            None,
            10,
            10,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn references_defer_then_release_to_exactly_one_receipt() {
        let repository = Arc::new(MemoryRepository::default());
        let physical = Arc::new(Physical::default());
        let guard = Arc::new(Guard(Mutex::new(vec![ResourceReference {
            kind: ResourceReferenceKind::SessionBinding,
            reference_id: "session-1".into(),
        }])));
        let service = ResourceReclaimer::new("worker-a", 100, repository.clone(), physical.clone())
            .unwrap()
            .with_guard(guard.clone());
        service.enqueue(intent()).await.unwrap();

        assert_eq!(
            service.reconcile(10, 10).await.unwrap(),
            ReconcileSummary {
                deferred: 1,
                ..Default::default()
            }
        );
        assert_eq!(*physical.0.lock().unwrap(), 0);
        guard.0.lock().unwrap().clear();
        assert_eq!(
            service.reconcile(11, 10).await.unwrap(),
            ReconcileSummary {
                completed: 1,
                ..Default::default()
            }
        );
        assert_eq!(*physical.0.lock().unwrap(), 1);
        let stored = repository.get("purge-1").await.unwrap().unwrap();
        assert_eq!(stored.status, ResourcePurgeStatus::Completed);
        assert!(stored.receipt.is_some());
        assert_eq!(service.reconcile(12, 10).await.unwrap(), Default::default());
    }

    #[tokio::test]
    async fn expired_claim_is_recovered_and_stale_owner_is_fenced() {
        let repository = Arc::new(MemoryRepository::default());
        let physical = Arc::new(Physical::default());
        let mut claimed = intent();
        let generation = claimed.claim("dead-worker", 10, 5).unwrap();
        repository.put(claimed.clone()).await.unwrap();
        assert_eq!(
            ResourceReclaimer::new("recovery", 10, repository.clone(), physical)
                .unwrap()
                .reconcile(15, 10)
                .await
                .unwrap()
                .completed,
            1
        );
        assert_eq!(
            claimed.retry("dead-worker", generation, 15, "late"),
            Err(ResourcePurgeError::StaleClaim)
        );
    }
}
