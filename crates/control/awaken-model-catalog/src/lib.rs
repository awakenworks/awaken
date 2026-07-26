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
    /// The OpenAI Responses API wire. This remains distinct from Chat
    /// Completions even though both are served by the OpenAI adapter family.
    OpenAiResponses,
    /// The Gemini wire.
    Gemini,
    /// Gemini on Vertex AI: native Gemini payloads with OAuth Bearer auth and
    /// a project/location endpoint.
    VertexGemini,
}

impl ApiDialect {
    /// Stable wire token carried into immutable runtime publications.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiChat => "open_ai_chat",
            Self::OpenAiResponses => "open_ai_responses",
            Self::Gemini => "gemini",
            Self::VertexGemini => "vertex_gemini",
        }
    }

    /// The adapter kind that speaks this dialect.
    #[must_use]
    pub fn adapter_kind(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenAiChat => "openai",
            Self::OpenAiResponses => "openai",
            Self::Gemini => "gemini",
            Self::VertexGemini => "vertex",
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
    /// Field-level origin. Values stay flat and convenient for runtime consumers;
    /// management surfaces use this map to explain whether each fact was authored,
    /// observed from a provider API, or supplied by Awaken's curated registry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<String, ModelAttributeProvenance>,
}

/// Authentication input a provider connection can request. This describes the
/// authoring UI only; credential material remains owned by the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthMethod {
    ApiKey,
    OAuth,
}

/// Input widget rendered from a provider descriptor. Descriptors are static
/// adapter capabilities, not user configuration and not another model catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProviderFieldKind {
    Secret,
    Text,
    Url,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderConfigurationField {
    pub key: String,
    pub label: String,
    pub kind: ProviderFieldKind,
    pub required: bool,
    pub advanced: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DefaultProtocolEndpoint {
    pub id_suffix: String,
    pub dialect: ApiDialect,
    pub base_url: String,
}

/// One supported provider driver's authoring capabilities. The list returned by
/// [`provider_driver_descriptors`] is the single source used by API clients to
/// render provider cards and forms.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderDriverDescriptor {
    pub provider_kind: String,
    pub display_name: String,
    pub supported_dialects: Vec<ApiDialect>,
    pub auth_methods: Vec<ProviderAuthMethod>,
    pub configuration_fields: Vec<ProviderConfigurationField>,
    pub default_endpoints: Vec<DefaultProtocolEndpoint>,
    pub supports_model_discovery: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

/// Built-in provider-driver descriptors. This is capability metadata only: it
/// never creates a Provider, endpoint, credential, offering, or executable route.
#[must_use]
pub fn provider_driver_descriptors() -> Vec<ProviderDriverDescriptor> {
    let api_key = || ProviderConfigurationField {
        key: "api_key".into(),
        label: "API key".into(),
        kind: ProviderFieldKind::Secret,
        required: true,
        advanced: false,
        placeholder: None,
    };
    let custom_url = || ProviderConfigurationField {
        key: "base_url".into(),
        label: "Custom endpoint".into(),
        kind: ProviderFieldKind::Url,
        required: false,
        advanced: true,
        placeholder: Some("https://…/v1".into()),
    };
    vec![
        ProviderDriverDescriptor {
            provider_kind: "anthropic".into(),
            display_name: "Anthropic".into(),
            supported_dialects: vec![ApiDialect::AnthropicMessages],
            auth_methods: vec![ProviderAuthMethod::ApiKey],
            configuration_fields: vec![api_key(), custom_url()],
            default_endpoints: vec![DefaultProtocolEndpoint {
                id_suffix: "messages".into(),
                dialect: ApiDialect::AnthropicMessages,
                base_url: "https://api.anthropic.com/v1".into(),
            }],
            supports_model_discovery: true,
            documentation_url: Some("https://docs.anthropic.com/en/api/getting-started".into()),
        },
        ProviderDriverDescriptor {
            provider_kind: "openai".into(),
            display_name: "OpenAI".into(),
            supported_dialects: vec![ApiDialect::OpenAiResponses, ApiDialect::OpenAiChat],
            auth_methods: vec![ProviderAuthMethod::ApiKey],
            configuration_fields: vec![api_key(), custom_url()],
            default_endpoints: vec![
                DefaultProtocolEndpoint {
                    id_suffix: "responses".into(),
                    dialect: ApiDialect::OpenAiResponses,
                    base_url: "https://api.openai.com/v1".into(),
                },
                DefaultProtocolEndpoint {
                    id_suffix: "chat".into(),
                    dialect: ApiDialect::OpenAiChat,
                    base_url: "https://api.openai.com/v1".into(),
                },
            ],
            supports_model_discovery: true,
            documentation_url: Some("https://developers.openai.com/api/docs".into()),
        },
        ProviderDriverDescriptor {
            provider_kind: "gemini".into(),
            display_name: "Google AI Studio".into(),
            supported_dialects: vec![ApiDialect::Gemini],
            auth_methods: vec![ProviderAuthMethod::ApiKey],
            configuration_fields: vec![api_key(), custom_url()],
            default_endpoints: vec![DefaultProtocolEndpoint {
                id_suffix: "gemini".into(),
                dialect: ApiDialect::Gemini,
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            }],
            supports_model_discovery: true,
            documentation_url: Some("https://ai.google.dev/gemini-api/docs".into()),
        },
        ProviderDriverDescriptor {
            provider_kind: "vertex".into(),
            display_name: "Vertex AI".into(),
            supported_dialects: vec![ApiDialect::VertexGemini],
            auth_methods: vec![ProviderAuthMethod::OAuth],
            configuration_fields: vec![
                ProviderConfigurationField {
                    key: "project_id".into(),
                    label: "Google Cloud project".into(),
                    kind: ProviderFieldKind::Text,
                    required: true,
                    advanced: false,
                    placeholder: None,
                },
                ProviderConfigurationField {
                    key: "location".into(),
                    label: "Location".into(),
                    kind: ProviderFieldKind::Text,
                    required: true,
                    advanced: false,
                    placeholder: Some("global".into()),
                },
            ],
            default_endpoints: Vec::new(),
            supports_model_discovery: true,
            documentation_url: Some("https://cloud.google.com/vertex-ai/generative-ai/docs".into()),
        },
    ]
}

/// Authority behind one published model-attribute value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ModelAttributeSource {
    Manual,
    ProviderApi,
    Curated,
}

