//! `awaken-server-local` — the single-machine assembly.
//!
//! It composes one protocol-neutral [`SharedHost`] (from `awaken-runtime-host`,
//! the thread-keyed session substrate) and mounts public protocol adapters over
//! it. Each adapter is a thin port implementation that translates its own wire
//! vocabulary to the host's neutral operations; because every adapter keys by the
//! same thread id and drives the same coordinator, a turn started through one
//! protocol can be resumed or observed through another on the *same thread*.
//!
//! This crate is the ASSEMBLY layer: the service layer (the host, the two port
//! adapters, and the per-plane resource routers) lives in `awaken-runtime-host`;
//! here we only wire routers, demo models, and the embedded management plane.

pub mod admin_assistant;
mod authz;
mod brain_admin;
mod config_executor;
mod control_stores;
mod hand_server;
pub mod model_resolver;
mod no_model;
pub mod placement;
pub mod resource_owner;
pub mod webhooks;
pub mod workspace_path;
pub use crate::hand_server::{run_hand_server, run_hand_server_nats};

pub use crate::brain_admin::{DrainController, with_brain_admin};

use std::collections::HashSet;
use std::sync::Arc;

use awaken_config_resolver::ResolvedInference;
use awaken_protocol_managed::{ManagedState, router};
use awaken_protocol_transport::ProtocolRuntime;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{
    LlmExecutor,
};
use axum::Router;

// Embedded management-plane IAM (ADR-0042/0043 P1): the authorizer, its boot
// fn, the mint spec (tests / operator embeddings), and the bootstrap constants.
pub use crate::authz::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz, TokenSpec,
    embedded_iam,
};
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
    Err("AWAKEN_ROLE=hand requires one of AWAKEN_HAND_NATS / AWAKEN_HAND_DIAL / AWAKEN_HAND_LISTEN".into())
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
        // Empty inline + whitespace file path → neither is present → the unset error,
        // and the (empty) file path is never read.
        let err = resolve_seal_key_hex(Some("  ".into()), Some("".into()), no_read).unwrap_err();
        assert!(err.contains("neither AWAKEN_MGMT_SEAL_KEY"), "{err}");
    }

    #[test]
    fn an_unreadable_file_reports_the_path() {
        let err = resolve_seal_key_hex(None, Some("/nope".into()), |_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"))
        })
        .unwrap_err();
        assert!(err.contains("/nope") && err.contains("could not be read"), "{err}");
    }
}

/// Run this process as a database-less **worker** of the cell server at `upstream`:
/// its dispatch pool claims and settles runs over the server's dispatch transport,
/// and its commit boundary posts facts to the server's commit ingest
/// (`with_upstream`). It holds no store and serves no HTTP — it only drains.
/// Requires `AWAKEN_INGRESS=durable` (the pool's enable gate); the injected remote
/// store routes the drain over HTTP instead of a local queue. A deterministic echo
/// model keeps the worker self-contained (no upstream model needed).
pub async fn run_worker(upstream: &str) -> Result<(), Box<dyn std::error::Error>> {
    awaken_runtime_host::init_shared_dispatch_store(awaken_runtime_host::worker_dispatch_store(
        upstream,
    ));
    // Production worker: no mock model. Stage C replaces this with a real
    // `ConfigExecutorProvider` over the shared stores so a drained run resolves its
    // configured model; until then the fallback is the provider-free guidance model.
    let host = Arc::new(
        SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "worker")
            .with_upstream(upstream),
    );
    host.ensure_dispatch_pool();
    eprintln!("awaken-server-local worker draining from {upstream}");
    // Drain in the background; block until stopped.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    Ok(())
}

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

/// A router whose agent can delegate to a `researcher` sub-agent via `agent_run`
/// (the multi-agent e2e). `ghost` is deliberately absent from the roster so the
/// fail-closed path can be exercised.
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
/// shared by the admin router, the vault front door, and session prepare.
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
    /// management console authors directly, and their publications. Distinct from
    /// the SDK-facing `/v1/agents` registry — this is the console's agent source.
    /// Scoped so a workspace's config is fenced from another's (ADR-0051).
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
/// sqlite-repos: one SQLite file per domain bundle —
///
/// - `catalog.db`   — the `awaken.catalog` bundle (providers/endpoints/offerings)
/// - `credential.db` — the `awaken.credential` bundle: the secret-free
///   source/pool rows (`SqliteCredentialRepo`) **and** the AEAD-sealed secret
///   blobs (`SqliteSealedBlobStore` under `SealedAeadSecretStore::over`, sealed
///   with `key`). The two adapters share the one file safely: both run the same
///   `credential` migration bundle, and the scoped-migration ledger makes the
///   second run a no-op; admin-plane writes are short single statements, so two
///   connections on one file do not contend in practice.
/// - `admin.db`     — the `awaken.admin` bundle (profiles / MCP defs / agent↔MCP)
///
/// Panics on open/migrate failure: the binary's mode selection has no error
/// channel (matching e.g. `build_config_router`), and a management server that
/// silently fell back to ephemeral stores would be worse than one that refuses
/// to start.
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

