use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use awaken_contract::PluginConfigKey;
use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;

use awaken_runtime::agent::state::AddContextMessage;
use awaken_runtime::plugins::{ConfigSchema, Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::MutationBatch;
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};

use super::tool::A2uiRenderTool;
use super::{A2UI_PLUGIN_ID, A2UI_TOOL_ID};

pub const DEFAULT_A2UI_CATALOG_ID: &str =
    "https://a2ui.org/specification/v0_8/standard_catalog_definition.json";

const A2UI_INSTRUCTION_CONTEXT_KEY: &str = "generative_ui.instructions";

/// Prompt customization for the A2UI plugin.
///
/// Stored in `AgentSpec.sections["generative-ui"]` and resolved on each
/// inference step, so a caller can override the catalog hint, append
/// examples, or replace the injected instructions entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct A2uiPromptConfig {
    /// Override the catalog identifier mentioned in the default instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    /// Extra examples appended after the built-in instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub examples: Option<String>,
    /// Full instruction override. When set, `catalog_id` and `examples`
    /// are ignored for prompt construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl A2uiPromptConfig {
    #[must_use]
    pub fn with_catalog_id(mut self, catalog_id: impl Into<String>) -> Self {
        self.catalog_id = Some(catalog_id.into());
        self
    }

    #[must_use]
    pub fn with_examples(mut self, examples: impl Into<String>) -> Self {
        self.examples = Some(examples.into());
        self
    }

    #[must_use]
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.catalog_id.is_none() && self.examples.is_none() && self.instructions.is_none()
    }

    fn overlay(&self, overrides: Self) -> Self {
        Self {
            catalog_id: overrides.catalog_id.or_else(|| self.catalog_id.clone()),
            examples: overrides.examples.or_else(|| self.examples.clone()),
            instructions: overrides.instructions.or_else(|| self.instructions.clone()),
        }
    }

    fn instructions_text(&self) -> String {
        if let Some(instructions) = &self.instructions {
            return instructions.clone();
        }

        build_instructions(
            self.catalog_id
                .as_deref()
                .unwrap_or(DEFAULT_A2UI_CATALOG_ID),
            self.examples.as_deref(),
        )
    }
}

/// [`PluginConfigKey`] binding for A2UI prompt overrides in agent specs.
pub struct A2uiPromptConfigKey;

impl PluginConfigKey for A2uiPromptConfigKey {
    const KEY: &'static str = A2UI_PLUGIN_ID;
    type Config = A2uiPromptConfig;
}

#[derive(Clone)]
pub(crate) struct A2uiInstructionHook {
    defaults: A2uiPromptConfig,
}

impl A2uiInstructionHook {
    pub(crate) fn new(defaults: A2uiPromptConfig) -> Self {
        Self { defaults }
    }

    pub(crate) fn instructions_for(&self, agent_spec: &AgentSpec) -> Result<String, StateError> {
        let overrides = agent_spec.config::<A2uiPromptConfigKey>()?;
        Ok(self.defaults.overlay(overrides).instructions_text())
    }
}

#[async_trait]
impl PhaseHook for A2uiInstructionHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let instructions = self.instructions_for(ctx.agent_spec.as_ref())?;
        if instructions.trim().is_empty() {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        cmd.schedule_action::<AddContextMessage>(ContextMessage::system(
            A2UI_INSTRUCTION_CONTEXT_KEY,
            instructions,
        ))?;
        Ok(cmd)
    }
}

/// A2UI plugin that provides the render tool and prompt instructions.
pub struct A2uiPlugin {
    default_prompt_config: A2uiPromptConfig,
    instructions: String,
}

impl A2uiPlugin {
    /// Create with the default standard v0.8 catalog guidance.
    pub fn new() -> Self {
        Self::with_prompt_config(A2uiPromptConfig::default())
    }

    /// Create with a default prompt configuration that can still be overridden
    /// by the active agent's `A2uiPromptConfigKey` section.
    pub fn with_prompt_config(prompt_config: A2uiPromptConfig) -> Self {
        let instructions = prompt_config.instructions_text();
        Self {
            default_prompt_config: prompt_config,
            instructions,
        }
    }

    /// Create with a specific catalog URI description for prompt guidance.
    pub fn with_catalog_id(catalog_id: &str) -> Self {
        Self::with_prompt_config(A2uiPromptConfig::default().with_catalog_id(catalog_id))
    }

    /// Create with a catalog ID and custom examples.
    pub fn with_catalog_and_examples(catalog_id: &str, examples: &str) -> Self {
        Self::with_prompt_config(
            A2uiPromptConfig::default()
                .with_catalog_id(catalog_id)
                .with_examples(examples),
        )
    }

    /// Create with fully custom instructions.
    pub fn with_custom_instructions(instructions: String) -> Self {
        Self::with_prompt_config(A2uiPromptConfig::default().with_instructions(instructions))
    }

    /// Returns the default prompt config resolved by the plugin when the
    /// active agent does not provide overrides.
    pub fn prompt_config(&self) -> &A2uiPromptConfig {
        &self.default_prompt_config
    }

    /// Returns the default instructions that will be injected when there is no
    /// per-agent override.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }
}

impl Default for A2uiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for A2uiPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: A2UI_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            A2UI_PLUGIN_ID,
            Phase::BeforeInference,
            A2uiInstructionHook::new(self.default_prompt_config.clone()),
        )?;
        registrar.register_tool(A2UI_TOOL_ID, Arc::new(A2uiRenderTool::new()))?;
        Ok(())
    }

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![ConfigSchema::for_key::<A2uiPromptConfigKey>()]
    }

    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        _patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        Ok(())
    }
}

