//! Test-only scenario host: the deterministic mock models and the `build_*_router`
//! scenario assemblies the e2e harness + integration tests drive. Extracted from
//! `awaken-server` so the product crate carries zero mocks. It reuses the
//! product crate's now-`pub` data-plane assembly helpers (`mount` / `mount_with_managed`
//! / `data_subject_plane`) and production executors via `awaken_server::`.

mod models;
pub use crate::models::*;

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_config_resolver::ResolvedInference;
use awaken_protocol_managed::ManagedState;
use awaken_provider_genai::GenaiExecutor;
use awaken_runtime_contract::RunActivation;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use axum::Router;

// The managed-agents service layer (`awaken-runtime-host`): the neutral host,
// the two port adapters, the per-plane routers, and the authoring/transport
// re-exports a composition root (and the integration tests) drive directly.
pub use awaken_managed_routers::{default_models, files_router, models_router};
// The A2A remote-delegate adapter + its transport constructor: the composition root
// builds the adapter here and injects it behind the host's neutral RemoteAgent interface.
use awaken_run_executor_a2a::{A2aRemoteAgent, HttpTransport};
use awaken_runtime_contract::InferenceAccess;
pub use awaken_runtime_host::{
    ConfigService, ExtMcpProbe, HostResume, InferenceExecutorMaterializer, ManagedHost,
    PreparedMcpRefresh, ProtocolHost, SharedHost, SkillContext, SkillSpec, ThreadEvent,
    ThreadEventHub, VaultRefresher, advertised_tools, capabilities_router, config_router,
    content_fingerprint, durable_ops_router, memory_stores_router, parse_skill_md, skills_router,
};

use awaken_server::placement;
/// An [`InferenceExecutorMaterializer`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
use awaken_server::{ResolvedExecutorError, executor_from_resolved, mount, mount_with_managed};
struct RouteProvider;

impl InferenceExecutorMaterializer for RouteProvider {
    fn materialize(
        &self,
        activation: &RunActivation,
        access: &InferenceAccess,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let model_ref = activation.effective_model_ref();
        if !access.is_host_executor_for(model_ref) {
            return None;
        }
        let labeled: Arc<dyn LlmExecutor> = match model_ref {
            "fast" => Arc::new(LabelModel("fast")),
            "slow" => Arc::new(LabelModel("slow")),
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
        SharedHost::new(default_model, "default")
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
    let (model, model_ref) = scenario_model(Arc::new(MemoryProbeModel), "memory");
    let host = SharedHost::new(model, model_ref).with_memory(mem_dir);
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
    let host = with_scenario_memory_registry(SharedHost::new(model, model_ref));
    mount(Arc::new(host))
}

/// A router for the github_repository RESOURCE e2e (ADR-0038): a deterministic model
/// reads a host-cloned repo's file and writes a change the host commits + pushes back
/// to the remote on harvest. `AWAKEN_MODEL_MODE=git-repo`.
pub fn build_git_repo_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::GitRepoModel), "git-repo");
    let host = SharedHost::new(model, model_ref);
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
            std::env::temp_dir().join(format!("awaken-fullchain-e2e-{}", std::process::id()))
        });
    std::fs::create_dir_all(&mem_dir).expect("create memory dir");
    let greet = SkillSpec::new("greet", "Greet", "say hello", "GREETING-FROM-SKILL");
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "full-chain");
    let host = with_scenario_memory_registry(SharedHost::new(model, model_ref))
        .with_memory(mem_dir)
        .with_skills(vec![greet]);
    mount(Arc::new(host))
}

