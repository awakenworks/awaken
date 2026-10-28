//! One current Run supplies opted-in MCP metadata on every ACP step.
use awaken_protocol_acp::TurnConfig;
use awaken_runtime_contract::activation::RunActivation;

pub(super) fn install(
    config: &mut TurnConfig<'_>,
    servers: &[awaken_acp_contract::SessionMcpServer],
    activation: &RunActivation,
) {
    config.mcp_servers = servers.to_vec();
    config.mcp_execution_context = Some(awaken_acp_contract::McpExecutionContext {
        run_id: activation.run_id.0.clone(),
        agent_id: activation.snapshot.root_agent_id.0.clone(),
    });
}
