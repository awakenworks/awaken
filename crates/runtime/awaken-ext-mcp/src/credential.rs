//! Opaque credentials for authenticating to an MCP server.
//!
//! A [`Credential`] carries an already-resolved secret value (a bearer token or
//! a header pair) — never a vault reference or lookup policy. The host resolves
//! its secrets and hands this crate the opaque value, keeping credential
//! mechanics (vaults, OAuth refresh) out of the runtime/extension boundary
//! (D6/D9): this crate only attaches the value to a request.
//!
//! Rotation stays host-owned through two hooks on the HTTP transport: the host
//! can push a fresh value at any time
//! ([`set_credential`](crate::http::HttpTransport::set_credential)), and it can
//! register a [`CredentialRefresher`] that the transport calls when a server
//! answers 401/403 — the OAuth/vault machinery runs on the host side of that
//! callback, and this crate only retries with whatever value comes back.

use async_trait::async_trait;

/// The auth failure a server answered with: the HTTP status plus the
/// `WWW-Authenticate` header when present (for OAuth servers this carries the
/// RFC 9728 resource-metadata pointer the host needs to start authorization).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    pub status: u16,
    pub www_authenticate: Option<String>,
}

impl std::fmt::Display for AuthChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.www_authenticate {
            Some(header) => write!(f, "HTTP {} (WWW-Authenticate: {header})", self.status),
            None => write!(f, "HTTP {}", self.status),
        }
    }
}

/// Host-side hook for re-resolving a credential after an auth failure.
///
/// Returning `Some` makes the transport store the new credential and retry the
/// failed request once; returning `None` surfaces the challenge as a transport
/// error (the host may still complete an interactive flow out-of-band and call
/// `set_credential` later).
#[async_trait]
pub trait CredentialRefresher: Send + Sync {
    async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential>;
}

/// How to authenticate an HTTP MCP request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Credential {
    /// No authentication.
    #[default]
    None,
    /// `Authorization: Bearer <token>`.
    Bearer(String),
    /// An arbitrary header, e.g. `X-Api-Key: <value>`.
    Header { name: String, value: String },
}

impl Credential {
    /// The header this credential contributes, if any: `(name, value)`.
    pub fn header(&self) -> Option<(String, String)> {
        match self {
            Credential::None => None,
            Credential::Bearer(token) => {
                Some(("Authorization".to_string(), format!("Bearer {token}")))
            }
            Credential::Header { name, value } => Some((name.clone(), value.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_contributes_no_header() {
        assert_eq!(Credential::None.header(), None);
    }

    #[test]
    fn bearer_becomes_an_authorization_header() {
        assert_eq!(
            Credential::Bearer("tok".to_string()).header(),
            Some(("Authorization".to_string(), "Bearer tok".to_string()))
        );
    }

    #[test]
    fn challenge_display_includes_www_authenticate() {
        let challenge = AuthChallenge {
            status: 401,
            www_authenticate: Some("Bearer realm=\"mcp\"".to_string()),
        };
        assert_eq!(
            challenge.to_string(),
            "HTTP 401 (WWW-Authenticate: Bearer realm=\"mcp\")"
        );
    }

    #[test]
    fn challenge_display_without_www_authenticate() {
        let challenge = AuthChallenge {
            status: 403,
            www_authenticate: None,
        };
        assert_eq!(challenge.to_string(), "HTTP 403");
    }

    #[test]
    fn custom_header_passes_through() {
        assert_eq!(
            Credential::Header {
                name: "X-Api-Key".to_string(),
                value: "k".to_string(),
            }
            .header(),
            Some(("X-Api-Key".to_string(), "k".to_string()))
        );
    }
}
