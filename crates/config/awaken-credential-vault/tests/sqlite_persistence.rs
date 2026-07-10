//! SQLite-only persistence round-trips (feature `sqlite`): credential sources,
//! pools, and — composed with `sealed-aead` — sealed secrets that survive a
//! process restart (drop + reopen from file) and stay fail-closed against a
//! wrong key or SQL-level tampering with the at-rest blob.
#![cfg(feature = "sqlite")]

use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::sqlite::SqliteCredentialRepo;
use awaken_credential_vault::{
    CredentialKind, CredentialPool, CredentialPoolId, CredentialPoolMember, CredentialSource,
    CredentialSourceId, CredentialStatus,
};

fn source(id: &str, ws: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        workspace_id: ws.into(),
        kind: CredentialKind::Vault,
        provider_id: Some("anthropic".into()),
        env_key: Some("ANTHROPIC_API_KEY".into()),
        material_ref: None,
        oauth_command: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

#[tokio::test]
async fn sources_and_pools_survive_a_reopen_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credential.db");
    let path = path.to_str().unwrap();
    let pool = CredentialPool {
        id: CredentialPoolId("pool:a".into()),
        workspace_id: "ws".into(),
        members: vec![CredentialPoolMember {
            credential_source_id: CredentialSourceId("cred:a".into()),
            ordinal: 0,
            enabled: true,
            selection_weight: 0,
        }],
    };
    {
        let repo = SqliteCredentialRepo::open(path).unwrap();
        repo.put(source("cred:a", "ws")).await.unwrap();
        repo.put_pool(pool.clone()).await.unwrap();
    }
    // A fresh handle on the same file sees identical rows.
    let repo = SqliteCredentialRepo::open(path).unwrap();
    assert_eq!(
        repo.get(&CredentialSourceId("cred:a".into()))
            .await
            .unwrap(),
        source("cred:a", "ws")
    );
    assert_eq!(
        repo.get_pool(&CredentialPoolId("pool:a".into()))
            .await
            .unwrap(),
        pool
    );
    assert_eq!(repo.list("ws").await.unwrap().len(), 1);
    assert_eq!(repo.list_pools("ws").await.unwrap().len(), 1);
}

/// The durable secret path: AEAD sealing composed over the sqlite blob store.
/// Deliberately no bare (plaintext) sqlite SecretStore exists to test.
#[cfg(feature = "sealed-aead")]
mod sealed_secrets {
    use std::sync::Arc;

    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::sqlite::SqliteSealedBlobStore;
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialError, SealedAeadSecretStore, SecretStore, create_source,
        materialize,
    };

    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    fn sealed_store_on(path: &str, key: &[u8; 32]) -> SealedAeadSecretStore {
        SealedAeadSecretStore::over(key, Arc::new(SqliteSealedBlobStore::open(path).unwrap()))
    }

    /// Seal a secret into `path` via the full create seam; returns the row.
    async fn enter(path: &str) -> CredentialSource {
        let store = sealed_store_on(path, &KEY);
        create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-super-secret-value")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_sealed_secret_survives_a_reopen_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let path = path.to_str().unwrap();
        let row = enter(path).await;

        // Reopen from file under the same key → the secret materializes.
        let reopened = sealed_store_on(path, &KEY);
        let secret = materialize(&row, &reopened).await.unwrap();
        assert_eq!(secret.expose_secret(), "sk-super-secret-value");
    }

    #[tokio::test]
    async fn a_reopen_with_the_wrong_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let path = path.to_str().unwrap();
        let row = enter(path).await;

        let wrong = sealed_store_on(path, &[8u8; 32]);
        assert!(matches!(
            materialize(&row, &wrong).await,
            Err(CredentialError::Seal)
        ));
    }

    #[tokio::test]
    async fn a_blob_tampered_via_sql_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let path = path.to_str().unwrap();
        let row = enter(path).await;
        let secret_ref = row.material_ref.clone().unwrap();

        // Flip one ciphertext byte (past the 12-byte nonce) straight in the
        // database — the AEAD tag must reject the doctored at-rest bytes.
        {
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut blob: Vec<u8> = conn
                .query_row(
                    "SELECT sealed FROM credential_secret WHERE secret_ref = ?1",
                    rusqlite::params![secret_ref.0],
                    |r| r.get(0),
                )
                .unwrap();
            blob[12] ^= 0x01;
            conn.execute(
                "UPDATE credential_secret SET sealed = ?1 WHERE secret_ref = ?2",
                rusqlite::params![blob, secret_ref.0],
            )
            .unwrap();
        }
        let store = sealed_store_on(path, &KEY);
        assert!(matches!(
            store.get(&secret_ref).await,
            Err(CredentialError::Seal)
        ));
    }

    #[tokio::test]
    async fn the_at_rest_bytes_never_contain_the_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        let path = path.to_str().unwrap();
        let row = enter(path).await;
        let secret_ref = row.material_ref.unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT sealed FROM credential_secret WHERE secret_ref = ?1",
                rusqlite::params![secret_ref.0],
                |r| r.get(0),
            )
            .unwrap();
        let plaintext = b"sk-super-secret-value";
        assert!(!blob.windows(plaintext.len()).any(|w| w == plaintext));
    }
}
