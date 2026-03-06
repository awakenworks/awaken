use super::*;
use crate::contracts::runtime::behavior::ReadOnlyContext;
use crate::contracts::runtime::phase::{ActionSet, BeforeInferenceAction, LifecycleAction};
use crate::contracts::runtime::state::AnyStateAction;
use crate::contracts::runtime::tool_call::ToolDescriptor;
use crate::contracts::runtime::tool_call::{ToolError, ToolResult};
use crate::contracts::storage::{ThreadReader, ThreadWriter};
use crate::contracts::thread::Thread;
use crate::contracts::AgentBehavior;
use crate::contracts::ToolCallContext;
use crate::extensions::skills::{
    FsSkill, FsSkillRegistryManager, InMemorySkillRegistry, ScriptResult, Skill, SkillError,
    SkillMeta, SkillRegistry, SkillRegistryError, SkillResource, SkillResourceKind,
};
use crate::orchestrator::agent_tools::SCOPE_CALLER_AGENT_ID_KEY;
use crate::runtime::loop_runner::{
    TOOL_SCOPE_CALLER_AGENT_ID_KEY, TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tirea_contract::testing::{
    apply_before_inference_for_test, apply_lifecycle_for_test, TestFixture,
};
use tirea_contract::TerminationReason;

fn decision_for(
    target_id: &str,
    action: crate::contracts::io::ResumeDecisionAction,
    result: Value,
) -> crate::contracts::ToolCallDecision {
    crate::contracts::ToolCallDecision {
        target_id: target_id.to_string(),
        resume: crate::contracts::runtime::ToolCallResume {
            decision_id: format!("decision_{target_id}"),
            action,
            result,
            reason: None,
            updated_at: 0,
        },
    }
}

fn assert_run_lifecycle_state(
    state: &Value,
    run_id: &str,
    status: &str,
    done_reason: Option<&str>,
) {
    assert_eq!(state["__run"]["id"], json!(run_id));
    assert_eq!(state["__run"]["status"], json!(status));
    match done_reason {
        Some(reason) => assert_eq!(state["__run"]["done_reason"], json!(reason)),
        None => assert!(state["__run"]["done_reason"].is_null()),
    }
}

fn make_skills_root() -> (TempDir, PathBuf) {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("s1")).unwrap();
    fs::write(
        root.join("s1").join("SKILL.md"),
        "---\nname: s1\ndescription: ok\n---\nDo X\n",
    )
    .unwrap();
    (td, root)
}

#[derive(Clone)]
struct FailOnNthAppendStorage {
    inner: Arc<tirea_store_adapters::MemoryStore>,
    fail_on_nth_append: usize,
    append_calls: Arc<AtomicUsize>,
}

impl FailOnNthAppendStorage {
    fn new(fail_on_nth_append: usize) -> Self {
        Self {
            inner: Arc::new(tirea_store_adapters::MemoryStore::new()),
            fail_on_nth_append,
            append_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn append_call_count(&self) -> usize {
        self.append_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl crate::contracts::storage::ThreadReader for FailOnNthAppendStorage {
    async fn load(
        &self,
        thread_id: &str,
    ) -> Result<
        Option<crate::contracts::storage::ThreadHead>,
        crate::contracts::storage::ThreadStoreError,
    > {
        <tirea_store_adapters::MemoryStore as crate::contracts::storage::ThreadReader>::load(
            self.inner.as_ref(),
            thread_id,
        )
        .await
    }

    async fn list_threads(
        &self,
        query: &crate::contracts::storage::ThreadListQuery,
    ) -> Result<
        crate::contracts::storage::ThreadListPage,
        crate::contracts::storage::ThreadStoreError,
    > {
        <tirea_store_adapters::MemoryStore as crate::contracts::storage::ThreadReader>::list_threads(
            self.inner.as_ref(),
            query,
        )
        .await
    }
}

#[async_trait]
impl crate::contracts::storage::ThreadWriter for FailOnNthAppendStorage {
    async fn create(
        &self,
        thread: &Thread,
    ) -> Result<crate::contracts::storage::Committed, crate::contracts::storage::ThreadStoreError>
    {
        <tirea_store_adapters::MemoryStore as crate::contracts::storage::ThreadWriter>::create(
            self.inner.as_ref(),
            thread,
        )
        .await
    }

    async fn append(
        &self,
        thread_id: &str,
        changeset: &crate::contracts::ThreadChangeSet,
        precondition: crate::contracts::storage::VersionPrecondition,
    ) -> Result<crate::contracts::storage::Committed, crate::contracts::storage::ThreadStoreError>
    {
        let append_idx = self.append_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if append_idx == self.fail_on_nth_append {
            return Err(crate::contracts::storage::ThreadStoreError::Serialization(
                format!("injected append failure on call {append_idx}"),
            ));
        }
        <tirea_store_adapters::MemoryStore as crate::contracts::storage::ThreadWriter>::append(
            self.inner.as_ref(),
            thread_id,
            changeset,
            precondition,
        )
        .await
    }

    async fn delete(
        &self,
        thread_id: &str,
    ) -> Result<(), crate::contracts::storage::ThreadStoreError> {
        <tirea_store_adapters::MemoryStore as crate::contracts::storage::ThreadWriter>::delete(
            self.inner.as_ref(),
            thread_id,
        )
        .await
    }

    async fn save(
        &self,
        thread: &Thread,
    ) -> Result<(), crate::contracts::storage::ThreadStoreError> {
        <tirea_store_adapters::MemoryStore as crate::contracts::storage::ThreadWriter>::save(
            self.inner.as_ref(),
            thread,
        )
        .await
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RunEndMarkerState(bool);

impl tirea_state::State for RunEndMarkerState {
    type Ref<'a> = ();

    const PATH: &'static str = "run_end_marker";

    fn state_ref<'a>(
        _doc: &'a tirea_state::DocCell,
        _base: tirea_state::Path,
        _sink: tirea_state::PatchSink<'a>,
    ) -> Self::Ref<'a> {
    }

    fn from_value(value: &serde_json::Value) -> tirea_state::TireaResult<Self> {
        Ok(Self(value.as_bool().unwrap_or(false)))
    }

    fn to_value(&self) -> tirea_state::TireaResult<serde_json::Value> {
        Ok(json!(self.0))
    }
}

impl crate::contracts::runtime::state::StateSpec for RunEndMarkerState {
    type Action = bool;

    fn reduce(&mut self, action: bool) {
        self.0 = action;
    }
}

#[derive(Debug)]
struct TerminateWithRunEndPatchPlugin;

#[async_trait]
impl AgentBehavior for TerminateWithRunEndPatchPlugin {
    fn id(&self) -> &str {
        "terminate_with_run_end_patch"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::Terminate(
            TerminationReason::BehaviorRequested,
        ))
    }

    fn register_action_deserializers(
        &self,
        registry: &mut crate::contracts::runtime::state::ActionDeserializerRegistry,
    ) {
        registry.register::<RunEndMarkerState>();
    }

    async fn run_end(&self, _ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        ActionSet::single(LifecycleAction::State(AnyStateAction::new::<
            RunEndMarkerState,
        >(true)))
    }
}

#[tokio::test]
async fn wire_skills_inserts_tools_and_plugin() {
    let (_td, root) = make_skills_root();
    let os = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: true,
            ..SkillsConfig::default()
        })
        .build()
        .unwrap();

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    let cfg = AgentDefinition::new("gpt-4o-mini");
    let cfg = os.wire_skills_into(cfg, &mut tools).unwrap();

    assert!(tools.contains_key("skill"));
    assert!(tools.contains_key("load_skill_resource"));
    assert!(tools.contains_key("skill_script"));

    let behavior_ids = cfg.behavior.behavior_ids();
    assert_eq!(behavior_ids.len(), 2);
    assert_eq!(behavior_ids[0], "skills_discovery");
    assert_eq!(behavior_ids[1], "stop_policy");

    // Verify injection does not panic and includes catalog.
    let state = json!({
        "skills": {
            "active": ["s1"],
            "instructions": {"s1": "Do X"},
            "references": {},
            "scripts": {}
        }
    });
    let doc = tirea_state::DocCell::new(state);
    let run_config = crate::contracts::RunConfig::new();
    let ctx = ReadOnlyContext::new(
        crate::contracts::runtime::phase::Phase::BeforeInference,
        "thread_1",
        &[],
        &run_config,
        &doc,
    );
    let actions = cfg.behavior.before_inference(&ctx).await;
    let apply_fixture = TestFixture::new();
    let mut apply_step = apply_fixture.step(vec![]);
    apply_before_inference_for_test(&mut apply_step, actions);
    let merged: String = apply_step.inference.system_context.join("\n");
    assert!(merged.contains("<available_skills>"));
    assert!(
        !merged.contains("<skill_instructions skill=\"s1\">"),
        "runtime skill instructions are delivered via ToolExecutionEffect user messages, not system context"
    );
}

#[tokio::test]
async fn wire_skills_runtime_only_injects_active_skills_without_catalog() {
    let (_td, root) = make_skills_root();
    let os = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: true,
            advertise_catalog: false,
            ..SkillsConfig::default()
        })
        .build()
        .unwrap();

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    let cfg = AgentDefinition::new("gpt-4o-mini");
    let cfg = os.wire_skills_into(cfg, &mut tools).unwrap();

    // With advertise_catalog=false, no discovery plugin is registered — only tools.
    let behavior_ids = cfg.behavior.behavior_ids();
    assert_eq!(behavior_ids.len(), 1);
    assert_eq!(behavior_ids[0], "stop_policy");

    // Tools are still available even without the catalog plugin.
    assert!(tools.contains_key("skill"));
    assert!(tools.contains_key("load_skill_resource"));
    assert!(tools.contains_key("skill_script"));
}

