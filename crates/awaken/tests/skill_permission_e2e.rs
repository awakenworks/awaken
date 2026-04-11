#![allow(missing_docs)]
//! End-to-end tests verifying skill permission elevation within a run.
//!
//! Scenario 1 (with skill activation):
//!   - Agent has deny-all permission policy
//!   - Skill "power-skill" declares `allowed-tools: dangerous_tool`
//!   - LLM calls "skill" to activate "power-skill", then calls "dangerous_tool"
//!   - Both tool calls succeed (permission elevated by skill)
//!
//! Scenario 2 (without skill activation):
//!   - Same deny-all policy
//!   - LLM directly calls "dangerous_tool" without skill activation
//!   - "dangerous_tool" is blocked by permission

use std::collections::HashMap;
use std::sync::Arc;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use awaken::agent::state::{
    ContextMessageStore, ContextThrottleState, RunLifecycle, ToolCallStates,
};
use awaken::contract::content::ContentBlock;
use awaken::contract::event::AgentEvent;
use awaken::contract::event_sink::VecEventSink;
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::identity::{RunIdentity, RunOrigin};
use awaken::contract::inference::{StopReason, StreamResult};
use awaken::contract::lifecycle::TerminationReason;
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::suspension::ToolCallOutcome;
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::loop_runner::{AgentLoopParams, build_agent_env, run_agent_loop};
use awaken::registry::AgentSpec;
use awaken::*;
use awaken::{AgentResolver, ResolvedAgent, RuntimeError};
use awaken_runtime::execution::ParallelToolExecutor;

use awaken::ext_permission::PermissionPlugin;
use awaken::ext_skills::{
    ActiveSkillInstructionsPlugin, EmbeddedSkill, EmbeddedSkillData, InMemorySkillRegistry,
    SkillActivateTool, SkillDiscoveryPlugin,
};

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

struct ScriptedLlm {
    responses: std::sync::Mutex<Vec<StreamResult>>,
    captured_requests: Mutex<Vec<InferenceRequest>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            captured_requests: Mutex::new(Vec::new()),
        }
    }

    fn captured_requests(&self) -> Vec<InferenceRequest> {
        self.captured_requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        req: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.captured_requests.lock().unwrap().push(req);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(StreamResult {
                content: vec![ContentBlock::text("Nothing more.")],
                tool_calls: vec![],
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        } else {
            Ok(responses.remove(0))
        }
    }
    fn name(&self) -> &str {
        "scripted"
    }
}

// ---------------------------------------------------------------------------
// DangerousTool
// ---------------------------------------------------------------------------

struct DangerousTool;

#[async_trait]
impl Tool for DangerousTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "dangerous_tool",
            "Dangerous Tool",
            "A tool that requires permission elevation",
        )
    }
    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("dangerous_tool", json!({"status": "executed"})).into())
    }
}

struct CountingDangerousTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingDangerousTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "dangerous_tool",
            "Dangerous Tool",
            "A tool that requires permission elevation",
        )
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::success("dangerous_tool", json!({"status": "executed"})).into())
    }
}

struct CountingWeatherTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingWeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_weather",
            "Get Weather",
            "Return weather for a location",
        )
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::success(
            "get_weather",
            json!({
                "location": args["location"].clone(),
                "temp": 25,
                "condition": "sunny"
            }),
        )
        .into())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct LoopStatePlugin;
impl Plugin for LoopStatePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "loop-state" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(StateKeyOptions::default())?;
        r.register_key::<ContextThrottleState>(StateKeyOptions::default())?;
        r.register_key::<ContextMessageStore>(StateKeyOptions::default())?;
        Ok(())
    }
}

fn make_runtime() -> PhaseRuntime {
    let store = StateStore::new();
    let rt = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(LoopStatePlugin).unwrap();
    rt
}

struct FixedResolver {
    agent: ResolvedAgent,
    user_plugins: Vec<Arc<dyn Plugin>>,
}

impl FixedResolver {
    fn with_plugins(agent: ResolvedAgent, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        Self {
            agent,
            user_plugins: plugins,
        }
    }
}

impl AgentResolver for FixedResolver {
    fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let mut agent = self.agent.clone();
        agent.env = build_agent_env(&self.user_plugins, &agent)?;
        Ok(agent)
    }
}

fn id() -> RunIdentity {
    RunIdentity::new(
        "t1".into(),
        None,
        "r1".into(),
        None,
        "agent".into(),
        RunOrigin::User,
    )
}

