//! The deterministic model zoo: network-free `LlmExecutor`s that drive the
//! e2e scenarios (echo/probe/tool/statemachine/memory/…). Each model scripts
//! exactly the behavior its scenario asserts, so the server runs end-to-end in
//! CI without an API key. Split from `lib.rs` (which keeps the routers and
//! process entry points).

use awaken_agent_contract::agent::content::{ContentBlock, ImageSource};
use awaken_agent_contract::agent::message::Role;
use awaken_ext_builtin_tools::{LIST_AGENTS, SEND_MESSAGE};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, TokenUsage, ToolCall,
};

use awaken_runtime_host::block_text;

/// A deterministic, network-free model: it replies with the latest User message's
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

/// Echoes ordinary Runs, but drives a real Native builtin-tool loop for the
/// oversized-output e2e. It then uses the returned relative path in another real
/// builtin tool call, proving the model can access the complete sandbox file.
pub struct OversizedToolModel;

fn deterministic_scenario_usage() -> TokenUsage {
    TokenUsage {
        prompt_tokens: 1,
        completion_tokens: 1,
        ..Default::default()
    }
}

fn materialized_tool_output_path(text: &str) -> Option<&str> {
    text.rsplit_once("complete output was written to ")?
        .1
        .split_once(". Read that file")
        .map(|(path, _)| path)
}

#[async_trait::async_trait]
impl LlmExecutor for OversizedToolModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| block_text(&message.content))
            .unwrap_or_default();
        if !last_user.contains("oversized-tool-output") {
            let mut response = EchoModel.infer(request).await?;
            response.usage = Some(deterministic_scenario_usage());
            return Ok(response);
        }
        if let Some(result) = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
        {
            let text = block_text(&result.content);
            if let Some(path) = materialized_tool_output_path(&text) {
                return Ok(ChatResponse {
                    output: AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "verify-native-spill-1".into(),
                        tool_id: "bash".into(),
                        arguments: serde_json::json!({
                            "command": format!("/usr/bin/wc -c < {path}")
                        }),
                    }]),
                    usage: Some(deterministic_scenario_usage()),
                    stop_reason: None,
                });
            }
            return Ok(ChatResponse {
                output: AssistantOutput::text(format!(
                    "native oversized tool spill readable bytes={}",
                    text.trim()
                )),
                usage: Some(deterministic_scenario_usage()),
                stop_reason: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "oversized-native-1".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({
                    "command": "/usr/bin/head -c 100001 /dev/zero | /usr/bin/tr '\\000' x"
                }),
            }]),
            usage: Some(deterministic_scenario_usage()),
            stop_reason: None,
        })
    }
}

/// A deterministic model that fails a Run on demand, so an e2e can observe the
/// `session.error` projection. A user message containing `BOOM` returns a
/// permanent (non-retryable) provider failure — surfaced as an internal
/// `RunError` and committed as `session.error`; any other message echoes, so the
/// scenario can prove the Session stays usable after a failed Run.
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
            // terminal failure); the Native Run path maps it to internal.
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
/// (executor) a Session Run resolved to (R1/R2/R5). Replies `model=<label>`.
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
/// memory via the `write_memory` tool; on a primary Run it prefixes its echo with
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
        if system_text.contains("memory extraction Agent") {
            let already_saved = request.messages.iter().any(|m| m.role == Role::Tool);
            if already_saved {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("memory saved"),
                    usage: None,
                    stop_reason: None,
                });
            }
            // Name the memory after a `fact-<tag>` token in the transcript when
            // present, so distinct Runs accumulate distinct memories (which
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
/// latest User message, so an e2e can assert an image survived the whole
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
                        ImageSource::File { .. } => "image/unmaterialized-file".to_string(),
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
/// On its first Run it writes the User text into the mounted memory store,
/// realized read-write at the catalog-derived
/// `.mnt/memory/durable-memory-store` path; it then reads the same path before
/// finishing, proving the tool observed the mounted bytes rather than an unrelated
/// workdir file. The host harvests that write back into the store under its stable
/// id on Run completion. Driving write -> read -> harvest lets an e2e prove the store's
/// contents survive a real process restart.
pub struct MemoryResourceModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryResourceModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        const MEMORY_NOTE_PATH: &str = ".mnt/memory/durable-memory-store/note.md";
        let tool_results = request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count();
        let user_text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        let output = match tool_results {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "memres-1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": MEMORY_NOTE_PATH,
                    "content": user_text,
                }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "memres-2".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({ "path": MEMORY_NOTE_PATH }),
            }]),
            _ => AssistantOutput::text("memory persisted"),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the github_repository resource e2e (ADR-0038). The
