use std::collections::BTreeSet;

use super::{
    PresentedToolCatalog, TOOL_SEARCH_ID, ToolDescriptor, ToolDiscoverySettings, ToolExposure,
    ToolExposurePolicy, ToolKind, ToolModelProjection, ToolPresentation, ToolPresentationOverride,
    ToolPromptInjection, ToolSchema, tool_search_descriptor,
};

/// The in-Session MCP server through which a builtin tool is bridged to an SDK
/// that can only address MCP identities.
const AWAKEN_SESSION_MCP_SERVER: &str = "awaken_session";

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

    /// Compile the existing presentation intent to single-hop SDK lookup aliases.
    /// MCP descriptors retain their canonical names and schemas; permissions do too.
    pub fn acp_aliases(&self) -> Result<std::collections::BTreeMap<String, String>, String> {
        if !self.exposure.is_default() || self.discovery != ToolDiscoverySettings::default() {
            return Err("ACP_TOOL_UNSUPPORTED: tool exposure/discovery is Native-only".into());
        }
        let mut aliases = std::collections::BTreeMap::new();
        for (target, value) in &self.overrides {
            if value.description.is_some() || value.exposure.is_some() {
                return Err("ACP_TOOL_UNSUPPORTED: SDK lookup aliases cannot rewrite descriptions or exposure".into());
            }
            let Some(alias) = value.alias.as_ref() else {
                continue;
            };
            if alias.is_empty()
                || alias.len() > 128
                || !alias
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            {
                return Err("ACP_TOOL_UNSUPPORTED: invalid tool alias".into());
            }
            // A builtin target is bridged through the in-Session MCP server; an
            // already-canonical target keeps its own identity. Both spellings
            // come from the one owning constructor.
            let actual = if awaken_agent_contract::McpToolId::claims_namespace(target) {
                target.clone()
            } else {
                awaken_agent_contract::McpToolId::new(AWAKEN_SESSION_MCP_SERVER, target)
                    .map_err(|error| format!("ACP_TOOL_UNSUPPORTED: {error}"))?
                    .as_str()
                    .to_string()
            };
            if aliases.insert(alias.clone(), actual).is_some() {
                return Err("ACP_TOOL_UNSUPPORTED: duplicate tool alias".into());
            }
        }
        if aliases.values().any(|target| aliases.contains_key(target)) {
            return Err(
                "ACP_TOOL_UNSUPPORTED: alias chains and target shadowing are forbidden".into(),
            );
        }
        Ok(aliases)
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

#[cfg(test)]
mod acp_alias_tests {
    use super::*;
    fn presentation(target: &str, alias: &str) -> ToolPresentation {
        ToolPresentation::from_overrides([(
            target.into(),
            ToolPresentationOverride {
                alias: Some(alias.into()),
                description: None,
                exposure: None,
            },
        )])
    }
    #[test]
    fn lookup_alias_decision_table() {
        // Causes: canonical source(local/MCP), alias syntax, duplicate sources,
        // description/exposure/discovery policy. Effects: exact single-hop map or
        // refusal before launch. R1 empty => unchanged; R2 local => reserved MCP;
        // R3 external => identity; R4 malformed/duplicate/shadow => refusal;
        // R5 presentation semantics unsupported => refusal. No arguments are changed.
        assert!(
            ToolPresentation::default()
                .acp_aliases()
                .unwrap()
                .is_empty()
        );
        for (target, actual) in [
            ("bash", "mcp__awaken_session__bash"),
            ("mcp__docs__read", "mcp__docs__read"),
        ] {
            let p = presentation(target, "Bash");
            assert_eq!(p.acp_aliases().unwrap()["Bash"], actual);
            assert_eq!(p.resolve("Bash"), target);
        }
        for alias in ["", "with space", "../Bash", "🛠"] {
            assert!(presentation("bash", alias).acp_aliases().is_err());
        }
        let duplicate = ToolPresentation::from_overrides([
            (
                "bash".into(),
                ToolPresentationOverride {
                    alias: Some("same".into()),
                    description: None,
                    exposure: None,
                },
            ),
            (
                "read".into(),
                ToolPresentationOverride {
                    alias: Some("same".into()),
                    description: None,
                    exposure: None,
                },
            ),
        ]);
        assert!(duplicate.acp_aliases().is_err());
        assert!(
            presentation("bash", "mcp__awaken_session__bash")
                .acp_aliases()
                .is_err()
        );
        for value in [
            ToolPresentationOverride {
                alias: Some("Read".into()),
                description: Some("changed".into()),
                exposure: None,
            },
            ToolPresentationOverride {
                alias: None,
                description: None,
                exposure: Some(ToolExposure::OnDemand),
            },
        ] {
            assert!(
                ToolPresentation::from_overrides([("read".into(), value)])
                    .acp_aliases()
                    .is_err()
            );
        }
    }
}
