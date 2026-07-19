//! The deterministic model zoo: network-free `LlmExecutor`s that drive the
//! e2e scenarios (echo/probe/tool/statemachine/memory/…). Each model scripts
//! exactly the behavior its scenario asserts, so the server runs end-to-end in
//! CI without an API key. Split from `lib.rs` (which keeps the routers and
//! composition roots).

use awaken_agent_contract::agent::content::{ContentBlock, ImageSource};
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};

use awaken_runtime_host::block_text;

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
            .find(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model that fails a turn on demand, so an e2e can observe the
/// `session.error` projection. A user message containing `BOOM` returns a
/// permanent (non-retryable) provider failure — surfaced as an internal
/// `RunError` and committed as `session.error`; any other message echoes, so the
/// scenario can prove the session stays usable after a failed turn.
pub struct ErrorModel;

#[async_trait::async_trait]
impl LlmExecutor for ErrorModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        if last_user.contains("BOOM") {
            // Permanent so the engine does not retry (a fast, deterministic
            // terminal failure); the native turn path maps it to internal.
            return Err(awaken_runtime_contract::llm::Error::InvalidRequest(
                "scenario: BOOM".into(),
            ));
        }
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
            .find(|m| m.role == Role::User)
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
            .filter(|m| m.role == Role::System)
            .map(|m| block_text(&m.content))
            .collect::<Vec<_>>()
            .join(" | ");
        // The extractor sub-run: save one deterministic memory, then finish.
        if system_text.contains("memory extraction sub-agent") {
            let already_saved = request.messages.iter().any(|m| m.role == Role::Tool);
            if already_saved {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("memory saved"),
                    usage: None,
                    stop_reason: None,
                });
            }
            // Name the memory after a `fact-<tag>` token in the transcript when
            // present, so distinct turns accumulate distinct memories (which
            // drives the recall SELECTOR once the store passes its threshold);
            // otherwise fall back to the fixed sky-color memory.
            let transcript: String = request
                .messages
                .iter()
                .map(|m| block_text(&m.content))
                .collect::<Vec<_>>()
                .join(" ");
            let (name, content) = transcript
                .split_whitespace()
                .find(|w| w.starts_with("fact-"))
                .map(|tag| (tag.to_string(), format!("remember {tag}")))
                .unwrap_or_else(|| {
                    (
                        "sky-color".to_string(),
                        "the sky is green today".to_string(),
                    )
                });
            return Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "memwrite-1".to_string(),
                    tool_id: "write_memory".to_string(),
                    arguments: serde_json::json!({
                        "name": name,
                        "kind": "project",
                        "content": content
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
            .find(|m| m.role == Role::User)
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
        let last_user = request.messages.iter().rev().find(|m| m.role == Role::User);
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
/// relative `probe.txt` (asked -> awaits for confirmation), reads it back (allowed
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
            .filter(|m| m.role == Role::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .find(|m| m.role == Role::User)
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

/// A deterministic model for the memory_store RESOURCE durability e2e (ADR-0038).
/// On its first turn it writes the user's text into the mounted memory store,
/// realized read-write at `.mnt/notes.txt`; the host harvests that write back into
/// the store under its stable id on turn end. On the follow-up turn (a tool result
/// is present) it replies `memory persisted`. Driving write -> harvest lets an e2e
/// prove the store's contents survive a real process restart.
pub struct MemoryResourceModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryResourceModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let wrote = request.messages.iter().any(|m| m.role == Role::Tool);
        let user_text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let output = if wrote {
            AssistantOutput::text("memory persisted")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "memres-1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": ".mnt/notes.txt", "content": user_text }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the github_repository resource e2e (ADR-0038). The
/// repo is cloned host-side into the working tree at `workspace/repo`; the model
/// first `read`s the seed file (proving the clone landed inside the jail) and then
/// `write`s a new file, which the host commits and pushes back to the remote on
/// harvest. Sequenced off the tool-result count so it needs no transcript parsing.
pub struct GitRepoModel;

#[async_trait::async_trait]
impl LlmExecutor for GitRepoModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count();
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "r".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": "workspace/repo/README.md" }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": "workspace/repo/NEW.txt",
                    "content": "AGENT_REPO_MARKER_3390"
                }),
            }]),
            _ => AssistantOutput::text("repo turn done"),
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
/// Drives the remote-hand e2e (ADR-0044). Turn 1 calls the `bash` hand tool to
/// echo a fixed marker; turn 2 reads the tool result the hand returned and answers
/// with it. When the host wires a remote hand, `bash` runs OUT of the run loop,
/// across the framed channel, in a separate hand task — the final answer carrying
/// the marker proves the whole brain→hand→brain path end to end. Stateless.
pub struct RemoteHandModel;

