//! A scripted LLM executor that returns predetermined tool calls based on message content.
//! Used for deterministic E2E testing of tool execution flow.

use async_trait::async_trait;
use awaken_contract::contract::content::{ContentBlock, extract_text};
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::{Role, ToolCall};
use awaken_ext_deferred_tools::TOOL_SEARCH_ID;
use serde_json::json;

/// An LLM executor that inspects the last user message for directive keywords
/// and returns deterministic tool calls or text responses accordingly.
pub struct ScriptedLlmExecutor;

impl ScriptedLlmExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ScriptedLlmExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// A scripted directive: if the user message contains `keyword`, emit a tool call
/// with the given id, name, and arguments.
struct Directive {
    keyword: &'static str,
    call_id: &'static str,
    tool_name: &'static str,
}

const DIRECTIVES: &[Directive] = &[
    Directive {
        keyword: "RUN_WEATHER_TOOL",
        call_id: "call_weather_1",
        tool_name: "get_weather",
    },
    Directive {
        keyword: "RUN_STOCK_TOOL",
        call_id: "call_stock_1",
        tool_name: "get_stock_price",
    },
    Directive {
        keyword: "RUN_TOOL_SEARCH_WEATHER",
        call_id: "call_tool_search_weather_1",
        tool_name: TOOL_SEARCH_ID,
    },
];

fn directive_args(keyword: &str) -> serde_json::Value {
    match keyword {
        "RUN_WEATHER_TOOL" => json!({"location": "Tokyo"}),
        "RUN_STOCK_TOOL" => json!({"symbol": "AAPL"}),
        "RUN_TOOL_SEARCH_WEATHER" => json!({
            "query": "select:get_weather",
            "max_results": 5
        }),
        _ => json!({}),
    }
}

fn usage_stub(prompt: i32, completion: i32) -> Option<TokenUsage> {
    Some(TokenUsage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        total_tokens: Some(prompt + completion),
        ..Default::default()
    })
}

fn scripted_openui_response(user_text: &str) -> String {
    let title = if user_text.to_ascii_lowercase().contains("weather") {
        "Beijing weather card"
    } else {
        "Awaken assistant"
    };

    format!(
        r#"root = Card([title, summary, actions])
title = TextContent("{title}", "large-heavy")
summary = Callout("info", "Ready", "I can answer questions, create structured UI, and call backend tools in deterministic starter mode.")
actions = Buttons([primary], "row")
primary = Button("Continue", {{ type: "continue_conversation" }}, "primary")"#
    )
}

