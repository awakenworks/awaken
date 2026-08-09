use super::*;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};

fn spec(instructions: &str) -> ResolvedSpec {
    ResolvedSpec {
        model_candidates: Vec::new(),
        catalog_fingerprint: CatalogFingerprint("c".to_string()),
        instructions: instructions.to_string(),
        max_steps: 16,
        delegation_limits: Default::default(),
        model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding {
                provider_identity_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
        ),
        tool_descriptors: Vec::new(),
        plugin_ids: Vec::new(),
        plugin_config: Default::default(),
        context_policy: ContextPolicy::KeepAll,
        tool_presentation: Default::default(),
    }
}

fn user_message() -> Message {
    Message::text(MessageId("m1".to_string()), Role::User, "hi")
}

#[test]
fn instructions_lead_the_request_as_a_system_message() {
    let request = build_chat_request(
        &spec("be helpful"),
        &[],
        &[user_message()],
        &[],
        &Default::default(),
    );
    assert_eq!(request.messages.len(), 2);
    assert!(matches!(request.messages[0].role, Role::System));
    assert_eq!(
        request.messages[0].content,
        vec![ContentBlock::text("be helpful")]
    );
    assert!(matches!(request.messages[1].role, Role::User));
}

#[test]
fn empty_instructions_contribute_no_system_message() {
    let request = build_chat_request(&spec(""), &[], &[user_message()], &[], &Default::default());
    assert_eq!(request.messages.len(), 1);
    assert!(matches!(request.messages[0].role, Role::User));
}

#[tokio::test]
async fn model_visible_text_tool_results_are_never_semantically_empty() {
    /*
     * Tool-result materialization decision table. Causes: C1 output contains
     * only text (including no blocks); C2 extracted text is empty/whitespace;
     * C3 output is an error; C4 output has meaningful text; C5 output contains
     * a structured non-text block. Effects: E1 materialize the canonical
     * success placeholder; E2 materialize the distinct error placeholder; E3
     * preserve the original payload. Constraints: C4 excludes C2; C5 excludes
     * C1. Rules: T1 C1+C2+!C3=>E1; T2 C1+C2+C3=>E2;
     * T3 C1+C4=>E3; T4 C5=>E3. The existing spiller tests own the orthogonal
     * post-materialization size/storage policy and are not duplicated here.
     */
    let context = RuntimeRunContext::new();
    let run_id = RunId("run-empty-tool-output".into());

    let success = spill_tool_output(
        &context,
        &run_id,
        ToolOutput::ok_blocks("success", Vec::new()),
    )
    .await
    .expect("empty success materializes");
    assert_eq!(
        success.content,
        vec![ContentBlock::text(
            "Tool completed successfully without output."
        )]
    );

    let failure = spill_tool_output(&context, &run_id, ToolOutput::error("failure", "  \n"))
        .await
        .expect("empty failure materializes");
    assert_eq!(
        failure.content,
        vec![ContentBlock::text("Tool failed without an error message.")]
    );

    let meaningful = ToolOutput::ok("meaningful", "zero rows changed");
    assert_eq!(
        spill_tool_output(&context, &run_id, meaningful.clone())
            .await
            .expect("meaningful output remains valid"),
        meaningful
    );

    let structured = ToolOutput::ok_blocks(
        "structured",
        vec![ContentBlock::image_url(
            "https://example.invalid/result.png",
        )],
    );
    assert_eq!(
        spill_tool_output(&context, &run_id, structured.clone())
            .await
            .expect("structured output remains valid"),
        structured
    );
}

