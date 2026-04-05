//! ACP stdio server backed by the official `agent-client-protocol` Rust SDK.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client as _};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use awaken_contract::contract::content::{ContentBlock as RuntimeContentBlock, ImageSource};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_contract::contract::tool::Tool;
use awaken_ext_mcp::{McpServerConnectionConfig, McpToolRegistryManager};
use awaken_runtime::{AgentResolver, AgentRuntime, ResolvedAgent, RuntimeError};

use super::encoder::{AcpEncoder, AcpOutput};
use super::types::{
    AgentCapabilities, AudioContent, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, Implementation, InitializeRequest, InitializeResponse, McpCapabilities,
    McpServer, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
    PromptResponse, RequestPermissionResponse, ResourceLink, SessionConfigOption,
    SessionConfigSelectOption, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
};

const AGENT_CONFIG_ID: &str = "agent";

struct SessionState {
    #[allow(dead_code)]
    cwd: String,
    runtime: Arc<AgentRuntime>,
    agent_id: Option<String>,
    available_agent_ids: Vec<String>,
    thread_id: String,
}

type Sessions = Arc<Mutex<HashMap<String, SessionState>>>;

#[derive(Clone)]
struct ToolAugmentingResolver {
    inner: Arc<dyn AgentResolver>,
    extra_tools: Vec<Arc<dyn Tool>>,
}

impl ToolAugmentingResolver {
    fn new(inner: Arc<dyn AgentResolver>, extra_tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { inner, extra_tools }
    }
}

impl AgentResolver for ToolAugmentingResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
        let agent = self.inner.resolve(agent_id)?;
        Ok(agent.with_tools(self.extra_tools.clone()))
    }

    fn agent_ids(&self) -> Vec<String> {
        self.inner.agent_ids()
    }
}

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

        let session_id = generate_session_id();
        let thread_id = uuid::Uuid::now_v7().to_string();
        let runtime = build_session_runtime(&self.runtime, &args.mcp_servers).await?;
        let (agent_id, available_agent_ids) = select_session_agent_id(runtime.resolver());
        let config_options =
            build_session_config_options(&available_agent_ids, agent_id.as_deref());

        self.sessions.lock().await.insert(
            session_id.clone(),
            SessionState {
                cwd: args.cwd.to_string_lossy().into_owned(),
                runtime,
                agent_id,
                available_agent_ids,
                thread_id,
            },
        );

        Ok(NewSessionResponse::new(session_id).config_options(config_options))
    }

    async fn set_session_config_option(
        &self,
        args: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(args.session_id.0.as_ref()) else {
            return Err(acp::Error::new(
                -32002,
                format!("session not found: {}", args.session_id.0),
            ));
        };

        if args.config_id.0.as_ref() != AGENT_CONFIG_ID {
            return Err(acp::Error::new(
                -32602,
                format!("unknown session config option: {}", args.config_id.0),
            ));
        }

        let selected_agent_id = args.value.0.as_ref();
        if !session
            .available_agent_ids
            .iter()
            .any(|agent_id| agent_id == selected_agent_id)
        {
            return Err(acp::Error::new(
                -32602,
                format!("unknown agent: {selected_agent_id}"),
            ));
        }

        session.agent_id = Some(selected_agent_id.to_string());
        Ok(SetSessionConfigOptionResponse::new(
            build_session_config_options(&session.available_agent_ids, session.agent_id.as_deref())
                .unwrap_or_default(),
        ))
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

        let (runtime, agent_id, thread_id) = {
            let guard = self.sessions.lock().await;
            match guard.get(&session_id) {
                Some(state) => (
                    Arc::clone(&state.runtime),
                    state.agent_id.clone(),
                    state.thread_id.clone(),
                ),
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
        let run_runtime = Arc::clone(&runtime);
        let run_handle =
            tokio::spawn(async move { run_runtime.run(run_request, Arc::new(sink)).await });

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
                        let resume = permission_response_to_resume(response);
                        if !runtime.send_decisions(&thread_id, vec![(tool_call_id, resume)]) {
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
                    }
                }
            }
        }

        match run_handle.await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => return Err(acp::Error::into_internal_error(err)),
            Err(err) => return Err(acp::Error::into_internal_error(err)),
        }

        if let Some(err) = prompt_error {
            return Err(err);
        }

        Ok(PromptResponse::new(final_stop_reason))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        let session_runtime = {
            let guard = self.sessions.lock().await;
            guard
                .get(args.session_id.0.as_ref())
                .map(|state| (Arc::clone(&state.runtime), state.thread_id.clone()))
        };
        if let Some((runtime, thread_id)) = session_runtime {
            runtime.cancel(&thread_id);
        }
        Ok(())
    }
}

