//! `awaken-cli` library: the single-machine **composition root**.
//!
//! Stage B2 split the management assembly into two sibling planes — the authoring /
//! authz plane (`awaken-control`) and the data plane (`awaken-server`) — that do NOT
//! depend on each other. This crate is the composition root that weaves them: it
//! opens the management stores, builds the shared handles (the config plane, the
//! vault/environment state), asks `awaken-control` for the guarded authoring router,
//! asks `awaken-server` for the data-plane router (`mount_with_managed`), merges
//! them, and applies workspace path addressing — behavior byte-identical to the
//! pre-split `build_management_router`.
//!
//! The `awaken` binary ([`main`](../main.rs)) is a thin shell over this library.

mod brain_admin;
pub mod config;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use awaken_protocol_managed::{EnvironmentState, ManagedState, VaultState};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{
    ConfigPlane, ConfigService, ExtMcpProbe, ManagedHost, RESERVED_ADMIN_SCOPE, ScopedToolCatalog,
    SharedHost, ToolCatalogSource, advertised_tools,
};
use axum::Router;

pub use crate::brain_admin::{
    DrainController, brain_admin_router, register_active_streams_gauge, with_brain_admin,
    with_connection_metric,
};
// Embedded management-plane IAM (ADR-0042/0043 P1) + the mint spec and bootstrap
// constants a test / operator embedding drives — re-exported from the authoring plane.
pub use awaken_control::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz,
    ManagementIdentityMode, RemoteManagementAuthz, TokenSpec, embedded_iam,
};

/// The live credential probe backing the admin router, backed by provider-genai
/// here — the only place the model SDK is named in the composition; the admin CRUD
/// crate stays SDK-free. `ghost` providers simply resolve `Unknown`.
struct GenaiProbe;

#[async_trait::async_trait]
impl awaken_admin_config_api::CredentialProbe for GenaiProbe {
    async fn probe(
        &self,
        base_url: &str,
        secret: &awaken_agent_contract::RedactedString,
        model: &str,
    ) -> awaken_admin_config_api::ProbeStatus {
        use awaken_admin_config_api::ProbeStatus;
        use awaken_provider_genai::CredentialProbe;
        match awaken_provider_genai::probe_credential(base_url, secret.expose_secret(), model).await
        {
            CredentialProbe::Valid => ProbeStatus::Valid,
            CredentialProbe::Invalid => ProbeStatus::Invalid,
            CredentialProbe::Unknown => ProbeStatus::Unknown,
        }
    }
}

/// The store set the management plane runs over — one instance of each port,
/// shared by the authoring router, the vault front door, and session prepare.
struct ManagementStores {
    /// Durable installation root used to persist the platform Workspace id.
    workspace_root: Option<std::path::PathBuf>,
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>,
    mcp: Arc<dyn awaken_admin_config_api::McpStore>,
    /// Memory-store identity registry (ADR-0038): the durable id/name/metadata aggregate,
    /// the same admin store as `mcp`/`profiles`, a distinct port. Injected into the host
    /// so a store survives a restart, and into the capability inventory so the admin
    /// assistant can enumerate stores.
    memory_registry: Arc<dyn awaken_admin_config_api::MemoryStoreRegistry>,
    resources: Arc<dyn awaken_config_resolver::ResourceStore>,
    /// Authored webhook endpoints (ADR-0048), an id-addressed config resource beside
    /// profiles/MCP — the same admin store, a distinct port.
    webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>,
    /// Durable home for the Managed session aggregate (its own `sessions.db`), so a
    /// rehydrated session reports its real config across a restart / peer process.
    sessions: Arc<dyn awaken_protocol_managed::ManagedSessionRepository>,
    /// The config authoring plane (`config.db`): the rich `AgentConfig` drafts the
    /// management console authors directly, and their publications. Scoped so a
    /// workspace's config is fenced from another's (ADR-0051).
    config: Arc<dyn awaken_config_store::ScopedConfigRegistry>,
    /// Self-hosted environments registry + work queue, durable per deployment mode.
    environments: Arc<awaken_protocol_managed::EnvironmentState>,
}

const CREDENTIAL_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(60);

/// Keep retrying interrupted credential creations after startup. A failed secret
/// deletion leaves its durable intent intact, so the next tick resumes safely.
fn spawn_credential_creation_reconciliation(
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CREDENTIAL_RECONCILIATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The composition root already performed the first pass synchronously.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = awaken_credential_vault::repo::recover_credential_creations(
                secrets.as_ref(),
                credentials.as_ref(),
            )
            .await
            {
                eprintln!("credential creation reconciliation failed: {error}");
            }
            match awaken_credential_vault::repo::reconcile_credential_inventory(
                secrets.as_ref(),
                credentials.as_ref(),
            )
            .await
            {
                Ok(report) if !report.missing_material.is_empty() => eprintln!(
                    "credential inventory is missing referenced material: {:?}",
                    report.missing_material
                ),
                Err(error) => eprintln!("credential inventory reconciliation failed: {error}"),
                _ => {}
            }
        }
    });
}