fn tool_step(calls: Vec<ToolCall>) -> StreamResult {
    StreamResult {
        content: vec![],
        tool_calls: calls,
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }
}

fn text_step(text: &str) -> StreamResult {
    StreamResult {
        content: vec![ContentBlock::text(text)],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }
}

// ---------------------------------------------------------------------------
// Skill definition
// ---------------------------------------------------------------------------

const POWER_SKILL_MD: &str = "\
---
name: power-skill
description: A skill that elevates dangerous_tool permission
allowed-tools: dangerous_tool
---
# Power Skill

This skill grants access to dangerous_tool.
";

fn make_skill_registry() -> Arc<InMemorySkillRegistry> {
    let skills = EmbeddedSkill::from_static_slice(&[EmbeddedSkillData {
        skill_md: POWER_SKILL_MD,
        references: &[],
        assets: &[],
    }])
    .unwrap();
    Arc::new(InMemorySkillRegistry::from_skills(skills))
}

fn make_agent_spec_deny_all() -> AgentSpec {
    make_agent_spec_deny_all_with_extra_allowed_tools(&[])
}

fn make_agent_spec_deny_all_with_extra_allowed_tools(extra_allowed_tools: &[&str]) -> AgentSpec {
    let mut rules = vec![
        json!({ "tool": "skill", "behavior": "allow" }),
        json!({ "tool": "load_skill_resource", "behavior": "allow" }),
        json!({ "tool": "skill_script", "behavior": "allow" }),
    ];
    rules.extend(
        extra_allowed_tools
            .iter()
            .map(|tool| json!({ "tool": tool, "behavior": "allow" })),
    );

    AgentSpec {
        id: "test".into(),
        model_id: "m".into(),
        system_prompt: "sys".into(),
        max_rounds: 16,
        max_continuation_retries: 2,
        reasoning_effort: None,
        context_policy: None,
        plugin_ids: Vec::new(),
        active_hook_filter: Default::default(),
        allowed_tools: None,
        excluded_tools: None,
        endpoint: None,
        delegates: Vec::new(),
        sections: HashMap::from([(
            "permission".to_string(),
            json!({
                "default_behavior": "deny",
                "rules": rules
            }),
        )]),
        registry: None,
    }
}

// ===========================================================================
// TEST 1: Skill activation elevates permission for dangerous_tool
// ===========================================================================

#[tokio::test]
async fn skill_activation_elevates_permission_for_dangerous_tool() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());

    let llm = Arc::new(ScriptedLlm::new(vec![
        // Step 1: LLM calls "skill" tool to activate "power-skill"
        tool_step(vec![ToolCall::new(
            "c1",
            "skill",
            json!({"skill": "power-skill"}),
        )]),
        // Step 2: LLM calls "dangerous_tool" (now allowed by skill elevation)
        tool_step(vec![ToolCall::new("c2", "dangerous_tool", json!({}))]),
        // Step 3: LLM produces final text
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    // Register tools directly on the agent: dangerous_tool + skill activate tool
    let agent = agent
        .with_tool(Arc::new(DangerousTool))
        .with_tool(Arc::new(SkillActivateTool::new(registry)));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // The run should complete naturally (not blocked)
    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "run should complete naturally after skill activation elevates permission"
    );

    // Verify both tool calls succeeded via events
    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone {
                id,
                outcome,
                result,
                ..
            } = e
            {
                Some((id.clone(), *outcome, result.clone()))
            } else {
                None
            }
        })
        .collect();

    // We expect two successful ToolCallDone events (skill activation + dangerous_tool)
    assert!(
        tool_dones.len() >= 2,
        "expected at least 2 ToolCallDone events, got {}",
        tool_dones.len()
    );

    let by_id: HashMap<String, (ToolCallOutcome, ToolResult)> = tool_dones
        .into_iter()
        .map(|(id, outcome, result)| (id, (outcome, result)))
        .collect();

    // Skill activation should succeed
    let (skill_outcome, _) = by_id
        .get("c1")
        .expect("skill tool call c1 should be present");
    assert_eq!(
        *skill_outcome,
        ToolCallOutcome::Succeeded,
        "skill activation should succeed"
    );

    // dangerous_tool should succeed (elevated by skill)
    let (danger_outcome, _) = by_id
        .get("c2")
        .expect("dangerous_tool call c2 should be present");
    assert_eq!(
        *danger_outcome,
        ToolCallOutcome::Succeeded,
        "dangerous_tool should succeed after skill elevation"
    );
}

