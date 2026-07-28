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

mod acp_local_credentials;
mod assistant_selection;
mod brain_admin;
pub mod config;
mod management_surface;
mod observation_reconcile;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use awaken_protocol_managed::{EnvironmentState, ManagedState, VaultState};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{
    ConfigPlane, ConfigService, ExtMcpProbe, ManagedHost, RESERVED_ADMIN_SCOPE, ScopedToolCatalog,
    SharedHost, ToolCatalogSource,
};
use axum::Router;

pub use crate::brain_admin::{
    DrainController, brain_admin_router, register_active_streams_gauge, with_brain_admin,
    with_connection_metric,
};
pub use acp_local_credentials::{
    AcpLocalCredentialResolver, PreparedLocalAcp, local_acp_diagnostics, prepare_local_acp,
};
// Embedded management-plane IAM (ADR-0042/0043 P1) + the mint spec and bootstrap
// constants a test / operator embedding drives — re-exported from the authoring plane.
pub use awaken_control::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz,
    ManagementIdentityMode, RemoteManagementAuthz, TokenSpec, embedded_iam,
};
mod live_runtime_capabilities;
use live_runtime_capabilities::LiveRuntimeCapabilities;

/// Project the one executable ACP catalog into the management read model. This
/// composition edge is intentionally the only place that knows both contexts;
/// neither Control nor the executor keeps a synchronized adapter list.
fn runtime_capabilities(
    observations: &[awaken_acp_application::AcpHostObservation],
) -> Vec<awaken_control::RuntimeCapability> {
    std::iter::once(awaken_control::RuntimeCapability::native())
        .chain(awaken_run_executor_acp::known_acp_clis().iter().map(|cli| {
            let capability =
                awaken_control::RuntimeCapability::acp(cli.id, cli.display_name, cli.description);
            let Some(observation) = observations.iter().find(|row| row.cli_id == cli.id) else {
                return capability;
            };
            capability.with_local(awaken_control::LocalRuntimeCapability {
                detected: observation.detected(),
                version: observation.version.clone(),
                login_state: observation.credential_state.map(credential_state_name),
                reason_code: observation.reason_code.clone(),
                remediation: cli
                    .remediation(observation.reason_code.as_deref())
                    .map(str::to_string),
                negotiated: None,
            })
        }))
        .collect()
}

fn credential_state_name(state: awaken_runtime_contract::CredentialObservationState) -> String {
    serde_json::to_value(state)
        .expect("credential observation state serializes")
        .as_str()
        .expect("credential observation state serializes as a string")
        .to_string()
}

#[cfg(test)]
mod runtime_capability_tests {
    use super::*;

    #[test]
    fn management_runtime_projection_is_exactly_the_executable_catalog() {
        // Cause graph:
        // C1 native runtime is intrinsic -> E1 exactly one `awaken` row.
        // C2 an AcpCli catalog row exists -> E2 exactly one matching `acp:<id>` row.
        // C3 no AcpCli row exists -> E3 no management capability can advertise it.
        //
        // Decision table:
        // | Rule | Native | catalog row | capability |
        // | R1 | T | - | awaken exactly once |
        // | R2 | - | T | matching acp:<id> exactly once |
        // | R3 | - | F | absent |
        let projected = runtime_capabilities(&[]);
        assert_eq!(
            projected.iter().filter(|row| row.id == "awaken").count(),
            1,
            "R1"
        );

        let catalog = awaken_run_executor_acp::known_acp_clis();
        let projected_acp: Vec<_> = projected.iter().filter(|row| row.kind == "acp").collect();
        assert_eq!(projected_acp.len(), catalog.len(), "R2/R3 cardinality");
        for cli in catalog {
            let row = projected_acp
                .iter()
                .find(|row| row.cli.as_deref() == Some(cli.id))
                .unwrap_or_else(|| panic!("R2 missing catalog projection for {}", cli.id));
            assert_eq!(row.id, format!("acp:{}", cli.id), "R2");
            assert_eq!(row.label, cli.display_name, "R2 metadata");
            assert_eq!(row.description, cli.description, "R2 metadata");
        }
        assert!(
            projected.iter().all(|row| row.id != "acp:kimi"),
            "R3 unsupported Kimi is not advertised"
        );
        assert!(
            projected.iter().all(|row| row.id != "acp:hermes"),
            "R3 unsupported Hermes is not advertised"
        );
    }

    #[test]
    fn management_runtime_projection_joins_one_secret_free_local_observation() {
        // Cause graph: catalog row + same-id startup observation -> enriched
        // read model. Rows without an observation stay supported with unknown
        // local status; no joined inventory is persisted.
        //
        // Decision table:
        // L1 same id + detected/login-required -> detected status + remediation
        // L2 no observation                    -> local status absent
        let projected = runtime_capabilities(&[awaken_acp_application::AcpHostObservation {
            cli_id: "codex".into(),
            display_name: "Codex".into(),
            detection: awaken_acp_application::AcpDetectionState::Detected,
            version: Some("codex 1".into()),
            credential_state: Some(
                awaken_runtime_contract::CredentialObservationState::LoginRequired,
            ),
            reason_code: Some("acp_login_required".into()),
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
        }]);
        let codex = projected.iter().find(|row| row.id == "acp:codex").unwrap();
        let local = codex.local.as_ref().expect("L1");
        assert!(local.detected, "L1");
        assert_eq!(local.login_state.as_deref(), Some("login_required"), "L1");
        assert!(
            local
                .remediation
                .as_deref()
                .unwrap()
                .contains("codex login")
        );
        assert!(
            projected
                .iter()
                .find(|row| row.id == "acp:gemini")
                .unwrap()
                .local
                .is_none(),
            "L2"
        );
    }
}

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

/// Provisioning-side model directory adapter. It receives an exact authored
/// credential row, materializes it at this seam, calls the configured provider
/// endpoint, and returns only normalized ids. It never selects a credential and
/// never mutates the catalog itself.
struct GenaiModelDiscovery {
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
}

#[async_trait::async_trait]
impl awaken_admin_config_api::ModelCatalogDiscovery for GenaiModelDiscovery {
    async fn discover(
        &self,
        endpoint: &awaken_model_catalog::ProtocolEndpoint,
        credential: &awaken_credential_vault::CredentialSource,
    ) -> Result<
        Vec<awaken_model_catalog::DiscoveredModel>,
        awaken_admin_config_api::ModelCatalogDiscoveryError,
    > {
        use awaken_model_catalog::ApiDialect;
        use awaken_provider_genai::AdapterKind;

        let adapter = match endpoint.dialect {
            ApiDialect::AnthropicMessages => AdapterKind::Anthropic,
            ApiDialect::OpenAiChat | ApiDialect::OpenAiResponses => AdapterKind::OpenAI,
            ApiDialect::Gemini => AdapterKind::Gemini,
            ApiDialect::VertexGemini => AdapterKind::Vertex,
        };
        let secret = awaken_credential_vault::materialize(credential, self.secrets.as_ref())
            .await
            .map_err(|error| {
                awaken_admin_config_api::ModelCatalogDiscoveryError::CredentialUnavailable(
                    error.to_string(),
                )
            })?;
        awaken_provider_genai::discover_model_ids(
            adapter,
            endpoint.base_url.as_deref(),
            secret.expose_secret(),
        )
        .await
        .map(|ids| {
            ids.into_iter()
                .map(|model_id| awaken_model_catalog::DiscoveredModel {
                    model_id,
                    upstream_model: None,
                })
                .collect()
        })
        .map_err(|error| {
            awaken_admin_config_api::ModelCatalogDiscoveryError::Provider(error.to_string())
        })
    }

