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
    /// modality set is unspecified, so runtime stays permissive. Explicit
    /// empty arrays carry the same meaning as omission. When `input` is
    /// non-empty, runtime rejects requests containing unsupported input
    /// modalities before calling the provider. To advertise a text-only model,
    /// set `input: vec![Modality::Text]` explicitly.
    #[serde(default, skip_serializing_if = "Modalities::is_empty")]
    pub modalities: Modalities,
    /// ISO date string (e.g. "2026-01") for the model's training cutoff.
    ///
    /// Deserialization rejects any value that is not a well-formed `YYYY-MM` or
    /// `YYYY-MM-DD` date. This field is runtime-trusted — it is injected
    /// verbatim into the agent's system context — so an unvalidated string from
    /// config, a tenant, or an external registry would be a prompt-injection
    /// surface. Validating at the deserialization boundary closes it for every
    /// source.
    #[serde(
        default,
        deserialize_with = "deserialize_knowledge_cutoff",
        skip_serializing_if = "Option::is_none"
    )]
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

/// Validate an ISO knowledge-cutoff date (`YYYY-MM` or `YYYY-MM-DD`) and return
/// its trimmed canonical form, or `None` when the value is malformed.
///
/// Shared between `ModelSpec` deserialization (which rejects malformed explicit
/// values) and runtime provider-capability discovery (which drops malformed
/// discovered values). Pure and side-effect free so callers choose how to
/// react to `None`.
#[must_use]
pub fn normalize_knowledge_cutoff(value: &str) -> Option<String> {
    let value = value.trim();
    let bytes = value.as_bytes();
    let valid_shape = match bytes.len() {
        7 => {
            bytes[4] == b'-'
                && bytes[..4].iter().all(u8::is_ascii_digit)
                && bytes[5..].iter().all(u8::is_ascii_digit)
        }
        10 => {
            bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes[..4].iter().all(u8::is_ascii_digit)
                && bytes[5..7].iter().all(u8::is_ascii_digit)
                && bytes[8..].iter().all(u8::is_ascii_digit)
        }
        _ => false,
    };
    if !valid_shape {
        return None;
    }
    let month = value[5..7].parse::<u32>().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if bytes.len() == 10 {
        let year = value[..4].parse::<i32>().ok()?;
        let day = value[8..10].parse::<u32>().ok()?;
        if day < 1 || day > days_in_month(year, month) {
            return None;
        }
    }
    Some(value.to_owned())
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Reject explicit `knowledge_cutoff` values that are not well-formed ISO
/// dates, canonicalizing accepted values to their trimmed form.
fn deserialize_knowledge_cutoff<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(value) => normalize_knowledge_cutoff(&value).map(Some).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "knowledge_cutoff must be an ISO date of the form YYYY-MM or YYYY-MM-DD, got {value:?}"
            ))
        }),
    }
}
