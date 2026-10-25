//! The provider authority a Managed Sandbox layout validation runs with, and
//! the resource prompt fragments a realized Session renders; the layout
//! projection itself is the contract's (`managed_layout`).

mod resource_prompt;

pub use resource_prompt::{AgentPathView, ResourcePromptProjection};

/// Authority available while validating one frozen Managed Sandbox layout.
///
/// A Coordinator dispatching a Worker-owned Session can validate only the
/// provider-neutral frozen structure. The claimed Worker owns the exact
/// provider projection and repeats the same structural kernel against that
/// provider's effective spec before any physical effect.
#[derive(Clone, Copy)]
pub enum ManagedLayoutValidationScope<'a> {
    Structural,
    Exact(&'a crate::session_environment::SessionEnvironmentProvider),
}

impl ManagedLayoutValidationScope<'_> {
    pub fn network_isolation(self) -> bool {
        match self {
            Self::Structural => true,
            Self::Exact(provider) => provider.realizer().capabilities().network_isolation,
        }
    }
}