#[test]
fn published_inference_controls_reach_each_model_call_unchanged() {
    // Causal graph: resolved Agent snapshot -> request builder -> provider-facing
    // ChatRequest. The route identity and call controls travel together but remain
    // separate typed axes.
    //
    // Decision table:
    // | snapshot effort | snapshot speed | request effort | request speed |
    // | xhigh           | fast           | xhigh          | fast          |
    // | omitted         | omitted        | omitted        | omitted       |
    use awaken_runtime_contract::agent_bindings::{
        InferenceOptions, InferenceSpeed, ReasoningEffort,
    };

    let mut controlled = spec("");
    controlled.plugin_config.inference = InferenceOptions {
        effort: Some(ReasoningEffort::Xhigh),
        speed: Some(InferenceSpeed::Fast),
    };
    let request = build_chat_request(
        &controlled,
        &[],
        &[user_message()],
        &[],
        &Default::default(),
    );
    assert_eq!(request.inference, controlled.plugin_config.inference);

    let defaults = build_chat_request(&spec(""), &[], &[user_message()], &[], &Default::default());
    assert_eq!(defaults.inference, InferenceOptions::default());
}

#[test]
fn toolset_policy_changes_the_actual_model_tool_surface() {
    // Cause graph: exact published/session toolset policy -> the one request
    // assembly seam for static + dynamic tools -> provider-facing ChatRequest.
    // Disabled tools must disappear; permission is deliberately enforced later
    // by the gate and must not change visibility by itself.
    //
    // Decision table:
    // | source | enabled | permission    | model-visible |
    // | Agent | false   | always_allow  | no            |
    // | Agent | true    | always_ask    | yes           |
    // | MCP   | false   | always_allow  | no            |
    // | MCP   | true    | always_allow  | yes           |
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let descriptor = |id: &str| {
        awaken_runtime_contract::resolved::ToolDescriptor::pinned(
            "test",
            id,
            "behavior probe",
            serde_json::json!({"type": "object"}),
        )
    };
    let mut configured = spec("");
    configured.tool_descriptors = vec![descriptor("alpha"), descriptor("omega")];
    configured.plugin_config.agent.toolsets = vec![
        ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![
                ToolPolicyOverride {
                    name: "alpha".into(),
                    policy: ToolExecutionPolicy {
                        enabled: false,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                },
                ToolPolicyOverride {
                    name: "omega".into(),
                    policy: ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                },
            ],
        },
        ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "docs".into(),
            },
            default: ToolExecutionPolicy::default(),
            overrides: vec![ToolPolicyOverride {
                name: "search".into(),
                policy: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
            }],
        },
    ];
    let dynamic = vec![
        descriptor("mcp__docs__search"),
        descriptor("mcp__docs__fetch"),
    ];

    let request = build_chat_request(
        &configured,
        &[],
        &[user_message()],
        &dynamic,
        &Default::default(),
    );
    let visible = request
        .tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(visible, vec!["omega", "mcp__docs__fetch"]);
}

fn numbered(n: usize) -> Message {
    Message::text(MessageId(format!("m{n}")), Role::User, n.to_string())
}

fn spec_with(policy: ContextPolicy) -> ResolvedSpec {
    ResolvedSpec {
        model_candidates: Vec::new(),
        context_policy: policy,
        ..spec("sys")
    }
}

fn user_texts(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}

