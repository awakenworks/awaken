//! End-to-end tests with in-memory fakes: a scripted duplex channel stands in for
//! the launched CLI — no daemon, no network.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
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
            codec: awaken_protocol_acp::Codec::Newline,
            workspace_cwd: None,
            mcp_session_servers: Vec::new(),
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
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                model_binding: ModelBinding::new("prov", "model", "acp:claude"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        input: vec![Message::text(MessageId("u1".into()), Role::User, "do it")],
        model_ref_override: None,
    }
}

fn exec(frames: Vec<String>) -> AcpRunExecutor {
    AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames,
        open_error: None,
    }))
}

#[test]
fn advertises_remote_abort_and_auth_wait() {
    // The supervisor interrupts the opaque CLI turn on cancel, and the ACP
    // permission flow parks on an authorization decision (ADR-0055).
    let caps = exec(vec![]).capabilities();
    assert_eq!(caps.cancellation, Cancellation::RemoteAbort);
    assert_eq!(caps.wait, Wait::Auth);
}

/// Records every lifecycle event the executor emits during bring-up.
#[derive(Default)]
struct RecordingObserver {
    stages: Mutex<Vec<AcpLaunchStage>>,
}

impl LaunchObserver for RecordingObserver {
    fn on_launch(&self, _scope: &str, event: &AcpLaunchEvent) {
        self.stages.lock().unwrap().push(event.stage);
    }
}

#[tokio::test]
async fn observer_sees_install_launch_and_ready_for_a_dynamic_install_backend() {
    // The fixture activation's backend_ref is `acp:claude` (an npx adapter), so the
    // executor surfaces an Installing phase before launch, then Ready once the
    // (newline-fixture) agent is live.
    let observer = Arc::new(RecordingObserver::default());
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![
            r#"{"type":"message","text":"hi"}"#.into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ],
        open_error: None,
    }))
    .with_launch_observer(observer.clone());

    e.execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();

    let stages = observer.stages.lock().unwrap().clone();
    assert_eq!(
        stages,
        vec![
            AcpLaunchStage::Installing,
            AcpLaunchStage::Launching,
            AcpLaunchStage::Ready,
        ],
        "install → launch → ready, in order"
    );
}

#[tokio::test]
async fn observer_sees_failed_when_the_launch_faults() {
    let observer = Arc::new(RecordingObserver::default());
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("spawn npx: No such file or directory".into()),
    }))
    .with_launch_observer(observer.clone());

    // A launch fault is committed as a classified failure (not an Err).
    e.execute(
        activation(),
        RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default())),
    )
    .await
    .unwrap();

    let stages = observer.stages.lock().unwrap().clone();
    assert_eq!(
        stages,
        vec![
            AcpLaunchStage::Installing,
            AcpLaunchStage::Launching,
            AcpLaunchStage::Failed,
        ],
        "a spawn fault surfaces a Failed phase"
    );
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
async fn live_inbox_steer_folds_into_a_relaunched_turn() {
    // ADR-0054 P4: a steer message queued on the run's live inbox is drained at the
    // turn boundary, folded (re-identified) into the transcript, and drives a second
    // relaunched turn — so steer/redirect reaches an external-CLI run.
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    let e = exec(vec![
        r#"{"type":"message","text":"turn"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let inbox = LiveInbox::new();
    let _ = inbox.offer_as(
        MessageOrigin::External,
        Message::text(MessageId("client-id".into()), Role::User, "steer me"),
    );
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox.clone()),
        )
        .await
        .unwrap();

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1, "one commit at the terminal phase");
    let steer = commits[0]
        .messages
        .iter()
        .find(|m| m.id.0 == "run-1-inbox-0")
        .expect("steer drained + re-identified into the committed transcript");
    assert_eq!(steer.text_content(), "steer me");
    // The caller-supplied id never reaches the transcript; the inbox was consumed.
    assert!(commits[0].messages.iter().all(|m| m.id.0 != "client-id"));
    assert!(inbox.list().is_empty());
}

#[tokio::test]
async fn a_requested_pause_parks_the_run_on_a_waiting_ticket() {
    // ADR-0054 P5/U2: an operator pause requested by the next boundary parks the ACP
    // run durably — `Phase::Waiting` on a no-tool `ManualPause` ticket — rather than
    // ending, even though the turn reached a natural end. The turn's messages commit
    // before the park (clean commit-then-park), mirroring the native engine.
    use awaken_agent_contract::agent::waiting::WaitingReason;
    use awaken_runtime_contract::pause::PauseSignal;

    let e = exec(vec![
        r#"{"type":"message","text":"turn"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let pause = PauseSignal::new();
    pause.request();
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_pause(pause),
        )
        .await
        .unwrap();

    assert_eq!(phase, Phase::Waiting, "a requested pause parks, not ends");
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1, "one commit at the park boundary");
    // The in-flight turn commits before the park.
    assert!(
        commits[0]
            .messages
            .iter()
            .any(|m| m.text_content() == "turn"),
        "the turn's assistant text commits before parking"
    );
    // A resumable no-tool `ManualPause` ticket rode the same commit.
    let ticket = commits[0]
        .waiting
        .as_ref()
        .expect("a parked run commits its waiting ticket");
    assert_eq!(ticket.reason, WaitingReason::ManualPause);
    assert_eq!(ticket.run_id, RunId("run-1".into()));
    assert_eq!(ticket.thread_id, ThreadId("thread-1".into()));
    assert!(
        ticket.call_id.is_none() && ticket.pending_tool.is_none(),
        "an operator pause parks on no tool"
    );
}