fn build_initialize_response(request: InitializeRequest) -> InitializeResponse {
    let capabilities = AgentCapabilities::new()
        .prompt_capabilities(
            PromptCapabilities::new()
                .image(true)
                .audio(true)
                .embedded_context(true),
        )
        .mcp_capabilities(McpCapabilities::new().http(true));
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
) -> (Option<String>, Vec<String>) {
    let mut agent_ids = resolver.agent_ids();
    agent_ids.sort();
    agent_ids.dedup();

    let selected = if agent_ids.iter().any(|agent_id| agent_id == "default") {
        Some("default".to_string())
    } else {
        agent_ids.first().cloned()
    };

    (selected, agent_ids)
}

fn build_session_config_options(
    available_agent_ids: &[String],
    current_agent_id: Option<&str>,
) -> Option<Vec<SessionConfigOption>> {
    if available_agent_ids.len() <= 1 {
        return None;
    }

    let current_agent_id = current_agent_id?;
    let options = available_agent_ids
        .iter()
        .map(|agent_id| SessionConfigSelectOption::new(agent_id.clone(), agent_id.clone()))
        .collect::<Vec<_>>();
    let current_agent_id = current_agent_id.to_string();

    Some(vec![
        SessionConfigOption::select(AGENT_CONFIG_ID, "Agent", current_agent_id, options)
            .description("Target agent for this session"),
    ])
}

async fn build_session_runtime(
    base_runtime: &Arc<AgentRuntime>,
    mcp_servers: &[McpServer],
) -> acp::Result<Arc<AgentRuntime>> {
    if mcp_servers.is_empty() {
        return Ok(Arc::clone(base_runtime));
    }

    let configs = mcp_servers
        .iter()
        .map(acp_mcp_server_to_connection_config)
        .collect::<acp::Result<Vec<_>>>()?;
    let manager = McpToolRegistryManager::connect(configs)
        .await
        .map_err(|err| acp::Error::new(-32603, format!("failed to connect MCP servers: {err}")))?;
    let extra_tools = manager
        .registry()
        .snapshot()
        .into_values()
        .collect::<Vec<_>>();
    let resolver = Arc::new(ToolAugmentingResolver::new(
        base_runtime.resolver_arc(),
        extra_tools,
    ));

    Ok(Arc::new(base_runtime.clone_with_resolver(resolver)))
}