// ===========================================================================
// TEST 2: Without skill activation, dangerous_tool is blocked
// ===========================================================================

#[tokio::test]
async fn dangerous_tool_blocked_without_skill_activation() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry);

    let llm = Arc::new(ScriptedLlm::new(vec![
        // LLM directly calls "dangerous_tool" without skill activation
        tool_step(vec![ToolCall::new("c1", "dangerous_tool", json!({}))]),
        // Text step (should not be reached if blocked)
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent.with_tool(Arc::new(DangerousTool));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    // The run should be blocked (permission denied)
    assert!(
        matches!(result.termination, TerminationReason::Blocked(_)),
        "run should be blocked when dangerous_tool is called without skill activation, got: {:?}",
        result.termination
    );

    // Verify the tool call was failed/blocked via events
    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone {
                id,
                outcome,
                result,
                ..
            } = e
            {
                Some((id.clone(), *outcome, result.clone()))
            } else {
                None
            }
        })
        .collect();

    // The dangerous_tool call should show as Failed (blocked by permission)
    let danger_done = tool_dones
        .iter()
        .find(|(id, _, _)| id == "c1")
        .expect("dangerous_tool call c1 should have a ToolCallDone event");
    assert_eq!(
        danger_done.1,
        ToolCallOutcome::Failed,
        "dangerous_tool should be failed/blocked by permission"
    );
}

// ===========================================================================
// TEST 3: Permission elevation persists across steps within a run
// ===========================================================================

#[tokio::test]
async fn permission_elevation_persists_across_steps_within_run() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());

    let llm = Arc::new(ScriptedLlm::new(vec![
        // Step 1: LLM calls "skill" to activate "power-skill"
        tool_step(vec![ToolCall::new(
            "c1",
            "skill",
            json!({"skill": "power-skill"}),
        )]),
        // Step 2: LLM calls "dangerous_tool" (elevated by skill)
        tool_step(vec![ToolCall::new("c2", "dangerous_tool", json!({}))]),
        // Step 3: LLM calls "dangerous_tool" again (elevation should persist)
        tool_step(vec![ToolCall::new("c3", "dangerous_tool", json!({}))]),
        // Step 4: Final text
        text_step("all done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(DangerousTool))
        .with_tool(Arc::new(SkillActivateTool::new(registry)));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "run should complete naturally — elevation must persist across all steps"
    );

    let events = sink.take();
    let tool_dones: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ToolCallDone {
                id,
                outcome,
                result,
                ..
            } = e
            {
                Some((id.clone(), *outcome, result.clone()))
            } else {
                None
            }
        })
        .collect();

    let by_id: HashMap<String, (ToolCallOutcome, ToolResult)> = tool_dones
        .into_iter()
        .map(|(id, outcome, result)| (id, (outcome, result)))
        .collect();

    // All three tool calls should succeed
    let (skill_outcome, _) = by_id.get("c1").expect("skill call c1 should be present");
    assert_eq!(
        *skill_outcome,
        ToolCallOutcome::Succeeded,
        "skill activation should succeed"
    );

    let (danger1_outcome, _) = by_id
        .get("c2")
        .expect("dangerous_tool call c2 should be present");
    assert_eq!(
        *danger1_outcome,
        ToolCallOutcome::Succeeded,
        "first dangerous_tool call should succeed"
    );

    let (danger2_outcome, _) = by_id
        .get("c3")
        .expect("dangerous_tool call c3 should be present");
    assert_eq!(
        *danger2_outcome,
        ToolCallOutcome::Succeeded,
        "second dangerous_tool call should succeed (elevation persists)"
    );
}

// ===========================================================================
// TEST 4: Same-step skill activation unlocks guarded tools
// ===========================================================================