    async fn discover_with_secret(
        &self,
        endpoint: &awaken_model_catalog::ProtocolEndpoint,
        secret: &awaken_agent_contract::RedactedString,
    ) -> Result<
        Vec<awaken_model_catalog::DiscoveredModel>,
        awaken_admin_config_api::ModelCatalogDiscoveryError,
    > {
        use awaken_model_catalog::ApiDialect;
        use awaken_provider_genai::AdapterKind;

        let adapter = match endpoint.dialect {
            ApiDialect::AnthropicMessages => AdapterKind::Anthropic,
            ApiDialect::OpenAiChat | ApiDialect::OpenAiResponses => AdapterKind::OpenAI,
            ApiDialect::Gemini => AdapterKind::Gemini,
            ApiDialect::VertexGemini => AdapterKind::Vertex,
        };
        awaken_provider_genai::discover_model_ids(
            adapter,
            endpoint.base_url.as_deref(),
            secret.expose_secret(),
        )
        .await
        .map(|ids| {
            ids.into_iter()
                .map(|model_id| awaken_model_catalog::DiscoveredModel {
                    model_id,
                    upstream_model: None,
                })
                .collect()
        })
        .map_err(|error| {
            awaken_admin_config_api::ModelCatalogDiscoveryError::Provider(error.to_string())
        })
    }
}

/// The two legal composition modes are deliberately disjoint: production
/// publishes catalog-backed provider candidates and installs their credential
/// materializer; deterministic scenarios publish one exact host executor and do
/// not install a provider materializer.
enum ManagementModelComposition {
    PublishedProviders,
    Host {
        executor: Arc<dyn LlmExecutor>,
        binding: awaken_runtime_contract::resolved::ModelBinding,
    },
}

/// Concrete wiring produced by one legal composition mode. Keeping these four
/// values together prevents a provider resolver from being paired with a host
/// executor or a Host publication from receiving a credential materializer.
struct ManagementModelWiring {
    executor: Arc<dyn LlmExecutor>,
    model_ref: String,
    publication_resolver: Arc<dyn awaken_runtime_host::ModelPublicationResolver>,
    materializer: Option<Arc<dyn awaken_runtime_host::InferenceExecutorMaterializer>>,
}

#[derive(Default)]
struct AssemblyOverrides {
    deployment: Option<awaken_runtime_host::DeploymentConfig>,
    org_id: Option<String>,
    mcp_bearer_token: Option<String>,
    management_only: bool,
    cloud_api_base_url: Option<String>,
    cloud_models_enabled: bool,
    local_acp_observations: Vec<awaken_acp_application::AcpHostObservation>,
    hand_executors: BTreeMap<String, Arc<dyn awaken_runtime_contract::tool::ToolExecutor>>,
}

struct ConfigDeclaredHandSource(Arc<ConfigService>);

impl awaken_server::placement::DeclaredHandSource for ConfigDeclaredHandSource {
    fn declared_hand(&self, agent_id: &str) -> Result<Option<String>, String> {
        self.0.declared_hand_for_agent(agent_id)
    }
}

type IdentityWiring = (
    Option<Arc<ManagementAuthz>>,
    Option<Arc<RemoteManagementAuthz>>,
);

struct ExactHostModelPublicationResolver {
    binding: awaken_runtime_contract::resolved::ModelBinding,
}

#[async_trait::async_trait]
impl awaken_runtime_host::ModelPublicationResolver for ExactHostModelPublicationResolver {
    async fn resolve_models(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        selection: &awaken_config_store::ModelSelection,
        candidates: &[awaken_runtime_contract::resolved::ModelBinding],
    ) -> Result<
        awaken_runtime_host::ResolvedPublicationModels,
        awaken_runtime_host::PublicationResolutionError,
    > {
        if let Some(authored) = selection.resolved() {
            let matches_host = authored.model_ref == self.binding.model_ref
                && (authored.provider_identity_ref.is_empty()
                    || authored.provider_identity_ref == self.binding.provider_identity_ref)
                && (authored.backend_ref.is_empty()
                    || authored.backend_ref == self.binding.backend_ref);
            if !matches_host {
                return Err(format!(
                    "scenario host executor `{}` cannot publish model `{}`",
                    self.binding.model_ref, authored.model_ref
                )
                .into());
            }
        }
        if !candidates.is_empty() {
            return Err("a single host executor cannot publish fallback candidates".into());
        }
        Ok(awaken_runtime_host::ResolvedPublicationModels::host(
            self.binding.clone(),
            Vec::new(),
            None,
            None,
        ))
    }
}

/// The store set the management plane runs over — one instance of each port,
/// shared by the authoring router, the vault front door, and session prepare.
struct ManagementStores {
    /// Durable installation root used to persist the platform Workspace id.
    workspace_root: Option<std::path::PathBuf>,
    resource_plane: ResourcePlaneStores,
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>,
    resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>,
    resource_catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
    /// Authored webhook endpoints (ADR-0048), an id-addressed config resource beside
    /// profiles/MCP — the same admin store, a distinct port.
    webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>,
    /// Durable home for the Managed session aggregate (its own `sessions.db`), so a
    /// rehydrated session reports its real config across a restart / peer process.
    sessions: Arc<dyn awaken_protocol_managed::ManagedSessionRepository>,
    /// Same Session application repository viewed through the extraction-work
    /// port; kept separate from MemoryRepository and from IAM.
    memory_extractions: Arc<dyn awaken_protocol_managed::MemoryExtractionRepository>,
    /// The config authoring plane (`config.db`): the rich `AgentConfig` drafts the
    /// management console authors directly, and their publications. Scoped so a
    /// workspace's config is fenced from another's (ADR-0051).
    config: Arc<dyn awaken_config_store::ScopedConfigRegistry>,
    /// Self-hosted environments registry + work queue, durable per deployment mode.
    environments: Arc<awaken_protocol_managed::EnvironmentState>,
}

