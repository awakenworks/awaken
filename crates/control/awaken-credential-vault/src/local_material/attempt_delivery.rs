// @formal entrypoint credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving
//! Claim-scoped envelope issuance from the same Control Vault authority as
//! ordinary exact material resolution. No credential selection occurs here.

use awaken_credential_contract::CredentialEnvelopeValidation;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::credential::CredentialUsage;
use awaken_credential_contract::{
    AttemptCredentialDelivery, AttemptCredentialDeliveryRequest, AttemptCredentialDeliverySource,
    credential_access_without_delivery,
};
use awaken_credential_contract::{
    CredentialEnvelopeIssuance, CredentialEnvelopeIssuer, CredentialMaterialError,
};
use awaken_time::WallClockExt as _;

use super::VaultLocalMaterialAuthority;

#[derive(Clone)]
pub struct VaultAttemptCredentialDeliverySource {
    authority: Arc<VaultLocalMaterialAuthority>,
    issuer: Arc<dyn CredentialEnvelopeIssuer>,
}

impl VaultAttemptCredentialDeliverySource {
    #[must_use]
    pub fn new(
        authority: Arc<VaultLocalMaterialAuthority>,
        issuer: Arc<dyn CredentialEnvelopeIssuer>,
    ) -> Self {
        Self { authority, issuer }
    }
}

#[async_trait]
impl AttemptCredentialDeliverySource for VaultAttemptCredentialDeliverySource {
    async fn deliver(
        &self,
        request: AttemptCredentialDeliveryRequest,
    ) -> Result<AttemptCredentialDelivery, String> {
        let access = credential_access_without_delivery(&request.published_access);
        let expected_provider = request
            .expected_provider
            .as_ref()
            .map(|value| value.as_borrowed());
        if access.usage == CredentialUsage::ProviderAdapter && expected_provider.is_none() {
            return Err(CredentialMaterialError::BindingMismatch.to_string());
        }
        let source = self
            .authority
            .load_active_source(&access.credential.id)
            .await
            .map_err(|error| error.to_string())?;
        self.authority
            .validate_exact_source(
                &source,
                &access,
                &request.selected_plaintext_holder,
                &request.binding,
                expected_provider,
            )
            .map_err(|error| error.to_string())?;
        let material = self
            .authority
            .materialize_source(&source)
            .await
            .map_err(|error| error.to_string())?;
        let envelope = self
            .issuer
            .issue(CredentialEnvelopeIssuance {
                access: access.clone(),
                selected_holder: request.selected_plaintext_holder.clone(),
                binding: request.binding.clone(),
                material,
            })
            .await?;
        envelope
            .validate_issuance(
                &access,
                &request.selected_plaintext_holder,
                &request.binding,
            )
            .map_err(|error| error.to_string())?;
        envelope
            .validate_for_holder(
                &request.selected_plaintext_holder,
                self.authority.clock.now_ms(),
            )
            .map_err(|error| error.to_string())?;
        Ok(AttemptCredentialDelivery {
            candidate_fingerprint: request.candidate_fingerprint,
            access: access.with_envelope(envelope),
            claim_epoch: request.claim_epoch,
        })
    }
}
