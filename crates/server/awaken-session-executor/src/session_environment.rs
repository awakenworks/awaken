//! The Session's live execution environment and its provider, as the role
//! handles the Host holds (`awaken-session-environment-contract`). The
//! sandbox-backed adapters (`awaken-session-environment`) are installed by the
//! process composition; this crate links them only in its own tests.

#[cfg(test)]
pub(crate) use awaken_session_environment_contract::AgentFilesystemClient;
pub(crate) use awaken_session_environment_contract::{
    ProviderEffect, SessionEnvironment, SessionEnvironmentProvider,
};

#[cfg(test)]
pub(crate) use awaken_session_environment::test_support::{
    SandboxEnvironmentAccess, UnusedHandExecutorFactory, relocated_provider,
    spec as environment_test_spec,
};
#[cfg(test)]
pub(crate) use awaken_session_environment::{SandboxEnvironment, SandboxEnvironmentProvider};

#[cfg(test)]
mod tests;