/// repo is cloned host-side into the working tree at `/workspace/repo`; the model
/// first `read`s the seed file (proving the clone landed inside the jail) and then
/// `write`s a new file that remains sandbox-local until an explicit publication
/// workflow consumes it. Sequenced off the tool-result count so it needs no transcript parsing.
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
                arguments: serde_json::json!({ "path": "/workspace/repo/README.md" }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": "/workspace/repo/NEW.txt",
                    "content": "AGENT_REPO_MARKER_3390"
                }),
            }]),
            _ => AssistantOutput::text("repo Run done"),
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

/// Return one deterministic step of the fixed Managed coordination protocol.
/// Both coordinator fixtures use this helper so `list_agents` -> `send_message`
/// sequencing has one owner. A tool-free child returns `None` and keeps its own
/// model behavior; the `send_message` result is only an admission receipt, never
/// the child Agent's eventual reply.
fn managed_coordination_output(
    request: &ChatRequest,
    agent_id: &str,
    message: &str,
) -> Option<AssistantOutput> {
    let has_list = request.tools.iter().any(|tool| tool.id == LIST_AGENTS);
    let has_send = request.tools.iter().any(|tool| tool.id == SEND_MESSAGE);
    if !has_list || !has_send {
        return None;
    }
    // SessionApplication resumes the coordinator after a child settles by
    // appending a typed internal User message whose stable text envelope starts
    // this way. ChatRequest intentionally omits persisted Message ids, so this
    // deterministic scenario model recognizes the envelope at the remaining
    // contract boundary. A report Run terminates directly: treating it as a new
    // user request would recursively fan out one child per completed child.
    if request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .is_some_and(|message| block_text(&message.content).starts_with("Message from agent "))
    {
        return Some(AssistantOutput::text(
            "coordination completed from child report",
        ));
    }
    let current_step = request
        .messages
        .iter()
        .rev()
        .take_while(|message| message.role != Role::User)
        .collect::<Vec<_>>();
    let tool_results = current_step
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count();
    let run_ordinal = request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .count();
    Some(match tool_results {
        0 => AssistantOutput::from_tool_calls(vec![ToolCall {
            call_id: format!("list-agents-{run_ordinal}"),
            tool_id: LIST_AGENTS.into(),
            arguments: serde_json::json!({}),
        }]),
        1 => AssistantOutput::from_tool_calls(vec![ToolCall {
            call_id: format!("send-agent-{run_ordinal}"),
            tool_id: SEND_MESSAGE.into(),
            arguments: serde_json::json!({
                "agent_id": agent_id,
                "message": message,
            }),
        }]),
        _ => {
            let result = current_step
                .iter()
                .find(|message| message.role == Role::Tool)
                .map(|message| block_text(&message.content))
                .unwrap_or_default();
            let failed = current_step.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
            });
            let outcome = if failed { "failed" } else { "accepted" };
            AssistantOutput::text(format!("coordination {outcome}: {result}"))
        }
    })
}

/// Registry-backed coordinator fixture: a coordinator discovers its fixed
/// Managed roster, sends to the Agent id named after `delegate to `, and ends
/// after the admission receipt. A tool-free worker returns its frozen
/// instructions, making an authored roster's source-revision pin observable.
pub struct RegistryDelegatingModel;

