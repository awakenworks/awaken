//! The execution loop runs one model step, commits durable facts, and streams
//! live progress that is independent of committed truth (G1/G13).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::StateKey;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    RunDelegationService, RunDelegations,
};
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ModelRequestAdmission,
    ModelRequestAdmissionRequest, ModelRequestGate, ModelRequestObservation, StopReason,
    ThreadUsage, TokenUsage, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::{
    ADVISOR_FAILURE_NOTICE, ADVISOR_TOOL_ID, ADVISOR_UNAVAILABLE_NOTICE, CatalogFingerprint,
    ContextPolicy, ModelBinding, ResolvedModelCandidate, ResolvedSpec, ToolDescriptor, ToolKind,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink, replay_latest_state};

fn committed_model_requests(store: &MemoryCommitCoordinator) -> Vec<ModelRequestObservation> {
    store
        .committed()
        .events
        .iter()
        .filter_map(|record| ModelRequestObservation::from_record(record).transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("canonical model-request audit payload")
}

/// A deterministic provider that always answers with fixed text.
struct TextLlm(&'static str);

struct AdvisorThenTextLlm {
    primary_calls: AtomicUsize,
    expected_tool_result: &'static str,
}

struct FailedAdvisorThenTextLlm {
    primary_calls: AtomicUsize,
}

struct SuspendAdvisorGate;

#[async_trait::async_trait]
impl ToolGateHook for SuspendAdvisorGate {
    async fn gate(
        &self,
        call: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        assert_eq!(call.tool_id, ADVISOR_TOOL_ID, "D4/C1");
        GateOutcome::RequireConfirmation {
            correlation_id: "advisor-approval".into(),
        }
    }
}

struct SequencedModelRequestGate {
    decisions: std::sync::Mutex<std::collections::VecDeque<ModelRequestAdmission>>,
    requests: std::sync::Mutex<Vec<ModelRequestAdmissionRequest>>,
}

impl SequencedModelRequestGate {
    fn new(decisions: impl IntoIterator<Item = ModelRequestAdmission>) -> Self {
        Self {
            decisions: std::sync::Mutex::new(decisions.into_iter().collect()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequestAdmissionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ModelRequestGate for SequencedModelRequestGate {
    async fn admit_model_request(
        &self,
        request: ModelRequestAdmissionRequest,
    ) -> Result<ModelRequestAdmission, String> {
        self.requests.lock().unwrap().push(request);
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "test gate received an unexpected logical model request".into())
    }
}

struct MaxTokensThenTextLlm(AtomicUsize);

#[async_trait::async_trait]
impl LlmExecutor for MaxTokensThenTextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let call = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::text(if call == 0 {
                "completed request at the cap"
            } else {
                "must not be sampled"
            }),
            usage: Some(TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                ..Default::default()
            }),
            stop_reason: Some(if call == 0 {
                StopReason::MaxTokens
            } else {
                StopReason::NaturalEnd
            }),
        })
    }
}

struct PrimaryFailureThenTextLlm {
    models: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl LlmExecutor for PrimaryFailureThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let model = request.model_binding.model_ref;
        self.models.lock().unwrap().push(model.clone());
        if model == "model-1" {
            return Err(awaken_runtime_contract::llm::Error::Provider(
                "primary unavailable".into(),
            ));
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("fallback must not be sampled"),
            usage: None,
            stop_reason: Some(StopReason::NaturalEnd),
        })
    }
}

struct RetryOnceLlm(AtomicUsize);

#[async_trait::async_trait]
impl LlmExecutor for RetryOnceLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(awaken_runtime_contract::llm::Error::Timeout(
                "retry this provider attempt".into(),
            ));
        }
        Ok(ChatResponse {
            output: AssistantOutput::text("retry completed"),
            usage: None,
            stop_reason: Some(StopReason::NaturalEnd),
        })
    }
}

