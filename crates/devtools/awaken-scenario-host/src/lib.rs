//! Test-only scenario host: the deterministic mock models and the `build_*_router`
//! scenario assemblies the e2e harness + integration tests drive. Extracted from
//! `awaken-server` so the product crate carries zero mocks. It reuses the
//! product crate's now-`pub` data-plane assembly helpers (`mount` / `mount_with_managed`
//! / `data_subject_plane`) and production executors via `awaken_server::`.

mod acp_gateway;
mod attempt_credential;
mod composition;
mod deployment;
mod model_publication;
mod models;
pub use crate::models::*;
pub use acp_gateway::build_acp_gateway_router;
pub use composition::build_unscoped_resource_router;
pub use deployment::scenario_deployment;

mod scenario_shell;
use composition::{
    fixed_host_backend_publication, mount, mount_with_environments,
    mount_with_environments_and_agent_source, mount_with_host_backend_publication,
};
use deployment::{resource_host, resource_host_with_deployment, scenario_storage_dir};
use scenario_shell::{scenario_argv, scenario_host_acp_cli, scenario_shell_argv};

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_protocol_managed::ManagedState;
use awaken_provider_genai::{AdapterKind, GenaiExecutor};
use awaken_runtime_contract::StaticPublishedAgentSnapshots;
use awaken_runtime_contract::agent_bindings::AgentBindings;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate, ToolKind};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use axum::Router;

// The managed-agents service layer (`awaken-runtime-host`): the neutral host,
// the two port adapters, the per-plane routers, and the authoring/transport
// re-exports a composition root (and the integration tests) drive directly.
pub use awaken_managed_routers::{default_models, files_router, models_router};
pub use awaken_runtime_host::{
    ConfigService, ExtMcpProbe, HostResume, InferenceExecutorMaterializer, ManagedHost,
    ProtocolHost, SharedHost, SkillContext, SkillSpec, ThreadEvent, ThreadEventHub, VaultRefresher,
    advertised_tools, capabilities_router, config_router, content_fingerprint, durable_ops_router,
    memory_stores_router_with_catalog, parse_skill_md, skills_router,
};

/// An [`InferenceExecutorMaterializer`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
use awaken_server::mount_with_managed;
use awaken_server::placement;
struct RouteProvider;

impl InferenceExecutorMaterializer for RouteProvider {
    fn materialize_pinned(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if !matches!(
            candidate.provisioning,
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        ) {
            return None;
        }
        let model_ref = candidate.binding.model_ref.as_str();
        let labeled: Arc<dyn LlmExecutor> = match model_ref {
            "fast" => Arc::new(LabelModel("fast")),
            "slow" => Arc::new(LabelModel("slow")),
            "default" => Arc::new(LabelModel("default")),
            _ => return None,
        };
        // Over the real wire the label rides in the model name the session bound
        // (GenaiExecutor sends `model_binding.model_ref`), which the fake upstream's
        // `label` behavior echoes back — so we keep the route's ref and only swap the
        // executor. `scenario_model`'s ref (the env model name) is intentionally
        // discarded here; the routing ref is the observable, not the wire model.
        Some(scenario_model(labeled, model_ref).0)
    }
}

/// A router whose per-session/per-turn model selection routes to distinct labeled
/// executors (R1/R2/R5/R6). `AWAKEN_MODEL_MODE=model-route`.
pub fn build_model_route_router() -> Router {
    let (default_model, _) = scenario_model(Arc::new(LabelModel("default")), "default");
    mount(Arc::new(
        resource_host(default_model, "default")
            .with_inference_materializer(Arc::new(RouteProvider)),
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
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";

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
    mount(Arc::new(host))
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
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING-FROM-SKILL");
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "full-chain");
    let host = resource_host(model, model_ref)
        .with_skills(vec![greet])
        .with_skill_store(scenario_skill_store_dir());
    mount(Arc::new(host))
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
fn scenario_resource_catalog() -> Arc<dyn awaken_protocol_managed::ResourceCatalog> {
    let root = scenario_storage_dir();
    let Some(root) = root else {
        return Arc::new(
            awaken_admin_config_api::SqliteAdminStore::open_in_memory()
                .expect("open ephemeral scenario resource catalog"),
        );
    };
    std::fs::create_dir_all(&root).expect("create scenario resource registry directory");
    let catalog =
        awaken_admin_config_api::SqliteAdminStore::open(&root.join("admin.db").to_string_lossy())
            .expect("open durable scenario resource catalog");
    catalog
        .migrate_legacy_memory_stores()
        .expect("migrate legacy scenario MemoryStore rows");
    Arc::new(catalog)
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
/// Run this process as a database-less **echo worker** of the cell server at
/// `upstream` — the test-only drain the worker-pool e2e spawns (`AWAKEN_ROLE=worker`
/// on this scenario host). Its dispatch pool claims/settles runs over the server's
/// dispatch transport and posts committed facts back over the commit ingest
/// (`with_upstream`); it holds no store and serves no HTTP. A deterministic
/// [`EchoModel`] keeps the worker self-contained (no upstream model needed), so the
/// e2e can assert the worker drove the run without configuring a provider. The
/// PRODUCTION worker (real per-run model resolution) lives in `awaken-worker`.
pub async fn run_echo_worker(
    upstream: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    struct EchoWorkerProvider;

    impl InferenceExecutorMaterializer for EchoWorkerProvider {
        fn supported_access_schemes(&self) -> &'static [&'static str] {
            &[awaken_runtime_host::HOST_EXECUTOR_CAPABILITY]
        }

        fn materialize_pinned(
            &self,
            candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Option<Arc<dyn LlmExecutor>> {
            if !matches!(
                candidate.provisioning,
                awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
            ) {
                return None;
            }
            Some(Arc::new(EchoModel))
        }
    }

    awaken_worker::run_with_inference_materializer(upstream, Arc::new(EchoWorkerProvider)).await
}

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
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"c1\",\"status\":\"completed\",\"content\":[{\"type\":\"content\",\"content\":{\"type\":\"text\",\"text\":\"file body\"}}]}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"acp-jsonrpc reply\"}}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

/// Official-wire ACP agent that asks the host to authorize one mutating tool.
/// The first process is cancelled at the durable permission boundary. On resume,
/// the executor reloads the same ACP session and answers the repeated request
/// with the user's one-shot decision; the marker makes allow and deny externally
/// distinguishable without performing an effect in this deterministic fixture.
const FAKE_ACP_PERMISSION_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true}}}';; \
        *'\"id\":2'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"permission-s1\"}}';; \
        *'\"id\":3'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"permission-s1\",\"toolCall\":{\"toolCallId\":\"permission-call\",\"title\":\"bash\",\"rawInput\":{\"command\":\"printf permission\"}},\"options\":[{\"optionId\":\"ok\",\"name\":\"Allow\",\"kind\":\"allow_once\"},{\"optionId\":\"no\",\"name\":\"Reject\",\"kind\":\"reject_once\"}]}}';; \
        *'\"id\":42'*'\"ok\"'*) \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"permission-s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"ACP-PERMISSION-ALLOWED\"}}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; exit 0;; \
        *'\"id\":42'*'\"no\"'*) \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"permission-s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"ACP-PERMISSION-DENIED\"}}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; exit 0;; \
        *'\"id\":42'*) exit 0;; \
      esac; \
    done";

