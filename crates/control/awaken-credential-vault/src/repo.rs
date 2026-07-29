//! The credential source repository port (ADR-0043) — stores the **secret-free**
//! [`CredentialSource`] rows (the sealed material lives behind [`SecretStore`], a
//! separate port). Its own `credential` migration scope is what lets the whole
//! domain be split into its own database/service (blast-radius isolation).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use crate::{
    CredentialCreateParams, CredentialError, CredentialKind, CredentialPool, CredentialPoolId,
    CredentialSource, CredentialSourceId, CredentialStatus, SecretStore, WorkerLocalBinding,
    prepare_source, validate_create_params,
};

/// Secret-free write-ahead intent for create, rotate, disable, archive, or
/// revoke. `before = None` is creation; every other change compares the exact
/// previous revision before atomically publishing `after`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CredentialMutationIntent {
    #[serde(default)]
    pub before: Option<CredentialSource>,
    #[serde(alias = "source")]
    pub after: CredentialSource,
}

/// Terminal material-reclaim outcome. Both variants make the source
/// non-materializable; `Archived` additionally communicates terminal retention
/// to management projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRetirement {
    Disable,
    Archive,
}

/// Material changes published as one credential revision. `primary = None`
/// preserves the compatibility primary slot; auxiliary `Some` values rotate a
/// named slot and `None` values remove it. Slot names are extension-owned.
#[derive(Default)]
pub struct CredentialMaterialPatch {
    pub primary: Option<awaken_agent_contract::RedactedString>,
    pub auxiliary: BTreeMap<String, Option<awaken_agent_contract::RedactedString>>,
}

/// The credential-source store port. Secret-free rows only. Pools are stored here
/// too (they are secret-free groupings of sources the resolver fails over across).
#[async_trait::async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError>;
    /// Atomically retain an existing source with the same id or insert `source`.
    /// The returned row is the durable winner.
    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError>;
    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError>;

    async fn begin_mutation(&self, intent: CredentialMutationIntent)
    -> Result<(), CredentialError>;
    /// Atomically compare/publish the source while retaining the WAL intent until
    /// material cleanup completes.
    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError>;
    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError>;
    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError>;
    /// Every material reference reachable from committed metadata.
    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Err(CredentialError::Storage(
            "credential material inventory is not supported by this repository".to_string(),
        ))
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError>;
    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError>;
    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError>;
}

/// In-memory [`CredentialRepo`] (dev / tests / single-machine default).
#[derive(Default)]
struct RepoState {
    rows: HashMap<String, CredentialSource>,
    pools: HashMap<String, CredentialPool>,
    intents: HashMap<String, CredentialMutationIntent>,
}

#[derive(Default)]
pub struct InMemoryCredentialRepo {
    state: Mutex<RepoState>,
}

impl InMemoryCredentialRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

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
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        match state.intents.entry(intent.after.id.0.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(intent);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() == &intent => Ok(()),
            std::collections::hash_map::Entry::Occupied(_) => Err(
                CredentialError::MutationConflict("another credential mutation is pending".into()),
            ),
        }
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        if state.intents.get(&intent.after.id.0) != Some(intent) {
            return Err(CredentialError::MutationConflict(
                "credential mutation has no matching durable intent".into(),
            ));
        }
        let current = state.rows.get(&intent.after.id.0);
        if current == Some(&intent.after) {
            return Ok(());
        }
        if current != intent.before.as_ref() {
            return Err(CredentialError::MutationConflict(
                "credential revision changed during mutation".into(),
            ));
        }
        state
            .rows
            .insert(intent.after.id.0.clone(), intent.after.clone());
        Ok(())
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .intents
            .values()
            .cloned()
            .collect())
    }

    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .intents
            .remove(&id.0);
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

