// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Environment component owns the original per-Session lifecycle mutex and physical owner projection; no second registry or durable authority is introduced.
//! Environment holding of the realization executor: the Environment each
//! installed Session holds, through the sandbox providers it realizes with.
//! It selects the provider, creates, adopts, restores and retires the exact
//! Environment owner under the Session's lifecycle lock, and keeps the warm
//! capacity those providers prepare.

/// Holds each installed Session's Environment. It owns the sandbox providers;
/// staging and the execution authority are the ports its effects go through.
pub struct EnvironmentHolding {
    /// The selected aggregate publication authority; the lock permits fixture installation only.
    pub environment_binding_sink: std::sync::RwLock<
        Option<std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    >,
    /// The same dispatch instance used by this process, narrowed to execution binding.
    pub dispatch_execution:
        Option<std::sync::Arc<dyn awaken_dispatch_contract::DispatchExecution>>,
    /// Frozen publication selection over the installed Session slots.
    pub publication: crate::session_publication::SessionPublicationSelection,

    /// The Session-owned, BackendOwned and housekeeping sandbox providers.
    pub env: crate::environment_providers::EnvironmentProviders,
    /// The installed Sessions, as far as realization may reach them.
    pub sessions: crate::session_realizations::SessionRealizations,
    /// Resource staging: spec projection, registration and layout validation.
    pub staging: crate::resource_staging::ResourceStaging,
    /// The authority physical Environment effects run under.
    pub execution: crate::execution_authority::ExecutionAuthority,
}

/// Test-only spec helpers over the default Session provider. A fixture with a
/// poisoned registry is a broken test, so these helpers panic instead of
/// reporting the realization denial production paths return.
#[cfg(any(test, feature = "test-support"))]
impl EnvironmentHolding {
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
                self.env
                    .session_provider
                    .realizer()
                    .capabilities()
                    .network_isolation,
            )
            .expect("test fixture registry is healthy")
    }
}