#[tokio::test]
async fn same_step_skill_activation_unlocks_guarded_tool() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let dangerous_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "skill", json!({"skill": "power-skill"})),
            ToolCall::new("c2", "dangerous_tool", json!({})),
        ]),
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "same-step skill activation should unlock the guarded tool and still reach step 2"
    );
    assert_eq!(result.steps, 2, "run should advance past the tool step");
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        1,
        "guarded tool execute() should run exactly once after skill activation"
    );

    let events = sink.take();
    let tool_dones: HashMap<String, ToolCallOutcome> = events
        .iter()
        .filter_map(|event| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = event {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();

    assert_eq!(tool_dones.get("c1"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c2"), Some(&ToolCallOutcome::Succeeded));
}

// ===========================================================================
// TEST 5: Same-step skill activation also works with the default sequential executor
// ===========================================================================

#[tokio::test]
async fn same_step_skill_activation_unlocks_guarded_tool_with_sequential_executor() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let dangerous_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "skill", json!({"skill": "power-skill"})),
            ToolCall::new("c2", "dangerous_tool", json!({})),
        ]),
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(VecEventSink::new()),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.steps, 2);
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        1,
        "sequential execution should still run the guarded tool after skill activation"
    );
}

// ===========================================================================
// TEST 6: Same-step skill activation should not drop other already-allowed tools
// ===========================================================================

#[tokio::test]
async fn same_step_skill_activation_preserves_other_parallel_tools() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let dangerous_calls = Arc::new(AtomicUsize::new(0));
    let weather_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "skill", json!({"skill": "power-skill"})),
            ToolCall::new("c2", "get_weather", json!({"location": "Tokyo"})),
            ToolCall::new("c3", "dangerous_tool", json!({})),
        ]),
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all_with_extra_allowed_tools(&["get_weather"]);
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(CountingWeatherTool {
            calls: weather_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(
        weather_calls.load(Ordering::SeqCst),
        1,
        "already-allowed tool should still execute even when a later call needs skill elevation"
    );
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        1,
        "guarded tool should execute after re-checking permissions with the activated skill"
    );

    let tool_dones: HashMap<String, ToolCallOutcome> = sink
        .take()
        .iter()
        .filter_map(|event| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = event {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_dones.get("c1"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c2"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c3"), Some(&ToolCallOutcome::Succeeded));
}

// ===========================================================================
// TEST 7: Same-step skill activation should allow multiple guarded tools
// ===========================================================================

#[tokio::test]
async fn same_step_skill_activation_allows_multiple_guarded_tools() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let dangerous_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "skill", json!({"skill": "power-skill"})),
            ToolCall::new("c2", "dangerous_tool", json!({"slot": 1})),
            ToolCall::new("c3", "dangerous_tool", json!({"slot": 2})),
        ]),
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        2,
        "both guarded tools should execute after the same-step skill activation"
    );

    let tool_dones: HashMap<String, ToolCallOutcome> = sink
        .take()
        .iter()
        .filter_map(|event| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = event {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_dones.get("c1"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c2"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c3"), Some(&ToolCallOutcome::Succeeded));
}

// ===========================================================================
// TEST 8: Same-step skill activation should work when another tool comes first
// ===========================================================================

#[tokio::test]
async fn same_step_skill_activation_preserves_allowed_tool_before_skill() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let dangerous_calls = Arc::new(AtomicUsize::new(0));
    let weather_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![
            ToolCall::new("c1", "get_weather", json!({"location": "Tokyo"})),
            ToolCall::new("c2", "skill", json!({"skill": "power-skill"})),
            ToolCall::new("c3", "dangerous_tool", json!({})),
        ]),
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all_with_extra_allowed_tools(&["get_weather"]);
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingWeatherTool {
            calls: weather_calls.clone(),
        }))
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(
        weather_calls.load(Ordering::SeqCst),
        1,
        "allowed tool before skill activation should not be dropped"
    );
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        1,
        "guarded tool after the skill should still execute"
    );

    let tool_dones: HashMap<String, ToolCallOutcome> = sink
        .take()
        .iter()
        .filter_map(|event| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = event {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_dones.get("c1"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c2"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c3"), Some(&ToolCallOutcome::Succeeded));
}

// ===========================================================================
// TEST 9: Guarded tool before skill should still block the batch
// ===========================================================================

#[tokio::test]
async fn guarded_tool_before_skill_blocks_same_step_activation_attempt() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let dangerous_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![
        ToolCall::new("c1", "dangerous_tool", json!({})),
        ToolCall::new("c2", "skill", json!({"skill": "power-skill"})),
    ])]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert!(
        matches!(result.termination, TerminationReason::Blocked(_)),
        "guarded tool appearing before skill should still block the step"
    );
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        0,
        "blocked guarded tool must not execute"
    );

    let tool_dones: HashMap<String, ToolCallOutcome> = sink
        .take()
        .iter()
        .filter_map(|event| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = event {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_dones.get("c1"), Some(&ToolCallOutcome::Failed));
    assert_eq!(
        tool_dones.get("c2"),
        Some(&ToolCallOutcome::Failed),
        "later skill call should be backfilled as interrupted, not executed"
    );
}

