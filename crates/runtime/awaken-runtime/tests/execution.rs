//! The execution loop runs one model step, commits durable facts, and streams
//! live progress that is independent of committed truth (G1/G13).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ThreadUsage, TokenUsage, ToolCall,
};
use awaken_runtime_contract::resolved::{
    ADVISOR_TOOL_ID, CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedModelCandidate,
    ResolvedSpec, ToolDescriptor, ToolKind,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink, replay_latest_state};

/// A deterministic provider that always answers with fixed text.
struct TextLlm(&'static str);

struct AdvisorThenTextLlm {
    primary_calls: AtomicUsize,
}

struct UnavailableAdvisorThenTextLlm {
    primary_calls: AtomicUsize,
    fail_provider: bool,
}

#[async_trait::async_trait]
impl LlmExecutor for AdvisorThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if request.model_binding.model_ref == "advisor-model" {
            assert!(request.tools.is_empty(), "A1");
            return Ok(ChatResponse {
                output: AssistantOutput::text("independent advice"),
                usage: Some(TokenUsage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    ..Default::default()
                }),
                stop_reason: None,
            });
        }
        let output = if self.primary_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "advisor-call".into(),
                tool_id: ADVISOR_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        } else {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.iter().any(|block| matches!(
                        block,
                        ContentBlock::ToolResult { content, .. }
                            if awaken_agent_contract::agent::content::extract_text(content)
                                == "independent advice"
                    ))),
                "A2"
            );
            AssistantOutput::text("primary done")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for UnavailableAdvisorThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if request.model_binding.model_ref == "advisor-model" && self.fail_provider {
            return Err(awaken_runtime_contract::llm::Error::Provider(
                "advisor outage".into(),
            ));
        }
        let output = if self.primary_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "advisor-call".into(),
                tool_id: ADVISOR_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        } else {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.iter().any(
                        |block| matches!(block, ContentBlock::ToolResult { content, .. }
                        if awaken_agent_contract::agent::content::extract_text(content)
                            == "Advisor consultation unavailable.")
                    )),
                "A2/A3"
            );
            AssistantOutput::text("primary survived")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0.to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

fn activation(fingerprint: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "provider-1".to_string(),
                        model_ref: "model-1".to_string(),
                        backend_ref: "backend-1".to_string(),
                    },
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("message-1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hello")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

fn advisor_activation(with_candidate: bool) -> RunActivation {
    let mut activation = activation("catalog-a");
    activation.snapshot.resolved_spec.tool_descriptors = vec![
        ToolDescriptor::pinned(
            "managed:advisor",
            ADVISOR_TOOL_ID,
            "consult",
            serde_json::json!({"type":"object"}),
        )
        .with_kind(ToolKind::Advisor),
    ];
    if with_candidate {
        activation
            .snapshot
            .resolved_spec
            .plugin_config
            .agent
            .advisor = Some(
            awaken_runtime_contract::agent_bindings::AgentAdvisorBinding {
                model: "claude-opus-5".into(),
                candidate: ResolvedModelCandidate::host(ModelBinding::new(
                    "provider-1",
                    "advisor-model",
                    "backend-1",
                )),
            },
        );
    }
    activation
}

/// Test design for the split engine critical path.
/// Causes: C1=fresh Execute, C2=deterministic text response, C3=commit and live
/// sink configured. Effects: E1=NaturalEnd, E2=input/reply and state facts are
/// committed, E3=live deltas remain separate from replay truth. Rule R1:
/// C1+C2+C3 -> E1+E2+E3. This crosses run_commands, run_loop, inference,
/// progress, and finalize without relying on module-local implementation detail.
#[tokio::test]
async fn one_model_step_commits_facts_and_streams_progress() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi there")));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let outcome = runtime
        .execute(activation("catalog-a"), context)
        .await
        .expect("run executes");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    // Committed truth: the input commits at the first step boundary (under a
    // Running fact), the assistant reply with the terminal fact. The user
    // input is committed so the next Step sees it.
    assert_eq!(commit.commit_count(), 2);
    let committed = commit.committed();
    assert_eq!(committed.messages.len(), 2);
    assert_eq!(committed.messages[0].text_content(), "hello");
    assert_eq!(committed.messages[0].role, Role::User);
    assert_eq!(committed.messages[1].text_content(), "hi there");
    assert_eq!(committed.messages[1].role, Role::Assistant);

    // Replay reads committed facts, not the live stream.
    assert_eq!(
        replay_latest_state(&committed, &RunId("run-1".to_string())),
        Some(RunState::Ended(EndCause::NaturalEnd))
    );
    // Two state events: the transition into Running, then the terminal.
    assert_eq!(committed.events.len(), 2);

    // Live stream order is RunStarted -> OutputText -> RunFinished.
    let kinds = sink.events();
    assert!(matches!(kinds[0].kind, AgentEvent::Fact(Fact::RunStarted)));
    assert!(matches!(
        kinds[1].kind,
        AgentEvent::Delta(Delta::TextDelta { .. })
    ));
    assert!(matches!(
        kinds[2].kind,
        AgentEvent::Fact(Fact::RunFinished { .. })
    ));
}

