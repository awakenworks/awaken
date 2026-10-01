//! The sole projection of the executor's ACP CLI catalog into Host admission.

use awaken_acp_contract::AcpAdapterPolicy;
use awaken_credential_contract::McpCredentialDelivery;
use awaken_runtime_contract::resolved::ProviderServerTool;

/// A stateless view of the authoritative `AcpCli` rows.
pub struct CanonicalAcpAdapterPolicy;

static ACP_ADAPTER_POLICY: CanonicalAcpAdapterPolicy = CanonicalAcpAdapterPolicy;

/// One process-wide catalog policy for Worker, CLI, and embedded composition.
#[must_use]
pub fn acp_adapter_policy() -> &'static dyn AcpAdapterPolicy<ProviderServerTool> {
    &ACP_ADAPTER_POLICY
}

impl AcpAdapterPolicy<ProviderServerTool> for CanonicalAcpAdapterPolicy {
    fn lookup_id(&self, id: &str) -> Option<&'static str> {
        awaken_run_executor_acp::acp_cli(id).map(|cli| cli.id)
    }

    fn known_ids(&self) -> Vec<&'static str> {
        awaken_run_executor_acp::known_acp_clis()
            .iter()
            .map(|cli| cli.id)
            .collect()
    }

    fn admits_mcp_client_credential(
        &self,
        id: &str,
        delivery: Option<McpCredentialDelivery>,
        http_transport: bool,
    ) -> bool {
        awaken_run_executor_acp::acp_cli(id)
            .is_some_and(|cli| cli.admits_mcp_client_credential(delivery, http_transport))
    }

    fn realizes_provider_server_tool(&self, id: &str, tool: &ProviderServerTool) -> bool {
        awaken_run_executor_acp::acp_cli(id)
            .is_some_and(|cli| cli.realizes_provider_server_tool(tool))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_policy_projects_only_canonical_decisions() {
        // Causes: C1 known/unknown exact CLI id; C2 MCP delivery is client
        // injection or another mode; C3 transport is HTTP or stdio; C4 selected
        // provider tool has a verified launch projection for that CLI.
        // Effects: E1 known ids are exactly the sole executor catalog; E2 only
        // the catalog-admitted MCP combination succeeds; E3 provider tools are
        // admitted only through the row's existing method. Constraint: unknown
        // IDs have no row and cannot acquire either capability.
        // Decision table: R1 known+matching MCP -> catalog result; R2
        // known+nonmatching MCP -> false; R3 known+tool -> catalog result;
        // R4 unknown+any combination -> absent/false.
        let policy = acp_adapter_policy();
        let rows = awaken_run_executor_acp::known_acp_clis();
        assert_eq!(
            policy.known_ids(),
            rows.iter().map(|row| row.id).collect::<Vec<_>>()
        );
        for row in rows {
            assert_eq!(policy.lookup_id(row.id), Some(row.id));
            assert!(policy.contains(row.id));
            for delivery in [
                None,
                Some(McpCredentialDelivery::ClientInjection),
                Some(McpCredentialDelivery::GatewayMediation),
            ] {
                for http in [false, true] {
                    assert_eq!(
                        policy.admits_mcp_client_credential(row.id, delivery, http),
                        row.admits_mcp_client_credential(delivery, http),
                    );
                }
            }
            for tool in [
                ProviderServerTool::OpenAiWebSearch,
                ProviderServerTool::DeepSeekResponsesWebSearch,
                ProviderServerTool::AnthropicWebSearch,
                ProviderServerTool::GeminiWebSearch,
            ] {
                assert_eq!(
                    policy.realizes_provider_server_tool(row.id, &tool),
                    row.realizes_provider_server_tool(&tool),
                );
            }
        }
        let unknown = "unknown-third-party-cli";
        assert_eq!(policy.lookup_id(unknown), None);
        assert!(!policy.contains(unknown));
        assert!(!policy.admits_mcp_client_credential(
            unknown,
            Some(McpCredentialDelivery::ClientInjection),
            true,
        ));
        assert!(
            !policy.realizes_provider_server_tool(unknown, &ProviderServerTool::OpenAiWebSearch,)
        );
        assert!(
            policy.realizes_provider_server_tool("codex", &ProviderServerTool::OpenAiWebSearch,)
        );
        assert!(
            !policy.realizes_provider_server_tool("claude", &ProviderServerTool::OpenAiWebSearch,)
        );
        let profile = awaken_session_executor::AcpWorkerProfile::new(["codex".into()], policy)
            .expect("canonical id admits one exact Worker route");
        assert_eq!(profile.cli_ids().collect::<Vec<_>>(), vec!["codex"]);
        assert!(awaken_session_executor::AcpWorkerProfile::new([unknown.into()], policy).is_err());
    }
}
