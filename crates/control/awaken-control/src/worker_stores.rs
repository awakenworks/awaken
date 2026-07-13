//! The shared control-plane store subset a database-less **worker** reads to resolve
//! a drained run's model, opened exactly as the Serve composition opens them.
//!
//! A worker holds no session / config / admin store of its own; it needs only the
//! authored **catalog**, the **credential** repo, and the sealed **secret** store to
//! turn a run's `model_ref` into a real executor (`model_ref → offering(provider) →
//! the workspace's Active credential → resolve_inference → executor`). This module
//! opens exactly those three ports from a [`ControlStoreConfig`], plus the seal-key
//! resolution the durable path needs — the same per-component databases the control
//! plane authored (Option A, shared-DB), so the worker and the console agree on
//! model identity without the worker re-implementing the store wiring.

use std::path::Path;
use std::sync::Arc;

use crate::control_stores::{ControlStoreConfig, StoreBackend};

/// The three ports a worker resolves models from: the authored catalog, the
/// credential repo, and the sealed secret store.
pub struct SharedConfigStores {
    pub catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    pub credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    pub secrets: Arc<dyn awaken_credential_vault::SecretStore>,
}

/// Open the catalog + credential vault + sealed secret store from `cfg`, sealing
/// secrets under `key`. Each component honors its own `AWAKEN_<COMPONENT>_DB`
/// backend (a SQLite file or a shared Postgres) — the durable, shared-DB path.
pub async fn open_shared_config_stores(
    cfg: &ControlStoreConfig,
    key: &[u8; 32],
) -> SharedConfigStores {
    fn ensure_parent(backend: &StoreBackend) {
        if let StoreBackend::Sqlite(path) = backend {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create control-store directory");
            }
        }
    }
    let path = |p: &Path| p.to_string_lossy().into_owned();

    ensure_parent(&cfg.catalog);
    let catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo> = match &cfg.catalog {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_model_catalog::SqliteCatalogRepo::open(&path(p)).expect("open catalog sqlite"),
        ),
        StoreBackend::Postgres(url) => Arc::new(
            awaken_model_catalog::PostgresCatalogRepo::connect(url)
                .await
                .expect("connect catalog postgres"),
        ),
    };

    // The credential repo and its sealed-secret blobs share the one credential backend.
    ensure_parent(&cfg.credential);
    let (credentials, secrets): (
        Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        Arc<dyn awaken_credential_vault::SecretStore>,
    ) = match &cfg.credential {
        StoreBackend::Sqlite(p) => {
            let file = path(p);
            let creds = Arc::new(
                awaken_credential_vault::SqliteCredentialRepo::open(&file)
                    .expect("open credential sqlite"),
            );
            let blobs = awaken_credential_vault::SqliteSealedBlobStore::open(&file)
                .expect("open credential sealed-blob sqlite");
            (
                creds,
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
        StoreBackend::Postgres(url) => {
            let creds = Arc::new(
                awaken_credential_vault::PostgresCredentialRepo::connect(url)
                    .await
                    .expect("connect credential postgres"),
            );
            let blobs = awaken_credential_vault::PostgresSealedBlobStore::connect(url)
                .await
                .expect("connect credential sealed-blob postgres");
            (
                creds,
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
    };

    SharedConfigStores {
        catalog,
        credentials,
        secrets,
    }
}

/// Ephemeral in-process stores (dev / e2e default): a worker sharing an in-memory
/// deployment resolves models from the same process-global stores an all-in-one
/// server would — empty until a model is authored, so a run falls back to the
/// no-model default.
fn in_memory_shared_config_stores() -> SharedConfigStores {
    SharedConfigStores {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
    }
}

/// Open the worker's shared stores **selected from the environment**, exactly the way
/// the Serve composition selects persistence:
///
/// - `AWAKEN_MGMT_DIR=<dir>` — the per-component durable backends under `<dir>` (each
///   honoring its `AWAKEN_<COMPONENT>_DB` override), secrets AEAD-sealed under the
///   seal key from exactly one of `AWAKEN_MGMT_SEAL_KEY` / `AWAKEN_MGMT_SEAL_KEY_FILE`.
/// - unset — in-memory stores.
pub async fn open_shared_config_stores_from_env() -> SharedConfigStores {
    match std::env::var("AWAKEN_MGMT_DIR") {
        Ok(dir) => {
            let key = mgmt_seal_key_from_env();
            let cfg = ControlStoreConfig::from_env(Path::new(&dir));
            open_shared_config_stores(&cfg, &key).await
        }
        Err(_) => in_memory_shared_config_stores(),
    }
}

/// Parse `AWAKEN_MGMT_SEAL_KEY`: exactly 64 hex characters (a 32-byte AEAD key).
fn parse_seal_key(hex: &str) -> Result<[u8; 32], String> {
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

/// Pure resolution of the seal-key hex from its two possible sources (inline env or a
/// file path), so the precedence + mutual-exclusion rules are testable without the
/// environment or filesystem. Mirrors the composition root's resolution exactly.
fn resolve_seal_key_hex(
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

/// The AEAD key for the durable shared stores, from `AWAKEN_MGMT_SEAL_KEY` (inline)
/// or `AWAKEN_MGMT_SEAL_KEY_FILE` (a path). Exactly one must be set. Panics loudly
/// when unset, both-set, unreadable, or malformed — the same fail-closed behavior as
/// the Serve composition (a worker that sealed nothing or read a wrong key is worse
/// than one that refuses to start).
fn mgmt_seal_key_from_env() -> [u8; 32] {
    let hex = resolve_seal_key_hex(
        std::env::var("AWAKEN_MGMT_SEAL_KEY").ok(),
        std::env::var("AWAKEN_MGMT_SEAL_KEY_FILE").ok(),
        |p| std::fs::read_to_string(p),
    )
    .unwrap_or_else(|reason| panic!("{reason}."));
    parse_seal_key(&hex).unwrap_or_else(|reason| {
        panic!("the management seal key is malformed: {reason}. Provide 64 hex characters (a 32-byte key).")
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_seal_key_hex;

    fn no_read(_: &str) -> std::io::Result<String> {
        panic!("read_file should not be called");
    }

    #[test]
    fn inline_key_is_used_verbatim() {
        assert_eq!(
            resolve_seal_key_hex(Some("abc".into()), None, no_read).unwrap(),
            "abc"
        );
    }

    #[test]
    fn both_sources_set_is_a_hard_error() {
        let err = resolve_seal_key_hex(Some("abc".into()), Some("/p".into()), no_read).unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn neither_source_set_is_the_brick_on_restart_error() {
        let err = resolve_seal_key_hex(None, None, no_read).unwrap_err();
        assert!(err.contains("neither AWAKEN_MGMT_SEAL_KEY"), "{err}");
    }
}
