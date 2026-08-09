//! Vault anti-corruption layer (ADR-0043 / G16) between the public Anthropic Managed
//! Agents wire (`@anthropic-ai/sdk`, mirrored by `awaken-protocol-managed`) and
//! the neutral credential/catalog domain. The wire keeps Anthropic's snake_case
//! tags (`environment_variable`/`static_bearer`/`mcp_oauth`); the domain keeps
//! neutral names. This module is the only place the two vocabularies meet.
//!
//! Scope: the `environment_variable`, `static_bearer`, and `mcp_oauth` credential
//! mappings (secret-in create params ← wire; secret-free projection → wire). Every
//! wire secret (`secret_value` / `token` / `access_token` / `refresh_token`)
//! crosses into a `RedactedString` here and is sealed by the domain's
//! `SecretStore`; nothing this crate returns carries plaintext on a row.

#![forbid(unsafe_code)]

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{CredentialCreateParams, CredentialKind};

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
        oauth_command: None,
    }
}

/// The `static_bearer` create params as they arrive on the Managed wire
/// (`BetaManagedAgentsStaticBearerCreateParams`). `token` is write-only and never
/// echoed back — the ACL consumes it into the domain. The `mcp_server_url` stays
/// on the wire-side record (the neutral domain row does not model MCP bindings).
pub struct WireStaticBearerCreate {
    pub token: String,
}

/// Map a wire `static_bearer` credential into secret-in domain create params.
/// The bearer token crosses into a `RedactedString` here and is sealed by the
/// `SecretStore` in `create_source`; it is never placed on the domain row.
#[must_use]
pub fn static_bearer_to_create_params(
    workspace_id: impl Into<String>,
    wire: WireStaticBearerCreate,
) -> CredentialCreateParams {
    CredentialCreateParams {
        workspace_id: workspace_id.into(),
        kind: CredentialKind::Vault,
        // Runtime MCP material is deliberately not an unscoped model-provider
        // credential. The MCP materializer skips the provider join, while model
        // publication cannot accidentally select this bearer token as an API key.
        provider_id: Some("mcp".into()),
        // No env-var name: a bearer credential is bound to its MCP server by URL
        // (kept on the wire record), not injected into a process environment.
        env_key: None,
        secret: Some(RedactedString::new(wire.token)),
        oauth_command: None,
    }
}

/// The `mcp_oauth` create params as they arrive on the Managed wire
/// (`BetaManagedAgentsMCPOAuthCreateParams`), reduced to the secret axis the
/// domain cares about. `access_token` and `refresh_token` are write-only.
pub struct WireMcpOauthCreate {
    pub access_token: String,
    /// The refresh token from the wire `refresh` object, if one was supplied.
    pub refresh_token: Option<String>,
}

/// An `mcp_oauth` credential mapped for the domain: the access token rides the
/// row's create params (sealed as its `material_ref`); the refresh token — a
/// *second* secret — comes back as a sealed-ready `RedactedString` for the caller
/// to enter in the same aggregate's named material set. Consumer: the vault
/// surface's create handler in `awaken-protocol-managed`.
pub struct McpOauthDomainCreate {
    pub params: CredentialCreateParams,
    /// Present iff the wire supplied a refresh token; never placed on the row.
    pub refresh_secret: Option<RedactedString>,
}

