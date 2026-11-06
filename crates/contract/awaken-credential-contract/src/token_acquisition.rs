// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Neutral response and private issuance contracts; the credential owner alone admits expiry and publication.
//! Provider-neutral token response and acquisition ports. Protocol paths are
//! declaration data. Issued material is private and intentionally not Serde.
use async_trait::async_trait;
use awaken_agent_contract::{RedactedString, credential::CredentialRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenResponseCondition {
    pub pointer: String,
    pub equals: Value,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTokenPolicy {
    #[default]
    NotExpected,
    PreserveIfAbsent,
    ReplaceRequired,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TokenExpiryUnit {
    #[default]
    Seconds,
    Milliseconds,
}

/// One immutable response contract shared by client-credential issuance and
/// refresh-token exchange. A missing optional expiry disables token caching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenResponseSpec {
    #[serde(default)]
    pub access_ttl_unit: TokenExpiryUnit,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub additional_fields: std::collections::BTreeMap<String, String>,
    pub access_token_pointer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_ttl_pointer: Option<String>,
    #[serde(default)]
    pub require_access_ttl: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_pointer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_ttl_pointer: Option<String>,
    #[serde(default)]
    pub require_refresh_ttl: bool,
    #[serde(default)]
    pub refresh_policy: RefreshTokenPolicy,
    #[serde(default)]
    pub success: Vec<TokenResponseCondition>,
    #[serde(default)]
    pub authorization_required: Vec<TokenResponseCondition>,
    pub max_access_ttl_seconds: u64,
    pub max_refresh_ttl_seconds: u64,
    #[serde(default)]
    pub refresh_margin_ms: u64,
}

impl TokenResponseSpec {
    pub fn validate(&self) -> Result<(), TokenAcquisitionError> {
        let pointers = std::iter::once(&self.access_token_pointer)
            .chain(self.additional_fields.values())
            .chain(self.access_ttl_pointer.iter())
            .chain(self.refresh_token_pointer.iter())
            .chain(self.refresh_ttl_pointer.iter())
            .chain(
                self.success
                    .iter()
                    .chain(&self.authorization_required)
                    .map(|v| &v.pointer),
            );
        for pointer in pointers {
            if !valid_pointer(pointer) {
                return Err(TokenAcquisitionError::InvalidContract);
            }
        }
        if self.additional_fields.len() > 16
            || self.additional_fields.keys().any(|v| {
                v.is_empty()
                    || v.len() > 128
                    || !v
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            })
            || self.success.len() > 8
            || self.authorization_required.len() > 8
            || self
                .success
                .iter()
                .chain(&self.authorization_required)
                .any(|v| v.equals.is_array() || v.equals.is_object())
            || self.max_access_ttl_seconds == 0
            || self.max_access_ttl_seconds > 31_622_400
            || self.max_refresh_ttl_seconds == 0
            || self.max_refresh_ttl_seconds > 31_622_400
            || self.refresh_margin_ms > 600_000
            || (self.require_access_ttl && self.access_ttl_pointer.is_none())
            || (self.require_refresh_ttl && self.refresh_ttl_pointer.is_none())
            || (self.refresh_policy == RefreshTokenPolicy::NotExpected
                && (self.refresh_token_pointer.is_some() || self.refresh_ttl_pointer.is_some()))
            || (self.refresh_policy != RefreshTokenPolicy::NotExpected
                && self.refresh_token_pointer.is_none())
        {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        Ok(())
    }
}

fn valid_pointer(pointer: &str) -> bool {
    if !pointer.starts_with('/') || pointer.len() > 256 {
        return false;
    }
    let mut bytes = pointer.bytes();
    while let Some(c) = bytes.next() {
        if c.is_ascii_control() || (c == b'~' && !matches!(bytes.next(), Some(b'0' | b'1'))) {
            return false;
        }
    }
    true
}

/// Raw issuer data stays inside the last-boundary acquisition operation.
pub struct TokenEndpointResponse {
    pub status: u16,
    pub body: Value,
}

pub struct IssuedCredentialTokens {
    pub additional_fields: std::collections::BTreeMap<String, RedactedString>,
    pub access_token: RedactedString,
    pub refresh_token: Option<RedactedString>,
    pub access_expires_at_unix_ms: Option<u64>,
    pub refresh_expires_at_unix_ms: Option<u64>,
}

/// A private token lease. Public receipts expose only `reference` and expiry.
#[derive(Clone)]
pub struct AcquiredCredentialToken {
    pub additional_fields: std::collections::BTreeMap<String, RedactedString>,
    pub access_token: RedactedString,
    pub expires_at_unix_ms: Option<u64>,
    pub reference: Option<CredentialRef>,
    pub cache_generation: Option<u64>,
    pub cache_key: Option<String>,
    pub cache_partition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenAcquisitionError {
    #[error("invalid token acquisition contract")]
    InvalidContract,
    #[error("credential authority no longer permits token use")]
    AuthorityDenied,
    #[error("token authorization service is unavailable")]
    IssuerUnavailable,
    #[error("token authorization was rejected")]
    IssuerRejected,
    #[error("token response has no valid access token")]
    InvalidAccessToken,
    #[error("token response has invalid expiry")]
    InvalidExpiry,
    #[error("token response has no valid required refresh token")]
    InvalidRefreshToken,
    #[error("token publication is unavailable")]
    PublicationFailed,
    #[error("token acquisition is already in progress")]
    InProgress,
    #[error("credential needs authorization")]
    NeedsAuthorization,
    #[error("token exchange outcome is unknown; reconcile before another exchange")]
    OutcomeUnknown,
    #[error("token cache capacity reached")]
    Capacity,
}

/// IO is supplied by the credential authority or an admitted last-mile adapter.
/// Rotating grants must claim before exchange and seal before `publish` returns.
#[async_trait]
pub trait CredentialTokenIssuer: Send + Sync {
    async fn authorize(&self) -> Result<(), TokenAcquisitionError>;
    async fn begin(&self) -> Result<Option<AcquiredCredentialToken>, TokenAcquisitionError> {
        Ok(None)
    }
    async fn exchange(&self) -> Result<TokenEndpointResponse, TokenAcquisitionError>;
    async fn publish(
        &self,
        tokens: IssuedCredentialTokens,
    ) -> Result<AcquiredCredentialToken, TokenAcquisitionError>;
    async fn failed(&self, error: TokenAcquisitionError) -> TokenAcquisitionError {
        error
    }
    fn durable_publication(&self) -> bool {
        false
    }
}

#[async_trait]
pub trait CredentialTokenAcquisition: Send + Sync {
    async fn acquire(
        &self,
        partition: Option<&str>,
        response: &TokenResponseSpec,
        port: &dyn CredentialTokenIssuer,
    ) -> Result<AcquiredCredentialToken, TokenAcquisitionError>;
    fn invalidate(&self, partition: &str, expected: &AcquiredCredentialToken);
}
