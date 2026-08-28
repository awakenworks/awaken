//! Canonical projection from immutable execution candidates to holder and
//! Worker-placement requirements.

use super::credential_capabilities::model_realization_capability;
use super::{HostError, RunActivation};
use awaken_run_ingress::PlacementRequirements;
use awaken_runtime_contract::CredentialMaterialSource;
use std::collections::BTreeSet;

pub(super) fn worker_local_credentials(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_run_ingress::WorkerCredentialRevision> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match candidate.provisioning() {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
                credential,
                ..
            } => Some(credential.clone()),
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                credential: Some(credential),
                ..
            }
            | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
                credential: Some(credential),
                ..
            } if credential.material_source == CredentialMaterialSource::WorkerReference => {
                Some(credential.credential.clone())
            }
            _ => None,
        })
        .collect()
}

fn worker_acp_capabilities(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_run_ingress::WorkerAcpCapabilityRequirement> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match candidate.provisioning() {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { acp, .. }
                if !acp.capability_fingerprint.trim().is_empty() =>
            {
                Some(awaken_run_ingress::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding().backend_ref.clone(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                acp: Some(acp),
                ..
            } if !acp.capability_fingerprint.trim().is_empty() => {
                Some(awaken_run_ingress::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding().backend_ref.clone(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// Whether any publication-pinned execution candidate needs the Session's local
/// Environment. This is deliberately a set-wide decision: an A2A primary with a
/// Native/ACP fallback is not A2A-only and must retain one realizable Environment.
pub(crate) fn requires_local_environment(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> bool {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .any(|candidate| {
            !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &candidate.binding().backend_ref
                ),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            )
        })
}

/// Resolve the canonical cold-start inference holder from immutable candidate
/// backends. Embedded applications that author their own `RunDispatch` use this
/// same decision instead of duplicating the self-hosted boundary mapping.
pub(super) enum InferencePlaintextHolderDecision {
    NotRequired,
    Exact(awaken_runtime_contract::PlaintextHolder),
    Boundary(awaken_runtime_contract::PlaintextBoundary),
}

pub(super) fn inference_plaintext_holder_decision(
    activation: &RunActivation,
) -> Result<InferencePlaintextHolderDecision, HostError> {
    let mut boundary = None;
    let mut common_holders = None;
    for candidate in activation
        .snapshot
        .resolved_spec
        .attempt_candidates(activation.model_ref_override.as_deref())
    {
        let (awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        }
        | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: Some(credential),
            ..
        }) = candidate.provisioning()
        else {
            continue;
        };
        match &mut common_holders {
            None => common_holders = Some(credential.policy.allowed_plaintext_holders.clone()),
            Some(common) => {
                common.retain(|holder| credential.policy.allowed_plaintext_holders.contains(holder))
            }
        }
        let candidate_boundary = match awaken_runtime_contract::resolved::Backend::from_ref(
            &candidate.binding().backend_ref,
        ) {
            awaken_runtime_contract::resolved::Backend::Acp(_) => {
                awaken_runtime_contract::PlaintextBoundary::Workload
            }
            awaken_runtime_contract::resolved::Backend::Native
            | awaken_runtime_contract::resolved::Backend::Remote(_) => {
                awaken_runtime_contract::PlaintextBoundary::Worker
            }
            awaken_runtime_contract::resolved::Backend::Invalid(invalid) => {
                return Err(HostError::bad_request(format!(
                    "invalid backend_ref {}",
                    invalid.as_str()
                )));
            }
        };
        if boundary.is_some_and(|existing| existing != candidate_boundary) {
            return Err(HostError::bad_request(
                "one execution candidate set cannot require multiple credential plaintext holders",
            ));
        }
        boundary = Some(candidate_boundary);
    }
    if let Some(common_holders) = common_holders {
        if common_holders.is_empty() {
            return Err(HostError::bad_request(
                "one execution candidate set has no common credential plaintext holder",
            ));
        }
        if common_holders.len() == 1
            && let Some(holder) = common_holders.into_iter().next()
        {
            return Ok(InferencePlaintextHolderDecision::Exact(holder));
        }
    }
    Ok(boundary.map_or(
        InferencePlaintextHolderDecision::NotRequired,
        InferencePlaintextHolderDecision::Boundary,
    ))
}

pub(super) fn self_hosted_holder_for_boundary(
    boundary: awaken_runtime_contract::PlaintextBoundary,
) -> awaken_runtime_contract::PlaintextHolder {
    match boundary {
        awaken_runtime_contract::PlaintextBoundary::Workload => {
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp()
                .inference_holder
        }
        awaken_runtime_contract::PlaintextBoundary::Worker
        | awaken_runtime_contract::PlaintextBoundary::Platform => {
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
                .inference_holder
        }
    }
}

pub fn self_hosted_inference_holder(
    activation: &RunActivation,
) -> Result<Option<awaken_runtime_contract::PlaintextHolder>, HostError> {
    Ok(match inference_plaintext_holder_decision(activation)? {
        InferencePlaintextHolderDecision::NotRequired => None,
        InferencePlaintextHolderDecision::Exact(holder) => Some(holder),
        InferencePlaintextHolderDecision::Boundary(boundary) => {
            Some(self_hosted_holder_for_boundary(boundary))
        }
    })
}

/// Compile the complete immutable Worker claim requirements for one resolved
/// model set. Application adapters that author their own [`RunDispatch`] must
/// use this function instead of projecting backend or credential capabilities
/// independently.
#[must_use]
pub fn remote_worker_placement(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
    environment: Option<&awaken_session_contract::EnvironmentSnapshot>,
    resources: Option<&awaken_session_contract::SessionResourceManifest>,
    remote_required: bool,
) -> PlacementRequirements {
    let required_credentials = worker_local_credentials(models);
    let mut placement = if remote_required || !required_credentials.is_empty() {
        PlacementRequirements::remote_required()
    } else {
        PlacementRequirements::remote_preferred()
    };
    placement.required_credentials = required_credentials;
    placement.required_acp_capabilities = worker_acp_capabilities(models);
    placement.required_sandbox_tool_recovery =
        awaken_ext_builtin_tools::selected_hand_recovery_modes(&models.tool_descriptors)
            .into_iter()
            .filter(|mode| *mode != awaken_runtime_contract::tool::ToolRecoveryMode::NeverReplay)
            .collect();
    let requires_local_environment = requires_local_environment(models);
    let requires_opaque_process = std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .any(|candidate| {
            matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &candidate.binding().backend_ref
                ),
                awaken_runtime_contract::resolved::Backend::Acp(_)
            ) && !matches!(
                candidate.provisioning(),
                awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. }
            )
        });
    if requires_local_environment {
        placement.sandbox = environment.map_or_else(
            || awaken_provisioning_contract::SandboxRequirements {
                ..Default::default()
            },
            |environment| {
                crate::provisioning::sandbox_requirements(environment, requires_opaque_process)
            },
        );
        if let Some(environment) = environment {
            placement.resources = crate::provisioning::sandbox_resource_requests(environment);
        }
    }
    for candidate in std::iter::once(&models.model_binding).chain(models.model_candidates.iter()) {
        placement.required_capabilities.insert(
            awaken_runtime_contract::execution::execution_capability(
                &candidate.binding().backend_ref,
            ),
        );
        if let Some(capability) = model_realization_capability(candidate) {
            placement
                .required_capabilities
                .insert(capability.to_string());
        }
    }
    if let Some(resources) = resources {
        placement.sandbox.enforced_readonly |= resources
            .resources
            .inputs()
            .iter()
            .any(|input| input.access == awaken_resource_contract::ResourceAccess::ReadOnly);
        placement
            .required_capabilities
            .insert(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY.to_string());
        let credentialed_repository = resources.resources.inputs().iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::Repository { config, .. }
                    if config.credential_binding.is_some()
            )
        });
        if credentialed_repository {
            placement
                .required_capabilities
                .insert(awaken_run_ingress::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
    placement
}
