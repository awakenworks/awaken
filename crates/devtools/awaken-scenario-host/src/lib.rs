//! Test-only scenario host: the deterministic mock models and the `build_*_router`
//! scenario assemblies the e2e harness + integration tests drive. Extracted from
//! `awaken-coordinator` so the product crate carries zero mocks. It reuses the
//! product crate's now-`pub` data-plane assembly helpers (`mount` /
//! `mount_with_managed`) and production executors via `awaken_coordinator::`.

mod acp_gateway;
mod acp_scenarios;
mod attempt_credential;
mod delegation;
mod deployment;
mod distributed_control;
mod dream;
mod managed_state;
mod model_publication;
mod model_routing;
mod models;
pub mod realization_observation;
mod scenario_platform;
mod web;
mod worker;
pub use crate::models::*;
pub use acp_scenarios::{
    build_acp_container_router, build_acp_control_router, build_acp_gateway_router,
    build_acp_gateway_router_with_deployment, build_acp_jsonrpc_router,
    build_acp_jsonrpc_router_with_deployment, build_acp_managed_mcp_router,
    build_acp_permission_router, build_acp_permission_router_with_deployment,
    build_acp_real_mcp_router, build_acp_relaunch_failure_router, build_acp_router,
    build_acp_sandboxed_router, build_acp_sandboxed_router_with_deployment,
    build_acp_tool_bridge_router,
};
pub use delegation::{build_delegation_router, build_delegation_router_with_deployment};
pub use deployment::{install_scenario_runtime_authority, scenario_deployment};
pub use distributed_control::build_distributed_provider_router;
pub use distributed_control::serve_distributed_control;
pub use dream::{build_dream_router, build_dream_router_and_host, build_dream_runtime_router};
pub use managed_state::{
    LocalManagedState, local_managed_state, local_managed_state_with_agent_source,
    local_managed_state_with_agent_source_and_list_prices,
    local_managed_state_with_agent_source_and_model_publication_resolver,
    local_managed_state_with_environments, local_managed_state_with_environments_and_agent_source,
    local_managed_state_with_model_publication_resolver, volatile_environment_execution,
};
pub use model_routing::{build_model_route_router, scenario_model};
pub use scenario_platform::build_unscoped_resource_router;
pub use web::build_management_web_router;
pub use worker::run_echo_worker;

mod scenario_shell;

use deployment::{
    resource_host, resource_host_with_deployment, runtime_resource_host_with_deployment,
};
use scenario_platform::{
    fixed_agent_publication, fixed_host_backend_publication,
    fixed_host_backend_publication_with_acp_mcp, fixed_host_backend_publication_with_mcp,
    mount_platform, mount_with_agent_source, mount_with_environments,
    mount_with_environments_and_agent_source, mount_with_memory_publication,
};
use scenario_shell::{scenario_argv, scenario_host_acp_cli, scenario_shell_argv};

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::extract_text;
use awaken_agent_contract::agent::message::Role;
use awaken_dispatch::SessionRunReservations as _;
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate, ToolKind};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_tool_contract::tool::ToolCall;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

// This Scenario platform depends on each authoritative owner directly.
pub use awaken_config_service::ConfigService;
pub use awaken_control::{capabilities_router, config_router};
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_protocol_managed::{
    default_models, files_router, memory_stores_router, models_router, skills_router,
};
pub use awaken_session_executor::{
    ExecutorComponents, HostResume, SessionExecutorBuilder, ThreadEvent, ThreadEventHub,
    advertised_tools,
};
pub use awaken_session_mcp::ExtMcpProbe;

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
pub async fn build_outcome_matrix_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        scenario_shell_argv(FAKE_OUTCOME_ACP_SCRIPT),
        vec![],
    );
    let (model, model_ref) = scenario_model(Arc::new(ReviseModel), "outcome-native");
    let judge_backend = match std::env::var("AWAKEN_OUTCOME_JUDGE_RUNTIME").as_deref() {
        Ok("acp") => "acp:claude",
        _ => "default",
    };
    let judge_binding = ModelBinding::parse("outcome-test", &model_ref, judge_backend)
        .unwrap_or_else(|error| {
            panic!("scenario model `{model_ref}` on AWAKEN_OUTCOME_JUDGE_RUNTIME backend: {error}")
        });
    let judge = ExecutableAgentSnapshot::builder("outcome-judge")
        .instructions("Return only the required strict Outcome Grade JSON.")
        .model(judge_binding)
        .max_steps(2)
        .build();
    let publication = fixed_host_backend_publication("acp-agent", "acp:claude", Vec::new());
    let platform = acp_scenarios::bind_acp_session_environment(
        resource_host(model, model_ref).await,
        awaken_execution_composition::LaunchSource::Fixed(Box::new(launch)),
        None,
    )
    .await
    .map_dependencies(|host| {
        host.with_agent_publications(publication.clone())
            .with_judge_snapshot(judge)
    });
    mount_with_agent_source(platform, publication).await
}

/// A router with out-of-band memory extraction + bounded recall (the memory
/// e2e): after each Run the extractor sub-run saves a memory, and later
/// sessions see it injected request-only by the recall plugin.
/// `AWAKEN_MODEL_MODE=memory`; the scenario seeds one governed MemoryStore and
/// callers attach that exact resource to each participating Session.
pub async fn build_memory_router() -> Router {
    build_memory_service().await.0
}

/// The Memory Scenario's one mounted Router and outer-owned service lifecycle.
/// The process entrypoint retains the lifecycle so graceful shutdown drains the
/// exact auxiliary Runtime work whose spans and durable effects it serves.
pub async fn build_memory_service() -> (Router, awaken_service_lifecycle::ServiceLifecycle) {
    let (model, model_ref) = scenario_model(Arc::new(MemoryProbeModel), "memory");
    let host = resource_host(model, model_ref).await;
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let router = scenario_platform::mount_with_memory_publication_on_lifecycle(
        host,
        "memory",
        Vec::new(),
        lifecycle.clone(),
    )
    .await;
    (router, lifecycle)
}

