//! Test-only scenario host: the deterministic mock models and the `build_*_router`
//! scenario assemblies the e2e harness + integration tests drive. Extracted from
//! `awaken-coordinator` so the product crate carries zero mocks. It reuses the
//! product crate's now-`pub` data-plane assembly helpers (`mount` /
//! `mount_with_managed`) and production executors via `awaken_coordinator::`.

mod acp_gateway;
mod acp_scenarios;
mod attempt_credential;
mod composition;
mod delegation;
mod deployment;
mod distributed_control;
mod dream;
mod model_publication;
mod model_routing;
mod models;
mod worker;
pub use crate::models::*;
pub use acp_gateway::build_acp_gateway_router;
use acp_scenarios::FAKE_ACP_CLI;
pub use acp_scenarios::{
    build_acp_container_router, build_acp_control_router, build_acp_jsonrpc_router,
    build_acp_managed_mcp_router, build_acp_permission_router, build_acp_real_mcp_router,
    build_acp_relaunch_failure_router, build_acp_router, build_acp_sandboxed_router,
    build_acp_sandboxed_router_with_deployment,
};
pub use composition::build_unscoped_resource_router;
pub use delegation::build_delegation_router;
pub use deployment::scenario_deployment;
pub use distributed_control::build_distributed_control_router;
pub use distributed_control::build_distributed_provider_router;
pub use dream::{build_dream_router, build_dream_router_and_host};
pub use model_routing::{build_model_route_router, scenario_model};
pub use worker::run_echo_worker;

mod scenario_shell;
use composition::{
    fixed_host_backend_publication, fixed_host_backend_publication_with_acp_mcp,
    fixed_host_backend_publication_with_mcp, mount, mount_with_agent_source,
    mount_with_environments, mount_with_environments_and_agent_source,
    mount_with_host_backend_publication, mount_with_memory_publication,
};
use deployment::{resource_host, resource_host_with_deployment, scenario_storage_dir};
use scenario_shell::{scenario_argv, scenario_host_acp_cli, scenario_shell_argv};

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::extract_text;
use awaken_agent_contract::agent::message::Role;
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate, ToolKind};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use axum::Router;

// This scenario composition depends on each authoritative owner directly.
pub use awaken_config_service::{ConfigService, capabilities_router, config_router};
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_protocol_managed::{
    default_models, files_router, memory_stores_router, models_router, skills_router,
};
pub use awaken_run_ingress_http::durable_ops_router;
pub use awaken_runtime_host::{
    ExtMcpProbe, HostResume, RunApplicationHost, SharedHost, ThreadEvent, ThreadEventHub,
    VaultRefresher, advertised_tools,
};
pub use awaken_sandbox_local::content_fingerprint;

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

/// One tool-free ACP stand-in that can act as either Outcome Worker or Judge.
/// Its prompt classification makes the Worker/Grader backend matrix observable
/// without provider credentials.
const FAKE_OUTCOME_ACP_SCRIPT: &str = "read _p; \
    case \"$_p\" in \
      *'Evaluate this Outcome input'*'FINAL answer'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"{\\\"result\\\":\\\"satisfied\\\",\\\"explanation\\\":\\\"ACP judge accepted evidence\\\"}\"}';; \
      *'Evaluate this Outcome input'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"{\\\"result\\\":\\\"needs_revision\\\",\\\"explanation\\\":\\\"ACP judge requests FINAL\\\"}\"}';; \
      *'Revise the deliverable'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"FINAL answer from ACP worker\"}';; \
      *'iteration limit was reached'*) \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"ACP worker acknowledged remaining feedback\"}';; \
      *) printf '%s\\n' '{\"type\":\"message\",\"text\":\"a rough draft from ACP worker\"}';; \
    esac; \
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'; \
    trap 'exit 0' TERM INT; \
    while :; do sleep 1; done";

/// Managed Outcome backend matrix: each Session independently selects a Native
/// or ACP Worker, while `AWAKEN_OUTCOME_JUDGE_RUNTIME` pins the Judge snapshot.
pub fn build_outcome_matrix_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        scenario_shell_argv(FAKE_OUTCOME_ACP_SCRIPT),
        vec![],
    );
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    let (model, model_ref) = scenario_model(Arc::new(ReviseModel), "outcome-native");
    let judge_backend = match std::env::var("AWAKEN_OUTCOME_JUDGE_RUNTIME").as_deref() {
        Ok("acp") => "acp:claude",
        _ => "default",
    };
    let judge = ExecutableAgentSnapshot::builder("outcome-judge")
        .instructions("Return only the required strict Outcome Grade JSON.")
        .model(ModelBinding::new(
            "outcome-test",
            model_ref.clone(),
            judge_backend,
        ))
        .max_steps(2)
        .build();
    mount_with_host_backend_publication(
        resource_host(model, model_ref)
            .with_acp(acp)
            .with_judge_snapshot(judge),
        "acp-agent",
        "acp:claude",
    )
}

