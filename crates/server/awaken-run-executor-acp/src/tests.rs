//! End-to-end tests with in-memory fakes: a scripted duplex channel stands in for
//! the launched CLI — no daemon, no network.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, RunTerminalObserverError,
};
use awaken_runtime_contract::tool::{ToolError, ToolOutputSpiller};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

struct FakeProcess;

struct SpillProbe {
    fail: bool,
    seen: Arc<Mutex<Vec<(String, String, String)>>>,
}

#[async_trait]
impl ToolOutputSpiller for SpillProbe {
    async fn spill(
        &self,
        run_id: &RunId,
        call_id: &str,
        content: String,
    ) -> std::result::Result<String, ToolError> {
        self.seen
            .lock()
            .unwrap()
            .push((run_id.0.clone(), call_id.to_string(), content.clone()));
        if self.fail {
            Err(ToolError::Execution("ACP spill unavailable".into()))
        } else {
            Ok(format!("preview: {content}"))
        }
    }
}

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

#[test]
fn only_pre_session_acp_transport_failures_are_retryable() {
    let reset = AcpError::Io("connection reset".into());
    assert!(retryable_handshake_failure(Codec::Acp, None, true, &reset));
    assert!(!retryable_handshake_failure(
        Codec::Acp,
        Some("session-created"),
        true,
        &reset,
    ));
    assert!(!retryable_handshake_failure(
        Codec::Newline,
        None,
        true,
        &reset,
    ));
    assert!(!retryable_handshake_failure(
        Codec::Acp,
        None,
        false,
        &reset,
    ));
    assert!(!retryable_handshake_failure(
        Codec::Acp,
        None,
        true,
        &AcpError::Frame("bad frame".into()),
    ));
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
        _context: &RuntimeRunContext,
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
            session_model: None,
            session_mode: None,
            session_config_options: Vec::new(),
            expected_capability: None,
        })
    }
}

#[derive(Default)]
struct RecordingCoordinator {
    commits: Mutex<Vec<ThreadCommit>>,
}

impl RecordingCoordinator {
    fn resume_ticket_for(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.commits
            .lock()
            .ok()?
            .iter()
            .rev()
            .find(|commit| commit.run_id() == run_id)
            .and_then(|commit| commit.resume_ticket().cloned())
    }

