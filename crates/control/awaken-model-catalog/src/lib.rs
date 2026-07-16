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

#[cfg(feature = "postgres")]
pub mod postgres;
pub mod repo;
pub mod schema;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::PostgresCatalogRepo;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteCatalogRepo;

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
/// the credential/model bindings are resolved against this dialect (ADR-0043).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApiDialect {
    /// The `claude` adapter's wire.
    AnthropicMessages,
    /// The `codex`/OpenAI chat wire.
    OpenAiChat,
    /// The Gemini wire.
    Gemini,
}

impl ApiDialect {
    /// The adapter kind that speaks this dialect.
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
    pub dialect: ApiDialect,
    /// `http(s)` base URL override (proxy / self-hosted / compat endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    pub display_name: String,
    pub version: i64,
}

/// The published intrinsic attributes of a catalog model, keyed by `model_id`. This is
/// the control plane's OWN projection of what a console publishes — deliberately not the
/// agent-domain `ModelSpec` (a runtime type the control plane must not depend on, per the
/// dependency-direction ban); it carries only what the catalog's consumers read. Extended
/// as the console publishes more attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelAttributes {
    /// Max context window in tokens — the single budget both the ACP CLIs' auto-compact
    /// window and the native compaction ext derive from. Absent → the consumer falls back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Max output tokens the model emits, when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// A model reachable on a protocol surface. `model_id` references the catalog's
/// [`ModelAttributes`] (the intrinsic model attributes the control plane publishes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Offering {
    /// The protocol-agnostic catalog model id (e.g. `claude-opus-4-8`).
    pub model_id: String,
    pub provider_id: ProviderId,
    pub protocol_endpoint_id: ProtocolEndpointId,
    pub dialect: ApiDialect,
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
    /// The published attributes of the models the offerings reference, keyed by
    /// `model_id`. Absent for a model whose attributes aren't published (the consumer
    /// then falls back — e.g. compaction stays message-count based).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_attributes: BTreeMap<String, ModelAttributes>,
}

impl ProviderCatalog {
    /// The published context window (max tokens) of `model_id`, when the catalog carries
    /// its [`ModelAttributes`]. This is the single source of truth both the ACP CLIs'
    /// auto-compact window and the native compaction ext derive their token budget from.
    #[must_use]
    pub fn context_window(&self, model_id: &str) -> Option<u32> {
        self.model_attributes
            .get(model_id)
            .and_then(|a| a.context_window)
    }

    /// A model's published output-token ceiling from its [`ModelAttributes`] — the headroom
    /// the compaction window reserves so input + output stays within `context_window`.
    #[must_use]
    pub fn max_output_tokens(&self, model_id: &str) -> Option<u32> {
        self.model_attributes
            .get(model_id)
            .and_then(|a| a.max_output_tokens)
    }
}

/// A write/publish-time invariant violation (fail-closed, G22 / ADR-0043).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("endpoint `{0}` references unknown provider `{1}`")]
    EndpointProviderUnknown(String, String),
    #[error("offering `{model}` references unknown endpoint `{endpoint}`")]
    OfferingEndpointUnknown { model: String, endpoint: String },
    #[error("offering `{model}` dialect {offering:?} disagrees with endpoint dialect {endpoint:?}")]
    OfferingDialectMismatch {
        model: String,
        offering: ApiDialect,
        endpoint: ApiDialect,
    },
    /// Not an invariant: a durable-backend failure (I/O, serde, poisoned lock)
    /// surfaced by a persistent [`repo::CatalogRepo`] such as the sqlite one. It
    /// lives here rather than on [`repo::RepoError`] so downstream exhaustive
    /// matches on `RepoError` (admin config API) stay valid; it reaches callers
    /// as `RepoError::Invariant(CatalogError::Storage(_))` and is still
    /// fail-closed. [`ProviderCatalog::validate`] never returns it.
    #[error("catalog storage: {0}")]
    Storage(String),
}

