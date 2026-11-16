//! Checkpoint-and-release idle retention on the shipped topology: Sessions of
//! a self-hosted Environment run on the co-located registered Worker under
//! their Session Work. An idle Session is checkpointed, its Work and Sandbox
//! are released, and the next message resumes it from the checkpoint.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_protocol_managed_testkit::DurableFact;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const CHECKPOINT_FORMAT: &str = "awaken-fs-tar-v1";
const FILE: &str = "idle-checkpoint.txt";
/// Upper bound for one lifecycle-driven transition: the checkpoint deadline
/// (one second) plus the Coordinator's continuation pass and the Worker's
/// one-second preparation poll.
const LIFECYCLE: std::time::Duration = std::time::Duration::from_secs(60);

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// `write <text>` writes `<text>` to [`FILE`] in the Session's Sandbox;
/// `read` reads it back and replies with exactly what the tool returned.
struct FileProbeModel;

#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for FileProbeModel {
    async fn infer(
        &self,
        request: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        use awaken_agent_contract::agent::message::Role;
        use awaken_runtime_contract::llm::AssistantOutput;
        let last_user = request
            .messages
            .iter()
            .rposition(|message| message.role == Role::User)
            .unwrap_or_default();
        let user = awaken_session_executor::block_text(&request.messages[last_user].content);
        let tool_result = request.messages[last_user..]
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .map(|message| awaken_session_executor::block_text(&message.content));
        let call = |tool: &str, arguments: Value| {
            AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                call_id: format!("{tool}-{last_user}"),
                tool_id: tool.into(),
                arguments,
            }])
        };
        let output = match (user.strip_prefix("write "), tool_result) {
            (Some(text), None) => call("write", json!({ "path": FILE, "content": text })),
            (None, None) => call("read", json!({ "path": FILE })),
            (Some(_), Some(_)) => AssistantOutput::text("written"),
            (None, Some(result)) => AssistantOutput::text(format!("read: {result}")),
        };
        Ok(awaken_runtime_contract::llm::ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The co-located Worker's checkpoint byte custody: an exact-key, byte-equal
/// idempotent object map (the `SandboxCheckpointStore` contract the S3
/// adapter implements in production). `uploads` can hold every upload until
/// the test releases it, to admit a message while a checkpoint is in flight.
#[derive(Default)]
struct MemoryCheckpointStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    reads: AtomicUsize,
    uploads: Option<tokio::sync::Semaphore>,
}

impl MemoryCheckpointStore {
    fn holding_uploads() -> Self {
        Self {
            uploads: Some(tokio::sync::Semaphore::new(0)),
            ..Self::default()
        }
    }

    fn release_uploads(&self) {
        if let Some(uploads) = &self.uploads {
            uploads.add_permits(tokio::sync::Semaphore::MAX_PERMITS / 2);
        }
    }

