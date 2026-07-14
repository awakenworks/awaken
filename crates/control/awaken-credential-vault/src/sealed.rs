//! AEAD-sealed [`SecretStore`] adapter (ADR-0043, feature `sealed-aead`).
//!
//! Encryption-at-rest so a stolen disk / DB dump / backup is inert without the
//! key: each secret is sealed with ChaCha20-Poly1305 under a 256-bit key the
//! composition root holds **separately** (OS keyring / KMS / env — never on the
//! same medium as the ciphertext). A fresh random nonce is drawn per `put`, so
//! sealing the same secret twice yields distinct ciphertexts; the nonce is stored
//! alongside (`nonce ‖ ciphertext`). The domain and runtime still only ever see a
//! resolved [`RedactedString`] at the seam — never ciphertext, never the key.
//!
//! Where the blobs *live* is a separate axis: this decorator writes them through
//! the [`SealedBlobStore`] port, so the same AEAD layer composes with the
//! in-memory map ([`with_key`](SealedAeadSecretStore::with_key)) or a durable
//! engine such as `sqlite::SqliteSealedBlobStore`
//! ([`over`](SealedAeadSecretStore::over)).

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::{CredentialError, InMemorySealedBlobStore, SealedBlobStore, SecretRef, SecretStore};

/// The 96-bit ChaCha20-Poly1305 nonce width, in bytes.
const NONCE_LEN: usize = 12;

/// A [`SecretStore`] that seals every secret under an AEAD key and persists only
/// `nonce ‖ ciphertext` blobs through a [`SealedBlobStore`]. The key never leaves
/// this struct; the blob store never learns it.
pub struct SealedAeadSecretStore {
    cipher: ChaCha20Poly1305,
    blobs: Arc<dyn SealedBlobStore>,
}

impl SealedAeadSecretStore {
    /// Build a store sealing under `key` (32 bytes) over the in-memory blob map
    /// (the sealing is the point; a restart forgets the blobs). The caller
    /// sources the key from a keystore/KMS/env kept apart from the ciphertext
    /// medium.
    #[must_use]
    pub fn with_key(key: &[u8; 32]) -> Self {
        Self::over(key, Arc::new(InMemorySealedBlobStore::new()))
    }

    /// Build a store sealing under `key` over an explicit blob backend —
    /// AEAD-at-rest composed with a durable engine (e.g.
    /// `SqliteSealedBlobStore`, feature `sqlite`).
    #[must_use]
    pub fn over(key: &[u8; 32], blobs: Arc<dyn SealedBlobStore>) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            blobs,
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
        self.blobs.put_blob(r, blob).await
    }

    async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError> {
        let blob = self.blobs.get_blob(r).await?;
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

/// Parse a seal-key hex string into the 32-byte AEAD key
/// [`SealedAeadSecretStore`] uses: exactly 64 hex characters. The single home for
/// this so the Serve composition root and the worker share one implementation.
pub fn parse_seal_key(hex: &str) -> Result<[u8; 32], String> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(format!(
            "expected 64 hex characters (a 32-byte key), got {} characters",
            hex.len()
        ));
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("not hex at position {}", 2 * i))?;
    }
    Ok(key)
}