impl ProviderCatalog {
    /// Fail-closed reference-integrity check run before a catalog is published:
    /// every endpoint's provider and every offering's endpoint must resolve, and
    /// an offering's dialect must match its endpoint (so `Offering(model) ∩ dialect`
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
            if ep.dialect != off.dialect {
                return Err(CatalogError::OfferingDialectMismatch {
                    model: off.model_id.clone(),
                    offering: off.dialect,
                    endpoint: ep.dialect,
                });
            }
        }
        Ok(())
    }

    /// Resolve a `model_id` to the offering on the endpoint speaking `dialect`
    /// (`Offering(model) ∩ dialect` — the `Derive` endpoint axis of ADR-0043).
    #[must_use]
    pub fn resolve_offering(&self, model_id: &str, dialect: ApiDialect) -> Option<&Offering> {
        self.offerings
            .iter()
            .find(|o| o.model_id == model_id && o.dialect == dialect)
    }
}

/// A provider catalog whose reference integrity has been checked. The ONLY way
/// to obtain one is `parse`, so any `ValidCatalog` in hand is guaranteed valid —
/// the integrity check lives at this single construction boundary, not at every read.
///
/// This makes the illegal state (a stored catalog with a dangling reference)
/// unrepresentable past construction: write paths store a `ValidCatalog` and read
/// paths hand back its inner without re-validating; a durable backend funnels its
/// load-time integrity guard (against corrupt rows) through the same `parse`.
#[derive(Debug, Clone)]
pub struct ValidCatalog(ProviderCatalog);

impl ValidCatalog {
    /// Check reference integrity and, on success, seal the catalog behind the type.
    /// The single construction boundary — the only place the invariant is enforced.
    pub fn parse(cat: ProviderCatalog) -> Result<Self, CatalogError> {
        cat.validate()?;
        Ok(Self(cat))
    }

    /// Borrow the checked catalog (read path — no re-validation needed).
    #[must_use]
    pub fn get(&self) -> &ProviderCatalog {
        &self.0
    }