/// A router with out-of-band memory extraction + bounded recall (the memory
/// e2e): after each turn the extractor sub-run saves a memory, and later
/// sessions see it injected request-only by the recall plugin.
/// `AWAKEN_MODEL_MODE=memory`; the caller must create and attach a governed
/// MemoryStore resource to each participating Session.
pub fn build_memory_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(MemoryProbeModel), "memory");
    let host = resource_host(model, model_ref);
    mount_with_memory_publication(host, "memory", Vec::new())
}

/// A router for the memory_store RESOURCE durability e2e (ADR-0038 MemoryStore
/// family): a deterministic model writes into a mounted, read-write memory store
/// and the host harvests the write back under the store's stable id. Distinct from
/// [`build_memory_router`]'s cross-session *extraction* memory — this exercises the
/// `resources[{type:"memory_store"}]` mount + write-back + `/v1/memory_stores` API.
/// `AWAKEN_MODEL_MODE=memory-resource`.
pub fn build_memory_resource_router() -> Router {
    let (model, model_ref) = scenario_model(
        Arc::new(crate::models::MemoryResourceModel),
        "memory-resource",
    );
    let host = resource_host(model, model_ref);
    mount(Arc::new(host))
}

/// A router for the github_repository RESOURCE e2e (ADR-0038): a deterministic model
/// reads a host-cloned repo's file and writes a change the host commits + pushes back
/// to the remote on harvest. `AWAKEN_MODEL_MODE=git-repo`.
pub fn build_git_repo_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::GitRepoModel), "git-repo");
    let host = resource_host(model, model_ref);
    mount(Arc::new(host))
}

/// The combined-chain router (native full-chain e2e): one session configures a
/// memory_store + github_repository resource, is offered a skill, and has out-of-band
/// memory extraction — so a single conversation exercises skill use → memory-store
/// write-back → git commit/push → output-artifact harvest end to end on the native
/// backend. Driven over the real wire by the `fullChain` behavior (this in-process
/// `EchoModel` is only the non-http fallback, never run by the e2e).
/// `AWAKEN_MODEL_MODE=full-chain` with `AWAKEN_MODEL_SOURCE=http`.
pub fn build_full_chain_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "full-chain");
    let host = resource_host(model, model_ref).with_skill_store(scenario_skill_store_dir());
    mount_with_memory_publication(
        host,
        "full-chain",
        vec![awaken_agent_contract::AgentSkillBinding::custom("greet")],
    )
}

/// Resolve the durable SkillStore once at the scenario composition edge. Both the
/// Skill HTTP adapter and agent-authored harvest then operate on the same canonical
/// repository; neither the resource store nor the runtime receives authorization
/// concepts.
fn scenario_skill_store_dir() -> std::path::PathBuf {
    scenario_storage_dir()
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("awaken-skills-durable-{}", std::process::id()))
        })
        .join("skills_catalog")
}

/// Give scenario compositions the same durable resource catalog the production
/// management composition injects. It owns definition/configuration/lifecycle only;
/// authentication and policy remain outside this resource-plane adapter.
fn scenario_resource_catalog() -> Arc<dyn awaken_resource_contract::ResourceCatalog> {
    let root = scenario_storage_dir();
    let Some(root) = root else {
        return Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open ephemeral scenario resource catalog"),
        );
    };
    std::fs::create_dir_all(&root).expect("create scenario resource registry directory");
    Arc::new(
        awaken_resource_store::SqliteResourceStore::open(root.join("resources.db"))
            .expect("open durable scenario resource catalog"),
    )
}

/// A router with context compaction (the compaction e2e): a low threshold folds
/// the older transcript into a summary after a few turns. The deterministic
/// model returns a fixed summary on the `compactor` sub-run and otherwise
/// reports the compaction context it received, so an e2e can observe the folded
/// summary being injected on a later turn. `AWAKEN_MODEL_MODE=compaction`.
pub fn build_compaction_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::CompactionModel), "compaction");
    // Compaction changes run behavior, not deployment ownership. Reuse the
    // canonical scenario composition so durable Session/resource identity is
    // reconstructed from the same storage root after restart.
    let host = resource_host(model, model_ref);
    // Token-aware when the model's context window is configured
    // (`AWAKEN_COMPACT_MAX_TOKENS`): fold at `AWAKEN_COMPACT_TRIGGER_RATIO` of it
    // (default 0.8), keeping `AWAKEN_COMPACT_KEEP_LAST` (default 2) messages. This
    // is the real-model path (a small window trips compaction on large input).
    // Without it, the deterministic message-count trigger (fold after 2 messages).
    let env_u = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
    // Harness-local window wiring: an explicit override (`AWAKEN_COMPACT_MAX_TOKENS`) wins,
    // else the model's published context window (`AWAKEN_MODEL_CONTEXT_WINDOW` — this harness's
    // projection of the catalog's `ModelSpec.context_window`). Production derives the effective
    // trigger at publish (config-service `apply_compaction`); this driver sets it directly.
    let window =
        env_u("AWAKEN_COMPACT_MAX_TOKENS").or_else(|| env_u("AWAKEN_MODEL_CONTEXT_WINDOW"));
    let host = match window {
        Some(max_tokens) => {
            let ratio = std::env::var("AWAKEN_COMPACT_TRIGGER_RATIO")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.8);
            let keep_last = env_u("AWAKEN_COMPACT_KEEP_LAST").unwrap_or(2) as usize;
            host.with_compaction_tokens(max_tokens, ratio, keep_last)
        }
        None => host.with_compaction(2, 1),
    };
    mount(Arc::new(host))
}