/// Pure resolution of the seal-key hex from its two possible sources — an inline
/// value or a file path — so the precedence + mutual-exclusion rules are testable
/// without the environment or filesystem. `read_file` is injected (real:
/// `std::fs::read_to_string`). Exactly one source must be set: both is ambiguous,
/// neither is a brick-on-restart error (a durable store must not seal under an
/// ephemeral key). The single home shared by the Serve root and the worker.
pub fn resolve_seal_key_hex(
    inline: Option<String>,
    file_path: Option<String>,
    read_file: impl Fn(&str) -> std::io::Result<String>,
) -> Result<String, String> {
    let inline = inline.filter(|v| !v.trim().is_empty());
    let file_path = file_path.filter(|v| !v.trim().is_empty());
    match (inline, file_path) {
        (Some(_), Some(_)) => Err(
            "both AWAKEN_MGMT_SEAL_KEY and AWAKEN_MGMT_SEAL_KEY_FILE are set; they are \
             mutually exclusive — set exactly one"
                .to_string(),
        ),
        (Some(hex), None) => Ok(hex),
        (None, Some(path)) => read_file(&path)
            .map_err(|e| format!("AWAKEN_MGMT_SEAL_KEY_FILE={path} could not be read: {e}")),
        (None, None) => Err(
            "AWAKEN_MGMT_DIR is set but neither AWAKEN_MGMT_SEAL_KEY nor \
             AWAKEN_MGMT_SEAL_KEY_FILE is set. A durable management store needs a stable \
             AEAD key (64 hex characters = 32 bytes); sealing under an ephemeral key \
             would brick every restart"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod seal_key_tests {
    use super::{parse_seal_key, resolve_seal_key_hex};

    fn no_read(_p: &str) -> std::io::Result<String> {
        Err(std::io::Error::other("no file read expected"))
    }

    #[test]
    fn parse_requires_exactly_64_hex_chars() {
        assert!(
            parse_seal_key(&"ab".repeat(32)).is_ok(),
            "64 hex chars parse"
        );
        assert!(parse_seal_key("abc").is_err(), "too short");
        assert!(parse_seal_key(&"zz".repeat(32)).is_err(), "non-hex");
    }

    #[test]
    fn inline_wins_alone_file_read_alone_both_and_neither_error() {
        // Inline alone → returned as-is.
        assert_eq!(
            resolve_seal_key_hex(Some("abc".into()), None, no_read).unwrap(),
            "abc"
        );
        // File alone → read via the injected reader.
        assert_eq!(
            resolve_seal_key_hex(None, Some("/p".into()), |_| Ok("fromfile".into())).unwrap(),
            "fromfile"
        );
        // Both set → ambiguous, rejected.
        assert!(
            resolve_seal_key_hex(Some("abc".into()), Some("/p".into()), no_read)
                .unwrap_err()
                .contains("mutually exclusive")
        );
        // Neither → the brick-on-restart refusal.
        assert!(
            resolve_seal_key_hex(None, None, no_read)
                .unwrap_err()
                .contains("brick every restart")
        );
        // Empty strings are treated as unset.
        assert!(resolve_seal_key_hex(Some("  ".into()), None, no_read).is_err());
    }

    #[test]
    fn an_unreadable_file_reports_the_path() {
        let err = resolve_seal_key_hex(None, Some("/nope".into()), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ))
        })
        .unwrap_err();
        assert!(
            err.contains("/nope") && err.contains("could not be read"),
            "{err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store plus a handle on its blob backend, so tests can inspect and
    /// tamper with exactly what is at rest.
    fn store_with_blobs(key: &[u8; 32]) -> (SealedAeadSecretStore, Arc<InMemorySealedBlobStore>) {
        let blobs = Arc::new(InMemorySealedBlobStore::new());
        (SealedAeadSecretStore::over(key, blobs.clone()), blobs)
    }

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
        let (store, blobs) = store_with_blobs(&[9u8; 32]);
        store
            .put(&SecretRef("a".into()), RedactedString::new("top-secret"))
            .await
            .unwrap();
        store
            .put(&SecretRef("b".into()), RedactedString::new("top-secret"))
            .await
            .unwrap();
        let a = blobs.get_blob(&SecretRef("a".into())).await.unwrap();
        let b = blobs.get_blob(&SecretRef("b".into())).await.unwrap();
        // At-rest bytes never contain the plaintext...
        assert!(!a.windows(10).any(|w| w == b"top-secret"));
        // ...and a fresh nonce per put makes identical secrets seal differently.
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn a_wrong_key_cannot_open() {
        let (sealer, sealer_blobs) = store_with_blobs(&[1u8; 32]);
        let r = SecretRef("cred:1".into());
        sealer
            .put(&r, RedactedString::new("sk-secret"))
            .await
            .unwrap();
        // Move the ciphertext blob under a store with a different key.
        let blob = sealer_blobs.get_blob(&r).await.unwrap();
        let (attacker, attacker_blobs) = store_with_blobs(&[2u8; 32]);
        attacker_blobs.put_blob(&r, blob).await.unwrap();
        assert!(matches!(attacker.get(&r).await, Err(CredentialError::Seal)));
    }

    #[tokio::test]
    async fn a_tampered_ciphertext_is_rejected() {
        let (store, blobs) = store_with_blobs(&[3u8; 32]);
        let r = SecretRef("cred:1".into());
        store
            .put(&r, RedactedString::new("sk-secret"))
            .await
            .unwrap();
        // Flip one ciphertext byte (past the nonce) in the stored blob.
        let mut blob = blobs.get_blob(&r).await.unwrap();
        blob[NONCE_LEN] ^= 0x01;
        blobs.put_blob(&r, blob).await.unwrap();
        assert!(matches!(store.get(&r).await, Err(CredentialError::Seal)));
    }

    #[tokio::test]
    async fn a_truncated_blob_is_a_seal_error() {
        let (store, blobs) = store_with_blobs(&[4u8; 32]);
        let r = SecretRef("cred:1".into());
        // A blob shorter than the nonce cannot even be split, let alone opened.
        blobs.put_blob(&r, vec![0u8; NONCE_LEN - 1]).await.unwrap();
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
