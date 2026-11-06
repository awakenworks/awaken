// @formal evidence acp.tool_export_requires_claim_execution_environment
//! ACP export leases and the exact executor built from the realized Environment.

use super::{SemanticSkillTools, resources::ContextResources};
use crate::host::session::input_projection::{
    merge_acp_mcp_servers, merge_process_local_mcp_servers,
};
use crate::host::session_ctx::{RuntimeContextPurpose, acp_tool_export_admitted};
use crate::{SessionExecutionError, web_search::ConfiguredWebTool};
use awaken_runtime::Runtime;
use awaken_runtime_contract::{ExecutableAgentSnapshot, execution::RunAttemptExecutor};
use std::sync::Arc;

pub(super) struct ExecutionWiring {
    pub(super) runtime: Arc<Runtime>,
    pub(super) acp_executor: Option<Arc<dyn RunAttemptExecutor>>,
    pub(super) acp_tool_exports:
        Vec<awaken_session_environment_contract::tool_export::AcpToolExport>,
    pub(super) acp_provider_server_tools:
        Vec<awaken_runtime_contract::resolved::ProviderServerTool>,
}

pub(super) struct AcpTools<'a> {
    pub(super) skills: Option<&'a SemanticSkillTools>,
    pub(super) memory: Option<&'a crate::session_memory_tools::SessionMemoryTools>,
    pub(super) search: Option<ConfiguredWebTool>,
    pub(super) fetch: Option<ConfiguredWebTool>,
}