/// Idempotently register one Worker-owned, non-secret local credential binding.
/// Primary-key identity is a collision-free length-prefixed projection of the
/// tuple, making the existing repository constraint the uniqueness authority.
pub async fn ensure_worker_local(
    repo: &dyn CredentialRepo,
    workspace_id: &str,
    binding: WorkerLocalBinding,
    provider_id: Option<String>,
) -> Result<CredentialSource, CredentialError> {
    if workspace_id.trim().is_empty()
        || binding.driver_id.trim().is_empty()
        || binding.subject_id.trim().is_empty()
    {
        return Err(CredentialError::InvalidSource(
            "worker-local workspace, driver, and subject must be non-empty".into(),
        ));
    }
    let segment = |value: &str| format!("{}:{value}", value.len());
    let id = CredentialSourceId(format!(
        "cred:worker-local:{}:{}:{}",
        segment(workspace_id),
        segment(&binding.driver_id),
        segment(&binding.subject_id)
    ));
    let expected = CredentialSource {
        id,
        workspace_id: workspace_id.to_string(),
        kind: CredentialKind::WorkerLocal,
        provider_id,
        env_key: None,
        material_ref: None,
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: Some(binding),
        status: CredentialStatus::Active,
        version: 1,
    };
    let durable = repo.put_if_absent(expected.clone()).await?;
    if durable.workspace_id != expected.workspace_id
        || durable.kind != CredentialKind::WorkerLocal
        || durable.provider_id != expected.provider_id
        || durable.worker_local_binding != expected.worker_local_binding
        || durable.env_key.is_some()
        || durable.material_ref.is_some()
        || !durable.auxiliary_material_refs.is_empty()
        || durable.oauth_command.is_some()
    {
        return Err(CredentialError::InvalidSource(format!(
            "worker-local binding conflicts with existing source {}",
            durable.id.0
        )));
    }
    Ok(durable)
}

/// Enter a credential end-to-end (secret-in / secret-free-out): seal the secret in
/// the [`SecretStore`], persist the secret-free row in the [`CredentialRepo`], and
/// return the row. The one write path an operator/the Managed wire drives.
pub async fn enter_credential(
    params: CredentialCreateParams,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    enter_credential_with_materials(params, BTreeMap::new(), store, repo).await
}

/// Enter one credential and every extension-defined auxiliary material slot as
/// a single recoverable aggregate revision.
pub async fn enter_credential_with_materials(
    params: CredentialCreateParams,
    auxiliary: BTreeMap<String, awaken_agent_contract::RedactedString>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    validate_create_params(&params)?;
    validate_material_slots(auxiliary.keys().map(String::as_str))?;
    let (mut source, secret) = prepare_source(params);
    let mut materials = Vec::new();
    if let (Some(reference), Some(secret)) = (source.material_ref.clone(), secret) {
        materials.push((reference, secret));
    }
    for (slot, secret) in auxiliary {
        let reference = material_ref_for(&source.id, source.version, &slot);
        source
            .auxiliary_material_refs
            .insert(slot, reference.clone());
        materials.push((reference, secret));
    }
    let intent = CredentialMutationIntent {
        before: None,
        after: source.clone(),
    };
    repo.begin_mutation(intent.clone()).await?;

    for (reference, secret) in materials {
        if let Err(error) = store.put(&reference, secret).await {
            // A failed put may still have partially written. Only retire the
            // durable intent after every candidate ref is idempotently clean.
            if cleanup_unpublished_material(&intent, store).await.is_ok() {
                repo.complete_mutation(&source.id).await?;
            }
            return Err(error);
        }
    }
    // Never compensate an ambiguous apply error inline: the intent remains the
    // recovery authority, preventing deletion of a source that actually committed.
    repo.apply_mutation(&intent).await?;
    repo.complete_mutation(&source.id).await?;
    Ok(source)
}

fn validate_material_slots<'a>(
    slots: impl Iterator<Item = &'a str>,
) -> Result<(), CredentialError> {
    for slot in slots {
        if slot.is_empty()
            || !slot
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(CredentialError::InvalidSource(format!(
                "credential material slot `{slot}` is invalid"
            )));
        }
    }
    Ok(())
}

fn material_ref_for(id: &CredentialSourceId, version: i64, slot: &str) -> crate::SecretRef {
    crate::SecretRef(format!("sec:{}:r{version}:{slot}", id.0))
}