/// Backend-neutral resource ports selected together at the composition root.
/// This is wiring, not an aggregate and not an authorization context.
struct ResourcePlaneStores {
    lifecycle: Arc<dyn awaken_protocol_managed::resource_plane::ResourceLifecycleRepository>,
    files: Arc<dyn awaken_file_store::FileStore>,
    memory: Arc<dyn awaken_memory_store::MemoryRepository>,
    skills: Arc<dyn awaken_skill_store::SkillStore>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostgresSchemaMode {
    Migrate,
    Verify,
}

impl ResourcePlaneStores {
    fn ephemeral() -> Self {
        Self {
            lifecycle: Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("open ephemeral resource lifecycle sqlite"),
            ),
            files: Arc::new(awaken_file_store::InMemoryFileStore::new()),
            memory: Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
            skills: Arc::new(awaken_skill_store::InMemorySkillStore::new()),
        }
    }

    fn embedded(root: &std::path::Path) -> Self {
        let (files, memory, skills, lifecycle) =
            awaken_server::embedded_resource_plane(root).into_parts();
        Self {
            lifecycle,
            files,
            memory,
            skills,
        }
    }

    async fn open(
        backend: config::ResourcePlaneStoreBackend,
        postgres_schema: PostgresSchemaMode,
    ) -> Result<Self, String> {
        match backend {
            config::ResourcePlaneStoreBackend::Embedded(root) => Ok(Self::embedded(&root)),
            config::ResourcePlaneStoreBackend::Postgres(url) => {
                let lifecycle = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_resource_store::PostgresResourceStore::connect(&url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_resource_store::PostgresResourceStore::connect_existing(&url).await
                    }
                }
                .map_err(|error| format!("connect resource lifecycle Postgres: {error}"))?;
                let files = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_file_store::postgres::PgFileStore::connect(&url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_file_store::postgres::PgFileStore::connect_existing(&url).await
                    }
                }
                .map_err(|error| format!("connect resource file Postgres: {error}"))?;
                let memory = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_memory_store::PostgresMemoryRepository::connect(&url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_memory_store::PostgresMemoryRepository::connect_existing(&url).await
                    }
                }
                .map_err(|error| format!("connect resource memory Postgres: {error}"))?;
                let skills = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_skill_store::PgSkillStore::connect(&url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_skill_store::PgSkillStore::connect_existing(&url).await
                    }
                }
                .map_err(|error| format!("connect resource skill Postgres: {error}"))?;
                Ok(Self {
                    lifecycle: Arc::new(lifecycle),
                    files: Arc::new(files),
                    memory: Arc::new(memory),
                    skills: Arc::new(skills),
                })
            }
        }
    }
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
    let sessions = Arc::new(
        awaken_protocol_managed::SqliteManagedSessionRepository::open_in_memory()
            .expect("open ephemeral managed Session repository"),
    );
    let admin = Arc::new(
        awaken_admin_config_api::SqliteAdminStore::open_in_memory()
            .expect("open ephemeral admin store"),
    );
    ManagementStores {
        workspace_root: None,
        resource_plane: ResourcePlaneStores::ephemeral(),
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: admin.clone(),
        resources: admin.clone(),
        resource_catalog: admin.clone(),
        webhooks: admin,
        sessions: sessions.clone(),
        memory_extractions: sessions,
        config: Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
        ),
        environments: Arc::new(EnvironmentState::new()),
    }
}

/// Keep the Managed Session aggregate durable whenever the runtime itself is
/// durable, even when the rest of the management plane intentionally remains
/// ephemeral. A restarted runtime can only rehydrate a governed Session when its
/// configuration and owner fence survive beside the committed thread facts.
#[cfg(test)]
fn management_stores_for_runtime_storage(
    storage_dir: Option<&std::path::Path>,
) -> ManagementStores {
    let mut stores = in_memory_management_stores();
    let Some(dir) = storage_dir else {
        return stores;
    };
    stores.workspace_root = Some(dir.to_path_buf());
    stores.sessions = awaken_runtime_host::local_managed_session_repository(Some(dir));
    stores
}

