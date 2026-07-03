//! Opaque credentials for authenticating to an MCP server.
//!
//! A [`Credential`] carries an already-resolved secret value (a bearer token or
//! a header pair) — never a vault reference or lookup policy. The host resolves
//! its secrets and hands this crate the opaque value, keeping credential
//! mechanics (vaults, OAuth refresh) out of the runtime/extension boundary
//! (D6/D9): this crate only attaches the value to a request.

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
