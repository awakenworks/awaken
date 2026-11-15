// @formal evidence worker.secret_materialization_is_claim_fenced
//! Brokered model grants over the one Worker credential grant endpoint.
//!
//! On a registered Worker a brokered candidate obtains its grants under the
//! attempt's dispatch claim ([`ClaimBrokeredInferenceClient`]); the Coordinator
//! answers them with the deployment's broker identity
//! ([`BrokeredInferenceGrantIssuer`]). Both sides map the broker's refusals
//! through one table, so a Worker reports exactly what the broker decided.

use std::sync::Arc;

use super::{
    BrokeredInferenceClient, BrokeredInferenceError, BrokeredInferenceLease,
    BrokeredInferenceRequest,
};
use async_trait::async_trait;
use awaken_agent_contract::credential::CandidateFingerprint;
use awaken_credential_contract::{
    BrokeredGrantIssuer, BrokeredGrantOperation, BrokeredGrantRefusal, BrokeredGrantTarget,
    BrokeredModelAccess, ClaimCredentialGrant, ClaimCredentialGrantMaterial, ClaimCredentialGrants,
    GrantedSecret,
};

fn millis(duration: Option<std::time::Duration>) -> Option<u64> {
    duration.map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
}

/// The broker's error as the wire refusal (Coordinator side).
#[must_use]
pub fn brokered_refusal(error: BrokeredInferenceError) -> BrokeredGrantRefusal {
    match error {
        BrokeredInferenceError::AuthenticationRequired => {
            BrokeredGrantRefusal::AuthenticationRequired
        }
        BrokeredInferenceError::AccountSelectionRequired => {
            BrokeredGrantRefusal::AccountSelectionRequired
        }
        BrokeredInferenceError::SubscriptionRequired => BrokeredGrantRefusal::SubscriptionRequired,
        BrokeredInferenceError::ModelNotEntitled => BrokeredGrantRefusal::ModelNotEntitled,
        BrokeredInferenceError::ModelUnavailable => BrokeredGrantRefusal::ModelUnavailable,
        BrokeredInferenceError::InsufficientBalance => BrokeredGrantRefusal::InsufficientBalance,
        BrokeredInferenceError::QuotaExceeded { retry_after } => {
            BrokeredGrantRefusal::QuotaExceeded {
                retry_after_ms: millis(retry_after),
            }
        }
        BrokeredInferenceError::RateLimited { retry_after } => BrokeredGrantRefusal::RateLimited {
            retry_after_ms: millis(retry_after),
        },
        BrokeredInferenceError::TemporarilyUnavailable => {
            BrokeredGrantRefusal::TemporarilyUnavailable
        }
        BrokeredInferenceError::InvalidRequest => BrokeredGrantRefusal::InvalidRequest,
        BrokeredInferenceError::CloudModelsDisabled => BrokeredGrantRefusal::CloudModelsDisabled,
        BrokeredInferenceError::SignInRequired => BrokeredGrantRefusal::SignInRequired,
    }
}

/// The wire refusal as the broker's error (Worker side).
#[must_use]
pub fn brokered_error(refusal: BrokeredGrantRefusal) -> BrokeredInferenceError {
    let duration = |ms: Option<u64>| ms.map(std::time::Duration::from_millis);
    match refusal {
        BrokeredGrantRefusal::AuthenticationRequired => {
            BrokeredInferenceError::AuthenticationRequired
        }
        BrokeredGrantRefusal::AccountSelectionRequired => {
            BrokeredInferenceError::AccountSelectionRequired
        }
        BrokeredGrantRefusal::SubscriptionRequired => BrokeredInferenceError::SubscriptionRequired,
        BrokeredGrantRefusal::ModelNotEntitled => BrokeredInferenceError::ModelNotEntitled,
        BrokeredGrantRefusal::ModelUnavailable => BrokeredInferenceError::ModelUnavailable,
        BrokeredGrantRefusal::InsufficientBalance => BrokeredInferenceError::InsufficientBalance,
        BrokeredGrantRefusal::QuotaExceeded { retry_after_ms } => {
            BrokeredInferenceError::QuotaExceeded {
                retry_after: duration(retry_after_ms),
            }
        }
        BrokeredGrantRefusal::RateLimited { retry_after_ms } => {
            BrokeredInferenceError::RateLimited {
                retry_after: duration(retry_after_ms),
            }
        }
        BrokeredGrantRefusal::TemporarilyUnavailable => {
            BrokeredInferenceError::TemporarilyUnavailable
        }
        BrokeredGrantRefusal::InvalidRequest => BrokeredInferenceError::InvalidRequest,
        BrokeredGrantRefusal::CloudModelsDisabled => BrokeredInferenceError::CloudModelsDisabled,
        BrokeredGrantRefusal::SignInRequired => BrokeredInferenceError::SignInRequired,
    }
}

