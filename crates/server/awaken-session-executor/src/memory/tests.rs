use super::*;
use crate::SharedHost;
use async_trait::async_trait;
use awaken_ext_memory::{MEMORY_AGENT_ID, MemoryExtractionStatus, RecallSelector as _};
use awaken_resource_contract::{MemoryVersionQueries as _, MemoryWriter as _};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult, ToolCall,
};

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

#[test]
fn committed_agent_tool_calls_rebuild_only_policy_accepted_mutations() {
    let messages = vec![Message::new(
        MessageId("assistant".into()),
        Role::Assistant,
        vec![
            ContentBlock::tool_use(
                "valid",
                "write_memory",
                serde_json::json!({
                    "name": "User Pref",
                    "content": "The user prefers concise reviews."
                }),
            ),
            ContentBlock::tool_use(
                "rejected",
                "write_memory",
                serde_json::json!({
                    "name": "fix",
                    "content": "Fixed the bug by patching queue.rs."
                }),
            ),
            ContentBlock::tool_use(
                "other",
                "unrelated",
                serde_json::json!({"name": "ignored", "content": "ignored"}),
            ),
        ],
    )];

    assert_eq!(
        writes_from_committed_agent(&messages),
        BTreeMap::from([(
            "/User-Pref.md".to_string(),
            "The user prefers concise reviews.".to_string()
        )])
    );
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
    let platform = Arc::new(PlatformMemoryHandle::new(
        repository.clone(),
        "test-store".into(),
        true,
    ));
    let bound = runtime.bound(Arc::new(crate::memory_binding::MemoryBinding::new(
        session_id,
        "ws-test",
        platform,
        Some(crate::host::test_resource_validator()),
        &config,
        true,
    )));
    bound
        .binding()
        .attach_extraction_commit(Arc::new(HostCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            )),
        ))))
        .expect("healthy Memory context");
    (runtime, bound, repository, extractions)
}

struct RecallSnapshotRepository {
    heads: Vec<awaken_memory_store::Memory>,
}

