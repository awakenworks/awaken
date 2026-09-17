//! Wire types for the `vaults` resource (`beta.vaults.*`, ADR-0043): vaults and
//! their credentials. The DTOs mirror the SDK types exactly
//! (`BetaManagedAgentsVault`, `BetaManagedAgentsCredential`, the per-type
//! auth/create shapes, `BetaManagedAgentsCredentialValidation`), keeping
//! Anthropic's snake_case tags. Every wire secret (`secret_value`, `token`,
//! `access_token`, `refresh_token`, `client_secret`) is write-only — present on
//! the create/update params, never on a response projection.
//!
//! Pure serde shapes only. The store, the secret-sealing, the record→wire
//! projection and the live `McpProbe` port live in `routes::vaults`; exact
//! refresh execution facts use `CredentialRefreshAccess` directly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `BetaManagedAgentsVault` — a credential container.
#[derive(Debug, Clone, Serialize)]
pub struct Vault {
    pub id: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub display_name: String,
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub updated_at: String,
}

/// `VaultCreateParams` body.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultCreateParams {
    pub display_name: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// `BetaManagedAgentsDeletedVault`.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedVault {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
}

/// `BetaManagedAgentsDeletedCredential` — the `DELETE .../credentials/:id` receipt.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedCredential {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
}

/// `VaultListParams` / `CredentialListParams` query: the only knob is whether
/// archived rows are included (default `false` = active only).
#[derive(Debug, Clone, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub include_archived: bool,
}

/// The outbound-host substitution scope of an env-var credential
/// (`BetaManagedAgentsCredentialNetworking*`), tagged by `type`.
///
/// `BetaManagedAgentsCredentialNetworkingParams`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialNetworking {
    Unrestricted,
    Limited { allowed_hosts: Vec<String> },
}

/// Resolved `BetaManagedAgentsInjectionLocationResponse`. Both booleans are
/// always present in responses even when the create request omitted them.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct CredentialInjectionLocation {
    pub body: bool,
    pub header: bool,
}

/// Create/update spelling of `injection_location`. Presence is tracked on each
/// field so create can default an explicitly supplied object's missing member
/// to `false`, while update can merge each omitted member independently.
///
/// `BetaManagedAgentsInjectionLocationParams`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CredentialInjectionLocationParams {
    #[serde(default, deserialize_with = "super::presence::optional_non_null")]
    pub body: Option<bool>,
    #[serde(default, deserialize_with = "super::presence::optional_non_null")]
    pub header: Option<bool>,
}

/// The token-endpoint auth scheme as it arrives on the wire
/// (`BetaManagedAgentsTokenEndpointAuth{None,Basic,Post}Param`). The
/// `client_secret` is write-only: for a confidential-client scheme it is sealed
/// into the credential aggregate's named client-secret slot, never echoed —
/// every response projection stays tag-only
/// ([`TokenEndpointAuthResponse`]).
///
/// SDK: inline — `BetaManagedAgentsMCPOAuthRefreshParams.token_endpoint_auth`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenEndpointAuthParams {
    None,
    ClientSecretBasic { client_secret: String },
    ClientSecretPost { client_secret: String },
}

/// The secret-free token-endpoint auth projection
/// (`BetaManagedAgentsTokenEndpointAuth{None,Basic,Post}Response`): the scheme
/// tag only — the SDK response types carry no `client_secret`.
///
/// SDK: inline — `BetaManagedAgentsMCPOAuthRefreshResponse.token_endpoint_auth`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenEndpointAuthResponse {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

