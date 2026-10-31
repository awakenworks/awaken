//! T33-C03 (cross-adapter consistency), ACP row: the shared injected upstream
//! fault (a 529 `overloaded_error` carrying a Retry-After hint, see
//! `awaken-runtime/tests/cross_adapter_fault_local.rs`) reaches the production
//! `AcpRunExecutor` the way an opaque ACP CLI reports it: as the provider's
//! structured error envelope followed by a clean `end_turn`, over a real
//! tokio duplex byte stream in the newline ACP codec.
//!
//! | rule | ACP agent output                          | expected terminal                               |
//! | A1   | provider error envelope, then end_turn    | `Error`, provider account kept, retryable class |
//! | A2   | ordinary text, then end_turn (control)    | `NaturalEnd` (the checker sees real success)    |
//!
//! Oracles: O0 authority/uncertainty/retry hint preserved; O1 never success.
//! O1 is asserted below. Since ADR-0103 the executor's `failure_cause` keeps
//! the native `overloaded` (retryable) classification, which the former
//! reproducer asserts.
//! Negative control (executed): A2 drives the same adapter to a genuine
//! success, and a mutant projection of the real A1 terminal to `NaturalEnd`;
//! the same checker must flag both as success.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use awaken_run_executor_acp::{
    AcpInstructions, AcpRunExecutor, AgentChannelSource, AgentSession, ChannelOpenDoor, Codec,
    OpenError,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The provider's own account of the shared fault, including its retry hint.
const UPSTREAM_DETAIL: &str = "upstream 529 overloaded_error (retry-after 7s)";
/// The native codes whose class `Error::is_retryable` reports as retryable.
const RETRYABLE_CODES: [&str; 4] = ["provider_error", "rate_limited", "overloaded", "timeout"];

#[derive(Clone, Debug, PartialEq, Eq)]
struct Classification {
    authority: String,
    uncertain: bool,
    retryable: bool,
    success: bool,
}

fn classify(state: &RunState) -> Classification {
    match state {
        RunState::Ended(EndCause::Error(failure)) => Classification {
            authority: failure.code().to_string(),
            uncertain: false,
            retryable: RETRYABLE_CODES.contains(&failure.code()),
            success: false,
        },
        RunState::Ended(EndCause::Indeterminate) => Classification {
            authority: "indeterminate".to_string(),
            uncertain: true,
            retryable: false,
            success: false,
        },
        RunState::Ended(EndCause::NaturalEnd) => Classification {
            authority: "natural_end".to_string(),
            uncertain: false,
            retryable: false,
            success: true,
        },
        other => Classification {
            authority: format!("{other:?}"),
            uncertain: false,
            retryable: false,
            success: false,
        },
    }
}

/// O1: the injected fault never surfaces as success.
fn check_not_success(adapter: &str, observed: &Classification) -> Result<(), String> {
    if observed.success {
        return Err(format!(
            "O1 {adapter}: the injected fault surfaced as success: {observed:?}"
        ));
    }
    Ok(())
}

/// O0 for the certain, retryable F1 fault: the native classification is a
/// certain failure whose class is retryable.
fn check_retryable_authority(adapter: &str, observed: &Classification) -> Result<(), String> {
    check_not_success(adapter, observed)?;
    if observed.uncertain || !observed.retryable {
        return Err(format!(
            "O0 {adapter}: expected a certain failure with a retryable class \
             (one of {RETRYABLE_CODES:?}), observed {observed:?}"
        ));
    }
    Ok(())
}

struct ExitedProcess;

#[async_trait]
impl ProcessHandle for ExitedProcess {
    fn id(&self) -> &str {
        "t33-c03-acp-agent"
    }
    async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        Ok(ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }
    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
        Ok(Some(ExitStatus {
            code: Some(0),
            signaled: false,
        }))
    }
    async fn signal(&self, _signal: Signal) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// A scripted ACP agent: reads the prompt line, then writes its frames.
struct ScriptedAgent {
    frames: Vec<String>,
}

#[async_trait]
impl AgentChannelSource for ScriptedAgent {
    async fn open(
        &self,
        _activation: &RunActivation,
        _context: &RuntimeRunContext,
        _door: ChannelOpenDoor,
    ) -> Result<AgentSession, OpenError> {
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let frames = self.frames.clone();
        tokio::spawn(async move {
            let mut prompt = String::new();
            let mut reader = BufReader::new(&mut theirs);
            let _ = reader.read_line(&mut prompt).await;
            for frame in frames {
                let _ = theirs.write_all(frame.as_bytes()).await;
                let _ = theirs.write_all(b"\n").await;
                let _ = theirs.flush().await;
            }
        });
        Ok(AgentSession {
            channel: Box::new(ours),
            process: Arc::new(ExitedProcess),
            codec: Codec::Newline,
            workspace_cwd: None,
            mcp_session_servers: Vec::new(),
            session_model: None,
            session_mode: None,
            tool_aliases: Default::default(),
            session_config_options: Vec::new(),
            expected_capability: None,
            instructions: AcpInstructions::default(),
        })
    }
}

