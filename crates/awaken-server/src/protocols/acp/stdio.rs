//! ACP stdio server backed by the official `agent-client-protocol` Rust SDK.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client as _};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use awaken_contract::contract::content::ContentBlock as RuntimeContentBlock;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_runtime::AgentRuntime;

use super::encoder::{AcpEncoder, AcpOutput};
use super::types::{
    AgentCapabilities, ContentBlock, EmbeddedResource, EmbeddedResourceResource, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, RequestPermissionResponse, ResourceLink,
};

struct SessionState {
    agent_id: Option<String>,
    thread_id: String,
}

type Sessions = Arc<Mutex<HashMap<String, SessionState>>>;

#[derive(Debug)]
enum ClientCommand {
    SessionNotification {
        notification: acp::SessionNotification,
        response_tx: oneshot::Sender<acp::Result<()>>,
    },
    RequestPermission {
        request: acp::RequestPermissionRequest,
        response_tx: oneshot::Sender<acp::Result<acp::RequestPermissionResponse>>,
    },
}

struct AcpAgent {
    runtime: Arc<AgentRuntime>,
    sessions: Sessions,
    client_tx: mpsc::UnboundedSender<ClientCommand>,
}

impl AcpAgent {
    fn new(
        runtime: Arc<AgentRuntime>,
        sessions: Sessions,
        client_tx: mpsc::UnboundedSender<ClientCommand>,
    ) -> Self {
        Self {
            runtime,
            sessions,
            client_tx,
        }
    }