/// A router whose model fails a turn on the `BOOM` trigger (the session-error
/// e2e): the failed turn surfaces an internal `RunError` (HTTP `api_error`) and
/// commits a `session.error` event, while other turns echo — proving the session
/// stays usable after a failure. `AWAKEN_MODEL_MODE=error`.
pub fn build_error_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::ErrorModel), "error");
    let host = resource_host(model, model_ref);
    mount(Arc::new(host))
}

/// A router that mounts `/v1/environments` and shares its state with the session
/// surface, so a session created on a **self-hosted** environment is dispatched as a
/// `session` work item. The self-hosted worker e2e polls the queue, claims that
/// work, then drives the session — whose agent awaits on a client-executed
/// `submit_answer` tool — by **running the tool and posting the result back**, the
/// way a self-hosted worker executes the session's tool calls. Heartbeats the lease
/// and stops the work on completion. `AWAKEN_MODEL_MODE=worker`.
pub fn build_worker_router() -> Router {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let (model, model_ref) = scenario_model(Arc::new(CustomToolModel), "worker");
    let host = Arc::new(resource_host(model, model_ref).with_client_tools(client_tools));
    mount_with_environments(host)
}

/// Echo-model composition with the official Environment API, exact sandbox-policy
/// store, Resource Catalog, Managed Sessions, and all protocol adapters. It exists
/// solely to drive the orthogonal configuration matrix without application-auth
/// concerns obscuring the baseline/provisioning behavior under test.
pub fn build_environment_matrix_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "environment-matrix");
    mount_with_environments(Arc::new(resource_host(model, model_ref)))
}

/// A fake ACP agent speaking the OFFICIAL JSON-RPC 2.0 wire (shell builtins only,
/// so it survives `env_clear`): answer `initialize` (id 1) and `session/new`
/// (id 2), then on `session/prompt` (id 3) stream a tool call, its completed
/// result, and one agent-message chunk as `session/update`s, and reply with
/// `stopReason:"end_turn"`. Stands in for a real `claude --acp` to exercise the
/// [`awaken_run_executor_acp::Codec::Acp`] driver, including the tool-call/result
/// projection (a `tool_call` + a terminal `tool_call_update` with content).
const FAKE_ACP_JSONRPC_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"c1\",\"title\":\"read\",\"rawInput\":{\"path\":\"a.txt\"}}}}'; \
          payload=$(/usr/bin/head -c 100001 /dev/zero | /usr/bin/tr '\\000' x); \
          printf '%s%s%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"c1\",\"status\":\"completed\",\"content\":[{\"type\":\"content\",\"content\":{\"type\":\"text\",\"text\":\"file body ' \"$payload\" '\"}}]}}}'; \
          spill=''; attempts=0; \
          while [ \"$attempts\" -lt 100 ] && [ -z \"$spill\" ]; do \
            for candidate in \"$AWAKEN_PROJECT_DIR\"/.awaken/tool-results/*.txt; do \
              if [ -f \"$candidate\" ]; then spill=\"$candidate\"; break; fi; \
            done; \
            attempts=$((attempts + 1)); \
            if [ -z \"$spill\" ]; then /usr/bin/sleep 0.01; fi; \
          done; \
          bytes=''; if [ -n \"$spill\" ]; then bytes=$(/usr/bin/wc -c < \"$spill\"); fi; \
          printf '%s%s%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"acp-spill-readable=' \"$bytes\" '\"}}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"acp-jsonrpc reply\"}}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

// ── Router assembly ─────────────────────────────────────────────────────────

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    build_router_and_host(llm, model_ref).0
}