#[tokio::test]
async fn a_pause_commits_in_flight_steer_before_parking() {
    // Pause preempts queued input, but the in-flight steer is not lost: it rides out
    // with the park (fold) and commits before the run parks (boundary priority is
    // pause > queued-input > idle).
    use awaken_agent_contract::agent::waiting::WaitingReason;
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};
    use awaken_runtime_contract::pause::PauseSignal;

    let e = exec(vec![
        r#"{"type":"message","text":"turn"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let inbox = LiveInbox::new();
    let _ = inbox.offer_as(
        MessageOrigin::External,
        Message::text(MessageId("client-id".into()), Role::User, "late steer"),
    );
    let pause = PauseSignal::new();
    pause.request();
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox.clone())
                .with_pause(pause),
        )
        .await
        .unwrap();

    assert_eq!(phase, Phase::Waiting, "pause preempts the queued input");
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    let steer = commits[0]
        .messages
        .iter()
        .find(|m| m.id.0 == "run-1-inbox-0")
        .expect("in-flight steer rides out with the park and commits");
    assert_eq!(steer.text_content(), "late steer");
    assert_eq!(
        commits[0].waiting.as_ref().map(|t| &t.reason),
        Some(&WaitingReason::ManualPause)
    );
    assert!(
        inbox.list().is_empty(),
        "the inbox was drained at the boundary"
    );
}

#[tokio::test]
async fn a_usage_event_is_committed_as_thread_state() {
    // A Usage event projects onto the neutral `__usage` `ThreadUsage` thread state,
    // attributed to the bound model — the same committed truth a native run writes,
    // so a session's ACP usage is readable identically.
    use awaken_agent_contract::agent::state::Action;
    use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage, TokenUsage};

    let e = exec(vec![
        r#"{"type":"message","text":"hi"}"#.into(),
        r#"{"type":"usage","prompt_tokens":10,"completion_tokens":20,"cache_read_tokens":5}"#
            .into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    e.execute(
        activation(),
        RuntimeRunContext::new().with_commit(coord.clone()),
    )
    .await
    .unwrap();

    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(
        commits[0].state.len(),
        1,
        "usage committed as one state command"
    );
    let cmd = &commits[0].state[0];
    assert_eq!(cmd.key.0, THREAD_USAGE_STATE_KEY);
    let Action::Set(value) = &cmd.action else {
        panic!("usage is a Set command");
    };
    let tally: ThreadUsage = serde_json::from_value(value.clone()).unwrap();
    // The fixture activation binds model_ref "model".
    assert_eq!(
        tally.by_model["model"],
        TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 5,
            cache_creation_tokens: 0,
        }
    );
}