/// Give resource-focused scenario compositions the same durable identity repository
/// that the production management composition injects. The blob bytes alone are not
/// enough to recover tenant ownership after a restart: the memory-store definition is
/// the aggregate that records its owning workspace. Without a durable registry the
/// ownership PEP correctly fails closed, making an otherwise durable blob unreachable.
fn with_scenario_memory_registry(host: SharedHost) -> SharedHost {
    let root = std::env::var("AWAKEN_MGMT_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("AWAKEN_STORAGE_DIR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    let Some(root) = root else {
        return host;
    };
    let root = std::path::PathBuf::from(root);
    std::fs::create_dir_all(&root).expect("create scenario resource registry directory");
    let registry =
        awaken_admin_config_api::SqliteAdminStore::open(&root.join("admin.db").to_string_lossy())
            .expect("open durable scenario memory-store registry");
    host.with_memory_registry(Arc::new(registry))
}

/// A router with context compaction (the compaction e2e): a low threshold folds
/// the older transcript into a summary after a few turns. The deterministic
/// model returns a fixed summary on the `compactor` sub-run and otherwise
/// reports the compaction context it received, so an e2e can observe the folded
/// summary being injected on a later turn. `AWAKEN_MODEL_MODE=compaction`.
pub fn build_compaction_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(crate::models::CompactionModel), "compaction");
    let host = SharedHost::new(model, model_ref);
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
    let host = SharedHost::new(model, model_ref);
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
pub async fn run_echo_worker(upstream: &str) -> Result<(), Box<dyn std::error::Error>> {
    struct EchoWorkerProvider;

    impl InferenceExecutorMaterializer for EchoWorkerProvider {
        fn materialize(
            &self,
            activation: &RunActivation,
            access: &InferenceAccess,
        ) -> Option<Arc<dyn LlmExecutor>> {
            if !access.is_host_executor_for(activation.effective_model_ref()) {
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
    let host = Arc::new(SharedHost::new(model, model_ref).with_client_tools(client_tools));
    let env_state = std::sync::Arc::new(awaken_protocol_managed::EnvironmentState::new());
    let environments = awaken_protocol_managed::environments_router(env_state.clone());
    let managed_state =
        Arc::new(ManagedState::new(ManagedHost::new(host.clone())).with_environments(env_state));
    mount_with_managed(host, managed_state).merge(environments)
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

/// [`build_acp_router`]'s official-wire twin: `acp:*` sessions drive the fake agent
/// over real ACP JSON-RPC (the [`awaken_run_executor_acp::Codec::Acp`] driver),
/// proving the production codec end-to-end. `AWAKEN_MODEL_MODE=acp-jsonrpc`.
pub fn build_acp_jsonrpc_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            FAKE_ACP_JSONRPC_SCRIPT.to_string(),
        ],
        vec![],
    );
    let source = Arc::new(
        awaken_run_executor_acp::SubprocessChannelSource::new(launch)
            .with_codec(awaken_run_executor_acp::Codec::Acp),
    );
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    mount(Arc::new(
        SharedHost::new(Arc::new(EchoModel), "awaken").with_acp(acp),
    ))
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
    // The native fallback model runs over the real wire when the harness asks
    // (`AWAKEN_MODEL_SOURCE=http`); the ACP `runtime:"acp:*"` path is unaffected — it
    // runs on the real CLI subprocess either way.
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "awaken");
    mount(Arc::new(SharedHost::new(model, model_ref).with_acp(acp)))
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

/// The [`FAKE_ACP_GATEWAY_JSONRPC_SCRIPT`] wired as a real [`AcpCli`] row, so the
/// projecting launch path resolves + projects the model env onto it exactly as a
/// production CLI (its delivery keys are the `ANTHROPIC_*` ones the script echoes).
/// Used only by [`build_acp_gateway_router`] to exercise host model resolution
/// (self-credentialed vs cloud-managed gateway, D-R2) end to end.
const FAKE_ACP_CLI: awaken_run_executor_acp::AcpCli = awaken_run_executor_acp::AcpCli {
    id: "fake",
    command: "/bin/sh",
    args: &["-c", FAKE_ACP_GATEWAY_JSONRPC_SCRIPT],
    container_argv: &["/bin/sh", "-c", FAKE_ACP_GATEWAY_JSONRPC_SCRIPT],
    model_delivery: awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "ANTHROPIC_API_KEY",
        aliases: &[],
    },
    mcp_interface: awaken_run_executor_acp::McpInterface::AcpSession,
    config_home_env: "CLAUDE_CONFIG_DIR",
    credential_file: None,
    memory_entrypoint: "CLAUDE.md",
    retained_paths: &[],
    // The fake gateway CLI keeps no local session (it is a scripted stand-in).
    session_persistence: awaken_run_executor_acp::SessionPersistence::None,
    context_window_env: None,
    env: &[],
};

