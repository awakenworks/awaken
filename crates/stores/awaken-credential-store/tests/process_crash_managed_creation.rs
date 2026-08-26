#![cfg(all(feature = "sqlite", feature = "sealed-aead"))]

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_store::SealedAeadSecretStore;
use awaken_credential_store::sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore};
use awaken_credential_vault::catalog::{
    ManagedCredentialAuth, ManagedCredentialLifecycle, ManagedVault, ManagedVaultCredential,
    ManagedVaultRepo,
};
use awaken_credential_vault::repo::{
    CredentialRepo, ManagedCredentialRepository, PendingManagedCredentialMutation,
    recover_managed_credential_mutations,
};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, SecretRef, SecretStore,
};

const CHILD_MODE: &str = "AWAKEN_MANAGED_CREDENTIAL_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_MANAGED_CREDENTIAL_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_MANAGED_CREDENTIAL_CRASH_MARKER";
const KEY: [u8; 32] = [23; 32];

fn source() -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId("cred:ws:managed-process-crash".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: None,
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(SecretRef("sec:cred:ws:managed-process-crash".into())),
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

fn child() -> ManagedVaultCredential {
    ManagedVaultCredential {
        id: "credential-process-crash".into(),
        vault_id: "vault-process-crash".into(),
        workspace_id: "ws".into(),
        source_id: source().id,
        auth: ManagedCredentialAuth::StaticBearer {
            mcp_server_url: "https://mcp.example.com".into(),
        },
        metadata: BTreeMap::new(),
        display_name: None,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    }
}

fn stores(path: &str) -> (SqliteCredentialRepo, SealedAeadSecretStore) {
    let repo = SqliteCredentialRepo::open(path).unwrap();
    let blob = SqliteSealedBlobStore::open(path).unwrap();
    (repo, SealedAeadSecretStore::over(&KEY, Arc::new(blob)))
}

#[tokio::test]
async fn complete_material_survives_kill_and_recovers_one_atomic_managed_pair() {
    if std::env::var_os(CHILD_MODE).is_some() {
        let db = std::env::var(DB_PATH).unwrap();
        let marker = std::env::var(MARKER_PATH).unwrap();
        let (repo, secrets) = stores(&db);
        repo.insert_vault(
            "ws",
            ManagedVault {
                id: "vault-process-crash".into(),
                workspace_id: "ws".into(),
                display_name: "Crash".into(),
                metadata: BTreeMap::new(),
                archived_at: None,
                deletion: None,
                revision: 1,
            },
        )
        .await
        .unwrap();
        let mut pending = PendingManagedCredentialMutation::create(source(), child()).unwrap();
        let material_ref = pending.after_source.material_ref.clone().unwrap();
        // Deterministically model a process that is killed after its lease has
        // expired; recovery must not depend on sleeping for the lease duration.
        pending.writer_lease_expires_at_unix_ms = 1;
        repo.begin_managed_mutation(pending).await.unwrap();
        secrets
            .put(&material_ref, RedactedString::new("process-crash-secret"))
            .await
            .unwrap();
        std::fs::write(marker, b"material-ready").unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        panic!("parent failed to kill crash child");
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("managed-credential.db");
    let marker = dir.path().join("material-ready.marker");
    let mut process = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("complete_material_survives_kill_and_recovers_one_atomic_managed_pair")
        .arg("--nocapture")
        .env(CHILD_MODE, "1")
        .env(DB_PATH, &db)
        .env(MARKER_PATH, &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "child did not reach the material failpoint"
    );
    process.kill().unwrap();
    assert!(!process.wait().unwrap().success());

    let (repo, secrets) = stores(db.to_str().unwrap());
    assert_eq!(repo.pending_managed_mutations().await.unwrap().len(), 1);
    assert!(matches!(
        repo.get(&source().id).await,
        Err(awaken_credential_vault::CredentialError::SourceNotFound(_))
    ));
    assert_eq!(
        repo.get_vault_credential("ws", &child().id).await.unwrap(),
        None
    );
    assert_eq!(
        recover_managed_credential_mutations(&secrets, &repo)
            .await
            .unwrap(),
        1
    );
    let published = repo.get(&source().id).await.unwrap();
    let mut expected = source();
    expected.material_ref = published.material_ref.clone();
    assert_eq!(published, expected);
    assert!(
        published
            .material_ref
            .as_ref()
            .is_some_and(|reference| reference.0.contains(":attempt:"))
    );
    assert_eq!(
        secrets
            .get(published.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "process-crash-secret"
    );
    assert_eq!(
        repo.get_vault_credential("ws", &child().id).await.unwrap(),
        Some(child())
    );
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    assert!(matches!(
        repo.begin_managed_mutation(
            PendingManagedCredentialMutation::create(source(), child()).unwrap()
        )
        .await,
        Err(awaken_credential_vault::CredentialError::MutationConflict(
            _
        ))
    ));
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    assert_eq!(
        recover_managed_credential_mutations(&secrets, &repo)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn sqlite_writing_lease_fences_live_and_stale_owners() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lease-fence.db");
    let repo = SqliteCredentialRepo::open(db.to_str().unwrap()).unwrap();
    repo.insert_vault(
        "ws",
        ManagedVault {
            id: "vault-process-crash".into(),
            workspace_id: "ws".into(),
            display_name: "Lease".into(),
            metadata: BTreeMap::new(),
            archived_at: None,
            deletion: None,
            revision: 1,
        },
    )
    .await
    .unwrap();
    let mut stale = PendingManagedCredentialMutation::create(source(), child()).unwrap();
    stale.writer_lease_expires_at_unix_ms = 100;
    repo.begin_managed_mutation(stale.clone()).await.unwrap();

    assert!(
        repo.claim_expired_managed_mutation(&stale, 99, 200)
            .await
            .unwrap()
            .is_none(),
        "a live writer lease must not be stolen"
    );
    let claimed = repo
        .claim_expired_managed_mutation(&stale, 100, 200)
        .await
        .unwrap()
        .expect("expired writer must be atomically fenced");
    assert_eq!(claimed.writer_epoch, stale.writer_epoch + 1);
    assert_ne!(claimed.writer_token, stale.writer_token);
    assert!(repo.mark_managed_mutation_ready(&stale).await.is_err());

    let mut stale_ready = stale;
    stale_ready.phase = awaken_credential_vault::repo::ManagedCredentialMutationPhase::Ready;
    assert!(repo.commit_managed_mutation(&stale_ready).await.is_err());

    let ready = repo.mark_managed_mutation_ready(&claimed).await.unwrap();
    repo.commit_managed_mutation(&ready).await.unwrap();
}