/// The same single scenario assembly as [`build_router`], with the shared Host
/// returned for cross-module tests that must replace a neutral infrastructure
/// port (for example, a deterministic write-through Memory mounter). This is not
/// a second router path: [`build_router`] delegates here.
pub fn build_router_and_host(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> (Router, Arc<SharedHost>) {
    let host = Arc::new(resource_host(llm, model_ref));
    (mount(host.clone()), host)
}

/// Test/embedder composition with one explicitly resolved deployment snapshot.
/// Production callers resolve this snapshot from typed configuration before
/// constructing the host.
pub fn build_router_with_deployment(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    deployment: awaken_runtime_host::DeploymentConfig,
) -> Router {
    let host = resource_host_with_deployment(llm, model_ref, deployment);
    mount(Arc::new(host))
}

/// Choose a usable model only when an operator omitted the explicit model. Kimi's
/// Anthropic-compatible coding endpoint does not accept Anthropic model ids; all
/// other endpoints retain the ordinary Anthropic default.
fn default_anthropic_compatible_model(base_url: &str) -> &'static str {
    if base_url.contains("api.kimi.com/coding") {
        "kimi-for-coding"
    } else {
        "claude-3-5-haiku-latest"
    }
}

/// Claude Code accepts Kimi's `/coding/` root and appends `/v1` itself, while
/// `GenaiExecutor` expects the Anthropic Messages API base. Accept both operator
/// forms and canonicalize only the known Kimi coding endpoint.
fn normalize_anthropic_compatible_base(base_url: String) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.contains("api.kimi.com/coding") && !trimmed.ends_with("/v1") {
        format!("{trimmed}/v1/")
    } else {
        format!("{trimmed}/")
    }
}

/// A server backed by a **live** Anthropic-compatible model, configured from the
/// environment: `ANTHROPIC_API_KEY` (or `KIMI_API_KEY`), `ANTHROPIC_BASE_URL` (or
/// `KIMI_BASE_URL`), `ANTHROPIC_MODEL` (or `KIMI_MODEL`). This is the same
/// `RedactedString` → `GenaiExecutor` seam used by worker materialization, exposed as a server mode
/// so the TypeScript e2e can drive a real turn through the managed / ai-sdk / a2a
/// adapters. Panics if no API key is set, so a misconfigured run fails loudly.
pub fn build_real_router() -> Router {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let base = normalize_anthropic_compatible_base(base);
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| default_anthropic_compatible_model(&base).to_string());
    let executor = GenaiExecutor::anthropic_compatible(base, key);
    mount(Arc::new(resource_host_with_deployment(
        Arc::new(executor),
        model,
        scenario_deployment(),
    )))
}

/// A server backed by **Gemini on Vertex AI**, authenticated by an OAuth2 Bearer
/// token (ADR-0043 Phase 3 multi-dialect + OAuth). The token is refreshed through
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

/// A live-model server whose executor is built through the production
/// publication + worker-materialization path. It authors an in-memory catalog,
/// persists a credential, freezes one complete model candidate, then lets the
/// worker adapter materialize exactly that candidate. Env:
/// `ANTHROPIC_API_KEY`/`KIMI_API_KEY` (+ `*_BASE_URL`, `*_MODEL`).
pub async fn build_resolved_real_router() -> Router {
    use awaken_agent_config::ModelSelection;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };

    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real-resolved");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let base = normalize_anthropic_compatible_base(base);
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| default_anthropic_compatible_model(&base).to_string());

    // Author the catalog: one provider + endpoint + offering for `model`.
    let catalog_repo = Arc::new(InMemoryCatalogRepo::new());
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
            dialect: ApiDialect::AnthropicMessages,
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
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        })
        .await
        .expect("put offering");

    // Persist the credential, publish a secret-free candidate, and materialize it
    // only at the worker boundary.
    let secrets = Arc::new(InMemorySecretStore::new());
    let cred_repo = Arc::new(InMemoryCredentialRepo::new());
    enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new(key)),
            oauth_command: None,
        },
        secrets.as_ref(),
        cred_repo.as_ref(),
    )
    .await
    .expect("enter credential");
    let resolver = awaken_coordinator::model_resolver::CatalogModelPublicationResolver::from_repo(
        catalog_repo,
        cred_repo.clone(),
    );
    let published = awaken_config_service::ModelPublicationResolver::resolve_models(
        &resolver,
        &awaken_tenancy::ScopeId::from("ws"),
        &ModelSelection::Pinned(ModelBinding::new("anthropic", &model, "genai")),
        &[],
    )
    .await
    .expect("publish model candidate");
    let materializer =
        awaken_coordinator::inference_materializer::CredentialInferenceMaterializer::new(
            cred_repo, secrets,
        );
    let context = attempt_credential::context(&published.primary);
    let executor = materializer
        .materialize_candidate(&published.primary, &context)
        .await
        .expect("materialize published model candidate");
    build_router(executor, published.primary.binding.model_ref)
}