/// Open the control-plane stores per the [`ControlStoreConfig`] — each component on
/// its own database (SQLite file or shared Postgres). This generalizes
/// [`durable_management_stores`] (which is the all-SQLite bundle special case): each
/// store independently honors its `AWAKEN_<COMPONENT>_DB` override, so credentials can
/// live on a hardened Postgres while config stays elsewhere, and a separate control /
/// server process can share the same per-component databases (Option A, shared-DB).
async fn open_management_stores(
    cfg: crate::control_stores::ControlStoreConfig,
    key: &[u8; 32],
) -> ManagementStores {
    use crate::control_stores::StoreBackend;

    // Create the parent directory for any SQLite path (a bundle dir or a custom path).
    fn ensure_parent(backend: &StoreBackend) {
        if let StoreBackend::Sqlite(path) = backend {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create control-store directory");
            }
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
                awaken_admin_config_api::SqliteAdminStore::open(&path(p)).expect("open admin sqlite"),
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
/// Two sources, exactly one required:
///   - `AWAKEN_MGMT_SEAL_KEY`       — the key hex inline (today's behavior),
///   - `AWAKEN_MGMT_SEAL_KEY_FILE`  — a path to a file holding the key hex
///     (keeps the key out of the environment / `ps` / logs; put it on tmpfs, mode
///     0600, backed up separately from the DB — this is the source that lets an
///     operator manage/rotate the key via the deployment rather than the runtime).
/// Both set is a hard error (ambiguous); neither set is the same brick-on-restart
/// error as before. The file's contents are parsed identically to the inline value
/// (64 hex chars; a trailing newline is trimmed by [`parse_seal_key`]).
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

/// The AEAD key for the durable management plane, from `AWAKEN_MGMT_SEAL_KEY` (the
/// key hex inline) **or** `AWAKEN_MGMT_SEAL_KEY_FILE` (a path to a file holding it).
/// Exactly one must be set. Fails loudly when unset, both-set, unreadable, or
/// malformed: a durable store sealed under an ephemeral random key would look
/// healthy until the first restart, then every persisted secret would be unopenable.
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

/// Serve the management plane (admin + vaults + sessions) with **persistence
/// selected from the environment** (mirrors `AWAKEN_STORE` / `AWAKEN_INGRESS`):
///
/// - `AWAKEN_MGMT_DIR` unset — in-memory stores, exactly the previous behavior.
/// - `AWAKEN_MGMT_DIR=<dir>` — SQLite-backed stores under `<dir>`
///   (`catalog.db` / `credential.db` / `admin.db`), with secrets AEAD-sealed
///   under the seal key (**required** then: 64 hex characters = a 32-byte key;
///   unset or malformed panics rather than sealing under a key that cannot
///   survive a restart). The key comes from exactly one of `AWAKEN_MGMT_SEAL_KEY`
///   (inline) or `AWAKEN_MGMT_SEAL_KEY_FILE` (a path to a file holding it — keeps
///   the key out of the environment / `ps` / logs); setting both is an error.
///
/// What persists across a restart is the authored **domain** state: the catalog,
/// the secret-free credential/pool rows plus their sealed secrets, and the
/// admin aggregates (inference profiles, MCP server defs, agent↔MCP bindings).
/// The Managed **wire** bookkeeping stays host-ephemeral by design: vault ids /
/// vault-credential wire objects (`VaultState`), sessions, and thread state are
/// rebuilt fresh per process (session durability has its own axis,
/// `AWAKEN_STORAGE_DIR`). After a restart a vault wire GET 404s while the
/// domain row it entered is still there for the resolver.
///
/// Additionally (ADR-0042/0043 P1), `AWAKEN_MGMT_IAM=embedded` gates the
/// management surfaces (`/v1/config/*` + `/v1/vaults/*`) behind bearer
/// `ApiToken` authn + preset-role authz (see [`crate::authz`]); it requires
/// `AWAKEN_MGMT_DIR` (the token/binding rows live in `<dir>/iam.sqlite`) and
/// panics with a clear message when it is missing. Unset — the default — is
/// today's open behavior, byte-identical.
pub async fn build_management_router() -> Router {
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
            let cfg = crate::control_stores::ControlStoreConfig::from_env(std::path::Path::new(&dir));
            management_router_over(
                open_management_stores(cfg, &key).await,
                iam,
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                crate::no_model::UNCONFIGURED_MODEL_REF.to_string(),
            )
            .await
        }
        Err(_) => {
            management_router_over(
                in_memory_management_stores(),
                iam,
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                crate::no_model::UNCONFIGURED_MODEL_REF.to_string(),
            )
            .await
        }
    }
}

