use crate::{
    LoadSkillResourceTool, SkillActivateTool, SkillDiscoveryPlugin, SkillRegistry, SkillScriptTool,
};
use std::collections::HashMap;
use std::sync::Arc;
use tirea_contract::runtime::tool_call::{Tool, ToolAccessGranter};

/// Errors returned when wiring the skills subsystem into an agent.
#[derive(Debug, thiserror::Error)]
pub enum SkillSubsystemError {
    #[error("tool id already registered: {0}")]
    ToolIdConflict(String),
}

/// High-level facade for wiring skills into an agent.
///
/// Callers should prefer this over manually instantiating the tools/plugins so:
/// - tool ids stay consistent
/// - plugin ordering is stable (discovery first, runtime second)
///
/// # Example
///
/// ```ignore
/// use tirea::extensions::skills::{
///     FsSkill, InMemorySkillRegistry, SkillSubsystem,
/// };
/// use std::sync::Arc;
///
/// // 1) Discover skills and build a registry.
/// let result = FsSkill::discover("skills").unwrap();
/// let registry = Arc::new(
///     InMemorySkillRegistry::from_skills(FsSkill::into_arc_skills(result.skills)),
/// );
///
/// // 2) Wire into subsystem.
/// let skills = SkillSubsystem::new(registry);
///
/// // 3) Register tools (skill activation + reference/script utilities).
/// let mut tools = std::collections::HashMap::new();
/// skills.extend_tools(&mut tools).unwrap();
///
/// // 4) Register the discovery plugin: injects skills catalog before inference.
/// let config = BaseAgent::new("gpt-4o-mini").with_plugin(Arc::new(skills.discovery_plugin()));
/// # let _ = config;
/// # let _ = tools;
/// ```
#[derive(Clone)]
pub struct SkillSubsystem {
    registry: Arc<dyn SkillRegistry>,
    access_granter: Option<Arc<dyn ToolAccessGranter>>,
}

impl std::fmt::Debug for SkillSubsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillSubsystem").finish_non_exhaustive()
    }
}