/// The resolved path with an **OAuth** credential (#5): the credential source is
/// `CredentialKind::Oauth`; publication freezes only its reference, and worker
/// materialization runs its `oauth_command` helper (`printf oauth-minted-key`).
/// The minted token becomes the executor's API key, so a run succeeds only if the
/// worker realization path actually ran the helper. `AWAKEN_MODEL_MODE=
/// oauth-resolved` with a fake upstream that authenticates exactly that token.
pub async fn build_oauth_resolved_router() -> Router {
    use awaken_agent_config::ModelSelection;
    use awaken_credential_contract::CredentialSourceId;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
    use awaken_credential_vault::{
        CredentialKind, CredentialSource, CredentialStatus, InMemorySecretStore,
    };
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };

    // The token the helper mints — the fake upstream authenticates exactly this.
    const OAUTH_MINTED_KEY: &str = "oauth-minted-key"; // awaken-allow: secret
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .expect("set ANTHROPIC_BASE_URL for AWAKEN_MODEL_MODE=oauth-resolved");
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string());

    let catalog_repo = Arc::new(InMemoryCatalogRepo::new());
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
            dialect: ApiDialect::AnthropicMessages,
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
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        })
        .await
        .expect("put offering");

    // An OAuth-kind source: nothing sealed; its token is minted by the helper.
    let source = CredentialSource {
        id: CredentialSourceId("cred:ws:oauth".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Oauth,
        provider_id: Some("anthropic".into()),
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: None,
        auxiliary_material_refs: Default::default(),
        // OAuth helper portability decision table:
        // | Windows | helper                                      | token bytes |
        // | true    | cmd.exe /D /C echo|set /p=<token>           | exact       |
        // | false   | printf <token>                              | exact       |
        // Both rules avoid a trailing newline after materialization trims stdout.
        oauth_command: Some(if cfg!(windows) {
            vec![
                "cmd.exe".into(),
                "/D".into(),
                "/C".into(),
                format!("echo|set /p={OAUTH_MINTED_KEY} & exit /b 0"),
            ]
        } else {
            vec!["printf".into(), OAUTH_MINTED_KEY.into()]
        }),
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    };
    let secrets = Arc::new(InMemorySecretStore::new());
    let cred_repo = Arc::new(InMemoryCredentialRepo::new());
    cred_repo
        .put(source)
        .await
        .expect("persist OAuth credential");
    let resolver = awaken_coordinator::model_resolver::CatalogModelPublicationResolver::from_repo(
        catalog_repo,
        cred_repo.clone(),
    );
    let published = awaken_config_service::ModelPublicationResolver::resolve_models(
        &resolver,
        &awaken_tenancy::ScopeId::from("ws"),
        &ModelSelection::Pinned(ModelBinding::new("anthropic", &model, "genai")),
        &[],
    )
    .await
    .expect("publish OAuth model candidate");
    let materializer =
        awaken_coordinator::inference_materializer::CredentialInferenceMaterializer::new(
            cred_repo, secrets,
        );
    let context = attempt_credential::context(&published.primary);
    let executor = materializer
        .materialize_candidate(&published.primary, &context)
        .await
        .expect("materialize published OAuth candidate");
    build_router(executor, published.primary.binding.model_ref)
}

/// Build the server router offering `skills` on every thread (ADR-0036): the whole
/// set is fronted by the single `Skill` tool, whose catalog lists them and whose
/// invocation returns the activated skill's instructions.
pub fn build_router_with_skills(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    skills: Vec<SkillSpec>,
) -> Router {
    mount(Arc::new(resource_host(llm, model_ref).with_skills(skills)))
}

/// A router whose Outcomes are graded by the named Judge Agent through the
/// ordinary Run boundary.
pub fn build_graded_router(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    judge_agent_id: impl Into<String>,
) -> Router {
    mount(Arc::new(
        resource_host(llm, model_ref).with_judge(judge_agent_id),
    ))
}

/// The default deterministic router (echo model) — the CI / e2e server.
pub fn build_echo_router() -> Router {
    build_router(Arc::new(EchoModel), "echo-model")
}