#[tokio::test]
async fn a_session_persisted_in_one_dir_is_recovered_in_another_through_the_executor() {
    // End-to-end through the real executor: run 1 in config-home A writes a session
    // marker (a stand-in for the CLI's own session files); the executor harvests it.
    // Run 2 in a *different* config-home B restores it before launch, and the agent
    // finds the marker — proving cross-directory recovery of the whole
    // restore→run→harvest chain (a real CLI's `session/load` is the same shape).
    use std::path::{Path, PathBuf};

    /// A fake source whose agent persists a marker into its config-home on a fresh
    /// session and echoes it back once the dir was restored.
    struct PersistingSource {
        dir: PathBuf,
    }
    #[async_trait]
    impl AgentChannelSource for PersistingSource {
        async fn open(&self, _a: &RunActivation) -> std::result::Result<AgentSession, OpenError> {
            let (ours, mut theirs) = tokio::io::duplex(4096);
            let marker = self.dir.join("projects/marker");
            tokio::spawn(async move {
                let mut prompt = String::new();
                let mut reader = BufReader::new(&mut theirs);
                let _ = reader.read_line(&mut prompt).await;
                let text = match std::fs::read_to_string(&marker) {
                    Ok(s) => format!("resumed:{s}"),
                    Err(_) => {
                        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
                        std::fs::write(&marker, "s1").unwrap();
                        "fresh".to_string()
                    }
                };
                let _ = theirs
                    .write_all(format!("{{\"type\":\"message\",\"text\":\"{text}\"}}\n").as_bytes())
                    .await;
                let _ = theirs
                    .write_all(b"{\"type\":\"turn_end\",\"reason\":\"natural_end\"}\n")
                    .await;
                let _ = theirs.flush().await;
            });
            Ok(AgentSession {
                channel: Box::new(ours),
                process: Arc::new(FakeProcess),
                codec: awaken_protocol_acp::Codec::Newline,
                workspace_cwd: Some("/workspace".to_string()),
                mcp_session_servers: Vec::new(),
            })
        }
    }

    fn copy_dir(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            let to = dst.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &to);
            } else {
                std::fs::copy(e.path(), to).unwrap();
            }
        }
    }

    /// The session-home harvest/restore essence: copy the config-home's session
    /// subtree to/from a durable blob keyed by thread (what `DirSessionHome` does).
    struct TmpHome {
        blobs: PathBuf,
        dir: PathBuf,
    }
    #[async_trait]
    impl SessionHomeProvider for TmpHome {
        async fn restore(&self, key: &SessionHomeKey, _p: &SessionHomePlan) {
            let blob = self.blobs.join(&key.thread_id);
            if blob.is_dir() {
                copy_dir(&blob, &self.dir.join("projects"));
            }
        }
        async fn harvest(&self, key: &SessionHomeKey, _p: &SessionHomePlan) {
            let src = self.dir.join("projects");
            if src.is_dir() {
                let dst = self.blobs.join(&key.thread_id);
                let _ = std::fs::remove_dir_all(&dst);
                copy_dir(&src, &dst);
            }
        }
    }

    let root = std::env::temp_dir().join(format!("acp-recovery-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (dir_a, dir_b, blobs) = (root.join("a"), root.join("b"), root.join("blobs"));

    // Run 1 in dir A: a fresh session; the marker is written and harvested.
    let coord_a = Arc::new(RecordingCoordinator::default());
    AcpRunExecutor::new(Arc::new(PersistingSource { dir: dir_a.clone() }))
        .with_session_home(Arc::new(TmpHome {
            blobs: blobs.clone(),
            dir: dir_a.clone(),
        }))
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord_a.clone()),
        )
        .await
        .unwrap();
    assert!(
        coord_a.commits.lock().unwrap()[0]
            .messages
            .iter()
            .any(|m| m.text_content() == "fresh"),
        "run 1 starts a fresh session"
    );

    // Run 2 in a different dir B: the executor restores the harvested session first,
    // so the agent finds the marker and resumes.
    let coord_b = Arc::new(RecordingCoordinator::default());
    AcpRunExecutor::new(Arc::new(PersistingSource { dir: dir_b.clone() }))
        .with_session_home(Arc::new(TmpHome {
            blobs: blobs.clone(),
            dir: dir_b.clone(),
        }))
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord_b.clone()),
        )
        .await
        .unwrap();
    assert!(
        coord_b.commits.lock().unwrap()[0]
            .messages
            .iter()
            .any(|m| m.text_content() == "resumed:s1"),
        "the session persisted in dir A was recovered in dir B through the executor"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[derive(Default)]
struct RecordingSessionHome {
    calls: Mutex<Vec<(String, SessionHomeKey, SessionHomePlan)>>,
}

#[async_trait]
impl SessionHomeProvider for RecordingSessionHome {
    async fn restore(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        self.calls
            .lock()
            .unwrap()
            .push(("restore".into(), key.clone(), plan.clone()));
    }
    async fn harvest(&self, key: &SessionHomeKey, plan: &SessionHomePlan) {
        self.calls
            .lock()
            .unwrap()
            .push(("harvest".into(), key.clone(), plan.clone()));
    }
}

#[tokio::test]
async fn a_local_dir_session_home_is_restored_before_and_harvested_after() {
    // A LocalDir CLI (the fixture's backend is `acp:claude`) restores its portable
    // session before the run and harvests it after, keyed by (thread, adapter), with
    // the harvest plan drawn from the catalog row (subpath, cwd-keying, exclusions).
    let recorder = Arc::new(RecordingSessionHome::default());
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![
            r#"{"type":"message","text":"hi"}"#.into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ],
        open_error: None,
    }))
    .with_session_home(recorder.clone());

    e.execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();

    let calls = recorder.calls.lock().unwrap();
    let seq: Vec<&str> = calls.iter().map(|(c, _, _)| c.as_str()).collect();
    assert_eq!(
        seq,
        vec!["restore", "harvest"],
        "restore before the run, harvest after"
    );
    let (_, key, plan) = &calls[0];
    assert_eq!(key.adapter, "claude");
    assert_eq!(key.thread_id, "thread-1");
    assert_eq!(plan.config_home_env, "CLAUDE_CONFIG_DIR");
    assert_eq!(plan.session_subpath, "projects");
    assert!(plan.keyed_by_cwd, "Claude keys sessions by cwd");
    assert!(
        plan.exclude.contains(&".credentials.json".to_string()),
        "credentials are excluded from the harvest"
    );
}

#[test]
fn session_home_binding_is_none_for_a_non_acp_backend() {
    // A native (non-ACP) backend has no CLI session-home — the binding is absent, so
    // the provider is never engaged (Gateway/stateless adapters skip the same way).
    let mut act = activation();
    act.snapshot.resolved_spec.model_binding = ModelBinding::new("prov", "model", "native");
    let e = exec(vec![]);
    assert!(e.session_home_binding(&act).is_none());
}

