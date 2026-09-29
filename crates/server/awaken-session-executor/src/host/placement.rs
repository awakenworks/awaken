// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
//! Canonical projection from immutable execution candidates to holder and
//! Worker-placement requirements.

use super::credential_capabilities::model_realization_capability;
use super::{HostError, RunActivation};
use awaken_dispatch::PlacementRequirements;
use awaken_runtime_contract::CredentialMaterialSource;
use std::collections::BTreeSet;

pub(super) fn worker_local_credentials(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_dispatch::WorkerCredentialRevision> {
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
) -> BTreeSet<awaken_dispatch::WorkerAcpCapabilityRequirement> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match candidate.provisioning() {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { acp, .. }
                if !acp.capability_fingerprint.trim().is_empty() =>
            {
                Some(awaken_dispatch::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding().identity().2.to_owned(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                acp: Some(acp),
                ..
            } if !acp.capability_fingerprint.trim().is_empty() => {
                Some(awaken_dispatch::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding().identity().2.to_owned(),
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
                    candidate.binding().identity().2
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
            candidate.binding().identity().2,
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
    environment: Option<&awaken_session_configuration_contract::EnvironmentSnapshot>,
    resources: Option<&awaken_session_resource_contract::SessionResourceManifest>,
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
    let resource_facts = resources
        .map(|manifest| manifest.resources.compatibility_facts())
        .unwrap_or_default();
    let has_repository = resource_facts.has_repository();
    let has_readonly_input = resource_facts.has_readonly_input();
    let has_credentialed_repository = resource_facts.has_credentialed_repository();
    let requires_local_environment = requires_local_environment(models);
    let requires_trusted_host = requires_local_environment
        && environment.is_some_and(|environment| {
            environment.sandbox.host_access
                == awaken_provisioning_contract::HostAccessPolicy::TrustedHost
        });
    let requires_opaque_process = crate::provisioning::model_candidates_require_opaque_process(
        std::iter::once(&models.model_binding).chain(models.model_candidates.iter()),
    );
    if requires_local_environment {
        placement.sandbox = if requires_trusted_host {
            // TrustedHost is an exact Worker backend, not a weaker isolation
            // floor. Its Workdir does not claim path/read-only/network/limit
            // guarantees; the backend id below prevents a stronger isolated
            // provider from being substituted and losing host identity.
            awaken_provisioning_contract::SandboxRequirements {
                process_tree_supervision: environment
                    .is_some_and(|environment| environment.sandbox.process_tree_supervision),
                ..Default::default()
            }
        } else {
            environment.map_or_else(
                || {
                    crate::provisioning::session_sandbox_projection(
                        &crate::provisioning::default_session_sandbox_spec("worker-placement"),
                        has_repository,
                        requires_opaque_process,
                    )
                    .1
                },
                |environment| {
                    crate::provisioning::sandbox_requirements(
                        environment,
                        has_repository,
                        requires_opaque_process,
                    )
                },
            )
        };
        if requires_trusted_host {
            placement.sandbox_backend =
                Some(awaken_dispatch::TRUSTED_HOST_SANDBOX_BACKEND.to_string());
        }
        if let Some(environment) = environment {
            placement.resources = crate::provisioning::sandbox_resource_requests(environment);
        }
    }
    for candidate in std::iter::once(&models.model_binding).chain(models.model_candidates.iter()) {
        placement.required_capabilities.insert(
            awaken_runtime_contract::execution::execution_capability(
                candidate.binding().identity().2,
            ),
        );
        if let Some(capability) = model_realization_capability(candidate) {
            placement
                .required_capabilities
                .insert(capability.to_string());
        }
    }
    if resources.is_some() {
        // Read-only remains an access intent inside TrustedHost, but cannot be
        // advertised as an enforced Worker guarantee.
        if !requires_trusted_host {
            placement.sandbox.enforced_readonly |= has_readonly_input;
        }
        placement
            .required_capabilities
            .insert(awaken_dispatch::SESSION_RESOURCES_CAPABILITY.to_string());
        if has_credentialed_repository {
            placement
                .required_capabilities
                .insert(awaken_dispatch::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
    placement
}

/// Bind the immutable inherited-input format before a root can author children.
/// Old unregistered executors reject its closed activation marker.
pub(super) fn bind_root_tool_configuration_format(
    activation: awaken_runtime_contract::RunActivation,
    is_session_dispatch: bool,
) -> awaken_runtime_contract::RunActivation {
    // A root that may author child work also needs the new execution
    // format. Registered capability admission alone cannot protect an old
    // unregistered local executor; its activation decoder must reject the
    // closed marker before it can create a child with weakened settings.
    if activation.inherited_toolsets().is_none()
        && (is_session_dispatch
            || !activation
                .snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty())
    {
        activation.with_inherited_toolsets(Some(
            awaken_runtime_contract::activation::InheritedToolsets::V1(Vec::new()),
        ))
    } else {
        activation
    }
}