/// `BetaManagedAgentsMCPOAuthRefreshParams` — refresh configuration on create.
/// `refresh_token` is write-only: sealed in the credential aggregate's named
/// refresh-token slot, never echoed back.
#[derive(Debug, Clone, Deserialize)]
pub struct McpOauthRefreshParams {
    pub client_id: String,
    /// Write-only: sealed into the `SecretStore`, never echoed back.
    pub refresh_token: String,
    pub token_endpoint: String,
    /// The SDK requires this, but we tolerate its absence (defaults to `none`) —
    /// nothing in this slice exchanges tokens, so nothing can misfire.
    #[serde(default)]
    pub token_endpoint_auth: Option<TokenEndpointAuthParams>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// `BetaManagedAgentsMCPOAuthRefreshResponse` — the secret-free refresh
/// projection: configuration only, never the refresh token or client secret.
#[derive(Debug, Clone, Serialize)]
pub struct McpOauthRefreshResponse {
    pub client_id: String,
    pub token_endpoint: String,
    pub token_endpoint_auth: TokenEndpointAuthResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// The `auth` projection of a credential, one variant per SDK `*AuthResponse`
/// shape. None of them ever carries secret material: env-var projects the
/// variable name + networking scope, `static_bearer` only the server URL, and
/// `mcp_oauth` the server URL + secret-free refresh configuration.
///
/// SDK: inline — `BetaManagedAgentsCredential.auth`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialAuth {
    EnvironmentVariable {
        secret_name: String,
        networking: CredentialNetworking,
        injection_location: CredentialInjectionLocation,
    },
    StaticBearer {
        mcp_server_url: String,
    },
    McpOauth {
        mcp_server_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh: Option<McpOauthRefreshResponse>,
    },
}

/// `BetaManagedAgentsCredential` — the secret-free projection.
#[derive(Debug, Clone, Serialize)]
pub struct Credential {
    pub id: String,
    pub archived_at: Option<String>,
    pub auth: CredentialAuth,
    pub created_at: String,
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub updated_at: String,
    pub vault_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// `CredentialCreateParams`.
///
/// Credential create params, discriminated by `type`, one variant per SDK
/// `*CreateParams` shape. An unknown tag deserialize-fails into a clean `400`.
/// All secret fields are write-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialCreateParams {
    EnvironmentVariable {
        secret_name: String,
        /// Write-only: sealed into the `SecretStore`, never echoed back.
        secret_value: String,
        networking: CredentialNetworking,
        #[serde(default, deserialize_with = "super::presence::optional_non_null")]
        injection_location: Option<CredentialInjectionLocationParams>,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
    /// `BetaManagedAgentsStaticBearerCreateParams`.
    StaticBearer {
        mcp_server_url: String,
        /// Write-only: sealed into the `SecretStore`, never echoed back.
        token: String,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
    /// `BetaManagedAgentsMCPOAuthCreateParams`.
    McpOauth {
        mcp_server_url: String,
        /// Write-only: sealed into the `SecretStore`, never echoed back.
        access_token: String,
        #[serde(default)]
        expires_at: Option<String>,
        #[serde(default)]
        refresh: Option<McpOauthRefreshParams>,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
}

/// Accept both the historical flat body and the official SDK's `{ auth: ... }`
/// body, then normalize immediately into the one creation model above.
///
/// `CredentialCreateParams` body; also accepts the older flat Awaken body.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CredentialCreateWire {
    Legacy(CredentialCreateParams),
    Official {
        auth: CredentialCreateParams,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
}

impl CredentialCreateWire {
    #[must_use]
    pub fn into_params(self) -> CredentialCreateParams {
        match self {
            Self::Legacy(params) => params,
            Self::Official {
                auth,
                metadata,
                display_name,
            } => match auth {
                CredentialCreateParams::EnvironmentVariable {
                    secret_name,
                    secret_value,
                    networking,
                    injection_location,
                    metadata: auth_metadata,
                    display_name: auth_display_name,
                } => CredentialCreateParams::EnvironmentVariable {
                    secret_name,
                    secret_value,
                    networking,
                    injection_location,
                    metadata: if metadata.is_empty() {
                        auth_metadata
                    } else {
                        metadata
                    },
                    display_name: display_name.or(auth_display_name),
                },
                CredentialCreateParams::StaticBearer {
                    mcp_server_url,
                    token,
                    metadata: auth_metadata,
                    display_name: auth_display_name,
                } => CredentialCreateParams::StaticBearer {
                    mcp_server_url,
                    token,
                    metadata: if metadata.is_empty() {
                        auth_metadata
                    } else {
                        metadata
                    },
                    display_name: display_name.or(auth_display_name),
                },
                CredentialCreateParams::McpOauth {
                    mcp_server_url,
                    access_token,
                    expires_at,
                    refresh,
                    metadata: auth_metadata,
                    display_name: auth_display_name,
                } => CredentialCreateParams::McpOauth {
                    mcp_server_url,
                    access_token,
                    expires_at,
                    refresh,
                    metadata: if metadata.is_empty() {
                        auth_metadata
                    } else {
                        metadata
                    },
                    display_name: display_name.or(auth_display_name),
                },
            },
        }
    }
}

/// serde helper distinguishing an absent field (`None`) from an explicit JSON
/// `null` (`Some(None)`) from a present value (`Some(Some(v))`). Lets an update
/// PATCH a nullable field to `null` (clear) without conflating it with "omitted"
/// (keep) — the `display_name` semantics the SDK documents.
/// `VaultUpdateParams` body — a partial update. `display_name` replaces when
/// present (a vault name cannot be cleared, so `null`/absent both mean "keep");
/// `metadata` is a patch (an entry's `null` value deletes the key).
#[derive(Debug, Clone, Deserialize)]
pub struct VaultUpdateParams {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Option<String>>>,
}

/// `CredentialUpdateParams` body — a partial update. `auth` (when present) must
/// carry the credential's own `type` (the kind is immutable, as are `secret_name`
/// / `mcp_server_url`); a mismatch is a clean `400`. `display_name` uses
/// the shared presence-aware serde helper so `null` clears it. `metadata` is a patch.
#[derive(Debug, Clone, Deserialize)]
pub struct CredentialUpdateParams {
    #[serde(default)]
    pub auth: Option<CredentialUpdateAuth>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub display_name: Option<Option<String>>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Option<String>>>,
}

/// The typed `auth` patch of a credential update, one variant per SDK
/// `*UpdateParams`. Every field is optional; secret fields are write-only and
/// re-sealed under the credential's existing `SecretRef` when present.
///
/// SDK: inline — `CredentialUpdateParams.auth`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUpdateAuth {
    EnvironmentVariable {
        #[serde(default)]
        networking: Option<CredentialNetworking>,
        #[serde(default, deserialize_with = "super::presence::optional_non_null")]
        injection_location: Option<CredentialInjectionLocationParams>,
        /// Write-only: re-sealed under the row's `material_ref`, never echoed.
        #[serde(default)]
        secret_value: Option<String>,
    },
    StaticBearer {
        /// Write-only: re-sealed under the row's `material_ref`, never echoed.
        #[serde(default)]
        token: Option<String>,
    },
    McpOauth {
        /// Write-only: re-sealed under the row's `material_ref`, never echoed.
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        expires_at: Option<String>,
        #[serde(default)]
        refresh: Option<McpOauthRefreshUpdate>,
    },
}

/// The `refresh` patch of an `mcp_oauth` credential update
/// (`BetaManagedAgentsMCPOAuthRefreshUpdateParams`). Only applies to a credential
/// that already carries a refresh configuration; the `refresh_token` and any
/// confidential-client `client_secret` are re-sealed under their existing sibling
/// refs, never echoed.
#[derive(Debug, Clone, Deserialize)]
pub struct McpOauthRefreshUpdate {
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth: Option<TokenEndpointAuthUpdate>,
}

/// The token-endpoint auth **update** patch
/// (`BetaManagedAgentsTokenEndpointAuth{Basic,Post}UpdateParam`). Distinct from
/// the create shape ([`TokenEndpointAuthParams`]): on update `client_secret` is
/// OPTIONAL — omitting it switches/keeps the scheme against the *already-sealed*
/// client secret, while supplying it re-seals. `None` is accepted too (lenient;
/// the SDK's update type does not send it) and drops the confidential binding.
///
/// SDK: inline — `BetaManagedAgentsMCPOAuthRefreshUpdateParams.token_endpoint_auth` (also accepts `none`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenEndpointAuthUpdate {
    None,
    ClientSecretBasic {
        #[serde(default)]
        client_secret: Option<String>,
    },
    ClientSecretPost {
        #[serde(default)]
        client_secret: Option<String>,
    },
}

/// `BetaManagedAgentsCredentialValidationStatus`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialValidationStatus {
    Valid,
    Invalid,
    Unknown,
}

/// `BetaManagedAgentsRefreshHTTPResponse` — an HTTP response captured during a
/// credential validation probe (the MCP server's, or the token endpoint's).
/// `body` has sensitive values scrubbed and is bounded; `content_type` is empty
/// when the response carried no `Content-Type` header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialProbeHttpResponse {
    pub body: String,
    pub body_truncated: bool,
    pub content_type: String,
    pub status_code: u16,
}

