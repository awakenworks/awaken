//! Host-side credential resolution (ADR-0045 D4).
//!
//! A `ConnectionPlan` carries a [`CredentialRef`]; the host resolves it to opaque
//! [`AppliedAuth`] material immediately before dialing, via the vault. Resolved
//! material never lives in the plan. This mirrors `awaken-connection-auth`'s
//! header material without binding this slice to that crate.

use async_trait::async_trait;

use crate::plan::CredentialRef;

/// Opaque handshake material applied to a connection. Header-shaped so a bearer
/// token or an injected header is expressible without a bespoke proof format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedAuth {
    headers: Vec<(String, String)>,
}

impl AppliedAuth {
    /// No credential — correct for a loopback InProcess/Unix connection.
    pub fn none() -> Self {
        Self::default()
    }

    /// A bearer token presented as an `authorization` header.
    pub fn bearer(token: impl Into<String>) -> Self {
        Self {
            headers: vec![("authorization".to_string(), format!("Bearer {}", token.into()))],
        }
    }

    /// The header pairs to present on dial.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }
}

/// Resolves a `CredentialRef` to applied material, in the host, just before dial.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    async fn resolve(&self, credential: &CredentialRef) -> Result<AppliedAuth, CredentialError>;
}

/// Why a credential could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("unknown credential reference: {0}")]
    Unknown(String),
    #[error("credential resolution failed: {0}")]
    Failed(String),
}

/// A resolver that grants no credential — the loopback default.
pub struct NoAuth;

#[async_trait]
impl CredentialResolver for NoAuth {
    async fn resolve(&self, _credential: &CredentialRef) -> Result<AppliedAuth, CredentialError> {
        Ok(AppliedAuth::none())
    }
}