/// Open the control-plane stores per the [`ControlStoreConfig`](awaken_control::ControlStoreConfig)
/// — each component on its own database (SQLite file or shared Postgres). This
/// is the one durable store assembly: each store independently honors its
/// typed per-component database binding, so a
/// separate control / server process can share the same per-component databases
/// (Option A, shared-DB).
async fn open_management_stores(
    cfg: awaken_control::ControlStoreConfig,
    resource_plane: ResourcePlaneStores,
    workspace_root: std::path::PathBuf,
    key: &[u8; 32],
    postgres_schema: PostgresSchemaMode,
) -> Result<ManagementStores, String> {
    use awaken_control::StoreBackend;

    // Create the parent directory for any SQLite path (a bundle dir or a custom path).
    fn ensure_parent(backend: &StoreBackend) -> Result<(), String> {
        if let StoreBackend::Sqlite(path) = backend
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create control-store directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        Ok(())
    }
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();

    ensure_parent(&cfg.catalog)?;
    let catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo> = match &cfg.catalog {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_model_catalog::SqliteCatalogRepo::open(&path(p))
                .map_err(|error| format!("open catalog SQLite {}: {error}", p.display()))?,
        ),
        StoreBackend::Postgres(url) => Arc::new(
            match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_model_catalog::PostgresCatalogRepo::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_model_catalog::PostgresCatalogRepo::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect catalog Postgres: {error}"))?,
        ),
    };

    // The credential repo and its sealed-secret blobs share the one credential backend.
    ensure_parent(&cfg.credential)?;
    let (credentials, secrets): (
        Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        Arc<dyn awaken_credential_vault::SecretStore>,
    ) = match &cfg.credential {
        StoreBackend::Sqlite(p) => {
            let file = path(p);
            let creds = Arc::new(
                awaken_credential_vault::SqliteCredentialRepo::open(&file)
                    .map_err(|error| format!("open credential SQLite {}: {error}", p.display()))?,
            );
            let blobs =
                awaken_credential_vault::SqliteSealedBlobStore::open(&file).map_err(|error| {
                    format!("open sealed credential SQLite {}: {error}", p.display())
                })?;
            (
                creds,
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
        StoreBackend::Postgres(url) => {
            let (creds, blobs) = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_credential_vault::postgres::connect_migrated_pair(url)
                        .await
                        .map_err(|error| format!("connect credential Postgres: {error}"))?
                }
                PostgresSchemaMode::Verify => {
                    awaken_credential_vault::postgres::connect_existing_pair(url)
                        .await
                        .map_err(|error| format!("connect credential Postgres: {error}"))?
                }
            };
            (
                Arc::new(creds),
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
    };

    // The admin aggregate backs three ports (profiles / MCP / webhooks) off one store.
    ensure_parent(&cfg.admin)?;
    let admin_profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>;
    let admin_webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>;
    let admin_resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>;
    let admin_catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>;
    match &cfg.admin {
        StoreBackend::Sqlite(p) => {
            let admin = awaken_admin_config_api::SqliteAdminStore::open(&path(p))
                .map_err(|error| format!("open admin SQLite {}: {error}", p.display()))?;
            admin
                .migrate_legacy_memory_stores()
                .map_err(|error| format!("migrate legacy MemoryStore rows: {error}"))?;
            let admin = Arc::new(admin);
            admin_profiles = admin.clone();
            admin_resources = admin.clone();
            admin_catalog = admin.clone();
            admin_webhooks = admin;
        }
        StoreBackend::Postgres(url) => {
            // `PostgresAdminStore::connect` builds its own runtime and blocks, so it
            // must run off the async worker thread to avoid a nested-runtime panic.
            let url = url.clone();
            let admin = Arc::new(
                tokio::task::spawn_blocking(move || match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_admin_config_api::PostgresAdminStore::connect(&url)
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_admin_config_api::PostgresAdminStore::connect_existing(&url)
                    }
                })
                .await
                .map_err(|error| format!("join admin Postgres connection: {error}"))?
                .map_err(|error| format!("connect admin Postgres: {error}"))?,
            );
            if postgres_schema == PostgresSchemaMode::Migrate {
                admin
                    .migrate_legacy_memory_stores()
                    .map_err(|error| format!("migrate legacy MemoryStore rows: {error}"))?;
            }
            admin_profiles = admin.clone();
            admin_resources = admin.clone();
            admin_catalog = admin.clone();
            admin_webhooks = admin;
        }
    }

    ensure_parent(&cfg.sessions)?;
    let (sessions, memory_extractions): (
        Arc<dyn awaken_protocol_managed::ManagedSessionRepository>,
        Arc<dyn awaken_protocol_managed::MemoryExtractionRepository>,
    ) = match &cfg.sessions {
        StoreBackend::Sqlite(p) => {
            let repository = Arc::new(
                awaken_runtime_host::SqliteManagedSessionRepository::open(&path(p))
                    .map_err(|error| format!("open sessions SQLite {}: {error}", p.display()))?,
            );
            (repository.clone(), repository)
        }
        StoreBackend::Postgres(url) => {
            let repository = Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_runtime_host::PostgresManagedSessionRepository::connect(url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_runtime_host::PostgresManagedSessionRepository::connect_existing(url)
                            .await
                    }
                }
                .map_err(|error| format!("connect sessions Postgres: {error}"))?,
            );
            (repository.clone(), repository)
        }
    };

    ensure_parent(&cfg.config)?;
    let config: Arc<dyn awaken_config_store::ScopedConfigRegistry> = match &cfg.config {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_config_store::SqliteConfigStore::open(&path(p))
                .map_err(|error| format!("open config SQLite {}: {error}", p.display()))?,
        ),
        StoreBackend::Postgres(url) => Arc::new(
            match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_config_store::PostgresConfigStore::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_config_store::PostgresConfigStore::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect config Postgres: {error}"))?,
        ),
    };

    // Self-hosted env registry + work queue follow the session store's backend kind
    // (their own table namespaces, so a shared DB is fine).
    let environments: Arc<EnvironmentState> = match &cfg.sessions {
        StoreBackend::Sqlite(sp) => Arc::new(
            EnvironmentState::with_stores(
                Arc::new(
                    awaken_runtime_host::SqliteEnvRegistry::open(&path(
                        &sp.with_file_name("environments.db"),
                    ))
                    .map_err(|error| format!("open environments SQLite: {error}"))?,
                ),
                Arc::new(
                    awaken_runtime_host::SqliteWorkQueue::open(&path(
                        &sp.with_file_name("work_queue.db"),
                    ))
                    .map_err(|error| format!("open work queue SQLite: {error}"))?,
                ),
            )
            .with_sandbox_policies(Arc::new(
                awaken_sandbox_policy_store::SqliteSandboxExecutionPolicyStore::open(&path(
                    &sp.with_file_name("sandbox_policies.db"),
                ))
                .map_err(|error| format!("open sandbox policies SQLite: {error}"))?,
            )),
        ),
        StoreBackend::Postgres(url) => {
            let environments = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_runtime_host::PostgresEnvRegistry::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_runtime_host::PostgresEnvRegistry::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect environments Postgres: {error}"))?;
            let work = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_runtime_host::PostgresWorkQueue::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_runtime_host::PostgresWorkQueue::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect work queue Postgres: {error}"))?;
            let sandbox_policies = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::connect(url)
                        .await
                }
                PostgresSchemaMode::Verify => {
                    awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::connect_existing(url)
                        .await
                }
            }
            .map_err(|error| format!("connect sandbox policies Postgres: {error}"))?;
            Arc::new(
                EnvironmentState::with_stores(Arc::new(environments), Arc::new(work))
                    .with_sandbox_policies(Arc::new(sandbox_policies)),
            )
        }
    };

    Ok(ManagementStores {
        workspace_root: Some(workspace_root),
        resource_plane,
        catalog,
        credentials,
        secrets,
        profiles: admin_profiles,
        resources: admin_resources,
        resource_catalog: admin_catalog,
        webhooks: admin_webhooks,
        sessions,
        memory_extractions,
        config,
        environments,
    })
}

/// Local SQLite is a backend selection, not a second store assembly.
async fn open_local_management_stores(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> Result<ManagementStores, String> {
    let resource_plane = ResourcePlaneStores::open(
        config::ResourcePlaneStoreBackend::Embedded(dir.to_path_buf()),
        PostgresSchemaMode::Migrate,
    )
    .await?;
    open_management_stores(
        awaken_control::ControlStoreConfig::local(dir),
        resource_plane,
        dir.to_path_buf(),
        key,
        PostgresSchemaMode::Migrate,
    )
    .await
}

/// Serve the management plane from the standard typed deployment configuration.
pub async fn build_management_router() -> Router {
    build_management_router_with_composition(ManagementModelComposition::PublishedProviders).await
}

/// Hermetic management composition for tests and embedders that explicitly want
/// volatile stores. It never consults the standard deployment config path.
pub async fn build_ephemeral_management_router() -> Router {
    management_router_over(
        in_memory_management_stores(),
        None,
        None,
        ManagementModelComposition::PublishedProviders,
        AssemblyOverrides::default(),
        None,
    )
    .await
}

/// Canonical product assembly from the command's one resolved configuration.
pub async fn build_management_router_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    let (iam, remote_iam) = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let postgres_schema = match deployment.mode {
        config::OperatingMode::Local => PostgresSchemaMode::Migrate,
        config::OperatingMode::Server => PostgresSchemaMode::Verify,
    };
    let resource_plane =
        ResourcePlaneStores::open(deployment.resources.clone(), postgres_schema).await?;
    let stores = open_management_stores(
        deployment.control.clone(),
        resource_plane,
        deployment.data_dir.clone(),
        key,
        postgres_schema,
    )
    .await?;
    let hand_executors =
        awaken_server::placement::connect_declared_hands(&deployment.hand_connections).await?;
    Ok(management_router_over(
        stores,
        iam,
        remote_iam,
        ManagementModelComposition::PublishedProviders,
        AssemblyOverrides {
            deployment: Some(deployment.runtime.clone()),
            org_id: Some(deployment.org_id.clone()),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            management_only: false,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: deployment.local_acp_observations.clone(),
            hand_executors,
        },
        None,
    )
    .await)
}

