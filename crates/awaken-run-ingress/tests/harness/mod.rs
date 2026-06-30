//! Shared runtime harness for the durable-ingress integration tests.
//!
//! It builds a real `Runtime` with deterministic model providers — a plain text
//! model whose runs end naturally, and a tool-then-text model that parks on a
//! gate so the resume path can be driven. The commit coordinator and dispatch
//! store are supplied by each test (in-memory or Postgres), so this harness is
//! storage-agnostic and shared by every suite.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

pub const FP: &str = "catalog-a";
pub const SNAP: &str = "snapshot-1";
pub const THREAD: &str = "thread-1";
pub const TICKET: &str = "ticket-1";

/// Always answers with fixed text — a fresh run ends naturally in one step.
struct TextLlm(&'static str);
#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0.to_string()),
            usage: None,
        })
    }
}

/// Calls `echo` once, then ends with text — drives the park/resume path.
struct ToolThenText {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "ping"}),
            }])
        } else {
            AssistantOutput::text("all done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

struct EchoTool {
    ran: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, "echoed: ping"))
    }
}

struct SuspendGate;
#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(&self, _c: &PermissionContext) -> GateOutcome {
        GateOutcome::Suspend {
            ticket_id: TICKET.to_string(),
        }
    }
}

pub fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_instance_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            tool_descriptors: vec![ToolDescriptor::pinned(
                "test",
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids: Vec::new(),
        },
        fingerprint: fp,
    }
}

fn install(runtime: &Runtime) {
    let fp = CatalogFingerprint(FP.to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fp.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fp,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
    runtime.register_snapshot(snapshot());
}

/// A runtime with a plain text model — fresh runs end naturally.
pub fn text_runtime() -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm("done"))));
    install(&runtime);
    runtime
}

/// A runtime that parks on a tool gate, exposing the tool-run counter so a test
/// can assert the pending tool runs exactly once on resume.
pub fn tool_runtime() -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(SuspendGate)),
    );
    install(&runtime);
    (runtime, ran)
}

pub fn activation(run: &str) -> RunActivation {
    RunActivation {
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    }
}

// --- Live Postgres helpers, shared by the live suites -----------------------

use sqlx::postgres::PgPool;

/// The test database URL: `AWAKEN_TEST_DATABASE_URL`, or the local dev container.
pub fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

/// Connect, or return `None` with a skip notice when no Postgres is reachable so
/// the suite still passes on a machine without a database.
pub async fn pool() -> Option<PgPool> {
    match PgPool::connect(&database_url()).await {
        Ok(pool) => Some(pool),
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            None
        }
    }
}

/// Shared spec for revision-guarded pending edit/retract (M3a): every backend
/// must match this behaviour, so the test body lives here once.
pub async fn assert_pending_revision_cas<S: awaken_run_ingress::PendingInbox>(store: &S) {
    use awaken_run_ingress::{CasOutcome, PendingInput};
    let thread = ThreadId(THREAD.to_string());
    let input = |result| PendingInput {
        message_id: "m1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: thread.clone(),
        correlation_id: TICKET.to_string(),
        result,
    };
    store
        .append(input(ResumeResult::Input("a".to_string())))
        .await
        .unwrap();

    let records = store.list(&thread).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision, 1);

    // A stale-revision edit is rejected; the correct revision applies and bumps.
    assert_eq!(
        store
            .edit("m1", 99, ResumeResult::Input("b".to_string()))
            .await
            .unwrap(),
        CasOutcome::RevisionMismatch
    );
    assert_eq!(
        store
            .edit("m1", 1, ResumeResult::Input("b".to_string()))
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    let records = store.list(&thread).await.unwrap();
    assert_eq!(records[0].revision, 2);
    assert_eq!(
        records[0].input.result,
        ResumeResult::Input("b".to_string())
    );

    // Retract is likewise guarded; a stale revision fails, the current one wins.
    assert_eq!(
        store.retract("m1", 1).await.unwrap(),
        CasOutcome::RevisionMismatch
    );
    assert_eq!(store.retract("m1", 2).await.unwrap(), CasOutcome::Applied);
    assert_eq!(store.retract("m1", 2).await.unwrap(), CasOutcome::NotFound);
    assert!(store.list(&thread).await.unwrap().is_empty());
}

/// Drop every commit- and dispatch-schema table for a prefix (plus the shared
/// migration ledger) so each test starts from a clean, isolated schema.
pub async fn reset(pool: &PgPool, prefix: &str) {
    for table in [
        "commit",
        "message",
        "state_command",
        "event",
        "run_record",
        "waiting",
        "dispatch",
        "pending",
        "schema_migrations",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {prefix}_{table} CASCADE"))
            .execute(pool)
            .await;
    }
}
