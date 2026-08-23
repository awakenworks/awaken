//! One Session-scoped dynamic-tool projection shared by Resource capabilities.
//!
//! Immutable Agent publications cannot be rewritten when a frozen Session adds
//! Skill or Memory bindings. This adapter is the single convergence point for
//! their model-visible descriptors and executors.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, DynamicTool, IdBound, Plugin, PluginManifest,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;

pub(crate) struct SessionToolPlugin {
    id: String,
    tools: Vec<DynamicTool>,
}

impl SessionToolPlugin {
    pub(crate) fn new(
        id: impl Into<String>,
        descriptors: Vec<ToolDescriptor>,
        executors: Vec<Arc<dyn RawTool>>,
    ) -> Result<Self, String> {
        let id = id.into();
        let executors = executors
            .into_iter()
            .map(|tool| (tool.id().to_string(), tool))
            .collect::<BTreeMap<_, _>>();
        let mut tools = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            let tool = executors.get(&descriptor.id).cloned().ok_or_else(|| {
                format!(
                    "Session descriptor `{}` has no matching runtime executor",
                    descriptor.id
                )
            })?;
            tools.push(
                DynamicTool::try_new(descriptor, tool)
                    .map_err(|error| format!("Invalid Session runtime tool: {error}"))?,
            );
        }
        if tools.len() != executors.len() {
            return Err("Session runtime executor has no matching descriptor".into());
        }
        Ok(Self { id, tools })
    }
}

impl Plugin for SessionToolPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: self.id.clone(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tools: IdBound::Exact(
                    self.tools
                        .iter()
                        .map(|tool| tool.descriptor().id.clone())
                        .collect(),
                ),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new(self.id.clone());
        for tool in &self.tools {
            contributions.register_dynamic_tool(tool.clone());
        }
        contributions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl RawTool for Echo {
        fn id(&self) -> &str {
            "echo"
        }

        async fn invoke(
            &self,
            call: awaken_runtime_contract::tool::ToolCall,
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
            Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                call.call_id,
                "ok",
            ))
        }
    }

    #[test]
    fn descriptor_executor_sets_must_match_exactly() {
        // Cause/effect decision table: R1 exact descriptor/executor ids -> one
        // valid plugin; R2 missing executor -> reject; R3 extra executor ->
        // reject. This prevents Skill and Memory projections from drifting into
        // a model-visible-only or executable-only parallel surface.
        let descriptor = ToolDescriptor::pinned(
            "test",
            "echo",
            "echo",
            serde_json::json!({"type": "object"}),
        );
        assert!(
            SessionToolPlugin::new("test", vec![descriptor.clone()], vec![Arc::new(Echo)]).is_ok(),
            "R1"
        );
        assert!(
            SessionToolPlugin::new("test", vec![descriptor], Vec::new()).is_err(),
            "R2"
        );
        assert!(
            SessionToolPlugin::new("test", Vec::new(), vec![Arc::new(Echo)]).is_err(),
            "R3"
        );
    }
}
