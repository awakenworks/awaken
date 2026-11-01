// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
//! Canonical projection from immutable execution candidates to holder and
//! Worker-placement requirements.

use crate::AdmissionError;
use crate::model_capability::model_realization_capability;
use awaken_agent_contract::credential::CredentialMaterialSource;
use awaken_runtime_contract::RunActivation;
use awaken_session_execution_contract::requires_local_environment;
use awaken_worker_contract::PlacementRequirements;
use std::collections::BTreeSet;

pub fn worker_local_credentials(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_worker_contract::WorkerCredentialRevision> {
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
) -> BTreeSet<awaken_worker_contract::WorkerAcpCapabilityRequirement> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match candidate.provisioning() {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { acp, .. }
                if !acp.capability_fingerprint.trim().is_empty() =>
            {
                Some(awaken_worker_contract::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding().identity().2.to_owned(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                acp: Some(acp),
                ..
            } if !acp.capability_fingerprint.trim().is_empty() => {
                Some(awaken_worker_contract::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding().identity().2.to_owned(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// Resolve the canonical cold-start inference holder from immutable candidate
/// backends. Embedded applications that author their own `RunDispatch` use this
/// same decision instead of duplicating the self-hosted boundary mapping.
pub enum InferencePlaintextHolderDecision {
    NotRequired,
    Exact(awaken_agent_contract::credential::PlaintextHolder),
    Boundary(awaken_agent_contract::credential::PlaintextBoundary),
}

pub fn inference_plaintext_holder_decision(
    activation: &RunActivation,
) -> Result<InferencePlaintextHolderDecision, AdmissionError> {
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
                awaken_agent_contract::credential::PlaintextBoundary::Workload
            }
            awaken_runtime_contract::resolved::Backend::Native
            | awaken_runtime_contract::resolved::Backend::Remote(_) => {
                awaken_agent_contract::credential::PlaintextBoundary::Worker
            }
            awaken_runtime_contract::resolved::Backend::Invalid(invalid) => {
                return Err(AdmissionError::rejected(format!(
                    "invalid backend_ref {}",
                    invalid.as_str()
                )));
            }
        };
        if boundary.is_some_and(|existing| existing != candidate_boundary) {
            return Err(AdmissionError::rejected(
                "one execution candidate set cannot require multiple credential plaintext holders",
            ));
        }
        boundary = Some(candidate_boundary);
    }
    if let Some(common_holders) = common_holders {
        if common_holders.is_empty() {
            return Err(AdmissionError::rejected(
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

pub fn self_hosted_holder_for_boundary(
    boundary: awaken_agent_contract::credential::PlaintextBoundary,
) -> awaken_agent_contract::credential::PlaintextHolder {
    match boundary {
        awaken_agent_contract::credential::PlaintextBoundary::Workload => {
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp()
                .inference_holder
        }
        awaken_agent_contract::credential::PlaintextBoundary::Worker
        | awaken_agent_contract::credential::PlaintextBoundary::Platform => {
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
                .inference_holder
        }
    }
}

pub fn self_hosted_inference_holder(
    activation: &RunActivation,
) -> Result<Option<awaken_agent_contract::credential::PlaintextHolder>, AdmissionError> {
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
    resources: Option<&awaken_session_configuration_contract::resource::SessionResourceManifest>,
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
        awaken_session_configuration_contract::hand_recovery_modes(
            models
                .tool_descriptors
                .iter()
                .map(|tool| (tool.id.as_str(), &tool.recovery_policy)),
        )
        .into_iter()
        .filter(|mode| *mode != awaken_tool_contract::tool::ToolRecoveryMode::NeverReplay)
        .collect();
    let requires_local_environment = requires_local_environment(models);
    let requires_opaque_process =
        awaken_session_execution_contract::model_candidates_require_opaque_process(
            std::iter::once(&models.model_binding).chain(models.model_candidates.iter()),
        );
    apply_session_physical_placement(
        &mut placement,
        environment,
        resources.map(|manifest| &manifest.resources),
        requires_local_environment,
        requires_opaque_process,
    );
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
    // This is a persisted codec demand, not a placement preference. The same
    // intrinsic compiler is consumed by Dispatch/Worker admission, which also
    // rejects an unmarked remote row before recovery reads or executor effects.
    if awaken_dispatch_contract::RunDispatch::requires_tool_result_journal(models) {
        placement
            .required_capabilities
            .insert(awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_string());
    }
    // Same persisted codec demand for the private Task input and attention.
    if awaken_dispatch_contract::RunDispatch::requires_background_task_control(models) {
        placement
            .required_capabilities
            .insert(awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY.to_string());
    }
    placement
}

/// The one physical Environment/Resource capability compiler shared by Run
/// placement and Run-free initial Session preparation. Model and ACP demands
/// are layered only by the Run caller above.
fn apply_session_physical_placement(
    placement: &mut PlacementRequirements,
    environment: Option<&awaken_session_configuration_contract::EnvironmentSnapshot>,
    resources: Option<&awaken_session_configuration_contract::resource::ResolvedSessionResources>,
    requires_local_environment: bool,
    requires_opaque_process: bool,
) {
    let resource_facts = resources
        .map(|value| value.compatibility_facts())
        .unwrap_or_default();
    let has_repository = resource_facts.has_repository();
    let requires_trusted_host = requires_local_environment
        && environment.is_some_and(|environment| {
            environment.sandbox.host_access
                == awaken_provisioning_contract::HostAccessPolicy::TrustedHost
        });
    if requires_local_environment {
        placement.sandbox = if requires_trusted_host {
            awaken_provisioning_contract::SandboxRequirements {
                process_tree_supervision: environment
                    .is_some_and(|environment| environment.sandbox.process_tree_supervision),
                ..Default::default()
            }
        } else {
            environment.map_or_else(
                || {
                    awaken_provisioning_contract::session_sandbox_projection(
                        &awaken_session_configuration_contract::environment_projection::default_session_sandbox_spec(
                            "worker-placement",
                        ),
                        has_repository,
                        requires_opaque_process,
                    )
                    .1
                },
                |environment| {
                    awaken_session_configuration_contract::environment_projection::sandbox_requirements(
                        environment,
                        has_repository,
                        requires_opaque_process,
                    )
                },
            )
        };
        if requires_trusted_host {
            placement.sandbox_backend =
                Some(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND.to_string());
        }
        if let Some(environment) = environment {
            placement.resources =
                awaken_session_configuration_contract::environment_projection::sandbox_resource_requests(environment);
        }
    }
    if resources.is_some() {
        // TrustedHost is an exact backend; its read-only intent is not an
        // enforceable isolation claim.
        if !requires_trusted_host {
            placement.sandbox.enforced_readonly |= resource_facts.has_readonly_input();
        }
        placement
            .required_capabilities
            .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
        if resource_facts.has_credentialed_repository() {
            placement
                .required_capabilities
                .insert(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
}

/// Run-free Session preparation uses the same physical capability projection,
/// without inheriting a model, inference credential, ACP, or Run-tool demand.
#[must_use]
pub fn session_preparation_placement(
    environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    resources: &awaken_session_configuration_contract::resource::ResolvedSessionResources,
    requires_opaque_process: bool,
) -> PlacementRequirements {
    let mut placement = PlacementRequirements::remote_required();
    apply_session_physical_placement(
        &mut placement,
        Some(environment),
        Some(resources),
        true,
        requires_opaque_process,
    );
    placement
}

/// Bind the immutable inherited-input format before a root can author children.
/// Old unregistered executors reject its closed activation marker.
pub fn bind_root_tool_configuration_format(
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
            awaken_agent_contract::toolset::InheritedToolsets::V1(Vec::new()),
        ))
    } else {
        activation
    }
}

/// Pure preparation-placement compiler over the original frozen Session law.
/// It holds no dispatch service, registry, provider or execution capability.
#[derive(Default)]
pub struct PreparationPlacement;

impl awaken_session_execution_contract::SessionPreparationPlacement for PreparationPlacement {
    fn requirements(
        &self,
        session: &awaken_session_contract::PersistedSession,
    ) -> Result<
        PlacementRequirements,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        session_preparation_requirements(session)
    }
}

/// Run-free preparation requirements from the existing frozen aggregate.
pub fn session_preparation_requirements(
    session: &awaken_session_contract::PersistedSession,
) -> Result<
    PlacementRequirements,
    awaken_session_execution_contract::SessionRealizationControlFailure,
> {
    let baseline = session
        .frozen_baseline()
        .ok_or(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)?;
    let requires_opaque_process = session
        .mcp
        .attachments
        .iter()
        .any(|attachment| attachment.target.sandbox_stdio_target().is_some());
    Ok(session_preparation_placement(
        &baseline.environment,
        session.resources.active_generation().1,
        requires_opaque_process,
    ))
}

#[cfg(test)]
mod preparation_tests {
    use super::*;

    fn model_set(
        primary: &str,
        candidates: &[&str],
    ) -> awaken_runtime_contract::resolved::ResolvedSpec {
        use awaken_runtime_contract::resolved::{
            CatalogFingerprint, ModelBinding, ResolvedModelCandidate,
        };
        awaken_runtime_contract::resolved::ResolvedSpec {
            model_binding: ResolvedModelCandidate::host(
                ModelBinding::parse("p", "m", primary).expect("valid primary fixture binding"),
            ),
            model_candidates: candidates
                .iter()
                .map(|backend| {
                    ResolvedModelCandidate::host(
                        ModelBinding::parse("p", "m", *backend)
                            .expect("valid fallback fixture binding"),
                    )
                })
                .collect(),
            catalog_fingerprint: CatalogFingerprint("journal-placement".into()),
            instructions: String::new(),
            max_steps: Some(1),
            delegation_limits: Default::default(),
            tool_descriptors: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: Default::default(),
            tool_presentation: Default::default(),
        }
    }

    #[test]
    fn raw_result_codec_demand_is_persisted_for_every_native_or_acp_candidate() {
        // Codec decision table: C1 primary is Native/ACP/A2A; C2 no fallback,
        // Native/ACP fallback, or only A2A fallback; C3 remote-preferred versus
        // remote-required. Native and ACP may recover ActiveToolBatch through
        // the Runtime-owned continuation, while A2A-only does not. Rules R1 any
        // Native/ACP candidate => the immutable placement carries the codec
        // token; R2 all A2A => no invented raw-journal demand. C3 changes only
        // the original execution location, never whether the token is carried.
        // The expected column is independent of the production predicate. The
        // Worker-entry/old-Coordinator omission table is owned by Dispatch's
        // same intrinsic compiler and capability-admission tests.
        for (primary, candidates, required) in [
            ("genai", Vec::new(), true),
            ("acp:claude", Vec::new(), true),
            ("a2a:https://primary.example", Vec::new(), false),
            ("a2a:https://primary.example", vec!["genai"], true),
            ("a2a:https://primary.example", vec!["acp:claude"], true),
            (
                "a2a:https://primary.example",
                vec!["a2a:https://fallback.example"],
                false,
            ),
            ("genai", vec!["a2a:https://fallback.example"], true),
            ("acp:claude", vec!["a2a:https://fallback.example"], true),
        ] {
            for remote_required in [false, true] {
                let models = model_set(primary, &candidates);
                let placement = remote_worker_placement(&models, None, None, remote_required);
                assert_eq!(
                    placement
                        .required_capabilities
                        .contains(awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY,),
                    required,
                    "primary={primary}, candidates={candidates:?}, remote={remote_required}",
                );
                assert_eq!(
                    placement.location,
                    if remote_required {
                        awaken_worker_contract::ExecutionLocation::RemoteRequired
                    } else {
                        awaken_worker_contract::ExecutionLocation::RemotePreferred
                    },
                    "codec demand does not force an unrelated local/remote policy",
                );
            }
        }
    }

    #[test]
    fn run_free_preparation_uses_the_same_physical_projection_as_run_placement() {
        // Causes: C1 isolated versus TrustedHost Environment, C2 opaque stdio
        // MCP need, C3 an exact (possibly empty) Resource projection. Effects:
        // E1 Sandbox requirements equal the canonical provisioning compiler;
        // E2 TrustedHost selects only its exact backend/supervision contract;
        // E3 no model/ACP/credential demand is introduced; E4 Session Resource
        // transport remains required. Rules P1=isolated+C2/C3=>E1+E3+E4,
        // P2=TrustedHost+C2/C3=>E2+E3+E4. The Run compiler above calls the
        // same `apply_session_physical_placement` function as this subject.
        let mut environment = awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "env".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "env-1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let resources =
            awaken_session_configuration_contract::resource::ResolvedSessionResources::default();
        for opaque in [false, true] {
            let required = session_preparation_placement(&environment, &resources, opaque);
            assert_eq!(
                required.sandbox,
                awaken_session_configuration_contract::environment_projection::sandbox_requirements(
                    &environment,
                    false,
                    opaque
                ),
                "P1/E1"
            );
            assert!(required.required_credentials.is_empty(), "P1/E3");
            assert!(required.required_acp_capabilities.is_empty(), "P1/E3");
            assert!(required.required_sandbox_tool_recovery.is_empty(), "P1/E3");
            assert!(
                required
                    .required_capabilities
                    .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY,),
                "P1/E4"
            );
        }
        environment.sandbox.host_access =
            awaken_provisioning_contract::HostAccessPolicy::TrustedHost;
        environment.sandbox.process_tree_supervision = true;
        let trusted = session_preparation_placement(&environment, &resources, true);
        assert_eq!(
            trusted.sandbox_backend.as_deref(),
            Some(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND),
            "P2/E2"
        );
        assert!(trusted.sandbox.process_tree_supervision, "P2/E2");
        assert!(trusted.required_credentials.is_empty(), "P2/E3");
    }
}
