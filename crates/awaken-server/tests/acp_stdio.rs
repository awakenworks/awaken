use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use agent_client_protocol::{self as acp, Agent as _};
use async_trait::async_trait;
use awaken_contract::contract::content::ContentBlock as RuntimeContentBlock;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{
    StopReason as RuntimeStopReason, StreamResult, TokenUsage,
};
use awaken_contract::contract::message::ToolCall as RuntimeToolCall;
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_permission::PermissionPlugin;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelEntry;
use awaken_server::protocols::acp::stdio::{
    JsonRpcResponse, parse_request, serialize_response, serve_stdio_io,
};
use serde_json::{Value, json};
use tokio::io::{BufReader, split};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[test]
fn stdio_jsonrpc_roundtrip() {
    let req = parse_request(r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#).unwrap();
    assert_eq!(req.method, "initialize");
    let out = serialize_response(&JsonRpcResponse::success(
        req.id,
        serde_json::json!({"ok":true}),
    ));
    assert!(out.contains("\"ok\":true"));
}

#[derive(Default, Clone)]
struct TestClient {
    notifications: Arc<Mutex<Vec<acp::SessionNotification>>>,
    permission_requests: Arc<Mutex<Vec<acp::RequestPermissionRequest>>>,
}

#[async_trait(?Send)]
impl acp::Client for TestClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        self.permission_requests.lock().unwrap().push(args.clone());
        let selected = args
            .options
            .iter()
            .find(|option| {
                matches!(
                    option.kind,
                    acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways
                )
            })
            .map(|option| option.option_id.clone())
            .unwrap_or_else(|| acp::PermissionOptionId::new("opt_allow_once"));

        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(selected)),
        ))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.notifications.lock().unwrap().push(args);
        Ok(())
    }
}

