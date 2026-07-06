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

mod authz;
mod models;

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_config_resolver::ResolvedInference;
use awaken_protocol_managed::{ManagedState, router};
use awaken_protocol_transport::ProtocolRuntime;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use axum::Router;

// Embedded management-plane IAM (ADR-0042/0043 P1): the authorizer, its boot
// fn, the mint spec (tests / operator embeddings), and the bootstrap constants.
pub use crate::authz::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz, TokenSpec,
    embedded_iam,
};
pub use crate::models::*;
// The managed-agents service layer (`awaken-runtime-host`): the neutral host,
// the two port adapters, the per-plane routers, and the authoring/transport
// re-exports a composition root (and the integration tests) drive directly.
pub use awaken_runtime_host::{
    ConfigService, ExecutorProvider, ExtMcpProbe, HostResume, HttpTransport, ManagedHost,
    PreparedMcpRefresh, ProtocolHost, Response, SharedHost, SkillContext, SkillSpec, ThreadEvent,
    ThreadEventHub, Transport, VaultRefresher, advertised_tools, config_router,
    content_fingerprint, durable_ops_router, files_router, memory_stores_router, parse_skill_md,
    skills_router,
};

/// An [`ExecutorProvider`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
struct RouteProvider;

impl ExecutorProvider for RouteProvider {
    fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        match model_ref {
            "fast" => Some(Arc::new(LabelModel("fast"))),
            "slow" => Some(Arc::new(LabelModel("slow"))),
            _ => None,
        }
    }
}

/// A router whose per-session/per-turn model selection routes to distinct labeled
/// executors (R1/R2/R5/R6). `AWAKEN_MODEL_MODE=model-route`.
pub fn build_model_route_router() -> Router {
    mount(Arc::new(
        SharedHost::new(Arc::new(LabelModel("default")), "default")
            .with_executor_provider(Arc::new(RouteProvider)),
    ))
}

/// A minimal ACP agent (shell): read the prompt line, emit a message + turn_end —
/// stands in for `claude --acp` so the ACP-runtime path runs without a real CLI.
const FAKE_ACP_SCRIPT: &str = "read _p; \
    case \"$_p\" in \
      *acp-refuse*) printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"refusal\"}';; \
      *acp-truncate*) printf '%s\\n' '{\"type\":\"message\",\"text\":\"partial\"}';; \
      *acp-auth*) printf '%s\\n' 'not json: 401 Unauthorized invalid api key';; \
      *acp-ratelimit*) printf '%s\\n' 'not json: HTTP 429 too many requests';; \
      *acp-login*) printf '%s\\n' 'not json: Please run /login to continue';; \
      *) printf '%s\\n' '{\"type\":\"message\",\"text\":\"acp-runtime reply\"}'; \
         printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}';; \
    esac";

/// A router with out-of-band memory extraction + bounded recall (the memory
/// e2e): after each turn the extractor sub-run saves a memory, and later
/// sessions see it injected request-only by the recall plugin.
/// `AWAKEN_MODEL_MODE=memory`; the store lives under `AWAKEN_MEMORY_DIR` (a
/// fresh temp dir when unset).
pub fn build_memory_router() -> Router {
    // The extraction store's durable root: an explicit `AWAKEN_MEMORY_DIR` override
    // wins; otherwise it lives under the standard `AWAKEN_STORAGE_DIR` (so memory is
    // governed by the same durable root as every other piece of committed state and
    // survives a restart); only with neither set does it fall back to a per-process
    // temp dir (ephemeral).
    let mem_dir = std::env::var("AWAKEN_MEMORY_DIR")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("AWAKEN_STORAGE_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(awaken_memory_store::memory_scope_root)
        })
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("awaken-memory-e2e-{}", std::process::id()))
        });
    std::fs::create_dir_all(&mem_dir).expect("create memory dir");
    let host = SharedHost::new(Arc::new(MemoryProbeModel), "memory").with_memory(mem_dir);
    mount(Arc::new(host))
}

