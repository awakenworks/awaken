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
//! ([`executor_from_resolved`]), the config-plane executor provider, the model
//! resolver, the no-model fallback, workspace path addressing, and the Worker /
//! Hand role helpers. Its sibling **authoring / authz plane** lives in
//! `awaken-control`; neither depends on the other, and `awaken-cli` is the
//! composition root that weaves them into one management router. The service layer
//! (the host, the two port adapters, the per-plane resource routers) lives in
//! `awaken-runtime-host`.

pub mod config_executor;
mod hand_server;
pub mod model_resolver;
pub mod no_model;
pub mod dynamic_placement;
pub mod placement;
pub mod resource_owner;
pub mod webhooks;
pub mod workspace_path;
pub use crate::hand_server::{run_hand_server, run_hand_server_nats};

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
pub use awaken_runtime_host::{
    ConfigService, ExecutorProvider, ExtMcpProbe, HostResume, HttpTransport, ManagedHost,
    PreparedMcpRefresh, ProtocolHost, Response, SharedHost, SkillContext, SkillSpec, ThreadEvent,
    ThreadEventHub, Transport, VaultRefresher, advertised_tools, capabilities_router,
    config_router, content_fingerprint, default_models, durable_ops_router, files_router,
    memory_stores_router, models_router, parse_skill_md, skills_router,
};

/// An [`ExecutorProvider`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
pub fn mount(host: Arc<SharedHost>) -> Router {
    // The webhook plane (ADR-0048) now lives in the management path
    // (`management_router_over`): subscriptions are a config resource in the admin
    // store and their secret is sealed in the vault, so a webhook needs the config
    // plane. The plain mount has neither, so it wires no sink — a bare host emits no
    // webhooks (identical to an unconfigured plane before).
    let state = ManagedState::new(ManagedHost::new(host.clone()));
    mount_with_managed(host, Arc::new(state))
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
    /// A remote ACP executor endpoint (the hand role, ADR-0044/0045).
    Hand,
}

/// Pure role selection: an explicit `AWAKEN_ROLE` wins; otherwise infer from
/// whether the historic hand / worker env is configured. Unit-tested without env.
fn role_from(explicit: Option<&str>, hand_configured: bool, worker_configured: bool) -> Role {
    match explicit {
        Some("worker") => Role::Worker,
        Some("hand") => Role::Hand,
        Some("serve") | Some("server") | Some("coordinator") | Some("all-in-one") => Role::Serve,
        _ if hand_configured => Role::Hand,
        _ if worker_configured => Role::Worker,
        _ => Role::Serve,
    }
}

/// The deployment role from the environment.
pub fn deployment_role() -> Role {
    let set = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).is_some();
    let explicit = std::env::var("AWAKEN_ROLE").ok();
    role_from(
        explicit.as_deref(),
        set("AWAKEN_HAND_NATS") || set("AWAKEN_HAND_DIAL") || set("AWAKEN_HAND_LISTEN"),
        set("AWAKEN_UPSTREAM_URL"),
    )
}

/// Run the hand role: a remote ACP executor endpoint over the transport selected by
/// `AWAKEN_HAND_*` (NATS relay / dial-out / listen).
pub async fn run_hand_role() -> Result<(), Box<dyn std::error::Error>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(nats_url) = env("AWAKEN_HAND_NATS") {
        let subject =
            std::env::var("AWAKEN_HAND_SUBJECT").unwrap_or_else(|_| "awaken.hand.exec".to_string());
        return run_hand_server_nats(&nats_url, &subject).await;
    }
    if let Some(dial_addr) = env("AWAKEN_HAND_DIAL") {
        return run_hand_server(&dial_addr, true).await;
    }
    if let Some(hand_addr) = env("AWAKEN_HAND_LISTEN") {
        return run_hand_server(&hand_addr, false).await;
    }
    Err(
        "AWAKEN_ROLE=hand requires one of AWAKEN_HAND_NATS / AWAKEN_HAND_DIAL / AWAKEN_HAND_LISTEN"
            .into(),
    )
}

#[cfg(test)]
mod role_tests {
    use super::{Role, role_from};

    #[test]
    fn explicit_role_wins() {
        assert_eq!(role_from(Some("worker"), false, false), Role::Worker);
        assert_eq!(role_from(Some("hand"), false, false), Role::Hand);
        assert_eq!(role_from(Some("coordinator"), true, true), Role::Serve);
        assert_eq!(role_from(Some("all-in-one"), true, true), Role::Serve);
    }