/// Map a wire `mcp_oauth` credential into secret-in domain create params. Both
/// tokens cross into `RedactedString`s here; the domain row stays secret-free
/// (the access token and refresh slot are committed together by the caller).
#[must_use]
pub fn mcp_oauth_to_create_params(
    workspace_id: impl Into<String>,
    wire: WireMcpOauthCreate,
) -> McpOauthDomainCreate {
    McpOauthDomainCreate {
        params: CredentialCreateParams {
            workspace_id: workspace_id.into(),
            kind: CredentialKind::Vault,
            provider_id: Some("mcp".into()),
            // As with `static_bearer`: URL-bound, not env-injected.
            env_key: None,
            secret: Some(RedactedString::new(wire.access_token)),
            oauth_command: None,
        },
        refresh_secret: wire.refresh_token.map(RedactedString::new),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::{InMemorySecretStore, create_source, materialize};

    #[tokio::test]
    async fn wire_env_var_seals_secret_off_the_domain_row() {
        // Cause/effect rule V1: an environment-variable wire secret crosses the
        // ACL -> the persisted domain row contains no plaintext, while the
        // authorized materialization seam returns the exact original value.
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

        let json = serde_json::to_string(&source).unwrap();
        assert!(!json.contains("sk-from-the-wire"));
        assert_eq!(source.env_key.as_deref(), Some("ANTHROPIC_API_KEY"));

        // But the secret materializes back at the seam.
        assert_eq!(
            materialize(&source, &store).await.unwrap().expose_secret(),
            "sk-from-the-wire"
        );
    }

    #[tokio::test]
    async fn wire_static_bearer_seals_the_token_off_the_row() {
        let store = InMemorySecretStore::new();
        let params = static_bearer_to_create_params(
            "vlt_1",
            WireStaticBearerCreate {
                token: "brr-from-the-wire".into(), // awaken-allow: secret
            },
        );
        let source = create_source(params, &store).await.unwrap();

        // The row is secret-free and env-key-free (URL-bound, not env-injected)…
        let json = serde_json::to_string(&source).unwrap();
        assert!(!json.contains("brr-from-the-wire"));
        assert!(source.env_key.is_none());
        assert_eq!(source.provider_id.as_deref(), Some("mcp"));
        // …but the token materializes back at the seam.
        assert_eq!(
            materialize(&source, &store).await.unwrap().expose_secret(),
            "brr-from-the-wire"
        );
    }

    #[tokio::test]
    async fn wire_mcp_oauth_splits_access_and_refresh_secrets() {
        let store = InMemorySecretStore::new();
        let bridged = mcp_oauth_to_create_params(
            "vlt_1",
            WireMcpOauthCreate {
                access_token: "at-from-the-wire".into(), // awaken-allow: secret
                refresh_token: Some("rt-from-the-wire".into()),
            },
        );
        // The refresh token is a second secret, handed back sealed-ready.
        assert_eq!(
            bridged.refresh_secret.as_ref().unwrap().expose_secret(),
            "rt-from-the-wire"
        );
        let source = create_source(bridged.params, &store).await.unwrap();
        assert_eq!(source.provider_id.as_deref(), Some("mcp"));
        assert!(
            !serde_json::to_string(&source)
                .unwrap()
                .contains("at-from-the-wire")
        );
        assert_eq!(
            materialize(&source, &store).await.unwrap().expose_secret(),
            "at-from-the-wire"
        );

        // Without a wire refresh object there is no second secret.
        let bridged = mcp_oauth_to_create_params(
            "vlt_1",
            WireMcpOauthCreate {
                access_token: "at2".into(),
                refresh_token: None,
            },
        );
        assert!(bridged.refresh_secret.is_none());
    }

    #[test]
    fn env_var_create_params_map_all_axes_without_a_provider() {
        // The pure mapping (independent of the store): Vault kind, the wire
        // `secret_name` becomes the env key, the secret is captured, no provider.
        let params = env_var_to_create_params(
            "ws1",
            None,
            WireEnvVarCreate {
                secret_name: "OPENAI_API_KEY".into(),
                secret_value: "sk-x".into(),
            },
        );
        assert_eq!(params.workspace_id, "ws1");
        assert_eq!(params.kind, CredentialKind::Vault);
        assert!(params.provider_id.is_none());
        assert_eq!(params.env_key.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(params.secret.as_ref().unwrap().expose_secret(), "sk-x");
        assert!(params.oauth_command.is_none());
    }
}