/// A router for the memory_store RESOURCE durability e2e (ADR-0038 MemoryStore
/// family): a deterministic model writes into a mounted, read-write memory store
/// and the host harvests the write back under the store's stable id. Distinct from
/// [`build_memory_router`]'s cross-session *extraction* memory — this exercises the
/// `resources[{type:"memory_store"}]` mount + write-back + `/v1/memory_stores` API.
/// `AWAKEN_MODEL_MODE=memory-resource`.
pub fn build_memory_resource_router() -> Router {
    let host = SharedHost::new(
        Arc::new(crate::models::MemoryResourceModel),
        "memory-resource",
    );
    mount(Arc::new(host))
}

/// A router for the github_repository RESOURCE e2e (ADR-0038): a deterministic model
/// reads a host-cloned repo's file and writes a change the host commits + pushes back
/// to the remote on harvest. `AWAKEN_MODEL_MODE=git-repo`.
pub fn build_git_repo_router() -> Router {
    let host = SharedHost::new(Arc::new(crate::models::GitRepoModel), "git-repo");
    mount(Arc::new(host))
}

/// A router with context compaction (the compaction e2e): a low threshold folds
/// the older transcript into a summary after a few turns. The deterministic
/// model returns a fixed summary on the `compactor` sub-run and otherwise
/// reports the compaction context it received, so an e2e can observe the folded
/// summary being injected on a later turn. `AWAKEN_MODEL_MODE=compaction`.
pub fn build_compaction_router() -> Router {
    let host = SharedHost::new(Arc::new(crate::models::CompactionModel), "compaction")
        .with_compaction(2, 1);
    mount(Arc::new(host))
}

/// A router where a session can select `runtime: "acp:*"` to run on an external
/// ACP CLI (here the fake agent), else the native echo model (R3/R4/R7).
/// `AWAKEN_MODEL_MODE=acp`.
pub fn build_acp_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            FAKE_ACP_SCRIPT.to_string(),
        ],
        vec![],
    );
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    mount(Arc::new(
        SharedHost::new(Arc::new(EchoModel), "awaken").with_acp(acp),
    ))
}

// ── Router assembly ─────────────────────────────────────────────────────────

/// Mount every public protocol adapter over one shared host. Managed Agents, AI
/// SDK, and AG-UI routes have disjoint path prefixes (`/v1/sessions...`,
/// `/v1/ai-sdk...`, `/v1/ag-ui...`) and drive the same `host`, so all three
/// protocols operate on the same threads.
fn mount(host: Arc<SharedHost>) -> Router {
    mount_with_managed(
        host.clone(),
        Arc::new(ManagedState::new(ManagedHost::new(host))),
    )
}

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
fn mount_with_managed(host: Arc<SharedHost>, managed_state: Arc<ManagedState>) -> Router {
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
    // The Files API (`/v1/files`) over the host's blob store — file resources + artifacts.
    let files = files_router(host.clone());
    let memory_stores = memory_stores_router(host.clone());
    // The skills API (`/v1/skills`) over the host's durable delivered-skill catalog.
    let skills = skills_router(host.clone());
    managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
        .merge(files)
        .merge(memory_stores)
        .merge(skills)
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
    match inference.adapter_kind {
        // The genai provider speaks the Anthropic Messages wire (native + the many
        // Anthropic-compatible gateways). Its base URL and key come from the catalog
        // endpoint and the resolved credential, never inlined by the Managed wire.
        "anthropic" => {
            let base_url = inference
                .base_url
                .clone()
                .ok_or(ResolvedExecutorError::MissingBaseUrl("anthropic"))?;
            let credential = inference
                .credential
                .as_ref()
                .ok_or(ResolvedExecutorError::MissingCredential)?;
            Ok(Arc::new(GenaiExecutor::anthropic_compatible(
                base_url,
                credential.expose_secret(),
            )))
        }
        other => Err(ResolvedExecutorError::UnsupportedAdapter(other.to_string())),
    }
}

/// Build the full server router for a resolved run: turn the [`ResolvedInference`]
/// into the host's model executor and mount the protocol adapters over it. The
/// router's model ref is the resolved triple's model id, so a run started here calls
/// the exact model the management plane bound. This is the composition-root end of
/// the config → resolve → run chain (ADR-0043).
pub fn build_resolved_router(
    inference: &ResolvedInference,
) -> Result<Router, ResolvedExecutorError> {
    let executor = executor_from_resolved(inference)?;
    Ok(build_router(executor, inference.triple.model_id.clone()))
}

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    mount(Arc::new(SharedHost::new(llm, model_ref)))
}

