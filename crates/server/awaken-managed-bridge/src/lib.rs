//! Anti-corruption layer (ADR-0043 / G16) between the public Anthropic Managed
//! Agents wire (`@anthropic-ai/sdk`, mirrored by `awaken-protocol-managed`) and
//! the neutral credential/catalog domain. The wire keeps Anthropic's snake_case
//! tags (`environment_variable`/`static_bearer`/`mcp_oauth`); the domain keeps
//! neutral names. This crate is the only place the two vocabularies meet.
//!
//! Scope: the `environment_variable`, `static_bearer`, and `mcp_oauth` credential
//! mappings (secret-in create params ← wire; secret-free projection → wire). Every
//! wire secret (`secret_value` / `token` / `access_token` / `refresh_token`)
//! crosses into a `RedactedString` here and is sealed by the domain's
//! `SecretStore`; nothing this crate returns carries plaintext on a row.

#![forbid(unsafe_code)]

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{CredentialCreateParams, CredentialKind, CredentialSource};

/// The Managed Agents beta wire header this bridge targets.
pub const MANAGED_BETA: &str = "managed-agents-2026-04-01";

/// A resolved model reference decoded from a Managed agent definition (ADR-0043
/// "model-extension resolution"). The public wire keeps a bare Anthropic-compatible
/// `model` string; any binding detail rides in `metadata.awaken`. The resolver
/// consumes this — the Managed API references a model, never inlines provider config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    /// The catalog model id the resolver looks up.
    pub model_id: String,
    /// An explicit credential-binding hint from `metadata.awaken`, if the operator
    /// pinned one; `None` means "resolve by default binding".
    pub credential_source_id: Option<String>,
}

/// Errors decoding the model axis (fail-closed — A-G12 analogue).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelAxisError {
    #[error("managed agent has no `model` and no metadata.awaken model axis (fail closed)")]
    NoModel,
}

