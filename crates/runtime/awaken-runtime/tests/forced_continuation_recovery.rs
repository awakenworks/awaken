//! A run-end guard's forced-continuation budget is run-scoped truth: it is
//! recovered from the committed steer-feedback messages, so a run steered by a
//! guard and then parked mid-loop resumes with the count intact rather than
//! restarting at zero (CE-9). Without recovery a bounded guard would steer its
//! whole budget again after every park.

use std::sync::Arc;
use std::sync::Mutex;

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
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginManifest, RunEndContext, RunEndDecision,
    RunEndGuard,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";

/// Always ends its turn with text, so every step is a natural-end boundary the
/// run-end guard is consulted at.
struct TextLlm;
#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Steers while `forced_continuations < steer_budget` (recording each value it
/// saw), and requests a pause on its steering turn so the run parks at the very
/// next boundary — after the steer feedback is committed. Once the budget is
/// reached it completes.
struct PauseGuard {
    seen_fc: Arc<Mutex<Vec<usize>>>,
    pause: PauseSignal,
    steer_budget: usize,
}
#[async_trait::async_trait]
impl RunEndGuard for PauseGuard {
    fn id(&self) -> &str {
        "pauser"
    }
    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision {
        self.seen_fc.lock().unwrap().push(ctx.forced_continuations);
        if ctx.forced_continuations < self.steer_budget {
            // Park at the next boundary, after this steer's feedback commits.
            self.pause.request();
            RunEndDecision::Steer {
                feedback: "revise".to_string(),
                detail: serde_json::json!({ "fc": ctx.forced_continuations }),
            }
        } else {
            RunEndDecision::Complete {
                detail: serde_json::json!({ "done": true }),
            }
        }
    }
}

struct PauseGuardPlugin {
    seen_fc: Arc<Mutex<Vec<usize>>>,
    pause: PauseSignal,
    steer_budget: usize,
}
impl Plugin for PauseGuardPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "pauser".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                run_end_guards: IdBound::Exact(vec!["pauser".to_string()]),
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("pauser");
        c.run_end_guards.push(Arc::new(PauseGuard {
            seen_fc: self.seen_fc.clone(),
            pause: self.pause.clone(),
            steer_budget: self.steer_budget,
        }));
        c
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
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
            plugin_ids: vec!["pauser".to_string()],
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn install(runtime: &Runtime) {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
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
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        model_ref_override: None,
    }
}

fn resume_command() -> ResumeCommand {
    ResumeCommand {
        // A manual-pause ticket is correlated by the run id.
        correlation_id: "run-1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result: ResumeResult::Input("keep going".to_string()),
        now_ms: 0,
    }
}

#[tokio::test]
async fn a_guards_forced_continuation_count_survives_a_park_and_resume() {
    let seen_fc = Arc::new(Mutex::new(Vec::new()));
    let pause = PauseSignal::new();
    let runtime = Runtime::new()
        .with_llm(Arc::new(TextLlm))
        .with_plugin(Arc::new(PauseGuardPlugin {
            seen_fc: seen_fc.clone(),
            pause: pause.clone(),
            steer_budget: 1,
        }));
    install(&runtime);
    runtime.register_snapshot(snapshot());

    let commit = Arc::new(MemoryCommitCoordinator::new());

    // First leg: the guard steers once (fc 0 -> 1) and requests a pause, so the run
    // parks at the next boundary with one steer-feedback message committed.
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_pause(pause.clone());
    let phase = runtime
        .execute(activation(), context)
        .await
        .expect("first leg runs");
    assert_eq!(phase, Phase::Waiting, "the steered run parked");
    assert_eq!(*seen_fc.lock().unwrap(), vec![0], "the guard steered once");

    // Second leg: resume WITHOUT a pause. The forced-continuation count is recovered
    // from the committed steer message (= 1), so the bounded guard completes at once
    // rather than steering its whole budget again.
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime
        .resume(resume_command(), commit.as_ref(), context)
        .await
        .expect("resume runs");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    // The guard observed 0 before the park and 1 after — the count was recovered,
    // not reset. A reset would show a second 0 (steering the budget over again).
    assert_eq!(
        *seen_fc.lock().unwrap(),
        vec![0, 1],
        "forced_continuations was recovered across the park, not reset to 0"
    );
}
