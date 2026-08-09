//! Explicit Awaken HTTP extensions layered beside, never inside, the Anthropic
//! Managed Agents compatibility adapter.
//!
//! Every route is namespaced under `/v1/awaken`. This crate drives neutral
//! application ports; concrete application services are injected only by the
//! composition root.

mod dream_policies;
mod live_inbox;
mod resource_manifests;
mod sandbox_policies;

pub use dream_policies::dream_policy_router;
pub use live_inbox::live_inbox_router;
pub use resource_manifests::session_resource_manifest_router;
pub use sandbox_policies::environment_extensions_router;
