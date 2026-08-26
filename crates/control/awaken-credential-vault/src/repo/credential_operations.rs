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
        replacement_of: None,
        workspace_id: workspace_id.to_string(),
        kind: CredentialKind::WorkerLocal,
        descriptor: None,
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
        || durable.replacement_of != expected.replacement_of
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

fn prepare_described_source(
    id: Option<CredentialSourceId>,
    params: CredentialCreateParams,
    protocol_endpoint_id: Option<String>,
    descriptor: awaken_credential_contract::CredentialDescriptor,
) -> Result<
    (
        CredentialSource,
        Option<awaken_agent_contract::RedactedString>,
    ),
    CredentialError,
> {
    validate_create_params(&params)?;
    descriptor
        .validate()
        .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
    if params.provider_id.is_some() {
        return Err(CredentialError::InvalidSource(
            "a described credential cannot also persist legacy provider_id".into(),
        ));
    }
    if protocol_endpoint_id
        .as_deref()
        .is_some_and(|endpoint| endpoint.trim().is_empty())
    {
        return Err(CredentialError::InvalidSource(
            "credential endpoint scope must not be empty".into(),
        ));
    }
    let material = params.secret.as_ref().ok_or_else(|| {
        CredentialError::InvalidSource("a described credential requires primary material".into())
    })?;
    crate::validate_described_material(&descriptor, material)?;
    let (mut source, secret) = match id {
        Some(id) => prepare_source_with_id(id, params),
        None => crate::prepare_source(params),
    };
    source.protocol_endpoint_id = protocol_endpoint_id;
    source.descriptor = Some(descriptor);
    Ok((source, secret))
}

/// Enter one described source through the canonical create WAL/store path.
pub async fn enter_credential_described(
    params: CredentialCreateParams,
    descriptor: awaken_credential_contract::CredentialDescriptor,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let (source, secret) = prepare_described_source(None, params, None, descriptor)?;
    enter_prepared_credential(source, secret, BTreeMap::new(), store, repo).await
}

