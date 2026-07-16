//! The run-end continuation guard mechanism (ADR: run-end guard). At a
//! natural-end boundary the runtime consults registered guards: a `Steer` injects
//! a feedback user turn and continues the loop; a `Complete` ends it carrying the
//! guard's opaque `detail`; with no guard the run just ends. The runtime's
//! `max_steps` remains the runaway backstop for a guard that always steers.

use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Fact};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginManifest, RunEndContext, RunEndDecision,
    RunEndGuard,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// A model that echoes the last user turn's text, so an injected feedback turn is
/// observable in the next committed assistant message.
struct EchoLlm;

#[async_trait::async_trait]
impl LlmExecutor for EchoLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, awaken_agent_contract::agent::message::Role::User))
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(last_user),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A guard that steers a fixed number of times (recording the `forced_continuations`
/// it saw each call), then completes. Feedback is the constant `"revise"`.
struct ScriptedGuard {
    steer_budget: usize,
    seen_fc: Arc<Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl RunEndGuard for ScriptedGuard {
    fn id(&self) -> &str {
        "scripted"
    }
    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision {
        self.seen_fc.lock().unwrap().push(ctx.forced_continuations);
        if ctx.forced_continuations < self.steer_budget {
            RunEndDecision::Steer {
                feedback: "revise".to_string(),
                detail: serde_json::json!({ "round": ctx.forced_continuations }),
            }
        } else {
            RunEndDecision::Complete {
                detail: serde_json::json!({ "done": true }),
            }
        }
    }
}

struct ScriptedGuardPlugin {
    steer_budget: usize,
    seen_fc: Arc<Mutex<Vec<usize>>>,
}

impl Plugin for ScriptedGuardPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "scripted".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                run_end_guards: IdBound::Exact(vec!["scripted".to_string()]),
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("scripted");
        c.run_end_guards.push(Arc::new(ScriptedGuard {
            steer_budget: self.steer_budget,
            seen_fc: self.seen_fc.clone(),
        }));
        c
    }
}

/// A guard whose `id()` is not in its plugin's bound — must fail the run closed.
struct RogueGuardPlugin;

impl Plugin for RogueGuardPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "rogue".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound::default(), // declares no guard
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new("rogue");
        c.run_end_guards.push(Arc::new(ScriptedGuard {
            steer_budget: 0,
            seen_fc: Arc::new(Mutex::new(Vec::new())),
        }));
        c
    }
}

/// Collects `Continuation` stream events surfaced during a run.
#[derive(Default)]
struct ContinuationCollector {
    events: Mutex<Vec<(bool, serde_json::Value)>>,
}

#[async_trait::async_trait]
impl Sink for ContinuationCollector {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        if let AgentEvent::Fact(Fact::Continuation { steered, detail }) = event.kind {
            self.events.lock().unwrap().push((steered, detail));
        }
        Ok(())
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

fn activation(plugin_ids: Vec<String>, max_steps: usize) -> RunActivation {
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
                max_steps,
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
        model_ref_override: None,
    }
}

/// Count committed user messages whose text equals `text`.
fn count_user_text(commit: &MemoryCommitCoordinator, text: &str) -> usize {
    commit
        .committed_messages(&ThreadId("thread-1".to_string()))
        .iter()
        .filter(|m| m.role == Role::User && m.text_content() == text)
        .count()
}