pub(super) async fn prepare(
    host: &crate::RunAssembly,
    resources: &ContextResources,
    requested_purpose: RuntimeContextPurpose,
    config: &ExecutableAgentSnapshot,
    mut runtime: Runtime,
    tools: AcpTools<'_>,
) -> Result<ExecutionWiring, SessionExecutionError> {
    let active_mcp = &resources.active_mcp;
    let execution_backend = &resources.plan.execution_backend;
    let is_acp = execution_backend.is_acp();
    let env = &resources.env;
    let permission = resources.authorization.policy.clone();
    let semantic_skill_tools = tools.skills;
    let semantic_memory_tools = tools.memory;
    let web_search = tools.search;
    let web_fetch = tools.fetch;
    // D6: for an ACP run, merge publication-pinned secret-free stdio routes with
    // the Session's staged URL routes, then hand the exact set to the CLI's own MCP
    // client. The immutable publication's `backend_ref` is the routing authority.
    // Environment provisioning may install a stdio command, but it never owns the
    // route: the Agent publication declares it and the Session receives it here.
    // The credential form is the host's
    // isolation decision: the raw bearer never reaches the CLI; every authenticated
    // ACP server uses the Worker-held exact-generation relay. A native run is untouched
    // (its MCP servers are already the in-process tools connected above).
    // Runtime construction consumes effects staged by the one public MCP
    // realization lifecycle.  It must not start a relay or recreate routes:
    // after restart, durable rehydration stages and publishes them first.
    let relay = host.mcp.relay();
    let mut acp_mcp_servers = if is_acp {
        let cli = match &execution_backend {
            awaken_runtime_contract::resolved::Backend::Acp(backend) => backend.cli(),
            _ => unreachable!("is_acp is derived from the execution backend"),
        };
        let policy = host.capabilities.acp_adapter_policy()?;
        if !policy.contains(cli) {
            return Err(SessionExecutionError::internal(format!(
                "unknown ACP adapter `acp:{cli}`"
            )));
        }
        let publication = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(
            config.resolved_spec.plugin_config.plugins(),
        )
        .mcp_servers;
        let staged = active_mcp
            .iter()
            .filter_map(|projection| {
                projection
                    .server
                    .as_ref()
                    .map(|server| (projection, server))
            })
            .map(|(projection, server)| {
                awaken_session_mcp::project_mcp_session_transport(
                    server,
                    &projection.request,
                    relay,
                    &projection.receipt,
                    policy,
                    cli,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        merge_acp_mcp_servers(publication, staged)?
    } else {
        Vec::new()
    };
    let mut acp_tool_exports = Vec::new();
    let mut acp_attached_tools = None;
    let mut acp_provider_server_tools = Vec::new();
    let mut acp_tool_grants = None;
    let mut acp_client_tool_ids = std::collections::HashSet::new();
    let mut acp_client_tool_requests = None;
    // A coordination envelope owns no physical Environment and must
    // not publish an executable ACP MCP server. The claim-owning
    // execution context is rebuilt later and exports the exact
    // descriptor/executor set once its Environment exists.
    if acp_tool_export_admitted(requested_purpose, is_acp, env.is_some()) {
        let acp_cli_id = match &execution_backend {
            awaken_runtime_contract::resolved::Backend::Acp(backend) => backend.cli(),
            _ => unreachable!("is_acp is derived from the execution backend"),
        };
        let (host_web_tools, provider_server_tools) =
            host.capabilities.realize_configured_web_tools_for_acp(
                acp_cli_id,
                &config.resolved_spec.plugin_config.agent,
                web_search,
                web_fetch,
            )?;
        acp_provider_server_tools = provider_server_tools;

        let (mut descriptors, mut executors): (Vec<_>, Vec<_>) = host_web_tools.into_iter().unzip();
        // This is the one registration point for ACP's configured Host web
        // executors. Hand tools already belong to build_runtime; Skill/Memory
        // tools already belong to their extensions. Re-registering those ids
        // poisons RawToolRegistry as ambiguous, even for the same implementation.
        for tool in &executors {
            runtime = runtime.try_with_tool(tool.clone()).map_err(|error| {
                SessionExecutionError::internal(format!("RUNTIME_CONFIGURATION: {error}"))
            })?;
        }
        // Project the exact publication-selected Hand subset. The
        // Session environment supplies rooted executors; the immutable
        // snapshot supplies model-visible descriptors. Neither side may
        // independently add a tool.
        let hand_ids = awaken_session_run_delivery::hand_tool_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.id)
            .collect::<std::collections::HashSet<_>>();
        descriptors.extend(
            config
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| hand_ids.contains(&descriptor.id))
                .cloned(),
        );
        if let Some(environment) = env.as_ref() {
            let selected_hand_ids = descriptors
                .iter()
                .filter(|descriptor| hand_ids.contains(&descriptor.id))
                .map(|descriptor| descriptor.id.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>();
            executors.extend(crate::acp_tool_export::environment_executor_tools(
                selected_hand_ids,
                environment.agent_tools().tool_executor(),
            ));
        }
        if let Some((skill_descriptors, skill_executors)) = semantic_skill_tools.as_ref() {
            descriptors.extend(skill_descriptors.iter().cloned());
            executors.extend(skill_executors.iter().cloned());
        }
        if let Some(memory) = semantic_memory_tools.as_ref() {
            descriptors.extend(memory.descriptors.iter().cloned());
            executors.extend(memory.executors.iter().cloned());
        }
        // Claude and other ACP SDKs discover custom tools through MCP.
        // Export the same frozen ClientExecuted descriptors as Native.
        // The adapter only reports arrival and never performs the effect;
        // execution belongs exclusively to the Managed Session client
        // through ExternalEvent resume.
        let client_tools =
            crate::acp_tool_export::client_executed_tools(&config.resolved_spec.tool_descriptors);
        descriptors.extend(client_tools.descriptors);
        executors.extend(client_tools.executors);
        acp_client_tool_ids = client_tools.ids;
        acp_client_tool_requests = client_tools.requests;
        if !descriptors.is_empty() {
            let grants = Arc::new(crate::acp_tool_export::AcpToolExecutionGrants::for_exports(
                &descriptors,
            ));
            acp_tool_grants = Some(grants.clone());
            executors = crate::acp_tool_export::permission_guarded_tools(
                executors,
                permission.clone(),
                grants.clone(),
                client_tools.sender,
            );
            let exporter = host.capabilities.exec.acp_tool_exporter.as_ref().ok_or_else(|| {
                SessionExecutionError::internal("ACP_TOOL_BRIDGE_MISSING: ACP Session tools require an installed tool-export adapter")
            })?;
            if env
                .as_deref()
                .is_some_and(|env| env.agent_process().is_container())
            {
                acp_attached_tools = Some(Arc::new(
                    awaken_session_environment_contract::tool_export::AttachedToolSet::new(
                        exporter.clone(),
                        descriptors,
                        executors,
                    )
                    .map_err(SessionExecutionError::internal)?,
                ));
            } else {
                let (servers, exports) = crate::acp_tool_export::export_tools(
                    exporter.as_ref(),
                    "awaken_session",
                    descriptors,
                    executors,
                )
                .await
                .map_err(|error| {
                    SessionExecutionError::internal(format!("ACP_TOOL_BRIDGE_MISSING: {error}"))
                })?;
                acp_mcp_servers = merge_process_local_mcp_servers(acp_mcp_servers, servers)?;
                acp_tool_exports.extend(exports);
            }
        }
    }
    awaken_runtime::validate_configuration(&runtime).map_err(|error| {
        SessionExecutionError::internal(format!("RUNTIME_CONFIGURATION: {error}"))
    })?;
    let runtime = Arc::new(runtime);
    // Only the original ACP factory branch needs this read. The exact
    // resident's historical layout, not baseline absence, owns process defaults.
    let environment_variables = match (host.capabilities.exec.acp.installed(), env.as_ref()) {
        (Some(_), Some(environment)) => Some(crate::host::resident_process_environment(
            &host.realization_executor,
            &resources.thread_id.0,
            environment,
        )?),
        _ => None,
    };
    let acp_executor = host
        .capabilities
        .exec
        .acp
        .installed()
        .zip(env.clone())
        .map(|(acp, env)| {
            let continuation = acp_tool_grants.clone().map(|exports| {
                let port: Arc<dyn awaken_runtime_contract::resume::OwnedToolContinuation> =
                    Arc::new(crate::acp_tool_export::AcpToolContinuation {
                        runtime: runtime.clone(),
                        host_exports: Some(exports),
                        client_tools: acp_client_tool_ids.clone(),
                        client_requests: acp_client_tool_requests.clone(),
                    });
                port
            });
            acp.create(awaken_session_environment_contract::AcpSessionExecution {
                environment: env.agent_process().clone(),
                environment_variables,
                backend: execution_backend.clone(),
                permission,
                mcp_servers: acp_mcp_servers,
                attached_tools: acp_attached_tools,
                grant_observer: acp_tool_grants.clone().map(|observer| {
                    observer
                        as Arc<dyn awaken_runtime_contract::permission::PermissionGrantObserver>
                }),
                continuation,
            })
        })
        .transpose()
        .map_err(SessionExecutionError::internal)?;
    Ok(ExecutionWiring {
        runtime,
        acp_executor,
        acp_tool_exports,
        acp_provider_server_tools,
    })
}