#[test]
fn wire_skills_disabled_is_noop() {
    let (_td, root) = make_skills_root();
    let os = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: false,
            ..SkillsConfig::default()
        })
        .build()
        .unwrap();

    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    let cfg = AgentDefinition::new("gpt-4o-mini");
    let cfg2 = os.wire_skills_into(cfg, &mut tools).unwrap();

    assert!(tools.is_empty());
    // Only the synthesized stop-policy plugin (from default max_rounds=10).
    let behavior_ids = cfg2.behavior.behavior_ids();
    assert_eq!(behavior_ids.len(), 1);
    assert_eq!(behavior_ids[0], "stop_policy");
}

#[test]
fn wire_behaviors_into_orders_behavior_ids() {
    #[derive(Debug)]
    struct LocalPlugin(&'static str);

    #[async_trait]
    impl AgentBehavior for LocalPlugin {
        fn id(&self) -> &str {
            self.0
        }
    }

    let os = AgentOs::builder()
        .with_registered_behavior("policy1", Arc::new(LocalPlugin("policy1")))
        .with_registered_behavior("p1", Arc::new(LocalPlugin("p1")))
        .build()
        .unwrap();

    let cfg = AgentDefinition::new("gpt-4o-mini")
        .with_behavior_id("policy1")
        .with_behavior_id("p1");

    let wired = os.wire_behaviors_into(cfg).unwrap();
    let ids: Vec<&str> = wired.iter().map(|p| p.id()).collect();
    assert_eq!(ids, vec!["policy1", "p1"]);
}

#[test]
fn wire_behaviors_into_rejects_duplicate_behavior_ids_after_assembly() {
    #[derive(Debug)]
    struct LocalPlugin(&'static str);

    #[async_trait]
    impl AgentBehavior for LocalPlugin {
        fn id(&self) -> &str {
            self.0
        }
    }

    // Register two different plugins with the same id — this is normally prevented
    // by the registry, but we can test the wire_behaviors_into dedup via an agent
    // that references the same id twice.
    let os = AgentOs::builder()
        .with_registered_behavior("p1", Arc::new(LocalPlugin("p1")))
        .build()
        .unwrap();

    // Referencing same behavior_id twice should fail at wire time
    let cfg = AgentDefinition::new("gpt-4o-mini")
        .with_behavior_id("p1")
        .with_behavior_id("p1");

    let err = os.wire_behaviors_into(cfg).err().expect("expected error");
    assert!(matches!(err, AgentOsWiringError::BehaviorAlreadyInstalled(id) if id == "p1"));
}

#[derive(Debug)]
struct FakeSkillsPlugin;

#[async_trait::async_trait]
impl AgentBehavior for FakeSkillsPlugin {
    fn id(&self) -> &str {
        "skills"
    }
}

#[test]
fn build_errors_if_agent_references_reserved_skills_plugin_id() {
    let (_td, root) = make_skills_root();
    let err = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: true,
            ..SkillsConfig::default()
        })
        .with_registered_behavior("skills", Arc::new(FakeSkillsPlugin))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("skills"),
        )
        .build()
        .unwrap_err();

    assert!(matches!(
        err,
        AgentOsBuildError::AgentReservedBehaviorId { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "skills"
    ));
}

#[derive(Debug)]
struct FakeAgentToolsPlugin;

#[async_trait::async_trait]
impl AgentBehavior for FakeAgentToolsPlugin {
    fn id(&self) -> &str {
        "agent_tools"
    }
}

#[test]
fn build_errors_if_agent_references_reserved_agent_tools_plugin_id() {
    let err = AgentOs::builder()
        .with_registered_behavior("agent_tools", Arc::new(FakeAgentToolsPlugin))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("agent_tools"),
        )
        .build()
        .unwrap_err();

    assert!(matches!(
        err,
        AgentOsBuildError::AgentReservedBehaviorId { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "agent_tools"
    ));
}

#[derive(Debug)]
struct FakeAgentRecoveryPlugin;

#[async_trait::async_trait]
impl AgentBehavior for FakeAgentRecoveryPlugin {
    fn id(&self) -> &str {
        "agent_recovery"
    }
}

#[test]
fn build_errors_if_agent_references_reserved_agent_recovery_plugin_id() {
    let err = AgentOs::builder()
        .with_registered_behavior("agent_recovery", Arc::new(FakeAgentRecoveryPlugin))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("agent_recovery"),
        )
        .build()
        .unwrap_err();

    assert!(matches!(
        err,
        AgentOsBuildError::AgentReservedBehaviorId { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "agent_recovery"
    ));
}

#[test]
fn resolve_errors_if_agent_missing() {
    let os = AgentOs::builder().build().unwrap();
    let err = os.resolve("missing").err().unwrap();
    assert!(matches!(err, AgentOsResolveError::AgentNotFound(_)));
}

#[tokio::test]
async fn resolve_wires_skills_and_preserves_base_tools() {
    #[derive(Debug)]
    struct BaseTool;

    #[async_trait::async_trait]
    impl Tool for BaseTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("base_tool", "Base Tool", "Base Tool")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("base_tool", json!({"ok": true})))
        }
    }

    let (_td, root) = make_skills_root();
    let os = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: true,
            ..SkillsConfig::default()
        })
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .with_tools(HashMap::from([(
            "base_tool".to_string(),
            Arc::new(BaseTool) as Arc<dyn Tool>,
        )]))
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();

    assert_eq!(resolved.agent.id, "a1");
    assert!(resolved.tools.contains_key("base_tool"));
    assert!(resolved.tools.contains_key("skill"));
    assert!(resolved.tools.contains_key("load_skill_resource"));
    assert!(resolved.tools.contains_key("skill_script"));
    assert!(resolved.tools.contains_key("agent_run"));
    assert!(resolved.tools.contains_key("agent_stop"));
    let behavior_ids = resolved.agent.behavior.behavior_ids();
    assert_eq!(behavior_ids.len(), 4);
    assert_eq!(behavior_ids[0], "skills_discovery");
    assert_eq!(behavior_ids[1], "agent_tools");
    assert_eq!(behavior_ids[2], "agent_recovery");
    assert_eq!(behavior_ids[3], "stop_policy");
}

#[test]
fn resolve_freezes_tool_snapshot_per_run_boundary() {
    #[derive(Debug)]
    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(self.0, self.0, "dynamic tool")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(self.0, json!({"ok": true})))
        }
    }

    #[derive(Default)]
    struct MutableRegistry {
        tools: std::sync::RwLock<HashMap<String, Arc<dyn Tool>>>,
    }

    impl MutableRegistry {
        fn replace(&self, ids: &[&'static str]) {
            let mut next = HashMap::new();
            for id in ids {
                next.insert((*id).to_string(), Arc::new(NamedTool(id)) as Arc<dyn Tool>);
            }
            match self.tools.write() {
                Ok(mut guard) => *guard = next,
                Err(poisoned) => *poisoned.into_inner() = next,
            }
        }
    }

    impl ToolRegistry for MutableRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
            match self.tools.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    let dynamic_registry = Arc::new(MutableRegistry::default());
    dynamic_registry.replace(&["mcp__s1__echo"]);

    let os = AgentOs::builder()
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .with_tool_registry(dynamic_registry.clone() as Arc<dyn ToolRegistry>)
        .build()
        .expect("build agent os");

    let resolved1 = os.resolve("a1").expect("resolve #1");
    let tools_first_run = resolved1.tools;
    assert!(tools_first_run.contains_key("mcp__s1__echo"));
    assert!(!tools_first_run.contains_key("mcp__s1__sum"));

    dynamic_registry.replace(&["mcp__s1__sum"]);

    // The first run snapshot is frozen.
    assert!(tools_first_run.contains_key("mcp__s1__echo"));
    assert!(!tools_first_run.contains_key("mcp__s1__sum"));

    // The next resolve picks up refreshed registry state.
    let resolved2 = os.resolve("a1").expect("resolve #2");
    let tools_second_run = resolved2.tools;
    assert!(!tools_second_run.contains_key("mcp__s1__echo"));
    assert!(tools_second_run.contains_key("mcp__s1__sum"));
}