#[derive(Default)]
struct RecordingCoordinator {
    commits: Mutex<Vec<ThreadCommit>>,
}

#[async_trait]
impl Coordinator for RecordingCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        let mut commits = self.commits.lock().unwrap();
        commits.push(commit);
        Ok(CommitRecord {
            sequence: commits.len() as u64,
        })
    }
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("t33-c03-acp-run".into()),
        ThreadId("t33-c03-acp-thread".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("t33-c03-acp-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("t33-c03-acp-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("t33-c03-acp".into()),
                instructions: String::new(),
                max_steps: Some(8),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::parse("prov", "model", "acp:claude").expect("valid test binding"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("t33-c03-acp".into()),
        },
        vec![Message::text(
            MessageId("t33-c03-acp-user".into()),
            Role::User,
            "do it",
        )],
    )
}

/// A1: the shared fault as the opaque CLI reports it.
fn injected_fault_frames() -> Vec<String> {
    vec![
        serde_json::json!({
            "type": "message",
            "text": serde_json::json!({
                "error": { "type": "overloaded_error", "message": UPSTREAM_DETAIL }
            })
            .to_string(),
        })
        .to_string(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.to_string(),
    ]
}

/// A2: an ordinary successful turn (the control row).
fn successful_frames() -> Vec<String> {
    vec![
        r#"{"type":"message","text":"done"}"#.to_string(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.to_string(),
    ]
}

/// Drive the production executor; returns the returned state and every commit.
async fn run_acp(frames: Vec<String>) -> (RunState, Vec<ThreadCommit>) {
    let executor = AcpRunExecutor::new(Arc::new(ScriptedAgent { frames }));
    let coordinator = Arc::new(RecordingCoordinator::default());
    let state = executor
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(coordinator.clone()),
        )
        .await
        .expect("the ACP executor commits a terminal");
    let commits = coordinator.commits.lock().unwrap().clone();
    (state, commits)
}

fn committed_terminals(commits: &[ThreadCommit]) -> Vec<RunState> {
    commits
        .iter()
        .map(ThreadCommit::run_state)
        .filter(|state| matches!(state, RunState::Ended(_)))
        .collect()
}

#[tokio::test]
async fn acp_executor_never_reports_the_injected_upstream_fault_as_success() {
    let (state, commits) = run_acp(injected_fault_frames()).await;
    let terminals = committed_terminals(&commits);
    assert_eq!(
        terminals,
        vec![state.clone()],
        "A1: one committed terminal, the returned one"
    );
    check_not_success("acp", &classify(&state))
        .unwrap_or_else(|violation| panic!("A1: {violation}"));
    let RunState::Ended(EndCause::Error(failure)) = &state else {
        panic!("A1: the provider envelope must end as an execution failure, got {state:?}");
    };
    assert_eq!(
        failure.message(),
        UPSTREAM_DETAIL,
        "A1: the provider's own account (with its hint text) is the failure message"
    );
    let explanation = commits
        .iter()
        .flat_map(|commit| commit.messages.iter())
        .rev()
        .find(|message| message.role == Role::Assistant)
        .expect("A1: an explanatory assistant message is committed")
        .text_content();
    assert!(
        explanation.contains(UPSTREAM_DETAIL),
        "A1: the committed explanation carries the provider account: {explanation}"
    );
}

#[tokio::test]
async fn acp_fault_checker_detects_success_from_the_real_adapter_and_a_downgrading_mutant() {
    // A2: a genuine success through the same adapter is seen as success.
    let (success, _) = run_acp(successful_frames()).await;
    assert_eq!(success, RunState::Ended(EndCause::NaturalEnd), "A2");
    let violation = check_not_success("acp", &classify(&success))
        .expect_err("A2: the checker observes real adapter success");
    assert!(violation.starts_with("O1"), "{violation}");

    // A mutant of the real A1 terminal that drops the failure is flagged.
    let (state, _) = run_acp(injected_fault_frames()).await;
    assert!(
        check_not_success("acp", &classify(&state)).is_ok(),
        "A1 unmutated"
    );
    let downgraded = RunState::Ended(EndCause::NaturalEnd);
    assert_ne!(downgraded, state);
    assert!(
        check_not_success("acp-mutant", &classify(&downgraded)).is_err(),
        "the checker detects a silently downgraded ACP projection"
    );
    // The O0 checker is not vacuous: a native retryable classification passes.
    assert!(
        check_retryable_authority(
            "native",
            &Classification {
                authority: "overloaded".to_string(),
                uncertain: false,
                retryable: true,
                success: false,
            }
        )
        .is_ok()
    );
}

#[tokio::test]
async fn acp_executor_keeps_the_retryable_class_of_the_injected_overloaded_upstream() {
    let (state, _) = run_acp(injected_fault_frames()).await;
    check_retryable_authority("acp", &classify(&state))
        .unwrap_or_else(|violation| panic!("A1: {violation}"));
}
