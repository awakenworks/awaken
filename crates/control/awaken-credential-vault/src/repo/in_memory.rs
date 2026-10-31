// @formal entrypoint credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
// @formal entrypoint credential.managed_creation_begin_rejects_every_published_source_or_child_identity
// @formal entrypoint credential.managed_creation_publishes_source_and_child_atomically
// @formal entrypoint credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected
// @formal entrypoint credential.managed_rollout_outbox_is_atomic_with_exact_pair_publication
// @formal entrypoint credential.rotation_revision_cas
// @formal evidence credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
// @formal surface exclude=test_support_adapter
// @formal owner crates/control/awaken-credential-vault/src/repo.rs
// @formal reason: Every state type and repository implementation is compiled only under cfg(any(test,
// @formal reason: feature = test-support)) and serves tests or scenario fixtures. Production Credential
// @formal reason: authority remains the neutral repository contract and external durable adapters.
//! Canonical in-memory credential repository used by tests and fixtures.

use std::{collections::HashMap, sync::MutexGuard};

use awaken_sync::InvariantLock;

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[derive(Clone)]
pub(super) struct StoredRollout {
    event: ManagedCredentialRollout,
    delivered: bool,
}

impl From<ManagedCredentialRollout> for StoredRollout {
    fn from(event: ManagedCredentialRollout) -> Self {
        Self {
            event,
            delivered: false,
        }
    }
}

/// In-memory [`CredentialRepo`] for tests and scenario fixtures.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub(crate) struct RepoState {
    pub(super) rows: HashMap<String, CredentialSource>,
    pub(super) pools: HashMap<String, CredentialPool>,
    pub(super) intents: HashMap<String, CredentialMutationIntent>,
    pub(super) managed_mutations: HashMap<String, PendingManagedCredentialMutation>,
    pub(super) managed_rollouts: HashMap<String, StoredRollout>,
    pub(crate) vaults: HashMap<String, ManagedVault>,
    pub(crate) vault_credentials: HashMap<String, ManagedVaultCredential>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryCredentialRepo {
    pub(crate) state: InvariantLock<RepoState>,
}