#[tokio::test]
async fn resolve_freezes_agent_snapshot_per_run_boundary() {
    #[derive(Default)]
    struct MutableAgentRegistry {
        agents: std::sync::RwLock<HashMap<String, AgentDefinition>>,
    }

    impl MutableAgentRegistry {
        fn replace_ids(&self, ids: &[&str]) {
            let mut map = HashMap::new();
            for id in ids {
                map.insert((*id).to_string(), AgentDefinition::new("gpt-4o-mini"));
            }
            match self.agents.write() {
                Ok(mut guard) => *guard = map,
                Err(poisoned) => *poisoned.into_inner() = map,
            }
        }
    }

    impl AgentRegistry for MutableAgentRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<AgentDefinition> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, AgentDefinition> {
            match self.agents.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    let dynamic_agents = Arc::new(MutableAgentRegistry::default());
    dynamic_agents.replace_ids(&["worker_a"]);

    let os = AgentOs::builder()
        .with_agent("root", AgentDefinition::new("gpt-4o-mini"))
        .with_agent_registry(dynamic_agents.clone() as Arc<dyn AgentRegistry>)
        .build()
        .expect("build agent os");

    let resolved1 = os.resolve("root").expect("resolve #1");
    let tools_first_run = resolved1.tools;
    let run_tool_first = tools_first_run
        .get("agent_run")
        .cloned()
        .expect("agent_run tool should exist");

    // Update source registry after resolve #1. First run should still see worker_a.
    dynamic_agents.replace_ids(&["worker_b"]);

    let scope = {
        let mut scope = tirea_contract::RunConfig::new();
        scope
            .set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, "owner-thread")
            .expect("set caller thread id");
        scope
            .set(TOOL_SCOPE_CALLER_AGENT_ID_KEY, "root")
            .expect("set caller agent id");
        scope
    };
    let mut fix_first = TestFixture::new();
    fix_first.run_config = scope.clone();
    let first_result = run_tool_first
        .execute(
            json!({
                "agent_id": "worker_a",
                "prompt": "hi",
                "background": true
            }),
            &fix_first.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .expect("execute first run tool");
    assert!(
        first_result.is_success(),
        "first run should use frozen agents"
    );

    // Next resolve should use refreshed source and reject worker_a.
    let resolved2 = os.resolve("root").expect("resolve #2");
    let tools_second_run = resolved2.tools;
    let run_tool_second = tools_second_run
        .get("agent_run")
        .cloned()
        .expect("agent_run tool should exist");
    let mut fix_second = TestFixture::new();
    fix_second.run_config = scope.clone();
    let second_result = run_tool_second
        .execute(
            json!({
                "agent_id": "worker_a",
                "prompt": "hi",
                "background": false
            }),
            &fix_second.ctx_with("call-2", "tool:agent_run"),
        )
        .await
        .expect("execute second run tool");
    assert!(
        second_result.is_error(),
        "second run should observe updated agents snapshot"
    );
    assert!(second_result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[tokio::test]
async fn resolve_freezes_skill_snapshot_per_run_boundary() {
    #[derive(Debug)]
    struct MockSkill {
        meta: SkillMeta,
        raw: String,
    }

    impl MockSkill {
        fn new(id: &str) -> Self {
            Self {
                meta: SkillMeta {
                    id: id.to_string(),
                    name: id.to_string(),
                    description: format!("{id} skill"),
                    allowed_tools: Vec::new(),
                },
                raw: format!("---\nname: {id}\ndescription: ok\n---\nBody\n"),
            }
        }
    }

    #[async_trait::async_trait]
    impl Skill for MockSkill {
        fn meta(&self) -> &SkillMeta {
            &self.meta
        }

        async fn read_instructions(&self) -> Result<String, SkillError> {
            Ok(self.raw.clone())
        }

        async fn load_resource(
            &self,
            _kind: SkillResourceKind,
            _path: &str,
        ) -> Result<SkillResource, SkillError> {
            Err(SkillError::Unsupported("not used".to_string()))
        }

        async fn run_script(
            &self,
            _script: &str,
            _args: &[String],
        ) -> Result<ScriptResult, SkillError> {
            Err(SkillError::Unsupported("not used".to_string()))
        }
    }

    #[derive(Default)]
    struct MutableSkillRegistry {
        skills: std::sync::RwLock<HashMap<String, Arc<dyn Skill>>>,
    }

    impl MutableSkillRegistry {
        fn replace_ids(&self, ids: &[&str]) {
            let mut map: HashMap<String, Arc<dyn Skill>> = HashMap::new();
            for id in ids {
                map.insert((*id).to_string(), Arc::new(MockSkill::new(id)));
            }
            match self.skills.write() {
                Ok(mut guard) => *guard = map,
                Err(poisoned) => *poisoned.into_inner() = map,
            }
        }
    }

    impl SkillRegistry for MutableSkillRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
            match self.skills.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    let dynamic_skills = Arc::new(MutableSkillRegistry::default());
    dynamic_skills.replace_ids(&["s1"]);

    let os = AgentOs::builder()
        .with_agent("root", AgentDefinition::new("gpt-4o-mini"))
        .with_skill_registry(dynamic_skills.clone() as Arc<dyn SkillRegistry>)
        .with_skills_config(SkillsConfig {
            enabled: true,
            advertise_catalog: true,
            discovery_max_entries: 32,
            discovery_max_chars: 8 * 1024,
        })
        .build()
        .expect("build agent os");

    let resolved1 = os.resolve("root").expect("resolve #1");
    let tools_first_run = resolved1.tools;
    let activate_first = tools_first_run
        .get("skill")
        .cloned()
        .expect("skill activate tool should exist");

    dynamic_skills.replace_ids(&["s2"]);

    let fix_first = TestFixture::new();
    let first_result = activate_first
        .execute(
            json!({"skill": "s1"}),
            &fix_first.ctx_with("call-skill-1", "tool:skill"),
        )
        .await
        .expect("execute first skill tool");
    assert!(
        first_result.is_success(),
        "first run should use frozen skills"
    );

    let resolved2 = os.resolve("root").expect("resolve #2");
    let tools_second_run = resolved2.tools;
    let activate_second = tools_second_run
        .get("skill")
        .cloned()
        .expect("skill activate tool should exist");
    let fix_second = TestFixture::new();
    let second_result = activate_second
        .execute(
            json!({"skill": "s1"}),
            &fix_second.ctx_with("call-skill-2", "tool:skill"),
        )
        .await
        .expect("execute second skill tool");
    assert!(
        second_result.is_error(),
        "second run should observe updated skills snapshot"
    );
    assert!(second_result
        .message
        .unwrap_or_default()
        .contains("Unknown skill"));
}

#[test]
fn build_skill_registry_refresh_interval_starts_periodic_refresh() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("s1")).unwrap();
    fs::write(
        root.join("s1").join("SKILL.md"),
        "---\nname: s1\ndescription: ok\n---\nBody\n",
    )
    .unwrap();

    let manager = FsSkillRegistryManager::discover_roots(vec![root.clone()]).unwrap();
    assert!(!manager.periodic_refresh_running());

    let _os = AgentOs::builder()
        .with_agent("root", AgentDefinition::new("gpt-4o-mini"))
        .with_skill_registry(Arc::new(manager.clone()) as Arc<dyn SkillRegistry>)
        .with_skill_registry_refresh_interval(Duration::from_millis(20))
        .with_skills_config(SkillsConfig {
            enabled: true,
            advertise_catalog: true,
            discovery_max_entries: 32,
            discovery_max_chars: 8 * 1024,
        })
        .build()
        .expect("build agent os");

    assert!(manager.periodic_refresh_running());

    fs::create_dir_all(root.join("s2")).unwrap();
    fs::write(
        root.join("s2").join("SKILL.md"),
        "---\nname: s2\ndescription: ok\n---\nBody\n",
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(150));
    assert!(manager.get("s2").is_some());
    assert!(manager.stop_periodic_refresh());
}

#[tokio::test]
async fn run_and_run_stream_work_without_llm_when_terminate_behavior_requested() {
    #[derive(Debug)]
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait::async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let os = AgentOs::builder()
        .with_registered_behavior(
            "terminate_behavior_requested",
            Arc::new(TerminateBehaviorRequestedPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_behavior_requested"),
        )
        .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
        .build()
        .unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some("s2".to_string()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: Some(json!({})),
            messages: vec![],
            initial_decisions: vec![],
        })
        .await
        .unwrap();
    let mut stream = run.events;
    let ev = futures::StreamExt::next(&mut stream).await.unwrap();
    assert!(matches!(ev, AgentEvent::RunStart { .. }));
    let ev = futures::StreamExt::next(&mut stream).await.unwrap();
    assert!(matches!(ev, AgentEvent::RunFinish { .. }));
}

#[tokio::test]
async fn run_stream_stop_policy_plugin_terminates_without_passing_stop_conditions_to_loop() {
    use crate::orchestrator::StopPolicyInput;
    use crate::runtime::loop_runner::run_loop;

    #[derive(Debug)]
    struct AlwaysStopPolicy;

    impl crate::orchestrator::StopPolicy for AlwaysStopPolicy {
        fn id(&self) -> &str {
            "always_stop"
        }

        fn evaluate(
            &self,
            _input: &StopPolicyInput<'_>,
        ) -> Option<crate::contracts::StoppedReason> {
            Some(crate::contracts::StoppedReason::with_detail(
                "custom", "always",
            ))
        }
    }

    let os = AgentOs::builder()
        .with_stop_policy("always", Arc::new(AlwaysStopPolicy))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_stop_condition_id("always"),
        )
        .with_agent_state_store(Arc::new(tirea_store_adapters::MemoryStore::new()))
        .build()
        .unwrap();

    let resolved = os.resolve("a1").expect("resolve a1");
    assert!(
        resolved
            .agent
            .behavior
            .behavior_ids()
            .iter()
            .any(|id| *id == "stop_policy"),
        "resolved agent should carry stop policy via behavior"
    );

    #[derive(Debug)]
    struct OneShotLlm;

    #[async_trait]
    impl crate::runtime::loop_runner::LlmExecutor for OneShotLlm {
        async fn exec_chat_response(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<genai::chat::ChatResponse> {
            let model_iden =
                genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock-model");
            Ok(genai::chat::ChatResponse {
                content: genai::chat::MessageContent::from_text("ok".to_string()),
                reasoning_content: None,
                model_iden: model_iden.clone(),
                provider_model_iden: model_iden,
                usage: genai::chat::Usage::default(),
                captured_raw_body: None,
            })
        }

        async fn exec_chat_stream_events(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<crate::runtime::loop_runner::LlmEventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }

        fn name(&self) -> &'static str {
            "one_shot_llm"
        }
    }

    let config = resolved.agent.with_llm_executor(
        Arc::new(OneShotLlm) as Arc<dyn crate::runtime::loop_runner::LlmExecutor>
    );
    let thread = crate::contracts::thread::Thread::new("stop-plugin-thread")
        .with_message(crate::contracts::thread::Message::user("go"));
    let run_ctx = crate::contracts::RunContext::from_thread(&thread, resolved.run_config)
        .expect("build run context");
    let outcome = run_loop(&config, resolved.tools, run_ctx, None, None, None).await;
    assert!(
        matches!(
            outcome.termination,
            TerminationReason::Stopped(ref stopped)
                if stopped.code == "custom" && stopped.detail.as_deref() == Some("always")
        ),
        "stop_policy plugin should terminate run with configured stop reason: {:?}",
        outcome.termination
    );
    assert_eq!(
        outcome.stats.llm_calls, 1,
        "stop policy should be evaluated after inference"
    );
}