// ===========================================================================
// TEST 10: Allowed prefix should commit before later blocked guarded tool
// ===========================================================================

#[tokio::test]
async fn allowed_prefix_commits_before_later_guarded_tool_blocks() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let weather_calls = Arc::new(AtomicUsize::new(0));
    let dangerous_calls = Arc::new(AtomicUsize::new(0));

    let llm = Arc::new(ScriptedLlm::new(vec![tool_step(vec![
        ToolCall::new("c1", "get_weather", json!({"location": "Tokyo"})),
        ToolCall::new("c2", "dangerous_tool", json!({})),
        ToolCall::new("c3", "skill", json!({"skill": "power-skill"})),
    ])]));

    let spec = make_agent_spec_deny_all_with_extra_allowed_tools(&["get_weather"]);
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent
        .with_tool(Arc::new(CountingWeatherTool {
            calls: weather_calls.clone(),
        }))
        .with_tool(Arc::new(CountingDangerousTool {
            calls: dangerous_calls.clone(),
        }))
        .with_tool(Arc::new(SkillActivateTool::new(registry)))
        .with_tool_executor(Arc::new(ParallelToolExecutor::streaming()));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(discovery_plugin), Arc::new(PermissionPlugin)];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert!(
        matches!(result.termination, TerminationReason::Blocked(_)),
        "later guarded tool should still block when no prior skill has activated"
    );
    assert_eq!(
        weather_calls.load(Ordering::SeqCst),
        1,
        "allowed tool before the blocked guarded call should still execute"
    );
    assert_eq!(
        dangerous_calls.load(Ordering::SeqCst),
        0,
        "blocked guarded tool must not execute"
    );

    let tool_dones: HashMap<String, ToolCallOutcome> = sink
        .take()
        .iter()
        .filter_map(|event| {
            if let AgentEvent::ToolCallDone { id, outcome, .. } = event {
                Some((id.clone(), *outcome))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(tool_dones.get("c1"), Some(&ToolCallOutcome::Succeeded));
    assert_eq!(tool_dones.get("c2"), Some(&ToolCallOutcome::Failed));
    assert_eq!(
        tool_dones.get("c3"),
        Some(&ToolCallOutcome::Failed),
        "later skill call should be backfilled as interrupted, not executed"
    );
}

// ===========================================================================
// TEST 11: Standalone skill activation still advances to the next step
// ===========================================================================

#[tokio::test]
async fn standalone_skill_activation_advances_to_next_step() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let instructions_plugin = ActiveSkillInstructionsPlugin::new(registry.clone());

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new(
            "c1",
            "skill",
            json!({"skill": "power-skill"}),
        )]),
        text_step("done"),
    ]));

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent.with_tool(Arc::new(SkillActivateTool::new(registry)));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(discovery_plugin),
        Arc::new(instructions_plugin),
        Arc::new(PermissionPlugin),
    ];
    let resolver = FixedResolver::with_plugins(agent, plugins);
    let sink = Arc::new(VecEventSink::new());

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: sink.clone(),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(
        result.termination,
        TerminationReason::NaturalEnd,
        "standalone skill activation should not stall the loop"
    );
    assert_eq!(
        result.steps, 2,
        "run should enter step 2 after skill activation"
    );

    let events = sink.take();
    let inference_complete_count = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::InferenceComplete { .. }))
        .count();
    assert_eq!(
        inference_complete_count, 2,
        "loop should perform a second inference after the standalone skill activation"
    );
}

// ===========================================================================
// TEST 12: Step 2 should receive injected active skill instructions
// ===========================================================================

