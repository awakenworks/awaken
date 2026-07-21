//! `awaken-server` — the single-machine **data plane** (Stage B2).
//!
//! It composes one protocol-neutral [`SharedHost`] (from `awaken-runtime-host`,
//! the thread-keyed session substrate) and mounts public protocol adapters over
//! it. Each adapter is a thin port implementation that translates its own wire
//! vocabulary to the host's neutral operations; because every adapter keys by the
//! same thread id and drives the same coordinator, a turn started through one
//! protocol can be resumed or observed through another on the *same thread*.
//!
//! This crate is the DATA PLANE: the session surface + protocol adapters
//! ([`mount`] / [`mount_with_managed`]), the resolved-inference executor seam
//! ([`executor_from_resolved`]), inference access resolution/materialization, the model
//! resolver, the no-model fallback, workspace path addressing, and the Worker
//! role helper (the hand is now the separate `awaken-sandbox` execution-plane
//! binary). Its sibling **authoring / authz plane** lives in
//! `awaken-control`; neither depends on the other, and `awaken-cli` is the
//! composition root that weaves them into one management router. The service layer
//! (the host, the two port adapters, the per-plane resource routers) lives in
//! `awaken-runtime-host`.

pub mod admin;
pub mod dynamic_placement;
pub mod inference_materializer;
pub mod mcp_export;
pub mod model_resolver;
pub mod no_model;
pub mod placement;
pub mod resource_owner;
pub mod webhooks;
mod worker_registry;
pub mod workspace_path;

use std::sync::Arc;

use awaken_config_resolver::ResolvedInference;
use awaken_protocol_managed::{ManagedState, router};
use awaken_protocol_transport::ProtocolRuntime;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::LlmExecutor;
use axum::Router;

// The managed-agents service layer (`awaken-runtime-host`): the neutral host,
// the two port adapters, the per-plane routers, and the authoring/transport
// re-exports a composition root (and the integration tests) drive directly.
pub use awaken_managed_routers::{default_models, files_router, models_router};
pub use awaken_runtime_host::{
    ConfigService, ExtMcpProbe, HostResume, InferenceExecutorMaterializer, ManagedHost,
    PreparedMcpRefresh, ProtocolHost, SharedHost, SkillContext, SkillSpec, ThreadEvent,
    ThreadEventHub, VaultRefresher, advertised_tools, capabilities_router, config_router,
    content_fingerprint, durable_ops_router, memory_stores_router,
    memory_stores_router_with_catalog, parse_skill_md, skills_router,
};
pub use worker_registry::{
    init_postgres as init_postgres_worker_registry, inject as init_worker_registry,
};

/// Assemble the governed MemoryFs data plane with its worker-side mount adapter.
/// Authorization has already selected workspace/store/access before this adapter
/// sees an opaque store id; no IAM vocabulary crosses this seam.
pub fn install_platform_memory_data_plane(host: &SharedHost) {
    host.install_memory_mounter(Arc::new(awaken_sandbox_memoryd::MemoryStoreMounter::new(
        host.memory_fs(),
    )));
}

/// An [`InferenceExecutorMaterializer`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
pub fn mount(host: Arc<SharedHost>) -> Router {
    // The webhook plane (ADR-0048) now lives in the management path
    // (`management_router_over`): subscriptions are a config resource in the admin
    // store and their secret is sealed in the vault, so a webhook needs the config
    // plane. The plain mount has neither, so it wires no sink — a bare host emits no
    // webhooks (identical to an unconfigured plane before).
    let catalog = Arc::new(awaken_config_resolver::InMemoryResourceCatalog::new());
    let state = local_managed_state(host.clone(), catalog.clone());
    mount_with_managed_and_resource_catalog(host, state, catalog)
}

/// Assemble the local/single-process Managed adapter with one shared ephemeral
/// credential plane. Repository tokens are sealed immediately and runtime receives
/// only a credential reference. Production composition roots replace these
/// in-memory adapters with their durable equivalents; resource services remain
/// unaware of principals, API keys, roles, or authorization policy.
pub fn local_managed_state(
    host: Arc<SharedHost>,
    catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
) -> Arc<ManagedState> {
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let mcp_store = Arc::new(awaken_config_resolver::InMemoryMcpStore::new());
    let vaults = Arc::new(awaken_protocol_managed::VaultState::new(
        secrets.clone(),
        credentials.clone(),
    ));
    Arc::new(
        ManagedState::new(
            ManagedHost::new(host)
                .with_resource_configs(catalog.clone())
                .with_mcp(credentials, secrets, mcp_store),
        )
        .with_vaults(vaults)
        .with_resource_catalog(catalog),
    )
}