/// Ephemeral ResourceComponent behind the production workspace-path adapter. This
/// is the sole multi-workspace process fixture for volatile resource semantics;
/// it decorates the canonical scenario Host instead of defining another store.
pub fn build_ephemeral_resource_router() -> Router {
    let flat = mount(Arc::new(resource_host(Arc::new(EchoModel), "echo-model")));
    awaken_coordinator::workspace_path::with_workspace_path_addressing(flat)
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
    let (model, model_ref) = scenario_model(Arc::new(CustomToolModel), "custom");
    let host = resource_host(model, model_ref).with_client_tools(client_tools);
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
    let (model, model_ref) = scenario_model(Arc::new(StateMachineModel), "statemachine");
    let host = resource_host(model, model_ref).with_state_machine(machine);
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
        }, {
            "name": "lifecycle",
            "scope": "run",
            "key": "",
            "initial": "tracking",
            "transitions": [{
                "on": { "event": "step.started" },
                "from": "tracking",
                "to": "tracking",
                "update": {
                    "capture": { "last_event": "{event.name}" },
                    "increment": ["started"]
                }
            }, {
                "on": { "event": "step.before_inference" },
                "from": "tracking",
                "to": "tracking",
                "counters": { "started": { "gte": 1 } },
                "emit": {
                    "target": "context",
                    "content": "lifecycle reminder {instance.counters.started}",
                    "cooldown_turns": 0
                },
                "update": { "reset": ["started"] }
            }, {
                "on": { "event": "step.after_inference" },
                "from": "tracking",
                "to": "tracking",
                "update": { "increment": ["inferences"] }
            }, {
                "on": { "event": "step.ended" },
                "from": "tracking",
                "to": "tracking",
                "update": { "capture": { "last_event": "{event.name}" } }
            }]
        }, {
            "name": "warning",
            "key": "${pattern}",
            "initial": "locked",
            "transitions": [{
                "on": "glob(pattern ~ \"*\")",
                "from": "unlocked",
                "to": "done",
                "on_violation": {
                    "action": "warn",
                    "reason": "glob warning for {pattern}"
                }
            }]
        }, {
            "name": "result_fallback",
            "initial": "start",
            "on_unmatched": "fallback",
            "transitions": [{
                "on": "glob(pattern ~ \"*\")",
                "from": ["start", "fallback"],
                "to": "error",
                "when": "error"
            }]
        }, {
            "name": "audit",
            "initial": "tracking",
            "transitions": [{
                "on": "glob(pattern ~ \"*\")",
                "from": "tracking",
                "to": "tracking",
                "update": {
                    "capture": { "last_pattern": "{pattern}" },
                    "increment": ["calls"]
                }
            }]
        }]
    });
    let (model, model_ref) = scenario_model(Arc::new(StateMachineModel), "statemachine-rich");
    let host = resource_host(model, model_ref).with_state_machine(machine);
    mount(Arc::new(host))
}

/// A router with the config data plane (`/v1/config/agents/*`) over an in-memory
/// SQLite config store, plus the protocol adapters. A session for a *published*
/// agent runs with that agent's installed config (slice A); the model echoes the
/// agent's instructions so an e2e can assert the published config took effect.
struct ScheduleGate;

#[async_trait::async_trait]
impl awaken_runtime_contract::permission::ToolGateHook for ScheduleGate {
    async fn gate(
        &self,
        ctx: &awaken_runtime_contract::permission::ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> awaken_runtime_contract::permission::GateOutcome {
        awaken_runtime_contract::permission::GateOutcome::Schedule {
            correlation_id: format!("sched-{}", ctx.call_id),
            action_kind: None,
        }
    }
}

/// A router whose tool gate defers every tool call as a `ScheduledAction`
/// (ADR-0020, slice E). Drive it with `typed durable ingress` so the dispatch
/// worker performs the deferred actions out of band: the probe model's
/// write→read tool calls are each scheduled and auto-performed, so the run
/// completes without any human confirmation.
pub fn build_schedule_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(ProbeModel), "schedule");
    let host = resource_host_with_deployment(model, model_ref, scenario_deployment())
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
    let security_fingerprint = std::env::var("AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT")
        .expect("AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT must be set for delegate-remote mode");
    let (model, model_ref) = scenario_model(Arc::new(DelegatingModel), "delegate-remote");
    // Both parent and child are ordinary immutable publications. The child's
    // resolved backend selects the shared A2A attempt executor; delegation owns
    // no transport registry or protocol-specific execution path.
    let mut tools = awaken_runtime_host::authorable_tools();
    tools.retain(|tool| tool.kind == ToolKind::AgentDelegation);
    let assistant = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::new("default", &model_ref, "default"))
        .tools(tools)
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let researcher = ExecutableAgentSnapshot::builder("researcher")
        .resolved_model(ResolvedModelCandidate::remote(
            ModelBinding::new("remote", "", format!("a2a:{url}")),
            awaken_tenancy::ScopeId::from("default"),
            None,
            security_fingerprint,
        ))
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([assistant, researcher])
        .expect("valid remote delegation publication");
    let host = resource_host(model, model_ref)
        .with_agent_publications(Arc::new(publications))
        .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(None));
    mount(Arc::new(host))
}

/// A deterministic model for the skills e2e (ADR-0036). On the user turn it calls
/// `list_skills` to discover the offered skills; given the catalog it activates the
/// `greet` skill via the `Skill` tool; given the activation instructions it replies
/// with them — so an e2e can assert discover → activate → use end to end. Stateless.
pub struct SkillDrivingModel;

/// Deterministic Native model used to prove that a Session Environment-installed
/// Playwright MCP process is attached to the in-process Runtime rather than
/// spawned on the host.
pub struct NativePlaywrightMcpModel;

