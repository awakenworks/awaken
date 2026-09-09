//! Canonical in-memory credential repository used by tests and fixtures.

use std::{collections::HashMap, sync::Mutex};

use super::*;

/// In-memory [`CredentialRepo`] for tests and scenario fixtures.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub(crate) struct RepoState {
    pub(super) rows: HashMap<String, CredentialSource>,
    pub(super) pools: HashMap<String, CredentialPool>,
    pub(super) intents: HashMap<String, CredentialMutationIntent>,
    pub(super) managed_mutations: HashMap<String, PendingManagedCredentialMutation>,
    pub(super) managed_rollouts: HashMap<String, ManagedCredentialRollout>,
    pub(crate) vaults: HashMap<String, ManagedVault>,
    pub(crate) vault_credentials: HashMap<String, ManagedVaultCredential>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryCredentialRepo {
    pub(crate) state: Mutex<RepoState>,
}

#[cfg(any(test, feature = "test-support"))]
impl InMemoryCredentialRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl CredentialRepo for InMemoryCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .rows
            .insert(source.id.0.clone(), source);
        Ok(())
    }

    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        Ok(state
            .rows
            .entry(source.id.0.clone())
            .or_insert(source)
            .clone())
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .rows
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::SourceNotFound(id.0.clone()))
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .rows
            .values()
            .filter(|s| s.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<bool, CredentialError> {
        intent.validate_for_begin()?;
        let mut state = self.state.lock().expect("credential repo");
        if let Some(durable) = state.intents.get(&intent.after.id.0) {
            return if durable.matches_logical_command(&intent) {
                Ok(false)
            } else {
                Err(CredentialError::MutationConflict(
                    "another credential mutation is pending".into(),
                ))
            };
        }
        let current_after = state.rows.get(&intent.after.id.0);
        let predecessor = intent.before.as_ref().and_then(|before| {
            intent
                .is_distinct_replacement()
                .then(|| state.rows.get(&before.id.0))
                .flatten()
        });
        let before_matches = intent.publication_preimage_matches(current_after, predecessor);
        if !before_matches {
            return Err(CredentialError::MutationConflict(
                "credential changed before its material mutation was prepared".into(),
            ));
        }
        state.intents.insert(intent.after.id.0.clone(), intent);
        Ok(true)
    }

    async fn mark_mutation_ready(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        let ready = intent.with_material_ready()?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state.intents.get_mut(&intent.after.id.0).ok_or_else(|| {
            CredentialError::MutationConflict(
                "credential material mutation has no durable pending fact".into(),
            )
        })?;
        if durable != intent {
            return Err(CredentialError::MutationConflict(
                "credential ready transition does not match its durable Writing owner".into(),
            ));
        }
        *durable = ready.clone();
        Ok(ready)
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        let prepared = intent.prepare_publication()?;
        let mut state = self.state.lock().expect("credential repo");
        let reclaiming = match prepared.admit(
            state.intents.get(&intent.after.id.0),
            state.rows.get(&intent.after.id.0),
        )? {
            CredentialPublicationAdmission::Replayed(reclaiming) => return Ok(reclaiming),
            CredentialPublicationAdmission::Apply(reclaiming) => reclaiming,
            CredentialPublicationAdmission::NeedsPredecessor(publication) => {
                let current = state.rows.get(&publication.before().id.0);
                publication.admit(current)?
            }
        };
        state
            .rows
            .insert(intent.after.id.0.clone(), intent.after.clone());
        state
            .intents
            .insert(intent.after.id.0.clone(), reclaiming.clone());
        Ok(reclaiming)
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .intents
            .iter()
            .map(|(source_id, intent)| {
                intent.validate_durable_key(source_id)?;
                Ok(intent.clone())
            })
            .collect()
    }

    async fn claim_expired_mutation(
        &self,
        intent: &CredentialMutationIntent,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<CredentialMutationIntent>, CredentialError> {
        let Some(claimed) = intent.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.intents.get_mut(&intent.after.id.0) else {
            return Ok(None);
        };
        // Successful claim already requires Writing; full snapshot equality
        // also fences phase changes before this locked write.
        if durable != intent {
            return Ok(None);
        }
        *durable = claimed.clone();
        Ok(Some(claimed))
    }

    async fn abort_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state.intents.get(&intent.after.id.0).ok_or_else(|| {
            CredentialError::MutationConflict("credential abort has no durable pending fact".into())
        })?;
        let abort = intent.admit_abort(durable, state.rows.get(&intent.after.id.0))?;
        state
            .intents
            .insert(intent.after.id.0.clone(), abort.clone());
        Ok(abort)
    }

    async fn complete_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        intent.validate()?;
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.intents.get(&intent.after.id.0) else {
            return Ok(());
        };
        intent.admit_completion(durable, state.rows.get(&intent.after.id.0))?;
        state.intents.remove(&intent.after.id.0);
        Ok(())
    }

    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .rows
            .values()
            .flat_map(|source| source.material_refs().cloned())
            .collect())
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .pools
            .insert(pool.id.0.clone(), pool);
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .pools
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::PoolNotFound(id.0.clone()))
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .pools
            .values()
            .filter(|p| p.workspace_id == workspace_id)
            .cloned()
            .collect())
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl ManagedCredentialRepository for InMemoryCredentialRepo {
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
    ) -> Result<bool, CredentialError> {
        pending
            .validate_for_begin()
            .map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        if let Some(durable) = state.managed_mutations.get(&pending.after_source.id.0) {
            return if durable.matches_logical_command(&pending) {
                Ok(false)
            } else {
                Err(CredentialError::MutationConflict(
                    "another Managed credential mutation is pending".into(),
                ))
            };
        }
        let current_source = state.rows.get(&pending.after_source.id.0);
        let current_child = state.vault_credentials.get(&pending.after_credential.id);
        if !pending.is_unpublished_at(current_source, current_child) {
            return Err(CredentialError::MutationConflict(
                "Managed credential changed before its mutation was prepared".into(),
            ));
        }
        state
            .managed_mutations
            .insert(pending.after_source.id.0.clone(), pending);
        Ok(true)
    }

    async fn commit_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
        pending.validate()?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state
            .managed_mutations
            .get(&pending.after_source.id.0)
            .cloned()
            .ok_or_else(|| {
                ManagedCredentialMutationError::Store(CredentialError::MutationConflict(
                    "Managed credential mutation has no durable pending fact".into(),
                ))
            })?;
        durable.validate()?;
        if durable.material_fence.phase == CredentialMaterialMutationPhase::Reclaiming {
            let expected_reclaiming = pending.with_material_reclaiming()?;
            if durable == expected_reclaiming
                && state.rows.get(&pending.after_source.id.0) == Some(&pending.after_source)
                && state.vault_credentials.get(&pending.after_credential.id)
                    == Some(&pending.after_credential)
            {
                return Ok(durable);
            }
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        if &durable != pending
            || pending.material_fence.phase != CredentialMaterialMutationPhase::Ready
        {
            return Err(ManagedCredentialMutationError::Store(
                CredentialError::MutationConflict(
                    "Managed credential mutation is not ready or does not match its durable fact"
                        .into(),
                ),
            ));
        }

        let current_source = state.rows.get(&pending.after_source.id.0);
        let current_child = state.vault_credentials.get(&pending.after_credential.id);
        if !pending.is_unpublished_at(current_source, current_child) {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }

        let vault = state.vaults.get(&pending.after_credential.vault_id);
        let existing = state
            .vault_credentials
            .values()
            .filter(|current| {
                current.workspace_id == pending.after_credential.workspace_id
                    && current.vault_id == pending.after_credential.vault_id
                    && current.id != pending.after_credential.id
                    && !current.lifecycle.is_deleted()
            })
            .cloned()
            .collect::<Vec<_>>();
        match pending.operation {
            ManagedCredentialOperation::Create => {
                if state
                    .vault_credentials
                    .values()
                    .any(|credential| credential.source_id == pending.after_source.id)
                {
                    return Err(ManagedCredentialMutationError::RevisionConflict);
                }
                admit_managed_credential_insert(
                    &pending.after_credential.workspace_id,
                    vault,
                    &existing,
                    &pending.after_credential,
                )?;
            }
            ManagedCredentialOperation::Update => {
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    pending.before_credential.as_ref(),
                    pending
                        .before_credential
                        .as_ref()
                        .ok_or(ManagedCredentialMutationError::NotFound)?
                        .revision,
                    &pending.after_credential,
                )?;
                admit_managed_credential_insert(
                    &pending.after_credential.workspace_id,
                    vault,
                    &existing,
                    &ManagedVaultCredential {
                        revision: 1,
                        lifecycle: ManagedCredentialLifecycle::Active,
                        ..pending.after_credential.clone()
                    },
                )?;
            }
            ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete => {
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    pending.before_credential.as_ref(),
                    pending
                        .before_credential
                        .as_ref()
                        .ok_or(ManagedCredentialMutationError::NotFound)?
                        .revision,
                    &pending.after_credential,
                )?;
                if !managed_retirement_parent_admitted(
                    pending.operation,
                    vault,
                    &pending.after_credential,
                ) {
                    return Err(if vault.is_some() {
                        ManagedCredentialMutationError::InvalidLifecycle
                    } else {
                        ManagedCredentialMutationError::NotFound
                    });
                }
            }
        }
        let rollout = managed_rollout_from_committed(pending);
        if rollout.as_ref().is_some_and(|proposed| {
            state
                .managed_rollouts
                .get(&proposed.id)
                .is_some_and(|durable| durable != proposed)
        }) {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        state.rows.insert(
            pending.after_source.id.0.clone(),
            pending.after_source.clone(),
        );
        state.vault_credentials.insert(
            pending.after_credential.id.clone(),
            pending.after_credential.clone(),
        );
        if let Some(rollout) = rollout {
            state
                .managed_rollouts
                .entry(rollout.id.clone())
                .or_insert(rollout);
        }
        let reclaiming = pending
            .with_material_reclaiming()
            .map_err(ManagedCredentialMutationError::Store)?;
        state
            .managed_mutations
            .insert(pending.after_source.id.0.clone(), reclaiming.clone());
        Ok(reclaiming)
    }

    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state
            .managed_mutations
            .get_mut(&pending.after_source.id.0)
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "Managed credential mutation has no durable pending fact".into(),
                )
            })?;
        if durable != pending
            || pending.material_fence.phase != CredentialMaterialMutationPhase::Writing
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential ready transition does not match Writing".into(),
            ));
        }
        let ready = pending.with_material_ready()?;
        *durable = ready.clone();
        Ok(ready)
    }

    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .managed_mutations
            .iter()
            .map(|(source_id, pending)| {
                pending.validate_durable_key(source_id)?;
                Ok(pending.clone())
            })
            .collect()
    }

    async fn claim_expired_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<PendingManagedCredentialMutation>, CredentialError> {
        let Some(claimed) = pending.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        claimed.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.managed_mutations.get_mut(&pending.after_source.id.0) else {
            return Ok(None);
        };
        // Successful claim already requires Writing; full snapshot equality
        // also fences phase changes before this locked write.
        if durable != pending {
            return Ok(None);
        }
        *durable = claimed.clone();
        Ok(Some(claimed))
    }

    async fn abort_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let durable = state
            .managed_mutations
            .get(&pending.after_source.id.0)
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "Managed credential abort has no durable pending fact".into(),
                )
            })?;
        let reclaiming = pending.admit_abort(
            durable,
            state.rows.get(&pending.after_source.id.0),
            state.vault_credentials.get(&pending.after_credential.id),
        )?;
        let durable = state
            .managed_mutations
            .get_mut(&pending.after_source.id.0)
            .expect("checked under the same lock");
        *durable = reclaiming.clone();
        Ok(reclaiming)
    }

    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state.lock().expect("credential repo");
        let Some(durable) = state.managed_mutations.get(&pending.after_source.id.0) else {
            return Ok(());
        };
        pending.admit_completion(
            durable,
            state.rows.get(&pending.after_source.id.0),
            state.vault_credentials.get(&pending.after_credential.id),
        )?;
        state.managed_mutations.remove(&pending.after_source.id.0);
        Ok(())
    }

    async fn pending_managed_rollouts(
        &self,
    ) -> Result<Vec<ManagedCredentialRollout>, CredentialError> {
        let mut events = self
            .state
            .lock()
            .expect("credential repo")
            .managed_rollouts
            .values()
            .cloned()
            .collect::<Vec<_>>();
        events.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(events)
    }

    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .managed_rollouts
            .get(event_id)
            .cloned())
    }

    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        match state.managed_rollouts.get(&rollout.id) {
            Some(durable) if durable == rollout => {
                state.managed_rollouts.remove(&rollout.id);
                Ok(())
            }
            None => Ok(()),
            Some(_) => Err(CredentialError::MutationConflict(
                "Managed credential rollout acknowledgement is stale".into(),
            )),
        }
    }

    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError> {
        let mut vaults = self
            .state
            .lock()
            .expect("credential repo")
            .vaults
            .values()
            .filter(|vault| vault.deletion_requested())
            .cloned()
            .collect::<Vec<_>>();
        vaults.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(vaults)
    }
}
