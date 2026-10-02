//! The sole projection of this executor's ACP CLI catalog into Host admission.

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
        crate::acp_cli(id).map(|cli| cli.id)
    }

    fn known_ids(&self) -> Vec<&'static str> {
        crate::known_acp_clis().iter().map(|cli| cli.id).collect()
    }

    fn admits_mcp_client_credential(
        &self,
        id: &str,
        delivery: Option<McpCredentialDelivery>,
        http_transport: bool,
    ) -> bool {
        crate::acp_cli(id)
            .is_some_and(|cli| cli.admits_mcp_client_credential(delivery, http_transport))
    }

    fn realizes_provider_server_tool(&self, id: &str, tool: &ProviderServerTool) -> bool {
        crate::acp_cli(id).is_some_and(|cli| cli.realizes_provider_server_tool(tool))
    }
}
