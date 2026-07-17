//! The self-hosted environment registry: the port + neutral `EnvItem` now live in
//! `awaken-session-contract` (a contract/ leaf); this module re-exports them and owns
//! the two projections the neutral crate must not name — the `BetaEnvironment` wire
//! shape and the sandbox `NetworkPolicy` derived from a record's `config`.

use awaken_provisioning_contract::NetworkPolicy;
use serde_json::Value;

pub use awaken_env_store::InMemoryEnvRegistry;
use awaken_session_contract::env_registry::OBJECT_AT;
pub use awaken_session_contract::env_registry::{EnvItem, EnvRegistry, EnvUpdate};

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
        config: item.config.clone(),
    }
}

/// Map an environment's `networking` wire config onto the neutral [`NetworkPolicy`]
/// the sandbox understands: `unrestricted → Unrestricted`, `limited{hosts} →
/// Allowlist`, `none → None`. Absent networking (incl. `self_hosted`) or an unknown
/// type shares the host network.
#[must_use]
pub(crate) fn env_network_policy(config: &Value) -> NetworkPolicy {
    let Some(net) = config.get("networking") else {
        return NetworkPolicy::Unrestricted;
    };
    match net.get("type").and_then(Value::as_str) {
        Some("none") => NetworkPolicy::None,
        Some("limited") => {
            let hosts = net
                .get("allowed_hosts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|h| h.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            NetworkPolicy::Allowlist { hosts }
        }
        _ => NetworkPolicy::Unrestricted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn network_policy_maps_all_wire_types() {
        // unrestricted → shares host network
        assert_eq!(
            env_network_policy(&json!({ "networking": { "type": "unrestricted" } })),
            NetworkPolicy::Unrestricted
        );
        // limited{allowed_hosts} → typed Allowlist, denies under bwrap (fail-closed)
        assert_eq!(
            env_network_policy(&json!({
                "networking": { "type": "limited", "allowed_hosts": ["api.anthropic.com"] }
            })),
            NetworkPolicy::Allowlist {
                hosts: vec!["api.anthropic.com".to_string()],
            }
        );
        // none → no egress
        assert_eq!(
            env_network_policy(&json!({ "networking": { "type": "none" } })),
            NetworkPolicy::None
        );
        // absent networking / self_hosted / unknown → Unrestricted (shares host)
        assert_eq!(
            env_network_policy(&json!({ "type": "self_hosted" })),
            NetworkPolicy::Unrestricted
        );
    }
}