#[async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemorySnapshotSource for RecallSnapshotRepository {
    async fn snapshot_heads(
        &self,
        _store: &str,
    ) -> Result<Vec<awaken_memory_store::Memory>, awaken_memory_store::MemErr> {
        Ok(self.heads.clone())
    }
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
    let commit = Arc::new(HostCommit::Local(Arc::new(
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
    assert!(runtime.drain(Duration::from_secs(10)).await);

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

#[tokio::test]
async fn platform_handle_enforces_read_only_at_the_data_plane_boundary() {
    let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    fs.create("store-a", "/existing.md", "safe").await.unwrap();
    let read_only = PlatformMemoryHandle::new(fs.clone(), "store-a".into(), false);

    let error = read_only
        .write("new", "must not persist")
        .await
        .unwrap_err();
    assert!(error.contains("read-only"));
    assert!(
        awaken_resource_contract::MemoryQueries::get_by_path(fs.as_ref(), "store-a", "/new.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(read_only.entries().await.unwrap().len(), 1);
}

#[tokio::test]
async fn platform_semantic_mutations_preserve_repository_cas_and_aba_fences() {
    // Cause/effect decision table: R1 absent expected generation + absent path ->
    // create; R2 absent generation + present path -> path conflict; R3 current
    // generation -> update; R4 stale generation -> conflict/no clobber; R5 exact
    // generation -> delete; R6 recreated path + stale generation -> no ABA delete.
    let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let handle = PlatformMemoryHandle::new(fs.clone(), "store".into(), true);
    let created = handle
        .write_exact("/note.md", "v1", None)
        .await
        .expect("R1");
    assert!(
        handle
            .write_exact("/note.md", "duplicate", None)
            .await
            .is_err(),
        "R2"
    );
    let updated = handle
        .write_exact("/note.md", "v2", Some(&created.generation()))
        .await
        .expect("R3");
    assert!(
        handle
            .write_exact("/note.md", "clobber", Some(&created.generation()))
            .await
            .is_err(),
        "R4"
    );
    assert_eq!(
        handle
            .read("/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "R4"
    );
    assert!(
        handle
            .delete_exact("/note.md", &updated.generation())
            .await
            .expect("R5"),
        "R5"
    );
    let recreated = handle
        .write_exact("/note.md", "v3", None)
        .await
        .expect("recreate");
    assert!(
        handle
            .delete_exact("/note.md", &updated.generation())
            .await
            .is_err(),
        "R6"
    );
    assert_eq!(
        handle.read("/note.md").await.unwrap().unwrap().id,
        recreated.id,
        "R6"
    );
}

#[tokio::test]
async fn planned_memory_head_is_an_opaque_token_only_the_store_decodes() {
    // Cause/effect decision table: T1 a planned mutation over an existing path
    // carries the observed head as a token; T2 the token is the store's own
    // head encoding; T3 a token the store cannot decode is refused and nothing
    // is written (never read as a stale head); T4 the planned token applies.
    let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let handle = PlatformMemoryHandle::new(fs.clone(), "store".into(), true);
    let created = handle
        .write_exact("/note.md", "v1", None)
        .await
        .expect("seed");
    let planned = handle
        .plan_mutations(std::collections::BTreeMap::from([(
            "/note.md".to_string(),
            "v2".to_string(),
        )]))
        .await
        .expect("T1");
    let token = planned[0].observed_generation.clone().expect("T1");
    assert_eq!(
        token.encoded(),
        &serde_json::to_value(created.generation()).unwrap(),
        "T2"
    );
    let mut unreadable = planned[0].clone();
    unreadable.observed_generation = Some(awaken_ext_memory::MemoryHeadToken::new(
        serde_json::json!("not a memory head"),
    ));
    let error = handle.apply_mutation(&unreadable).await.expect_err("T3");
    assert!(
        error.contains("unreadable observed memory head"),
        "T3: {error}"
    );
    let current = handle.read("/note.md").await.unwrap().unwrap();
    assert_eq!(current.content.as_deref(), Some("v1"), "T3");
    handle.apply_mutation(&planned[0]).await.expect("T4");
    let current = handle.read("/note.md").await.unwrap().unwrap();
    assert_eq!(current.content.as_deref(), Some("v2"), "T4");
}

/// Cause/effect decision table for Recall reads:
/// | Rule | atomic snapshot | legacy list/get | Effect |
/// |---|---|---|---|
/// | R1 | one non-empty head | panic if called | return one trimmed Recall entry |
/// | R2 | one blank head | panic if called | omit blank content |
#[tokio::test]
async fn recall_entries_use_only_the_atomic_memory_snapshot() {
    let memory = |id: &str, path: &str, content: &str| awaken_memory_store::Memory {
        id: id.into(),
        path: path.into(),
        content_sha256: awaken_memory_store::sha256_hex(content),
        content_size: content.len() as u64,
        version: 1,
        created_unix_nanos: 1,
        updated_unix_nanos: 2,
        content: Some(content.into()),
    };
    let source = RecallSnapshotRepository {
        heads: vec![
            memory("memory-a", "/a.md", "  remembered  "),
            memory("memory-blank", "/blank.md", "  "),
        ],
    };

    let entries = PlatformMemoryHandle::entries_from_snapshot(&source, "store-a")
        .await
        .expect("R1/R2");
    assert_eq!(entries.len(), 1, "R1/R2");
    assert_eq!(entries[0].path, std::path::PathBuf::from("/a.md"), "R1");
    assert_eq!(entries[0].content, "remembered", "R1");
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
    assert!(runtime.drain(Duration::from_secs(10)).await);

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
    assert!(runtime.drain(Duration::from_secs(10)).await);

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
    assert!(runtime.drain(Duration::from_secs(10)).await);

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
    assert!(runtime.drain(Duration::from_secs(10)).await);
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
    // Constraints: no child Session slot or physical commit partition exists;
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
    let platform = Arc::new(PlatformMemoryHandle::new(
        content.clone(),
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
    let child_binding = first_runtime.bound(Arc::new(crate::memory_binding::MemoryBinding::new(
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
    assert!(first_runtime.drain(Duration::from_secs(1)).await, "R1/E2");
    drop(child_binding);
    drop(first_runtime);
    drop(first_repository);

    let recovered_repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(&extraction_db)
            .await
            .expect("R2 reopen extraction repository"),
    );
    let recovered_host = (crate::HostDependencies::ephemeral(Arc::new(ExtractorModel), "stub")
        .await
        .with_memory_extraction_repository(Arc::new(FailFirstRecoverableRead::new(
            recovered_repository.clone(),
        ))))
    .build()
    .expect("validated Host dependencies");
    let parent_commit = Arc::new(HostCommit::Local(Arc::new(
        crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        )),
    )));
    let parent_binding = Arc::new(crate::memory_binding::MemoryBinding::new(
        parent,
        "ws-test",
        platform.clone(),
        Some(crate::host::test_resource_validator()),
        &config,
        true,
    ));
    let read_only_binding = Arc::new(crate::memory_binding::MemoryBinding::new(
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
    let direct_binding = Arc::new(crate::memory_binding::MemoryBinding::new(
        parent,
        "ws-test",
        Arc::new(PlatformMemoryHandle::new(
            content.clone(),
            "direct-store".into(),
            true,
        )),
        Some(crate::host::test_resource_validator()),
        &direct_config,
        true,
    ));
    recovered_host.register_thread_memory_bindings(
        parent,
        std::collections::HashMap::from([
            ("child-memory".into(), parent_binding.clone()),
            ("duplicate-alias".into(), parent_binding.clone()),
            ("read-only".into(), read_only_binding.clone()),
        ]),
    );
    recovered_host
        .installed_sessions
        .realizations()
        .update_realization(parent, |r| r.staging.memory = Some(direct_binding.clone()))
        .expect("installed Sessions");
    let first_recovery = recovered_host
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
            .bind_thread_memory_recovery(parent, parent_commit.clone())
            .await
            .unwrap(),
        1,
        "R2/R3/R4 E3,E6,E7 next wake schedules one matching writable binding"
    );
    assert!(
        recovered_host.drain_runtime(Duration::from_secs(10)).await,
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
    let host = (crate::HostDependencies::for_worker(
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
        crate::DeploymentConfig::ephemeral()
            .host_process(
                SharedHost::mint_local_workspace(),
                awaken_time::system_clock(),
            )
            .expect("ephemeral process"),
        crate::host::test_upstream_authority(),
        awaken_dispatch_contract::RegisteredCapacity::of(
            &awaken_dispatch_contract::WorkerManifest::default(),
        )
        .unwrap(),
    )
    .expect("complete Worker connections"))
    .build()
    .expect("validated Host dependencies");
    assert!(
        !host.authority.observes_terminal_memory_locally(),
        "the Worker constructor alone gives the Host its upstream authority"
    );
    let config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: "worker-memory-store".into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let binding = Arc::new(crate::memory_binding::MemoryBinding::new(
        session,
        "worker-memory-workspace",
        Arc::new(PlatformMemoryHandle::new(
            content,
            config.memory_store_id.to_string(),
            true,
        )),
        Some(crate::host::test_resource_validator()),
        &config,
        true,
    ));
    host.register_thread_memory_bindings(
        session,
        std::collections::HashMap::from([("memory".into(), binding.clone())]),
    );
    assert!(
        host.memory_for_thread(session).is_none(),
        "R4/C5 no automatic Memory binding is selected"
    );
    let commit = Arc::new(
        host.authority
            .open_commit(session)
            .await
            .expect("R4 Worker commit"),
    );

    assert_eq!(
        host.bind_thread_memory_recovery(session, commit)
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