/// A server backed by a **live** Anthropic-compatible model, configured from the
/// environment: `ANTHROPIC_API_KEY` (or `KIMI_API_KEY`), `ANTHROPIC_BASE_URL` (or
/// `KIMI_BASE_URL`), `ANTHROPIC_MODEL` (or `KIMI_MODEL`). This is the same
/// `RedactedString` → `GenaiExecutor` seam the resolver's `executor_from_resolved`
/// uses (verified equivalent by `tests/resolved_run.rs`), exposed as a server mode
/// so the TypeScript e2e can drive a real turn through the managed / ai-sdk / a2a
/// adapters. Panics if no API key is set, so a misconfigured run fails loudly.
pub fn build_real_router() -> Router {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());
    let executor = GenaiExecutor::anthropic_compatible(base, key);
    build_router(Arc::new(executor), model)
}

/// A server backed by **Gemini on Vertex AI**, authenticated by an OAuth2 Bearer
/// token (ADR-0043 Phase 3 multi-flavor + OAuth). The token is refreshed through
/// the credential domain's OAuth helper: `GEMINI_ACCESS_TOKEN` if set, else
/// `gcloud auth print-access-token` (which holds the long-lived Google grant).
/// Config from the environment: `GEMINI_PROJECT` (required), `GEMINI_LOCATION`
/// (default `global`), `GEMINI_MODEL` (default `gemini-2.5-flash`). Exposed as a
/// server mode so the TypeScript e2e can drive a real Gemini turn — proving the
/// OAuth + Gemini path through the managed / ai-sdk adapters.
pub async fn build_real_gemini_router() -> Router {
    use awaken_credential_vault::{CommandTokenSource, TokenSource};

    let project = std::env::var("GEMINI_PROJECT")
        .expect("set GEMINI_PROJECT for AWAKEN_MODEL_MODE=real-gemini");
    let location = std::env::var("GEMINI_LOCATION").unwrap_or_else(|_| "global".to_string());
    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
    let token = match std::env::var("GEMINI_ACCESS_TOKEN") {
        Ok(token) if !token.is_empty() => token,
        _ => CommandTokenSource::gcloud()
            .access_token()
            .await
            .expect("refresh a Google OAuth2 token via gcloud")
            .expose_secret()
            .to_string(),
    };
    let executor = GenaiExecutor::vertex_gemini(project, location, token);
    build_router(Arc::new(executor), model)
}

/// A live-model server whose executor is built **through the resolver**
/// (ADR-0043): it authors an in-memory catalog + enters a credential from the
/// environment, then `resolve_inference` + `executor_from_resolved` produce the
/// host executor — the same config → resolve → run path a managed run takes, rather
/// than constructing the provider directly (as `build_real_router` does). Exposed
/// so a session e2e exercises the resolver end to end against a real model. Env:
/// `ANTHROPIC_API_KEY`/`KIMI_API_KEY` (+ `*_BASE_URL`, `*_MODEL`).
pub async fn build_resolved_real_router() -> Router {
    use std::collections::HashMap;

    use awaken_agent_contract::RedactedString;
    use awaken_config_resolver::resolve_inference;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialBinding, CredentialCreateParams, CredentialKind, CredentialSource,
        CredentialSourceId, InMemorySecretStore,
    };
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };

    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real-resolved");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());

    // Author the catalog: one provider + endpoint + offering for `model`.
    let catalog_repo = InMemoryCatalogRepo::new();
    catalog_repo
        .put_provider(Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        })
        .await
        .expect("put provider");
    catalog_repo
        .put_endpoint(ProtocolEndpoint {
            id: ProtocolEndpointId::new("ep1"),
            provider_id: ProviderId::new("anthropic"),
            flavor: ModelApiCompat::AnthropicMessages,
            base_url: Some(base),
            timeout_secs: 300,
            display_name: "prod".into(),
            version: 1,
        })
        .await
        .expect("put endpoint");
    catalog_repo
        .put_offering(Offering {
            model_id: model.clone(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        })
        .await
        .expect("put offering");

    // Enter the credential (secret-in), then resolve the inference against the
    // authored catalog and build the executor from the resolved value.
    let secrets = InMemorySecretStore::new();
    let cred_repo = InMemoryCredentialRepo::new();
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(key)),
        },
        &secrets,
        &cred_repo,
    )
    .await
    .expect("enter credential");
    let catalog = catalog_repo.snapshot().await.expect("catalog snapshot");
    let row = cred_repo.get(&source.id).await.expect("credential row");
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(row.id.0.clone(), row);
    let inference = resolve_inference(
        &catalog,
        &model,
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(source.id.0.clone()),
        },
        &sources,
        &secrets,
    )
    .await
    .expect("resolve inference");
    let executor = executor_from_resolved(&inference).expect("build executor from resolved");
    build_router(executor, inference.triple.model_id.clone())
}

