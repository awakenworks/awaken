use super::*;

pub struct CredentialInventoryReport {
    /// Reserved for backends that can prove and delete an orphan atomically.
    /// The generic reconciler never populates this field.
    pub orphaned_deleted: Vec<crate::SecretRef>,
    pub missing_material: Vec<crate::SecretRef>,
}

/// Advisory inventory observation. Candidates are reported but retained until
/// a backend acquires a durable exact orphan claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInventoryObservation {
    pub orphaned_detected: Vec<crate::SecretRef>,
    pub missing_material: Vec<crate::SecretRef>,
}

/// Compare the secret inventory with committed metadata and in-flight intents.
/// This generic boundary is deliberately observational: a SecretStore delete
/// cannot be committed atomically with the repository protection check. An
/// unreferenced deterministic key can become owned by a new pending mutation
/// immediately after the snapshot, so automatic deletion here would be a
/// time-of-check/time-of-use data-loss bug. A backend-specific collector may
/// delete only after acquiring a durable exact orphan claim.
pub async fn inspect_credential_inventory(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<CredentialInventoryObservation, CredentialError> {
    let inventory = store.inventory().await?;
    // Read intents first. Publication atomically removes an intent while adding
    // its metadata row, so either this read sees the in-flight protection or the
    // following committed-reference read sees the published source.
    let pending = repo.pending_mutations().await?;
    let managed_pending = repo.pending_managed_mutations().await?;
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
        .chain(managed_pending.iter().flat_map(|mutation| {
            mutation
                .before_source
                .iter()
                .flat_map(CredentialSource::material_refs)
                .chain(mutation.after_source.material_refs())
                .map(|reference| reference.0.clone())
        }))
        .collect();

    let mut orphaned_detected = Vec::new();
    for reference in inventory {
        if reference.0.starts_with("sec:cred:") && !protected.contains(&reference.0) {
            orphaned_detected.push(reference);
        }
    }
    let missing_material = committed
        .into_iter()
        .filter(|reference| !present.contains(&reference.0))
        .collect();
    Ok(CredentialInventoryObservation {
        orphaned_detected,
        missing_material,
    })
}

/// Compatibility report for the former generic reconciliation entry point.
/// The generic implementation is now deliberately report-only; callers that
/// need the observed orphan candidates should use [`inspect_credential_inventory`].
pub async fn reconcile_credential_inventory(
    store: &dyn SecretStore,
    repo: &dyn ManagedCredentialRepository,
) -> Result<CredentialInventoryReport, CredentialError> {
    let observation = inspect_credential_inventory(store, repo).await?;
    Ok(CredentialInventoryReport {
        orphaned_deleted: Vec::new(),
        missing_material: observation.missing_material,
    })
}
