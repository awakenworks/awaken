use super::*;
use awaken_credential_vault::catalog::ManagedVaultRepo;
use awaken_credential_vault::repo::InMemoryCredentialRepo;

#[tokio::test]
async fn stale_delete_writer_resumes_the_durable_winner() {
    let repository = InMemoryCredentialRepo::new();
    let current = VaultRecord {
        id: "vault-delete-race".into(),
        workspace_id: "workspace".into(),
        display_name: "Vault".into(),
        metadata: BTreeMap::new(),
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        archived_at: None,
        deletion: None,
        revision: 1,
    };
    repository
        .insert_vault("workspace", current.clone())
        .await
        .unwrap();
    let (requested, changed) =
        request_managed_vault_deletion(&current, "2026-09-17T08:00:00Z".to_owned()).unwrap();
    assert!(changed);

    // Deterministically reproduce the interleaving of two DELETE handlers:
    // both read `current`, then the first publishes `requested` before the
    // second attempts the same stale CAS.
    repository
        .replace_vault("workspace", current.revision, requested.clone())
        .await
        .unwrap();
    let resumed = persist_vault_deletion_request(
        &repository,
        "workspace",
        &current,
        requested.clone(),
        changed,
    )
    .await
    .expect("the stale loser must resume the durable delete request");

    assert_eq!(resumed, requested);
    assert!(resumed.deletion_requested());
}