    async fn send_notification(&self, notification: acp::SessionNotification) -> acp::Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.client_tx
            .send(ClientCommand::SessionNotification {
                notification,
                response_tx,
            })
            .map_err(|_| acp::Error::internal_error())?;
        response_rx
            .await
            .map_err(|_| acp::Error::internal_error())?
    }

    async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        self.client_tx
            .send(ClientCommand::RequestPermission {
                request,
                response_tx,
            })
            .map_err(|_| acp::Error::internal_error())?;
        response_rx
            .await
            .map_err(|_| acp::Error::internal_error())?
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for AcpAgent {
    async fn initialize(&self, args: InitializeRequest) -> acp::Result<InitializeResponse> {
        Ok(build_initialize_response(args))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(&self, args: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        if !args.cwd.is_absolute() {
            return Err(acp::Error::new(-32602, "cwd must be an absolute path"));
        }
        if !args.mcp_servers.is_empty() {
            return Err(acp::Error::new(
                -32602,
                "mcpServers are not supported by this ACP stdio server",
            ));
        }

        let session_id = generate_session_id();
        let thread_id = uuid::Uuid::now_v7().to_string();
        let agent_id = select_session_agent_id(self.runtime.resolver())?;

        self.sessions.lock().await.insert(
            session_id.clone(),
            SessionState {
                agent_id,
                thread_id,
            },
        );

        Ok(NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, args: PromptRequest) -> acp::Result<PromptResponse> {
        let session_id = args.session_id.0.to_string();
        let content = prompt_blocks_to_message_content(&args.prompt)
            .map_err(|e| acp::Error::new(-32602, e))?;
        if content.is_empty() {
            return Err(acp::Error::new(
                -32602,
                "prompt must contain at least one supported content block",
            ));
        }

        let (agent_id, thread_id) = {
            let guard = self.sessions.lock().await;
            match guard.get(&session_id) {
                Some(state) => (state.agent_id.clone(), state.thread_id.clone()),
                None => {
                    return Err(acp::Error::new(
                        -32002,
                        format!("session not found: {session_id}"),
                    ));
                }
            }
        };

        let messages = vec![Message::user_with_content(content)];
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
        let mut run_request = awaken_runtime::RunRequest::new(thread_id.clone(), messages);
        if let Some(agent_id) = agent_id {
            run_request = run_request.with_agent_id(agent_id);
        }
        let runtime = Arc::clone(&self.runtime);
        let run_handle =
            tokio::spawn(async move { runtime.run(run_request, Arc::new(sink)).await });

        let mut encoder = AcpEncoder::new().with_session_id(&session_id);
        let mut final_stop_reason = acp::StopReason::EndTurn;
        let mut prompt_error: Option<acp::Error> = None;

        while let Some(event) = event_rx.recv().await {
            for output in encoder.on_agent_event(&event) {
                match output {
                    AcpOutput::Notification(notification) => {
                        self.send_notification(notification)
                            .await
                            .map_err(acp::Error::into_internal_error)?;
                    }
                    AcpOutput::PermissionRequest(request) => {
                        let tool_call_id = request.tool_call.tool_call_id.0.to_string();
                        let response = self.request_permission(request).await?;
                        let resume = permission_response_to_resume(response)?;
                        if !self
                            .runtime
                            .send_decisions(&thread_id, vec![(tool_call_id, resume)])
                        {
                            return Err(acp::Error::new(
                                -32603,
                                "no active run for permission response",
                            ));
                        }
                    }
                    AcpOutput::Finished(reason) => {
                        final_stop_reason = reason;
                    }
                    AcpOutput::Error { message, code } => {
                        let mut err = acp::Error::new(-32603, message);
                        if let Some(code) = code {
                            err = err.data(serde_json::json!({ "code": code }));
                        }
                        prompt_error = Some(err);
                        self.runtime.cancel(&thread_id);
                        break;
                    }
                }
            }

            if prompt_error.is_some() {
                break;
            }
        }

        if let Some(err) = prompt_error {
            run_handle.abort();
            let _ = run_handle.await;
            return Err(err);
        }

        match run_handle.await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => return Err(acp::Error::into_internal_error(err)),
            Err(err) => return Err(acp::Error::into_internal_error(err)),
        }

        Ok(PromptResponse::new(final_stop_reason))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        let thread_id = {
            let guard = self.sessions.lock().await;
            guard
                .get(args.session_id.0.as_ref())
                .map(|state| state.thread_id.clone())
        };
        if let Some(thread_id) = thread_id {
            self.runtime.cancel(&thread_id);
        }
        Ok(())
    }
}

fn build_initialize_response(request: InitializeRequest) -> InitializeResponse {
    let capabilities = AgentCapabilities::new();
    InitializeResponse::new(request.protocol_version)
        .agent_capabilities(capabilities)
        .agent_info(Implementation::new("awaken-acp", env!("CARGO_PKG_VERSION")))
}

async fn run_client_commands(
    conn: acp::AgentSideConnection,
    mut rx: mpsc::UnboundedReceiver<ClientCommand>,
) {
    while let Some(command) = rx.recv().await {
        match command {
            ClientCommand::SessionNotification {
                notification,
                response_tx,
            } => {
                let _ = response_tx.send(conn.session_notification(notification).await);
            }
            ClientCommand::RequestPermission {
                request,
                response_tx,
            } => {
                let _ = response_tx.send(conn.request_permission(request).await);
            }
        }
    }
}

fn generate_session_id() -> String {
    format!("sess_{}", uuid::Uuid::now_v7().simple())
}

fn select_session_agent_id(
    resolver: &dyn awaken_runtime::AgentResolver,
) -> acp::Result<Option<String>> {
    let mut agent_ids = resolver.agent_ids();
    agent_ids.sort();
    agent_ids.dedup();

    if agent_ids.iter().any(|agent_id| agent_id == "default") {
        return Ok(Some("default".to_string()));
    }

    match agent_ids.as_slice() {
        [] => Ok(None),
        [agent_id] => Ok(Some(agent_id.clone())),
        _ => Err(acp::Error::new(
            -32603,
            "ACP stdio requires a `default` agent or exactly one registered agent",
        )),
    }
}

pub async fn serve_stdio_io<R, W>(runtime: Arc<AgentRuntime>, input: R, output: W)
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
            let (client_tx, client_rx) = mpsc::unbounded_channel();
            let agent = AcpAgent::new(runtime, sessions, client_tx);

            let (conn, io_task) = acp::AgentSideConnection::new(
                agent,
                output.compat_write(),
                input.compat(),
                |future| {
                    tokio::task::spawn_local(future);
                },
            );

            let client_task = tokio::task::spawn_local(run_client_commands(conn, client_rx));
            let io_result = io_task.await;
            client_task.abort();
            let _ = client_task.await;

            if let Err(err) = io_result {
                tracing::warn!(error = ?err, "acp stdio connection terminated with error");
            }
        })
        .await;
}

pub async fn serve_stdio(runtime: Arc<AgentRuntime>) {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_stdio_io(runtime, stdin, stdout).await;
}

fn permission_response_to_resume(
    response: RequestPermissionResponse,
) -> acp::Result<ToolCallResume> {
    let (action, result) = match &response.outcome {
        acp::RequestPermissionOutcome::Cancelled => (
            ResumeDecisionAction::Cancel,
            serde_json::json!({
                "kind": "permission_decision",
                "approved": false,
                "cancelled": true,
            }),
        ),
        acp::RequestPermissionOutcome::Selected(selected) => {
            permission_option_to_resume(&selected.option_id.0)?
        }
        _ => {
            return Err(acp::Error::new(
                -32602,
                "unsupported ACP permission response outcome",
            ));
        }
    };

    Ok(ToolCallResume {
        decision_id: uuid::Uuid::now_v7().to_string(),
        action,
        result,
        reason: None,
        updated_at: unix_timestamp_millis(),
    })
}

