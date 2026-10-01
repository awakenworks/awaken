//! Read-only projection of the one ACP adapter catalog into process composition.
//!
//! The executor keeps the authoritative adapter rows. Consumers ask this port
//! only for decisions they must make before an ACP attempt is constructed.

use awaken_credential_contract::McpCredentialDelivery;

/// The adapter catalog policy supplied by the Worker execution composition.
/// `ProviderTool` is the caller's frozen provider-tool vocabulary, so this
/// capability contract does not acquire a dependency on Runtime's model types.
pub trait AcpAdapterPolicy<ProviderTool>: Send + Sync {
    /// The exact canonical id of a catalog row, if it exists.
    fn lookup_id(&self, id: &str) -> Option<&'static str>;

    /// Every id in the same catalog used by [`Self::lookup_id`].
    fn known_ids(&self) -> Vec<&'static str>;

    /// Admission uses the one lookup and rejects aliases of a canonical id;
    /// implementations must not keep another id set.
    fn contains(&self, id: &str) -> bool {
        self.lookup_id(id).is_some_and(|canonical| canonical == id)
    }

    /// The catalog row's exact MCP client-credential delivery decision.
    fn admits_mcp_client_credential(
        &self,
        id: &str,
        delivery: Option<McpCredentialDelivery>,
        http_transport: bool,
    ) -> bool;

    /// The catalog row's exact provider-owned tool projection decision.
    fn realizes_provider_server_tool(&self, id: &str, tool: &ProviderTool) -> bool;
}
