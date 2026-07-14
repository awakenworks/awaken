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

use std::collections::HashSet;
use std::sync::Arc;

use awaken_protocol_managed::{EnvironmentState, ManagedState, VaultState};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{
    ConfigPlane, ConfigService, ExtMcpProbe, ManagedHost, RESERVED_ADMIN_SCOPE, ScopedToolCatalog,
    SharedHost, ToolCatalogSource, advertised_tools,
};
use axum::Router;

pub use crate::brain_admin::{DrainController, with_brain_admin};
// Embedded management-plane IAM (ADR-0042/0043 P1) + the mint spec and bootstrap
// constants a test / operator embedding drives — re-exported from the authoring plane.
pub use awaken_control::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz, TokenSpec,
    embedded_iam,
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
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>,
    mcp: Arc<dyn awaken_admin_config_api::McpStore>,
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
}

/// Ephemeral management stores: everything in process memory (dev / e2e default).
fn in_memory_management_stores() -> ManagementStores {
    ManagementStores {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        webhooks: Arc::new(awaken_admin_config_api::InMemoryWebhookStore::new()),
        sessions: Arc::new(awaken_protocol_managed::InMemorySessionRepository::default()),
        config: Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
        ),
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
    let admin_webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>;
    match &cfg.admin {
        StoreBackend::Sqlite(p) => {
            let admin = Arc::new(
                awaken_admin_config_api::SqliteAdminStore::open(&path(p))
                    .expect("open admin sqlite"),
            );
            admin_profiles = admin.clone();
            admin_mcp = admin.clone();
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

    ManagementStores {
        catalog,
        credentials,
        secrets,
        profiles: admin_profiles,
        mcp: admin_mcp,
        webhooks: admin_webhooks,
        sessions,
        config,
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

/// Pure resolution of the seal-key hex from its two possible sources, so the
/// precedence + mutual-exclusion rules are testable without touching the process
/// environment or the filesystem. `read_file` is injected (real: `read_to_string`).
///
/// Two sources, exactly one required: `AWAKEN_MGMT_SEAL_KEY` (inline hex) or
/// `AWAKEN_MGMT_SEAL_KEY_FILE` (a path to a file holding the hex). Both set is a
/// hard error (ambiguous); neither set is the same brick-on-restart error as before.
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

/// The AEAD key for the durable management plane, from `AWAKEN_MGMT_SEAL_KEY` (inline)
/// **or** `AWAKEN_MGMT_SEAL_KEY_FILE` (a path to a file holding it). Exactly one must
/// be set. Fails loudly when unset, both-set, unreadable, or malformed.
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
    let iam = match std::env::var("AWAKEN_MGMT_IAM") {
        Ok(mode) if mode == "embedded" => {
            let dir = std::env::var("AWAKEN_MGMT_DIR").unwrap_or_else(|_| {
                panic!(
                    "AWAKEN_MGMT_IAM=embedded requires AWAKEN_MGMT_DIR: the embedded \
                     IAM persists its API tokens and role bindings under \
                     <AWAKEN_MGMT_DIR>/iam.sqlite; an in-memory token directory would \
                     mint a fresh bootstrap admin token on every restart."
                )
            });
            Some(embedded_iam(std::path::Path::new(&dir)))
        }
        Ok(other) => panic!(
            "unsupported AWAKEN_MGMT_IAM value `{other}`: only `embedded` (or unset for \
             the open management plane) is supported"
        ),
        Err(_) => None,
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
                fallback_model,
                fallback_model_ref,
            )
            .await
        }
        Err(_) => {
            management_router_over(
                in_memory_management_stores(),
                iam,
                fallback_model,
                fallback_model_ref,
            )
            .await
        }
    }
}

/// Build the management router over in-memory stores with an explicit host default
/// model injected — a **test-only** seam so an integration test can drive the real
/// management router with a deterministic (mock) model, keeping the mock out of the
/// production assembly.
pub async fn build_management_router_with_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> Router {
    management_router_over(in_memory_management_stores(), None, model, model_ref.into()).await
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
        Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
        awaken_server::no_model::UNCONFIGURED_MODEL_REF.to_string(),
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
        Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
        awaken_server::no_model::UNCONFIGURED_MODEL_REF.to_string(),
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
    // The host default model for the window before an operator publishes one. In
    // production this is the provider-free `NoModelConfiguredExecutor` (guidance,
    // never a mock); a test may inject a deterministic model.
    fallback_model: Arc<dyn LlmExecutor>,
    fallback_model_ref: String,
) -> Router {
    let ManagementStores {
        catalog,
        credentials,
        secrets,
        profiles,
        mcp: mcp_store,
        webhooks: webhook_store,
        sessions,
        config,
    } = stores;
    // Clones for the config-plane executor provider (M2): it resolves a session's
    // model to a real executor from the live catalog + the workspace's credential.
    let exec_catalog = catalog.clone();
    let exec_credentials = credentials.clone();
    let exec_secrets = secrets.clone();
    // ONE resource-binding store shared by the admin router (which authors an agent's
    // resources) and the config service (which reads them into resource prompts +
    // mounts at compile) — so a binding authored through the API reaches the compiled
    // config (ADR-0038).
    let resource_store: Arc<dyn awaken_config_resolver::ResourceStore> =
        Arc::new(awaken_admin_config_api::InMemoryResourceStore::new());
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
    let env_state = std::sync::Arc::new(EnvironmentState::new());

    // The server's default model for the window before a publish; and the seed
    // catalog carrying it so the assistant's `Auto` selection resolves at seed/publish.
    let (model, model_ref) = (fallback_model, fallback_model_ref);
    let global = advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
    let tool_catalog: Arc<dyn ToolCatalogSource> = Arc::new(ScopedToolCatalog::new(
        global.clone(),
        RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    let seed_catalog = awaken_model_catalog::ProviderCatalog {
        offerings: vec![awaken_model_catalog::Offering {
            model_id: model_ref.clone(),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            protocol_endpoint_id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            upstream_model: None,
        }],
        ..Default::default()
    };
    let config_service = Arc::new(
        ConfigService::new()
            .with_model_resolver(Arc::new(
                awaken_server::model_resolver::CatalogModelResolver::new(seed_catalog.clone()),
            ))
            .with_resources(resource_store.clone()),
    );
    // Warm-load the installed catalog from the durable config store BEFORE the plane
    // takes ownership, so a fresh process (a restart, or a server that did not author
    // the publish) repopulates `installed`. A no-op on the in-memory path.
    let warmed = config_service
        .warm_install(
            config.as_ref(),
            &awaken_tenancy::ScopeId::from(awaken_config_store::DEFAULT_SCOPE),
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
    // The management tool executables (ADR-0052 D3/D4): the capability reader reads the
    // shared catalog + advertised tools; the validator runs the publish-time compile
    // check on drafts in the tenant scope; every call is audited.
    let admin_execs = awaken_admin_assistant::admin_tools(
        Arc::new(awaken_control::CatalogCapabilityReader::new(
            &seed_catalog,
            &global,
            &[],
        )),
        Arc::new(awaken_control::ConfigServiceDraftValidator::new(
            plane.clone(),
            awaken_config_store::DEFAULT_SCOPE,
        )),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );

    // The authoring / authz plane (admin + vault + webhooks + user profiles +
    // deployments + environments + config plane + capabilities), guard applied over
    // admin + vault only. Returns the webhook sink the data plane feeds.
    let (mgmt, webhook_sink) = awaken_control::control_router(awaken_control::ControlRouterInput {
        catalog,
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles,
        mcp_store: mcp_store.clone(),
        webhook_store,
        resource_store: resource_store.clone(),
        probe: Arc::new(GenaiProbe),
        vault_state: vault_state.clone(),
        env_state: env_state.clone(),
        plane,
        config_service: config_service.clone(),
        global_tools: global,
        org_id: std::env::var("AWAKEN_ORG_ID").ok(),
        iam,
    });

    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. A durable skill catalog under the
    // management storage dir when set, else a per-process temp dir.
    let skill_dir = std::env::var("AWAKEN_MGMT_DIR")
        .map(|d| std::path::PathBuf::from(d).join("skills"))
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!("awaken-skills-{}", std::process::id()))
        });
    let host = Arc::new(
        SharedHost::new(model, model_ref)
            .with_config_service(config_service.clone())
            .with_admin_tools(admin_execs)
            .with_skill_store(skill_dir)
            // Resolve a session's model to a real executor from the config plane (M2):
            // an unconfigured/unresolvable model falls back to the scenario model above.
            .with_executor_provider(Arc::new(
                awaken_server::config_executor::ConfigExecutorProvider::new(
                    exec_catalog,
                    exec_credentials,
                    exec_secrets,
                    awaken_control::BOOTSTRAP_WORKSPACE,
                ),
            )),
    );
    let managed_state = Arc::new(
        ManagedState::new(
            ManagedHost::new(host.clone())
                .with_mcp(credentials, secrets, mcp_store)
                // Share the SAME binding store the config service uses, so a published
                // agent's bound memory store is actually mounted at session-create.
                .with_resources(resource_store.clone()),
        )
        .with_vaults(vault_state)
        .with_environments(env_state)
        .with_session_repo(sessions)
        .with_lifecycle_sink(webhook_sink),
    );
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope before it re-enters
    // routing. Flat requests fall through unchanged.
    let flat = awaken_server::mount_with_managed(host, managed_state).merge(mgmt);
    awaken_server::workspace_path::with_workspace_path_addressing(flat)
}

#[cfg(test)]
mod seal_key_tests {
    use super::resolve_seal_key_hex;

    // A reader that must not be consulted (the inline / error paths never read a file).
    fn no_read(_: &str) -> std::io::Result<String> {
        panic!("read_file should not be called");
    }

    #[test]
    fn inline_key_is_used_verbatim() {
        let hex = resolve_seal_key_hex(Some("abc".into()), None, no_read).unwrap();
        assert_eq!(hex, "abc");
    }

    #[test]
    fn file_source_reads_the_path() {
        let hex = resolve_seal_key_hex(None, Some("/run/seal.key".into()), |p| {
            assert_eq!(p, "/run/seal.key");
            Ok("deadbeef\n".to_string())
        })
        .unwrap();
        // Returned as-read; the trailing newline is trimmed later by parse_seal_key.
        assert_eq!(hex, "deadbeef\n");
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

    #[test]
    fn empty_values_are_treated_as_unset() {
        let err = resolve_seal_key_hex(Some("  ".into()), Some("".into()), no_read).unwrap_err();
        assert!(err.contains("neither AWAKEN_MGMT_SEAL_KEY"), "{err}");
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
