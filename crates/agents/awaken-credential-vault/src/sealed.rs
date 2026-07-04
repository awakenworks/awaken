//! AEAD-sealed [`SecretStore`] adapter (ADR-0043, feature `sealed-aead`).
//!
//! Encryption-at-rest so a stolen disk / DB dump / backup is inert without the
//! key: each secret is sealed with ChaCha20-Poly1305 under a 256-bit key the
//! composition root holds **separately** (OS keyring / KMS / env — never on the
//! same medium as the ciphertext). A fresh random nonce is drawn per `put`, so
//! sealing the same secret twice yields distinct ciphertexts; the nonce is stored
//! alongside (`nonce ‖ ciphertext`). The domain and runtime still only ever see a
//! resolved [`RedactedString`] at the seam — never ciphertext, never the key.

use std::collections::HashMap;
use std::sync::Mutex;

use awaken_agent_contract::RedactedString;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::{CredentialError, SecretRef, SecretStore};

/// The 96-bit ChaCha20-Poly1305 nonce width, in bytes.
const NONCE_LEN: usize = 12;

/// A [`SecretStore`] that seals every secret under an AEAD key. The ciphertext map
/// here is in-memory (the sealing is the point); a durable backend stores the same
/// `nonce ‖ ciphertext` blobs. The key never leaves this struct.
pub struct SealedAeadSecretStore {
    cipher: ChaCha20Poly1305,
    sealed: Mutex<HashMap<String, Vec<u8>>>,
}

impl SealedAeadSecretStore {
    /// Build a store sealing under `key` (32 bytes). The caller sources the key
    /// from a keystore/KMS/env kept apart from the ciphertext medium.
    #[must_use]
    pub fn with_key(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            sealed: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl SecretStore for SealedAeadSecretStore {
    async fn put(&self, r: &SecretRef, secret: RedactedString) -> Result<(), CredentialError> {
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, secret.expose_secret().as_bytes())
            .map_err(|_| CredentialError::Seal)?;
        // Store nonce ‖ ciphertext; the nonce is not secret, only single-use.
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ciphertext);
        self.sealed
            .lock()
            .expect("sealed store mutex")
            .insert(r.0.clone(), blob);
        Ok(())
    }

    async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError> {
        let blob = self
            .sealed
            .lock()
            .expect("sealed store mutex")
            .get(&r.0)
            .cloned()
            .ok_or_else(|| CredentialError::SecretNotFound(r.0.clone()))?;
        if blob.len() < NONCE_LEN {
            return Err(CredentialError::Seal);
        }
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| CredentialError::Seal)?;
        let text = String::from_utf8(plaintext).map_err(|_| CredentialError::Seal)?;
        Ok(RedactedString::new(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn seals_and_opens_round_trip() {
        let store = SealedAeadSecretStore::with_key(&[7u8; 32]);
        let r = SecretRef("cred:1".into());
        store
            .put(&r, RedactedString::new("sk-secret"))
            .await
            .unwrap();
        assert_eq!(store.get(&r).await.unwrap().expose_secret(), "sk-secret");
    }

    #[tokio::test]
    async fn ciphertext_is_not_the_plaintext_and_nonce_is_random() {
        let store = SealedAeadSecretStore::with_key(&[9u8; 32]);
        store
            .put(&SecretRef("a".into()), RedactedString::new("top-secret"))
            .await
            .unwrap();
        store
            .put(&SecretRef("b".into()), RedactedString::new("top-secret"))
            .await
            .unwrap();
        let map = store.sealed.lock().unwrap();
        let a = &map["a"];
        let b = &map["b"];
        // At-rest bytes never contain the plaintext...
        assert!(!a.windows(10).any(|w| w == b"top-secret"));
        // ...and a fresh nonce per put makes identical secrets seal differently.
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn a_wrong_key_cannot_open() {
        let sealer = SealedAeadSecretStore::with_key(&[1u8; 32]);
        let r = SecretRef("cred:1".into());
        sealer
            .put(&r, RedactedString::new("sk-secret"))
            .await
            .unwrap();
        // Move the ciphertext blob under a store with a different key.
        let blob = sealer.sealed.lock().unwrap().get(&r.0).cloned().unwrap();
        let attacker = SealedAeadSecretStore::with_key(&[2u8; 32]);
        attacker.sealed.lock().unwrap().insert(r.0.clone(), blob);
        assert!(matches!(attacker.get(&r).await, Err(CredentialError::Seal)));
    }

    #[tokio::test]
    async fn a_tampered_ciphertext_is_rejected() {
        let store = SealedAeadSecretStore::with_key(&[3u8; 32]);
        let r = SecretRef("cred:1".into());
        store
            .put(&r, RedactedString::new("sk-secret"))
            .await
            .unwrap();
        // Flip one ciphertext byte (past the nonce) in the stored blob.
        {
            let mut map = store.sealed.lock().unwrap();
            let blob = map.get_mut(&r.0).unwrap();
            blob[NONCE_LEN] ^= 0x01;
        }
        assert!(matches!(store.get(&r).await, Err(CredentialError::Seal)));
    }

    #[tokio::test]
    async fn a_truncated_blob_is_a_seal_error() {
        let store = SealedAeadSecretStore::with_key(&[4u8; 32]);
        let r = SecretRef("cred:1".into());
        // A blob shorter than the nonce cannot even be split, let alone opened.
        store
            .sealed
            .lock()
            .unwrap()
            .insert(r.0.clone(), vec![0u8; NONCE_LEN - 1]);
        assert!(matches!(store.get(&r).await, Err(CredentialError::Seal)));
    }

    #[tokio::test]
    async fn a_missing_ref_is_secret_not_found() {
        let store = SealedAeadSecretStore::with_key(&[5u8; 32]);
        assert!(matches!(
            store.get(&SecretRef("cred:absent".into())).await,
            Err(CredentialError::SecretNotFound(id)) if id == "cred:absent"
        ));
    }
}
