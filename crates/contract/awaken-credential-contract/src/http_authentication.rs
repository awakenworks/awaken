// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: HTTP authentication is declaration data consumed by the sole credential-owned acquisition kernel.
//! Secret-free HTTP token-acquisition declaration. Request realization remains
//! in the existing transport compiler; response/state execution is credential-owned.
use crate::{RefreshTokenPolicy, TokenAcquisitionError, TokenResponseCondition, TokenResponseSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenDestination {
    Header {
        name: String,
        #[serde(default)]
        prefix: String,
    },
    Query {
        name: String,
        #[serde(default)]
        prefix: String,
    },
}

/// The legacy request-level representation remains readable. New Transport
/// declarations use this same type and the same native acquisition executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialHttpAuthentication {
    pub request: Value,
    pub token_pointer: String,
    pub ttl_pointer: String,
    pub destination: TokenDestination,
    #[serde(default)]
    pub success: Vec<TokenResponseCondition>,
    #[serde(default)]
    pub invalid_token: Vec<TokenResponseCondition>,
    #[serde(default)]
    pub response_status: Option<TokenResponseCondition>,
    #[serde(default = "margin")]
    pub refresh_margin_ms: u64,
}
fn margin() -> u64 {
    60000
}
impl CredentialHttpAuthentication {
    #[must_use]
    pub fn response_spec(&self) -> TokenResponseSpec {
        TokenResponseSpec {
            access_ttl_unit: crate::TokenExpiryUnit::Seconds,
            additional_fields: Default::default(),
            access_token_pointer: self.token_pointer.clone(),
            access_ttl_pointer: Some(self.ttl_pointer.clone()),
            require_access_ttl: true,
            refresh_token_pointer: None,
            refresh_ttl_pointer: None,
            require_refresh_ttl: false,
            refresh_policy: RefreshTokenPolicy::NotExpected,
            success: self.success.clone(),
            authorization_required: vec![],
            max_access_ttl_seconds: 86400,
            max_refresh_ttl_seconds: 31_622_400,
            refresh_margin_ms: self.refresh_margin_ms,
        }
    }
    pub fn validate(&self) -> Result<(), TokenAcquisitionError> {
        self.response_spec().validate()?;
        let (name, prefix) = match &self.destination {
            TokenDestination::Header { name, prefix }
            | TokenDestination::Query { name, prefix } => (name, prefix),
        };
        if !self.request.is_object()
            || self.invalid_token.len() > 16
            || name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || b"-_".contains(&v))
            || prefix.len() > 32
            || prefix.bytes().any(|v| v.is_ascii_control())
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "proxy-authorization"
            )
        {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        // Reuse the one JSON-pointer and scalar-condition validator.
        let mut conditions = self.response_spec();
        conditions.success = self
            .invalid_token
            .iter()
            .chain(self.response_status.iter())
            .cloned()
            .collect();
        for chunk in conditions.success.chunks(8) {
            let mut part = conditions.clone();
            part.success = chunk.to_vec();
            part.validate()?;
        }
        Ok(())
    }
}