/// Canonical hosted authoring/control assembly. It reuses the same stores,
/// resolver, IAM PEP and routes as the full local product, but deliberately
/// omits every Session/Run/protocol/Worker data-plane route.
pub async fn build_control_router_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    let (iam, remote_iam) = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let stores = open_management_stores(
        deployment.control.clone(),
        // Hosted Management exposes no File/Memory/Skill routes. These ports
        // satisfy shared control-plane collaborators without acquiring a second
        // durable resource-plane authority.
        ResourcePlaneStores::ephemeral(),
        deployment.data_dir.clone(),
        key,
        PostgresSchemaMode::Verify,
    )
    .await?;
    Ok(management_router_over(
        stores,
        iam,
        remote_iam,
        ManagementModelComposition::PublishedProviders,
        AssemblyOverrides {
            deployment: None,
            org_id: Some(deployment.org_id.clone()),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            management_only: true,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: Vec::new(),
            hand_executors: BTreeMap::new(),
        },
        None,
    )
    .await)
}

/// Explicit deployment migration phase for every management-owned store.
/// Local SQLite startup retains its existing auto-migration behavior; managed
/// PostgreSQL deployments invoke this command before starting application Pods.
pub async fn migrate_management_schema_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<(), String> {
    let resource_plane =
        ResourcePlaneStores::open(deployment.resources.clone(), PostgresSchemaMode::Migrate)
            .await?;
    open_management_stores(
        deployment.control.clone(),
        resource_plane,
        deployment.data_dir.clone(),
        key,
        PostgresSchemaMode::Migrate,
    )
    .await
    .map(drop)
}

/// Build the real env-selected management surface with one explicit in-process
/// scenario executor. This is a dev/e2e composition, not a provider fallback:
/// its exact host candidate is published by a dedicated resolver and no
/// credential/provider materializer is installed.
pub async fn build_management_router_with_scenario_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: String,
) -> Router {
    build_management_router_with_composition(ManagementModelComposition::Host {
        executor: model,
        binding: awaken_runtime_contract::resolved::ModelBinding::new(
            "default", model_ref, "default",
        ),
    })
    .await
}

async fn build_management_router_with_composition(
    model_composition: ManagementModelComposition,
) -> Router {
    let deployment = config::ResolvedDeployment::load(config::ConfigOverrides::default())
        .unwrap_or_else(|error| panic!("deployment configuration: {error}"));
    let key = deployment
        .seal_key
        .load_or_create()
        .unwrap_or_else(|error| panic!("control seal key: {error}"));
    let (iam, remote_iam) = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )
    .unwrap_or_else(|error| panic!("identity configuration: {error}"));
    let postgres_schema = match deployment.mode {
        config::OperatingMode::Local => PostgresSchemaMode::Migrate,
        config::OperatingMode::Server => PostgresSchemaMode::Verify,
    };
    let resource_plane = ResourcePlaneStores::open(deployment.resources.clone(), postgres_schema)
        .await
        .unwrap_or_else(|error| panic!("open resource stores: {error}"));
    let stores = open_management_stores(
        deployment.control.clone(),
        resource_plane,
        deployment.data_dir.clone(),
        &key,
        postgres_schema,
    )
    .await
    .unwrap_or_else(|error| panic!("open management stores: {error}"));
    let hand_executors =
        awaken_server::placement::connect_declared_hands(&deployment.hand_connections)
            .await
            .unwrap_or_else(|error| panic!("declared Hand topology: {error}"));
    management_router_over(
        stores,
        iam,
        remote_iam,
        model_composition,
        AssemblyOverrides {
            deployment: Some(deployment.runtime),
            org_id: Some(deployment.org_id),
            mcp_bearer_token: deployment.mcp_bearer_token,
            management_only: false,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: deployment.local_acp_observations,
            hand_executors,
        },
        None,
    )
    .await
}

fn identity_wiring(
    identity_mode: ManagementIdentityMode,
    data_dir: Option<&std::path::Path>,
    org_id: &str,
    iam_workspaces: &[String],
    cloud_iam: &config::CloudIamConfig,
) -> Result<IdentityWiring, String> {
    match identity_mode {
        ManagementIdentityMode::SelfManaged => {
            let dir = data_dir.ok_or_else(|| {
                "self-managed IAM requires a persistent data directory".to_owned()
            })?;
            let workspace = SharedHost::provision_local_workspace_at(dir);
            let iam = awaken_control::embedded_iam_for_tenant(dir, org_id, &workspace);
            for workspace_id in iam_workspaces {
                iam.register_workspace(workspace_id);
            }
            Ok((Some(iam), None))
        }
        ManagementIdentityMode::AwakenCloud => Ok((None, Some(awaken_cloud_authz(cloud_iam)?))),
        ManagementIdentityMode::NoLogin => Ok((None, None)),
    }
}

fn awaken_cloud_authz(
    config: &config::CloudIamConfig,
) -> Result<Arc<RemoteManagementAuthz>, String> {
    if let Some(path) = &config.service_token_file {
        return RemoteManagementAuthz::connect_with_projected_service_token(
            config.base_url.clone(),
            config.audience.clone(),
            config.issuer.clone(),
            path.clone(),
        );
    }
    let user_token = config
        .access_token
        .clone()
        .or_else(|| {
            awaken_iam_client::CredentialCache::open()
                .load(&config.base_url)
                .map(|entry| entry.token.expose().to_owned())
        })
        .ok_or_else(|| "Awaken Cloud login credential is missing or expired".to_string())?;
    RemoteManagementAuthz::connect(
        config.base_url.clone(),
        config.audience.clone(),
        config.issuer.clone(),
        user_token,
        config.service_token.clone(),
    )
}

/// [`build_management_router_with_model`] plus a last-mile hook on the assembled host
/// (`customize_host`) — the seam a composition root uses to wire a runtime backend the
/// management plane does not assemble itself, e.g. `host.with_acp(executor)` so `acp:*`
/// threads run on an external CLI while the full managed plane (vault + MCP staging +
/// config plane) is still in play. Keeps the ACP executor's crate out of this module.
pub async fn build_management_router_with_host_customizer(
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    management_router_over(
        in_memory_management_stores(),
        None,
        None,
        ManagementModelComposition::Host {
            executor: model,
            binding,
        },
        AssemblyOverrides::default(),
        Some(Box::new(customize_host)),
    )
    .await
}

