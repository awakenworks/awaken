// @formal surface obligation=resource_reclamation.reference_and_lease_races_fail_closed
// @formal reason: Publishes a candidate's complete Session retention references before its root
// @formal reason: CAS and repairs them from repository truth after a lost or replayed CAS, so a
// @formal reason: Resource addition is retained before it becomes durable; the reclaimer keeps the
// @formal reason: lease and effect-fence requirements owned downstream.
//! Session Resource retention references and the Resource commit bracket.
//!
//! One responsibility: every Session-root commit that can change a Session's
//! Resource pins first publishes the candidate's complete retention set to
//! the Resource reference index, then commits through the one CAS kernel, and
//! repairs the projection from repository truth when the CAS loses or replays.
//! The component holds the CAS kernel and the optional reference index/File
//! catalog pair; without the pair it commits without a projection.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, ResourceReference, ResourceReferenceIndex, ResourceReferenceKind,
    ResourceReferenceRecord,
};
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionAggregate,
};

use crate::credentials::internal;
use crate::credentials::repository_preparation;
use crate::resource_reconciliation::resource_targets;
use crate::{SessionMutationError, SessionMutations, SessionPreparationError, SnapshotCommit};

pub struct SessionResourceReferences {
    mutations: Arc<SessionMutations>,
    retention: Option<(Arc<dyn ResourceReferenceIndex>, Arc<dyn FileCatalog>)>,
}

impl SessionResourceReferences {
    pub(crate) fn new(
        mutations: Arc<SessionMutations>,
        references: Option<Arc<dyn ResourceReferenceIndex>>,
        files: Option<Arc<dyn FileCatalog>>,
    ) -> Self {
        Self {
            mutations,
            retention: references.zip(files),
        }
    }

    pub(crate) async fn synchronize(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionPreparationError> {
        let Some((references, files)) = &self.retention else {
            return Ok(());
        };
        let resource_pins = session.resources.resource_references();
        let targets = resource_targets(files.as_ref(), owner_scope, &resource_pins)
            .await
            .map_err(internal)?;
        let records = targets
            .into_iter()
            .map(|target| ResourceReferenceRecord {
                target,
                reference: ResourceReference {
                    kind: ResourceReferenceKind::SessionBinding,
                    reference_id: session.session_id.to_string(),
                },
            })
            .collect();
        references
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                session.session_id.as_str(),
                records,
            )
            .await
            .map_err(internal)
    }

    /// Publish the candidate's complete retention set before its root CAS. A
    /// CAS failure repairs the projection from repository truth. This ordering
    /// makes additions fail closed against reclamation; removals remain safe
    /// because [`crate::SessionResourcePurgeGuard`] still observes the pre-CAS
    /// aggregate. Automatic and explicit records return the same original
    /// snapshot receipt; only a caller that does not report it may discard it.
    pub(crate) async fn commit(
        &self,
        owner_scope: &str,
        candidate: SessionAggregate,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SnapshotCommit, SessionMutationError> {
        let idempotency =
            crate::mutation::compile_session_snapshot_idempotency(&candidate, operation);
        // Keep the complete aggregate-owning Resource bracket on the heap.
        // Its references and root CAS are polled inline by this same caller;
        // no task or cancellation owner is detached.
        Box::pin(self.commit_with_record(owner_scope, candidate, idempotency, lifecycle_facts))
            .await
    }

    pub(crate) async fn commit_with_record(
        &self,
        owner_scope: &str,
        candidate: SessionAggregate,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SnapshotCommit, SessionMutationError> {
        let session_id = candidate.session_id.clone();
        self.synchronize(owner_scope, &candidate)
            .await
            .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
        match self
            .mutations
            .commit_session_snapshot_with_record(
                owner_scope,
                candidate,
                idempotency,
                lifecycle_facts,
            )
            .await
        {
            Ok(commit) => {
                if !commit.transitioned() {
                    // The exact receipt may reload a newer Resource manifest.
                    // Restore its projection after publishing our stale candidate,
                    // without turning an accepted replay into another failure.
                    self.repair(owner_scope, &session_id).await;
                }
                Ok(commit)
            }
            Err(error) => {
                self.repair(owner_scope, &session_id).await;
                Err(error)
            }
        }
    }

    pub(crate) async fn repair(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
    ) {
        let authoritative = self.mutations.session_aggregate(session_id).await;
        let repair = match authoritative {
            Ok(session) => self.synchronize(owner_scope, &session).await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                match &self.retention {
                    Some((references, _)) => references
                        .replace_references(
                            ResourceReferenceKind::SessionBinding,
                            session_id.as_str(),
                            Vec::new(),
                        )
                        .await
                        .map_err(internal),
                    None => Ok(()),
                }
            }
            Err(error) => Err(repository_preparation(error)),
        };
        if let Err(error) = repair {
            tracing::warn!(
                session_id = session_id.as_str(),
                workspace_id = owner_scope,
                error = %error,
                "Session Resource reference repair remains pending"
            );
        }
    }
}

#[cfg(test)]
mod tests;
