//! ADR-0030 D2: hooks must fill prompt_id, tool_desc_ids, skill_ids on
//! SpanContext from the resolved snapshot at run start / before inference /
//! before tool execute.

use std::sync::Arc;

use awaken_runtime::registry::memory::{
    MapAgentSpecRegistry, MapBackendRegistry, MapModelRegistry, MapPluginSource,
    MapProviderRegistry, MapToolRegistry,
};
use awaken_runtime::registry::traits::RegistrySet;
use awaken_runtime::registry::{RegistryHandle, RegistrySnapshot};
use awaken_runtime::{PhaseContext, PhaseHook};
use awaken_runtime_contract::contract::content::ContentBlock;
use awaken_runtime_contract::contract::inference::{
    LLMResponse, StopReason, StreamResult, TokenUsage,
};
use awaken_runtime_contract::contract::tool::{FrontEndTool, ToolDescriptor};
use awaken_runtime_contract::model::Phase;
use awaken_runtime_contract::registry_spec::AgentSpec;
use awaken_runtime_contract::state::{Snapshot, StateMap};

use crate::InMemorySink;
use crate::plugin::{
    BeforeInferenceHook, BeforeToolExecuteHook, ObservabilityPlugin, RunStartHook,
};

fn empty_snapshot() -> Snapshot {
    Snapshot::new(0, Arc::new(StateMap::default()))
}

fn make_success_response() -> LLMResponse {
    LLMResponse::success(StreamResult {
        content: vec![ContentBlock::text("ok")],
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    })
}

/// Build a RegistrySnapshot with one agent and the given tools.
fn make_snapshot_with_tools(
    agent_id: &str,
    system_prompt: &str,
    tool_ids: &[&str],
) -> Arc<RegistrySnapshot> {
    let mut agents = MapAgentSpecRegistry::new();
    agents
        .register_spec(AgentSpec {
            id: agent_id.into(),
            model_id: "default".into(),
            system_prompt: system_prompt.into(),
            allowed_tools: if tool_ids.is_empty() {
                None
            } else {
                Some(tool_ids.iter().map(|s| s.to_string()).collect())
            },
            ..Default::default()
        })
        .expect("register agent");

    let mut tools = MapToolRegistry::new();
    for &tid in tool_ids {
        let desc = ToolDescriptor::new(tid, tid, format!("Description for {tid}"));
        let tool: Arc<dyn awaken_runtime_contract::contract::tool::Tool> =
            Arc::new(FrontEndTool::new(desc));
        tools.register_tool(tid, tool).expect("register tool");
    }

    let mut models = MapModelRegistry::new();
    models
        .register_model(awaken_runtime_contract::ModelSpec::new(
            "default",
            "provider",
            "test-model",
        ))
        .expect("register model");

    let registries = RegistrySet {
        agents: Arc::new(agents),
        tools: Arc::new(tools),
        models: Arc::new(models),
        providers: Arc::new(MapProviderRegistry::new()),
        plugins: Arc::new(MapPluginSource::new()),
        backends: Arc::new(MapBackendRegistry::new()),
    };
    let handle = RegistryHandle::new(registries);
    Arc::new(handle.snapshot())
}

/// Run a minimal run-start + before-inference + after-inference cycle and
/// return the plugin so tests can inspect span_context and sink.metrics().
async fn drive_simple_run(agent_id: &str, system_prompt: &str) -> ObservabilityPlugin {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("test-model")
        .with_provider("test-provider");

    let snapshot = make_snapshot_with_tools(agent_id, system_prompt, &[]);

    // RunStart with agent_spec populated and registry snapshot attached.
    let agent_spec = Arc::new(AgentSpec {
        id: agent_id.into(),
        model_id: "default".into(),
        system_prompt: system_prompt.into(),
        ..Default::default()
    });
    let run_start_ctx = PhaseContext::new(Phase::RunStart, empty_snapshot())
        .with_agent_spec(Arc::clone(&agent_spec))
        .with_registry_snapshot(Arc::clone(&snapshot));
    RunStartHook(Arc::clone(&plugin.inner))
        .run(&run_start_ctx)
        .await
        .unwrap();

    // BeforeInference
    let before_ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot())
        .with_agent_spec(Arc::clone(&agent_spec))
        .with_registry_snapshot(Arc::clone(&snapshot));
    BeforeInferenceHook(Arc::clone(&plugin.inner))
        .run(&before_ctx)
        .await
        .unwrap();

    // AfterInference
    let after_ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
        .with_llm_response(make_success_response());
    crate::plugin::AfterInferenceHook(Arc::clone(&plugin.inner))
        .run(&after_ctx)
        .await
        .unwrap();

    plugin
}

