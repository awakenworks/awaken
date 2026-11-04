use super::*;
use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_memory::{MEMORY_AGENT_ID, MemoryExtractionStatus, RecallSelector as _};
use awaken_resource_contract::{MemoryVersionQueries as _, MemoryWriter as _};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult,
};
use awaken_session_run_delivery::PlatformMemoryHandle;
use awaken_tool_contract::tool::ToolCall;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[test]
fn extractor_run_error_classification_never_replays_unknown_execution() {
    // Cause/effect graph: C1 the error occurs before Runtime execution or at
    // Runtime; C2 committed Run state is absent/running/terminal; C3 terminal
    // cause is known/indeterminate. Effects: E1 provisioning is retryable on
    // the same stable id; E2 unknown execution carries that exact id; E3 a
    // committed known failure is permanent. Decision rules:
    // R1=pre-runtime provisioning=>E1; R2=runtime+(absent|running|unknown)
    // =>E2; R3=runtime+known-terminal=>E3. Configuration is also permanent.
    let intent = MemoryExtractionIntent::new_range(
        "extract-1",
        "run-1:terminal",
        "ws-a",
        "session-a",
        "terminal",
        "memory-a",
        1,
        0,
        0,
        Vec::new(),
        MemoryExtractorSnapshot::host_executor("memory-agent", "host", "model", "host")
            .expect("literal coordinates"),
    )
    .unwrap();
    let runtime_error = || {
        AgentRunError::Runtime(awaken_runtime_contract::execution::Error::Execution(
            "runtime boundary lost".into(),
        ))
    };

    assert!(matches!(
        classify_extraction_run_error(
            &intent,
            None,
            AgentRunError::Provisioning("sandbox unavailable".into())
        ),
        MemoryExtractionOutcome::Retryable { .. }
    ));
    for state in [
        None,
        Some(RunState::Running),
        Some(RunState::Ended(EndCause::Indeterminate)),
    ] {
        assert!(matches!(
            classify_extraction_run_error(&intent, state, runtime_error()),
            MemoryExtractionOutcome::ExecutionIndeterminate { auxiliary_run_id, .. }
                if auxiliary_run_id == "extract-1/agent/run"
        ));
    }
    assert!(matches!(
        classify_extraction_run_error(
            &intent,
            Some(RunState::Ended(EndCause::Stopped("policy".into()))),
            runtime_error()
        ),
        MemoryExtractionOutcome::PermanentFailure { .. }
    ));
    assert!(matches!(
        classify_extraction_run_error(
            &intent,
            None,
            AgentRunError::Configuration("invalid frozen Agent".into())
        ),
        MemoryExtractionOutcome::PermanentFailure { .. }
    ));
}

/// A stub extractor model: first Step emits a write_memory call; once it sees
/// the tool result, it replies done.
struct ExtractorModel;

struct CountingExtractorModel(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl LlmExecutor for CountingExtractorModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        ExtractorModel.infer(request).await
    }
}