/// A router for the memory_store RESOURCE durability e2e (ADR-0038 MemoryStore
/// family): a deterministic model writes into a mounted, read-write memory store
/// and the host harvests the write back under the store's stable id. Distinct from
/// [`build_memory_router`]'s cross-session *extraction* memory — this exercises the
/// `resources[{type:"memory_store"}]` mount + write-back + `/v1/memory_stores` API.
/// `AWAKEN_MODEL_MODE=memory-resource`.
pub async fn build_memory_resource_router() -> Router {
    let (model, model_ref) = scenario_model(
        Arc::new(crate::models::MemoryResourceModel),
        "memory-resource",
    );
    let host = resource_host(model, model_ref).await;
    mount_platform(host).await
}

/// A router for the github_repository RESOURCE e2e (ADR-0038): a deterministic model
/// reads a host-cloned repo's file and writes a sandbox-local change. Remote publication
/// remains an explicit operator workflow. Repository execution requires one
/// path-faithful environment across Hand, Bash, Git, and Agent, so this factory
/// consumes the same typed Deployment environment selector as other
/// provider-backed scenarios. `AWAKEN_MODEL_MODE=git-repo`.
pub async fn build_git_repo_router(
    deployment: awaken_deployment_config::DeploymentConfig,
) -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::GitRepoModel), "git-repo");
    mount_platform(runtime_resource_host_with_deployment(model, model_ref, deployment).await).await
}

/// The combined-chain router (native full-chain e2e): one session configures a
/// memory_store + github_repository resource, is offered a skill, and has out-of-band
/// memory extraction — so a single conversation exercises skill use → memory-store
/// write-back → git commit/patch export → output-artifact harvest end to end on the native
/// backend. Driven over the real wire by the `fullChain` behavior (this in-process
/// `EchoModel` is only the non-http fallback, never run by the e2e).
/// `AWAKEN_MODEL_MODE=full-chain` with `AWAKEN_MODEL_SOURCE=http`.
pub async fn build_full_chain_router() -> Router {
    build_full_chain_router_with_deployment(scenario_deployment()).await
}

/// The full-chain assembly with an explicit deployment snapshot. Tests use the
/// local tier while the process entry point keeps the configured production
/// tier; both paths consume the same Runtime/resource composition.
pub async fn build_full_chain_router_with_deployment(
    deployment: awaken_deployment_config::DeploymentConfig,
) -> Router {
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "full-chain");
    let host = runtime_resource_host_with_deployment(model, model_ref, deployment).await;
    mount_with_memory_publication(
        host,
        "full-chain",
        vec![awaken_agent_contract::AgentSkillBinding::custom(
            awaken_resource_contract::skill_catalog_id("greet"),
        )],
    )
    .await
}

/// A router with context compaction (the compaction e2e): a low token window folds
/// the older transcript into a summary after a few Runs. The deterministic
/// model returns a fixed summary on the `compactor` sub-run and otherwise
/// reports the compaction context it received, so an e2e can observe the folded
/// summary being injected on a later Run. `AWAKEN_MODEL_MODE=compaction`.
pub async fn build_compaction_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::CompactionModel), "compaction");
    // Compaction changes run behavior, not deployment ownership. Reuse the
    // canonical Scenario platform so durable Session/resource identity is
    // reconstructed from the same storage root after restart.
    let host = resource_host(model, model_ref).await;
    // `AWAKEN_COMPACT_MAX_TOKENS` is an exact effective trigger override, matching
    // the publication output rather than reopening a ratio decision in the Host.
    // Otherwise derive the typed default (3/4 of the model window); the fully
    // deterministic fixture uses a one-token window when it publishes no model
    // capability. `AWAKEN_COMPACT_KEEP_LAST` defaults to 2 messages.
    let env_u = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
    // Harness-local window wiring: an explicit override (`AWAKEN_COMPACT_MAX_TOKENS`) wins,
    // else the model's published context window (`AWAKEN_MODEL_CONTEXT_WINDOW` — this harness's
    // projection of the catalog's `ModelSpec.context_window`). Production derives the effective
    // trigger at publish (config-service `apply_compaction`); this driver sets it directly.
    let window = env_u("AWAKEN_COMPACT_MAX_TOKENS")
        .or_else(|| env_u("AWAKEN_MODEL_CONTEXT_WINDOW").map(|window| window / 4 * 3))
        .unwrap_or(1);
    let keep_last = env_u("AWAKEN_COMPACT_KEEP_LAST").unwrap_or(2) as usize;
    let host = host.map_dependencies(|host| host.with_compaction_tokens(window, keep_last));
    mount_platform(host).await
}

/// A router whose model fails a Run on the `BOOM` trigger (the session-error
/// e2e): the failed Run surfaces an internal `RunError` (HTTP `api_error`) and
/// commits a `session.error` event, while other Runs echo — proving the Session
/// stays usable after a failure. `AWAKEN_MODEL_MODE=error`.
pub async fn build_error_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::ErrorModel), "error");
    let host = resource_host(model, model_ref).await;
    mount_platform(host).await
}

/// A router that mounts `/v1/environments` and shares its state with the session
/// surface, so a session created on a **self-hosted** environment is dispatched as a
/// `session` work item. The self-hosted worker e2e polls the queue, claims that
/// work, then drives the session — whose agent awaits on a client-executed
/// `submit_answer` tool — by **running the tool and posting the result back**, the
/// way a self-hosted worker executes the session's tool calls. Heartbeats the lease
/// and stops the work on completion. `AWAKEN_MODEL_MODE=worker`.
pub async fn build_worker_router() -> Router {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let (model, model_ref) = scenario_model(Arc::new(CustomToolModel), "worker");
    let host = resource_host(model, model_ref)
        .await
        .map_dependencies(|host| host.with_client_tools(client_tools));
    mount_with_environments(host).await
}