#[tokio::test]
async fn neutral_permission_resolver_projects_the_policy_decision() {
    // The ACP permission port is decided by the single neutral `PermissionPolicy`:
    // Allow→Allow, Deny→Deny, and Ask fails safe to Deny (no synchronous HITL over
    // the held ACP turn yet).
    use awaken_protocol_acp::{PermissionAsk, PermissionResolver, PermissionVerdict};
    use awaken_runtime_contract::permission::{
        PermissionContext, PermissionDecision, PermissionPolicy,
    };

    struct FixedPolicy(PermissionDecision);
    #[async_trait]
    impl PermissionPolicy for FixedPolicy {
        async fn decide(&self, _ctx: &PermissionContext) -> PermissionDecision {
            self.0.clone()
        }
    }

    let ask = PermissionAsk {
        tool: "bash".into(),
        call_id: "t1".into(),
        arguments: serde_json::json!({"cmd": "ls"}),
    };
    let cases = [
        (PermissionDecision::Allow, PermissionVerdict::Allow),
        (
            PermissionDecision::Deny {
                reason: "policy".into(),
            },
            PermissionVerdict::Deny,
        ),
        (
            PermissionDecision::Ask {
                ticket_id: "tk".into(),
            },
            PermissionVerdict::Deny,
        ),
    ];
    for (decision, want) in cases {
        let resolver = NeutralPermissionResolver {
            policy: Arc::new(FixedPolicy(decision)),
        };
        assert_eq!(resolver.resolve(&ask).await, want);
    }
}

#[tokio::test]
async fn a_tool_call_and_its_result_commit_as_neutral_messages() {
    use awaken_agent_contract::agent::content::ContentBlock;

    // The external agent surfaces a tool call, then reports it completed with output.
    let e = exec(vec![
        r#"{"type":"tool_call","id":"c1","name":"read","input":{"path":"a.txt"}}"#.into(),
        r#"{"type":"tool_result","id":"c1","content":"file body","is_error":false}"#.into(),
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
    let messages = &commits[0].messages;
    assert_eq!(messages.len(), 2, "the call and its result both commit");

    // The call is an assistant ToolUse carrying the correlating id.
    assert_eq!(messages[0].role, Role::Assistant);
    match &messages[0].content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "c1");
            assert_eq!(name, "read");
            assert_eq!(input["path"], "a.txt");
        }
        other => panic!("expected a ToolUse, got {other:?}"),
    }

    // The result is a Role::Tool ToolResult addressed to that call — proving the
    // external agent's tool output now reaches the neutral transcript.
    assert_eq!(messages[1].role, Role::Tool);
    match &messages[1].content[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
        } => {
            assert_eq!(tool_use_id, "c1");
            assert_eq!(content[0], ContentBlock::text("file body"));
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
}