#[async_trait]
impl LlmExecutor for ExtractorModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let saw_tool_result = request.messages.iter().any(|m| m.role == Role::Tool);
        let output = if saw_tool_result {
            AssistantOutput::text("saved 1 memory")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write_memory".into(),
                arguments: serde_json::json!({
                    "name": "user prefs",
                    "content": "user likes rust",
                }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct FailFirstRecoverableRead {
    inner: Arc<dyn MemoryExtractionRepository>,
    remaining: std::sync::atomic::AtomicUsize,
}

impl FailFirstRecoverableRead {
    fn new(inner: Arc<dyn MemoryExtractionRepository>) -> Self {
        Self {
            inner,
            remaining: std::sync::atomic::AtomicUsize::new(1),
        }
    }
}

#[async_trait]
impl MemoryExtractionRepository for FailFirstRecoverableRead {
    async fn put_extraction_if_absent(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<awaken_ext_memory::PutMemoryExtractionOutcome, MemoryExtractionError> {
        self.inner.put_extraction_if_absent(intent).await
    }

    async fn get_extraction(
        &self,
        intent_id: &str,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        self.inner.get_extraction(intent_id).await
    }

    async fn extraction_cursor(&self, thread_id: &str) -> Result<usize, MemoryExtractionError> {
        self.inner.extraction_cursor(thread_id).await
    }

    async fn recoverable_extractions(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryExtractionIntent>, MemoryExtractionError> {
        if self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(MemoryExtractionError::Storage(
                "transient recovery read".into(),
            ));
        }
        self.inner.recoverable_extractions(limit).await
    }

    async fn compare_and_swap_extraction(
        &self,
        expected_revision: u64,
        intent: MemoryExtractionIntent,
    ) -> Result<(), MemoryExtractionError> {
        self.inner
            .compare_and_swap_extraction(expected_revision, intent)
            .await
    }
}

fn user(text: &str) -> Message {
    Message {
        id: MessageId("u1".into()),
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}

fn snapshot(thread: &str, messages: Vec<Message>) -> TranscriptSnapshot {
    TranscriptSnapshot::new(
        awaken_agent_contract::agent::thread::Id(thread.to_string()),
        awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
        messages,
    )
}

fn extractor(
    instructions: Option<&str>,
    extraction_prompt: Option<&str>,
) -> MemoryExtractorSnapshot {
    let mut extractor =
        MemoryExtractorSnapshot::host_executor(MEMORY_AGENT_ID, "host", "stub", "host")
            .expect("literal coordinates");
    if let Some(instructions) = instructions {
        extractor.agent.resolved_spec.instructions = instructions.to_string();
        extractor.agent.recompute_fingerprint().unwrap();
    }
    extractor.extraction_prompt = extraction_prompt.map(str::to_string);
    extractor
}

struct InspectExtractorContext {
    subject: Option<awaken_runtime_contract::DataSubjectId>,
    calls: std::sync::atomic::AtomicUsize,
}

impl awaken_inference_endpoint::InferenceExecutorMaterializer for InspectExtractorContext {
    fn materialize_pinned(
        &self,
        _model: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        assert_eq!(context.capture.subject, self.subject);
        assert_eq!(
            context
                .execution_scope
                .as_ref()
                .map(|scope| scope.0.as_str()),
            Some("ws-test")
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(Arc::new(ExtractorModel))
    }
}

#[tokio::test]
async fn extractor_context_retains_frozen_attribution_before_materialization() {
    // Cause/effect table: C1 no materializer / installed exact materializer;
    // C2 frozen subject missing/A/B. All six rows retain C2 in the returned
    // context together with the intent's original Workspace; the installed
    // materializer observes both before it creates its
    // executor. No caller/process default fills the missing historical subject.
    for materialized in [false, true] {
        for subject in [None, Some("subject-a"), Some("subject-b")] {
            let root = tempfile::tempdir().unwrap();
            let (_runtime, bound, _content, _intents) = bound_test_memory(
                Arc::new(ExtractorModel),
                "subject-context",
                root.path(),
                root.path(),
            )
            .await;
            let subject = subject.map(|s| awaken_runtime_contract::DataSubjectId(s.into()));
            let materializer = Arc::new(InspectExtractorContext {
                subject: subject.clone(),
                calls: Default::default(),
            });
            let runtime = MemoryRuntime::new(
                bound.runtime.llm.clone(),
                bound.runtime.provider.clone(),
                bound.runtime.background(),
                bound.runtime.extraction_repository(),
                bound.runtime.clock.clone(),
                materialized.then(|| {
                    materializer.clone()
                        as Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>
                }),
            );
            let mut intent = MemoryExtractionIntent::new_range(
                "execution-context",
                "terminal",
                "ws-test",
                "subject-context",
                "terminal",
                "test-store",
                1,
                0,
                1,
                vec![user("prefers tea")],
                extractor(None, None),
            )
            .unwrap()
            .with_execution(awaken_ext_memory::MemoryExtractionExecution {
                commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::Session,
                data_subject_id: subject.clone(),
            })
            .unwrap();
            use awaken_time::WallClockExt as _;
            intent
                .claim("owner", runtime.clock.now_ms(), 60_000)
                .unwrap();
            let (_, context) = runtime.materialize_extractor(&intent).unwrap();
            assert_eq!(context.capture.subject, subject);
            assert_eq!(
                context
                    .execution_scope
                    .as_ref()
                    .map(|scope| scope.0.as_str()),
                Some("ws-test")
            );
            assert_eq!(
                materializer.calls.load(Ordering::SeqCst),
                usize::from(materialized)
            );
        }
    }
}

#[tokio::test]
async fn embedded_extraction_cannot_claim_an_independent_commit_partition() {
    // P1 the exact Session/Workspace/Memory binding matches, but its frozen
    // commit scope is AuxiliaryThread => this embedded driver does not accept
    // it. P2 explicit direct misuse also refuses before materializer/model IO.
    // A Session scope with identical coordinates remains accepted (P3).
    let root = tempfile::tempdir().unwrap();
    let (_runtime, bound, _content, _intents) = bound_test_memory(
        Arc::new(ExtractorModel),
        "independent-partition",
        root.path(),
        root.path(),
    )
    .await;
    let materializer = Arc::new(InspectExtractorContext {
        subject: None,
        calls: Default::default(),
    });
    let selected = Arc::new(MemoryRuntime::new(
        bound.runtime.llm.clone(),
        bound.runtime.provider.clone(),
        bound.runtime.background(),
        bound.runtime.extraction_repository(),
        bound.runtime.clock.clone(),
        Some(materializer.clone()),
    ))
    .bound(bound.binding.clone());
    let original = MemoryExtractionIntent::new_range(
        "partition-context",
        "terminal",
        "ws-test",
        "independent-partition",
        "terminal",
        "test-store",
        1,
        0,
        1,
        vec![user("prefers tea")],
        extractor(None, None),
    )
    .unwrap();
    assert!(selected.accepts(&original), "P3");
    let intent = original
        .with_execution(awaken_ext_memory::MemoryExtractionExecution {
            commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::AuxiliaryThread,
            data_subject_id: None,
        })
        .unwrap();
    assert!(!selected.accepts(&intent), "P1");
    assert!(
        matches!(
            selected.extract_mutations(&intent).await,
            MemoryExtractionOutcome::PermanentFailure { .. }
        ),
        "P2"
    );
    assert_eq!(materializer.calls.load(Ordering::SeqCst), 0, "P2");
}

async fn bound_test_memory(
    llm: Arc<dyn LlmExecutor>,
    session_id: &str,
    sandbox_base: &std::path::Path,
    _memory_root: &std::path::Path,
) -> (
    Arc<MemoryRuntime>,
    BoundMemory,
    Arc<awaken_memory_store::VolatileMemoryRepository>,
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
) {
    let extractions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("open ephemeral Memory extraction repository"),
    );
    let runtime = Arc::new(MemoryRuntime::new(
        llm,
        Arc::new(LocalProvider::new(sandbox_base)),
        Arc::new(BackgroundRuns::new()),
        extractions.clone(),
        awaken_time::system_clock(),
        None,
    ));
    let config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: "test-store".into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let platform = Arc::new(PlatformMemoryHandle::from_roles(
        awaken_resource_contract::MemoryExecutionContent::new(repository.clone()),
        "test-store".into(),
        true,
    ));
    let bound = runtime.bound(Arc::new(awaken_session_run_delivery::MemoryBinding::new(
        session_id,
        "ws-test",
        platform,
        Some(crate::host::test_resource_validator()),
        &config,
        true,
    )));
    bound
        .binding()
        .attach_extraction_commit(Arc::new(SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            )),
        ))))
        .expect("healthy Memory context");
    (runtime, bound, repository, extractions)
}