    fn messages(&self) -> Vec<Message> {
        self.commits
            .lock()
            .map(|commits| {
                commits
                    .iter()
                    .flat_map(|commit| commit.messages.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct TerminalRecordingObserver {
    events: Mutex<Vec<CommittedTerminalRun>>,
}

#[async_trait]
impl RunTerminalObserver for TerminalRecordingObserver {
    fn observer_id(&self) -> &str {
        "acp-terminal-test"
    }

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> std::result::Result<(), RunTerminalObserverError> {
        self.events.lock().unwrap().push(terminal.clone());
        Ok(())
    }
}

#[async_trait]
impl Coordinator for RecordingCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
        self.commits.lock().unwrap().push(commit);
        Ok(CommitRecord { sequence: 1 })
    }
}

impl ThreadReader for RecordingCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.commits
            .lock()
            .map(|commits| {
                commits
                    .iter()
                    .filter(|commit| &commit.thread_id == thread_id)
                    .flat_map(|commit| commit.messages.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.resume_ticket_for(run_id)
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        self.commits
            .lock()
            .ok()?
            .iter()
            .rev()
            .find(|commit| commit.run_id() == run_id)
            .map(ThreadCommit::run_state)
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        self.commits
            .lock()
            .map(|commits| {
                commits
                    .iter()
                    .filter(|commit| &commit.thread_id == thread_id)
                    .flat_map(|commit| commit.state.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub(crate) fn activation() -> RunActivation {
    RunActivation {
        run_id: RunId("run-1".into()),
        thread_id: ThreadId("thread-1".into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("prov", "model", "acp:claude"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        input: vec![Message::text(MessageId("u1".into()), Role::User, "do it")],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

#[test]
fn initial_acp_prompt_projects_frozen_agent_instructions_before_untrusted_input() {
    let activation = activation();
    let prompt = super::initial_prompt(&activation, &[]);
    let instructions_at = prompt.find("be helpful").unwrap();
    let input_at = prompt.find("do it").unwrap();
    assert!(instructions_at < input_at, "{prompt}");
    assert!(prompt.contains("untrusted data"), "{prompt}");

    let mut blank = activation;
    blank.snapshot.resolved_spec.instructions.clear();
    assert_eq!(super::initial_prompt(&blank, &[]), "do it");
}

#[test]
fn acp_prompt_loads_request_context_without_mixing_it_into_run_input() {
    // Cause/effect graph: frozen instructions (C1), transient backend context
    // (C2), and durable user input (C3) must become three ordered prompt
    // sections; C2 must not mutate the activation input (E2), which is the list
    // the executor commits. Decision rule R1: C1+C2+C3 -> ordered projection and
    // byte-identical durable input.
    let mut activation = activation();
    activation.snapshot.resolved_spec.instructions = "follow policy".into();
    let original = activation.input.clone();
    let context = vec![Message::text(
        MessageId("request-memory".into()),
        Role::System,
        "remember the user's preference",
    )];

    let prompt = super::initial_prompt(&activation, &context);

    let instructions = prompt.find("follow policy").expect("C1");
    let recalled = prompt.find("remember the user's preference").expect("C2");
    let input = prompt.find("do it").expect("C3");
    assert!(instructions < recalled && recalled < input, "R1: {prompt}");
    assert_eq!(activation.input, original, "E2");
}

fn exec(frames: Vec<String>) -> AcpRunExecutor {
    AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames,
        open_error: None,
    }))
}

async fn commit_ticket(coordinator: &Arc<RecordingCoordinator>, ticket: ResumeTicket) {
    awaken_agent_contract::thread::commit::commit_run(
        coordinator.as_ref(),
        &ticket.thread_id.clone(),
        awaken_agent_contract::thread::commit::RunDisposition::awaiting(ticket),
        Vec::new(),
        Vec::new(),
    )
    .await
    .unwrap();
}

fn resume_command_for(ticket: &ResumeTicket) -> awaken_runtime_contract::resume::ResumeCommand {
    awaken_runtime_contract::resume::ResumeCommand::from_ticket(
        ticket,
        awaken_runtime_contract::resume::ResumeResult::Decision {
            allow: true,
            note: None,
        },
        0,
    )
}

#[cfg(feature = "real-acp")]
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

#[test]
fn advertises_remote_abort_and_auth_wait() {
    // The supervisor interrupts the opaque CLI turn on cancel, and the ACP
    // permission flow awaits on an authorization decision (ADR-0055).
    let caps = exec(vec![]).capabilities();
    assert_eq!(caps.cancellation, Cancellation::RemoteAbort);
    assert_eq!(caps.wait, Wait::Auth);
}

#[tokio::test]
async fn resume_requires_history_then_an_active_ticket() {
    let activation = activation();
    let ticket = pause_ticket(&activation, &activation.run_id, AwaitReason::ToolPermission);
    let command = resume_command_for(&ticket);
    assert!(
        exec(Vec::new())
            .resume(
                activation.clone(),
                command.clone(),
                RuntimeRunContext::new()
            )
            .await
            .is_err()
    );

    let empty = Arc::new(RecordingCoordinator::default());
    assert!(
        exec(Vec::new())
            .resume(
                activation,
                command,
                RuntimeRunContext::new().with_reader(empty),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn permission_resume_requires_the_call_and_pending_tool_facts() {
    let activation = activation();

    let missing_call = Arc::new(RecordingCoordinator::default());
    let ticket = pause_ticket(&activation, &activation.run_id, AwaitReason::ToolPermission);
    commit_ticket(&missing_call, ticket.clone()).await;
    let error = exec(Vec::new())
        .resume(
            activation.clone(),
            resume_command_for(&ticket),
            RuntimeRunContext::new().with_reader(missing_call),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tool call id"));

    let missing_tool = Arc::new(RecordingCoordinator::default());
    let mut ticket = ticket;
    ticket.call_id = Some("call-7".into());
    commit_ticket(&missing_tool, ticket.clone()).await;
    let error = exec(Vec::new())
        .resume(
            activation,
            resume_command_for(&ticket),
            RuntimeRunContext::new().with_reader(missing_tool),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pending tool"));
}

#[test]
fn restored_session_rejects_removed_mismatched_and_empty_ids() {
    let activation = activation();
    let coordinator = Arc::new(RecordingCoordinator::default());
    let context = RuntimeRunContext::new().with_reader(coordinator.clone());
    assert_eq!(
        restored_session_id(
            &context,
            &activation.thread_id,
            &activation.snapshot.resolved_spec.model_binding.backend_ref,
        ),
        None
    );

    let commands = [
        StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            ACP_SESSION_ID_STATE_KEY,
            serde_json::json!({"backend_ref": "acp:other", "session_id": "session-7"}),
        ),
        StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            ACP_SESSION_ID_STATE_KEY,
            serde_json::json!({"backend_ref": "acp:claude", "session_id": ""}),
        ),
        StateCommand::remove(
            Scope::Thread,
            MergePolicy::Disjoint,
            ACP_SESSION_ID_STATE_KEY,
        ),
    ];
    for command in commands {
        coordinator.commits.lock().unwrap().push(ThreadCommit {
            thread_id: activation.thread_id.clone(),
            run: awaken_agent_contract::thread::commit::RunDisposition::running(
                activation.run_id.clone(),
            ),
            messages: Vec::new(),
            state: vec![command],
            events: Vec::new(),
        });
        assert_eq!(
            restored_session_id(
                &context,
                &activation.thread_id,
                &activation.snapshot.resolved_spec.model_binding.backend_ref,
            ),
            None
        );
        coordinator.commits.lock().unwrap().clear();
    }
}

#[test]
fn an_existing_pending_tool_use_is_not_duplicated() {
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_protocol_acp::PermissionAsk;

    let ask = PermissionAsk {
        tool: "bash".into(),
        call_id: "call-7".into(),
        arguments: serde_json::json!({"cmd": "pwd"}),
    };
    let mut messages = vec![Message {
        id: MessageId("tool-use".into()),
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(
            "call-7",
            "bash",
            serde_json::json!({"cmd": "pwd"}),
        )],
    }];
    ensure_pending_tool_use(&RunId("run-1".into()), &mut messages, &ask);
    assert_eq!(messages.len(), 1);
}

#[test]
fn acp_message_ids_are_replay_stable_and_cross_run_unique() {
    // Cause/effect graph:
    // C1 same durable Run + same fact suffix -> E1 same id (retry/replay dedupe).
    // C2 different Run + same suffix -> E2 different id (later turns survive the
    // Managed projection's message-id dedupe).
    // C3 same Run + different suffix -> E3 different id (facts within a turn do
    // not collide).
    // Decision table: R1=C1 => E1; R2=C2 => E2; R3=C3 => E3. Run identity and
    // suffix are the complete id inputs; Thread identity is intentionally absent
    // because Run ids are already the durable execution identity.
    let run_one = RunId("run-1".into());
    let run_two = RunId("run-2".into());

    assert_eq!(acp_message_id(&run_one, 1), acp_message_id(&run_one, 1));
    assert_ne!(acp_message_id(&run_one, 1), acp_message_id(&run_two, 1));
    assert_ne!(acp_message_id(&run_one, 1), acp_message_id(&run_one, 2));
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
async fn observer_starts_at_launch_after_startup_acquisition() {
    // Wrapper acquisition belongs to product startup. A run observes only launch
    // and protocol readiness, so no network/package phase can occur per turn.
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
        vec![AcpLaunchStage::Launching, AcpLaunchStage::Ready],
        "launch → ready, in order"
    );
}

#[tokio::test]
async fn observer_sees_failed_when_the_launch_faults() {
    let observer = Arc::new(RecordingObserver::default());
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("spawn claude-agent-acp: No such file or directory".into()),
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
        vec![AcpLaunchStage::Launching, AcpLaunchStage::Failed],
        "a spawn fault surfaces a Failed state"
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].messages.len(), 2);
    assert_eq!(commits[0].messages[0].text_content(), "do it");
    assert_eq!(commits[0].messages[1].text_content(), "workingdone");
    assert_eq!(
        commits[0].run_state(),
        RunState::Ended(EndCause::NaturalEnd)
    );
}

#[tokio::test]
async fn an_empty_natural_end_is_a_provider_failure() {
    let e = exec(vec![r#"{"type":"turn_end","reason":"natural_end"}"#.into()]);
    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(matches!(state, RunState::Ended(EndCause::Error(_))));
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(
        commits[0].messages.len(),
        2,
        "the input and an explanatory assistant failure are committed"
    );
    assert!(
        !commits[0].messages[1].text_content().is_empty(),
        "an empty opaque-provider turn must not remain an empty public response"
    );
}

#[tokio::test]
async fn acp_delivers_the_same_post_commit_terminal_extension_contract() {
    let e = exec(vec![
        r#"{"type":"message","text":"done"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let observer = Arc::new(TerminalRecordingObserver::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(Arc::new(RecordingCoordinator::default()))
                .with_terminal_observer(observer.clone()),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        observer.events.lock().unwrap().as_slice(),
        &[CommittedTerminalRun {
            run_id: RunId("run-1".into()),
            thread_id: ThreadId("thread-1".into()),
            cause: EndCause::NaturalEnd,
        }]
    );
}

#[tokio::test]
async fn contiguous_acp_text_chunks_commit_as_one_message_but_tools_break_the_stream() {
    let e = exec(vec![
        r#"{"type":"message","text":"before "}"#.into(),
        r#"{"type":"message","text":"tool"}"#.into(),
        r#"{"type":"tool_call","id":"c1","name":"read","input":{"path":"a.txt"}}"#.into(),
        r#"{"type":"tool_result","id":"c1","content":"body","is_error":false}"#.into(),
        r#"{"type":"message","text":"after "}"#.into(),
        r#"{"type":"message","text":"tool"}"#.into(),
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
    let messages = &commits[0].messages;
    assert_eq!(messages.len(), 5, "input, text, call, result, then text");
    assert_eq!(messages[0].text_content(), "do it");
    assert_eq!(messages[1].text_content(), "before tool");
    assert_eq!(messages[4].text_content(), "after tool");
}

#[tokio::test]
async fn acp_message_id_change_preserves_distinct_assistant_messages() {
    // Cause/effect table: C1 adjacent chunks without ids remain one legacy
    // stream; C2 a new ACP message id starts another message; C3 repeated id
    // continues it. Effects: E1 diagnostics and deliverables stay distinct,
    // while E2 token chunks for one deliverable remain coalesced.
    let e = exec(vec![
        r#"{"type":"message","text":"diagnostic"}"#.into(),
        r#"{"type":"message","text":"deliver","message_id":"026a96a1-698c-472e-9a08-ef52a4530f79"}"#.into(),
        r#"{"type":"message","text":"able","message_id":"026a96a1-698c-472e-9a08-ef52a4530f79"}"#.into(),
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
    let messages = &commits[0].messages;
    assert_eq!(messages.len(), 3, "input, diagnostic, then deliverable");
    assert_eq!(messages[1].text_content(), "diagnostic", "E1");
    assert_eq!(messages[2].text_content(), "deliverable", "E2");
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox.clone()),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1, "one commit at the terminal state");
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
async fn a_requested_pause_awaits_the_run_on_a_resume_ticket() {
    // ADR-0054 P5/U2: an operator pause requested by the next boundary awaits the ACP
    // run durably — `RunState::Awaiting` on a no-tool `ManualPause` ticket — rather than
    // ending, even though the turn reached a natural end. The turn's messages commit
    // before the await (clean commit-then-await), mirroring the native engine.
    use awaken_agent_contract::agent::awaiting::AwaitReason;
    use awaken_runtime_contract::pause::PauseSignal;

    let e = exec(vec![
        r#"{"type":"message","text":"turn"}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let pause = PauseSignal::new();
    pause.request();
    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_pause(pause),
        )
        .await
        .unwrap();

    assert_eq!(
        state,
        RunState::Awaiting,
        "a requested pause awaits, not ends"
    );
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1, "one commit at the await boundary");
    // The in-flight turn commits before the await.
    assert!(
        commits[0]
            .messages
            .iter()
            .any(|m| m.text_content() == "turn"),
        "the turn's assistant text commits before awaiting"
    );
    // A resumable no-tool `ManualPause` ticket rode the same commit.
    let ticket = commits[0]
        .resume_ticket()
        .expect("an awaiting run commits its awaiting ticket");
    assert_eq!(ticket.reason, AwaitReason::ManualPause);
    assert_eq!(ticket.run_id, RunId("run-1".into()));
    assert_eq!(ticket.thread_id, ThreadId("thread-1".into()));
    assert!(
        ticket.call_id.is_none() && ticket.pending_tool.is_none(),
        "an operator pause awaits on no tool"
    );
}

#[tokio::test]
async fn a_pause_commits_in_flight_steer_before_awaiting() {
    // Pause preempts queued input, but the in-flight steer is not lost: it rides out
    // with the await (fold) and commits before the run awaits (boundary priority is
    // pause > queued-input > idle).
    use awaken_agent_contract::agent::awaiting::AwaitReason;
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox.clone())
                .with_pause(pause),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Awaiting, "pause preempts the queued input");
    let commits = coord.commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    let steer = commits[0]
        .messages
        .iter()
        .find(|m| m.id.0 == "run-1-inbox-0")
        .expect("in-flight steer rides out with the await and commits");
    assert_eq!(steer.text_content(), "late steer");
    assert_eq!(
        commits[0].resume_ticket().map(|t| &t.reason),
        Some(&AwaitReason::ManualPause)
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
        async fn open(
            &self,
            _a: &RunActivation,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<AgentSession, OpenError> {
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
                session_model: None,
                session_mode: None,
                session_config_options: Vec::new(),
                expected_capability: None,
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
    *act.snapshot.resolved_spec.model_binding = ModelBinding::new("prov", "model", "native");
    let e = exec(vec![]);
    assert!(e.session_home_binding(&act).is_none());
}

#[tokio::test]
async fn neutral_permission_resolver_projects_the_policy_decision() {
    // The ACP permission port is decided by the single neutral `ToolPermissionPolicy`:
    // Allow→Allow, Deny→Deny, and Ask carries its durable correlation instead of
    // collapsing into a denial.
    use awaken_protocol_acp::{PermissionAsk, PermissionResolver, PermissionVerdict};
    use awaken_runtime_contract::permission::{
        ToolCall, ToolPermissionPolicy, ToolPermissionVerdict,
    };

    struct FixedPolicy(ToolPermissionVerdict);
    #[async_trait]
    impl ToolPermissionPolicy for FixedPolicy {
        async fn evaluate(&self, _ctx: &ToolCall) -> ToolPermissionVerdict {
            self.0.clone()
        }
    }

    let ask = PermissionAsk {
        tool: "bash".into(),
        call_id: "t1".into(),
        arguments: serde_json::json!({"cmd": "ls"}),
    };
    let cases = [
        (ToolPermissionVerdict::Allow, PermissionVerdict::Allow),
        (
            ToolPermissionVerdict::Deny {
                reason: "policy".into(),
            },
            PermissionVerdict::Deny,
        ),
        (
            ToolPermissionVerdict::RequireConfirmation {
                correlation_id: "tk".into(),
            },
            PermissionVerdict::Await {
                correlation_id: "tk".into(),
            },
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
async fn per_run_permission_is_an_intersection_with_acp_authority() {
    use awaken_protocol_acp::{PermissionAsk, PermissionResolver, PermissionVerdict};
    use awaken_runtime_contract::permission::{
        DenyAllTools, ToolCall, ToolPermissionPolicy, ToolPermissionVerdict,
    };

    struct FixedPolicy(ToolPermissionVerdict);
    #[async_trait]
    impl ToolPermissionPolicy for FixedPolicy {
        async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionVerdict {
            self.0.clone()
        }
    }

    struct FixedResolver(PermissionVerdict);
    #[async_trait]
    impl PermissionResolver for FixedResolver {
        async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
            self.0.clone()
        }
    }

    let ask = PermissionAsk {
        tool: "bash".into(),
        call_id: "tool-1".into(),
        arguments: serde_json::json!({"cmd": "echo unsafe"}),
    };
    let narrowing_allow = FixedPolicy(ToolPermissionVerdict::Allow);
    let base_allow = FixedResolver(PermissionVerdict::Allow);
    let base_deny = FixedResolver(PermissionVerdict::Deny);
    let deny_all = DenyAllTools::new("restricted Run");

    let narrowed = NarrowedPermissionResolver {
        base: &base_allow,
        narrowing: &deny_all,
    };
    assert_eq!(narrowed.resolve(&ask).await, PermissionVerdict::Deny);

    let cannot_widen = NarrowedPermissionResolver {
        base: &base_deny,
        narrowing: &narrowing_allow,
    };
    assert_eq!(cannot_widen.resolve(&ask).await, PermissionVerdict::Deny);
}

#[cfg(feature = "real-acp")]
#[tokio::test]
async fn permission_wait_survives_executor_replacement_and_resumes_the_loaded_session() {
    // FMECA cause-effect graph: C1=ACP agent requests permission; C2=neutral
    // policy requires confirmation; C3=the first executor/process exits after
    // committing the wait; C4=replacement loads the durable ACP session;
    // C5=operator decision. Effects: E1=one ToolPermission ticket with exact
    // correlation/tool identity; E2=first ACP request receives `cancelled` and
    // cannot keep an in-memory authority alive; E3=replacement uses
    // `session/load`; E4=exact allow/reject option is returned; E5=one terminal
    // continuation consumes the ticket.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5    | E1 | E2 | E3 | E4     | E5 |
    // |---|---|---|---|---|---|---|---|---|---|---|
    // | AR1 | T | T | T | T | allow | T | T | T | allow  | T |
    // | AR2 | T | T | T | T | deny  | T | T | T | reject | T |
    //
    // Invalid free-form input and stale/mismatched ticket rules belong to the
    // authoritative runtime resume state machine and are covered in
    // awaken-runtime/tests/awaiting.rs; this adapter test owns only ACP wire
    // projection plus replacement recovery, avoiding a duplicate validator.
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};
    use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};

    const PERMISSION: &str = r#"{"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{"sessionId":"permission-session","toolCall":{"toolCallId":"tool-1","title":"bash","rawInput":{"cmd":"echo ok"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]}}"#;

    struct AskPolicy;
    #[async_trait]
    impl ToolPermissionPolicy for AskPolicy {
        async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionVerdict {
            ToolPermissionVerdict::RequireConfirmation {
                correlation_id: "approval-1".to_string(),
            }
        }
    }

    struct PermissionSource {
        opens: AtomicUsize,
        permission_replies: Arc<Mutex<Vec<String>>>,
        resumed_prompts: Arc<Mutex<Vec<String>>>,
        permission_events: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl AgentChannelSource for PermissionSource {
        async fn open(
            &self,
            _activation: &RunActivation,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<AgentSession, OpenError> {
            let attempt = self.opens.fetch_add(1, Ordering::SeqCst);
            let replies = self.permission_replies.clone();
            let prompts = self.resumed_prompts.clone();
            let events = self.permission_events.clone();
            let (ours, theirs) = tokio::io::duplex(8192);
            tokio::spawn(async move {
                let mut lines = BufReader::new(theirs);
                let mut line = String::new();
                loop {
                    line.clear();
                    if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let message: serde_json::Value =
                        serde_json::from_str(line.trim()).expect("client JSON-RPC");
                    let id = message.get("id").and_then(serde_json::Value::as_u64);
                    let output = lines.get_mut();
                    match id {
                        Some(1) => {
                            output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true}}}\n").await.unwrap();
                            output.flush().await.unwrap();
                        }
                        Some(2) if attempt == 0 => {
                            output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"permission-session\"}}\n").await.unwrap();
                            output.flush().await.unwrap();
                        }
                        Some(2) => {
                            assert_eq!(
                                message.get("method").and_then(serde_json::Value::as_str),
                                Some("session/load"),
                                "replacement resumes the committed ACP session"
                            );
                            output
                                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n")
                                .await
                                .unwrap();
                            output.flush().await.unwrap();
                        }
                        Some(3) => {
                            if attempt > 0 {
                                let prompt = message
                                    .pointer("/params/prompt/0/content/text")
                                    .or_else(|| message.pointer("/params/prompt/0/text"))
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                prompts.lock().unwrap().push(prompt);
                            }
                            output.write_all(PERMISSION.as_bytes()).await.unwrap();
                            output.write_all(b"\n").await.unwrap();
                            output.flush().await.unwrap();
                            line.clear();
                            lines.read_line(&mut line).await.unwrap();
                            replies.lock().unwrap().push(line.trim().to_string());
                            events.notify_one();
                            if attempt == 0 {
                                return;
                            }
                            let output = lines.get_mut();
                            output.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"permission-session\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"continued\"}}}}\n").await.unwrap();
                            output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}\n").await.unwrap();
                            output.flush().await.unwrap();
                            return;
                        }
                        _ => return,
                    }
                }
            });
            Ok(AgentSession {
                channel: Box::new(ours),
                process: Arc::new(FakeProcess),
                codec: Codec::Acp,
                workspace_cwd: None,
                mcp_session_servers: Vec::new(),
                session_model: None,
                session_mode: None,
                session_config_options: Vec::new(),
                expected_capability: None,
            })
        }
    }

    for allow in [true, false] {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(tokio::sync::Notify::new());
        let source: Arc<dyn AgentChannelSource> = Arc::new(PermissionSource {
            opens: AtomicUsize::new(0),
            permission_replies: replies.clone(),
            resumed_prompts: prompts.clone(),
            permission_events: events.clone(),
        });
        let committed = Arc::new(RecordingCoordinator::default());
        let first = AcpRunExecutor::new(source.clone())
            .with_permission_policy(Arc::new(AskPolicy))
            .execute(
                activation(),
                RuntimeRunContext::new()
                    .with_commit(committed.clone())
                    .with_reader(committed.clone()),
            )
            .await
            .expect("ACP permission request commits an await");
        assert_eq!(first, RunState::Awaiting);
        let ticket = committed
            .resume_ticket_for(&RunId("run-1".into()))
            .expect("permission ticket");
        assert_eq!(ticket.reason, AwaitReason::ToolPermission);
        assert_eq!(ticket.correlation_id, "approval-1");
        assert_eq!(ticket.call_id.as_deref(), Some("tool-1"));
        assert_eq!(
            ticket
                .pending_tool
                .as_ref()
                .map(|tool| tool.tool_id.as_str()),
            Some("bash")
        );
        events.notified().await;
        assert!(
            replies.lock().unwrap()[0].contains("cancelled"),
            "the first process is released before durable HITL"
        );

        let result = ResumeResult::Decision {
            allow,
            note: (!allow).then(|| "operator policy".to_string()),
        };
        let resumed = AcpRunExecutor::new(source)
            .with_permission_policy(Arc::new(AskPolicy))
            .resume(
                activation(),
                ResumeCommand::from_ticket(&ticket, result, 1),
                RuntimeRunContext::new()
                    .with_commit(committed.clone())
                    .with_reader(committed.clone()),
            )
            .await
            .expect("replacement consumes the durable decision");
        events.notified().await;
        assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd));
        let replies = replies.lock().unwrap();
        assert_eq!(replies.len(), 2);
        assert!(replies[1].contains(if allow { "allow" } else { "reject" }));
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains(if allow { "approved" } else { "denied" }));
        assert!(
            committed
                .resume_ticket_for(&RunId("run-1".into()))
                .is_none(),
            "terminal continuation consumes the permission ticket"
        );
    }
}

#[tokio::test]
async fn a_tool_call_and_its_result_commit_as_neutral_messages() {
    use awaken_agent_contract::agent::content::ContentBlock;

    // Causal graph:
    // ACP frames -> AcpProjectedEvent staging -> executor ACL -> committed Message.
    //
    // Decision table:
    // | projected input | committed role      | committed block | correlation |
    // | tool_call       | Assistant           | ToolUse         | ACP id       |
    // | tool_result     | Tool                | ToolResult       | same ACP id  |
    // | turn_end        | no extra message    | —                | —            |
    // This is intentionally a behavior test: success requires a terminal run and
    // an externally readable neutral transcript, not merely wire deserialization.

    // The external agent surfaces a tool call, then reports it completed with output.
    let e = exec(vec![
        r#"{"type":"tool_call","id":"c1","name":"read","input":{"path":"a.txt"}}"#.into(),
        r#"{"type":"tool_result","id":"c1","content":"file body","is_error":false}"#.into(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    let messages = &commits[0].messages;
    assert_eq!(messages.len(), 3, "the input, call, and result all commit");

    // The call is an assistant ToolUse carrying the correlating id.
    assert_eq!(messages[1].role, Role::Assistant);
    match &messages[1].content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "c1");
            assert_eq!(name, "read");
            assert_eq!(input["path"], "a.txt");
        }
        other => panic!("expected a ToolUse, got {other:?}"),
    }

    // The result is a Role::Tool ToolResult addressed to that call — proving the
    // external agent's tool output now reaches the neutral transcript.
    assert_eq!(messages[2].role, Role::Tool);
    match &messages[2].content[0] {
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
async fn acp_tool_results_use_the_bound_spiller_and_fail_closed() {
    // Cause-effect graph:
    // C1=ACP projects ToolResult; C2=spiller succeeds; C3=spiller fails.
    // C1+C2 -> transformed result alone enters the neutral transcript.
    // C1+C3 -> appender rejects the fact, turn ends as an error, and no Tool
    // result carrying the unmaterialized payload commits.
    //
    // | Rule | ACP result | spill | Expected effect |
    // | A1 | yes | success | stable run/call sent once; preview committed |
    // | A2 | yes | failure | terminal error; no Tool-role result committed |
    let frames = || {
        vec![
            r#"{"type":"tool_call","id":"c1","name":"read","input":{"path":"a.txt"}}"#.into(),
            r#"{"type":"tool_result","id":"c1","content":"file body","is_error":false}"#.into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ]
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let committed = Arc::new(RecordingCoordinator::default());
    let state = exec(frames())
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(committed.clone())
                .with_tool_output_spiller(Arc::new(SpillProbe {
                    fail: false,
                    seen: seen.clone(),
                })),
        )
        .await
        .expect("A1");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "A1");
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("run-1".into(), "c1".into(), "file body".into())],
        "A1"
    );
    assert!(
        committed.commits.lock().unwrap()[0]
            .messages
            .iter()
            .any(|message| message.role == Role::Tool
                && message.text_content() == "preview: file body"),
        "A1"
    );

    let failed = Arc::new(RecordingCoordinator::default());
    let state = exec(frames())
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(failed.clone())
                .with_tool_output_spiller(Arc::new(SpillProbe {
                    fail: true,
                    seen: Arc::new(Mutex::new(Vec::new())),
                })),
        )
        .await
        .expect("A2 is a classified terminal backend failure");
    assert!(matches!(state, RunState::Ended(EndCause::Error(_))), "A2");
    assert!(
        failed.commits.lock().unwrap()[0]
            .messages
            .iter()
            .all(|message| message.role != Role::Tool),
        "A2"
    );
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
    match &commits[0].messages[1].content[0] {
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
    match &commits[0].messages[1].content[0] {
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(matches!(state, RunState::Ended(EndCause::Error(_))));
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(matches!(state, RunState::Ended(EndCause::Error(_))));
    let commits = coord.commits.lock().unwrap();
    let prompt = commits[0].messages.last().unwrap().text_content();
    // Credential-rejection prompt (auth error) surfaced to the run.
    assert!(prompt.contains("credential"));
}

#[tokio::test]
async fn refusal_maps_to_stopped() {
    let e = exec(vec![r#"{"type":"turn_end","reason":"refusal"}"#.into()]);
    let state = e
        .execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert!(matches!(state, RunState::Ended(EndCause::Stopped(_))));
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();

    assert!(
        matches!(state, RunState::Ended(EndCause::Error(_))),
        "a clean error turn must end in a terminal Error, got {state:?}"
    );
    assert_ne!(
        state,
        RunState::Ended(EndCause::NaturalEnd),
        "a reported error must never be recorded as a natural (successful) end"
    );
    // The committed run fact carries the same terminal Error — committed truth is not
    // a success either.
    let commits = coord.commits.lock().unwrap();
    assert!(matches!(
        commits[0].run_state(),
        RunState::Ended(EndCause::Error(_))
    ));
}

// A clean turn that ends on `TerminationReason::TimedOut` (the agent/supervisor
// reporting the turn hit its deadline) maps through `end_cause` to a terminal
// `Stopped`, never to a success — the last untested clean-outcome row.
#[tokio::test]
async fn a_timed_out_turn_end_maps_to_stopped_not_natural_end() {
    let e = exec(vec![r#"{"type":"turn_end","reason":"timed_out"}"#.into()]);
    let state = e
        .execute(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert!(
        matches!(state, RunState::Ended(EndCause::Stopped(_))),
        "a timed-out turn must end Stopped, got {state:?}"
    );
    assert_ne!(state, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn an_org_subscription_disabled_launch_fault_surfaces_a_credential_prompt() {
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("Your organization has disabled Claude subscription access".into()),
    }));
    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(state, RunState::Ended(EndCause::Error(_))));
    let prompt = coord.commits.lock().unwrap()[0]
        .messages
        .last()
        .unwrap()
        .text_content();
    assert!(prompt.contains("credential"), "{prompt}");
}

#[tokio::test]
async fn a_login_required_launch_fault_surfaces_a_login_prompt() {
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![],
        open_error: Some("Please run /login to continue".into()),
    }));
    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coord.clone()),
        )
        .await
        .unwrap();
    assert!(matches!(state, RunState::Ended(EndCause::Error(_))));
    let prompt = coord.commits.lock().unwrap()[0]
        .messages
        .last()
        .unwrap()
        .text_content()
        .to_lowercase();
    assert!(prompt.contains("login"), "{prompt}");
}

// ── R7: ACP mid-switch relaunches the CLI per turn ───────────────────────────

#[tokio::test]
async fn acp_relaunches_the_cli_every_turn_so_a_model_switch_takes_effect() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSource(Arc<AtomicUsize>);
    #[async_trait]
    impl AgentChannelSource for CountingSource {
        async fn open(
            &self,
            _a: &RunActivation,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<AgentSession, OpenError> {
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
                session_model: None,
                session_mode: None,
                session_config_options: Vec::new(),
                expected_capability: None,
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
    fn model(
        &self,
        _a: &RunActivation,
        _context: &RuntimeRunContext,
    ) -> std::result::Result<ResolvedModel, OpenError> {
        Ok(self.0.clone())
    }
    fn extra_env(
        &self,
        _a: &RunActivation,
    ) -> std::result::Result<Vec<(String, String)>, OpenError> {
        Ok(vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/run/agent/.claude".to_string(),
        )])
    }
}

fn projected_env<'a>(launch: &'a AcpLaunch, key: &str) -> Option<&'a str> {
    launch
        .env
        .iter()
        .find(|var| var.name == key)
        .map(|var| match &var.value {
            awaken_provisioning_contract::EnvValue::Inline { value } => value.as_str(),
            awaken_provisioning_contract::EnvValue::Secret { reference } => reference.as_str(),
        })
}

#[test]
fn projecting_source_plans_launch_from_resolved_model_and_host_env() {
    let cli = *acp_cli("claude").expect("claude in the catalog");
    let resolver = Arc::new(FixedModel(ResolvedModel::Managed {
        base_url: "https://api.kimi.com/coding/".to_string(),
        model: "kimi-k2".to_string(),
        process_secret: Some(ProcessSecretRequirement::new("lease://projected-host")),
        credential_artifact: None,
        acp: None,
    }));
    // The resolver supplies the config-home path as non-secret per-run env.
    let source = ProjectingChannelSource::new(cli, resolver);
    let launch = source
        .plan(&activation(), &RuntimeRunContext::new())
        .expect("plan");
    let env = |k: &str| projected_env(&launch, k);
    assert_eq!(launch.argv, vec!["claude-agent-acp"]);
    assert_eq!(env("ANTHROPIC_MODEL"), Some("kimi-k2"));
    assert_eq!(
        env("ANTHROPIC_BASE_URL"),
        Some("https://api.kimi.com/coding/")
    );
    assert_eq!(env("ANTHROPIC_API_KEY"), Some("lease://projected-host"));
    // The host-provided config-home path threads through as extra env.
    assert_eq!(env("CLAUDE_CONFIG_DIR"), Some("/run/agent/.claude"));
}

/// A resolver that supplies a config-home dir under an arbitrary env key.
#[cfg(unix)]
struct ConfigHomeAt {
    key: &'static str,
    dir: String,
}
#[cfg(unix)]
impl LaunchResolver for ConfigHomeAt {
    fn model(
        &self,
        _a: &RunActivation,
        _context: &RuntimeRunContext,
    ) -> std::result::Result<ResolvedModel, OpenError> {
        Ok(ResolvedModel::Managed {
            base_url: "u".into(),
            model: "m".into(),
            process_secret: None,
            credential_artifact: None,
            acp: None,
        })
    }
    fn extra_env(
        &self,
        _a: &RunActivation,
    ) -> std::result::Result<Vec<(String, String)>, OpenError> {
        Ok(vec![(self.key.to_string(), self.dir.clone())])
    }
}

/// End to end through the real launch path: a run that declares an MCP server on its ACP
/// plugin config (what the host's `overlay_acp_mcp` produces) makes `open()` write the
/// a legacy adapter config into an isolated config home before it spawns — proving the whole
/// host→plugin_config→config-file chain. The file contains only the mediated route;
/// credential material and references remain outside the ACP process contract.
#[tokio::test]
#[cfg(unix)]
async fn open_writes_legacy_mcp_config_into_the_isolated_config_home() {
    let dir = std::env::temp_dir().join(format!("awaken-acpmcp-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A synthetic legacy ConfigFileToml row with a cheap spawnable command. Codex
    // deliberately does not use this protocol.
    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.acquisition = AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", "exit 0"],
    };
    cli.mcp_interface = McpInterface::ConfigFileToml {
        path: "config.toml",
    };
    cli.config_home_env = Some("TEST_CONFIG_HOME");
    cli.session_export_excludes = &["config.toml"];
    let source = ProjectingChannelSource::new(
        cli,
        Arc::new(ConfigHomeAt {
            key: cli.config_home_env.expect("test CLI has config home"),
            dir: dir.to_string_lossy().to_string(),
        }),
    );

    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({
            "mcp_servers": [{
                "name": "github",
                "transport": { "kind": "http", "url": "http://127.0.0.1/session/t1/mcp/github/1" }
            }]
        }),
    );

    // open() writes the config.toml before spawning the (immediately-exiting) child.
    let session = source
        .open(&act, &RuntimeRunContext::new())
        .await
        .expect("open");
    drop(session); // reap the child

    let written = std::fs::read_to_string(dir.join("config.toml")).expect("config.toml written");
    assert!(
        written.contains("[mcp_servers.github]"),
        "server section present: {written}"
    );
    assert!(
        !written.to_ascii_lowercase().contains("credential")
            && !written.to_ascii_lowercase().contains("authorization"),
        "ACP config contains no credential channel: {written}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fake ACP agent (JSON-RPC, shell builtins only) reports whether `session/new`
/// carried the MCP server and whether a forbidden credential marker leaked. It
/// classifies the request as `saw-github` plus either `noauth` or `credential-leaked`.
/// Thus the test asserts the D5 wire (plugin_config →
/// `to_session_mcp_server` → `to_acp_mcp_servers` → `session/new`) actually reached the CLI.
#[cfg(all(feature = "real-acp", unix))]
const FAKE_ACP_MCP_ECHO_SCRIPT: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
        *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
        *'\"id\":3'*) \
          M=none; case \"$SN\" in *github*) M=saw-github;; esac; \
          A=noauth; case \"$SN\" in *'broker://'*) A=credential-leaked;; *'sk-trusted'*) A=credential-leaked;; *'authorization'*) A=credential-leaked;; esac; \
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"mcp %s %s\"}}}}\\n' \"$M\" \"$A\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

/// End to end over the REAL ACP JSON-RPC codec: a run that declares an MCP server on
/// its ACP plugin config (what the host's `overlay_acp_mcp` produces for an `AcpSession`
/// CLI) makes `open()` stage it as a session server and `drive()` inject it into the
/// `session/new` request — the D5 seam. The fake agent echoes that it saw the server and
/// that no credential channel is present, proving the whole
/// host→plugin_config→session/new chain is route-only. Gated on `real-acp`: only
/// the official codec serializes `mcpServers` into `session/new`.
#[cfg(feature = "real-acp")]
#[tokio::test]
#[cfg(unix)]
async fn open_and_drive_inject_the_mcp_server_into_session_new_for_an_acp_session_cli() {
    // A claude row (AcpSession, session/new delivery) with a cheap JSON-RPC echo agent.
    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.acquisition = AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", FAKE_ACP_MCP_ECHO_SCRIPT],
    };
    let source = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel::Managed {
            base_url: "u".into(),
            model: "m".into(),
            process_secret: None,
            credential_artifact: None,
            acp: None,
        })),
    ));
    let e = AcpRunExecutor::new(source);

    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({
            "mcp_servers": [{
                "name": "github",
                "transport": { "kind": "http", "url": "http://127.0.0.1/session/t1/mcp/github/1" }
            }]
        }),
    );

    let coord = Arc::new(RecordingCoordinator::default());
    let state = e
        .execute(act, RuntimeRunContext::new().with_commit(coord.clone()))
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    let commits = coord.commits.lock().unwrap();
    let reply = commits[0].messages.last().unwrap().text_content();
    assert_eq!(
        reply, "mcp saw-github noauth",
        "session/new must carry the mediated MCP route without a credential, got {reply:?}",
    );
}

/// Retained snapshots may still contain the deleted credential field. Decode compatibility
/// must not revive that path: the ACP launch receives only the mediated route.
#[cfg(feature = "real-acp")]
#[tokio::test]
#[cfg(unix)]
async fn retained_inline_mcp_credential_is_ignored_before_session_new() {
    const LEGACY_ECHO: &str = "while IFS= read -r line; do \
          case \"$line\" in \
            *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}';; \
            *'\"id\":2'*) SN=\"$line\"; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}';; \
            *'\"id\":3'*) \
              A=no-secret; case \"$SN\" in *sk-trusted*) A=credential-leaked;; esac; \
              printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"mcp %s\"}}}}\\n' \"$A\"; \
              printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
              exit 0;; \
          esac; \
        done";
    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.acquisition = AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", LEGACY_ECHO],
    };
    let source = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel::Managed {
            base_url: "u".into(),
            model: "m".into(),
            process_secret: None,
            credential_artifact: None,
            acp: None,
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
    let reply = coord.commits.lock().unwrap()[0]
        .messages
        .last()
        .unwrap()
        .text_content();
    assert_eq!(
        reply, "mcp no-secret",
        "retained credential fields must not reach session/new, got {reply:?}",
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
#[cfg(unix)]
async fn acp_session_id_is_carried_across_the_per_turn_relaunch() {
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    // id:1 initialize (advertise loadSession) · id:2 session/new|load · id:3 prompt.
    // The id:2 request distinguishes the verb by whether the carried id `s1` is present
    // in it: turn 1's session/new has none (→ `new`, returns sessionId s1); turn 2's
    // session/load carries `s1` (→ `load-s1`, empty result). The turn's agent message
    // echoes which verb fired, so the committed transcript proves the carry.
    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.acquisition = AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", SESSION_CARRY_AGENT],
    };
    let source = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel::Managed {
            base_url: "u".into(),
            model: "m".into(),
            process_secret: None,
            credential_artifact: None,
            acp: None,
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
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox.clone()),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
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

/// A durable ACP pause survives executor/process replacement. The replacement
/// validates the committed ticket, restores the protocol session id from thread
/// state, and issues `session/load` before continuing the SAME Run.
#[cfg(feature = "real-acp")]
#[tokio::test]
async fn paused_run_resumes_after_executor_replacement_with_the_committed_session_id() {
    use awaken_runtime_contract::pause::PauseSignal;
    use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};

    let mut cli = *acp_cli("claude").expect("claude in the catalog");
    cli.acquisition = AcpAcquisition::Direct {
        executable: "/bin/sh",
        args: &["-c", SESSION_CARRY_AGENT],
    };
    let source: Arc<dyn AgentChannelSource> = Arc::new(ProjectingChannelSource::new(
        cli,
        Arc::new(FixedModel(ResolvedModel::Managed {
            base_url: "u".into(),
            model: "m".into(),
            process_secret: None,
            credential_artifact: None,
            acp: None,
        })),
    ));
    let committed = Arc::new(RecordingCoordinator::default());
    let pause = PauseSignal::new();
    pause.request();

    let first = AcpRunExecutor::new(source.clone())
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(committed.clone())
                .with_reader(committed.clone())
                .with_pause(pause),
        )
        .await
        .expect("first ACP attempt pauses durably");
    assert_eq!(first, RunState::Awaiting);
    let ticket = committed
        .resume_ticket_for(&RunId("run-1".into()))
        .expect("pause committed an active ticket");

    // A new executor models worker/process replacement; no live in-memory ACP
    // session is shared with the first attempt.
    let resumed = AcpRunExecutor::new(source)
        .resume(
            activation(),
            ResumeCommand::from_ticket(&ticket, ResumeResult::Input("continue".into()), 1),
            RuntimeRunContext::new()
                .with_commit(committed.clone())
                .with_reader(committed.clone()),
        )
        .await
        .expect("replacement resumes the committed ACP run");

    assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd));
    let texts: Vec<String> = committed
        .messages()
        .iter()
        .map(Message::text_content)
        .collect();
    assert!(texts.iter().any(|text| text == "turn:new"));
    assert!(
        texts.iter().any(|text| text == "turn:load-s1"),
        "replacement must restore the durable id and use session/load; got {texts:?}"
    );
    assert!(
        committed
            .resume_ticket_for(&RunId("run-1".into()))
            .is_none(),
        "terminal resume consumes the active ticket"
    );
}