/// Build the server router offering `skills` on every thread (ADR-0036): the whole
/// set is fronted by the single `Skill` tool, whose catalog lists them and whose
/// invocation returns the activated skill's instructions.
pub fn build_router_with_skills(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    skills: Vec<SkillSpec>,
) -> Router {
    mount(Arc::new(
        SharedHost::new(llm, model_ref).with_skills(skills),
    ))
}

/// A router whose outcomes are graded by a judge sub-agent (`judge_agent_id`) run
/// through the kernel, rather than the deterministic keyword grader.
pub fn build_graded_router(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    judge_agent_id: impl Into<String>,
) -> Router {
    mount(Arc::new(
        SharedHost::new(llm, model_ref).with_judge(judge_agent_id),
    ))
}

/// The default deterministic router (echo model) — the CI / e2e server.
pub fn build_echo_router() -> Router {
    build_router(Arc::new(EchoModel), "echo-model")
}

/// A router whose model reports the media it received (the multimodal e2e): every
/// protocol adapter must carry an image block through to the model for the probe
/// reply to name its media type.
pub fn build_vision_router() -> Router {
    build_router(Arc::new(VisionProbeModel), "vision-probe")
}

/// A router with a client-executed tool `submit_answer` (the custom-tool e2e).
pub fn build_custom_router() -> Router {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let host = SharedHost::new(Arc::new(CustomToolModel), "custom").with_client_tools(client_tools);
    mount(Arc::new(host))
}

/// A router whose agent can delegate to a `researcher` sub-agent via `agent_run`
/// (the multi-agent e2e). `ghost` is deliberately absent from the roster so the
/// fail-closed path can be exercised.
pub fn build_delegation_router() -> Router {
    let roster = HashSet::from(["researcher".to_string()]);
    let host = SharedHost::new(Arc::new(DelegatingModel), "delegate").with_delegates(roster);
    mount(Arc::new(host))
}

/// A router whose agent activates the tool state machine (the state-machine e2e).
/// The machine defines `glob` as a single transition out of the initial state, so
/// the driving model's first `glob` advances it (emitting a context message) and
/// the second is a precondition violation the gate denies.
pub fn build_statemachine_router() -> Router {
    let machine = serde_json::json!({
        "machines": [{
            "name": "walk",
            "initial": "s0",
            "terminal": ["s1"],
            "transitions": [{
                "on": "glob(pattern ~ \"*\")",
                "from": ["s0"],
                "to": "s1",
                "emit": { "target": "system", "content": "advanced to s1", "cooldown_turns": 0 },
                "on_violation": { "action": "deny", "reason": "glob is only allowed from the start state" }
            }]
        }]
    });
    let host =
        SharedHost::new(Arc::new(StateMachineModel), "statemachine").with_state_machine(machine);
    mount(Arc::new(host))
}

