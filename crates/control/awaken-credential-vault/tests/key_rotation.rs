//! KNOWN GAP (adjudicate): no key rotation / dual-key read. The AEAD
//! [`SealedAeadSecretStore`] holds exactly ONE key/cipher (see `src/sealed.rs`);
//! there is no path that reads a blob under a prior key during a rotation. This
//! suite DOCUMENTS that absence — a blob sealed under the old key becomes
//! UNREADABLE the instant the key changes, with no grace window in which both the
//! old and new keys open it. Pins the CURRENT behavior; does not endorse it. The
//! only migration available today is to re-seal every plaintext under the new key.
#![cfg(feature = "sealed-aead")]

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{
    CredentialError, InMemorySealedBlobStore, SealedAeadSecretStore, SealedBlobStore, SecretRef,
    SecretStore,
};

const OLD_KEY: [u8; 32] = [1u8; 32];
const NEW_KEY: [u8; 32] = [2u8; 32];

/// KNOWN GAP (adjudicate): no key rotation / dual-key read. A secret sealed under
/// `OLD_KEY`, then read through a store built with `NEW_KEY` over the SAME at-rest
/// blobs, fails closed as `Seal`. There is no dual-key window: the store cannot be
/// asked to fall back to the prior key, so the only recovery is to re-seal the
/// plaintext under the new key (after which the old-key store can no longer read it
/// either). This pins the asymmetry a real rotation would have to bridge.
#[tokio::test]
async fn a_rotated_key_cannot_read_blobs_sealed_under_the_old_key() {
    // Shared at-rest blobs, so only the KEY changes between the two stores.
    let blobs: Arc<dyn SealedBlobStore> = Arc::new(InMemorySealedBlobStore::new());
    let r = SecretRef("cred:rotate".into());

    // Seal under the old key; the old-key store round-trips it.
    let old = SealedAeadSecretStore::over(&OLD_KEY, blobs.clone());
    old.put(&r, RedactedString::new("sk-old-era"))
        .await
        .unwrap();
    assert_eq!(old.get(&r).await.unwrap().expose_secret(), "sk-old-era");

    // Rotate the key: a NEW-key store over the very same ciphertext CANNOT open it —
    // no dual-key read exists, so the AEAD tag fails and it closes as `Seal`.
    let rotated = SealedAeadSecretStore::over(&NEW_KEY, blobs.clone());
    assert!(matches!(rotated.get(&r).await, Err(CredentialError::Seal)));

    // The only migration is to re-seal the plaintext under the new key...
    rotated
        .put(&r, RedactedString::new("sk-new-era"))
        .await
        .unwrap();
    assert_eq!(rotated.get(&r).await.unwrap().expose_secret(), "sk-new-era");

    // ...and once re-sealed, the OLD-key store can no longer read it — the rotation
    // is a hard cutover in both directions, never an overlapping dual-key window.
    assert!(matches!(old.get(&r).await, Err(CredentialError::Seal)));
}