    fn objects(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxCheckpointStore for MemoryCheckpointStore {
    async fn put(
        &self,
        metadata: &awaken_provisioning_contract::CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<
        awaken_provisioning_contract::StoredCheckpointObject,
        awaken_provisioning_contract::SandboxError,
    > {
        if let Some(uploads) = &self.uploads {
            drop(uploads.acquire().await.expect("upload gate stays open"));
        }
        let id = format!(
            "{}/{}/{}/{}",
            metadata.workspace_id,
            metadata.session_id,
            metadata.generation_id,
            metadata.suspend_effect_id
        );
        let mut objects = self.objects.lock().unwrap();
        match objects.get(&id) {
            Some(existing) if *existing != bytes => {
                return Err(awaken_provisioning_contract::SandboxError::conflict(
                    "checkpoint key is already bound to different bytes",
                ));
            }
            Some(_) => {}
            None => {
                objects.insert(id.clone(), bytes.clone());
            }
        }
        Ok(awaken_provisioning_contract::StoredCheckpointObject {
            id,
            digest: awaken_resource_contract::content_id(&bytes),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.objects
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                awaken_provisioning_contract::SandboxError::backend("get", "no such checkpoint")
            })
    }

    async fn delete(&self, id: &str) -> Result<(), awaken_provisioning_contract::SandboxError> {
        self.objects.lock().unwrap().remove(id);
        Ok(())
    }
}

async fn start(store: Arc<MemoryCheckpointStore>) -> awaken_process_startup::ColocatedTestProcess {
    let binding =
        awaken_runtime_contract::resolved::ModelBinding::parse("default", "file-probe", "genai")
            .unwrap();
    awaken_process_startup::start_all_in_one_with_worker(
        awaken_process_startup::ColocatedTestStores::Volatile,
        Some((Arc::new(FileProbeModel), binding)),
        move |inputs| {
            let mut worker = awaken_cli::PreparedLocalWorker::over_stores(inputs.stores)
                .with_environment_checkpoint_store(store);
            if let Some(inference) = inputs.host_inference {
                worker = worker.with_inference_materializer(inference);
            }
            Arc::new(worker)
        },
    )
    .await
    .expect("start the AllInOne test process with its co-located Worker")
}

/// Create a self-hosted Environment (the built-in one is immutable) bound to
/// a checkpoint-and-release policy whose idle deadline is one second and
/// whose checkpoints are retained for `retention_secs`.
async fn checkpoint_environment(app: &Router, retention_secs: u64) -> String {
    checkpoint_environment_with(app, retention_secs, json!({ "type": "self_hosted" })).await
}

async fn checkpoint_environment_with(app: &Router, retention_secs: u64, config: Value) -> String {
    let (status, body) = call(
        app,
        "POST",
        "/v1/awaken/sandbox-execution-policies",
        Some(json!({
            "id": "idle-checkpoint",
            "config": {},
            "idle_retention": {
                "mode": "checkpoint_and_release",
                "checkpoint_after_secs": 1,
                "retention_secs": retention_secs,
                "max_checkpoint_bytes": 67_108_864u64,
                "max_checkpoint_duration_secs": 60,
                "checkpoint_format": CHECKPOINT_FORMAT
            }
        })),
    )
    .await;
    assert!(status.is_success(), "create policy: {status} {body}");
    let (status, environment) = call(
        app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "idle-checkpoint", "config": config })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create Environment: {environment}");
    let environment = environment["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/awaken/environments/{environment}/sandbox-execution-policy"),
        Some(json!({ "policy_id": "idle-checkpoint", "version": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bind policy: {body}");
    environment
}

async fn create_session(app: &Router, environment: &str) -> String {
    let (status, agent) = call(
        app,
        "POST",
        "/v1/agents",
        Some(json!({
            "name": "idle-checkpoint-agent",
            "model": "file-probe",
            "tools": [{
                "type": "agent_toolset_20260401",
                "configs": [],
                "default_config": {
                    "enabled": true,
                    "permission_policy": { "type": "always_allow" }
                }
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    let (status, session) = call(
        app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": agent["id"], "environment_id": environment })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    session["id"].as_str().unwrap().to_owned()
}

async fn paced_events(app: &Router, session: &str) -> (u16, Value) {
    // awaken-allow(wait): pace one read under the Org Read request bucket; the settle condition stays the DurableFact
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (status, events) = call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events?limit=500"),
        None,
    )
    .await;
    (status.as_u16(), events)
}

async fn send(app: &Router, session: &str, text: &str) -> Value {
    let (status, receipt) = call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        Some(json!({
            "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    receipt
}

/// Wait until the command of `receipt` is processed and some agent message
/// satisfies `reply`; returns the agent messages' texts.
async fn processed(
    app: &Router,
    session: &str,
    receipt: &Value,
    why: &str,
    reply: impl Fn(&str) -> bool,
) -> Vec<String> {
    let observed = awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::command_processed(receipt),
        LIFECYCLE,
        why,
        || paced_events(app, session),
        |events| {
            events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"].as_str().is_some_and(&reply)
            })
        },
    )
    .await;
    observed["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["type"] == "agent.message")
        .filter_map(|event| event["content"][0]["text"].as_str().map(str::to_owned))
        .collect()
}

/// Send `text` and wait until its Run committed an agent message containing
/// `reply`.
async fn converse(app: &Router, session: &str, text: &str, reply: &str, why: &str) {
    let receipt = send(app, session, text).await;
    processed(app, session, &receipt, why, |said| said.contains(reply)).await;
}

/// The committed Environment phase of `session`, from its execution evidence.
async fn environment_phase(app: &Router, session: &str) -> String {
    let (status, evidence) = call(
        app,
        "GET",
        &format!("/v1/awaken/sessions/{session}/execution-evidence?include_inspection=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{evidence}");
    evidence["environment"]["phase"]
        .as_str()
        .unwrap_or("absent")
        .to_owned()
}

/// The state of `session`'s Work item in `environment`.
async fn session_work_state(app: &Router, environment: &str, session: &str) -> String {
    let (status, work) = call(
        app,
        "GET",
        &format!("/v1/environments/{environment}/work?limit=100"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{work}");
    work["data"]
        .as_array()
        .expect("Work page")
        .iter()
        .find(|item| item["data"]["type"] == "session" && item["data"]["id"] == session)
        .and_then(|item| item["state"].as_str())
        .unwrap_or("absent")
        .to_owned()
}

/// Poll the committed root until its Environment reaches `phase` and its Work
/// reaches `work`. Idle transitions are lifecycle-driven (no request commits
/// them), so a bounded poll over committed truth is the only observation.
async fn await_idle_state(
    app: &Router,
    environment: &str,
    session: &str,
    (phase, work): (&str, &str),
    why: &str,
) {
    let deadline = tokio::time::Instant::now() + LIFECYCLE;
    // awaken-allow(wait): the idle checkpoint is committed by the lifecycle supervisor and the Worker's preparation poll; no request or Event anchors it, so this bounded loop polls the committed Environment phase and Work state
    loop {
        let observed = (
            environment_phase(app, session).await,
            session_work_state(app, environment, session).await,
        );
        if observed.0 == phase && observed.1 == work {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let (_, root) = call(app, "GET", &format!("/v1/sessions/{session}"), None).await;
            let (_, evidence) = call(
                app,
                "GET",
                &format!(
                    "/v1/awaken/sessions/{session}/execution-evidence?include_inspection=true"
                ),
                None,
            )
            .await;
            panic!(
                "{why}: still {observed:?} after {LIFECYCLE:?}; status {}; execution {}; environment {}; runs {}",
                root["status"],
                evidence["execution"],
                evidence["environment"],
                evidence["runs"]
                    .as_array()
                    .map(|runs| runs
                        .iter()
                        .map(|run| run["state"].to_string())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_default()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

const RELEASED: (&str, &str) = ("hibernated", "stopped");

async fn shut_down(process: awaken_process_startup::ColocatedTestProcess) {
    process
        .service_lifecycle
        .shutdown(std::time::Duration::from_secs(10))
        .await
        .expect("the AllInOne test process drains cleanly");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_worker_session_is_checkpointed_released_and_resumed_from_its_checkpoint() {
    // Causes: C1 a self-hosted Environment (its Sessions run on the
    // co-located registered Worker under Session Work) is bound to a
    // checkpoint-and-release policy whose idle deadline is one second; C2 a
    // Run writes a file into the Session's Sandbox and settles; C3 the
    // Session then stays idle past the deadline; C4 a later message reads the
    // file; C5 two messages arrive together after a release.
    // Effects: E1 the Session is created (this layout was refused before);
    // E2 the idle Session is checkpointed into the Worker's checkpoint store
    // and Hibernates; E3 its Session Work is released (stopped) and its
    // Sandbox disposed; E4 the next message restores the Sandbox from the
    // checkpoint, the Run reads the file written before the release, and the
    // Session's Work is stopped again after it; E5 the restored Sandbox is
    // checkpointed and released again; E6 both messages run, in order, on
    // one restore of the one checkpoint (a duplicate resume joins it).
    // Rules: R1 C1 => E1; R2 C1+C2+C3 => E2+E3; R3 R2+C4 => E4;
    // R4 R3+C3 => E5; R5 R4+C5 => E6.
    let store = Arc::new(MemoryCheckpointStore::default());
    let process = start(store.clone()).await;
    let app = process.public_router.clone();
    let environment = checkpoint_environment(&app, 3_600).await;
    let session = create_session(&app, &environment).await;

    converse(
        &app,
        &session,
        "write survives the release",
        "written",
        "C2 the first Run writes the file",
    )
    .await;
    await_idle_state(&app, &environment, &session, RELEASED, "R2 E2/E3").await;
    assert_eq!(store.objects(), 1, "R2 E2 one checkpoint object");

    converse(
        &app,
        &session,
        "read",
        "read: survives the release",
        "R3 E4 the next message restores the file",
    )
    .await;
    assert_eq!(store.reads(), 1, "R3 E4 one restore read");
    await_idle_state(&app, &environment, &session, RELEASED, "R4 E5").await;

    let first = send(&app, &session, "read").await;
    let second = send(&app, &session, "read").await;
    for (rule, receipt) in [("R5 first", &first), ("R5 second", &second)] {
        processed(&app, &session, receipt, rule, |said| {
            said.contains("read: survives the release")
        })
        .await;
    }
    assert_eq!(store.reads(), 2, "R5 E6 one restore for both messages");
    await_idle_state(&app, &environment, &session, RELEASED, "R5 released").await;
    shut_down(process).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_during_the_checkpoint_waits_for_the_release_and_then_resumes() {
    // Causes: C1 the idle Session's checkpoint upload is in flight (the
    // store holds it); C2 a message arrives then; C3 the upload completes.
    // Effects: E1 the message does not run while the suspend is unfinished:
    // the Environment stays Suspending and the command stays unprocessed;
    // E2 once the suspend completes, the message restores from the
    // checkpoint and reads the file written before it.
    // Rules: R1 C1+C2 => E1; R2 R1+C3 => E2.
    let store = Arc::new(MemoryCheckpointStore::holding_uploads());
    let process = start(store.clone()).await;
    let app = process.public_router.clone();
    let environment = checkpoint_environment(&app, 3_600).await;
    let session = create_session(&app, &environment).await;
    converse(
        &app,
        &session,
        "write written before the suspend",
        "written",
        "the first Run writes the file",
    )
    .await;
    await_idle_state(
        &app,
        &environment,
        &session,
        ("suspending", "active"),
        "C1 the suspend holds its Work while uploading",
    )
    .await;
    let receipt = send(&app, &session, "read").await;
    // awaken-allow(wait): a bounded window in which the admitted message must not run while its Environment is suspending; nothing commits during it
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(
        environment_phase(&app, &session).await,
        "suspending",
        "R1 E1"
    );
    let (_, events) = paced_events(&app, &session).await;
    let input = events["data"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|event| event["id"] == receipt["data"][0]["id"])
        .cloned()
        .unwrap_or_else(|| panic!("the message is listed: {events}"));
    assert!(input["processed_at"].is_null(), "R1 E1 {input}");
    store.release_uploads();
    processed(&app, &session, &receipt, "R2 E2", |said| {
        said.contains("read: written before the suspend")
    })
    .await;
    assert_eq!(store.reads(), 1, "R2 E2 restored from the checkpoint");
    shut_down(process).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_checkpoint_is_deleted_and_the_next_message_starts_fresh() {
    // Causes: C1 the checkpoint's retention (forty seconds from the
    // Sandbox's creation, after the idle checkpoint completed) ends while the
    // Session is still idle; C2 a later message reads the file.
    // Effects: E1 the checkpoint object is deleted without waiting for a
    // message, the Environment leaves Hibernated and the Session Work is
    // stopped again; E2 the next message runs in a fresh Sandbox from the
    // frozen Environment: the file written before the release is gone.
    // Rules: R1 C1 => E1; R2 R1+C2 => E2.
    let store = Arc::new(MemoryCheckpointStore::default());
    let process = start(store.clone()).await;
    let app = process.public_router.clone();
    let environment = checkpoint_environment(&app, 40).await;
    let session = create_session(&app, &environment).await;
    converse(
        &app,
        &session,
        "write expires with its checkpoint",
        "written",
        "the first Run writes the file",
    )
    .await;
    await_idle_state(&app, &environment, &session, RELEASED, "C1 released").await;
    assert_eq!(store.objects(), 1, "C1 one checkpoint object");
    let deadline = tokio::time::Instant::now() + LIFECYCLE;
    // awaken-allow(wait): retention ends on the wall clock and the lifecycle supervisor deletes the checkpoint; no request anchors it, so this bounded loop polls the committed phase, Work state and store
    loop {
        let observed = (
            environment_phase(&app, &session).await,
            session_work_state(&app, &environment, &session).await,
            store.objects(),
        );
        if observed.0 != "hibernated" && observed.1 == "stopped" && observed.2 == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "R1 E1 the expired checkpoint is deleted: still {observed:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let receipt = send(&app, &session, "read").await;
    let replies = processed(&app, &session, &receipt, "R2 E2", |said| {
        said.starts_with("read: ")
    })
    .await;
    assert!(
        replies
            .iter()
            .filter(|said| said.starts_with("read: "))
            .all(|said| !said.contains("expires with its checkpoint")),
        "R2 E2 a fresh Sandbox: {replies:?}"
    );
    assert_eq!(store.reads(), 0, "R2 E2 nothing restored");
    shut_down(process).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_environment_session_on_the_registered_worker_takes_the_same_path() {
    // Causes: C1 a cloud Environment (no Session Work) bound to the same
    // checkpoint-and-release policy, realized by the co-located registered
    // Worker; C2 a Run writes the file; C3 idle past the deadline; C4 a later
    // message. Effects: E1 the idle Session Hibernates through the real
    // Worker projection install and checkpoint; E2 the next message restores
    // the file. Rules: R1 C1+C2+C3 => E1; R2 R1+C4 => E2.
    let store = Arc::new(MemoryCheckpointStore::default());
    let process = start(store.clone()).await;
    let app = process.public_router.clone();
    let environment = checkpoint_environment_with(&app, 3_600, json!({ "type": "cloud" })).await;
    let session = create_session(&app, &environment).await;
    converse(
        &app,
        &session,
        "write survives a cloud release",
        "written",
        "C2 the first Run writes the file",
    )
    .await;
    await_idle_state(
        &app,
        &environment,
        &session,
        ("hibernated", "absent"),
        "R1 E1 a cloud Environment holds no Session Work",
    )
    .await;
    assert_eq!(store.objects(), 1, "R1 E1");
    converse(
        &app,
        &session,
        "read",
        "read: survives a cloud release",
        "R2 E2",
    )
    .await;
    shut_down(process).await;
}