#[tokio::test]
async fn host_extractor_checks_the_same_durable_claim_before_model_effects() {
    // Cause/effect table: H1 the original owner+epoch still has a live row =>
    // the real extractor makes its two model requests and proposes one write.
    // H2 expired lease; H3 another owner; H4 same owner at a newer epoch;
    // H5 missing intent => zero model requests and no proposal batch. All rows
    // leave the durable claim and Memory content untouched: this adapter only
    // executes extraction, while the controller still owns transitions/CAS.
    // Error classification remains with the existing SDK and Run classifier;
    // these cases test that HostExecutor actually installs their ownership port.
    use awaken_time::WallClockExt as _;
    for case in ["live", "expired", "other-owner", "new-epoch", "missing"] {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_runtime, bound, content, intents) = bound_test_memory(
            Arc::new(CountingExtractorModel(calls.clone())),
            case,
            root.path(),
            root.path(),
        )
        .await;
        let mut intent = MemoryExtractionIntent::new_range(
            format!("claim-fence-{case}"),
            format!("{case}:terminal"),
            "ws-test",
            case,
            "terminal",
            "test-store",
            1,
            0,
            1,
            vec![user("I like rust")],
            extractor(None, None),
        )
        .unwrap();
        let now = awaken_time::system_clock().now_ms();
        if case == "live" || case == "missing" {
            intent.claim("owner", now, 60_000).unwrap();
        } else {
            intent.claim("owner", now - 10_000, 1).unwrap();
        }
        let admitted = intent.clone();
        if matches!(case, "other-owner" | "new-epoch") {
            let owner = if case == "other-owner" {
                "successor"
            } else {
                "owner"
            };
            intent.claim(owner, now, 60_000).unwrap();
        }
        if case != "missing" {
            intents
                .put_extraction_if_absent(intent.clone())
                .await
                .unwrap();
        }
        let outcome = bound.extract_mutations(&admitted).await;
        if case == "live" {
            assert_eq!(calls.load(Ordering::SeqCst), 2, "H1");
            assert!(
                matches!(outcome, MemoryExtractionOutcome::Mutations(ref writes) if writes.len() == 1),
                "H1"
            );
        } else {
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{case}");
            assert!(
                !matches!(outcome, MemoryExtractionOutcome::Mutations(_)),
                "{case}"
            );
        }
        assert_eq!(
            intents.get_extraction(&intent.intent_id).await.unwrap(),
            (case != "missing").then_some(intent),
            "the caller's current claim is never changed by a stale extractor"
        );
        assert!(
            content
                .list_versions("test-store")
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn memory_recovery_projection_preserves_handle_identity_and_read_only_bindings() {
    // C1 standard aliases={0,1,2}; C2 automatic={absent,shared,distinct};
    // C3 maximum access={read-only,writable}. All18 rows use equal storage
    // coordinates: aliases collapse, distinct handles survive, and selecting
    // recovery work or attaching commit authority remains the caller's step.
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: "projection-store".into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let mut checked = 0;
    for writable in [false, true] {
        let platform = Arc::new(PlatformMemoryHandle::from_roles(
            awaken_resource_contract::MemoryExecutionContent::new(repository.clone()),
            config.memory_store_id.to_string(),
            writable,
        ));
        let binding = || {
            Arc::new(MemoryBinding::new(
                "projection-session",
                "projection-workspace",
                platform.clone(),
                None,
                &config,
                writable,
            ))
        };
        let first = binding();
        let distinct = binding();
        for aliases in 0..=2 {
            for automatic in ["absent", "shared", "distinct"] {
                let mut staging =
                    awaken_session_realization::realization_state::ResourceStagingState::default();
                for index in 0..aliases {
                    staging
                        .memory_bindings
                        .insert(format!("binding-{index}"), first.clone());
                }
                staging.memory = match automatic {
                    "shared" => Some(first.clone()),
                    "distinct" => Some(distinct.clone()),
                    _ => None,
                };
                let refs = (Arc::strong_count(&first), Arc::strong_count(&distinct));
                let projected = staging.memory_recovery_bindings();
                let expected_first = aliases > 0 || automatic == "shared";
                let expected_distinct = automatic == "distinct";
                assert_eq!(
                    projected.len(),
                    usize::from(expected_first) + usize::from(expected_distinct)
                );
                assert_eq!(
                    projected.iter().any(|memory| Arc::ptr_eq(memory, &first)),
                    expected_first
                );
                assert_eq!(
                    projected
                        .iter()
                        .any(|memory| Arc::ptr_eq(memory, &distinct)),
                    expected_distinct
                );
                assert_eq!(staging.memory_bindings.len(), aliases);
                assert!(
                    first
                        .extraction_commit()
                        .expect("healthy Memory context")
                        .is_none()
                        && distinct
                            .extraction_commit()
                            .expect("healthy Memory context")
                            .is_none()
                );
                drop(projected);
                assert_eq!(
                    (Arc::strong_count(&first), Arc::strong_count(&distinct)),
                    refs
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 18);
}

/// A model that replies with fixed selection indices, standing in for the
/// `memory-selector` sub-agent.
struct IndexModel;

#[async_trait]
impl LlmExecutor for IndexModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("[1], [2]"),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn agent_selector_runs_the_subagent_and_parses_its_reply() {
    let clock = awaken_time::system_clock();
    // Cause/effect graph and decision-table rule S1:
    // C1 selector snapshot valid + C2 explicit commit/history authority present
    // -> E1 auxiliary Run commits through that authority + E2 parsed selection
    // is returned. The forbidden complement (!C2 -> process-local fallback) is
    // unrepresentable because `AgentSelector::new` requires the authority.
    // C3 the same clock Arc feeds that commit owner and auxiliary Runtime; it
    // is required by the selector's construction, without a second fallback.
    let commit = Arc::new(SessionCommit::Local(Arc::new(
        crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
            clock.clone(),
        )),
    )));
    let selector = AgentSelector::new(
        Arc::new(IndexModel),
        awaken_ext_memory::default_selector_agent(
            "stub",
            awaken_ext_memory::DEFAULT_SELECTOR_INSTRUCTIONS,
        )
        .expect("literal model reference"),
        commit,
        clock.clone(),
        Arc::new(LocalProvider::new(
            std::env::temp_dir().join(format!("awaken-mem-select-{}", std::process::id())),
        )),
    );
    let manifest = vec![
        (0usize, "alpha".to_string()),
        (1, "beta".to_string()),
        (2, "gamma".to_string()),
    ];
    let picked = selector.select("which?", &manifest, 5).await;
    assert_eq!(picked, vec![1, 2]);
}

#[tokio::test]
async fn extraction_writes_a_memory_then_recall_reads_it_back() {
    // Recovery-wrapper relocation table: C1 no durable intent => no spawned
    // work; C2 one ready frozen binding + terminal enqueue => the same selected
    // repository/controller drives the original extractor and commits content.
    // The committed store/recall assertions below, rather than drain alone,
    // prove that the shared recovery entry retains the original behavior.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sandbox_base = std::env::temp_dir().join(format!("awaken-mem-sbx-{stamp}"));
    let mem_root = std::env::temp_dir().join(format!("awaken-mem-root-{stamp}"));

    let (runtime, extraction, repository, _extractions) = bound_test_memory(
        Arc::new(ExtractorModel),
        "thread-1",
        &sandbox_base,
        &mem_root,
    )
    .await;

    assert!(
        !extraction.reconcile().await.unwrap(),
        "an empty recovery scan must not create a background run"
    );
    extraction
        .trigger(
            "terminal-1",
            snapshot("thread-1", vec![user("I really like rust")]),
            extractor(None, None),
        )
        .await
        .unwrap();
    assert!(runtime.background().drain(Duration::from_secs(10)).await);

    assert_eq!(
        awaken_resource_contract::MemoryQueries::get_by_path(
            repository.as_ref(),
            "test-store",
            "/user-prefs.md"
        )
        .await
        .unwrap()
        .unwrap()
        .content
        .as_deref(),
        Some("user likes rust")
    );
    // The read side surfaces it through bounded recall.
    let entries = extraction.store().entries().await.unwrap();
    let block =
        awaken_ext_memory::recall::render(&entries, &awaken_ext_memory::RecallBounds::default())
            .expect("recall block");
    assert!(block.contains("user likes rust"), "got: {block}");
}

struct MalformedExtractorModel;

#[async_trait]
impl LlmExecutor for MalformedExtractorModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        if request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            return ExtractorModel.infer(request).await;
        }
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![
                ToolCall {
                    call_id: "valid".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({"name":"valid","content":"prefers tea"}),
                },
                ToolCall {
                    call_id: "malformed".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({"name":"malformed"}),
                },
            ]),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn a_malformed_extractor_batch_commits_failure_without_partial_memory() {
    // Cause/effect table: C1 one valid call precedes a malformed write in the
    // same real extractor Run; C2 the Run reaches its committed boundary.
    // M2 C1+C2 => the existing durable intent becomes TerminalFailed, no
    // mutations/receipt are admitted, and even the earlier valid content is
    // absent. Drain is only a join: committed intent/content are the oracles.
    let root = tempfile::tempdir().unwrap();
    let (runtime, extraction, content, intents) = bound_test_memory(
        Arc::new(MalformedExtractorModel),
        "malformed-thread",
        root.path(),
        root.path(),
    )
    .await;
    extraction
        .trigger(
            "terminal",
            snapshot("malformed-thread", vec![user("prefers tea")]),
            extractor(None, None),
        )
        .await
        .unwrap();
    assert!(runtime.background().drain(Duration::from_secs(10)).await);
    let intent = intents
        .get_extraction("memory-extraction:malformed-thread:terminal")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.status, MemoryExtractionStatus::TerminalFailed);
    assert!(intent.mutations.is_empty());
    assert!(intent.receipt.is_none());
    assert!(
        awaken_resource_contract::MemorySnapshotSource::snapshot_heads(
            content.as_ref(),
            "test-store"
        )
        .await
        .unwrap()
        .is_empty()
    );
}