#[tokio::test]
async fn standalone_skill_activation_injects_active_instructions_on_next_inference() {
    let registry = make_skill_registry();
    let discovery_plugin = SkillDiscoveryPlugin::new(registry.clone());
    let instructions_plugin = ActiveSkillInstructionsPlugin::new(registry.clone());

    let llm = Arc::new(ScriptedLlm::new(vec![
        tool_step(vec![ToolCall::new(
            "c1",
            "skill",
            json!({"skill": "power-skill"}),
        )]),
        text_step("done"),
    ]));
    let llm_clone = llm.clone();

    let spec = make_agent_spec_deny_all();
    let mut agent = ResolvedAgent::new("test", "m", "sys", llm);
    agent.spec = Arc::new(spec);
    let agent = agent.with_tool(Arc::new(SkillActivateTool::new(registry)));

    let rt = make_runtime();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(discovery_plugin),
        Arc::new(instructions_plugin),
        Arc::new(PermissionPlugin),
    ];
    let resolver = FixedResolver::with_plugins(agent, plugins);

    let result = run_agent_loop(AgentLoopParams {
        resolver: &resolver,
        agent_id: "test",
        runtime: &rt,
        sink: Arc::new(VecEventSink::new()),
        checkpoint_store: None,
        messages: vec![Message::user("go")],
        run_identity: id(),
        cancellation_token: None,
        decision_rx: None,
        overrides: None,
        frontend_tools: Vec::new(),
        inbox: None,
        is_continuation: false,
    })
    .await
    .unwrap();

    assert_eq!(result.termination, TerminationReason::NaturalEnd);

    let requests = llm_clone.captured_requests();
    assert_eq!(
        requests.len(),
        2,
        "skill activation should trigger a second inference request"
    );

    let second_request_messages = &requests[1].messages;
    let has_active_instructions = second_request_messages
        .iter()
        .any(|message| message.text().contains("<active_skill_instructions>"))
        && second_request_messages.iter().any(|message| {
            message
                .text()
                .contains("This skill grants access to dangerous_tool.")
        });
    assert!(
        has_active_instructions,
        "second inference request should contain the activated skill instructions, got messages: {:?}",
        second_request_messages
    );
}

// ===========================================================================
// TEST 13: New run starts without previous permission elevation
// ===========================================================================