/// Durable counterpart of [`build_management_router_with_host_customizer`].
///
/// This is an explicit-input composition seam for restart tests and embeddings
/// that need a real external runtime while retaining the same management and
/// resource-plane state across host lifetimes. The sealing key and storage root
/// are supplied by the caller, avoiding process-global environment races.
pub async fn build_durable_management_router_with_host_customizer(
    dir: &std::path::Path,
    key: &[u8; 32],
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    management_router_over(
        open_local_management_stores(dir, key)
            .await
            .unwrap_or_else(|error| panic!("open local management stores: {error}")),
        None,
        None,
        ManagementModelComposition::Host {
            executor: model,
            binding,
        },
        AssemblyOverrides::default(),
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
        ManagementModelComposition::Host {
            executor: model,
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", model_ref, "default",
            ),
        },
        AssemblyOverrides::default(),
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
        open_local_management_stores(dir, key)
            .await
            .unwrap_or_else(|error| panic!("open local management stores: {error}")),
        None,
        None,
        ManagementModelComposition::PublishedProviders,
        AssemblyOverrides::default(),
        None,
    )
    .await
}

/// [`build_durable_management_router`] with the embedded IAM guard enabled — the
/// typed self-managed identity composition. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint further
/// workspace tokens against the same policy state.
pub async fn build_secured_management_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let router = management_router_over(
        open_local_management_stores(dir, key)
            .await
            .unwrap_or_else(|error| panic!("open local management stores: {error}")),
        Some(iam.clone()),
        None,
        ManagementModelComposition::PublishedProviders,
        AssemblyOverrides::default(),
        None,
    )
    .await;
    (router, iam)
}