/// An extractor that saves whatever non-prompt text it was seeded with, so the
/// test can see what reached it.
struct SeedEchoModel;

#[async_trait]
impl LlmExecutor for SeedEchoModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let saved = request.messages.iter().any(|m| {
            m.role == Role::Tool
                && m.content.iter().any(|b| match b {
                    awaken_agent_contract::agent::content::ContentBlock::ToolResult {
                        content,
                        ..
                    } => crate::config::block_text(content).contains("saved memory"),
                    _ => false,
                })
        });
        if saved {
            return Ok(ChatResponse {
                output: AssistantOutput::text("done"),
                usage: None,
                stop_reason: None,
            });
        }
        let seen: Vec<String> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User || m.role == Role::System)
            .map(|m| crate::config::block_text(&m.content))
            .filter(|t| !t.contains("Extract durable memories"))
            .collect();
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write_memory".into(),
                arguments: serde_json::json!({ "name": "seen", "content": seen.join("|") }),
            }]),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn extraction_seed_excludes_recalled_memory_messages() {
    // Test design. Causes: C1 committed history contains a recalled-memory
    // System message plus real Run messages; C2 extraction builds its seed.
    // Effects: E1 C2 excludes the recalled payload; E2 genuine conversation
    // remains eligible. Constraint/Invariant: recalled context is read-only
    // input, never new memory evidence. Decision rule: seed both classes and
    // assert only genuine history reaches extraction.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sandbox_base = std::env::temp_dir().join(format!("awaken-mem2-sbx-{stamp}"));
    let mem_root = std::env::temp_dir().join(format!("awaken-mem2-root-{stamp}"));
    let (runtime, extraction, repository, _extractions) =
        bound_test_memory(Arc::new(SeedEchoModel), "t", &sandbox_base, &mem_root).await;

    // A committed history with a recalled-memory system message + a real Run.
    let recall = Message::text(
        MessageId(format!("{}-1", awaken_ext_memory::RECALL_MESSAGE_ID_PREFIX)),
        Role::System,
        "RECALLED SECRET",
    );
    extraction
        .trigger(
            "terminal-1",
            snapshot("t", vec![recall, user("please note this")]),
            extractor(None, None),
        )
        .await
        .unwrap();
    assert!(runtime.background().drain(Duration::from_secs(10)).await);

    let seen = awaken_resource_contract::MemoryQueries::get_by_path(
        repository.as_ref(),
        "test-store",
        "/seen.md",
    )
    .await
    .unwrap()
    .unwrap()
    .content
    .unwrap();
    assert!(seen.contains("please note this"), "real Run seen: {seen}");
    assert!(
        !seen.contains("RECALLED SECRET"),
        "recalled content must not reach the extractor: {seen}"
    );
}

