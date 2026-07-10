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
pub mod placement;
pub mod webhooks;

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
    content_fingerprint, default_models, durable_ops_router, files_router, memory_stores_router,
    models_router, parse_skill_md, skills_router,
};

/// An [`ExecutorProvider`] mapping a model ref to a labeled executor, so a
/// session bound to `fast`/`slow` resolves a distinct model — the R1/R2/R5 demo
/// surface.
struct RouteProvider;

impl ExecutorProvider for RouteProvider {
    fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
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
        SharedHost::new(default_model, "default").with_executor_provider(Arc::new(RouteProvider)),
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
    let host = SharedHost::new(model, model_ref);
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
    let host = SharedHost::new(model, model_ref)
        .with_memory(mem_dir)
        .with_skills(vec![greet]);
    mount(Arc::new(host))
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
    let host = match env_u("AWAKEN_COMPACT_MAX_TOKENS") {
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
/// work, then drives the session — whose agent parks on a client-executed
/// `submit_answer` tool — by **running the tool and posting the result back**, the
/// way a self-hosted worker executes the session's tool calls. Heartbeats the lease
/// and stops the work on completion. `AWAKEN_MODEL_MODE=worker`.
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
/// (id 2), then on `session/prompt` (id 3) stream one `session/update`
/// agent-message chunk and reply with `stopReason:"end_turn"`. Stands in for a real
/// `claude --acp` to exercise the [`awaken_run_executor_acp::Codec::Acp`] driver.
const FAKE_ACP_JSONRPC_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
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
    model_delivery: awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        key: "ANTHROPIC_API_KEY",
        aliases: &[],
    },
    mcp_interface: awaken_run_executor_acp::McpInterface::AcpSession,
    config_home_env: "CLAUDE_CONFIG_DIR",
    memory_entrypoint: "CLAUDE.md",
    retained_paths: &[],
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

// ── Router assembly ─────────────────────────────────────────────────────────

/// Mount every public protocol adapter over one shared host. Managed Agents, AI
/// SDK, and AG-UI routes have disjoint path prefixes (`/v1/sessions...`,
/// `/v1/ai-sdk...`, `/v1/ag-ui...`) and drive the same `host`, so all three
/// protocols operate on the same threads.
fn mount(host: Arc<SharedHost>) -> Router {
    // Wire the webhook plane when configured (ADR-0048 / S10): the lifecycle sink
    // goes into the managed state (so a committed session fact fans out) and the
    // subscription CRUD router is merged into the surface. Unset env = no plane.
    let mut state = ManagedState::new(ManagedHost::new(host.clone()));
    let webhook_router = match webhooks::webhook_plane() {
        Some((sink, router)) => {
            state = state.with_lifecycle_sink(sink);
            Some(router)
        }
        None => None,
    };
    let base = mount_with_managed(host, Arc::new(state));
    match webhook_router {
        Some(router) => base.merge(router),
        None => base,
    }
}

/// [`mount`], with a caller-assembled Managed state: the management server passes
/// a vault-aware `ManagedState` over an MCP-wired `ManagedHost` (ADR-0043 Phase
/// 3); every other mode goes through [`mount`], whose state is the plain host.
fn mount_with_managed(host: Arc<SharedHost>, managed_state: Arc<ManagedState>) -> Router {
    // Spawn the process-level dispatch pool once when durable ingress is enabled
    // (O2): it is the sole claimer of the shared queue and drives every session's
    // runs. This is the single seam that owns an `Arc<SharedHost>`, which the pool's
    // session resolver needs.
    host.ensure_dispatch_pool();
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
    // The Models API (`/v1/models`) over the deployment's model directory.
    let models = models_router(std::sync::Arc::new(default_models()));
    managed
        .merge(ai_sdk)
        .merge(ag_ui)
        .merge(a2a)
        .merge(durable_ops)
        .merge(files)
        .merge(memory_stores)
        .merge(skills)
        .merge(models)
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
            let model = std::env::var("ANTHROPIC_MODEL")
                .or_else(|_| std::env::var("KIMI_MODEL"))
                .unwrap_or_else(|_| "fake-haiku".to_string());
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
struct AllowAllGate;

#[async_trait::async_trait]
impl awaken_runtime_contract::permission::ToolGateHook for AllowAllGate {
    async fn gate(
        &self,
        _ctx: &awaken_runtime_contract::permission::PermissionContext,
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

/// Run this binary as a HAND over NATS (ADR-0045 Relay): connect to the broker,
/// subscribe the shared subject, and reply to each `HandRequest` with the built-in
/// hand tools' result (G33). Loops until killed.
pub async fn run_hand_server_nats(
    url: &str,
    subject: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use awaken_tool_relay::{HandSession, wire::HandRequest};
    use futures::StreamExt;

    // Retry while the broker pod comes up.
    let mut client = None;
    for _ in 0..120 {
        match async_nats::connect(url).await {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    let client = client.ok_or_else(|| format!("hand failed to reach NATS {url}"))?;
    let mut sub = client.subscribe(subject.to_string()).await?;
    let mut session = HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
    eprintln!("awaken hand: serving the executor channel over NATS {url} subject '{subject}'");
    while let Some(msg) = sub.next().await {
        let Some(reply_to) = msg.reply else { continue };
        let request: HandRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("awaken hand: bad request over NATS: {e}");
                continue;
            }
        };
        let reply = session.handle(request).await;
        let bytes = serde_json::to_vec(&reply)?;
        client.publish(reply_to, bytes.into()).await?;
        client.flush().await?;
    }
    Ok(())
}

/// Run this binary as a HAND (ADR-0044/0045): serve the neutral executor channel —
/// the built-in hand tools, and nothing else (G33) — to a brain. Two topologies:
///   - listen (`AWAKEN_HAND_LISTEN`): bind and accept brains that dial in (Direct).
///   - dial   (`AWAKEN_HAND_DIAL`):   dial the brain's rendezvous and serve over
///     that outbound connection (Reverse / NAT). Reconnects if the link drops.
/// Loops until killed.
pub async fn run_hand_server(addr: &str, dial: bool) -> Result<(), Box<dyn std::error::Error>> {
    use awaken_tool_relay::{HandSession, serve_hand};

    if dial {
        let factory = awaken_connection_plan::TokioChannelFactory;
        let plan = awaken_connection_plan::ConnectionPlan::tcp_dial(addr);
        eprintln!("awaken hand: reverse-dialing the brain rendezvous at tcp://{addr}");
        loop {
            match awaken_connection_plan::connect_with_retry(
                &factory,
                &plan,
                240,
                std::time::Duration::from_millis(500),
            )
            .await
            {
                Ok(channel) => {
                    let session =
                        HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
                    // Serve this brain until the link drops, then re-dial.
                    let _ = serve_hand(channel, session).await;
                    eprintln!("awaken hand: brain link closed; re-dialing");
                }
                Err(e) => eprintln!("awaken hand: reverse-dial failed: {e}"),
            }
        }
    }

    let plan = awaken_connection_plan::ConnectionPlan::tcp_listen(addr);
    let listener = awaken_connection_plan::bind_tcp(&plan)
        .await
        .map_err(|e| format!("hand failed to bind {addr}: {e}"))?;
    eprintln!("awaken hand: serving the executor channel on tcp://{addr}");
    loop {
        match listener.accept().await {
            Ok(channel) => {
                let session = HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
                tokio::spawn(async move {
                    let _ = serve_hand(channel, session).await;
                });
            }
            Err(e) => eprintln!("awaken hand: accept error: {e}"),
        }
    }
}

/// A router whose agent can delegate to a `researcher` sub-agent via `agent_run`
/// (the multi-agent e2e). `ghost` is deliberately absent from the roster so the
/// fail-closed path can be exercised.
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
pub fn build_config_router() -> Router {
    let registry = Arc::new(
        awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
    );
    let tools = advertised_tools(&HashSet::new(), &HashSet::new(), &[]);
    let service = Arc::new(ConfigService::new(registry, tools));
    let (model, model_ref) = scenario_model(Arc::new(InstructionEchoModel), "config");
    let host = SharedHost::new(model, model_ref).with_config_service(service.clone());
    // `/v1/agents` over this server projects the config plane it hosts: an agent
    // published via `/v1/config/agents` is retrievable as a managed-wire projection
    // of that single truth (no second store).
    let agents = awaken_protocol_managed::agents_router(std::sync::Arc::new(
        awaken_protocol_managed::AgentRegistryState::new().with_config_source(std::sync::Arc::new(
            awaken_runtime_host::ConfigServiceAgentSource(service.clone()),
        )),
    ));
    mount(Arc::new(host))
        .merge(config_router(service))
        .merge(agents)
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
        mcp: admin,
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
    // The user-profiles front door (`/v1/user_profiles`) over its own in-mem store.
    let user_profiles = awaken_protocol_managed::user_profiles_router(std::sync::Arc::new(
        awaken_protocol_managed::UserProfileState::new(),
    ));
    // The public agent registry (`/v1/agents`) over its own in-mem store.
    let agents = awaken_protocol_managed::agents_router(std::sync::Arc::new(
        awaken_protocol_managed::AgentRegistryState::new(),
    ));
    // Deployments + deployment runs (`/v1/deployments`, `/v1/deployment_runs`).
    let deployments = awaken_protocol_managed::deployments_router(std::sync::Arc::new(
        awaken_protocol_managed::DeploymentState::new(),
    ));
    // Environments + work queue (`/v1/environments`, single-worker open cap). Shared
    // with the session state so `POST /v1/sessions` resolves an environment's
    // networking policy (egress on/off) at creation.
    let env_state = std::sync::Arc::new(awaken_protocol_managed::EnvironmentState::new());
    let environments = awaken_protocol_managed::environments_router(env_state.clone());

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
        .merge(environments);
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
    let (model, model_ref) = scenario_model(Arc::new(McpToolModel), "management");
    let host = Arc::new(SharedHost::new(model, model_ref));
    let managed_state = Arc::new(
        ManagedState::new(ManagedHost::new(host.clone()).with_mcp(credentials, secrets, mcp_store))
            .with_vaults(vault_state)
            .with_environments(env_state)
            .with_session_repo(sessions),
    );
    mount_with_managed(host, managed_state).merge(mgmt)
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
    let host = SharedHost::new(model, model_ref)
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