impl From<awaken_session_contract::ProbeHttpResponse> for CredentialProbeHttpResponse {
    fn from(response: awaken_session_contract::ProbeHttpResponse) -> Self {
        Self {
            body: response.body,
            body_truncated: response.body_truncated,
            content_type: response.content_type,
            status_code: response.status_code,
        }
    }
}

/// `BetaManagedAgentsMCPProbe` — the failing step of an MCP validation probe:
/// the MCP method that failed (for example `initialize` or `tools/list`) and,
/// when the server answered, its HTTP response. Absent (`null`) when the probe
/// completed or never ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialMcpProbe {
    pub http_response: Option<CredentialProbeHttpResponse>,
    pub method: String,
}

impl From<awaken_session_contract::McpProbeFailure> for CredentialMcpProbe {
    fn from(failure: awaken_session_contract::McpProbeFailure) -> Self {
        Self {
            http_response: failure.http_response.map(Into::into),
            method: failure.method,
        }
    }
}

/// SDK: inline — `BetaManagedAgentsRefreshObject.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRefreshStatus {
    Succeeded,
    Failed,
    ConnectError,
    NoRefreshToken,
}

/// `BetaManagedAgentsRefreshObject` — the outcome of a refresh-token exchange
/// attempted during credential validation. `http_response` is the token
/// endpoint's response whenever one arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialRefreshResult {
    pub http_response: Option<CredentialProbeHttpResponse>,
    pub status: CredentialRefreshStatus,
}

