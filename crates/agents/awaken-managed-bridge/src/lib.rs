//! Anti-corruption layer (ADR-0043 / G16) between the public Anthropic Managed
//! Agents wire (`@anthropic-ai/sdk`, mirrored by `awaken-protocol-managed`) and
//! the neutral credential/catalog domain. The wire keeps Anthropic's snake_case
//! tags (`environment_variable`/`static_bearer`/`mcp_oauth`); the domain keeps
//! neutral names. This crate is the only place the two vocabularies meet.
//!
//! P0/P1 scope: the `environment_variable` credential mapping both ways
//! (secret-in create params ← wire; secret-free projection → wire).

#![forbid(unsafe_code)]

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{CredentialCreateParams, CredentialKind, CredentialSource};

/// The Managed Agents beta wire header this bridge targets.
pub const MANAGED_BETA: &str = "managed-agents-2026-04-01";

/// The `environment_variable` create params as they arrive on the Managed wire
/// (`BetaManagedAgentsEnvironmentVariableCreateParams`). `secret_value` is
/// write-only and never echoed back — the ACL consumes it into the domain.
pub struct WireEnvVarCreate {
    pub secret_name: String,
    pub secret_value: String,
}

/// Map a wire `environment_variable` credential into secret-in domain create
/// params. The secret crosses into a `RedactedString` here and is sealed by the
/// `SecretStore` in `create_source`; it is never placed on the domain row.
#[must_use]
pub fn env_var_to_create_params(
    workspace_id: impl Into<String>,
    provider_id: Option<String>,
    wire: WireEnvVarCreate,
) -> CredentialCreateParams {
    CredentialCreateParams {
        workspace_id: workspace_id.into(),
        kind: CredentialKind::Vault,
        provider_id,
        env_key: Some(wire.secret_name),
        secret: Some(RedactedString::new(wire.secret_value)),
    }
}

/// The secret-free wire projection of a credential (`ManagedCredential` shape):
/// id + type + the resolved auth kind. Never carries secret material.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ManagedCredentialView {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    /// The wire auth-kind tag.
    pub auth_type: &'static str,
    /// The environment-variable name (non-secret), when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
}

/// Project a domain [`CredentialSource`] to its secret-free wire view. The wire
/// keeps Anthropic's `environment_variable` tag for a vault/env credential.
#[must_use]
pub fn to_managed_view(source: &CredentialSource) -> ManagedCredentialView {
    ManagedCredentialView {
        id: source.id.0.clone(),
        object_type: "credential",
        auth_type: "environment_variable",
        secret_name: source.env_key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::{InMemorySecretStore, create_source, materialize};

    #[tokio::test]
    async fn wire_env_var_round_trips_secret_in_secret_free_out() {
        let store = InMemorySecretStore::new();
        let params = env_var_to_create_params(
            "ws1",
            Some("anthropic".into()),
            WireEnvVarCreate {
                secret_name: "ANTHROPIC_API_KEY".into(),
                secret_value: "sk-from-the-wire".into(),
            },
        );
        let source = create_source(params, &store).await.unwrap();

        // The wire projection is secret-free and keeps Anthropic's tags.
        let view = to_managed_view(&source);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("sk-from-the-wire"));
        assert!(json.contains("environment_variable"));
        assert_eq!(view.secret_name.as_deref(), Some("ANTHROPIC_API_KEY"));

        // But the secret materializes back at the seam.
        assert_eq!(
            materialize(&source, &store).await.unwrap().expose_secret(),
            "sk-from-the-wire"
        );
    }
}
