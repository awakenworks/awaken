//! Which upstream may consume a referenced credential, and why.

use serde::{Deserialize, Serialize};

/// Exact upstream audience authorized for a credential purpose.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct CredentialAudienceRef(pub String);

impl From<String> for CredentialAudienceRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for CredentialAudienceRef {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Why a target is allowed to consume the credential. Purpose is orthogonal to
/// provider identity and material shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialPurpose {
    ProviderAdapter,
    RepositoryTransport,
    McpAuthorization,
    RemoteAgentAuthorization,
    /// One registered Host Web provider identified by its immutable provider id.
    WebProviderAuthorization,
    HttpEffect,
    SignatureVerification,
    Extension,
}

/// Exact purpose/audience identity frozen into executable access. Usage remains
/// the existing independent field on `CredentialAccess`, so executable payloads
/// cannot serialize two copies of the same usage truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialTarget {
    pub purpose: CredentialPurpose,
    pub audience: CredentialAudienceRef,
}

impl CredentialTarget {
    #[must_use]
    pub fn new(purpose: CredentialPurpose, audience: impl Into<CredentialAudienceRef>) -> Self {
        Self {
            purpose,
            audience: audience.into(),
        }
    }
}