#[async_trait]
impl LlmExecutor for ScriptedLlmExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let user_text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.text())
            .unwrap_or_default();
        let mut system_text = extract_text(&request.system);
        for message in request.messages.iter().filter(|m| m.role == Role::System) {
            system_text.push_str(&message.text());
        }

        if system_text.contains("OpenUI Lang") {
            return Ok(StreamResult {
                content: vec![ContentBlock::text(scripted_openui_response(&user_text))],
                tool_calls: vec![],
                usage: usage_stub(10, 20),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            });
        }

        // A2UI directive (complex args, handled separately)
        if user_text.contains("RUN_A2UI_TOOL") {
            return Ok(StreamResult {
                content: vec![],
                tool_calls: vec![ToolCall::new(
                    "call_a2ui_1",
                    "render_a2ui",
                    json!({
                        "messages": [
                            {
                                "surfaceUpdate": {
                                    "surfaceId": "demo",
                                    "components": [
                                        {
                                            "id": "root",
                                            "component": {
                                                "Card": {
                                                    "child": "text1"
                                                }
                                            }
                                        },
                                        {
                                            "id": "text1",
                                            "component": {
                                                "Text": {
                                                    "text": {
                                                        "literalString": "Hello A2UI"
                                                    }
                                                }
                                            }
                                        }
                                    ]
                                }
                            },
                            {
                                "dataModelUpdate": {
                                    "surfaceId": "demo",
                                    "path": "/",
                                    "contents": [
                                        {
                                            "key": "greeting",
                                            "valueString": "Hello A2UI"
                                        }
                                    ]
                                }
                            },
                            {
                                "beginRendering": {
                                    "surfaceId": "demo",
                                    "root": "root"
                                }
                            }
                        ]
                    }),
                )],
                usage: usage_stub(10, 15),
                stop_reason: Some(StopReason::ToolUse),
                has_incomplete_tool_calls: false,
            });
        }

        // Check each directive keyword
        for d in DIRECTIVES {
            if user_text.contains(d.keyword) {
                return Ok(StreamResult {
                    content: vec![],
                    tool_calls: vec![ToolCall::new(
                        d.call_id,
                        d.tool_name,
                        directive_args(d.keyword),
                    )],
                    usage: usage_stub(10, 5),
                    stop_reason: Some(StopReason::ToolUse),
                    has_incomplete_tool_calls: false,
                });
            }
        }

        // Default: return text response
        let truncated = if user_text.len() > 50 {
            &user_text[..50]
        } else {
            &user_text
        };
        Ok(StreamResult {
            content: vec![ContentBlock::text(format!(
                "Scripted response to: {truncated}"
            ))],
            tool_calls: vec![],
            usage: usage_stub(10, 20),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::message::Message;

    fn make_request(text: &str) -> InferenceRequest {
        InferenceRequest {
            upstream_model: "scripted".into(),
            messages: vec![Message::user(text)],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    fn make_request_with_system(text: &str, system: &str) -> InferenceRequest {
        InferenceRequest {
            system: vec![ContentBlock::text(system)],
            ..make_request(text)
        }
    }

    fn make_request_with_system_message(text: &str, system: &str) -> InferenceRequest {
        InferenceRequest {
            messages: vec![Message::system(system), Message::user(text)],
            ..make_request(text)
        }
    }

    #[tokio::test]
    async fn weather_directive_returns_tool_call() {
        let executor = ScriptedLlmExecutor::new();
        let result = executor
            .execute(make_request("RUN_WEATHER_TOOL"))
            .await
            .unwrap();
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "get_weather");
        assert_eq!(result.tool_calls[0].arguments["location"], "Tokyo");
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn stock_directive_returns_tool_call() {
        let executor = ScriptedLlmExecutor::new();
        let result = executor
            .execute(make_request("RUN_STOCK_TOOL"))
            .await
            .unwrap();
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls[0].name, "get_stock_price");
        assert_eq!(result.tool_calls[0].arguments["symbol"], "AAPL");
    }

    #[tokio::test]
    async fn tool_search_weather_directive_returns_tool_call() {
        let executor = ScriptedLlmExecutor::new();
        let result = executor
            .execute(make_request("RUN_TOOL_SEARCH_WEATHER"))
            .await
            .unwrap();
        assert!(result.needs_tools());
        assert_eq!(result.tool_calls[0].name, TOOL_SEARCH_ID);
        assert_eq!(
            result.tool_calls[0].arguments["query"],
            "select:get_weather"
        );
    }

    #[tokio::test]
    async fn plain_message_returns_text() {
        let executor = ScriptedLlmExecutor::new();
        let result = executor.execute(make_request("Hello there")).await.unwrap();
        assert!(!result.needs_tools());
        assert!(result.text().contains("Scripted response to:"));
        assert!(result.text().contains("Hello there"));
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn openui_system_prompt_returns_openui_lang() {
        let executor = ScriptedLlmExecutor::new();
        let result = executor
            .execute(make_request_with_system(
                "Show me a weather card",
                "Use the OpenUI Lang statement form.",
            ))
            .await
            .unwrap();

        assert!(!result.needs_tools());
        let text = result.text();
        assert!(text.contains("root = Card"));
        assert!(text.contains("Beijing weather card"));
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn openui_system_message_returns_openui_lang() {
        let executor = ScriptedLlmExecutor::new();
        let result = executor
            .execute(make_request_with_system_message(
                "Show me a weather card",
                "Use the OpenUI Lang statement form.",
            ))
            .await
            .unwrap();

        assert!(result.text().contains("root = Card"));
        assert!(result.text().contains("Beijing weather card"));
    }

    #[tokio::test]
    async fn long_message_is_truncated_in_response() {
        let executor = ScriptedLlmExecutor::new();
        let long_msg = "A".repeat(100);
        let result = executor.execute(make_request(&long_msg)).await.unwrap();
        let text = result.text();
        // The echoed portion should be truncated to 50 chars
        assert!(text.len() < 100);
    }

    #[test]
    fn name_returns_scripted() {
        let executor = ScriptedLlmExecutor::new();
        assert_eq!(executor.name(), "scripted");
    }

    #[test]
    fn default_trait_works() {
        let executor = ScriptedLlmExecutor;
        assert_eq!(executor.name(), "scripted");
    }
}