#[async_trait::async_trait]
impl LlmExecutor for RegistryDelegatingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| block_text(&message.content))
            .unwrap_or_default();
        let agent_id = user.strip_prefix("delegate to ").unwrap_or_default().trim();
        let output =
            managed_coordination_output(&request, agent_id, "report your frozen instructions")
                .unwrap_or_else(|| {
                    let system = request
                        .messages
                        .iter()
                        .rev()
                        .find(|message| message.role == Role::System)
                        .map(|message| block_text(&message.content))
                        .unwrap_or_default();
                    AssistantOutput::text(format!("worker instructions: {system}"))
                });
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A deterministic model for the Outcome E2E. It acts as both the Worker and
/// the default tool-free Judge, making the production Agent-Grader path
/// observable without provider credentials.
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
        let reply = if last_user.contains("Evaluate this Outcome input") {
            if last_user.contains("\"rubric\":\"FINAL\"") && last_user.contains("FINAL answer") {
                r#"{"result":"satisfied","explanation":"native judge accepted FINAL"}"#
            } else {
                r#"{"result":"needs_revision","explanation":"native judge requests the rubric deliverable"}"#
            }
        } else if last_user.contains("Revise the deliverable") {
            "FINAL answer"
        } else if last_user.contains("iteration limit was reached") {
            "acknowledged remaining feedback"
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
                .find(|message| message.role == Role::Tool)
                .map(|message| block_text(&message.content))
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

/// A deterministic model that requests the real `web_fetch` or `web_search`
/// tool named by the latest User command, then reports the returned result and
/// whether the runtime marked it as an error. It owns no Web policy or provider
/// behavior; those stay in the production Host and built-in extension paths.
pub(crate) struct WebToolDrivingModel;

#[async_trait::async_trait]
impl LlmExecutor for WebToolDrivingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if let Some(last) = request.messages.last()
            && last.role == Role::Tool
        {
            let failed = last
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }));
            let status = if failed { "error" } else { "result" };
            return Ok(ChatResponse {
                output: AssistantOutput::text(format!(
                    "web-{status}: {}",
                    block_text(&last.content)
                )),
                usage: Some(deterministic_scenario_usage()),
                stop_reason: None,
            });
        }

        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| block_text(&message.content))
            .unwrap_or_default();
        let call = last_user
            .strip_prefix("web-fetch ")
            .filter(|url| !url.is_empty())
            .map(|url| ("web_fetch", serde_json::json!({ "url": url })))
            .or_else(|| {
                last_user
                    .strip_prefix("web-search ")
                    .filter(|query| !query.is_empty())
                    .map(|query| ("web_search", serde_json::json!({ "query": query })))
            });
        let output = call.map_or_else(
            || AssistantOutput::text(format!("Echo: {last_user}")),
            |(tool_id, arguments)| {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: format!("{tool_id}-{}", request.messages.len()),
                    tool_id: tool_id.into(),
                    arguments,
                }])
            },
        );
        Ok(ChatResponse {
            output,
            usage: Some(deterministic_scenario_usage()),
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
        // A tool result came back: report it (`result: <text>`), completing the Run.
        if let Some(last) = request.messages.last()
            && last.role == Role::Tool
        {
            let result = block_text(&last.content);
            return Ok(ChatResponse {
                output: AssistantOutput::text(format!("result: {result}")),
                usage: Some(deterministic_scenario_usage()),
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
                    // Unique per Step so multi-Run tool-use events keep distinct ids.
                    call_id: format!("mcp-{}", request.messages.len()),
                    tool_id: "mcp__calc__add".into(),
                    arguments: serde_json::json!({ "a": a, "b": b }),
                }]),
                usage: Some(deterministic_scenario_usage()),
                stop_reason: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {last_user}")),
            usage: Some(deterministic_scenario_usage()),
            stop_reason: None,
        })
    }
}

/// A deterministic model for the Managed coordination e2e. A coordinator first
/// calls fixed `list_agents`, then asynchronously calls fixed `send_message`
/// (Native `researcher`, ACP `acp-worker`, explicit `self`, or `ghost`) and ends
/// on the admission receipt. A child answers on its own Thread. The self-copy
/// task answers directly so one recursive edge cannot manufacture a deeper tree.
pub struct DelegatingModel;

