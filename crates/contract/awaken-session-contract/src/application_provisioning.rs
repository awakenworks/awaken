//! Worker-application projection into one claimed Session.
//!
//! This is a Session application port: implementations produce only a neutral,
//! secret-free contribution. Dispatch claims and Worker transport remain outside
//! this contract behind the neutral attempt-ownership verifier.

use std::sync::Arc;

use awaken_runtime_contract::{AttemptOwnershipVerifier, RunActivation};

use crate::ApplicationSessionContribution;

/// Failure while an embedding application prepares its Session contribution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("application Session provisioning failed: {0}")]
pub struct ApplicationSessionProvisionError(String);

impl ApplicationSessionProvisionError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
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
        _activation: &RunActivation,
        _session_id: &str,
        _ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<(), ApplicationSessionProvisionError> {
        Ok(())
    }
}