impl SkillSubsystem {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            access_granter: None,
        }
    }

    /// Set the tool access granter for run-scoped permission overrides.
    #[must_use]
    pub fn with_access_granter(mut self, granter: Arc<dyn ToolAccessGranter>) -> Self {
        self.access_granter = Some(granter);
        self
    }

    pub fn registry(&self) -> &Arc<dyn SkillRegistry> {
        &self.registry
    }

    /// Build the discovery plugin (injects skills catalog before inference).
    pub fn discovery_plugin(&self) -> SkillDiscoveryPlugin {
        SkillDiscoveryPlugin::new(self.registry.clone())
    }

    /// Construct the skills tools map.
    ///
    /// Tool ids:
    /// - `SKILL_ACTIVATE_TOOL_ID`
    /// - `SKILL_LOAD_RESOURCE_TOOL_ID`
    /// - `SKILL_SCRIPT_TOOL_ID`
    pub fn tools(&self) -> HashMap<String, Arc<dyn Tool>> {
        let mut out: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        // These inserts cannot conflict inside an empty map.
        let _ = self.extend_tools(&mut out);
        out
    }

    /// Add skills tools to an existing tool map.
    ///
    /// Returns an error if any tool id is already present.
    pub fn extend_tools(
        &self,
        tools: &mut HashMap<String, Arc<dyn Tool>>,
    ) -> Result<(), SkillSubsystemError> {
        let registry = self.registry.clone();
        let mut activate = SkillActivateTool::new(registry.clone());
        if let Some(granter) = &self.access_granter {
            activate = activate.with_access_granter(granter.clone());
        }
        let tool_defs: Vec<Arc<dyn Tool>> = vec![
            Arc::new(activate),
            Arc::new(LoadSkillResourceTool::new(registry.clone())),
            Arc::new(SkillScriptTool::new(registry)),
        ];

        for t in tool_defs {
            let id = t.descriptor().id.clone();
            if tools.contains_key(&id) {
                return Err(SkillSubsystemError::ToolIdConflict(id));
            }
            tools.insert(id, t);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FsSkill, InMemorySkillRegistry, Skill, SKILL_ACTIVATE_TOOL_ID, SKILL_LOAD_RESOURCE_TOOL_ID,
        SKILL_SCRIPT_TOOL_ID,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use serde_json::Value;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;
    use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::runtime::state::{reduce_state_actions, ScopeContext};
    use tirea_contract::runtime::tool_call::{ToolError, ToolResult};
    use tirea_contract::testing::TestFixture;
    use tirea_contract::thread::Thread;
    use tirea_contract::thread::{Message, ToolCall};
    use tirea_state::TrackedPatch;

    fn make_registry(skills: Vec<Arc<dyn Skill>>) -> Arc<dyn SkillRegistry> {
        Arc::new(InMemorySkillRegistry::from_skills(skills))
    }

    struct LocalToolExecution {
        result: ToolResult,
        patch: Option<TrackedPatch>,
    }

    async fn execute_single_tool(
        tool: Option<&dyn Tool>,
        call: &ToolCall,
        state: &Value,
    ) -> LocalToolExecution {
        let Some(tool) = tool else {
            return LocalToolExecution {
                result: ToolResult::error(&call.name, format!("Tool '{}' not found", call.name)),
                patch: None,
            };
        };

        let fix = TestFixture::new_with_state(state.clone());
        let tool_ctx = fix.ctx_with(&call.id, format!("tool:{}", call.name));
        let effect = match tool.execute_effect(call.arguments.clone(), &tool_ctx).await {
            Ok(e) => e,
            Err(e) => {
                return LocalToolExecution {
                    result: ToolResult::error(&call.name, e.to_string()),
                    patch: None,
                };
            }
        };
        let (result, actions) = effect.into_parts();
        let state_actions: Vec<_> = actions
            .into_iter()
            .filter_map(|a| match a {
                tirea_contract::runtime::phase::AfterToolExecuteAction::State(sa) => Some(sa),
                _ => None,
            })
            .collect();
        let scope_ctx = ScopeContext::run();
        let patches = reduce_state_actions(
            state_actions,
            state,
            &format!("tool:{}", call.name),
            &scope_ctx,
        )
        .unwrap();
        let patch = patches
            .into_iter()
            .reduce(|mut acc, p| {
                acc.patch.merge(p.into_patch());
                acc
            })
            .filter(|p| !p.patch().is_empty());
        LocalToolExecution { result, patch }
    }

    #[derive(Debug)]
    struct DummyTool;

    #[async_trait]
    impl Tool for DummyTool {
        fn descriptor(&self) -> tirea_contract::runtime::tool_call::ToolDescriptor {
            tirea_contract::runtime::tool_call::ToolDescriptor::new(
                SKILL_ACTIVATE_TOOL_ID,
                "x",
                "x",
            )
            .with_parameters(json!({}))
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &tirea_contract::runtime::tool_call::ToolCallContext<'_>,
        ) -> Result<
            tirea_contract::runtime::tool_call::ToolResult,
            tirea_contract::runtime::tool_call::ToolError,
        > {
            Ok(tirea_contract::runtime::tool_call::ToolResult::success(
                SKILL_ACTIVATE_TOOL_ID,
                json!({}),
            ))
        }
    }

    fn make_subsystem() -> (TempDir, SkillSubsystem) {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("s1")).unwrap();
        fs::write(
            root.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(root).unwrap();
        let sys = SkillSubsystem::new(make_registry(FsSkill::into_arc_skills(result.skills)));
        (td, sys)
    }

    #[test]
    fn subsystem_extend_tools_detects_conflict() {
        let (_td, sys) = make_subsystem();
        let mut tools = HashMap::<String, Arc<dyn Tool>>::new();
        tools.insert(SKILL_ACTIVATE_TOOL_ID.to_string(), Arc::new(DummyTool));
        let err = sys.extend_tools(&mut tools).unwrap_err();
        assert!(err.to_string().contains("tool id already registered"));
    }

    #[test]
    fn subsystem_tools_returns_expected_ids() {
        let (_td, sys) = make_subsystem();
        let tools = sys.tools();
        assert!(tools.contains_key(SKILL_ACTIVATE_TOOL_ID));
        assert!(tools.contains_key(SKILL_LOAD_RESOURCE_TOOL_ID));
        assert!(tools.contains_key(SKILL_SCRIPT_TOOL_ID));
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn subsystem_extend_tools_inserts_tools_into_existing_map() {
        let (_td, sys) = make_subsystem();
        let mut tools = HashMap::<String, Arc<dyn Tool>>::new();
        tools.insert("other".to_string(), Arc::new(DummyOtherTool));
        sys.extend_tools(&mut tools).unwrap();
        assert!(tools.contains_key("other"));
        assert!(tools.contains_key(SKILL_ACTIVATE_TOOL_ID));
        assert!(tools.contains_key(SKILL_LOAD_RESOURCE_TOOL_ID));
        assert!(tools.contains_key(SKILL_SCRIPT_TOOL_ID));
        assert_eq!(tools.len(), 4);
    }

    #[derive(Debug)]
    struct DummyOtherTool;

    #[async_trait]
    impl Tool for DummyOtherTool {
        fn descriptor(&self) -> tirea_contract::runtime::tool_call::ToolDescriptor {
            tirea_contract::runtime::tool_call::ToolDescriptor::new("other", "x", "x")
                .with_parameters(json!({}))
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &tirea_contract::runtime::tool_call::ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("other", json!({})))
        }
    }

    #[tokio::test]
    async fn subsystem_plugin_injects_catalog_and_activated_skill() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("docx").join("references")).unwrap();
        fs::write(
            root.join("docx").join("references").join("DOCX-JS.md"),
            "Use docx-js for new documents.",
        )
        .unwrap();

        let mut f = fs::File::create(root.join("docx").join("SKILL.md")).unwrap();
        f.write_all(
            b"---\nname: docx\ndescription: DOCX guidance\n---\nUse docx-js for new documents.\n\n",
        )
        .unwrap();

        let result = FsSkill::discover(root).unwrap();
        let sys = SkillSubsystem::new(make_registry(FsSkill::into_arc_skills(result.skills)));
        let tools = sys.tools();

        // Activate the skill via the registered "skill" tool.
        let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));
        let state = thread.rebuild_state().unwrap();
        let call = ToolCall::new("call_1", SKILL_ACTIVATE_TOOL_ID, json!({"skill": "docx"}));
        let activate_tool = tools.get(SKILL_ACTIVATE_TOOL_ID).unwrap().as_ref();
        let exec = execute_single_tool(Some(activate_tool), &call, &state).await;
        assert!(exec.result.is_success());
        let thread = thread.with_patch(exec.patch.unwrap());

        let state = thread.rebuild_state().unwrap();
        let call = ToolCall::new(
            "call_2",
            SKILL_LOAD_RESOURCE_TOOL_ID,
            json!({"skill": "docx", "path": "references/DOCX-JS.md"}),
        );
        let load_resource_tool = tools.get(SKILL_LOAD_RESOURCE_TOOL_ID).unwrap().as_ref();
        let exec = execute_single_tool(Some(load_resource_tool), &call, &state).await;
        assert!(exec.result.is_success());
        let thread = if let Some(patch) = exec.patch {
            thread.with_patch(patch)
        } else {
            thread
        };

        // Run the discovery plugin and verify discovery catalog is injected.
        let plugin: Arc<dyn AgentBehavior> = Arc::new(sys.discovery_plugin());
        let state = thread.rebuild_state().unwrap();
        let fix = tirea_contract::testing::TestFixture::new_with_state(state);
        let run_policy = tirea_contract::RunPolicy::default();
        let fixture_ctx = fix.ctx();
        let ctx = ReadOnlyContext::new(
            Phase::BeforeInference,
            &thread.id,
            &thread.messages,
            &run_policy,
            fixture_ctx.doc(),
        );
        let actions = plugin.before_inference(&ctx).await;

        use tirea_contract::runtime::phase::BeforeInferenceAction;
        let system_context_count = actions
            .as_slice()
            .iter()
            .filter(|a| matches!(a, BeforeInferenceAction::AddContextMessage(_)))
            .count();
        assert_eq!(system_context_count, 1);
    }
}