/// The deployment role this process runs as — the single role axis, selected by
/// `AWAKEN_ROLE` with backward-compatible inference from the historic per-role env
/// (`AWAKEN_HAND_*`, `AWAKEN_UPSTREAM_URL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Serve the HTTP surface: single-machine all-in-one, or a coordinator when
    /// `AWAKEN_DISABLE_LOCAL_POOL=1`. The default.
    Serve,
    /// A database-less worker of a cell server (claims/commits over HTTP).
    Worker,
}

/// Pure role selection: an explicit `AWAKEN_ROLE` wins; otherwise infer from whether
/// the historic worker env is configured. Unit-tested without env. (The hand is now a
/// separate execution-plane binary — `awaken-sandbox hand` — not a server role.)
fn role_from(explicit: Option<&str>, worker_configured: bool) -> Role {
    match explicit {
        Some("worker") => Role::Worker,
        Some("serve") | Some("server") | Some("coordinator") | Some("all-in-one") => Role::Serve,
        _ if worker_configured => Role::Worker,
        _ => Role::Serve,
    }
}

/// The deployment role from the environment.
pub fn deployment_role() -> Role {
    let set = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).is_some();
    let explicit = std::env::var("AWAKEN_ROLE").ok();
    role_from(explicit.as_deref(), set("AWAKEN_UPSTREAM_URL"))
}

#[cfg(test)]
mod role_tests {
    use super::{Role, role_from};

    #[test]
    fn explicit_role_wins() {
        assert_eq!(role_from(Some("worker"), false), Role::Worker);
        assert_eq!(role_from(Some("coordinator"), true), Role::Serve);
        assert_eq!(role_from(Some("all-in-one"), true), Role::Serve);
    }

    #[test]
    fn inference_from_historic_env_when_role_unset() {
        // Upstream → Worker; neither → Serve (the default). The hand is no longer a
        // server role — it is the `awaken-sandbox hand` execution-plane binary.
        assert_eq!(role_from(None, true), Role::Worker);
        assert_eq!(role_from(None, false), Role::Serve);
    }

    #[test]
    fn an_unknown_explicit_role_falls_back_to_inference() {
        assert_eq!(role_from(Some("bogus"), true), Role::Worker);
        assert_eq!(role_from(Some("bogus"), false), Role::Serve);
    }
}

// The database-less **worker** role moved to the production `awaken-worker` crate
// (Stage C): it resolves EACH drained run's model from the DB-configured catalog +
// vault via `CredentialInferenceMaterializer`, with `NoModelConfiguredExecutor` only as the
// fallback. The `awaken` binary's Worker role delegates to `awaken_worker::run`. The
// test-only echo-draining worker (for the worker-pool e2e) lives in
// `awaken-scenario-host::run_echo_worker`.

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
pub fn mount_with_managed(host: Arc<SharedHost>, managed_state: Arc<ManagedState>) -> Router {
    mount_with_managed_over(
        host,
        managed_state,
        Arc::new(awaken_config_resolver::InMemoryResourceCatalog::new()),
    )
}

/// Assemble the data plane with the same secret-free Resource Catalog used by
/// the Managed Session ACL. Authorization remains an outer middleware concern;
/// this only shares resource identity/configuration/lifecycle truth.
pub fn mount_with_managed_and_resource_catalog(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
) -> Router {
    mount_with_managed_over(host, managed_state, resource_catalog)
}