/// Enter one described source at a stable command identity. This is the same
/// idempotent create authority as legacy entry, with descriptor validation added
/// before any WAL or SecretStore effect.
pub async fn enter_credential_idempotent_described(
    id: CredentialSourceId,
    params: CredentialCreateParams,
    protocol_endpoint_id: Option<String>,
    descriptor: awaken_credential_contract::CredentialDescriptor,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialEntry, CredentialError> {
    let (source, secret) =
        prepare_described_source(Some(id), params, protocol_endpoint_id, descriptor)?;
    let expected_material = secret.clone();
    let entry = enter_prepared_credential_idempotent(source, secret, store, repo).await?;
    verify_idempotent_material(entry, expected_material.as_ref(), store).await
}

/// Create a distinct described source while atomically fencing the exact active
/// source revision it replaces. The predecessor remains unchanged so already
/// pinned consumers can be relinked before a separate exact retirement command.
/// A stable replacement id makes a lost-response retry return the same source
/// and verify the same sealed material.
pub async fn enter_credential_replacement_idempotent_described(
    replacement_of: &awaken_credential_contract::CredentialRef,
    replacement_id: CredentialSourceId,
    params: CredentialCreateParams,
    protocol_endpoint_id: Option<String>,
    descriptor: awaken_credential_contract::CredentialDescriptor,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialEntry, CredentialError> {
    if replacement_of.id.trim().is_empty() || replacement_of.revision == 0 {
        return Err(CredentialError::InvalidSource(
            "replacement_of requires a source id and positive revision".into(),
        ));
    }
    if replacement_of.id == replacement_id.0 {
        return Err(CredentialError::InvalidSource(
            "credential replacement requires a distinct source id".into(),
        ));
    }
    let (mut expected, secret) = prepare_described_source(
        Some(replacement_id),
        params,
        protocol_endpoint_id,
        descriptor,
    )?;
    expected.replacement_of = Some(replacement_of.clone());
    expected.validate_authority()?;
    let expected_material = secret.clone();

    // Exact durable replay wins even if the predecessor was retired after this
    // replacement committed. The stable id plus public source projection and
    // sealed-byte verification remain the idempotency authority.
    match repo.get(&expected.id).await {
        Ok(source) => {
            validate_idempotent_source(&source, &expected)?;
            return verify_idempotent_material(
                CredentialEntry {
                    source,
                    created: false,
                },
                expected_material.as_ref(),
                store,
            )
            .await;
        }
        Err(CredentialError::SourceNotFound(_)) => {}
        Err(error) => return Err(error),
    }

    let predecessor_id = CredentialSourceId(replacement_of.id.clone());
    let before = repo.get(&predecessor_id).await?;
    if before.workspace_id != expected.workspace_id {
        return Err(CredentialError::SourceNotFound(replacement_of.id.clone()));
    }
    let expected_version = i64::try_from(replacement_of.revision).map_err(|_| {
        CredentialError::InvalidSource("replacement_of revision exceeds i64".into())
    })?;
    if before.version != expected_version {
        return Err(CredentialError::MutationConflict(
            "credential revision changed before replacement creation".into(),
        ));
    }
    if before.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(before.id.0.clone()));
    }
    let before_descriptor = before.descriptor.as_ref().ok_or_else(|| {
        CredentialError::InvalidSource(
            "described replacement requires a described predecessor".into(),
        )
    })?;
    let replacement_descriptor = expected.descriptor.as_ref().ok_or_else(|| {
        CredentialError::InvalidSource("replacement source must remain described".into())
    })?;
    if before_descriptor.provider != replacement_descriptor.provider {
        return Err(CredentialError::InvalidSource(
            "described replacement cannot change the canonical provider".into(),
        ));
    }
    if !before.auxiliary_material_refs.is_empty() {
        return Err(CredentialError::InvalidSource(
            "primary-only described replacement does not admit auxiliary material slots".into(),
        ));
    }
    let entry = enter_prepared_credential_idempotent_with_before(
        expected,
        secret,
        Some(before),
        store,
        repo,
    )
    .await?;
    verify_idempotent_material(entry, expected_material.as_ref(), store).await
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
    verify_idempotent_material(entry, expected_material.as_ref(), store).await
}

async fn verify_idempotent_material(
    entry: CredentialEntry,
    expected_material: Option<&awaken_agent_contract::RedactedString>,
    store: &dyn SecretStore,
) -> Result<CredentialEntry, CredentialError> {
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
    enter_prepared_credential_idempotent_with_before(expected, secret, None, store, repo).await
}

