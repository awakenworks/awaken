use super::*;
use crate::persist_material_exact;

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
        protocol_endpoint_id: None,
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

/// Result of an idempotent credential-entry command.
pub struct CredentialEntry {
    pub source: CredentialSource,
    pub created: bool,
}

/// Enter a credential once at a stable application-command identity.
///
/// Replaying the same identity returns the durable source without writing
/// material again. A conflicting identity projection fails closed rather than
/// treating two different credential commands as equivalent.
pub async fn enter_credential_idempotent(
    id: CredentialSourceId,
    params: CredentialCreateParams,
    protocol_endpoint_id: Option<String>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialEntry, CredentialError> {
    validate_create_params(&params)?;
    if protocol_endpoint_id
        .as_deref()
        .is_some_and(|endpoint| endpoint.trim().is_empty())
    {
        return Err(CredentialError::InvalidSource(
            "credential endpoint scope must not be empty".into(),
        ));
    }
    if protocol_endpoint_id.is_some() && params.provider_id.is_none() {
        return Err(CredentialError::InvalidSource(
            "an endpoint-scoped credential requires a provider".into(),
        ));
    }
    let (mut expected, secret) = prepare_source_with_id(id, params);
    expected.protocol_endpoint_id = protocol_endpoint_id;
    enter_prepared_credential_idempotent(expected, secret, store, repo).await
}

/// Enter one stable Vault credential and verify that every replay names the
/// same sealed material. The operation identity remains caller-owned; the
/// credential aggregate owns the create WAL, source CAS, and material check.
pub async fn enter_credential_idempotent_verified(
    id: CredentialSourceId,
    params: CredentialCreateParams,
    protocol_endpoint_id: Option<String>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialEntry, CredentialError> {
    let expected_material = params.secret.clone();
    let entry = enter_credential_idempotent(id, params, protocol_endpoint_id, store, repo).await?;
    if let Some(expected) = expected_material {
        let reference = entry
            .source
            .material_ref
            .as_ref()
            .ok_or_else(|| CredentialError::MissingMaterialRef(entry.source.id.0.clone()))?;
        let actual = store.get(reference).await?;
        if actual.expose_secret() != expected.expose_secret() {
            return Err(CredentialError::MutationConflict(
                "credential Idempotency-Key was reused with different material".into(),
            ));
        }
    }
    Ok(entry)
}

pub(super) async fn enter_prepared_credential_idempotent(
    expected: CredentialSource,
    secret: Option<awaken_agent_contract::RedactedString>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialEntry, CredentialError> {
    match repo.get(&expected.id).await {
        Ok(source) => {
            validate_idempotent_source(&source, &expected)?;
            return Ok(CredentialEntry {
                source,
                created: false,
            });
        }
        Err(CredentialError::SourceNotFound(_)) => {}
        Err(error) => return Err(error),
    }

    match enter_prepared_credential(expected.clone(), secret, BTreeMap::new(), store, repo).await {
        Ok(source) => Ok(CredentialEntry {
            source,
            created: true,
        }),
        Err(conflict @ CredentialError::MutationConflict(_)) => {
            match repo.get(&expected.id).await {
                Ok(source) => {
                    validate_idempotent_source(&source, &expected)?;
                    Ok(CredentialEntry {
                        source,
                        created: false,
                    })
                }
                Err(CredentialError::SourceNotFound(_)) => Err(conflict),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn validate_idempotent_source(
    actual: &CredentialSource,
    expected: &CredentialSource,
) -> Result<(), CredentialError> {
    if actual.id != expected.id
        || actual.workspace_id != expected.workspace_id
        || actual.kind != expected.kind
        || actual.provider_id != expected.provider_id
        || actual.protocol_endpoint_id != expected.protocol_endpoint_id
        || actual.env_key != expected.env_key
        || actual.material_ref != expected.material_ref
        || actual.oauth_command != expected.oauth_command
        || actual.worker_local_binding.is_some()
        || !actual.auxiliary_material_refs.is_empty()
    {
        return Err(CredentialError::InvalidSource(format!(
            "idempotent credential identity conflicts with existing source {}",
            actual.id.0
        )));
    }
    Ok(())
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
    let (source, secret) = prepare_source(params);
    enter_prepared_credential(source, secret, auxiliary, store, repo).await
}

/// Create one Managed Vault child and its executable Source through the single
/// Credential persistence boundary. Secret material may exist while the
/// durable pending fact is recoverable, but the executable Source and Managed
/// child become visible only in the repository's one atomic commit.
pub async fn create_managed_credential(
    command: ManagedCredentialCreateCommand,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<(CredentialSource, ManagedVaultCredential), ManagedCredentialCreationError> {
    validate_create_params(&command.source)?;
    validate_material_slots(command.auxiliary_materials.keys().map(String::as_str))?;
    let (mut source, secret) = match command.source_id {
        Some(id) => prepare_source_with_id(id, command.source),
        None => prepare_source(command.source),
    };
    source.protocol_endpoint_id = command.protocol_endpoint_id;
    if command.primary_material_ref.is_some() {
        source.material_ref = command.primary_material_ref;
    }
    let mut materials = Vec::new();
    if let (Some(reference), Some(secret)) = (source.material_ref.clone(), secret) {
        materials.push((reference, secret));
    }
    for (slot, secret) in command.auxiliary_materials {
        let reference = material_ref_for(&source.id, source.version, &slot);
        source
            .auxiliary_material_refs
            .insert(slot, reference.clone());
        materials.push((reference, secret));
    }
    let credential = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, credential)?;
    let committed = execute_managed_mutation(pending, materials, store, repo).await?;
    Ok((committed.after_source, committed.after_credential))
}

async fn cleanup_retired_managed_material(
    pending: &PendingManagedCredentialMutation,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    let before = pending
        .before_source
        .iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<HashSet<_>>();
    let after = pending.after_source.material_refs().collect::<HashSet<_>>();
    for reference in before.difference(&after) {
        store.delete(reference).await?;
    }
    Ok(())
}

async fn cleanup_unpublished_managed_material(
    pending: &PendingManagedCredentialMutation,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    let before = pending
        .before_source
        .iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<HashSet<_>>();
    let after = pending.after_source.material_refs().collect::<HashSet<_>>();
    for reference in after.difference(&before) {
        store.delete(reference).await?;
    }
    Ok(())
}

/// Durably retain the exact abort cleanup authority before compensating its
/// external writes. A stale writer must never delete material after another
/// owner has claimed or published the same pending fact. If the exact
/// transition fails, no external material is touched; if deletion fails or the
/// process crashes, `ReclaimingAbort` remains available to recovery.
async fn abort_managed_mutation_before_cleanup(
    pending: &PendingManagedCredentialMutation,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<(), CredentialError> {
    let reclaiming = repo.abort_managed_mutation(pending).await?;
    cleanup_unpublished_managed_material(&reclaiming, store).await?;
    repo.complete_managed_mutation(&reclaiming).await
}

/// Reconcile interrupted Managed creation after every possible crash prefix.
/// A complete material set is published atomically; a partial set is reclaimed
/// before the exact pending fact is retired. Transient store failures retain the
/// pending fact for the next supervised pass.
pub async fn recover_managed_credential_mutations(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<usize, ManagedCredentialCreationError> {
    recover_managed_credential_mutations_at(store, repo, managed_credential_now_unix_ms()?).await
}

/// Periodic reconciliation never races a live writer. It resumes `Ready` and
/// `Reclaiming` work immediately, but must atomically fence an expired
/// `Writing` lease before inspecting or reclaiming its material.
pub async fn reconcile_ready_managed_credential_mutations(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<usize, ManagedCredentialCreationError> {
    recover_managed_credential_mutations_at(store, repo, managed_credential_now_unix_ms()?).await
}

async fn recover_managed_credential_mutations_at(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
    now_unix_ms: u64,
) -> Result<usize, ManagedCredentialCreationError> {
    let pending = repo.pending_managed_mutations().await?;
    let mut recovered = 0;
    let mut first_error = None;
    for mutation in pending {
        match recover_managed_credential_mutation(&mutation, store, repo, now_unix_ms).await {
            Ok(true) => recovered += 1,
            Ok(false) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(recovered), Err)
}

async fn recover_managed_credential_mutation(
    pending: &PendingManagedCredentialMutation,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
    now_unix_ms: u64,
) -> Result<bool, ManagedCredentialCreationError> {
    let mut mutation = pending.clone();
    if mutation.phase == ManagedCredentialMutationPhase::Writing {
        let lease_expires_at_unix_ms = now_unix_ms
            .checked_add(MANAGED_CREDENTIAL_WRITER_LEASE_MS)
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "Managed credential recovery lease deadline overflowed".into(),
                )
            })?;
        let Some(claimed) = repo
            .claim_expired_managed_mutation(&mutation, now_unix_ms, lease_expires_at_unix_ms)
            .await?
        else {
            return Ok(false);
        };
        mutation = claimed;
    }
    match mutation.phase {
        ManagedCredentialMutationPhase::Reclaiming => {
            cleanup_retired_managed_material(&mutation, store).await?;
            repo.complete_managed_mutation(&mutation).await?;
            return Ok(true);
        }
        ManagedCredentialMutationPhase::ReclaimingAbort => {
            cleanup_unpublished_managed_material(&mutation, store).await?;
            repo.complete_managed_mutation(&mutation).await?;
            return Ok(true);
        }
        ManagedCredentialMutationPhase::Writing | ManagedCredentialMutationPhase::Ready => {}
    }
    let mut material_complete = true;
    let before = mutation
        .before_source
        .iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<HashSet<_>>();
    for reference in mutation.after_source.material_refs() {
        if before.contains(reference) {
            continue;
        }
        match store.get(reference).await {
            Ok(_) => {}
            Err(CredentialError::SecretNotFound(_)) => material_complete = false,
            Err(error) => return Err(error.into()),
        }
    }
    if material_complete {
        if mutation.phase == ManagedCredentialMutationPhase::Writing {
            mutation = repo.mark_managed_mutation_ready(&mutation).await?;
        }
        match repo.commit_managed_mutation(&mutation).await {
            Ok(reclaiming) => {
                cleanup_retired_managed_material(&reclaiming, store).await?;
                repo.complete_managed_mutation(&reclaiming).await?;
            }
            Err(ManagedCredentialMutationError::Store(error)) => return Err(error.into()),
            Err(_) => {
                abort_managed_mutation_before_cleanup(&mutation, store, repo).await?;
            }
        }
    } else {
        abort_managed_mutation_before_cleanup(&mutation, store, repo).await?;
    }
    Ok(true)
}

fn fence_managed_material_refs(
    pending: &mut PendingManagedCredentialMutation,
    materials: &mut [(crate::SecretRef, awaken_agent_contract::RedactedString)],
) -> Result<(), ManagedCredentialMutationError> {
    if materials.is_empty() {
        return Ok(());
    }
    if !pending.has_valid_writer_owner() {
        return Err(ManagedCredentialMutationError::RevisionConflict);
    }
    for (reference, _) in materials {
        let old = reference.clone();
        let fenced = crate::SecretRef(format!("{}:attempt:{}", old.0, pending.attempt_id));
        let mut replaced = false;
        if pending.after_source.material_ref.as_ref() == Some(&old) {
            pending.after_source.material_ref = Some(fenced.clone());
            replaced = true;
        }
        for candidate in pending.after_source.auxiliary_material_refs.values_mut() {
            if *candidate == old {
                *candidate = fenced.clone();
                replaced = true;
            }
        }
        if pending.after_source.material_ref.as_ref() == Some(&fenced)
            || pending
                .after_source
                .auxiliary_material_refs
                .values()
                .any(|candidate| candidate == &fenced)
        {
            replaced = true;
        }
        if !replaced {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        *reference = fenced;
    }
    Ok(())
}

async fn execute_managed_mutation(
    mut pending: PendingManagedCredentialMutation,
    mut materials: Vec<(crate::SecretRef, awaken_agent_contract::RedactedString)>,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
    // A late effect from a fenced writer may leave an orphan, but it can never
    // overwrite a later attempt's material. Inventory reconciliation reports
    // such unowned material for a backend-specific exact collector.
    fence_managed_material_refs(&mut pending, &mut materials)?;
    repo.begin_managed_mutation(pending.clone()).await?;
    for (reference, material) in materials {
        if let Err(error) = persist_material_exact(store, &reference, material).await {
            if let Err(cleanup) = abort_managed_mutation_before_cleanup(&pending, store, repo).await
            {
                return Err(ManagedCredentialMutationError::Compensation {
                    primary: error.to_string(),
                    cleanup: cleanup.to_string(),
                });
            }
            return Err(error.into());
        }
    }
    if pending.phase == ManagedCredentialMutationPhase::Writing {
        pending = repo.mark_managed_mutation_ready(&pending).await?;
    }
    let reclaiming = match repo.commit_managed_mutation(&pending).await {
        Ok(reclaiming) => reclaiming,
        Err(error @ ManagedCredentialMutationError::Store(_)) => return Err(error),
        Err(error) => {
            if let Err(cleanup) = abort_managed_mutation_before_cleanup(&pending, store, repo).await
            {
                return Err(ManagedCredentialMutationError::Compensation {
                    primary: error.to_string(),
                    cleanup: cleanup.to_string(),
                });
            }
            return Err(error);
        }
    };
    // The pair and its rollout event are already committed. Cleanup is a
    // retryable after-commit obligation: surfacing its failure to the command
    // caller would invite a duplicate logical mutation. Leave `Reclaiming`
    // durable for the supervisor and report the committed command as success.
    if cleanup_retired_managed_material(&reclaiming, store)
        .await
        .is_ok()
    {
        let _completion = repo.complete_managed_mutation(&reclaiming).await;
    }
    Ok(reclaiming)
}

fn prepare_managed_source_update(
    before: &CredentialSource,
    patch: CredentialMaterialPatch,
    advance_without_material: bool,
) -> Result<
    (
        CredentialSource,
        Vec<(crate::SecretRef, awaken_agent_contract::RedactedString)>,
    ),
    CredentialError,
> {
    validate_material_slots(patch.auxiliary.keys().map(String::as_str))?;
    if before.kind != CredentialKind::Vault || before.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(before.id.0.clone()));
    }
    let material_changed = patch.primary.is_some() || !patch.auxiliary.is_empty();
    if !material_changed && !advance_without_material {
        return Ok((before.clone(), Vec::new()));
    }
    let version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    let mut after = before.clone();
    after.version = version;
    let mut materials = Vec::new();
    if let Some(material) = patch.primary {
        let reference = material_ref_for(&before.id, version, "primary");
        after.material_ref = Some(reference.clone());
        materials.push((reference, material));
    }
    for (slot, material) in patch.auxiliary {
        match material {
            Some(material) => {
                let reference = material_ref_for(&before.id, version, &slot);
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
    Ok((after, materials))
}

/// Publish a Managed child patch and its exact executable Source as one
/// consistency transition. The caller supplies only the already-mapped
/// secret-free projection; this service owns both revision fences and material
/// recovery.
pub async fn update_managed_credential(
    before_credential: ManagedVaultCredential,
    mut after_credential: ManagedVaultCredential,
    material_patch: CredentialMaterialPatch,
    advance_source_without_material: bool,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<ManagedVaultCredential, ManagedCredentialMutationError> {
    let before_source = repo.get(&before_credential.source_id).await?;
    let (after_source, materials) = prepare_managed_source_update(
        &before_source,
        material_patch,
        advance_source_without_material,
    )?;
    after_credential.revision = before_credential
        .revision
        .checked_add(1)
        .ok_or(ManagedCredentialMutationError::RevisionExhausted)?;
    let operation_id = format!(
        "managed-update:{}:{}:{}",
        before_source.id.0, before_source.version, before_credential.revision
    );
    let pending = PendingManagedCredentialMutation::change(
        operation_id,
        ManagedCredentialOperation::Update,
        before_source,
        after_source,
        before_credential,
        after_credential.clone(),
        !materials.is_empty(),
    )?;
    let committed = execute_managed_mutation(pending, materials, store, repo).await?;
    Ok(committed.after_credential)
}

/// Publish a caller-prepared hosted bearer rotation through the same Managed
/// pair transaction. The preparation owns idempotency-specific material refs;
/// this command still owns child revision, recovery, outbox, and cleanup.
pub async fn update_managed_credential_prepared(
    before_credential: ManagedVaultCredential,
    mut after_credential: ManagedVaultCredential,
    before_source: CredentialSource,
    rotation: PreparedApplicationMcpBearerRotation,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<(CredentialSource, ManagedVaultCredential), ManagedCredentialMutationError> {
    if before_credential.source_id != before_source.id
        || rotation.after_source.id != before_source.id
        || rotation.after_source.version != before_source.version.saturating_add(1)
    {
        return Err(ManagedCredentialMutationError::RevisionConflict);
    }
    after_credential.revision = before_credential
        .revision
        .checked_add(1)
        .ok_or(ManagedCredentialMutationError::RevisionExhausted)?;
    let after_source = rotation.after_source;
    let pending = PendingManagedCredentialMutation::change(
        rotation.operation_id,
        ManagedCredentialOperation::Update,
        before_source,
        after_source.clone(),
        before_credential,
        after_credential.clone(),
        true,
    )?;
    let committed = execute_managed_mutation(
        pending,
        vec![(rotation.material_ref, rotation.bearer)],
        store,
        repo,
    )
    .await?;
    Ok((committed.after_source, committed.after_credential))
}

/// Archive or tombstone a Managed child together with the exact Source. Delete
/// is a logical absorbing transition; physical purge is intentionally outside
/// the command so an older writer can never recreate the child.
pub async fn retire_managed_credential(
    workspace_id: &str,
    vault_id: &str,
    credential_id: &str,
    operation: ManagedCredentialOperation,
    at: String,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<ManagedVaultCredential, ManagedCredentialMutationError> {
    if !matches!(
        operation,
        ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete
    ) {
        return Err(ManagedCredentialMutationError::InvalidLifecycle);
    }
    let before_credential = repo
        .get_vault_credential(workspace_id, credential_id)
        .await?
        .filter(|credential| credential.vault_id == vault_id)
        .ok_or(ManagedCredentialMutationError::NotFound)?;
    if (operation == ManagedCredentialOperation::Archive
        && matches!(
            before_credential.lifecycle,
            ManagedCredentialLifecycle::Archived { .. }
        ))
        || (operation == ManagedCredentialOperation::Delete
            && before_credential.lifecycle.is_deleted())
    {
        return Ok(before_credential);
    }
    if operation == ManagedCredentialOperation::Archive && before_credential.lifecycle.is_deleted()
    {
        return Err(ManagedCredentialMutationError::NotFound);
    }
    let before_source = repo.get(&before_credential.source_id).await?;
    let mut after_source = before_source.clone();
    if before_source.status != CredentialStatus::Archived
        || before_source.material_ref.is_some()
        || !before_source.auxiliary_material_refs.is_empty()
    {
        after_source.version = before_source
            .version
            .checked_add(1)
            .ok_or(ManagedCredentialMutationError::RevisionExhausted)?;
    }
    after_source.status = CredentialStatus::Archived;
    after_source.material_ref = None;
    after_source.auxiliary_material_refs.clear();
    let mut after_credential = before_credential.clone();
    after_credential.revision = before_credential
        .revision
        .checked_add(1)
        .ok_or(ManagedCredentialMutationError::RevisionExhausted)?;
    after_credential.lifecycle = match operation {
        ManagedCredentialOperation::Archive => ManagedCredentialLifecycle::Archived { at },
        ManagedCredentialOperation::Delete => ManagedCredentialLifecycle::Deleted { at },
        ManagedCredentialOperation::Create | ManagedCredentialOperation::Update => {
            unreachable!("operation was checked above")
        }
    };
    let operation_id = format!(
        "managed-{}:{}:{}:{}",
        match operation {
            ManagedCredentialOperation::Archive => "archive",
            ManagedCredentialOperation::Delete => "delete",
            ManagedCredentialOperation::Create | ManagedCredentialOperation::Update => {
                unreachable!("operation was checked above")
            }
        },
        before_source.id.0,
        before_source.version,
        before_credential.revision
    );
    let pending = PendingManagedCredentialMutation::change(
        operation_id,
        operation,
        before_source,
        after_source,
        before_credential,
        after_credential.clone(),
        false,
    )?;
    let committed = execute_managed_mutation(pending, Vec::new(), store, repo).await?;
    Ok(committed.after_credential)
}

async fn enter_prepared_credential(
    mut source: CredentialSource,
    secret: Option<awaken_agent_contract::RedactedString>,
    auxiliary: BTreeMap<String, awaken_agent_contract::RedactedString>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    validate_material_slots(auxiliary.keys().map(String::as_str))?;
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
        if let Err(error) = persist_material_exact(store, &reference, secret).await {
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
    rotate_credential_materials_exact_with_primary_ref(
        id,
        expected_version,
        patch,
        None,
        store,
        repo,
    )
    .await
}

pub(super) async fn rotate_credential_materials_exact_with_primary_ref(
    id: &CredentialSourceId,
    expected_version: i64,
    patch: CredentialMaterialPatch,
    primary_ref: Option<crate::SecretRef>,
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
        let reference = primary_ref.unwrap_or_else(|| material_ref_for(id, version, "primary"));
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
        if let Err(error) = persist_material_exact(store, &reference, material).await {
            if cleanup_unpublished_material(&intent, store).await.is_ok() {
                repo.complete_mutation(id).await?;
            }
            return Err(error);
        }
    }
    if let Err(error) = repo.apply_mutation(&intent).await {
        // A CAS conflict proves this intent was not published, so its newly
        // sealed refs can be reclaimed immediately. Storage errors remain
        // ambiguous and deliberately retain the WAL for recovery.
        if matches!(error, CredentialError::MutationConflict(_))
            && cleanup_unpublished_material(&intent, store).await.is_ok()
        {
            repo.complete_mutation(id).await?;
        }
        return Err(error);
    }
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

/// Explicitly widen one active, provider-bound credential from an exact
/// ProtocolEndpoint to every endpoint owned by that same Provider. The
/// mutation retains the existing material references and uses exact revision
/// and endpoint checks so a stale connection command cannot broaden authority.
pub async fn widen_credential_to_provider_scope_exact(
    id: &CredentialSourceId,
    expected_version: i64,
    expected_provider_id: &str,
    expected_endpoint_id: &str,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let before = repo.get(id).await?;
    if before.version != expected_version {
        return Err(CredentialError::MutationConflict(
            "credential revision changed before provider-scope widening".into(),
        ));
    }
    if before.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(id.0.clone()));
    }
    if before.provider_id.as_deref() != Some(expected_provider_id)
        || before.protocol_endpoint_id.as_deref() != Some(expected_endpoint_id)
    {
        return Err(CredentialError::NoCredential);
    }
    let mut after = before.clone();
    after.version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    after.protocol_endpoint_id = None;
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

#[cfg(test)]
mod managed_tests;
#[cfg(test)]
mod tests;