/// Ephemeral management stores: everything in process memory (dev / e2e default).
fn in_memory_management_stores() -> ManagementStores {
    ManagementStores {
        workspace_root: None,
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        memory_registry: Arc::new(awaken_admin_config_api::InMemoryMemoryStoreRegistry::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryResourceStore::new()),
        webhooks: Arc::new(awaken_admin_config_api::InMemoryWebhookStore::new()),
        sessions: Arc::new(awaken_protocol_managed::InMemorySessionRepository::default()),
        config: Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
        ),
        environments: Arc::new(EnvironmentState::new()),
    }
}

/// Durable management stores under `dir` (created if absent), ADR-0043
/// sqlite-repos: one SQLite file per domain bundle (`catalog.db` / `credential.db`
/// / `admin.db` / `sessions.db` / `config.db`), secrets AEAD-sealed under `key`.
///
/// Panics on open/migrate failure: the binary's mode selection has no error
/// channel, and a management server that silently fell back to ephemeral stores
/// would be worse than one that refuses to start.
fn durable_management_stores(dir: &std::path::Path, key: &[u8; 32]) -> ManagementStores {
    std::fs::create_dir_all(dir).expect("create AWAKEN_MGMT_DIR");
    let db = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let catalog = awaken_model_catalog::sqlite::SqliteCatalogRepo::open(&db("catalog.db"))
        .expect("open catalog.db under AWAKEN_MGMT_DIR");
    let credentials = awaken_credential_vault::SqliteCredentialRepo::open(&db("credential.db"))
        .expect("open credential.db under AWAKEN_MGMT_DIR");
    let blobs = awaken_credential_vault::SqliteSealedBlobStore::open(&db("credential.db"))
        .expect("open credential.db sealed-blob store under AWAKEN_MGMT_DIR");
    let admin = Arc::new(
        awaken_admin_config_api::SqliteAdminStore::open(&db("admin.db"))
            .expect("open admin.db under AWAKEN_MGMT_DIR"),
    );
    ManagementStores {
        workspace_root: Some(dir.to_path_buf()),
        catalog: Arc::new(catalog),
        credentials: Arc::new(credentials),
        // The only durable secret path is sealed: `nonce ‖ ciphertext` under the
        // operator-held key — plaintext never reaches the disk.
        secrets: Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
            key,
            Arc::new(blobs),
        )),
        profiles: admin.clone(),
        mcp: admin.clone(),
        // Memory-store identity is one more secret-free table under the `admin` bundle,
        // so the same admin store serves the registry port (durable across a restart).
        memory_registry: admin.clone(),
        resources: admin.clone(),
        // Webhook endpoints share admin.db (one more secret-free table under the
        // `admin` bundle) — a config resource like the profiles/MCP defs above.
        webhooks: admin,
        // A separate `sessions.db` (not a table in admin.db): a live session
        // instance is a different aggregate from the agent/MCP definitions admin.db
        // holds (ADR-0039 one-repository-per-aggregate).
        sessions: Arc::new(
            awaken_runtime_host::SqliteManagedSessionRepository::open(&db("sessions.db"))
                .expect("open sessions.db under AWAKEN_MGMT_DIR"),
        ),
        // The config authoring plane persists agent drafts/publications under its
        // own `config.db` (ADR-0029/0031 `config` namespace).
        config: Arc::new(
            awaken_config_store::SqliteConfigStore::open(&db("config.db"))
                .expect("open config.db under AWAKEN_MGMT_DIR"),
        ),
        // Self-hosted env registry + work queue, their own sqlite files beside the
        // session store (durable so a self-hosted worker survives a restart).
        environments: Arc::new(EnvironmentState::with_stores(
            Arc::new(
                awaken_runtime_host::SqliteEnvRegistry::open(&db("environments.db"))
                    .expect("open environments.db under AWAKEN_MGMT_DIR"),
            ),
            Arc::new(
                awaken_runtime_host::SqliteWorkQueue::open(&db("work_queue.db"))
                    .expect("open work_queue.db under AWAKEN_MGMT_DIR"),
            ),
        )),
    }
}

