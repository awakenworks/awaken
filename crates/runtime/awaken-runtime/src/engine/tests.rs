use super::*;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointError;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::tool::{RawTool, ToolOutputSpiller};

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
        inference_geo: None,
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
    // | MCP server absent from normalized toolsets | - | no          |
    // | legacy publication (no toolsets) | -       | yes            |
    // Effects: only enabled, declared static/dynamic descriptors reach the model.
    // Constraints/invariants: visibility has one assembly seam; permission does
    // not change visibility and undeclared dynamic sources cannot leak.
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
                ToolPolicyOverride::new(
                    "alpha",
                    ToolExecutionPolicy {
                        enabled: false,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                ),
                ToolPolicyOverride::new(
                    "omega",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                ),
            ],
        },
        ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "docs".into(),
            },
            default: ToolExecutionPolicy::default(),
            overrides: vec![ToolPolicyOverride::new(
                "search",
                ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
            )],
        },
    ];
    let dynamic = vec![
        descriptor("mcp__docs__search"),
        descriptor("mcp__docs__fetch"),
        descriptor("mcp__browser__navigate"),
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

    // FMECA: Session-scoped dynamic plugins are live wiring shared by a Session;
    // without this explicit-source fence a delegated Agent can see its parent's
    // undeclared connector. A legacy snapshot with no typed toolsets keeps its
    // historical dynamic exact-id face rather than being silently narrowed.
    let legacy = build_chat_request(
        &spec(""),
        &[],
        &[user_message()],
        &[descriptor("mcp__browser__navigate")],
        &Default::default(),
    );
    assert_eq!(
        legacy
            .tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp__browser__navigate"]
    );
}

#[test]
fn provider_server_tool_requires_explicit_always_allow() {
    // Cause/effect decision table: R1 absent policy -> hidden; R2 AlwaysAsk ->
    // hidden because inference-side execution cannot pause for approval; R3
    // enabled AlwaysAllow -> visible exact server projection.
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    let tool = awaken_runtime_contract::resolved::ToolDescriptor::pinned(
        "builtin",
        "provider_server_probe",
        "Search",
        serde_json::json!({"type":"object"}),
    )
    .with_provider_server_tool("openrouter", "openrouter:web_search", serde_json::json!({}));
    let mut configured = spec("");
    configured.tool_descriptors = vec![tool];
    assert!(
        build_chat_request(
            &configured,
            &[],
            &[user_message()],
            &[],
            &Default::default()
        )
        .tools
        .is_empty(),
        "R1"
    );
    configured.plugin_config.agent.toolsets = vec![ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: vec![ToolPolicyOverride {
            name: "provider_server_probe".into(),
            policy: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
        }],
    }];
    assert!(
        build_chat_request(
            &configured,
            &[],
            &[user_message()],
            &[],
            &Default::default()
        )
        .tools
        .is_empty(),
        "R2"
    );
    configured.plugin_config.agent.toolsets[0].overrides[0]
        .policy
        .permission = ToolPermissionRequirement::AlwaysAllow;
    assert_eq!(
        build_chat_request(
            &configured,
            &[],
            &[user_message()],
            &[],
            &Default::default()
        )
        .tools
        .len(),
        1,
        "R3"
    );
}

