//! Awaken's own HTTP routes: Awaken platform resources that are not extensions
//! of another protocol (a Session's live inbox, a Memory store's Dream policy,
//! sandbox execution policies of Environments).
//!
//! Every route is namespaced under `/v1/awaken`. This crate drives neutral
//! application ports; concrete application services are injected only by the
//! composition root. Extensions of the Anthropic Managed Agents protocol (they
//! speak its envelope, ids and headers) live in `awaken-protocol-managed`.

mod dream_policies;
mod live_inbox;
mod sandbox_policies;

pub use dream_policies::dream_policy_router;
pub use live_inbox::live_inbox_router;
pub use sandbox_policies::{
    EnvironmentCreateInput, SandboxPolicyBindingInput, SandboxPolicyCreate,
    environment_extensions_router,
};