/// Open the control-plane stores per the [`ControlStoreConfig`](awaken_control::ControlStoreConfig)
/// — each component on its own database (SQLite file or shared Postgres). This
/// generalizes [`durable_management_stores`] (the all-SQLite bundle special case):
/// each store independently honors its `AWAKEN_<COMPONENT>_DB` override, so a
/// separate control / server process can share the same per-component databases
/// (Option A, shared-DB).
async fn open_management_stores(
    cfg: awaken_control::ControlStoreConfig,
    key: &[u8; 32],
) -> ManagementStores {
    use awaken_control::StoreBackend;

    // Create the parent directory for any SQLite path (a bundle dir or a custom path).
    fn ensure_parent(backend: &StoreBackend) {
        if let StoreBackend::Sqlite(path) = backend
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).expect("create control-store directory");
        }
    }
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();

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

    // The admin aggregate backs three ports (profiles / MCP / webhooks) off one store.
    ensure_parent(&cfg.admin);
    let admin_profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>;
    let admin_mcp: Arc<dyn awaken_admin_config_api::McpStore>;
    let admin_memory: Arc<dyn awaken_admin_config_api::MemoryStoreRegistry>;
    let admin_webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>;
    let admin_resources: Arc<dyn awaken_config_resolver::ResourceStore>;
    match &cfg.admin {
        StoreBackend::Sqlite(p) => {
            let admin = Arc::new(
                awaken_admin_config_api::SqliteAdminStore::open(&path(p))
                    .expect("open admin sqlite"),
            );
            admin_profiles = admin.clone();
            admin_mcp = admin.clone();
            admin_memory = admin.clone();
            admin_resources = admin.clone();
            admin_webhooks = admin;
        }
        StoreBackend::Postgres(url) => {
            // `PostgresAdminStore::connect` builds its own runtime and blocks, so it
            // must run off the async worker thread to avoid a nested-runtime panic.
            let url = url.clone();
            let admin = Arc::new(
                tokio::task::spawn_blocking(move || {
                    awaken_admin_config_api::PostgresAdminStore::connect(&url)
                })
                .await
                .expect("join admin postgres connect")
                .expect("connect admin postgres"),
            );
            admin_profiles = admin.clone();
            admin_mcp = admin.clone();
            admin_memory = admin.clone();
            admin_resources = admin.clone();
            admin_webhooks = admin;
        }
    }

    ensure_parent(&cfg.sessions);
    let sessions: Arc<dyn awaken_protocol_managed::ManagedSessionRepository> = match &cfg.sessions {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_runtime_host::SqliteManagedSessionRepository::open(&path(p))
                .expect("open sessions sqlite"),
        ),
        StoreBackend::Postgres(url) => Arc::new(
            awaken_runtime_host::PostgresManagedSessionRepository::connect(url)
                .await
                .expect("connect sessions postgres"),
        ),
    };

    ensure_parent(&cfg.config);
    let config: Arc<dyn awaken_config_store::ScopedConfigRegistry> = match &cfg.config {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_config_store::SqliteConfigStore::open(&path(p)).expect("open config sqlite"),
        ),
        StoreBackend::Postgres(url) => Arc::new(
            awaken_config_store::PostgresConfigStore::connect(url)
                .await
                .expect("connect config postgres"),
        ),
    };

    // Self-hosted env registry + work queue follow the session store's backend kind
    // (their own table namespaces, so a shared DB is fine).
    let environments: Arc<EnvironmentState> = match &cfg.sessions {
        StoreBackend::Sqlite(sp) => Arc::new(EnvironmentState::with_stores(
            Arc::new(
                awaken_runtime_host::SqliteEnvRegistry::open(&path(
                    &sp.with_file_name("environments.db"),
                ))
                .expect("open environments sqlite"),
            ),
            Arc::new(
                awaken_runtime_host::SqliteWorkQueue::open(&path(
                    &sp.with_file_name("work_queue.db"),
                ))
                .expect("open work_queue sqlite"),
            ),
        )),
        StoreBackend::Postgres(url) => Arc::new(EnvironmentState::with_stores(
            Arc::new(
                awaken_runtime_host::PostgresEnvRegistry::connect(url)
                    .await
                    .expect("connect environments postgres"),
            ),
            Arc::new(
                awaken_runtime_host::PostgresWorkQueue::connect(url)
                    .await
                    .expect("connect work_queue postgres"),
            ),
        )),
    };

    ManagementStores {
        workspace_root: None,
        catalog,
        credentials,
        secrets,
        profiles: admin_profiles,
        mcp: admin_mcp,
        memory_registry: admin_memory,
        resources: admin_resources,
        webhooks: admin_webhooks,
        sessions,
        config,
        environments,
    }
}