#[test]
fn resolve_sets_runtime_caller_agent_id() {
    let os = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini")
                .with_allowed_skills(vec!["s1".to_string()])
                .with_allowed_agents(vec!["worker".to_string()])
                .with_allowed_tools(vec!["echo".to_string()]),
        )
        .build()
        .unwrap();
    let resolved = os.resolve("a1").unwrap();
    assert_eq!(
        resolved
            .run_config
            .value(SCOPE_CALLER_AGENT_ID_KEY)
            .and_then(|v| v.as_str()),
        Some("a1")
    );
    assert_eq!(
        resolved
            .run_config
            .value(tirea_contract::scope::SCOPE_ALLOWED_SKILLS_KEY),
        Some(&json!(["s1"]))
    );
    assert_eq!(
        resolved
            .run_config
            .value(super::policy::SCOPE_ALLOWED_AGENTS_KEY),
        Some(&json!(["worker"]))
    );
    assert_eq!(
        resolved
            .run_config
            .value(tirea_contract::scope::SCOPE_ALLOWED_TOOLS_KEY),
        Some(&json!(["echo"]))
    );
}

#[tokio::test]
async fn resolve_errors_on_skills_tool_id_conflict() {
    #[derive(Debug)]
    struct ConflictingTool;

    #[async_trait::async_trait]
    impl Tool for ConflictingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("skill", "Conflicting", "Conflicting")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("skill", json!({"ok": true})))
        }
    }

    let (_td, root) = make_skills_root();
    let os = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: true,
            ..SkillsConfig::default()
        })
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .with_tools(HashMap::from([(
            "skill".to_string(),
            Arc::new(ConflictingTool) as Arc<dyn Tool>,
        )]))
        .build()
        .unwrap();

    let err = os.resolve("a1").err().unwrap();
    assert!(matches!(
        err,
        AgentOsResolveError::Wiring(AgentOsWiringError::SkillsToolIdConflict(ref id))
        if id == "skill"
    ));
}

#[tokio::test]
async fn resolve_wires_agent_tools_by_default() {
    let os = AgentOs::builder()
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    assert!(resolved.tools.contains_key("agent_run"));
    assert!(resolved.tools.contains_key("agent_stop"));
    let behavior_ids = resolved.agent.behavior.behavior_ids();
    assert_eq!(behavior_ids[0], "agent_tools");
    assert_eq!(behavior_ids[1], "agent_recovery");
}

#[tokio::test]
async fn resolve_errors_on_agent_tools_tool_id_conflict() {
    #[derive(Debug)]
    struct ConflictingRunTool;

    #[async_trait::async_trait]
    impl Tool for ConflictingRunTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("agent_run", "Conflicting", "Conflicting")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("agent_run", json!({"ok": true})))
        }
    }

    let os = AgentOs::builder()
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .with_tools(HashMap::from([(
            "agent_run".to_string(),
            Arc::new(ConflictingRunTool) as Arc<dyn Tool>,
        )]))
        .build()
        .unwrap();

    let err = os.resolve("a1").err().unwrap();
    assert!(matches!(
        err,
        AgentOsResolveError::Wiring(AgentOsWiringError::AgentToolIdConflict(ref id))
        if id == "agent_run"
    ));
}

#[test]
fn build_errors_if_skills_enabled_without_root() {
    let err = AgentOs::builder()
        .with_skills_config(SkillsConfig {
            enabled: true,
            ..SkillsConfig::default()
        })
        .build()
        .unwrap_err();
    assert!(matches!(err, AgentOsBuildError::SkillsNotConfigured));
}

#[test]
fn build_errors_on_duplicate_skill_id_across_skill_registries() {
    let (_td, root) = make_skills_root();
    let skills = FsSkill::into_arc_skills(FsSkill::discover(&root).unwrap().skills);
    let duplicate_registry = InMemorySkillRegistry::from_skills(skills.clone());

    let err = AgentOs::builder()
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .with_skills(skills)
        .with_skill_registry(Arc::new(duplicate_registry) as Arc<dyn SkillRegistry>)
        .with_skills_config(SkillsConfig {
            enabled: true,
            advertise_catalog: true,
            discovery_max_entries: 32,
            discovery_max_chars: 8 * 1024,
        })
        .build()
        .unwrap_err();

    assert!(matches!(
        err,
        AgentOsBuildError::SkillRegistry(SkillRegistryError::DuplicateSkillId(ref id))
            if id == "s1"
    ));
}

#[tokio::test]
async fn resolve_errors_if_models_registry_present_but_model_missing() {
    let os = AgentOs::builder()
        .with_provider("p1", Client::default())
        .with_model(
            "m1",
            ModelDefinition::new("p1", "gpt-4o-mini")
                .with_chat_options(genai::chat::ChatOptions::default().with_capture_usage(true)),
        )
        .with_agent("a1", AgentDefinition::new("missing_model_ref"))
        .build()
        .unwrap();

    let err = os.resolve("a1").err().unwrap();
    assert!(matches!(err, AgentOsResolveError::ModelNotFound(ref id) if id == "missing_model_ref"));
}

#[tokio::test]
async fn resolve_rewrites_model_when_registry_present() {
    let os = AgentOs::builder()
        .with_provider("p1", Client::default())
        .with_model("m1", ModelDefinition::new("p1", "gpt-4o-mini"))
        .with_agent("a1", AgentDefinition::new("m1"))
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    assert_eq!(resolved.agent.model, "gpt-4o-mini");
}

#[derive(Debug)]
struct TestPlugin(&'static str);

#[async_trait]
impl AgentBehavior for TestPlugin {
    fn id(&self) -> &str {
        self.0
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::AddSystemContext(format!(
            "<plugin id=\"{}\"/>",
            self.0
        )))
    }
}

#[tokio::test]
async fn resolve_wires_plugins_from_registry() {
    let os = AgentOs::builder()
        .with_registered_behavior("p1", Arc::new(TestPlugin("p1")))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("p1"),
        )
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    assert!(resolved
        .agent
        .behavior
        .behavior_ids()
        .iter()
        .any(|id| *id == "p1"));

    let doc = tirea_state::DocCell::new(json!({}));
    let run_config = crate::contracts::RunConfig::new();
    let ctx = ReadOnlyContext::new(
        crate::contracts::runtime::phase::Phase::BeforeInference,
        "thread_1",
        &[],
        &run_config,
        &doc,
    );
    let actions = resolved.agent.behavior.before_inference(&ctx).await;
    let apply_fixture = TestFixture::new();
    let mut apply_step = apply_fixture.step(vec![]);
    apply_before_inference_for_test(&mut apply_step, actions);
    assert!(apply_step
        .inference
        .system_context
        .iter()
        .any(|s| s.contains("p1")));
}

#[tokio::test]
async fn resolve_wires_plugins_in_order() {
    let os = AgentOs::builder()
        .with_registered_behavior("policy1", Arc::new(TestPlugin("policy1")))
        .with_registered_behavior("p1", Arc::new(TestPlugin("p1")))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("policy1")
                .with_behavior_id("p1"),
        )
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    let behavior_ids = resolved.agent.behavior.behavior_ids();
    assert_eq!(behavior_ids[0], "agent_tools");
    assert_eq!(behavior_ids[1], "agent_recovery");
    assert_eq!(behavior_ids[2], "policy1");
    assert_eq!(behavior_ids[3], "p1");
}

#[tokio::test]
async fn resolve_wires_skills_before_plugins() {
    let (_td, root) = make_skills_root();
    let os = AgentOs::builder()
        .with_skills(FsSkill::into_arc_skills(
            FsSkill::discover(root).unwrap().skills,
        ))
        .with_skills_config(SkillsConfig {
            enabled: true,
            ..SkillsConfig::default()
        })
        .with_registered_behavior("policy1", Arc::new(TestPlugin("policy1")))
        .with_registered_behavior("p1", Arc::new(TestPlugin("p1")))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("policy1")
                .with_behavior_id("p1"),
        )
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    assert!(resolved.tools.contains_key("skill"));
    assert!(resolved.tools.contains_key("load_skill_resource"));
    assert!(resolved.tools.contains_key("skill_script"));
    assert!(resolved.tools.contains_key("agent_run"));
    assert!(resolved.tools.contains_key("agent_stop"));

    let behavior_ids = resolved.agent.behavior.behavior_ids();
    assert_eq!(behavior_ids[0], "skills_discovery");
    assert_eq!(behavior_ids[1], "agent_tools");
    assert_eq!(behavior_ids[2], "agent_recovery");
    assert_eq!(behavior_ids[3], "policy1");
    assert_eq!(behavior_ids[4], "p1");
}

#[test]
fn build_errors_if_builder_agent_references_missing_plugin() {
    let err = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("p1"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentBehaviorNotFound { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "p1"
    ));
}

#[test]
fn build_errors_on_duplicate_plugin_id_in_agent() {
    let err = AgentOs::builder()
        .with_registered_behavior("p1", Arc::new(TestPlugin("p1")))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("p1")
                .with_behavior_id("p1"),
        )
        .build()
        .unwrap_err();

    assert!(matches!(
        err,
        AgentOsBuildError::AgentDuplicateBehaviorRef { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "p1"
    ));
}

#[test]
fn build_errors_on_duplicate_plugin_ref_in_builder_agent() {
    let err = AgentOs::builder()
        .with_registered_behavior("p1", Arc::new(TestPlugin("p1")))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("p1")
                .with_behavior_id("p1"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentDuplicateBehaviorRef { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "p1"
    ));
}

#[test]
fn build_errors_on_reserved_plugin_id_in_builder_agent() {
    let err = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("skills"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentReservedBehaviorId { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "skills"
    ));
}

#[test]
fn resolve_errors_on_reserved_plugin_id() {
    let os = AgentOs::builder()
        .with_agent_registry(Arc::new({
            let mut reg = InMemoryAgentRegistry::new();
            reg.upsert(
                "a1",
                AgentDefinition::new("gpt-4o-mini").with_behavior_id("skills"),
            );
            reg
        }))
        .build()
        .unwrap();

    let err = os.resolve("a1").err().unwrap();
    assert!(matches!(
        err,
        AgentOsResolveError::Wiring(AgentOsWiringError::ReservedBehaviorId(ref id)) if id == "skills"
    ));
}