/// Explainability metadata for one field in [`ModelAttributes`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelAttributeProvenance {
    pub source: ModelAttributeSource,
    pub observed_at_unix_ms: u64,
}

impl ModelAttributes {
    /// Stamp every populated field with one trusted application-side source.
    /// Omitted fields and their old provenance disappear because PUT is replace,
    /// keeping an explicit unknown distinct from a stale known value.
    #[must_use]
    pub fn stamped(mut self, source: ModelAttributeSource, observed_at_unix_ms: u64) -> Self {
        self.provenance.clear();
        let provenance = ModelAttributeProvenance {
            source,
            observed_at_unix_ms,
        };
        if self.context_window.is_some() {
            self.provenance
                .insert("context_window".into(), provenance.clone());
        }
        if self.max_output_tokens.is_some() {
            self.provenance
                .insert("max_output_tokens".into(), provenance);
        }
        self
    }

    fn validate(&self, model_id: &str) -> Result<(), CatalogError> {
        if self.context_window == Some(0) {
            return Err(CatalogError::InvalidModelAttributes {
                model: model_id.into(),
                reason: "context_window must be greater than zero".into(),
            });
        }
        if self.max_output_tokens == Some(0) {
            return Err(CatalogError::InvalidModelAttributes {
                model: model_id.into(),
                reason: "max_output_tokens must be greater than zero".into(),
            });
        }
        if matches!(
            (self.context_window, self.max_output_tokens),
            (Some(context), Some(output)) if output > context
        ) {
            return Err(CatalogError::InvalidModelAttributes {
                model: model_id.into(),
                reason: "max_output_tokens cannot exceed context_window".into(),
            });
        }
        for field in self.provenance.keys() {
            let populated = match field.as_str() {
                "context_window" => self.context_window.is_some(),
                "max_output_tokens" => self.max_output_tokens.is_some(),
                _ => false,
            };
            if !populated {
                return Err(CatalogError::InvalidModelAttributes {
                    model: model_id.into(),
                    reason: format!("provenance references unknown or absent field `{field}`"),
                });
            }
        }
        Ok(())
    }
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
    /// Who owns the catalog fact. Manually authored rows are never demoted by a
    /// provider refresh; provider-discovered rows follow the provider's latest
    /// complete listing.
    #[serde(default, skip_serializing_if = "OfferingSource::is_manual")]
    pub source: OfferingSource,
    /// Whether this route may be selected for a new publication. Provider sync is
    /// non-destructive: a missing discovered model becomes unavailable instead of
    /// being deleted, so existing immutable publications remain explainable.
    #[serde(default, skip_serializing_if = "OfferingStatus::is_active")]
    pub status: OfferingStatus,
    /// Unix timestamp (milliseconds) of the most recent complete provider listing
    /// that contained this route. Manual offerings never carry this observation.
    /// When a later complete listing omits the model, the timestamp is retained so
    /// operators can distinguish "last seen then" from "never observed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at_unix_ms: Option<u64>,
}

/// Authority that published an [`Offering`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OfferingSource {
    /// Explicit UI/API authoring is authoritative over provider discovery.
    #[default]
    Manual,
    /// Observed from the configured endpoint's provider API.
    ProviderApi,
}

