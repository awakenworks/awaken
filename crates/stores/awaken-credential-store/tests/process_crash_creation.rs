#![cfg(all(feature = "sqlite", feature = "sealed-aead"))]

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_store::SealedAeadSecretStore;
use awaken_credential_store::sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore};
use awaken_credential_vault::repo::{
    CredentialMutationIntent, CredentialRepo, recover_credential_mutations,
};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, SecretRef, SecretStore,
};
use awaken_reliability_testkit::CrashProcess;

const CHILD_MODE: &str = "AWAKEN_CREDENTIAL_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_CREDENTIAL_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_CREDENTIAL_CRASH_MARKER";
const KEY: [u8; 32] = [19; 32];

fn source() -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId("cred:ws:process-crash".into()),
        replacement_of: None,
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: Some("anthropic".into()),
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(SecretRef("sec:cred:ws:process-crash".into())),
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

fn stores(path: &str) -> (SqliteCredentialRepo, SealedAeadSecretStore) {
    let repo = SqliteCredentialRepo::open(path).unwrap();
    let blob = SqliteSealedBlobStore::open(path).unwrap();
    (repo, SealedAeadSecretStore::over(&KEY, Arc::new(blob)))
}

#[tokio::test]
async fn secret_write_survives_kill_and_is_compensated_from_the_intent() {
    // Cause/effect rule CR1: before=None, unpublished source, and a durable new
    // secret after process death => recovery deletes the unpublished secret,
    // completes the WAL record, and a second recovery is a no-op.
    if std::env::var_os(CHILD_MODE).is_some() {
        let db = std::env::var(DB_PATH).unwrap();
        let marker = std::env::var(MARKER_PATH).unwrap();
        let (repo, secrets) = stores(&db);
        let intent = CredentialMutationIntent::prepare(None, source()).unwrap();
        repo.begin_mutation(intent.clone()).await.unwrap();
        secrets
            .put(
                intent.after.material_ref.as_ref().unwrap(),
                RedactedString::new("process-crash-secret"),
            )
            .await
            .unwrap();
        std::fs::write(marker, b"secret-written").unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        panic!("parent failed to kill crash child");
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("credential.db");
    let marker = dir.path().join("secret-written.marker");
    let status = CrashProcess::new(
        "secret_write_survives_kill_and_is_compensated_from_the_intent",
        &marker,
    )
    .env(CHILD_MODE, "1")
    .env(DB_PATH, &db)
    .env(MARKER_PATH, &marker)
    .run()
    .expect("child reaches the secret-write boundary and is killed");
    assert!(!status.success());

    let (repo, secrets) = stores(db.to_str().unwrap());
    let pending = repo.pending_mutations().await.unwrap();
    assert_eq!(pending.len(), 1);
    let attempted_ref = pending[0].after.material_ref.as_ref().unwrap();
    assert!(secrets.get(attempted_ref).await.is_ok());
    let claim_now = pending[0].material_fence.writer_lease_expires_at_unix_ms + 1;
    let claimed = repo
        .claim_expired_mutation(&pending[0], claim_now, claim_now + 120_000)
        .await
        .unwrap()
        .expect("expired crash writer claim");
    repo.abort_mutation(&claimed).await.unwrap();
    assert_eq!(
        recover_credential_mutations(&secrets, &repo).await.unwrap(),
        1
    );
    assert!(repo.pending_mutations().await.unwrap().is_empty());
    assert!(secrets.get(attempted_ref).await.is_err());
    assert_eq!(
        recover_credential_mutations(&secrets, &repo).await.unwrap(),
        0
    );
}