/// [`build_acp_router`]'s twin that drives the fake CLI through the REAL projecting
/// launch path ([`SharedHost::with_projected_acp`] → `EnvLaunchResolver` →
/// `ProjectingChannelSource`), so the host's model resolution is exercised end to
/// end. With `AWAKEN_ACP_GATEWAY_URL` + `AWAKEN_ACP_LEASE_TOKEN` in the environment,
/// the resolver takes the cloud-managed gateway path (D-R2): the CLI is pointed at
/// the gateway with a lease token, never a raw provider key. `AWAKEN_MODEL_MODE=acp-gateway`.
pub fn build_acp_gateway_router() -> Router {
    let store_dir = std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "awaken");
    mount(Arc::new(
        SharedHost::new(model, model_ref).with_projected_acp(FAKE_ACP_CLI, store_dir),
    ))
}

/// A fake ACP agent (JSON-RPC, shell builtins only) that reports whether the
/// `session/new` request it received carried the session's MCP server and, if so,
/// whether the bearer is the α secretless reference (`session-mcp:<name>`) rather than
/// the raw vault token. It captures the `session/new` line (`id:2`) and, on the prompt
/// (`id:3`), classifies it into its agent message: `saw-calc` if the `calc` server name
/// crossed, `alpha-ref` if a `session-mcp:` reference is the bearer. So the managed-API
/// e2e can assert the whole D6→D5 chain (session `mcp_servers` → staged → α overlay →
/// `session/new`) reached the CLI without the raw secret ever leaving the host.
const FAKE_ACP_MCP_ECHO_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          N=noname; case \"$SN\" in *calc*) N=saw-calc;; esac; \
          A=noref; case \"$SN\" in *'session-mcp:'*) A=alpha-ref;; esac; \
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
    command: "/bin/sh",
    args: &["-c", FAKE_ACP_MCP_ECHO_SCRIPT],
    container_argv: &["/bin/sh", "-c", FAKE_ACP_MCP_ECHO_SCRIPT],
    model_delivery: awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "ANTHROPIC_API_KEY",
        aliases: &[],
    },
    mcp_interface: awaken_run_executor_acp::McpInterface::AcpSession,
    config_home_env: "CLAUDE_CONFIG_DIR",
    credential_file: None,
    memory_entrypoint: "CLAUDE.md",
    retained_paths: &[],
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
    ) -> std::result::Result<
        awaken_run_executor_acp::ResolvedModel,
        awaken_run_executor_acp::OpenError,
    > {
        Ok(awaken_run_executor_acp::ResolvedModel {
            base_url: "http://fake".into(),
            model: "fake".into(),
            api_key: "fake".into(), // awaken-allow: secret
        })
    }
}

/// The managed plane (vault + MCP staging + config plane, via awaken-cli's real
/// assembly) with an ACP backend wired on: a session that selects `runtime: "acp:*"` and
/// declares `mcp_servers` (bound to a vault credential) has its staged servers projected
/// α-secretless into the fake CLI's `session/new`. Proves the D6→D5 chain end to end
/// through the HTTP managed API. `AWAKEN_MODEL_MODE=acp-managed-mcp`.
pub async fn build_acp_managed_mcp_router() -> Router {
    let source = Arc::new(awaken_run_executor_acp::ProjectingChannelSource::new(
        FAKE_ACP_MCP_CLI,
        Arc::new(FixedAcpModel),
    ));
    let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    awaken_cli::build_management_router_with_host_customizer(
        Arc::new(McpToolModel),
        "acp-managed-mcp".to_string(),
        move |host| host.with_acp(executor),
    )
    .await
}

