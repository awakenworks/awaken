// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Immutable credential capability selection from the admitted model candidate.
//! Immutable candidate credential capability requirement.
use awaken_agent_contract::credential::CredentialMaterialSource;
use awaken_runtime_contract::resolved::{ModelProvisioning, ProviderAccessKind};
use awaken_worker_contract::{
    BROKERED_MODEL_GRANT_CAPABILITY, HOST_EXECUTOR_CAPABILITY,
    PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
};
/// Select the exact Worker capability required to realize one immutable model
/// candidate. Credential custody owns this mapping; dispatch placement only
/// aggregates the selected capabilities across candidates.
///
/// Every candidate demands the capability of the one path that realizes its
/// access, so a brokered Provider candidate is placed exactly like a directly
/// configured one: only on a Worker that advertises that path.
///
/// | Rule | provisioning | access | credential source | capability |
/// |---|---|---|---|---|
/// | M1 | HostExecutor | - | - | `host-executor/v1` |
/// | M2 | BackendOwned | - | Worker-local | `worker-local-credentials/v1` |
/// | M3 | Provider/Remote | Direct | Worker reference | `worker-local-credentials/v1` |
/// | M4 | Provider | Direct | Vault / none | `credential-source/v1` |
/// | M5 | Remote | - | Vault | `credential-source/v1` |
/// | M6 | Provider | Brokered | none | `inference.brokered_grant` |
/// | M7 | Remote | - | none | no capability |
pub fn model_realization_capability(
    candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
) -> Option<&'static str> {
    match candidate.provisioning() {
        ModelProvisioning::HostExecutor => Some(HOST_EXECUTOR_CAPABILITY),
        ModelProvisioning::BackendOwned { .. } => Some(WORKER_LOCAL_CREDENTIALS_CAPABILITY),
        ModelProvisioning::Provider {
            access_kind: ProviderAccessKind::Brokered,
            ..
        } => Some(BROKERED_MODEL_GRANT_CAPABILITY),
        ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        }
        | ModelProvisioning::Remote {
            credential: Some(credential),
            ..
        } if credential.material_source == CredentialMaterialSource::WorkerReference => {
            Some(WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        }
        ModelProvisioning::Provider { .. }
        | ModelProvisioning::Remote {
            credential: Some(_),
            ..
        } => Some(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
        ModelProvisioning::Remote {
            credential: None, ..
        } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::credential::{
        CredentialAccess, CredentialExecutionPolicy, CredentialRef, CredentialUsage,
        ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_runtime_contract::InferenceEndpoint;
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};

    fn endpoint() -> InferenceEndpoint {
        InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://provider.example.invalid".into(),
            upstream_model: "chat".into(),
            processing_placement: None,
        }
    }

    fn binding() -> ModelBinding {
        ModelBinding::parse("vendor", "chat", "native").expect("valid fixture binding")
    }

    fn credential(source: CredentialMaterialSource) -> CredentialAccess {
        CredentialAccess::new(
            CredentialRef {
                id: "provider-key".into(),
                revision: 1,
            },
            source,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::exact(
                PlaintextHolder {
                    boundary: PlaintextBoundary::Worker,
                    trust_domain: awaken_agent_contract::credential::TrustDomainRef(
                        "workers".into(),
                    ),
                },
                ModelExposurePolicy::Forbidden,
            ),
        )
    }

    fn direct(credential: Option<CredentialAccess>) -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_provider(
            binding(),
            "vendor@1",
            "vendor-chat@1",
            "workspace",
            credential,
            endpoint(),
        )
        .expect("coherent direct candidate")
    }

    fn brokered() -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_brokered_provider(
            binding(),
            "vendor@1",
            "vendor-chat@1",
            "workspace",
            endpoint(),
        )
        .expect("coherent brokered candidate")
    }

    #[test]
    fn each_candidate_demands_the_capability_of_its_own_realization_path() {
        // Causes: provisioning kind, Provider access kind, credential source.
        // Effect: the one Worker capability placement and claim admission
        // require. The expected column is written independently of the match.
        for (rule, candidate, expected) in [
            (
                "M1",
                ResolvedModelCandidate::host(binding()),
                Some(HOST_EXECUTOR_CAPABILITY),
            ),
            (
                "M3",
                direct(Some(credential(CredentialMaterialSource::WorkerReference))),
                Some(WORKER_LOCAL_CREDENTIALS_CAPABILITY),
            ),
            (
                "M4 vault",
                direct(Some(credential(
                    CredentialMaterialSource::ControlPlaneReference,
                ))),
                Some(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
            ),
            (
                "M4 keyless",
                direct(None),
                Some(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
            ),
            ("M6", brokered(), Some(BROKERED_MODEL_GRANT_CAPABILITY)),
        ] {
            assert_eq!(model_realization_capability(&candidate), expected, "{rule}");
        }
    }

    #[test]
    fn brokered_and_direct_candidates_are_placed_by_the_same_capability_law() {
        // The lossy mapping placed a brokered candidate on any Worker that
        // could open Coordinator-delivered Provider credentials, although only
        // a Worker that obtains brokered grants can realize it. Rules:
        // P1 direct + Worker offering the credential source => admitted;
        // P2 brokered + Worker offering only the credential source => refused
        //    as missing `inference.brokered_grant`;
        // P3 brokered + Worker offering brokered grants => admitted;
        // P4 direct + Worker offering only brokered grants => refused.
        let spec =
            |candidate: ResolvedModelCandidate| awaken_runtime_contract::resolved::ResolvedSpec {
                model_binding: candidate,
                model_candidates: Vec::new(),
                catalog_fingerprint: awaken_runtime_contract::resolved::CatalogFingerprint(
                    "capability-law".into(),
                ),
                instructions: String::new(),
                max_steps: Some(1),
                delegation_limits: Default::default(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            };
        let offers = |path: &str| {
            [
                awaken_runtime_contract::execution::execution_capability("native"),
                awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_string(),
                path.to_string(),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
        };
        for (rule, candidate, offered, admitted) in [
            (
                "P1",
                direct(Some(credential(
                    CredentialMaterialSource::ControlPlaneReference,
                ))),
                PROVIDER_CREDENTIAL_SOURCE_CAPABILITY,
                true,
            ),
            (
                "P2",
                brokered(),
                PROVIDER_CREDENTIAL_SOURCE_CAPABILITY,
                false,
            ),
            ("P3", brokered(), BROKERED_MODEL_GRANT_CAPABILITY, true),
            (
                "P4",
                direct(Some(credential(
                    CredentialMaterialSource::ControlPlaneReference,
                ))),
                BROKERED_MODEL_GRANT_CAPABILITY,
                false,
            ),
        ] {
            let placement =
                crate::placement::remote_worker_placement(&spec(candidate), None, None, true);
            let result = awaken_worker_contract::admit_required_capabilities(
                placement.required_capabilities.iter().map(String::as_str),
                &offers(offered),
            );
            assert_eq!(result.is_ok(), admitted, "{rule}: {result:?}");
        }
    }
}
