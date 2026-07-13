//! Worker egress authorization (ADR-0044/0046 line; open Gap D/E).
//!
//! How a run's model/tool egress is credentialed is a **neutral grant** carried in
//! the run manifest — never a provider secret. Two shapes:
//!
//! - [`ModelAccessGrant::LocalSelfCredentialed`] — the worker uses its own local
//!   provider config/tokens (self-hosted; the platform takes no custody).
//! - [`ModelAccessGrant::CloudManagedGateway`] — egress is mediated by a gateway:
//!   the worker holds a **short-lived lease token**, not a provider key, and a
//!   base URL that is the gateway (not an arbitrary provider endpoint). This is
//!   what lets a placed hand run credential-free while a gateway injects the real
//!   provider credentials out of the worker's address space.
//!
//! [`ModelAccessMaterializer`] renders a grant into a concrete
//! [`ResolvedModelEndpoint`] the runtime dials. The open default
//! ([`FieldMaterializer`]) maps both variants by field; a host injects a richer
//! materializer (e.g. external-agent adapter profiles) for gateway grants.

use serde::{Deserialize, Serialize};

/// How a run reaches its model — a neutral, secret-free grant in the manifest.
///
/// The default is [`ModelAccessGrant::LocalSelfCredentialed`] with no references,
/// so an activation that names no access mode reads as an explicit "use local
/// config" — there is no "unspecified" state to reason about (a manifest always
/// carries a concrete mode).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelAccessGrant {
    /// The worker uses its own local provider credentials/config; the platform
    /// takes no custody. Only *references* travel, never secret material.
    LocalSelfCredentialed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_ref: Option<String>,
    },
    /// Egress is mediated by a gateway. The worker holds a lease token (short-lived,
    /// revocable), not a provider key; `gateway_base_url` is the gateway, so the
    /// worker can never be pointed at an arbitrary provider endpoint.
    CloudManagedGateway {
        gateway_base_url: String,
        surface: String,
        model_ref: String,
        /// Short-lived lease capability — opaque handshake material, never logged.
        lease_token: String,
    },
}

impl Default for ModelAccessGrant {
    fn default() -> Self {
        Self::LocalSelfCredentialed {
            model_ref: None,
            provider_ref: None,
        }
    }
}

impl ModelAccessGrant {
    /// Materialize this grant into the concrete [`ResolvedModelEndpoint`] the
    /// runtime dials (ADR-0004). Pure, total, and infallible — the single place the
    /// local-vs-gateway distinction is decided, so no downstream altitude re-branches
    /// on the variant. A local grant resolves to "use local config" (`base_url` and
    /// `bearer` both `None`); a gateway grant resolves to the gateway URL + lease
    /// bearer. Because a gateway grant's `base_url`/`bearer` are structurally always
    /// `Some`, a consumer that falls back to local env only for a `None` field can
    /// never route a gateway run onto a raw provider key.
    #[must_use]
    pub fn materialize(&self) -> ResolvedModelEndpoint {
        match self {
            Self::LocalSelfCredentialed {
                model_ref,
                provider_ref: _,
            } => ResolvedModelEndpoint {
                base_url: None,
                model_ref: model_ref.clone(),
                bearer: None,
                surface: None,
            },
            Self::CloudManagedGateway {
                gateway_base_url,
                surface,
                model_ref,
                lease_token,
            } => ResolvedModelEndpoint {
                base_url: Some(gateway_base_url.clone()),
                model_ref: Some(model_ref.clone()),
                bearer: Some(lease_token.clone()),
                surface: Some(surface.clone()),
            },
        }
    }

    /// Whether this grant routes egress through a cloud-managed gateway. An altitude
    /// that cannot honor a gateway grant (e.g. the native in-process provider path)
    /// checks this to fail closed rather than degrade to local credentials.
    #[must_use]
    pub fn is_gateway(&self) -> bool {
        matches!(self, Self::CloudManagedGateway { .. })
    }
}

