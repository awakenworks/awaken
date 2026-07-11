//! Wire types for the `vaults` resource (`beta.vaults.*`, ADR-0043): vaults and
//! their credentials. The DTOs mirror the SDK types exactly
//! (`BetaManagedAgentsVault`, `BetaManagedAgentsCredential`, the per-type
//! auth/create shapes, `BetaManagedAgentsCredentialValidation`), keeping
//! Anthropic's snake_case tags. Every wire secret (`secret_value`, `token`,
//! `access_token`, `refresh_token`, `client_secret`) is write-only — present on
//! the create/update params, never on a response projection.
//!
//! Pure serde shapes only. The store, the secret-sealing, the record→wire
//! projection, and the internal binding vocabulary (`McpRefreshBinding`,
//! `TokenEndpointAuthBinding`, `McpProbe`) live in `routes::vaults`.

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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialNetworking {
    Unrestricted,
    Limited { allowed_hosts: Vec<String> },
}

/// The token-endpoint auth scheme as it arrives on the wire
/// (`BetaManagedAgentsTokenEndpointAuth{None,Basic,Post}Param`). The
/// `client_secret` is write-only: for a confidential-client scheme it is sealed
/// into the `SecretStore` under `sec:client:{source_id}` (see `routes::vaults`),
/// never echoed — every response projection stays tag-only
/// ([`TokenEndpointAuthResponse`]).
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
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenEndpointAuthResponse {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

/// `BetaManagedAgentsMCPOAuthRefreshParams` — refresh configuration on create.
/// `refresh_token` is write-only: sealed under its own `SecretRef` next to the
/// credential row, never echoed back.
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
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialAuth {
    EnvironmentVariable {
        secret_name: String,
        networking: CredentialNetworking,
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

/// serde helper distinguishing an absent field (`None`) from an explicit JSON
/// `null` (`Some(None)`) from a present value (`Some(Some(v))`). Lets an update
/// PATCH a nullable field to `null` (clear) without conflating it with "omitted"
/// (keep) — the `display_name` semantics the SDK documents.
pub(crate) fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

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
/// [`double_option`] so `null` clears it. `metadata` is a patch.
#[derive(Debug, Clone, Deserialize)]
pub struct CredentialUpdateParams {
    #[serde(default)]
    pub auth: Option<CredentialUpdateAuth>,
    #[serde(default, deserialize_with = "double_option")]
    pub display_name: Option<Option<String>>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Option<String>>>,
}

/// The typed `auth` patch of a credential update, one variant per SDK
/// `*UpdateParams`. Every field is optional; secret fields are write-only and
/// re-sealed under the credential's existing `SecretRef` when present.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUpdateAuth {
    EnvironmentVariable {
        #[serde(default)]
        networking: Option<CredentialNetworking>,
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

/// The live MCP handshake detail of a credential validation — either a successful
/// handshake or the auth-challenge HTTP status. Present only when a probe ran.
#[derive(Debug, Clone, Serialize)]
pub struct McpProbeResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl McpProbeResult {
    pub fn ok() -> Self {
        Self {
            handshake: Some("ok"),
            http_status: None,
        }
    }

    pub fn invalid(http_status: u16) -> Self {
        Self {
            handshake: None,
            http_status: Some(http_status),
        }
    }
}

/// `BetaManagedAgentsCredentialValidation`.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialValidation {
    pub credential_id: String,
    pub has_refresh_token: bool,
    pub mcp_probe: Option<McpProbeResult>,
    pub refresh: Option<McpOauthRefreshResponse>,
    pub status: CredentialValidationStatus,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub validated_at: String,
    pub vault_id: String,
}
