//! Serializable model offering: addressing, intrinsic capabilities, pricing.
//!
//! Carved out of `registry_spec/mod.rs` so the file stays under the
//! repository's per-file line cap. Public types are re-exported from
//! `registry_spec` so import paths remain unchanged.

use serde::{Deserialize, Serialize};

/// Input/output modality supported by a model.
///
/// Closed set covering the modalities present in major provider APIs
/// (Anthropic, OpenAI, Google Gemini, Vertex) as of the 2026-Q1
/// reference window: text, images, audio, video, and PDF documents.
/// Adding a variant is a breaking change for exhaustive `match` consumers;
/// removing one is a breaking serde change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
}

/// Set of modalities a model accepts on input and produces on output.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Modalities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<Modality>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<Modality>,
}

impl Modalities {
    /// True when both `input` and `output` lists are empty. Used by serde's
    /// `skip_serializing_if` so a defaulted `Modalities` is elided rather
    /// than emitted as `{"input":[],"output":[]}` — keeping minimal
    /// `ModelSpec` JSON free of empty containers.
    pub(crate) fn is_empty(&self) -> bool {
        self.input.is_empty() && self.output.is_empty()
    }
}

/// Serializable model offering: addressing (id, provider, upstream model),
/// intrinsic capabilities (context window, max output tokens, modalities,
/// knowledge cutoff), and per-million-token pricing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    /// Stable id used by `AgentSpec.model_id`. Unique within a registry.
    pub id: String,
    /// Provider this offering routes through.
    pub provider_id: String,
    /// Model name sent to the upstream API.
    pub upstream_model: String,

    /// Maximum context window in tokens, when published by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Hard ceiling on a single response's output tokens, when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Input/output modalities supported by the model.
    ///
    /// **Semantics:** an empty `Modalities` (or `default()`) means the model's
    /// modality set is unspecified — runtime treats it as catalog metadata only
    /// and performs no modality enforcement. Explicit empty arrays carry the
    /// same meaning as omission. To advertise a text-only model, set
    /// `input: vec![Modality::Text]` explicitly.
    ///
    /// Note: this PR uses modalities for catalog/UI purposes; runtime request
    /// gating against `input` is intentionally NOT implemented (would belong
    /// in a separate request-validation pass).
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub modalities: Modalities,
    /// ISO date string (e.g. "2026-01") for the model's training cutoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<String>,

    /// Optional input-token price in USD per million tokens. When paired
    /// with `output_token_price_per_million_usd`, eval runs populate
    /// `ReplayReport.cost_usd` so cost surfaces in regression diffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_token_price_per_million_usd: Option<f64>,
    /// Optional output-token price in USD per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_token_price_per_million_usd: Option<f64>,
}

impl ModelSpec {
    /// Convenience constructor for tests and bootstrap code. Capability
    /// and pricing fields default to `None` / empty.
    pub fn new(
        id: impl Into<String>,
        provider_id: impl Into<String>,
        upstream_model: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            provider_id: provider_id.into(),
            upstream_model: upstream_model.into(),
            context_window: None,
            max_output_tokens: None,
            modalities: Modalities::default(),
            knowledge_cutoff: None,
            input_token_price_per_million_usd: None,
            output_token_price_per_million_usd: None,
        }
    }

    /// USD cost from per-million pricing. Returns `None` unless **both**
    /// input and output prices are set — partial pricing would silently
    /// under-report cost.
    pub fn compute_cost_usd(&self, input_tokens: u32, output_tokens: u32) -> Option<f64> {
        let ip = self.input_token_price_per_million_usd?;
        let op = self.output_token_price_per_million_usd?;
        Some(
            f64::from(input_tokens) * ip / 1_000_000.0
                + f64::from(output_tokens) * op / 1_000_000.0,
        )
    }
}
