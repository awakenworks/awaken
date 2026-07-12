//! Active plugins contribute behavior only through declared seams: a phase hook
//! stages state via the commit path, and an out-of-bound plugin fails closed
//! (G9/G30). Plugins are resolved once per run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, Key, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, replay_state};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, PhaseContext, PhaseHook, PhaseHookPoint, PhaseReaction, Plugin,
    PluginManifest,
};
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
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A hook that stages one state command at StepStart.
struct MarkHook;

#[async_trait::async_trait]
impl PhaseHook for MarkHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::StepStart
    }
    async fn on_phase(&self, ctx: &PhaseContext, _conversation: &[Message]) -> PhaseReaction {
        PhaseReaction::state(vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "phase",
            serde_json::json!(format!("{:?}@{}", ctx.point, ctx.step)),
        )])
    }
}

/// A well-behaved plugin: declares the hook point and state key it uses, and
/// counts how many times it is resolved.
struct MarkPlugin {
    resolves: Arc<AtomicUsize>,
}

impl Plugin for MarkPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "mark".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tool_ids: Vec::new(),
                state_keys: vec!["phase".to_string()],
                phase_hooks: vec![PhaseHookPoint::StepStart],
                action_kinds: Vec::new(),
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let mut c = Contributions::new("mark");
        c.state_keys.push("phase".to_string());
        c.phase_hooks.push(Arc::new(MarkHook));
        c
    }
}

/// A misbehaving plugin: registers a hook point it never declared in its bound.
struct OutOfBoundPlugin;

impl Plugin for OutOfBoundPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "rogue".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound::default(),
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("rogue");
        c.phase_hooks.push(Arc::new(MarkHook)); // StepStart, not in the empty bound
        c
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

fn activation(plugin_ids: Vec<String>) -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids,
                plugin_config: Default::default(),
                context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        trace: Default::default(),
    }
}

#[tokio::test]
async fn active_plugin_hook_stages_state_through_the_commit_path() {
    let resolves = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(MarkPlugin {
            resolves: resolves.clone(),
        }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(vec!["mark".to_string()]), context)
        .await
        .expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    // The hook's state command was committed and is replayable.
    let store = replay_state(&commit.committed());
    assert!(store.get(Scope::Run, &Key("phase".into())).is_some());

    // The plugin resolved exactly once for the run.
    assert_eq!(resolves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn inactive_plugin_contributes_nothing() {
    let resolves = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(MarkPlugin {
            resolves: resolves.clone(),
        }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    // plugin_ids is empty: the plugin is inert and never resolved.
    runtime
        .execute(activation(Vec::new()), context)
        .await
        .expect("runs");

    assert!(commit.committed().state.is_empty());
    assert_eq!(resolves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn out_of_bound_plugin_fails_the_run_closed() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(OutOfBoundPlugin));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(vec!["rogue".to_string()]), context)
        .await
        .expect("runs");

    assert_eq!(
        outcome,
        Phase::Ended(EndCause::Error(Failure::CapabilityBound)),
        "a contribution outside the declared bound fails closed (G30)"
    );
}