/// Rotate an exact credential revision and all requested material slots in one
/// WAL/CAS transaction. A stale expected revision fails before any new secret is
/// written.
pub async fn rotate_credential_materials_exact(
    id: &CredentialSourceId,
    expected_version: i64,
    patch: CredentialMaterialPatch,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    validate_material_slots(patch.auxiliary.keys().map(String::as_str))?;
    let before = repo.get(id).await?;
    if before.version != expected_version {
        return Err(CredentialError::MutationConflict(
            "credential revision changed before material rotation".into(),
        ));
    }
    if before.kind != CredentialKind::Vault || before.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(id.0.clone()));
    }
    if patch.primary.is_none() && patch.auxiliary.is_empty() {
        return Ok(before);
    }
    let version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    let mut after = before.clone();
    after.version = version;
    let mut materials = Vec::new();
    if let Some(material) = patch.primary {
        let reference = material_ref_for(id, version, "primary");
        after.material_ref = Some(reference.clone());
        materials.push((reference, material));
    }
    for (slot, material) in patch.auxiliary {
        match material {
            Some(material) => {
                let reference = material_ref_for(id, version, &slot);
                after
                    .auxiliary_material_refs
                    .insert(slot, reference.clone());
                materials.push((reference, material));
            }
            None => {
                after.auxiliary_material_refs.remove(&slot);
            }
        }
    }
    let intent = CredentialMutationIntent {
        before: Some(before),
        after: after.clone(),
    };
    repo.begin_mutation(intent.clone()).await?;
    for (reference, material) in materials {
        if let Err(error) = store.put(&reference, material).await {
            if cleanup_unpublished_material(&intent, store).await.is_ok() {
                repo.complete_mutation(id).await?;
            }
            return Err(error);
        }
    }
    repo.apply_mutation(&intent).await?;
    cleanup_retired_material(&intent, store).await?;
    repo.complete_mutation(id).await?;
    Ok(after)
}

/// Rotate material from the currently committed revision. Callers that retain
/// an execution pin should use [`rotate_credential_materials_exact`] instead.
pub async fn rotate_credential_materials(
    id: &CredentialSourceId,
    patch: CredentialMaterialPatch,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let version = repo.get(id).await?.version;
    rotate_credential_materials_exact(id, version, patch, store, repo).await
}

/// Rotate the material of one active Vault source without ever overwriting the
/// reference used by an older exact revision.
pub async fn rotate_credential(
    id: &CredentialSourceId,
    material: awaken_agent_contract::RedactedString,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    rotate_credential_materials(
        id,
        CredentialMaterialPatch {
            primary: Some(material),
            auxiliary: BTreeMap::new(),
        },
        store,
        repo,
    )
    .await
}

/// Publish a higher exact revision for executable, secret-free configuration
/// changes while retaining the complete material set.
pub async fn advance_credential_revision(
    id: &CredentialSourceId,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let before = repo.get(id).await?;
    if before.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(id.0.clone()));
    }
    let mut after = before.clone();
    after.version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    let intent = CredentialMutationIntent {
        before: Some(before),
        after: after.clone(),
    };
    repo.begin_mutation(intent.clone()).await?;
    repo.apply_mutation(&intent).await?;
    repo.complete_mutation(id).await?;
    Ok(after)
}

/// Change only lifecycle availability while retaining material. This is the
/// canonical reversible transition for staged publication; retirement/reclaim
/// remains a separate terminal operation.
pub async fn transition_credential_status(
    id: &CredentialSourceId,
    status: CredentialStatus,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let before = repo.get(id).await?;
    if before.status == status {
        return Ok(before);
    }
    let mut after = before.clone();
    after.version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    after.status = status;
    let intent = CredentialMutationIntent {
        before: Some(before),
        after: after.clone(),
    };
    repo.begin_mutation(intent.clone()).await?;
    repo.apply_mutation(&intent).await?;
    repo.complete_mutation(id).await?;
    Ok(after)
}