fn mount_with_managed_over(
    host: Arc<SharedHost>,
    managed_state: Arc<ManagedState>,
    resource_catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
) -> Router {
    install_platform_memory_data_plane(&host);
    // Spawn the process-level dispatch pool once when durable ingress is enabled
    // (O2): it is the sole claimer of the shared queue and drives every session's
    // runs. This is the single seam that owns an `Arc<SharedHost>`, which the pool's
    // session resolver needs.
    //
    // A coordinator-only cell server (`AWAKEN_DISABLE_LOCAL_POOL=1`) skips its
    // co-located pool so remote database-less workers are the sole drainers, claiming
    // and settling over the dispatch transport.
    if host.runs_local_dispatch_pool() {
        host.ensure_dispatch_pool();
    }
    let managed = router(managed_state);
    // One neutral port impl behind the three wire adapters (each `router` takes
    // `Arc<dyn ProtocolRuntime>`), so they share the host with no per-protocol twin.
    let port: Arc<dyn ProtocolRuntime> = Arc::new(ProtocolHost::new(host.clone()));
    let ai_sdk = awaken_protocol_ai_sdk::router(port.clone());
    let ag_ui = awaken_protocol_ag_ui::router(port.clone());
    let a2a = awaken_protocol_a2a::router(port.clone());
    // The durable-ingress operations surface (slice E): ADR-0009 follow-on verbs
    // (supersede / reconcile / reap / dead-letter GC) over the same shared host.
    let durable_ops = durable_ops_router(host.clone());
    // The worker-facing cross-node seam: a database-less worker claims/settles runs
    // over the dispatch transport and pushes committed facts to the commit ingest.
    let worker_transport = awaken_runtime_host::registered_worker_transport_router(
        host.clone(),
        worker_registry::shared(),
        dynamic_placement::shared_worker_placement_policy(),
    );
    // The Files API (`/v1/files`) over the host's blob store — file resources + artifacts.
    let files = files_router(host.clone());
    // Tenant ownership for memory stores (ADR-0053 / ADR-0051): fence cross-tenant
    // access to a store (and its memories/versions) by the scope that created it. A
    // single-tenant deployment resolves to the default scope and is never fenced.
    let memory_stores = memory_stores_router_with_catalog(host.clone(), resource_catalog.clone())
        .layer(axum::middleware::from_fn_with_state(
            crate::resource_owner::ResourceOwners::over(resource_catalog),
            crate::resource_owner::memory_store_ownership_guard,
        ));
    // The skills API (`/v1/skills`) over the host's durable delivered-skill catalog.
    let skills = skills_router(host.clone());
    // The Models API (`/v1/models`) over the deployment's model directory.
    let models = models_router(std::sync::Arc::new(default_models()));
    // ADR-0050: install the process-global captured-content sink and expose the
    // erasure + consent routes over the SAME store, so content a run captures is
    // erasable within this one server (the run→capture→store→erase loop). Durable
    // (sqlite under AWAKEN_STORAGE_DIR) so captured content + consent survive a
    // restart; in-memory otherwise.
    let (sink, eraser, ds_repo) = data_subject_plane();
    awaken_runtime_host::install_capture_sink(sink);
    let resolver: Arc<dyn awaken_runtime_contract::DataSubjectResolver> = Arc::new(
        awaken_data_subject::RepoDataSubjectResolver::new(ds_repo.clone()).with_eraser(eraser),
    );
    let erasure = awaken_runtime_host::erasure_router(resolver);
    let consent = awaken_runtime_host::consent_router(ds_repo);
    let local_workspace = host.local_workspace().to_string();
    managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
        .merge(worker_transport)
        .merge(files)
        .merge(memory_stores)
        .merge(skills)
        .merge(models)
        .merge(erasure)
        .merge(consent)
        // A scope-less request is the local/single-tenant mode. Resolve that mode
        // once at the composition edge so sessions and every resource adapter see
        // the same platform-provisioned workspace. Authenticated/cloud edges stamp
        // `WorkspaceScope` before this layer and therefore keep their resolved scope.
        .layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: axum::middleware::Next| {
                let local_workspace = local_workspace.clone();
                async move {
                    if request
                        .extensions()
                        .get::<awaken_protocol_managed::WorkspaceScope>()
                        .is_none()
                    {
                        request
                            .extensions_mut()
                            .insert(awaken_protocol_managed::WorkspaceScope(local_workspace));
                    }
                    next.run(request).await
                }
            },
        ))
}

