// @formal entrypoint credential.mcp_claim_admits_only_when_every_requirement_is_installed
//! The published-or-legacy MCP projection carried by one frozen Run.
//!
//! This codec extracts requirements only. Worker credential policy, installed
//! capabilities and effect admission stay with their existing owners.

use super::RunDispatch;
use awaken_credential_contract::{McpCredentialRequirement, McpCredentialRequirementError};
use std::borrow::Cow;

pub(crate) enum SessionMcpRequirementProjectionError {
    Decode(String),
    Requirement(McpCredentialRequirementError),
}

impl RunDispatch {
    /// Published requirements are authoritative. Only historical rows without
    /// them decode their full runtime projection, retaining every original
    /// pairing and usage error instead of returning an empty healthy set.
    pub(crate) fn session_mcp_credential_requirements(
        &self,
    ) -> Result<Cow<'_, [McpCredentialRequirement]>, SessionMcpRequirementProjectionError> {
        if let Some(requirements) = &self.placement.session_mcp_credentials {
            return Ok(Cow::Borrowed(requirements));
        }
        let Some(envelope) = &self.session_runtime else {
            return Ok(Cow::Borrowed(&[]));
        };
        let projection = awaken_session_configuration_contract::SessionRuntimeProjection::decode(
            &envelope.projection_json,
        )
        .map_err(|error| SessionMcpRequirementProjectionError::Decode(error.to_string()))?;
        Ok(Cow::Owned(
            projection
                .mcp_credential_requirements()
                .map_err(SessionMcpRequirementProjectionError::Requirement)?,
        ))
    }
}