#[tokio::test]
async fn a_tool_call_without_an_id_gets_a_correlating_fallback_id() {
    use awaken_agent_contract::agent::content::ContentBlock;

    // An agent that omits tool_call_id (some CLIs do): the call and its result
    // still correlate via a per-seq fallback id, so the transcript stays coherent.
    let e = exec(vec![
        r#"{"type":"tool_call","name":"read","input":{}}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    e.execute(
        activation(),
        RuntimeRunContext::new().with_commit(coord.clone()),
    )
    .await
    .unwrap();

    let commits = coord.commits.lock().unwrap();
    match &commits[0].messages[0].content[0] {
        ContentBlock::ToolUse { id, .. } => {
            assert!(id.starts_with("acp-tool-"), "fallback id, got {id}");
        }
        other => panic!("expected a ToolUse, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_tool_result_is_marked_in_the_committed_text() {
    use awaken_agent_contract::agent::content::ContentBlock;

    let e = exec(vec![
        r#"{"type":"tool_result","id":"c9","content":"denied","is_error":true}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    e.execute(
        activation(),
        RuntimeRunContext::new().with_commit(coord.clone()),
    )
    .await
    .unwrap();

    let commits = coord.commits.lock().unwrap();
    match &commits[0].messages[0].content[0] {
        ContentBlock::ToolResult { content, .. } => {
            assert_eq!(content[0], ContentBlock::text("[tool error] denied"));
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
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

// M12/T82: the driver-error seam `failure_cause` preserves a rate-limit (HARD-limit
// banner) as a terminal Inference error stamped with the neutral `"acp_failure"` code —
// never a success, never a bare stop. The RateLimited class terminates as `Error`
// (`AcpFailure::termination`), so the run surfaces `EndCause::Error(Failure::Inference)`
// carrying the raw provider message. Timeout/Refusal are the two classes that instead
// map to `Stopped` — pinned here as the masking contrast so a refactor cannot silently
// fold a rate-limit into a stop (which a host would not retry as an error).
#[test]
fn a_rate_limited_acp_failure_ends_error_with_the_acp_failure_code() {
    use awaken_protocol_acp::error::{AcpFailure, AcpFailureClass};

    let rate_limited = AcpFailure {
        class: AcpFailureClass::RateLimited {
            retry_after_secs: Some(30),
        },
        message: "quota exhausted".to_string(),
    };
    match failure_cause(&rate_limited) {
        EndCause::Error(Failure::Inference { code, message }) => {
            assert_eq!(
                code, "acp_failure",
                "the neutral rate-limit code is preserved"
            );
            assert_eq!(
                message, "quota exhausted",
                "the raw provider message rides along"
            );
        }
        other => panic!("a rate-limited failure must be a terminal Inference error, got {other:?}"),
    }

    // Contrast: only Timeout and Refusal short-circuit to a (non-retryable) Stopped.
    let timeout = AcpFailure {
        class: AcpFailureClass::Timeout,
        message: "deadline".to_string(),
    };
    assert!(matches!(failure_cause(&timeout), EndCause::Stopped(_)));
    let refusal = AcpFailure {
        class: AcpFailureClass::Refusal,
        message: "refused".to_string(),
    };
    assert!(matches!(failure_cause(&refusal), EndCause::Stopped(_)));

    // A permanent/credential/transient failure is likewise an Error with the same code
    // (masked-preservation: the host, not this crate, decides retryability).
    let permanent = AcpFailure {
        class: AcpFailureClass::Permanent,
        message: "boom".to_string(),
    };
    assert!(matches!(
        failure_cause(&permanent),
        EndCause::Error(Failure::Inference { .. })
    ));
}

// Fail-open guard (the class found in run-executor-a2a): a clean turn that ends on
// `TerminationReason::Error` — the agent reporting an error as its own terminal frame,
// NOT a driver/IO fault — flows through the `Idle` boundary arm's `end_cause`. It must
// map to a terminal ERROR, never to a success (`NaturalEnd`), or a failed run would be
// recorded as a clean completion. Exercises the `end_cause(Error)` row the truncated-
// stream test (a driver `Err`, i.e. `failure_cause`) never reaches.
#[tokio::test]
async fn a_clean_error_turn_end_maps_to_error_not_natural_end() {
    let e = exec(vec![
        r#"{"type":"message","text":"partial"}"#.into(),
        r#"{"type":"turn_end","reason":"error"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(
        matches!(phase, Phase::Ended(EndCause::Error(_))),
        "a clean error turn must end in a terminal Error, got {phase:?}"
    );
    assert_ne!(
        phase,
        Phase::Ended(EndCause::NaturalEnd),
        "a reported error must never be recorded as a natural (successful) end"
    );
    // The committed run fact carries the same terminal Error — committed truth is not
    // a success either.
    let commits = coord.commits.lock().unwrap();
    assert!(matches!(
        commits[0].run_fact.phase,
        Phase::Ended(EndCause::Error(_))
    ));
}

// A clean turn that ends on `TerminationReason::TimedOut` (the agent/supervisor
// reporting the turn hit its deadline) maps through `end_cause` to a terminal
// `Stopped`, never to a success — the last untested clean-outcome row.
#[tokio::test]
async fn a_timed_out_turn_end_maps_to_stopped_not_natural_end() {
    let e = exec(vec![r#"{"type":"turn_end","reason":"timed_out"}"#.into()]);
    let phase = e
        .execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert!(
        matches!(phase, Phase::Ended(EndCause::Stopped(_))),
        "a timed-out turn must end Stopped, got {phase:?}"
    );
    assert_ne!(phase, Phase::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn an_org_subscription_disabled_launch_fault_surfaces_a_credential_prompt() {
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("Your organization has disabled Claude subscription access".into()),
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
    let prompt = coord.commits.lock().unwrap()[0].messages[0].text_content();
    assert!(prompt.contains("credential"), "{prompt}");
}

#[tokio::test]
async fn a_login_required_launch_fault_surfaces_a_login_prompt() {
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("Please run /login to continue".into()),
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
    let prompt = coord.commits.lock().unwrap()[0].messages[0]
        .text_content()
        .to_lowercase();
    assert!(prompt.contains("login"), "{prompt}");
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
        Arc::new(Spy("remote", hits.clone())),
    );

    // backend_ref "default" → Native.
    let mut native_act = activation();
    native_act.snapshot.resolved_spec.model_binding = ModelBinding::new("prov", "model", "default");
    dispatch
        .execute(native_act, RuntimeRunContext::new())
        .await
        .unwrap();
    // The default fixture's backend_ref is "acp:claude" → Acp.
    dispatch
        .execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    // `a2a:*` → Remote.
    let mut remote_act = activation();
    remote_act.snapshot.resolved_spec.model_binding =
        ModelBinding::new("prov", "model", "a2a:https://host/a2a");
    dispatch
        .execute(remote_act, RuntimeRunContext::new())
        .await
        .unwrap();

    assert_eq!(*hits.lock().unwrap(), vec!["native", "acp", "remote"]);
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
                codec: awaken_protocol_acp::Codec::Newline,
                workspace_cwd: None,
                mcp_session_servers: Vec::new(),
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
impl LaunchResolver for FixedModel {
    fn model(&self, _a: &RunActivation) -> std::result::Result<ResolvedModel, OpenError> {
        Ok(self.0.clone())
    }
    fn extra_env(&self, _a: &RunActivation) -> Vec<(String, String)> {
        vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/run/agent/.claude".to_string(),
        )]
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
    // The resolver supplies the config-home path as non-secret per-run env.
    let source = ProjectingChannelSource::new(cli, resolver);
    let launch = source.plan(&activation()).expect("plan");
    let env = |k: &str| {
        launch
            .env
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        launch.argv,
        vec!["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.44"]
    );
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

/// A resolver that supplies a config-home dir under an arbitrary env key.
struct ConfigHomeAt {
    key: &'static str,
    dir: String,
}
impl LaunchResolver for ConfigHomeAt {
    fn model(&self, _a: &RunActivation) -> std::result::Result<ResolvedModel, OpenError> {
        Ok(ResolvedModel {
            base_url: "u".into(),
            model: "m".into(),
            api_key: "k".into(), // awaken-allow: secret
        })
    }
    fn extra_env(&self, _a: &RunActivation) -> Vec<(String, String)> {
        vec![(self.key.to_string(), self.dir.clone())]
    }
}

/// End to end through the real launch path: a run that declares an MCP server on its ACP
/// plugin config (what the host's `overlay_acp_mcp` produces) makes `open()` write the
/// codex `config.toml` into the config home before it spawns — proving the whole
/// host→plugin_config→config-file chain, secretlessly (α: a broker reference, never a raw
/// secret). Uses a cheap spawnable command so no real CLI/creds are needed.
#[tokio::test]
async fn open_writes_the_codex_mcp_config_into_the_config_home() {
    let dir = std::env::temp_dir().join(format!("awaken-acpmcp-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A codex row (ConfigFileToml, CODEX_HOME) with a cheap spawnable command.
    let mut cli = *acp_cli("codex").expect("codex in the catalog");
    cli.command = "/bin/sh";
    cli.args = &["-c", "exit 0"];
    let source = ProjectingChannelSource::new(
        cli,
        Arc::new(ConfigHomeAt {
            key: cli.config_home_env,
            dir: dir.to_string_lossy().to_string(),
        }),
    );

    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({
            "mcp_servers": [{
                "name": "github",
                "transport": { "kind": "http", "url": "https://mcp.gh" },
                "credential": { "auth": "reference", "reference": "broker://gh" }
            }]
        }),
    );

    // open() writes the config.toml before spawning the (immediately-exiting) child.
    let session = source.open(&act).await.expect("open");
    drop(session); // reap the child

    let written = std::fs::read_to_string(dir.join("config.toml")).expect("config.toml written");
    assert!(
        written.contains("[mcp_servers.github]"),
        "server section present: {written}"
    );
    assert!(written.contains("broker://gh"), "α reference present");
    assert!(
        !written.contains("\"secret\""),
        "no raw inline secret (α is secretless)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fake ACP agent (JSON-RPC, shell builtins only) that reports whether the
/// `session/new` request it received carried our MCP server and, if so, whether the
/// bearer was the α broker reference (secretless) — echoed as its agent message. It
/// captures the raw `session/new` line (`id:2`) and, on the prompt (`id:3`), classifies
/// it: `saw-github` if the server name crossed, `alpha-ref` if `broker://gh` (the α
/// reference) is the bearer. So the test asserts the D5 wire (plugin_config →
/// `to_session_mcp_server` → `to_acp_mcp_servers` → `session/new`) actually reached the CLI.
#[cfg(feature = "real-acp")]
const FAKE_ACP_MCP_ECHO_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          M=none; case \"$SN\" in *github*) M=saw-github;; esac; \
          A=noauth; case \"$SN\" in *'broker://gh'*) A=alpha-ref;; esac; \
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"mcp %s %s\"}}}}\\n' \"$M\" \"$A\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

/// End to end over the REAL ACP JSON-RPC codec: a run that declares an MCP server on
/// its ACP plugin config (what the host's `overlay_acp_mcp` produces for an `AcpSession`
/// CLI) makes `open()` stage it as a session server and `drive()` inject it into the
/// `session/new` request — the D5 seam. The fake agent echoes that it saw the server and
/// that the bearer is the α broker reference (secretless), proving the whole
/// host→plugin_config→session/new chain without a raw secret. Gated on `real-acp`: only
/// the official codec serializes `mcpServers` into `session/new`.
#[cfg(feature = "real-acp")]
#[tokio::test]
async fn open_and_drive_inject_the_mcp_server_into_session_new_for_an_acp_session_cli() {
    // A claude row (AcpSession, session/new delivery) with a cheap JSON-RPC echo agent.
    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.command = "/bin/sh";
    cli.args = &["-c", FAKE_ACP_MCP_ECHO_SCRIPT];
    let source = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel {
            base_url: "u".into(),
            model: "m".into(),
            api_key: "k".into(), // awaken-allow: secret
        })),
    ));
    let e = AcpRunExecutor::new(source);

    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({
            "mcp_servers": [{
                "name": "github",
                "transport": { "kind": "http", "url": "https://mcp.gh" },
                "credential": { "auth": "reference", "reference": "broker://gh" }
            }]
        }),
    );

    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(act, RuntimeRunContext::new().with_commit(coord.clone()))
        .await
        .unwrap();

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    let reply = commits[0].messages[0].text_content();
    assert_eq!(
        reply, "mcp saw-github alpha-ref",
        "session/new carried the MCP server with the α broker reference as its bearer, got {reply:?}",
    );
}

/// The β (trusted-inline) counterpart of the α test: a trusted-local host projects the
/// staged MCP server with the raw bearer inline (what `overlay_acp_mcp(..., trusted=true)`
/// produces), and `session/new` carries that secret to the CLI. The fake agent reports the
/// bearer it received so the test asserts β delivers the raw token (never a reference).
#[cfg(feature = "real-acp")]
#[tokio::test]
async fn open_and_drive_inject_a_trusted_inline_mcp_credential_into_session_new() {
    // A JSON-RPC echo agent: classifies the `session/new` bearer as `beta-inline` when it
    // saw the raw secret `sk-trusted`, else `no-secret`.
    const BETA_ECHO: &str = "while IFS= read -r line; do \
          case \"$line\" in \
            *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
            *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
            *'\"id\":3'*) \
              A=no-secret; case \"$SN\" in *sk-trusted*) A=beta-inline;; esac; \
              printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"mcp %s\"}}}}\\n' \"$A\"; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
              exit 0;; \
          esac; \
        done";
    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.command = "/bin/sh";
    cli.args = &["-c", BETA_ECHO];
    let source = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel {
            base_url: "u".into(),
            model: "m".into(),
            api_key: "k".into(), // awaken-allow: secret
        })),
    ));
    let e = AcpRunExecutor::new(source);

    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({
            "mcp_servers": [{
                "name": "github",
                "transport": { "kind": "http", "url": "https://mcp.gh" },
                "credential": { "auth": "trusted_inline", "secret": "sk-trusted" } // awaken-allow: secret
            }]
        }),
    );

    let coord = Arc::new(RecordingCoordinator::default());
    e.execute(act, RuntimeRunContext::new().with_commit(coord.clone()))
        .await
        .unwrap();
    let reply = coord.commits.lock().unwrap()[0].messages[0].text_content();
    assert_eq!(
        reply, "mcp beta-inline",
        "β hands the trusted-local CLI the raw bearer inline on session/new, got {reply:?}",
    );
}

/// The ACP `acp_session_id` is carried across the per-turn relaunch loop (R7): the id
/// negotiated on turn 1's `session/new` is threaded into turn 2's config, so the
/// relaunched CLI is resumed via `session/load` with the SAME id (context survives)
/// rather than starting fresh. A fake ACP CLI (JSON-RPC, shell builtins) advertises
/// `loadSession` and, per turn, reports which session verb it received — `new` (turn 1,
/// no prior id) or `load-s1` (turn 2, resumed with the carried id `s1`). Each relaunch
/// is a fresh child (the shell var resets), so the only thing that can carry `s1` into
/// turn 2 is the executor threading it through `config.session_id`. Gated on `real-acp`:
/// only the official codec negotiates/loads a session id (the newline stand-in leaves it
/// `None`).
#[cfg(feature = "real-acp")]
#[tokio::test]
async fn acp_session_id_is_carried_across_the_per_turn_relaunch() {
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    // id:1 initialize (advertise loadSession) · id:2 session/new|load · id:3 prompt.
    // The id:2 request distinguishes the verb by whether the carried id `s1` is present
    // in it: turn 1's session/new has none (→ `new`, returns sessionId s1); turn 2's
    // session/load carries `s1` (→ `load-s1`, empty result). The turn's agent message
    // echoes which verb fired, so the committed transcript proves the carry.
    const SESSION_CARRY_AGENT: &str = "while IFS= read -r line; do \
          case \"$line\" in \
            *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true}}}';; \
            *'\"id\":2'*) \
              case \"$line\" in \
                *s1*) K=load-s1; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}';; \
                *) K=new; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
              esac;; \
            *'\"id\":3'*) \
              printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"turn:%s\"}}}}\\n' \"$K\"; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
              exit 0;; \
          esac; \
        done";

    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.command = "/bin/sh";
    cli.args = &["-c", SESSION_CARRY_AGENT];
    let source = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel {
            base_url: "u".into(),
            model: "m".into(),
            api_key: "k".into(), // awaken-allow: secret
        })),
    ));
    let e = AcpRunExecutor::new(source);

    // One queued steer forces exactly one relaunch → a second turn (without it the run
    // ends after turn 1 and never relaunches, so the carry is never exercised).
    let inbox = LiveInbox::new();
    let _ = inbox.offer_as(
        MessageOrigin::External,
        Message::text(MessageId("steer".into()), Role::User, "keep going"),
    );
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox.clone()),
        )
        .await
        .unwrap();

    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    let texts: Vec<String> = commits[0]
        .messages
        .iter()
        .map(|m| m.text_content())
        .collect();
    assert!(
        texts.iter().any(|t| t == "turn:new"),
        "turn 1 opened a fresh session via session/new; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "turn:load-s1"),
        "turn 2 resumed via session/load carrying the id `s1` — acp_session_id survived \
         the per-turn relaunch; got {texts:?}"
    );
}

