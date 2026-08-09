//! The self-hosted environment registry: the port + neutral `EnvItem` now live in
//! `awaken-environment-contract`; this module consumes them and owns
//! the two projections the neutral crate must not name — the `BetaEnvironment` wire
//! shape and the sandbox `NetworkPolicy` derived from a record's `config`.

use awaken_environment_contract::OBJECT_AT;
pub(crate) use awaken_environment_contract::{
    EnvItem, EnvRegistry, EnvUpdate, EnvironmentConfigMutation, EnvironmentNetworkingMutation,
    EnvironmentPackagesMutation,
};

use crate::types::environment::Environment;

/// Project an [`EnvItem`] to the official `BetaEnvironment` wire shape. No `scope` on
/// the wire: ownership is credential-implicit (authz enforces the workspace).
#[must_use]
pub(crate) fn project_env(item: &EnvItem) -> Environment {
    Environment {
        id: item.id.clone(),
        object_type: "environment",
        archived_at: item.archived_at.clone(),
        created_at: OBJECT_AT.to_string(),
        updated_at: OBJECT_AT.to_string(),
        name: item.name.clone(),
        description: item.description.clone(),
        metadata: item.metadata.clone(),
        scope: item.scope.clone(),
        config: item.config.clone(),
    }
}