#[test]
fn build_errors_on_reserved_plugin_id_agent_tools_in_builder_agent() {
    let err = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("agent_tools"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentReservedBehaviorId { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "agent_tools"
    ));
}

#[tokio::test]
async fn run_stream_applies_frontend_state_to_existing_thread() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    #[derive(Debug)]
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_behavior_requested",
            Arc::new(TerminateBehaviorRequestedPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    // Create thread with initial state {"counter": 0}
    let thread = Thread::with_initial_state("t1", json!({"counter": 0}));
    storage.create(&thread).await.unwrap();

    // Verify initial state
    let head = storage.load("t1").await.unwrap().unwrap();
    assert_eq!(head.thread.state, json!({"counter": 0}));

    // Run with frontend state that replaces the thread state
    let request = RunRequest {
        agent_id: "a1".to_string(),
        thread_id: Some("t1".to_string()),
        run_id: Some("run-1".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        state: Some(json!({"counter": 42, "new_field": true})),
        messages: vec![crate::contracts::thread::Message::user("hello")],
        initial_decisions: vec![],
    };

    let run_stream = os.run_stream(request).await.unwrap();
    // Drain the stream to completion
    let _events: Vec<_> = run_stream.events.collect().await;

    // Verify state was replaced in storage
    let head = storage.load("t1").await.unwrap().unwrap();
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["counter"], json!(42));
    assert_eq!(state["new_field"], json!(true));
    assert_run_lifecycle_state(&state, "run-1", "done", Some("behavior_requested"));
}

#[tokio::test]
async fn run_stream_uses_state_as_initial_for_new_thread() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    #[derive(Debug)]
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_behavior_requested",
            Arc::new(TerminateBehaviorRequestedPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    // Run with state on a new thread
    let request = RunRequest {
        agent_id: "a1".to_string(),
        thread_id: Some("t-new".to_string()),
        run_id: Some("run-1".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        state: Some(json!({"initial": true})),
        messages: vec![crate::contracts::thread::Message::user("hello")],
        initial_decisions: vec![],
    };

    let run_stream = os.run_stream(request).await.unwrap();
    let _events: Vec<_> = run_stream.events.collect().await;

    // Verify state was set as initial state
    let head = storage.load("t-new").await.unwrap().unwrap();
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["initial"], json!(true));
    assert_run_lifecycle_state(&state, "run-1", "done", Some("behavior_requested"));
}

#[tokio::test]
async fn run_stream_preserves_state_when_no_frontend_state() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    #[derive(Debug)]
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_behavior_requested",
            Arc::new(TerminateBehaviorRequestedPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    // Create thread with initial state
    let thread = Thread::with_initial_state("t1", json!({"counter": 5}));
    storage.create(&thread).await.unwrap();

    // Run without frontend state — state should be preserved
    let request = RunRequest {
        agent_id: "a1".to_string(),
        thread_id: Some("t1".to_string()),
        run_id: Some("run-1".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        state: None,
        messages: vec![crate::contracts::thread::Message::user("hello")],
        initial_decisions: vec![],
    };

    let run_stream = os.run_stream(request).await.unwrap();
    let _events: Vec<_> = run_stream.events.collect().await;

    // Verify state was not changed
    let head = storage.load("t1").await.unwrap().unwrap();
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["counter"], json!(5));
    assert_run_lifecycle_state(&state, "run-1", "done", Some("behavior_requested"));
}

#[tokio::test]
async fn prepare_run_sets_identity_and_persists_user_delta_before_execution() {
    use tirea_store_adapters::MemoryStore;

    #[derive(Debug)]
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_behavior_requested",
            Arc::new(TerminateBehaviorRequestedPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-prepare".to_string()),
                run_id: Some("run-prepare".to_string()),
                parent_run_id: Some("run-parent".to_string()),
                parent_thread_id: None,
                resource_id: None,
                state: Some(json!({"count": 1})),
                messages: vec![crate::contracts::thread::Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    assert_eq!(prepared.thread_id, "t-prepare");
    assert_eq!(prepared.run_id, "run-prepare");
    assert_eq!(
        prepared.run_ctx.run_config.value("run_id"),
        Some(&json!("run-prepare"))
    );
    assert_eq!(
        prepared.run_ctx.run_config.value("parent_run_id"),
        Some(&json!("run-parent"))
    );

    let head = storage.load("t-prepare").await.unwrap().unwrap();
    assert_eq!(head.thread.messages.len(), 1);
    assert_eq!(
        head.thread.messages[0].role,
        crate::contracts::thread::Role::User
    );
    assert_eq!(head.thread.messages[0].content, "hello");
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["__run"]["id"], json!("run-prepare"));
    assert_eq!(state["__run"]["status"], json!("running"));
}

#[tokio::test]
async fn prepare_run_returns_error_when_run_id_already_exists_in_run_config() {
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let mut resolved = os.resolve("a1").unwrap();
    resolved
        .run_config
        .set("run_id", "preset-run-id")
        .expect("preset run_id");

    let err = match os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-prepare-duplicate-run-id".to_string()),
                run_id: Some("run-prepare".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![crate::contracts::thread::Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
    {
        Ok(_) => panic!("duplicate run_id in run_config should fail"),
        Err(err) => err,
    };

    assert!(matches!(
        err,
        AgentOsRunError::RunConfig(crate::contracts::RunConfigError::AlreadySet(ref key))
            if key == "run_id"
    ));
}

#[tokio::test]
async fn prepare_run_sets_parent_thread_id_for_existing_thread_without_lineage() {
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    storage
        .create(&Thread::new("t-parent-upsert"))
        .await
        .unwrap();

    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    os.prepare_run(
        RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some("t-parent-upsert".to_string()),
            run_id: Some("run-parent-upsert".to_string()),
            parent_run_id: None,
            parent_thread_id: Some("parent-thread-a".to_string()),
            resource_id: None,
            state: None,
            messages: vec![crate::contracts::thread::Message::user("hello")],
            initial_decisions: vec![],
        },
        resolved,
    )
    .await
    .unwrap();

    let head = storage.load("t-parent-upsert").await.unwrap().unwrap();
    assert_eq!(
        head.thread.parent_thread_id.as_deref(),
        Some("parent-thread-a")
    );
}

#[tokio::test]
async fn prepare_run_rejects_parent_thread_id_mismatch_for_existing_thread() {
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    storage
        .create(&Thread::new("t-parent-conflict").with_parent_thread_id("parent-thread-a"))
        .await
        .unwrap();

    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    let err = match os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-parent-conflict".to_string()),
                run_id: Some("run-parent-conflict".to_string()),
                parent_run_id: None,
                parent_thread_id: Some("parent-thread-b".to_string()),
                resource_id: None,
                state: None,
                messages: vec![crate::contracts::thread::Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
    {
        Ok(_) => panic!("mismatched parent_thread_id must fail"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("parent_thread_id mismatch"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn execute_prepared_runs_stream() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    #[derive(Debug)]
    struct TerminateBehaviorRequestedPlugin;

    #[async_trait]
    impl AgentBehavior for TerminateBehaviorRequestedPlugin {
        fn id(&self) -> &str {
            "terminate_behavior_requested"
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_behavior_requested",
            Arc::new(TerminateBehaviorRequestedPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-exec-prepared".to_string()),
                run_id: Some("run-exec-prepared".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![crate::contracts::thread::Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let events: Vec<_> = run.events.collect().await;
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::RunStart { .. })),
        "prepared stream should emit RunStart"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::RunFinish { .. })),
        "prepared stream should emit RunFinish"
    );
}

#[derive(Debug)]
struct DecisionTerminatePlugin;

#[async_trait]
impl AgentBehavior for DecisionTerminatePlugin {
    fn id(&self) -> &str {
        "decision_terminate_behavior_requested"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::Terminate(
            TerminationReason::BehaviorRequested,
        ))
    }
}

#[derive(Debug)]
struct DecisionEchoTool;

#[async_trait]
impl Tool for DecisionEchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo tool").with_parameters(json!({"type":"object"}))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("echo", args))
    }
}

fn make_decision_test_os(storage: Arc<tirea_store_adapters::MemoryStore>) -> AgentOs {
    make_decision_test_os_with_mode(storage, None)
}

fn make_decision_test_os_with_mode(
    storage: Arc<tirea_store_adapters::MemoryStore>,
    tool_execution_mode: Option<ToolExecutionMode>,
) -> AgentOs {
    let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
    tools.insert(
        "echo".to_string(),
        Arc::new(DecisionEchoTool) as Arc<dyn Tool>,
    );
    let mut def = AgentDefinition::new("gpt-4o-mini")
        .with_behavior_id("decision_terminate_behavior_requested");
    if let Some(mode) = tool_execution_mode {
        def = def.with_tool_execution_mode(mode);
    }
    AgentOs::builder()
        .with_agent_state_store(storage as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_tools(tools)
        .with_registered_behavior(
            "decision_terminate_behavior_requested",
            Arc::new(DecisionTerminatePlugin),
        )
        .with_agent("a1", def)
        .build()
        .unwrap()
}

#[tokio::test]
async fn run_stream_exposes_decision_sender_and_replays_suspended_calls() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": {
                            "message": "approved-from-channel"
                        }
                    },
                    "arguments": {
                            "message": "approved-from-channel"
                        },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": {
                            "message": "approved-from-channel"
                        }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state("t-decision-sender", pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some("t-decision-sender".to_string()),
            run_id: Some("run-decision-sender".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![],
        })
        .await
        .unwrap();

    run.submit_decision(decision_for(
        "call_pending",
        crate::contracts::io::ResumeDecisionAction::Resume,
        json!(true),
    ))
    .expect("decision channel should be connected");

    let events: Vec<_> = run.events.collect().await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending"
        )),
        "run stream should emit interaction resolution: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_pending"
        )),
        "run stream should replay suspended call after decision: {events:?}"
    );
}