/// Run a before-inference cycle with the given tool IDs in the agent spec.
async fn drive_run_with_tools(agent_id: &str, tool_ids: &[&str]) -> ObservabilityPlugin {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("test-model")
        .with_provider("test-provider");

    let system_prompt = "You are a test assistant.";
    let snapshot = make_snapshot_with_tools(agent_id, system_prompt, tool_ids);

    let agent_spec = Arc::new(AgentSpec {
        id: agent_id.into(),
        model_id: "default".into(),
        system_prompt: system_prompt.into(),
        allowed_tools: Some(tool_ids.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    });

    // RunStart
    RunStartHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::RunStart, empty_snapshot())
                .with_agent_spec(Arc::clone(&agent_spec))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();

    // BeforeInference — this is where tool_desc_ids gets populated.
    BeforeInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::BeforeInference, empty_snapshot())
                .with_agent_spec(Arc::clone(&agent_spec))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();

    // AfterInference
    crate::plugin::AfterInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::AfterInference, empty_snapshot())
                .with_llm_response(make_success_response()),
        )
        .await
        .unwrap();

    plugin
}

/// Drive a run end-to-end but inject a *synthetic* `effective_tool_ids`
/// onto the AfterInference context — simulating what the loop runner now
/// does post-`apply_tool_filter_payloads`. Used to pin the F2 contract:
/// AfterInferenceHook must prefer this override over whatever
/// BeforeInferenceHook computed from `agent_spec.allowed_tools`. Returns
/// both the plugin (for `span_context` inspection) and the sink clone
/// (for `metrics()` inspection).
async fn drive_run_with_effective_override(
    agent_id: &str,
    declared_tool_ids: &[&str],
    effective_override: Vec<String>,
) -> (ObservabilityPlugin, InMemorySink) {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("test-model")
        .with_provider("test-provider");

    let system_prompt = "You are a test assistant.";
    let snapshot = make_snapshot_with_tools(agent_id, system_prompt, declared_tool_ids);
    let agent_spec = Arc::new(AgentSpec {
        id: agent_id.into(),
        model_id: "default".into(),
        system_prompt: system_prompt.into(),
        allowed_tools: Some(declared_tool_ids.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    });

    RunStartHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::RunStart, empty_snapshot())
                .with_agent_spec(Arc::clone(&agent_spec))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();
    BeforeInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::BeforeInference, empty_snapshot())
                .with_agent_spec(Arc::clone(&agent_spec))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();
    crate::plugin::AfterInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::AfterInference, empty_snapshot())
                .with_llm_response(make_success_response())
                .with_effective_tool_ids(effective_override),
        )
        .await
        .unwrap();
    (plugin, sink)
}

/// Run a before-tool-execute cycle for the named tool and return the plugin.
async fn drive_run_invoking_tool(agent_id: &str, tool_id: &str) -> ObservabilityPlugin {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("test-model")
        .with_provider("test-provider");

    let system_prompt = "You are a test assistant.";
    let snapshot = make_snapshot_with_tools(agent_id, system_prompt, &[tool_id]);

    let agent_spec = Arc::new(AgentSpec {
        id: agent_id.into(),
        model_id: "default".into(),
        system_prompt: system_prompt.into(),
        allowed_tools: Some(vec![tool_id.to_string()]),
        ..Default::default()
    });

    // RunStart
    RunStartHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::RunStart, empty_snapshot())
                .with_agent_spec(Arc::clone(&agent_spec))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();

    // BeforeInference + AfterInference to bump step counter
    BeforeInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::BeforeInference, empty_snapshot())
                .with_agent_spec(Arc::clone(&agent_spec))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();
    crate::plugin::AfterInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::AfterInference, empty_snapshot())
                .with_llm_response(make_success_response()),
        )
        .await
        .unwrap();

    // BeforeToolExecute — this is where tool_desc_ids for the specific tool is stamped.
    BeforeToolExecuteHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
                .with_tool_info(tool_id, "call-1", Some(serde_json::json!({})))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();

    // AfterToolExecute to create the ToolSpan
    crate::plugin::AfterToolExecuteHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
                .with_tool_info(tool_id, "call-1", Some(serde_json::json!({})))
                .with_tool_result(
                    awaken_runtime_contract::contract::tool::ToolResult::success(
                        tool_id,
                        serde_json::json!({}),
                    ),
                ),
        )
        .await
        .unwrap();

    plugin
}

#[tokio::test]
async fn run_start_fills_prompt_id_from_resolved_agent() {
    let plugin = drive_simple_run("weather", "You are a weather assistant.").await;
    let metrics = plugin.inner.metrics.try_lock().unwrap().clone();
    let span = metrics
        .inferences
        .first()
        .expect("at least one inference span");
    let pid = span
        .context
        .prompt_id
        .as_deref()
        .expect("prompt_id populated");
    assert_eq!(pid.len(), 12, "prompt_id must be a 12-char hex prefix");
    assert!(
        pid.chars().all(|c| c.is_ascii_hexdigit()),
        "prompt_id must be hex"
    );
}

