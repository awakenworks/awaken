//! ACP execution Scenario platforms and their deterministic fixtures.

use super::*;

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
        resource_host(Arc::new(EchoModel), "awaken").map_host(|host| host.with_acp(acp)),
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
        resource_host(Arc::new(EchoModel), "awaken").map_host(|host| host.with_acp(acp)),
        "acp-agent",
        "acp:claude",
    )
}

/// [`build_acp_router`]'s official-wire twin: `acp:*` sessions drive the fake agent
/// over real ACP JSON-RPC (the [`awaken_run_executor_acp::Codec::Acp`] driver),
/// proving the production codec end-to-end. `AWAKEN_MODEL_MODE=acp-jsonrpc`.
pub async fn build_acp_jsonrpc_router() -> Router {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(
        scenario_shell_argv(FAKE_ACP_JSONRPC_SCRIPT),
        vec![],
    );
    let mut deployment = scenario_deployment();
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
    let platform =
        resource_host_with_deployment(Arc::new(OversizedToolModel), "awaken", deployment)
            .map_host_async(|host| async move {
                host.with_gate_override(Arc::new(AllowAllGate))
                    .with_acp_launch_source(
                        awaken_worker::relay_hand_executor_factory(),
                        awaken_runtime_host::LaunchSource::FixedAcp(Box::new(launch)),
                    )
                    .await
            })
            .await;
    mount_with_host_backend_publication(platform, "acp-agent", "acp:claude")
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
        resource_host(Arc::new(EchoModel), "awaken").map_host(|host| host.with_acp(acp)),
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
        resource_host(model, model_ref).map_host(|host| host.with_acp(acp)),
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
        minimum_version: awaken_run_executor_acp::AcpVersion::new(0, 0, 0),
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
pub(super) const FAKE_ACP_CLI: awaken_run_executor_acp::AcpCli = awaken_run_executor_acp::AcpCli {
    id: "fake",
    display_name: "Fake ACP",
    description: "Deterministic scenario ACP fixture.",
    acquisition: awaken_run_executor_acp::AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", FAKE_ACP_GATEWAY_JSONRPC_SCRIPT],
    },
    discovery: FAKE_ACP_DISCOVERY,
    image_requirements: &[],
    container_argv: &["/bin/sh", "-c", FAKE_ACP_GATEWAY_JSONRPC_SCRIPT],
    container_probe_argv: None,
    capability_probe_auth_method_id: None,
    model_delivery: Some(awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        credential_env: &["ANTHROPIC_API_KEY"],
        aliases: &[],
    }),
    model_api_dialects: &["anthropic_messages"],
    backend_model_interface: awaken_run_executor_acp::BackendModelInterface::Unsupported,
    managed_model_interface: awaken_run_executor_acp::ManagedModelInterface::Environment,
    managed_credential_delivery: awaken_run_executor_acp::ManagedCredentialDelivery::ProcessSecret,
    managed_provider_config: None,
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
/// prompt (`id:3`), classifies it into its agent message: `saw-calc`/`saw-search`
/// when that exact server name crossed, `host-relay` if the URL points at the
/// per-session loopback relay. The
/// managed-API e2e can therefore assert the whole D6→D5 chain (session `mcp_servers` →
/// staged → host relay → `session/new`) without exposing authorization material to the
/// external ACP process. Any retained `session-mcp:` credential marker is classified as
/// a leak so the E2E cannot accidentally bless the deleted fallback.
const FAKE_ACP_MCP_ECHO_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          N=noname; case \"$SN\" in *search*) N=saw-search;; *calc*) N=saw-calc;; esac; \
          A=noref; case \"$SN\" in *'/sesn_'*) A=host-relay;; *'session-mcp:'*) A=credential-leaked;; *'Authorization'*) A=process-auth;; esac; \
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
    image_requirements: &[],
    container_argv: &["/bin/sh", "-c", FAKE_ACP_MCP_ECHO_SCRIPT],
    container_probe_argv: None,
    capability_probe_auth_method_id: None,
    model_delivery: Some(awaken_run_executor_acp::ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        credential_env: &["ANTHROPIC_API_KEY"],
        aliases: &[],
    }),
    model_api_dialects: &["anthropic_messages"],
    backend_model_interface: awaken_run_executor_acp::BackendModelInterface::Unsupported,
    managed_model_interface: awaken_run_executor_acp::ManagedModelInterface::Environment,
    managed_credential_delivery: awaken_run_executor_acp::ManagedCredentialDelivery::ProcessSecret,
    managed_provider_config: None,
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

