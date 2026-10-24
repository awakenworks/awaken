// Cross-crate Vault envelope issuance tests over the real recipient-key issuer.
#[cfg(test)]
mod tests {
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::credential::CandidateFingerprint;
    use awaken_credential_contract::credential_access_has_same_authority;
    use awaken_credential_contract::{
        AttemptCredentialDeliveryRequest, AttemptCredentialDeliverySource,
    };
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding,
        CredentialMaterialSource, CredentialPurpose, CredentialRef, CredentialTarget,
        CredentialUsage, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialStatus, InMemorySecretStore,
        VaultAttemptCredentialDeliverySource, VaultLocalMaterialAuthority,
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::sync::Arc;

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
        // source becomes Disabled; C4 provider coordinate is absent or foreign.
        // R1 C1+C2 => fresh claim-epoch delivery whose
        // authority equals publication and whose plaintext resolves exactly.
        // R2 C3 => reject before issuance/material crosses the Vault boundary.
        // R3 the same still-current claim epoch asks again after a carrier
        // expires => issue another exact ephemeral delivery, not a new claim or
        // Credential revision. R4 C4 => reject before material read/issuance.
        // This proves issuer capability, not that Worker
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
            awaken_time::system_clock().as_ref(),
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
        let issuer = Arc::new(
            awaken_credential_envelope::RecipientKeyEnvelope::open(
                &keys,
                awaken_time::system_clock(),
            )
            .unwrap(),
        );
        let delivery_source = VaultAttemptCredentialDeliverySource::new(
            Arc::new(VaultLocalMaterialAuthority::new(
                credentials.clone(),
                secrets.clone(),
                awaken_time::system_clock(),
            )),
            issuer.clone(),
        );
        let request = AttemptCredentialDeliveryRequest {
            candidate_fingerprint: CandidateFingerprint("candidate-a".into()),
            published_access: published.clone(),
            selected_plaintext_holder: holder.clone(),
            binding: binding.clone(),
            expected_provider: Some(
                awaken_credential_contract::CredentialProviderExpectationOwned {
                    provider_id: "deepseek".into(),
                    protocol_endpoint_id: None,
                },
            ),
            claim_epoch: 7,
        };
        let mut missing_coordinate = request.clone();
        missing_coordinate.expected_provider = None;
        assert!(
            delivery_source.deliver(missing_coordinate).await.is_err(),
            "R4 missing"
        );
        let mut foreign_coordinate = request.clone();
        foreign_coordinate
            .expected_provider
            .as_mut()
            .unwrap()
            .provider_id = "other".into();
        assert!(
            delivery_source.deliver(foreign_coordinate).await.is_err(),
            "R4 foreign"
        );
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
                expected_provider: None,
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
                    expected_provider: None,
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
                Arc::new(VaultLocalMaterialAuthority::new(
                    credentials,
                    secrets,
                    awaken_time::system_clock()
                )),
                issuer,
            )
            .deliver(AttemptCredentialDeliveryRequest {
                candidate_fingerprint: CandidateFingerprint("candidate-a".into()),
                published_access: published,
                selected_plaintext_holder: holder,
                binding,
                expected_provider: Some(
                    awaken_credential_contract::CredentialProviderExpectationOwned {
                        provider_id: "deepseek".into(),
                        protocol_endpoint_id: None,
                    }
                ),
                claim_epoch: 8,
            })
            .await
            .is_err(),
            "R2"
        );
    }
}
