// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Read-only composition delegates the existing physical observation and
// @formal reason: specification projections; every physical effect refuses before backend access.
//! Container metadata and original-source observation without execution roles.

use awaken_provisioning_contract as pc;
use awaken_session_environment_contract::{
    EnvironmentAdoption, EnvironmentAdoptionLayout, EnvironmentRealizationKind,
    EnvironmentRealizer, SessionEnvironment,
};
use std::sync::Arc;

/// Read-only view over the canonical container backend. No Hand, capacity,
/// terminal, installation or execution authority is constructed by this view.
pub struct ContainerEnvironmentObservation {
    provider: Arc<dyn pc::SandboxProvider<Environment = dyn pc::SessionSandbox>>,
    extra_mounts: Vec<pc::MountRequirement>,
}

impl ContainerEnvironmentObservation {
    /// Carry the canonical backend and its creation-layout metadata as read
    /// roles, without constructing any Hand or physical-effect capability.
    #[must_use]
    pub fn new(
        provider: Arc<dyn pc::SandboxProvider<Environment = dyn pc::SessionSandbox>>,
        extra_mounts: Vec<pc::MountRequirement>,
    ) -> Self {
        Self {
            provider,
            extra_mounts,
        }
    }
}

fn read_only() -> pc::SandboxError {
    pc::SandboxError::unsupported("this process only observes retained Session sources")
}

#[async_trait::async_trait]
impl EnvironmentRealizer for ContainerEnvironmentObservation {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        self.provider.capabilities()
    }

    fn supports_host_identity(&self) -> bool {
        false
    }

    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
        super::provider::container_spec(spec, &self.extra_mounts)
    }

    async fn create_effective_for_effect(
        &self,
        _: &pc::SandboxSpec,
        _: pc::SandboxEffect<'_>,
        _: Option<&pc::SandboxHandle>,
        _: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        Err(read_only())
    }
}

#[async_trait::async_trait]
impl EnvironmentAdoption for ContainerEnvironmentObservation {
    fn effective_adoption_layout(
        &self,
        _: &pc::SandboxSpec,
        _: &pc::SandboxHandle,
    ) -> Result<EnvironmentAdoptionLayout, pc::SandboxError> {
        Err(read_only())
    }

    async fn observe_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: Option<&SessionEnvironment>,
        basis: pc::SandboxObservationBasis<'_>,
        purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        if !matches!(
            purpose,
            pc::SandboxObservationPurpose::WorkerRetirement { .. }
        ) {
            return Err(read_only());
        }
        super::provider::observe_container(
            self.provider.as_ref(),
            spec,
            handle,
            retained,
            basis,
            purpose,
        )
        .await
    }

    fn validate_closed_observation(
        &self,
        _: &pc::SandboxHandle,
        _: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        Err(read_only())
    }

    async fn adopt_effective_for_effect(
        &self,
        _: &pc::SandboxSpec,
        _: &pc::SandboxHandle,
        _: pc::SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        Err(read_only())
    }

    async fn rebind_retained(
        &self,
        _: &pc::SandboxSpec,
        _: &pc::SandboxHandle,
        _: &SessionEnvironment,
        _: pc::SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        Err(read_only())
    }
}
