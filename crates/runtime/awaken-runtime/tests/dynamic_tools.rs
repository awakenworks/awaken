//! A plugin whose tool set is dynamic (`live_version`) contributes tools that
//! reach the model and execute, and a version bump between steps re-resolves the
//! tool face at the step boundary — the seam MCP `tools/list_changed` uses.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, DynamicTool, Plugin, PluginManifest,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

/// A trivial dynamic tool that echoes its id.
struct EchoDyn(String);

#[async_trait::async_trait]
impl RawTool for EchoDyn {
    fn id(&self) -> &str {
        &self.0
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, format!("ran {}", self.0)))
    }
}

/// A plugin whose contributed tool set and `live_version` come from shared state,
/// so a test can mutate them mid-run.
struct DynPlugin {
    version: Arc<AtomicU64>,
    tools: Arc<Mutex<Vec<String>>>,
}

impl Plugin for DynPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "mcp:srv".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tool_namespaces: vec!["mcp__srv__".to_string()],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new("mcp:srv");
        for id in self.tools.lock().unwrap().iter() {
            contributions.dynamic_tools.push(DynamicTool {
                descriptor: ToolDescriptor::pinned(
                    "mcp",
                    id.clone(),
                    "dynamic",
                    serde_json::json!({ "type": "object" }),
                ),
                tool: Arc::new(EchoDyn(id.clone())),
            });
        }
        contributions
    }

    fn live_version(&self) -> Option<u64> {
        Some(self.version.load(Ordering::SeqCst))
    }
}

/// Records the tool ids visible on each inference; on the first step it also
/// bumps the plugin version and swaps the tool set (simulating list_changed),
/// then calls the step-0 tool. The second step ends the run.
struct RefreshProbe {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    version: Arc<AtomicU64>,
    tools: Arc<Mutex<Vec<String>>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for RefreshProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let visible: Vec<String> = request.tools.iter().map(|t| t.id.clone()).collect();
        self.seen.lock().unwrap().push(visible);
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            // Simulate a tools/list_changed: swap the set and advance the version.
            *self.tools.lock().unwrap() = vec!["mcp__srv__b".to_string()];
            self.version.fetch_add(1, Ordering::SeqCst);
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".to_string(),
                tool_id: "mcp__srv__a".to_string(),
                arguments: serde_json::json!({}),
            }])
        } else {
            AssistantOutput::text("done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
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
        .expect("catalog installs");
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
                max_steps: 8,
                model_binding: ModelBinding {
                    provider_instance_ref: "provider-1".to_string(),
                    model_ref: "model-1".to_string(),
                    backend_ref: "backend-1".to_string(),
                },
                // No static descriptors: every visible tool is dynamic.
                tool_descriptors: Vec::new(),
                plugin_ids: vec!["mcp:srv".to_string()],
                plugin_config: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("message-1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        trace: Default::default(),
    }
}

#[tokio::test]
async fn dynamic_tool_is_visible_executes_and_refreshes_at_the_step_boundary() {
    let version = Arc::new(AtomicU64::new(1));
    let tools = Arc::new(Mutex::new(vec!["mcp__srv__a".to_string()]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::new()
        .with_llm(Arc::new(RefreshProbe {
            seen: seen.clone(),
            version: version.clone(),
            tools: tools.clone(),
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(Arc::new(DynPlugin {
            version: version.clone(),
            tools: tools.clone(),
        }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "two inferences");
    // Step 0 saw the initial dynamic tool.
    assert_eq!(seen[0], vec!["mcp__srv__a".to_string()]);
    // Step 1 saw the refreshed set after the version bump — proving the
    // step-boundary re-resolution took effect.
    assert_eq!(seen[1], vec!["mcp__srv__b".to_string()]);

    // The step-0 dynamic tool actually executed (a model-visible tool result).
    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("ran mcp__srv__a")),
        "the dynamic tool executed"
    );
}