/// The open data-subject plane (ADR-0050): the captured-content store (used as
/// both the capture sink a run writes to and the eraser the endpoint fans out to)
/// and the subject/consent repo. One captured-content instance backs both the sink
/// and the eraser, so a run's content is erasable. Durable (sqlite under
/// `AWAKEN_STORAGE_DIR`) or in-memory. Built once at composition (build_router).
pub fn data_subject_plane() -> (
    Arc<dyn awaken_runtime_contract::CaptureSink>,
    Arc<dyn awaken_runtime_contract::ContentEraser>,
    Arc<dyn awaken_data_subject::DataSubjectRepo>,
) {
    use awaken_data_subject::{
        InMemoryCapturedContentStore, InMemoryDataSubjectRepo, SqliteCapturedContentStore,
        SqliteDataSubjectRepo,
    };

    let dir = std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .filter(|v| !v.is_empty());
    match dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir).expect("create AWAKEN_STORAGE_DIR");
            let cap = Arc::new(
                SqliteCapturedContentStore::open(&format!("{dir}/captured_content.db"))
                    .expect("open captured-content db"),
            );
            let repo = Arc::new(
                SqliteDataSubjectRepo::open(&format!("{dir}/data_subject.db"))
                    .expect("open data-subject db"),
            );
            (cap.clone(), cap, repo)
        }
        None => {
            let cap = Arc::new(InMemoryCapturedContentStore::new());
            let repo = Arc::new(InMemoryDataSubjectRepo::new());
            (cap.clone(), cap, repo)
        }
    }
}

/// The composition seam refuses to build an executor from an incomplete or
/// unservable [`ResolvedInference`] (ADR-0043, fail-closed).
#[derive(Debug, thiserror::Error)]
pub enum ResolvedExecutorError {
    #[error("resolved inference has no base_url for adapter `{0}`")]
    MissingBaseUrl(String),
    #[error("resolved inference carries no credential (unauthenticated run refused)")]
    MissingCredential,
    #[error("no provider executor in this build serves adapter `{0}`")]
    UnsupportedAdapter(String),
}

/// Build the run-loop's model executor from a management-plane [`ResolvedInference`]
/// (ADR-0043). The resolver already produced the execution triple's adapter kind,
/// endpoint base URL, and the *resolved* credential value; this composition seam is
/// the only place that turns that into the concrete provider executor the host
/// drives. The runtime never sees the credential binding — only the already-resolved
/// [`RedactedString`](awaken_agent_contract::RedactedString) crosses in here (D6/D9),
/// and it is exposed exactly once to construct the client. Fail-closed on a missing
/// base URL/credential or an adapter this build cannot serve.
pub fn executor_from_resolved(
    inference: &ResolvedInference,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    executor_from_materialized_access(
        inference.adapter_kind,
        inference.base_url.as_deref(),
        inference.credential.as_ref(),
    )
}

/// Construct a provider executor from publication-pinned endpoint facts and
/// request-time credential material. This is the runtime half of resolution: it
/// does not consult a catalog, select a route, or select a credential.
pub fn executor_from_materialized_access(
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    // One path for every API-key provider: map the catalog's adapter-kind to a genai
    // adapter and hand it the resolved credential + (optional) gateway base URL. The
    // key comes from the resolved credential, never inlined by the Managed wire. A new
    // provider is one line in `genai_adapter` + catalog config — no new branch here.
    let adapter = genai_adapter(adapter_kind)
        .ok_or_else(|| ResolvedExecutorError::UnsupportedAdapter(adapter_kind.to_string()))?;
    // Fail closed on an incomplete resolution: the management plane always resolves the
    // execution triple's endpoint, so a `None` base URL means the inference never bound
    // an endpoint — refuse rather than silently fall back to the genai default endpoint.
    let base_url =
        base_url.ok_or_else(|| ResolvedExecutorError::MissingBaseUrl(adapter_kind.to_string()))?;
    let credential = credential.ok_or(ResolvedExecutorError::MissingCredential)?;
    Ok(Arc::new(GenaiExecutor::from_resolved(
        adapter,
        Some(base_url.to_string()),
        credential.expose_secret(),
    )))
}

/// Map our catalog's wire dialect (`ApiDialect::adapter_kind`) to a genai adapter.
/// The one place a supported provider wire is named; genai's default endpoint is used
/// unless the catalog endpoint supplies a gateway base URL.
fn genai_adapter(adapter_kind: &str) -> Option<awaken_provider_genai::AdapterKind> {
    use awaken_provider_genai::AdapterKind;
    Some(match adapter_kind {
        "anthropic" => AdapterKind::Anthropic,
        "gemini" => AdapterKind::Gemini,
        "vertex" => AdapterKind::Vertex,
        "openai" => AdapterKind::OpenAI,
        _ => return None,
    })
}

