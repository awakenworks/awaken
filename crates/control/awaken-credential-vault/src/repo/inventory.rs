// @formal entrypoint credential_inventory.committed_material_is_preserved
// @formal entrypoint credential_inventory.orphans_are_reported_and_retained_without_an_atomic_claim
// @formal entrypoint credential_inventory.pending_intent_material_is_preserved
use super::*;

/// One-reference classification used by the generic credential inventory.
/// There is deliberately no delete outcome: deletion requires a backend-owned
/// atomic orphan claim, which this cross-port snapshot cannot acquire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialInventoryDecision {
    Preserve,
    ReportOrphan,
    ReportMissing,
}

#[must_use]
const fn credential_inventory_decision(
    credential_namespace: bool,
    present: bool,
    committed: bool,
    pending: bool,
) -> CredentialInventoryDecision {
    if committed && !present {
        CredentialInventoryDecision::ReportMissing
    } else if credential_namespace && present && !committed && !pending {
        CredentialInventoryDecision::ReportOrphan
    } else {
        CredentialInventoryDecision::Preserve
    }
}

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
    let pending_keys: HashSet<String> = pending
        .iter()
        .flat_map(|intent| {
            intent
                .before
                .iter()
                .flat_map(CredentialSource::material_refs)
                .chain(intent.after.material_refs())
                .map(|reference| reference.0.clone())
        })
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
        if credential_inventory_decision(
            reference.0.starts_with("sec:cred:"),
            true,
            committed_keys.contains(&reference.0),
            pending_keys.contains(&reference.0),
        ) == CredentialInventoryDecision::ReportOrphan
        {
            orphaned_detected.push(reference);
        }
    }
    let missing_material = committed
        .into_iter()
        .filter(|reference| {
            credential_inventory_decision(
                reference.0.starts_with("sec:cred:"),
                present.contains(&reference.0),
                true,
                pending_keys.contains(&reference.0),
            ) == CredentialInventoryDecision::ReportMissing
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_decision_table_preserves_every_protected_or_foreign_reference() {
        /* Causes: C1=credential namespace, C2=present, C3=committed,
         * C4=pending. Effects: E1=preserve, E2=report orphan, E3=report
         * missing. Rules: R1=C3+!C2=>E3; R2=C1+C2+!C3+!C4=>E2;
         * R3=all other combinations=>E1. No rule has a delete effect. */
        use CredentialInventoryDecision::{Preserve, ReportMissing, ReportOrphan};

        let cases = [
            ((true, true, true, false), Preserve, "committed present"),
            (
                (true, false, true, false),
                ReportMissing,
                "committed missing",
            ),
            ((true, true, false, true), Preserve, "pending present"),
            (
                (true, true, false, false),
                ReportOrphan,
                "unprotected credential",
            ),
            ((false, true, false, false), Preserve, "foreign namespace"),
            ((true, false, false, false), Preserve, "absent unreferenced"),
        ];
        for ((namespace, present, committed, pending), expected, rule) in cases {
            assert_eq!(
                credential_inventory_decision(namespace, present, committed, pending),
                expected,
                "{rule}"
            );
        }
    }
}

#[cfg(kani)]
/// @obligation credential.material_is_always_published_or_intent_tracked
/// @obligation credential_inventory.committed_material_is_preserved
/// @obligation credential_inventory.orphans_are_reported_and_retained_without_an_atomic_claim
/// @obligation credential_inventory.pending_intent_material_is_preserved
/// @proof-input-debt free_symbol credential_inventory_decision committed
/// @proof-input-debt free_symbol credential_inventory_decision pending
/// @proof-input-debt free_symbol credential_inventory_decision present
#[kani::proof]
fn credential_inventory_never_orphans_committed_pending_or_foreign_material() {
    let credential_namespace = kani::any::<bool>();
    let present = kani::any::<bool>();
    let committed = kani::any::<bool>();
    let pending = kani::any::<bool>();
    let decision = credential_inventory_decision(credential_namespace, present, committed, pending);

    assert_eq!(
        decision == CredentialInventoryDecision::ReportMissing,
        committed && !present
    );
    assert_eq!(
        decision == CredentialInventoryDecision::ReportOrphan,
        credential_namespace && present && !committed && !pending
    );
    if committed || pending || !credential_namespace {
        assert_ne!(decision, CredentialInventoryDecision::ReportOrphan);
    }
}
