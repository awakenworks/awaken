//! Explicit Awaken HTTP extensions layered beside, never inside, the Anthropic
//! Managed Agents compatibility adapter.
//!
//! Every route is namespaced under `/v1/awaken`. This crate may depend on the
//! compatible adapter's application state, but the compatible adapter never
//! imports or mounts this crate.

mod live_inbox;
mod sandbox_policies;

pub use live_inbox::live_inbox_router;
pub use sandbox_policies::environment_extensions_router;