/// The AEAD key for the durable management plane, from `AWAKEN_MGMT_SEAL_KEY` (inline)
/// **or** `AWAKEN_MGMT_SEAL_KEY_FILE` (a path to a file holding it). Exactly one must
/// be set. Fails loudly when unset, both-set, unreadable, or malformed. The pure
/// resolution lives once in `awaken_credential_vault` (shared with the worker); this
/// only wires the env.
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

/// Serve the management plane (authoring + data plane) with **persistence selected
/// from the environment**:
///
/// - `AWAKEN_MGMT_DIR` unset — in-memory stores, exactly the previous behavior.
/// - `AWAKEN_MGMT_DIR=<dir>` — SQLite-backed stores under `<dir>`, secrets AEAD-sealed
///   under the seal key (required then; unset or malformed panics rather than sealing
///   under a key that cannot survive a restart). The key comes from exactly one of
///   `AWAKEN_MGMT_SEAL_KEY` (inline) or `AWAKEN_MGMT_SEAL_KEY_FILE`.
///
/// Additionally (ADR-0042/0043 P1), `AWAKEN_MGMT_IAM=embedded` gates the management
/// surfaces behind bearer `ApiToken` authn + preset-role authz; it requires
/// `AWAKEN_MGMT_DIR` and panics with a clear message when it is missing. Unset — the
/// default — is today's open behavior, byte-identical.
pub async fn build_management_router() -> Router {
    build_management_router_with_fallback(
        Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
        awaken_server::no_model::UNCONFIGURED_MODEL_REF.to_string(),
    )
    .await
}

/// [`build_management_router`] with the host default (pre-published) model injected —
/// the env-driven store selection (durable/in-memory + IAM) is IDENTICAL to the
/// production entry point, only the no-model fallback differs. Exposed so the e2e
/// scenario host can drive the REAL management router with a deterministic model
/// (e.g. the MCP scenario model) while keeping the production fallback provider-free.
pub async fn build_management_router_with_fallback(
    fallback_model: Arc<dyn LlmExecutor>,
    fallback_model_ref: String,
) -> Router {
    let legacy_mode = std::env::var("AWAKEN_MGMT_IAM").ok();
    let identity_mode = std::env::var("AWAKEN_IDENTITY_MODE")
        .ok()
        .as_deref()
        .and_then(ManagementIdentityMode::parse)
        .or_else(|| {
            legacy_mode
                .as_deref()
                .and_then(ManagementIdentityMode::parse)
        })
        .unwrap_or(ManagementIdentityMode::NoLogin);
    let (iam, remote_iam) = match identity_mode {
        ManagementIdentityMode::SelfManaged => {
            let dir = std::env::var("AWAKEN_MGMT_DIR").unwrap_or_else(|_| {
                panic!(
                    "self-managed IAM requires AWAKEN_MGMT_DIR: the embedded \
                     IAM persists its API tokens and role bindings under \
                     <AWAKEN_MGMT_DIR>/iam.sqlite; an in-memory token directory would \
                     mint a fresh bootstrap admin token on every restart."
                )
            });
            let workspace = SharedHost::provision_local_workspace_at(std::path::Path::new(&dir));
            let iam = awaken_control::embedded_iam_for_tenant(
                std::path::Path::new(&dir),
                &local_org_id(),
                &workspace,
            );
            // Workspace membership is platform topology (PIP), not an implicit side
            // effect of token minting. A self-managed composition may explicitly
            // declare additional workspaces; registering them grants no permissions,
            // it only makes them valid policy scopes under the hidden local org.
            if let Ok(configured) = std::env::var("AWAKEN_IAM_WORKSPACES") {
                for workspace_id in configured
                    .split(',')
                    .map(str::trim)
                    .filter(|workspace_id| !workspace_id.is_empty())
                {
                    iam.register_workspace(workspace_id);
                }
            }
            (Some(iam), None)
        }
        ManagementIdentityMode::AwakenCloud => (
            None,
            Some(
                awaken_cloud_authz_from_env()
                    .unwrap_or_else(|error| panic!("Awaken Cloud identity: {error}")),
            ),
        ),
        ManagementIdentityMode::NoLogin => (None, None),
    };
    match std::env::var("AWAKEN_MGMT_DIR") {
        Ok(dir) => {
            let key = mgmt_seal_key_from_env();
            // Each control-plane store honors its own `AWAKEN_<COMPONENT>_DB` override
            // (SQLite path or shared Postgres), defaulting to `<dir>/<name>.db`.
            let cfg = awaken_control::ControlStoreConfig::from_env(std::path::Path::new(&dir));
            management_router_over(
                open_management_stores(cfg, &key).await,
                iam,
                remote_iam,
                fallback_model,
                fallback_model_ref,
                None,
            )
            .await
        }
        Err(_) => {
            management_router_over(
                in_memory_management_stores(),
                iam,
                remote_iam,
                fallback_model,
                fallback_model_ref,
                None,
            )
            .await
        }
    }
}