/// Container-only ACP fixture used by the Environment package E2E. The image
/// supplies this executable; its prompt handler opens the publication-pinned
/// Playwright stdio MCP server and proves a real browser tool round trip.
const PLAYWRIGHT_MCP_FIXTURE_CLI: awaken_run_executor_acp::AcpCli =
    awaken_run_executor_acp::AcpCli {
        id: "playwright-fixture",
        display_name: "Playwright MCP fixture",
        description: "Deterministic container ACP client for Playwright MCP.",
        acquisition: awaken_run_executor_acp::AcpAcquisition::Direct {
            executable: "/usr/local/bin/awaken-playwright-acp-fixture",
            args: &[],
        },
        discovery: FAKE_ACP_DISCOVERY,
        image_requirements: &[],
        container_argv: &["/usr/local/bin/awaken-playwright-acp-fixture"],
        container_probe_argv: None,
        capability_probe_auth_method_id: None,
        model_delivery: Some(awaken_run_executor_acp::ModelDelivery {
            base_url: "ANTHROPIC_BASE_URL",
            model: "ANTHROPIC_MODEL",
            credential_env: &["ANTHROPIC_API_KEY"],
            aliases: &[],
        }),
        model_api_dialects: &["anthropic_messages"],
        backend_model_interface: awaken_run_executor_acp::BackendModelInterface::Unsupported,
        managed_model_interface: awaken_run_executor_acp::ManagedModelInterface::Environment,
        managed_credential_delivery:
            awaken_run_executor_acp::ManagedCredentialDelivery::ProcessSecret,
        managed_provider_config: None,
        auth_method_id: None,
        mcp_interface: awaken_run_executor_acp::McpInterface::AcpSession,
        config_home_env: Some("AWAKEN_PLAYWRIGHT_CONFIG_HOME"),
        config_home_aliases: &[],
        memory_entrypoint: "AGENTS.md",
        session_export_excludes: &[],
        session_persistence: awaken_run_executor_acp::SessionPersistence::None,
        context_window_env: None,
        env: &[],
    };

