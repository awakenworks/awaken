//! Worker-application projection into one claimed Session.
//!
//! This is a Session application port: implementations produce only a neutral,
//! secret-free contribution. Dispatch claims and Worker transport remain outside
//! this contract behind the neutral attempt-ownership verifier.

use std::sync::Arc;

use awaken_runtime_contract::{AttemptOwnershipVerifier, RunActivation};

use crate::ApplicationSessionContribution;

/// Whether retrying application-owned Session preparation can change its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationSessionProvisionFailureKind {
    /// Ownership, transport, or another mutable dependency may recover.
    Retryable,
    /// The frozen request or authoritative application policy rejected the attempt.
    Terminal,
}

/// Explicit result of revalidating attempt-scoped material for a frozen Session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationSessionMaterialRefresh {
    Refreshed,
    NotRequired,
}

/// Failure while an embedding application prepares its Session contribution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("application Session provisioning failed: {message}")]
pub struct ApplicationSessionProvisionError {
    message: String,
    kind: ApplicationSessionProvisionFailureKind,
}

impl ApplicationSessionProvisionError {
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ApplicationSessionProvisionFailureKind::Retryable,
        }
    }

    #[must_use]
    pub fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ApplicationSessionProvisionFailureKind::Terminal,
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.kind == ApplicationSessionProvisionFailureKind::Terminal
    }
}

/// Claim-time application projection port.
///
/// The ownership verifier hides dispatch topology. Implementations recheck it
/// around external effects; the execution host also checks the complete call.
#[async_trait::async_trait]
pub trait ApplicationSessionProvisioner: Send + Sync {
    async fn prepare(
        &self,
        activation: &RunActivation,
        session_id: &str,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<ApplicationSessionContribution, ApplicationSessionProvisionError>;

    /// Refresh attempt-scoped material referenced by an already-frozen Session.
    /// Durable baseline input must not be rebuilt or mutated by this operation.
    async fn refresh_frozen(
        &self,
        activation: &RunActivation,
        session_id: &str,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<ApplicationSessionMaterialRefresh, ApplicationSessionProvisionError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_failure_disposition_is_explicit() {
        // Cause/effect table: retryable dependency failure -> ordinary resolver
        // retry; terminal policy/request rejection -> absorbing Run failure. No
        // unclassified constructor exists, so callers must select one rule.
        let retryable = ApplicationSessionProvisionError::retryable("transport unavailable");
        let terminal = ApplicationSessionProvisionError::terminal("request rejected");

        assert!(!retryable.is_terminal());
        assert!(terminal.is_terminal());
        assert!(terminal.to_string().contains("request rejected"));
    }
}