#[async_trait::async_trait]
impl LlmExecutor for DelegatingModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| block_text(&message.content))
            .unwrap_or_default();
        if user.contains("self-copy task") {
            return Ok(ChatResponse {
                output: AssistantOutput::text("self copy: 42"),
                usage: None,
                stop_reason: None,
            });
        }
        let matrix_acp = awaken_run_executor_acp::known_acp_clis()
            .iter()
            .find(|cli| user.contains(&format!("{} acp agent", cli.id)))
            .map(|cli| format!("acp-{}-worker", cli.id));
        let agent_id = if user.contains("ghost") {
            "ghost".to_string()
        } else if let Some(agent_id) = matrix_acp {
            agent_id
        } else if user.contains("acp agent") {
            "acp-worker".to_string()
        } else if user.contains("self agent") {
            "assistant".to_string()
        } else {
            "researcher".to_string()
        };
        let message = if agent_id == "assistant" {
            "self-copy task"
        } else if agent_id.starts_with("acp-") && agent_id.ends_with("-worker") {
            "matrix-basic delegated child"
        } else if user.contains("delegate lifecycle:") {
            user.as_str()
        } else {
            "do the research"
        };
        let output = managed_coordination_output(&request, &agent_id, message)
            .unwrap_or_else(|| AssistantOutput::text("researched: 42"));
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The compaction e2e model. On the `compactor` sub-run (its system prompt is
/// the summarize instructions) it returns a fixed summary line; on a primary Run
/// it prefixes its reply with the system/context text it received, so an e2e
/// can observe the folded summary being injected on a later Run.
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
                output: AssistantOutput::text("SUMMARY: earlier Runs folded"),
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
    //! contracts the e2e scenarios rely on (which tool a Run calls, how a marker is
    //! parsed, when a Run fails). These are network-free `infer(request)` functions,
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

    fn tool_error(call_id: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::tool_result_with_error(
                call_id,
                vec![ContentBlock::text(text)],
                true,
            )],
        }
    }

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model_binding: ModelBinding::new("id", "m", "default"),
            inference: Default::default(),
            messages,
            tools: vec![],
        }
    }

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("scenario", id, "", serde_json::json!({}))
    }

    async fn infer(model: &impl LlmExecutor, request: ChatRequest) -> ChatResponse {
        model
            .infer(request)
            .await
            .expect("deterministic model replies")
    }

    #[tokio::test]
    async fn echo_model_reflects_the_last_user_message() {
        // Causes: C1 the request contains ordered User messages. Effects: E1 the
        // fixture replies with only the last User text under the stable `Echo:`
        // prefix. Constraints/invariants: non-User messages and earlier User
        // messages cannot replace the final User input. Decision rule ECHO1:
        // C1 with `first,second` -> E1=`Echo: second`.
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
        // Anything else stays a usable echo Run.
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
        // Cause/effect decision table: valid add -> one calculator call; invalid
        // add -> echo; tool result -> final report. Every logical request also
        // reports the same deterministic two-token usage so the management
        // Scenario can exercise the real shared-budget request gate.
        // Constraints/invariants: only two integer operands select calc.add, and
        // only a committed Tool result selects the final report; every branch
        // emits exactly one identical deterministic usage observation.
        let call = infer(&McpToolModel, req(vec![msg(Role::User, "add 2 3")])).await;
        assert_eq!(call.usage, Some(deterministic_scenario_usage()));
        let calls = call.output.tool_calls();
        assert_eq!(calls[0].tool_id, "mcp__calc__add");
        assert_eq!(calls[0].arguments["a"], 2);
        assert_eq!(calls[0].arguments["b"], 3);
        // Non-integer operands do NOT match the calculator: fall through to echo.
        let echo = infer(&McpToolModel, req(vec![msg(Role::User, "add x y")])).await;
        assert_eq!(echo.usage, Some(deterministic_scenario_usage()));
        assert!(echo.output.tool_calls().is_empty());
        assert_eq!(echo.output.text_content(), "Echo: add x y");
        // A returned tool result is reported as `result: <text>`.
        let reported = infer(
            &McpToolModel,
            req(vec![msg(Role::User, "add 2 3"), tool_result("mcp-1", "5")]),
        )
        .await;
        assert_eq!(reported.usage, Some(deterministic_scenario_usage()));
        assert_eq!(reported.output.text_content(), "result: 5");
    }

    #[tokio::test]
    async fn delegating_model_uses_the_fixed_managed_coordination_sequence() {
        // Cause/effect graph: C1=both fixed coordination descriptors are present,
        // C2=current-Run result count is 0/1/2, C3=target is published/missing/self,
        // C4=send result is success/error, C5=latest User input is an internal
        // child report. Effects are E1=plain child reply,
        // E2=list_agents, E3=send_message with exactly one selector, E4=receipt
        // acknowledgement, E5=surfaced failure, and E6=report acknowledgement
        // without another tool call. Results before the last User Run are
        // constrained to have no effect on the new Run.
        //
        // Decision table:
        // | Rule | C1 | C2 | C3       | C4      | Effect |
        // | D1   | no | -  | child    | -       | E1     |
        // | D2   | yes| 0  | any      | -       | E2     |
        // | D3   | yes| 1  | roster   | -       | E3     |
        // | D4   | yes| 2  | roster   | success | E4     |
        // | D5   | yes| 2  | missing  | error   | E5     |
        // | D6   | yes| 0  | self task| -       | E1     |
        // | D7   | yes| -  | child report | -   | E6     |
        // Constraints/invariants: only current-Run results advance this fixed
        // sequence; without the complete fixed surface the model is a child
        // Agent and answers plainly, and a child report can never trigger a
        // second send.
        let plain = infer(&DelegatingModel, req(vec![msg(Role::User, "research")])).await;
        assert_eq!(plain.output.text_content(), "researched: 42");

        // A Managed coordinator discovers the roster before addressing a child.
        let mut r = req(vec![msg(Role::User, "go research this")]);
        r.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let listed = infer(&DelegatingModel, r).await;
        assert_eq!(listed.output.tool_calls()[0].tool_id, LIST_AGENTS);
        assert_eq!(
            listed.output.tool_calls()[0].arguments,
            serde_json::json!({})
        );

        // The roster result advances to the asynchronous send receipt boundary.
        let mut r = req(vec![
            msg(Role::User, "go research this"),
            tool_result("list-agents-1", r#"[{"agent_id":"researcher"}]"#),
        ]);
        r.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let delegated = infer(&DelegatingModel, r).await;
        let call = &delegated.output.tool_calls()[0];
        assert_eq!(call.tool_id, SEND_MESSAGE);
        assert_eq!(call.arguments["agent_id"], "researcher");
        assert_eq!(call.arguments["message"], "do the research");
        assert!(call.arguments.get("session_thread_id").is_none());

        // A missing target is still sent through the one fixed command; runtime
        // roster authority rejects it instead of the fixture inventing a child.
        let mut rg = req(vec![
            msg(Role::User, "delegate to the ghost agent"),
            tool_result("list-agents-1", "[]"),
        ]);
        rg.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let ghost = infer(&DelegatingModel, rg).await;
        assert_eq!(ghost.output.tool_calls()[0].arguments["agent_id"], "ghost");

        // Successful/error send results end the coordinator Run; neither is a
        // synchronous child reply.
        let mut accepted = req(vec![
            msg(Role::User, "go"),
            tool_result("list-agents-1", "[]"),
            tool_result("send-agent-1", r#"{"accepted":true}"#),
        ]);
        accepted.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let accepted = infer(&DelegatingModel, accepted).await;
        assert_eq!(
            accepted.output.text_content(),
            r#"coordination accepted: {"accepted":true}"#
        );

        let mut failed = req(vec![
            msg(Role::User, "ghost"),
            tool_result("list-agents-1", "[]"),
            tool_error("send-agent-1", "not in frozen roster"),
        ]);
        failed.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let failed = infer(&DelegatingModel, failed).await;
        assert_eq!(
            failed.output.text_content(),
            "coordination failed: not in frozen roster"
        );

        // A completed child wakes the coordinator in a later Run. That report
        // is terminal input, not another delegation request (D7).
        let mut report = req(vec![msg(
            Role::User,
            "Message from agent researcher (thread child-1):\nresearched: 42",
        )]);
        report.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let report = infer(&DelegatingModel, report).await;
        assert!(report.output.tool_calls().is_empty());
        assert_eq!(
            report.output.text_content(),
            "coordination completed from child report"
        );

        let mut self_copy = req(vec![msg(Role::User, "self-copy task")]);
        self_copy.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let self_copy = infer(&DelegatingModel, self_copy).await;
        assert_eq!(self_copy.output.text_content(), "self copy: 42");
    }

    #[tokio::test]
    async fn registry_delegating_model_reuses_the_fixed_coordination_sequence() {
        // Causes: fixed surface absent/present and 0/1/2 current-Run results.
        // Effects: worker instruction echo or the shared list/send/receipt steps.
        // Decision rules: R1(absent)->worker; R2(present,0)->list;
        // R3(present,1)->send exact authored id; R4(present,2)->end on receipt;
        // R5(present,child report)->acknowledge without a second send.
        // Constraints/invariants: the shared fixed descriptors and current-Run
        // results are the only sequence inputs; authored Agent identity is
        // preserved exactly and a child report is terminal for coordination.
        let worker = infer(
            &RegistryDelegatingModel,
            req(vec![msg(Role::System, "WORKER_REVISION_ONE")]),
        )
        .await;
        assert_eq!(
            worker.output.text_content(),
            "worker instructions: WORKER_REVISION_ONE"
        );

        let mut listed = req(vec![msg(Role::User, "delegate to agent_worker")]);
        listed.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let listed = infer(&RegistryDelegatingModel, listed).await;
        assert_eq!(listed.output.tool_calls()[0].tool_id, LIST_AGENTS);

        let mut sent = req(vec![
            msg(Role::User, "delegate to agent_worker"),
            tool_result("list-agents-1", r#"[{"agent_id":"agent_worker"}]"#),
        ]);
        sent.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let sent = infer(&RegistryDelegatingModel, sent).await;
        let call = &sent.output.tool_calls()[0];
        assert_eq!(call.tool_id, SEND_MESSAGE);
        assert_eq!(call.arguments["agent_id"], "agent_worker");
        assert_eq!(call.arguments["message"], "report your frozen instructions");

        let mut accepted = req(vec![
            msg(Role::User, "delegate to agent_worker"),
            tool_result("list-agents-1", "[]"),
            tool_result("send-agent-1", r#"{"accepted":true}"#),
        ]);
        accepted.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let accepted = infer(&RegistryDelegatingModel, accepted).await;
        assert!(
            accepted
                .output
                .text_content()
                .starts_with("coordination accepted:")
        );

        let mut report = req(vec![msg(
            Role::User,
            "Message from agent execution-worker (thread child-1):\nworker instructions",
        )]);
        report.tools = vec![tool(LIST_AGENTS), tool(SEND_MESSAGE)];
        let report = infer(&RegistryDelegatingModel, report).await;
        assert!(report.output.tool_calls().is_empty());
        assert_eq!(
            report.output.text_content(),
            "coordination completed from child report"
        );
    }

    #[tokio::test]
    async fn memory_probe_names_a_memory_after_a_fact_tag_else_falls_back() {
        let extractor = |text: &str| {
            req(vec![
                msg(Role::System, "you are a memory extraction Agent"),
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
                msg(Role::System, "you are a memory extraction Agent"),
                msg(Role::User, "remember fact-blue"),
                tool_result("memwrite-1", "ok"),
            ]),
        )
        .await;
        assert!(saved.output.tool_calls().is_empty());
        assert_eq!(saved.output.text_content(), "memory saved");
    }

    #[tokio::test]
    async fn revise_model_follows_outcome_revision_sequence() {
        let draft = infer(&ReviseModel, req(vec![msg(Role::User, "write it")])).await;
        assert_eq!(draft.output.text_content(), "a rough draft");

        let rejected = infer(
            &ReviseModel,
            req(vec![msg(
                Role::User,
                r#"Evaluate this Outcome input against its rubric. {"rubric":"FINAL","input":"a rough draft"}"#,
            )]),
        )
        .await;
        assert_eq!(
            rejected.output.text_content(),
            r#"{"result":"needs_revision","explanation":"native judge requests the rubric deliverable"}"#
        );

        let finalized = infer(
            &ReviseModel,
            req(vec![msg(
                Role::User,
                "Revise the deliverable for this Outcome. Grader feedback: include FINAL",
            )]),
        )
        .await;
        assert_eq!(finalized.output.text_content(), "FINAL answer");

        let accepted = infer(
            &ReviseModel,
            req(vec![msg(
                Role::User,
                r#"Evaluate this Outcome input against its rubric. {"rubric":"FINAL","input":"FINAL answer"}"#,
            )]),
        )
        .await;
        assert_eq!(
            accepted.output.text_content(),
            r#"{"result":"satisfied","explanation":"native judge accepted FINAL"}"#
        );

        let capped = infer(
            &ReviseModel,
            req(vec![msg(
                Role::User,
                "The Outcome iteration limit was reached. Briefly acknowledge remaining feedback.",
            )]),
        )
        .await;
        assert_eq!(
            capped.output.text_content(),
            "acknowledged remaining feedback"
        );
    }
}
