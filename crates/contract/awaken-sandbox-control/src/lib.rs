// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Passive control-protocol vocabulary: the only plaintext holder is SandboxControlSecret,
// @formal reason: zeroized on drop and redacted in Debug alongside the response username/password;
// @formal reason: RepositoryGitCredentialQuery::validate rejects non-HTTPS and line-injected queries
// @formal reason: before any service sees them. The crate stores and resolves no credential itself; the
// @formal reason: Session-owned SandboxControlService remains the sole resolver and closing its
// @formal reason: publication lease revokes the transport.
//! Closed, provider-neutral control protocol for one live Sandbox.
//!
//! This crate owns only the protocol vocabulary (service kinds, request and
//! response messages, the reserved in-Sandbox directory) and the publication
//! ports. Framing, rendezvous endpoints, and the shared publication lifecycle
//! are execution mechanics owned by `awaken-sandbox-control-transport`. It owns
//! no Session, claim, credential, repository, policy, cache, or persistence
//! state. A [`SandboxControlService`] must resolve every request against its
//! owning application's current authority.
//!
//! # Boundary
//!
//! Context: `environment`.
//!
//! Implemented by: `awaken-sandbox-container`, `awaken-sandbox-local`,
//! `awaken-session-environment`.
//!
//! Called by: `awaken-provisioning-contract`, `awaken-sandbox`, `awaken-sandbox-container`,
//! `awaken-sandbox-control-transport`, `awaken-sandbox-kubernetes`, `awaken-sandbox-local`,
//! `awaken-session-environment`, `awaken-session-environment-contract`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Runtime-owned rendezvous directory. It is outside the repository/workspace
/// namespace and is projected read-only into untrusted workloads. Spec
/// admission reserves this whole directory, so it is contract vocabulary;
/// provider-private endpoints inside it belong to the transport.
pub const SANDBOX_CONTROL_DIRECTORY_PATH: &str = "/run/awaken/control";

/// The one in-Sandbox endpoint of [`SandboxControlServiceKind::RepositoryGitCredential`]
/// shared by Namespace and hosted Kubernetes. Providers project it differently
/// outside the Sandbox, but Agent/Hand processes inherit one
/// transport-independent helper overlay, so the workload-facing address is
/// contract vocabulary: the Runtime Host renders it into helper argv without
/// linking a transport.
pub const REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH: &str =
    "/run/awaken/control/repository-git-credential.sock";

/// Closed service families a Sandbox provider may publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxControlServiceKind {
    RepositoryGitCredential,
}

/// Exact rewritten HTTPS coordinate supplied by Git's credential-helper wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryGitCredentialQuery {
    pub protocol: String,
    pub host: String,
    pub path: String,
}

impl RepositoryGitCredentialQuery {
    /// Reject ambiguous or line-oriented values before they reach an authority.
    pub fn validate(&self) -> Result<(), SandboxControlProtocolError> {
        if self.protocol != "https"
            || self.host.trim().is_empty()
            || self.path.trim_matches('/').is_empty()
            || [&self.protocol, &self.host, &self.path]
                .into_iter()
                .any(|value| {
                    value
                        .chars()
                        .any(|character| matches!(character, '\r' | '\n' | '\0'))
                })
        {
            return Err(SandboxControlProtocolError::InvalidMessage);
        }
        Ok(())
    }
}

/// One request on the closed Sandbox control port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlRequest {
    RepositoryGitCredentialGet { query: RepositoryGitCredentialQuery },
}

/// Exact wall-clock upper bound supplied by the capability issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityExpiresAtUnixMs(pub u64);

impl CapabilityExpiresAtUnixMs {
    #[must_use]
    pub const fn is_live_at(self, now_unix_ms: u64) -> bool {
        self.0 > now_unix_ms
    }
}

/// Secret bytes used only on the private, process-local Sandbox control wire.
///
/// Unlike durable/public contracts, this type is intentionally serializable so
/// the provider-neutral transport can deliver one freshly verified capability
/// to Git. It is redacted in operator surfaces and zeroized on drop.
#[derive(Serialize)]
#[serde(transparent)]
pub struct SandboxControlSecret(String);

impl SandboxControlSecret {
    pub fn new(value: impl Into<String>) -> Result<Self, SandboxControlProtocolError> {
        let mut value = value.into();
        if value.is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            value.zeroize();
            return Err(SandboxControlProtocolError::InvalidMessage);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SandboxControlSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl Drop for SandboxControlSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SandboxControlSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SandboxControlSecret([redacted])")
    }
}

