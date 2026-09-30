use super::*;

#[test]
fn native_manifest_admission_uses_installed_materializer_evidence() {
    // Precondition: the access has a valid exact Provider target and revision.
    // Cause/effect graph: local stores OR an external resolver determine
    // source support; only the resolver supplies envelope support. The
    // Native consumer fixes Worker/self-hosted + WorkerProviderAdapter.
    // R1 neither source installed -> reject all requests; R2 local only ->
    // unsealed Control reference; R3 external only -> this fixture's two
    // sources and envelopes; R4 both -> union of those installed sources.
    // Cross each rule with source, envelope, holder and mechanism. Admission
    // after the real manifest codec must reject every unsupported tuple;
    // an allowed policy cannot manufacture a different adapter capability.
    // This covers evidence projection, not concrete file resolver fidelity.
    use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
    let worker = selected_holder();
    let resolver = Arc::new(ExactExternalResolver {
        expected_binding: CredentialMaterialBinding::for_target(
            "workspace-a",
            &"provider-a",
            &CredentialUsage::ProviderAdapter,
        ),
        expected_payload_fingerprint: "payload-a",
        returned_holder: worker.clone(),
    });
    for local in [false, true] {
        for external in [false, true] {
            let mut pinned = if local {
                PinnedCredentialMaterializer::new(
                    Arc::new(InMemoryCredentialRepo::new()),
                    Arc::new(InMemorySecretStore::new()),
                    awaken_lease_contract::system_clock(),
                )
            } else {
                PinnedCredentialMaterializer::external_only(
                    resolver.clone(),
                    awaken_lease_contract::system_clock(),
                )
            };
            pinned.external_material_resolver =
                external.then(|| resolver.clone() as Arc<dyn CredentialMaterialResolver>);
            let inference = CredentialInferenceMaterializer::from_pinned(pinned);
            let advertised = inference.credential_realization_capabilities();
            let wire = advertised
                .manifest_capability()
                .unwrap()
                .into_iter()
                .collect();
            let decoded = CredentialRealizationCapabilities::from_manifest_capabilities(&wire)
                .expect("canonical Worker capability codec");
            for source in [
                CredentialMaterialSource::ControlPlaneReference,
                CredentialMaterialSource::WorkerReference,
            ] {
                for envelope in [false, true] {
                    for holder in [
                        worker.clone(),
                        PlaintextHolder::new(PlaintextBoundary::Worker, "another-worker-domain"),
                        PlaintextHolder::new(
                            PlaintextBoundary::Workload,
                            awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                        ),
                    ] {
                        for kind in [
                            CredentialRealizationKind::WorkerProviderAdapter,
                            CredentialRealizationKind::WorkerRelay,
                            CredentialRealizationKind::ProcessSecretEnvironment,
                        ] {
                            let mut access =
                                CredentialAccess::new(
                                    CredentialRef {
                                        id: "credential-a".into(),
                                        revision: 1,
                                    },
                                    source,
                                    CredentialUsage::ProviderAdapter,
                                    CredentialExecutionPolicy::exact(
                                        holder.clone(),
                                        ModelExposurePolicy::Forbidden,
                                    ),
                                )
                                .with_target(awaken_credential_contract::CredentialTarget::new(
                                    awaken_credential_contract::CredentialPurpose::ProviderAdapter,
                                    "provider-a",
                                ));
                            if envelope {
                                access =
                                    access.with_envelope(CredentialEnvelope::SealedForWorker {
                                        envelope_ref: SealedCredentialEnvelopeRef {
                                            id: "envelope-a".into(),
                                            payload_fingerprint: "payload-a".into(),
                                        },
                                        recipient: worker.trust_domain.clone(),
                                        expires_at_unix_ms: 200,
                                    });
                            }
                            let supported = (external
                                || (local
                                    && source == CredentialMaterialSource::ControlPlaneReference
                                    && !envelope))
                                && holder == worker
                                && kind == CredentialRealizationKind::WorkerProviderAdapter;
                            assert_eq!(
                                access.admit(&holder, kind, &decoded, 100).is_ok(),
                                supported,
                                "local={local} external={external} source={source:?} envelope={envelope} holder={holder:?} kind={kind:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