/// A launch resolver with a fixed (dummy) model: the fake CLI ignores the model env, so
/// this keeps the scenario off the "model config via env" path — no ANTHROPIC_* need be
/// exported for the resolver to succeed. Only the MCP/`session/new` wire is under test.
struct FixedAcpModel;
#[async_trait::async_trait]
impl awaken_run_executor_acp::LaunchResolver for FixedAcpModel {
    async fn model(
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
    let mut fixture_cli = FAKE_ACP_MCP_CLI;
    // Keep the deterministic executable, but bind it to a registered adapter
    // identity so credential-delivery and MCP capability declarations come
    // from the same production catalog row used by admission.
    fixture_cli.id = "claude";
    let source = Arc::new(awaken_run_executor_acp::ProjectingChannelSource::new(
        scenario_host_acp_cli(fixture_cli),
        Arc::new(FixedAcpModel),
    ));
    let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    awaken_cli::build_all_in_one_router_with_host_customizer(
        Arc::new(McpToolModel),
        ModelBinding::new("scenario", "acp-managed-mcp", "acp:claude"),
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
    let wrapper_root = store_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("awaken-acp-sbx-{}", std::process::id()))
    });
    let launch_argv = awaken_acp_application::AcpWrapperInstaller::resolved_argv(
        &awaken_acp_application::NpmWrapperInstaller,
        &cli,
        &wrapper_root.join("acp-wrappers"),
    )
    .await
    .unwrap_or_else(|error| panic!("acquire pinned ACP wrapper for {cli_id}: {error}"));
    // The host default model_ref mirrors the operator's `ANTHROPIC_MODEL` — the same env
    // the ACP model-delivery reads — so a session that names no model still hands the CLI
    // the real model name (not the scenario label). A session may still override it.
    let model_ref = std::env::var("ANTHROPIC_MODEL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "acp-real-mcp".to_string());
    awaken_cli::build_all_in_one_router_with_host_customizer(
        Arc::new(McpToolModel),
        ModelBinding::new("scenario", model_ref, format!("acp:{cli_id}")),
        move |host| {
            host.with_projected_acp_argv(
                cli,
                Arc::new(acp_gateway::ScenarioEnvAcpModel),
                store_dir,
                launch_argv,
            )
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
/// environment lifecycle used by production. There is no second per-attempt sandbox.
/// Egress follows each Session's frozen Environment projection — a deny-egress
/// session's CLI runs under `--unshare-net`.
/// `AWAKEN_MODEL_MODE=acp-sandboxed`; the sandbox roots come from the same typed
/// Scenario deployment as every other mode (a process temp root when unset).
pub async fn build_acp_sandboxed_router() -> Router {
    build_acp_sandboxed_router_with_deployment(scenario_deployment()).await
}

/// Typed-deployment variant used by embedders and hermetic tests. The ordinary
/// scenario entry point above remains fail-closed when its requested namespace
/// cannot be created; callers must explicitly select `Local` to run unsandboxed.
pub async fn build_acp_sandboxed_router_with_deployment(
    mut deployment: awaken_runtime_host::DeploymentConfig,
) -> Router {
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
    // One typed owner for both roots. Explicit storage/sandbox coordinates are
    // preserved independently; the scenario-only fallback colocates them under
    // one process root without reopening an AWAKEN_* compatibility path.
    let fallback = std::env::temp_dir().join(format!("awaken-acp-sbx-{}", std::process::id()));
    let storage_dir = deployment.storage_dir.clone().unwrap_or_else(|| {
        deployment
            .sandbox_dir
            .as_ref()
            .and_then(|sandbox| sandbox.parent())
            .map_or_else(|| fallback.clone(), std::path::Path::to_path_buf)
    });
    deployment.storage_dir = Some(storage_dir.clone());
    deployment
        .sandbox_dir
        .get_or_insert_with(|| storage_dir.join("sandboxes"));
    let platform = resource_host_with_deployment(Arc::new(EchoModel), "awaken", deployment)
        .map_host_async(|host| async move {
            host.with_acp_launch_source(
                awaken_worker::relay_hand_executor_factory(),
                awaken_runtime_host::LaunchSource::Fixed(launch),
            )
            .await
        })
        .await;
    let publication = fixed_host_backend_publication("acp-agent", "acp:claude", Vec::new());
    let platform = platform.map_host(|host| host.with_agent_publications(publication.clone()));
    // Mount `/v1/environments` over the same complete Managed state/resource
    // catalog used by every other scenario.
    mount_with_environments_and_agent_source(platform, Some(publication))
}

/// The container-tier sibling of [`build_acp_sandboxed_router`]: the deterministic ACP
/// agent and the Native tool hand run in one Session-owned Docker environment, driven
/// through the full external SDK → managed → container-agent path. Configuration goes
/// through an explicit fixed test launch plus `SESSION_ENVIRONMENT_TIER=docker`; product
/// deployment has no fixed-argv environment override. Needs `--features container-docker`, a production
/// sandbox image, and a reachable Docker daemon. Misconfiguration fails closed while
/// building the host rather than falling back to a local process.
pub async fn build_acp_container_router() -> Router {
    let mut deployment = scenario_deployment();
    let storage_dir = deployment.storage_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("awaken-acp-container-{}", std::process::id()))
    });
    deployment.storage_dir = Some(storage_dir.clone());
    deployment
        .sandbox_dir
        .get_or_insert_with(|| storage_dir.join("sandboxes"));
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
    deployment.sandbox.package_image_registry = std::env::var("AWAKEN_PACKAGE_IMAGE_REGISTRY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    deployment.sandbox.package_registry_insecure =
        std::env::var("AWAKEN_PACKAGE_REGISTRY_INSECURE")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"));
    deployment.sandbox.package_registry_auth_file =
        std::env::var_os("AWAKEN_PACKAGE_REGISTRY_AUTH_FILE").map(std::path::PathBuf::from);
    deployment.sandbox.package_image_builder = match std::env::var("AWAKEN_PACKAGE_IMAGE_BUILDER")
        .as_deref()
    {
        Ok("docker") => Some(awaken_runtime_host::PackageImageBuilder::Docker),
        Ok("podman") => Some(awaken_runtime_host::PackageImageBuilder::Podman),
        Ok("k8s") | Ok("kubernetes") => Some(awaken_runtime_host::PackageImageBuilder::Kubernetes),
        Ok(other) => panic!("unsupported scenario package image builder: {other}"),
        Err(_) => None,
    };
    let delivered_skill = match deployment.sandbox_tier {
        awaken_runtime_host::SandboxTier::Docker
        | awaken_runtime_host::SandboxTier::Podman
        | awaken_runtime_host::SandboxTier::K8s => "delivered-container",
        _ => "delivered-namespace",
    };
    let skills = vec![awaken_agent_contract::AgentSkillBinding::custom(
        delivered_skill,
    )];
    let playwright_mcp = std::env::var("AWAKEN_SCENARIO_PLAYWRIGHT_MCP").as_deref() == Ok("1");
    let native_playwright_mcp =
        std::env::var("AWAKEN_SCENARIO_NATIVE_PLAYWRIGHT_MCP").as_deref() == Ok("1");
    let (publication, launch, model): (_, _, Arc<dyn LlmExecutor>) = if native_playwright_mcp {
        let publication = fixed_host_backend_publication_with_mcp(
            "namespace-agent",
            "native",
            skills.clone(),
            vec![
                awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
                    name: "playwright".into(),
                    transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::sandbox_stdio(
                        "playwright-mcp",
                        vec![
                            "--headless".into(),
                            "--no-sandbox".into(),
                            "--isolated".into(),
                            "--executable-path".into(),
                            "/usr/bin/chromium".into(),
                        ],
                    ),
                    credential: None,
                    prompts_as_skills: false,
                },
            ],
        );
        let launch = awaken_runtime_host::LaunchSource::Fixed(
            awaken_run_executor_acp::AcpLaunch::custom(vec!["/bin/false".into()], vec![]),
        );
        (publication, launch, Arc::new(NativePlaywrightMcpModel))
    } else if playwright_mcp {
        let publication = fixed_host_backend_publication_with_acp_mcp(
            "namespace-agent",
            "acp:playwright-fixture",
            skills.clone(),
            vec![awaken_runtime_contract::resolved::AcpMcpServer {
                name: "playwright".into(),
                transport: awaken_runtime_contract::resolved::AcpMcpTransport::Stdio {
                    command: "playwright-mcp".into(),
                    args: vec![
                        "--headless".into(),
                        "--no-sandbox".into(),
                        "--isolated".into(),
                        "--executable-path".into(),
                        "/usr/bin/chromium".into(),
                    ],
                },
            }],
        );
        let launch = awaken_runtime_host::LaunchSource::Projected(
            awaken_runtime_host::AcpLaunchRegistry::single(
                PLAYWRIGHT_MCP_FIXTURE_CLI,
                Arc::new(FixedAcpModel),
            ),
        );
        (publication, launch, Arc::new(EchoModel))
    } else {
        let publication = fixed_host_backend_publication("namespace-agent", "acp:custom", skills);
        let argv = scenario_argv(
            &std::env::var("AWAKEN_ACP_ARGV").expect("container scenario requires AWAKEN_ACP_ARGV"),
        );
        let launch = awaken_runtime_host::LaunchSource::Fixed(
            awaken_run_executor_acp::AcpLaunch::custom(argv, vec![]),
        );
        (publication, launch, Arc::new(EchoModel))
    };
    let host_publication = publication.clone();
    let platform = resource_host_with_deployment(model, "awaken", deployment)
        .map_host_async(|host| async move {
            host.with_agent_publications(host_publication)
                .with_acp_launch_source(awaken_worker::relay_hand_executor_factory(), launch)
                .await
        })
        .await;
    // Use the same shared Resource Catalog + Managed ACL assembly as every other
    // scenario, with the exact Environment Execution application mounted by the API.
    mount_with_environments_and_agent_source(platform, Some(publication))
}
