//! Resolution: `agent_id` + `RegistrySet` -> `ResolvedAgent`.

mod error;
mod pipeline;
mod pool;

pub use error::ResolveError;
pub(crate) use pipeline::DynamicRegistryResolver;
pub use pipeline::RegistrySetResolver;
pub(crate) use pipeline::inject_default_plugins_with_stop_policies;
