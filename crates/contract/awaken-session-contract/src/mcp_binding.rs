//! Session MCP-server credential bindings (ADR-0043), the secret-free projection a
//! session carries for its transport-level token refresh.
//!
//! Neutral by construction: every ref is a plain string (`sec:refresh:…` /
//! `sec:client:…`), never vault-typed and never secret material. The Managed wire
//! adapter resolves these from its vault; the host re-types the string refs into the
//! vault's `SecretRef` at the secret-store lookup.

/// The stored refresh configuration of an `mcp_oauth` credential, exposed for a
/// session's transport-level token refresh. Secret-free: it carries the sealed
/// refresh token's ref (and, for a confidential-client scheme, the sealed client
/// secret's ref via [`TokenEndpointAuthBinding`]), never material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRefreshBinding {
    pub token_endpoint: String,
    pub client_id: String,
    /// The ref the sealed refresh token lives under (`sec:refresh:{source_id}`), as a
    /// plain string — the port speaks no control-plane vocabulary; the host re-types it
    /// into the vault's `SecretRef` at the secret-store lookup.
    pub refresh_token_ref: String,
    /// How the refresher must authenticate the grant at the token endpoint.
    pub token_endpoint_auth: TokenEndpointAuthBinding,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

/// The client-authentication method of a refresh grant, as the session's refresher
/// must apply it (RFC 6749 §2.3.1 `Basic` header for `client_secret_basic`, form
/// field for `client_secret_post`). A confidential scheme carries the sealed client
/// secret's ref (`sec:client:{source_id}`) as a plain string, never material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenEndpointAuthBinding {
    None,
    /// Carries the sealed client secret's ref as a plain string (`sec:client:…`); the
    /// host re-types it into the vault's `SecretRef` at the secret-store lookup.
    ClientSecretBasic {
        secret_ref: String,
    },
    ClientSecretPost {
        secret_ref: String,
    },
}