#[test]
fn projecting_source_reads_the_cli_compact_window_from_config() {
    let cli = *acp_cli("claude").expect("claude in the catalog");
    let resolver = Arc::new(FixedModel(ResolvedModel::Managed {
        base_url: "u".to_string(),
        model: "m".to_string(),
        process_secret: None,
        credential_artifact: None,
        acp: None,
    }));
    let source = ProjectingChannelSource::new(cli, resolver);

    // The run carries an ACP-scoped compaction window in plugin_config.
    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({ "compact_window": 262144 }),
    );
    let launch = source.plan(&act, &RuntimeRunContext::new()).expect("plan");
    let window = launch
        .env
        .iter()
        .find(|var| var.name == "CLAUDE_CODE_AUTO_COMPACT_WINDOW")
        .and_then(|var| match &var.value {
            awaken_provisioning_contract::EnvValue::Inline { value } => Some(value.as_str()),
            awaken_provisioning_contract::EnvValue::Secret { .. } => None,
        });
    assert_eq!(window, Some("262144"));
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
        async fn open(
            &self,
            _a: &RunActivation,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<AgentSession, OpenError> {
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
                session_model: None,
                session_mode: None,
                session_config_options: Vec::new(),
                expected_capability: None,
            })
        }
    }

    let token = CancellationToken::new();
    token.cancel(); // pre-cancelled → the supervisor's cancel arm fires deterministically
    let coord = Arc::new(RecordingCoordinator::default());
    let state = AcpRunExecutor::new(Arc::new(HangingSource))
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_cancellation(token),
        )
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
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
        async fn open(
            &self,
            _a: &RunActivation,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<AgentSession, OpenError> {
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
                session_model: None,
                session_mode: None,
                session_config_options: Vec::new(),
                expected_capability: None,
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
    let state = AcpRunExecutor::new(Arc::new(FlakySource(opens.clone())))
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coord.clone())
                .with_live_inbox(inbox),
        )
        .await
        .unwrap();

    assert!(
        matches!(state, RunState::Ended(EndCause::Error(_))),
        "a mid-run relaunch-open failure ends the run classified, got {state:?}"
    );
    assert_eq!(
        opens.load(Ordering::SeqCst),
        2,
        "the first open ran the turn; the relaunch attempted a second open and failed"
    );
}

