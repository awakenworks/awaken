//! Canonical model-control normalization between official Managed fields and
//! the neutral executable snapshot.

use awaken_protocol_managed::ManagedAgentError;
use awaken_protocol_managed::types::{ModelConfig, ModelEffort, ModelSpeed};
use awaken_runtime_contract::agent_bindings::{
    InferenceGeography, InferenceOptions, InferenceSpeed, ReasoningEffort,
};

pub(super) fn inference_from_wire(
    speed: Option<ModelSpeed>,
    effort: Option<ModelEffort>,
    inference_geo: Option<String>,
) -> Result<InferenceOptions, ManagedAgentError> {
    Ok(InferenceOptions {
        speed: speed.map(|value| match value {
            ModelSpeed::Standard => InferenceSpeed::Standard,
            ModelSpeed::Fast => InferenceSpeed::Fast,
        }),
        effort: effort.map(|value| match value {
            ModelEffort::Low => ReasoningEffort::Low,
            ModelEffort::Medium => ReasoningEffort::Medium,
            ModelEffort::High => ReasoningEffort::High,
            ModelEffort::Xhigh => ReasoningEffort::Xhigh,
            ModelEffort::Max => ReasoningEffort::Max,
        }),
        inference_geo: match inference_geo.as_deref() {
            None | Some("global") => None,
            Some("us") => Some(InferenceGeography::Us),
            Some(value) => {
                return Err(ManagedAgentError::Invalid(format!(
                    "official inference_geo supports only `us` or `global`, got `{value}`"
                )));
            }
        },
    })
}

pub(super) fn model_config(model: String, inference: InferenceOptions) -> ModelConfig {
    ModelConfig::from_inference(model, inference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_official_geography_can_enter_from_the_managed_wire() {
        // Cause/effect graph: C1=official omitted/global, C2=official us,
        // C3=other official value.
        // Effects: E1=canonical optional typed value; E2=reject before authoring.
        //
        // | Rule | official   | effect   |
        // | W1   | none/global | E1(None) |
        // | W2   | us          | E1(Us)   |
        // | W3   | eu          | E2       |
        for official in [None, Some("global".to_owned())] {
            assert_eq!(
                inference_from_wire(None, None, official)
                    .unwrap()
                    .inference_geo,
                None,
                "W1"
            );
        }
        let us = inference_from_wire(None, None, Some("us".into())).unwrap();
        assert_eq!(us.inference_geo, Some(InferenceGeography::Us), "W2");
        assert!(
            inference_from_wire(None, None, Some("eu".into())).is_err(),
            "W3"
        );
    }
}