fn acp_mcp_server_to_connection_config(
    server: &McpServer,
) -> acp::Result<McpServerConnectionConfig> {
    match server {
        McpServer::Stdio(config) => {
            let command = config.command.to_string_lossy().into_owned();
            let mut cfg =
                McpServerConnectionConfig::stdio(config.name.clone(), command, config.args.clone());
            for env in &config.env {
                cfg = cfg.with_env(env.name.clone(), env.value.clone());
            }
            Ok(cfg)
        }
        McpServer::Http(config) => {
            if !config.headers.is_empty() {
                return Err(acp::Error::new(
                    -32602,
                    format!(
                        "HTTP MCP server '{}' uses headers, which are not supported by the current MCP transport",
                        config.name
                    ),
                ));
            }
            Ok(McpServerConnectionConfig::http(
                config.name.clone(),
                config.url.clone(),
            ))
        }
        McpServer::Sse(config) => Err(acp::Error::new(
            -32602,
            format!(
                "SSE MCP server '{}' is not supported by the current MCP transport",
                config.name
            ),
        )),
        _ => Err(acp::Error::new(
            -32602,
            "unsupported MCP server configuration",
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

fn permission_response_to_resume(response: RequestPermissionResponse) -> ToolCallResume {
    let action = match &response.outcome {
        acp::RequestPermissionOutcome::Cancelled => ResumeDecisionAction::Cancel,
        acp::RequestPermissionOutcome::Selected(selected) => {
            if selected.option_id.0.contains("reject") {
                ResumeDecisionAction::Cancel
            } else {
                ResumeDecisionAction::Resume
            }
        }
        _ => ResumeDecisionAction::Cancel,
    };

    ToolCallResume {
        decision_id: uuid::Uuid::now_v7().to_string(),
        action,
        result: serde_json::to_value(&response).unwrap_or(Value::Null),
        reason: None,
        updated_at: unix_timestamp_millis(),
    }
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
            ContentBlock::Image(image) => {
                content.push(image_content_to_runtime_content(image));
            }
            ContentBlock::Audio(audio) => {
                content.push(audio_content_to_runtime_content(audio));
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

fn image_content_to_runtime_content(image: &ImageContent) -> RuntimeContentBlock {
    if image.data.is_empty()
        && let Some(uri) = &image.uri
    {
        return RuntimeContentBlock::Image {
            source: ImageSource::Url { url: uri.clone() },
        };
    }

    RuntimeContentBlock::image_base64(image.mime_type.clone(), image.data.clone())
}

fn audio_content_to_runtime_content(audio: &AudioContent) -> RuntimeContentBlock {
    RuntimeContentBlock::audio_base64(audio.mime_type.clone(), audio.data.clone())
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
    use serde_json::json;
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
        assert_eq!(
            json["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        assert_eq!(
            json["agentCapabilities"]["promptCapabilities"]["audio"],
            true
        );
        assert_eq!(
            json["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
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

        let (selected, available) = select_session_agent_id(&SingleAgentResolver);
        assert_eq!(selected.as_deref(), Some("echo"));
        assert_eq!(available, vec!["echo"]);
    }

    #[test]
    fn select_session_agent_id_prefers_default_then_sorted_first() {
        let (selected, available) = select_session_agent_id(&MultiAgentResolver);
        assert_eq!(selected.as_deref(), Some("alpha"));
        assert_eq!(available, vec!["alpha", "beta"]);
    }

    #[test]
    fn build_session_config_options_emits_agent_selector_for_multi_agent() {
        let options = build_session_config_options(&["alpha".into(), "beta".into()], Some("beta"))
            .expect("config options should exist");
        let json = serde_json::to_value(&options).unwrap();
        assert_eq!(json[0]["id"], AGENT_CONFIG_ID);
        assert_eq!(json[0]["type"], "select");
        assert_eq!(json[0]["currentValue"], "beta");
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
    fn prompt_blocks_to_message_content_supports_image_and_audio() {
        let blocks = vec![
            ContentBlock::Image(ImageContent::new("aGVsbG8=", "image/png")),
            ContentBlock::Audio(AudioContent::new("d29ybGQ=", "audio/mpeg")),
        ];
        let content = prompt_blocks_to_message_content(&blocks).unwrap();
        assert!(matches!(content[0], RuntimeContentBlock::Image { .. }));
        assert!(matches!(content[1], RuntimeContentBlock::Audio { .. }));
    }

    #[test]
    fn acp_mcp_server_to_connection_config_supports_stdio_and_http() {
        let stdio_server: McpServer = serde_json::from_value(json!({
            "name": "local",
            "command": "node",
            "args": ["server.js"],
            "env": [{"name": "FOO", "value": "bar"}]
        }))
        .unwrap();
        let stdio = acp_mcp_server_to_connection_config(&stdio_server).unwrap();
        assert_eq!(stdio.name, "local");
        assert_eq!(stdio.transport.to_string(), "stdio");
        assert_eq!(stdio.command.as_deref(), Some("node"));
        assert_eq!(stdio.args, vec!["server.js"]);
        assert_eq!(stdio.env.get("FOO").map(String::as_str), Some("bar"));

        let http_server: McpServer = serde_json::from_value(json!({
            "type": "http",
            "name": "remote",
            "url": "https://example.com/mcp",
            "headers": []
        }))
        .unwrap();
        let http = acp_mcp_server_to_connection_config(&http_server).unwrap();
        assert_eq!(http.name, "remote");
        assert_eq!(http.transport.to_string(), "http");
        assert_eq!(http.url.as_deref(), Some("https://example.com/mcp"));
    }

    #[test]
    fn acp_mcp_server_to_connection_config_rejects_unsupported_variants() {
        let http_with_headers: McpServer = serde_json::from_value(json!({
            "type": "http",
            "name": "remote",
            "url": "https://example.com/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer token"}]
        }))
        .unwrap();
        let err = acp_mcp_server_to_connection_config(&http_with_headers).unwrap_err();
        assert!(err.message.contains("headers"));

        let sse_server: McpServer = serde_json::from_value(json!({
            "type": "sse",
            "name": "events",
            "url": "https://example.com/sse",
            "headers": []
        }))
        .unwrap();
        let err = acp_mcp_server_to_connection_config(&sse_server).unwrap_err();
        assert!(err.message.contains("SSE"));
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
}