#[tokio::test]
async fn extraction_uses_the_per_agent_memory_prompts() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sandbox_base = std::env::temp_dir().join(format!("awaken-mem3-sbx-{stamp}"));
    let mem_root = std::env::temp_dir().join(format!("awaken-mem3-root-{stamp}"));
    let (runtime, extraction, repository, _extractions) = bound_test_memory(
        Arc::new(SeedEchoModel),
        "t-custom",
        &sandbox_base,
        &mem_root,
    )
    .await;

    extraction
        .trigger(
            "terminal-1",
            snapshot("t-custom", vec![user("remember this")]),
            extractor(Some("CUSTOM MEMORY SYSTEM"), Some("CUSTOM EXTRACTION TASK")),
        )
        .await
        .unwrap();
    assert!(runtime.background().drain(Duration::from_secs(10)).await);

    let seen = awaken_resource_contract::MemoryQueries::get_by_path(
        repository.as_ref(),
        "test-store",
        "/seen.md",
    )
    .await
    .unwrap()
    .unwrap()
    .content
    .unwrap();
    assert!(
        seen.contains("CUSTOM MEMORY SYSTEM"),
        "custom instructions reached extractor: {seen}"
    );
    assert!(
        seen.contains("CUSTOM EXTRACTION TASK"),
        "custom task reached extractor: {seen}"
    );
}