/// Build the management router over in-memory stores with an explicit host default
/// model injected — a **test-only** seam so an integration test can drive the real
/// management router with a deterministic (mock) model, keeping the mock out of the
/// production assembly (which uses [`no_model::NoModelConfiguredExecutor`]).
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
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        crate::no_model::UNCONFIGURED_MODEL_REF.to_string(),
    )
    .await
}

/// [`build_durable_management_router`] with the embedded IAM guard enabled —
/// the env-free equivalent of `AWAKEN_MGMT_IAM=embedded`. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint
/// further workspace tokens against the same policy state.
pub async fn build_secured_management_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let router = management_router_over(
        durable_management_stores(dir, key),
        Some(iam.clone()),
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        crate::no_model::UNCONFIGURED_MODEL_REF.to_string(),
    )
    .await;
    (router, iam)
}

/// Mount the management plane over an explicit store set, optionally gated by
/// the embedded IAM guard (`iam`). The guard wraps ONLY the admin + vault
/// routers: the Managed session surface keeps its own axis and P1 does not
/// gate it (ADR-0043).
async fn management_router_over(
    stores: ManagementStores,
    iam: Option<Arc<ManagementAuthz>>,
    // The host default model for the window before an operator publishes one. In
    // production this is the provider-free `NoModelConfiguredExecutor` (guidance,
    // never a mock); a test may inject a deterministic model to drive a scenario
    // through the real management router.
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
    // model to a real executor from the live catalog + the workspace's credential,
    // so the console configures models via the API (no `AWAKEN_MODEL_SOURCE` env).
    let exec_catalog = catalog.clone();
    let exec_credentials = credentials.clone();
    let exec_secrets = secrets.clone();
    // ONE resource-binding store shared by the admin router (which authors an agent's
    // resources via `PUT /v1/config/agents/:id/resources`) and the config service
    // (which reads them into resource prompts + mounts at compile) — so a binding
    // authored through the API reaches the compiled config (ADR-0038).
    let resource_store: Arc<dyn awaken_config_resolver::ResourceStore> =
        Arc::new(awaken_admin_config_api::InMemoryResourceStore::new());
    // ONE MCP store across the admin router and the ManagedHost, and ONE
    // credential repo + secret store across admin, vaults, and sessions: a
    // credential or MCP config entered through any surface is the same row a
    // session's prepare reads (ADR-0043 Phase 3).
    let admin = awaken_admin_config_api::admin_router(awaken_admin_config_api::AdminState {
        catalog,
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles,
        mcp: mcp_store.clone(),
        // Per-agent resource bindings (ADR-0038). Ephemeral in-memory for now; the
        // durable SqliteAdminStore also implements `ResourceStore` for a later wire.
        resources: resource_store.clone(),
        // The live credential probe is backed by provider-genai here — the only
        // place the model SDK is named; the admin CRUD crate stays SDK-free.
        probe: Some(Arc::new(GenaiProbe)),
        // Shared credential-availability cooldowns (E3-4): the ops cooldown routes
        // record here and pool resolution reads it.
        availability: Default::default(),
    });
    // The webhook plane (ADR-0048): subscriptions are an id-addressed config resource
    // in the same admin store, their `whsec_` secret sealed in the shared vault. Merge
    // the front door into the admin router BEFORE the ownership fence so
    // `/v1/config/webhook-subscriptions/{id}` is tenant-fenced like profiles/MCP; the
    // sink (fed the same store + secrets) fans committed session facts out-of-band.
    let (webhook_sink, webhook_crud) = webhooks::assemble(
        webhook_store,
        secrets.clone(),
        std::env::var("AWAKEN_ORG_ID").ok(),
    );
    let admin = admin.merge(webhook_crud);
    // Tenant ownership for the id-addressed config resources (ADR-0051): MCP server
    // defs, inference profiles, and webhook subscriptions are fenced by the authoring
    // scope. The shared catalog is intentionally uncovered (org/deployment-level
    // config). Wraps the admin router only; these are matched routes, so a route
    // `layer` runs correctly.
    let admin = admin.layer(axum::middleware::from_fn_with_state(
        crate::resource_owner::ResourceOwners::new(),
        crate::resource_owner::resource_ownership_guard,
    ));
    let vault_state = Arc::new(
        awaken_protocol_managed::VaultState::new(secrets.clone(), credentials.clone())
            // The live MCP probe is backed by ext-mcp here — the only place the
            // MCP client is named for validation; the adapter crate stays
            // wire-client-free (mirrors the GenaiProbe pattern above).
            .with_probe(Arc::new(ExtMcpProbe)),
    );
    let vaults = awaken_protocol_managed::vault_router(vault_state.clone());
    // The user-profiles front door (`/v1/user_profiles`) over its own in-mem store.
    let user_profiles = awaken_protocol_managed::user_profiles_router(std::sync::Arc::new(
        awaken_protocol_managed::UserProfileState::new(),
    ));
    // ADR-0050 consent/erasure/enrollment routes come from `mount_with_managed`
    // (over the process-global captured-content store), so they are not mounted
    // here — doing so would double-mount and conflict.
    // `/v1/agents` is defined with the config plane below, so it projects published
    // config agents (ADR-0052) rather than a second in-mem store.
    // Deployments + deployment runs (`/v1/deployments`, `/v1/deployment_runs`).
    let deployments = awaken_protocol_managed::deployments_router(std::sync::Arc::new(
        awaken_protocol_managed::DeploymentState::new(),
    ));
    // Environments + work queue (`/v1/environments`, single-worker open cap). Shared
    // with the session state so `POST /v1/sessions` resolves an environment's
    // networking policy (egress on/off) at creation.
    let env_state = std::sync::Arc::new(awaken_protocol_managed::EnvironmentState::new());
    let environments = awaken_protocol_managed::environments_router(env_state.clone());
    // The config authoring plane (`/v1/config/agents/*`): the console authors the
    // rich `AgentConfig` here (basics + tools + plugins + plugin_config policy +
    // context) and `publish` compiles + installs it so sessions run that config.
    // The same service is wired into the host below, so a session for a published
    // agent resolves its installed config.
    // The server's model (real Gemini under `AWAKEN_MODEL_SOURCE=gemini`, else the
    // in-process MCP-driving model). Chosen up here because the admin assistant's
    // `Auto` binding resolves against a catalog carrying this model at seed time.
    let (model, model_ref) = (fallback_model, fallback_model_ref);
    // Scope-free `ConfigService` + the `ConfigPlane` scope edge (ADR-0051/0052): the
    // plane binds the request scope (a `ScopedConfig` registry + the scope's tool
    // catalog) onto the service per call. The reserved admin scope additionally sees
    // the four management descriptors (ADR-0052 D3), so the seeded assistant compiles.
    let global = advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
    let tool_catalog: Arc<dyn awaken_runtime_host::ToolCatalogSource> =
        Arc::new(awaken_runtime_host::ScopedToolCatalog::new(
            global.clone(),
            awaken_runtime_host::RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
    // A minimal catalog with an offering for the server model, so the assistant's
    // `Auto` selection resolves to a concrete binding at seed/publish (ADR-0052 D5).
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
            .with_model_resolver(Arc::new(crate::model_resolver::CatalogModelResolver::new(
                seed_catalog.clone(),
            )))
            .with_resources(resource_store.clone()),
    );
    // Warm-load the installed catalog from the durable config store BEFORE the plane
    // takes ownership: a fresh process (a restart, or — once control/server split —
    // a server that did not author the publish) repopulates `installed` so a
    // rehydrated session resolves its published agent, not the seed model. A no-op on
    // the in-memory path (nothing durable to reload).
    let warmed = config_service
        .warm_install(
            config.as_ref(),
            &awaken_tenancy::ScopeId::from(awaken_config_store::DEFAULT_SCOPE),
        )
        .await;
    if warmed > 0 {
        eprintln!("config: warm-loaded {warmed} published agent(s) from the durable store");
    }
    let plane = awaken_runtime_host::ConfigPlane::new(config_service.clone(), config, tool_catalog);
    // Seed the in-console Admin Assistant as an ordinary published agent in the
    // reserved scope (ADR-0052 D1/D2), so `/v1/agents/__admin_assistant` is live and a
    // session can run it. Best-effort: a server booted without a resolvable model still
    // starts (the assistant stays a draft until a model is configured + it republishes).
    if let Err(err) = crate::admin_assistant::seed_admin_assistant(&plane).await {
        eprintln!("admin assistant not seeded (configure a model, then republish): {err}");
    }
    // The management tool executables (ADR-0052 D3/D4): the capability reader reads the
    // shared catalog + advertised tools; the validator runs the publish-time compile
    // check on drafts in the tenant scope; every call is audited.
    let admin_execs = awaken_admin_assistant::admin_tools(
        Arc::new(crate::admin_assistant::CatalogCapabilityReader::new(
            &seed_catalog,
            &global,
            &[],
        )),
        Arc::new(crate::admin_assistant::ConfigServiceDraftValidator::new(
            plane.clone(),
            awaken_config_store::DEFAULT_SCOPE,
        )),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    let config_plane = config_router(plane);
    // `/v1/agents` projects the config plane it hosts: an agent published via
    // `/v1/config/agents` is retrievable as a managed-wire projection of that single
    // truth (no second store), which is how the console probes the assistant.
    let agents = awaken_protocol_managed::agents_router(std::sync::Arc::new(
        awaken_protocol_managed::AgentRegistryState::new().with_config_source(std::sync::Arc::new(
            awaken_runtime_host::ConfigServiceAgentSource(config_service.clone()),
        )),
    ));
    // Capability snapshot (`GET /v1/capabilities`): the host's tool descriptors +
    // installable plugins (with config schema) so the console authors data-driven.
    let capabilities = capabilities_router(global);

    // The IAM guard (when enabled) wraps the admin + vault routers only. An
    // axum layer binds to the routes present when it is applied, so merging
    // the guarded sub-router later leaves every other surface untouched. The
    // token-management routes exist ONLY under the guard (they authorize
    // against the same embedded IAM the guard authenticates with), and they
    // are merged before the layer so the guard authenticates them first.
    let mut mgmt = admin
        .merge(vaults)
        .merge(user_profiles)
        .merge(agents)
        .merge(deployments)
        .merge(environments)
        .merge(config_plane)
        .merge(capabilities);
    if let Some(iam) = iam {
        mgmt = mgmt.merge(crate::authz::token_router(iam.clone()));
        mgmt = mgmt.layer(axum::middleware::from_fn_with_state(
            iam,
            crate::authz::management_guard,
        ));
    }

    // The host runs the server model (Gemini or MCP-driving), resolves a session's
    // agent to its installed config, and carries the management tool executables so
    // the reserved-scope assistant can call them.
    // A durable skill catalog so `POST /v1/skills` persists a SKILL.md (without one the
    // route 409s); a skill created here appears in `/v1/skills` and is deliverable to
    // sessions. Under the management storage dir when set, else a per-process temp dir.
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
            .with_executor_provider(Arc::new(config_executor::ConfigExecutorProvider::new(
                exec_catalog,
                exec_credentials,
                exec_secrets,
                crate::authz::BOOTSTRAP_WORKSPACE,
            ))),
    );
    let managed_state = Arc::new(
        ManagedState::new(
            ManagedHost::new(host.clone())
                .with_mcp(credentials, secrets, mcp_store)
                // Share the SAME binding store the config service uses, so a published
                // agent's bound memory store is actually mounted at session-create — the
                // prompt (config) and the mount (here) come from one source of truth.
                .with_resources(resource_store.clone()),
        )
        .with_vaults(vault_state)
        .with_environments(env_state)
        .with_session_repo(sessions)
        .with_lifecycle_sink(webhook_sink),
    );
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope (RequestTenancy +
    // WorkspaceScope) before it re-enters routing; the guard then authenticates +
    // fences it and the per-resource ownership guards read the scope. Flat requests
    // fall through unchanged — the data plane (`/v1/sessions`) is never prefixed.
    let flat = mount_with_managed(host, managed_state).merge(mgmt);
    crate::workspace_path::with_workspace_path_addressing(flat)
}
