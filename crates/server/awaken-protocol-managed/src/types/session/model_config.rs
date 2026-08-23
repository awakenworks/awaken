//! Official Managed model configuration wire shapes and neutral projection.

use serde::{Deserialize, Serialize};

/// The resolved `BetaManagedAgentsModelConfig` object. A session/agent's
/// `model` is this object on the wire, never a bare string (the SDK reads
/// `agent.model.id`). The single definition of the model-config shape — the agent
/// registry and session/thread projections all reuse it rather than rebuild it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<ModelSpeed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ModelEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<ModelInferenceGeo>,
}

impl ModelConfig {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            speed: None,
            effort: None,
            inference_geo: None,
        }
    }

    /// Canonical neutral-to-Managed inference projection. Agent reads and
    /// Session inheritance share this owner so a new immutable control cannot
    /// drift between the two wire paths.
    pub fn from_inference(
        id: impl Into<String>,
        inference: awaken_runtime_contract::agent_bindings::InferenceOptions,
    ) -> Self {
        use awaken_runtime_contract::agent_bindings::{
            InferenceGeography, InferenceSpeed, ReasoningEffort,
        };

        Self {
            id: id.into(),
            speed: inference.speed.map(|value| match value {
                InferenceSpeed::Standard => ModelSpeed::Standard,
                InferenceSpeed::Fast => ModelSpeed::Fast,
            }),
            effort: inference.effort.map(|value| match value {
                ReasoningEffort::Low => ModelEffort::Low,
                ReasoningEffort::Medium => ModelEffort::Medium,
                ReasoningEffort::High => ModelEffort::High,
                ReasoningEffort::Xhigh => ModelEffort::Xhigh,
                ReasoningEffort::Max => ModelEffort::Max,
            }),
            inference_geo: (inference.inference_geo == Some(InferenceGeography::Us))
                .then_some(ModelInferenceGeo::Us),
        }
    }

    /// Lower official Managed inference controls into the one neutral runtime
    /// contract. `global` is the absence of an extra geography restriction.
    #[must_use]
    pub fn inference_options(&self) -> awaken_runtime_contract::agent_bindings::InferenceOptions {
        use awaken_runtime_contract::agent_bindings::{
            InferenceGeography, InferenceOptions, InferenceSpeed, ReasoningEffort,
        };

        let inference_geo = match self.inference_geo {
            None | Some(ModelInferenceGeo::Global) => None,
            Some(ModelInferenceGeo::Us) => Some(InferenceGeography::Us),
        };
        InferenceOptions {
            speed: self.speed.map(|speed| match speed {
                ModelSpeed::Standard => InferenceSpeed::Standard,
                ModelSpeed::Fast => InferenceSpeed::Fast,
            }),
            effort: self.effort.map(|effort| match effort {
                ModelEffort::Low => ReasoningEffort::Low,
                ModelEffort::Medium => ReasoningEffort::Medium,
                ModelEffort::High => ReasoningEffort::High,
                ModelEffort::Xhigh => ReasoningEffort::Xhigh,
                ModelEffort::Max => ReasoningEffort::Max,
            }),
            inference_geo,
        }
    }
}

/// The SDK's closed inference-geography vocabulary. Unsupported geography
/// strings fail at the serde boundary instead of surviving as partially
/// validated configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInferenceGeo {
    Global,
    Us,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSpeed {
    Standard,
    Fast,
}

/// Responses use the SDK's tagged effort union (`{type: ...}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Create/update accepts either a bare effort level or the tagged response form.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
pub enum ModelEffortInput {
    Level(ModelEffortLevel),
    Tagged(ModelEffort),
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelEffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ModelEffortInput {
    #[must_use]
    pub fn resolved(self) -> ModelEffort {
        match self {
            Self::Level(ModelEffortLevel::Low) | Self::Tagged(ModelEffort::Low) => ModelEffort::Low,
            Self::Level(ModelEffortLevel::Medium) | Self::Tagged(ModelEffort::Medium) => {
                ModelEffort::Medium
            }
            Self::Level(ModelEffortLevel::High) | Self::Tagged(ModelEffort::High) => {
                ModelEffort::High
            }
            Self::Level(ModelEffortLevel::Xhigh) | Self::Tagged(ModelEffort::Xhigh) => {
                ModelEffort::Xhigh
            }
            Self::Level(ModelEffortLevel::Max) | Self::Tagged(ModelEffort::Max) => ModelEffort::Max,
        }
    }
}

/// Input-only model configuration; nullable option values normalize to absence.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfigParams {
    pub id: String,
    #[serde(default)]
    pub speed: Option<ModelSpeed>,
    #[serde(default)]
    pub effort: Option<ModelEffortInput>,
    #[serde(default)]
    pub inference_geo: Option<ModelInferenceGeo>,
}

impl ModelConfigParams {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            speed: None,
            effort: None,
            inference_geo: None,
        }
    }

    #[must_use]
    pub fn into_resolved(self) -> ModelConfig {
        ModelConfig {
            id: self.id,
            speed: self.speed,
            effort: self.effort.map(ModelEffortInput::resolved),
            inference_geo: self.inference_geo,
        }
    }

    /// Resolve a per-Session model override.
    ///
    /// Managed Agents treats the override as a complete model replacement, but
    /// deliberately does not apply an `effort` value carried by that override:
    /// the selected model runs at its default effort. Keep accepting the field
    /// at the wire boundary for old and forward SDK compatibility while making
    /// the execution projection match the service contract. Agent authoring
    /// continues to use [`Self::into_resolved`], where effort is meaningful.
    #[must_use]
    pub fn into_session_override(self) -> ModelConfig {
        ModelConfig {
            id: self.id,
            speed: self.speed,
            effort: None,
            inference_geo: self.inference_geo,
        }
    }
}