#[tokio::test]
async fn terminal_redelivery_creates_one_logical_extraction_and_one_memory_version() {
    // Structural-extraction coverage: the private terminal-observer adapter
    // delegates committed terminal truth to this same `BoundMemory::trigger`
    // path. Keeping the redelivery cause here proves the module split leaves
    // the one durable intent and one governed Memory version effects intact;
    // the child module owns no observer state, repository, or retry policy.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sandbox_base = std::env::temp_dir().join(format!("awaken-mem4-sbx-{stamp}"));
    let mem_root = std::env::temp_dir().join(format!("awaken-mem4-root-{stamp}"));
    let (runtime, extraction, repository, extractions) = bound_test_memory(
        Arc::new(ExtractorModel),
        "thread-redelivery",
        &sandbox_base,
        &mem_root,
    )
    .await;

    for _ in 0..2 {
        extraction
            .trigger(
                "terminal-7",
                snapshot("thread-redelivery", vec![user("I really like rust")]),
                extractor(None, None),
            )
            .await
            .unwrap();
    }
    assert!(runtime.background().drain(Duration::from_secs(10)).await);

    let intent = extractions
        .get_extraction("memory-extraction:thread-redelivery:terminal-7")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(intent.status, MemoryExtractionStatus::Completed);
    assert_eq!(intent.attempts, 1);
    assert_eq!(
        repository.list_versions("test-store").await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn recovery_after_store_before_receipt_does_not_duplicate_a_memory_version() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sandbox_base = std::env::temp_dir().join(format!("awaken-mem5-sbx-{stamp}"));
    let mem_root = std::env::temp_dir().join(format!("awaken-mem5-root-{stamp}"));
    let (runtime, extraction, repository, extractions) = bound_test_memory(
        Arc::new(ExtractorModel),
        "thread-crash",
        &sandbox_base,
        &mem_root,
    )
    .await;
    let content = "customer maintenance is Sunday";
    let target = awaken_memory_store::sha256_hex(content);
    let mut intent = MemoryExtractionIntent::new_range(
        "memory-extraction:thread-crash:terminal-8",
        "thread-crash:terminal-8",
        "ws-test",
        "thread-crash",
        "terminal-8",
        "test-store",
        1,
        0,
        1,
        vec![user("remember the maintenance window")],
        MemoryExtractorSnapshot::host_executor(MEMORY_AGENT_ID, "host", "stub", "host")
            .expect("literal coordinates"),
    )
    .unwrap();
    let generation = intent.claim("crashed-worker", 0, 1).unwrap();
    intent
        .mark_extracted(
            "crashed-worker",
            generation,
            0,
            vec![MemoryExtractionMutation {
                path: "/maintenance.md".into(),
                content: content.into(),
                observed_generation: None,
                target_sha256: target.clone(),
            }],
        )
        .unwrap();
    extractions.put_extraction_if_absent(intent).await.unwrap();
    // The data-plane commit happened, then the process died before the intent
    // advanced to Stored. Recovery must recognize the target hash as applied.
    repository
        .create("test-store", "/maintenance.md", content)
        .await
        .unwrap();

    extraction.reconcile().await.unwrap();
    assert!(runtime.background().drain(Duration::from_secs(10)).await);
    let recovered = extractions
        .get_extraction("memory-extraction:thread-crash:terminal-8")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.status, MemoryExtractionStatus::Completed);
    assert!(recovered.receipt.unwrap().mutations[0].already_applied);
    assert_eq!(
        repository.list_versions("test-store").await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn parent_rehydration_recovers_a_child_intent_after_enqueue_crash() {
    // Cause/effect graph: C1 the terminal transcript belongs to a logical
    // child; C2 its extraction binding and commit authority belong to the
    // parent physical Session; C3 the first process dies after durable
    // enqueue but before any claim; C4 the parent binding is rehydrated from
    // the same Workspace/Store/config; C5 the selected direct binding is/is
    // not already in the binding map and one Arc may appear under two binding
    // ids; C6 a read-only sibling binding exists; C7 the first recovery read
    // fails transiently. Effects: E1 the intent
    // persists the parent Session and logical child without a third identity;
    // E2 no work starts without a commit binding; E3 parent rehydration
    // accepts and completes the child intent independently of plugin
    // selection; E4 the stable auxiliary Run commits through the parent
    // authority; E5 one governed Memory version is written; E6 identical
    // Arcs are driven once, the direct-only writable binding receives the
    // commit authority, and the read-only binding does not drive recovery;
    // E7 the first rehydrate fails closed and the same recovery entry
    // converges on its next resident/cold wake.
    // Constraints: no child installed Session or physical commit partition exists;
    // recovery reuses the ordinary BoundMemory/controller/repository path.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5       | C6 | C7        | effects          |
    // | R1   | T  | T  | T  | F  | -        | -  | F         | E1,E2           |
    // | R2   | T  | T  | T  | T  | duplicate| T  | F         | E1,E3,E4,E5,E6 |
    // | R3   | T  | T  | T  | T  | direct   | T  | F         | E3,E6           |
    // | R4   | T  | T  | T  | T  | either   | T  | once      | E3,E6,E7        |
    let storage = tempfile::tempdir().expect("Memory recovery tempdir");
    let extraction_db = storage.path().join("sessions.db");
    let extraction_db = extraction_db.to_string_lossy().to_string();
    let parent = "memory-parent-session";
    let child = "memory-logical-child";
    let terminal = "memory-child-terminal";
    let intent_id = format!("memory-extraction:{child}:{terminal}");
    let config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: "test-store".into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let content = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let platform = Arc::new(PlatformMemoryHandle::from_roles(
        awaken_resource_contract::MemoryExecutionContent::new(content.clone()),
        "test-store".into(),
        true,
    ));

    let first_repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&extraction_db)
            .await
            .expect("R1 open extraction repository"),
    );
    let first_runtime = Arc::new(MemoryRuntime::new(
        Arc::new(ExtractorModel),
        Arc::new(LocalProvider::new(storage.path().join("first-sandbox"))),
        Arc::new(BackgroundRuns::new()),
        first_repository.clone(),
        awaken_time::system_clock(),
        None,
    ));
    let child_binding =
        first_runtime.bound(Arc::new(awaken_session_run_delivery::MemoryBinding::new(
            parent,
            "ws-test",
            platform.clone(),
            Some(crate::host::test_resource_validator()),
            &config,
            true,
        )));
    child_binding
        .trigger(
            terminal,
            snapshot(child, vec![user("remember child recovery")]),
            extractor(None, None),
        )
        .await
        .expect("R1 durable enqueue");
    let pending = first_repository
        .get_extraction(&intent_id)
        .await
        .expect("R1 query pending intent")
        .expect("R1 pending intent exists");
    assert_eq!(pending.session_id, parent, "R1/E1 physical Session");
    assert_eq!(pending.logical_thread_id(), child, "R1/E1 logical child");
    assert_eq!(pending.status, MemoryExtractionStatus::Pending, "R1/E2");
    assert!(
        first_runtime
            .background()
            .drain(Duration::from_secs(1))
            .await,
        "R1/E2"
    );
    drop(child_binding);
    drop(first_runtime);
    drop(first_repository);

    let recovered_repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&extraction_db)
            .await
            .expect("R2 reopen extraction repository"),
    );
    let recovered_host =
        (crate::SessionExecutorBuilder::ephemeral(Arc::new(ExtractorModel), "stub")
            .await
            .with_memory_extraction_repository(Arc::new(FailFirstRecoverableRead::new(
                recovered_repository.clone(),
            ))))
        .build()
        .expect("validated Host dependencies");
    let parent_commit = Arc::new(SessionCommit::Local(Arc::new(
        crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        )),
    )));
    let parent_binding = Arc::new(awaken_session_run_delivery::MemoryBinding::new(
        parent,
        "ws-test",
        platform.clone(),
        Some(crate::host::test_resource_validator()),
        &config,
        true,
    ));
    let read_only_binding = Arc::new(awaken_session_run_delivery::MemoryBinding::new(
        parent,
        "ws-test",
        platform.clone(),
        Some(crate::host::test_resource_validator()),
        &config,
        false,
    ));
    let direct_config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: "direct-store".into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let direct_binding = Arc::new(awaken_session_run_delivery::MemoryBinding::new(
        parent,
        "ws-test",
        Arc::new(PlatformMemoryHandle::from_roles(
            awaken_resource_contract::MemoryExecutionContent::new(content.clone()),
            "direct-store".into(),
            true,
        )),
        Some(crate::host::test_resource_validator()),
        &direct_config,
        true,
    ));
    recovered_host.run_assembly.register_thread_memory_bindings(
        parent,
        std::collections::HashMap::from([
            ("child-memory".into(), parent_binding.clone()),
            ("duplicate-alias".into(), parent_binding.clone()),
            ("read-only".into(), read_only_binding.clone()),
        ]),
    );
    recovered_host
        .run_assembly
        .installed_sessions
        .realizations()
        .update_realization(parent, |r| {
            r.staging_mut().memory = Some(direct_binding.clone())
        })
        .expect("installed Sessions");
    let first_recovery = recovered_host
        .run_assembly
        .bind_thread_memory_recovery(parent, parent_commit.clone())
        .await;
    assert!(
        first_recovery
            .unwrap_err()
            .to_string()
            .contains("transient recovery read"),
        "R4/E7 first wake exposes the repository failure"
    );
    assert_eq!(
        recovered_host
            .run_assembly
            .bind_thread_memory_recovery(parent, parent_commit.clone())
            .await
            .unwrap(),
        1,
        "R2/R3/R4 E3,E6,E7 next wake schedules one matching writable binding"
    );
    assert!(
        crate::test_application_bindings::lifecycle(
            recovered_host.as_ref(),
            recovered_host.worker_completion_sink()
        )
        .drain(Duration::from_secs(10))
        .await,
        "R2/E3 recovery drained"
    );
    assert!(
        parent_binding
            .extraction_commit()
            .expect("healthy Memory context")
            .is_some(),
        "R2/E6 mapped writable binding receives commit"
    );
    assert!(
        direct_binding
            .extraction_commit()
            .expect("healthy Memory context")
            .is_some(),
        "R3/E6 direct-only selected binding receives commit"
    );
    assert!(
        read_only_binding
            .extraction_commit()
            .expect("healthy Memory context")
            .is_none(),
        "R2/R3 E6 read-only binding never drives extraction"
    );

    let completed = recovered_repository
        .get_extraction(&intent_id)
        .await
        .expect("R2 query recovered intent")
        .expect("R2 recovered intent exists");
    assert_eq!(completed.status, MemoryExtractionStatus::Completed, "R2/E3");
    assert_eq!(completed.session_id, parent, "R2/E1");
    assert_eq!(completed.logical_thread_id(), child, "R2/E1");
    let auxiliary_thread = ThreadId(completed.auxiliary_thread_id());
    assert!(
        parent_commit.latest_run(&auxiliary_thread).is_some(),
        "R2/E4 auxiliary Run uses parent commit authority"
    );
    assert_eq!(
        content.list_versions("test-store").await.unwrap().len(),
        1,
        "R2/E5"
    );
}

