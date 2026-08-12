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
    inference: &mut InferenceOptions,
    extensions: Option<AwakenModelExtensions>,
) -> Result<(), ManagedAgentError> {
    let Some(AwakenModelExtensions {
        acp,
        processing_geo,
    }) = extensions
    else {
        return Ok(());
    };
    if let Some(processing_geo) = processing_geo {
        if inference
            .inference_geo
            .is_some_and(|official| official != processing_geo)
        {
            return Err(ManagedAgentError::Invalid(
                "model.inference_geo conflicts with model.x_awaken.processing_geo".into(),
            ));
        }
        inference.inference_geo = Some(processing_geo);
    }
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
            .get_or_insert(AwakenModelExtensions {
                acp: None,
                processing_geo: None,
            })
            .acp = Some(configuration.clone());
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_and_extension_geographies_have_one_normalized_snapshot_value() {
        // Cause/effect graph: C1=official omitted/global, C2=official us,
        // C3=other official value, C4=Awaken exact extension, C5=conflict.
        // Effects: E1=canonical optional typed value; E2=reject before authoring.
        //
        // | Rule | official | extension | effect |
        // | W1   | none/global | none   | E1(None) |
        // | W2   | us       | none      | E1(Us) |
        // | W3   | eu       | none      | E2 |
        // | W4   | none     | eu        | E1(Eu) |
        // | W5   | us       | eu        | E2 |
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

        let mut selection = ModelSelection::pinned("provider", "model", "gateway");
        let mut extended = inference_from_wire(None, None, None).unwrap();
        apply_model_extensions(
            &mut selection,
            &mut extended,
            Some(AwakenModelExtensions {
                acp: None,
                processing_geo: Some(InferenceGeography::Eu),
            }),
        )
        .expect("W4");
        assert_eq!(extended.inference_geo, Some(InferenceGeography::Eu), "W4");

        let mut conflict = us;
        assert!(
            apply_model_extensions(
                &mut selection,
                &mut conflict,
                Some(AwakenModelExtensions {
                    acp: None,
                    processing_geo: Some(InferenceGeography::Eu),
                }),
            )
            .is_err(),
            "W5"
        );
    }
}