const SLOW_FAKE_ACP_SCRIPT: &str = "read _prompt; sleep 3; \
    printf '%s\\n' '{\"type\":\"message\",\"text\":\"ACP-SLOW-TURN\"}'; \
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";

fn slow_acp_source() -> awaken_run_executor_acp::SubprocessChannelSource {
    awaken_run_executor_acp::SubprocessChannelSource::new(
        awaken_run_executor_acp::AcpLaunch::custom(
            scenario_shell_argv(SLOW_FAKE_ACP_SCRIPT),
            vec![],
        ),
    )
}

/// Slow newline ACP process used to exercise live pause and safe-boundary resume.
pub fn build_acp_control_router() -> Router {
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(Arc::new(
        slow_acp_source(),
    )));
    mount_with_host_backend_publication(
        resource_host(Arc::new(EchoModel), "awaken").with_acp(acp),
        "acp-agent",
        "acp:claude",
    )
}

struct FailSecondAcpSource {
    inner: awaken_run_executor_acp::SubprocessChannelSource,
    opens: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_run_executor_acp::AgentChannelSource for FailSecondAcpSource {
    async fn open(
        &self,
        activation: &awaken_runtime_contract::activation::RunActivation,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<awaken_run_executor_acp::AgentSession, awaken_run_executor_acp::OpenError> {
        if self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
            return Err(awaken_run_executor_acp::OpenError(
                "deliberate replacement launch failure".to_string(),
            ));
        }
        awaken_run_executor_acp::AgentChannelSource::open(&self.inner, activation, context).await
    }
}

/// First ACP turn starts normally; a live-inbox continuation reaches the normal
/// relaunch seam, where the deterministic source fails. Production error handling
/// is exercised without adding a diagnostic endpoint to the server.
pub fn build_acp_relaunch_failure_router() -> Router {
    let source = Arc::new(FailSecondAcpSource {
        inner: slow_acp_source(),
        opens: std::sync::atomic::AtomicUsize::new(0),
    });
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    mount_with_host_backend_publication(
        resource_host(Arc::new(EchoModel), "awaken").with_acp(acp),
        "acp-agent",
        "acp:claude",
    )
}

/// [`build_acp_router`]'s official-wire twin: `acp:*` sessions drive the fake agent
/// over real ACP JSON-RPC (the [`awaken_run_executor_acp::Codec::Acp`] driver),
/// proving the production codec end-to-end. `AWAKEN_MODEL_MODE=acp-jsonrpc`.
pub fn build_acp_jsonrpc_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        scenario_shell_argv(FAKE_ACP_JSONRPC_SCRIPT),
        vec![],
    );
    let source = Arc::new(
        awaken_run_executor_acp::SubprocessChannelSource::new(launch)
            .with_codec(awaken_run_executor_acp::Codec::Acp),
    );
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    mount_with_host_backend_publication(
        resource_host(Arc::new(EchoModel), "awaken").with_acp(acp),
        "acp-agent",
        "acp:claude",
    )
}

/// Durable allow/deny coverage for ACP `session/request_permission`, through the
/// same per-Session policy and Managed resume API used by native execution.
pub fn build_acp_permission_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        scenario_shell_argv(FAKE_ACP_PERMISSION_SCRIPT),
        vec![],
    );
    let source = Arc::new(
        awaken_run_executor_acp::SubprocessChannelSource::new(launch)
            .with_codec(awaken_run_executor_acp::Codec::Acp),
    );
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    mount_with_host_backend_publication(
        resource_host(Arc::new(EchoModel), "awaken").with_acp(acp),
        "acp-agent",
        "acp:claude",
    )
}

