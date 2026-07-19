#![cfg(all(feature = "sqlite", feature = "sealed-aead"))]

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{
    CredentialCreationIntent, CredentialRepo, recover_credential_creations,
};
use awaken_credential_vault::sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialSourceId, CredentialStatus, SealedAeadSecretStore,
    SecretRef, SecretStore,
};

const CHILD_MODE: &str = "AWAKEN_CREDENTIAL_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_CREDENTIAL_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_CREDENTIAL_CRASH_MARKER";
const KEY: [u8; 32] = [19; 32];

fn source() -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId("cred:ws:process-crash".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: Some("anthropic".into()),
        env_key: None,
        material_ref: Some(SecretRef("sec:cred:ws:process-crash".into())),
        oauth_command: None,
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
    if std::env::var_os(CHILD_MODE).is_some() {
        let db = std::env::var(DB_PATH).unwrap();
        let marker = std::env::var(MARKER_PATH).unwrap();
        let (repo, secrets) = stores(&db);
        repo.begin_creation(CredentialCreationIntent { source: source() })
            .await
            .unwrap();
        secrets
            .put(
                source().material_ref.as_ref().unwrap(),
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
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("secret_write_survives_kill_and_is_compensated_from_the_intent")
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
        "child did not reach the secret-write failpoint"
    );
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());

    let (repo, secrets) = stores(db.to_str().unwrap());
    assert_eq!(repo.pending_creations().await.unwrap().len(), 1);
    assert!(
        secrets
            .get(source().material_ref.as_ref().unwrap())
            .await
            .is_ok()
    );
    assert_eq!(
        recover_credential_creations(&secrets, &repo).await.unwrap(),
        1
    );
    assert!(repo.pending_creations().await.unwrap().is_empty());
    assert!(
        secrets
            .get(source().material_ref.as_ref().unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        recover_credential_creations(&secrets, &repo).await.unwrap(),
        0
    );
}