#[test]
fn a_live_dynamic_tool_replaces_its_publication_selection_placeholder() {
    // Cause/effect graph: C1 a publication carries an enabled catalog descriptor;
    // C2 the selected plugin contributes the same canonical id and its live
    // executable descriptor. E1 the provider sees the id exactly once; E2 its
    // schema is the live plugin schema; E3 the ordinary enabled policy still
    // controls visibility. Constraint: plugin merge rejects two dynamic owners.
    //
    // Decision table:
    // | C1 static | C2 dynamic same id | enabled | provider face        |
    // | yes       | no                 | yes     | static once          |
    // | yes       | yes                | yes     | dynamic once (R1)    |
    // | yes       | yes                | no      | absent (R2)          |
    // FMECA: forwarding both copies is rejected by OpenAI-compatible providers;
    // preferring the frozen placeholder can expose a schema the live executor
    // does not implement. The convergence seam therefore gives the one live
    // DynamicTool ownership of that model-facing id.
    let placeholder = awaken_runtime_contract::resolved::ToolDescriptor::pinned(
        "catalog",
        "evidence_lookup",
        "publication placeholder",
        serde_json::json!({"type": "object", "properties": {}}),
    );
    let live = awaken_runtime_contract::resolved::ToolDescriptor::pinned(
        "plugin",
        "evidence_lookup",
        "configured provider search",
        serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
    );
    let mut configured = spec("");
    configured.tool_descriptors = vec![placeholder];

    let request = build_chat_request(
        &configured,
        &[],
        &[user_message()],
        std::slice::from_ref(&live),
        &Default::default(),
    );
    assert_eq!(request.tools, vec![live.clone()], "R1/E1+E2");

    configured.plugin_config.agent.toolsets = vec![
        awaken_runtime_contract::agent_bindings::ToolsetPolicy {
            source: awaken_runtime_contract::agent_bindings::ToolsetSource::Agent,
            default: Default::default(),
            overrides: vec![awaken_runtime_contract::agent_bindings::ToolPolicyOverride::new(
                "evidence_lookup",
                awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
                    enabled: false,
                    permission: awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAllow,
                },
            )],
        },
    ];
    let hidden = build_chat_request(
        &configured,
        &[],
        &[user_message()],
        &[live],
        &Default::default(),
    );
    assert!(hidden.tools.is_empty(), "R2/E3");
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
    // Test design — Causes: every KeepLast boundary 0..=6 cuts a transcript
    // containing pinned System context and a two-call/two-result round. Effects:
    // all System rows survive, conversational rows never exceed the requested
    // limit, and retained tool uses/results remain occurrence-paired.
    // Constraints/invariants: compaction cannot orphan either half of a tool
    // exchange or count System rows against the conversational hard limit.
    // Decision rationale: exhaustive bounded expansion covers every possible
    // cut of this minimal transcript and therefore CE-CP1..CP10 interactions.
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
fn system_context_between_tool_use_and_result_preserves_the_exact_pair() {
    // Cause/effect graph: C1 one assistant tool use is followed by C2 stable
    // System context and C3 its exact Tool result. Effects: E1 the System row is
    // retained, E2 the correlated use/result pair is retained, and E3 neither
    // half can be borrowed across a later conversational boundary.
    //
    // | Rule | System between use/result | matching result | Effect |
    // |---|---|---|---|
    // | R1 | yes | yes | E1 + E2 |
    // | R2 | yes | no | E1; incomplete use removed |
    // | R3 | yes | only after later User | E1 + E3; both halves removed |
    // Constraints/invariants: correlation cannot cross a conversational boundary
    // and System context is retained independently of an incomplete tool pair.
    let system = Message::text(MessageId("s-reply".into()), Role::System, "reply context");
    let exact = vec![
        assistant_with_tools("a1", &[("c1", "tool-a")]),
        system.clone(),
        results("t1", &["c1"]),
    ];
    let exact = build_chat_request(
        &spec_with(ContextPolicy::KeepAll),
        &[],
        &exact,
        &[],
        &Default::default(),
    );
    assert_eq!(
        request_tool_ids(&exact),
        (vec!["c1".into()], vec!["c1".into()])
    );
    assert!(
        exact
            .messages
            .iter()
            .any(|message| { message.role == Role::System && message.content == system.content })
    );

    let missing = vec![
        assistant_with_tools("a1", &[("c1", "tool-a")]),
        system.clone(),
    ];
    let missing = build_chat_request(
        &spec_with(ContextPolicy::KeepAll),
        &[],
        &missing,
        &[],
        &Default::default(),
    );
    assert_eq!(
        request_tool_ids(&missing),
        (Vec::new(), Vec::new()),
        "R2/E2"
    );
    assert!(
        missing
            .messages
            .iter()
            .any(|message| message.role == Role::System),
        "R2/E1"
    );

    let crossed = vec![
        assistant_with_tools("a1", &[("c1", "tool-a")]),
        system,
        numbered(7),
        results("t1", &["c1"]),
    ];
    let crossed = build_chat_request(
        &spec_with(ContextPolicy::KeepAll),
        &[],
        &crossed,
        &[],
        &Default::default(),
    );
    assert_eq!(
        request_tool_ids(&crossed),
        (Vec::new(), Vec::new()),
        "R3/E3"
    );
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

/// Claim authority that becomes stale at one configured verification. The
/// counter is process-local test instrumentation; production authority remains
/// the dispatch claim verifier carried by RuntimeRunContext.
struct SequencedOwnership {
    verifications: std::sync::atomic::AtomicUsize,
    lose_at: usize,
}

struct UnavailableOwnership;

#[async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for UnavailableOwnership {
    async fn verify_current(
        &self,
    ) -> std::result::Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        Err(awaken_runtime_contract::AttemptOwnershipError::Unavailable(
            "authority down".into(),
        ))
    }
}

