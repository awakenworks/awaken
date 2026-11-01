// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Select the binding sink at construction and execute through the existing installed Sessions and provider authority.
//! The realization executor (ADR-0089): executes the Environment realization
//! effects the Session aggregate projects, through ports chosen once when the
//! process is composed. It keeps no durable state.
//!
//! The executor owns four peer components, none of which holds another:
//! [`EnvironmentHolding`] (the Environment each installed Session holds),
//! [`ResourceStaging`] (the Session's Resources as the Environment receives
//! them), [`TerminalEffects`] (terminal cleanup effects) and
//! [`ExecutionAuthority`] (the authority provider side effects run under). An
//! operation that needs more than one of them is a named method of the
//! executor, which lends the component it drives shared references to the
//! peers it reads.

pub mod continuation;
mod effects_role;

use crate::environment_holding::EnvironmentHolding;
use crate::execution_authority::ExecutionAuthority;
#[cfg(any(test, feature = "test-support"))]
use crate::peers::EnvironmentPeer as _;
use crate::resource_staging::ResourceStaging;
use crate::session_realizations::SessionRealizations;
use crate::terminal_effects::TerminalEffects;

/// Executes Session Environment realization through its four peer components.
pub struct RealizationExecutor {
    /// The same process clock that authorizes installed realization leases.
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
    /// The installed Sessions, as far as realization may reach them.
    sessions: SessionRealizations,
    environments: EnvironmentHolding,
    staging: ResourceStaging,
    effects: TerminalEffects,
    authority: ExecutionAuthority,
}

impl RealizationExecutor {
    /// Compose the executor once from components built over the same installed
    /// Sessions.
    pub fn new(
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
        sessions: SessionRealizations,
        environments: EnvironmentHolding,
        staging: ResourceStaging,
        effects: TerminalEffects,
        authority: ExecutionAuthority,
    ) -> Self {
        Self {
            clock,
            sessions,
            environments,
            staging,
            effects,
            authority,
        }
    }

    pub fn clock(&self) -> &std::sync::Arc<dyn awaken_time::WallClock> {
        &self.clock
    }

    pub fn sessions(&self) -> &SessionRealizations {
        &self.sessions
    }

    pub fn environments(&self) -> &EnvironmentHolding {
        &self.environments
    }

    /// The Environment holding of a test fixture, for replacing its ports.
    #[cfg(any(test, feature = "test-support"))]
    pub fn environments_mut(&mut self) -> &mut EnvironmentHolding {
        &mut self.environments
    }

    pub fn staging(&self) -> &ResourceStaging {
        &self.staging
    }

    pub fn effects(&self) -> &TerminalEffects {
        &self.effects
    }

    pub fn authority(&self) -> &ExecutionAuthority {
        &self.authority
    }

    /// The views lent to the component an operation drives.
    pub fn peers(&self) -> crate::peers::Peers<'_> {
        crate::peers::Peers {
            environments: &self.environments,
            staging: &self.staging,
            effects: &self.effects,
            authority: &self.authority,
        }
    }
}

/// The cross-component operations: each drives one component and lends it its
/// peers.
impl RealizationExecutor {
    /// Adopt the Session's durable Environment binding (see
    /// [`EnvironmentHolding::adopt_bound_session_environment`]).
    pub async fn adopt_bound_session_environment(
        &self,
        thread: &str,
        encoded: Option<&str>,
        provider: &crate::session_environment::SandboxProviderRoles,
        source_generation_id: Option<&str>,
        rebuild_unavailable: bool,
    ) -> Result<
        crate::session_environment::SessionEnvironmentAdoptionDisposition,
        crate::realization_error::RealizationError,
    > {
        self.environments
            .adopt_bound_session_environment(
                self.peers(),
                thread,
                encoded,
                provider,
                source_generation_id,
                rebuild_unavailable,
            )
            .await
    }

    /// Replace a revoked legacy Environment of a claimed Session.
    pub async fn rebuild_claimed_legacy_environment_after_revocation(
        &self,
        thread: &str,
        provider: &crate::session_environment::SandboxProviderRoles,
    ) -> Result<(), crate::realization_error::RealizationError> {
        self.environments
            .rebuild_claimed_legacy_environment_after_revocation(self.peers(), thread, provider)
            .await
    }

    /// The Session's Environment for a tool, created on first use.
    pub async fn ensure_session_environment_for_tool(
        &self,
        thread: &str,
    ) -> Result<
        std::sync::Arc<crate::session_environment::SessionEnvironment>,
        crate::realization_error::RealizationError,
    > {
        self.environments
            .ensure_session_environment_for_tool(self.peers(), thread)
            .await
    }

    /// Apply one Resource transition to the Session's Environment.
    pub async fn apply_session_inputs_with_context(
        &self,
        thread: &str,
        transition: &awaken_session_configuration_contract::resource::SessionResourceTransition,
        claim: Option<&awaken_worker_contract::RunClaim>,
        realization_lease: Option<
            &awaken_session_configuration_contract::resource::SessionRealizationLease,
        >,
    ) -> Result<(), awaken_session_types::RunError> {
        self.staging
            .apply_session_inputs_with_context(
                self.peers(),
                thread,
                transition,
                claim,
                realization_lease,
            )
            .await
    }

    /// Revalidate the Session's live Resource bindings before a Run.
    pub async fn validate_thread_resource_bindings(
        &self,
        thread: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        self.staging
            .validate_thread_resource_bindings(self.peers(), thread)
            .await
    }

    /// Validate a prospective Sandbox layout against the Session's provider.
    pub fn validate_prospective_session_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_types::RunError> {
        self.staging
            .validate_prospective_session_layout(self.peers(), thread, layout)
    }

    /// Install the Resources a claimed dispatch carries.
    pub async fn install_dispatched_resources(
        &self,
        thread: &str,
        manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
        claim: Option<&awaken_worker_contract::RunClaim>,
    ) -> Result<(), awaken_session_types::RunError> {
        self.staging
            .install_dispatched_resources(self.peers(), thread, manifest, claim)
            .await
    }
}

/// Test-only spec helpers over the default Session provider. A fixture with a
/// poisoned registry is a broken test, so these helpers panic instead of
/// reporting the realization denial production paths return.
#[cfg(any(test, feature = "test-support"))]
impl RealizationExecutor {
    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sandbox_spec(&self, thread: &str) -> awaken_provisioning_contract::SandboxSpec {
        let resources = self
            .staging
            .thread_resources_snapshot(thread)
            .expect("test fixture registry is healthy");
        self.sandbox_spec_for_resources(thread, &resources)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn sandbox_spec_for_resources(
        &self,
        thread: &str,
        resources: &crate::staged_resources::StagedResources,
    ) -> awaken_provisioning_contract::SandboxSpec {
        self.sandbox_spec_for_mount_layout(
            thread,
            resources.mounts.clone(),
            !resources.repositories.is_empty(),
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn sandbox_spec_for_mount_layout(
        &self,
        thread: &str,
        resource_mounts: Vec<awaken_provisioning_contract::MountRequirement>,
        has_repositories: bool,
    ) -> awaken_provisioning_contract::SandboxSpec {
        self.staging
            .sandbox_spec_for_mount_layout_with_network(
                thread,
                resource_mounts,
                has_repositories,
                self.environments
                    .providers()
                    .session_provider
                    .readiness()
                    .capabilities()
                    .network_isolation,
            )
            .expect("test fixture registry is healthy")
    }
}
