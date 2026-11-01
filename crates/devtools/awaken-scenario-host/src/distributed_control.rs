//! Deterministic model adapter over the production split-Control deployment.
//!
//! This module adds no alternate Control service. It loads the same typed
//! deployment as the product command and supplies only the existing model
//! publication SPI so a provider-free cluster can exercise the real
//! Control-to-Coordinator boundaries.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_sync::LeafLock;
use axum::Router;
use axum::body::Body;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};

use crate::model_publication::{DistributedProviderPublicationResolver, scenario_model_catalog};

#[expect(
    clippy::disallowed_methods,
    reason = "the scenario harness is its own process root and reads its scenario configuration from the environment"
)]
pub async fn serve_distributed_control(
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<(), String> {
    // The scenario process is a thin model adapter, not a second config
    // owner. Its deployment fixture supplies the same explicit file used by the
    // production Control migration command; omission retains the normal default
    // path for focused unit tests.
    let config_path = std::env::var_os("AWAKEN_SCENARIO_CONFIG").map(std::path::PathBuf::from);
    let deployment = awaken_process_startup::config::ResolvedDeployment::load(
        awaken_process_startup::config::ConfigOverrides {
            config_path,
            ..Default::default()
        },
        clock,
    )
    .map_err(|error| format!("distributed Control deployment configuration: {error}"))?;
    if deployment.role != awaken_process_startup::config::Role::Control {
        return Err("distributed Control scenario requires role = \"control\"".into());
    }
    let key = deployment
        .seal_key
        .load_or_create()
        .map_err(|error| format!("distributed Control seal key: {error}"))?;
    let resolver = Arc::new(DistributedProviderPublicationResolver::new(
        scenario_model_catalog("adr71-echo").await,
    ));
    let process = awaken_process_startup::prepare_control_process_with_publication_resolver(
        &deployment,
        &key,
        resolver,
    )
    .await
    .map_err(|error| format!("assemble distributed Control scenario: {error}"))?;
    awaken_process_startup::serve_prepared_control(deployment, process).await
}

/// Hermetic Anthropic Messages edge used by the distributed-role cluster proof.
/// It is a protocol fixture only: publication, credential selection/materialization,
/// Worker execution, and commit all remain on their production paths.
pub fn build_distributed_provider_router() -> Router {
    distributed_provider_router(Arc::new(ProviderFixture::default()))
}

fn distributed_provider_router(fixture: Arc<ProviderFixture>) -> Router {
    Router::new()
        .route("/v1/messages", post(distributed_provider_message))
        .route("/__scenario/invocations/{marker}", get(observe_invocations))
        .route(
            "/__scenario/invocations/{marker}/release",
            post(release_invocations),
        )
        .with_state(fixture)
}

#[derive(Default)]
struct ProviderFixture {
    calls: LeafLock<BTreeMap<String, Vec<FixtureInvocation>>>,
}

struct FixtureInvocation {
    started: tokio::sync::watch::Sender<bool>,
    completed: tokio::sync::watch::Sender<bool>,
    release: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    result: String,
}

impl ProviderFixture {
    fn admit(&self, marker: &str) -> tokio::sync::oneshot::Receiver<()> {
        let (release, mut released) = tokio::sync::watch::channel(false);
        let (result, receipt) = tokio::sync::oneshot::channel();
        let (started, _) = tokio::sync::watch::channel(false);
        let (completed, _) = tokio::sync::watch::channel(false);
        let began = started.clone();
        let finished = completed.clone();
        // Provider-owned work is registered before the handler awaits. Client
        // disconnect cannot cancel this independently owned mock computation.
        // The release command wakes it; only the task publishes completion.
        let task = tokio::spawn(async move {
            began.send_replace(true);
            while !*released.borrow_and_update() {
                if released.changed().await.is_err() {
                    return;
                }
            }
            finished.send_replace(true);
            let _ = result.send(());
        });
        self.calls
            .lock()
            .entry(marker.to_owned())
            .or_default()
            .push(FixtureInvocation {
                started,
                completed,
                release,
                task,
                result: format!("ADR71-PROVIDER:{marker}"),
            });
        receipt
    }

    fn observation(&self, marker: &str) -> Option<serde_json::Value> {
        let calls = self.calls.lock();
        let calls = calls.get(marker)?;
        Some(serde_json::json!({
            "marker": marker, "accepted": calls.len(),
            "started": calls.iter().filter(|call| *call.started.borrow()).count(),
            "running": calls.iter().filter(|call| *call.started.borrow()
                && !*call.completed.borrow() && !call.task.is_finished()).count(),
            "completed": calls.iter().filter(|call| *call.completed.borrow()).count(),
            "results": calls.iter().filter(|call| *call.completed.borrow())
                .map(|call| &call.result).collect::<Vec<_>>(),
        }))
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        == Some("adr71-provider-secret")
}

async fn observe_invocations(
    State(fixture): State<Arc<ProviderFixture>>,
    Path(marker): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match fixture.observation(&marker) {
        Some(observation) => Json(observation).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn release_invocations(
    State(fixture): State<Arc<ProviderFixture>>,
    Path(marker): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let calls = fixture.calls.lock();
    let Some(calls) = calls.get(&marker) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    for call in calls {
        let _ = call.release.send(true);
    }
    StatusCode::ACCEPTED.into_response()
}

const CHECKPOINT_MARKER_PREFIX: &str = "ADR71-NATIVE-CHECKPOINT";

async fn distributed_provider_message(
    State(fixture): State<Arc<ProviderFixture>>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Response<Body> {
    if !authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "type": "error",
                "error": { "type": "authentication_error", "message": "invalid key" }
            })),
        )
            .into_response();
    }
    let texts = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .filter(|message| message["role"] == "user")
        .map(|message| message.get("content").map(message_text).unwrap_or_default())
        .collect::<Vec<_>>();
    let text = texts.first().cloned().unwrap_or_default();
    // Ordinary requests retain the old latest-user semantics. Only this one
    // fixture's tool_result has no direct user text and keeps its original marker.
    let text = if text.is_empty() {
        texts
            .into_iter()
            .find(|text| text.starts_with(CHECKPOINT_MARKER_PREFIX))
            .unwrap_or(text)
    } else {
        text
    };
    // Scenario-only arrival evidence for the k3d crash test. JSON encoding keeps
    // one request on one log line; durable product truth remains ThreadCommit.
    eprintln!(
        "ADR71_PROVIDER_REQUEST {}",
        serde_json::to_string(&text).expect("Provider fixture text encoding")
    );
    if text.starts_with("ADR71-NATIVE-INFLIGHT-CRASH") {
        // This correlation is fixture-only, not a production operation query
        // contract. It deliberately accepts every HTTP attempt independently
        // so an accidental replay increments accepted work rather than being
        // hidden by mock idempotency. Native journal/slot remains production.
        if fixture.admit(&text).await.is_err() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let reply = format!("ADR71-PROVIDER:{text}");
    let checkpoint = text.starts_with(CHECKPOINT_MARKER_PREFIX);
    let returned = request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|block| {
            block["type"] == "tool_result" && block["tool_use_id"] == "tool_adr71_checkpoint"
        })
        .collect::<Vec<_>>();
    let (content, stop_reason) = if checkpoint && returned.is_empty() {
        // This fixture requests a client-owned input, not an external action:
        // the production Runtime must commit its real Awaiting ticket before
        // the gate kills the Worker. Never manufacture a resume/closure fact.
        let tools = request["tools"].as_array().filter(|tools| tools.len() == 1);
        let Some(name) = tools.and_then(|tools| tools[0]["name"].as_str()) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "type":"error", "error":{"type":"invalid_request_error",
                    "message":"checkpoint fixture requires its one declared client tool"}
                })),
            )
                .into_response();
        };
        (
            serde_json::json!([{"type":"tool_use", "id":"tool_adr71_checkpoint",
            "name":name, "input":{"reason":"await the original client"}}]),
            "tool_use",
        )
    } else if checkpoint {
        if returned.len() != 1 || returned[0]["is_error"] == true {
            return StatusCode::BAD_REQUEST.into_response();
        }
        (
            serde_json::json!([{"type":"text", "text":format!("{reply}:{}",
            message_text(&returned[0]["content"]))}]),
            "end_turn",
        )
    } else {
        (
            serde_json::json!([{"type":"text", "text":reply}]),
            "end_turn",
        )
    };
    let response = serde_json::json!({
        "id": "msg_adr71",
        "type": "message",
        "role": "assistant",
        "model": request.get("model").and_then(serde_json::Value::as_str).unwrap_or("adr71-echo"),
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    if request.get("stream").and_then(serde_json::Value::as_bool) == Some(true) {
        return anthropic_message_stream(&response);
    }
    Json(response).into_response()
}

fn message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type") == Some(&serde_json::Value::String("text".into())))
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

fn anthropic_message_stream(response: &serde_json::Value) -> Response<Body> {
    // One protocol renderer owns old text and the new client-input fixture.
    // JSON and SSE consume the same response value; neither owns Run state.
    let frame =
        |event: &str, value: serde_json::Value| format!("event: {event}\ndata: {value}\n\n");
    let mut initial = response.clone();
    initial["content"] = serde_json::json!([]);
    initial["stop_reason"] = serde_json::Value::Null;
    initial["usage"]["output_tokens"] = serde_json::json!(0);
    let mut body = frame(
        "message_start",
        serde_json::json!({
            "type":"message_start", "message":initial,
        }),
    );
    for (index, block) in response["content"]
        .as_array()
        .expect("fixture content")
        .iter()
        .enumerate()
    {
        let mut start = block.clone();
        let delta = if block["type"] == "tool_use" {
            start["input"] = serde_json::json!({});
            serde_json::json!({"type":"input_json_delta",
                "partial_json":serde_json::to_string(&block["input"]).expect("fixture input")})
        } else {
            start["text"] = serde_json::json!("");
            serde_json::json!({"type":"text_delta", "text":block["text"]})
        };
        body.push_str(&frame(
            "content_block_start",
            serde_json::json!({
                "type":"content_block_start", "index":index, "content_block":start,
            }),
        ));
        body.push_str(&frame(
            "content_block_delta",
            serde_json::json!({
                "type":"content_block_delta", "index":index, "delta":delta,
            }),
        ));
        body.push_str(&frame(
            "content_block_stop",
            serde_json::json!({
                "type":"content_block_stop", "index":index,
            }),
        ));
    }
    body.push_str(&frame("message_delta", serde_json::json!({
        "type":"message_delta", "delta":{"stop_reason":response["stop_reason"], "stop_sequence":null},
        "usage":{"output_tokens":1},
    })));
    body.push_str(&frame(
        "message_stop",
        serde_json::json!({"type":"message_stop"}),
    ));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .expect("static Anthropic response")
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::*;

    #[tokio::test]
    async fn provider_double_retains_actual_work_and_result_after_caller_drop() {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        use std::time::Duration;
        // Mock contract decision table: M1 unauthenticated control =>401 and
        // zero changes; M2 absent invocation=>404, never a stopped receipt;
        // M3 accepted held invocation + caller Drop=>independent task remains
        // running; M4 explicit release=>task publishes retained exact result;
        // M5 repeated release stutters; M6 a repeated model POST creates TWO
        // executions so a product replay defect cannot hide behind mock dedup.
        // Controlled polling reaches the actual handler's first await; watches
        // observe that task's actual start/completion, not elapsed-time settling.
        // Live k3d owns Worker crash, original journal and physical-slot oracles.
        let fixture = Arc::new(ProviderFixture::default());
        let app = distributed_provider_router(fixture.clone());
        let marker = "ADR71-NATIVE-INFLIGHT-CRASH-unit";
        let path = format!("/__scenario/invocations/{marker}");
        let request = || {
            Request::post("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "adr71-provider-secret")
            .body(Body::from(serde_json::json!({"stream":true,"messages":[{"role":"user","content":marker}]}).to_string()))
            .unwrap()
        };
        let absent = app
            .clone()
            .oneshot(
                Request::get(&path)
                    .header("x-api-key", "adr71-provider-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(absent.status(), StatusCode::NOT_FOUND, "M2");
        let mut inference = Box::pin(app.clone().oneshot(request()));
        poll_fn(|cx| {
            assert!(inference.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let (mut started, mut completed) = {
            let calls = fixture.calls.lock();
            let call = &calls[marker][0];
            (call.started.subscribe(), call.completed.subscribe())
        };
        tokio::time::timeout(Duration::from_secs(2), started.wait_for(|value| *value))
            .await
            .unwrap()
            .unwrap();
        drop(inference);
        assert_eq!(
            fixture.observation(marker).unwrap()["running"],
            1,
            "M3 actual owned task"
        );
        let release = format!("{path}/release");
        for method in ["GET", "POST"] {
            let denied = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(if method == "GET" { &path } else { &release })
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "M1");
        }
        assert_eq!(
            fixture.observation(marker).unwrap()["completed"],
            0,
            "M1 no completion"
        );
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::post(&release)
                        .header("x-api-key", "adr71-provider-secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED, "M4/M5");
        }
        tokio::time::timeout(Duration::from_secs(2), completed.wait_for(|value| *value))
            .await
            .unwrap()
            .unwrap();
        let observed = fixture.observation(marker).unwrap();
        assert_eq!(observed["accepted"], 1, "M5");
        assert_eq!(observed["completed"], 1, "M4");
        assert_eq!(observed["running"], 0, "M4");
        assert_eq!(
            observed["results"],
            serde_json::json!([format!("ADR71-PROVIDER:{marker}")]),
            "M4 exact retained output"
        );

        let mut replay = Box::pin(app.clone().oneshot(request()));
        poll_fn(|cx| {
            assert!(replay.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            fixture.observation(marker).unwrap()["accepted"],
            2,
            "M6 no mock dedup"
        );
        let response = app
            .oneshot(
                Request::post(&release)
                    .header("x-api-key", "adr71-provider-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let response = tokio::time::timeout(Duration::from_secs(2), replay)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "M6 response ladder");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("event: message_stop")
        );
        assert_eq!(
            fixture.observation(marker).unwrap()["results"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "M6 two real completions"
        );
    }

    #[tokio::test]
    async fn distributed_provider_requires_the_projected_secret_and_speaks_streaming_messages() {
        // Cause/effect decision table: P1 absent/wrong projected key -> 401 and no
        // model response; P2 exact key + streaming Messages request -> one valid
        // Anthropic SSE terminal ladder carrying the input marker. This fixture
        // never substitutes for Credential or Runtime behavior.
        let app = build_distributed_provider_router();
        let denied = app
            .clone()
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "P1");

        let accepted = app
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .header("x-api-key", "adr71-provider-secret")
                    .body(Body::from(
                        r#"{"model":"adr71-echo","stream":true,"messages":[{"role":"user","content":"marker"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK, "P2");
        let body = to_bytes(accepted.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("ADR71-PROVIDER:marker"), "P2: {body}");
        assert!(body.contains("event: message_stop"), "P2: {body}");
    }
    #[tokio::test]
    async fn checkpoint_fixture_requests_one_client_input_then_consumes_its_actual_result() {
        // C/E: C1 auth+one declared client tool+original marker -> ToolUse ladder,
        // no detached Provider invocation. C2 its exact tool_result -> one text
        // terminal ladder; duplicate/error result or absent descriptor ->400.
        // Both wire modes share one renderer/value. This tests the protocol
        // fixture only; real Awaiting/ticket/recovery remain the live gate's job.
        let app = build_distributed_provider_router();
        for stream in [false, true] {
            for returned in [false, true] {
                let mut messages = vec![serde_json::json!({"role":"user",
                    "content":"ADR71-NATIVE-CHECKPOINT-unit"})];
                if returned {
                    messages.push(serde_json::json!({"role":"user", "content":[{
                        "type":"tool_result", "tool_use_id":"tool_adr71_checkpoint",
                        "content":"resume-original", "is_error":false}]}));
                }
                let response = app
                    .clone()
                    .oneshot(
                        Request::post("/v1/messages")
                            .header("content-type", "application/json")
                            .header("x-api-key", "adr71-provider-secret")
                            .body(Body::from(
                                serde_json::json!({"model":"adr71-echo", "stream":stream,
                        "tools":[{"name":"checkpoint","input_schema":{"type":"object"}}],
                        "messages":messages})
                                .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body = String::from_utf8(body.to_vec()).unwrap();
                assert!(body.contains(if returned {
                    "resume-original"
                } else {
                    "tool_adr71_checkpoint"
                }));
                assert!(body.contains(if returned { "end_turn" } else { "tool_use" }));
                if stream {
                    assert!(body.starts_with("event: message_start\ndata: "));
                    assert!(
                        body.ends_with(
                            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                        )
                    );
                    let values = body
                        .split("\n\n")
                        .filter_map(|frame| {
                            frame.lines().find_map(|line| line.strip_prefix("data: "))
                        })
                        .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(values[0]["message"]["content"], serde_json::json!([]));
                    assert_eq!(
                        values[1]["content_block"]["type"],
                        if returned { "text" } else { "tool_use" }
                    );
                    assert_eq!(
                        values[2]["delta"]["type"],
                        if returned {
                            "text_delta"
                        } else {
                            "input_json_delta"
                        }
                    );
                }
            }
        }
        for malformed in [
            serde_json::json!([]),
            serde_json::json!([{"name":"a"},{"name":"b"}]),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/messages")
                        .header("content-type", "application/json")
                        .header("x-api-key", "adr71-provider-secret")
                        .body(Body::from(
                            serde_json::json!({"tools":malformed,
                    "messages":[{"role":"user","content":"ADR71-NATIVE-CHECKPOINT-unit"}]})
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        for content in [
            serde_json::json!([{"type":"tool_result","tool_use_id":"tool_adr71_checkpoint","content":"x","is_error":true}]),
            serde_json::json!([{"type":"tool_result","tool_use_id":"tool_adr71_checkpoint","content":"x"},
                              {"type":"tool_result","tool_use_id":"tool_adr71_checkpoint","content":"x"}]),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/messages")
                        .header("content-type", "application/json")
                        .header("x-api-key", "adr71-provider-secret")
                        .body(Body::from(
                            serde_json::json!({"tools":[{"name":"checkpoint"}],
                    "messages":[{"role":"user","content":"ADR71-NATIVE-CHECKPOINT-unit"},
                                {"role":"user","content":content}]})
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
}