struct CountingRawTool(std::sync::atomic::AtomicUsize);

#[async_trait]
impl RawTool for CountingRawTool {
    fn id(&self) -> &str {
        "ownership-probe"
    }

    async fn invoke(&self, call: ToolCall) -> std::result::Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutput::ok(&call.call_id, "executed"))
    }
}

struct CountingSpiller(std::sync::atomic::AtomicUsize);

#[async_trait]
impl ToolOutputSpiller for CountingSpiller {
    async fn spill(
        &self,
        _run_id: &RunId,
        _call_id: &str,
        _content: String,
    ) -> std::result::Result<String, ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("spilled".into())
    }
}

#[async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for SequencedOwnership {
    async fn verify_current(
        &self,
    ) -> std::result::Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        let current = self
            .verifications
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if current >= self.lose_at {
            Err(awaken_runtime_contract::AttemptOwnershipError::Lost)
        } else {
            Ok(())
        }
    }
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
    async fn get(
        &self,
        run_id: &str,
    ) -> std::result::Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        Ok(self.map.lock().unwrap().get(run_id).cloned())
    }
    async fn put(
        &self,
        checkpoint: StreamCheckpoint,
    ) -> std::result::Result<(), StreamCheckpointError> {
        self.puts.lock().unwrap().push(checkpoint.clone());
        self.map
            .lock()
            .unwrap()
            .insert(checkpoint.run_id.clone(), checkpoint);
        Ok(())
    }
    async fn delete(&self, run_id: &str) -> std::result::Result<(), StreamCheckpointError> {
        self.deletes.lock().unwrap().push(run_id.to_string());
        self.map.lock().unwrap().remove(run_id);
        Ok(())
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
    // Test design — Causes: a provider stream never produces a terminal response
    // and the attempt budget is 10ms. Effects: inference returns a Timeout naming
    // that budget. Constraints/invariants: one attempt cannot remain pending past
    // its configured deadline. Decision rule D1: hanging+10ms=>typed 10ms timeout.
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
    )
    .await
    .unwrap_err();
    assert!(matches!(error, LlmError::Timeout(message) if message.contains("10ms")));
}

