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

pub mod admin;
pub mod config_executor;
pub mod dynamic_placement;
mod hand_server;
pub mod model_resolver;
pub mod no_model;
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

/// The hand's transport, selected from `AWAKEN_HAND_*`. One of three ADR-0044/0045
/// topologies: a NATS relay (both sides behind NAT), a dial-out to a rendezvous
/// (reverse), or a listen socket the brain dials (direct).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandTransport {
    /// `AWAKEN_HAND_NATS` — relay over a NATS subject (`AWAKEN_HAND_SUBJECT`, default
    /// `awaken.hand.exec`).
    Nats { url: String, subject: String },
    /// `AWAKEN_HAND_DIAL` — the hand dials out to a rendezvous the brain also dials
    /// (reverse topology, for a hand behind NAT).
    Dial { addr: String },
    /// `AWAKEN_HAND_LISTEN` — the hand listens; the brain dials it (direct topology).
    Listen { addr: String },
}

/// Select the hand transport from an injected env `lookup` (empty values read as
/// unset), so the precedence is pure and unit-testable without the process env. NATS
/// wins over dial wins over listen when several are set; none set is a fail-closed
/// error naming the three keys.
pub fn hand_transport(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<HandTransport, &'static str> {
    let get = |k: &str| lookup(k).filter(|v| !v.is_empty());
    if let Some(url) = get("AWAKEN_HAND_NATS") {
        let subject = get("AWAKEN_HAND_SUBJECT").unwrap_or_else(|| "awaken.hand.exec".to_string());
        return Ok(HandTransport::Nats { url, subject });
    }
    if let Some(addr) = get("AWAKEN_HAND_DIAL") {
        return Ok(HandTransport::Dial { addr });
    }
    if let Some(addr) = get("AWAKEN_HAND_LISTEN") {
        return Ok(HandTransport::Listen { addr });
    }
    Err("AWAKEN_ROLE=hand requires one of AWAKEN_HAND_NATS / AWAKEN_HAND_DIAL / AWAKEN_HAND_LISTEN")
}

/// Run the hand role: a remote ACP executor endpoint over the transport selected by
/// `AWAKEN_HAND_*` (NATS relay / dial-out / listen).
pub async fn run_hand_role() -> Result<(), Box<dyn std::error::Error>> {
    match hand_transport(|k| std::env::var(k).ok())? {
        HandTransport::Nats { url, subject } => run_hand_server_nats(&url, &subject).await,
        HandTransport::Dial { addr } => run_hand_server(&addr, true).await,
        HandTransport::Listen { addr } => run_hand_server(&addr, false).await,
    }
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

/// S11 — the hand's transport selection across the three ADR-0044/0045 topologies.
/// Decision table over the `AWAKEN_HAND_*` keys: which transport is chosen, the
/// precedence when several are set, the NATS subject default, and the fail-closed
/// error when none is set. This covers the (topology × transport) pairs the k3d
/// `topology_e2e.sh` exercises end to end, but as a pure, fast, deterministic unit.
#[cfg(test)]
mod hand_transport_tests {
    use super::{HandTransport, hand_transport};

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn listen_selects_the_direct_topology() {
        // direct: the hand listens, the brain dials it.
        assert_eq!(
            hand_transport(lookup(&[("AWAKEN_HAND_LISTEN", "0.0.0.0:9000")])).unwrap(),
            HandTransport::Listen {
                addr: "0.0.0.0:9000".into()
            }
        );
    }

    #[test]
    fn dial_selects_the_reverse_topology() {
        // reverse: the hand dials out to a rendezvous (it is behind NAT).
        assert_eq!(
            hand_transport(lookup(&[("AWAKEN_HAND_DIAL", "brain-rendezvous:9000")])).unwrap(),
            HandTransport::Dial {
                addr: "brain-rendezvous:9000".into()
            }
        );
    }

    #[test]
    fn nats_selects_the_relay_topology_with_a_default_subject() {
        // relay: both sides reach a NATS broker; the subject defaults when unset.
        assert_eq!(
            hand_transport(lookup(&[("AWAKEN_HAND_NATS", "nats://nats:4222")])).unwrap(),
            HandTransport::Nats {
                url: "nats://nats:4222".into(),
                subject: "awaken.hand.exec".into(),
            }
        );
    }

    #[test]
    fn nats_subject_is_overridable() {
        assert_eq!(
            hand_transport(lookup(&[
                ("AWAKEN_HAND_NATS", "nats://nats:4222"),
                ("AWAKEN_HAND_SUBJECT", "team.hand"),
            ]))
            .unwrap(),
            HandTransport::Nats {
                url: "nats://nats:4222".into(),
                subject: "team.hand".into(),
            }
        );
    }

    #[test]
    fn precedence_is_nats_then_dial_then_listen() {
        // All three set: NATS wins.
        let all = [
            ("AWAKEN_HAND_NATS", "nats://n:4222"),
            ("AWAKEN_HAND_DIAL", "r:9000"),
            ("AWAKEN_HAND_LISTEN", "0.0.0.0:9000"),
        ];
        assert!(matches!(
            hand_transport(lookup(&all)).unwrap(),
            HandTransport::Nats { .. }
        ));
        // Dial wins over listen when NATS is absent.
        assert!(matches!(
            hand_transport(lookup(&all[1..])).unwrap(),
            HandTransport::Dial { .. }
        ));
    }

    #[test]
    fn no_transport_is_a_fail_closed_error_naming_the_keys() {
        let err = hand_transport(lookup(&[])).unwrap_err();
        assert!(err.contains("AWAKEN_HAND_NATS"));
        assert!(err.contains("AWAKEN_HAND_DIAL"));
        assert!(err.contains("AWAKEN_HAND_LISTEN"));
    }

    #[test]
    fn an_empty_value_reads_as_unset() {
        // An empty AWAKEN_HAND_NATS must not select the NATS path over a real dial.
        assert_eq!(
            hand_transport(lookup(&[
                ("AWAKEN_HAND_NATS", ""),
                ("AWAKEN_HAND_DIAL", "r:9000"),
            ]))
            .unwrap(),
            HandTransport::Dial {
                addr: "r:9000".into()
            }
        );
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