// ── Backend matrix (P1-#4): every catalog CLI drives, not just claude ─────────
//
// The per-row *projection* is covered exhaustively in `acp_cli.rs`
// (`every_cli_*`), and claude is driven end-to-end over the real ACP codec
// (`open_and_drive_inject_*`). What was missing is the parity across the whole
// catalog *through the executor's own launch seam*: that EVERY row
// (claude/codex/gemini/opencode) projects a launchable process through
// `ProjectingChannelSource` and drives a plain turn to a committed reply. This is
// the hermetic analogue of awaken-next's `e2e_external_cli_acp` multi-CLI matrix
// — no real binaries, so the drive path's row-agnosticism is pinned in CI.

/// A model resolver for the matrix: fixed coordinates, no per-run env (a plain
/// turn declares no MCP, so no config-home is needed — keeping it row-agnostic).
struct MatrixModel {
    artifact_path: Option<&'static str>,
}

impl MatrixModel {
    fn for_cli(cli: &AcpCli) -> Self {
        Self {
            artifact_path: cli
                .managed_credential_delivery
                .credential_artifact(false)
                .map(|artifact| artifact.relative_path),
        }
    }
}

struct MatrixSecretBroker;

#[async_trait]
impl awaken_provisioning_contract::SecretBroker for MatrixSecretBroker {
    async fn materialize(
        &self,
        reference: &str,
    ) -> std::result::Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        (reference == "lease://matrix")
            .then(|| br#"{"OPENAI_API_KEY":"matrix-secret"}"#.to_vec()) // awaken-allow: secret
            .ok_or_else(|| awaken_provisioning_contract::SandboxError::new("unknown lease"))
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> std::result::Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        (reference == "lease://matrix")
            .then(|| b"matrix-secret".to_vec())
            .ok_or_else(|| awaken_provisioning_contract::SandboxError::new("unknown lease"))
    }