/// A router where a session can select `runtime: "acp:*"` to run on an external
/// ACP CLI (here the fake agent), else the native echo model (R3/R4/R7).
/// `AWAKEN_MODEL_MODE=acp`.
pub fn build_acp_router() -> Router {
    let launch =
        awaken_run_executor_acp::AcpLaunch::custom(scenario_shell_argv(FAKE_ACP_SCRIPT), vec![]);
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    // The native fallback model runs over the real wire when the harness asks
    // (`AWAKEN_MODEL_SOURCE=http`); the ACP `runtime:"acp:*"` path is unaffected — it
    // runs on the real CLI subprocess either way.
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "awaken");
    mount_with_host_backend_publication(
        resource_host(model, model_ref).with_acp(acp),
        "acp-agent",
        "acp:claude",
    )
}

/// A JSON-RPC fake agent (the codec [`ProjectingChannelSource`] speaks) that echoes
/// the model env it was LAUNCHED with into its reply: `base=<ANTHROPIC_BASE_URL>` and
/// `keypfx=<first 6 chars of ANTHROPIC_API_KEY>` — a prefix, never the full secret. So
/// an e2e can assert the host resolved+projected the gateway URL + a `lease-` token
/// (D-R2), not a raw provider key. Its own [`FAKE_ACP_JSONRPC_SCRIPT`] twin (untouched)
/// keeps the plain "acp-jsonrpc reply" for the codec e2e.
const FAKE_ACP_GATEWAY_JSONRPC_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"acp-env base=%s keypfx=%s\"}}}}\\n' \"$ANTHROPIC_BASE_URL\" \"$(printf %s \"$ANTHROPIC_API_KEY\" | cut -c1-6)\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

const FAKE_ACP_DISCOVERY: awaken_run_executor_acp::AcpDiscoverySpec =
    awaken_run_executor_acp::AcpDiscoverySpec {
        version: awaken_run_executor_acp::AcpProbeCommand {
            executable: "/bin/true",
            args: &[],
        },
        login: awaken_run_executor_acp::AcpLoginProbe {
            command: awaken_run_executor_acp::AcpProbeCommand {
                executable: "/bin/true",
                args: &[],
            },
            rules: &[awaken_run_executor_acp::AcpLoginRule {
                predicate: awaken_run_executor_acp::AcpProbePredicate::ExitSuccess,
                state: awaken_runtime_contract::CredentialObservationState::Available,
                reason_code: "scenario_fixture_available",
            }],
            remediation: "scenario fixture requires no login",
        },
        install_remediation: "scenario fixture is built in",
    };

/// The [`FAKE_ACP_GATEWAY_JSONRPC_SCRIPT`] wired as a real [`AcpCli`] row, so the
/// projecting launch path resolves + projects the model env onto it exactly as a
/// production CLI (its delivery keys are the `ANTHROPIC_*` ones the script echoes).
/// Used only by [`build_acp_gateway_router`] to exercise host model resolution
/// (self-credentialed vs cloud-managed gateway, D-R2) end to end.
const FAKE_ACP_CLI: awaken_run_executor_acp::AcpCli = awaken_run_executor_acp::AcpCli {
    id: "fake",
    display_name: "Fake ACP",
    description: "Deterministic scenario ACP fixture.",
    acquisition: awaken_run_executor_acp::AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", FAKE_ACP_GATEWAY_JSONRPC_SCRIPT],
    },
    discovery: FAKE_ACP_DISCOVERY,
    container_argv: &["/bin/sh", "-c", FAKE_ACP_GATEWAY_JSONRPC_SCRIPT],
    model_delivery: Some(awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        credential_env: &["ANTHROPIC_API_KEY"],
        aliases: &[],
    }),
    backend_model_interface: awaken_run_executor_acp::BackendModelInterface::Unsupported,
    managed_credential_delivery: awaken_run_executor_acp::ManagedCredentialDelivery::ProcessSecret,
    auth_method_id: None,
    mcp_interface: awaken_run_executor_acp::McpInterface::AcpSession,
    config_home_env: Some("CLAUDE_CONFIG_DIR"),
    config_home_aliases: &[],
    memory_entrypoint: "CLAUDE.md",
    session_export_excludes: &[],
    // The fake gateway CLI keeps no local session (it is a scripted stand-in).
    session_persistence: awaken_run_executor_acp::SessionPersistence::None,
    context_window_env: None,
    env: &[],
};

/// A fake ACP agent (JSON-RPC, shell builtins only) that reports whether the
/// `session/new` request it received carried the session's MCP server and, if so,
/// whether the endpoint is the host-owned loopback relay rather than a provider endpoint
/// carrying a raw vault token. It captures the `session/new` line (`id:2`) and, on the
/// prompt (`id:3`), classifies it into its agent message: `saw-calc` if the `calc` server
/// name crossed, `host-relay` if the URL points at the per-session loopback relay. The
/// managed-API e2e can therefore assert the whole D6→D5 chain (session `mcp_servers` →
/// staged → host relay → `session/new`) without exposing authorization material to the
/// external ACP process. Any retained `session-mcp:` credential marker is classified as
/// a leak so the E2E cannot accidentally bless the deleted fallback.
const FAKE_ACP_MCP_ECHO_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          N=noname; case \"$SN\" in *calc*) N=saw-calc;; esac; \
          A=noref; case \"$SN\" in *'/sesn_'*) A=host-relay;; *'session-mcp:'*) A=credential-leaked;; esac; \
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"mcp %s %s\"}}}}\\n' \"$N\" \"$A\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