#[tokio::test]
async fn prompt_id_is_stable_across_runs() {
    let p1 = drive_simple_run("weather", "You are a weather assistant.").await;
    let p2 = drive_simple_run("weather", "You are a weather assistant.").await;
    let id1 = p1
        .inner
        .metrics
        .try_lock()
        .unwrap()
        .inferences
        .first()
        .unwrap()
        .context
        .prompt_id
        .clone();
    let id2 = p2
        .inner
        .metrics
        .try_lock()
        .unwrap()
        .inferences
        .first()
        .unwrap()
        .context
        .prompt_id
        .clone();
    assert_eq!(id1, id2, "same prompt must produce the same prompt_id");
}

#[tokio::test]
async fn prompt_id_differs_for_different_prompts() {
    let p1 = drive_simple_run("weather", "You are a weather assistant.").await;
    let p2 = drive_simple_run("weather", "You are a different assistant.").await;
    let id1 = p1
        .inner
        .metrics
        .try_lock()
        .unwrap()
        .inferences
        .first()
        .unwrap()
        .context
        .prompt_id
        .clone();
    let id2 = p2
        .inner
        .metrics
        .try_lock()
        .unwrap()
        .inferences
        .first()
        .unwrap()
        .context
        .prompt_id
        .clone();
    assert_ne!(
        id1, id2,
        "different prompts must produce different prompt_ids"
    );
}

#[tokio::test]
async fn before_inference_advertises_tool_desc_ids() {
    let plugin = drive_run_with_tools("weather", &["get_forecast", "list_cities"]).await;
    let metrics = plugin.inner.metrics.try_lock().unwrap().clone();
    let span = metrics.inferences.first().unwrap();
    assert_eq!(
        span.context.tool_desc_ids.len(),
        2,
        "two tools must yield two tool_desc_ids"
    );
    for id in &span.context.tool_desc_ids {
        assert_eq!(id.len(), 12, "each tool_desc_id must be 12-char hex");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

#[tokio::test]
async fn before_inference_no_tools_yields_empty_tool_desc_ids() {
    let plugin = drive_simple_run("weather", "You are a weather assistant.").await;
    let metrics = plugin.inner.metrics.try_lock().unwrap().clone();
    let span = metrics.inferences.first().unwrap();
    assert!(
        span.context.tool_desc_ids.is_empty(),
        "no tools must yield empty tool_desc_ids"
    );
}

#[tokio::test]
async fn before_tool_execute_records_specific_tool_desc_id() {
    let plugin = drive_run_invoking_tool("weather", "get_forecast").await;
    let metrics = plugin.inner.metrics.try_lock().unwrap().clone();
    let tool = metrics.tools.last().expect("at least one tool span");
    assert_eq!(
        tool.context.tool_desc_ids.len(),
        1,
        "single tool call must record exactly one tool_desc_id"
    );
    let id = &tool.context.tool_desc_ids[0];
    assert_eq!(id.len(), 12, "tool_desc_id must be 12-char hex");
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn skill_ids_stays_empty() {
    // skill_content_id is intentionally deferred per RegistrySnapshot's in-code
    // note — confirm the field is empty (not populated) after all three hooks run.
    let plugin = drive_run_invoking_tool("weather", "get_forecast").await;
    let span_ctx = plugin.inner.span_context.try_lock().unwrap().clone();
    assert!(
        span_ctx.skill_ids.is_empty(),
        "skill_ids must remain empty until the follow-up ADR lands"
    );
}

#[tokio::test]
async fn handoff_refreshes_prompt_id_to_new_agent() {
    // Regression for F13: BeforeInferenceHook used to refresh `agent_id`
    // on handoff but leave `prompt_id` stale. After handoff to a
    // different agent with a different system prompt, the next
    // inference must carry the NEW prompt_id.
    use awaken_runtime_contract::contract::identity::{RunIdentity, RunRef};
    use awaken_runtime_contract::identity::agent_prompt_id;

    fn ident(agent: &str) -> RunIdentity {
        RunIdentity {
            run: RunRef {
                thread_id: "thread-1".into(),
                parent_thread_id: None,
                run_id: "run-1".into(),
                parent_run_id: None,
                agent_id: agent.into(),
                parent_tool_call_id: None,
            },
            ..RunIdentity::default()
        }
    }

    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("test-model")
        .with_provider("test-provider");

    // RunStart on agent A.
    let agent_a = Arc::new(AgentSpec {
        id: "agent-a".into(),
        model_id: "default".into(),
        system_prompt: "You are A.".into(),
        ..Default::default()
    });
    RunStartHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::RunStart, empty_snapshot())
                .with_run_identity(ident("agent-a"))
                .with_agent_spec(Arc::clone(&agent_a)),
        )
        .await
        .unwrap();

    // Handoff: BeforeInference fires with new agent identity and new spec.
    let agent_b = Arc::new(AgentSpec {
        id: "agent-b".into(),
        model_id: "default".into(),
        system_prompt: "You are B, different.".into(),
        ..Default::default()
    });
    BeforeInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::BeforeInference, empty_snapshot())
                .with_run_identity(ident("agent-b"))
                .with_agent_spec(Arc::clone(&agent_b)),
        )
        .await
        .unwrap();

    let span_ctx = plugin.inner.span_context.lock().await.clone();
    assert_eq!(span_ctx.agent_id, "agent-b");
    let expected = agent_prompt_id("agent-b", "system", "You are B, different.");
    assert_eq!(
        span_ctx.prompt_id.as_deref(),
        Some(expected.as_str()),
        "post-handoff prompt_id must reflect the NEW agent's spec, not stay stale"
    );
}

