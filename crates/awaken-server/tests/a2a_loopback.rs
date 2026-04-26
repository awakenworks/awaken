//! A2A loopback integration test — orchestrator delegates to a worker agent
//! on the **same** server via the A2A HTTP protocol.
//!
//! Requires a live OpenAI-compatible LLM. Set these env vars before running:
//!
//! ```bash
//! export OPENAI_BASE_URL=http://localhost:8000/codex/v1
//! export OPENAI_API_KEY=sk-ccproxy
//! export OPENAI_MODEL=gpt-5.4        # optional, defaults to "gpt-4o-mini"
//! export OPENAI_ADAPTER=deepseek      # optional, defaults to "deepseek"
//! ```
//!
//! Run with:
//!   cargo test -p awaken-server --test a2a_loopback -- --ignored

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::ToolCall;
use awaken_contract::contract::storage::{RunQuery, RunStore};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::registry_spec::{AgentSpec, RemoteEndpoint};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::engine::executor::GenaiExecutor;
use awaken_runtime::extensions::background::{
    BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext,
    TaskResult as BackgroundTaskResult, TaskStatus,
};
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_stores::InMemoryMailboxStore;
use awaken_stores::memory::InMemoryStore;
use serde_json::Value;
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a genai Client using OPENAI_BASE_URL / OPENAI_API_KEY env vars.
fn build_genai_client() -> genai::Client {
    let base_url = std::env::var("OPENAI_BASE_URL").unwrap_or_default();
    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");

    if base_url.is_empty() {
        // Use default genai client (routes via genai's built-in resolver)
        return genai::Client::default();
    }

    use genai::resolver::{AuthData, Endpoint};
    use genai::{ModelIden, ServiceTarget};

    let mut url = base_url;
    if !url.ends_with('/') {
        url.push('/');
    }

    let adapter_override = std::env::var("OPENAI_ADAPTER").ok();

    genai::Client::builder()
        .with_service_target_resolver_fn(move |st: ServiceTarget| {
            let adapter_kind = resolve_openai_compatible_adapter(
                &st.model.model_name,
                adapter_override.as_deref(),
            );
            Ok(ServiceTarget {
                endpoint: Endpoint::from_owned(url.clone()),
                auth: AuthData::from_single(api_key.clone()),
                model: ModelIden::new(adapter_kind, st.model.model_name),
            })
        })
        .build()
}

fn resolve_openai_compatible_adapter(
    model_name: &str,
    adapter_override: Option<&str>,
) -> genai::adapter::AdapterKind {
    use genai::adapter::AdapterKind;

    match adapter_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "openai" => AdapterKind::OpenAI,
            "openai_resp" | "openai-resp" | "responses" => AdapterKind::OpenAIResp,
            "deepseek" => AdapterKind::DeepSeek,
            "together" => AdapterKind::Together,
            "fireworks" => AdapterKind::Fireworks,
            _ => infer_openai_compatible_adapter(model_name),
        },
        None => infer_openai_compatible_adapter(model_name),
    }
}

fn infer_openai_compatible_adapter(model_name: &str) -> genai::adapter::AdapterKind {
    use genai::adapter::AdapterKind;

    let inferred = AdapterKind::from_model(model_name).unwrap_or(AdapterKind::OpenAI);
    match inferred {
        AdapterKind::OpenAI
        | AdapterKind::OpenAIResp
        | AdapterKind::DeepSeek
        | AdapterKind::Together
        | AdapterKind::Fireworks => inferred,
        _ => AdapterKind::OpenAI,
    }
}

/// POST JSON and return parsed body.
async fn post_json(client: &reqwest::Client, url: &str, body: Value) -> Value {
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("POST request");
    let status = resp.status();
    let text = resp.text().await.expect("response body");
    assert!(status.is_success(), "POST {url} failed ({status}): {text}");
    serde_json::from_str(&text).expect("valid JSON response")
}

