//! Authoring validation and projection for the model-facing tool catalog.

use awaken_runtime_contract::resolved::{
    ADVISOR_TOOL_ID, TOOL_SEARCH_ID, ToolDescriptor, ToolPresentation, ToolPresentationOverride,
    ToolPromptInjection, ToolSelector,
};

use crate::config::AgentConfig;

use super::CompileError;

pub(super) fn compile(
    config: &AgentConfig,
    descriptors: &[ToolDescriptor],
) -> Result<ToolPresentation, CompileError> {
    let invalid_override = |reason| CompileError::InvalidToolOverride {
        agent: config.id.clone(),
        reason,
    };
    let alias_of: std::collections::BTreeMap<&str, &str> = config
        .tool_overrides
        .iter()
        .filter_map(|override_| {
            override_
                .alias
                .as_deref()
                .map(|alias| (override_.target.as_str(), alias))
        })
        .collect();

    for override_ in &config.tool_overrides {
        if override_.target == ADVISOR_TOOL_ID {
            return Err(invalid_override(
                "the reserved advisor service tool cannot be aliased, deferred, or rewritten"
                    .into(),
            ));
        }
        if !override_.target.starts_with("mcp__")
            && !descriptors
                .iter()
                .any(|descriptor| descriptor.id == override_.target)
        {
            return Err(invalid_override(format!(
                "target {:?} is not a selected tool",
                override_.target
            )));
        }
        if override_.alias.as_deref() == Some(TOOL_SEARCH_ID) {
            return Err(invalid_override(
                "alias uses the reserved tool_search id".into(),
            ));
        }
    }

    let mut model_ids = std::collections::BTreeSet::new();
    for descriptor in descriptors {
        let model_id = alias_of
            .get(descriptor.id.as_str())
            .copied()
            .unwrap_or(descriptor.id.as_str());
        if !model_ids.insert(model_id) {
            return Err(invalid_override(format!(
                "model-facing tool id {model_id:?} is not unique"
            )));
        }
        if model_id == TOOL_SEARCH_ID {
            return Err(invalid_override(
                "selected tool uses the reserved tool_search id".into(),
            ));
        }
    }

    for rule in &config.tool_exposure.rules {
        let value = match &rule.selector {
            ToolSelector::Exact(value) | ToolSelector::Prefix(value) => value,
        };
        if value.trim().is_empty() {
            return Err(CompileError::InvalidToolExposure {
                agent: config.id.clone(),
                reason: "exact and prefix selectors must not be empty; use the policy default to match all tools"
                    .into(),
            });
        }
    }
    if let ToolPromptInjection::Custom { text } = &config.tool_discovery.prompt
        && text.trim().is_empty()
    {
        return Err(CompileError::InvalidToolDiscovery {
            agent: config.id.clone(),
            reason: "custom prompt must not be empty".into(),
        });
    }

    Ok(
        ToolPresentation::from_overrides(config.tool_overrides.iter().map(|override_| {
            (
                override_.target.clone(),
                ToolPresentationOverride {
                    alias: override_.alias.clone(),
                    description: override_.description.clone(),
                    exposure: override_.exposure,
                },
            )
        }))
        .with_exposure_policy(config.tool_exposure.clone())
        .with_discovery(config.tool_discovery.clone()),
    )
}