#[tokio::test]
async fn execution_fails_closed_on_fingerprint_mismatch() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi")));
    let mut activation = activation("catalog-a");
    activation.snapshot.resolved_spec.catalog_fingerprint =
        CatalogFingerprint("catalog-b".to_string());

    let result = runtime.execute(activation, RuntimeRunContext::new()).await;
    assert!(matches!(result, Err(Error::Resolution(_))));
}

#[tokio::test]
async fn execution_does_not_require_an_installed_catalog() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi")));
    let outcome = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await
        .expect("snapshot is the execution authority");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn execution_requires_a_model_provider() {
    let runtime = Runtime::new();
    let result = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await;
    assert!(matches!(result, Err(Error::Execution(_))));
}

#[tokio::test]
async fn advisor_consultation_uses_exact_candidate_and_folds_usage_without_a_raw_tool() {
    // Cause/effect graph: a published advisor candidate plus a primary advisor
    // call causes an exact no-tools consultation; its text becomes the primary
    // tool result and its usage joins the Session tally.
    //
    // Decision table:
    // | Rule | candidate | advisor result | primary effect             |
    // | A1   | exact     | text + usage   | continue with advice+usage |
    let activation = advisor_activation(true);
    let runtime = Runtime::new().with_llm(Arc::new(AdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
    }));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let result = runtime
        .execute(
            activation,
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("A1");
    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "A1");
    let committed = commit.committed();
    assert_eq!(
        committed.messages.last().unwrap().text_content(),
        "primary done"
    );
    let usage = ThreadUsage::from_committed_state(&committed.state);
    assert_eq!(usage.by_model["advisor-model"].prompt_tokens, 7, "A1");
    assert_eq!(usage.by_model["advisor-model"].completion_tokens, 3, "A1");
}

#[tokio::test]
async fn advisor_unavailability_never_terminates_the_primary_turn() {
    // Cause/effect graph: missing publication binding, provider failure, or a
    // delegated child attempting the primary-only capability makes consultation
    // unavailable. Every effect is the same redacted error tool result; the
    // active model receives it and may complete normally.
    //
    // Decision table:
    // | Rule | candidate | provider | terminal effect       |
    // | A2   | absent    | n/a      | primary natural end   |
    // | A3   | exact     | error    | primary natural end   |
    // | A4   | exact     | child    | primary-only rejection|
    for (rule, candidate, fail_provider, child) in [
        ("A2", false, false, false),
        ("A3", true, true, false),
        ("A4", true, false, true),
    ] {
        let runtime = Runtime::new().with_llm(Arc::new(UnavailableAdvisorThenTextLlm {
            primary_calls: AtomicUsize::new(0),
            fail_provider,
        }));
        let mut activation = advisor_activation(candidate);
        if child {
            activation.delegation_origin = Some(
                awaken_agent_contract::agent::delegation::DelegationOrigin::root(
                    RunId("parent".into()),
                    "parent-call",
                ),
            );
        }
        let result = runtime
            .execute(activation, RuntimeRunContext::new())
            .await
            .expect(rule);
        assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "{rule}");
    }
}

#[tokio::test]
async fn run_without_commit_coordinator_still_completes() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("ok")));
    let outcome = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
}
