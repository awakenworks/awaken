//! Trace content-capture gate (#3, ADR-0050): prompt/completion text is personal
//! data, so it is recorded only when the resolved capture level permits it and
//! only after passing the redactor. The default level (`Structured`) records no
//! content at all — the guarantee that keeps content out of exported telemetry.
//! A `SpyCaptureSink` observes exactly what the engine would persist.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::data_subject::{CaptureSink, DataSubjectId, Purpose};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, StopReason,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::{CaptureDecision, ContentCapture, ContentKind, ContentRedactor};

/// Records every content item the engine persists.
#[derive(Default)]
struct SpyCaptureSink {
    items: Mutex<Vec<(ContentKind, String)>>,
}

#[async_trait::async_trait]
impl CaptureSink for SpyCaptureSink {
    async fn record(
        &self,
        _subject: &DataSubjectId,
        _purpose: Purpose,
        kind: ContentKind,
        content: &str,
    ) {
        self.items.lock().unwrap().push((kind, content.to_string()));
    }
}

/// Replaces the literal `secret-token` with `[REDACTED]`, proving the recorded
/// text is the scrubbed projection, never the raw content.
struct ScrubRedactor;
impl ContentRedactor for ScrubRedactor {
    fn redact<'a>(&self, _kind: ContentKind, text: &'a str) -> Cow<'a, str> {
        if text.contains("secret-token") {
            Cow::Owned(text.replace("secret-token", "[REDACTED]"))
        } else {
            Cow::Borrowed(text)
        }
    }
}

struct OkLlm;
#[async_trait::async_trait]
impl LlmExecutor for OkLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done secret-token".to_string()),
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
        })
    }
}

fn install(runtime: &Runtime) {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                model_candidates: Vec::new(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hello secret-token")],
        }],
        delegation_origin: None,
        model_ref_override: None,
    }
}

fn run_context(sink: &Arc<SpyCaptureSink>, capture: CaptureDecision) -> RuntimeRunContext {
    RuntimeRunContext::new()
        .with_commit(Arc::new(MemoryCommitCoordinator::new()))
        .with_capture(capture)
        .with_capture_sink(
            DataSubjectId("dsub-1".to_string()),
            sink.clone() as Arc<dyn CaptureSink>,
        )
}

#[tokio::test]
async fn structured_default_records_no_content() {
    let sink = Arc::new(SpyCaptureSink::default());
    let runtime = Runtime::new().with_llm(Arc::new(OkLlm));
    install(&runtime);

    // Default decision is `Structured` — the safe operational default.
    let ctx = run_context(&sink, CaptureDecision::default());
    runtime.execute(activation(), ctx).await.expect("runs");

    assert!(
        sink.items.lock().unwrap().is_empty(),
        "Structured must persist no prompt/completion content"
    );
}

#[tokio::test]
async fn full_records_input_and_output_content() {
    let sink = Arc::new(SpyCaptureSink::default());
    let runtime = Runtime::new().with_llm(Arc::new(OkLlm));
    install(&runtime);

    let ctx = run_context(&sink, CaptureDecision::new(ContentCapture::Full));
    runtime.execute(activation(), ctx).await.expect("runs");

    let items = sink.items.lock().unwrap();
    let kinds: Vec<ContentKind> = items.iter().map(|(k, _)| *k).collect();
    assert!(
        kinds.contains(&ContentKind::InputMessages),
        "Full records the prompt"
    );
    assert!(
        kinds.contains(&ContentKind::OutputMessages),
        "Full records the completion"
    );
    // The raw content is present because no redactor was set (NoopRedactor).
    let joined: String = items.iter().map(|(_, t)| t.clone()).collect();
    assert!(joined.contains("secret-token"));
}

#[tokio::test]
async fn full_with_redactor_persists_only_the_scrubbed_projection() {
    let sink = Arc::new(SpyCaptureSink::default());
    let runtime = Runtime::new().with_llm(Arc::new(OkLlm));
    install(&runtime);

    let decision = CaptureDecision::with_redactor(ContentCapture::Full, Arc::new(ScrubRedactor));
    let ctx = run_context(&sink, decision);
    runtime.execute(activation(), ctx).await.expect("runs");

    let items = sink.items.lock().unwrap();
    let joined: String = items.iter().map(|(_, t)| t.clone()).collect();
    assert!(!joined.is_empty(), "Full still records content");
    // The redactor scrubbed the sensitive token from both prompt and completion.
    assert!(
        !joined.contains("secret-token"),
        "raw content must never be persisted: {joined:?}"
    );
    assert!(joined.contains("[REDACTED]"), "scrubbed marker present");
}