impl CredentialRefreshResult {
    /// An auth-rejected credential that holds no refresh token to exchange.
    #[must_use]
    pub fn no_refresh_token() -> Self {
        Self {
            http_response: None,
            status: CredentialRefreshStatus::NoRefreshToken,
        }
    }
}

impl From<awaken_session_contract::RefreshExchangeOutcome> for CredentialRefreshResult {
    fn from(outcome: awaken_session_contract::RefreshExchangeOutcome) -> Self {
        use awaken_session_contract::RefreshExchangeStatus;
        Self {
            http_response: outcome.http_response.map(Into::into),
            status: match outcome.status {
                RefreshExchangeStatus::Succeeded => CredentialRefreshStatus::Succeeded,
                RefreshExchangeStatus::Failed => CredentialRefreshStatus::Failed,
                RefreshExchangeStatus::ConnectError => CredentialRefreshStatus::ConnectError,
            },
        }
    }
}

/// `BetaManagedAgentsCredentialValidation`.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialValidation {
    pub credential_id: String,
    pub has_refresh_token: bool,
    pub mcp_probe: Option<CredentialMcpProbe>,
    pub refresh: Option<CredentialRefreshResult>,
    pub status: CredentialValidationStatus,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub validated_at: String,
    pub vault_id: String,
}
