//! End-to-end tests with in-memory fakes: a scripted duplex channel stands in for
//! the launched CLI — no daemon, no network.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_agent_contract::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

struct FakeProcess;

#[async_trait]
impl ProcessHandle for FakeProcess {
    fn id(&self) -> &str {
        "fake-proc"
    }
    async fn wait(&self) -> std::result::Result<ExitStatus, SandboxError> {
        Ok(ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }
    async fn poll(&self) -> std::result::Result<Option<ExitStatus>, SandboxError> {
        Ok(Some(ExitStatus {
            code: Some(0),
            signaled: false,
        }))
    }
    async fn signal(&self, _signal: Signal) -> std::result::Result<(), SandboxError> {
        Ok(())
    }
}

/// A source that scripts a duplex agent: reads the prompt line, emits `frames`.
struct ScriptedSource {
    frames: Vec<String>,
    /// When set, `open` fails with this launch fault instead of scripting a turn.
    open_error: Option<String>,
}

#[async_trait]
impl AgentChannelSource for ScriptedSource {
    async fn open(
        &self,
        _activation: &RunActivation,
    ) -> std::result::Result<AgentSession, OpenError> {
        if let Some(e) = &self.open_error {
            return Err(OpenError(e.clone()));
        }
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let frames = self.frames.clone();
        tokio::spawn(async move {
            let mut prompt = String::new();
            let mut reader = BufReader::new(&mut theirs);
            let _ = reader.read_line(&mut prompt).await;
            for f in frames {
                let _ = theirs.write_all(f.as_bytes()).await;
                let _ = theirs.write_all(b"\n").await;
                let _ = theirs.flush().await;
            }
        });
        Ok(AgentSession {
            channel: Box::new(ours),
            process: Arc::new(FakeProcess),
        })
    }
}

#[derive(Default)]
struct RecordingCoordinator {
    commits: Mutex<Vec<ThreadCommit>>,
}

#[async_trait]
impl Coordinator for RecordingCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
        self.commits.lock().unwrap().push(commit);
        Ok(CommitRecord { sequence: 1 })
    }
}

pub(crate) fn activation() -> RunActivation {
    RunActivation {
        run_id: RunId("run-1".into()),
        thread_id: ThreadId("thread-1".into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                model_binding: ModelBinding::new("prov", "model", "acp:claude"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        input: vec![Message::text(MessageId("u1".into()), Role::User, "do it")],
        trace: Default::default(),
    }
}

fn exec(frames: Vec<String>) -> AcpRunExecutor {
    AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames,
        open_error: None,
    }))
}