fn awaken_cloud_authz_from_env() -> Result<Arc<RemoteManagementAuthz>, String> {
    let base_url = std::env::var("AWAKEN_CLOUD_IAM_URL")
        .unwrap_or_else(|_| "https://accounts.awakenworks.com".to_string());
    let user_token = std::env::var("AWAKEN_CLOUD_ACCESS_TOKEN")
        .ok()
        .or_else(|| {
            awaken_iam_client::CredentialCache::open()
                .load(&base_url)
                .map(|entry| entry.token.expose().to_owned())
        })
        .ok_or_else(|| "Awaken Cloud login credential is missing or expired".to_string())?;
    RemoteManagementAuthz::connect(
        base_url,
        std::env::var("AWAKEN_CLOUD_IAM_AUDIENCE").unwrap_or_else(|_| "awaken-runtime".to_string()),
        std::env::var("AWAKEN_CLOUD_IAM_ISSUER")
            .unwrap_or_else(|_| "https://accounts.awakenworks.com".to_string()),
        user_token,
        std::env::var("AWAKEN_CLOUD_IAM_SERVICE_TOKEN").ok(),
    )
}

/// [`build_management_router_with_model`] plus a last-mile hook on the assembled host
/// (`customize_host`) — the seam a composition root uses to wire a runtime backend the
/// management plane does not assemble itself, e.g. `host.with_acp(executor)` so `acp:*`
/// threads run on an external CLI while the full managed plane (vault + MCP staging +
/// config plane) is still in play. Keeps the ACP executor's crate out of this module.
pub async fn build_management_router_with_host_customizer(
    model: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    management_router_over(
        in_memory_management_stores(),
        None,
        None,
        model,
        model_ref.into(),
        Some(Box::new(customize_host)),
    )
    .await
}

/// Build the management router over in-memory stores with an explicit host default
/// model injected — a **test-only** seam so an integration test can drive the real
/// management router with a deterministic (mock) model, keeping the mock out of the
/// production assembly.
pub async fn build_management_router_with_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> Router {
    management_router_over(
        in_memory_management_stores(),
        None,
        None,
        model,
        model_ref.into(),
        None,
    )
    .await
}

/// [`build_management_router`] with explicit persistence inputs (no environment
/// read): the durable management plane over `dir`, sealing secrets under `key`.
/// Exposed so a restart test can rebuild a router over one directory across
/// simulated process lifetimes without racing on process-global env vars.
/// No IAM guard — the open (default) management plane.
pub async fn build_durable_management_router(dir: &std::path::Path, key: &[u8; 32]) -> Router {
    management_router_over(
        durable_management_stores(dir, key),
        None,
        None,
        Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
        awaken_server::no_model::UNCONFIGURED_MODEL_REF.to_string(),
        None,
    )
    .await
}

/// [`build_durable_management_router`] with the embedded IAM guard enabled — the
/// env-free equivalent of `AWAKEN_MGMT_IAM=embedded`. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint further
/// workspace tokens against the same policy state.
pub async fn build_secured_management_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let router = management_router_over(
        durable_management_stores(dir, key),
        Some(iam.clone()),
        None,
        Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
        awaken_server::no_model::UNCONFIGURED_MODEL_REF.to_string(),
        None,
    )
    .await;
    (router, iam)
}

