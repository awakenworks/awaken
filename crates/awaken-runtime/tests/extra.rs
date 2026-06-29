//! Small coverage for default/edge paths: empty capability catalog, the thread
//! reader on unknown ids, and a system-role message reaching inference.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::{RuntimeCapabilityCatalog, RuntimeCapabilitySource};
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

struct TextLlm;

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::Text("done".to_string()),
            usage: None,
        })
    }
}

#[test]
fn capability_source_returns_an_empty_default_before_install() {
    let runtime = Runtime::new();
    let caps = runtime.runtime_capabilities();
    assert!(caps.tools.is_empty());
    assert!(caps.plugins.is_empty());
    assert_eq!(caps.catalog_fingerprint, CatalogFingerprint(String::new()));
    assert!(!caps.runtime_version.is_empty());
}

#[test]
fn thread_reader_returns_empty_for_unknown_ids() {
    let commit = MemoryCommitCoordinator::new();
    assert!(
        commit
            .committed_messages(&ThreadId("nope".to_string()))
            .is_empty()
    );
    assert!(commit.waiting_ticket(&RunId("nope".to_string())).is_none());
}

#[tokio::test]
async fn a_system_role_message_is_carried_into_inference() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint.clone(),
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");

    let activation = RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                model_binding: ModelBinding {
                    provider_instance_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
            },
            fingerprint,
        },
        input: vec![
            Message {
                id: MessageId("sys".to_string()),
                role: Role::System,
                content: "be terse".to_string(),
            },
            Message {
                id: MessageId("m1".to_string()),
                role: Role::User,
                content: "hi".to_string(),
            },
        ],
        options: RunOptions {
            persistence: PersistenceMode::Disabled,
        },
        trace: Default::default(),
    };

    let outcome = runtime
        .execute(
            activation,
            RuntimeRunContext::new(PersistenceMode::Disabled),
        )
        .await
        .expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
}