struct EchoExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for EchoExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let user_text = request
            .messages
            .iter()
            .rev()
            .find_map(|message| {
                if message.role == awaken_contract::contract::message::Role::User {
                    Some(message.text())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        Ok(StreamResult {
            content: vec![RuntimeContentBlock::text(format!("echo: {user_text}"))],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(RuntimeStopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "echo"
    }
}

struct PrefixExecutor {
    prefix: &'static str,
}

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for PrefixExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let user_text = request
            .messages
            .iter()
            .rev()
            .find_map(|message| {
                if message.role == awaken_contract::contract::message::Role::User {
                    Some(message.text())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        Ok(StreamResult {
            content: vec![RuntimeContentBlock::text(format!(
                "{}: {user_text}",
                self.prefix
            ))],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(RuntimeStopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        self.prefix
    }
}

struct MultimodalEchoExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for MultimodalEchoExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let user_message = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == awaken_contract::contract::message::Role::User)
            .expect("user message should exist");

        let mut text_blocks = 0;
        let mut image_blocks = 0;
        let mut audio_blocks = 0;

        for block in &user_message.content {
            match block {
                RuntimeContentBlock::Text { .. } => text_blocks += 1,
                RuntimeContentBlock::Image { .. } => image_blocks += 1,
                RuntimeContentBlock::Audio { .. } => audio_blocks += 1,
                _ => {}
            }
        }

        Ok(StreamResult {
            content: vec![RuntimeContentBlock::text(format!(
                "text={text_blocks} image={image_blocks} audio={audio_blocks}"
            ))],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(RuntimeStopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "multimodal-echo"
    }
}

struct ToolCallMockExecutor {
    call_count: AtomicUsize,
}

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for ToolCallMockExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![RuntimeToolCall::new(
                    "call_1",
                    "get_weather",
                    json!({"location": "Tokyo"}),
                )],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(RuntimeStopReason::ToolUse),
                has_incomplete_tool_calls: false,
            })
        } else {
            Ok(StreamResult {
                content: vec![RuntimeContentBlock::text("It's sunny in Tokyo")],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(RuntimeStopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }
    }

    fn name(&self) -> &str {
        "tool-mock"
    }
}

struct GetWeatherTool;

#[async_trait]
impl Tool for GetWeatherTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "get_weather",
            "get_weather",
            "Gets the weather for a location",
        )
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolResult::success("get_weather", json!({"temp": 25, "condition": "sunny"})).into())
    }
}

fn echo_runtime() -> Arc<awaken_runtime::AgentRuntime> {
    let builder = AgentRuntimeBuilder::new()
        .with_model(
            "test-model",
            ModelEntry {
                provider: "mock".into(),
                model_name: "mock-model".into(),
            },
        )
        .with_provider("mock", Arc::new(EchoExecutor))
        .with_agent_spec(AgentSpec {
            id: "echo".into(),
            model: "test-model".into(),
            system_prompt: "You are an echo bot".into(),
            max_rounds: 2,
            ..Default::default()
        });
    Arc::new(builder.build().expect("build runtime"))
}

fn permission_runtime() -> Arc<awaken_runtime::AgentRuntime> {
    let mut sections = HashMap::new();
    sections.insert("permission".to_string(), json!({"default_behavior": "ask"}));

    let builder = AgentRuntimeBuilder::new()
        .with_model(
            "test-model",
            ModelEntry {
                provider: "mock".into(),
                model_name: "mock-model".into(),
            },
        )
        .with_provider(
            "mock",
            Arc::new(ToolCallMockExecutor {
                call_count: AtomicUsize::new(0),
            }),
        )
        .with_tool("get_weather", Arc::new(GetWeatherTool))
        .with_plugin("permission", Arc::new(PermissionPlugin))
        .with_agent_spec(AgentSpec {
            id: "weather".into(),
            model: "test-model".into(),
            system_prompt: "You are a weather bot".into(),
            max_rounds: 2,
            plugin_ids: vec!["permission".into()],
            sections,
            ..Default::default()
        });

    Arc::new(builder.build().expect("build runtime"))
}

fn multi_agent_runtime() -> Arc<awaken_runtime::AgentRuntime> {
    let builder = AgentRuntimeBuilder::new()
        .with_model(
            "alpha-model",
            ModelEntry {
                provider: "alpha-provider".into(),
                model_name: "mock-alpha".into(),
            },
        )
        .with_model(
            "beta-model",
            ModelEntry {
                provider: "beta-provider".into(),
                model_name: "mock-beta".into(),
            },
        )
        .with_provider(
            "alpha-provider",
            Arc::new(PrefixExecutor { prefix: "alpha" }),
        )
        .with_provider("beta-provider", Arc::new(PrefixExecutor { prefix: "beta" }))
        .with_agent_spec(AgentSpec {
            id: "alpha".into(),
            model: "alpha-model".into(),
            system_prompt: "You are alpha".into(),
            max_rounds: 2,
            ..Default::default()
        })
        .with_agent_spec(AgentSpec {
            id: "beta".into(),
            model: "beta-model".into(),
            system_prompt: "You are beta".into(),
            max_rounds: 2,
            ..Default::default()
        });

    Arc::new(builder.build().expect("build runtime"))
}

fn multimodal_runtime() -> Arc<awaken_runtime::AgentRuntime> {
    let builder = AgentRuntimeBuilder::new()
        .with_model(
            "test-model",
            ModelEntry {
                provider: "mock".into(),
                model_name: "mock-model".into(),
            },
        )
        .with_provider("mock", Arc::new(MultimodalEchoExecutor))
        .with_agent_spec(AgentSpec {
            id: "multimodal".into(),
            model: "test-model".into(),
            system_prompt: "You inspect multimodal input".into(),
            max_rounds: 2,
            ..Default::default()
        });

    Arc::new(builder.build().expect("build runtime"))
}

async fn with_sdk_client<F, Fut>(runtime: Arc<awaken_runtime::AgentRuntime>, test: F)
where
    F: FnOnce(acp::ClientSideConnection, TestClient) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let client = TestClient::default();
            let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
            let (client_reader, client_writer) = split(client_stream);
            let (server_reader, server_writer) = split(server_stream);

            let server_task = tokio::task::spawn_local(async move {
                serve_stdio_io(runtime, BufReader::new(server_reader), server_writer).await;
            });

            let (conn, io_task) = acp::ClientSideConnection::new(
                client.clone(),
                client_writer.compat_write(),
                client_reader.compat(),
                |future| {
                    tokio::task::spawn_local(future);
                },
            );
            tokio::task::spawn_local(io_task);

            test(conn, client.clone()).await;

            drop(client);
            server_task.abort();
            let _ = server_task.await;
        })
        .await;
}

