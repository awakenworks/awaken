//! Claim-scoped recipient delivery from the existing Vault authority.
//!
//! This adapter owns no selection, queue or publication state. It revalidates
//! the exact published credential revision and issues only a new transport for
//! the already-frozen holder and target binding.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_credential_contract::{
    CredentialEnvelopeIssuance, CredentialEnvelopeIssuer, CredentialMaterialError,
};
use awaken_runtime_contract::{
    AttemptCredentialDelivery, AttemptCredentialDeliveryRequest, AttemptCredentialDeliverySource,
    credential_access_without_delivery,
};

use super::{PinnedCredentialMaterializer, unix_time_ms};

#[derive(Clone)]
pub struct VaultAttemptCredentialDeliverySource {
    materializer: PinnedCredentialMaterializer,
    issuer: Arc<dyn CredentialEnvelopeIssuer>,
}

impl VaultAttemptCredentialDeliverySource {
    #[must_use]
    pub fn new(
        materializer: PinnedCredentialMaterializer,
        issuer: Arc<dyn CredentialEnvelopeIssuer>,
    ) -> Self {
        Self {
            materializer,
            issuer,
        }
    }
}

#[async_trait]
impl AttemptCredentialDeliverySource for VaultAttemptCredentialDeliverySource {
    async fn deliver(
        &self,
        request: AttemptCredentialDeliveryRequest,
    ) -> Result<AttemptCredentialDelivery, String> {
        let access = credential_access_without_delivery(&request.published_access);
        let source = self
            .materializer
            .load_active_source(&access.credential.id)
            .await
            .map_err(|error| error.to_string())?;
        let revision = u64::try_from(source.version)
            .map_err(|_| CredentialMaterialError::RevisionMismatch.to_string())?;
        if revision != access.credential.revision {
            return Err(CredentialMaterialError::RevisionMismatch.to_string());
        }
        if source.workspace_id != request.binding.workspace_id {
            return Err(CredentialMaterialError::RecipientMismatch.to_string());
        }
        source
            .validate_access_target(access.target.as_ref(), &access.usage)
            .map_err(|_| CredentialMaterialError::BindingMismatch.to_string())?;
        if let Some(descriptor) = &source.descriptor {
            descriptor
                .validate_expiry(unix_time_ms())
                .map_err(|_| CredentialMaterialError::Expired.to_string())?;
        }
        let material = self
            .materializer
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
            .validate_for_holder(&request.selected_plaintext_holder, unix_time_ms())
            .map_err(|error| error.to_string())?;
        Ok(AttemptCredentialDelivery {
            candidate_fingerprint: request.candidate_fingerprint,
            access: access.with_envelope(envelope),
            claim_epoch: request.claim_epoch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding,
        CredentialMaterialSource, CredentialPurpose, CredentialRef, CredentialTarget,
        CredentialUsage, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialStatus, InMemorySecretStore,
    };
    use awaken_runtime_contract::CandidateFingerprint;
    use awaken_runtime_contract::credential_access_has_same_authority;
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    fn key_document(path: &std::path::Path, holder: &PlaintextHolder) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "ttl_seconds": 3600,
                "keys": [{
                    "id": "worker-v1",
                    "holder": holder,
                    "key_base64": STANDARD.encode([0x42; 32]),
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[tokio::test]
    async fn redelivery_revalidates_exact_revision_and_changes_only_the_envelope() {
        // Cause/effect decision table: C1 current source is Active and exact
        // revision/workspace/target; C2 published delivery is expired; C3
        // source becomes Disabled. R1 C1+C2 => fresh claim-epoch delivery whose
        // authority equals publication and whose plaintext resolves exactly.
        // R2 C3 => reject before issuance/material crosses the Vault boundary.
        // R3 the same still-current claim epoch asks again after a carrier
        // expires => issue another exact ephemeral delivery, not a new claim or
        // Credential revision. This proves issuer capability, not that Worker
        // runtime currently invokes it during a long Run.
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("deepseek".into()),
                env_key: None,
                secret: Some(RedactedString::new("claim-delivery-secret")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
        let target = CredentialTarget::new(CredentialPurpose::ProviderAdapter, "deepseek");
        let usage = CredentialUsage::ProviderAdapter;
        let published = CredentialAccess::new(
            CredentialRef {
                id: source.id.0.clone(),
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage.clone(),
            CredentialExecutionPolicy::exact(
                holder.clone(),
                awaken_credential_contract::ModelExposurePolicy::Forbidden,
            ),
        )
        .with_target(target)
        .with_envelope(
            awaken_credential_contract::CredentialEnvelope::SealedForWorker {
                envelope_ref: awaken_credential_contract::SealedCredentialEnvelopeRef {
                    id: "expired".into(),
                    payload_fingerprint: "expired".into(),
                },
                recipient: holder.trust_domain.clone(),
                expires_at_unix_ms: 1,
            },
        );
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &("deepseek@1", "endpoint"),
            &usage,
        );
        let root = tempfile::tempdir().unwrap();
        let keys = root.path().join("keys.json");
        key_document(&keys, &holder);
        let issuer =
            Arc::new(crate::recipient_envelope::RecipientKeyEnvelope::open(&keys).unwrap());
        let delivery_source = VaultAttemptCredentialDeliverySource::new(
            PinnedCredentialMaterializer::new(credentials.clone(), secrets.clone()),
            issuer.clone(),
        );
        let request = AttemptCredentialDeliveryRequest {
            candidate_fingerprint: CandidateFingerprint("candidate-a".into()),
            published_access: published.clone(),
            selected_plaintext_holder: holder.clone(),
            binding: binding.clone(),
            claim_epoch: 7,
        };
        let delivery = delivery_source.deliver(request.clone()).await.expect("R1");
        assert!(
            credential_access_has_same_authority(&delivery.access, &published),
            "R1"
        );
        assert_eq!(delivery.claim_epoch, 7, "R1");
        let material = awaken_credential_contract::CredentialMaterialResolver::resolve_exact(
            issuer.as_ref(),
            awaken_credential_contract::CredentialMaterialRequest {
                access: &delivery.access,
                selected_holder: &holder,
                binding: &binding,
            },
        )
        .await
        .expect("R1 exact ciphertext");
        assert_eq!(
            material.material.into_secret().unwrap().expose_secret(),
            "claim-delivery-secret"
        );
        let reissued = delivery_source.deliver(request).await.expect("R3");
        assert_eq!(reissued.claim_epoch, delivery.claim_epoch, "R3 same claim");
        assert_eq!(
            reissued.candidate_fingerprint, delivery.candidate_fingerprint,
            "R3 exact candidate"
        );
        assert!(
            credential_access_has_same_authority(&reissued.access, &published),
            "R3"
        );
        let reissued_material =
            awaken_credential_contract::CredentialMaterialResolver::resolve_exact(
                issuer.as_ref(),
                awaken_credential_contract::CredentialMaterialRequest {
                    access: &reissued.access,
                    selected_holder: &holder,
                    binding: &binding,
                },
            )
            .await
            .expect("R3 exact ciphertext");
        assert_eq!(
            reissued_material
                .material
                .into_secret()
                .unwrap()
                .expose_secret(),
            "claim-delivery-secret",
            "R3"
        );

        let mut disabled = source;
        disabled.status = CredentialStatus::Disabled;
        credentials.put(disabled).await.unwrap();
        assert!(
            VaultAttemptCredentialDeliverySource::new(
                PinnedCredentialMaterializer::new(credentials, secrets),
                issuer,
            )
            .deliver(AttemptCredentialDeliveryRequest {
                candidate_fingerprint: CandidateFingerprint("candidate-a".into()),
                published_access: published,
                selected_plaintext_holder: holder,
                binding,
                claim_epoch: 8,
            })
            .await
            .is_err(),
            "R2"
        );
    }
}
