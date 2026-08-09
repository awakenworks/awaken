//! Managed control-plane HTTP adapters and wire anti-corruption mappings.

mod models;
pub(crate) mod vault_acl;

pub use models::{
    ModelDirectory, ModelDirectoryFuture, ModelEntry, default_models, models_router,
    models_router_with_directory,
};