/// [`FAKE_ACP_MCP_ECHO_SCRIPT`] wired as an `AcpSession` (session/new delivery) CLI row,
/// so the projecting launch path hands it the run's staged MCP servers through the
/// `session/new` request the [`awaken_run_executor_acp::Codec::Acp`] driver builds.
const FAKE_ACP_MCP_CLI: awaken_run_executor_acp::AcpCli = awaken_run_executor_acp::AcpCli {
    id: "fake-mcp",
    display_name: "Fake ACP MCP",
    description: "Deterministic scenario ACP MCP fixture.",
    acquisition: awaken_run_executor_acp::AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", FAKE_ACP_MCP_ECHO_SCRIPT],
    },
    discovery: FAKE_ACP_DISCOVERY,
    container_argv: &["/bin/sh", "-c", FAKE_ACP_MCP_ECHO_SCRIPT],
    model_delivery: Some(awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        credential_env: &["ANTHROPIC_API_KEY"],
        aliases: &[],
    }),
    backend_model_interface: awaken_run_executor_acp::BackendModelInterface::Unsupported,
    managed_credential_delivery: awaken_run_executor_acp::ManagedCredentialDelivery::ProcessSecret,
    auth_method_id: None,
    mcp_interface: awaken_run_executor_acp::McpInterface::AcpSession,
    config_home_env: Some("CLAUDE_CONFIG_DIR"),
    config_home_aliases: &[],
    memory_entrypoint: "CLAUDE.md",
    session_export_excludes: &[],
    session_persistence: awaken_run_executor_acp::SessionPersistence::None,
    context_window_env: None,
    env: &[],
};

/// A launch resolver with a fixed (dummy) model: the fake CLI ignores the model env, so
/// this keeps the scenario off the "model config via env" path — no ANTHROPIC_* need be
/// exported for the resolver to succeed. Only the MCP/`session/new` wire is under test.
struct FixedAcpModel;
impl awaken_run_executor_acp::LaunchResolver for FixedAcpModel {
    fn model(
        &self,
        _activation: &awaken_runtime_contract::activation::RunActivation,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> std::result::Result<
        awaken_run_executor_acp::ResolvedModel,
        awaken_run_executor_acp::OpenError,
    > {
        Ok(awaken_run_executor_acp::ResolvedModel::managed(
            "http://fake",
            "fake",
            None,
            None,
        ))
    }
}

/// The managed plane (vault + MCP staging + config plane, via awaken-cli's real
/// assembly) with an ACP backend wired on: a session that selects `runtime: "acp:*"` and
/// declares `mcp_servers` (bound to a vault credential) has its staged servers projected
/// α-secretless into the fake CLI's `session/new`. Proves the D6→D5 chain end to end
/// through the HTTP managed API. `AWAKEN_MODEL_MODE=acp-managed-mcp`.
pub async fn build_acp_managed_mcp_router() -> Router {
    let source = Arc::new(awaken_run_executor_acp::ProjectingChannelSource::new(
        scenario_host_acp_cli(FAKE_ACP_MCP_CLI),
        Arc::new(FixedAcpModel),
    ));
    let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    awaken_cli::build_management_router_with_host_customizer(
        Arc::new(McpToolModel),
        ModelBinding::new("scenario", "acp-managed-mcp", "acp:fake-mcp"),
        move |host| host.with_acp(executor),
    )
    .await
}

/// The REAL-CLI, REAL-LLM twin of [`build_acp_managed_mcp_router`]: the managed plane
/// with the **actual** `claude --acp` adapter (the catalog `claude` row, launched via
/// `npx`) wired as the ACP backend, its model resolved from the operator env (KIMI:
/// `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL`/`ANTHROPIC_API_KEY`), and α loopback-relay MCP
/// delivery so the sandboxed CLI receives no vault secret while the host relay authenticates
/// upstream. Each thread's config home is isolated under
/// `DeploymentConfig::storage_dir/threads/<t>/config_home` — the CLI never touches the host's real
/// `~/.claude`. Drives a real dynamic MCP tool call end to end. `AWAKEN_MODEL_MODE=acp-real-mcp`.
pub async fn build_acp_real_mcp_router() -> Router {
    let store_dir = scenario_storage_dir();
    let cli_id = std::env::var("AWAKEN_ACP_CLI")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "claude".to_string());
    let cli = *awaken_run_executor_acp::acp_cli(&cli_id)
        .unwrap_or_else(|| panic!("{cli_id} is not an ACP catalog row"));
    // The host default model_ref mirrors the operator's `ANTHROPIC_MODEL` — the same env
    // the ACP model-delivery reads — so a session that names no model still hands the CLI
    // the real model name (not the scenario label). A session may still override it.
    let model_ref = std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "acp-real-mcp".to_string());
    awaken_cli::build_management_router_with_host_customizer(
        Arc::new(McpToolModel),
        ModelBinding::new("scenario", model_ref, format!("acp:{cli_id}")),
        move |host| {
            host.with_projected_acp(cli, Arc::new(acp_gateway::ScenarioEnvAcpModel), store_dir)
        },
    )
    .await
}

/// [`FAKE_ACP_SCRIPT`]'s sandboxed twin (bash, for `/dev/tcp`), with an OS-egress
/// probe: when `AWAKEN_ACP_PROBE_PORT` names a host-loopback listener (the e2e's),
/// the reply reports whether the sandbox could reach it (`net=UP` / `net=DOWN`).
/// Under a deny-egress environment bwrap unshares the network namespace, so even
/// the host loopback is unreachable. Without the probe env the reply is bare.
const SANDBOXED_FAKE_ACP_SCRIPT: &str = "read _p; net=''; \
    if [ -n \"$AWAKEN_ACP_PROBE_PORT\" ]; then \
      if (exec 3<>\"/dev/tcp/127.0.0.1/$AWAKEN_ACP_PROBE_PORT\") 2>/dev/null; \
      then net=' net=UP'; else net=' net=DOWN'; fi; \
    fi; \
    printf '%s\\n' \"{\\\"type\\\":\\\"message\\\",\\\"text\\\":\\\"acp-runtime reply$net\\\"}\"; \
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";