/// Runtime-only proof that advisor is routed through the same durable
/// delegation capability as every other child Run. The Host integration tests
/// own child dispatch/Thread persistence; this fixture only verifies the
/// runtime boundary and usage fold.
struct AdvisorDelegation {
    starts: AtomicUsize,
    terminal_failure: Option<&'static str>,
}

#[async_trait::async_trait]
impl RunDelegationService for AdvisorDelegation {
    fn tool_id(&self) -> &str {
        ADVISOR_TOOL_ID
    }

    fn target_agent_id(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<AgentId, DelegationExecutionError> {
        Ok(AgentId("__managed_advisor__".into()))
    }

    fn handles_tool(&self, tool_id: &str) -> bool {
        tool_id == ADVISOR_TOOL_ID || tool_id == "agent_run"
    }

    fn target_agent_id_for(
        &self,
        tool_id: &str,
        _arguments: &serde_json::Value,
    ) -> Result<AgentId, DelegationExecutionError> {
        match tool_id {
            ADVISOR_TOOL_ID => Ok(AgentId("__managed_advisor__".into())),
            "agent_run" => Ok(AgentId("worker".into())),
            other => Err(DelegationExecutionError::new(format!(
                "unexpected tool {other}"
            ))),
        }
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.target_agent_id.0, "__managed_advisor__", "A1");
        if let Some(message) = self.terminal_failure {
            return Err(DelegationExecutionError::new(message));
        }
        let mut usage = ThreadUsage::default();
        usage.record(
            "advisor-model",
            TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                ..Default::default()
            },
        );
        Ok(DelegationStep::Ended {
            text: "independent advice".into(),
            usage,
        })
    }

    async fn resume(
        &self,
        _request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        unreachable!("advisor consultations are terminal-only")
    }
}

struct HiddenDelegationThenTextLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for HiddenDelegationThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "hidden-agent-run".into(),
                tool_id: "agent_run".into(),
                arguments: serde_json::json!({"agent_id":"worker","input":"do work"}),
            }])
        } else {
            assert!(request.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, .. }
                        if awaken_agent_contract::agent::content::extract_text(content)
                            .contains("unknown tool"))
                })
            }));
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
impl LlmExecutor for AdvisorThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        assert_eq!(
            request.model_binding.model_ref, "model-1",
            "M12/K1: Runtime must never sample the advisor provider directly"
        );
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
                                == self.expected_tool_result
                    ))),
                "M12/E2"
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
impl LlmExecutor for FailedAdvisorThenTextLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if self.primary_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "advisor-call".into(),
                tool_id: ADVISOR_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        } else {
            let result = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => Some((
                        awaken_agent_contract::agent::content::extract_text(content),
                        *is_error,
                    )),
                    _ => None,
                })
                .expect("F1 generic advisor ToolResult");
            assert_eq!(result.0, ADVISOR_FAILURE_NOTICE, "F1/E1");
            assert!(result.1, "F1/E1 remains an error result");
            assert!(!result.0.contains("sensitive"), "F1/E2");
            AssistantOutput::text("primary survived advisor failure")
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
        .with_kind(ToolKind::Advisor)
        .with_recovery(awaken_runtime_contract::tool::ToolRecoveryPolicy::durable_request()),
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
/// Constraints/invariants: committed facts are replay authority while live deltas
/// are observational and cannot substitute for the two durable commits.
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
    // Lifecycle and model observations share the same committed audit log.
    assert_eq!(committed.events.len(), 3);
    assert_eq!(
        committed_model_requests(&commit),
        vec![ModelRequestObservation::default()]
    );

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
async fn model_request_gate_fences_every_logical_request_but_not_provider_retries() {
    // Cause/effect graph: C1 a gate admits the first logical request; C2 that
    // request completes with a MaxTokens continuation and committed usage; C3
    // the next continuation or failover request is denied; C4 a provider retry
    // stays inside one logical request. Effects: E1 C2 remains committed; E2 C3
    // commits one same-Run BudgetReached ticket before any denied Provider call;
    // E3 gate coordinates always name the exact Run/Thread; E4 C4 samples twice
    // behind one gate decision. Provider attempts are not a second admission
    // owner, and an admitted request is never revoked midway through its result.
    //
    // | Rule | First request | Next logical request | Provider attempts | Effect |
    // |---|---|---|---:|---|
    // | G0 | denied | no request | 0 | E2+E3, no model audit |
    // | G1 | admitted MaxTokens | continuation denied | 1 | E1+E2+E3 |
    // | G2 | admitted primary failure | failover denied | 1 | E2+E3 |
    // | G3 | admitted retryable failure | no new request | 2 | E3+E4 |
    // Constraints/invariants: admission occurs once per logical request, before
    // Provider I/O; transparent retries never become separately gated requests.
    let paused_model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
    let paused_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Pause(
            awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
        ),
    ]));
    let paused_commit = Arc::new(MemoryCommitCoordinator::new());
    let paused = Runtime::new()
        .with_llm(paused_model.clone())
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(paused_commit.clone())
                .with_model_request_gate(paused_gate.clone()),
        )
        .await
        .expect("G0 pauses before provider sampling");
    assert_eq!(paused, RunState::Awaiting, "G0/E2");
    assert_eq!(paused_model.0.load(Ordering::SeqCst), 0, "G0/E2");
    assert_eq!(paused_gate.requests().len(), 1, "G0/E3");
    assert!(
        committed_model_requests(&paused_commit).is_empty(),
        "G0/no audit"
    );

    let continuation_model = Arc::new(MaxTokensThenTextLlm(AtomicUsize::new(0)));
    let continuation_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Admit,
        ModelRequestAdmission::Pause(
            awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
        ),
    ]));
    let continuation_commit = Arc::new(MemoryCommitCoordinator::new());
    let continuation = Runtime::new()
        .with_llm(continuation_model.clone())
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(continuation_commit.clone())
                .with_model_request_gate(continuation_gate.clone()),
        )
        .await
        .expect("G1 pauses after the completed request");
    assert_eq!(continuation, RunState::Awaiting, "G1/E2");
    assert_eq!(continuation_model.0.load(Ordering::SeqCst), 1, "G1/E1+E2");
    let continuation_requests = continuation_gate.requests();
    assert_eq!(continuation_requests.len(), 2, "G1/E3");
    assert!(continuation_requests.iter().all(|request| {
        request.run_id == RunId("run-1".into()) && request.thread_id == ThreadId("thread-1".into())
    }));
    let continuation_ticket = continuation_commit
        .resume_ticket(&RunId("run-1".into()))
        .expect("G1 committed pause ticket");
    assert_eq!(
        continuation_ticket.reason(),
        awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached,
        "G1/E2"
    );
    assert_eq!(continuation_ticket.run_id, RunId("run-1".into()), "G1/E2");
    assert!(
        continuation_commit
            .committed_messages(&ThreadId("thread-1".into()))
            .iter()
            .any(|message| message.text_content() == "completed request at the cap"),
        "G1/E1"
    );
    let continuation_observations = committed_model_requests(&continuation_commit);
    assert_eq!(continuation_observations.len(), 1, "G1/E1+E2");
    assert!(!continuation_observations[0].is_error, "G1/E1");
    assert_eq!(
        continuation_observations[0].usage,
        TokenUsage {
            prompt_tokens: 7,
            completion_tokens: 3,
            ..Default::default()
        },
        "G1/E1"
    );

    let failover_model = Arc::new(PrimaryFailureThenTextLlm {
        models: Default::default(),
    });
    let failover_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Admit,
        ModelRequestAdmission::Pause(
            awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
        ),
    ]));
    let failover_commit = Arc::new(MemoryCommitCoordinator::new());
    let mut failover_activation = activation("catalog-a");
    failover_activation
        .snapshot
        .resolved_spec
        .model_candidates
        .push(ResolvedModelCandidate::host(ModelBinding {
            provider_identity_ref: "provider-2".into(),
            model_ref: "model-2".into(),
            backend_ref: "backend-2".into(),
        }));
    let no_retries = awaken_runtime::LlmRetryPolicy {
        max_retries: 0,
        ..Default::default()
    };
    let failover = Runtime::new()
        .with_retry_policy(no_retries)
        .with_llm(failover_model.clone())
        .execute(
            failover_activation,
            RuntimeRunContext::new()
                .with_commit(failover_commit.clone())
                .with_model_request_gate(failover_gate.clone()),
        )
        .await
        .expect("G2 pauses before failover sampling");
    assert_eq!(failover, RunState::Awaiting, "G2/E2");
    assert_eq!(
        failover_model.models.lock().unwrap().as_slice(),
        &["model-1"],
        "G2/E2"
    );
    assert_eq!(failover_gate.requests().len(), 2, "G2/E3");
    let failover_observations = committed_model_requests(&failover_commit);
    assert_eq!(failover_observations.len(), 1, "G2/E2");
    assert!(failover_observations[0].is_error, "G2/E2");

    let retry_model = Arc::new(RetryOnceLlm(AtomicUsize::new(0)));
    let retry_gate = Arc::new(SequencedModelRequestGate::new([
        ModelRequestAdmission::Admit,
    ]));
    let one_retry = awaken_runtime::LlmRetryPolicy {
        max_retries: 1,
        backoff_base_ms: 0,
        overloaded_backoff_base_ms: 0,
        ..Default::default()
    };
    let retry_commit = Arc::new(MemoryCommitCoordinator::new());
    let retried = Runtime::new()
        .with_retry_policy(one_retry)
        .with_llm(retry_model.clone())
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(retry_commit.clone())
                .with_model_request_gate(retry_gate.clone()),
        )
        .await
        .expect("G3 transparent retry completes");
    assert_eq!(retried, RunState::Ended(EndCause::NaturalEnd), "G3/E4");
    assert_eq!(retry_model.0.load(Ordering::SeqCst), 2, "G3/E4");
    assert_eq!(retry_gate.requests().len(), 1, "G3/E4");
    assert_eq!(
        committed_model_requests(&retry_commit),
        vec![ModelRequestObservation {
            retry_count: 1,
            ..Default::default()
        }],
        "G3/E4"
    );
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
    // Test design — Cause: a valid frozen activation is executed without an
    // installed model provider. Effect: execution fails with the Runtime
    // Execution error before inference. Constraints/invariants: the snapshot is
    // configuration authority but cannot fabricate an executable provider.
    // Decision rule M1=no provider=>fail closed; the installed-provider success
    // partition is covered by `execution_does_not_require_an_installed_catalog`.
    let runtime = Runtime::new();
    let result = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await;
    assert!(matches!(result, Err(Error::Execution(_))));
}