#[tokio::test]
async fn handoff_with_empty_spec_prompt_falls_back_to_registry_snapshot() {
    // F23: the handoff branch of BeforeInferenceHook must use the same
    // precedence as RunStart — prefer the resolved `agent_spec.system_prompt`,
    // but fall back to the registry snapshot when the spec was handed off
    // without a hydrated prompt. Without the fallback, a handoff into an
    // agent whose spec is still a stub would silently null the prompt_id
    // even when the snapshot carries the canonical text.
    use awaken_runtime_contract::contract::identity::{RunIdentity, RunRef};
    use awaken_runtime_contract::identity::agent_prompt_id;

    fn ident(agent: &str) -> RunIdentity {
        RunIdentity {
            run: RunRef {
                thread_id: "thread-1".into(),
                parent_thread_id: None,
                run_id: "run-1".into(),
                parent_run_id: None,
                agent_id: agent.into(),
                parent_tool_call_id: None,
            },
            ..RunIdentity::default()
        }
    }

    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("test-model")
        .with_provider("test-provider");

    // RunStart on agent A with a hydrated system prompt.
    let agent_a = Arc::new(AgentSpec {
        id: "agent-a".into(),
        model_id: "default".into(),
        system_prompt: "You are A.".into(),
        ..Default::default()
    });
    RunStartHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::RunStart, empty_snapshot())
                .with_run_identity(ident("agent-a"))
                .with_agent_spec(Arc::clone(&agent_a)),
        )
        .await
        .unwrap();

    // Handoff into agent B. The handed-off `agent_spec` carries an empty
    // system_prompt — simulates a deferred resolution where the snapshot
    // holds the canonical text. The hook must consult the snapshot.
    let agent_b_stub = Arc::new(AgentSpec {
        id: "agent-b".into(),
        model_id: "default".into(),
        system_prompt: String::new(),
        ..Default::default()
    });
    let snapshot = make_snapshot_with_tools("agent-b", "Snapshot-side prompt for B.", &[]);
    BeforeInferenceHook(Arc::clone(&plugin.inner))
        .run(
            &PhaseContext::new(Phase::BeforeInference, empty_snapshot())
                .with_run_identity(ident("agent-b"))
                .with_agent_spec(Arc::clone(&agent_b_stub))
                .with_registry_snapshot(Arc::clone(&snapshot)),
        )
        .await
        .unwrap();

    let span_ctx = plugin.inner.span_context.lock().await.clone();
    assert_eq!(span_ctx.agent_id, "agent-b");
    let expected = agent_prompt_id("agent-b", "system", "Snapshot-side prompt for B.");
    assert_eq!(
        span_ctx.prompt_id.as_deref(),
        Some(expected.as_str()),
        "handoff with empty spec.system_prompt must fall back to snapshot"
    );
}

#[tokio::test]
async fn after_inference_prefers_effective_tool_ids_over_before_inference_stamp() {
    // Regression for F2: BeforeInferenceHook stamps a pre-filter list
    // from `agent_spec.allowed_tools`; AfterInferenceHook MUST override
    // it with `ctx.effective_tool_ids` when the loop runner threads in
    // the post-filter list. Otherwise the GenAI span reports tools the
    // LLM never actually saw (e.g. when a tool-gate hook excluded one,
    // or when the orchestrator injected a frontend tool the agent_spec
    // never declared).
    let override_ids = vec!["ffffffffffff".to_string()];
    let (_plugin, sink) = drive_run_with_effective_override(
        "weather",
        &["get_forecast", "list_cities"],
        override_ids.clone(),
    )
    .await;
    let metrics = sink.metrics();
    let inference = metrics
        .inferences
        .last()
        .expect("at least one inference span");
    assert_eq!(
        inference.context.tool_desc_ids, override_ids,
        "the post-filter override must win over the BeforeInference stamp"
    );
}