/// A materialized inference endpoint the runtime dials. `bearer` is opaque
/// handshake material (a lease token, or `None` when local config supplies creds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelEndpoint {
    /// The base URL to dial, or `None` to fall back to the worker's local provider
    /// configuration (the `LocalSelfCredentialed` case).
    pub base_url: Option<String>,
    /// The model id to request.
    pub model_ref: Option<String>,
    /// Opaque bearer presented on egress (a lease token). Never logged.
    pub bearer: Option<String>,
    /// The provider surface (e.g. an Anthropic/OpenAI-compatible shape), if named.
    pub surface: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_serde_round_trips_and_carries_no_provider_key_field() {
        let local = ModelAccessGrant::LocalSelfCredentialed {
            model_ref: Some("m".into()),
            provider_ref: None,
        };
        let gateway = ModelAccessGrant::CloudManagedGateway {
            gateway_base_url: "https://gw.internal".into(),
            surface: "AnthropicMessages".into(),
            model_ref: "claude".into(),
            lease_token: "lease-abc".into(), // awaken-allow: secret
        };
        for g in [&local, &gateway] {
            let json = serde_json::to_string(g).unwrap();
            assert_eq!(&serde_json::from_str::<ModelAccessGrant>(&json).unwrap(), g);
            // Fail-closed (ADR-0004): a grant never carries a raw provider key.
            for forbidden in ["api_key", "apiKey", "x-api-key", "secret", "refresh_token"] {
                assert!(
                    !json.contains(forbidden),
                    "grant leaked a credential field: {forbidden} in {json}"
                );
            }
        }
    }

    #[test]
    fn local_grant_materializes_to_use_local_config() {
        let ep = ModelAccessGrant::LocalSelfCredentialed {
            model_ref: Some("m".into()),
            provider_ref: Some("p".into()),
        }
        .materialize();
        assert_eq!(ep.base_url, None); // fall back to local provider config
        assert_eq!(ep.bearer, None); // no cloud lease; local creds apply
        assert_eq!(ep.model_ref.as_deref(), Some("m"));
    }

    #[test]
    fn gateway_grant_materializes_to_gateway_url_and_lease_bearer() {
        let ep = ModelAccessGrant::CloudManagedGateway {
            gateway_base_url: "https://gw.internal".into(),
            surface: "AnthropicMessages".into(),
            model_ref: "claude".into(),
            lease_token: "lease-abc".into(), // awaken-allow: secret
        }
        .materialize();
        assert_eq!(ep.base_url.as_deref(), Some("https://gw.internal"));
        assert_eq!(ep.bearer.as_deref(), Some("lease-abc")); // lease, not a key
        assert_eq!(ep.surface.as_deref(), Some("AnthropicMessages"));
    }

    #[test]
    fn default_grant_is_an_explicit_local_self_credentialed() {
        // No "unspecified" state: the default is a concrete local grant, so a
        // manifest that names no mode reads as "use local config", not None.
        assert_eq!(
            ModelAccessGrant::default(),
            ModelAccessGrant::LocalSelfCredentialed {
                model_ref: None,
                provider_ref: None,
            }
        );
        assert!(!ModelAccessGrant::default().is_gateway());
    }

    #[test]
    fn value_object_materialize_matches_the_shim_and_flags_gateway() {
        // The inherent projection is the single source of truth; the `FieldMaterializer`
        // shim just delegates to it. A local grant yields a None endpoint (fall back to
        // local config); a gateway grant yields the gateway URL + lease bearer.
        let local = ModelAccessGrant::LocalSelfCredentialed {
            model_ref: Some("m".into()),
            provider_ref: None,
        };
        let local_ep = local.materialize();
        assert_eq!(local_ep.base_url, None);
        assert_eq!(local_ep.bearer, None);
        assert!(!local.is_gateway());

        let gateway = ModelAccessGrant::CloudManagedGateway {
            gateway_base_url: "https://gw.internal".into(),
            surface: "AnthropicMessages".into(),
            model_ref: "claude".into(),
            lease_token: "lease-abc".into(), // awaken-allow: secret
        };
        let gw_ep = gateway.materialize();
        assert_eq!(gw_ep.base_url.as_deref(), Some("https://gw.internal"));
        assert_eq!(gw_ep.bearer.as_deref(), Some("lease-abc"));
        assert!(gateway.is_gateway());
    }
}