/// [`build_acp_router`]'s isolated twin: `acp:*` sessions launch the ACP CLI
/// from the Session-owned namespace Environment through the same bound ACP
/// composition used by production. There is no second per-attempt sandbox.
/// Egress follows each Session's frozen Environment projection — a deny-egress
/// session's CLI runs under `--unshare-net`.
/// `AWAKEN_MODEL_MODE=acp-sandboxed`; the sandbox roots live under
/// `AWAKEN_SANDBOX_DIR` (a per-process temp dir when unset).
pub async fn build_acp_sandboxed_router() -> Router {
    // The launch env is the ONLY env projected into the agent command; the probe
    // port (when the e2e sets one) must cross into the sandbox explicitly.
    let mut env = Vec::new();
    if let Ok(port) = std::env::var("AWAKEN_ACP_PROBE_PORT") {
        env.push(("AWAKEN_ACP_PROBE_PORT".to_string(), port));
    }
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            SANDBOXED_FAKE_ACP_SCRIPT.to_string(),
        ],
        env,
    );
    let base = std::env::var("AWAKEN_SANDBOX_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("awaken-acp-sbx-{}", std::process::id()))
        });
    let mut deployment = scenario_deployment();
    deployment.storage_dir = Some(base.clone());
    deployment.sandbox_dir = Some(base.join("sandboxes"));
    let host = resource_host_with_deployment(Arc::new(EchoModel), "awaken", deployment)
        .with_acp_launch_source(
            awaken_server::relay_hand_executor_factory(),
            awaken_runtime_host::LaunchSource::Fixed(launch),
        )
        .await;
    let publication = fixed_host_backend_publication("acp-agent", "acp:claude", Vec::new());
    let host = Arc::new(host.with_agent_publications(publication.clone()));
    // Mount `/v1/environments` over the same complete Managed state/resource
    // catalog used by every other scenario.
    mount_with_environments_and_agent_source(host, Some(publication))
}

/// The container-tier sibling of [`build_acp_sandboxed_router`]: the deterministic ACP
/// agent and the Native tool hand run in one Session-owned Docker environment, driven
/// through the full external SDK → managed → container-agent path. Configuration goes
/// through an explicit fixed test launch plus `SESSION_ENVIRONMENT_TIER=docker`; product
/// composition has no fixed-argv environment override. Needs `--features container-docker`, a production
/// sandbox image, and a reachable Docker daemon. Misconfiguration fails closed while
/// building the host rather than falling back to a local process.
pub async fn build_acp_container_router() -> Router {
    let storage_dir = scenario_storage_dir().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("awaken-acp-container-{}", std::process::id()))
    });
    let mut deployment = scenario_deployment();
    deployment.storage_dir = Some(storage_dir.clone());
    deployment.sandbox_dir = Some(
        std::env::var("AWAKEN_SANDBOX_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| storage_dir.join("sandboxes")),
    );
    deployment.sandbox_tier = match std::env::var("SESSION_ENVIRONMENT_TIER").as_deref() {
        Ok("local") => awaken_runtime_host::SandboxTier::Local,
        Ok("namespace") | Err(_) => awaken_runtime_host::SandboxTier::Namespace,
        Ok("docker") => awaken_runtime_host::SandboxTier::Docker,
        Ok("podman") => awaken_runtime_host::SandboxTier::Podman,
        Ok("k8s") | Ok("kubernetes") => awaken_runtime_host::SandboxTier::K8s,
        Ok(other) => panic!("unsupported scenario Session environment tier: {other}"),
    };
    deployment.container_image = std::env::var("AWAKEN_CONTAINER_IMAGE")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let delivered_skill = match deployment.sandbox_tier {
        awaken_runtime_host::SandboxTier::Docker
        | awaken_runtime_host::SandboxTier::Podman
        | awaken_runtime_host::SandboxTier::K8s => "delivered-container",
        _ => "delivered-namespace",
    };
    let publication = fixed_host_backend_publication(
        "namespace-agent",
        "acp:custom",
        vec![delivered_skill.into()],
    );
    let host = resource_host_with_deployment(Arc::new(EchoModel), "awaken", deployment)
        .with_agent_publications(publication.clone());
    let argv = scenario_argv(
        &std::env::var("AWAKEN_ACP_ARGV").expect("container scenario requires AWAKEN_ACP_ARGV"),
    );
    let host = host
        .with_acp_launch_source(
            awaken_server::relay_hand_executor_factory(),
            awaken_runtime_host::LaunchSource::Fixed(awaken_run_executor_acp::AcpLaunch::custom(
                argv,
                vec![],
            )),
        )
        .await;
    // Use the same shared Resource Catalog + Managed ACL assembly as every other
    // scenario, with the exact EnvironmentState mounted by the environment API.
    mount_with_environments_and_agent_source(Arc::new(host), Some(publication))
}

// ── Router assembly ─────────────────────────────────────────────────────────

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    mount(Arc::new(resource_host(llm, model_ref)))
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