impl OfferingSource {
    fn is_manual(&self) -> bool {
        *self == Self::Manual
    }
}

/// Admission status for new publications using an offering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OfferingStatus {
    #[default]
    Active,
    Unavailable,
}

impl OfferingStatus {
    fn is_active(&self) -> bool {
        *self == Self::Active
    }
}

/// Provider-neutral model observation returned by a discovery adapter. It carries
/// no provider, endpoint, credential, or authorization decision; the catalog
/// application service supplies those from its already-authored endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiscoveredModel {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
}

/// Durable outcome of reconciling one complete provider model listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CatalogSyncResult {
    pub discovered: usize,
    pub activated: usize,
    pub marked_unavailable: usize,
    /// Observation time shared by every row in this successful atomic refresh.
    pub observed_at_unix_ms: u64,
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
    #[error(
        "offering `{model}` provider `{offering}` disagrees with endpoint provider `{endpoint}`"
    )]
    OfferingProviderMismatch {
        model: String,
        offering: String,
        endpoint: String,
    },
    #[error("provider discovery returned an empty model id")]
    EmptyDiscoveredModelId,
    #[error("provider discovery references unknown endpoint `{0}`")]
    DiscoveryEndpointUnknown(String),
    #[error("model `{model}` has invalid attributes: {reason}")]
    InvalidModelAttributes { model: String, reason: String },
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
            if ep.provider_id != off.provider_id {
                return Err(CatalogError::OfferingProviderMismatch {
                    model: off.model_id.clone(),
                    offering: off.provider_id.0.clone(),
                    endpoint: ep.provider_id.0.clone(),
                });
            }
        }
        for (model_id, attributes) in &self.model_attributes {
            attributes.validate(model_id)?;
        }
        Ok(())
    }

    /// Resolve a `model_id` to the offering on the endpoint speaking `dialect`
    /// (`Offering(model) ∩ dialect` — the `Derive` endpoint axis of ADR-0043).
    #[must_use]
    pub fn resolve_offering(&self, model_id: &str, dialect: ApiDialect) -> Option<&Offering> {
        self.offerings.iter().find(|o| {
            o.status == OfferingStatus::Active && o.model_id == model_id && o.dialect == dialect
        })
    }

    /// Reconcile a complete provider listing into this aggregate. Only rows owned
    /// by provider discovery are changed; explicit authoring wins on the same key.
    /// Missing discovered rows are retained as unavailable rather than deleted.
    pub fn reconcile_discovered_models(
        &mut self,
        endpoint_id: &ProtocolEndpointId,
        models: Vec<DiscoveredModel>,
        observed_at_unix_ms: u64,
    ) -> Result<CatalogSyncResult, CatalogError> {
        let endpoint = self
            .endpoints
            .get(endpoint_id.as_str())
            .cloned()
            .ok_or_else(|| CatalogError::DiscoveryEndpointUnknown(endpoint_id.0.clone()))?;
        let mut normalized = BTreeMap::new();
        for model in models {
            let model_id = model.model_id.trim();
            if model_id.is_empty() {
                return Err(CatalogError::EmptyDiscoveredModelId);
            }
            normalized.insert(model_id.to_string(), model.upstream_model);
        }

        let mut result = CatalogSyncResult {
            discovered: normalized.len(),
            observed_at_unix_ms,
            ..CatalogSyncResult::default()
        };
        for offering in self.offerings.iter_mut().filter(|offering| {
            offering.protocol_endpoint_id == *endpoint_id
                && offering.source == OfferingSource::ProviderApi
        }) {
            if let Some(upstream_model) = normalized.remove(&offering.model_id) {
                if offering.status != OfferingStatus::Active {
                    result.activated += 1;
                }
                offering.status = OfferingStatus::Active;
                offering.upstream_model = upstream_model;
                offering.last_seen_at_unix_ms = Some(observed_at_unix_ms);
            } else if offering.status != OfferingStatus::Unavailable {
                offering.status = OfferingStatus::Unavailable;
                result.marked_unavailable += 1;
            }
        }

        for (model_id, upstream_model) in normalized {
            if self.offerings.iter().any(|offering| {
                offering.model_id == model_id
                    && offering.protocol_endpoint_id == *endpoint_id
                    && offering.source == OfferingSource::Manual
            }) {
                continue;
            }
            self.offerings.push(Offering {
                model_id,
                provider_id: endpoint.provider_id.clone(),
                protocol_endpoint_id: endpoint.id.clone(),
                dialect: endpoint.dialect,
                upstream_model,
                source: OfferingSource::ProviderApi,
                status: OfferingStatus::Active,
                last_seen_at_unix_ms: Some(observed_at_unix_ms),
            });
            result.activated += 1;
        }
        self.offerings.sort_by(|left, right| {
            (left.model_id.as_str(), left.protocol_endpoint_id.as_str())
                .cmp(&(right.model_id.as_str(), right.protocol_endpoint_id.as_str()))
        });
        self.validate()?;
        Ok(result)
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
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
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
                provenance: Default::default(),
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
    fn populated_attributes_are_stamped_per_field_and_unknown_stays_unknown() {
        let stamped = ModelAttributes {
            context_window: Some(200_000),
            max_output_tokens: Some(32_000),
            provenance: BTreeMap::from([(
                "forged".into(),
                ModelAttributeProvenance {
                    source: ModelAttributeSource::ProviderApi,
                    observed_at_unix_ms: 1,
                },
            )]),
        }
        .stamped(ModelAttributeSource::Manual, 42);
        assert_eq!(
            stamped
                .provenance
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["context_window", "max_output_tokens"]
        );
        assert!(stamped.provenance.values().all(|item| {
            item.source == ModelAttributeSource::Manual && item.observed_at_unix_ms == 42
        }));

        let unknown = ModelAttributes::default().stamped(ModelAttributeSource::Curated, 43);
        assert!(unknown.provenance.is_empty());

        for (attributes, expected_field) in [
            (
                ModelAttributes {
                    context_window: Some(128_000),
                    ..Default::default()
                },
                "context_window",
            ),
            (
                ModelAttributes {
                    max_output_tokens: Some(8_192),
                    ..Default::default()
                },
                "max_output_tokens",
            ),
        ] {
            let stamped = attributes.stamped(ModelAttributeSource::ProviderApi, 44);
            assert_eq!(stamped.provenance.len(), 1);
            assert_eq!(
                stamped.provenance[expected_field].source,
                ModelAttributeSource::ProviderApi
            );
        }
    }

    #[test]
    fn legacy_model_attributes_without_provenance_remain_readable() {
        let attrs: ModelAttributes = serde_json::from_str(r#"{"context_window":128000}"#).unwrap();
        assert_eq!(attrs.context_window, Some(128_000));
        assert!(attrs.provenance.is_empty());
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
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
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
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
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
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
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
        assert_eq!(ApiDialect::OpenAiResponses.adapter_kind(), "openai");
        assert_eq!(ApiDialect::Gemini.adapter_kind(), "gemini");
        assert_eq!(ApiDialect::VertexGemini.adapter_kind(), "vertex");
    }

    #[test]
    fn provider_descriptors_are_unique_and_internally_consistent() {
        let descriptors = provider_driver_descriptors();
        assert!(!descriptors.is_empty());
        let mut kinds = std::collections::BTreeSet::new();
        for descriptor in &descriptors {
            assert!(kinds.insert(descriptor.provider_kind.as_str()));
            assert!(!descriptor.supported_dialects.is_empty());
            assert!(!descriptor.auth_methods.is_empty());
            let mut fields = std::collections::BTreeSet::new();
            for field in &descriptor.configuration_fields {
                assert!(fields.insert(field.key.as_str()));
            }
            for endpoint in &descriptor.default_endpoints {
                assert!(descriptor.supported_dialects.contains(&endpoint.dialect));
                assert!(endpoint.base_url.starts_with("https://"));
            }
        }
    }

    #[test]
    fn openai_descriptor_prefers_responses_without_hiding_chat() {
        let openai = provider_driver_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.provider_kind == "openai")
            .unwrap();
        assert_eq!(openai.supported_dialects[0], ApiDialect::OpenAiResponses);
        assert!(openai.supported_dialects.contains(&ApiDialect::OpenAiChat));
        assert_eq!(
            openai.default_endpoints[0].base_url,
            "https://api.openai.com/v1"
        );
        assert!(openai.supports_model_discovery);
    }

    #[test]
    fn api_dialect_serde_is_snake_case_on_the_wire() {
        // The wire tokens the console/config API round-trips — `rename_all = snake_case`.
        for (dialect, wire) in [
            (ApiDialect::AnthropicMessages, "\"anthropic_messages\""),
            (ApiDialect::OpenAiChat, "\"open_ai_chat\""),
            (ApiDialect::OpenAiResponses, "\"open_ai_responses\""),
            (ApiDialect::Gemini, "\"gemini\""),
            (ApiDialect::VertexGemini, "\"vertex_gemini\""),
        ] {
            assert_eq!(serde_json::to_string(&dialect).unwrap(), wire);
            assert_eq!(dialect.as_str(), wire.trim_matches('"'));
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
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
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
                provenance: Default::default(),
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
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        });
        assert!(c.validate().is_ok());
        let got = c
            .resolve_offering("gemini-2.5-pro", ApiDialect::Gemini)
            .expect("gemini offering resolves");
        assert_eq!(got.upstream_model.as_deref(), Some("models/gemini-2.5-pro"));
    }
}
