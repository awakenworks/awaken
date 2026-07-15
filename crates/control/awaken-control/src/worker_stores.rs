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
        if let StoreBackend::Sqlite(path) = backend
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).expect("create control-store directory");
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

/// Open the config store selected from the environment and build a **warm-loaded**
/// [`ConfigService`](awaken_config_service::ConfigService) over it, so a
/// database-less worker resolves a published run's config from the shared control
/// plane — its `installed` projection repopulated on boot exactly like the Serve
/// composition. Reads the same `AWAKEN_CONFIG_DB` backend the authoring plane writes;
/// an in-memory service (no publications) when `AWAKEN_MGMT_DIR` is unset. No model
/// resolver is wired: a worker only *reads* installed publications, it never
/// (re)publishes.
pub async fn warm_config_service_from_env() -> std::sync::Arc<awaken_config_service::ConfigService>
{
    let service = awaken_config_service::ConfigService::new();
    if let Ok(dir) = std::env::var("AWAKEN_MGMT_DIR") {
        let cfg = ControlStoreConfig::from_env(Path::new(&dir));
        if let StoreBackend::Sqlite(path) = &cfg.config
            && let Some(parent) = path.parent()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let store: Arc<dyn awaken_config_store::ScopedConfigRegistry> = match &cfg.config {
            StoreBackend::Sqlite(p) => Arc::new(
                awaken_config_store::SqliteConfigStore::open(&p.to_string_lossy())
                    .expect("open config sqlite"),
            ),
            StoreBackend::Postgres(url) => Arc::new(
                awaken_config_store::PostgresConfigStore::connect(url)
                    .await
                    .expect("connect config postgres"),
            ),
        };
        let warmed = service
            .warm_install(
                store.as_ref(),
                &awaken_tenancy::ScopeId::from(awaken_config_store::DEFAULT_SCOPE),
            )
            .await;
        if warmed > 0 {
            eprintln!(
                "worker config: warm-loaded {warmed} published agent(s) from the config store"
            );
        }
    }
    std::sync::Arc::new(service)
}

/// The AEAD key for the durable shared stores, from `AWAKEN_MGMT_SEAL_KEY` (inline)
/// or `AWAKEN_MGMT_SEAL_KEY_FILE` (a path). Exactly one must be set. Panics loudly
/// when unset, both-set, unreadable, or malformed — the same fail-closed behavior as
/// the Serve composition (a worker that sealed nothing or read a wrong key is worse
/// than one that refuses to start). The pure resolution lives once in
/// `awaken_credential_vault` (shared with the Serve root); this only wires the env.
fn mgmt_seal_key_from_env() -> [u8; 32] {
    let hex = awaken_credential_vault::resolve_seal_key_hex(
        std::env::var("AWAKEN_MGMT_SEAL_KEY").ok(),
        std::env::var("AWAKEN_MGMT_SEAL_KEY_FILE").ok(),
        |p| std::fs::read_to_string(p),
    )
    .unwrap_or_else(|reason| panic!("{reason}."));
    awaken_credential_vault::parse_seal_key(&hex).unwrap_or_else(|reason| {
        panic!("the management seal key is malformed: {reason}. Provide 64 hex characters (a 32-byte key).")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::{SecretRef, SecretStore};
    use awaken_model_catalog::repo::CatalogRepo;

    /// A [`ControlStoreConfig`] whose sqlite files live under a *nested,
    /// not-yet-existing* directory, so opening must exercise `ensure_parent`.
    fn nested_sqlite_cfg(root: &Path) -> ControlStoreConfig {
        let dir = root.join("deep").join("nested");
        ControlStoreConfig {
            catalog: StoreBackend::Sqlite(dir.join("catalog.db")),
            credential: StoreBackend::Sqlite(dir.join("credential.db")),
            config: StoreBackend::Sqlite(dir.join("config.db")),
            admin: StoreBackend::Sqlite(dir.join("admin.db")),
            sessions: StoreBackend::Sqlite(dir.join("sessions.db")),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sqlite_backends_open_under_a_missing_parent_and_the_seal_round_trips() {
        // The worker's model-resolution store subset must open cleanly from a
        // durable sqlite config (creating the missing parent dir), migrate, and
        // seal/reveal a secret under the provided key — the end-to-end wiring a
        // drained run relies on, with no live DB.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = nested_sqlite_cfg(tmp.path());
        let key = [7u8; 32];

        let stores = open_shared_config_stores(&cfg, &key).await;

        // The nested parent was created by ensure_parent (else the opens panic).
        assert!(tmp.path().join("deep").join("nested").is_dir());

        // A freshly-opened catalog migrates to an empty, valid snapshot.
        let snapshot = stores.catalog.snapshot().await.expect("catalog snapshot");
        assert!(snapshot.offerings.is_empty());
        // And the credential repo reads an empty workspace without error.
        assert!(
            stores
                .credentials
                .list("wrkspc_default")
                .await
                .expect("credential list")
                .is_empty()
        );

        // The sealed-AEAD secret store round-trips through the credential backend
        // under the supplied key: put ciphertext, reveal the original plaintext.
        let secret_ref = SecretRef("worker-seal-probe".to_string());
        stores
            .secrets
            .put(&secret_ref, RedactedString::new("s3cr3t-value"))
            .await
            .expect("seal a secret");
        let revealed = stores.secrets.get(&secret_ref).await.expect("reveal");
        assert_eq!(revealed.expose_secret(), "s3cr3t-value");
    }
}