#[tokio::test]
async fn advisor_call_uses_the_durable_delegation_capability_and_folds_usage() {
    // M12 cause/effect graph: C1=the frozen snapshot publishes Advisor and its
    // candidate, C2=one Host delegation service accepts that capability, and
    // C3=the child ends with text and usage. Effects: E1=the Host service starts
    // exactly once, E2=the text becomes the ordered ToolResult, E3=the primary
    // reaches NaturalEnd, and E4=the generic child-usage fold remains intact.
    //
    // Decision table:
    // | Rule | descriptor | Host service | child boundary | effects     |
    // | D1   | Advisor    | accepts      | text + usage   | E1+E2+E3+E4 |
    // Constraint K1: the Host service is the sole Advisor executor; Runtime
    // samples only the primary model and keeps the generic delivery/usage path.
    let model = Arc::new(AdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
        expected_tool_result: "independent advice",
    });
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: None,
    });
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_run_delegation(service.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let result = runtime
        .execute(
            advisor_activation(true),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("D1");
    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "D1/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 1, "D1/E1");
    assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "D1/K1");
    assert_eq!(
        commit
            .committed_messages(&ThreadId("thread-1".into()))
            .last()
            .unwrap()
            .text_content(),
        "primary done"
    );
    let usage =
        ThreadUsage::from_committed_state(&commit.committed_state(&ThreadId("thread-1".into())));
    assert_eq!(usage.by_model["advisor-model"].prompt_tokens, 7, "D1/E4");
    assert_eq!(
        usage.by_model["advisor-model"].completion_tokens, 3,
        "D1/E4"
    );
}

