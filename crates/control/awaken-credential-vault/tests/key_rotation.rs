//! CONTRACT: single-key AEAD sealing (hard cutover). The AEAD
//! [`SealedAeadSecretStore`] holds exactly ONE key/cipher (see `src/sealed.rs`), so
//! sealing is a single-key scheme by design: a blob sealed under a given key opens
//! ONLY under that same key, and a wrong/rotated key fails closed (the AEAD tag
//! rejects it) rather than leaking plaintext — the correct, secure behavior. There
//! is deliberately no dual-key grace window; rotating the sealing key is a hard
//! cutover, and the supported migration is to re-seal every plaintext under the new
//! key. This suite asserts that documented contract. (An overlapping dual-key
//! rotation window is a possible future feature, not a defect in this scheme.)
#![cfg(feature = "sealed-aead")]

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{
    CredentialError, InMemorySealedBlobStore, SealedAeadSecretStore, SealedBlobStore, SecretRef,
    SecretStore,
};

const OLD_KEY: [u8; 32] = [1u8; 32];
const NEW_KEY: [u8; 32] = [2u8; 32];

/// CONTRACT: single-key sealing is a hard cutover. A secret sealed under `OLD_KEY`,
/// then read through a store built with `NEW_KEY` over the SAME at-rest blobs, fails
/// closed as `Seal` — a wrong key MUST NOT open the ciphertext. The supported
/// migration is to re-seal the plaintext under the new key (after which the old-key
/// store can no longer read it either). This asserts that secure hard-cutover
/// contract; a dual-key overlap window would be a future feature, not a fix.
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
