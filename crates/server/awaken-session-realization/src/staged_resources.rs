// @formal surface obligation=session_resources.repository_rotation_requires_original_live_generation
// @formal surface obligation=session_resources.external_io_requires_durable_prepared_state
// @formal reason: Secret-free staged resource values (mounts, liveness checks, Repository activations) held by the installed Session.
//! The resources staged for one Session thread: neutral mount requirements,
//! prompt fragments, liveness checks and Repository activations. Provisioning
//! produces them and the installed Session holds them.

use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::ResourceOperationFence;

/// A thread's staged neutral mount requirements plus the prompt fragments derived
/// from the same effective Session inputs.
#[derive(Default, Clone)]
pub struct StagedResources {
    pub mounts: Vec<pc::MountRequirement>,
    /// Non-Resource request context retained for direct/profiled callers.
    /// Managed File/Memory/Repository context uses `resource_prompts` so every
    /// resource kind shares one typed renderer and one path-variable grammar.
    pub prompts: Vec<String>,
    pub resource_prompts: Vec<crate::managed_input_layout::ResourcePromptProjection>,
    /// Resource-domain liveness checks repeated at each Session operation. These
    /// carry only Workspace-owned resource identity and frozen, secret-free config
    /// facts; authorization was completed before staging. Repository effect edges
    /// reuse its authored remote/binding to revalidate the Session pin instead of
    /// accepting the pin's own source id as self-authorization.
    pub binding_checks: Vec<ResourceBindingCheck>,
    /// Mutable Repository inputs, realized after the environment is created (not a
    /// byte mount). The plan is secret-free; its transport credential is transient.
    pub repositories: Vec<RepositoryActivation>,
}

impl StagedResources {
    pub fn repository_at_path(
        &self,
        path: &str,
    ) -> Result<&RepositoryActivation, pc::SandboxError> {
        self.repositories
            .iter()
            .find(|repository| repository.plan.mount_path == path)
            .ok_or_else(|| {
                pc::SandboxError::invalid(format!(
                    "compiled Repository activation `{path}` is missing"
                ))
            })
    }
}

#[derive(Clone)]
pub enum ResourceBindingCheck {
    MemoryStore {
        memory_store_id: String,
        config_version: awaken_resource_contract::ConfigVersion,
    },
    Repository {
        repository_id: String,
        config_version: awaken_resource_contract::ConfigVersion,
        remote_url: String,
        credential_binding: Option<String>,
        authority: Option<Box<ResourceOperationFence>>,
    },
}

/// Runtime-only activation material for one already-resolved Repository config.
/// The plan is the neutral environment port and the credential pin is the exact,
/// secret-free Session decision. Plaintext is materialized only at a Git effect
/// edge, used for that one call, and never retained by this activation, the plan,
/// origin URL, Session manifest, or sandbox.
#[derive(Clone, PartialEq, Eq)]
pub struct RepositoryActivation {
    pub plan: pc::RepositoryRealizationPlan,
    pub credential_pin: Option<session_resource::ResolvedRepositoryCredential>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EnvironmentResourceReconciliation {
    #[default]
    None,
    Fresh,
    Adopted,
}

/// One compiled projection from the frozen Session manifest. Standard mounts
/// and optional automatic-memory candidates travel together so installation
/// cannot publish one generation with bindings from another.
pub struct CompiledEffectiveInputs {
    pub staged: crate::staged_resources::StagedResources,
    pub memory_bindings:
        std::collections::HashMap<String, std::sync::Arc<crate::memory_binding::MemoryBinding>>,
}

#[derive(Debug)]
pub enum RepositoryPublicationActivationError {
    Rejected(awaken_provisioning_contract::RepositoryPublicationRejection),
    Failed(crate::realization_error::RealizationError),
}
