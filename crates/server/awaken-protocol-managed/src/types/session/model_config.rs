//! Official Managed model configuration wire shapes and neutral projection.

use awaken_runtime_contract::agent_bindings::InferenceSpeed;
use serde::{Deserialize, Serialize};

/// The resolved `BetaManagedAgentsModelConfig` object. A session/agent's
/// `model` is this object on the wire, never a bare string (the SDK reads
/// `agent.model.id`). The single definition of the model-config shape — the agent
/// registry and session/thread projections all reuse it rather than rebuild it.
/// M4 typed authoring uses the existing SDK speed value without translation.
/// ```
/// use awaken_protocol_managed::types::ModelConfig;
/// use awaken_runtime_contract::agent_bindings::InferenceSpeed;
/// let mut model = ModelConfig::new("published-model");
/// model.speed = Some(InferenceSpeed::Standard);
/// let _: Option<InferenceSpeed> = model.speed;
/// ```
/// M5 the removed protocol mirror has no forwarding export.
/// ```compile_fail
/// use awaken_protocol_managed::types::ModelSpeed;
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<InferenceSpeed>,
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
        use awaken_runtime_contract::agent_bindings::{InferenceGeography, ReasoningEffort};

        Self {
            id: id.into(),
            speed: inference.speed,
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
            InferenceGeography, InferenceOptions, ReasoningEffort,
        };

        let inference_geo = match self.inference_geo {
            None | Some(ModelInferenceGeo::Global) => None,
            Some(ModelInferenceGeo::Us) => Some(InferenceGeography::Us),
        };
        InferenceOptions {
            speed: self.speed,
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
///
/// SDK: inline — `BetaManagedAgentsModelConfig.inference_geo` (an open string in the SDK; closed here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInferenceGeo {
    Global,
    Us,
}

/// Responses use the SDK's tagged effort union (`{type: ...}`).
///
/// SDK: inline — `BetaManagedAgentsModelConfig.effort` (`BetaManagedAgentsEffortLow` … `Max`).
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
///
/// SDK: inline — `BetaManagedAgentsModelConfigParams.effort`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
pub enum ModelEffortInput {
    Level(ModelEffortLevel),
    Tagged(ModelEffort),
}

/// SDK: inline — `BetaManagedAgentsModelConfigParams.effort` (its bare-string arm).
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

/// `BetaManagedAgentsModelConfigParams`.
///
/// Input-only model configuration; nullable option values normalize to absence.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfigParams {
    pub id: String,
    #[serde(default)]
    pub speed: Option<InferenceSpeed>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::agent_bindings::{InferenceGeography, ReasoningEffort};

    #[test]
    fn model_speed_round_trips_the_same_value_without_changing_geography_or_effort() {
        // Cause/effect table: M1 missing/null speed -> None + omitted output;
        // M2 standard/fast -> exact SDK value through params, neutral controls,
        // and response; M3 unknown/non-string -> reject before resolution.
        // Every accepted row crosses speed with tagged High effort and US geo:
        // these retain their original non-identity wire translations. Neither
        // an identity speed mapper nor a second enum participates in the test.
        for (rule, input, expected) in [
            ("M1 omitted", None, None),
            ("M1 null", Some(serde_json::Value::Null), None),
            (
                "M2 standard",
                Some(serde_json::json!("standard")),
                Some(InferenceSpeed::Standard),
            ),
            (
                "M2 fast",
                Some(serde_json::json!("fast")),
                Some(InferenceSpeed::Fast),
            ),
        ] {
            let mut wire = serde_json::json!({
                "id": "published-model", "effort": {"type": "high"}, "inference_geo": "us",
            });
            if let Some(input) = input {
                wire["speed"] = input;
            }
            let params: ModelConfigParams = serde_json::from_value(wire).expect(rule);
            let config = params.into_resolved();
            let typed: Option<InferenceSpeed> = config.speed;
            assert_eq!(typed, expected, "{rule} canonical value");
            let neutral = config.inference_options();
            assert_eq!(neutral.speed, expected, "{rule} frozen controls");
            assert_eq!(neutral.effort, Some(ReasoningEffort::High), "{rule}");
            assert_eq!(
                neutral.inference_geo,
                Some(InferenceGeography::Us),
                "{rule}"
            );
            let restored = ModelConfig::from_inference(config.id.clone(), neutral);
            let actual = serde_json::to_value(&config).unwrap();
            assert_eq!(serde_json::to_value(restored).unwrap(), actual, "{rule}");
            if let Some(expected) = expected {
                assert_eq!(
                    actual["speed"],
                    serde_json::to_value(expected).unwrap(),
                    "{rule}"
                );
            } else {
                assert!(actual.get("speed").is_none(), "{rule}");
            }
        }
        for invalid in [
            serde_json::json!("turbo"),
            serde_json::json!(0),
            serde_json::json!(true),
            serde_json::json!({}),
        ] {
            assert!(
                serde_json::from_value::<ModelConfigParams>(serde_json::json!({
                    "id": "published-model", "speed": invalid,
                }))
                .is_err(),
                "M3"
            );
        }
    }
}
