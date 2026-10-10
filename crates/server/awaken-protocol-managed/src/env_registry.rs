// @formal surface requirement=product.public_dto.anti_corruption
// @formal reason: Projects the canonical Control Environment definition into the official SDK DTO and
// @formal reason: rejects absent authored timestamps. It owns no durable state, registration, clock, or
// @formal reason: recovery transition. The timestamp decision table and HTTP inventory tests validate this
// @formal reason: protocol boundary; this requirement link does not claim a new formal proof.
//! The self-hosted environment registry: the port + neutral `EnvItem` now live in
//! `awaken-environment-contract`; this module consumes them and owns
//! the two projections the neutral crate must not name — the `BetaEnvironment` wire
//! shape and the sandbox `NetworkPolicy` derived from a record's `config`.

use awaken_environment_contract::EnvItem;

use crate::types::environment::Environment;

/// Project an [`EnvItem`] to the official `BetaEnvironment` wire shape. No `scope` on
/// the wire: ownership is credential-implicit (authz enforces the workspace).
pub(crate) fn project_env(item: &EnvItem) -> Result<Environment, &'static str> {
    Ok(Environment {
        id: item.id.clone(),
        object_type: "environment",
        archived_at: item.archived_at.clone(),
        created_at: item
            .created_at
            .clone()
            .ok_or("Environment creation timestamp is missing")?,
        updated_at: item
            .updated_at
            .clone()
            .ok_or("Environment update timestamp is missing")?,
        name: item.name.clone(),
        description: item.description.clone(),
        metadata: item.metadata.clone(),
        scope: item.scope.clone(),
        config: item.config.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_projection_requires_both_observed_timestamps() {
        // Decision table over two independently missing observations: only
        // (created, updated) present can produce an official resource. A
        // corrupt authored row must fail, never be silently dropped or given
        // fabricated time. The route separately excludes the built-in default.
        for (created, updated) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut item = awaken_environment_application::builtin_local_environment();
            item.id = "env_authored".into();
            item.created_at = created.then(|| "2026-09-22T00:00:00Z".into());
            item.updated_at = updated.then(|| "2026-09-22T01:00:00Z".into());
            assert_eq!(project_env(&item).is_ok(), created && updated);
        }
    }
}