    /// Unwrap the checked catalog by value.
    #[must_use]
    pub fn into_inner(self) -> ProviderCatalog {
        self.0
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
    fn endpoint(id: &str, provider: &str, dialect: ApiDialect) -> ProtocolEndpoint {
        ProtocolEndpoint {
            id: ProtocolEndpointId::new(id),
            provider_id: ProviderId::new(provider),
            dialect,
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
            endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages),
        );
        c.offerings.push(Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
        });
        c
    }

    #[test]
    fn valid_catalog_passes() {
        assert!(catalog().validate().is_ok());
    }

    #[test]
    fn context_window_reads_the_model_attributes_or_falls_back_to_none() {
        let mut c = catalog();
        c.model_attributes.insert(
            "claude-opus-4-8".into(),
            ModelAttributes {
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
            },
        );
        // Published window is returned…
        assert_eq!(c.context_window("claude-opus-4-8"), Some(200_000));
        // …an unknown model, or one with no published window, is None (the consumer
        // then falls back — compaction stays message-count based).
        assert_eq!(c.context_window("no-such-model"), None);
        c.model_attributes
            .insert("bare".into(), ModelAttributes::default());
        assert_eq!(c.context_window("bare"), None);
    }

    #[test]
    fn resolve_offering_intersects_model_and_flavor() {
        let c = catalog();
        assert!(
            c.resolve_offering("claude-opus-4-8", ApiDialect::AnthropicMessages)
                .is_some()
        );
        // Same model, wrong dialect → no offering (the intersection is empty).
        assert!(
            c.resolve_offering("claude-opus-4-8", ApiDialect::OpenAiChat)
                .is_none()
        );
    }

    #[test]
    fn resolve_offering_returns_first_match_and_none_for_unknown() {
        let mut c = catalog();
        // A second offering for the same model+dialect on a second endpoint. The
        // first-inserted offering (ep1) must win the intersection ("Some 首个").
        c.endpoints.insert(
            "ep2".into(),
            endpoint("ep2", "anthropic", ApiDialect::AnthropicMessages),
        );
        c.offerings.push(Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep2"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
        });
        let got = c
            .resolve_offering("claude-opus-4-8", ApiDialect::AnthropicMessages)
            .expect("first matching offering");
        assert_eq!(got.protocol_endpoint_id.as_str(), "ep1");
        // An unknown model has no offering at all (empty intersection).
        assert!(
            c.resolve_offering("ghost-model", ApiDialect::AnthropicMessages)
                .is_none()
        );
    }

    #[test]
    fn dangling_offering_endpoint_fails_closed() {
        let mut c = catalog();
        c.offerings.push(Offering {
            model_id: "orphan".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ghost"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
        });
        assert!(matches!(
            c.validate(),
            Err(CatalogError::OfferingEndpointUnknown { model, endpoint })
                if model == "orphan" && endpoint == "ghost"
        ));
    }

    #[test]
    fn valid_catalog_parse_rejects_a_dangling_offering() {
        // The read-path integrity guard, now testable at its single construction
        // boundary: a catalog carrying an offering that references an unknown
        // endpoint cannot be sealed into a `ValidCatalog`.
        let mut c = catalog();
        c.offerings.push(Offering {
            model_id: "orphan".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ghost"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
        });
        assert!(matches!(
            ValidCatalog::parse(c),
            Err(CatalogError::OfferingEndpointUnknown { model, endpoint })
                if model == "orphan" && endpoint == "ghost"
        ));
    }

    #[test]
    fn valid_catalog_parse_seals_a_sound_catalog() {
        let sealed = ValidCatalog::parse(catalog()).expect("sound catalog parses");
        assert_eq!(sealed.get().offerings.len(), 1);
        assert_eq!(sealed.into_inner().providers.len(), 1);
    }

    #[test]
    fn dangling_endpoint_provider_fails_closed() {
        let mut c = catalog();
        c.endpoints.insert(
            "bad".into(),
            endpoint("bad", "ghost", ApiDialect::OpenAiChat),
        );
        assert!(matches!(
            c.validate(),
            Err(CatalogError::EndpointProviderUnknown(..))
        ));
    }

    #[test]
    fn offering_flavor_must_match_endpoint() {
        let mut c = catalog();
        c.offerings[0].dialect = ApiDialect::OpenAiChat;
        assert!(matches!(
            c.validate(),
            Err(CatalogError::OfferingDialectMismatch { .. })
        ));
    }

    #[test]
    fn api_dialect_adapter_kind_maps_each_variant() {
        assert_eq!(ApiDialect::AnthropicMessages.adapter_kind(), "anthropic");
        assert_eq!(ApiDialect::OpenAiChat.adapter_kind(), "openai");
        assert_eq!(ApiDialect::Gemini.adapter_kind(), "gemini");
    }

    #[test]
    fn api_dialect_serde_is_snake_case_on_the_wire() {
        // The wire tokens the console/config API round-trips — `rename_all = snake_case`.
        for (dialect, wire) in [
            (ApiDialect::AnthropicMessages, "\"anthropic_messages\""),
            (ApiDialect::OpenAiChat, "\"open_ai_chat\""),
            (ApiDialect::Gemini, "\"gemini\""),
        ] {
            assert_eq!(serde_json::to_string(&dialect).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ApiDialect>(wire).unwrap(), dialect);
        }
        // An unknown dialect token is rejected (fail-closed), not silently defaulted.
        assert!(serde_json::from_str::<ApiDialect>("\"cohere\"").is_err());
    }

    #[test]
    fn id_newtype_accessors_display_and_transparent_serde() {
        let id = ProviderId::new("anthropic");
        assert_eq!(id.as_str(), "anthropic");
        assert_eq!(id.to_string(), "anthropic");
        // `serde(transparent)` ⇒ the id is a bare string on the wire, not `{"0": …}`.
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"anthropic\"");
        assert_eq!(
            serde_json::from_str::<ProtocolEndpointId>("\"ep1\"").unwrap(),
            ProtocolEndpointId::new("ep1")
        );
    }

    #[test]
    fn catalog_error_display_messages_are_stable() {
        assert_eq!(
            CatalogError::EndpointProviderUnknown("ep1".into(), "ghost".into()).to_string(),
            "endpoint `ep1` references unknown provider `ghost`"
        );
        assert_eq!(
            CatalogError::OfferingEndpointUnknown {
                model: "m".into(),
                endpoint: "ghost".into(),
            }
            .to_string(),
            "offering `m` references unknown endpoint `ghost`"
        );
        assert_eq!(
            CatalogError::OfferingDialectMismatch {
                model: "m".into(),
                offering: ApiDialect::OpenAiChat,
                endpoint: ApiDialect::AnthropicMessages,
            }
            .to_string(),
            "offering `m` dialect OpenAiChat disagrees with endpoint dialect AnthropicMessages"
        );
        assert_eq!(
            CatalogError::Storage("disk full".into()).to_string(),
            "catalog storage: disk full"
        );
    }

    #[test]
    fn optional_fields_are_omitted_when_absent_and_round_trip() {
        // ProtocolEndpoint: absent `base_url` is skipped on the wire; ModelAttributes
        // defaults skip both; Offering skips absent `upstream_model`.
        let ep = endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages);
        let json = serde_json::to_string(&ep).unwrap();
        assert!(
            !json.contains("base_url"),
            "absent base_url must be omitted"
        );
        assert_eq!(serde_json::from_str::<ProtocolEndpoint>(&json).unwrap(), ep);

        let attrs = ModelAttributes::default();
        assert_eq!(serde_json::to_string(&attrs).unwrap(), "{}");

        let off = Offering {
            model_id: "m".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
        };
        assert!(
            !serde_json::to_string(&off)
                .unwrap()
                .contains("upstream_model")
        );
    }

    #[test]
    fn catalog_omits_empty_model_attributes_but_carries_populated_ones() {
        let mut c = catalog();
        // Empty map ⇒ the whole field is skipped (older consumers see no key).
        assert!(
            !serde_json::to_string(&c)
                .unwrap()
                .contains("model_attributes")
        );
        c.model_attributes.insert(
            "claude-opus-4-8".into(),
            ModelAttributes {
                context_window: Some(200_000),
                max_output_tokens: None,
            },
        );
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("model_attributes"));
        // A populated attribute round-trips; the absent max_output_tokens stays absent.
        let back: ProviderCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.context_window("claude-opus-4-8"), Some(200_000));
        assert_eq!(
            back.model_attributes["claude-opus-4-8"].max_output_tokens,
            None
        );
        assert_eq!(back, c);
    }

    #[test]
    fn gemini_offering_and_endpoint_validate_and_resolve() {
        // Exercises the third dialect end-to-end through validate + resolve.
        let mut c = ProviderCatalog::default();
        c.providers.insert("google".into(), provider("google"));
        c.endpoints
            .insert("g1".into(), endpoint("g1", "google", ApiDialect::Gemini));
        c.offerings.push(Offering {
            model_id: "gemini-2.5-pro".into(),
            provider_id: ProviderId::new("google"),
            protocol_endpoint_id: ProtocolEndpointId::new("g1"),
            dialect: ApiDialect::Gemini,
            upstream_model: Some("models/gemini-2.5-pro".into()),
        });
        assert!(c.validate().is_ok());
        let got = c
            .resolve_offering("gemini-2.5-pro", ApiDialect::Gemini)
            .expect("gemini offering resolves");
        assert_eq!(got.upstream_model.as_deref(), Some("models/gemini-2.5-pro"));
    }
}
