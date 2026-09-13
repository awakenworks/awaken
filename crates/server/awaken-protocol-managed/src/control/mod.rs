//! Managed control-plane HTTP adapters and wire anti-corruption mappings.

pub(crate) mod models;
pub(crate) mod vault_acl;

pub use models::{ModelEntry, default_models, models_router, models_router_with_inventory};