#[test]
fn projecting_source_reads_the_cli_compact_window_from_config() {
    let cli = *acp_cli("claude").expect("claude in the catalog");
    let resolver = Arc::new(FixedModel(ResolvedModel {
        base_url: "u".to_string(),
        model: "m".to_string(),
        api_key: "k".to_string(), // awaken-allow: secret
    }));
    let source = ProjectingChannelSource::new(cli, resolver);

    // The run carries an ACP-scoped compaction window in plugin_config.
    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({ "compact_window": 262144 }),
    );
    let launch = source.plan(&act).expect("plan");
    let window = launch
        .env
        .iter()
        .find(|(k, _)| k == "CLAUDE_CODE_AUTO_COMPACT_WINDOW")
        .map(|(_, v)| v.clone());
    assert_eq!(window.as_deref(), Some("262144"));
}

// ── Cancellation, multi-turn usage, and mid-loop relaunch failure ────────────

/// A pre-cancelled run token ends the turn `Cancelled` (lease revocation / interrupt
/// at the executor level): the agent hangs with no `turn_end`, so the only way the
/// turn ends is the supervisor's cancel branch → `EndCause::Cancelled`.
#[tokio::test]
async fn a_cancelled_token_ends_the_run_cancelled() {
    use awaken_runtime_contract::CancellationToken;

    struct HangingSource;
    #[async_trait]
    impl AgentChannelSource for HangingSource {
        async fn open(&self, _a: &RunActivation) -> std::result::Result<AgentSession, OpenError> {
            let (ours, mut theirs) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut p = String::new();
                let mut r = BufReader::new(&mut theirs);
                let _ = r.read_line(&mut p).await;
                let _ = theirs
                    .write_all(b"{\"type\":\"message\",\"text\":\"working\"}\n")
                    .await;
                let _ = theirs.flush().await;
                // Hold the stream open (never turn_end), so only cancel ends the turn.
                std::future::pending::<()>().await;
                drop(theirs);
            });
            Ok(AgentSession {
                channel: Box::new(ours),
                process: Arc::new(FakeProcess),
                codec: awaken_protocol_acp::Codec::Newline,
                workspace_cwd: None,
                mcp_session_servers: Vec::new(),
            })
        }
    }

    let token = CancellationToken::new();
    token.cancel(); // pre-cancelled → the supervisor's cancel arm fires deterministically
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = AcpRunExecutor::new(Arc::new(HangingSource))
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_cancellation(token),
        )
        .await
        .unwrap();

    assert_eq!(phase, Phase::Ended(EndCause::Cancelled));
}