#[tokio::test]
async fn run_stream_replays_initial_decisions_without_submit_decision() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": {
                            "message": "approved-from-request"
                        }
                    },
                    "arguments": {
                            "message": "approved-from-request"
                        },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": {
                            "message": "approved-from-request"
                        }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state("t-decision-initial", pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some("t-decision-initial".to_string()),
            run_id: Some("run-decision-initial".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "call_pending",
                crate::contracts::io::ResumeDecisionAction::Resume,
                json!(true),
            )],
        })
        .await
        .unwrap();

    let events: Vec<_> = run.events.collect().await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending"
        )),
        "run stream should emit interaction resolution from initial decisions: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_pending"
        )),
        "run stream should replay suspended call from initial decisions: {events:?}"
    );
}

#[tokio::test]
async fn run_stream_persists_run_lifecycle_done_status() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-run-lifecycle-done";

    let thread = Thread::new(thread_id).with_message(crate::contracts::thread::Message::user("hi"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-lifecycle-done".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![],
        })
        .await
        .unwrap();
    let _events: Vec<_> = run.events.collect().await;

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    assert_eq!(rebuilt["__run"]["id"], json!("run-lifecycle-done"));
    assert_eq!(rebuilt["__run"]["status"], json!("done"));
    assert_eq!(rebuilt["__run"]["done_reason"], json!("behavior_requested"));
}

#[tokio::test]
async fn run_stream_persists_run_lifecycle_waiting_status_for_suspension() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-run-lifecycle-waiting";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": {
                            "message": "still waiting"
                        }
                    },
                    "arguments": {
                            "message": "still waiting"
                        },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": {
                            "message": "still waiting"
                        }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-lifecycle-waiting".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![],
        })
        .await
        .unwrap();
    let _events: Vec<_> = run.events.collect().await;

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    assert_eq!(rebuilt["__run"]["id"], json!("run-lifecycle-waiting"));
    assert_eq!(rebuilt["__run"]["status"], json!("waiting"));
    assert!(
        rebuilt["__run"]["done_reason"].is_null(),
        "waiting status should not carry done reason: {}",
        rebuilt["__run"]
    );
}

#[tokio::test]
async fn run_stream_initial_decisions_denied_returns_tool_error_and_clears_suspended() {
    use futures::StreamExt;
    use tirea_contract::runtime::suspended_calls_from_state;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-initial-denied";
    let run_id = "run-initial-denied";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": { "message": "denied" }
                    },
                    "arguments": { "message": "denied" },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": { "message": "denied" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some(run_id.to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "call_pending",
                crate::contracts::io::ResumeDecisionAction::Cancel,
                json!(false),
            )],
        })
        .await
        .unwrap();
    let events: Vec<_> = run.events.collect().await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, result }
                if target_id == "call_pending" && result == &json!(false)
        )),
        "denied decision should be emitted as ToolCallResumed: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, result, .. }
                if id == "call_pending" && result.is_error()
        )),
        "denied decision should produce ToolCallDone error result: {events:?}"
    );

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    assert!(
        !suspended_calls_from_state(&rebuilt).contains_key("call_pending"),
        "resolved denied call must be removed from suspended map: {rebuilt:?}"
    );
}

#[tokio::test]
async fn run_stream_initial_decisions_cancelled_returns_tool_error_and_clears_suspended() {
    use futures::StreamExt;
    use tirea_contract::runtime::suspended_calls_from_state;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-initial-cancelled";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": { "message": "cancelled" }
                    },
                    "arguments": { "message": "cancelled" },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": { "message": "cancelled" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let cancel_payload = json!({
        "status": "cancelled",
        "reason": "user canceled"
    });
    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-initial-cancelled".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "call_pending",
                crate::contracts::io::ResumeDecisionAction::Cancel,
                cancel_payload.clone(),
            )],
        })
        .await
        .unwrap();
    let events: Vec<_> = run.events.collect().await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallResumed { target_id, result }
                if target_id == "call_pending" && result == &cancel_payload
        )),
        "cancelled decision should be emitted as ToolCallResumed: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, result, .. }
                if id == "call_pending" && result.is_error()
        )),
        "cancelled decision should produce ToolCallDone error result: {events:?}"
    );

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    assert!(
        !suspended_calls_from_state(&rebuilt).contains_key("call_pending"),
        "resolved cancelled call must be removed from suspended map: {rebuilt:?}"
    );
}

#[tokio::test]
async fn run_stream_initial_decisions_partial_match_keeps_unresolved_suspended_call() {
    use futures::StreamExt;
    use tirea_contract::runtime::suspended_calls_from_state;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-initial-partial";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_approved": {
                "suspended_call": {
                    "call_id": "call_approved",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_approved",
                        "action": "confirm",
                        "parameters": { "message": "approve me" }
                    },
                    "arguments": { "message": "approve me" },
                    "pending": {
                        "id": "call_approved",
                        "name": "echo",
                        "arguments": { "message": "approve me" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            },
            "call_waiting": {
                "suspended_call": {
                    "call_id": "call_waiting",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_waiting",
                        "action": "confirm",
                        "parameters": { "message": "still waiting" }
                    },
                    "arguments": { "message": "still waiting" },
                    "pending": {
                        "id": "call_waiting",
                        "name": "echo",
                        "arguments": { "message": "still waiting" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-initial-partial".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "call_approved",
                crate::contracts::io::ResumeDecisionAction::Resume,
                json!(true),
            )],
        })
        .await
        .unwrap();
    let events: Vec<_> = run.events.collect().await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_approved"
        )),
        "approved call should be replayed: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::RunFinish {
                termination: TerminationReason::Suspended,
                ..
            }
        )),
        "run should remain suspended when unresolved calls remain: {events:?}"
    );

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    let suspended_calls = suspended_calls_from_state(&rebuilt);
    assert!(
        !suspended_calls.contains_key("call_approved"),
        "resolved call should be removed: {suspended_calls:?}"
    );
    assert!(
        suspended_calls.contains_key("call_waiting"),
        "unresolved call should remain suspended: {suspended_calls:?}"
    );
}

#[tokio::test]
async fn run_stream_batch_approval_mode_waits_for_all_suspended_decisions_before_replay() {
    use futures::StreamExt;
    use tirea_contract::runtime::suspended_calls_from_state;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os_with_mode(
        storage.clone(),
        Some(ToolExecutionMode::ParallelBatchApproval),
    );
    let thread_id = "t-initial-batch";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_approved": {
                "suspended_call": {
                    "call_id": "call_approved",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_approved",
                        "action": "confirm",
                        "parameters": { "message": "approve me" }
                    },
                    "arguments": { "message": "approve me" },
                    "pending": {
                        "id": "call_approved",
                        "name": "echo",
                        "arguments": { "message": "approve me" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            },
            "call_waiting": {
                "suspended_call": {
                    "call_id": "call_waiting",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_waiting",
                        "action": "confirm",
                        "parameters": { "message": "still waiting" }
                    },
                    "arguments": { "message": "still waiting" },
                    "pending": {
                        "id": "call_waiting",
                        "name": "echo",
                        "arguments": { "message": "still waiting" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let first_run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-initial-batch-1".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "call_approved",
                crate::contracts::io::ResumeDecisionAction::Resume,
                json!(true),
            )],
        })
        .await
        .unwrap();
    let first_events: Vec<_> = first_run.events.collect().await;

    assert!(
        !first_events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_approved"
        )),
        "batch mode should not replay partially approved suspended calls: {first_events:?}"
    );
    assert!(
        first_events.iter().any(|event| matches!(
            event,
            AgentEvent::RunFinish {
                termination: TerminationReason::Suspended,
                ..
            }
        )),
        "run should remain suspended while unresolved calls exist: {first_events:?}"
    );

    let saved_after_first = storage.load(thread_id).await.unwrap().unwrap();
    let state_after_first = saved_after_first.thread.rebuild_state().unwrap();
    assert_run_lifecycle_state(&state_after_first, "run-initial-batch-1", "waiting", None);
    let calls_after_first = suspended_calls_from_state(&state_after_first);
    assert!(
        calls_after_first.contains_key("call_approved")
            && calls_after_first.contains_key("call_waiting"),
        "both suspended calls should remain until approvals are complete: {calls_after_first:?}"
    );

    let second_run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-initial-batch-2".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "call_waiting",
                crate::contracts::io::ResumeDecisionAction::Resume,
                json!(true),
            )],
        })
        .await
        .unwrap();
    let second_events: Vec<_> = second_run.events.collect().await;

    assert!(
        second_events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_approved"
        )),
        "batch completion should replay call_approved: {second_events:?}"
    );
    assert!(
        second_events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDone { id, .. } if id == "call_waiting"
        )),
        "batch completion should replay call_waiting: {second_events:?}"
    );

    let saved_after_second = storage.load(thread_id).await.unwrap().unwrap();
    let state_after_second = saved_after_second.thread.rebuild_state().unwrap();
    assert_run_lifecycle_state(
        &state_after_second,
        "run-initial-batch-2",
        "done",
        Some("behavior_requested"),
    );
    assert!(
        suspended_calls_from_state(&state_after_second).is_empty(),
        "all suspended calls should be cleared after full batch approval replay"
    );
}

#[tokio::test]
async fn run_stream_initial_decisions_ignore_unknown_target() {
    use futures::StreamExt;
    use tirea_contract::runtime::suspended_calls_from_state;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-initial-unknown";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": { "message": "still waiting" }
                    },
                    "arguments": { "message": "still waiting" },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": { "message": "still waiting" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-initial-unknown".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision_for(
                "unknown_call",
                crate::contracts::io::ResumeDecisionAction::Resume,
                json!(true),
            )],
        })
        .await
        .unwrap();
    let events: Vec<_> = run.events.collect().await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallResumed { .. })),
        "unknown target should not emit ToolCallResumed: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::RunFinish {
                termination: TerminationReason::Suspended,
                ..
            }
        )),
        "run should remain suspended with unresolved original call: {events:?}"
    );

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    assert!(
        suspended_calls_from_state(&rebuilt).contains_key("call_pending"),
        "unknown target must not clear suspended call: {rebuilt:?}"
    );
}

