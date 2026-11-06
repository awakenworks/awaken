// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Versioned secret-free token request profiles reuse canonical material references and declare exact private slots.
//! Versioned, provider-neutral token request declarations. The same neutral
//! material-reference wire is used for JSON, form/query values and headers.
use crate::{HttpEffectMaterialReference, TokenAcquisitionError, TokenResponseSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const OAUTH_REFRESH_TOKEN_SLOT: &str = "oauth_refresh_token";
pub const OAUTH_CLIENT_SECRET_SLOT: &str = "oauth_client_secret";
pub const TOKEN_ACQUISITION_STATE_SLOT: &str = "token_acquisition_state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenRequestEncoding {
    Json,
    Form,
    Query,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum TokenRequestMethod {
    #[default]
    Post,
    Get,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenSourceField {
    AccessToken {},
    RefreshToken {},
    ClientSecret {},
    Auxiliary { slot: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenClientBasicAuth {
    pub client_id: String,
    pub secret_slot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialTokenExchangeProfile {
    pub version: u32,
    pub token_endpoint: String,
    pub encoding: TokenRequestEncoding,
    #[serde(default)]
    pub method: TokenRequestMethod,
    pub parameters: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, Value>,
    #[serde(default)]
    pub bindings: BTreeMap<String, BTreeMap<String, TokenSourceField>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<TokenClientBasicAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_field: Option<String>,
    pub response: TokenResponseSpec,
}

impl CredentialTokenExchangeProfile {
    pub fn validate(&self) -> Result<(), TokenAcquisitionError> {
        self.response.validate()?;
        // Durable generation publication currently has one declared access
        // field; multi-field capture belongs to transient transport exchanges.
        if !self.response.additional_fields.is_empty() {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        let url = url::Url::parse(&self.token_endpoint)
            .map_err(|_| TokenAcquisitionError::InvalidContract)?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        if self.version != 1
            || self.token_endpoint.len() > 4096
            || !matches!(url.scheme(), "http" | "https")
            || (url.scheme() == "http" && !loopback)
            || (self.method == TokenRequestMethod::Get
                && self.encoding != TokenRequestEncoding::Query)
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !self.parameters.is_object()
            || self.headers.len() > 32
            || self.bindings.len() > 16
        {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        if let Some(auth) = self.basic_auth.as_ref()
            && (auth.client_id.is_empty()
                || auth.client_id.len() > 512
                || !valid_name(&auth.secret_slot)
                || auth.secret_slot == TOKEN_ACQUISITION_STATE_SLOT)
        {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        for (name, value) in &self.headers {
            if !valid_name(name)
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "host" | "content-length" | "transfer-encoding" | "connection"
                )
                || (self.basic_auth.is_some() && name.eq_ignore_ascii_case("authorization"))
            {
                return Err(TokenAcquisitionError::InvalidContract);
            }
            self.validate_value(value, 0)?;
            if !value.is_string()
                && HttpEffectMaterialReference::from_json_value(value)
                    .map_err(|_| TokenAcquisitionError::InvalidContract)?
                    .is_none()
            {
                return Err(TokenAcquisitionError::InvalidContract);
            }
        }
        for (role, fields) in &self.bindings {
            if !valid_name(role) || fields.is_empty() || fields.len() > 16 {
                return Err(TokenAcquisitionError::InvalidContract);
            }
            for (field, source) in fields {
                if !valid_name(field)
                    || matches!(source,TokenSourceField::Auxiliary{slot} if !valid_name(slot)||matches!(slot.as_str(),OAUTH_REFRESH_TOKEN_SLOT|OAUTH_CLIENT_SECRET_SLOT|TOKEN_ACQUISITION_STATE_SLOT))
                {
                    return Err(TokenAcquisitionError::InvalidContract);
                }
                if matches!(source, TokenSourceField::RefreshToken {})
                    && self.response.refresh_policy == crate::RefreshTokenPolicy::NotExpected
                {
                    return Err(TokenAcquisitionError::InvalidContract);
                }
            }
        }
        self.validate_value(&self.parameters, 0)?;
        if self.encoding != TokenRequestEncoding::Json
            && self.parameters.as_object().is_some_and(|v| {
                v.values().any(|v| {
                    v.is_null()
                        || v.is_array()
                        || (v.is_object()
                            && HttpEffectMaterialReference::from_json_value(v)
                                .ok()
                                .flatten()
                                .is_none())
                })
            })
        {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        if serde_json::to_vec(self)
            .map_err(|_| TokenAcquisitionError::InvalidContract)?
            .len()
            > 65536
        {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        Ok(())
    }
    fn validate_value(&self, value: &Value, depth: usize) -> Result<(), TokenAcquisitionError> {
        if depth > 16 {
            return Err(TokenAcquisitionError::InvalidContract);
        }
        if let Some(reference) = HttpEffectMaterialReference::from_json_value(value)
            .map_err(|_| TokenAcquisitionError::InvalidContract)?
        {
            if !self
                .bindings
                .get(reference.credential())
                .is_some_and(|v| v.contains_key(reference.field()))
            {
                return Err(TokenAcquisitionError::InvalidContract);
            }
            return Ok(());
        }
        match value {
            Value::Object(values) => {
                for v in values.values() {
                    self.validate_value(v, depth + 1)?;
                }
            }
            Value::Array(values) => {
                for v in values {
                    self.validate_value(v, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String, TokenAcquisitionError> {
        self.validate()?;
        awaken_agent_contract::fingerprint::canonical_json_sha256(
            "awaken.credential.token-exchange.v1",
            self,
        )
        .map_err(|_| TokenAcquisitionError::InvalidContract)
    }
    #[must_use]
    pub fn auxiliary_slots(&self) -> std::collections::BTreeSet<String> {
        self.bindings
            .values()
            .flat_map(BTreeMap::values)
            .filter_map(|v| match v {
                TokenSourceField::AccessToken {} => None,
                TokenSourceField::RefreshToken {} => Some(OAUTH_REFRESH_TOKEN_SLOT.to_owned()),
                TokenSourceField::ClientSecret {} => Some(OAUTH_CLIENT_SECRET_SLOT.to_owned()),
                TokenSourceField::Auxiliary { slot } => Some(slot.clone()),
            })
            .chain(self.basic_auth.iter().map(|v| v.secret_slot.clone()))
            .collect()
    }
    #[must_use]
    pub fn admits_auxiliary_slots(&self, slots: &std::collections::BTreeSet<String>) -> bool {
        let required = self.auxiliary_slots();
        let mut allowed = required.clone();
        allowed.insert(TOKEN_ACQUISITION_STATE_SLOT.into());
        if self.response.refresh_policy != crate::RefreshTokenPolicy::NotExpected {
            allowed.insert(OAUTH_REFRESH_TOKEN_SLOT.into());
        }
        required.is_subset(slots) && slots.is_subset(&allowed)
    }
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"-_.".contains(&v))
}

/// Caller-supplied initial grant evidence; access and refresh expiry are distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialGrantLifetime {
    pub issued_at_unix_ms: u64,
    pub access_expires_at_unix_ms: Option<u64>,
    pub refresh_expires_at_unix_ms: Option<u64>,
}

/// Secret-free observation; the Vault source and its existing material WAL
/// remain authoritative. Consumers never infer that Unknown is retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRenewalStatus {
    Ready,
    Due,
    InProgress,
    NeedsAuthorization,
    OutcomeUnknown,
    ExpiryUnknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn profile() -> Value {
        json!({"version":1,"token_endpoint":"https://issuer.example/token","encoding":"json","method":"POST",
        "parameters":{"context":{"project":"one"},"client_id":123,"refresh":{"credential":"grant","field":"renewal"}},
        "bindings":{"grant":{"renewal":{"type":"refresh_token"}}},
        "response":{"access_token_pointer":"/data/access","access_ttl_pointer":"/data/ttl","require_access_ttl":true,
          "refresh_token_pointer":"/data/refresh","refresh_policy":"replace_required","max_access_ttl_seconds":86400,"max_refresh_ttl_seconds":2592000}})
    }
    #[test]
    fn versioned_profile_admission_and_material_slots_follow_one_closed_table() {
        // P1 version/endpoint/encoding/reference/response all admitted -> one
        // stable profile. P2 changing any boundary -> reject BEFORE dispatch.
        // P3 auxiliary inputs are exact, output/state slots are bounded; a
        // static credential never gains arbitrary auxiliary authorization.
        let original: CredentialTokenExchangeProfile = serde_json::from_value(profile()).unwrap();
        original.validate().unwrap();
        assert_eq!(original.method, TokenRequestMethod::Post);
        assert_eq!(
            original.fingerprint().unwrap(),
            original.fingerprint().unwrap()
        );
        let required = std::collections::BTreeSet::from([OAUTH_REFRESH_TOKEN_SLOT.to_owned()]);
        assert!(original.admits_auxiliary_slots(&required));
        assert!(!original.admits_auxiliary_slots(&Default::default()));
        let mut extra = required;
        extra.insert("undeclared".into());
        assert!(!original.admits_auxiliary_slots(&extra));
        for (pointer, value) in [
            ("/version", json!(2)),
            ("/token_endpoint", json!("http://issuer.example/token")),
            (
                "/token_endpoint",
                json!("https://user:password@issuer.example/token"),
            ),
            (
                "/token_endpoint",
                json!("https://issuer.example/token?credential=x"),
            ),
            ("/encoding", json!("form")),
            ("/method", json!("GET")),
            ("/parameters/refresh/field", json!("undeclared")),
            ("/response/access_token_pointer", json!("/invalid~2path")),
            ("/response/refresh_policy", json!("not_expected")),
            ("/response/max_access_ttl_seconds", json!(0)),
        ] {
            let mut value_profile = profile();
            *value_profile
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("invalid test coordinate {pointer}")) = value;
            let admitted = serde_json::from_value::<CredentialTokenExchangeProfile>(value_profile)
                .ok()
                .is_some_and(|v| v.validate().is_ok());
            assert!(!admitted, "P2 {pointer}");
        }
    }
}