#[async_trait::async_trait]
impl LlmExecutor for NativePlaywrightMcpModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request.messages.last().expect("a message");
        let last_text = extract_text(&last.content);
        let output = match last.role {
            Role::User => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "playwright-native".into(),
                tool_id: "mcp__playwright__browser_navigate".into(),
                arguments: serde_json::json!({
                    "url": "data:text/html,<title>AWAKEN-NATIVE-PLAYWRIGHT-MCP-OK</title><h1>AWAKEN-NATIVE-PLAYWRIGHT-MCP-OK</h1>"
                }),
            }]),
            Role::Tool if last_text.contains("AWAKEN-NATIVE-PLAYWRIGHT-MCP-OK") => {
                AssistantOutput::text("AWAKEN-NATIVE-PLAYWRIGHT-MCP-OK")
            }
            Role::Tool => {
                AssistantOutput::text(format!("NATIVE-PLAYWRIGHT-MCP-FAILED: {last_text}"))
            }
            _ => AssistantOutput::text("NATIVE-PLAYWRIGHT-MCP-FAILED"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for SkillDrivingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last = request.messages.last().expect("a message");
        let last_text = extract_text(&last.content);
        let output = match last.role {
            Role::User => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "l".into(),
                tool_id: "list_skills".into(),
                arguments: serde_json::json!({}),
            }]),
            Role::Tool if last_text.contains("\"skills\"") => {
                // The catalog came back — activate the id it actually advertised.
                // This keeps the fixture valid for both a legacy `greet` id and the
                // official multipart API's tagged catalog id.
                let skill = serde_json::from_str::<serde_json::Value>(&last_text)
                    .ok()
                    .and_then(|value| value["skills"][0]["id"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "greet".to_string());
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s".into(),
                    tool_id: "Skill".into(),
                    arguments: serde_json::json!({ "skill": skill }),
                }])
            }
            Role::Tool => AssistantOutput::text(format!("USED-SKILL: {last_text}")),
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
    let (model, model_ref) = scenario_model(Arc::new(SkillDrivingModel), "skills");
    build_router_with_skills(model, model_ref, vec![greet, review])
}