/// Decode the Managed agent's `model` field + optional `metadata.awaken` object
/// into a [`ModelRef`] for the resolver. The `metadata.awaken.model` axis may carry
/// `{ "id": "...", "credential_source_id": "..." }` to override/extend the bare
/// string. Fail-closed when neither yields a model id.
pub fn decode_model_axis(
    model: Option<&str>,
    metadata_awaken: Option<&serde_json::Value>,
) -> Result<ModelRef, ModelAxisError> {
    let axis = metadata_awaken.and_then(|m| m.get("model"));
    // The axis id (if present AND non-empty) takes precedence over the bare wire
    // `model` string. Filtering the empty string *before* the fallback is what makes
    // an empty axis id fall through to a valid bare `model` instead of shadowing it
    // (the empty check must not run only after `or_else`, or `Some("")` would win and
    // then be discarded, rejecting an otherwise-valid request).
    let model_id = axis
        .and_then(|a| a.get("id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| model.map(str::to_string))
        .filter(|s| !s.is_empty())
        .ok_or(ModelAxisError::NoModel)?;
    let credential_source_id = axis
        .and_then(|a| a.get("credential_source_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(ModelRef {
        model_id,
        credential_source_id,
    })
}

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
    pub mcp_server_url: String,
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
        provider_id: None,
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
    pub mcp_server_url: String,
    pub access_token: String,
    /// The refresh token from the wire `refresh` object, if one was supplied.
    pub refresh_token: Option<String>,
}

/// An `mcp_oauth` credential mapped for the domain: the access token rides the
/// row's create params (sealed as its `material_ref`); the refresh token — a
/// *second* secret — comes back as a sealed-ready `RedactedString` for the caller
/// to put under its own `SecretRef` next to the row. Consumer: the vault surface's
/// create handler in `awaken-protocol-managed`.
pub struct McpOauthDomainCreate {
    pub params: CredentialCreateParams,
    /// Present iff the wire supplied a refresh token; never placed on the row.
    pub refresh_secret: Option<RedactedString>,
}

/// Map a wire `mcp_oauth` credential into secret-in domain create params. Both
/// tokens cross into `RedactedString`s here; the domain row stays secret-free
/// (the access token sealed as `material_ref`, the refresh token sealed by the
/// caller under a sibling ref).
#[must_use]
pub fn mcp_oauth_to_create_params(
    workspace_id: impl Into<String>,
    wire: WireMcpOauthCreate,
) -> McpOauthDomainCreate {
    McpOauthDomainCreate {
        params: CredentialCreateParams {
            workspace_id: workspace_id.into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            // As with `static_bearer`: URL-bound, not env-injected.
            env_key: None,
            secret: Some(RedactedString::new(wire.access_token)),
            oauth_command: None,
        },
        refresh_secret: wire.refresh_token.map(RedactedString::new),
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

    #[tokio::test]
    async fn wire_static_bearer_seals_the_token_off_the_row() {
        let store = InMemorySecretStore::new();
        let params = static_bearer_to_create_params(
            "vlt_1",
            WireStaticBearerCreate {
                mcp_server_url: "https://mcp.example.com/sse".into(),
                token: "brr-from-the-wire".into(), // awaken-allow: secret
            },
        );
        let source = create_source(params, &store).await.unwrap();

        // The row is secret-free and env-key-free (URL-bound, not env-injected)…
        let json = serde_json::to_string(&source).unwrap();
        assert!(!json.contains("brr-from-the-wire"));
        assert!(source.env_key.is_none());
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
                mcp_server_url: "https://mcp.example.com/sse".into(),
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
                mcp_server_url: "https://mcp.example.com/sse".into(),
                access_token: "at2".into(),
                refresh_token: None,
            },
        );
        assert!(bridged.refresh_secret.is_none());
    }

    #[test]
    fn bare_model_string_decodes_to_model_ref() {
        let r = decode_model_axis(Some("claude-opus-4-8"), None).unwrap();
        assert_eq!(r.model_id, "claude-opus-4-8");
        assert!(r.credential_source_id.is_none());
    }

    #[test]
    fn metadata_awaken_axis_overrides_and_pins_credential() {
        let meta = serde_json::json!({
            "model": { "id": "glm-4.6", "credential_source_id": "cred:ws:7" }
        });
        let r = decode_model_axis(Some("claude-opus-4-8"), Some(&meta)).unwrap();
        assert_eq!(r.model_id, "glm-4.6"); // axis id wins over the bare wire string
        assert_eq!(r.credential_source_id.as_deref(), Some("cred:ws:7"));
    }

    #[test]
    fn no_model_anywhere_fails_closed() {
        assert!(matches!(
            decode_model_axis(None, None),
            Err(ModelAxisError::NoModel)
        ));
        assert!(matches!(
            decode_model_axis(Some(""), None),
            Err(ModelAxisError::NoModel)
        ));
    }

    #[test]
    fn an_empty_axis_id_falls_back_to_the_bare_model() {
        // A client that always sends the axis object but leaves `id` empty (no
        // override) must still resolve the bare wire `model` — the empty axis id
        // does not shadow it. It also does not discard a pinned credential source.
        let meta =
            serde_json::json!({ "model": { "id": "", "credential_source_id": "cred:ws:9" } });
        let r = decode_model_axis(Some("claude-opus-4-8"), Some(&meta)).unwrap();
        assert_eq!(
            r.model_id, "claude-opus-4-8",
            "bare model is used, not shadowed"
        );
        assert_eq!(r.credential_source_id.as_deref(), Some("cred:ws:9"));
    }

    #[test]
    fn an_empty_axis_id_with_no_bare_model_still_fails_closed() {
        let meta = serde_json::json!({ "model": { "id": "" } });
        assert!(matches!(
            decode_model_axis(None, Some(&meta)),
            Err(ModelAxisError::NoModel)
        ));
    }

    #[test]
    fn a_metadata_object_without_a_model_key_uses_the_bare_model() {
        // `metadata.awaken` present but carrying no `model` axis → the bare wire
        // string is used (the axis lookup is None, not an error).
        let meta = serde_json::json!({ "something_else": true });
        let r = decode_model_axis(Some("claude-sonnet-5"), Some(&meta)).unwrap();
        assert_eq!(r.model_id, "claude-sonnet-5");
        assert!(r.credential_source_id.is_none());
    }
}