#[tokio::test]
async fn guard_steers_then_completes_and_surfaces_each_round() {
    let seen_fc = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(Arc::new(ScriptedGuardPlugin {
            steer_budget: 2,
            seen_fc: seen_fc.clone(),
        }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let collector = Arc::new(ContinuationCollector::default());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(collector.clone());

    let phase = runtime
        .execute(activation(vec!["scripted".to_string()], 16), context)
        .await
        .expect("runs");

    // Two steers then a completion → the run ends naturally.
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    // The guard saw the run-scoped counter advance 0, 1, 2.
    assert_eq!(*seen_fc.lock().unwrap(), vec![0, 1, 2]);
    // Three rounds surfaced: steer, steer, complete.
    let events = collector.events.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert!(events[0].0, "first round steered");
    assert!(events[1].0, "second round steered");
    assert!(!events[2].0, "final round completed, not steered");
    // The opaque detail is forwarded verbatim (the kernel never reads it).
    assert_eq!(events[0].1, serde_json::json!({ "round": 0 }));
    assert_eq!(events[2].1, serde_json::json!({ "done": true }));
    // Each steer injected the feedback as a committed user turn.
    assert_eq!(count_user_text(&commit, "revise"), 2);

    // Durable truth (not just the best-effort stream): every round was committed
    // as a `Continuation` event, transactionally with the run.
    let committed: Vec<serde_json::Value> = commit
        .committed()
        .events
        .into_iter()
        .filter(|e| e.kind == awaken_agent_contract::audit::kind::Kind::Continuation)
        .map(|e| e.payload)
        .collect();
    assert_eq!(
        committed.len(),
        3,
        "all rounds are durable, not just streamed"
    );
    assert_eq!(committed[0], serde_json::json!({ "round": 0 }));
    assert_eq!(committed[2], serde_json::json!({ "done": true }));
}

#[tokio::test]
async fn no_guard_ends_at_the_first_natural_end() {
    let seen_fc = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(Arc::new(ScriptedGuardPlugin {
            steer_budget: 5,
            seen_fc: seen_fc.clone(),
        }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let collector = Arc::new(ContinuationCollector::default());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(collector.clone());

    // The plugin is installed but not selected → its guard is inert.
    let phase = runtime
        .execute(activation(Vec::new(), 16), context)
        .await
        .expect("runs");

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(seen_fc.lock().unwrap().is_empty());
    assert!(collector.events.lock().unwrap().is_empty());
    assert_eq!(count_user_text(&commit, "revise"), 0);
}

#[tokio::test]
async fn an_always_steering_guard_is_bounded_by_max_steps() {
    let seen_fc = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(Arc::new(ScriptedGuardPlugin {
            steer_budget: usize::MAX, // never completes on its own
            seen_fc: seen_fc.clone(),
        }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let collector = Arc::new(ContinuationCollector::default());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(collector.clone());

    // max_steps = 3: each step natural-ends and the guard steers, so the runaway
    // backstop terminates the run rather than looping forever.
    let phase = runtime
        .execute(activation(vec!["scripted".to_string()], 3), context)
        .await
        .expect("runs");

    assert_eq!(phase, Phase::Ended(EndCause::MaxSteps));
    let events = collector.events.lock().unwrap();
    assert_eq!(
        events.len(),
        3,
        "one steer per step, then the ceiling stops it"
    );
    assert!(events.iter().all(|(steered, _)| *steered));
    assert_eq!(*seen_fc.lock().unwrap(), vec![0, 1, 2]);
}

#[tokio::test]
async fn a_guard_outside_its_bound_fails_the_run_closed() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(Arc::new(RogueGuardPlugin));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime
        .execute(activation(vec!["rogue".to_string()], 16), context)
        .await
        .expect("runs");

    assert_eq!(
        phase,
        Phase::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::CapabilityBound
        )),
        "a run-end guard outside the declared bound fails closed (G30)"
    );
}

// ── Multiple guards: consulted in order, first steer wins ───────────────────

/// A configurable guard: always complete, or steer while under a cap. It also
/// records the last message text it saw, to prove the runtime hands it the real
/// conversation.
#[derive(Clone, Copy)]
enum Behavior {
    Complete,
    SteerUntil(usize),
}

struct ProgrammableGuard {
    id: &'static str,
    behavior: Behavior,
    last_seen: Arc<Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl RunEndGuard for ProgrammableGuard {
    fn id(&self) -> &str {
        self.id
    }
    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision {
        *self.last_seen.lock().unwrap() = ctx.conversation.last().map(|m| m.text_content());
        let steer =
            matches!(self.behavior, Behavior::SteerUntil(n) if ctx.forced_continuations < n);
        if steer {
            RunEndDecision::Steer {
                feedback: "revise".to_string(),
                detail: serde_json::json!({ "g": self.id }),
            }
        } else {
            RunEndDecision::Complete {
                detail: serde_json::json!({ "g": self.id }),
            }
        }
    }
}

struct ProgrammableGuardPlugin {
    id: &'static str,
    behavior: Behavior,
    last_seen: Arc<Mutex<Option<String>>>,
}

impl Plugin for ProgrammableGuardPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: self.id.to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                run_end_guards: IdBound::Exact(vec![self.id.to_string()]),
                ..Default::default()
            },
        }
    }
    fn resolve(&self) -> Contributions {
        let mut c = Contributions::new(self.id);
        c.run_end_guards.push(Arc::new(ProgrammableGuard {
            id: self.id,
            behavior: self.behavior,
            last_seen: self.last_seen.clone(),
        }));
        c
    }
}