/// Mount the management plane over an explicit store set, optionally gated by the
/// embedded IAM guard (`iam`). The authoring / authz half comes from
/// [`awaken_control::control_router`] (guard wraps ONLY admin + vault); the data
/// plane comes from [`awaken_server::mount_with_managed`]; this composition root
/// weaves them and keeps the warm-load + no-model fallback wired here.
async fn management_router_over(
    stores: ManagementStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    // The host default model for the window before an operator publishes one. In
    // production this is the provider-free `NoModelConfiguredExecutor` (guidance,
    // never a mock); a test may inject a deterministic model.
    fallback_model: Arc<dyn LlmExecutor>,
    fallback_model_ref: String,
    // An optional last-mile hook on the assembled data-plane host, applied before it is
    // shared. The composition root uses it to wire a runtime backend the management plane
    // does not assemble itself (e.g. an ACP executor for `acp:*` threads) without this
    // module naming that backend's crate. `None` in production; `Some` in a scenario that
    // serves external-CLI sessions.
    customize_host: Option<Box<dyn FnOnce(SharedHost) -> SharedHost + Send>>,
) -> Router {
    let ManagementStores {
        workspace_root,
        catalog,
        credentials,
        secrets,
        profiles,
        mcp: mcp_store,
        memory_registry,
        resources: resource_store,
        webhooks: webhook_store,
        sessions,
        config,
        environments,
    } = stores;
    // Resolve the installation's Workspace exactly once, then inject the same
    // coordinate into every adapter assembled below. Durable roots persist it;
    // ephemeral roots receive a process-local generated coordinate.
    let platform_workspace = workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    // Finish or compensate any credential creation interrupted by a prior hard
    // process crash before exposing the management/data planes.
    if let Err(error) = awaken_credential_vault::repo::recover_credential_creations(
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    {
        eprintln!("credential creation recovery failed: {error}");
    }
    match awaken_credential_vault::repo::reconcile_credential_inventory(
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    {
        Ok(report) if !report.missing_material.is_empty() => eprintln!(
            "credential inventory is missing referenced material: {:?}",
            report.missing_material
        ),
        Err(error) => eprintln!("credential inventory reconciliation failed: {error}"),
        _ => {}
    }
    spawn_credential_creation_reconciliation(secrets.clone(), credentials.clone());
    // ONE resource-binding store shared by the admin router (which authors an agent's
    // resources) and the config service (which reads them into resource prompts +
    // mounts at compile) — so a binding authored through the API reaches the compiled
    // config (ADR-0038).
    // The Managed vault state, shared by the vault router (authoring) and the managed
    // state (data plane): a credential entered through either surface is the same row.
    let vault_state = Arc::new(
        VaultState::new(secrets.clone(), credentials.clone())
            // The live MCP probe is backed by ext-mcp here — the only place the MCP
            // client is named for validation (mirrors the GenaiProbe pattern).
            .with_probe(Arc::new(ExtMcpProbe)),
    );
    // Environments + work queue, shared with the session state so `POST /v1/sessions`
    // resolves an environment's networking policy (egress on/off) at creation.
    let env_state = environments;

    // The server's default model for the window before a publish.
    let (model, model_ref) = (fallback_model, fallback_model_ref);
    let inference_access_publisher = Arc::new(
        awaken_server::inference_materializer::CatalogInferenceAccessPublisher::new(
            catalog.clone(),
            credentials.clone(),
        )
        .with_fallback_model(model_ref.clone()),
    );
    let inference_materializer = Arc::new(
        awaken_server::inference_materializer::CredentialInferenceMaterializer::new(
            credentials.clone(),
            secrets.clone(),
        )
        .with_fallback_executor(model_ref.clone(), model.clone()),
    );
    let global = advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
    let tool_catalog: Arc<dyn ToolCatalogSource> = Arc::new(ScopedToolCatalog::new(
        global.clone(),
        RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    let config_service = Arc::new(
        ConfigService::new()
            // Resolve `Auto` against the LIVE catalog repo (not the frozen seed), so a
            // model an operator adds AFTER startup is visible when we re-publish the
            // reserved-scope assistant. `catalog` is still in scope here (moved into the
            // control router below); clone the Arc for the resolver.
            .with_model_resolver(Arc::new(
                awaken_server::model_resolver::CatalogModelResolver::from_repo(catalog.clone()),
            ))
            .with_inference_access_publisher(inference_access_publisher)
            .with_resources(resource_store.clone()),
    );
    // Warm-load the installed catalog from the durable config store BEFORE the plane
    // takes ownership, so a fresh process (a restart, or a server that did not author
    // the publish) repopulates `installed`. A no-op on the in-memory path.
    let warmed = config_service
        .warm_install(
            config.as_ref(),
            &awaken_tenancy::ScopeId::from(platform_workspace.as_str()),
        )
        .await;
    if warmed > 0 {
        eprintln!("config: warm-loaded {warmed} published agent(s) from the durable store");
    }
    let plane = ConfigPlane::new(config_service.clone(), config, tool_catalog);
    // Seed the in-console Admin Assistant as an ordinary published agent in the
    // reserved scope (ADR-0052 D1/D2). Best-effort: a server booted without a
    // resolvable model still starts (the assistant stays a draft until one is set).
    if let Err(err) = awaken_control::seed_admin_assistant(&plane).await {
        eprintln!("admin assistant not seeded (configure a model, then republish): {err}");
    }
    // When an operator adds a model AFTER startup, re-publish the reserved-scope
    // assistant so its `Auto` binding resolves off `unconfigured` onto the new model.
    // Same reserved scope + agent id `seed_admin_assistant` published under, so the
    // reconcile targets exactly the seeded agent (ADR-0052 D2). Fired by the middleware
    // layer below on a successful catalog write. `ConfigPlane` is `Clone`.
    let reconciler = Arc::new(awaken_runtime_host::ConfigServiceReconciler::new(
        plane.clone(),
        RESERVED_ADMIN_SCOPE,
        vec![awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID.to_string()],
    ));
    // The durable skill catalog under the management storage dir when set, else a
    // per-process temp dir. Built HERE (not inside the host) so ONE store is shared by
    // the host (delivered skills) and the capability inventory (skill enumeration).
    let skill_dir = std::env::var("AWAKEN_MGMT_DIR")
        .map(|d| std::path::PathBuf::from(d).join("skills"))
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!("awaken-skills-{}", std::process::id()))
        });
    let skill_store: Arc<dyn awaken_skill_store::SkillStore> = Arc::new(
        awaken_skill_store::FsSkillStore::open(skill_dir).expect("open durable skill store root"),
    );
    // The LIVE data-plane resource inventory (ADR-0038): memory-store ids from the durable
    // registry (the same admin backend the host writes identity through, so this stays
    // consistent) + skill ids from the shared skill store. Unlike before, this is now
    // reachable at wire time because both handles are assembled by the composition root.
    let resource_inventory = Arc::new(awaken_control::HostResourceInventory::new(
        memory_registry.clone(),
        skill_store.clone(),
        platform_workspace.clone(),
    ));
    // The management tool executables (ADR-0052 D3/D4): the capability reader reads the
    // shared catalog + advertised tools; the validator runs the publish-time compile
    // check on drafts in the tenant scope; Runtime history records every call and
    // mutating tools additionally enter the durable config-change path.
    let admin_execs = awaken_admin_assistant::admin_tools(
        Arc::new(awaken_control::CatalogCapabilityReader::new(
            // The LIVE catalog repo — models/providers an operator adds after startup
            // are visible on the next capabilities call (not a frozen seed snapshot).
            catalog.clone(),
            &global,
            // The installable plugins (state_machine / memory / compact) so the assistant
            // knows it CAN author a state machine etc. — not an empty list (it would
            // otherwise refuse, thinking no plugins exist).
            &awaken_runtime_host::authorable_config_sections(),
            // The LIVE authored MCP servers.
            mcp_store.clone(),
            // The config plane, to list existing agent ids in the tenant scope.
            plane.clone(),
            // LIVE data-plane inventory: memory-store ids (durable registry) + skill ids
            // (shared skill store). Both handles are assembled by this composition root
            // before the host, so the assistant enumerates real memory stores + skills.
            Some(resource_inventory),
        )),
        Arc::new(awaken_control::ConfigServiceDraftValidator::new(
            plane.clone(),
            platform_workspace.clone(),
        )),
        // Persist/read drafts as unpublished config agents through the same plane the
        // editor's Save uses, in the tenant/default scope (ADR-0052).
        Arc::new(awaken_control::ConfigServiceDraftStore::new(
            plane.clone(),
            platform_workspace.clone(),
            resource_store.clone(),
        )),
        // Author environments through the SAME managed-plane registry the console's
        // New-environment modal drives, so `admin_draft_environment` persists for real.
        Arc::new(awaken_control::EnvironmentStateAuthor::new(
            env_state.clone(),
        )),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    // The MCP server export is explicit and capability-token gated. Clone the
    // management executables before the host takes ownership, pairing each with
    // its authoritative descriptor rather than reconstructing a schema here.
    let mcp_export = awaken_server::mcp_export::router(
        awaken_admin_assistant::admin_tool_descriptors(),
        admin_execs.clone(),
        std::env::var("AWAKEN_MCP_BEARER_TOKEN").ok(),
    );

    // The authoring / authz plane (admin + vault + webhooks + user profiles +
    // deployments + environments + config plane + capabilities), guard applied over
    // admin + vault only. Returns the webhook sink the data plane feeds.
    let deployment_state = Arc::new(awaken_protocol_managed::DeploymentState::new());
    let (mgmt, webhook_sink) = awaken_control::control_router(awaken_control::ControlRouterInput {
        catalog,
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles,
        mcp_store: mcp_store.clone(),
        webhook_store,
        sessions: sessions.clone(),
        resource_store: resource_store.clone(),
        probe: Arc::new(GenaiProbe),
        vault_state: vault_state.clone(),
        env_state: env_state.clone(),
        deployment_state: deployment_state.clone(),
        plane,
        config_service: config_service.clone(),
        global_tools: global,
        org_id: Some(local_org_id()),
        iam,
        remote_iam,
    });

    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. It shares the SAME skill store and
    // memory-store identity registry the capability inventory reads, so a skill or memory
    // store the host serves is exactly what the assistant enumerates, and identity
    // survives a restart.
    let host_builder = SharedHost::new(model, model_ref)
        .with_local_workspace(platform_workspace.clone())
        .with_config_service(config_service.clone())
        .with_admin_tools(admin_execs)
        .with_skill_store_backend(skill_store)
        .with_memory_registry(memory_registry)
        // Resolve a session's model to a real executor from the config plane (M2):
        // an unconfigured/unresolvable model falls back to the scenario model above.
        .with_inference_materializer(inference_materializer);
    // Production ACP wiring (`acp:*` threads): `AWAKEN_ACP_CLI` / `AWAKEN_ACP_ARGV`
    // realized in `AWAKEN_SANDBOX_TIER`. The one shared helper both the server and
    // worker roots call, so they never drift (ADR-0057).
    let host_builder = host_builder.with_acp_from_env().await;
    // Last-mile backend wiring the management plane does not assemble itself, injected
    // by the composition root (a scenario that serves external-CLI sessions).
    let host_builder = match customize_host {
        Some(customize) => customize(host_builder),
        None => host_builder,
    };
    let host = Arc::new(host_builder);
    let managed_state = Arc::new(
        ManagedState::new(ManagedHost::new(host.clone()).with_mcp(credentials, secrets, mcp_store))
            .with_vaults(vault_state)
            .with_environments(env_state)
            // Share the SAME config plane `/v1/agents` reads, so a session inheriting a
            // published agent's model sees the authoritative config-plane truth (M2).
            .with_config_source(Arc::new(awaken_runtime_host::ConfigServiceAgentSource(
                config_service.clone(),
            )))
            .with_session_repo(sessions)
            .with_lifecycle_sink(webhook_sink),
    );
    deployment_state.bind_launcher(managed_state.clone());
    // Drive cron Deployments in production. The state mints due runs and launches
    // them through the exact same Session port as the manual `/run` action.
    let scheduled_deployments = deployment_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            interval.tick().await;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            scheduled_deployments.tick_and_launch(now_ms).await;
        }
    });
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope before it re-enters
    // routing. Flat requests fall through unchanged.
    let mut flat = awaken_server::mount_with_managed(host, managed_state).merge(mgmt);
    // Serving tools to an external MCP client is disabled until an operator sets a
    // dedicated bearer. This avoids turning the management toolset into an open
    // mutation surface while still making the `awaken` binary the complete adapter.
    flat = flat.merge(mcp_export);
    // After a successful catalog-mutating write, re-publish the reserved-scope assistant
    // so its `Auto` model binding picks up the model the operator just added. The layer
    // sits on the flat surface INSIDE the workspace path rewrite (which rewrites a
    // `/v1/workspaces/{ws}/config/...` request to its flat `/v1/config/...` form BEFORE
    // re-entering this router), so matching the flat shape covers both address forms.
    // Best-effort: a failed reconcile never fails the operator's request.
    let reconcile_on_catalog_write = axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let reconciler = reconciler.clone();
            async move {
                let method = req.method().clone();
                let path = req.uri().path().to_string();
                let is_write =
                    method == axum::http::Method::POST || method == axum::http::Method::PUT;
                let is_catalog = path.contains("/config/offerings")
                    || path.contains("/config/providers")
                    || path.contains("/config/endpoints")
                    || path.contains("/config/model-attributes");
                let should_reconcile = is_write && is_catalog;
                let resp = next.run(req).await;
                if should_reconcile && resp.status().is_success() {
                    // Ignore the Result — reconcile is best-effort.
                    use awaken_runtime_host::AssistantBindingReconciler;
                    let _ = reconciler.reconcile().await;
                }
                resp
            }
        },
    );
    let flat = flat.layer(reconcile_on_catalog_write);
    let flat = awaken_server::workspace_path::with_platform_workspace(flat, platform_workspace);
    awaken_server::workspace_path::with_workspace_path_addressing(flat)
}

/// Resolve the hidden local Org from one composition-root seam. Self-managed
/// deployments may explicitly configure it; single-machine mode never asks the
/// user and consistently uses the default Org.
fn local_org_id() -> String {
    std::env::var("AWAKEN_ORG_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| awaken_control::DEFAULT_ORG_ID.to_owned())
}

// The seal-key resolution tests moved to `awaken_credential_vault::sealed`, the
// single home of `resolve_seal_key_hex` / `parse_seal_key`.