#[tokio::test]
async fn sdk_client_can_complete_prompt_turn() {
    with_sdk_client(echo_runtime(), |conn, client| async move {
        conn.initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
            .await
            .expect("initialize should succeed");

        let session = conn
            .new_session(acp::NewSessionRequest::new("/tmp"))
            .await
            .expect("new_session should succeed");

        let response = conn
            .prompt(acp::PromptRequest::new(
                session.session_id.clone(),
                vec!["hello acp".into()],
            ))
            .await
            .expect("prompt should succeed");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

        let notifications = client.notifications.lock().unwrap();
        assert!(
            notifications.iter().any(|notification| {
                matches!(
                    &notification.update,
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: acp::ContentBlock::Text(text),
                        ..
                    }) if text.text.contains("echo: hello acp")
                )
            }),
            "expected echoed agent message, got: {notifications:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn sdk_client_can_approve_permission_request() {
    with_sdk_client(permission_runtime(), |conn, client| async move {
        conn.initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
            .await
            .expect("initialize should succeed");

        let session = conn
            .new_session(acp::NewSessionRequest::new("/tmp"))
            .await
            .expect("new_session should succeed");

        let response = conn
            .prompt(acp::PromptRequest::new(
                session.session_id.clone(),
                vec!["what's the weather in Tokyo?".into()],
            ))
            .await
            .expect("prompt should succeed");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);

        let permission_requests = client.permission_requests.lock().unwrap();
        assert_eq!(
            permission_requests.len(),
            1,
            "expected one permission request"
        );

        let notifications = client.notifications.lock().unwrap();
        assert!(
            notifications.iter().any(|notification| {
                matches!(&notification.update, acp::SessionUpdate::ToolCall(_))
            }),
            "expected tool call notification, got: {notifications:?}"
        );
        assert!(
            notifications.iter().any(|notification| {
                matches!(
                    &notification.update,
                    acp::SessionUpdate::ToolCallUpdate(update)
                        if update.fields.status == Some(acp::ToolCallStatus::Completed)
                )
            }),
            "expected completed tool update, got: {notifications:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn sdk_client_can_select_agent_via_session_config_option() {
    with_sdk_client(multi_agent_runtime(), |conn, client| async move {
        conn.initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
            .await
            .expect("initialize should succeed");

        let session = conn
            .new_session(acp::NewSessionRequest::new("/tmp"))
            .await
            .expect("new_session should succeed");
        let config_options = session
            .config_options
            .clone()
            .expect("multi-agent session should expose config options");

        assert_eq!(config_options.len(), 1);
        assert_eq!(config_options[0].id.0.as_ref(), "agent");
        match &config_options[0].kind {
            acp::SessionConfigKind::Select(select) => {
                assert_eq!(select.current_value.0.as_ref(), "alpha");
                match &select.options {
                    acp::SessionConfigSelectOptions::Ungrouped(options) => {
                        assert_eq!(options.len(), 2);
                    }
                    other => panic!("expected ungrouped options, got: {other:?}"),
                }
            }
            other => panic!("expected select config option, got: {other:?}"),
        }

        let response = conn
            .set_session_config_option(acp::SetSessionConfigOptionRequest::new(
                session.session_id.clone(),
                "agent",
                "beta",
            ))
            .await
            .expect("set_session_config_option should succeed");

        match &response.config_options[0].kind {
            acp::SessionConfigKind::Select(select) => {
                assert_eq!(select.current_value.0.as_ref(), "beta");
            }
            other => panic!("expected select config option, got: {other:?}"),
        }

        let prompt = conn
            .prompt(acp::PromptRequest::new(
                session.session_id.clone(),
                vec!["hello from config".into()],
            ))
            .await
            .expect("prompt should succeed");
        assert_eq!(prompt.stop_reason, acp::StopReason::EndTurn);

        let notifications = client.notifications.lock().unwrap();
        assert!(
            notifications.iter().any(|notification| {
                matches!(
                    &notification.update,
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: acp::ContentBlock::Text(text),
                        ..
                    }) if text.text.contains("beta: hello from config")
                )
            }),
            "expected beta agent response, got: {notifications:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn sdk_client_can_send_multimodal_prompt_blocks() {
    with_sdk_client(multimodal_runtime(), |conn, client| async move {
        conn.initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
            .await
            .expect("initialize should succeed");

        let session = conn
            .new_session(acp::NewSessionRequest::new("/tmp"))
            .await
            .expect("new_session should succeed");

        let prompt = conn
            .prompt(acp::PromptRequest::new(
                session.session_id.clone(),
                vec![
                    "describe the attachments".into(),
                    acp::ContentBlock::Image(acp::ImageContent::new("aGVsbG8=", "image/png")),
                    acp::ContentBlock::Audio(acp::AudioContent::new("d29ybGQ=", "audio/mpeg")),
                ],
            ))
            .await
            .expect("prompt should succeed");
        assert_eq!(prompt.stop_reason, acp::StopReason::EndTurn);

        let notifications = client.notifications.lock().unwrap();
        assert!(
            notifications.iter().any(|notification| {
                matches!(
                    &notification.update,
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: acp::ContentBlock::Text(text),
                        ..
                    }) if text.text.contains("text=1 image=1 audio=1")
                )
            }),
            "expected multimodal summary, got: {notifications:?}"
        );
    })
    .await;
}