/// Echo-model Scenario platform with the official Environment API, exact sandbox-policy
/// store, Resource Registry, Managed Sessions, and all protocol adapters. It exists
/// solely to drive the orthogonal configuration matrix without application-auth
/// concerns obscuring the baseline/provisioning behavior under test.
pub async fn build_environment_matrix_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "environment-matrix");
    mount_with_environments(resource_host(model, model_ref).await).await
}

fn ask_tool_bindings(tool_name: &str) -> AgentBindings {
    AgentBindings {
        toolsets: vec![awaken_agent_contract::ToolsetPolicy {
            source: awaken_agent_contract::ToolsetSource::Agent,
            default: awaken_agent_contract::ToolExecutionPolicy::default(),
            overrides: vec![awaken_agent_contract::ToolPolicyOverride::new(
                tool_name,
                awaken_agent_contract::ToolExecutionPolicy {
                    enabled: true,
                    permission: awaken_agent_contract::ToolPermissionRequirement::AlwaysAsk,
                },
            )],
        }],
        ..Default::default()
    }
}

/// Native tool-control fixture with an explicitly authored policy. The Probe
/// model's `write` call is the mutating edge used by Managed, A2A, and
/// cross-protocol HITL tests; leaving its policy implicit would make those tests
/// follow whichever neutral default the runtime currently uses.
pub async fn build_probe_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(ProbeModel), "probe");
    let binding = ModelBinding::parse("scenario", &model_ref, "native")
        .unwrap_or_else(|error| panic!("scenario model `{model_ref}`: {error}"));
    let snapshot = ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(ResolvedModelCandidate::host(binding))
        .metadata(awaken_runtime_contract::snapshot::AgentSnapshotMetadata {
            source: awaken_runtime_contract::snapshot::AgentConfigRevisionRef {
                agent_id: AgentId("assistant".into()),
                revision: 1,
            },
            ..Default::default()
        })
        .agent_bindings(ask_tool_bindings("write"))
        .build();
    let publication = fixed_agent_publication([snapshot]);
    let platform = resource_host(model, model_ref)
        .await
        .map_dependencies(|host| host.with_agent_publications(publication.clone()));
    mount_with_agent_source(platform, publication).await
}

/// The one deterministic ACP JSON-RPC fixture. It exercises the canonical codec
/// for every catalog route without copying a fake per CLI. `AWAKEN_MATRIX_RUNTIME`
/// is scenario-only observability injected by the catalog projection; delegated
/// static launches recover the same id from their frozen instructions. The
/// optional readiness directory proves that an interrupt target reached its
/// blocking prompt branch without adding a second fake or a timing guess.
const FAKE_ACP_JSONRPC_SCRIPT: &str = "runtime=${AWAKEN_MATRIX_RUNTIME:-unknown}; \
    opened=new; \
    request_permission() { \
      printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"s1\",\"toolCall\":{\"toolCallId\":\"matrix-permission-call\",\"title\":\"write\",\"rawInput\":{\"path\":\"matrix.txt\"}},\"options\":[{\"optionId\":\"ok\",\"name\":\"Allow\",\"kind\":\"allow_once\"},{\"optionId\":\"no\",\"name\":\"Reject\",\"kind\":\"reject_once\"}]}}'; \
    }; \
    while IFS= read -r line; do \
      case \"$line\" in \
        *'matrix-runtime=claude'*) runtime=claude;; \
        *'matrix-runtime=codex'*) runtime=codex;; \
        *'matrix-runtime=gemini'*) runtime=gemini;; \
        *'matrix-runtime=opencode'*) runtime=opencode;; \
        *'matrix-runtime=hermes'*) runtime=hermes;; \
      esac; \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true}}}';; \
        *'\"id\":2'*) \
          case \"$line\" in *'session/load'*) opened=load;; *) opened=new;; esac; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\",\"models\":{\"currentModelId\":\"default\",\"availableModels\":[]}}}';; \
        *'\"id\":6'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{}}';; \
        *'\"id\":42'*'ok'*) \
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"matrix-%s-hitl-allowed\"}}}}\\n' \"$runtime\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\",\"usage\":{\"totalTokens\":18,\"inputTokens\":11,\"outputTokens\":7,\"cachedReadTokens\":3,\"cachedWriteTokens\":2}}}'; \
          exit 0;; \
        *'\"id\":42'*) \
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"matrix-%s-hitl-denied\"}}}}\\n' \"$runtime\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\",\"usage\":{\"totalTokens\":18,\"inputTokens\":11,\"outputTokens\":7,\"cachedReadTokens\":3,\"cachedWriteTokens\":2}}}'; \
          exit 0;; \
        *'\"id\":3'*) \
          case \"$line\" in \
            *'The pending write request (matrix-permission-call) was approved.'*) \
              request_permission;; \
            *matrix-hitl*) \
              request_permission;; \
            *matrix-error*) printf '%s\\n' 'not-json: deterministic ACP adapter failure'; exit 0;; \
            *matrix-interrupt*) while :; do :; done; \
              printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"matrix-%s-uncancelled\"}}}}\\n' \"$runtime\"; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; exit 0;; \
            *matrix-basic*) \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"matrix-c1\",\"title\":\"read\",\"rawInput\":{\"path\":\"matrix.txt\"}}}}'; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"matrix-c1\",\"status\":\"completed\",\"content\":[{\"type\":\"content\",\"content\":{\"type\":\"text\",\"text\":\"matrix tool result\"}}]}}}'; \
              printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"acp-runtime reply; matrix-%s-reply open=%s\"}}}}\\n' \"$runtime\" \"$opened\"; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\",\"usage\":{\"totalTokens\":18,\"inputTokens\":11,\"outputTokens\":7,\"cachedReadTokens\":3,\"cachedWriteTokens\":2}}}'; exit 0;; \
            *matrix-suite=true*) request_permission;; \
            *) \
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
              printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"acp-runtime reply; acp-jsonrpc reply runtime=%s\"}}}}\\n' \"$runtime\"; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\",\"usage\":{\"totalTokens\":18,\"inputTokens\":11,\"outputTokens\":7,\"cachedReadTokens\":3,\"cachedWriteTokens\":2}}}'; exit 0;; \
          esac;; \
      esac; \
    done";