/// A richer tool state machine (coverage): a per-key machine (`key`/`key_normalizer`)
/// whose transitions gate on the tool *result* (`when`) rather than just its args —
/// exercising the result matchers (status + content) and the key template. The
/// driving model calls `glob` twice with the same pattern: the first advances
/// `s0 -> s1` on a `success` result (its emit fires), the second advances `s1 -> s2`
/// (terminal) on `any` result (a second emit). No violation — both calls advance.
pub fn build_statemachine_rich_router() -> Router {
    let machine = serde_json::json!({
        "machines": [{
            "name": "keyed",
            "key": "${pattern}",
            "key_normalizer": "lowercase",
            "initial": "s0",
            "terminal": ["s2"],
            "transitions": [
                // Decoy transitions across the full pattern grammar (regex tool
                // name, =~ / != / !~ field ops, nested paths, multi-field AND):
                // parsed at machine load and EVALUATED against every call from
                // s0/s1, but never taken (each has a non-matching guard), so the
                // original advance order below is untouched. They keep the whole
                // pattern engine hot under e2e, not just the glob arm.
                {
                    "on": "/re(ad|grep)/(path =~ \"(?i)\\.rs$\")",
                    "from": ["s0"],
                    "to": "s2",
                    "on_violation": { "action": "warn", "reason": "decoy regex-tool transition" }
                },
                {
                    "on": "glob(pattern != \"never-this-literal\", pattern !~ \"zzz-*\", pattern =~ \"^zzz-only\")",
                    "from": ["s0", "s1"],
                    "to": "s2",
                    "on_violation": { "action": "warn", "reason": "decoy multi-field transition" }
                },
                {
                    "on": "glob(options.nested[*].flag = \"on\")",
                    "from": ["s0", "s1"],
                    "to": "s2",
                    "on_violation": { "action": "warn", "reason": "decoy nested-path transition" }
                },
                {
                    "on": "mcp__calc__*(a != \"1\")",
                    "from": ["s0"],
                    "to": "s2",
                    "on_violation": { "action": "warn", "reason": "decoy mcp-glob transition" }
                },
                {
                    "on": "glob(pattern ~ \"*\")",
                    "from": ["s0"],
                    "to": "s1",
                    "when": "success",
                    "emit": { "target": "system", "content": "first glob succeeded", "cooldown_turns": 0 }
                },
                {
                    "on": "glob(pattern ~ \"*\")",
                    "from": ["s1"],
                    "to": "s2",
                    "when": { "status": "success", "content": "*" },
                    "emit": { "target": "system", "content": "second glob advanced", "cooldown_turns": 0 }
                }
            ]
        }]
    });
    let host = SharedHost::new(Arc::new(StateMachineModel), "statemachine-rich")
        .with_state_machine(machine);
    mount(Arc::new(host))
}

/// A router with the config data plane (`/v1/config/agents/*`) over an in-memory
/// SQLite config store, plus the protocol adapters. A session for a *published*
/// agent runs with that agent's installed config (slice A); the model echoes the
/// agent's instructions so an e2e can assert the published config took effect.
pub fn build_config_router() -> Router {
    let registry = Arc::new(
        awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
    );
    let tools = advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
    let service = Arc::new(ConfigService::new(registry, tools));
    let host = SharedHost::new(Arc::new(InstructionEchoModel), "config")
        .with_config_service(service.clone());
    mount(Arc::new(host)).merge(config_router(service))
}

/// The live credential-validation probe port (ADR-0043), backed by provider-genai.
/// This is the only place the model SDK is named for validation — the admin CRUD
/// crate depends on the `CredentialProbe` trait, not on genai.
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
    projects: Arc<dyn awaken_admin_config_api::ProjectStore>,
    /// Durable home for the Managed session aggregate (its own `sessions.db`), so a
    /// rehydrated session reports its real config across a restart / peer process.
    sessions: Arc<dyn awaken_protocol_managed::ManagedSessionRepository>,
}

