use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{
    ContentBlock, DocumentSource, ImageSource, SearchResultContent,
};
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error, LlmExecutor, StopReason, TokenUsage,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::classify_error;

/// Dedicated OpenAI Responses API adapter. It never aliases the Responses wire
/// onto the genai OpenAI Chat Completions adapter.
pub struct OpenAiResponsesExecutor {
    client: reqwest::Client,
    url: Url,
    api_key: String,
}

impl OpenAiResponsesExecutor {
    pub fn new(base_url: &str, api_key: impl Into<String>) -> Result<Self, Error> {
        let mut url = Url::parse(base_url)
            .map_err(|error| Error::Binding(format!("invalid Responses base URL: {error}")))?;
        if !url.path().trim_end_matches('/').ends_with("responses") {
            url.set_path(&format!("{}/responses", url.path().trim_end_matches('/')));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| Error::Provider(error.to_string()))?;
        Ok(Self {
            client,
            url,
            api_key: api_key.into(),
        })
    }

    async fn execute(&self, request: &ChatRequest) -> Result<ResponsesResponse, Error> {
        let response = self
            .client
            .post(self.url.clone())
            .bearer_auth(&self.api_key)
            .json(&request_body(request)?)
            .send()
            .await
            .map_err(|error| classify_error(&error.without_url().to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| Error::Provider(error.without_url().to_string()))?;
        if !status.is_success() {
            return Err(classify_error(&format!("HTTP {status}: {text}")));
        }
        serde_json::from_str(&text)
            .map_err(|error| Error::Provider(format!("invalid Responses JSON: {error}")))
    }
}

#[async_trait]
impl LlmExecutor for OpenAiResponsesExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, Error> {
        response_from_wire(self.execute(&request).await?)
    }
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponseInputItem>,
    store: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponseTool>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseInputItem {
    Message {
        role: ResponseRole,
        content: ResponseMessageContent,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: ResponseFunctionOutput,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ResponseRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ResponseMessageContent {
    Text(String),
    Blocks(Vec<ResponseInputContent>),
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ResponseFunctionOutput {
    Text(String),
    Blocks(Vec<ResponseInputContent>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseInputContent {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image { image_url: String },
    #[serde(rename = "input_file")]
    File {
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_data: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_url: Option<String>,
    },
}

#[derive(Debug, Serialize)]
struct ResponseTool {
    #[serde(rename = "type")]
    kind: ResponseToolType,
    name: String,
    description: String,
    parameters: Value,
    strict: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ResponseToolType {
    Function,
}

fn request_body(request: &ChatRequest) -> Result<ResponsesRequest, Error> {
    let mut items = Vec::new();
    for message in &request.messages {
        let mut text = String::new();
        let mut media = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text: value } => text.push_str(value),
                ContentBlock::Image { source } => media.push(image_content(source)?),
                ContentBlock::Document { source, title, .. } => {
                    media.push(document_content(source, title.as_deref())?)
                }
                ContentBlock::SearchResult {
                    source,
                    title,
                    content,
                    ..
                } => media.push(ResponseInputContent::Text {
                    text: render_search_result(source, title, content),
                }),
                ContentBlock::Redacted | ContentBlock::Thinking { .. } => {}
                ContentBlock::ToolUse { id, name, input } => {
                    items.push(ResponseInputItem::FunctionCall {
                        call_id: id.clone(),
                        name: name.clone(),
                        arguments: serde_json::to_string(input)
                            .map_err(|error| Error::InvalidRequest(error.to_string()))?,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => items.push(ResponseInputItem::FunctionCallOutput {
                    call_id: tool_use_id.clone(),
                    output: tool_output(content, *is_error)?,
                }),
            }
        }
        if !text.is_empty() || !media.is_empty() {
            let role = match message.role {
                Role::System => ResponseRole::System,
                Role::User | Role::Tool => ResponseRole::User,
                Role::Assistant => ResponseRole::Assistant,
            };
            let content = if media.is_empty() {
                ResponseMessageContent::Text(text)
            } else {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(ResponseInputContent::Text { text });
                }
                content.extend(media);
                ResponseMessageContent::Blocks(content)
            };
            items.push(ResponseInputItem::Message { role, content });
        }
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| ResponseTool {
            kind: ResponseToolType::Function,
            name: tool.id.clone(),
            description: tool.description.clone(),
            parameters: tool.model_parameters(),
            strict: false,
        })
        .collect();
    Ok(ResponsesRequest {
        model: request.model_binding.model_ref.clone(),
        input: items,
        store: false,
        tools,
    })
}

fn image_content(source: &ImageSource) -> Result<ResponseInputContent, Error> {
    let image_url = match source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
        ImageSource::File { file_id } => {
            return Err(unmaterialized_file(file_id));
        }
    };
    Ok(ResponseInputContent::Image { image_url })
}

fn document_content(
    source: &DocumentSource,
    title: Option<&str>,
) -> Result<ResponseInputContent, Error> {
    let default_filename = || title.unwrap_or("document").to_owned();
    match source {
        DocumentSource::Base64 { media_type, data } => Ok(ResponseInputContent::File {
            filename: Some(default_filename()),
            file_data: Some(format!("data:{media_type};base64,{data}")),
            file_url: None,
        }),
        DocumentSource::Text { media_type, data } => {
            use base64::Engine as _;
            Ok(ResponseInputContent::File {
                filename: Some(title.unwrap_or("document.txt").to_owned()),
                file_data: Some(format!(
                    "data:{media_type};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(data)
                )),
                file_url: None,
            })
        }
        DocumentSource::Url { url } => Ok(ResponseInputContent::File {
            filename: None,
            file_data: None,
            file_url: Some(url.clone()),
        }),
        DocumentSource::File { file_id } => Err(unmaterialized_file(file_id)),
    }
}

fn tool_output(blocks: &[ContentBlock], is_error: bool) -> Result<ResponseFunctionOutput, Error> {
    let mut output = Vec::new();
    if is_error {
        output.push(ResponseInputContent::Text {
            text: "Tool execution failed.".into(),
        });
    }
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                output.push(ResponseInputContent::Text { text: text.clone() })
            }
            ContentBlock::Image { source } => output.push(image_content(source)?),
            ContentBlock::Document { source, title, .. } => {
                output.push(document_content(source, title.as_deref())?);
            }
            ContentBlock::SearchResult {
                source,
                title,
                content,
                ..
            } => output.push(ResponseInputContent::Text {
                text: render_search_result(source, title, content),
            }),
            ContentBlock::Redacted | ContentBlock::Thinking { .. } => {}
            ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {
                return Err(Error::InvalidRequest(
                    "Responses tool output cannot contain nested tool protocol blocks".into(),
                ));
            }
        }
    }
    if output.len() == 1
        && let ResponseInputContent::Text { text } = output.remove(0)
    {
        Ok(ResponseFunctionOutput::Text(text))
    } else {
        Ok(ResponseFunctionOutput::Blocks(output))
    }
}

fn render_search_result(source: &str, title: &str, content: &[SearchResultContent]) -> String {
    format!(
        "{title}\nSource: {source}\n{}",
        content
            .iter()
            .map(|part| part.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn unmaterialized_file(file_id: &str) -> Error {
    Error::InvalidRequest(format!(
        "File {file_id} was not materialized before Responses dispatch"
    ))
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    status: Option<String>,
    output: Vec<ResponseOutputItem>,
    usage: Option<ResponseUsage>,
    incomplete_details: Option<ResponseIncompleteDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseOutputItem {
    Message {
        #[serde(default)]
        content: Vec<ResponseOutputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseOutputContent {
    OutputText {
        text: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    input_tokens_details: Option<ResponseInputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponseInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ResponseIncompleteDetails {
    reason: Option<String>,
}

#[cfg(test)]
fn response_from_value(value: Value) -> Result<ChatResponse, Error> {
    let wire: ResponsesResponse = serde_json::from_value(value)
        .map_err(|error| Error::Provider(format!("invalid Responses JSON: {error}")))?;
    response_from_wire(wire)
}

fn response_from_wire(wire: ResponsesResponse) -> Result<ChatResponse, Error> {
    let mut blocks = Vec::new();
    for item in wire.output {
        match item {
            ResponseOutputItem::Message { content } => {
                for content in content {
                    if let ResponseOutputContent::OutputText { text } = content {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let arguments = serde_json::from_str(&arguments).map_err(|error| {
                    Error::Provider(format!("function_call arguments are invalid JSON: {error}"))
                })?;
                blocks.push(ContentBlock::tool_use(call_id, name, arguments));
            }
            ResponseOutputItem::Unsupported => {}
        }
    }
    let usage = wire.usage.map(|usage| TokenUsage {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        cache_read_tokens: usage
            .input_tokens_details
            .map_or(0, |details| details.cached_tokens),
        cache_creation_tokens: 0,
    });
    let stop_reason = if blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    {
        Some(StopReason::ToolUse)
    } else if wire.status.as_deref() == Some("incomplete") {
        match wire
            .incomplete_details
            .and_then(|details| details.reason)
            .as_deref()
        {
            Some("max_output_tokens") => Some(StopReason::MaxTokens),
            Some("content_filter") => Some(StopReason::ContentFilter),
            _ => None,
        }
    } else {
        Some(StopReason::EndTurn)
    };
    Ok(ChatResponse {
        output: AssistantOutput::from_blocks(blocks),
        usage,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{ChatMessage, LlmExecutor};
    use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model_binding: ModelBinding::new("openai", "gpt-exact", "genai"),
            inference: Default::default(),
            messages,
            tools: Vec::new(),
        }
    }

    #[test]
    fn request_maps_multimodal_messages_tools_and_tool_results() {
        // Cause/effect graph and FMECA for Responses request projection:
        // C1=text/image/document input -> E1 typed input_* block; C2=tool result
        // text/image/document -> E2 typed function_call_output content; C3=search
        // result -> E3 deterministic citation-bearing text fallback; C4=logical
        // Awaken File -> E4 fail before HTTP. FMECA: passing an Awaken id as an
        // OpenAI id is cross-authority disclosure (critical, C4 gate); flattening
        // document/image tool output loses semantics (high, closed DTO list);
        // fabricating OpenAI search result wire is protocol-invalid (high, E3).
        // Decision rule O1=C1+C2+C3; O2=C4 is covered by the next test.
        let mut request = request(vec![
            ChatMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::text("look"),
                    ContentBlock::image_url("https://example.test/image.png"),
                ],
            },
            ChatMessage {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call_1",
                    "weather",
                    json!({"city":"Paris"}),
                )],
            },
            ChatMessage {
                role: Role::Tool,
                content: vec![ContentBlock::tool_result(
                    "call_1",
                    vec![
                        ContentBlock::text("sunny"),
                        ContentBlock::Document {
                            source: DocumentSource::Base64 {
                                media_type: "application/pdf".into(),
                                data: "cGRm".into(),
                            },
                            title: Some("weather.pdf".into()),
                            context: None,
                        },
                        ContentBlock::SearchResult {
                            source: "https://example.test/weather".into(),
                            title: "Weather".into(),
                            content: vec![SearchResultContent::text("clear")],
                            citations:
                                awaken_agent_contract::agent::content::SearchResultCitations {
                                    enabled: true,
                                },
                        },
                    ],
                )],
            },
        ]);
        request.tools.push(ToolDescriptor::pinned(
            "test",
            "weather",
            "Get weather",
            json!({"type":"object"}),
        ));
        let body = serde_json::to_value(request_body(&request).unwrap()).unwrap();
        assert_eq!(body["model"], "gpt-exact");
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][2]["output"][1]["type"], "input_file");
        assert!(
            body["input"][2]["output"][2]["text"]
                .as_str()
                .unwrap()
                .contains("https://example.test/weather"),
            "O1/E3"
        );
        assert_eq!(body["tools"][0]["name"], "weather");
        // Cause/effect rule O1: a legacy zero-argument object descriptor is
        // projected through the same canonical ToolDescriptor schema owner.
        assert_eq!(body["tools"][0]["parameters"]["properties"], json!({}));
    }

    #[test]
    fn request_rejects_unmaterialized_awaken_file_ids() {
        let error = request_body(&request(vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::document_file("file-awaken")],
        }]))
        .expect_err("O2 logical File must not cross the provider boundary");
        assert!(
            error.to_string().contains("not materialized"),
            "O2: {error}"
        );
    }

    /// Response decision table and FMECA: R1=completed text -> EndTurn; R2=valid
    /// function call -> typed ToolUse; R3=incomplete max tokens -> MaxTokens;
    /// R4=content filter -> ContentFilter; R5=missing required output or invalid
    /// function JSON -> provider error. Silent success on R5 is critical; typed
    /// deserialization and fail-closed argument parsing mitigate it. This test
    /// owns R1-R3; the following two tests own R4-R5.
    #[test]
    fn response_folds_text_tools_usage_and_incomplete_reason() {
        let response = response_from_value(json!({
            "status":"incomplete", "incomplete_details":{"reason":"max_output_tokens"},
            "output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}],
            "usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}
        }))
        .unwrap();
        assert_eq!(response.output.text_content(), "hello");
        assert_eq!(response.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(response.usage.unwrap().cache_read_tokens, 4);

        let response = response_from_value(json!({
            "status":"completed",
            "output":[{"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Paris\"}"}]
        }))
        .unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(response.output.tool_calls()[0].tool_id, "weather");
    }

    #[test]
    fn response_stop_reasons_cover_completed_and_content_filter() {
        let completed = response_from_value(json!({
            "status":"completed",
            "output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]
        }))
        .unwrap();
        assert_eq!(completed.stop_reason, Some(StopReason::EndTurn));

        let filtered = response_from_value(json!({
            "status":"incomplete", "incomplete_details":{"reason":"content_filter"},
            "output":[]
        }))
        .unwrap();
        assert_eq!(filtered.stop_reason, Some(StopReason::ContentFilter));
    }

    #[test]
    fn malformed_response_payloads_fail_closed() {
        let missing_output = response_from_value(json!({"status":"completed"})).unwrap_err();
        assert_eq!(missing_output.code(), "provider_error");

        let malformed_arguments = response_from_value(json!({
            "status":"completed",
            "output":[{"type":"function_call","call_id":"call_1","name":"weather","arguments":"{"}]
        }))
        .unwrap_err();
        assert_eq!(malformed_arguments.code(), "provider_error");
    }

    #[tokio::test]
    async fn executor_posts_the_responses_path_with_bearer_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&bytes);
                let Some(head_end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let length = text[..head_end]
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= head_end + 4 + length {
                    break;
                }
            }
            let captured = String::from_utf8_lossy(&bytes).into_owned();
            let body = r#"{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":2,"output_tokens":1}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            captured
        });

        let executor =
            OpenAiResponsesExecutor::new(&format!("http://{address}/v1"), "private-key").unwrap();
        let result = executor
            .infer(request(vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("hello")],
            }]))
            .await
            .unwrap();
        assert_eq!(result.output.text_content(), "ok");
        let captured = server.await.unwrap();
        assert!(
            captured.starts_with("POST /v1/responses HTTP/1.1"),
            "{captured}"
        );
        assert!(
            captured
                .to_ascii_lowercase()
                .contains("authorization: bearer private-key")
        );
        assert!(captured.contains("\"model\":\"gpt-exact\""));
        assert!(captured.contains("\"store\":false"));
    }

    #[tokio::test]
    async fn executor_classifies_non_success_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            let body = r#"{"error":{"message":"bad key"}}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let executor =
            OpenAiResponsesExecutor::new(&format!("http://{address}/v1"), "bad-key").unwrap();
        let error = executor
            .infer(request(vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("hello")],
            }]))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "unauthorized");
        server.await.unwrap();
    }
}
