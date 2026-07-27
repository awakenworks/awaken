use std::time::Duration;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, ImageSource, extract_text};
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error, LlmExecutor, StopReason, TokenUsage,
};
use reqwest::Url;
use serde_json::{Value, json};

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

    async fn execute(&self, request: &ChatRequest) -> Result<Value, Error> {
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
        response_from_value(self.execute(&request).await?)
    }
}

fn request_body(request: &ChatRequest) -> Result<Value, Error> {
    let mut items = Vec::new();
    for message in &request.messages {
        let mut text = String::new();
        let mut media = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text: value } => text.push_str(value),
                ContentBlock::Image { source } => media.push(match source {
                    ImageSource::Url { url } => json!({"type":"input_image", "image_url":url}),
                    ImageSource::Base64 { media_type, data } => json!({
                        "type":"input_image",
                        "image_url":format!("data:{media_type};base64,{data}")
                    }),
                }),
                ContentBlock::ToolUse { id, name, input } => items.push(json!({
                    "type":"function_call", "call_id":id, "name":name,
                    "arguments":serde_json::to_string(input)
                        .map_err(|error| Error::InvalidRequest(error.to_string()))?
                })),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => items.push(json!({
                    "type":"function_call_output", "call_id":tool_use_id,
                    "output":extract_text(content)
                })),
                ContentBlock::Thinking { .. } => {}
            }
        }
        if !text.is_empty() || !media.is_empty() {
            let role = match message.role {
                Role::System => "system",
                Role::User | Role::Tool => "user",
                Role::Assistant => "assistant",
            };
            let content = if media.is_empty() {
                Value::String(text)
            } else {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(json!({"type":"input_text", "text":text}));
                }
                content.extend(media);
                Value::Array(content)
            };
            items.push(json!({"type":"message", "role":role, "content":content}));
        }
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type":"function", "name":tool.id, "description":tool.description,
                "parameters":tool.parameters, "strict":false
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model":request.model_binding.model_ref,
        "input":items,
        "store":false
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    Ok(body)
}

fn response_from_value(value: Value) -> Result<ChatResponse, Error> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Provider("Responses payload has no output array".into()))?;
    let mut blocks = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if content.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(text) = content.get("text").and_then(Value::as_str)
                    {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            Some("function_call") => {
                let call_id = required_str(item, "call_id")?;
                let name = required_str(item, "name")?;
                let arguments =
                    serde_json::from_str(required_str(item, "arguments")?).map_err(|error| {
                        Error::Provider(format!(
                            "function_call arguments are invalid JSON: {error}"
                        ))
                    })?;
                blocks.push(ContentBlock::tool_use(call_id, name, arguments));
            }
            _ => {}
        }
    }
    let usage = value
        .get("usage")
        .and_then(Value::as_object)
        .map(|usage| TokenUsage {
            prompt_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            completion_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read_tokens: usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_creation_tokens: 0,
        });
    let stop_reason = if blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    {
        Some(StopReason::ToolUse)
    } else if value.get("status").and_then(Value::as_str) == Some("incomplete") {
        match value
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
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

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Provider(format!("Responses item has no {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{ChatMessage, LlmExecutor};
    use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
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
                    vec![ContentBlock::text("sunny")],
                )],
            },
        ]);
        request.tools.push(ToolDescriptor {
            id: "weather".into(),
            description: "Get weather".into(),
            parameters: json!({"type":"object"}),
            content_hash: "hash".into(),
            kind: Default::default(),
            recovery_policy: Default::default(),
        });
        let body = request_body(&request).unwrap();
        assert_eq!(body["model"], "gpt-exact");
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "weather");
    }

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