/// Token usage is summed across relaunched turns: each launched turn emits a usage
/// frame, and a live-inbox steer forces a second turn, so the single committed
/// `__usage` tally is the sum of both turns — a per-turn overwrite would under-report.
#[tokio::test]
async fn usage_accumulates_across_relaunched_turns() {
    use awaken_agent_contract::agent::state::Action;
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};
    use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage, TokenUsage};

    // Every launched turn (the ScriptedSource re-emits its frames on each open) reports
    // the same usage; two turns → the tally must double.
    let e = exec(vec![
        r#"{"type":"message","text":"turn"}"#.into(),
        r#"{"type":"usage","prompt_tokens":10,"completion_tokens":20,"cache_read_tokens":5}"#
            .into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let inbox = LiveInbox::new();
    let _ = inbox.offer_as(
        MessageOrigin::External,
        Message::text(MessageId("s".into()), Role::User, "again"),
    );
    let coord = Arc::new(RecordingCoordinator::default());
    e.execute(
        activation(),
        RuntimeRunContext::new()
            .with_commit(coord.clone())
            .with_live_inbox(inbox.clone()),
    )
    .await
    .unwrap();

    let commits = coord.commits.lock().unwrap();
    let usage_cmd = commits
        .last()
        .expect("a terminal commit")
        .state
        .iter()
        .find(|c| c.key.0 == THREAD_USAGE_STATE_KEY)
        .expect("usage committed as thread state");
    let Action::Set(value) = &usage_cmd.action else {
        panic!("usage is a Set command");
    };
    let tally: ThreadUsage = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        tally.by_model["model"],
        TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 40,
            cache_read_tokens: 10,
            cache_creation_tokens: 0,
        },
        "usage is summed across the two relaunched turns, not overwritten"
    );
}