/// GET JSON and return parsed body.
async fn get_json(client: &reqwest::Client, url: &str) -> Value {
    let resp = client.get(url).send().await.expect("GET request");
    let status = resp.status();
    let text = resp.text().await.expect("response body");
    assert!(status.is_success(), "GET {url} failed ({status}): {text}");
    serde_json::from_str(&text).expect("valid JSON response")
}

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> StreamResult {
        StreamResult {
            content: vec![],
            tool_calls: calls,
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        }
    }

    fn text_response(text: &str) -> StreamResult {
        StreamResult {
            content: vec![ContentBlock::text(text)],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut responses = self.responses.lock().expect("responses lock");
        Ok(if responses.is_empty() {
            ScriptedLlm::text_response("")
        } else {
            responses.remove(0)
        })
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

struct SpawnBackgroundChildTool {
    manager: Arc<BackgroundTaskManager>,
}

#[async_trait]
impl Tool for SpawnBackgroundChildTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "spawn_bg_child",
            "spawn_bg_child",
            "Spawn a cancellable background child task",
        )
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("leaf");
        let task_id = self
            .manager
            .spawn(
                &ctx.run_identity.thread_id,
                "background_child",
                Some(name),
                "child spawned from loopback worker",
                TaskParentContext::default(),
                |task_ctx| async move {
                    task_ctx.cancelled().await;
                    BackgroundTaskResult::Cancelled
                },
            )
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;

        Ok(ToolResult::success("spawn_bg_child", serde_json::json!({ "task_id": task_id })).into())
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a2a_loopback_remote_self_cancel_cascades_background_children() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to random port");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    let manager = Arc::new(BackgroundTaskManager::new());

    let orchestrator_executor = Arc::new(ScriptedLlm::new(vec![
        ScriptedLlm::tool_call_response(vec![ToolCall::new(
            "delegate-1",
            "agent_run_worker-remote",
            serde_json::json!({"prompt": "spawn a child and cancel yourself"}),
        )]),
        ScriptedLlm::text_response("observed remote cancellation"),
    ]));
    let worker_executor = Arc::new(ScriptedLlm::new(vec![ScriptedLlm::tool_call_response(
        vec![
            ToolCall::new(
                "spawn-1",
                "spawn_bg_child",
                serde_json::json!({"name": "leaf"}),
            ),
            ToolCall::new(
                "cancel-1",
                "cancel_task",
                serde_json::json!({"target": {"relation": "self"}}),
            ),
        ],
    )]));

    let orchestrator_spec = AgentSpec {
        id: "orchestrator".into(),
        model_id: "orchestrator-model".into(),
        system_prompt: "delegate to the remote worker".into(),
        max_rounds: 2,
        delegates: vec!["worker-remote".into()],
        ..Default::default()
    };
    let worker_local = AgentSpec {
        id: "worker".into(),
        model_id: "worker-model".into(),
        system_prompt: "spawn a child and cancel yourself".into(),
        max_rounds: 2,
        plugin_ids: vec!["background".into()],
        ..Default::default()
    };
    let worker_remote = AgentSpec {
        id: "worker-remote".into(),
        model_id: "worker-model".into(),
        system_prompt: "remote worker".into(),
        endpoint: Some(RemoteEndpoint {
            backend: "a2a".into(),
            base_url: format!("{base_url}/v1/a2a"),
            auth: None,
            target: Some("worker".into()),
            timeout_ms: 5_000,
            options: std::collections::BTreeMap::from([(
                "poll_interval_ms".into(),
                serde_json::json!(10_u64),
            )]),
        }),
        ..Default::default()
    };

    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("orchestrator", orchestrator_executor)
            .with_provider("worker", worker_executor)
            .with_model_binding(
                "orchestrator-model",
                ModelBinding {
                    provider_id: "orchestrator".into(),
                    upstream_model: "mock-orchestrator".into(),
                },
            )
            .with_model_binding(
                "worker-model",
                ModelBinding {
                    provider_id: "worker".into(),
                    upstream_model: "mock-worker".into(),
                },
            )
            .with_tool(
                "spawn_bg_child",
                Arc::new(SpawnBackgroundChildTool {
                    manager: manager.clone(),
                }),
            )
            .with_plugin(
                "background",
                Arc::new(BackgroundTaskPlugin::new(manager.clone())),
            )
            .with_agent_spec(orchestrator_spec)
            .with_agent_spec(worker_local)
            .with_agent_spec(worker_remote)
            .with_thread_run_store(store.clone())
            .build_unchecked()
            .expect("build runtime"),
    );

    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "loopback-test".to_string(),
        MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );

    let router = build_router(&state).with_state(state);
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let http = reqwest::Client::new();
    let task_id = format!("loopback-cancel-{}", uuid::Uuid::now_v7());
    let submit_resp = post_json(
        &http,
        &format!("{base_url}/v1/a2a/orchestrator/message:send"),
        serde_json::json!({
            "message": {
                "taskId": task_id,
                "contextId": task_id,
                "messageId": format!("msg-{task_id}"),
                "role": "ROLE_USER",
                "parts": [{"text": "spawn a child and cancel yourself"}]
            }
        }),
    )
    .await;

    assert_eq!(submit_resp["task"]["id"].as_str(), Some(task_id.as_str()));
    assert_eq!(
        submit_resp["task"]["status"]["state"].as_str(),
        Some("TASK_STATE_COMPLETED"),
        "orchestrator task should complete after observing remote cancellation: {submit_resp}"
    );

    let orchestrator_run = store
        .load_run(&task_id)
        .await
        .expect("load orchestrator run")
        .expect("orchestrator run should exist");
    assert_eq!(orchestrator_run.status, RunStatus::Done);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let worker_run = loop {
        let page = store
            .list_runs(&RunQuery {
                limit: 20,
                ..RunQuery::default()
            })
            .await
            .expect("list runs");
        let worker_runs = page
            .items
            .into_iter()
            .filter(|run| run.agent_id == "worker")
            .collect::<Vec<_>>();
        if let Some(run) = worker_runs
            .iter()
            .find(|run| run.termination_reason == Some(TerminationReason::Cancelled))
            .cloned()
        {
            break run;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cancelled worker run did not appear before deadline: {worker_runs:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(worker_run.status, RunStatus::Done);
    assert_eq!(
        worker_run.termination_reason,
        Some(TerminationReason::Cancelled)
    );

    let tasks = manager.list(&worker_run.thread_id).await;
    assert_eq!(
        tasks.len(),
        1,
        "expected one background child task: {tasks:?}"
    );
    assert_eq!(tasks[0].status, TaskStatus::Cancelled);
    assert_eq!(tasks[0].task_type, "background_child");
    assert_eq!(
        tasks[0].parent_context.run_id.as_deref(),
        Some(worker_run.run_id.as_str())
    );

    let runs = get_json(&http, &format!("{base_url}/v1/runs")).await;
    let worker_statuses = runs["items"]
        .as_array()
        .expect("runs items")
        .iter()
        .filter(|item| item["agent_id"].as_str() == Some("worker"))
        .filter_map(|item| item["status"].as_str())
        .collect::<Vec<_>>();
    assert!(
        worker_statuses.contains(&"done"),
        "expected worker run to be visible in run listing: {runs}"
    );

    server_handle.abort();
}

/// Dual-server A2A loopback: orchestrator server delegates over HTTP to a
/// separate worker server, where the remote worker self-cancels and cascades
/// cancellation to its background descendants.
#[tokio::test]
async fn a2a_dual_server_remote_self_cancel_cascades_background_children() {
    let worker_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind worker listener");
    let worker_addr = worker_listener.local_addr().expect("worker local addr");
    let worker_base_url = format!("http://127.0.0.1:{}", worker_addr.port());
    let worker_manager = Arc::new(BackgroundTaskManager::new());
    let worker_store = Arc::new(InMemoryStore::new());
    let worker_runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider(
                "worker",
                Arc::new(ScriptedLlm::new(vec![ScriptedLlm::tool_call_response(
                    vec![
                        ToolCall::new(
                            "spawn-1",
                            "spawn_bg_child",
                            serde_json::json!({"name": "leaf"}),
                        ),
                        ToolCall::new(
                            "cancel-1",
                            "cancel_task",
                            serde_json::json!({"target": {"relation": "self"}}),
                        ),
                    ],
                )])),
            )
            .with_model_binding(
                "worker-model",
                ModelBinding {
                    provider_id: "worker".into(),
                    upstream_model: "mock-worker".into(),
                },
            )
            .with_tool(
                "spawn_bg_child",
                Arc::new(SpawnBackgroundChildTool {
                    manager: worker_manager.clone(),
                }),
            )
            .with_plugin(
                "background",
                Arc::new(BackgroundTaskPlugin::new(worker_manager.clone())),
            )
            .with_agent_spec(AgentSpec {
                id: "worker".into(),
                model_id: "worker-model".into(),
                system_prompt: "spawn a child and cancel yourself".into(),
                max_rounds: 2,
                plugin_ids: vec!["background".into()],
                ..Default::default()
            })
            .with_thread_run_store(worker_store.clone())
            .build()
            .expect("build worker runtime"),
    );
    let worker_mailbox = Arc::new(Mailbox::new(
        worker_runtime.clone(),
        Arc::new(InMemoryMailboxStore::new()),
        worker_store.clone(),
        "dual-loopback-worker".to_string(),
        MailboxConfig::default(),
    ));
    let worker_state = AppState::new(
        worker_runtime.clone(),
        worker_mailbox,
        worker_store.clone(),
        worker_runtime.resolver_arc(),
        ServerConfig::default(),
    );
    let worker_router = build_router(&worker_state).with_state(worker_state);
    let worker_handle = tokio::spawn(async move {
        axum::serve(worker_listener, worker_router).await.ok();
    });

    let orchestrator_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind orchestrator listener");
    let orchestrator_addr = orchestrator_listener
        .local_addr()
        .expect("orchestrator local addr");
    let orchestrator_base_url = format!("http://127.0.0.1:{}", orchestrator_addr.port());
    let orchestrator_store = Arc::new(InMemoryStore::new());
    let orchestrator_runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider(
                "orchestrator",
                Arc::new(ScriptedLlm::new(vec![
                    ScriptedLlm::tool_call_response(vec![ToolCall::new(
                        "delegate-1",
                        "agent_run_worker-remote",
                        serde_json::json!({"prompt": "spawn a child and cancel yourself"}),
                    )]),
                    ScriptedLlm::text_response("observed remote cancellation"),
                ])),
            )
            .with_model_binding(
                "orchestrator-model",
                ModelBinding {
                    provider_id: "orchestrator".into(),
                    upstream_model: "mock-orchestrator".into(),
                },
            )
            .with_model_binding(
                "worker-remote-model",
                ModelBinding {
                    provider_id: "orchestrator".into(),
                    upstream_model: "mock-worker-remote".into(),
                },
            )
            .with_agent_spec(AgentSpec {
                id: "orchestrator".into(),
                model_id: "orchestrator-model".into(),
                system_prompt: "delegate to the remote worker".into(),
                max_rounds: 2,
                delegates: vec!["worker-remote".into()],
                ..Default::default()
            })
            .with_agent_spec(AgentSpec {
                id: "worker-remote".into(),
                model_id: "worker-remote-model".into(),
                system_prompt: "remote worker".into(),
                endpoint: Some(RemoteEndpoint {
                    backend: "a2a".into(),
                    base_url: format!("{worker_base_url}/v1/a2a"),
                    auth: None,
                    target: Some("worker".into()),
                    timeout_ms: 5_000,
                    options: std::collections::BTreeMap::from([(
                        "poll_interval_ms".into(),
                        serde_json::json!(10_u64),
                    )]),
                }),
                ..Default::default()
            })
            .with_thread_run_store(orchestrator_store.clone())
            .build_unchecked()
            .expect("build orchestrator runtime"),
    );
    let orchestrator_mailbox = Arc::new(Mailbox::new(
        orchestrator_runtime.clone(),
        Arc::new(InMemoryMailboxStore::new()),
        orchestrator_store.clone(),
        "dual-loopback-orchestrator".to_string(),
        MailboxConfig::default(),
    ));
    let orchestrator_state = AppState::new(
        orchestrator_runtime.clone(),
        orchestrator_mailbox,
        orchestrator_store.clone(),
        orchestrator_runtime.resolver_arc(),
        ServerConfig::default(),
    );
    let orchestrator_router = build_router(&orchestrator_state).with_state(orchestrator_state);
    let orchestrator_handle = tokio::spawn(async move {
        axum::serve(orchestrator_listener, orchestrator_router)
            .await
            .ok();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let http = reqwest::Client::new();
    let task_id = format!("dual-loopback-cancel-{}", uuid::Uuid::now_v7());
    let submit_resp = post_json(
        &http,
        &format!("{orchestrator_base_url}/v1/a2a/orchestrator/message:send"),
        serde_json::json!({
            "message": {
                "taskId": task_id,
                "contextId": task_id,
                "messageId": format!("msg-{task_id}"),
                "role": "ROLE_USER",
                "parts": [{"text": "spawn a child and cancel yourself"}]
            }
        }),
    )
    .await;

    assert_eq!(submit_resp["task"]["id"].as_str(), Some(task_id.as_str()));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut task_state = submit_resp["task"]["status"]["state"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    while task_state != "TASK_STATE_COMPLETED" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "orchestrator task did not complete before deadline; last state: {task_state}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snapshot = get_json(
            &http,
            &format!("{orchestrator_base_url}/v1/a2a/orchestrator/tasks/{task_id}"),
        )
        .await;
        task_state = snapshot["status"]["state"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
    }

    let orchestrator_run = orchestrator_store
        .load_run(&task_id)
        .await
        .expect("load orchestrator run")
        .expect("orchestrator run should exist");
    assert_eq!(orchestrator_run.status, RunStatus::Done);
    assert_eq!(orchestrator_run.agent_id, "orchestrator");

    let worker_run = loop {
        let page = worker_store
            .list_runs(&RunQuery {
                limit: 20,
                ..RunQuery::default()
            })
            .await
            .expect("list worker runs");
        let worker_runs = page
            .items
            .into_iter()
            .filter(|run| run.agent_id == "worker")
            .collect::<Vec<_>>();
        if let Some(run) = worker_runs
            .iter()
            .find(|run| run.termination_reason == Some(TerminationReason::Cancelled))
            .cloned()
        {
            break run;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cancelled worker run did not appear before deadline: {worker_runs:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(worker_run.status, RunStatus::Done);
    assert_eq!(
        worker_run.termination_reason,
        Some(TerminationReason::Cancelled)
    );

    let worker_tasks = worker_manager.list(&worker_run.thread_id).await;
    assert_eq!(
        worker_tasks.len(),
        1,
        "expected one background child task on worker server: {worker_tasks:?}"
    );
    assert_eq!(worker_tasks[0].status, TaskStatus::Cancelled);
    assert_eq!(worker_tasks[0].task_type, "background_child");
    assert_eq!(
        worker_tasks[0].parent_context.run_id.as_deref(),
        Some(worker_run.run_id.as_str())
    );

    let orchestrator_runs = get_json(&http, &format!("{orchestrator_base_url}/v1/runs")).await;
    let orchestrator_agents = orchestrator_runs["items"]
        .as_array()
        .expect("orchestrator runs items")
        .iter()
        .filter_map(|item| item["agent_id"].as_str())
        .collect::<Vec<_>>();
    assert!(orchestrator_agents.contains(&"orchestrator"));
    assert!(
        !orchestrator_agents.contains(&"worker"),
        "worker run should not be persisted on orchestrator server: {orchestrator_runs}"
    );

    let worker_runs = get_json(&http, &format!("{worker_base_url}/v1/runs")).await;
    let worker_statuses = worker_runs["items"]
        .as_array()
        .expect("worker runs items")
        .iter()
        .filter(|item| item["agent_id"].as_str() == Some("worker"))
        .filter_map(|item| item["status"].as_str())
        .collect::<Vec<_>>();
    assert!(
        worker_statuses.contains(&"done"),
        "expected worker run to be visible on worker server: {worker_runs}"
    );

    orchestrator_handle.abort();
    worker_handle.abort();
}

/// Full A2A loopback: orchestrator → HTTP A2A → worker (same server) → done.
///
/// The orchestrator's system prompt instructs it to ALWAYS delegate via the
/// `agent_run_worker` tool. The worker simply replies with "PONG".
/// We verify the orchestrator completes and the worker thread is created.
#[tokio::test]
#[ignore] // requires OPENAI_API_KEY
async fn a2a_loopback_orchestrator_delegates_to_worker() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        // OPENAI_API_KEY not set — skip
        return;
    }

    let model_name = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    // We need to know the server port before building agents (for RemoteEndpoint).
    // Bind early so we can capture the address.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to random port");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    // Register two worker specs with different IDs:
    //   - "worker" (no endpoint) — runs locally when A2A task_send arrives
    //   - "worker-remote" (with endpoint) — orchestrator delegates to it via A2A HTTP
    // This avoids infinite recursion (worker resolving its own endpoint).

    let worker_local = AgentSpec::new("worker")
        .with_model_id("default")
        .with_system_prompt(
            "You are a simple worker agent. \
             Always reply with exactly one word: PONG. \
             Do not use any tools. Do not add explanation.",
        )
        .with_max_rounds(1);

    let worker_remote = AgentSpec {
        id: "worker-remote".into(),
        model_id: "default".into(),
        system_prompt: "Reply with PONG".into(),
        max_rounds: 1,
        endpoint: Some(RemoteEndpoint {
            backend: "a2a".into(),
            base_url: base_url.clone(),
            auth: None,
            target: Some("worker".into()),
            timeout_ms: 60_000,
            options: std::collections::BTreeMap::from([(
                "poll_interval_ms".into(),
                serde_json::json!(500),
            )]),
        }),
        ..Default::default()
    };

    let orchestrator_spec = AgentSpec::new("orchestrator")
        .with_model_id("default")
        .with_system_prompt(
            "You are an orchestrator. You MUST delegate every user request to the worker \
             by calling the `agent_run_worker-remote` tool with the user's message as the prompt. \
             After receiving the worker's response, reply to the user with the worker's answer. \
             ALWAYS call the tool first — never answer directly.",
        )
        .with_max_rounds(3)
        .with_delegate("worker-remote");

    let executor: Arc<dyn awaken_contract::contract::executor::LlmExecutor> =
        Arc::new(GenaiExecutor::with_client(build_genai_client()));

    let store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("default", executor)
            .with_model_binding(
                "default",
                ModelBinding {
                    provider_id: "default".into(),
                    upstream_model: model_name,
                },
            )
            .with_agent_spec(orchestrator_spec)
            .with_agent_spec(worker_local)
            .with_agent_spec(worker_remote)
            .with_thread_run_store(store.clone())
            // Use build_unchecked: the "worker-remote" spec has an endpoint
            // and cannot be resolved locally (by design — it's a remote delegate).
            .build_unchecked()
            .expect("build runtime"),
    );

    let mailbox_store = Arc::new(InMemoryMailboxStore::new());
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        mailbox_store,
        store.clone(),
        "loopback-test".to_string(),
        MailboxConfig::default(),
    ));

    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        ServerConfig::default(),
    );

    // -- Start server ---------------------------------------------------------

    let router = build_router(&state).with_state(state);
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Give the server a moment to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // -- Submit task to orchestrator via A2A -----------------------------------

    let http = reqwest::Client::new();
    let task_id = format!("loopback-{}", uuid::Uuid::now_v7());

    let submit_resp = post_json(
        &http,
        &format!("{base_url}/v1/a2a/orchestrator/message:send"),
        serde_json::json!({
            "message": {
                "taskId": task_id,
                "contextId": task_id,
                "messageId": format!("msg-{task_id}"),
                "role": "ROLE_USER",
                "parts": [{"text": "Say ping to the worker"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        }),
    )
    .await;

    assert_eq!(
        submit_resp["task"]["id"].as_str(),
        Some(task_id.as_str()),
        "task ID preserved"
    );
    assert_eq!(
        submit_resp["task"]["status"]["state"].as_str(),
        Some("TASK_STATE_SUBMITTED"),
        "initial state is submitted"
    );

    // -- Poll until orchestrator completes ------------------------------------

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut final_state = String::new();

    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;

        if tokio::time::Instant::now() >= deadline {
            panic!("orchestrator did not complete within 90s — last state: {final_state}");
        }

        // Poll via latest thread run (task_id = thread_id)
        let url = format!("{base_url}/v1/threads/{task_id}/runs/latest");
        let resp = http.get(&url).send().await.expect("poll request");

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Run not created yet, keep waiting
            continue;
        }

        let body: Value = resp.json().await.expect("poll JSON");
        let status = body["status"].as_str().unwrap_or("unknown").to_string();

        tracing::info!("[loopback] orchestrator status: {status}");
        final_state = status.clone();

        if status == "done" {
            break;
        }
    }

    assert_eq!(final_state, "done", "orchestrator should complete");

    // -- Verify worker was invoked (its thread exists) ------------------------

    // Verify ALL runs — the A2A backend submitted a task for the worker,
    // creating a separate thread/run on the same server.
    let runs_resp = get_json(&http, &format!("{base_url}/v1/runs")).await;
    let items = runs_resp["items"].as_array().expect("runs items array");

    let agent_ids: Vec<&str> = items
        .iter()
        .filter_map(|r| r["agent_id"].as_str())
        .collect();

    tracing::info!("[loopback] all runs agents: {agent_ids:?}");

    assert!(
        agent_ids.contains(&"orchestrator"),
        "orchestrator run should exist: {agent_ids:?}"
    );

    // The worker run should exist (A2aBackend now sends agentId="worker-remote",
    // which maps to the local "worker" agent on the server side via A2A routing).
    // The server resolves the agentId from the A2A request; if unknown, falls
    // back to default. Either way, we expect at least 2 runs.
    assert!(
        items.len() >= 2,
        "expected at least 2 runs (orchestrator + worker), got {}: {agent_ids:?}",
        items.len()
    );

    // loopback verified: orchestrator delegated to worker via HTTP

    server_handle.abort();
}