async fn enter_prepared_credential_idempotent_with_before(
    expected: CredentialSource,
    secret: Option<awaken_agent_contract::RedactedString>,
    before: Option<CredentialSource>,
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

    match enter_prepared_credential_inner(
        expected.clone(),
        secret,
        BTreeMap::new(),
        before,
        store,
        repo,
    )
    .await
    {
        Ok(entry) => Ok(entry),
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
    if !idempotent_credential_source_matches(actual, expected) {
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
    recover_managed_credential_mutations_at(store, repo, credential_material_now_unix_ms()?).await
}

/// Periodic reconciliation never races a live writer. It resumes `Ready` and
/// `Reclaiming` work immediately. An expired `Writing` owner is fenced and
/// durably aborted; it is never promoted from inspected bytes because the
/// SecretStore has no conditional-put CAS against the writer epoch.
pub async fn reconcile_ready_managed_credential_mutations(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<usize, ManagedCredentialCreationError> {
    recover_managed_credential_mutations_at(store, repo, credential_material_now_unix_ms()?).await
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
    let mutation = pending.clone();
    mutation.validate()?;
    match credential_material_recovery_action(&mutation.material_fence, now_unix_ms) {
        CredentialMaterialRecoveryAction::SkipLiveWriting => return Ok(false),
        CredentialMaterialRecoveryAction::AbortExpiredWriting => {
            let lease_expires_at_unix_ms = now_unix_ms
                .checked_add(CREDENTIAL_MATERIAL_WRITER_LEASE_MS)
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
            abort_managed_mutation_before_cleanup(&claimed, store, repo).await?;
            return Ok(true);
        }
        CredentialMaterialRecoveryAction::CleanupPublished => {
            cleanup_retired_managed_material(&mutation, store).await?;
            repo.complete_managed_mutation(&mutation).await?;
            return Ok(true);
        }
        CredentialMaterialRecoveryAction::CleanupAborted => {
            cleanup_unpublished_managed_material(&mutation, store).await?;
            repo.complete_managed_mutation(&mutation).await?;
            return Ok(true);
        }
        CredentialMaterialRecoveryAction::PublishReady => {}
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

async fn execute_managed_mutation(
    mut pending: PendingManagedCredentialMutation,
    mut materials: Vec<(crate::SecretRef, awaken_agent_contract::RedactedString)>,
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
    // A late effect from a fenced writer may leave an orphan, but it can never
    // overwrite a later attempt's material. Inventory reconciliation reports
    // such unowned material for a backend-specific exact collector.
    fence_material_writes(
        &pending.material_fence,
        pending.before_source.as_ref(),
        &pending.after_source,
        &mut materials,
    )
    .map_err(ManagedCredentialMutationError::Store)?;
    if !repo.begin_managed_mutation(pending.clone()).await? {
        return Err(ManagedCredentialMutationError::Store(
            CredentialError::MutationConflict(
                "Managed credential command is pending under its durable material attempt; retry after publication or recovery"
                    .into(),
            ),
        ));
    }
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
    if pending.material_fence.phase == CredentialMaterialMutationPhase::Writing {
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
    source: CredentialSource,
    secret: Option<awaken_agent_contract::RedactedString>,
    auxiliary: BTreeMap<String, awaken_agent_contract::RedactedString>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    enter_prepared_credential_inner(source, secret, auxiliary, None, store, repo)
        .await
        .map(|entry| entry.source)
}

async fn enter_prepared_credential_inner(
    mut source: CredentialSource,
    secret: Option<awaken_agent_contract::RedactedString>,
    auxiliary: BTreeMap<String, awaken_agent_contract::RedactedString>,
    before: Option<CredentialSource>,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialEntry, CredentialError> {
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
    let committed = execute_source_mutation(before, source, materials, Some(store), repo).await?;
    Ok(CredentialEntry {
        source: committed.after,
        created: true,
    })
}

/// Execute the source-only publication envelope through the same durable
/// writer/lease/attempt kernel as Managed source+child publication. The two
/// envelopes retain distinct repository transactions, but there is only one
/// external SecretStore process and one transition vocabulary.
async fn execute_source_mutation(
    before: Option<CredentialSource>,
    after: CredentialSource,
    mut materials: Vec<(crate::SecretRef, awaken_agent_contract::RedactedString)>,
    store: Option<&dyn SecretStore>,
    repo: &dyn CredentialRepo,
) -> Result<CredentialMutationIntent, CredentialError> {
    if !materials.is_empty() && store.is_none() {
        return Err(CredentialError::MutationConflict(
            "credential material mutation requires its SecretStore participant".into(),
        ));
    }
    let mut intent = CredentialMutationIntent::prepare(before, after)?;
    fence_material_writes(
        &intent.material_fence,
        intent.before.as_ref(),
        &intent.after,
        &mut materials,
    )?;
    if !repo.begin_mutation(intent.clone()).await? {
        return Err(CredentialError::MutationConflict(
            "credential command is pending under its durable material attempt; retry after publication or recovery"
                .into(),
        ));
    }

    for (reference, material) in materials {
        let store = store.expect("material store checked before durable begin");
        if let Err(error) = persist_material_exact(store, &reference, material).await {
            // A failed put may have written bytes. External cleanup starts only
            // after exact repository CAS has made ReclaimingAbort durable. If
            // fencing or cleanup fails, retain the WAL and attempt-owned refs
            // for supervised recovery.
            if let Ok(abort) = repo.abort_mutation(&intent).await
                && cleanup_unpublished_material(&abort, store).await.is_ok()
            {
                let _completion = repo.complete_mutation(&abort).await;
            }
            return Err(error);
        }
    }
    if intent.material_fence.phase == CredentialMaterialMutationPhase::Writing {
        intent = repo.mark_mutation_ready(&intent).await?;
    }

    // Any apply error keeps the exact WAL and candidate material. Publication
    // and WAL advancement are one repository transaction, so recovery alone
    // may classify a deterministic conflict and durably enter abort cleanup.
    let reclaiming = repo.apply_mutation(&intent).await?;
    // Publication already committed. Cleanup is a retryable terminal phase;
    // returning an error would invite a duplicate logical command.
    if let Some(store) = store {
        if cleanup_retired_material(&reclaiming, store).await.is_ok() {
            let _completion = repo.complete_mutation(&reclaiming).await;
        }
    } else {
        let _completion = repo.complete_mutation(&reclaiming).await;
    }
    Ok(reclaiming)
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
    let effective_descriptor = patch.descriptor.as_ref().or(before.descriptor.as_ref());
    if let Some(descriptor) = effective_descriptor {
        descriptor
            .validate()
            .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
        if before.provider_id.is_some() {
            return Err(CredentialError::InvalidSource(
                "a described credential cannot also persist legacy provider_id".into(),
            ));
        }
        if before
            .descriptor
            .as_ref()
            .zip(patch.descriptor.as_ref())
            .is_some_and(|(current, replacement)| current.provider != replacement.provider)
        {
            return Err(CredentialError::InvalidSource(
                "credential descriptor provider is immutable".into(),
            ));
        }
        if let Some(primary) = patch.primary.as_ref() {
            crate::validate_described_material(descriptor, primary)?;
        }
    }
    if let Some(descriptor) = patch.descriptor.as_ref()
        && patch.primary.is_none()
        && before
            .descriptor
            .as_ref()
            .is_none_or(|current| current.material != descriptor.material)
    {
        return Err(CredentialError::InvalidSource(
            "changing a credential material descriptor requires replacement primary material"
                .into(),
        ));
    }
    if let Some(descriptor) = patch.descriptor.as_ref()
        && patch.primary.is_none()
    {
        let current_ref = before
            .material_ref
            .as_ref()
            .ok_or_else(|| CredentialError::MissingMaterialRef(id.0.clone()))?;
        let current_material = store.get(current_ref).await?;
        crate::validate_described_material(descriptor, &current_material)?;
    }
    if patch.primary.is_none() && patch.auxiliary.is_empty() && patch.descriptor.is_none() {
        return Ok(before);
    }
    let version = before
        .version
        .checked_add(1)
        .ok_or_else(|| CredentialError::InvalidSource("credential revision overflow".into()))?;
    let mut after = before.clone();
    after.version = version;
    if let Some(descriptor) = patch.descriptor {
        after.descriptor = Some(descriptor);
    }
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
    execute_source_mutation(Some(before), after, materials, Some(store), repo)
        .await
        .map(|committed| committed.after)
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
    execute_source_mutation(Some(before), after, Vec::new(), None, repo)
        .await
        .map(|committed| committed.after)
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
    execute_source_mutation(Some(before), after, Vec::new(), None, repo)
        .await
        .map(|committed| committed.after)
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
    execute_source_mutation(Some(before), after, Vec::new(), None, repo)
        .await
        .map(|committed| committed.after)
}

/// Terminally revoke one exact source revision: reject a stale command before
/// publishing lifecycle state or erasing any material, then publish a higher
/// disabled/archived revision and reclaim its material while the WAL intent
/// remains recoverable.
pub async fn revoke_credential_exact(
    id: &CredentialSourceId,
    expected_version: i64,
    retirement: CredentialRetirement,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let before = repo.get(id).await?;
    if before.version != expected_version {
        return Err(CredentialError::MutationConflict(
            "credential revision changed before retirement".into(),
        ));
    }
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
    execute_source_mutation(Some(before), after, Vec::new(), Some(store), repo)
        .await
        .map(|committed| committed.after)
}

async fn cleanup_retired_material(
    intent: &CredentialMutationIntent,
    store: &dyn SecretStore,
) -> Result<(), CredentialError> {
    // A distinct replacement publishes a second source and deliberately keeps
    // every predecessor ref alive for consumers still pinned to the old id.
    // Only a later exact retirement command may reclaim predecessor material.
    if intent.is_distinct_replacement() {
        return Ok(());
    }
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

/// Reconcile every interrupted source-only mutation through the shared
/// material fence. A live `Writing` owner is skipped. An expired owner is
/// atomically fenced and aborted, never promoted by inspecting its bytes: a
/// late non-CAS SecretStore put can then create only an unreachable
/// attempt-specific orphan. Only durable `Ready` may publish.
pub async fn recover_credential_mutations(
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<usize, CredentialError> {
    recover_credential_mutations_at(store, repo, credential_material_now_unix_ms()?).await
}

async fn recover_credential_mutations_at(
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
    now_unix_ms: u64,
) -> Result<usize, CredentialError> {
    let intents = repo.pending_mutations().await?;
    let mut recovered = 0;
    let mut first_error = None;
    for intent in intents {
        match recover_credential_mutation(&intent, store, repo, now_unix_ms).await {
            Ok(true) => recovered += 1,
            Ok(false) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(recovered), Err)
}

async fn recover_credential_mutation(
    pending: &CredentialMutationIntent,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
    now_unix_ms: u64,
) -> Result<bool, CredentialError> {
    pending.validate()?;
    let mutation = match credential_material_recovery_action(&pending.material_fence, now_unix_ms) {
        CredentialMaterialRecoveryAction::SkipLiveWriting => return Ok(false),
        CredentialMaterialRecoveryAction::AbortExpiredWriting => {
            let lease_expires_at_unix_ms = now_unix_ms
                .checked_add(CREDENTIAL_MATERIAL_WRITER_LEASE_MS)
                .ok_or_else(|| {
                    CredentialError::MutationConflict(
                        "credential recovery lease deadline overflowed".into(),
                    )
                })?;
            let Some(claimed) = repo
                .claim_expired_mutation(pending, now_unix_ms, lease_expires_at_unix_ms)
                .await?
            else {
                return Ok(false);
            };
            let abort = repo.abort_mutation(&claimed).await?;
            cleanup_unpublished_material(&abort, store).await?;
            repo.complete_mutation(&abort).await?;
            return Ok(true);
        }
        CredentialMaterialRecoveryAction::PublishReady => pending.clone(),
        CredentialMaterialRecoveryAction::CleanupPublished => {
            cleanup_retired_material(pending, store).await?;
            repo.complete_mutation(pending).await?;
            return Ok(true);
        }
        CredentialMaterialRecoveryAction::CleanupAborted => {
            cleanup_unpublished_material(pending, store).await?;
            repo.complete_mutation(pending).await?;
            return Ok(true);
        }
    };

    let before_refs = mutation
        .before
        .iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<HashSet<_>>();
    let mut material_complete = true;
    for reference in mutation.after.material_refs() {
        if before_refs.contains(reference) {
            continue;
        }
        match store.get(reference).await {
            Ok(_) => {}
            Err(CredentialError::SecretNotFound(_)) => material_complete = false,
            Err(error) => return Err(error),
        }
    }
    if !material_complete {
        let abort = repo.abort_mutation(&mutation).await?;
        cleanup_unpublished_material(&abort, store).await?;
        repo.complete_mutation(&abort).await?;
        return Ok(true);
    }

    match repo.apply_mutation(&mutation).await {
        Ok(reclaiming) => {
            cleanup_retired_material(&reclaiming, store).await?;
            repo.complete_mutation(&reclaiming).await?;
            Ok(true)
        }
        Err(CredentialError::MutationConflict(_)) => {
            // Exact abort CAS distinguishes an unpublished deterministic
            // conflict from a foreign row. A foreign row rejects this
            // transition, retaining both WAL and attempt-owned material.
            let abort = repo.abort_mutation(&mutation).await?;
            cleanup_unpublished_material(&abort, store).await?;
            repo.complete_mutation(&abort).await?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod managed_tests;
#[cfg(test)]
mod tests;