/// Terminally revoke one source: publish a higher disabled/archived revision
/// first, then erase its material while the WAL intent remains recoverable.
pub async fn revoke_credential(
    id: &CredentialSourceId,
    retirement: CredentialRetirement,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let before = repo.get(id).await?;
    let mut after = before.clone();
    after.version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    after.status = match retirement {
        CredentialRetirement::Disable => CredentialStatus::Disabled,
        CredentialRetirement::Archive => CredentialStatus::Archived,
    };
    after.material_ref = None;
    after.auxiliary_material_refs.clear();
    let intent = CredentialMutationIntent {
        before: Some(before),
        after: after.clone(),
    };
    repo.begin_mutation(intent.clone()).await?;
    repo.apply_mutation(&intent).await?;
    cleanup_retired_material(&intent, store).await?;
    repo.complete_mutation(id).await?;
    Ok(after)
}

async fn cleanup_retired_material(
    intent: &CredentialMutationIntent,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    let before = intent
        .before
        .iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<HashSet<_>>();
    let after = intent.after.material_refs().collect::<HashSet<_>>();
    for reference in before.difference(&after) {
        store.delete(reference).await?;
    }
    Ok(())
}

async fn cleanup_unpublished_material(
    intent: &CredentialMutationIntent,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    let before = intent
        .before
        .iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<HashSet<_>>();
    let after = intent.after.material_refs().collect::<HashSet<_>>();
    for reference in after.difference(&before) {
        store.delete(reference).await?;
    }
    Ok(())
}

