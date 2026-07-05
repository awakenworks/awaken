//! The deterministic model zoo: network-free `LlmExecutor`s that drive the
//! e2e scenarios (echo/probe/tool/statemachine/memory/…). Each model scripts
//! exactly the behavior its scenario asserts, so the server runs end-to-end in
//! CI without an API key. Split from `lib.rs` (which keeps the routers and
//! composition roots).

use awaken_agent_contract::agent::content::{ContentBlock, ImageSource};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};

use crate::config::block_text;

/// A deterministic, network-free model: it replies with the last user turn's
/// text, so the server runs end-to-end in CI and under the TypeScript SDK e2e
/// without an API key. Swap in a provider executor for real capability.
pub struct EchoModel;

#[async_trait::async_trait]
impl LlmExecutor for EchoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model tagged with a label, so an e2e can observe which model
/// (executor) a session/turn resolved to (R1/R2/R5). Replies `model=<label>`.
pub struct LabelModel(pub &'static str);

#[async_trait::async_trait]
impl LlmExecutor for LabelModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("model={}: {user}", self.0)),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic memory-probe model. On the extractor sub-run (its system
/// prompt is `awaken-ext-memory`'s extraction instructions) it saves one fixed
/// memory via the `write_memory` tool; on a main turn it prefixes its echo with
/// every system/context line it received, so an e2e can observe whether the
/// recall plugin injected stored memories into a LATER session's request.
pub struct MemoryProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| block_text(&m.content))
            .collect::<Vec<_>>()
            .join(" | ");
        // The extractor sub-run: save one deterministic memory, then finish.
        if system_text.contains("memory extraction sub-agent") {
            let already_saved = request.messages.iter().any(|m| m.role == ChatRole::Tool);
            if already_saved {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("memory saved"),
                    usage: None,
                    stop_reason: None,
                });
            }
            return Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "memwrite-1".to_string(),
                    tool_id: "write_memory".to_string(),
                    arguments: serde_json::json!({
                        "name": "sky-color",
                        "kind": "project",
                        "content": "the sky is green today"
                    }),
                }]),
                usage: None,
                stop_reason: None,
            });
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("recall:[{system_text}] echo:{last_user}")),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic vision-probe model: it reports the media it received on the
/// last user turn, so an e2e can assert an image survived the whole
/// adapter -> runtime -> model path (the echo model only sees text). Replies e.g.
/// `saw image/png; text: what color`.
pub struct VisionProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for VisionProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User);
        let mut medias = Vec::new();
        let mut text = String::new();
        if let Some(message) = last_user {
            for block in &message.content {
                match block {
                    ContentBlock::Text { text: t } => text.push_str(t),
                    ContentBlock::Image { source } => medias.push(match source {
                        ImageSource::Base64 { media_type, .. } => media_type.clone(),
                        ImageSource::Url { .. } => "image/url".to_string(),
                    }),
                    _ => {}
                }
            }
        }
        let reply = if medias.is_empty() {
            format!("saw no media; text: {text}")
        } else {
            format!("saw {}; text: {text}", medias.join(","))
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic probe model for the HITL e2e: it writes the user's text to a
/// relative `probe.txt` (asked -> parks for confirmation), reads it back (allowed
/// -> runs), then replies. Stateless: it decides from the transcript.
pub struct ProbeModel;

#[async_trait::async_trait]
impl LlmExecutor for ProbeModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "probe.txt", "content": user_text }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "r".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": "probe.txt" }),
            }]),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the state-machine e2e. It calls `glob` (an
/// auto-allowed perception tool that succeeds against an empty sandbox) twice: the
/// first walks the machine `s0 -> s1` (firing its emit); the second is out of order
/// (`glob` is only defined from `s0`), so the machine gate rejects it as a
/// violation. Then it ends. This exercises the tool state machine's gate, advance,
/// emit, and violation paths end to end. Stateless.
pub struct StateMachineModel;

#[async_trait::async_trait]
impl LlmExecutor for StateMachineModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let steps = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let glob = |call_id: &str| {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: call_id.into(),
                tool_id: "glob".into(),
                arguments: serde_json::json!({ "pattern": "*.txt" }),
            }])
        };
        let output = match steps {
            0 => glob("g1"),
            1 => glob("g2"),
            _ => AssistantOutput::text("done"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model that echoes the system prompt (an agent's instructions),
/// so a config e2e can assert a *published* agent's own instructions reached the
/// run. Reads the last system message. Stateless.
pub struct InstructionEchoModel;

#[async_trait::async_trait]
impl LlmExecutor for InstructionEchoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::System)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("instructions: {system}")),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the outcome e2e: replies with a draft, and revises to
/// include "FINAL" once it sees the goal loop's feedback. Stateless.
pub struct ReviseModel;

#[async_trait::async_trait]
impl LlmExecutor for ReviseModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let reply = if last_user.contains("did not meet the goal") {
            "FINAL answer"
        } else {
            "a rough draft"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the custom-tool e2e: it calls the client-executed
/// tool `submit_answer`, then replies with the result the client returned.
pub struct CustomToolModel;

#[async_trait::async_trait]
impl LlmExecutor for CustomToolModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let output = if tool_results == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".into(),
                tool_id: "submit_answer".into(),
                arguments: serde_json::json!({ "question": "what is 6 x 7?" }),
            }])
        } else {
            let result = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ChatRole::Tool)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            AssistantOutput::text(format!("got: {result}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the MCP e2e (ADR-0043 Phase 3), following the
/// `CustomToolModel` idiom. `add <a> <b>` emits a call to the namespaced MCP tool
/// `mcp__calc__add` — the fixture contract: the session registers the mock MCP
/// server under the name `calc` and the server offers a tool `add`, which
/// `awaken_ext_mcp::to_tool_id("calc", "add")` maps to exactly this id (the e2e
/// fixture must mirror that naming). A tool result is echoed as
/// `result: <text>`; anything else echoes like `EchoModel`, so the existing
/// echo-based expectations on the management server keep holding. Stateless.
pub struct McpToolModel;

#[async_trait::async_trait]
impl LlmExecutor for McpToolModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        // A tool result came back: report it (`result: <text>`), ending the turn.
        if let Some(last) = request.messages.last()
            && last.role == ChatRole::Tool
        {
            let result: String = last
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                    _ => None,
                })
                .collect();
            return Ok(ChatResponse {
                output: AssistantOutput::text(format!("result: {result}")),
                usage: None,
                stop_reason: None,
            });
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ChatRole::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        // `add <a> <b>` (two integers) → call the MCP calculator.
        let parts: Vec<&str> = last_user.split_whitespace().collect();
        if let ["add", a, b] = parts.as_slice()
            && let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>())
        {
            return Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    // Unique per step so multi-turn tool-use events keep distinct ids.
                    call_id: format!("mcp-{}", request.messages.len()),
                    tool_id: "mcp__calc__add".into(),
                    arguments: serde_json::json!({ "a": a, "b": b }),
                }]),
                usage: None,
                stop_reason: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the delegation e2e. When it holds `agent_run` it
/// delegates (to `researcher`, or to `ghost` if the user asks for it) and then
/// reports the delegate's result; without `agent_run` it answers plainly, so the
/// same model serves as the delegate sub-agent.
pub struct DelegatingModel;

#[async_trait::async_trait]
impl LlmExecutor for DelegatingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let has_delegation = request.tools.iter().any(|t| t.id == "agent_run");
        if !has_delegation {
            return Ok(ChatResponse {
                output: AssistantOutput::text("researched: 42"),
                usage: None,
                stop_reason: None,
            });
        }
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count();
        let output = if tool_results == 0 {
            let user = request
                .messages
                .iter()
                .find(|m| m.role == ChatRole::User)
                .map(|m| block_text(&m.content))
                .unwrap_or_default();
            let agent_id = if user.contains("ghost") {
                "ghost"
            } else {
                "researcher"
            };
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "d1".into(),
                tool_id: "agent_run".into(),
                arguments: serde_json::json!({ "agent_id": agent_id, "input": "do the research" }),
            }])
        } else {
            let result = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ChatRole::Tool)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            AssistantOutput::text(format!("delegate said: {result}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}