fn programmable(
    id: &'static str,
    behavior: Behavior,
) -> (Arc<ProgrammableGuardPlugin>, Arc<Mutex<Option<String>>>) {
    let last_seen = Arc::new(Mutex::new(None));
    (
        Arc::new(ProgrammableGuardPlugin {
            id,
            behavior,
            last_seen: last_seen.clone(),
        }),
        last_seen,
    )
}

#[tokio::test]
async fn a_later_guards_steer_overrides_an_earlier_guards_completion() {
    // g1 always completes; g2 (consulted after it) steers below its cap. Because
    // any guard steering keeps the run going, g2's steer wins over g1's completion
    // until g2's cap, then the run ends.
    let (g1, _) = programmable("g1", Behavior::Complete);
    let (g2, _) = programmable("g2", Behavior::SteerUntil(2));
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(g1)
        .with_plugin(g2);
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let collector = Arc::new(ContinuationCollector::default());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(collector.clone());

    let phase = runtime
        .execute(
            activation(vec!["g1".to_string(), "g2".to_string()], 16),
            context,
        )
        .await
        .expect("runs");

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let events = collector.events.lock().unwrap();
    // Two rounds steered by g2, then a completion.
    assert_eq!(events.len(), 3);
    assert!(
        events[0].0 && events[1].0,
        "g2 steered the first two rounds"
    );
    assert!(!events[2].0, "the run then completed");
    assert_eq!(count_user_text(&commit, "revise"), 2);
}

#[tokio::test]
async fn all_completing_guards_end_with_the_last_guards_detail() {
    // No guard steers → the run ends, carrying the last consulted guard's detail.
    let (g1, _) = programmable("g1", Behavior::Complete);
    let (g2, _) = programmable("g2", Behavior::Complete);
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(g1)
        .with_plugin(g2);
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let collector = Arc::new(ContinuationCollector::default());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(collector.clone());

    let phase = runtime
        .execute(
            activation(vec!["g1".to_string(), "g2".to_string()], 16),
            context,
        )
        .await
        .expect("runs");

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let events = collector.events.lock().unwrap();
    assert_eq!(events.len(), 1, "one completion round, no steers");
    assert!(!events[0].0);
    assert_eq!(
        events[0].1,
        serde_json::json!({ "g": "g2" }),
        "last guard's detail"
    );
    assert_eq!(count_user_text(&commit, "revise"), 0);
}

#[tokio::test]
async fn a_guard_receives_the_run_conversation() {
    // EchoLlm turns the input "go" into an assistant "go"; the guard must see that
    // committed transcript at the natural-end boundary.
    let (plugin, last_seen) = programmable("g1", Behavior::Complete);
    let runtime = Runtime::new()
        .with_llm(Arc::new(EchoLlm))
        .with_plugin(plugin);
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    runtime
        .execute(activation(vec!["g1".to_string()], 16), context)
        .await
        .expect("runs");

    assert_eq!(
        last_seen.lock().unwrap().as_deref(),
        Some("go"),
        "the guard saw the run's own assistant deliverable"
    );
}