    #[test]
    fn inference_from_historic_env_when_role_unset() {
        // Hand env → Hand; upstream → Worker; neither → Serve (the default).
        assert_eq!(role_from(None, true, false), Role::Hand);
        assert_eq!(role_from(None, false, true), Role::Worker);
        assert_eq!(role_from(None, false, false), Role::Serve);
        // Hand takes precedence over worker when both are somehow set.
        assert_eq!(role_from(None, true, true), Role::Hand);
    }

    #[test]
    fn an_unknown_explicit_role_falls_back_to_inference() {
        assert_eq!(role_from(Some("bogus"), false, true), Role::Worker);
        assert_eq!(role_from(Some("bogus"), false, false), Role::Serve);
    }
}

// The database-less **worker** role moved to the production `awaken-worker` crate
// (Stage C): it resolves EACH drained run's model from the DB-configured catalog +
// vault via `ConfigExecutorProvider`, with `NoModelConfiguredExecutor` only as the
// fallback. The `awaken` binary's Worker role delegates to `awaken_worker::run`. The
// test-only echo-draining worker (for the worker-pool e2e) lives in
// `awaken-scenario-host::run_echo_worker`.

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
pub fn mount_with_managed(host: Arc<SharedHost>, managed_state: Arc<ManagedState>) -> Router {
    // Spawn the process-level dispatch pool once when durable ingress is enabled
    // (O2): it is the sole claimer of the shared queue and drives every session's
    // runs. This is the single seam that owns an `Arc<SharedHost>`, which the pool's
    // session resolver needs.
    //
    // A coordinator-only cell server (`AWAKEN_DISABLE_LOCAL_POOL=1`) skips its
    // co-located pool so remote database-less workers are the sole drainers, claiming
    // and settling over the dispatch transport.
    if std::env::var("AWAKEN_DISABLE_LOCAL_POOL").as_deref() != Ok("1") {
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
    let dispatch_transport = awaken_runtime_host::dispatch_transport_router(host.clone());
    let commit_ingest = awaken_runtime_host::commit_ingest_router(host.clone());
    // The Files API (`/v1/files`) over the host's blob store — file resources + artifacts.
    let files = files_router(host.clone());
    // Tenant ownership for memory stores (ADR-0053 / ADR-0051): fence cross-tenant
    // access to a store (and its memories/versions) by the scope that created it. A
    // single-tenant deployment resolves to the default scope and is never fenced.
    let memory_stores =
        memory_stores_router(host.clone()).layer(axum::middleware::from_fn_with_state(
            crate::resource_owner::ResourceOwners::new(),
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
    managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
        .merge(dispatch_transport)
        .merge(commit_ingest)
        .merge(files)
        .merge(memory_stores)
        .merge(skills)
        .merge(models)
        .merge(erasure)
        .merge(consent)
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
    MissingBaseUrl(&'static str),
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
    // One path for every API-key provider: map the catalog's adapter-kind to a genai
    // adapter and hand it the resolved credential + (optional) gateway base URL. The
    // key comes from the resolved credential, never inlined by the Managed wire. A new
    // provider is one line in `genai_adapter` + catalog config — no new branch here.
    let adapter = genai_adapter(inference.adapter_kind).ok_or_else(|| {
        ResolvedExecutorError::UnsupportedAdapter(inference.adapter_kind.to_string())
    })?;
    let credential = inference
        .credential
        .as_ref()
        .ok_or(ResolvedExecutorError::MissingCredential)?;
    Ok(Arc::new(GenaiExecutor::from_resolved(
        adapter,
        inference.base_url.clone(),
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
        "openai" => AdapterKind::OpenAI,
        _ => return None,
    })
}

/// Map a cloud-managed grant's `dialect` (a free-form wire name carried in the
/// `ModelAccessGrant::CloudManagedGateway`) to a genai adapter. More lenient than
/// [`genai_adapter`] because the dialect is authored on the grant side, not our
/// catalog: match case-insensitively on the provider family (`AnthropicMessages`,
/// `anthropic`, `openai-compatible`, …). Unknown → `None`, so the factory fails
/// closed rather than dialing a wire it cannot speak.
fn gateway_dialect_adapter(dialect: &str) -> Option<awaken_provider_genai::AdapterKind> {
    use awaken_provider_genai::AdapterKind;
    let d = dialect.to_ascii_lowercase();
    Some(if d.contains("anthropic") {
        AdapterKind::Anthropic
    } else if d.contains("gemini") || d.contains("google") {
        AdapterKind::Gemini
    } else if d.contains("openai") {
        AdapterKind::OpenAI
    } else {
        return None;
    })
}

/// The genai implementation of the [`GatewayExecutorFactory`](awaken_runtime_host::GatewayExecutorFactory)
/// port (ADR-0004): builds a `GenaiExecutor` that dials a materialized cloud-managed
/// gateway endpoint. The `bearer` (a short-lived lease token) is presented on egress
/// as the API key — the gateway validates the lease and injects the real provider
/// credential out of this process, so the worker holds no provider secret. The base
/// URL is the gateway (never an arbitrary provider), so a gateway run can never be
/// pointed at a raw provider endpoint. Composition roots (serve, worker) install this
/// so the native path honors gateway grants; the closed awaken-cloud layer may inject
/// its own factory instead — the host depends only on the port.
#[derive(Default)]
pub struct GenaiGatewayExecutorFactory;

impl awaken_runtime_host::GatewayExecutorFactory for GenaiGatewayExecutorFactory {
    fn build(
        &self,
        endpoint: &awaken_runtime_contract::model_access::ResolvedModelEndpoint,
    ) -> Option<Arc<dyn LlmExecutor>> {
        // A gateway endpoint always carries base_url + bearer (structurally `Some`);
        // an unknown/absent dialect fails closed.
        let base_url = endpoint.base_url.clone()?;
        let bearer = endpoint.bearer.clone()?;
        let adapter = gateway_dialect_adapter(endpoint.dialect.as_deref()?)?;
        Some(Arc::new(GenaiExecutor::from_resolved(
            adapter,
            Some(base_url),
            bearer,
        )))
    }
}

#[cfg(test)]
mod gateway_factory_tests {
    use super::{GenaiGatewayExecutorFactory, gateway_dialect_adapter};
    use awaken_provider_genai::AdapterKind;
    use awaken_runtime_contract::model_access::{ModelAccessGrant, ResolvedModelEndpoint};
    use awaken_runtime_host::GatewayExecutorFactory;

    #[test]
    fn dialect_maps_case_insensitively_by_provider_family() {
        // Free-form grant dialects normalize to a genai adapter by family.
        assert_eq!(gateway_dialect_adapter("anthropic"), Some(AdapterKind::Anthropic));
        assert_eq!(
            gateway_dialect_adapter("AnthropicMessages"),
            Some(AdapterKind::Anthropic)
        );
        assert_eq!(gateway_dialect_adapter("OpenAI"), Some(AdapterKind::OpenAI));
        assert_eq!(
            gateway_dialect_adapter("openai-compatible"),
            Some(AdapterKind::OpenAI)
        );
        assert_eq!(gateway_dialect_adapter("gemini"), Some(AdapterKind::Gemini));
        assert_eq!(gateway_dialect_adapter("google-gemini"), Some(AdapterKind::Gemini));
        // Unknown dialect fails closed.
        assert_eq!(gateway_dialect_adapter("cohere"), None);
        assert_eq!(gateway_dialect_adapter(""), None);
    }

    #[test]
    fn factory_builds_an_executor_for_a_materialized_gateway_grant() {
        let endpoint = ModelAccessGrant::CloudManagedGateway {
            gateway_base_url: "https://gw.internal".into(),
            dialect: "AnthropicMessages".into(),
            model_ref: "claude".into(),
            lease_token: "lease-abc".into(), // awaken-allow: secret
        }
        .materialize();
        assert!(
            GenaiGatewayExecutorFactory.build(&endpoint).is_some(),
            "a gateway endpoint with a known dialect yields an executor"
        );
    }

    #[test]
    fn factory_fails_closed_on_unknown_dialect_and_on_a_local_endpoint() {
        // Unknown dialect → None (fail closed, never dial a wire we can't speak).
        let unknown = ResolvedModelEndpoint {
            base_url: Some("https://gw.internal".into()),
            model_ref: Some("m".into()),
            bearer: Some("lease".into()),
            dialect: Some("cohere".into()),
        };
        assert!(GenaiGatewayExecutorFactory.build(&unknown).is_none());

        // A local grant materializes to None base_url/bearer → not buildable as a
        // gateway executor (the factory only serves gateway endpoints).
        let local = ModelAccessGrant::LocalSelfCredentialed {
            model_ref: Some("m".into()),
            provider_ref: None,
        }
        .materialize();
        assert!(GenaiGatewayExecutorFactory.build(&local).is_none());
    }
}