fn lease(access: BrokeredModelAccess) -> BrokeredInferenceLease {
    BrokeredInferenceLease {
        grant_id: access.grant_id,
        gateway_base_url: access.gateway_base_url,
        capability: access.capability.into_redacted(),
        grant_expires_at: access.grant_expires_at,
    }
}

/// A registered Worker's broker for one brokered candidate: every grant is
/// requested under the attempt's dispatch claim. The Coordinator derives the
/// exact target from the frozen candidate, so the request's own target fields
/// are not sent; only the operation's idempotency key is.
pub struct ClaimBrokeredInferenceClient {
    grants: Arc<dyn ClaimCredentialGrants>,
    candidate: CandidateFingerprint,
}

impl ClaimBrokeredInferenceClient {
    #[must_use]
    pub fn new(grants: Arc<dyn ClaimCredentialGrants>, candidate: CandidateFingerprint) -> Self {
        Self { grants, candidate }
    }

    async fn grant(
        &self,
        operation: BrokeredGrantOperation,
    ) -> Result<ClaimCredentialGrantMaterial, BrokeredInferenceError> {
        match self
            .grants
            .grant(ClaimCredentialGrant::BrokeredModel {
                candidate_fingerprint: self.candidate.clone(),
                operation,
            })
            .await
        {
            Ok(Some(ClaimCredentialGrantMaterial::BrokeredModelRefused(refusal))) => {
                Err(brokered_error(refusal))
            }
            Ok(Some(material)) => Ok(material),
            // The claim was superseded: this attempt may not obtain access.
            Ok(None) => Err(BrokeredInferenceError::InvalidRequest),
            Err(_) => Err(BrokeredInferenceError::TemporarilyUnavailable),
        }
    }

    async fn access(
        &self,
        operation: BrokeredGrantOperation,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
        match self.grant(operation).await? {
            ClaimCredentialGrantMaterial::BrokeredModel(access) => Ok(lease(access)),
            _ => Err(BrokeredInferenceError::InvalidRequest),
        }
    }
}

#[async_trait]
impl BrokeredInferenceClient for ClaimBrokeredInferenceClient {
    async fn create_grant(
        &self,
        request: BrokeredInferenceRequest,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
        self.access(BrokeredGrantOperation::Acquire {
            idempotency_key: request.idempotency_key,
        })
        .await
    }

    async fn renew_grant(
        &self,
        grant_id: &str,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
        self.access(BrokeredGrantOperation::Renew {
            grant_id: grant_id.into(),
        })
        .await
    }

    async fn close_grant(&self, grant_id: &str) -> Result<(), BrokeredInferenceError> {
        match self
            .grant(BrokeredGrantOperation::Close {
                grant_id: grant_id.into(),
            })
            .await?
        {
            ClaimCredentialGrantMaterial::BrokeredModelClosed => Ok(()),
            _ => Err(BrokeredInferenceError::InvalidRequest),
        }
    }
}

/// The Coordinator's brokered grant issuer over the process broker identity.
/// Supply disabled or no authenticated identity are the issuer's own answers,
/// so a Worker learns exactly why no grant exists.
pub struct BrokeredInferenceGrantIssuer {
    client: Option<Arc<dyn BrokeredInferenceClient>>,
    enabled: bool,
}