#[tokio::test]
async fn logical_model_request_observations_cover_success_without_usage_and_failure() {
    use awaken_runtime_contract::llm::ModelRequestObservation;

    // Causes: C1 success/failure; C2 provider usage absent; C3 no transparent
    // retry. Effects: E1 exactly one observation per logical request; E2 the
    // failure bit follows the final result; E3 omitted usage remains zero.
    // Rules: R1=success+C2+C3=>E1+!E2+E3;
    // R2=failure+C2+C3=>E1+E2+E3.
    // Constraints/invariants: each logical request emits exactly one observation;
    // absent provider usage is zero rather than unknown parallel state.
    let success: Arc<dyn LlmExecutor> = Arc::new(SucceedingLlm {
        requests: Default::default(),
        text: "ok",
    });
    let success = infer_with_retry_observed(
        &success,
        one_step_request(),
        &policy(0),
        &crate::circuit_breaker::CircuitBreaker::default(),
        &recording(),
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        None,
    )
    .await;
    assert!(success.result.is_ok(), "R1/E1");
    assert_eq!(
        success.observation,
        ModelRequestObservation::default(),
        "R1/E2-E3"
    );

    let failure: Arc<dyn LlmExecutor> = Arc::new(HangingStreamLlm);
    let mut timeout_policy = policy(0);
    timeout_policy.attempt_timeout = std::time::Duration::from_millis(10);
    let failure = infer_with_retry_observed(
        &failure,
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
    .await;
    assert!(failure.result.is_err(), "R2/E1");
    assert!(failure.observation.is_error, "R2/E2");
    assert_eq!(failure.observation.usage, Default::default(), "R2/E3");
}

#[tokio::test]
async fn provider_attempts_fail_closed_when_live_ownership_is_lost() {
    // Provider-attempt ownership cause/effect table. C1 ownership is absent,
    // current, or lost; C2 the logical request is on its first attempt or a
    // transparent retry. E1 call provider; E2 fail Unauthorized before the
    // provider side effect. Rules: O1 absent/current+first => E1; O2
    // lost+first => E2 and zero calls; O3 current+first then lost+retry => the
    // first call occurs, but retry is fenced (one total call); O4 a synchronous
    // child inherits a lost parent authority => E2 and zero calls. Logical
    // request budget admission is intentionally orthogonal and remains outside
    // this per-attempt fence.
    //
    // | Rule | Context | Authority sequence | Provider calls |
    // | O1   | direct  | absent/current     | one            |
    // | O2   | direct  | lost               | zero           |
    // | O3   | direct  | current,lost       | one            |
    // | O4   | child   | lost parent        | zero           |
    // Constraints/invariants: ownership is verified immediately before every
    // provider attempt, and synchronous children share the parent verifier.
    let stale_provider = Arc::new(SucceedingLlm {
        requests: Default::default(),
        text: "must not run",
    });
    let stale_llm: Arc<dyn LlmExecutor> = stale_provider.clone();
    let stale = SequencedOwnership {
        verifications: Default::default(),
        lose_at: 0,
    };
    let error = infer_with_retry_observed(
        &stale_llm,
        one_step_request(),
        &policy(0),
        &crate::circuit_breaker::CircuitBreaker::default(),
        &recording(),
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        Some(&stale),
    )
    .await
    .result
    .expect_err("O2 stale claim fails closed");
    assert!(matches!(error, LlmError::Unauthorized(_)), "O2/E2");
    assert!(stale_provider.requests.lock().unwrap().is_empty(), "O2/E2");

    let child_provider = Arc::new(SucceedingLlm {
        requests: Default::default(),
        text: "must not run",
    });
    let child_llm: Arc<dyn LlmExecutor> = child_provider.clone();
    let child_context = RuntimeRunContext::new()
        .with_ownership(Arc::new(SequencedOwnership {
            verifications: Default::default(),
            lose_at: 0,
        }))
        .for_child_run();
    let error = infer_with_retry_observed(
        &child_llm,
        one_step_request(),
        &policy(0),
        &crate::circuit_breaker::CircuitBreaker::default(),
        &recording(),
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        child_context.ownership.as_deref(),
    )
    .await
    .result
    .expect_err("O4 child observes the parent's lost authority");
    assert!(matches!(error, LlmError::Unauthorized(_)), "O4/E2");
    assert!(child_provider.requests.lock().unwrap().is_empty(), "O4/E2");

    let retrying_provider = Arc::new(FlakyStreamLlm {
        requests: Default::default(),
        first_partial: "",
        first_tool: None,
        continuation: "must not run",
    });
    let retrying_llm: Arc<dyn LlmExecutor> = retrying_provider.clone();
    let loses_during_backoff = SequencedOwnership {
        verifications: Default::default(),
        lose_at: 1,
    };
    let error = infer_with_retry_observed(
        &retrying_llm,
        one_step_request(),
        &policy(2),
        &crate::circuit_breaker::CircuitBreaker::default(),
        &recording(),
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
        Some(&loses_during_backoff),
    )
    .await
    .result
    .expect_err("O3 retry observes lost claim");
    assert!(matches!(error, LlmError::Unauthorized(_)), "O3/E2");
    assert_eq!(
        retrying_provider.requests.lock().unwrap().len(),
        1,
        "O3/E1+E2"
    );
}

#[tokio::test]
async fn native_child_tool_effects_require_the_parent_attempt_authority() {
    // Cause/effect graph: C1=authority is absent/current/lost/down; C2=the
    // native tool runs directly or through a synchronous child context; C3=the
    // authority is lost after tool invocation but before output materialization.
    // Effects: E1=invoke exactly once; E2=zero tool calls and an attempt error;
    // E3=preserve the completed invocation but issue zero external spill calls.
    // The child must inherit the parent verifier because it has no independent
    // dispatch claim; a separately dispatched child receives its own verifier
    // from ingress.
    //
    // | Rule | Context       | Authority sequence | Effect |
    // | O1   | direct        | absent             | E1     |
    // | O2   | child         | current            | E1     |
    // | O3   | child         | lost/down          | E2     |
    // | O4   | child+spiller | current,lost       | E3     |
    // Constraints/invariants: every external effect rechecks the same attempt
    // authority; a completed tool result cannot authorize a later stale spill.
    let call = ToolCall {
        call_id: "tool-call".into(),
        tool_id: "ownership-probe".into(),
        arguments: serde_json::json!({}),
    };
    let run_id = RunId("ownership-run".into());
    let thread_id = ThreadId("ownership-thread".into());

    let absent_tool = Arc::new(CountingRawTool(Default::default()));
    execute_tool(
        &Runtime::new().with_tool(absent_tool.clone()),
        None,
        &call,
        &RuntimeRunContext::new(),
        &run_id,
        &thread_id,
        "operation-absent".into(),
    )
    .await
    .expect("O1 absent authority stays compatible");
    assert_eq!(
        absent_tool.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "O1/E1"
    );

    let current_tool = Arc::new(CountingRawTool(Default::default()));
    let current_child = RuntimeRunContext::new()
        .with_ownership(Arc::new(SequencedOwnership {
            verifications: Default::default(),
            lose_at: usize::MAX,
        }))
        .for_child_run();
    execute_tool(
        &Runtime::new().with_tool(current_tool.clone()),
        None,
        &call,
        &current_child,
        &run_id,
        &thread_id,
        "operation-current".into(),
    )
    .await
    .expect("O2 current parent authority permits the child effect");
    assert_eq!(
        current_tool.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "O2/E1"
    );

    for (label, authority) in [
        (
            "lost",
            Arc::new(SequencedOwnership {
                verifications: Default::default(),
                lose_at: 0,
            }) as Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
        ),
        (
            "down",
            Arc::new(UnavailableOwnership)
                as Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
        ),
    ] {
        let stale_tool = Arc::new(CountingRawTool(Default::default()));
        let stale_child = RuntimeRunContext::new()
            .with_ownership(authority)
            .for_child_run();
        execute_tool(
            &Runtime::new().with_tool(stale_tool.clone()),
            None,
            &call,
            &stale_child,
            &run_id,
            &thread_id,
            format!("operation-{label}"),
        )
        .await
        .expect_err("O3 stale parent authority fences the child tool");
        assert_eq!(
            stale_tool.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "O3/E2 {label}"
        );
    }

    let invoked_tool = Arc::new(CountingRawTool(Default::default()));
    let stale_spiller = Arc::new(CountingSpiller(Default::default()));
    let child = RuntimeRunContext::new()
        .with_tool_output_spiller(stale_spiller.clone())
        .with_ownership(Arc::new(SequencedOwnership {
            verifications: Default::default(),
            lose_at: 1,
        }))
        .for_child_run();
    let output = execute_tool(
        &Runtime::new().with_tool(invoked_tool.clone()),
        None,
        &call,
        &child,
        &run_id,
        &thread_id,
        "operation-spill".into(),
    )
    .await
    .expect("O4 tool starts while authority is current");
    spill_tool_output(&child, &run_id, output)
        .await
        .expect_err("O4 lost authority fences the later spill");
    assert_eq!(
        invoked_tool.0.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "O4/E3"
    );
    assert_eq!(
        stale_spiller.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "O4/E3"
    );
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
    let observed = infer_with_retry_observed(
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
    .await;
    let response = observed.result.expect("continues past the drop");

    // The committed step is the whole text: prefix + continuation, stitched once.
    assert_eq!(response.output.text_content(), "The answer is 42");
    // Cause P1: the first provider attempt drops and one transparent retry
    // succeeds. Effect E1: the one logical observation carries retry_count=1;
    // no process-local Host counter participates. Rule R1=P1=>E1.
    // Constraints/invariants: confirmed partial text is injected once and never
    // re-emitted; the retry belongs to the same logical request.
    assert_eq!(observed.observation.retry_count, 1, "R1/E1");
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
    // Test design — Causes: the model completes parseable tool arguments before
    // its stream drops. Effects: inference salvages text+ToolUse without another
    // provider call. Constraints/invariants: only complete JSON calls are
    // executable and salvage keeps the original call identity. Decision rule R2:
    // completed call+drop=>ToolUse response and provider count one.
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
    // Causes: R2 preempts budget exhaustion—a mid-stream drop left a COMPLETE tool
    // call (its args parse) executes the salvaged call without re-inference, even
    // though `max_retries == 0` leaves no budget for a text continuation. The
    // completed-tools short-circuit (inference.rs B8) sits before the budget gate.
    // Effects: the complete ToolUse succeeds with one provider call.
    // Constraints/invariants: retry budget governs re-inference, not recovery of
    // an already complete call. Decision rule R2b: complete+drop+zero budget=>salvage.
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
    // Test design — Causes: a drop leaves confirmed text plus unparseable in-flight
    // tool arguments. Effects: the tool is discarded and retry continues only
    // from text. Constraints/invariants: incomplete tool syntax is never executed
    // or replayed. Decision rule R3: partial tool+text=>two calls, stitched text,
    // zero committed ToolUse.
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
/// Constraints/invariants: the checkpoint is Run-scoped and survives only a
/// crash before the unconditional return-path deletion.
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
    assert!(store.get("run-x").await.expect("checkpoint read").is_none());
}

#[tokio::test]
async fn a_persisted_text_partial_resumes_in_a_fresh_call() {
    // Causes: C1 a checkpoint is left by a crash after one transparent retry
    // was scheduled; C2 a fresh process completes that same logical request.
    // Effects: E1 it continues from the partial rather than restarting; E2 the
    // final observation preserves retry_count=1. Rule R1=C1+C2=>E1+E2.
    // Constraints/invariants: recovery continues the same logical request,
    // injects the confirmed prefix once, and preserves the durable retry count.
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
        retry_count: 1,
    };

    let observed = infer_with_retry_observed(
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
    .await;
    let response = observed.result.expect("resumes from the persisted partial");

    assert_eq!(response.output.text_content(), "Hello world");
    assert_eq!(observed.observation.retry_count, 1, "R1/E2");
    let requests = succeeding.requests.lock().unwrap();
    // The very first call already carried the recovered prefix + continuation.
    assert_eq!(requests.len(), 1);
    assert_eq!(assistant_prefixes(&requests[0]), vec!["Hello ".to_string()]);
}

#[tokio::test]
async fn a_persisted_completed_tool_call_resumes_without_calling_the_model() {
    // Test design — Causes: a crash checkpoint contains one complete parseable
    // ToolUse. Effects: recovery returns that call with zero provider invocations.
    // Constraints/invariants: persisted complete tool truth outranks re-inference
    // and preserves id/name/arguments. Decision rule R2: complete checkpoint=>
    // direct ToolUse response, model-call count zero.
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
        retry_count: 0,
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
    // Test design — Causes: RunAttemptExecutor::resume has an exact activation
    // and command but no committed-history reader. Effects: it returns the typed
    // execution error before resuming. Constraints/invariants: durable history is
    // the sole recovery authority; inputs cannot reconstruct it. Decision rule
    // F1: missing reader=>fail closed with the pinned diagnostic.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("snapshot-1")
        .model(ModelBinding::new("test", "model", "native"))
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
        context_messages: Vec::new(),
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