/// Mount the management plane over an explicit store set, optionally gated by the
/// embedded IAM guard (`iam`). The authoring / authz half comes from
/// [`awaken_control::control_router`] (guard wraps ONLY admin + vault); the data
/// plane comes from [`awaken_server::mount_with_managed`]; this composition root
/// weaves them and keeps the warm-load + inert no-model placeholder wired here.
async fn management_router_over(
    stores: ManagementStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    model_composition: ManagementModelComposition,
    assembly: AssemblyOverrides,
    // An optional last-mile hook on the assembled data-plane host, applied before it is
    // shared. The composition root uses it to wire a runtime backend the management plane
    // does not assemble itself (e.g. an ACP executor for `acp:*` threads) without this
    // module naming that backend's crate. `None` in production; `Some` in a scenario that
    // serves external-CLI sessions.
    customize_host: Option<Box<dyn FnOnce(SharedHost) -> SharedHost + Send>>,
) -> Router {
    let management_only = assembly.management_only;
    let deployment = assembly.deployment;
    let hand_executors = assembly.hand_executors;
    let cloud_api_base_url = assembly.cloud_api_base_url;
    let cloud_models_enabled = assembly.cloud_models_enabled;
    let org_id = assembly.org_id.unwrap_or_else(local_org_id);
    let mcp_bearer_token = assembly.mcp_bearer_token;
    let ManagementStores {
        workspace_root,
        resource_plane,
        catalog,
        credentials,
        secrets,
        profiles,
        resources: resource_store,
        resource_catalog,
        webhooks: webhook_store,
        sessions,
        memory_extractions,
        config,
        environments,
    } = stores;
    let ResourcePlaneStores {
        lifecycle: resource_lifecycle,
        files: file_store,
        memory: memory_store,
        skills: skill_store,
    } = resource_plane;
    // Resolve the installation's Workspace exactly once, then inject the same
    // coordinate into every adapter assembled below. Durable roots persist it;
    // ephemeral roots receive a process-local generated coordinate.
    let platform_workspace = workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let brokered_client = cloud_models_enabled
        .then(|| {
            remote_iam
                .as_ref()
                .and_then(|authz| authz.cloud_user_token())
        })
        .flatten()
        .map(|token| {
            let base_url = cloud_api_base_url
                .clone()
                .expect("Awaken Cloud identity requires a Cloud inference API URL");
            Arc::new(
                awaken_server::brokered_inference::HttpBrokeredInferenceClient::new(
                    base_url,
                    token,
                    platform_workspace.clone(),
                )
                .unwrap_or_else(|error| panic!("Cloud inference configuration: {error}")),
            )
        });
    if let Some(root) = workspace_root.as_deref() {
        let migrated = awaken_server::migrate_legacy_skill_registry(root, skill_store.as_ref())
            .await
            .unwrap_or_else(|error| panic!("legacy Skill migration failed: {error}"));
        if migrated > 0 {
            eprintln!("migrated {migrated} legacy Skill aggregate(s)");
        }
    }
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

    let credential_materializer = awaken_runtime_host::PinnedCredentialMaterializer::new(
        credentials.clone(),
        secrets.clone(),
    );
    let model_wiring = match model_composition {
        ManagementModelComposition::PublishedProviders => ManagementModelWiring {
            executor: Arc::new(awaken_server::no_model::NoModelConfiguredExecutor),
            model_ref: awaken_server::no_model::UNCONFIGURED_MODEL_REF.to_string(),
            publication_resolver: Arc::new(
                awaken_server::model_resolver::CatalogModelPublicationResolver::from_repo(
                    catalog.clone(),
                    credentials.clone(),
                )
                .with_profiles(profiles.clone())
                .with_worker_directory(awaken_server::worker_directory())
                .with_brokered_access(cloud_models_enabled),
            ),
            materializer: Some(Arc::new({
                let materializer = awaken_server::inference_materializer::CredentialInferenceMaterializer::from_pinned(
                    credential_materializer.clone(),
                )
                .with_brokered_mode(cloud_models_enabled);
                match &brokered_client {
                    Some(client) => materializer.with_brokered_client(client.clone()),
                    None => materializer,
                }
            })),
        },
        ManagementModelComposition::Host { executor, binding } => ManagementModelWiring {
            executor,
            model_ref: binding.model_ref.clone(),
            publication_resolver: Arc::new(ExactHostModelPublicationResolver { binding }),
            materializer: None,
        },
    };
    let global = awaken_runtime_host::authorable_tools();
    let tool_catalog: Arc<dyn ToolCatalogSource> = Arc::new(ScopedToolCatalog::new(
        global.clone(),
        RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    // Resolve `Auto` against the LIVE catalog repo (not the frozen seed), so a
    // model an operator adds AFTER startup is visible when we re-publish the
    // reserved-scope assistant. The resolver is mandatory: no config service can
    // be constructed with an implicit model fallback.
    let config_service = Arc::new(
        ConfigService::new(model_wiring.publication_resolver)
            .with_credential_reference_validator(Arc::new(
                awaken_control::CredentialRevisionValidator::new(credentials.clone()),
            ))
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
    let assistant_catalog = catalog.snapshot().await.unwrap_or_default();
    let assistant_credentials = credentials
        .list(&platform_workspace)
        .await
        .unwrap_or_default();
    let assistant_selection = assistant_selection::select(
        &assistant_catalog,
        &assistant_credentials,
        &assembly.local_acp_observations,
    );
    if let Some(assistant_selection) = assistant_selection
        && let Err(err) =
            awaken_control::seed_admin_assistant(&plane, &platform_workspace, assistant_selection)
                .await
    {
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
        platform_workspace.clone(),
        vec![awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID.to_string()],
    ));
    // The LIVE data-plane resource inventory (ADR-0038): memory-store ids from the durable
    // Resource Catalog (the same aggregate Session resolution reads, so this stays
    // consistent) + skill ids from the shared skill store. Unlike before, this is now
    // reachable at wire time because both handles are assembled by the composition root.
    let resource_inventory = Arc::new(awaken_control::HostResourceInventory::new(
        resource_catalog.clone(),
        skill_store.clone(),
        platform_workspace.clone(),
    ));
    // The management tool executables (ADR-0052 D3/D4): the capability reader reads the
    // shared catalog + advertised tools; the validator runs the publish-time compile
    // check on drafts in the tenant scope; Runtime history records every call and
    // mutating tools additionally enter the durable config-change path.
    let capability_reader = Arc::new(awaken_control::CatalogCapabilityReader::new(
        // The LIVE catalog repo — models/providers an operator adds after startup
        // are visible on the next capabilities call (not a frozen seed snapshot).
        catalog.clone(),
        &global,
        // The installable plugins (state_machine / memory / compact) so the assistant
        // knows it CAN author a state machine etc. — not an empty list (it would
        // otherwise refuse, thinking no plugins exist).
        &awaken_runtime_host::authorable_config_sections(),
        // The config plane, to list existing agent ids in the tenant scope.
        plane.clone(),
        platform_workspace.clone(),
        // LIVE data-plane inventory: memory-store ids (durable registry) + skill ids
        // (shared skill store). Both handles are assembled by this composition root
        // before the host, so the assistant enumerates real memory stores + skills.
        Some(resource_inventory),
    ));
    let admin_execs = awaken_admin_assistant::admin_tools(
        capability_reader,
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
        mcp_bearer_token,
    );

    // The authoring / authz plane (admin + vault + webhooks + user profiles +
    // deployments + environments + config plane + capabilities), guard applied over
    // admin + vault only. Returns the webhook sink the data plane feeds.
    let deployment_state = Arc::new(awaken_protocol_managed::DeploymentState::new());
    // Keep the IAM handles for the sibling resource PEP. The authoring router owns
    // its PEP; File/Memory/Skill routes are wrapped independently after the data
    // router is assembled, so neither plane depends on the other's services.
    let resource_iam = iam.clone();
    let resource_remote_iam = remote_iam.clone();
    let application_access = Arc::new(awaken_authz_enforce::ApplicationAccessStore::new());
    let (mgmt, webhook_sink) = awaken_control::control_router(awaken_control::ControlRouterInput {
        platform_workspace: platform_workspace.clone(),
        catalog,
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles,
        webhook_store,
        sessions: sessions.clone(),
        resource_store: resource_store.clone(),
        probe: Arc::new(GenaiProbe),
        model_discovery: Arc::new(GenaiModelDiscovery {
            secrets: secrets.clone(),
        }),
        brokered_catalog: brokered_client
            .clone()
            .map(|client| client as Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>),
        cloud_models_enabled,
        vault_state: vault_state.clone(),
        env_state: env_state.clone(),
        deployment_state: deployment_state.clone(),
        plane,
        global_tools: global,
        runtimes: Arc::new(LiveRuntimeCapabilities {
            initial: assembly.local_acp_observations.clone(),
            workers: awaken_server::worker_directory(),
            credentials: credentials.clone(),
            workspace: platform_workspace.clone(),
        }),
        org_id: Some(org_id),
        iam,
        remote_iam,
        application_access: application_access.clone(),
    });

    if management_only {
        return management_surface::finish(mgmt, mcp_export, reconciler, platform_workspace);
    }

    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. It shares the SAME skill store and
    // Resource Catalog the capability inventory reads, so a skill or memory store the
    // host serves is exactly what the assistant enumerates, and identity survives a
    // restart.
    let resource_ports = awaken_runtime_host::ResourcePlanePorts::new(
        file_store,
        memory_store,
        skill_store,
        resource_lifecycle,
    );
    let mut host_builder = match deployment {
        Some(deployment) => SharedHost::new_with_resource_plane_and_deployment(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_ports,
            deployment,
        ),
        None => SharedHost::new_with_resource_plane(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_ports,
        ),
    };
    host_builder = host_builder
        .with_local_workspace(platform_workspace.clone())
        .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(Some(
            credential_materializer.clone(),
        )))
        .with_config_service(config_service.clone())
        .with_admin_tools(admin_execs);
    host_builder = host_builder.with_tool_executor_provider(Arc::new(
        awaken_server::placement::ConfigToolExecutorProvider::from_declared_hands(
            Arc::new(ConfigDeclaredHandSource(config_service.clone())),
            hand_executors,
        ),
    ));
    if let Some(materializer) = model_wiring.materializer {
        host_builder = host_builder.with_inference_materializer(materializer);
    }
    host_builder.install_memory_extraction_repository(memory_extractions);
    awaken_server::install_platform_memory_data_plane(&host_builder);
    // Production ACP wiring (`acp:*` threads): the environment advertises only the
    // installed CLI/sandbox capability. Provider coordinates and credentials are
    // realized from the same publication-pinned DB facts as native inference.
    let host_builder = host_builder
        .with_acp_from_deployment(
            awaken_server::relay_hand_executor_factory(),
            Some(credential_materializer.clone()),
        )
        .await;
    // Last-mile backend wiring the management plane does not assemble itself, injected
    // by the composition root (a scenario that serves external-CLI sessions).
    let host_builder = match customize_host {
        Some(customize) => customize(host_builder),
        None => host_builder,
    };
    let host = Arc::new(host_builder);
    let resource_reclamation = Arc::new(awaken_runtime_host::HostResourceReclamation::new(
        host.clone(),
        resource_catalog.clone(),
    ));
    let resource_reclaimer = Arc::new(
        awaken_resource_reclaimer::ResourceReclaimer::new(
            format!("awaken-resource-reclaimer:{}", std::process::id()),
            30_000,
            host.resource_lifecycle()
                .expect("resource-plane composition installs lifecycle repository"),
            resource_reclamation.clone(),
        )
        .expect("construct resource reclaimer")
        .with_guard(resource_reclamation),
    );
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    };
    if let Ok(summary) = resource_reclaimer.reconcile(now_ms(), 256).await
        && summary.completed > 0
    {
        eprintln!("reclaimed {} durable resource(s)", summary.completed);
    }
    let recurring_resource_reclaimer = resource_reclaimer.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            if let Err(error) = recurring_resource_reclaimer.reconcile(now, 256).await {
                eprintln!("resource reclamation retry remains pending: {error}");
            }
        }
    });
    let managed_state = Arc::new(
        ManagedState::new_with_mcp(
            ManagedHost::new(host.clone())
                .with_resource_validator(resource_catalog.clone())
                .with_credential_materializer(credential_materializer),
        )
        .with_vaults(vault_state)
        .with_environments(env_state)
        .with_resource_catalog(resource_catalog.clone())
        .with_resource_purge_scheduler(host.clone())
        // Share the SAME config plane `/v1/agents` reads, so a session inheriting a
        // published agent's model sees the authoritative config-plane truth (M2).
        .with_config_source(Arc::new(awaken_runtime_host::ConfigServiceAgentSource(
            config_service.clone(),
        )))
        .with_session_repo(sessions)
        .with_lifecycle_sink(webhook_sink),
    );
    let reconciled_resource_activations = managed_state.reconcile_resource_activations().await;
    if reconciled_resource_activations > 0 {
        eprintln!(
            "reconciled {reconciled_resource_activations} durable Session resource activation(s)"
        );
    }
    let reconciled_mcp_attachments = managed_state.reconcile_mcp_attachments().await;
    if reconciled_mcp_attachments > 0 {
        eprintln!("reconciled {reconciled_mcp_attachments} durable Session MCP projection(s)");
    }
    let _ = managed_state.spawn_realization_lease_supervisor();
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
    let mut data = awaken_server::mount_with_managed_and_application_access(
        host,
        managed_state,
        resource_catalog,
        application_access,
    );
    if let Some(iam) = resource_iam {
        data = data.layer(axum::middleware::from_fn_with_state(
            iam,
            awaken_control::authz::resource_guard,
        ));
    } else if let Some(remote_iam) = resource_remote_iam {
        data = data.layer(axum::middleware::from_fn_with_state(
            remote_iam,
            awaken_control::authz::cloud_resource_guard,
        ));
    }
    let flat = data.merge(mgmt);
    management_surface::finish(flat, mcp_export, reconciler, platform_workspace)
}