    async fn write_back(
        &self,
        _reference: &str,
        _bytes: Vec<u8>,
    ) -> std::result::Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::new(
            "not supported",
        ))
    }
}

impl LaunchResolver for MatrixModel {
    fn model(
        &self,
        _a: &RunActivation,
        _context: &RuntimeRunContext,
    ) -> std::result::Result<ResolvedModel, OpenError> {
        Ok(ResolvedModel::Managed {
            base_url: "https://gateway.example/anthropic".to_string(),
            model: "MiniMax-M2".to_string(),
            process_secret: self
                .artifact_path
                .is_none()
                .then(|| ProcessSecretRequirement::new("lease://matrix")),
            credential_artifact: self
                .artifact_path
                .map(|path| CredentialArtifactRequirement::new("lease://matrix", path)),
            acp: None,
        })
    }

    fn secret_broker(&self) -> Option<Arc<dyn awaken_provisioning_contract::SecretBroker>> {
        Some(Arc::new(MatrixSecretBroker))
    }
}

#[test]
fn every_backend_row_projects_a_launchable_process_through_the_source() {
    // The source seam (not just `AcpCli::project`) must be row-agnostic: for each
    // catalog CLI, `ProjectingChannelSource::plan` yields the row's own command as
    // argv[0] and delivers the resolved model under that row's env keys.
    let reference_cli = &known_acp_clis()[0];
    let model = MatrixModel::for_cli(reference_cli)
        .model(&activation(), &RuntimeRunContext::new())
        .unwrap();
    let ResolvedModel::Managed {
        base_url,
        model: model_id,
        ..
    } = &model
    else {
        unreachable!()
    };
    for cli in known_acp_clis() {
        let source = ProjectingChannelSource::new(*cli, Arc::new(MatrixModel::for_cli(cli)));
        let planned = source.plan(&activation(), &RuntimeRunContext::new());
        let Some(d) = cli.model_delivery.as_ref() else {
            let error = planned.unwrap_err();
            assert!(error.0.contains("credential_driver_required"), "{}", cli.id);
            continue;
        };
        let launch = planned.expect("plan");
        let env = |k: &str| projected_env(&launch, k);
        assert_eq!(
            launch.argv.first().map(String::as_str),
            Some(cli.acquisition.executable()),
            "{}: argv[0] is the row's command",
            cli.id
        );
        assert_eq!(
            env(d.base_url),
            Some(base_url.as_str()),
            "{}: base_url delivered",
            cli.id
        );
        assert_eq!(
            env(d.model),
            Some(model_id.as_str()),
            "{}: model delivered",
            cli.id
        );
        if cli.managed_credential_delivery.allows_process_secret() {
            assert_eq!(
                env(d
                    .default_credential_env()
                    .expect("catalog process-secret delivery has a default"),),
                Some("lease://matrix"),
                "{}: process secret delivered by the host",
                cli.id
            );
        } else {
            assert!(
                d.default_credential_env()
                    .is_none_or(|key| env(key).is_none()),
                "{}: artifact credentials never enter process env",
                cli.id
            );
        }
    }
}