fn permission_option_to_resume(option_id: &str) -> acp::Result<(ResumeDecisionAction, Value)> {
    let (action, approved, policy) = match option_id {
        "opt_allow_once" => (ResumeDecisionAction::Resume, true, "allow_once"),
        "opt_allow_always" => (ResumeDecisionAction::Resume, true, "allow_always"),
        "opt_reject_once" => (ResumeDecisionAction::Cancel, false, "reject_once"),
        "opt_reject_always" => (ResumeDecisionAction::Cancel, false, "reject_always"),
        other => {
            return Err(acp::Error::new(
                -32602,
                format!("unsupported ACP permission option id: {other}"),
            ));
        }
    };

    Ok((
        action,
        serde_json::json!({
            "kind": "permission_decision",
            "approved": approved,
            "policy": policy,
        }),
    ))
}

fn unix_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn prompt_blocks_to_message_content(
    blocks: &[ContentBlock],
) -> Result<Vec<RuntimeContentBlock>, String> {
    let mut content = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                content.push(RuntimeContentBlock::text(text.text.clone()));
            }
            ContentBlock::ResourceLink(link) => {
                content.push(resource_link_to_runtime_content(link));
            }
            ContentBlock::Resource(resource) => {
                content.push(embedded_resource_to_runtime_content(resource)?);
            }
            ContentBlock::Image(_) => {
                return Err("image prompt content is not supported by this ACP server".to_string());
            }
            ContentBlock::Audio(_) => {
                return Err("audio prompt content is not supported by this ACP server".to_string());
            }
            _ => return Err("unsupported ACP prompt content block".to_string()),
        }
    }
    Ok(content)
}

fn resource_link_to_runtime_content(link: &ResourceLink) -> RuntimeContentBlock {
    let title = link.title.clone().or_else(|| Some(link.name.clone()));
    RuntimeContentBlock::document_url(link.uri.clone(), title)
}

fn embedded_resource_to_runtime_content(
    resource: &EmbeddedResource,
) -> Result<RuntimeContentBlock, String> {
    match &resource.resource {
        EmbeddedResourceResource::TextResourceContents(text) => {
            Ok(RuntimeContentBlock::text(text.text.clone()))
        }
        EmbeddedResourceResource::BlobResourceContents(blob) => {
            let media_type = blob
                .mime_type
                .clone()
                .unwrap_or_else(|| infer_media_type_from_uri(&blob.uri));
            Ok(RuntimeContentBlock::document_base64(
                media_type,
                blob.blob.clone(),
                path_title(&blob.uri),
            ))
        }
        _ => Err("unsupported embedded ACP resource".to_string()),
    }
}