#[tokio::test]
async fn new_run_starts_without_previous_permission_elevation() {
    use awaken::UnknownKeyPolicy;
    use awaken::ext_permission::state::{PermissionAction, PermissionOverridesKey};
    use awaken::state::{MutationBatch, StateStore};

    // --- First store (simulating run 1) ---
    let registry = make_skill_registry();
    let discovery_plugin_1 = SkillDiscoveryPlugin::new(registry.clone());

    let store1 = StateStore::new();
    store1.install_plugin(LoopStatePlugin).unwrap();
    store1.install_plugin(discovery_plugin_1).unwrap();
    store1.install_plugin(PermissionPlugin).unwrap();

    // Write permission overrides (run-scoped) simulating skill elevation
    let mut batch = MutationBatch::new();
    batch.update::<PermissionOverridesKey>(PermissionAction::AllowTool {
        tool_id: "dangerous_tool".into(),
    });
    store1.commit(batch).unwrap();

    // Verify overrides exist in first store
    let overrides = store1.read::<PermissionOverridesKey>();
    assert!(
        overrides.is_some() && !overrides.unwrap().rules.is_empty(),
        "first store should have permission overrides"
    );

    // Export persisted state from run 1
    let persisted = store1.export_persisted().unwrap();

    // --- Second store (simulating run 2) ---
    let discovery_plugin_2 = SkillDiscoveryPlugin::new(registry);
    let store2 = StateStore::new();
    store2.install_plugin(LoopStatePlugin).unwrap();
    store2.install_plugin(discovery_plugin_2).unwrap();
    store2.install_plugin(PermissionPlugin).unwrap();

    // Only restore thread-scoped state (run-scoped should be dropped)
    store2
        .restore_thread_scoped(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    // PermissionOverridesKey is Run-scoped → should NOT be restored
    let overrides_in_new_run = store2.read::<PermissionOverridesKey>();
    assert!(
        overrides_in_new_run.is_none() || overrides_in_new_run.as_ref().unwrap().rules.is_empty(),
        "new run should NOT have permission overrides from previous run"
    );
}

// ===========================================================================
// TEST 14: Skill state resets between runs
// ===========================================================================

#[tokio::test]
async fn skill_state_resets_between_runs() {
    use awaken::UnknownKeyPolicy;
    use awaken::ext_skills::SkillStateUpdate;
    use awaken::ext_skills::state::SkillState;
    use awaken::state::{MutationBatch, StateStore};

    // --- First store (run 1) ---
    let registry = make_skill_registry();
    let discovery_plugin_1 = SkillDiscoveryPlugin::new(registry.clone());

    let store1 = StateStore::new();
    store1.install_plugin(LoopStatePlugin).unwrap();
    store1.install_plugin(discovery_plugin_1).unwrap();
    store1.install_plugin(PermissionPlugin).unwrap();

    // Activate a skill in the first run
    let mut batch = MutationBatch::new();
    batch.update::<SkillState>(SkillStateUpdate::Activate("power-skill".into()));
    store1.commit(batch).unwrap();

    // Verify skill is active in first store
    let skill_state = store1.read::<SkillState>();
    assert!(
        skill_state.is_some() && skill_state.unwrap().active.contains("power-skill"),
        "first store should have active skill"
    );

    // Export persisted state
    let persisted = store1.export_persisted().unwrap();

    // --- Second store (run 2) ---
    let discovery_plugin_2 = SkillDiscoveryPlugin::new(registry);
    let store2 = StateStore::new();
    store2.install_plugin(LoopStatePlugin).unwrap();
    store2.install_plugin(discovery_plugin_2).unwrap();
    store2.install_plugin(PermissionPlugin).unwrap();

    // Only restore thread-scoped state
    store2
        .restore_thread_scoped(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    // SkillState is registered with Run scope → should NOT be restored
    let skill_state_new = store2.read::<SkillState>();
    assert!(
        skill_state_new.is_none() || skill_state_new.as_ref().unwrap().active.is_empty(),
        "new run should NOT have active skills from previous run (SkillState is Run-scoped)"
    );
}

// ===========================================================================
// TEST 15: Thread-scoped policy persists across runs
// ===========================================================================

#[tokio::test]
async fn thread_scoped_policy_persists_across_runs() {
    use awaken::UnknownKeyPolicy;
    use awaken::ext_permission::rules::ToolPermissionBehavior;
    use awaken::ext_permission::state::{PermissionAction, PermissionPolicyKey};
    use awaken::state::{MutationBatch, StateStore};

    // --- First store (run 1) ---
    let registry = make_skill_registry();
    let discovery_plugin_1 = SkillDiscoveryPlugin::new(registry.clone());

    let store1 = StateStore::new();
    store1.install_plugin(LoopStatePlugin).unwrap();
    store1.install_plugin(discovery_plugin_1).unwrap();
    store1.install_plugin(PermissionPlugin).unwrap();

    // Write a thread-scoped permission policy
    let mut batch = MutationBatch::new();
    batch.update::<PermissionPolicyKey>(PermissionAction::SetDefault {
        behavior: ToolPermissionBehavior::Deny,
    });
    batch.update::<PermissionPolicyKey>(PermissionAction::AllowTool {
        tool_id: "Read".into(),
    });
    batch.update::<PermissionPolicyKey>(PermissionAction::AllowTool {
        tool_id: "Edit".into(),
    });
    store1.commit(batch).unwrap();

    // Verify policy exists in first store
    let policy = store1.read::<PermissionPolicyKey>().unwrap();
    assert_eq!(policy.default_behavior, ToolPermissionBehavior::Deny);
    assert_eq!(policy.rules.len(), 2);

    // Export persisted state
    let persisted = store1.export_persisted().unwrap();

    // --- Second store (run 2) ---
    let discovery_plugin_2 = SkillDiscoveryPlugin::new(registry);
    let store2 = StateStore::new();
    store2.install_plugin(LoopStatePlugin).unwrap();
    store2.install_plugin(discovery_plugin_2).unwrap();
    store2.install_plugin(PermissionPlugin).unwrap();

    // Restore thread-scoped state
    store2
        .restore_thread_scoped(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    // PermissionPolicyKey is Thread-scoped → SHOULD be restored
    let policy_new = store2.read::<PermissionPolicyKey>();
    assert!(
        policy_new.is_some(),
        "thread-scoped permission policy should be restored across runs"
    );
    let policy_new = policy_new.unwrap();
    assert_eq!(
        policy_new.default_behavior,
        ToolPermissionBehavior::Deny,
        "default_behavior should persist across runs"
    );
    assert_eq!(
        policy_new.rules.len(),
        2,
        "permission rules should persist across runs"
    );
    assert!(
        policy_new.rules.contains_key("tool:Read"),
        "Read rule should persist"
    );
    assert!(
        policy_new.rules.contains_key("tool:Edit"),
        "Edit rule should persist"
    );
}
