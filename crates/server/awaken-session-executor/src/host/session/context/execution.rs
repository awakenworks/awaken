// @formal evidence acp.tool_export_requires_claim_execution_environment
//! ACP export leases and the exact executor built from the realized Environment.

use super::{SemanticSkillTools, resources::ContextResources};
use crate::host::session::input_projection::{
    merge_acp_mcp_servers, merge_process_local_mcp_servers,
};
use crate::host::session_ctx::{RuntimeContextPurpose, acp_tool_export_admitted};
use crate::{HostError, SharedHost};
use awaken_runtime::Runtime;
use awaken_runtime_contract::{ExecutableAgentSnapshot, execution::RunAttemptExecutor};
use std::sync::Arc;

type ConfiguredTool = (
    awaken_runtime_contract::resolved::ToolDescriptor,
    Arc<dyn awaken_tool_contract::tool::RawTool>,
);

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
    pub(super) search: Option<ConfiguredTool>,
    pub(super) fetch: Option<ConfiguredTool>,
}

pub(super) async fn prepare(
    host: &SharedHost,
    resources: &ContextResources,
    requested_purpose: RuntimeContextPurpose,
    config: &ExecutableAgentSnapshot,
    runtime: Runtime,
    tools: AcpTools<'_>,
) -> Result<ExecutionWiring, HostError> {
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
        let policy = host.acp_adapter_policy()?;
        if !policy.contains(cli) {
            return Err(HostError::internal(format!(
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
        let (host_web_tools, provider_server_tools) = host.realize_configured_web_tools_for_acp(
            acp_cli_id,
            &config.resolved_spec.plugin_config.agent,
            web_search,
            web_fetch,
        )?;
        acp_provider_server_tools = provider_server_tools;

        let mut descriptors = Vec::new();
        let mut executors = Vec::new();
        for (descriptor, tool) in host_web_tools {
            descriptors.push(descriptor);
            executors.push(tool);
        }
        // Project the exact publication-selected Hand subset. The
        // Session environment supplies rooted executors; the immutable
        // snapshot supplies model-visible descriptors. Neither side may
        // independently add a tool.
        let hand_ids = crate::config::hand_tool_descriptors()
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
            let grants = Arc::new(crate::acp_tool_export::AcpToolExecutionGrants::new(
                descriptors
                    .iter()
                    .filter(|descriptor| {
                        descriptor.kind
                            != awaken_runtime_contract::resolved::ToolKind::ClientExecuted
                    })
                    .map(|descriptor| descriptor.id.clone()),
            ));
            acp_tool_grants = Some(grants.clone());
            executors = executors
                .into_iter()
                .map(|tool| {
                    Arc::new(crate::acp_tool_export::PermissionGuardedAcpTool::new(
                        tool,
                        permission.clone(),
                        grants.clone(),
                    )) as Arc<dyn awaken_tool_contract::tool::RawTool>
                })
                .collect();
            let exporter = host.exec.acp_tool_exporter.as_ref().ok_or_else(|| {
                HostError::internal("ACP_TOOL_BRIDGE_MISSING: ACP Session tools require an installed tool-export adapter")
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
                    .map_err(HostError::internal)?,
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
                    HostError::internal(format!("ACP_TOOL_BRIDGE_MISSING: {error}"))
                })?;
                acp_mcp_servers = merge_process_local_mcp_servers(acp_mcp_servers, servers)?;
                acp_tool_exports.extend(exports);
            }
        }
    }
    let runtime = Arc::new(runtime);
    let acp_executor = host
        .exec
        .acp
        .as_ref()
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
        .map_err(HostError::internal)?;
    Ok(ExecutionWiring {
        runtime,
        acp_executor,
        acp_tool_exports,
        acp_provider_server_tools,
    })
}
