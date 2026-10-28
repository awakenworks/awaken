// @formal surface obligation=credential.mcp_claim_admits_only_when_every_requirement_is_installed
// @formal reason: Typed Session MCP credential demand. The Session validates pairing and usage here at
// @formal reason: admission; the claim re-classifies usage and admits each requirement through the
// @formal reason: route and conjunction kernels proved in run-ingress credential admission. String
// @formal reason: usage classification is executable decision-table evidence, not a Kani claim.
//! Typed Session MCP credential demand carried by Worker placement.
//!
//! The Session validates each MCP attachment's credential pairing and usage at
//! admission and publishes only this secret-free requirement. Claim admission
//! compares installed Worker evidence against it and never reads the Session
//! projection that produced it.

use serde::{Deserialize, Serialize};

use crate::{CredentialAccess, CredentialUsage, PlaintextHolder};

/// One MCP credential that the claiming Worker must be able to realize for the
/// exact selected plaintext holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCredentialRequirement {
    pub credential: CredentialAccess,
    pub selected_plaintext_holder: PlaintextHolder,
}

/// Why one MCP attachment cannot publish a credential requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpCredentialRequirementError {
    #[error("Session MCP credential and selected plaintext holder must be present together")]
    UnpairedHolder,
    #[error("Session MCP credential usage is unsupported")]
    UnsupportedUsage,
}

/// The closed usage classes an MCP credential may carry. Only an HTTP
/// `Authorization: Bearer` header is deliverable to an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCredentialUsageClass {
    AuthorizationBearer,
    Unsupported,
}

impl McpCredentialUsageClass {
    #[must_use]
    pub fn of(usage: &CredentialUsage) -> Self {
        match usage {
            CredentialUsage::HttpHeader { name, scheme }
                if name.eq_ignore_ascii_case("authorization")
                    && scheme
                        .as_deref()
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer")) =>
            {
                Self::AuthorizationBearer
            }
            _ => Self::Unsupported,
        }
    }
}

impl McpCredentialRequirement {
    /// Validate one attachment's optional credential pair. A credential-free
    /// attachment publishes no requirement; a half pair or a non-Bearer usage
    /// is rejected before the Run is admitted.
    pub fn from_attachment(
        credential: Option<&CredentialAccess>,
        selected_plaintext_holder: Option<&PlaintextHolder>,
    ) -> Result<Option<Self>, McpCredentialRequirementError> {
        match (credential, selected_plaintext_holder) {
            (None, None) => Ok(None),
            (Some(credential), Some(holder)) => {
                let requirement = Self {
                    credential: credential.clone(),
                    selected_plaintext_holder: holder.clone(),
                };
                match requirement.usage_class() {
                    McpCredentialUsageClass::AuthorizationBearer => Ok(Some(requirement)),
                    McpCredentialUsageClass::Unsupported => {
                        Err(McpCredentialRequirementError::UnsupportedUsage)
                    }
                }
            }
            _ => Err(McpCredentialRequirementError::UnpairedHolder),
        }
    }

    #[must_use]
    pub fn usage_class(&self) -> McpCredentialUsageClass {
        if self.credential.policy.handling_mode
            != awaken_agent_contract::credential::CredentialHandlingMode::PlatformManaged
        {
            return McpCredentialUsageClass::Unsupported;
        }
        McpCredentialUsageClass::of(&self.credential.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef, ModelExposurePolicy,
        PlaintextBoundary,
    };

    fn holder() -> PlaintextHolder {
        PlaintextHolder::new(PlaintextBoundary::Worker, "worker-a")
    }

    fn access(usage: CredentialUsage) -> CredentialAccess {
        CredentialAccess::new(
            CredentialRef {
                id: "mcp-credential".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(holder(), ModelExposurePolicy::Forbidden),
        )
    }

    fn header(name: &str, scheme: Option<&str>) -> CredentialUsage {
        CredentialUsage::HttpHeader {
            name: name.into(),
            scheme: scheme.map(Into::into),
        }
    }

    #[test]
    fn mcp_credential_usage_class_admits_only_an_authorization_bearer_header() {
        // Cause/effect table: C1 header name is Authorization in any case;
        // C2 scheme is Bearer in any case. E1 C1+C2 -> AuthorizationBearer;
        // E2 any other header, a missing scheme or a non-header usage ->
        // Unsupported.
        for usage in [
            header("Authorization", Some("Bearer")),
            header("authorization", Some("bearer")),
            header("AUTHORIZATION", Some("BEARER")),
        ] {
            assert_eq!(
                McpCredentialUsageClass::of(&usage),
                McpCredentialUsageClass::AuthorizationBearer,
                "E1 {usage:?}"
            );
        }
        for usage in [
            header("Authorization", None),
            header("Authorization", Some("Basic")),
            header("X-Api-Key", Some("Bearer")),
            CredentialUsage::ProviderAdapter,
            CredentialUsage::HttpBasicAuth,
            CredentialUsage::QueryParameter {
                name: "token".into(),
            },
        ] {
            assert_eq!(
                McpCredentialUsageClass::of(&usage),
                McpCredentialUsageClass::Unsupported,
                "E2 {usage:?}"
            );
        }
    }

    #[test]
    fn mcp_credential_requirement_is_published_only_for_a_valid_pair() {
        // Cause/effect table: C1 credential present; C2 holder present; C3
        // usage is Authorization Bearer. R1 !C1+!C2 -> no requirement; R2
        // C1+C2+C3 -> the exact pair; R3 exactly one of C1/C2 -> UnpairedHolder;
        // R4 C1+C2+!C3 -> UnsupportedUsage.
        let bearer = access(header("Authorization", Some("Bearer")));
        assert_eq!(
            McpCredentialRequirement::from_attachment(None, None),
            Ok(None),
            "R1"
        );
        assert_eq!(
            McpCredentialRequirement::from_attachment(Some(&bearer), Some(&holder())),
            Ok(Some(McpCredentialRequirement {
                credential: bearer.clone(),
                selected_plaintext_holder: holder(),
            })),
            "R2"
        );
        assert_eq!(
            McpCredentialRequirement::from_attachment(Some(&bearer), None),
            Err(McpCredentialRequirementError::UnpairedHolder),
            "R3 credential only"
        );
        assert_eq!(
            McpCredentialRequirement::from_attachment(None, Some(&holder())),
            Err(McpCredentialRequirementError::UnpairedHolder),
            "R3 holder only"
        );
        assert_eq!(
            McpCredentialRequirement::from_attachment(
                Some(&access(CredentialUsage::ProviderAdapter)),
                Some(&holder()),
            ),
            Err(McpCredentialRequirementError::UnsupportedUsage),
            "R4"
        );
    }

    #[test]
    fn mcp_credential_requirement_wire_is_closed() {
        // A durable placement row carries this value: it round-trips exactly
        // and an unknown field is refused rather than silently dropped.
        let requirement = McpCredentialRequirement {
            credential: access(header("Authorization", Some("Bearer"))),
            selected_plaintext_holder: holder(),
        };
        let mut wire = serde_json::to_value(&requirement).expect("encodes");
        assert_eq!(
            serde_json::from_value::<McpCredentialRequirement>(wire.clone()).expect("decodes"),
            requirement
        );
        wire["widened"] = serde_json::json!(true);
        assert!(serde_json::from_value::<McpCredentialRequirement>(wire).is_err());
    }
}