/// Ephemeral management stores: everything in process memory (dev / e2e default).
fn in_memory_management_stores() -> ManagementStores {
    ManagementStores {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        projects: Arc::new(awaken_admin_config_api::InMemoryProjectStore::new()),
        sessions: Arc::new(awaken_protocol_managed::InMemorySessionRepository::default()),
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
        projects: admin,
        // A separate `sessions.db` (not a table in admin.db): a live session
        // instance is a different aggregate from the agent/MCP definitions admin.db
        // holds (ADR-0039 one-repository-per-aggregate).
        sessions: Arc::new(
            awaken_runtime_host::SqliteManagedSessionRepository::open(&db("sessions.db"))
                .expect("open sessions.db under AWAKEN_MGMT_DIR"),
        ),
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

/// The AEAD key for the durable management plane, from `AWAKEN_MGMT_SEAL_KEY`
/// (64 hex characters = 32 bytes). Fails loudly when unset or malformed: a
/// durable store sealed under an ephemeral random key would look healthy until
/// the first restart, then every persisted secret would be unopenable.
fn mgmt_seal_key_from_env() -> [u8; 32] {
    let hex = std::env::var("AWAKEN_MGMT_SEAL_KEY").unwrap_or_else(|_| {
        panic!(
            "AWAKEN_MGMT_DIR is set but AWAKEN_MGMT_SEAL_KEY is not. A durable \
             management store needs a stable AEAD key (64 hex characters = 32 bytes); \
             sealing under an ephemeral key would brick every restart."
        )
    });
    parse_seal_key(&hex).unwrap_or_else(|reason| {
        panic!("AWAKEN_MGMT_SEAL_KEY is malformed: {reason}. Provide 64 hex characters (a 32-byte key).")
    })
}

/// Serve the management plane (admin + vaults + sessions) with **persistence
/// selected from the environment** (mirrors `AWAKEN_STORE` / `AWAKEN_INGRESS`):
///
/// - `AWAKEN_MGMT_DIR` unset — in-memory stores, exactly the previous behavior.
/// - `AWAKEN_MGMT_DIR=<dir>` — SQLite-backed stores under `<dir>`
///   (`catalog.db` / `credential.db` / `admin.db`), with secrets AEAD-sealed
///   under `AWAKEN_MGMT_SEAL_KEY` (**required** then: 64 hex characters = a
///   32-byte key; unset or malformed panics rather than sealing under a key
///   that cannot survive a restart).
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
pub fn build_management_router() -> Router {
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
            management_router_over(
                durable_management_stores(std::path::Path::new(&dir), &key),
                iam,
            )
        }
        Err(_) => management_router_over(in_memory_management_stores(), iam),
    }
}

/// [`build_management_router`] with explicit persistence inputs (no environment
/// read): the durable management plane over `dir`, sealing secrets under `key`.
/// Exposed so a restart test can rebuild a router over one directory across
/// simulated process lifetimes without racing on process-global env vars.
/// No IAM guard — the open (default) management plane.
pub fn build_durable_management_router(dir: &std::path::Path, key: &[u8; 32]) -> Router {
    management_router_over(durable_management_stores(dir, key), None)
}

/// [`build_durable_management_router`] with the embedded IAM guard enabled —
/// the env-free equivalent of `AWAKEN_MGMT_IAM=embedded`. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint
/// further workspace tokens against the same policy state.
pub fn build_secured_management_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let router = management_router_over(durable_management_stores(dir, key), Some(iam.clone()));
    (router, iam)
}

/// Mount the management plane over an explicit store set, optionally gated by
/// the embedded IAM guard (`iam`). The guard wraps ONLY the admin + vault
/// routers: the Managed session surface keeps its own axis and P1 does not
/// gate it (ADR-0043).
fn management_router_over(stores: ManagementStores, iam: Option<Arc<ManagementAuthz>>) -> Router {
    let ManagementStores {
        catalog,
        credentials,
        secrets,
        profiles,
        mcp: mcp_store,
        projects,
        sessions,
    } = stores;
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
        projects: projects.clone(),
        // Per-agent resource bindings (ADR-0038). Ephemeral in-memory for now; the
        // durable SqliteAdminStore also implements `ResourceStore` for a later wire.
        resources: Arc::new(awaken_admin_config_api::InMemoryResourceStore::new()),
        // The live credential probe is backed by provider-genai here — the only
        // place the model SDK is named; the admin CRUD crate stays SDK-free.
        probe: Some(Arc::new(GenaiProbe)),
    });
    let vault_state = Arc::new(
        awaken_protocol_managed::VaultState::new(secrets.clone(), credentials.clone())
            // The live MCP probe is backed by ext-mcp here — the only place the
            // MCP client is named for validation; the adapter crate stays
            // wire-client-free (mirrors the GenaiProbe pattern above).
            .with_probe(Arc::new(ExtMcpProbe)),
    );
    let vaults = awaken_protocol_managed::vault_router(vault_state.clone());

    // The IAM guard (when enabled) wraps the admin + vault routers only. An
    // axum layer binds to the routes present when it is applied, so merging
    // the guarded sub-router later leaves every other surface untouched. The
    // token-management routes exist ONLY under the guard (they authorize
    // against the same embedded IAM the guard authenticates with), and they
    // are merged before the layer so the guard authenticates them first.
    let mut mgmt = admin.merge(vaults);
    if let Some(iam) = iam {
        mgmt = mgmt.merge(crate::authz::token_router(iam.clone()));
        mgmt = mgmt.layer(axum::middleware::from_fn_with_state(
            iam,
            crate::authz::management_guard,
        ));
    }

    // The MCP-driving deterministic model, so an e2e can hold a real multi-turn
    // conversation through ext-mcp (`add a b` → mcp__calc__add → `result: …`);
    // non-`add` turns still echo, preserving the prior expectations.
    let host = Arc::new(SharedHost::new(Arc::new(McpToolModel), "management"));
    let managed_state = Arc::new(
        ManagedState::new(ManagedHost::new(host.clone()).with_mcp(
            credentials,
            secrets,
            mcp_store,
            projects.clone(),
        ))
        .with_vaults(vault_state)
        .with_session_repo(sessions),
    );
    // The project ingress (ADR-0042 amendment): the SAME session surface
    // reachable under `/projects/{id}` — the stock SDK reaches it by baseURL
    // alone, no wire change. Implemented as a prefix-stripping proxy rather
    // than `nest` so the inner handlers' `Path<…>` extractors see exactly the
    // params they declare (a nest path param would leak into every route).
    // The proxy 404s an unauthored project and stamps the ProjectScope
    // extension `create_session` consumes.
    let project_sessions = Router::new().route(
        "/projects/:project_id/*rest",
        axum::routing::any(project_ingress).with_state((
            projects,
            awaken_protocol_managed::router(managed_state.clone()),
        )),
    );
    mount_with_managed(host, managed_state)
        .merge(mgmt)
        .merge(project_sessions)
}

