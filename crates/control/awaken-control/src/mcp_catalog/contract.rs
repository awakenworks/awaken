//! Typed read DTOs; the existing contract pipeline generates frontend types.
use awaken_agent_config::mcp_config::{McpCredentialPreference, McpServerDefinition};
use awaken_protocol_managed::McpCredentialChoice;
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use serde::Serialize;

#[derive(Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerView {
    pub key: String,
    pub origin: String,
    pub definition: McpServerDefinition,
    pub target_fingerprint: String,
    pub default_credential: Option<McpCredentialPreference>,
    pub default_matches_target: bool,
}
#[derive(Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpCatalogView {
    pub source_url: Option<String>,
    pub active_source_url: Option<String>,
    pub revision: u64,
    pub digest: String,
}
#[derive(Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpConfigurationView {
    pub revision: u64,
    pub servers: Vec<McpServerView>,
    pub catalog: McpCatalogView,
}
#[derive(Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpPreparedBinding {
    pub binding: AgentMcpServerBinding,
    pub credential: Option<McpCredentialChoice>,
    pub revision: u64,
}