// ── Router assembly ─────────────────────────────────────────────────────────

/// Build the server router backed by the kernel with the given model.
pub async fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    build_router_and_host(llm, model_ref).await.0
}

/// The same single scenario assembly as [`build_router`], with the shared Host
/// returned for cross-module tests that must replace a neutral infrastructure
/// port (for example, a deterministic write-through Memory mounter). This is not
/// a second router path: [`build_router`] delegates here.
pub async fn build_router_and_host(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> (Router, Arc<ExecutorComponents>) {
    build_router_and_host_from(resource_host(llm, model_ref).await).await
}

/// The dependencies of an unmounted Host for a test that owns the outer
/// product composition: it selects any further dependency, builds the Host,
/// and mounts it exactly once.
///
/// Unlike [`build_router_and_host`], this function starts no lifecycle and binds
/// no Session application authority.
pub async fn build_unmounted_dependencies(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> SessionExecutorBuilder {
    build_unmounted_dependencies_with_deployment(llm, model_ref, scenario_deployment()).await
}

/// The unmounted scenario composition with an explicit deployment. Resources
/// and Runtime consume the same deployment before any stores or sandboxes open.
/// No lifecycle or Session application is installed until the caller mounts it.
pub async fn build_unmounted_dependencies_with_deployment(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    deployment: awaken_deployment_config::DeploymentConfig,
) -> SessionExecutorBuilder {
    resource_host_with_deployment(llm, model_ref, deployment)
        .await
        .into_dependencies()
        .0
}

/// Scenario assembly whose Runtime and Managed adapters can share the exact
/// immutable Agent publication authority. The catalog may be populated after
/// construction once the generated local workspace id is known.
pub async fn build_router_and_host_with_agent_publications(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    publications: impl IntoIterator<Item = awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> (Router, Arc<ExecutorComponents>) {
    let publication = scenario_platform::fixed_agent_publication(publications);
    let (host, resources) = resource_host(llm, model_ref)
        .await
        .map_dependencies(|host| host.with_agent_publications(publication.clone()))
        .into_dependencies();
    scenario_platform::mount_parts_with_agent_source_on_lifecycle(
        host,
        resources,
        publication,
        awaken_service_lifecycle::ServiceLifecycle::new(),
    )
    .await
}

/// [`build_unmounted_dependencies`] with one immutable Agent publication
/// authority. Cross-module product-composition tests use this seam to avoid
/// mounting a disposable Session application before installing their real
/// composition.
pub async fn build_unmounted_dependencies_with_agent_publications(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    publications: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
) -> SessionExecutorBuilder {
    build_unmounted_platform_with_agent_publications(llm, model_ref, publications)
        .await
        .0
}

/// Unmounted Runtime dependencies and the exact Resources application selected
/// with them. Callers composing Managed routes retain both original owners.
pub async fn build_unmounted_platform_with_agent_publications(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    publications: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
) -> (
    SessionExecutorBuilder,
    awaken_resource_application::ResourcesApplication,
) {
    resource_host(llm, model_ref)
        .await
        .map_dependencies(|dependencies| dependencies.with_agent_publications(publications))
        .into_dependencies()
}

async fn build_router_and_host_from(
    platform: deployment::ScenarioPlatform,
) -> (Router, Arc<ExecutorComponents>) {
    let (host, resources) = platform.into_dependencies();
    scenario_platform::mount_parts(host, resources).await
}

pub(crate) async fn build_router_and_host_with_model_publication_resolver(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
) -> (Router, Arc<ExecutorComponents>) {
    let platform = resource_host(llm, model_ref).await;
    let (host, resources) = platform.into_dependencies();
    scenario_platform::mount_parts_with_model_publication_resolver(host, resources, resolver, None)
        .await
}

/// Test/embedder platform with one explicitly resolved deployment snapshot.
/// Production callers resolve this snapshot from typed configuration before
/// constructing the host.
pub async fn build_router_with_deployment(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    deployment: awaken_deployment_config::DeploymentConfig,
) -> Router {
    let host = resource_host_with_deployment(llm, model_ref, deployment).await;
    mount_platform(host).await
}

/// Choose a usable model only when an operator omitted the explicit model. Kimi
/// Code's Anthropic-compatible endpoint currently exposes Claude-compatible
/// public ids; all other endpoints retain the conservative Anthropic default.
fn default_anthropic_compatible_model(base_url: &str) -> &'static str {
    if base_url.contains("api.kimi.com/coding") {
        "claude-sonnet-5"
    } else {
        "claude-3-5-haiku-latest"
    }
}

/// Claude Code accepts Kimi's `/coding/` root and appends `/v1` itself, while
/// the provider executor consumes the Messages API base directly.
fn normalize_anthropic_compatible_base(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.contains("api.kimi.com/coding") && !trimmed.ends_with("/v1") {
        format!("{trimmed}/v1/")
    } else {
        format!("{trimmed}/")
    }
}

/// Route every direct scenario/live Anthropic endpoint through the same
/// dialect-aware executor factory as publication-pinned worker materialization.
/// Keeping this helper in the scenario assembly avoids reviving the legacy
/// generic-genai Anthropic path merely because a test supplies environment vars.
fn anthropic_messages_executor(base_url: &str, api_key: String) -> Arc<dyn LlmExecutor> {
    let credential = awaken_agent_contract::RedactedString::new(api_key);
    let base_url = normalize_anthropic_compatible_base(base_url);
    awaken_credential_materializer::executor_from_materialized_endpoint(
        "anthropic_messages",
        "anthropic",
        Some(&base_url),
        Some(&credential),
    )
    .expect("construct the canonical Anthropic Messages executor")
}

/// A server backed by a **live** Anthropic-compatible model, configured from the
/// environment: `ANTHROPIC_API_KEY` (or `KIMI_API_KEY`), `ANTHROPIC_BASE_URL` (or
/// `KIMI_BASE_URL`), `ANTHROPIC_MODEL` (or `KIMI_MODEL`). This is the same
/// dialect-aware executor factory used by worker materialization, exposed as a server mode
/// so the TypeScript e2e can drive a real Run through the Managed / AI SDK / A2A
/// adapters. Panics if no API key is set, so a misconfigured run fails loudly.
async fn real_platform() -> deployment::ScenarioPlatform {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or KIMI_API_KEY for AWAKEN_MODEL_MODE=real");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("KIMI_BASE_URL"))
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string());
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| default_anthropic_compatible_model(&base).to_string());
    let executor = anthropic_messages_executor(&base, key);
    runtime_resource_host_with_deployment(executor, model, scenario_deployment()).await
}