#[tokio::test]
async fn drives_a_turn_commits_messages_and_returns_natural_end() {
    let e = exec(vec![
        r#"{"type":"message","text":"working"}"#.into(),
        r#"{"type":"message","text":"done"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].messages.len(), 2);
    assert_eq!(commits[0].messages[0].text_content(), "working");
    assert_eq!(
        commits[0].run_fact.phase,
        Phase::Ended(EndCause::NaturalEnd)
    );
}

#[tokio::test]
async fn a_truncated_stream_is_classified_and_surfaces_an_error_prompt() {
    // Agent emits a message then closes without a turn_end → AcpError::Truncated.
    let e = exec(vec![r#"{"type":"message","text":"partial"}"#.into()]);
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(matches!(phase, Phase::Ended(EndCause::Error(_))));
    let commits = coord.commits.lock().unwrap();
    // The partial message plus the appended classified error prompt were committed.
    let last = commits[0].messages.last().unwrap().text_content();
    assert!(last.contains("failed") || last.contains("interruption") || last.contains("stopped"));
}

#[tokio::test]
async fn a_launch_fault_classifies_at_initialize_and_commits_a_prompt() {
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("401 Unauthorized: invalid api key".into()),
    }));
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(matches!(phase, Phase::Ended(EndCause::Error(_))));
    let commits = coord.commits.lock().unwrap();
    let prompt = commits[0].messages[0].text_content();
    // Credential-rejection prompt (auth error) surfaced to the run.
    assert!(prompt.contains("credential"));
}

#[tokio::test]
async fn refusal_maps_to_stopped() {
    let e = exec(vec![r#"{"type":"turn_end","reason":"refusal"}"#.into()]);
    let phase = e
        .execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert!(matches!(phase, Phase::Ended(EndCause::Stopped(_))));
}

// ── R3: DispatchRunExecutor routes by runtime_adapter ────────────────────────

struct Spy(&'static str, Arc<Mutex<Vec<&'static str>>>);

#[async_trait]
impl RunExecutor for Spy {
    async fn execute(
        &self,
        _a: RunActivation,
        _c: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<Phase> {
        self.1.lock().unwrap().push(self.0);
        Ok(Phase::Ended(EndCause::NaturalEnd))
    }
}

#[tokio::test]
async fn dispatch_routes_by_runtime_adapter() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let dispatch = DispatchRunExecutor::new(
        Arc::new(Spy("native", hits.clone())),
        Arc::new(Spy("acp", hits.clone())),
    );

    // backend_ref "default" → runtime_adapter "awaken" → native.
    let mut native_act = activation();
    native_act.snapshot.resolved_spec.model_binding = ModelBinding::new("prov", "model", "default");
    dispatch
        .execute(native_act, RuntimeRunContext::new())
        .await
        .unwrap();
    // The default fixture's backend_ref is "acp:claude" → runtime_adapter → acp.
    dispatch
        .execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();

    assert_eq!(*hits.lock().unwrap(), vec!["native", "acp"]);
}

// ── R7: ACP mid-switch relaunches the CLI per turn ───────────────────────────

#[tokio::test]
async fn acp_relaunches_the_cli_every_turn_so_a_model_switch_takes_effect() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSource(Arc<AtomicUsize>);
    #[async_trait]
    impl AgentChannelSource for CountingSource {
        async fn open(&self, _a: &RunActivation) -> std::result::Result<AgentSession, OpenError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let (ours, mut theirs) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut p = String::new();
                let mut r = BufReader::new(&mut theirs);
                let _ = r.read_line(&mut p).await;
                let _ = theirs
                    .write_all(b"{\"type\":\"turn_end\",\"reason\":\"natural_end\"}\n")
                    .await;
                let _ = theirs.flush().await;
            });
            Ok(AgentSession {
                channel: Box::new(ours),
                process: Arc::new(FakeProcess),
            })
        }
    }

    let opens = Arc::new(AtomicUsize::new(0));
    let exec = AcpRunExecutor::new(Arc::new(CountingSource(opens.clone())));
    assert_eq!(exec.model_switch(), ModelSwitch::Relaunch);

    // Two turns → two launches: an ACP thread relaunches its CLI each turn, which
    // is how a re-staged model takes effect (R7).
    exec.execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    exec.execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 2);
}

/// A host model resolver that returns fixed coordinates (stands in for the
/// config-plane + vault lookup).
struct FixedModel(ResolvedModel);
impl ModelResolver for FixedModel {
    fn resolve(&self, _a: &RunActivation) -> std::result::Result<ResolvedModel, OpenError> {
        Ok(self.0.clone())
    }
}

#[test]
fn projecting_source_plans_launch_from_resolved_model_and_host_env() {
    let cli = *acp_cli("claude").expect("claude in the catalog");
    let resolver = Arc::new(FixedModel(ResolvedModel {
        base_url: "https://api.kimi.com/coding/".to_string(),
        model: "kimi-k2".to_string(),
        api_key: "materialized-by-host".to_string(), // awaken-allow: secret
    }));
    // The host provides the config-home path as non-secret extra env.
    let source = ProjectingChannelSource::new(
        cli,
        resolver,
        vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/run/agent/.claude".to_string(),
        )],
    );
    let launch = source.plan(&activation()).expect("plan");
    let env = |k: &str| {
        launch
            .env
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(launch.argv, vec!["claude", "--acp"]);
    assert_eq!(env("ANTHROPIC_MODEL").as_deref(), Some("kimi-k2"));
    assert_eq!(
        env("ANTHROPIC_BASE_URL").as_deref(),
        Some("https://api.kimi.com/coding/")
    );
    assert_eq!(
        env("ANTHROPIC_API_KEY").as_deref(),
        Some("materialized-by-host")
    );
    // The host-provided config-home path threads through as extra env.
    assert_eq!(
        env("CLAUDE_CONFIG_DIR").as_deref(),
        Some("/run/agent/.claude")
    );
}