/// The marker the hand's `bash echo` emits; the e2e asserts it round-trips.
pub const REMOTE_HAND_MARKER: &str = "REMOTE-HAND-OK-9f31";

#[async_trait::async_trait]
impl LlmExecutor for RemoteHandModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        // The most recent tool result the (remote) hand returned, if any.
        let hand_said = request.messages.iter().rev().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                _ => None,
            })
        });
        let output = match hand_said {
            None => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "bash-1".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({ "command": format!("echo {REMOTE_HAND_MARKER}") }),
            }]),
            Some(text) => AssistantOutput::text(format!("hand said: {}", text.trim())),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

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
            .filter(|m| m.role == Role::Tool)
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
            .find(|m| m.role == Role::System)
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
            .find(|m| m.role == Role::User)
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
            .filter(|m| m.role == Role::Tool)
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
                .find(|m| m.role == Role::Tool)
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
            && last.role == Role::Tool
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
            .find(|m| m.role == Role::User)
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
            .filter(|m| m.role == Role::Tool)
            .count();
        let output = if tool_results == 0 {
            let user = request
                .messages
                .iter()
                .find(|m| m.role == Role::User)
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
                .find(|m| m.role == Role::Tool)
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

/// The compaction e2e model. On the `compactor` sub-run (its system prompt is
/// the summarize instructions) it returns a fixed summary line; on a main turn
/// it prefixes its reply with the system/context text it received, so an e2e
/// can observe the folded summary being injected on a later turn.
pub struct CompactionModel;

#[async_trait::async_trait]
impl LlmExecutor for CompactionModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| block_text(&m.content))
            .collect::<Vec<_>>()
            .join(" | ");
        let joined_user: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .collect::<Vec<_>>()
            .join(" ");
        if joined_user.contains("summarize") || system_text.contains("summar") {
            return Ok(ChatResponse {
                output: AssistantOutput::text("SUMMARY: earlier turns folded"),
                usage: None,
                stop_reason: None,
            });
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("ctx:[{system_text}] echo:{last_user}")),
            usage: None,
            stop_reason: None,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Unit pins on the deterministic model zoo's *decision logic* — the fixture
    //! contracts the e2e scenarios rely on (which tool a turn calls, how a marker is
    //! parsed, when a turn fails). These are network-free `infer(request)` functions,
    //! so a silent drift here (e.g. the `add a b` parser breaking) is caught locally
    //! rather than only as a hard-to-localize e2e failure driving a real binary.
    use super::*;
    use awaken_runtime_contract::llm::{ChatMessage, Error};
    use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};

    fn msg(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// A `Tool`-role message carrying one tool result (the shape a run appends after
    /// executing a model's tool call).
    fn tool_result(call_id: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::tool_result(
                call_id,
                vec![ContentBlock::text(text)],
            )],
        }
    }

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model_binding: ModelBinding::new("id", "m", "default"),
            messages,
            tools: vec![],
        }
    }

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor {
            id: id.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            content_hash: String::new(),
            recovery_policy: Default::default(),
        }
    }

    async fn infer(model: &impl LlmExecutor, request: ChatRequest) -> ChatResponse {
        model
            .infer(request)
            .await
            .expect("deterministic model replies")
    }

    #[tokio::test]
    async fn echo_model_reflects_the_last_user_turn() {
        let resp = infer(
            &EchoModel,
            req(vec![msg(Role::User, "first"), msg(Role::User, "second")]),
        )
        .await;
        assert_eq!(resp.output.text_content(), "Echo: second");
    }

    #[tokio::test]
    async fn error_model_fails_on_boom_and_echoes_otherwise() {
        // BOOM → a permanent (non-retryable) provider error the run maps to internal.
        let err = ErrorModel
            .infer(req(vec![msg(Role::User, "please BOOM now")]))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
        // Anything else stays a usable echo turn.
        let ok = infer(&ErrorModel, req(vec![msg(Role::User, "hello")])).await;
        assert_eq!(ok.output.text_content(), "Echo: hello");
    }

    #[tokio::test]
    async fn probe_model_sequences_write_then_read_then_done_by_tool_result_count() {
        // 0 results → write probe.txt with the user's text.
        let r0 = infer(&ProbeModel, req(vec![msg(Role::User, "payload")])).await;
        let calls = r0.output.tool_calls();
        assert_eq!(calls[0].tool_id, "write");
        assert_eq!(calls[0].arguments["path"], "probe.txt");
        assert_eq!(calls[0].arguments["content"], "payload");
        // 1 result → read it back.
        let r1 = infer(
            &ProbeModel,
            req(vec![msg(Role::User, "payload"), tool_result("w", "ok")]),
        )
        .await;
        assert_eq!(r1.output.tool_calls()[0].tool_id, "read");
        // 2 results → done (no more tool calls).
        let r2 = infer(
            &ProbeModel,
            req(vec![
                msg(Role::User, "payload"),
                tool_result("w", "ok"),
                tool_result("r", "payload"),
            ]),
        )
        .await;
        assert!(r2.output.tool_calls().is_empty());
        assert_eq!(r2.output.text_content(), "done");
    }

    #[tokio::test]
    async fn mcp_tool_model_parses_add_and_reports_results() {
        // `add <int> <int>` → the namespaced MCP calculator tool with parsed operands.
        let call = infer(&McpToolModel, req(vec![msg(Role::User, "add 2 3")])).await;
        let calls = call.output.tool_calls();
        assert_eq!(calls[0].tool_id, "mcp__calc__add");
        assert_eq!(calls[0].arguments["a"], 2);
        assert_eq!(calls[0].arguments["b"], 3);
        // Non-integer operands do NOT match the calculator: fall through to echo.
        let echo = infer(&McpToolModel, req(vec![msg(Role::User, "add x y")])).await;
        assert!(echo.output.tool_calls().is_empty());
        assert_eq!(echo.output.text_content(), "Echo: add x y");
        // A returned tool result is reported as `result: <text>`.
        let reported = infer(
            &McpToolModel,
            req(vec![msg(Role::User, "add 2 3"), tool_result("mcp-1", "5")]),
        )
        .await;
        assert_eq!(reported.output.text_content(), "result: 5");
    }

    #[tokio::test]
    async fn delegating_model_routes_by_tool_presence_and_target() {
        // Without the `agent_run` tool it is the delegate sub-agent: it answers plainly.
        let plain = infer(&DelegatingModel, req(vec![msg(Role::User, "research")])).await;
        assert_eq!(plain.output.text_content(), "researched: 42");
        // With `agent_run` it delegates — to `researcher` by default…
        let mut r = req(vec![msg(Role::User, "go research this")]);
        r.tools = vec![tool("agent_run")];
        let deleg = infer(&DelegatingModel, r).await;
        assert_eq!(
            deleg.output.tool_calls()[0].arguments["agent_id"],
            "researcher"
        );
        // …and to `ghost` when the user names it (the missing-agent fail path).
        let mut rg = req(vec![msg(Role::User, "delegate to the ghost agent")]);
        rg.tools = vec![tool("agent_run")];
        let ghost = infer(&DelegatingModel, rg).await;
        assert_eq!(ghost.output.tool_calls()[0].arguments["agent_id"], "ghost");
        // With a delegate result present it reports it.
        let mut rr = req(vec![msg(Role::User, "go"), tool_result("d1", "the answer")]);
        rr.tools = vec![tool("agent_run")];
        let reported = infer(&DelegatingModel, rr).await;
        assert_eq!(reported.output.text_content(), "delegate said: the answer");
    }

    #[tokio::test]
    async fn memory_probe_names_a_memory_after_a_fact_tag_else_falls_back() {
        let extractor = |text: &str| {
            req(vec![
                msg(Role::System, "you are a memory extraction sub-agent"),
                msg(Role::User, text),
            ])
        };
        // A `fact-<tag>` token in the transcript names the saved memory…
        let tagged = infer(&MemoryProbeModel, extractor("remember fact-blue please")).await;
        let call = &tagged.output.tool_calls()[0];
        assert_eq!(call.tool_id, "write_memory");
        assert_eq!(call.arguments["name"], "fact-blue");
        // …otherwise it falls back to the fixed sky-color memory.
        let fallback = infer(&MemoryProbeModel, extractor("no tag here")).await;
        assert_eq!(
            fallback.output.tool_calls()[0].arguments["name"],
            "sky-color"
        );
        // Once a tool result is present (already saved), the extractor sub-run ends.
        let saved = infer(
            &MemoryProbeModel,
            req(vec![
                msg(Role::System, "you are a memory extraction sub-agent"),
                msg(Role::User, "remember fact-blue"),
                tool_result("memwrite-1", "ok"),
            ]),
        )
        .await;
        assert!(saved.output.tool_calls().is_empty());
        assert_eq!(saved.output.text_content(), "memory saved");
    }

    #[tokio::test]
    async fn revise_model_finalizes_only_after_goal_feedback() {
        let draft = infer(&ReviseModel, req(vec![msg(Role::User, "write it")])).await;
        assert_eq!(draft.output.text_content(), "a rough draft");
        let finalized = infer(
            &ReviseModel,
            req(vec![msg(Role::User, "that did not meet the goal, revise")]),
        )
        .await;
        assert_eq!(finalized.output.text_content(), "FINAL answer");
    }
}
