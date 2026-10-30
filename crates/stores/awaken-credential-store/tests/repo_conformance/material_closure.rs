//! One material-close law shared by the existing material backends.
use super::*;

pub(super) async fn closure_table(store: &dyn SecretStore) {
    // Cause/effect graph: C1 reference absent/present; C2 put precedes/follows
    // close; C3 two independent callers overlap; C4 close replays; C5 another
    // reference stays open. D1 C1+C2 before => clear bytes; D2 C1+C2 after =>
    // reject put (MutationConflict); D3 C3 => either legal put outcome then
    // absence, never resurrection; D4 C4 => idempotent absence; D5 C5 => sibling
    // unchanged. Inventory lists material only, never closed markers. The same
    // table runs plaintext-memory, AEAD-memory, SQLite and required PostgreSQL.
    let sibling = SecretRef("closure-live".into());
    store
        .put(&sibling, RedactedString::new("sibling"))
        .await
        .unwrap();
    for present in [false, true] {
        let reference = SecretRef(format!("closure-{present}"));
        if present {
            store
                .put(&reference, RedactedString::new("first"))
                .await
                .unwrap();
            // Open keys retain their existing reseal/overwrite behavior.
            store
                .put(&reference, RedactedString::new("resealed"))
                .await
                .unwrap();
            assert_eq!(
                store.get(&reference).await.unwrap().expose_secret(),
                "resealed"
            );
        }
        for _ in 0..2 {
            store.delete(&reference).await.unwrap();
            assert!(matches!(store.get(&reference).await,
                Err(CredentialError::SecretNotFound(missing)) if missing == reference.0));
            assert!(
                matches!(
                    store.put(&reference, RedactedString::new("late")).await,
                    Err(CredentialError::MutationConflict(_))
                ),
                "D1/D2/D4 closed {present}"
            );
        }
    }
    let race = SecretRef("closure-race".into());
    let (put, close) = tokio::join!(
        store.put(&race, RedactedString::new("racing")),
        store.delete(&race)
    );
    close.unwrap();
    assert!(
        put.is_ok() || matches!(put, Err(CredentialError::MutationConflict(_))),
        "D3"
    );
    assert!(
        matches!(
            store.get(&race).await,
            Err(CredentialError::SecretNotFound(_))
        ),
        "D3"
    );
    assert_eq!(
        store.inventory().await.unwrap(),
        vec![sibling.clone()],
        "D1-D5 inventory"
    );
    assert_eq!(
        store.get(&sibling).await.unwrap().expose_secret(),
        "sibling",
        "D5"
    );
}

#[cfg(all(feature = "sealed-aead", any(feature = "sqlite", feature = "postgres")))]
pub(super) async fn closed_after_reopen(store: &dyn SecretStore) {
    // D6: a fresh adapter/connection over the same durable database must retain
    // all three close identities and the open sibling. Forgetting in-memory
    // state or merely deleting material cannot satisfy these post-restart puts.
    for name in ["closure-false", "closure-true", "closure-race"] {
        let reference = SecretRef(name.into());
        assert!(matches!(store.get(&reference).await,
            Err(CredentialError::SecretNotFound(missing)) if missing == name));
        assert!(
            matches!(
                store
                    .put(&reference, RedactedString::new("after-restart"))
                    .await,
                Err(CredentialError::MutationConflict(_))
            ),
            "D6 {name}"
        );
    }
    assert_eq!(
        store
            .get(&SecretRef("closure-live".into()))
            .await
            .unwrap()
            .expose_secret(),
        "sibling"
    );
}

#[tokio::test]
async fn memory_secret_and_sealed_stores_share_the_material_close_law() {
    closure_table(&InMemorySecretStore::new()).await;
    #[cfg(feature = "sealed-aead")]
    {
        let blobs = std::sync::Arc::new(awaken_credential_vault::InMemorySealedBlobStore::new());
        let store = awaken_credential_store::SealedAeadSecretStore::over(&[7; 32], blobs);
        closure_table(&store).await;
    }
}

#[cfg(all(feature = "sqlite", feature = "sealed-aead"))]
#[tokio::test]
async fn sqlite_material_close_survives_cold_reopen_without_ciphertext() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("closure.db");
    let path = path.to_str().unwrap();
    {
        let blobs = std::sync::Arc::new(
            awaken_credential_store::SqliteSealedBlobStore::open(path)
                .await
                .unwrap(),
        );
        let store = awaken_credential_store::SealedAeadSecretStore::over(&[7; 32], blobs);
        closure_table(&store).await;
    }
    let blobs = std::sync::Arc::new(
        awaken_credential_store::SqliteSealedBlobStore::open(path)
            .await
            .unwrap(),
    );
    let store = awaken_credential_store::SealedAeadSecretStore::over(&[7; 32], blobs);
    closed_after_reopen(&store).await;
    let pool = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT secret_ref,sealed FROM credential_secret WHERE closed=1 ORDER BY secret_ref",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "D6 close markers persist even for absent material"
    );
    assert!(
        rows.iter().all(|(_, bytes)| bytes.is_empty()),
        "D6 no ciphertext retained"
    );
}

#[cfg(all(feature = "postgres", feature = "sealed-aead"))]
#[test]
fn postgres_material_close_fixture_is_required_when_configured() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres::postgres_material_close_survives_new_connections_without_ciphertext",
    );
}