/// Resolve the hidden local Org from one composition-root seam. Self-managed
/// deployments may explicitly configure it; single-machine mode never asks the
/// user and consistently uses the default Org.
fn local_org_id() -> String {
    awaken_control::DEFAULT_ORG_ID.to_owned()
}

// The seal-key resolution tests moved to `awaken_credential_vault::sealed`, the
// single home of `resolve_seal_key_hex` / `parse_seal_key`.

#[cfg(test)]
mod runtime_session_store_tests {
    use super::*;
    use awaken_protocol_managed::{
        ApplicationContributionState, ControlSessionCreationInputs, EnvironmentFingerprint,
        EnvironmentRevision, EnvironmentSnapshot, IdempotencyRecord, PersistedSession,
        SessionBaselineState, SessionCreationIntent, SessionNetworkPolicy, stable_fingerprint,
    };
    use awaken_runtime_host::ModelPublicationResolver;

    fn creation_intent() -> SessionCreationIntent {
        SessionCreationIntent {
            control: ControlSessionCreationInputs {
                environment: EnvironmentSnapshot {
                    environment_id: "env_local".into(),
                    revision: EnvironmentRevision(1),
                    config_fingerprint: EnvironmentFingerprint("env-local".into()),
                    sandbox: serde_json::json!({}),
                    packages: Default::default(),
                    network: SessionNetworkPolicy::Unrestricted,
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Workload,
                            "awaken.workload.acp",
                        ),
                        mcp_holder: awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                        resource_holder: awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                    },
                },
                agent_id: "assistant".into(),
                model: "test-model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
            application: ApplicationContributionState::Absent,
        }
    }

    fn session(id: &str) -> PersistedSession {
        PersistedSession {
            session_id: id.to_string(),
            revision: Default::default(),
            baseline: SessionBaselineState::Preparing(creation_intent()),
            title: None,
            metadata: Default::default(),
            agent_tools: Vec::new(),
            environment_binding: None,
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: "idle".to_string(),
            archived_at: None,
        }
    }

    #[tokio::test]
    async fn runtime_storage_reopens_the_session_and_its_owner_fence() {
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        {
            let stores = management_stores_for_runtime_storage(Some(dir.path()));
            let value = session("sesn-restart");
            let payload_hash = stable_fingerprint(&value);
            stores
                .sessions
                .create(
                    "workspace-a",
                    value,
                    IdempotencyRecord {
                        key: "test:runtime-session-restart".into(),
                        payload_hash,
                    },
                    Vec::new(),
                )
                .await
                .unwrap();
        }

        let reopened = management_stores_for_runtime_storage(Some(dir.path()));
        assert_eq!(
            reopened.sessions.owner("sesn-restart").await.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            reopened
                .sessions
                .get("sesn-restart")
                .await
                .expect("session survives restart")
                .baseline,
            SessionBaselineState::Preparing(creation_intent())
        );
    }

    #[tokio::test]
    async fn scenario_host_resolves_model_only_authoring_to_its_exact_binding() {
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::new("default", "scenario", "default");
        let resolver = ExactHostModelPublicationResolver {
            binding: binding.clone(),
        };

        let resolved = resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &awaken_config_store::ModelSelection::Pinned(
                    awaken_runtime_contract::resolved::ModelBinding::new("", "scenario", ""),
                ),
                &[],
            )
            .await
            .expect("model-only SDK/UI selection resolves through the host adapter");

        assert_eq!(resolved.primary.binding, binding);
    }

    #[tokio::test]
    async fn scenario_host_rejects_a_conflicting_provider_qualified_binding() {
        let resolver = ExactHostModelPublicationResolver {
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", "scenario", "default",
            ),
        };

        let error = resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &awaken_config_store::ModelSelection::Pinned(
                    awaken_runtime_contract::resolved::ModelBinding::new(
                        "other", "scenario", "default",
                    ),
                ),
                &[],
            )
            .await
            .expect_err("an exact provider selection cannot drift to the host executor");

        assert!(error.to_string().contains("cannot publish model"));
    }
}

#[cfg(test)]
mod management_only_surface_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    use super::*;

    /// Cause/effect decision table:
    ///
    /// | management-only | route owner | expected |
    /// | --- | --- | --- |
    /// | yes | authoring/control | mounted |
    /// | yes | Session/runtime data plane | absent |
    #[tokio::test]
    async fn management_only_mounts_control_and_omits_runtime_authority() {
        let app = management_router_over(
            in_memory_management_stores(),
            None,
            None,
            ManagementModelComposition::PublishedProviders,
            AssemblyOverrides {
                management_only: true,
                ..Default::default()
            },
            None,
        )
        .await;

        let control = app
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(control.status(), StatusCode::OK);

        let session = app
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::NOT_FOUND);
    }
}