/// The model backing a scenario router, and its advertised ref. Normally the
/// deterministic in-process model the scenario scripts; but when the e2e harness
/// sets `AWAKEN_MODEL_SOURCE=http` (alongside a fake Anthropic upstream in
/// `ANTHROPIC_BASE_URL`), it is the real [`GenaiExecutor`] dialing that upstream —
/// the *same* seam [`build_real_router`] uses. This keeps a scenario's host config
/// (client tools / delegate roster / skills / state machine / compaction / memory /
/// config plane / MCP) intact while every model call crosses the real provider
/// adapter + a real socket + the Anthropic wire, so an e2e drops its model stub
/// without losing the scenario. With the real source the ref is the wire model name
/// (`ANTHROPIC_MODEL`); otherwise it is `default_ref`.
pub fn scenario_model(
    in_process: Arc<dyn LlmExecutor>,
    default_ref: &str,
) -> (Arc<dyn LlmExecutor>, String) {
    match std::env::var("AWAKEN_MODEL_SOURCE").as_deref() {
        Ok("http") => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .or_else(|_| std::env::var("KIMI_API_KEY"))
                .expect("AWAKEN_MODEL_SOURCE=http requires ANTHROPIC_API_KEY");
            let base = std::env::var("ANTHROPIC_BASE_URL")
                .or_else(|_| std::env::var("KIMI_BASE_URL"))
                .expect("AWAKEN_MODEL_SOURCE=http requires ANTHROPIC_BASE_URL");
            let base = normalize_anthropic_compatible_base(base);
            let model = std::env::var("ANTHROPIC_MODEL")
                .or_else(|_| std::env::var("KIMI_MODEL"))
                .unwrap_or_else(|_| default_anthropic_compatible_model(&base).to_string());
            (
                Arc::new(GenaiExecutor::anthropic_compatible(base, key)),
                model,
            )
        }
        // A dev-only live Gemini fixture. Even here the key is read explicitly and
        // injected into a fixed adapter; no SDK ambient-default path is exercised.
        Ok("gemini") => {
            let key = std::env::var("GEMINI_API_KEY")
                .or_else(|_| std::env::var("GOOGLE_API_KEY"))
                .expect("AWAKEN_MODEL_SOURCE=gemini requires GEMINI_API_KEY/GOOGLE_API_KEY");
            let model =
                std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
            (
                Arc::new(GenaiExecutor::from_resolved(AdapterKind::Gemini, None, key)),
                model,
            )
        }
        _ => (in_process, default_ref.to_string()),
    }
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
    use awaken_agent_contract::RedactedString;
    use awaken_config_store::ModelSelection;
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
    let resolver = awaken_server::model_resolver::CatalogModelPublicationResolver::from_repo(
        catalog_repo,
        cred_repo.clone(),
    );
    let published = awaken_runtime_host::ModelPublicationResolver::resolve_models(
        &resolver,
        &awaken_tenancy::ScopeId::from("ws"),
        &ModelSelection::Pinned(ModelBinding::new("anthropic", &model, "genai")),
        &[],
    )
    .await
    .expect("publish model candidate");
    let materializer = awaken_server::inference_materializer::CredentialInferenceMaterializer::new(
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
    use awaken_config_store::ModelSelection;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
    use awaken_credential_vault::{
        CredentialKind, CredentialSource, CredentialSourceId, CredentialStatus, InMemorySecretStore,
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
        env_key: None,
        material_ref: None,
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
    let resolver = awaken_server::model_resolver::CatalogModelPublicationResolver::from_repo(
        catalog_repo,
        cred_repo.clone(),
    );
    let published = awaken_runtime_host::ModelPublicationResolver::resolve_models(
        &resolver,
        &awaken_tenancy::ScopeId::from("ws"),
        &ModelSelection::Pinned(ModelBinding::new("anthropic", &model, "genai")),
        &[],
    )
    .await
    .expect("publish OAuth model candidate");
    let materializer = awaken_server::inference_materializer::CredentialInferenceMaterializer::new(
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

/// Ephemeral ResourcePlane behind the production workspace-path adapter. This
/// is the sole multi-workspace process fixture for volatile resource semantics;
/// it decorates the canonical scenario Host instead of defining another store.
pub fn build_ephemeral_resource_router() -> Router {
    let flat = mount(Arc::new(resource_host(Arc::new(EchoModel), "echo-model")));
    awaken_server::workspace_path::with_workspace_path_addressing(flat)
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

/// A router whose runs execute their tools on a REMOTE HAND (ADR-0044) instead of
/// the in-process registry. A hand task serving the built-in hand tools is spawned
/// over an in-process framed channel; the host routes every run's tool calls to it
/// via `with_remote_hand`. The driving model calls `bash` to echo a marker, so the
/// e2e proves the whole brain→(framed channel)→hand→brain path through the served
/// binary. `AWAKEN_MODEL_MODE=remote-hand`.
pub fn build_remote_hand_router() -> Router {
    use awaken_tool_relay::{HandSession, RemoteToolExecutor, serve_hand};

    let (model, model_ref) =
        scenario_model(Arc::new(crate::models::RemoteHandModel), "remote-hand");

    // Where the hand runs is a topology choice (ADR-0045):
    //   - AWAKEN_REMOTE_HAND=host:port (or tcp://host:port) → Direct-over-network:
    //     dial a hand serving the executor channel on TCP (e.g. a k8s Service in
    //     another pod). The tool calls leave the brain pod entirely.
    //   - AWAKEN_REMOTE_HAND_UNIX=/path/hand.sock → Co-located (C5): the hand runs in
    //     this run's `--network none` sandbox container; dial the unix socket it bound
    //     in the shared rendezvous — the transport that crosses a network-denied edge.
    //   - unset → the degenerate in-process hand: a framed duplex to a serve_hand
    //     task in this same process.
    let executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> = if let Some(nats_url) =
        std::env::var("AWAKEN_REMOTE_HAND_NATS")
            .ok()
            .filter(|v| !v.is_empty())
    {
        // Relay topology (ADR-0045): neither end reaches the other directly;
        // both meet at a NATS broker. The brain publishes each HandRequest on
        // the shared subject and awaits the reply (NATS request/reply).
        let subject =
            std::env::var("AWAKEN_HAND_SUBJECT").unwrap_or_else(|_| "awaken.hand.exec".to_string());
        Arc::new(connect_nats_executor_blocking(&nats_url, subject))
    } else if let Some(listen) = std::env::var("AWAKEN_REMOTE_HAND_LISTEN")
        .ok()
        .filter(|v| !v.is_empty())
    {
        // Reverse topology (ADR-0045): the hand has no inbound reachability
        // (NAT / outbound-only), so it dials US. Bind a rendezvous through the
        // one ChannelFactory and use the accepted channel as the executor
        // channel — the brain stays the requester; only the dial direction flips.
        let plan = awaken_connection_plan::ConnectionPlan::tcp_listen(&listen);
        let channel = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let listener = awaken_connection_plan::bind_tcp(&plan)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("brain failed to bind reverse rendezvous {listen}: {e}")
                    });
                eprintln!("awaken brain: awaiting a reverse-dial hand on tcp://{listen}");
                listener
                    .accept()
                    .await
                    .unwrap_or_else(|e| panic!("brain rendezvous accept failed: {e}"))
            })
        });
        Arc::new(RemoteToolExecutor::new(channel))
    } else if let Some(sock) = std::env::var("AWAKEN_REMOTE_HAND_UNIX")
        .ok()
        .filter(|v| !v.is_empty())
    {
        // Co-located topology (C5, ADR-0044/0045): the hand runs INSIDE this run's
        // sandbox container and the container's network is DENIED (`--network none`),
        // so no TCP port can be published. The brain dials the unix socket the hand
        // bound in the shared host<->container rendezvous (a CacheVolume bind-mount) —
        // the one transport that crosses a network-denied boundary. Same requester
        // role and ChannelFactory as the TCP branch; only DialAddr flips to Unix.
        let plan = awaken_connection_plan::ConnectionPlan::unix_dial(&sock);
        let channel = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                awaken_connection_plan::connect_with_retry(
                    &awaken_connection_plan::TokioChannelFactory,
                    &plan,
                    240,
                    std::time::Duration::from_millis(500),
                )
                .await
                .unwrap_or_else(|e| panic!("could not reach co-located hand at unix://{sock}: {e}"))
            })
        });
        Arc::new(RemoteToolExecutor::new(channel))
    } else if let Some(remote) = std::env::var("AWAKEN_REMOTE_HAND")
        .ok()
        .filter(|v| !v.is_empty())
    {
        // Direct topology (ADR-0045): the brain dials the hand's host:port (a k8s
        // Service) through the one ChannelFactory, retrying while the hand pod /
        // cluster DNS warms up.
        let addr = remote.strip_prefix("tcp://").unwrap_or(&remote).to_string();
        let plan = awaken_connection_plan::ConnectionPlan::tcp_dial(&addr);
        let channel = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                awaken_connection_plan::connect_with_retry(
                    &awaken_connection_plan::TokioChannelFactory,
                    &plan,
                    240,
                    std::time::Duration::from_millis(500),
                )
                .await
                .unwrap_or_else(|e| panic!("could not reach remote hand at {addr}: {e}"))
            })
        });
        Arc::new(RemoteToolExecutor::new(channel))
    } else {
        // InProcess degenerate: a framed duplex to a serve_hand task in-process.
        let (brain_end, hand_end) = awaken_connection_plan::in_process_pair();
        let session = HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
        tokio::spawn(serve_hand(hand_end, session));
        Arc::new(RemoteToolExecutor::new(brain_end))
    };

    // ADR-0046: place this run's hand through the `ToolExecutorProvider` seam
    // rather than the session-wide `with_remote_hand`. The served single-agent
    // mode is the degenerate one-entry policy — a catch-all that places every run
    // on the hand established above — so this e2e also exercises the placement
    // seam end to end, not just ADR-0044's executor.
    let provider = Arc::new(placement::ConfigToolExecutorProvider::new(vec![
        placement::PlacementEntry::any(executor),
    ]));
    let host = resource_host(model, model_ref)
        .with_gate_override(Arc::new(AllowAllGate))
        .with_tool_executor_provider(provider);
    mount(Arc::new(host))
}

