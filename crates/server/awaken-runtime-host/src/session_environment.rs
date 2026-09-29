//! The single live execution environment owned by a Session.
//!
//! Host code depends on this capability object instead of retaining a concrete
//! sandbox in `SessionCtx`. Workdir is the first adapter; Namespace and Container
//! plug into the same owner without adding another Native/ACP lifecycle.

mod agent_sandbox;
mod container_files;
mod container_repositories;
mod container_skills;
mod environment;
mod lifecycle;
mod provider;
mod repository_realizer;
mod roles;
mod session_files;
mod session_hand;
pub(crate) use agent_sandbox::{AgentFilesystemClient, AgentFilesystemView};
pub(crate) use awaken_session_environment_contract::SessionEnvironment;
pub(crate) use awaken_session_environment_contract::SessionEnvironmentProvider;
pub(crate) use environment::SandboxEnvironment;
pub(crate) use provider::{ProviderEffect, SandboxEnvironmentProvider};
pub(crate) use session_hand::HandExecutorFactory;
#[cfg(test)]
pub(crate) use session_hand::UnusedHandExecutorFactory;

/// Test-support relocation of a sandbox-backed provider under `base`, so a
/// durable fixture's environments survive a restart at a stable root. A
/// provider this crate did not build is kept as installed.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn relocated_provider(
    provider: &SessionEnvironmentProvider,
    base: impl Into<std::path::PathBuf>,
) -> SessionEnvironmentProvider {
    let base = base.into();
    provider
        .origin()
        .downcast_ref::<SandboxEnvironmentProvider>()
        .map_or_else(
            || provider.clone(),
            |sandbox| sandbox.at_root(base).into_session_provider(),
        )
}

/// Test access to the sandbox adapter behind a Session environment provider's
/// role handles.
#[cfg(test)]
pub(crate) trait SandboxEnvironmentProviderAccess {
    fn sandbox_provider(&self) -> &SandboxEnvironmentProvider;
}

#[cfg(test)]
impl SandboxEnvironmentProviderAccess for SessionEnvironmentProvider {
    fn sandbox_provider(&self) -> &SandboxEnvironmentProvider {
        self.origin()
            .downcast_ref()
            .expect("a sandbox-backed Session environment provider")
    }
}

/// Test access to the sandbox adapter behind a Session environment's role
/// handles, for observability the roles deliberately do not offer.
#[cfg(test)]
pub(crate) trait SandboxEnvironmentAccess {
    fn sandbox_environment(&self) -> &SandboxEnvironment;
}

#[cfg(test)]
impl SandboxEnvironmentAccess for SessionEnvironment {
    fn sandbox_environment(&self) -> &SandboxEnvironment {
        self.origin()
            .downcast_ref()
            .expect("a sandbox-backed Session environment")
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::spec as environment_test_spec;