/// Resolve the `/projects/{id}` ingress segment against the authored projects:
/// 404 (managed envelope) when unauthored; otherwise strip the prefix, stamp
/// [`awaken_protocol_managed::ProjectScope`] into the request extensions, and
/// forward to the shared session router — the same handlers as the bare
/// surface, so wire behavior is identical. The segment is ADDRESSING only —
/// authority still flows from the API key (ADR-0042 amendment); the sessions
/// surface keeps its own authz axis (P1 does not gate it).
async fn project_ingress(
    axum::extract::State((projects, sessions)): axum::extract::State<(
        Arc<dyn awaken_admin_config_api::ProjectStore>,
        Router,
    )>,
    axum::extract::Path((project_id, rest)): axum::extract::Path<(String, String)>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use tower::ServiceExt;
    let Some(project) = projects.get_project(&project_id) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(awaken_protocol_managed::dto::ErrorResponse::new(
                "not_found_error",
                format!("project `{project_id}` not found"),
            )),
        )
            .into_response();
    };
    // Rebuild the request against the bare path the inner router serves (keep
    // method/headers/body/query, drop the OUTER router's routing extensions —
    // stale `UrlParams` would otherwise stack onto the inner match and break
    // the inner handlers' `Path<…>` extractors).
    let stripped = match request.uri().query() {
        Some(query) => format!("/{rest}?{query}"),
        None => format!("/{rest}"),
    };
    let (parts, body) = request.into_parts();
    let mut forwarded = axum::extract::Request::builder()
        .method(parts.method)
        .uri(stripped)
        .body(body)
        .expect("a stripped project path re-parses as a URI");
    *forwarded.headers_mut() = parts.headers;
    forwarded
        .extensions_mut()
        .insert(awaken_protocol_managed::ProjectScope(project_id.clone()));
    // The tenancy the session-axis guard authorizes against: the project's OWN
    // workspace (so the scope fence is correct), plus the project id. Additive —
    // inert unless a guard layer wraps this router (the standalone does).
    forwarded
        .extensions_mut()
        .insert(awaken_authz_enforce::RequestTenancy {
            workspace_id: project.workspace_id.clone(),
            project_id: Some(project_id),
        });
    match sessions.oneshot(forwarded).await {
        Ok(response) => response,
        Err(err) => match err {},
    }
}

/// A tool gate that defers every tool call as a committed `ScheduledAction`
/// (ADR-0020, slice E): instead of running inline or parking for a human, the call
/// is scheduled, keyed by its call id, and the durable dispatch worker performs it
/// out of band. In direct mode a scheduled run would park; under
/// `AWAKEN_INGRESS=durable` the worker's scheduled-action loop performs it and the
/// run completes autonomously.
struct ScheduleGate;