/// A router whose delivered skills come from a DURABLE catalog (`/v1/skills`) instead
/// of static config, rooted under `DeploymentConfig::storage_dir` (a per-process temp dir when
/// unset). A skill posted to `/v1/skills` is offered on every thread and survives a
/// restart. The `SkillDrivingModel` discovers → activates `greet` → replies with its
/// body, so an e2e proves a durably-configured skill reaches the model across a
/// restart. `AWAKEN_MODEL_MODE=skills-durable`.
pub async fn build_skills_durable_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(SkillDrivingModel), "skills-durable");
    let deployment = scenario_deployment();
    let host = resource_host_with_deployment(model, model_ref, deployment);
    mount(Arc::new(host))
}
pub async fn build_config_router() -> Router {
    // The MODEL is chosen by `scenario_model` (in-process echo, or the real provider
    // pointed at the fake upstream when `AWAKEN_MODEL_SOURCE=http`); the fake upstream
    // is what drives the seeded assistant through its admin tools in the run e2e.
    let (model, model_ref) = scenario_model(Arc::new(InstructionEchoModel), "config");
    let deployment = scenario_deployment();
    let platform_workspace = deployment.storage_dir.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let store = Arc::new(
        awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
    );
    // Scope-keyed tool visibility (ADR-0052 D3): every scope sees the advertised
    // (global) tools; only the reserved admin scope additionally sees the four
    // management descriptors, so a config naming an `admin_*` tool compiles only there.
    let global = awaken_runtime_host::authorable_tools();
    let tools = Arc::new(awaken_config_service::ScopedToolCatalog::new(
        global.clone(),
        awaken_config_service::RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    // A minimal LIVE catalog repo with one provider + endpoint + offering for the
    // scenario model, so an `Auto` config (the management assistant) resolves to a
    // concrete binding at publish (ADR-0052 D5) AND the capability reader reports the
    // scenario model live.
    let catalog_repo = model_publication::scenario_model_catalog(&model_ref).await;
    // The service is scope-free (ADR-0051); `ConfigPlane` is the scope edge that binds
    // the request scope (a `ScopedConfig` registry + the scope's tool catalog) onto it.
    let executable_agent_catalog = Arc::new(ExecutableAgentCatalog::new());
    let service = Arc::new(ConfigService::new(
        Arc::new(model_publication::ScenarioHostModelResolver::new(
            catalog_repo.clone(),
        )),
        Arc::new(LocalExecutableAgentRegistrar::new(
            executable_agent_catalog.clone(),
        )),
    ));
    let plane = awaken_config_service::ConfigPlane::new(service.clone(), store, tools);
    // The management tool executables, backed by real ports (D3/D4): the capability
    // reader reads the shared catalog + advertised tools; the validator runs the same
    // compile check as `/v1/config/agents/validate` on drafts (in the tenant scope).
    let reader = Arc::new(awaken_control::CatalogCapabilityReader::new(
        catalog_repo.clone(),
        &global,
        &awaken_runtime_host::authorable_config_sections(),
        // The config plane, to list existing agent ids in the tenant scope.
        plane.clone(),
        platform_workspace.clone(),
        // No data-plane inventory wired here (scenario host); memory/skills stay empty.
        None,
    ));
    let validator = Arc::new(awaken_control::ConfigServiceDraftValidator::new(
        plane.clone(),
        platform_workspace.clone(),
    ));
    let admin_execs = awaken_admin_assistant::admin_tools(
        reader,
        validator,
        // Persist/read drafts as unpublished config agents through the same plane the
        // editor's Save uses, in the tenant/default scope (ADR-0052).
        Arc::new(awaken_control::ConfigServiceDraftStore::new(
            plane.clone(),
            platform_workspace.clone(),
            // The scenario host has no durable resource store in scope; an in-memory one
            // satisfies the port so the assistant can bind resources onto a draft.
            Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new()),
        )),
        // A fresh in-memory environment registry satisfies the author port for the
        // scenario host (no durable env state in scope).
        Arc::new(
            awaken_environment_application::EnvironmentApplicationAuthor::new(
                composition::test_environment_components().0.application(),
            ),
        ),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    let host = resource_host_with_deployment(model, model_ref, deployment)
        .with_local_workspace(platform_workspace.clone())
        .with_agent_publications(executable_agent_catalog.clone())
        .with_agent_resource_references(executable_agent_catalog.clone())
        .with_admin_tools(admin_execs)
        .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(None));
    // The reserved value owns only configuration/tool visibility. Install the
    // executable in the Host's real platform Workspace so Sessions, resources,
    // credentials, and runtime lookup share one coordinate.
    awaken_control::seed_admin_assistant(
        &plane,
        &platform_workspace,
        awaken_agent_config::ModelSelection::Auto,
    )
    .await
    .expect("seed admin assistant");
    // `/v1/agents` authors and reads through the same durable ConfigPlane.
    let agents = awaken_protocol_managed::agents_router(std::sync::Arc::new(
        awaken_protocol_managed::AgentRegistryState::from_repository(std::sync::Arc::new(
            awaken_control::ConfigPlaneManagedAgentRepository::new(
                plane.clone(),
                platform_workspace.clone(),
            ),
        )),
    ));
    // Workspace-path addressing (ADR-0048/0052 D2): `/v1/workspaces/{ws}/config/...`
    // is rewritten to the flat config route and stamped with `{ws}` as the scope, so
    // the reserved admin scope is reachable and the tenant/default scope is fenced.
    // Composition decision table: storage_dir absent -> the canonical scenario
    // mount supplies an ephemeral Session repository; storage_dir present -> it
    // supplies sessions.db beside runtime truth. The Agent source is an added port,
    // never a reason to rebuild ManagedState through a parallel in-memory path.
    let flat = mount_with_agent_source(Arc::new(host), executable_agent_catalog)
        .merge(config_router(plane))
        .merge(agents);
    let flat =
        awaken_coordinator::workspace_path::with_platform_workspace(flat, platform_workspace);
    awaken_coordinator::workspace_path::with_workspace_path_addressing(flat)
}

struct AllowAllGate;

#[async_trait::async_trait]
impl awaken_runtime_contract::permission::ToolGateHook for AllowAllGate {
    async fn gate(
        &self,
        _ctx: &awaken_runtime_contract::permission::ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> awaken_runtime_contract::permission::GateOutcome {
        awaken_runtime_contract::permission::GateOutcome::Allow
    }
}

#[cfg(test)]
mod compatible_endpoint_tests {
    use super::{default_anthropic_compatible_model, normalize_anthropic_compatible_base};

    #[test]
    fn kimi_coding_root_is_canonicalized_for_the_messages_provider() {
        assert_eq!(
            normalize_anthropic_compatible_base("https://api.kimi.com/coding/".into()),
            "https://api.kimi.com/coding/v1/"
        );
        assert_eq!(
            normalize_anthropic_compatible_base("https://api.kimi.com/coding/v1/".into()),
            "https://api.kimi.com/coding/v1/"
        );
    }

    #[test]
    fn omitted_model_uses_the_endpoint_vocabulary() {
        assert_eq!(
            default_anthropic_compatible_model("https://api.kimi.com/coding/v1/"),
            "kimi-for-coding"
        );
        assert_eq!(
            default_anthropic_compatible_model("https://api.anthropic.com/v1/"),
            "claude-3-5-haiku-latest"
        );
    }
}