#[tokio::test]
async fn database_less_worker_does_not_claim_memory_extraction_recovery() {
    // Cause/effect graph: C1 the Host owns local durable extraction authority
    // or is a database-less upstream Worker; C2 a frozen Memory binding is
    // writable or read-only; C3 the extraction repository is available or is
    // the Worker's fail-closed port; C4 a matching durable intent exists or
    // does not; C5 no automatic Memory binding is selected. Effects: E1 a
    // local writable owner schedules matching work;
    // E2 a local repository failure remains visible; E3 a read-only binding
    // never receives recovery execution; E4 an upstream Worker neither probes
    // the outbox nor binds a commit and returns normally; E5 direct Worker
    // authority access still fails closed. Constraints: the Coordinator's
    // guarded settlement observer remains the sole remote terminal/extraction
    // owner; the Worker constructor selects the one Host role, while no
    // Memory-specific role flag, repository, or reconciler is added.
    //
    // | Rule | role     | binding/use          | repository  | intent | C5 | effects |
    // | R1   | local    | RW                   | available   | yes    | any | E1      |
    // | R2   | local    | RW                   | fails       | any    | any | E2      |
    // | R3   | local    | RO                   | any         | any    | any | E3      |
    // | R4   | upstream | RW                   | unavailable | any    | yes | E4      |
    // | R5   | upstream | direct authority call| unavailable | any   | any | E5      |
    // R0 the actual Worker constructor gives the upstream authority before any
    // optional claimed-commit port; W1-W3 in commit_ingest cover missing/extra ports.
    // R1-R3 are exercised by
    // `parent_rehydration_recovers_a_child_intent_after_enqueue_crash`;
    // R5 is owned by `every_worker_extraction_authority_operation_fails_closed`.
    let session = "database-less-worker-memory-session";
    let content = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let host = (crate::SessionExecutorBuilder::for_worker(
        Arc::new(ExtractorModel),
        "stub",
        {
            let memory = content.clone();
            crate::WorkerContentAdapters {
                file_content_source: Arc::new(
                    awaken_resource_contract::UnavailableFileContentSource,
                ),
                memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
            }
        },
        crate::SessionExecutionProcess::from_deployment(
            &awaken_deployment_config::DeploymentConfig::ephemeral(),
            awaken_deployment_config::mint_local_workspace(),
            awaken_time::system_clock(),
            false,
        )
        .expect("ephemeral process"),
        crate::host::test_upstream_authority(),
        awaken_worker_contract::RegisteredCapacity::of(
            &awaken_worker_contract::WorkerManifest::default(),
        )
        .unwrap(),
    )
    .expect("complete Worker connections"))
    .build()
    .expect("validated Host dependencies");
    assert!(
        !host
            .run_assembly
            .authority
            .observes_terminal_memory_locally(),
        "the Worker constructor alone gives the Host its upstream authority"
    );
    let config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: "worker-memory-store".into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let binding = Arc::new(awaken_session_run_delivery::MemoryBinding::new(
        session,
        "worker-memory-workspace",
        Arc::new(PlatformMemoryHandle::from_roles(
            awaken_resource_contract::MemoryExecutionContent::new(content),
            config.memory_store_id.to_string(),
            true,
        )),
        Some(crate::host::test_resource_validator()),
        &config,
        true,
    ));
    host.run_assembly.register_thread_memory_bindings(
        session,
        std::collections::HashMap::from([("memory".into(), binding.clone())]),
    );
    assert!(
        host.run_assembly
            .memory_for_thread(session)
            .expect("healthy registry")
            .is_none(),
        "R4/C5 no automatic Memory binding is selected"
    );
    let commit = host
        .run_assembly
        .authority
        .open_for_read(session)
        .await
        .expect("R4 Worker commit");

    assert_eq!(
        host.run_assembly
            .bind_thread_memory_recovery(session, commit)
            .await
            .expect("R4 skips Coordinator-owned extraction recovery"),
        0,
        "R4/E4 no Worker recovery is scheduled"
    );
    assert!(
        binding
            .extraction_commit()
            .expect("healthy Memory context")
            .is_none(),
        "R4/E4 the Worker does not bind Coordinator extraction execution"
    );
}

struct UnavailableExtractorMaterializer(Arc<std::sync::atomic::AtomicUsize>);

impl awaken_inference_endpoint::InferenceExecutorMaterializer for UnavailableExtractorMaterializer {
    fn materialize_pinned(
        &self,
        _candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
}

fn unavailable_extractor(
    bound: &BoundMemory,
    materializations: Arc<std::sync::atomic::AtomicUsize>,
) -> BoundMemory {
    Arc::new(MemoryRuntime::new(
        Arc::new(crate::NoModelConfiguredExecutor),
        bound.runtime.provider.clone(),
        bound.runtime.background(),
        bound.runtime.extraction_repository(),
        bound.runtime.clock.clone(),
        Some(Arc::new(UnavailableExtractorMaterializer(materializations))),
    ))
    .bound(bound.binding.clone())
}

fn recovery_intent(session: &str) -> MemoryExtractionIntent {
    use awaken_time::WallClockExt as _;
    let mut intent = MemoryExtractionIntent::new_range(
        format!("recovery-{session}"),
        format!("{session}:terminal"),
        "ws-test",
        session,
        "terminal",
        "test-store",
        1,
        0,
        1,
        vec![user("I like rust")],
        extractor(None, None),
    )
    .unwrap();
    intent
        .claim("owner", awaken_time::system_clock().now_ms(), 60_000)
        .unwrap();
    intent
}

#[tokio::test]
async fn committed_extraction_recovery_does_not_materialize_an_unavailable_model() {
    // Cause/effect table: R1 the real extractor committed a valid proposal;
    // R2 it committed a malformed batch. C1 the governed content was not yet
    // applied; C2 recovery has neither a working materializer nor a default
    // model. Both rows must return the original exact outcome with zero new
    // materialization requests. R1 retains its planned CAS token; R2 retains
    // the permanent refusal. Neither row mutates the claim or content itself.
    for malformed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let model: Arc<dyn LlmExecutor> = if malformed {
            Arc::new(MalformedExtractorModel)
        } else {
            Arc::new(ExtractorModel)
        };
        let (_runtime, bound, content, intents) =
            bound_test_memory(model, "committed-recovery", root.path(), root.path()).await;
        let intent = recovery_intent("committed-recovery");
        intents
            .put_extraction_if_absent(intent.clone())
            .await
            .unwrap();
        // Leave the durable controller at Claimed, as after a crash between
        // the extractor's committed terminal and mark_extracted.
        let original = bound.extract_mutations(&intent).await;
        assert_eq!(
            matches!(original, MemoryExtractionOutcome::PermanentFailure { .. }),
            malformed
        );
        if !malformed {
            assert!(
                matches!(&original, MemoryExtractionOutcome::Mutations(writes) if writes.len() == 1)
            );
        }
        let materializations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recovered = unavailable_extractor(&bound, materializations.clone())
            .extract_mutations(&intent)
            .await;
        assert_eq!(recovered, original);
        assert_eq!(materializations.load(Ordering::SeqCst), 0);
        assert_eq!(
            intents.get_extraction(&intent.intent_id).await.unwrap(),
            Some(intent)
        );
        assert!(
            content
                .list_versions("test-store")
                .await
                .unwrap()
                .is_empty()
        );
    }
}

