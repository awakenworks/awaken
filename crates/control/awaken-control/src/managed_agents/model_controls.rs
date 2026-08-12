//! Canonical model-control normalization between public compatibility fields,
//! Awaken extensions, and the neutral executable snapshot.

use awaken_agent_config::ModelSelection;
use awaken_protocol_managed::ManagedAgentError;
use awaken_protocol_managed::types::{AwakenModelExtensions, ModelConfig, ModelEffort, ModelSpeed};
use awaken_runtime_contract::agent_bindings::{
    InferenceGeography, InferenceOptions, InferenceSpeed, ReasoningEffort,
};
use awaken_runtime_contract::resolved::AcpSessionConfiguration;

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

pub(super) fn apply_model_extensions(
    selection: &mut ModelSelection,
    extensions: Option<AwakenModelExtensions>,
) -> Result<(), ManagedAgentError> {
    let Some(AwakenModelExtensions { acp }) = extensions else {
        return Ok(());
    };
    let Some(configuration) = acp else {
        return Ok(());
    };
    selection
        .set_acp_configuration(configuration)
        .map_err(|error| ManagedAgentError::Invalid(error.into()))
}

pub(super) fn model_config(
    model: String,
    inference: InferenceOptions,
    acp: Option<&AcpSessionConfiguration>,
) -> ModelConfig {
    let mut projected = ModelConfig::from_inference(model, inference);
    if let Some(configuration) = acp.filter(|configuration| !configuration.is_empty()) {
        projected
            .x_awaken
            .get_or_insert(AwakenModelExtensions { acp: None })
            .acp = Some(configuration.clone());
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_official_geography_can_enter_from_the_managed_wire() {
        // Cause/effect graph: C1=official omitted/global, C2=official us,
        // C3=other official value, C4=old Provider-placement extension.
        // Effects: E1=canonical optional typed value; E2=reject before authoring.
        //
        // | Rule | official | x_awaken payload      | effect   |
        // | W1   | none/global | absent             | E1(None) |
        // | W2   | us          | absent             | E1(Us)   |
        // | W3   | eu          | absent             | E2       |
        // | W4   | absent      | processing_geo=eu  | E2       |
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

        assert!(
            serde_json::from_value::<AwakenModelExtensions>(serde_json::json!({
                "processing_geo": "eu"
            }))
            .is_err(),
            "W4"
        );
    }
}