#[async_trait::async_trait]
impl awaken_runtime_contract::permission::ToolGateHook for ScheduleGate {
    async fn gate(
        &self,
        ctx: &awaken_runtime_contract::permission::PermissionContext,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> awaken_runtime_contract::permission::GateOutcome {
        awaken_runtime_contract::permission::GateOutcome::Schedule {
            correlation_id: format!("sched-{}", ctx.call_id),
            action_kind: None,
        }
    }
}

/// A router whose tool gate defers every tool call as a `ScheduledAction`
/// (ADR-0020, slice E). Drive it with `AWAKEN_INGRESS=durable` so the dispatch
/// worker performs the deferred actions out of band: the probe model's
/// write→read tool calls are each scheduled and auto-performed, so the run
/// completes without any human confirmation.
pub fn build_schedule_router() -> Router {
    let host = SharedHost::new(Arc::new(ProbeModel), "schedule")
        .with_gate_override(Arc::new(ScheduleGate));
    mount(Arc::new(host))
}

/// A router that routes `agent_run` for `researcher` to a REMOTE A2A agent at
/// `AWAKEN_REMOTE_AGENT_URL` (instead of a local sub-run). Exercises the remote
/// delegation path — `message:send` → poll `get_task` → result — across a real A2A
/// hop to a peer server. The peer echoes, so the delegate result round-trips back.
pub fn build_remote_delegation_router() -> Router {
    let url = std::env::var("AWAKEN_REMOTE_AGENT_URL")
        .expect("AWAKEN_REMOTE_AGENT_URL must be set for delegate-remote mode");
    let host = SharedHost::new(Arc::new(DelegatingModel), "delegate-remote")
        .with_remote_a2a("researcher", Arc::new(HttpTransport::new(url)));
    mount(Arc::new(host))
}

/// A deterministic model for the skills e2e (ADR-0036). On the user turn it calls
/// `list_skills` to discover the offered skills; given the catalog it activates the
/// `greet` skill via the `Skill` tool; given the activation instructions it replies
/// with them — so an e2e can assert discover → activate → use end to end. Stateless.
pub struct SkillDrivingModel;

#[async_trait::async_trait]
impl LlmExecutor for SkillDrivingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request.messages.last().expect("a message");
        // A tool result carries its text in nested blocks, which `block_text` skips;
        // read those too so the catalog (a tool result) is visible to the model.
        let last_text: String = last
            .content
            .iter()
            .flat_map(|b| match b {
                ContentBlock::Text { text } => vec![text.clone()],
                ContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(|inner| match inner {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            })
            .collect::<Vec<_>>()
            .join("");
        let output = match last.role {
            ChatRole::User => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "l".into(),
                tool_id: "list_skills".into(),
                arguments: serde_json::json!({}),
            }]),
            ChatRole::Tool if last_text.contains("\"skills\"") => {
                // The catalog came back — activate the offered `greet` skill.
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s".into(),
                    tool_id: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "greet" }),
                }])
            }
            ChatRole::Tool => AssistantOutput::text(format!("USED-SKILL: {last_text}")),
            _ => AssistantOutput::text("hmm"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A router offering skills on every thread (ADR-0036), driven by a model that
/// discovers, activates, and uses one. The whole skill set is fronted by the single
/// `Skill` tool plus `list_skills`; activation returns the skill's instructions.
pub fn build_skills_router() -> Router {
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING-FROM-SKILL");
    let review = SkillSpec::new("review", "Review", "review code", "REVIEW-BODY");
    build_router_with_skills(Arc::new(SkillDrivingModel), "skills", vec![greet, review])
}

/// A router whose delivered skills come from a DURABLE catalog (`/v1/skills`) instead
/// of static config, rooted under `AWAKEN_STORAGE_DIR` (a per-process temp dir when
/// unset). A skill posted to `/v1/skills` is offered on every thread and survives a
/// restart. The `SkillDrivingModel` discovers → activates `greet` → replies with its
/// body, so an e2e proves a durably-configured skill reaches the model across a
/// restart. `AWAKEN_MODEL_MODE=skills-durable`.
pub fn build_skills_durable_router() -> Router {
    let dir = std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("awaken-skills-durable-{}", std::process::id()))
        })
        .join("skills_catalog");
    mount(Arc::new(
        SharedHost::new(Arc::new(SkillDrivingModel), "skills-durable").with_skill_store(dir),
    ))
}