fn infer_media_type_from_uri(uri: &str) -> String {
    match Path::new(uri).extension().and_then(|ext| ext.to_str()) {
        Some("png") => "image/png".to_string(),
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("gif") => "image/gif".to_string(),
        Some("pdf") => "application/pdf".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn path_title(uri: &str) -> Option<String> {
    Path::new(uri)
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::super::types::ProtocolVersion;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::protocols::acp::types as wire_types;
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{
        StopReason as RuntimeStopReason, StreamResult, TokenUsage,
    };
    use awaken_contract::contract::message::ToolCall as RuntimeToolCall;
    use awaken_contract::contract::tool::{FrontEndTool, ToolDescriptor};
    use awaken_runtime::builder::AgentRuntimeBuilder;
    use awaken_runtime::registry::traits::ModelBinding;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, split};
    use tokio::time::{Duration, timeout};

    struct StubResolver;

    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn test_runtime() -> Arc<AgentRuntime> {
        Arc::new(AgentRuntime::new(Arc::new(StubResolver)))
    }

    struct FrontendToolMockExecutor {
        call_count: AtomicUsize,
    }

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for FrontendToolMockExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let count = self.call_count.fetch_add(1, Ordering::Relaxed);
            if count == 0 {
                Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![RuntimeToolCall::new(
                        "call_frontend_1",
                        "ask_user",
                        serde_json::json!({"question": "What color?"}),
                    )],
                    usage: Some(TokenUsage::default()),
                    stop_reason: Some(RuntimeStopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                })
            } else {
                Ok(StreamResult {
                    content: vec![RuntimeContentBlock::text("unexpected follow-up turn")],
                    tool_calls: vec![],
                    usage: Some(TokenUsage::default()),
                    stop_reason: Some(RuntimeStopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }
        }

        fn name(&self) -> &str {
            "frontend-mock"
        }
    }

    fn frontend_tool_runtime() -> Arc<AgentRuntime> {
        let frontend_tool = ToolDescriptor::new("ask_user", "ask_user", "Ask the user a question");

        let builder = AgentRuntimeBuilder::new()
            .with_model_binding(
                "test-model",
                ModelBinding {
                    provider_id: "mock".into(),
                    upstream_model: "mock-model".into(),
                },
            )
            .with_provider(
                "mock",
                Arc::new(FrontendToolMockExecutor {
                    call_count: AtomicUsize::new(0),
                }),
            )
            .with_tool("ask_user", Arc::new(FrontEndTool::new(frontend_tool)))
            .with_agent_spec(awaken_contract::registry_spec::AgentSpec {
                id: "frontend".into(),
                model_id: "test-model".into(),
                system_prompt: "You delegate to a frontend tool".into(),
                max_rounds: 2,
                ..Default::default()
            });

        Arc::new(builder.build().expect("build runtime"))
    }

    async fn run_stdio_exchange(runtime: Arc<AgentRuntime>, input: &[u8]) -> String {
        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async move {
                let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
                let (mut client_reader, mut client_writer) = split(client_stream);
                let (server_reader, server_writer) = split(server_stream);

                let server_task = tokio::task::spawn_local(async move {
                    serve_stdio_io(runtime, BufReader::new(server_reader), server_writer).await;
                });

                client_writer.write_all(input).await.unwrap();
                client_writer.flush().await.unwrap();

                let mut output = Vec::new();
                let mut first_chunk = [0_u8; 4096];
                if let Ok(Ok(bytes_read)) = timeout(
                    Duration::from_millis(200),
                    client_reader.read(&mut first_chunk),
                )
                .await
                    && bytes_read > 0
                {
                    output.extend_from_slice(&first_chunk[..bytes_read]);
                }

                client_writer.shutdown().await.unwrap();
                client_reader.read_to_end(&mut output).await.unwrap();
                let _ = server_task.await;

                String::from_utf8(output).unwrap()
            })
            .await
    }

    fn parse_single_json_response(output: &str) -> serde_json::Value {
        serde_json::from_str(output.trim()).expect("stdio response should be valid JSON")
    }

    struct MultiAgentResolver;

    impl awaken_runtime::AgentResolver for MultiAgentResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }

        fn agent_ids(&self) -> Vec<String> {
            vec!["alpha".to_string(), "beta".to_string()]
        }
    }

    #[test]
    fn initialize_response_has_spec_fields() {
        let response = build_initialize_response(InitializeRequest::new(ProtocolVersion::V1));
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("protocolVersion").is_some());
        assert!(json.get("agentCapabilities").is_some());
        assert!(json.get("agentInfo").is_some());
    }

    #[test]
    fn generate_session_id_format() {
        let session_id = generate_session_id();
        assert!(session_id.starts_with("sess_"));
    }

    #[test]
    fn select_session_agent_id_uses_single_registered_agent() {
        struct SingleAgentResolver;

        impl awaken_runtime::AgentResolver for SingleAgentResolver {
            fn resolve(
                &self,
                agent_id: &str,
            ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
                Err(awaken_runtime::RuntimeError::AgentNotFound {
                    agent_id: agent_id.to_string(),
                })
            }

            fn agent_ids(&self) -> Vec<String> {
                vec!["echo".to_string()]
            }
        }

        let selected = select_session_agent_id(&SingleAgentResolver).unwrap();
        assert_eq!(selected.as_deref(), Some("echo"));
    }

    #[test]
    fn select_session_agent_id_rejects_ambiguous_registry() {
        let err = select_session_agent_id(&MultiAgentResolver).unwrap_err();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::InternalError);
    }

    #[test]
    fn prompt_blocks_to_message_content_supports_resource_link() {
        let blocks = vec![
            ContentBlock::from("hello"),
            ContentBlock::ResourceLink(ResourceLink::new("README", "file:///repo/README.md")),
        ];
        let content = prompt_blocks_to_message_content(&blocks).unwrap();
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], RuntimeContentBlock::Text { .. }));
        assert!(matches!(content[1], RuntimeContentBlock::Document { .. }));
    }

    #[test]
    fn permission_response_maps_stable_allow_option_ids() {
        let resume = permission_response_to_resume(RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("opt_allow_always"),
            )),
        ))
        .expect("allow option should map");

        assert_eq!(resume.action, ResumeDecisionAction::Resume);
        assert_eq!(
            resume.result,
            serde_json::json!({
                "kind": "permission_decision",
                "approved": true,
                "policy": "allow_always",
            })
        );
    }

    #[test]
    fn permission_response_maps_stable_reject_option_ids() {
        let resume = permission_response_to_resume(RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("opt_reject_once"),
            )),
        ))
        .expect("reject option should map");

        assert_eq!(resume.action, ResumeDecisionAction::Cancel);
        assert_eq!(
            resume.result,
            serde_json::json!({
                "kind": "permission_decision",
                "approved": false,
                "policy": "reject_once",
            })
        );
    }

    #[test]
    fn permission_response_rejects_unknown_option_ids() {
        let err = permission_response_to_resume(RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("reject_maybe"),
            )),
        ))
        .expect_err("unknown option id should fail");

        assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn serve_stdio_initialize() {
        let runtime = test_runtime();
        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":1}\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        let response = parse_single_json_response(&output_str);
        assert!(response.get("result").is_some());
        assert!(response.get("error").is_none());
    }

    #[tokio::test]
    async fn serve_stdio_session_new() {
        let runtime = test_runtime();
        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"session/new\",\"params\":{\"cwd\":\"/tmp\",\"mcpServers\":[]},\"id\":1}\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        let response = parse_single_json_response(&output_str);
        let result = &response["result"];
        assert!(result["sessionId"].as_str().unwrap().starts_with("sess_"));
    }

    #[tokio::test]
    async fn serve_stdio_session_new_rejects_relative_cwd() {
        let runtime = test_runtime();
        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"session/new\",\"params\":{\"cwd\":\"tmp\",\"mcpServers\":[]},\"id\":2}\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        let response = parse_single_json_response(&output_str);
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn serve_stdio_unknown_method() {
        let runtime = test_runtime();
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"unknown\",\"params\":{},\"id\":2}\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        let response = parse_single_json_response(&output_str);
        assert_eq!(response["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn serve_stdio_parse_error() {
        let runtime = test_runtime();
        let input = b"not json\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        assert!(output_str.trim().is_empty());
    }

    #[tokio::test]
    async fn serve_stdio_session_prompt_requires_session() {
        let runtime = test_runtime();
        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"id\":1}\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        let response = parse_single_json_response(&output_str);
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn serve_stdio_session_prompt_invalid_session() {
        let runtime = test_runtime();
        let input =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"sess_bad\",\"prompt\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"id\":1}\n";
        let output_str = run_stdio_exchange(runtime, &input[..]).await;
        let response = parse_single_json_response(&output_str);
        assert_eq!(response["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn serve_stdio_unknown_notification_silently_ignored() {
        let runtime = test_runtime();
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"_custom/something\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":1},\"id\":1}\n",
        );
        let output_str = run_stdio_exchange(runtime, input.as_bytes()).await;
        let lines: Vec<&str> = output_str.trim().lines().collect();
        assert_eq!(lines.len(), 1);
    }

    #[tokio::test]
    async fn prompt_rejects_generic_frontend_tool_suspension() {
        let runtime = frontend_tool_runtime();
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let agent = AcpAgent::new(runtime, Arc::clone(&sessions), client_tx);

        let client_task = tokio::spawn(async move {
            while let Some(command) = client_rx.recv().await {
                match command {
                    ClientCommand::SessionNotification { response_tx, .. } => {
                        let _ = response_tx.send(Ok(()));
                    }
                    ClientCommand::RequestPermission { response_tx, .. } => {
                        let _ = response_tx.send(Err(acp::Error::new(
                            -32603,
                            "generic frontend suspension must not be converted into request_permission",
                        )));
                    }
                }
            }
        });

        let session = acp::Agent::new_session(&agent, wire_types::NewSessionRequest::new("/tmp"))
            .await
            .expect("new_session should succeed");

        let err = acp::Agent::prompt(
            &agent,
            wire_types::PromptRequest::new(
                session.session_id,
                vec![wire_types::ContentBlock::from(
                    "ask the frontend user a question",
                )],
            ),
        )
        .await
        .expect_err("generic frontend tool suspension should be rejected");

        assert!(
            err.message
                .contains("only supports suspended tool action 'tool:PermissionConfirm'"),
            "unexpected ACP error: {err:?}"
        );

        client_task.abort();
        let _ = client_task.await;
    }
}