#[tokio::test]
async fn advisor_without_a_host_service_is_consistently_unavailable_without_provider_sampling() {
    // M12 cause/effect graph: C1=the exact Advisor descriptor and candidate are
    // frozen, C2=no Host delegation service accepts the call, C3=authorization
    // is either immediate or suspended then approved, and C4=the primary can
    // consume a ToolResult. Effects: E1=no child relationship is created,
    // E2=no advisor provider request is sampled, E3=the result is exactly
    // ADVISOR_UNAVAILABLE_NOTICE, and E4=the primary reaches NaturalEnd.
    //
    // | Rule | authorization         | Host service | Effects     |
    // | D2   | immediate             | absent       | E1+E2+E3+E4 |
    // | D4   | permission -> approve | absent       | E1+E2+E3+E4 |
    // Constraints K1/K2: a published candidate is configuration, not a second
    // Runtime execution path; approval changes authorization only and cannot
    // install an Advisor executor. ScheduledAction shares the same approved seam
    // and its wait-kind/precommit partition is owned by the scheduled suite.
    for (rule, requires_approval) in [("D2", false), ("D4", true)] {
        let model = Arc::new(AdvisorThenTextLlm {
            primary_calls: AtomicUsize::new(0),
            expected_tool_result: ADVISOR_UNAVAILABLE_NOTICE,
        });
        let runtime = Runtime::new().with_llm(model.clone());
        let runtime = if requires_approval {
            runtime.with_gate(Arc::new(SuspendAdvisorGate))
        } else {
            runtime
        };
        let commit = Arc::new(MemoryCommitCoordinator::new());

        let initial = runtime
            .execute(
                advisor_activation(true),
                RuntimeRunContext::new().with_commit(commit.clone()),
            )
            .await
            .expect(rule);
        let state = if requires_approval {
            assert_eq!(initial, RunState::Awaiting, "{rule}/C3");
            assert_eq!(model.primary_calls.load(Ordering::SeqCst), 1, "{rule}/C3");
            let ticket = commit
                .resume_ticket_for(&RunId("run-1".into()))
                .expect("D4 approval ticket");
            assert_eq!(
                ticket.reason(),
                awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission,
                "{rule}/C3"
            );
            runtime
                .resume(
                    ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0),
                    commit.as_ref(),
                    RuntimeRunContext::new().with_commit(commit.clone()),
                )
                .await
                .expect("D4 approved Advisor remains model-visible and non-fatal")
        } else {
            initial
        };

        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "{rule}/E4");
        assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "{rule}/E2");
        assert_eq!(committed_model_requests(&commit).len(), 2, "{rule}/E2");
        let committed = commit.committed();
        assert!(
            committed
                .messages
                .iter()
                .any(|message| message.role == Role::Tool
                    && message.text_content() == ADVISOR_UNAVAILABLE_NOTICE),
            "{rule}/E3"
        );
        assert!(
            committed
                .state
                .iter()
                .all(|command| command.key.0 != RunDelegations::KEY),
            "{rule}/E1"
        );
    }
}

