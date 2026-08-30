use std::collections::BTreeSet;

use super::{
    PresentedToolCatalog, TOOL_SEARCH_ID, ToolDescriptor, ToolDiscoverySettings, ToolExposure,
    ToolExposurePolicy, ToolKind, ToolModelProjection, ToolPresentation, ToolPresentationOverride,
    ToolPromptInjection, ToolSchema, tool_search_descriptor,
};

impl ToolPresentation {
    /// Build from `(canonical_id, override)` pairs; entirely default entries
    /// (no alias, description, or exposure change) are dropped so an all-default
    /// presentation is [`is_empty`](Self::is_empty) and stays byte-identical.
    pub fn from_overrides(
        overrides: impl IntoIterator<Item = (String, ToolPresentationOverride)>,
    ) -> Self {
        let overrides = overrides
            .into_iter()
            .filter(|(_, value)| {
                value.alias.is_some() || value.description.is_some() || value.exposure.is_some()
            })
            .collect();
        Self {
            overrides,
            exposure: ToolExposurePolicy::default(),
            discovery: ToolDiscoverySettings::default(),
        }
    }

    #[must_use]
    pub fn with_exposure_policy(mut self, policy: ToolExposurePolicy) -> Self {
        self.exposure = policy;
        self
    }

    #[must_use]
    pub fn with_discovery(mut self, settings: ToolDiscoverySettings) -> Self {
        self.discovery = settings;
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty() && self.exposure.is_default()
    }

    /// The canonical ids this presentation overrides (used at compile to validate each
    /// targets a selected tool).
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.overrides.keys().map(String::as_str)
    }

    /// Reverse a model-supplied tool id back to its canonical id (the identity when the
    /// id is not an alias). The single choke every internal consumer routes a tool call
    /// through, so the alias never leaks past the model-facing boundary.
    #[must_use]
    pub fn resolve<'a>(&'a self, model_id: &'a str) -> &'a str {
        self.overrides
            .iter()
            .find(|(_, f)| f.alias.as_deref() == Some(model_id))
            .map_or(model_id, |(canonical, _)| canonical.as_str())
    }

    /// Effective exposure for one canonical id. An exact override wins over the
    /// ordered catalog-wide policy.
    #[must_use]
    pub fn exposure(&self, canonical: &str) -> ToolExposure {
        self.overrides
            .get(canonical)
            .and_then(|value| value.exposure)
            .unwrap_or_else(|| self.exposure.resolve(canonical))
    }

    #[must_use]
    pub fn discovery(&self) -> &ToolDiscoverySettings {
        &self.discovery
    }

    /// Project the complete model-facing view for one Step in one pass: appearance,
    /// Run-scoped reveals, `tool_search`, and request-only guidance.
    #[must_use]
    pub fn model_projection(
        &self,
        descriptors: &[ToolDescriptor],
        is_revealed: impl Fn(&str, &str) -> bool,
    ) -> ToolModelProjection {
        let presented = self.present(descriptors);
        let mut tools = presented.visible;
        let mut discoverable = Vec::new();
        for descriptor in presented.discoverable {
            if is_revealed(self.resolve(&descriptor.id), &descriptor.content_hash()) {
                tools.push(descriptor);
            } else {
                discoverable.push(descriptor);
            }
        }
        if discoverable.is_empty() {
            return ToolModelProjection {
                tools,
                prompt: None,
            };
        }
        tools.push(tool_search_descriptor(&self.discovery));
        let prompt = match &self.discovery.prompt {
            ToolPromptInjection::Disabled => None,
            ToolPromptInjection::Custom { text } => Some(text.clone()),
            ToolPromptInjection::Automatic => {
                let names = discoverable
                    .iter()
                    .map(|descriptor| descriptor.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "Some tool definitions are available on demand to reduce context. Use `{TOOL_SEARCH_ID}` \
                     when a needed capability is not visible; search by capability or exact name \
                     with `select:name`. Returned tools become callable on the next step. \
                     On-demand tool names: {names}."
                ))
            }
        };
        ToolModelProjection { tools, prompt }
    }

    /// Split canonical descriptors into the model face (alias + description applied) and
    /// the on-demand set (withheld until revealed). A descriptor with no override passes
    /// through to the face unchanged.
    #[must_use]
    pub fn present(&self, descriptors: &[ToolDescriptor]) -> PresentedToolCatalog {
        let mut out = PresentedToolCatalog::default();
        let detached_targets = descriptors
            .iter()
            .flat_map(|descriptor| descriptor.detached_targets.iter().cloned())
            .collect::<BTreeSet<_>>();
        for d in descriptors {
            if d.kind == ToolKind::DetachedOnly || detached_targets.contains(&d.id) {
                continue;
            }
            let mut shown = d.clone();
            if !d.detached_targets.is_empty() {
                let variants = d
                    .detached_targets
                    .iter()
                    .filter_map(|target_id| {
                        descriptors
                            .iter()
                            .find(|candidate| candidate.id == *target_id)
                    })
                    .map(|target| {
                        serde_json::json!({
                            "type": "object",
                            "properties": {
                                "tool": {
                                    "const": target.id,
                                    "description": target.description,
                                },
                                "arguments": target.model_parameters(),
                            },
                            "required": ["tool", "arguments"],
                            "additionalProperties": false,
                        })
                    })
                    .collect::<Vec<_>>();
                if !variants.is_empty() {
                    shown.parameters = ToolSchema(serde_json::json!({
                        "type": "object",
                        "oneOf": variants,
                    }));
                }
            }
            if let Some(override_) = self.overrides.get(&d.id) {
                if let Some(alias) = &override_.alias {
                    shown.id = alias.clone();
                }
                if let Some(desc) = &override_.description {
                    shown.description = desc.clone();
                }
            }
            if self.exposure(&d.id) == ToolExposure::OnDemand {
                out.discoverable.push(shown);
            } else {
                out.visible.push(shown);
            }
        }
        out
    }
}