/// One response from the closed Sandbox control port.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlResponse {
    RepositoryGitCredential {
        username: SandboxControlSecret,
        password: SandboxControlSecret,
        expires_at_unix_ms: CapabilityExpiresAtUnixMs,
    },
    Denied,
    Unavailable,
}

impl std::fmt::Debug for SandboxControlResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepositoryGitCredential {
                expires_at_unix_ms, ..
            } => formatter
                .debug_struct("RepositoryGitCredential")
                .field("username", &"[REDACTED]")
                .field("password", &"[REDACTED]")
                .field("expires_at_unix_ms", expires_at_unix_ms)
                .finish(),
            Self::Denied => formatter.write_str("Denied"),
            Self::Unavailable => formatter.write_str("Unavailable"),
        }
    }
}

/// Application-owned request handler. Implementations must perform a fresh
/// authority read; this port deliberately offers no seed/store/cache method.
#[async_trait]
pub trait SandboxControlService: Send + Sync {
    async fn handle(&self, request: SandboxControlRequest) -> SandboxControlResponse;
}

/// Session-owned publication lease. Closing it revokes the provider transport;
/// it does not mutate application authority.
#[async_trait]
pub trait PublishedSandboxControlService: Send + Sync {
    async fn close(&self);
}

/// Provider capability for publishing one application-owned control handler.
#[async_trait]
pub trait SandboxControlServicePublisher: Send + Sync {
    async fn publish_sandbox_control_service(
        &self,
        kind: SandboxControlServiceKind,
        service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError>;
}

#[derive(Debug, thiserror::Error)]
#[error("Sandbox control service publication is unavailable")]
pub struct SandboxControlPublishError;

/// Stable framing failures. Payload bytes and serde diagnostics are omitted so
/// a credential-bearing response cannot enter an error or log accidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SandboxControlProtocolError {
    #[error("Sandbox control frame is invalid")]
    InvalidFrame,
    #[error("Sandbox control message is invalid")]
    InvalidMessage,
    #[error("Sandbox control transport is unavailable")]
    Transport,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_query_rejects_non_https_and_line_injection() {
        /* Credential validation table: C1=canonical HTTPS authority and
         * nonempty LF/CR/NUL-free secret; C2=non-HTTPS/empty/query injection;
         * C3=wire-deserialized empty, CRLF, or NUL secret; C4=valid credential
         * response is formatted for diagnostics. E1=C1 validates; E2=C2 and
         * C3 reject before use; E3=C4 redacts both fields. Rules CV1 C1=>E1;
         * CV2 C2=>E2; CV3 C3=>E2; CV4 C1+C4=>E3.
         */
        let query = |protocol: &str, host: &str, path: &str| RepositoryGitCredentialQuery {
            protocol: protocol.into(),
            host: host.into(),
            path: path.into(),
        };
        assert!(
            query("https", "gateway.test", "git/repo")
                .validate()
                .is_ok()
        );
        assert!(
            query("http", "gateway.test", "git/repo")
                .validate()
                .is_err()
        );
        assert!(
            query("https", "gateway.test\npassword=x", "git/repo")
                .validate()
                .is_err()
        );
        assert!(query("https", "gateway.test", "").validate().is_err());
        assert!(SandboxControlSecret::new("line\ninjection").is_err());
        assert!(SandboxControlSecret::new("").is_err());

        for encoded in [r#""""#, r#""line\r\nbreak""#, r#""prefix\u0000suffix""#] {
            assert!(
                serde_json::from_str::<SandboxControlSecret>(encoded).is_err(),
                "CV3/E2 {encoded}"
            );
        }
        let response: SandboxControlResponse = serde_json::from_value(serde_json::json!({
            "type": "repository_git_credential",
            "username": "git-user",
            "password": "valid-capability", // awaken-allow(secret): -- inert redaction fixture
            "expires_at_unix_ms": 42
        }))
        .unwrap();
        let debug = format!("{response:?}");
        assert!(debug.contains("REDACTED"), "CV4/E3 marker");
        assert!(!debug.contains("git-user"), "CV4/E3 username");
        assert!(!debug.contains("valid-capability"), "CV4/E3 password");
    }
}