/// A generic ACP JSON-RPC agent (shell builtins only) that answers any prompt with
/// the single word `pong` and a natural `end_turn`. Handshake: `id:1` initialize,
/// `id:2` session/new, `id:3` prompt → `session/update` chunk + result. Used to
/// drive each catalog row hermetically over the official codec.
#[cfg(all(feature = "real-acp", unix))]
const PONG_ECHO: &str = "while IFS= read -r line; do \
      case \"$line\" in \
        *'\"id\":1'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{},\"authMethods\":[{\"id\":\"api-key\",\"name\":\"API key\"}]}}';; \
        *'\"id\":5'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}';; \
        *'\"id\":6'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{}}';; \
        *'\"id\":2'*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\",\"models\":{\"currentModelId\":\"default\",\"availableModels\":[]}}}';; \
        *'\"id\":3'*) \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"pong\"}}}}'; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

/// Drive EVERY catalog row to a committed reply over the real ACP JSON-RPC codec.
/// Each row is launched via `ProjectingChannelSource` (the production seam), with
/// its command swapped for the hermetic `PONG_ECHO` agent — so the projection,
/// spawn, codec, drive, and commit path is exercised identically for
/// claude/codex/gemini/opencode. Gated on `real-acp`: only the official codec
/// serializes the `session/new`→prompt handshake the agent answers.
#[cfg(feature = "real-acp")]
#[tokio::test]
#[cfg(unix)]
async fn every_backend_row_drives_a_plain_turn_to_a_committed_reply() {
    for row in known_acp_clis() {
        if row.model_delivery.is_none() {
            let error = ProjectingChannelSource::new(*row, Arc::new(MatrixModel::for_cli(row)))
                .plan(&activation(), &RuntimeRunContext::new())
                .expect_err("driver-managed row must fail before spawn");
            assert_eq!(error.0, format!("credential_driver_required: {}", row.id));
            continue;
        }
        let mut cli = *row;
        cli.acquisition = AcpAcquisition::Direct {
            executable: "/bin/sh",
            args: &["-c", PONG_ECHO],
        };
        let source = Arc::new(ProjectingChannelSource::new(
            cli,
            Arc::new(MatrixModel::for_cli(row)),
        ));
        let exec = AcpRunExecutor::new(source);

        // Reflect the scenario: the run binds this row's backend (acp:<id>).
        let mut act = activation();
        *act.snapshot.resolved_spec.model_binding =
            ModelBinding::new("prov", "model", format!("acp:{}", row.id));

        let coord = Arc::new(RecordingCoordinator::default());
        let state = exec
            .execute(act, RuntimeRunContext::new().with_commit(coord.clone()))
            .await
            .unwrap_or_else(|e| panic!("{}: drive failed: {e:?}", row.id));

        assert_eq!(
            state,
            RunState::Ended(EndCause::NaturalEnd),
            "{}: a plain turn ends naturally",
            row.id
        );
        let commits = coord.commits.lock().unwrap();
        let reply = commits
            .first()
            .and_then(|c| c.messages.last())
            .map(|m| m.text_content())
            .unwrap_or_default();
        assert_eq!(reply, "pong", "{}: committed the agent's reply", row.id);
    }
}