#[cfg(any(test, feature = "test-support"))]
impl InMemoryCredentialRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The repository state: one mutex keeps every map consistent inside a
    /// transaction, so a poisoned state is refused as a storage failure.
    pub(crate) fn state(&self) -> Result<MutexGuard<'_, RepoState>, CredentialError> {
        Ok(self.state.lock()?)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl RepoState {
    // Called only after the complete source/child/rollout collision admission
    // under this state's one mutex; publication never creates a second fence.
    fn publish_rollout(&mut self, rollout: ManagedCredentialRollout) {
        if self
            .managed_rollouts
            .get(&rollout.id)
            .is_some_and(|stored| stored.delivered && stored.event != rollout)
        {
            self.managed_rollouts
                .insert(rollout.id.clone(), rollout.into());
        } else {
            self.managed_rollouts
                .entry(rollout.id.clone())
                .or_insert_with(|| rollout.into());
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl CredentialRepo for InMemoryCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        self.state()?.rows.insert(source.id.0.clone(), source);
        Ok(())
    }

    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        let mut state = self.state()?;
        Ok(state
            .rows
            .entry(source.id.0.clone())
            .or_insert(source)
            .clone())
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        self.state()?
            .rows
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::SourceNotFound(id.0.clone()))
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        Ok(self
            .state()?
            .rows
            .values()
            .filter(|s| s.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
        now_unix_ms: u64,
    ) -> Result<bool, CredentialError> {
        intent.validate_for_begin(now_unix_ms)?;
        let mut state = self.state()?;
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
        let before_matches = if intent.is_distinct_replacement() {
            current_after.is_none()
                && intent
                    .before
                    .as_ref()
                    .is_some_and(|before| state.rows.get(&before.id.0) == Some(before))
        } else {
            current_after == intent.before.as_ref()
        };
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
        let mut state = self.state()?;
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
        let mut state = self.state()?;
        let current_after = state.rows.get(&intent.after.id.0);
        let current_before = intent
            .before
            .as_ref()
            .filter(|before| before.id != intent.after.id)
            .and_then(|before| state.rows.get(&before.id.0));
        let durable = state.intents.get(&intent.after.id.0).ok_or_else(|| {
            CredentialError::MutationConflict(
                "credential mutation has no matching durable intent".into(),
            )
        })?;
        let plan = intent.publication_plan(durable, current_after, current_before)?;
        if plan.mode == CredentialMutationCommitMode::ExactReplay {
            return Ok(plan.reclaiming_mutation);
        }
        state.rows.insert(plan.source.id.0.clone(), plan.source);
        state
            .intents
            .insert(intent.after.id.0.clone(), plan.reclaiming_mutation.clone());
        Ok(plan.reclaiming_mutation)
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        self.state()?
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
        let mut state = self.state()?;
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
        let mut state = self.state()?;
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
        let mut state = self.state()?;
        let Some(durable) = state.intents.get(&intent.after.id.0) else {
            return Ok(());
        };
        intent.admit_completion(durable, state.rows.get(&intent.after.id.0))?;
        state.intents.remove(&intent.after.id.0);
        Ok(())
    }

    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Ok(self
            .state()?
            .rows
            .values()
            .flat_map(|source| source.material_refs().cloned())
            .collect())
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        self.state()?.pools.insert(pool.id.0.clone(), pool);
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        self.state()?
            .pools
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::PoolNotFound(id.0.clone()))
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        Ok(self
            .state()?
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
        now_unix_ms: u64,
    ) -> Result<bool, CredentialError> {
        pending
            .validate_for_begin(now_unix_ms)
            .map_err(invalid_pending_mutation)?;
        let mut state = self.state()?;
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
        let plan = managed_credential_commit_plan(pending)?;
        let mut state = self.state()?;
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
            if durable == plan.reclaiming_mutation
                && state.rows.get(&pending.after_source.id.0) == Some(&plan.source)
                && state.vault_credentials.get(&pending.after_credential.id)
                    == Some(&plan.credential)
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
        if plan.rollout.as_ref().is_some_and(|proposed| {
            state
                .managed_rollouts
                .get(&proposed.id)
                .is_some_and(|stored| !stored.delivered)
                && managed_credential_rollout_identity_decision(
                    state
                        .managed_rollouts
                        .get(&proposed.id)
                        .map(|stored| &stored.event),
                    proposed,
                ) == ManagedCredentialRolloutIdentityDecision::Collision
        }) {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        state.rows.insert(plan.source.id.0.clone(), plan.source);
        state
            .vault_credentials
            .insert(plan.credential.id.clone(), plan.credential);
        if let Some(rollout) = plan.rollout {
            state.publish_rollout(rollout);
        }
        state.managed_mutations.insert(
            pending.after_source.id.0.clone(),
            plan.reclaiming_mutation.clone(),
        );
        Ok(plan.reclaiming_mutation)
    }

    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_pending_mutation)?;
        let mut state = self.state()?;
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
        self.state()?
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
        let mut state = self.state()?;
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
        let mut state = self.state()?;
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
        let mut state = self.state()?;
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
            .state()?
            .managed_rollouts
            .values()
            .filter(|stored| !stored.delivered)
            .map(|stored| stored.event.clone())
            .collect::<Vec<_>>();
        events.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(events)
    }

    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError> {
        Ok(self
            .state()?
            .managed_rollouts
            .get(event_id)
            .map(|stored| stored.event.clone()))
    }

    async fn rearm_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        let mut state = self.state()?;
        let Some(stored) = state.managed_rollouts.get_mut(&rollout.id) else {
            return Err(CredentialError::MutationConflict(
                "Managed credential redelivery has no exact receipt".into(),
            ));
        };
        if stored.event != *rollout {
            return Err(CredentialError::MutationConflict(
                "Managed credential redelivery receipt conflicts".into(),
            ));
        }
        stored.delivered = false;
        Ok(())
    }

    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        let mut state = self.state()?;
        if state
            .managed_rollouts
            .get(&rollout.id)
            .is_some_and(|stored| stored.delivered)
        {
            // As before, repeated acknowledgement of an already completed key
            // is a no-op. Retained proof is never overwritten by that call.
            return Ok(());
        }
        match managed_credential_rollout_identity_decision(
            state
                .managed_rollouts
                .get(&rollout.id)
                .map(|stored| &stored.event),
            rollout,
        ) {
            ManagedCredentialRolloutIdentityDecision::Exact => {
                state
                    .managed_rollouts
                    .get_mut(&rollout.id)
                    .expect("exact receipt")
                    .delivered = true;
                let acknowledged = state
                    .managed_rollouts
                    .values()
                    .filter(|stored| stored.delivered)
                    .map(|stored| stored.event.clone())
                    .collect::<Vec<_>>();
                state.managed_rollouts.retain(|_, stored| {
                    !stored.delivered || !acknowledged.iter().any(|newer| {
                        awaken_credential_contract::managed_rollout_acknowledgement_superseded_by(
                            &stored.event,
                            newer,
                        )
                    })
                });
                Ok(())
            }
            ManagedCredentialRolloutIdentityDecision::Absent => Ok(()),
            ManagedCredentialRolloutIdentityDecision::Collision => {
                Err(CredentialError::MutationConflict(
                    "Managed credential rollout acknowledgement is stale".into(),
                ))
            }
        }
    }

    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError> {
        let mut vaults = self
            .state()?
            .vaults
            .values()
            .filter(|vault| vault.deletion_requested())
            .cloned()
            .collect::<Vec<_>>();
        vaults.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(vaults)
    }
}