#[cfg(test)]
mod executor_seam_tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_config_resolver::{InferenceTriple, ResolvedInference};
    use awaken_model_catalog::ApiDialect;
    use awaken_provider_genai::AdapterKind;

    /// Build a hermetic `ResolvedInference` — the resolver's output the composition
    /// seam turns into an executor — with a chosen adapter/base_url/credential.
    fn inference(
        adapter: &'static str,
        base_url: Option<&str>,
        credential: Option<&str>,
    ) -> ResolvedInference {
        ResolvedInference {
            triple: InferenceTriple {
                model_id: "m".into(),
                provider_id: "p".into(),
                protocol_endpoint_id: "ep".into(),
                dialect: ApiDialect::AnthropicMessages,
            },
            adapter_kind: adapter,
            base_url: base_url.map(str::to_string),
            credential: credential.map(RedactedString::new),
        }
    }

    /// The one place a supported provider wire is named maps exactly the three the
    /// build serves, and returns `None` (→ `UnsupportedAdapter`) for everything else,
    /// case-sensitively.
    #[test]
    fn genai_adapter_maps_the_supported_wires_and_rejects_the_rest() {
        assert_eq!(genai_adapter("anthropic"), Some(AdapterKind::Anthropic));
        assert_eq!(genai_adapter("gemini"), Some(AdapterKind::Gemini));
        assert_eq!(genai_adapter("vertex"), Some(AdapterKind::Vertex));
        assert_eq!(genai_adapter("openai"), Some(AdapterKind::OpenAI));
        // Fail-closed: an unserved wire, the empty string, and a case variant all miss.
        assert_eq!(genai_adapter("cohere"), None);
        assert_eq!(genai_adapter(""), None);
        assert_eq!(genai_adapter("Anthropic"), None);
    }

    /// `UnsupportedAdapter` is the reachable fail-closed arm: a resolved inference
    /// whose `adapter_kind` no provider in this build serves is refused, naming the
    /// adapter — never silently built into some default executor.
    #[test]
    fn executor_from_resolved_fails_closed_on_an_unsupported_adapter() {
        let inf = inference("cohere", Some("https://gw/"), Some("sk-secret"));
        // `Ok` carries an `Arc<dyn LlmExecutor>` (not `Debug`), so map to the error first.
        match executor_from_resolved(&inf).err() {
            Some(ResolvedExecutorError::UnsupportedAdapter(a)) => assert_eq!(a, "cohere"),
            other => panic!("expected UnsupportedAdapter, got {other:?}"),
        }
    }

    /// `MissingBaseUrl` is a reachable fail-closed arm: a resolved inference whose
    /// `base_url` is `None` never bound an endpoint, so the seam refuses to build an
    /// executor (rather than silently falling back to the genai default endpoint),
    /// naming the adapter.
    #[test]
    fn missing_base_url_is_refused_by_the_seam() {
        // A supported adapter + a credential but NO base URL fails closed.
        let inf = inference("anthropic", None, Some("sk-secret"));
        match executor_from_resolved(&inf).err() {
            Some(ResolvedExecutorError::MissingBaseUrl(a)) => assert_eq!(a, "anthropic"),
            other => panic!("expected MissingBaseUrl, got {other:?}"),
        }
        // The variant renders its intended message.
        let err = ResolvedExecutorError::MissingBaseUrl("anthropic".into());
        assert_eq!(
            err.to_string(),
            "resolved inference has no base_url for adapter `anthropic`"
        );
    }

    /// A supported adapter with both a base URL and a credential builds an executor
    /// (the happy path the three fail-closed arms bracket).
    #[test]
    fn executor_from_resolved_builds_for_a_supported_adapter_with_a_credential() {
        assert!(
            executor_from_resolved(&inference("openai", Some("https://gw/"), Some("k"))).is_ok()
        );
        assert!(
            executor_from_resolved(&inference("gemini", Some("https://gw/"), Some("k"))).is_ok()
        );
        assert!(
            executor_from_resolved(&inference("vertex", Some("https://gw/"), Some("oauth")))
                .is_ok()
        );
    }
}