/// Reconcile every interrupted create/rotate/revoke after restart. The exact
/// durable source decides whether old or new material is retained.
pub async fn recover_credential_mutations(
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<usize, CredentialError> {
    let intents = repo.pending_mutations().await?;
    let mut recovered = 0;
    for intent in intents {
        let current = match repo.get(&intent.after.id).await {
            Ok(source) => Some(source),
            Err(CredentialError::SourceNotFound(_)) => None,
            Err(error) => return Err(error),
        };
        if current.as_ref() == Some(&intent.after) {
            cleanup_retired_material(&intent, store).await?;
        } else if current == intent.before {
            cleanup_unpublished_material(&intent, store).await?;
        } else {
            return Err(CredentialError::MutationConflict(format!(
                "credential {} no longer matches its pending mutation",
                intent.after.id.0
            )));
        }
        repo.complete_mutation(&intent.after.id).await?;
        recovered += 1;
    }
    Ok(recovered)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInventoryReport {
    pub orphaned_deleted: Vec<crate::SecretRef>,
    pub missing_material: Vec<crate::SecretRef>,
}

/// Compare the secret inventory with committed metadata and in-flight intents.
/// Only credential-owned `sec:cred:` keys are eligible for deletion; webhook and
/// other domains' material may share the physical store and is deliberately
/// untouched.
pub async fn reconcile_credential_inventory(
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialInventoryReport, CredentialError> {
    let inventory = store.inventory().await?;
    // Read intents first. Publication atomically removes an intent while adding
    // its metadata row, so either this read sees the in-flight protection or the
    // following committed-reference read sees the published source.
    let pending = repo.pending_mutations().await?;
    let committed = repo.material_refs().await?;
    let present: HashSet<String> = inventory.iter().map(|item| item.0.clone()).collect();
    let committed_keys: HashSet<String> = committed.iter().map(|item| item.0.clone()).collect();
    let protected: HashSet<String> = committed_keys
        .iter()
        .cloned()
        .chain(pending.iter().flat_map(|intent| {
            intent
                .before
                .iter()
                .flat_map(CredentialSource::material_refs)
                .chain(intent.after.material_refs())
                .map(|reference| reference.0.clone())
        }))
        .collect();

    let mut orphaned_deleted = Vec::new();
    for reference in inventory {
        if reference.0.starts_with("sec:cred:") && !protected.contains(&reference.0) {
            store.delete(&reference).await?;
            orphaned_deleted.push(reference);
        }
    }
    let missing_material = committed
        .into_iter()
        .filter(|reference| !present.contains(&reference.0))
        .collect();
    Ok(CredentialInventoryReport {
        orphaned_deleted,
        missing_material,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::{CredentialKind, InMemorySecretStore, materialize};
    use awaken_agent_contract::RedactedString;

    struct FaultyDeleteStore {
        inner: InMemorySecretStore,
        fail_before_delete: AtomicBool,
        lose_first_response: AtomicBool,
    }

    #[async_trait::async_trait]
    impl SecretStore for FaultyDeleteStore {
        async fn put(
            &self,
            r: &crate::SecretRef,
            secret: RedactedString,
        ) -> Result<(), CredentialError> {
            self.inner.put(r, secret).await
        }

        async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
            self.inner.get(r).await
        }

        async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
            if self.fail_before_delete.load(Ordering::SeqCst) {
                return Err(CredentialError::Storage("delete timeout".into()));
            }
            self.inner.delete(r).await?;
            if self.lose_first_response.swap(false, Ordering::SeqCst) {
                return Err(CredentialError::Storage("delete response lost".into()));
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RejectingRepo {
        inner: InMemoryCredentialRepo,
    }

    #[async_trait::async_trait]
    impl CredentialRepo for RejectingRepo {
        async fn put(&self, _source: CredentialSource) -> Result<(), CredentialError> {
            Err(CredentialError::Storage("injected row failure".into()))
        }

        async fn put_if_absent(
            &self,
            _source: CredentialSource,
        ) -> Result<CredentialSource, CredentialError> {
            Err(CredentialError::Storage("injected row failure".into()))
        }

        async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
            Err(CredentialError::SourceNotFound(id.0.clone()))
        }

        async fn list(
            &self,
            _workspace_id: &str,
        ) -> Result<Vec<CredentialSource>, CredentialError> {
            Ok(Vec::new())
        }

        async fn begin_mutation(
            &self,
            intent: CredentialMutationIntent,
        ) -> Result<(), CredentialError> {
            self.inner.begin_mutation(intent).await
        }

        async fn apply_mutation(
            &self,
            _intent: &CredentialMutationIntent,
        ) -> Result<(), CredentialError> {
            Err(CredentialError::Storage("injected row failure".into()))
        }

        async fn pending_mutations(
            &self,
        ) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
            self.inner.pending_mutations().await
        }

        async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
            self.inner.complete_mutation(id).await
        }

        async fn put_pool(&self, _pool: CredentialPool) -> Result<(), CredentialError> {
            Ok(())
        }

        async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
            Err(CredentialError::PoolNotFound(id.0.clone()))
        }

        async fn list_pools(
            &self,
            _workspace_id: &str,
        ) -> Result<Vec<CredentialPool>, CredentialError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn enter_stores_row_and_secret_separately() {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-xyz")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();

        // The row is retrievable and secret-free; the secret materializes from the store.
        let got = repo.get(&source.id).await.unwrap();
        assert!(!serde_json::to_string(&got).unwrap().contains("sk-xyz"));
        assert_eq!(
            materialize(&got, &store).await.unwrap().expose_secret(),
            "sk-xyz"
        );
        assert_eq!(repo.list("ws").await.unwrap().len(), 1);
        assert_eq!(repo.list("other").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn ambiguous_row_commit_is_reconciled_from_the_durable_intent() {
        let store = InMemorySecretStore::new();
        let repo = RejectingRepo::default();
        let result = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("must-not-be-orphaned")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await;

        assert!(
            matches!(result, Err(CredentialError::Storage(message)) if message == "injected row failure")
        );
        // An ambiguous commit error is not compensated inline: deleting here could
        // break a row whose commit succeeded but whose response was lost.
        assert_eq!(store.map.lock().expect("secret store mutex").len(), 1);
        assert_eq!(
            recover_credential_mutations(&store, &repo).await.unwrap(),
            1
        );
        assert!(store.map.lock().expect("secret store mutex").is_empty());
        assert!(repo.pending_mutations().await.unwrap().is_empty());
    }

    async fn interrupted_creation(store: &dyn SecretStore, repo: &dyn CredentialRepo) {
        let source = CredentialSource {
            id: CredentialSourceId("cred:ws:interrupted".into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: None,
            material_ref: Some(crate::SecretRef("sec:cred:ws:interrupted".into())),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: crate::CredentialStatus::Active,
            version: 1,
        };
        repo.begin_mutation(CredentialMutationIntent {
            before: None,
            after: source.clone(),
        })
        .await
        .unwrap();
        store
            .put(
                source.material_ref.as_ref().unwrap(),
                RedactedString::new("orphan candidate"),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_timeout_keeps_the_intent_for_a_later_reconciliation() {
        let store = FaultyDeleteStore {
            inner: InMemorySecretStore::new(),
            fail_before_delete: AtomicBool::new(true),
            lose_first_response: AtomicBool::new(false),
        };
        let repo = InMemoryCredentialRepo::new();
        interrupted_creation(&store, &repo).await;
        assert!(recover_credential_mutations(&store, &repo).await.is_err());
        assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);
        store.fail_before_delete.store(false, Ordering::SeqCst);
        assert_eq!(
            recover_credential_mutations(&store, &repo).await.unwrap(),
            1
        );
        assert!(repo.pending_mutations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lost_delete_response_is_retried_idempotently() {
        let store = FaultyDeleteStore {
            inner: InMemorySecretStore::new(),
            fail_before_delete: AtomicBool::new(false),
            lose_first_response: AtomicBool::new(true),
        };
        let repo = InMemoryCredentialRepo::new();
        interrupted_creation(&store, &repo).await;
        assert!(recover_credential_mutations(&store, &repo).await.is_err());
        assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);
        assert_eq!(
            recover_credential_mutations(&store, &repo).await.unwrap(),
            1
        );
        assert!(repo.pending_mutations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rotation_publishes_a_new_revision_and_reclaims_the_old_material() {
        // Causes: L2 active Vault source + replacement material. Effects: source
        // revision/ref advance together, new material resolves, old ref is erased,
        // and no WAL remains. This is decision-table rule rotate/success.
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let before = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("old")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let old_ref = before.material_ref.clone().unwrap();

        let after = rotate_credential(&before.id, RedactedString::new("new"), &store, &repo)
            .await
            .unwrap();

        assert_eq!(after.version, before.version + 1);
        assert_ne!(after.material_ref, before.material_ref);
        assert_eq!(
            materialize(&after, &store).await.unwrap().expose_secret(),
            "new"
        );
        assert!(store.get(&old_ref).await.is_err());
        assert!(repo.pending_mutations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retirement_fails_closed_and_reclaims_material() {
        // Causes: L3 active source + Archive retirement. Effects: a higher
        // archived revision with no material ref is durable, the secret is erased,
        // and both direct and pinned materialization fail closed.
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let before = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("retire-me")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let old_ref = before.material_ref.clone().unwrap();

        let after = revoke_credential(&before.id, CredentialRetirement::Archive, &store, &repo)
            .await
            .unwrap();

        assert_eq!(after.status, CredentialStatus::Archived);
        assert_eq!(after.version, before.version + 1);
        assert!(after.material_ref.is_none());
        assert!(store.get(&old_ref).await.is_err());
        assert!(matches!(
            materialize(&after, &store).await,
            Err(CredentialError::NotActive(_))
        ));
    }

    #[tokio::test]
    async fn reversible_status_transition_retains_material_and_advances_revision() {
        // Decision rule L4: active source + reversible Disabled transition =>
        // higher fail-closed revision, unchanged material ref/bytes, no pending
        // WAL. A repeated identical transition is an idempotent no-op.
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let before = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("retained")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let disabled = transition_credential_status(&before.id, CredentialStatus::Disabled, &repo)
            .await
            .unwrap();
        assert_eq!(disabled.version, before.version + 1);
        assert_eq!(disabled.material_ref, before.material_ref);
        assert_eq!(
            store
                .get(disabled.material_ref.as_ref().unwrap())
                .await
                .unwrap()
                .expose_secret(),
            "retained"
        );
        assert_eq!(
            transition_credential_status(&disabled.id, CredentialStatus::Disabled, &repo,)
                .await
                .unwrap(),
            disabled
        );
        assert!(repo.pending_mutations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recovery_uses_the_durable_side_of_a_rotation_as_cleanup_authority() {
        // Mutation recovery decision table:
        // R6 current==before => delete unpublished after-ref and keep old-ref.
        // R7 current==after  => delete retired old-ref and keep new-ref.
        for publish_after in [false, true] {
            let store = InMemorySecretStore::new();
            let repo = InMemoryCredentialRepo::new();
            let before = enter_credential(
                CredentialCreateParams {
                    workspace_id: "ws".into(),
                    kind: CredentialKind::Vault,
                    provider_id: None,
                    env_key: None,
                    secret: Some(RedactedString::new("old")),
                    oauth_command: None,
                },
                &store,
                &repo,
            )
            .await
            .unwrap();
            let mut after = before.clone();
            after.version += 1;
            after.material_ref = Some(crate::SecretRef(format!(
                "sec:{}:r{}",
                after.id.0, after.version
            )));
            let intent = CredentialMutationIntent {
                before: Some(before.clone()),
                after: after.clone(),
            };
            repo.begin_mutation(intent.clone()).await.unwrap();
            store
                .put(
                    after.material_ref.as_ref().unwrap(),
                    RedactedString::new("new"),
                )
                .await
                .unwrap();
            if publish_after {
                repo.apply_mutation(&intent).await.unwrap();
            }

            assert_eq!(
                recover_credential_mutations(&store, &repo).await.unwrap(),
                1
            );
            let durable = repo.get(&before.id).await.unwrap();
            if publish_after {
                assert_eq!(durable, after);
                assert!(
                    store
                        .get(before.material_ref.as_ref().unwrap())
                        .await
                        .is_err()
                );
                assert!(
                    store
                        .get(after.material_ref.as_ref().unwrap())
                        .await
                        .is_ok()
                );
            } else {
                assert_eq!(durable, before);
                assert!(
                    store
                        .get(before.material_ref.as_ref().unwrap())
                        .await
                        .is_ok()
                );
                assert!(
                    store
                        .get(after.material_ref.as_ref().unwrap())
                        .await
                        .is_err()
                );
            }
        }
    }

    #[tokio::test]
    async fn mutation_rejects_a_competing_revision_and_retains_recovery_evidence() {
        // Causes: R8 matching WAL but current is neither exact before nor after.
        // Effects: publication fails with MutationConflict and the WAL remains so
        // no material can be silently reclaimed against ambiguous metadata.
        let repo = InMemoryCredentialRepo::new();
        let before = CredentialSource {
            id: CredentialSourceId("cred:ws:conflict".into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            material_ref: Some(crate::SecretRef("sec:cred:ws:conflict".into())),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        repo.put(before.clone()).await.unwrap();
        let mut after = before.clone();
        after.version = 2;
        let intent = CredentialMutationIntent {
            before: Some(before.clone()),
            after,
        };
        repo.begin_mutation(intent.clone()).await.unwrap();
        let mut competing = before;
        competing.version = 3;
        repo.put(competing).await.unwrap();

        assert!(matches!(
            repo.apply_mutation(&intent).await,
            Err(CredentialError::MutationConflict(_))
        ));
        assert_eq!(repo.pending_mutations().await.unwrap(), vec![intent]);
    }

    #[tokio::test]
    async fn inventory_deletes_only_unreferenced_credential_material() {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let committed = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("kept")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let orphan = crate::SecretRef("sec:cred:ws:legacy-orphan".into());
        let webhook = crate::SecretRef("whsec:shared-store".into());
        store
            .put(&orphan, RedactedString::new("delete"))
            .await
            .unwrap();
        store
            .put(&webhook, RedactedString::new("preserve"))
            .await
            .unwrap();

        let report = reconcile_credential_inventory(&store, &repo).await.unwrap();
        assert_eq!(report.orphaned_deleted, vec![orphan.clone()]);
        assert!(report.missing_material.is_empty());
        assert!(store.get(&orphan).await.is_err());
        assert!(store.get(&webhook).await.is_ok());
        assert!(
            store
                .get(committed.material_ref.as_ref().unwrap())
                .await
                .is_ok()
        );
        assert!(
            reconcile_credential_inventory(&store, &repo)
                .await
                .unwrap()
                .orphaned_deleted
                .is_empty()
        );
    }

    #[tokio::test]
    async fn inventory_reports_metadata_whose_secret_is_missing() {
        let repo = InMemoryCredentialRepo::new();
        let store = InMemorySecretStore::new();
        let reference = crate::SecretRef("sec:cred:ws:missing".into());
        let source = CredentialSource {
            id: CredentialSourceId("cred:ws:missing".into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            material_ref: Some(reference.clone()),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: crate::CredentialStatus::Active,
            version: 1,
        };
        repo.put(source.clone()).await.unwrap();

        let report = reconcile_credential_inventory(&store, &repo).await.unwrap();
        assert_eq!(report.missing_material, vec![reference]);
        assert!(materialize(&source, &store).await.is_err());
    }

    #[tokio::test]
    async fn pools_round_trip_and_list_scopes_by_workspace() {
        let repo = InMemoryCredentialRepo::new();
        let pool = |id: &str, ws: &str| CredentialPool {
            id: CredentialPoolId(id.into()),
            workspace_id: ws.into(),
            members: Vec::new(),
            policy: crate::SelectionPolicy::FirstHealthy,
        };
        repo.put_pool(pool("pool:a", "ws")).await.unwrap();
        repo.put_pool(pool("pool:b", "ws")).await.unwrap();
        repo.put_pool(pool("pool:c", "other")).await.unwrap();

        let got = repo
            .get_pool(&CredentialPoolId("pool:a".into()))
            .await
            .unwrap();
        assert_eq!(got.workspace_id, "ws");
        assert_eq!(repo.list_pools("ws").await.unwrap().len(), 2);
        assert_eq!(repo.list_pools("other").await.unwrap().len(), 1);
        assert_eq!(repo.list_pools("empty").await.unwrap().len(), 0);
    }

    /// F11(d): `get` is a deliberate **unscoped by-id primitive** — keyed by source
    /// id only, never by workspace (only `list`/`list_pools` filter by workspace).
    /// Tenancy is NOT enforced on this low-level read; it is enforced one layer up,
    /// at credential *resolution* (`awaken_config_resolver::resolve_credential`),
    /// which is where both the binding's and the source's workspace are known: a
    /// pool member whose source belongs to another workspace is skipped, and an
    /// `Exact` binding on a cross-workspace source fails closed (`SourceMissing`).
    ///
    /// Audit of every `CredentialRepo::get` caller confirms none performs an
    /// unfenced cross-workspace *secret materialization*:
    /// - `awaken-runtime-host::PrefetchedSourceLookup::for_defs` (managed MCP)
    ///   prefetches by id, then materializes only through the fenced resolver;
    /// - the admin-config-api routes (`get_credential`, `archive_credential`,
    ///   `put_mcp_server` existence-check) are IAM-gated by-id management ops that
    ///   read a **secret-free** row / mutate status — they never materialize a
    ///   secret; the sealed material stays behind `SecretStore`.
    ///
    /// So keeping `get` unscoped is correct: it is the shared read primitive, and
    /// the tenant fence lives at the resolution seam. This test pins that primitive.
    #[tokio::test]
    async fn get_is_an_unscoped_by_id_primitive_fenced_at_resolution() {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let owned = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws-owner".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("secret")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();

        // `list` for an unrelated workspace correctly hides the row...
        assert_eq!(repo.list("ws-other").await.unwrap().len(), 0);
        // ...but a direct `get` with the id returns it regardless of workspace.
        let cross_read = repo.get(&owned.id).await.unwrap();
        assert_eq!(cross_read.workspace_id, "ws-owner");
    }

    #[tokio::test]
    async fn a_missing_source_or_pool_is_not_found() {
        let repo = InMemoryCredentialRepo::new();
        assert!(matches!(
            repo.get(&CredentialSourceId("cred:absent".into())).await,
            Err(CredentialError::SourceNotFound(id)) if id == "cred:absent"
        ));
        assert!(matches!(
            repo.get_pool(&CredentialPoolId("pool:absent".into())).await,
            Err(CredentialError::PoolNotFound(id)) if id == "pool:absent"
        ));
    }
}
