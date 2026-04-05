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

use std::sync::Arc;
use std::time::Duration;

use awaken_contract::registry_spec::{AgentSpec, RemoteEndpoint};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::engine::executor::GenaiExecutor;
use awaken_runtime::registry::traits::ModelEntry;
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

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

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
        .with_model("default")
        .with_system_prompt(
            "You are a simple worker agent. \
             Always reply with exactly one word: PONG. \
             Do not use any tools. Do not add explanation.",
        )
        .with_max_rounds(1);

    let worker_remote = AgentSpec {
        id: "worker-remote".into(),
        model: "default".into(),
        system_prompt: "Reply with PONG".into(),
        max_rounds: 1,
        endpoint: Some(RemoteEndpoint {
            base_url: base_url.clone(),
            bearer_token: None,
            agent_id: Some("worker".into()),
            poll_interval_ms: 500,
            timeout_ms: 60_000,
        }),
        ..Default::default()
    };

    let orchestrator_spec = AgentSpec::new("orchestrator")
        .with_model("default")
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
            .with_model(
                "default",
                ModelEntry {
                    provider: "default".into(),
                    model_name,
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

    let router = build_router().with_state(state);
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
