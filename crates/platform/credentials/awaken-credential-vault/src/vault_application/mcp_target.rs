//! The credential owner's canonical application MCP target and source identity.

use awaken_credential_contract::CredentialSourceId;

/// The normalized MCP endpoint an application bearer authorizes: its canonical
/// audience and the persisted target fingerprint that keys credential identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCredentialTarget {
    pub audience: String,
    pub fingerprint: String,
}

impl McpCredentialTarget {
    /// Preserve the existing length-framed target identity for every spelling
    /// accepted by the canonical HTTP target parser.
    pub fn parse_http(url: &str) -> Result<Self, awaken_agent_contract::McpTargetError> {
        let identity = awaken_agent_contract::McpTarget::identity(url)?;
        let port = identity.port.map(|port| port.to_string());
        let fingerprint = super::identity_digest(
            "application-mcp-normalized-target-v1",
            &[
                &identity.scheme,
                &identity.host,
                port.as_deref().unwrap_or(""),
                &identity.path,
                if identity.query.is_some() {
                    "query"
                } else {
                    ""
                },
                identity.query.as_deref().unwrap_or(""),
            ],
        );
        Ok(Self {
            audience: identity.canonical_url(),
            fingerprint,
        })
    }

    /// Expected source identity only. It does not read a current revision,
    /// grant authority, or infer validity for an already prepared pin.
    #[must_use]
    pub fn source_id(
        &self,
        workspace_id: &str,
        application_authority_id: &str,
    ) -> CredentialSourceId {
        let vault_id = super::application_mcp_vault_id(workspace_id, application_authority_id);
        super::application_mcp_source_id(&vault_id, &self.fingerprint)
    }
}