pub async fn build_real_router() -> Router {
    mount_platform(real_platform().await).await
}

/// Real-model scenario with an outer-owned lifecycle, matching production's
/// graceful Runtime drain before the process flushes its trace provider.
pub async fn build_real_service() -> (Router, awaken_service_lifecycle::ServiceLifecycle) {
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let router =
        scenario_platform::mount_on_lifecycle(real_platform().await, lifecycle.clone()).await;
    (router, lifecycle)
}

/// A server backed by **Gemini on Vertex AI**, authenticated by an OAuth2 Bearer
/// token (ADR-0043 Phase 3 multi-dialect + OAuth). The token is refreshed through
/// the credential domain's OAuth helper: `GEMINI_ACCESS_TOKEN` if set, else
/// `gcloud auth print-access-token` (which holds the long-lived Google grant).
/// Config from the environment: `GEMINI_PROJECT` (required), `GEMINI_LOCATION`
/// (default `global`), `GEMINI_MODEL` (default `gemini-2.5-flash`). Exposed as a
/// server mode so the TypeScript e2e can drive a real Gemini Run — proving the
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
    let host = if location == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    let base_url = format!("https://{host}/v1/projects/{project}/locations/{location}/");
    let executor = GenaiExecutor::from_materialized_endpoint(
        awaken_provider_genai::AdapterKind::Vertex,
        base_url,
        token,
    )
    .expect("explicit Vertex client construction");
    mount_platform(
        runtime_resource_host_with_deployment(Arc::new(executor), model, scenario_deployment())
            .await,
    )
    .await
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
            workspace_id: None,
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
    let clock = awaken_time::system_clock();
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
        clock.as_ref(),
    )
    .await
    .expect("enter credential");
    let (direct_policy, direct_holder) = attempt_credential::direct_provider_execution();
    let resolver = awaken_control::model_publication::CatalogModelPublicationResolver::from_repo(
        catalog_repo,
        cred_repo.clone(),
        clock.clone(),
    )
    .with_direct_provider_credential_execution(direct_policy, direct_holder);
    let published = awaken_config_service::ModelPublicationResolver::resolve_models(
        &resolver,
        &awaken_tenancy::ScopeId::from("ws"),
        &ModelSelection::Pinned(
            ModelBinding::parse("anthropic", &model, "genai")
                .unwrap_or_else(|error| panic!("ANTHROPIC_MODEL `{model}`: {error}")),
        ),
        &[],
    )
    .await
    .expect("publish model candidate");
    let materializer = awaken_credential_materializer::CredentialInferenceMaterializer::new(
        Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
            cred_repo,
            secrets,
            clock.clone(),
        )),
        clock,
    );
    let context = attempt_credential::context(&published.primary);
    let executor = materializer
        .materialize_candidate(&published.primary, &context)
        .await
        .expect("materialize published model candidate");
    mount_platform(
        runtime_resource_host_with_deployment(
            executor,
            published.primary.binding().identity().1.to_owned(),
            scenario_deployment(),
        )
        .await,
    )
    .await
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
    const OAUTH_MINTED_KEY: &str = "oauth-minted-key"; // awaken-allow(secret):
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
            workspace_id: None,
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
        replacement_of: None,
        workspace_id: "ws".into(),
        kind: CredentialKind::Oauth,
        descriptor: None,
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
    let clock = awaken_time::system_clock();
    let (direct_policy, direct_holder) = attempt_credential::direct_provider_execution();
    let resolver = awaken_control::model_publication::CatalogModelPublicationResolver::from_repo(
        catalog_repo,
        cred_repo.clone(),
        clock.clone(),
    )
    .with_direct_provider_credential_execution(direct_policy, direct_holder);
    let published = awaken_config_service::ModelPublicationResolver::resolve_models(
        &resolver,
        &awaken_tenancy::ScopeId::from("ws"),
        &ModelSelection::Pinned(
            ModelBinding::parse("anthropic", &model, "genai")
                .unwrap_or_else(|error| panic!("ANTHROPIC_MODEL `{model}`: {error}")),
        ),
        &[],
    )
    .await
    .expect("publish OAuth model candidate");
    let materializer = awaken_credential_materializer::CredentialInferenceMaterializer::new(
        Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
            cred_repo,
            secrets,
            clock.clone(),
        )),
        clock,
    );
    let context = attempt_credential::context(&published.primary);
    let executor = materializer
        .materialize_candidate(&published.primary, &context)
        .await
        .expect("materialize published OAuth candidate");
    mount_platform(
        runtime_resource_host_with_deployment(
            executor,
            published.primary.binding().identity().1.to_owned(),
            scenario_deployment(),
        )
        .await,
    )
    .await
}