impl BrokeredInferenceGrantIssuer {
    #[must_use]
    pub fn new(enabled: bool, client: Option<Arc<dyn BrokeredInferenceClient>>) -> Self {
        Self { client, enabled }
    }

    fn client(&self) -> Result<&dyn BrokeredInferenceClient, BrokeredGrantRefusal> {
        if !self.enabled {
            return Err(BrokeredGrantRefusal::CloudModelsDisabled);
        }
        self.client
            .as_deref()
            .ok_or(BrokeredGrantRefusal::SignInRequired)
    }
}

fn access(lease: BrokeredInferenceLease) -> BrokeredModelAccess {
    BrokeredModelAccess {
        grant_id: lease.grant_id,
        gateway_base_url: lease.gateway_base_url,
        capability: GrantedSecret::from_redacted(&lease.capability),
        grant_expires_at: lease.grant_expires_at,
    }
}

#[async_trait]
impl BrokeredGrantIssuer for BrokeredInferenceGrantIssuer {
    async fn acquire(
        &self,
        target: BrokeredGrantTarget,
        idempotency_key: String,
    ) -> Result<BrokeredModelAccess, BrokeredGrantRefusal> {
        self.client()?
            .create_grant(BrokeredInferenceRequest {
                provider: target.provider,
                original_model_id: target.original_model_id,
                native_protocol: target.native_protocol,
                local_run_correlation: Some(target.local_run_correlation),
                idempotency_key,
            })
            .await
            .map(access)
            .map_err(brokered_refusal)
    }

    async fn renew(&self, grant_id: &str) -> Result<BrokeredModelAccess, BrokeredGrantRefusal> {
        self.client()?
            .renew_grant(grant_id)
            .await
            .map(access)
            .map_err(brokered_refusal)
    }

    async fn close(&self, grant_id: &str) -> Result<(), BrokeredGrantRefusal> {
        self.client()?
            .close_grant(grant_id)
            .await
            .map_err(brokered_refusal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_broker_refusal_survives_the_wire_unchanged() {
        let retry = Some(std::time::Duration::from_millis(1_500));
        for error in [
            BrokeredInferenceError::AuthenticationRequired,
            BrokeredInferenceError::AccountSelectionRequired,
            BrokeredInferenceError::SubscriptionRequired,
            BrokeredInferenceError::ModelNotEntitled,
            BrokeredInferenceError::ModelUnavailable,
            BrokeredInferenceError::InsufficientBalance,
            BrokeredInferenceError::QuotaExceeded { retry_after: retry },
            BrokeredInferenceError::QuotaExceeded { retry_after: None },
            BrokeredInferenceError::RateLimited { retry_after: retry },
            BrokeredInferenceError::TemporarilyUnavailable,
            BrokeredInferenceError::InvalidRequest,
            BrokeredInferenceError::CloudModelsDisabled,
            BrokeredInferenceError::SignInRequired,
        ] {
            let wire = serde_json::to_value(brokered_refusal(error.clone())).unwrap();
            let back: BrokeredGrantRefusal = serde_json::from_value(wire).unwrap();
            assert_eq!(brokered_error(back), error);
        }
    }

    #[tokio::test]
    async fn the_coordinator_issuer_answers_supply_refusals_itself() {
        // I1 supply disabled => CloudModelsDisabled; I2 enabled without an
        // authenticated identity => SignInRequired; both before any broker I/O.
        let target = BrokeredGrantTarget {
            provider: "vendor".into(),
            original_model_id: "model".into(),
            native_protocol: "openai_chat_completions".into(),
            local_run_correlation: "run".into(),
        };
        for (rule, enabled, expected) in [
            ("I1", false, BrokeredGrantRefusal::CloudModelsDisabled),
            ("I2", true, BrokeredGrantRefusal::SignInRequired),
        ] {
            let issuer = BrokeredInferenceGrantIssuer::new(enabled, None);
            assert_eq!(
                issuer.acquire(target.clone(), "key".into()).await,
                Err(expected.clone()),
                "{rule}"
            );
            assert_eq!(issuer.close("grant").await, Err(expected), "{rule}");
        }
    }
}
