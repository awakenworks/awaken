//! Model catalog — the management-plane config store answering "what models
//! exist and through which provider/endpoint are they reachable" (ADR-0043,
//! ADR-0088 style, agents bucket). Declarative, orthogonal to execution: this
//! crate is *queried* by the resolver, never depended on by the runtime.
//!
//! Aggregates: [`Provider`] (vendor), [`ProtocolEndpoint`] (a wire surface + URL),
//! [`Offering`] (a model reachable on an endpoint). The routing policy aggregate
//! (`InferenceProfile`) lands here in P1; P0 is the flat catalog + invariants.
//!
//! Secret-free by construction (G22): endpoints/offerings carry no credential
//! material — only a credential *reference* is resolved elsewhere.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

macro_rules! id_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            #[must_use]
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_newtype!(
    /// Stable id of a [`Provider`] (vendor namespace).
    ProviderId
);
id_newtype!(
    /// Stable id of a [`ProtocolEndpoint`].
    ProtocolEndpointId
);

/// The wire/model-API dialect a surface speaks. Replaces oversight's `WireFormat`;
/// the credential/model bindings are resolved against this flavor (ADR-0043).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ModelApiCompat {
    /// The `claude` adapter's wire.
    AnthropicMessages,
    /// The `codex`/OpenAI chat wire.
    OpenAiChat,
    /// The Gemini wire.
    Gemini,
}

impl ModelApiCompat {
    /// The adapter kind that speaks this flavor.
    #[must_use]
    pub fn adapter_kind(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenAiChat => "openai",
            Self::Gemini => "gemini",
        }
    }
}

/// A vendor namespace (`anthropic`, `openai`, …).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Provider {
    pub id: ProviderId,
    /// URL-safe vendor slug, unique in the catalog.
    pub slug: String,
    pub display_name: String,
    /// Append-only version bumped on change.
    pub version: i64,
}

/// A concrete protocol surface of a provider: which wire + which URL. Distinct
/// flavors of one provider typically have distinct `base_url`s.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProtocolEndpoint {
    pub id: ProtocolEndpointId,
    pub provider_id: ProviderId,
    pub flavor: ModelApiCompat,
    /// `http(s)` base URL override (proxy / self-hosted / compat endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    pub display_name: String,
    pub version: i64,
}

/// A model reachable on a protocol surface. `model_id` references the catalog's
/// `ModelSpec` (the intrinsic model attributes, owned by `awaken-agent-contract`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Offering {
    /// The protocol-agnostic catalog model id (e.g. `claude-opus-4-8`).
    pub model_id: String,
    pub provider_id: ProviderId,
    pub protocol_endpoint_id: ProtocolEndpointId,
    pub flavor: ModelApiCompat,
    /// Provider-canonical model name sent upstream when it differs from `model_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
}

/// The catalog aggregate: providers, their endpoints, and the offerings across
/// them. The in-memory projection the resolver queries.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderCatalog {
    pub providers: BTreeMap<String, Provider>,
    pub endpoints: BTreeMap<String, ProtocolEndpoint>,
    pub offerings: Vec<Offering>,
}

/// A write/publish-time invariant violation (fail-closed, G22 / ADR-0043).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("endpoint `{0}` references unknown provider `{1}`")]
    EndpointProviderUnknown(String, String),
    #[error("offering `{model}` references unknown endpoint `{endpoint}`")]
    OfferingEndpointUnknown { model: String, endpoint: String },
    #[error("offering `{model}` flavor {offering:?} disagrees with endpoint flavor {endpoint:?}")]
    OfferingFlavorMismatch {
        model: String,
        offering: ModelApiCompat,
        endpoint: ModelApiCompat,
    },
}

impl ProviderCatalog {
    /// Fail-closed reference-integrity check run before a catalog is published:
    /// every endpoint's provider and every offering's endpoint must resolve, and
    /// an offering's flavor must match its endpoint (so `Offering(model) ∩ flavor`
    /// resolution — ADR-0043 — is well-defined).
    pub fn validate(&self) -> Result<(), CatalogError> {
        for ep in self.endpoints.values() {
            if !self.providers.contains_key(ep.provider_id.as_str()) {
                return Err(CatalogError::EndpointProviderUnknown(
                    ep.id.0.clone(),
                    ep.provider_id.0.clone(),
                ));
            }
        }
        for off in &self.offerings {
            let ep = self
                .endpoints
                .get(off.protocol_endpoint_id.as_str())
                .ok_or_else(|| CatalogError::OfferingEndpointUnknown {
                    model: off.model_id.clone(),
                    endpoint: off.protocol_endpoint_id.0.clone(),
                })?;
            if ep.flavor != off.flavor {
                return Err(CatalogError::OfferingFlavorMismatch {
                    model: off.model_id.clone(),
                    offering: off.flavor,
                    endpoint: ep.flavor,
                });
            }
        }
        Ok(())
    }

    /// Resolve a `model_id` to the offering on the endpoint speaking `flavor`
    /// (`Offering(model) ∩ flavor` — the `Derive` endpoint axis of ADR-0043).
    #[must_use]
    pub fn resolve_offering(&self, model_id: &str, flavor: ModelApiCompat) -> Option<&Offering> {
        self.offerings
            .iter()
            .find(|o| o.model_id == model_id && o.flavor == flavor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> Provider {
        Provider {
            id: ProviderId::new(id),
            slug: id.into(),
            display_name: id.into(),
            version: 1,
        }
    }
    fn endpoint(id: &str, provider: &str, flavor: ModelApiCompat) -> ProtocolEndpoint {
        ProtocolEndpoint {
            id: ProtocolEndpointId::new(id),
            provider_id: ProviderId::new(provider),
            flavor,
            base_url: None,
            timeout_secs: 300,
            display_name: id.into(),
            version: 1,
        }
    }

    fn catalog() -> ProviderCatalog {
        let mut c = ProviderCatalog::default();
        c.providers
            .insert("anthropic".into(), provider("anthropic"));
        c.endpoints.insert(
            "ep1".into(),
            endpoint("ep1", "anthropic", ModelApiCompat::AnthropicMessages),
        );
        c.offerings.push(Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        });
        c
    }

    #[test]
    fn valid_catalog_passes() {
        assert!(catalog().validate().is_ok());
    }

    #[test]
    fn resolve_offering_intersects_model_and_flavor() {
        let c = catalog();
        assert!(
            c.resolve_offering("claude-opus-4-8", ModelApiCompat::AnthropicMessages)
                .is_some()
        );
        // Same model, wrong flavor → no offering (the intersection is empty).
        assert!(
            c.resolve_offering("claude-opus-4-8", ModelApiCompat::OpenAiChat)
                .is_none()
        );
    }

    #[test]
    fn dangling_endpoint_provider_fails_closed() {
        let mut c = catalog();
        c.endpoints.insert(
            "bad".into(),
            endpoint("bad", "ghost", ModelApiCompat::OpenAiChat),
        );
        assert!(matches!(
            c.validate(),
            Err(CatalogError::EndpointProviderUnknown(..))
        ));
    }

    #[test]
    fn offering_flavor_must_match_endpoint() {
        let mut c = catalog();
        c.offerings[0].flavor = ModelApiCompat::OpenAiChat;
        assert!(matches!(
            c.validate(),
            Err(CatalogError::OfferingFlavorMismatch { .. })
        ));
    }
}