/// Build a direct/non-Managed compatibility router from host-static Skills.
/// Managed-Agent scenarios must use [`build_router_with_managed_skills`] so the
/// Resources binding and immutable version bytes remain the sole authority.
pub async fn build_router_with_skills(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    skills: Vec<SkillSpec>,
) -> Router {
    mount_platform(
        resource_host(llm, model_ref)
            .await
            .map_dependencies(|host| host.with_skills(skills)),
    )
    .await
}

/// Build a Managed-Agent router whose deterministic Skill fixtures travel the
/// production path: Resources catalog version -> Agent binding -> frozen Session
/// bytes -> filesystem progressive disclosure. This is the one Managed Skill
/// fixture owner; it deliberately installs no host-static compatibility copy.
pub async fn build_router_with_managed_skills(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    skills: Vec<SkillSpec>,
) -> Router {
    let model_ref = model_ref.into();
    let platform = resource_host(llm, model_ref.clone()).await;
    let bindings = platform.publish_managed_skills(&skills).await;
    let publication = fixed_host_backend_publication("assistant", &model_ref, bindings);
    let platform =
        platform.map_dependencies(|host| host.with_agent_publications(publication.clone()));
    mount_with_agent_source(platform, publication).await
}

/// A router whose Outcomes are graded by the named Judge Agent through the
/// ordinary Run boundary.
pub async fn build_graded_router(
    llm: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
    judge_agent_id: impl Into<String>,
) -> Router {
    mount_platform(
        resource_host(llm, model_ref)
            .await
            .map_dependencies(|host| {
                host.with_judge(judge_agent_id)
                    .unwrap_or_else(|error| panic!("scenario host model reference: {error}"))
            }),
    )
    .await
}

/// The default deterministic router (echo model) — the CI / e2e server.
pub async fn build_echo_router() -> Router {
    build_router(Arc::new(EchoModel), "echo-model").await
}

/// Ephemeral Resource authorities behind the production workspace-path adapter. This
/// is the sole multi-workspace process fixture for volatile resource semantics;
/// it decorates the canonical scenario Host instead of defining another store.
pub async fn build_ephemeral_resource_router() -> Router {
    let flat = mount_platform(resource_host(Arc::new(EchoModel), "echo-model").await).await;
    awaken_coordinator::workspace_path::with_workspace_path_addressing(flat)
}

/// A router whose model reports the media it received (the multimodal e2e): every
/// protocol adapter must carry an image block through to the model for the probe
/// reply to name its media type.
pub async fn build_vision_router() -> Router {
    build_router(Arc::new(VisionProbeModel), "vision-probe").await
}

/// A router with a client-executed tool `submit_answer` (the custom-tool e2e).
pub async fn build_custom_router() -> Router {
    build_custom_router_and_host().await.0
}

/// The same custom-tool scenario with its owning Host for scope-aware tests.
pub async fn build_custom_router_and_host() -> (Router, Arc<ExecutorComponents>) {
    let client_tools = HashSet::from(["submit_answer".to_string()]);
    let (model, model_ref) = scenario_model(Arc::new(CustomToolModel), "custom");
    let host = resource_host(model, model_ref)
        .await
        .map_dependencies(|host| host.with_client_tools(client_tools));
    build_router_and_host_from(host).await
}

/// A router whose agent activates the tool state machine (the state-machine e2e).
/// The machine defines `glob` as a single transition out of the initial state, so
/// the driving model's first `glob` advances it (emitting a context message) and
/// the second is a precondition violation the gate denies.
pub async fn build_statemachine_router() -> Router {
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
    let host = resource_host(model, model_ref)
        .await
        .map_dependencies(|host| host.with_state_machine(machine));
    mount_platform(host).await
}

/// A richer tool state machine (coverage): a per-key machine (`key`/`key_normalizer`)
/// whose transitions gate on the tool *result* (`when`) rather than just its args —
/// exercising the result matchers (status + content) and the key template. The
/// driving model calls `glob` twice with the same pattern: the first advances
/// `s0 -> s1` on a `success` result (its emit fires), the second advances `s1 -> s2`
/// (terminal) on `any` result (a second emit). No violation — both calls advance.
pub async fn build_statemachine_rich_router() -> Router {
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
    let host = resource_host(model, model_ref)
        .await
        .map_dependencies(|host| host.with_state_machine(machine));
    mount_platform(host).await
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
        ctx: &awaken_tool_contract::tool::ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> awaken_runtime_contract::permission::GateDecision {
        awaken_runtime_contract::permission::GateDecision::plugin_gate(
            awaken_runtime_contract::permission::GateOutcome::Schedule {
                correlation_id: format!("sched-{}", ctx.call_id),
                action_kind: None,
            },
        )
    }
}

/// A router whose tool gate defers every tool call as a `ScheduledAction`
/// (ADR-0020, slice E). Drive it with `typed durable ingress` so the dispatch
/// worker performs the deferred actions out of band: the probe model's
/// write→read tool calls are each scheduled and auto-performed, so the run
/// completes without any human confirmation.
pub async fn build_schedule_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(ProbeModel), "schedule");
    let host = resource_host_with_deployment(model, model_ref, scenario_deployment())
        .await
        .map_dependencies(|host| host.with_gate_override(Arc::new(ScheduleGate)));
    mount_platform(host).await
}