#[test]
fn keep_all_sends_the_whole_transcript() {
    let transcript: Vec<Message> = (0..5).map(numbered).collect();
    let request = build_chat_request(
        &spec_with(ContextPolicy::KeepAll),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    // 1 system + 5 users
    assert_eq!(request.messages.len(), 6);
}

#[test]
fn keep_last_keeps_system_prefix_plus_the_last_n() {
    let transcript: Vec<Message> = (0..5).map(numbered).collect();
    let request = build_chat_request(
        &spec_with(ContextPolicy::KeepLast { keep_last: 2 }),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    // system stays; only the last 2 user messages survive.
    assert!(matches!(request.messages[0].role, Role::System));
    assert_eq!(user_texts(&request), vec!["3".to_string(), "4".to_string()]);
}

#[test]
fn keep_last_larger_than_history_keeps_everything() {
    let transcript: Vec<Message> = (0..3).map(numbered).collect();
    let request = build_chat_request(
        &spec_with(ContextPolicy::KeepLast { keep_last: 10 }),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    assert_eq!(user_texts(&request).len(), 3);
}

#[test]
fn keep_last_zero_keeps_only_the_system_prefix() {
    let transcript: Vec<Message> = (0..3).map(numbered).collect();
    let request = build_chat_request(
        &spec_with(ContextPolicy::KeepLast { keep_last: 0 }),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    assert_eq!(request.messages.len(), 1);
    assert!(matches!(request.messages[0].role, Role::System));
}

#[test]
fn keep_last_preserves_a_system_message_regardless_of_position() {
    // The positional invariant: KeepLast keeps EVERY system message wherever it
    // sits (agent instructions, an injected compaction summary), dropping only the
    // oldest conversational messages. A system message placed early in the stream must
    // survive a drop that removes older user messages around it.
    let transcript = vec![
        numbered(0),
        Message::text(MessageId("s-mid".to_string()), Role::System, "pinned"),
        numbered(1),
        numbered(2),
    ];
    let request = build_chat_request(
        &spec_with(ContextPolicy::KeepLast { keep_last: 1 }),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    // Oldest conversational messages "0" and "1" drop; only the last one survives.
    assert_eq!(user_texts(&request), vec!["2".to_string()]);
    // Both system messages survive: the instruction prefix and the mid-list one.
    let system_texts: Vec<String> = request
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect();
    assert!(
        system_texts.contains(&"pinned".to_string()),
        "a mid-list system message survives the KeepLast drop, got {system_texts:?}"
    );
}

fn assistant_with_tools(message_id: &str, calls: &[(&str, &str)]) -> Message {
    Message {
        id: MessageId(message_id.to_string()),
        role: Role::Assistant,
        content: calls
            .iter()
            .map(|(id, name)| ContentBlock::tool_use(*id, *name, serde_json::json!({})))
            .collect(),
    }
}

fn results(message_id: &str, call_ids: &[&str]) -> Message {
    Message {
        id: MessageId(message_id.to_string()),
        role: Role::Tool,
        content: call_ids
            .iter()
            .map(|id| ContentBlock::tool_result(*id, vec![ContentBlock::text("ok")]))
            .collect(),
    }
}

fn request_tool_ids(request: &ChatRequest) -> (Vec<String>, Vec<String>) {
    let uses = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let results = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();
    (uses, results)
}

#[test]
fn keep_last_never_exposes_half_of_a_tool_round() {
    // CE-CP5/CP6 decision table:
    // N=2 retains the complete use/result round; N=1 cuts before the use and
    // therefore drops the orphan result instead of exceeding the hard limit.
    let transcript = vec![
        numbered(0),
        assistant_with_tools("a1", &[("c1", "tool-a")]),
        results("t1", &["c1"]),
    ];
    let pair = build_chat_request(
        &spec_with(ContextPolicy::KeepLast { keep_last: 2 }),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    assert_eq!(
        request_tool_ids(&pair),
        (vec!["c1".into()], vec!["c1".into()])
    );

    let split = build_chat_request(
        &spec_with(ContextPolicy::KeepLast { keep_last: 1 }),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    assert_eq!(request_tool_ids(&split), (Vec::new(), Vec::new()));
    assert_eq!(
        split
            .messages
            .iter()
            .filter(|message| message.role != Role::System)
            .count(),
        0
    );
}

#[test]
fn request_pairing_is_local_for_multiple_partial_and_reused_calls() {
    // CE-CP7/CP8/CP9: c2 has no local result and the second occurrence of c1
    // cannot borrow the first round's result. Orphan/duplicate results are
    // removed; the first complete occurrence stays paired.
    let transcript = vec![
        assistant_with_tools("a1", &[("c1", "tool-a"), ("c2", "tool-b")]),
        results("t1", &["c1", "ghost", "c1"]),
        assistant_with_tools("a2", &[("c1", "read-again")]),
    ];
    let request = build_chat_request(
        &spec_with(ContextPolicy::KeepAll),
        &[],
        &transcript,
        &[],
        &Default::default(),
    );
    assert_eq!(
        request_tool_ids(&request),
        (vec!["c1".into()], vec!["c1".into()])
    );
}

#[test]
fn every_keep_last_cut_preserves_systems_hard_limit_and_tool_pairing() {
    // CE-CP1..CP10 exhaustive bounded expansion: for every cut of a transcript
    // containing a two-call round, all system messages survive, conversational
    // count stays <= N, and uses/results remain occurrence-paired.
    let transcript = vec![
        numbered(0),
        Message::text(MessageId("s-mid".into()), Role::System, "pinned"),
        assistant_with_tools("a1", &[("c1", "tool-a"), ("c2", "tool-b")]),
        results("t1", &["c1"]),
        results("t2", &["c2"]),
        numbered(1),
    ];
    for keep_last in 0..=6 {
        let request = build_chat_request(
            &spec_with(ContextPolicy::KeepLast { keep_last }),
            &[],
            &transcript,
            &[],
            &Default::default(),
        );
        assert!(
            request
                .messages
                .iter()
                .filter(|message| message.role != Role::System)
                .count()
                <= keep_last
        );
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message.role == Role::System)
                .count(),
            2
        );
        let (uses, results) = request_tool_ids(&request);
        assert_eq!(uses, results, "keep_last={keep_last}: {request:?}");
    }
}

#[test]
fn prelude_is_injected_after_instructions_before_the_transcript() {
    let prelude = vec![Message::text(
        MessageId("p1".to_string()),
        Role::System,
        "recalled context",
    )];
    let transcript = vec![user_message()];
    let request = build_chat_request(
        &spec("be helpful"),
        &prelude,
        &transcript,
        &[],
        &Default::default(),
    );
    assert_eq!(request.messages.len(), 3);
    assert!(matches!(request.messages[0].role, Role::System));
    assert_eq!(
        request.messages[0].content,
        vec![ContentBlock::text("be helpful")]
    );
    assert!(matches!(request.messages[1].role, Role::System));
    assert_eq!(
        request.messages[1].content,
        vec![ContentBlock::text("recalled context")]
    );
    assert!(matches!(request.messages[2].role, Role::User));
}

// --- mid-stream interruption recovery (R1–R3) + durable checkpoints ---

use awaken_runtime_contract::llm::{Error as LlmError, LlmExecutor, Result as LlmResult};
use std::sync::Arc;

/// Records every text chunk the live stream received, so a test can assert a
/// continued step never re-emits its already-streamed prefix.
struct RecordingSink {
    chunks: std::sync::Mutex<Vec<String>>,
}
#[async_trait]
impl DeltaSink for RecordingSink {
    async fn on_text(&self, chunk: &str) {
        self.chunks.lock().unwrap().push(chunk.to_string());
    }
}

/// Drops the first attempt with a retryable error after streaming a partial
/// (text and/or one tool call whose raw argument text is `first_tool.2`), then
/// succeeds on any retry — capturing every request it received.
struct FlakyStreamLlm {
    requests: std::sync::Mutex<Vec<ChatRequest>>,
    first_partial: &'static str,
    first_tool: Option<(&'static str, &'static str, &'static str)>,
    continuation: &'static str,
}
#[async_trait]
impl LlmExecutor for FlakyStreamLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("continuation path is streaming-only")
    }
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> LlmResult<ChatResponse> {
        let n = {
            let mut reqs = self.requests.lock().unwrap();
            reqs.push(request);
            reqs.len() - 1
        };
        if n == 0 {
            if !self.first_partial.is_empty() {
                sink.on_text(self.first_partial).await;
            }
            if let Some((call_id, tool_id, raw)) = self.first_tool {
                // Mid-stream, a provider hands a de-accumulated arg-fragment.
                sink.on_tool_call_delta(call_id, tool_id, raw).await;
            }
            return Err(LlmError::Timeout("connection reset".to_string()));
        }
        sink.on_text(self.continuation).await;
        Ok(ChatResponse {
            output: AssistantOutput::text(self.continuation),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Succeeds on every call, streaming a fixed text — used to prove a resumed
/// call continues from the recovered prefix (R1 resume).
struct SucceedingLlm {
    requests: std::sync::Mutex<Vec<ChatRequest>>,
    text: &'static str,
}
#[async_trait]
impl LlmExecutor for SucceedingLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("streaming-only")
    }
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> LlmResult<ChatResponse> {
        self.requests.lock().unwrap().push(request);
        sink.on_text(self.text).await;
        Ok(ChatResponse {
            output: AssistantOutput::text(self.text),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Fails on every call — used to prove a persisted partial resumes without the
/// provider ever running (R2 resume).
struct NeverCalledLlm {
    calls: std::sync::Mutex<usize>,
}

/// Never yields a stream item or a terminal response, modeling a provider
/// connection that stays open forever while the host heartbeat remains healthy.
struct HangingStreamLlm;
#[async_trait]
impl LlmExecutor for HangingStreamLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("streaming-only")
    }

    async fn infer_streaming(
        &self,
        _request: ChatRequest,
        _sink: &dyn DeltaSink,
    ) -> LlmResult<ChatResponse> {
        std::future::pending().await
    }
}
#[async_trait]
impl LlmExecutor for NeverCalledLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!()
    }
    async fn infer_streaming(
        &self,
        _request: ChatRequest,
        _sink: &dyn DeltaSink,
    ) -> LlmResult<ChatResponse> {
        *self.calls.lock().unwrap() += 1;
        Err(LlmError::Timeout("should not be called".to_string()))
    }
}

/// A `StreamCheckpointStore` that records every `put`/`delete` for assertions
/// while serving `get` from a live map.
#[derive(Default)]
struct SpyCheckpointStore {
    map: std::sync::Mutex<std::collections::HashMap<String, StreamCheckpoint>>,
    puts: std::sync::Mutex<Vec<StreamCheckpoint>>,
    deletes: std::sync::Mutex<Vec<String>>,
}
#[async_trait]
impl StreamCheckpointStore for SpyCheckpointStore {
    async fn get(&self, run_id: &str) -> Option<StreamCheckpoint> {
        self.map.lock().unwrap().get(run_id).cloned()
    }
    async fn put(&self, checkpoint: StreamCheckpoint) {
        self.puts.lock().unwrap().push(checkpoint.clone());
        self.map
            .lock()
            .unwrap()
            .insert(checkpoint.run_id.clone(), checkpoint);
    }
    async fn delete(&self, run_id: &str) {
        self.deletes.lock().unwrap().push(run_id.to_string());
        self.map.lock().unwrap().remove(run_id);
    }
}

fn one_step_request() -> ChatRequest {
    ChatRequest {
        model_binding: ModelBinding {
            provider_identity_ref: "p".to_string(),
            model_ref: "m".to_string(),
            backend_ref: "b".to_string(),
        },
        inference: Default::default(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::text("q")],
        }],
        tools: Vec::new(),
    }
}
fn policy(max_retries: usize) -> crate::retry::LlmRetryPolicy {
    crate::retry::LlmRetryPolicy {
        max_retries,
        backoff_base_ms: 0,
        overloaded_backoff_base_ms: 0,
        attempt_timeout: std::time::Duration::from_secs(5),
    }
}
fn assistant_prefixes(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}
fn recording() -> RecordingSink {
    RecordingSink {
        chunks: Default::default(),
    }
}

#[test]
fn content_gate_is_closed_by_default_and_open_at_full() {
    use awaken_runtime_contract::{CaptureDecision, ContentCapture, ContentKind};
    // Default decision (Structured) records nothing...
    let rendered = super::content::render_chat_messages(&one_step_request().messages);
    assert!(
        CaptureDecision::default()
            .content(ContentKind::InputMessages, &rendered)
            .is_none()
    );
    // ...Full records the rendered (redactor-scrubbed) content.
    assert_eq!(
        CaptureDecision::new(ContentCapture::Full)
            .content(ContentKind::InputMessages, &rendered)
            .as_deref(),
        Some(rendered.as_str())
    );
}

#[tokio::test]
async fn a_provider_stream_has_a_total_attempt_deadline() {
    let llm: Arc<dyn LlmExecutor> = Arc::new(HangingStreamLlm);
    let mut timeout_policy = policy(0);
    timeout_policy.attempt_timeout = std::time::Duration::from_millis(10);
    let error = infer_with_retry(
        &llm,
        one_step_request(),
        &timeout_policy,
        &crate::circuit_breaker::CircuitBreaker::default(),
        &recording(),
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, LlmError::Timeout(message) if message.contains("10ms")));
}

#[tokio::test]
async fn interrupted_text_stream_is_continued_from_the_partial() {
    let flaky = Arc::new(FlakyStreamLlm {
        requests: Default::default(),
        first_partial: "The ans",
        first_tool: None,
        continuation: "wer is 42",
    });
    let llm: Arc<dyn LlmExecutor> = flaky.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();
    // The transient-retry counter the host reads to surface session.status_rescheduled.
    let reschedules = Arc::new(std::sync::atomic::AtomicU32::new(0));

    let response = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(2),
        &breaker,
        &sink,
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        Some(&reschedules),
    )
    .await
    .expect("continues past the drop");

    // The committed step is the whole text: prefix + continuation, stitched once.
    assert_eq!(response.output.text_content(), "The answer is 42");
    // The single transparent retry was counted, so the host reports a reschedule.
    assert_eq!(
        reschedules.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "one transparent retry is counted for session.status_rescheduled"
    );

    let requests = flaky.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one drop, one successful retry");
    // The first attempt was a clean initial call — no injected prefix.
    assert!(assistant_prefixes(&requests[0]).is_empty());
    // The retry carried the confirmed partial as an assistant prefix ...
    assert_eq!(
        assistant_prefixes(&requests[1]),
        vec!["The ans".to_string()]
    );
    // ... followed by a continuation prompt as the final user message.
    let last = requests[1].messages.last().unwrap();
    assert!(matches!(last.role, Role::User));
    assert!(
        matches!(&last.content[0], ContentBlock::Text { text } if text.contains("interrupted"))
    );

    // The live stream saw each fragment exactly once: the prefix is not
    // re-emitted on the retry.
    assert_eq!(
        *sink.chunks.lock().unwrap(),
        vec!["The ans".to_string(), "wer is 42".to_string()]
    );
}

#[tokio::test]
async fn completed_tool_calls_before_a_drop_are_executed_without_re_inferring() {
    // R2: the model finished a tool call (its args parse) before the drop.
    let flaky = Arc::new(FlakyStreamLlm {
        requests: Default::default(),
        first_partial: "Let me search ",
        first_tool: Some(("c1", "search", r#"{"q":"rust"}"#)),
        continuation: "unused",
    });
    let llm: Arc<dyn LlmExecutor> = flaky.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();

    let response = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(2),
        &breaker,
        &sink,
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await
    .expect("salvages the completed tool call");

    // The salvaged step stops for tool use, carrying the text and the parsed call.
    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(response.output.text_content(), "Let me search ");
    let calls = response.output.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_id, "search");
    assert_eq!(calls[0].arguments, serde_json::json!({ "q": "rust" }));
    // No re-inference: the provider ran exactly once.
    assert_eq!(flaky.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn completed_tool_calls_are_salvaged_even_when_the_retry_budget_is_spent() {
    // R2 preempts budget exhaustion: a mid-stream drop that left a COMPLETE tool
    // call (its args parse) executes the salvaged call without re-inference, even
    // though `max_retries == 0` leaves no budget for a text continuation. The
    // completed-tools short-circuit (inference.rs B8) sits before the budget gate.
    let flaky = Arc::new(FlakyStreamLlm {
        requests: Default::default(),
        first_partial: "searching ",
        first_tool: Some(("c1", "search", r#"{"q":"rust"}"#)),
        continuation: "unused",
    });
    let llm: Arc<dyn LlmExecutor> = flaky.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();

    let response = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(0),
        &breaker,
        &sink,
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await
    .expect("salvages the completed tool call despite a spent budget");

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    let calls = response.output.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, serde_json::json!({ "q": "rust" }));
    // The salvage short-circuits before any retry: the provider ran exactly once.
    assert_eq!(flaky.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_in_flight_tool_call_is_dropped_and_the_text_continues() {
    // R3: text plus a tool whose args were still streaming (unparseable).
    let flaky = Arc::new(FlakyStreamLlm {
        requests: Default::default(),
        first_partial: "Calling ",
        first_tool: Some(("c1", "search", r#"{"q":"ru"#)),
        continuation: "done",
    });
    let llm: Arc<dyn LlmExecutor> = flaky.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();

    let response = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(2),
        &breaker,
        &sink,
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await
    .expect("continues the text past the in-flight tool");

    // The in-flight tool is dropped; the text continues from its prefix.
    assert_eq!(response.output.text_content(), "Calling done");
    assert!(response.output.tool_calls().is_empty());
    let requests = flaky.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        assistant_prefixes(&requests[1]),
        vec!["Calling ".to_string()]
    );
}

/// Test design for the inference checkpoint boundary.
/// Causes: C1=stream drops after a partial, C2=no retry budget,
/// C3=checkpoint store configured. Effects: E1=partial checkpoint is written,
/// E2=normal function return clears it. Rule R1: C1+C2+C3 -> E1+E2; the
/// following persisted-partial tests cover crash-before-clear recovery.
#[tokio::test]
async fn the_interruption_boundary_flushes_a_checkpoint_then_clears_it_on_return() {
    // A text-only drop with no retry budget: the boundary flush persists the
    // partial, and the unconditional delete-on-return clears it (the run
    // concluded in-process, so nothing to resume).
    let flaky = Arc::new(FlakyStreamLlm {
        requests: Default::default(),
        first_partial: "half ",
        first_tool: None,
        continuation: "unused",
    });
    let llm: Arc<dyn LlmExecutor> = flaky.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();
    let store = SpyCheckpointStore::default();
    let ctx = CheckpointCtx {
        store: &store,
        run_id: "run-x".to_string(),
        thread_id: "thread-x".to_string(),
        model: "m".to_string(),
    };

    let result = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(0),
        &breaker,
        &sink,
        Some(&ctx),
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await;
    assert!(result.is_err(), "no retry budget: the drop stands");

    // The boundary flushed the whole partial, keyed by the run.
    {
        let puts = store.puts.lock().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].run_id, "run-x");
        assert_eq!(puts[0].partial_text, "half ");
        assert!(puts[0].partial_tools.is_empty());
        // It was cleared on return — it survives only a crash before that point.
        assert_eq!(*store.deletes.lock().unwrap(), vec!["run-x".to_string()]);
    }
    assert!(store.get("run-x").await.is_none());
}

#[tokio::test]
async fn a_persisted_text_partial_resumes_in_a_fresh_call() {
    // Cross-process R1: a checkpoint left by a crash mid-recovery seeds the
    // next call, which continues from the partial rather than restarting.
    let succeeding = Arc::new(SucceedingLlm {
        requests: Default::default(),
        text: "world",
    });
    let llm: Arc<dyn LlmExecutor> = succeeding.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();
    let resume = StreamCheckpoint {
        run_id: "run-x".to_string(),
        thread_id: "thread-x".to_string(),
        model: "m".to_string(),
        partial_text: "Hello ".to_string(),
        partial_tools: Vec::new(),
    };

    let response = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(2),
        &breaker,
        &sink,
        None,
        Some(resume),
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await
    .expect("resumes from the persisted partial");

    assert_eq!(response.output.text_content(), "Hello world");
    let requests = succeeding.requests.lock().unwrap();
    // The very first call already carried the recovered prefix + continuation.
    assert_eq!(requests.len(), 1);
    assert_eq!(assistant_prefixes(&requests[0]), vec!["Hello ".to_string()]);
}

#[tokio::test]
async fn a_persisted_completed_tool_call_resumes_without_calling_the_model() {
    // Cross-process R2: the crash happened after the tool call completed, so
    // resume executes it directly — the provider is never invoked.
    let llm_impl = Arc::new(NeverCalledLlm {
        calls: Default::default(),
    });
    let llm: Arc<dyn LlmExecutor> = llm_impl.clone();
    let breaker = crate::circuit_breaker::CircuitBreaker::default();
    let sink = recording();
    let resume = StreamCheckpoint {
        run_id: "run-x".to_string(),
        thread_id: "thread-x".to_string(),
        model: "m".to_string(),
        partial_text: String::new(),
        partial_tools: vec![PartialToolCall {
            call_id: "c1".to_string(),
            tool_id: "search".to_string(),
            raw_arguments: r#"{"q":"rust"}"#.to_string(),
        }],
    };

    let response = infer_with_retry(
        &llm,
        one_step_request(),
        &policy(2),
        &breaker,
        &sink,
        None,
        Some(resume),
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await
    .expect("resumes the completed tool call");

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    let calls = response.output.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, serde_json::json!({ "q": "rust" }));
    assert_eq!(
        *llm_impl.calls.lock().unwrap(),
        0,
        "the model was not called"
    );
}

#[test]
fn merge_thread_usage_fails_closed_and_does_not_reset_a_drifted_tally() {
    // Audit #96: the delegate-rollup path must not silently reset a persisted tally
    // it cannot read. A drifted `__usage` cell → skip the rollup, leave the cell
    // untouched (never overwrite it with a fresh, partial tally).
    use awaken_agent_contract::agent::state::{Key, MergePolicy};
    use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage, TokenUsage};

    let mut store = Store::new();
    let corrupt = serde_json::json!("corrupt-not-a-usage");
    store.apply(&StateCommand::set(
        Scope::Thread,
        MergePolicy::Commutative,
        THREAD_USAGE_STATE_KEY,
        corrupt.clone(),
    ));

    // A non-empty delta that WOULD merge if the cell were readable.
    let mut delta = ThreadUsage::default();
    delta.record(
        "m",
        TokenUsage {
            prompt_tokens: 7,
            completion_tokens: 3,
            ..Default::default()
        },
    );

    let mut staged: Vec<StateCommand> = Vec::new();
    merge_thread_usage(&mut store, &mut staged, &delta);

    assert!(
        staged.is_empty(),
        "a drifted cell must not stage a usage rollup"
    );
    assert_eq!(
        store.get(Scope::Thread, &Key(THREAD_USAGE_STATE_KEY.to_string())),
        Some(&corrupt),
        "the drifted usage cell is left untouched, never reset"
    );
}

#[tokio::test]
async fn attempt_resume_without_committed_history_fails_closed() {
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("snapshot-1")
        .fingerprint("fingerprint-1")
        .build();
    let activation = RunActivation::new(
        RunId("run-1".into()),
        ThreadId("thread-1".into()),
        snapshot.clone(),
        Vec::new(),
    );
    let command = ResumeCommand {
        correlation_id: "ticket-1".into(),
        run_id: activation.run_id.clone(),
        thread_id: activation.thread_id.clone(),
        snapshot_id: snapshot.id,
        catalog_fingerprint: snapshot.fingerprint,
        result: ResumeResult::allow(),
        now_ms: 0,
    };

    let error = <Runtime as RunAttemptExecutor>::resume(
        &Runtime::new(),
        activation,
        command,
        RuntimeRunContext::new(),
    )
    .await
    .expect_err("resume requires the authoritative committed history reader");

    assert_eq!(
        error.to_string(),
        "runtime execution failed: RunAttemptExecutor::resume requires committed history"
    );
}