/// A relaunch that fails to reopen the channel mid-run (the `BoundaryOutcome::Continue`
/// branch) commits a classified terminal failure — distinct from the initial-open
/// fault. The first open scripts a turn; a steer forces a relaunch; the second open
/// fails, so the run ends `Error` after exactly two open attempts.
#[tokio::test]
async fn a_relaunch_open_failure_mid_run_classifies_and_ends() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    struct FlakySource(Arc<AtomicUsize>);
    #[async_trait]
    impl AgentChannelSource for FlakySource {
        async fn open(&self, _a: &RunActivation) -> std::result::Result<AgentSession, OpenError> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            if n >= 1 {
                // The relaunch (second open) fails.
                return Err(OpenError("relaunch could not reopen the channel".into()));
            }
            let (ours, mut theirs) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut p = String::new();
                let mut r = BufReader::new(&mut theirs);
                let _ = r.read_line(&mut p).await;
                for f in [
                    r#"{"type":"message","text":"turn"}"#,
                    r#"{"type":"turn_end","reason":"natural_end"}"#,
                ] {
                    let _ = theirs.write_all(f.as_bytes()).await;
                    let _ = theirs.write_all(b"\n").await;
                    let _ = theirs.flush().await;
                }
            });
            Ok(AgentSession {
                channel: Box::new(ours),
                process: Arc::new(FakeProcess),
                codec: awaken_protocol_acp::Codec::Newline,
                workspace_cwd: None,
                mcp_session_servers: Vec::new(),
            })
        }
    }

    let opens = Arc::new(AtomicUsize::new(0));
    // A queued steer forces the boundary to Continue → a second (failing) open.
    let inbox = LiveInbox::new();
    let _ = inbox.offer_as(
        MessageOrigin::External,
        Message::text(MessageId("s".into()), Role::User, "again"),
    );
    let coord = Arc::new(RecordingCoordinator::default());
    let phase = AcpRunExecutor::new(Arc::new(FlakySource(opens.clone())))
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox),
        )
        .await
        .unwrap();

    assert!(
        matches!(phase, Phase::Ended(EndCause::Error(_))),
        "a mid-run relaunch-open failure ends the run classified, got {phase:?}"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        2,
        "the first open ran the turn; the relaunch attempted a second open and failed"
    );
}