/// A router that routes a fixed Managed `send_message` command for `researcher`
/// to a REMOTE A2A Agent at `AWAKEN_REMOTE_AGENT_URL`. The ordinary child Run
/// crosses `message:send` → poll `get_task`, then projects its reply on the child
/// Thread independently from the parent's admission receipt.
pub async fn build_remote_delegation_router() -> Router {
    let url = std::env::var("AWAKEN_REMOTE_AGENT_URL")
        .expect("AWAKEN_REMOTE_AGENT_URL must be set for delegate-remote mode");
    let security_fingerprint = std::env::var("AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT")
        .expect("AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT must be set for delegate-remote mode");
    let (model, model_ref) = scenario_model(Arc::new(DelegatingModel), "delegate-remote");
    // Both parent and child are ordinary immutable publications. The child's
    // resolved backend selects the shared A2A attempt executor; delegation owns
    // no transport registry or protocol-specific execution path.
    let mut tools = awaken_session_executor::authorable_tools();
    tools.retain(|tool| tool.kind == ToolKind::AgentDelegation);
    let assistant = ExecutableAgentSnapshot::builder("assistant")
        .model(
            ModelBinding::parse("default", &model_ref, "default")
                .unwrap_or_else(|error| panic!("scenario model `{model_ref}`: {error}")),
        )
        .tools(tools)
        .metadata(awaken_runtime_contract::snapshot::AgentSnapshotMetadata {
            source: awaken_runtime_contract::snapshot::AgentConfigRevisionRef {
                agent_id: AgentId("assistant".into()),
                revision: 1,
            },
            ..Default::default()
        })
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: Some(1),
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let researcher = ExecutableAgentSnapshot::builder("researcher")
        .resolved_model(
            ResolvedModelCandidate::try_remote(
                ModelBinding::parse("remote", "", format!("a2a:{url}")).unwrap_or_else(|error| {
                    panic!("AWAKEN_REMOTE_AGENT_URL `{url}` is not an A2A backend: {error}")
                }),
                awaken_tenancy::ScopeId::from("default"),
                None,
                security_fingerprint,
            )
            .expect("environment supplied a coherent remote candidate"),
        )
        .metadata(awaken_runtime_contract::snapshot::AgentSnapshotMetadata {
            source: awaken_runtime_contract::snapshot::AgentConfigRevisionRef {
                agent_id: AgentId("researcher".into()),
                revision: 1,
            },
            ..Default::default()
        })
        .build();
    let publication = fixed_agent_publication([assistant, researcher]);
    let host = resource_host(model, model_ref)
        .await
        .map_dependencies(|host| {
            host.with_agent_publications(publication.clone())
                .with_remote_attempt_executor(awaken_process_startup::a2a_attempt_executor(None))
        });
    mount_with_agent_source(host, publication).await
}