#[tokio::test]
async fn advisor_terminal_failure_is_generic_and_the_primary_run_continues() {
    // Cause/effect graph: C1 the exact Advisor capability is published and
    // accepted; C2 its child service ends with a terminal provider detail; C3
    // the primary model can consume an error ToolResult. Effects: E1 the model
    // sees the one generic advisor failure notice with `is_error`; E2 provider
    // detail never enters the primary transcript; E3 the primary reaches a
    // natural end; E4 the stable call starts exactly once.
    //
    // Decision table:
    // | Rule | published | child terminal failure | model continues | Effects |
    // | F1   | yes       | yes                    | yes             | E1-E4   |
    // Constraints/invariants: provider detail is redacted at the ToolResult
    // boundary and child failure cannot terminate the still-runnable primary.
    let model = Arc::new(FailedAdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
    });
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: Some("sensitive provider detail"),
    });
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let result = Runtime::new()
        .with_llm(model.clone())
        .with_run_delegation(service.clone())
        .execute(
            advisor_activation(true),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("F1 primary remains runnable");
    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "F1/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 1, "F1/E4");
    assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "F1/E3");
    let transcript =
        serde_json::to_string(&commit.committed_messages(&ThreadId("thread-1".into()))).unwrap();
    assert!(transcript.contains(ADVISOR_FAILURE_NOTICE), "F1/E1");
    assert!(!transcript.contains("sensitive provider detail"), "F1/E2");
}

#[tokio::test]
async fn advisor_is_primary_only_even_when_a_child_shares_the_host_service() {
    // M12 cause/effect graph: C1=the child snapshot contains Advisor, C2=the
    // process shares a Host service that could execute Advisor for a primary,
    // and C3=the current activation has a delegation origin. Effects: E1=the
    // service starts zero children, E2=the exact unavailable ToolResult is fed
    // back, and E3=the child model may finish normally.
    // Constraint K1: Advisor is primary-only; sharing the sole Host executor
    // cannot create recursive advice. Decision rule D3: C1+C2+C3 -> E1+E2+E3.
    let model = Arc::new(AdvisorThenTextLlm {
        primary_calls: AtomicUsize::new(0),
        expected_tool_result: ADVISOR_UNAVAILABLE_NOTICE,
    });
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: None,
    });
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_run_delegation(service.clone());
    let mut activation = advisor_activation(true);
    activation.delegation_origin = Some(
        awaken_agent_contract::agent::delegation::DelegationOrigin::root(
            RunId("parent".into()),
            "parent-call",
        ),
    );

    let result = runtime
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("D3");

    assert_eq!(result, RunState::Ended(EndCause::NaturalEnd), "D3/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 0, "D3/E1");
    assert_eq!(model.primary_calls.load(Ordering::SeqCst), 2, "D3/E2");
}

#[tokio::test]
async fn an_unpublished_delegation_tool_cannot_use_a_shared_host_service() {
    // Cause/effect graph: C1=the process-wide Host service knows `agent_run`,
    // C2=this exact snapshot publishes no AgentDelegation descriptor, and C3=the
    // model hallucinates that tool id. Effects: E1=no child service entry,
    // E2=ordinary unknown-tool output, E3=the primary may finish. This is the
    // security boundary that lets Managed remove `agent_run` while sharing the
    // same Host with native SDK sessions.
    //
    // | Rule | Host handles | snapshot publishes | call | effects  |
    // | H1   | yes          | no                 | yes  | E1+E2+E3 |
    // Constraints/invariants: immutable snapshot publication gates capability;
    // a process-wide service cannot make an unpublished tool executable.
    let service = Arc::new(AdvisorDelegation {
        starts: AtomicUsize::new(0),
        terminal_failure: None,
    });
    let runtime = Runtime::new()
        .with_llm(Arc::new(HiddenDelegationThenTextLlm {
            calls: AtomicUsize::new(0),
        }))
        .with_run_delegation(service.clone());
    let outcome = runtime
        .execute(activation("catalog-a"), RuntimeRunContext::new())
        .await
        .expect("H1");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "H1/E3");
    assert_eq!(service.starts.load(Ordering::SeqCst), 0, "H1/E1");
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