/// The REAL-CLI, REAL-LLM twin of [`build_acp_managed_mcp_router`]: the managed plane
/// with the **actual** `claude --acp` adapter (the catalog `claude` row, launched via
/// `npx`) wired as the ACP backend, its model resolved from the operator env (KIMI:
/// `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL`/`ANTHROPIC_API_KEY`), and **β trusted-inline**
/// MCP delivery so a session's vault-bound MCP token reaches the CLI's own MCP client and
/// authenticates against the real server. Each thread's config home is isolated under
/// `AWAKEN_STORAGE_DIR/threads/<t>/config_home` — the CLI never touches the host's real
/// `~/.claude`. Drives a real dynamic MCP tool call end to end. `AWAKEN_MODEL_MODE=acp-real-mcp`.
pub async fn build_acp_real_mcp_router() -> Router {
    let store_dir = std::env::var("AWAKEN_STORAGE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let cli = *awaken_run_executor_acp::acp_cli("claude").expect("claude is a catalog row");
    // The host default model_ref mirrors the operator's `ANTHROPIC_MODEL` — the same env
    // the ACP model-delivery reads — so a session that names no model still hands the CLI
    // the real model name (not the scenario label). A session may still override it.
    let model_ref = std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "acp-real-mcp".to_string());
    awaken_cli::build_management_router_with_host_customizer(
        Arc::new(McpToolModel),
        model_ref,
        move |host| {
            // β: this is a trusted-local CLI launch, so a staged MCP server's bearer may
            // cross to the CLI inline (it must, to authenticate to the real MCP server —
            // α would hand it an unresolved `session-mcp:` reference).
            host.with_trusted_acp_mcp(true)
                .with_projected_acp(cli, store_dir)
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
/// inside a bubblewrap (namespace-tier) sandbox via
/// [`awaken_runtime_host::SandboxChannelSource`], so the agent process is
/// OS-confined regardless of what it does. Egress follows each session's
/// environment networking policy through the host's shared
/// [`awaken_runtime_host::ThreadEgress`] registrations — a deny-egress
/// session's CLI runs under `--unshare-net`.
/// `AWAKEN_MODEL_MODE=acp-sandboxed`; the sandbox roots live under
/// `AWAKEN_SANDBOX_DIR` (a per-process temp dir when unset).
pub fn build_acp_sandboxed_router() -> Router {
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
    let host = SharedHost::new(Arc::new(EchoModel), "awaken");
    let source = awaken_runtime_host::SandboxChannelSource::new(base, launch)
        .with_thread_egress(host.thread_egress());
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(Arc::new(
        source,
    )));
    let host = Arc::new(host.with_acp(acp));
    // Mount `/v1/environments` and share its state with the session surface, so a
    // session's environment networking policy reaches `register_thread_egress` —
    // the same registrations the sandboxed launch reads (unlike the plain `mount`,
    // whose managed state carries no environment resolver).
    let env_state = std::sync::Arc::new(awaken_protocol_managed::EnvironmentState::new());
    let environments = awaken_protocol_managed::environments_router(env_state.clone());
    let managed_state =
        Arc::new(ManagedState::new(ManagedHost::new(host.clone())).with_environments(env_state));
    mount_with_managed(host, managed_state).merge(environments)
}

/// The container-tier sibling of [`build_acp_sandboxed_router`]: the deterministic ACP
/// agent runs as a **process-as-container** in a real Docker container (not a bwrap
/// namespace), driven through the full external SDK → managed → container-agent path.
/// The agent is a busybox `nc` fixture speaking the newline ACP wire (the same shape
/// the k8s adapter e2e bakes), so no LLM or API key is needed. Realized through the
/// production seam [`awaken_runtime_host::build_acp_channel_source`] at the `Docker`
/// tier — which also spawns the cross-restart reaper. Needs the binary built with
/// `--features container-docker` and a running Docker daemon (the router build fails
/// closed otherwise, never a silent non-container fallback).
pub async fn build_acp_container_router() -> Router {
    // The deterministic in-container agent: busybox `nc` listens on the container's
    // agent port (8080, the tier's fixed internal port) and, per connection, reads the
    // prompt line and replies with a fixed marker over the newline wire. Keep the
    // socket alive briefly after the terminal frame so the host can drain it.
    let script = "read _p; \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"CONTAINER-AGENT-OK\"}'; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'; \
        sleep 0.1";
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        vec![
            "nc".into(),
            "-lk".into(),
            "-p".into(),
            "8080".into(),
            "-e".into(),
            "sh".into(),
            "-c".into(),
            script.into(),
        ],
        vec![],
    );
    let image = std::env::var("AWAKEN_SANDBOX_IMAGE").unwrap_or_else(|_| "awaken-bb:1".into());
    let base = std::env::temp_dir().join(format!("awaken-acp-ctr-{}", std::process::id()));
    let host = SharedHost::new(Arc::new(EchoModel), "awaken");
    let source = awaken_runtime_host::build_acp_channel_source(
        awaken_runtime_host::SandboxTier::Docker,
        Some(&image),
        awaken_runtime_host::LaunchSource::Fixed(launch),
        host.thread_egress(),
        host.thread_resources_handle(),
        host.thread_sandbox(),
        base,
    )
    .await
    .expect(
        "build the Docker container ACP source \
         (needs --features container-docker + a running Docker daemon)",
    );
    let acp = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    // This is a single-purpose ACP deployment: make `acp` the DEFAULT backend so a
    // session routes to the containerized agent WITHOUT a client `awaken.runtime`
    // override — the backend is resolved from the deployment, not per-session metadata.
    let host = Arc::new(host.with_acp_default(acp, "acp:custom"));
    let managed_state = Arc::new(ManagedState::new(ManagedHost::new(host.clone())));
    mount_with_managed(host, managed_state)
}

// ── Router assembly ─────────────────────────────────────────────────────────

/// Mount every public protocol adapter over one shared host. Managed Agents, AI
/// SDK, and AG-UI routes have disjoint path prefixes (`/v1/sessions...`,
/// `/v1/ai-sdk...`, `/v1/ag-ui...`) and drive the same `host`, so all three
/// protocols operate on the same threads.
pub fn build_resolved_router(
    inference: &ResolvedInference,
) -> Result<Router, ResolvedExecutorError> {
    let executor = executor_from_resolved(inference)?;
    Ok(build_router(executor, inference.triple.model_id.clone()))
}

/// Build the server router backed by the kernel with the given model.
pub fn build_router(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Router {
    mount(Arc::new(with_scenario_memory_registry(SharedHost::new(
        llm, model_ref,
    ))))
}

/// A plain host over the real wire for the model-pool failover e2e (#1). The
/// primary model is `ANTHROPIC_MODEL`; when the upstream fails exactly that model,
/// the run fails over to the ordered `AWAKEN_MODEL_FALLBACKS`. No memory/tools/skills,
/// so a single message is one clean main turn. `AWAKEN_MODEL_MODE=pool-failover`
/// with `AWAKEN_MODEL_SOURCE=http`.
pub fn build_pool_failover_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "pool-primary");
    mount(Arc::new(SharedHost::new(model, model_ref)))
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
        // A live Gemini via the genai default client's AI-Studio adapter, keyed by
        // `GEMINI_API_KEY`/`GOOGLE_API_KEY` in the environment. Same host-config seam
        // as `http`, but the model calls cross the real Gemini wire — used where a
        // real LLM is needed but only a Google key is available (KIMI creds dead).
        Ok("gemini") => {
            std::env::var("GEMINI_API_KEY")
                .or_else(|_| std::env::var("GOOGLE_API_KEY"))
                .expect("AWAKEN_MODEL_SOURCE=gemini requires GEMINI_API_KEY/GOOGLE_API_KEY");
            let model =
                std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
            (Arc::new(GenaiExecutor::new()), model)
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
    let base = normalize_anthropic_compatible_base(base);
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("KIMI_MODEL"))
        .unwrap_or_else(|_| default_anthropic_compatible_model(&base).to_string());
    let executor = GenaiExecutor::anthropic_compatible(base, key);
    build_router(Arc::new(executor), model)
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
            oauth_command: None,
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

/// The resolved path with an **OAuth** credential (#5): the credential source is
/// `CredentialKind::Oauth`, so `resolve_inference` materializes it by running its
/// `oauth_command` helper (`printf oauth-minted-key`) rather than reading a sealed
/// secret. The minted token becomes the executor's API key, so a run succeeds only
/// if the OAuth materialize path actually ran the helper. `AWAKEN_MODEL_MODE=
/// oauth-resolved` with a fake upstream that authenticates exactly that token.
pub async fn build_oauth_resolved_router() -> Router {
    use std::collections::HashMap;

    use awaken_config_resolver::resolve_inference;
    use awaken_credential_vault::{
        CredentialBinding, CredentialKind, CredentialSource, CredentialSourceId, CredentialStatus,
        InMemorySecretStore,
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
        oauth_command: Some(vec!["printf".into(), OAUTH_MINTED_KEY.into()]),
        status: CredentialStatus::Active,
        version: 1,
    };
    let secrets = InMemorySecretStore::new();
    let catalog = catalog_repo.snapshot().await.expect("catalog snapshot");
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(source.id.0.clone(), source.clone());
    let inference = resolve_inference(
        &catalog,
        &model,
        &CredentialBinding::Exact {
            credential_source_id: source.id.clone(),
        },
        &sources,
        &secrets,
    )
    .await
    .expect("resolve inference (OAuth materialize)");
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
    let (model, model_ref) = scenario_model(Arc::new(CustomToolModel), "custom");
    let host = SharedHost::new(model, model_ref).with_client_tools(client_tools);
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
    let host = SharedHost::new(model, model_ref)
        .with_gate_override(Arc::new(AllowAllGate))
        .with_tool_executor_provider(provider);
    mount(Arc::new(host))
}

/// Auto-allows every tool call, so an action tool (`bash`) runs without a HITL
/// pause — the remote-hand e2e asserts the hand's execution, not the gate.
pub fn build_delegation_router() -> Router {
    let roster = HashSet::from(["researcher".to_string()]);
    let (model, model_ref) = scenario_model(Arc::new(DelegatingModel), "delegate");
    let host = SharedHost::new(model, model_ref).with_delegates(roster);
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
    let host = SharedHost::new(model, model_ref).with_state_machine(machine);
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
    let (model, model_ref) = scenario_model(Arc::new(StateMachineModel), "statemachine-rich");
    let host = SharedHost::new(model, model_ref).with_state_machine(machine);
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
/// (ADR-0020, slice E). Drive it with `AWAKEN_INGRESS=durable` so the dispatch
/// worker performs the deferred actions out of band: the probe model's
/// write→read tool calls are each scheduled and auto-performed, so the run
/// completes without any human confirmation.
pub fn build_schedule_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(ProbeModel), "schedule");
    let host = SharedHost::new(model, model_ref).with_gate_override(Arc::new(ScheduleGate));
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
    let host = SharedHost::new(model, model_ref).with_remote_agent(
        "researcher",
        Arc::new(A2aRemoteAgent::new(Arc::new(HttpTransport::new(url)))),
    );
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
                // The catalog came back — activate the offered `greet` skill.
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "s".into(),
                    tool_id: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "greet" }),
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
    let (model, model_ref) = scenario_model(Arc::new(SkillDrivingModel), "skills-durable");
    mount(Arc::new(
        SharedHost::new(model, model_ref).with_skill_store(dir),
    ))
}
pub async fn build_config_router() -> Router {
    // The MODEL is chosen by `scenario_model` (in-process echo, or the real provider
    // pointed at the fake upstream when `AWAKEN_MODEL_SOURCE=http`); the fake upstream
    // is what drives the seeded assistant through its admin tools in the run e2e.
    let (model, model_ref) = scenario_model(Arc::new(InstructionEchoModel), "config");
    let store = Arc::new(
        awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
    );
    // Scope-keyed tool visibility (ADR-0052 D3): every scope sees the advertised
    // (global) tools; only the reserved admin scope additionally sees the four
    // management descriptors, so a config naming an `admin_*` tool compiles only there.
    let global = advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
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
        })
        .await
        .expect("put offering");
    // The service is scope-free (ADR-0051); `ConfigPlane` is the scope edge that binds
    // the request scope (a `ScopedConfig` registry + the scope's tool catalog) onto it.
    let service = Arc::new(ConfigService::new().with_model_resolver(Arc::new(
        awaken_server::model_resolver::CatalogModelResolver::from_repo(catalog_repo.clone()),
    )));
    let plane = awaken_runtime_host::ConfigPlane::new(service.clone(), store, tools);
    // Seed the management assistant as an ordinary published agent in the reserved
    // scope (ADR-0052 D1/D2): it becomes a compiled ExecutableAgentSnapshot via the same path
    // as any agent, projectable on `/v1/agents`.
    awaken_control::seed_admin_assistant(&plane)
        .await
        .expect("seed admin assistant");
    // The management tool executables, backed by real ports (D3/D4): the capability
    // reader reads the shared catalog + advertised tools; the validator runs the same
    // compile check as `/v1/config/agents/validate` on drafts (in the tenant scope).
    let reader = Arc::new(awaken_control::CatalogCapabilityReader::new(
        catalog_repo.clone(),
        &global,
        &awaken_runtime_host::authorable_config_sections(),
        // No authored MCP servers in the scenario host.
        Arc::new(awaken_config_resolver::InMemoryMcpStore::new()),
        // The config plane, to list existing agent ids in the tenant scope.
        plane.clone(),
        // No data-plane inventory wired here (scenario host); memory/skills stay empty.
        None,
    ));
    let validator = Arc::new(awaken_control::ConfigServiceDraftValidator::new(
        plane.clone(),
        awaken_config_store::DEFAULT_SCOPE,
    ));
    let admin_execs = awaken_admin_assistant::admin_tools(
        reader,
        validator,
        // Persist/read drafts as unpublished config agents through the same plane the
        // editor's Save uses, in the tenant/default scope (ADR-0052).
        Arc::new(awaken_control::ConfigServiceDraftStore::new(
            plane.clone(),
            awaken_config_store::DEFAULT_SCOPE,
            // The scenario host has no durable resource store in scope; an in-memory one
            // satisfies the port so the assistant can bind resources onto a draft.
            Arc::new(awaken_config_resolver::InMemoryResourceStore::new()),
        )),
        // A fresh in-memory environment registry satisfies the author port for the
        // scenario host (no durable env state in scope).
        Arc::new(awaken_control::EnvironmentStateAuthor::new(Arc::new(
            awaken_protocol_managed::EnvironmentState::new(),
        ))),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    let host = SharedHost::new(model, model_ref)
        .with_config_service(service.clone())
        .with_admin_tools(admin_execs);
    // `/v1/agents` over this server projects the config plane it hosts: an agent
    // published via `/v1/config/agents` is retrievable as a managed-wire projection
    // of that single truth (no second store).
    let agents = awaken_protocol_managed::agents_router(std::sync::Arc::new(
        awaken_protocol_managed::AgentRegistryState::new().with_config_source(std::sync::Arc::new(
            awaken_runtime_host::ConfigServiceAgentSource(service.clone()),
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