/// Auto-allows every tool call, so an action tool (`bash`) runs without a HITL
/// pause — the remote-hand e2e asserts the hand's execution, not the gate.
pub fn build_delegation_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(DelegatingModel), "delegate");
    let snapshot = |agent_id: &str, delegates: Vec<AgentId>| {
        let mut tools = awaken_runtime_host::authorable_tools();
        if delegates.is_empty() {
            tools.retain(|tool| tool.kind != ToolKind::AgentDelegation);
        }
        ExecutableAgentSnapshot::builder(agent_id)
            .model(ModelBinding::new("default", &model_ref, "default"))
            .tools(tools)
            .agent_bindings(AgentBindings {
                delegate_ids: delegates,
                ..Default::default()
            })
            .build()
    };
    let publications = StaticPublishedAgentSnapshots::try_new([
        snapshot("assistant", vec![AgentId("researcher".into())]),
        snapshot("researcher", Vec::new()),
    ])
    .expect("valid scenario Agent publications");
    let host = resource_host(model, model_ref).with_agent_publications(Arc::new(publications));
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
            delegate_ids: vec![AgentId("researcher".into())],
            ..Default::default()
        })
        .build();
    let researcher = ExecutableAgentSnapshot::builder("researcher")
        .resolved_model(ResolvedModelCandidate::remote(
            ModelBinding::new("remote", "", format!("a2a:{url}")),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "scenario-http-transport",
        ))
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([assistant, researcher])
        .expect("valid remote delegation publication");
    let host = resource_host(model, model_ref)
        .with_agent_publications(Arc::new(publications))
        .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(None));
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
    let storage_root = deployment.storage_dir.clone();
    let host = resource_host_with_deployment(model, model_ref, deployment);
    if let Some(storage_root) = storage_root {
        let skills = host
            .skill_store()
            .expect("canonical scenario ResourcePlane installs SkillStore");
        awaken_server::migrate_legacy_skill_registry(&storage_root, skills.as_ref())
            .await
            .expect("migrate legacy scenario Skill registry");
    }
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
    let tools = Arc::new(awaken_runtime_host::ScopedToolCatalog::new(
        global.clone(),
        awaken_runtime_host::RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    // A minimal LIVE catalog repo with one provider + endpoint + offering for the
    // scenario model, so an `Auto` config (the management assistant) resolves to a
    // concrete binding at publish (ADR-0052 D5) AND the capability reader reports the
    // scenario model live.
    let catalog_repo: Arc<dyn awaken_model_catalog::repo::CatalogRepo> =
        Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new());
    catalog_repo
        .put_provider(awaken_model_catalog::Provider {
            id: awaken_model_catalog::ProviderId::new("default"),
            slug: "default".into(),
            display_name: "Default".into(),
            version: 1,
        })
        .await
        .expect("put provider");
    catalog_repo
        .put_endpoint(awaken_model_catalog::ProtocolEndpoint {
            id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            base_url: None,
            timeout_secs: 30,
            display_name: "ep".into(),
            version: 1,
        })
        .await
        .expect("put endpoint");
    catalog_repo
        .put_offering(awaken_model_catalog::Offering {
            model_id: model_ref.clone(),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            protocol_endpoint_id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        })
        .await
        .expect("put offering");
    // The service is scope-free (ADR-0051); `ConfigPlane` is the scope edge that binds
    // the request scope (a `ScopedConfig` registry + the scope's tool catalog) onto it.
    let service = Arc::new(ConfigService::new(Arc::new(
        model_publication::ScenarioHostModelResolver::new(catalog_repo.clone()),
    )));
    let plane = awaken_runtime_host::ConfigPlane::new(service.clone(), store, tools);
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
        Arc::new(awaken_control::EnvironmentStateAuthor::new(Arc::new(
            awaken_protocol_managed::EnvironmentState::new(),
        ))),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    let host = resource_host_with_deployment(model, model_ref, deployment)
        .with_local_workspace(platform_workspace.clone())
        .with_config_service(service.clone())
        .with_admin_tools(admin_execs)
        .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(None));
    // The reserved value owns only configuration/tool visibility. Install the
    // executable in the Host's real platform Workspace so Sessions, resources,
    // credentials, and runtime lookup share one coordinate.
    awaken_control::seed_admin_assistant(
        &plane,
        &platform_workspace,
        awaken_config_store::ModelSelection::Auto,
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
    // Build the managed state explicitly (as `mount` does) but wire the config-plane
    // agent source — the SAME `ConfigServiceAgentSource(service)` that `/v1/agents`
    // projects from — so a session inheriting a published agent's model reads that one
    // config truth rather than the host default (reuse, no second source).
    let host = Arc::new(host);
    let managed_state = Arc::new(
        ManagedState::new(ManagedHost::new(host.clone())).with_config_source(Arc::new(
            awaken_runtime_host::ConfigServiceAgentSource(service.clone()),
        )),
    );
    let flat = mount_with_managed(host, managed_state)
        .merge(config_router(plane))
        .merge(agents);
    let flat = awaken_server::workspace_path::with_platform_workspace(flat, platform_workspace);
    awaken_server::workspace_path::with_workspace_path_addressing(flat)
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

/// A `ToolExecutor` that relays each call to a hand through a NATS broker (ADR-0045
/// Relay topology): publish the `HandRequest` on the shared subject, await the
/// `HandReply`. Reuses tool-relay's wire types + result mapping.
struct NatsToolExecutor {
    client: async_nats::Client,
    subject: String,
    next_id: std::sync::atomic::AtomicU64,
}

#[async_trait::async_trait]
impl awaken_runtime_contract::tool::ToolExecutor for NatsToolExecutor {
    async fn invoke(
        &self,
        call: &awaken_runtime_contract::llm::ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        use awaken_runtime_contract::tool::ToolError;
        use awaken_tool_relay::wire::{HandErrorKind, HandReply, HandRequest, HandResult};

        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let req = HandRequest::new(id, call.clone());
        let bytes = serde_json::to_vec(&req)
            .map_err(|e| ToolError::Execution(format!("encode hand request: {e}")))?;
        let msg = self
            .client
            .request(self.subject.clone(), bytes.into())
            .await
            .map_err(|e| {
                // The request may have run on a hand but the reply was lost.
                ToolError::Execution(format!("indeterminate: nats relay request failed: {e}"))
            })?;
        let reply: HandReply = serde_json::from_slice(&msg.payload)
            .map_err(|e| ToolError::Execution(format!("decode hand reply: {e}")))?;
        match reply.result {
            HandResult::Ok { output } => Ok(output),
            HandResult::Err { error } => match error.kind {
                HandErrorKind::UnknownTool => Err(ToolError::Unknown(call.tool_id.clone())),
                _ => Err(ToolError::Execution(error.message)),
            },
            HandResult::Indeterminate => Err(ToolError::Execution(
                "indeterminate: hand connection lost".to_string(),
            )),
        }
    }
}

/// Connect the brain to the NATS broker at startup (retrying while the broker pod
/// comes up), returning a relay executor. Blocking, before serving.
fn connect_nats_executor_blocking(url: &str, subject: String) -> NatsToolExecutor {
    let url = url.to_string();
    let client = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            for _ in 0..120 {
                match async_nats::connect(&url).await {
                    Ok(c) => return c,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
                }
            }
            panic!("could not reach NATS broker at {url}");
        })
    });
    NatsToolExecutor {
        client,
        subject,
        next_id: std::sync::atomic::AtomicU64::new(1),
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