#[tokio::test]
async fn run_stream_duplicate_initial_decisions_are_idempotent() {
    use futures::StreamExt;
    use tirea_contract::runtime::suspended_calls_from_state;
    use tirea_store_adapters::MemoryStore;

    let storage = Arc::new(MemoryStore::new());
    let os = make_decision_test_os(storage.clone());
    let thread_id = "t-initial-duplicate";

    let pending_state = json!({
        "__tool_call_scope": {
            "call_pending": {
                "suspended_call": {
                    "call_id": "call_pending",
                    "tool_name": "echo",
                    "suspension": {
                        "id": "call_pending",
                        "action": "confirm",
                        "parameters": { "message": "idempotent" }
                    },
                    "arguments": { "message": "idempotent" },
                    "pending": {
                        "id": "call_pending",
                        "name": "echo",
                        "arguments": { "message": "idempotent" }
                    },
                    "resume_mode": "replay_tool_call"
                }
            }
        }
    });
    let thread = Thread::with_initial_state(thread_id, pending_state)
        .with_message(crate::contracts::thread::Message::user("resume"));
    storage.create(&thread).await.unwrap();

    let decision: tirea_contract::ToolCallDecision = decision_for(
        "call_pending",
        crate::contracts::io::ResumeDecisionAction::Resume,
        json!(true),
    );
    let run = os
        .run_stream(RunRequest {
            agent_id: "a1".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some("run-initial-duplicate".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![decision.clone(), decision],
        })
        .await
        .unwrap();
    let events: Vec<_> = run.events.collect().await;

    let resumed_count = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::ToolCallResumed { target_id, .. } if target_id == "call_pending"
            )
        })
        .count();
    let done_count = events
        .iter()
        .filter(
            |event| matches!(event, AgentEvent::ToolCallDone { id, .. } if id == "call_pending"),
        )
        .count();
    assert_eq!(resumed_count, 1, "duplicate decisions should resume once");
    assert_eq!(done_count, 1, "duplicate decisions should replay tool once");

    let saved = storage.load(thread_id).await.unwrap().unwrap();
    let rebuilt = saved.thread.rebuild_state().unwrap();
    assert!(
        !suspended_calls_from_state(&rebuilt).contains_key("call_pending"),
        "idempotent replay should clear suspended call once: {rebuilt:?}"
    );
}

#[tokio::test]
async fn run_stream_checkpoint_append_failure_keeps_persisted_prefix_consistent() {
    use futures::StreamExt;

    let storage = Arc::new(FailOnNthAppendStorage::new(2));
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_with_run_end_patch",
            Arc::new(TerminateWithRunEndPatchPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_with_run_end_patch"),
        )
        .build()
        .unwrap();

    let request = RunRequest {
        agent_id: "a1".to_string(),
        thread_id: Some("t-checkpoint-fail".to_string()),
        run_id: Some("run-checkpoint-fail".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        state: Some(json!({"base": 1})),
        messages: vec![crate::contracts::thread::Message::user("hello")],
        initial_decisions: vec![],
    };

    let run_stream = os.run_stream(request).await.unwrap();
    let events: Vec<_> = run_stream.events.collect().await;

    assert!(
        matches!(events.first(), Some(AgentEvent::RunStart { .. })),
        "expected RunStart as first event, got: {events:?}"
    );
    let err_msg = events
        .iter()
        .find_map(|ev| match ev {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("expected checkpoint append failure to emit AgentEvent::Error");
    assert!(
        err_msg.contains("checkpoint append failed"),
        "unexpected error message: {err_msg}"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::RunFinish { .. })),
        "RunFinish must not be emitted after checkpoint append failure: {events:?}"
    );

    let head = storage.load("t-checkpoint-fail").await.unwrap().unwrap();
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["base"], json!(1));
    assert_run_lifecycle_state(&state, "run-checkpoint-fail", "running", None);
    assert!(
        state.get("run_end_marker").is_none(),
        "failed checkpoint must not persist RunEnd patch"
    );
    assert_eq!(
        head.thread.messages.len(),
        1,
        "only user message delta should be persisted before checkpoint failure"
    );
    assert_eq!(
        head.thread.messages[0].role,
        crate::contracts::thread::Role::User
    );
    assert_eq!(
        head.thread.messages[0].content.as_str(),
        "hello",
        "unexpected persisted user message content"
    );
    assert_eq!(head.version, 1, "failed append must not advance version");
    assert_eq!(
        storage.append_call_count(),
        2,
        "expected one successful user append and one failed checkpoint append"
    );
}

#[tokio::test]
async fn run_stream_checkpoint_failure_on_existing_thread_keeps_pre_checkpoint_state() {
    use futures::StreamExt;

    let storage = Arc::new(FailOnNthAppendStorage::new(2));
    let initial = Thread::with_initial_state("t-existing-fail", json!({"counter": 5}));
    storage.create(&initial).await.unwrap();

    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "terminate_with_run_end_patch",
            Arc::new(TerminateWithRunEndPatchPlugin),
        )
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("terminate_with_run_end_patch"),
        )
        .build()
        .unwrap();

    let request = RunRequest {
        agent_id: "a1".to_string(),
        thread_id: Some("t-existing-fail".to_string()),
        run_id: Some("run-existing-fail".to_string()),
        parent_run_id: None,
        parent_thread_id: None,
        resource_id: None,
        state: None,
        messages: vec![],
        initial_decisions: vec![],
    };

    let run_stream = os.run_stream(request).await.unwrap();
    let events: Vec<_> = run_stream.events.collect().await;

    assert!(
        matches!(events.first(), Some(AgentEvent::RunStart { .. })),
        "expected RunStart as first event, got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::Error { message, .. } if message.contains("checkpoint append failed"))),
        "checkpoint failure must emit AgentEvent::Error: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, AgentEvent::RunFinish { .. })),
        "RunFinish must not be emitted after checkpoint append failure: {events:?}"
    );

    let head = storage.load("t-existing-fail").await.unwrap().unwrap();
    let state = head.thread.rebuild_state().unwrap();
    assert_eq!(state["counter"], json!(5));
    assert_run_lifecycle_state(&state, "run-existing-fail", "running", None);
    assert!(
        head.thread.state.get("run_end_marker").is_none(),
        "failed checkpoint must not persist RunEnd patch"
    );
    assert_eq!(
        head.version, 1,
        "failed checkpoint append must keep prior run-start commit"
    );
    assert_eq!(storage.append_call_count(), 2);
}

#[test]
fn build_errors_on_reserved_plugin_id_agent_recovery_in_builder_agent() {
    let err = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_behavior_id("agent_recovery"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentReservedBehaviorId { ref agent_id, ref behavior_id }
        if agent_id == "a1" && behavior_id == "agent_recovery"
    ));
}

#[test]
fn builder_with_agent_state_store_exposes_accessor() {
    let agent_state_store = Arc::new(tirea_store_adapters::MemoryStore::new())
        as Arc<dyn crate::contracts::storage::ThreadStore>;
    let os = AgentOs::builder()
        .with_agent_state_store(agent_state_store)
        .build()
        .unwrap();
    assert!(os.agent_state_store().is_some());
}

#[tokio::test]
async fn load_agent_state_without_store_returns_not_configured() {
    let os = AgentOs::builder().build().unwrap();
    let err = os.load_thread("t1").await.unwrap_err();
    assert!(matches!(err, AgentOsRunError::AgentStateStoreNotConfigured));
}

#[tokio::test]
async fn prepare_run_scope_tool_registry_adds_new_tool() {
    struct FrontendTool;
    #[async_trait::async_trait]
    impl Tool for FrontendTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("frontend_action", "Frontend Action", "frontend stub")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("frontend_action", json!({})))
        }
    }

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let mut registry = InMemoryToolRegistry::new();
    registry.register(Arc::new(FrontendTool)).unwrap();
    let mut resolved = os.resolve("a1").unwrap();
    resolved.overlay_tools(registry.snapshot());

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-scope-registry".to_string()),
                run_id: Some("run-scope-registry".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    assert!(prepared.tools.contains_key("frontend_action"));
    // Backend tools (agent_run, agent_stop) should also be present
    assert!(prepared.tools.contains_key("agent_run"));
}

#[tokio::test]
async fn prepare_run_scope_tool_registry_omits_shadowed() {
    struct ShadowTool;
    #[async_trait::async_trait]
    impl Tool for ShadowTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("agent_run", "Shadow Agent Run", "frontend stub")
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("agent_run", json!({})))
        }
    }

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let mut registry = InMemoryToolRegistry::new();
    registry.register(Arc::new(ShadowTool)).unwrap();
    let mut resolved = os.resolve("a1").unwrap();
    resolved.overlay_tools(registry.snapshot());

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-scope-shadow".to_string()),
                run_id: Some("run-scope-shadow".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    // Backend agent_run wins — frontend shadow is omitted (overlay uses insert-if-absent)
    assert!(prepared.tools.contains_key("agent_run"));
    let tool = prepared.tools.get("agent_run").unwrap();
    assert_ne!(tool.descriptor().description, "frontend stub");
}