fn build_instructions(catalog_id: &str, examples: Option<&str>) -> String {
    let mut s = String::from(A2UI_SCHEMA_INSTRUCTIONS);
    s = s.replace("{{CATALOG_ID}}", catalog_id);
    if let Some(examples) = examples {
        s.push_str("\n\n---BEGIN A2UI EXAMPLES---\n");
        s.push_str(examples);
        s.push_str("\n---END A2UI EXAMPLES---");
    }
    s
}

const A2UI_SCHEMA_INSTRUCTIONS: &str = r#"
## A2UI Declarative UI

You have access to the `render_a2ui` tool. Pass ONE A2UI message key directly
as the tool argument. Use the official A2UI v0.8 message format and do NOT add
wrapper objects beyond the protocol itself.

Call the tool once per message in this order: surfaceUpdate → dataModelUpdate → beginRendering.

### 1. surfaceUpdate
```json
{"surfaceUpdate": {"surfaceId": "ops_request", "components": [
  {"id": "root", "component": {"Card": {"child": "content"}}},
  {"id": "content", "component": {"Column": {"children": {"explicitList": ["title", "requester", "notes", "priority_label", "priority", "submit_label", "submit_button"]}}}},
  {"id": "title", "component": {"Text": {"usageHint": "h2", "text": {"literalString": "Operations Request"}}}},
  {"id": "requester", "component": {"TextField": {"label": {"literalString": "Requester"}, "text": {"path": "/request/requester"}, "textFieldType": "shortText"}}},
  {"id": "notes", "component": {"TextField": {"label": {"literalString": "Request details"}, "text": {"path": "/request/notes"}, "textFieldType": "longText"}}},
  {"id": "priority_label", "component": {"Text": {"usageHint": "caption", "text": {"literalString": "Priority"}}}},
  {"id": "priority", "component": {"MultipleChoice": {"selections": {"path": "/request/priority"}, "options": [{"label": {"literalString": "Standard"}, "value": "standard"}, {"label": {"literalString": "Urgent"}, "value": "urgent"}], "maxAllowedSelections": 1}}},
  {"id": "submit_label", "component": {"Text": {"text": {"literalString": "Submit request"}}}},
  {"id": "submit_button", "component": {"Button": {"child": "submit_label", "primary": true, "action": {"name": "ops_request.submit", "context": [{"key": "requester", "value": {"path": "/request/requester"}}, {"key": "notes", "value": {"path": "/request/notes"}}, {"key": "priority", "value": {"path": "/request/priority"}}]}}}}
]}}
```

### 2. dataModelUpdate
```json
{"dataModelUpdate": {"surfaceId": "ops_request", "path": "/request", "contents": [
  {"key": "requester", "valueString": ""},
  {"key": "notes", "valueString": ""},
  {"key": "priority", "valueMap": [{"key": "0", "valueString": "standard"}]}
]}}
```

### 3. beginRendering
```json
{"beginRendering": {"surfaceId": "ops_request", "root": "root"}}
```

### Rules
- The client defaults to the standard v0.8 catalog: `{{CATALOG_ID}}`.
- Components are a flat list. Relationships are expressed by component IDs inside nested component props.
- Each component must have `"id"` and `"component"`, where `"component"` is an object like `{"Text": {...}}`.
- `Text` components must always nest copy under the `text` field, for example `{"Text":{"text":{"literalString":"Submit"}}}`.
- For container children, use `{"children": {"explicitList": ["id1", "id2"]}}`.
- For text input binding, v0.8 `TextField` uses the `text` property, not `value`.
- For selection inputs, v0.8 `MultipleChoice` uses `options`, `selections`, and `maxAllowedSelections`.
- Each `MultipleChoice.options` item must look like `{"label":{"literalString":"High"},"value":"high"}`.
- `MultipleChoice` does not have a `label` field. If you need a visible label, render a nearby `Text` component.
- `MultipleChoice.selections` binds to an array path. Initialize defaults with `valueMap`, for example `{"key":"priority","valueMap":[{"key":"0","valueString":"standard"}]}`.
- For `DateTimeInput` defaults, use browser-compatible local strings like `2026-04-10T09:00`. Do not include a trailing `Z` or timezone offset.
- For buttons, use `"action": {"name": "...", "context": [{"key":"field","value":{"path":"/request/field"}}]}`; do not use the v0.9 `event` wrapper.
- `dataModelUpdate.contents` must be an array of `{key, valueString|valueNumber|valueBoolean|valueMap}` entries.
- Call `beginRendering` only after the referenced root component already exists in `surfaceUpdate`.
- Use `deleteSurface` when the workflow is complete or should be dismissed.
- When updating an existing surface after a user interaction, resend the current values for any bound fields that remain visible on the surface. Do not send only the newly changed status fields if the next UI still displays older inputs.
- If the conversation includes an `A2UI action:` payload, treat its `context` object as the authoritative source for the user's current bound field values unless the user explicitly changed them in the new turn.

### Update reminder
If a follow-up state still shows the original form fields, the next `dataModelUpdate` should carry both the retained field values and the new status fields, for example:
```json
{"dataModelUpdate":{"surfaceId":"surface","path":"/request","contents":[
  {"key":"requester","valueString":"Jordan Patel"},
  {"key":"department","valueString":"Operations"},
  {"key":"status","valueString":"submitted"}
]}}
```
"#;