struct PausedExtractorModel {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl LlmExecutor for PausedExtractorModel {
    async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
        ExtractorModel.infer(request).await
    }
}

#[tokio::test]
async fn running_extractor_recovery_preserves_uncertainty_without_reentry() {
    // C1 a real extractor has durably begun and entered its first model call;
    // C2 that call is held at an explicit semaphore; C3 a second observer has
    // an unavailable materializer. U1 C1+C2+C3 => the same auxiliary Run is
    // ExecutionIndeterminate, zero new materialization and no second model
    // request. U2 release C2 => the original Run alone completes its proposal.
    // JoinSet owns cancellation on assertion failure; no detached test work or
    // timing-based sleep creates the Running state.
    let root = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (_runtime, bound, _content, intents) = bound_test_memory(
        Arc::new(PausedExtractorModel {
            entered: entered.clone(),
            release: release.clone(),
            calls: calls.clone(),
        }),
        "running-recovery",
        root.path(),
        root.path(),
    )
    .await;
    let intent = recovery_intent("running-recovery");
    intents
        .put_extraction_if_absent(intent.clone())
        .await
        .unwrap();
    let original = bound.clone();
    let admitted = intent.clone();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { original.extract_mutations(&admitted).await });
    tokio::time::timeout(Duration::from_secs(10), entered.acquire())
        .await
        .expect("the first model call was entered")
        .unwrap()
        .forget();
    let materializations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recovered = unavailable_extractor(&bound, materializations.clone())
        .extract_mutations(&intent)
        .await;
    assert!(
        matches!(recovered, MemoryExtractionOutcome::ExecutionIndeterminate { auxiliary_run_id, .. } if auxiliary_run_id == intent.auxiliary_run_id())
    );
    assert_eq!(materializations.load(Ordering::SeqCst), 0, "U1");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "U1");
    release.add_permits(1);
    let completed = tasks.join_next().await.unwrap().unwrap();
    assert!(
        matches!(completed, MemoryExtractionOutcome::Mutations(writes) if writes.len() == 1),
        "U2"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "U2");
}

#[tokio::test]
async fn extraction_recovery_refuses_a_worker_projection_as_committed_authority() {
    // A1 the binding has a Remote Worker projection instead of the original
    // committed authority => permanent refusal before materialization or model
    // IO. The existing Worker-placement test covers ordinary no-binding entry;
    // this row closes explicit misuse of the public binding attachment API.
    let root = tempfile::tempdir().unwrap();
    let (_runtime, bound, _content, _intents) = bound_test_memory(
        Arc::new(crate::NoModelConfiguredExecutor),
        "remote-recovery",
        root.path(),
        root.path(),
    )
    .await;
    bound
        .binding
        .attach_extraction_commit(Arc::new(SessionCommit::Remote(
            awaken_session_run_delivery::WorkerRecoveryCommit::new(),
        )))
        .unwrap();
    let materializations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outcome = unavailable_extractor(&bound, materializations.clone())
        .extract_mutations(&recovery_intent("remote-recovery"))
        .await;
    assert!(matches!(
        outcome,
        MemoryExtractionOutcome::PermanentFailure { .. }
    ));
    assert_eq!(materializations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unreadable_committed_extraction_source_never_materializes_a_model() {
    // F1 the real SQLite recovery source is attached but its pool is closed;
    // F2 the extraction intent and governed Memory store remain healthy.
    // Effect: return the existing Retryable storage outcome before any provider
    // materialization or model request; retain the exact claim/content. Closing
    // this test-owned in-memory pool exercises the real read failure, without
    // another read facade or a fabricated empty committed prefix.
    let root = tempfile::tempdir().unwrap();
    let (_runtime, bound, content, intents) = bound_test_memory(
        Arc::new(crate::NoModelConfiguredExecutor),
        "unreadable-recovery",
        root.path(),
        root.path(),
    )
    .await;
    let store = Arc::new(
        awaken_store_sqlite::SqliteCommitCoordinator::open_in_memory()
            .await
            .unwrap(),
    );
    bound
        .binding
        .attach_extraction_commit(Arc::new(SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected_shared(store.clone()),
        ))))
        .unwrap();
    store.close().await;
    let intent = recovery_intent("unreadable-recovery");
    intents
        .put_extraction_if_absent(intent.clone())
        .await
        .unwrap();
    let materializations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outcome = unavailable_extractor(&bound, materializations.clone())
        .extract_mutations(&intent)
        .await;
    let MemoryExtractionOutcome::Retryable { reason } = outcome else {
        panic!("closed committed source must stay retryable: {outcome:?}");
    };
    assert!(
        reason.contains("closed"),
        "the real closed-pool read must be reached before model preparation: {reason}"
    );
    assert_eq!(materializations.load(Ordering::SeqCst), 0);
    assert_eq!(
        intents.get_extraction(&intent.intent_id).await.unwrap(),
        Some(intent)
    );
    assert!(
        content
            .list_versions("test-store")
            .await
            .unwrap()
            .is_empty()
    );
}