#[tokio::test]
async fn prepare_run_scope_appends_plugins() {
    #[derive(Debug)]
    struct RunScopedPlugin;

    #[async_trait::async_trait]
    impl AgentBehavior for RunScopedPlugin {
        fn id(&self) -> &str {
            "run_scoped"
        }
    }

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let mut resolved = os.resolve("a1").unwrap();
    let new_b = Arc::new(RunScopedPlugin) as Arc<dyn AgentBehavior>;
    let id = format!("{}+{}", resolved.agent.behavior.id(), new_b.id());
    resolved.agent.behavior =
        super::compose_behaviors(id, vec![resolved.agent.behavior.clone(), new_b]);

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-scope-plugin".to_string()),
                run_id: Some("run-scope-plugin".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    let behavior_ids = prepared.agent.behavior().behavior_ids();
    assert!(behavior_ids.iter().any(|id| *id == "run_scoped"));
    // System plugins should still be present
    assert!(behavior_ids.iter().any(|id| *id == "agent_tools"));
}

#[tokio::test]
async fn prepare_run_scope_rejects_duplicate_plugin_id() {
    #[derive(Debug)]
    struct DupPlugin;

    #[async_trait::async_trait]
    impl AgentBehavior for DupPlugin {
        fn id(&self) -> &str {
            "agent_tools"
        }
    }

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage)
        .with_agent("a1", AgentDefinition::new("gpt-4o-mini"))
        .build()
        .unwrap();

    let mut resolved = os.resolve("a1").unwrap();
    let new_b = Arc::new(DupPlugin) as Arc<dyn AgentBehavior>;
    let id = format!("{}+{}", resolved.agent.behavior.id(), new_b.id());
    resolved.agent.behavior =
        super::compose_behaviors(id, vec![resolved.agent.behavior.clone(), new_b]);

    let result = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some("t-scope-dup-plugin".to_string()),
                run_id: Some("run-scope-dup-plugin".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![Message::user("hello")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await;
    let err = result.err().expect("duplicate plugin id should error");
    assert!(matches!(
        err,
        AgentOsRunError::Resolve(AgentOsResolveError::Wiring(
            AgentOsWiringError::BehaviorAlreadyInstalled(ref id)
        )) if id == "agent_tools"
    ));
}

#[derive(Debug)]
struct BundleTestTool;

#[async_trait::async_trait]
impl Tool for BundleTestTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("dup_tool", "Duplicate Tool", "bundle test tool")
            .with_parameters(json!({"type":"object"}))
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("dup_tool", json!({"ok": true})))
    }
}

#[derive(Clone)]
struct ToolConflictBundle;

impl RegistryBundle for ToolConflictBundle {
    fn id(&self) -> &str {
        "tool_conflict_bundle"
    }

    fn tool_definitions(&self) -> HashMap<String, Arc<dyn Tool>> {
        HashMap::from([(
            "dup_tool".to_string(),
            Arc::new(BundleTestTool) as Arc<dyn Tool>,
        )])
    }
}

#[test]
fn builder_fails_fast_on_bundle_registry_conflict() {
    let err = AgentOs::builder()
        .with_tools(HashMap::from([(
            "dup_tool".to_string(),
            Arc::new(BundleTestTool) as Arc<dyn Tool>,
        )]))
        .with_bundle(Arc::new(ToolConflictBundle))
        .build()
        .expect_err("duplicate tool id between base and bundle should fail");

    assert!(matches!(
        err,
        AgentOsBuildError::Bundle(BundleComposeError::DuplicateId {
            bundle_id,
            kind: BundleRegistryKind::Tool,
            id,
        }) if bundle_id == "tool_conflict_bundle" && id == "dup_tool"
    ));
}

// ── StopPolicyRegistry build-time validation tests ────────────────────────

#[test]
fn build_errors_if_agent_references_missing_stop_condition() {
    let err = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_stop_condition_id("missing_sc"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentStopConditionNotFound { ref agent_id, ref stop_condition_id }
        if agent_id == "a1" && stop_condition_id == "missing_sc"
    ));
}

#[test]
fn build_errors_on_duplicate_stop_condition_ref_in_builder_agent() {
    use crate::contracts::StoppedReason;
    use crate::orchestrator::StopPolicyInput;

    #[derive(Debug)]
    struct MockStop;

    impl crate::orchestrator::StopPolicy for MockStop {
        fn id(&self) -> &str {
            "mock_stop"
        }
        fn evaluate(&self, _input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
            None
        }
    }

    let err = AgentOs::builder()
        .with_stop_policy("sc1", Arc::new(MockStop))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini")
                .with_stop_condition_id("sc1")
                .with_stop_condition_id("sc1"),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentDuplicateStopConditionRef { ref agent_id, ref stop_condition_id }
        if agent_id == "a1" && stop_condition_id == "sc1"
    ));
}

#[test]
fn build_errors_on_empty_stop_condition_ref_in_builder_agent() {
    let err = AgentOs::builder()
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_stop_condition_id(""),
        )
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        AgentOsBuildError::AgentEmptyStopConditionRef { ref agent_id }
        if agent_id == "a1"
    ));
}

#[tokio::test]
async fn resolve_wires_stop_conditions_from_registry() {
    use crate::contracts::StoppedReason;
    use crate::orchestrator::StopPolicyInput;

    #[derive(Debug)]
    struct TestStopPolicy;

    impl crate::orchestrator::StopPolicy for TestStopPolicy {
        fn id(&self) -> &str {
            "test_stop"
        }
        fn evaluate(&self, _input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
            None
        }
    }

    let os = AgentOs::builder()
        .with_stop_policy("sc1", Arc::new(TestStopPolicy))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_stop_condition_id("sc1"),
        )
        .build()
        .unwrap();

    let resolved = os.resolve("a1").unwrap();
    assert!(
        resolved
            .agent
            .behavior
            .behavior_ids()
            .iter()
            .any(|id| *id == "stop_policy"),
        "stop policies should be handled by stop_policy behavior"
    );
}

#[tokio::test]
async fn prepare_run_cleans_up_run_scoped_state_between_consecutive_runs() {
    use futures::StreamExt;
    use tirea_store_adapters::MemoryStore;

    #[derive(Debug)]
    struct AlwaysStopPolicy;

    impl crate::orchestrator::StopPolicy for AlwaysStopPolicy {
        fn id(&self) -> &str {
            "always_stop"
        }
        fn evaluate(
            &self,
            _input: &crate::orchestrator::StopPolicyInput<'_>,
        ) -> Option<crate::contracts::StoppedReason> {
            Some(crate::contracts::StoppedReason::with_detail(
                "custom", "always",
            ))
        }
    }

    #[derive(Debug)]
    struct OneShotLlm;

    #[async_trait]
    impl crate::runtime::loop_runner::LlmExecutor for OneShotLlm {
        async fn exec_chat_response(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<genai::chat::ChatResponse> {
            let model_iden =
                genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock-model");
            Ok(genai::chat::ChatResponse {
                content: genai::chat::MessageContent::from_text("ok".to_string()),
                reasoning_content: None,
                model_iden: model_iden.clone(),
                provider_model_iden: model_iden,
                usage: genai::chat::Usage::default(),
                captured_raw_body: None,
            })
        }

        async fn exec_chat_stream_events(
            &self,
            _model: &str,
            _chat_req: genai::chat::ChatRequest,
            _options: Option<&genai::chat::ChatOptions>,
        ) -> genai::Result<crate::runtime::loop_runner::LlmEventStream> {
            Ok(Box::pin(futures::stream::empty()))
        }

        fn name(&self) -> &'static str {
            "one_shot_llm"
        }
    }

    let storage = Arc::new(MemoryStore::new());
    let os = AgentOs::builder()
        .with_stop_policy("always", Arc::new(AlwaysStopPolicy))
        .with_agent(
            "a1",
            AgentDefinition::new("gpt-4o-mini").with_stop_condition_id("always"),
        )
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .build()
        .unwrap();

    let thread_id = "t-run-scope-cleanup";

    // --- Run 1: creates StopPolicyRuntimeState via after_inference ---
    let mut resolved = os.resolve("a1").unwrap();
    resolved.agent = resolved.agent.with_llm_executor(
        Arc::new(OneShotLlm) as Arc<dyn crate::runtime::loop_runner::LlmExecutor>
    );
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![crate::contracts::thread::Message::user("go")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();
    let run = AgentOs::execute_prepared(prepared).unwrap();
    let _events: Vec<_> = run.events.collect().await;

    // Verify run-1 persisted StopPolicyRuntimeState
    let head = storage.load(thread_id).await.unwrap().unwrap();
    let state_after_run1 = head.thread.rebuild_state().unwrap();
    assert!(
        !state_after_run1["__kernel"]["stop_policy_runtime"].is_null(),
        "run-1 should have persisted stop_policy_runtime state: {state_after_run1}",
    );

    // --- Run 2: prepare_run should clean up run-scoped state ---
    let mut resolved2 = os.resolve("a1").unwrap();
    resolved2.agent = resolved2.agent.with_llm_executor(
        Arc::new(OneShotLlm) as Arc<dyn crate::runtime::loop_runner::LlmExecutor>
    );
    let prepared2 = os
        .prepare_run(
            RunRequest {
                agent_id: "a1".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some("run-2".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                state: None,
                messages: vec![crate::contracts::thread::Message::user("go again")],
                initial_decisions: vec![],
            },
            resolved2,
        )
        .await
        .unwrap();

    // After prepare_run, the persisted state should have cleaned up
    // __kernel.stop_policy_runtime from run-1. Verify via storage.
    let head = storage.load(thread_id).await.unwrap().unwrap();
    let state_after_prepare = head.thread.rebuild_state().unwrap();
    assert!(
        state_after_prepare["__kernel"]["stop_policy_runtime"].is_null(),
        "prepare_run should have cleaned run-scoped stop_policy_runtime from previous run: {state_after_prepare}",
    );
    // The new __run lifecycle should be set to "running" for run-2.
    assert_run_lifecycle_state(&state_after_prepare, "run-2", "running", None);

    // Complete run-2 to verify it works end-to-end.
    let run2 = AgentOs::execute_prepared(prepared2).unwrap();
    let _events2: Vec<_> = run2.events.collect().await;

    let head = storage.load(thread_id).await.unwrap().unwrap();
    let state_after_run2 = head.thread.rebuild_state().unwrap();
    // run-2 should have re-created its own stop_policy_runtime state
    assert!(
        !state_after_run2["__kernel"]["stop_policy_runtime"].is_null(),
        "run-2 should have its own stop_policy_runtime state: {state_after_run2}",
    );
    assert_run_lifecycle_state(&state_after_run2, "run-2", "done", Some("stopped:custom"));
}