/// A deterministic model for the skills e2e (ADR-0036). A Managed-filesystem
/// prompt makes it read the advertised `SKILL.md`; a filesystem-free Session uses
/// `list_skills` then `Skill`. Both delivery modes therefore prove their own
/// discover → load → use path without one test fixture forcing the other mode.
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
        let system_text = request
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(|message| extract_text(&message.content))
            .collect::<Vec<_>>()
            .join("\n");
        let managed_skill_path = system_text
            .split('`')
            .find(|part| part.ends_with("/SKILL.md"))
            .map(str::to_string);
        let output = match last.role {
            Role::User if managed_skill_path.is_some() => {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "read-skill".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({
                        "path": managed_skill_path.expect("guarded managed Skill path")
                    }),
                }])
            }
            Role::User => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "l".into(),
                tool_id: "list_skills".into(),
                arguments: serde_json::json!({}),
            }]),
            Role::Tool if last_text.contains("\"skills\"") => {
                // The catalog came back — activate only the canonical id it
                // actually advertised. A malformed catalog fails visibly instead
                // of reviving the removed legacy `greet` identity.
                let skill = serde_json::from_str::<serde_json::Value>(&last_text)
                    .ok()
                    .and_then(|value| value["skills"][0]["id"].as_str().map(str::to_string));
                match skill {
                    Some(skill) => AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "s".into(),
                        tool_id: "Skill".into(),
                        arguments: serde_json::json!({ "skill": skill }),
                    }]),
                    None => AssistantOutput::text(format!("INVALID-SKILL-CATALOG: {last_text}")),
                }
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
pub async fn build_skills_router() -> Router {
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING-FROM-SKILL");
    let review = SkillSpec::new("review", "Review", "review code", "REVIEW-BODY");
    let (model, model_ref) = scenario_model(Arc::new(SkillDrivingModel), "skills");
    build_router_with_managed_skills(model, model_ref, vec![greet, review]).await
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
    let skill_id = awaken_resource_contract::skill_catalog_id("greet");
    let publication = fixed_host_backend_publication(
        "assistant",
        "default",
        vec![awaken_agent_contract::AgentSkillBinding::custom(skill_id)],
    );
    let host = resource_host_with_deployment(model, model_ref, deployment)
        .await
        .map_dependencies(|host| host.with_agent_publications(publication.clone()));
    mount_with_agent_source(host, publication).await
}
pub async fn build_config_router() -> Router {
    // The MODEL is chosen by `scenario_model` (in-process echo, or the real provider
    // pointed at the fake upstream when `AWAKEN_MODEL_SOURCE=http`); the fake upstream
    // is what drives the seeded assistant through its admin tools in the run e2e.
    let (model, model_ref) = scenario_model(Arc::new(InstructionEchoModel), "config");
    let deployment = scenario_deployment();
    let platform_workspace = crate::deployment::installation_workspace(&deployment);
    // Config/Session durability decision table: no storage root keeps the
    // disposable scenario in memory; a configured root opens the canonical
    // SQLite ConfigStore beside the durable Session/Dispatch authorities. A
    // restart must never retain the latter while silently replacing the former.
    let store = Arc::new(match deployment.storage_dir.as_deref() {
        Some(root) => awaken_config_store::SqliteConfigStore::open(
            root.join("config.db")
                .to_str()
                .expect("scenario storage path is UTF-8"),
        )
        .await
        .expect("open durable scenario config store"),
        None => awaken_config_store::SqliteConfigStore::open_in_memory()
            .await
            .expect("open ephemeral scenario config store"),
    });
    // Scope-keyed tool visibility (ADR-0052 D3): every scope sees the advertised
    // (global) tools; only the reserved admin scope additionally sees the four
    // management descriptors, so a config naming an `admin_*` tool compiles only there.
    let global = awaken_session_executor::authorable_tools();
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
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        awaken_plugin_composition::platform_plugin_configurations(),
    ));
    let plane = awaken_config_service::ConfigPlane::new(service.clone(), store, tools);
    // The management tool executables, backed by real ports (D3/D4): the capability
    // reader reads the shared catalog + advertised tools; the validator runs the same
    // compile check as `/v1/config/agents/validate` on drafts (in the tenant scope).
    let reader = Arc::new(awaken_control::CatalogCapabilityReader::new(
        catalog_repo.clone(),
        &global,
        &awaken_plugin_composition::platform_plugin_capabilities(),
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
        awaken_protocol_managed_testkit::environment_fixture(awaken_time::system_clock())
            .application,
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    let host = resource_host_with_deployment(model, model_ref, deployment)
        .await
        .map_dependencies(|host| {
            host.with_local_workspace(platform_workspace.clone())
                .with_agent_publications(executable_agent_catalog.clone())
                .with_admin_tools(admin_execs)
                .with_remote_attempt_executor(awaken_process_startup::a2a_attempt_executor(None))
        });
    let (host, resources) = host.into_dependencies();
    // Rebuild the disposable executable catalog through the same registration
    // recovery used by Control. This replays durable publication truth; it does
    // not create a second warm-install path or reinterpret a claimed Run.
    plane
        .recover_all_registrations(&platform_workspace)
        .await
        .expect("recover durable scenario Agent registrations");
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
                awaken_time::system_clock(),
            ),
        )),
    ));
    // Workspace-path addressing (ADR-0048/0052 D2): `/v1/workspaces/{ws}/config/...`
    // is rewritten to the flat config route and stamped with `{ws}` as the scope, so
    // the reserved admin scope is reachable and the tenant/default scope is fenced.
    // Scenario persistence decision table: storage_dir absent -> the canonical scenario
    // mount supplies an ephemeral Session repository; storage_dir present -> it
    // supplies sessions.db beside runtime truth. The Agent source is an added port,
    // never a reason to rebuild ManagedState through a parallel in-memory path.
    let (flat, host) = scenario_platform::mount_parts_with_agent_source_on_lifecycle(
        host,
        resources,
        executable_agent_catalog,
        awaken_service_lifecycle::ServiceLifecycle::new(),
    )
    .await;
    let scenario_dispatch = host
        .session_runs()
        .coordinator_dispatch()
        .expect("config scenario installs one Dispatch authority");
    let flat = flat
        .merge(config_router(plane))
        .merge(agents)
        .merge(session_reservation_scenario_router(scenario_dispatch).await);
    let flat =
        awaken_coordinator::workspace_path::with_platform_workspace(flat, platform_workspace);
    awaken_coordinator::workspace_path::with_workspace_path_addressing(flat)
}

#[derive(Deserialize)]
struct SessionReservationScenarioRequest {
    request: awaken_dispatch::RunDispatch,
    reservation_ttl_ms: u64,
}

/// Scenario-only adapter for stopping at the real persistence-before-activity
/// crash boundary. It delegates the production Dispatch command and owns no
/// state, transition, clock, or SQL mutation of its own.
async fn session_reservation_scenario_router(
    dispatch: Arc<awaken_dispatch::AnyCoordinatorDispatch>,
) -> Router {
    async fn reserve(
        State(dispatch): State<Arc<awaken_dispatch::AnyCoordinatorDispatch>>,
        Json(command): Json<SessionReservationScenarioRequest>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        match dispatch
            .reserve_session_run(command.request, command.reservation_ttl_ms)
            .await
        {
            Ok(outcome) => (
                StatusCode::OK,
                Json(serde_json::json!({ "outcome": outcome })),
            ),
            Err(error) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            ),
        }
    }

    Router::new()
        .route("/v1/scenario/session-run/reserve", post(reserve))
        .with_state(dispatch)
}

#[cfg(test)]
mod compatible_endpoint_tests {
    use super::{default_anthropic_compatible_model, normalize_anthropic_compatible_base};

    #[test]
    fn kimi_coding_root_is_canonicalized_for_the_messages_provider() {
        // Causes: C1 Kimi coding root omits /v1; C2 it already includes /v1;
        // C3 another Anthropic-compatible base. Effect: one trailing-slash API
        // base, adding /v1 only for C1. Rules K1/K2/K3 map directly to cases.
        assert_eq!(
            normalize_anthropic_compatible_base("https://api.kimi.com/coding/"),
            "https://api.kimi.com/coding/v1/"
        );
        assert_eq!(
            normalize_anthropic_compatible_base("https://api.kimi.com/coding/v1/"),
            "https://api.kimi.com/coding/v1/"
        );
        assert_eq!(
            normalize_anthropic_compatible_base("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/"
        );
    }

    #[test]
    fn omitted_model_uses_the_endpoint_vocabulary() {
        assert_eq!(
            default_anthropic_compatible_model("https://api.kimi.com/coding/v1/"),
            "claude-sonnet-5"
        );
        assert_eq!(
            default_anthropic_compatible_model("https://api.anthropic.com/v1/"),
            "claude-3-5-haiku-latest"
        );
    }
}
