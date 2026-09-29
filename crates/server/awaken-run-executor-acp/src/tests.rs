// @formal evidence acp.mcp_alias_permission_consensus_never_widens_authority
// @formal evidence acp.permission_tool_identity_normalization_is_exact
// @formal evidence acp.process_cleanup_requires_observed_exit_and_retained_owner
// @formal evidence session_execution.one_physical_attempt_per_thread
// awaken-file-limit-ceiling: 5856 (legacy oversized test file: split, never grow)
//! End-to-end tests with in-memory fakes: a scripted duplex channel stands in for
//! the launched CLI — no daemon, no network.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Record as RunRecord, RunState};
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::{AttemptOwnershipError, RuntimeRunContext};
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

// All scripted sources share this one empty configuration fixture. Individual
// tests still own their exact channel, process, codec and recovery cwd.
fn fixture_session(
    channel: Box<dyn AgentChannel>,
    process: Arc<dyn ProcessHandle>,
    codec: Codec,
    workspace_cwd: Option<String>,
) -> AgentSession {
    AgentSession {
        channel,
        process,
        codec,
        workspace_cwd,
        mcp_session_servers: Vec::new(),
        session_model: None,
        session_mode: None,
        tool_aliases: Default::default(),
        session_config_options: Vec::new(),
        expected_capability: None,
        instructions: crate::AcpInstructions::default(),
    }
}

#[tokio::test]
async fn acp_permission_titles_use_the_canonical_mcp_tool_identity() {
    // Cause/effect graph: C1 Codex reports an MCP permission title in dotted
    // ACP form; C2 a native/unknown title has no MCP prefix. Effects: E1 C1 is
    // evaluated against the one canonical mcp__server__tool policy identity;
    // E2 C2 remains byte-for-byte unchanged. This adapter normalization does
    // not add a second policy or authorization path.
    //
    // | Rule | ACP title | Policy tool id | Verdict |
    // |---|---|---|---|
    // | M1 | mcp.pilot.set_plan | mcp__pilot__set_plan | allow |
    // | M2 | mcp.awaken_session.write | write | allow |
    // | M3 | bash | bash | allow |
    struct ExactPolicy {
        expected: String,
    }
    #[async_trait]
    impl ToolPermissionPolicy for ExactPolicy {
        async fn evaluate(&self, call: &ToolCall) -> ToolPermissionRuling {
            assert_eq!(call.tool_id, self.expected);
            ToolPermissionRuling::policy(
                ToolPermissionVerdict::Allow,
                awaken_runtime_contract::permission::PermissionBasis::fixed(
                    awaken_runtime_contract::permission::PermissionPolicy::Rule,
                ),
            )
        }
    }

    for (rule, title, expected) in [
        ("M1", "mcp.pilot.set_plan", "mcp__pilot__set_plan"),
        ("M2", "mcp.awaken_session.write", "write"),
        ("M3", "bash", "bash"),
        // M4 Claude SDK canonical MCP target; M5 external identity stays exact.
        ("M4", "mcp__awaken_session__write", "write"),
        ("M5", "mcp__pilot__set_plan", "mcp__pilot__set_plan"),
        // M6 a dotted title whose SERVER carries dots: the tool name is the
        // final segment, so the title lands on the namespace of the configured
        // `a.b` server. Splitting on the first dot produced `mcp__a__b.c`, an
        // identity no configured server owned and therefore no policy matched.
        ("M6", "mcp.a.b.c", "mcp__a-2eb__c"),
        // M7 a punctuated server in dotted form reaches the same canonical
        // identity the MCP producer emits for it.
        ("M7", "mcp.github-mcp.search", "mcp__github-2dmcp__search"),
    ] {
        let resolver = NeutralPermissionResolver {
            policy: Arc::new(ExactPolicy {
                expected: expected.into(),
            }),
            grant_observer: None,
            audit: None,
        };
        assert_eq!(
            resolver
                .resolve(&PermissionAsk {
                    tool: title.into(),
                    call_id: format!("{rule}-call"),
                    arguments: serde_json::json!({}),
                })
                .await,
            PermissionVerdict::Allow,
            "{rule}"
        );
    }
}

#[tokio::test]
async fn owned_client_result_uses_the_shared_tool_batch_before_reopening_acp() {
    // Cause/effect decision table: C1 a RuntimeToolBatch ticket names the
    // continuation's ClientExecution tool; C2 the Session supplies the exact
    // ToolResult. Effect: the unchanged command reaches the existing owned port
    // once and ACP is not opened. C3 a permission result or wrong tool is
    // rejected by ticket validation/routing in the surrounding matrix tests.
    let committed = Arc::new(RecordingCoordinator::default());
    let activation = activation_without_session_home();
    let ticket = ResumeTicket::new(
        "client-execution:wire-client",
        activation.run_id.clone(),
        activation.thread_id.clone(),
        activation.snapshot.id.0.clone(),
        activation.snapshot.fingerprint.0.clone(),
        AwaitTarget::ToolCall {
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
            reason: ToolAwaitReason::ClientExecution,
            call_id: "client-call".into(),
            tool: PendingTool {
                tool_id: "desktop_search".into(),
                arguments: serde_json::json!({"query":"customer"}),
            },
        },
    );
    super::commit::commit(
        &RuntimeRunContext::new().with_commit(committed.clone()),
        Vec::new(),
        None,
        &activation.thread_id,
        RunDisposition::awaiting(ticket.clone()),
        vec![Message::new(
            MessageId::assistant(&activation.run_id, 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "client-call",
                "desktop_search",
                serde_json::json!({"query":"customer"}),
            )],
        )],
        Vec::new(),
    )
    .await
    .expect("C1 commits one ExternalResult batch");
    let command = ResumeCommand::from_ticket(
        &ticket,
        ResumeResult::ToolResult(awaken_runtime_contract::tool::ToolOutput::ok(
            "client-call",
            "found",
        )),
        0,
    )
    .with_operation_id("client-result-operation");
    let probe = Arc::new(OwnedContinuationProbe {
        resumes: Default::default(),
        recoveries: AtomicUsize::new(0),
        fail: false,
    });
    let (executor, opens, _, _) = counting_executor();
    assert_eq!(
        executor
            .with_owned_tool_continuation(Some(probe.clone()))
            .resume(
                activation,
                command.clone(),
                RuntimeRunContext::new()
                    .with_commit(committed.clone())
                    .with_reader(committed),
            )
            .await
            .expect("C2 reaches the shared continuation"),
        RunState::Awaiting
    );
    assert_eq!(*probe.resumes.lock().unwrap(), vec![command]);
    assert_eq!(opens.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn acp_mcp_server_name_drift_uses_only_conservative_session_alias_consensus() {
    // Cause/effect graph: C1 the ACP server name matches a Session alias; C2 it
    // drifts to the MCP implementation's advertised name; C3 all configured
    // alias policies allow; C4 any alias denies; C5 no deny but one alias asks.
    // Effects: E1 exact-alias policy only; E2 allow; E3 deny; E4 await. FMECA:
    // blindly trusting the reported name bypasses authored policy (S10/O4/D8),
    // while choosing one arbitrary alias can widen authority (S10/O3/D7).
    // Consensus preserves the sole neutral policy and fails closed.
    //
    // | Rule | C1 | C2 | Alias verdicts | Effect |
    // |---|---|---|---|---|
    // | A1 | T | F | browser=allow, pilot=deny | E1 allow |
    // | A2 | F | T | allow, allow | E2 allow |
    // | A3 | F | T | allow, deny | E3 deny |
    // | A4 | F | T | allow, ask | E4 await |
    struct AliasPolicy {
        pilot: &'static str,
        browser: &'static str,
    }
    #[async_trait]
    impl ToolPermissionPolicy for AliasPolicy {
        async fn evaluate(&self, call: &ToolCall) -> ToolPermissionRuling {
            let behavior = if call.tool_id.starts_with("mcp__pilot__") {
                self.pilot
            } else if call.tool_id.starts_with("mcp__browser__") {
                self.browser
            } else {
                "ask"
            };
            ToolPermissionRuling::policy(
                match behavior {
                    "allow" => ToolPermissionVerdict::Allow,
                    "deny" => ToolPermissionVerdict::Deny {
                        reason: "denied by alias policy".into(),
                    },
                    _ => ToolPermissionVerdict::RequireConfirmation {
                        correlation_id: format!("confirm:{}", call.call_id),
                    },
                },
                awaken_runtime_contract::permission::PermissionBasis::fixed(
                    awaken_runtime_contract::permission::PermissionPolicy::Rule,
                ),
            )
        }
    }

    async fn verdict(pilot: &'static str, browser: &'static str, tool: &str) -> PermissionVerdict {
        let base = NeutralPermissionResolver {
            policy: Arc::new(AliasPolicy { pilot, browser }),
            grant_observer: None,
            audit: None,
        };
        AliasedMcpPermissionResolver {
            base: &base,
            mcp_server_names: &["pilot".into(), "browser".into()],
        }
        .resolve(&PermissionAsk {
            tool: tool.into(),
            call_id: "alias-call".into(),
            arguments: serde_json::json!({"url": "https://example.test"}),
        })
        .await
    }

    assert_eq!(
        verdict("deny", "allow", "mcp.browser.browser_navigate").await,
        PermissionVerdict::Allow,
        "A1"
    );
    assert_eq!(
        verdict("allow", "allow", "mcp.playwright.browser_navigate").await,
        PermissionVerdict::Allow,
        "A2"
    );
    assert_eq!(
        verdict("allow", "deny", "mcp.playwright.browser_navigate").await,
        PermissionVerdict::Deny,
        "A3"
    );
    assert_eq!(
        verdict("allow", "ask", "mcp.playwright.browser_navigate").await,
        PermissionVerdict::Await {
            correlation_id: "confirm:alias-call".into(),
        },
        "A4"
    );
}

#[test]
fn terminal_business_outcome_is_not_rewritten_by_cleanup_failure() {
    // C1 logical terminal; C2 cleanup failed => preserve NaturalEnd.
    // Physical return and owner-drop are covered by the execution matrix below.
    let outcome = preserve_terminal_outcome(
        Ok(TerminationReason::NaturalEnd),
        Err(AcpError::Io("cleanup failed".into())),
        "test-process",
    );
    assert_eq!(outcome.unwrap(), TerminationReason::NaturalEnd);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_term_escalation_requires_real_child_exit_before_ack() {
    // Causes: C1 TERM transport returns an error without delivery; C2 KILL is
    // delivered/refused; C3 terminal/cancellation entry. Effects: E1 the same
    // LocalProcess is killed and reaped, allowing return ACK; E2 it remains
    // alive at the cleanup deadline, retaining missing evidence and the logical end.
    // Scope ownership/drop and ACK veto are checked in the execution matrix below.
    // Decision table R1=C1+C2(delivered), each C3 => E1; R2=C1+C2(refused),
    // each C3 => E2. Signal results never substitute for an OS wait receipt.
    // The decorator injects only signal delivery faults; production LocalProcess
    // owns spawn, PID/group identity, poll, wait and final fixture cleanup.
    // This is real-child integration, not remote/container failure qualification.
    struct SignalFault {
        process: awaken_local_process::LocalProcess,
        deliver_kill: bool,
        signals: Mutex<Vec<Signal>>,
    }
    #[async_trait]
    impl ProcessHandle for SignalFault {
        fn id(&self) -> &str {
            self.process.id()
        }
        async fn poll(&self) -> std::result::Result<Option<ExitStatus>, SandboxError> {
            self.process.poll().await
        }
        async fn wait(&self) -> std::result::Result<ExitStatus, SandboxError> {
            self.process.wait().await
        }
        async fn signal(&self, signal: Signal) -> std::result::Result<(), SandboxError> {
            self.signals.lock().unwrap().push(signal);
            if signal == Signal::Kill && self.deliver_kill {
                self.process.signal(signal).await
            } else {
                Err(SandboxError::transport("injected signal transport failure"))
            }
        }
    }
    for terminal in [false, true] {
        for deliver_kill in [false, true] {
            let mut command = tokio::process::Command::new("sleep");
            command.arg("60");
            let process = SignalFault {
                process: awaken_local_process::LocalProcess::spawn(&mut command).unwrap(),
                deliver_kill,
                signals: Mutex::new(Vec::new()),
            };
            assert!(process.poll().await.unwrap().is_none());
            let grace = std::time::Duration::from_millis(200);
            let result = if terminal {
                let (mut channel, _peer) = tokio::io::duplex(64);
                Supervisor::reap_after_terminal(&mut channel, &process, grace).await
            } else {
                Supervisor::reap(&process, grace).await
            };
            let observed = process.poll().await.unwrap();
            // Clean up the exact fixture before assertions, including on a
            // regression where the supervisor never reaches KILL.
            process.process.signal(Signal::Kill).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), process.process.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.is_ok(), deliver_kill, "terminal={terminal}");
            assert_eq!(observed.is_some(), deliver_kill, "same-child receipt");
            if let Some(status) = observed {
                assert!(status.signaled, "R1 actual OS signal exit");
            }
            assert_eq!(
                *process.signals.lock().unwrap(),
                vec![Signal::Term, Signal::Kill]
            );
            let outcome =
                preserve_terminal_outcome(Ok(TerminationReason::NaturalEnd), result, process.id());
            assert_eq!(outcome.unwrap(), TerminationReason::NaturalEnd);
        }
    }
}

#[tokio::test]
async fn managed_acp_terminal_cannot_escape_without_commit_capability() {
    // Cause/effect graph: C1 ACP produces a terminal disposition; C2 commit
    // capability present/absent. Effects: E1 atomically commit messages+state;
    // E2 fail before the terminal outcome escapes. Decision table: A1 C1+C2 =>
    // E1 (covered by the executor success cases); A2 C1+!C2 => E2. FMECA: a
    // natural end without ThreadCommit creates false success and empty history
    // (severity 10, occurrence 4, detection 8), so Managed ACP fails closed.
    let error = commit(
        &RuntimeRunContext::new(),
        Vec::new(),
        None,
        &ThreadId("thread-no-commit".into()),
        RunDisposition::ended(RunId("run-no-commit".into()), EndCause::NaturalEnd),
        Vec::new(),
        Vec::new(),
    )
    .await
    .expect_err("A2 missing capability");
    assert!(
        error.to_string().contains("requires a CommitCoordinator"),
        "A2/E2: {error}"
    );
}

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
            Err(ToolError::DefinitiveFailure("ACP spill unavailable".into()))
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
    /// When set, `open` fails with this launch fault instead of scripting a Run.
    open_error: Option<String>,
}

#[async_trait]
impl AgentChannelSource for ScriptedSource {
    async fn open(
        &self,
        _activation: &RunActivation,
        _context: &RuntimeRunContext,
        _door: ChannelOpenDoor,
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
        Ok(fixture_session(
            Box::new(ours),
            Arc::new(FakeProcess),
            awaken_protocol_acp::Codec::Newline,
            None,
        ))
    }
}

#[derive(Clone)]
enum OwnershipDecision {
    Current,
    Lost,
    Unavailable,
}

struct ScriptedOwnership {
    decisions: Mutex<std::collections::VecDeque<OwnershipDecision>>,
}

impl ScriptedOwnership {
    fn new(decisions: impl IntoIterator<Item = OwnershipDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
        }
    }
}

#[async_trait]
impl AttemptOwnershipVerifier for ScriptedOwnership {
    async fn verify_current(&self) -> std::result::Result<(), AttemptOwnershipError> {
        match self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(OwnershipDecision::Current)
        {
            OwnershipDecision::Current => Ok(()),
            OwnershipDecision::Lost => Err(AttemptOwnershipError::Lost),
            OwnershipDecision::Unavailable => {
                Err(AttemptOwnershipError::Unavailable("authority down".into()))
            }
        }
    }
}

struct CountingProcess {
    polls: Arc<AtomicUsize>,
    observation: Arc<AtomicUsize>,
}

#[async_trait]
impl ProcessHandle for CountingProcess {
    fn id(&self) -> &str {
        "counting-process"
    }

    async fn wait(&self) -> std::result::Result<ExitStatus, SandboxError> {
        loop {
            if let Some(status) = self.poll().await? {
                return Ok(status);
            }
            tokio::task::yield_now().await;
        }
    }

    async fn poll(&self) -> std::result::Result<Option<ExitStatus>, SandboxError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        match self.observation.load(Ordering::SeqCst) {
            0 => Ok(Some(ExitStatus {
                code: Some(0),
                signaled: false,
            })),
            1 => Ok(None),
            2 => Err(SandboxError::transport("transport unavailable")),
            _ => std::future::pending().await,
        }
    }

    async fn signal(&self, _signal: Signal) -> std::result::Result<(), SandboxError> {
        Ok(())
    }
}

struct CountingSource {
    opens: Arc<AtomicUsize>,
    prompts: Arc<AtomicUsize>,
    process_polls: Arc<AtomicUsize>,
    observation: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentChannelSource for CountingSource {
    async fn open(
        &self,
        _activation: &RunActivation,
        _context: &RuntimeRunContext,
        _door: ChannelOpenDoor,
    ) -> std::result::Result<AgentSession, OpenError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let prompts = self.prompts.clone();
        tokio::spawn(async move {
            let mut prompt = String::new();
            let mut reader = BufReader::new(&mut theirs);
            if reader.read_line(&mut prompt).await.is_ok() && !prompt.is_empty() {
                prompts.fetch_add(1, Ordering::SeqCst);
            }
            let _ = theirs
                .write_all(b"{\"type\":\"message\",\"text\":\"done\"}\n")
                .await;
            let _ = theirs
                .write_all(b"{\"type\":\"turn_end\",\"reason\":\"natural_end\"}\n")
                .await;
            let _ = theirs.flush().await;
        });
        Ok(fixture_session(
            Box::new(ours),
            Arc::new(CountingProcess {
                polls: self.process_polls.clone(),
                observation: self.observation.clone(),
            }),
            Codec::Newline,
            None,
        ))
    }
}

fn counting_executor() -> (
    AcpRunExecutor,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let opens = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AtomicUsize::new(0));
    let process_polls = Arc::new(AtomicUsize::new(0));
    (
        AcpRunExecutor::new(Arc::new(CountingSource {
            opens: opens.clone(),
            prompts: prompts.clone(),
            process_polls: process_polls.clone(),
            observation: Arc::new(AtomicUsize::new(0)),
        })),
        opens,
        prompts,
        process_polls,
    )
}

#[tokio::test]
async fn acp_external_steps_recheck_live_attempt_ownership() {
    // Causes: the fixtures below establish `acp external steps recheck live attempt ownership` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1=authority absent/current/lost/unavailable;
    // C2=loss occurs before process open, after open but before prompt, or
    // between two ACP Steps. Effects: E1=open/send exactly once while current;
    // E2=return attempt error with zero later external calls; E3=reap a process
    // opened before authority was lost. Absence is the direct/embedded
    // compatibility topology, never a bypass for a bound authority.
    //
    // | Rule | Authority sequence          | Opens | Prompts | Effect |
    // | O1   | absent/current              | 1     | 1       | E1     |
    // | O2   | lost/unavailable before open| 0     | 0       | E2     |
    // | O3   | current -> lost             | 1     | 0       | E2+E3  |
    // | O4   | current x3 -> lost          | 1     | 1       | E1+E2  |
    for ownership in [
        None,
        Some(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Current,
        ])) as Arc<dyn AttemptOwnershipVerifier>),
    ] {
        let (executor, opens, prompts, _) = counting_executor();
        let mut context =
            RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default()));
        if let Some(ownership) = ownership {
            context = context.with_ownership(ownership);
        }
        assert_eq!(
            executor
                .execute(activation_without_session_home(), context)
                .await
                .expect("O1"),
            RunState::Ended(EndCause::NaturalEnd)
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1, "O1/E1");
        assert_eq!(prompts.load(Ordering::SeqCst), 1, "O1/E1");
    }

    for decision in [OwnershipDecision::Lost, OwnershipDecision::Unavailable] {
        let (executor, opens, prompts, _) = counting_executor();
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(RecordingCoordinator::default()))
            .with_ownership(Arc::new(ScriptedOwnership::new([decision])));
        assert!(
            executor
                .execute(activation_without_session_home(), context)
                .await
                .is_err(),
            "O2/E2"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 0, "O2/E2");
        assert_eq!(prompts.load(Ordering::SeqCst), 0, "O2/E2");
    }

    let (executor, opens, prompts, process_polls) = counting_executor();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(RecordingCoordinator::default()))
        .with_ownership(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Lost,
        ])));
    assert!(
        executor
            .execute(activation_without_session_home(), context)
            .await
            .is_err(),
        "O3/E2"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 1, "O3");
    assert_eq!(prompts.load(Ordering::SeqCst), 0, "O3/E2");
    assert!(process_polls.load(Ordering::SeqCst) > 0, "O3/E3");

    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};
    let inbox = LiveInbox::new();
    let _ = inbox.offer_as(
        MessageOrigin::External,
        Message::text(MessageId("steer".into()), Role::User, "continue"),
    );
    let (executor, opens, prompts, _) = counting_executor();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(RecordingCoordinator::default()))
        .with_live_inbox(inbox)
        .with_ownership(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Lost,
        ])));
    assert!(
        executor
            .execute(activation_without_session_home(), context)
            .await
            .is_err(),
        "O4/E2"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 1, "O4/E1");
    assert_eq!(prompts.load(Ordering::SeqCst), 1, "O4/E1");
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

impl CommittedThreadView for RecordingCoordinator {
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

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        let commits = self.commits.lock().ok()?;
        let latest = commits
            .iter()
            .rev()
            .find(|commit| &commit.thread_id == thread_id)?;
        let ticket = latest.resume_ticket()?.clone();
        (ticket.thread_id == *thread_id).then(|| (latest.run_id().clone(), ticket))
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.commits
            .lock()
            .ok()?
            .iter()
            .rev()
            .find(|commit| commit.run_id() == run_id)
            .map(|commit| RunRecord {
                id: run_id.clone(),
                thread_id: commit.thread_id.clone(),
                state: commit.run_state(),
            })
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.commits
            .lock()
            .ok()?
            .iter()
            .rev()
            .find(|commit| &commit.thread_id == thread_id)
            .map(|commit| RunRecord {
                id: commit.run_id().clone(),
                thread_id: thread_id.clone(),
                state: commit.run_state(),
            })
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
    RunActivation::new(
        RunId("run-1".into()),
        ThreadId("thread-1".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: Some(8),
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
        vec![Message::text(MessageId("u1".into()), Role::User, "do it")],
    )
}

fn activation_without_session_home() -> RunActivation {
    let mut activation = activation();
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
            "prov",
            "model",
            "acp:ownership-probe",
        ));
    activation
}

#[test]
fn acp_prompt_orders_frozen_policy_context_and_authoritative_input() {
    // Cause/effect graph: frozen instructions (C1), transient backend context
    // (C2), and durable current input (C3) produce ordered prompt sections (E1);
    // C2 does not mutate durable input (E2); without C1/C2, C3 passes through
    // byte-identically (E3). FMECA FM1: labelling C3 as untrusted made real ACP
    // agents refuse the current task in favor of quoted Plan context; explicitly
    // marking C3 authoritative under C1 mitigates that precedence inversion.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | PR1  | T  | T  | T  | E1+E2 |
    // | PR2  | F  | F  | T  | E3    |
    let mut activation = activation();
    activation.snapshot.resolved_spec.instructions = "follow policy".into();
    let original = activation.input.clone();
    let context = vec![
        Message::text(
            MessageId("session-baseline:historical".into()),
            Role::System,
            "obsolete resource revision",
        ),
        Message::text(
            MessageId("request-memory".into()),
            Role::System,
            "remember the user's preference",
        ),
    ];

    let prompt = super::initial_prompt(&activation, &context, &crate::AcpInstructions::default());

    let instructions = prompt.find("follow policy").expect("C1");
    let recalled = prompt.find("remember the user's preference").expect("C2");
    let input = prompt.find("do it").expect("C3");
    assert!(instructions < recalled && recalled < input, "R1: {prompt}");
    assert!(prompt.contains("authoritative task"), "R1: {prompt}");
    assert!(
        !prompt.contains("obsolete resource revision"),
        "R1: legacy derived context must not reach ACP"
    );
    assert_eq!(activation.input, original, "E2");

    activation.snapshot.resolved_spec.instructions.clear();
    assert_eq!(
        super::initial_prompt(&activation, &[], &crate::AcpInstructions::default()),
        "do it",
        "E3"
    );
}

fn exec(frames: Vec<String>) -> AcpRunExecutor {
    AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames,
        open_error: None,
    }))
}

fn resume_command_for(ticket: &ResumeTicket) -> awaken_runtime_contract::resume::ResumeCommand {
    awaken_runtime_contract::resume::ResumeCommand::from_ticket(
        ticket,
        awaken_runtime_contract::resume::ResumeResult::allow(),
        0,
    )
}

fn permission_ticket(activation: &RunActivation) -> ResumeTicket {
    ResumeTicket::new(
        "permission-correlation",
        activation.run_id.clone(),
        activation.thread_id.clone(),
        &activation.snapshot.id.0,
        &activation.snapshot.fingerprint.0,
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "call-7".into(),
            tool: PendingTool {
                tool_id: "bash".into(),
                arguments: serde_json::json!({}),
            },
            authority:
                awaken_agent_contract::agent::awaiting::ToolCallAuthority::RunAttemptExecutor,
        },
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
          printf '{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"run:%s\"}}}}\\n' \"$K\"; \
          printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}'; \
          exit 0;; \
      esac; \
    done";

#[test]
fn advertises_remote_abort_and_auth_wait() {
    // The supervisor interrupts the opaque CLI Run on cancel, and the ACP
    // permission flow awaits on an authorization decision (ADR-0055).
    let caps = exec(vec![]).capabilities();
    assert_eq!(caps.cancellation, Cancellation::RemoteAbort);
    assert_eq!(caps.wait, Wait::Auth);
}

#[tokio::test]
async fn unreissued_permission_is_settled_with_the_continuation_commit() {
    // Causes: C1 committed original permission ticket; C2 allow/deny; C3 the
    // resumed agent never requests the exact tool; C4 natural/refusal/EOF/open
    // failure/provider failure/empty end. C1+C3 => no one-shot execution grant.
    // For every C2 x C4 row: E1 exactly one error result addresses the original
    // call and states it was not executed; E2 the same terminal ThreadCommit
    // removes its ticket/batch; E3 immutable original arguments are retained.
    // The source/Coordinator are existing fixtures; no alternate resume path.
    for allow in [true, false] {
        for mode in ["natural", "refusal", "eof", "open", "provider", "empty"] {
            let active = activation();
            let ticket = permission_ticket(&active);
            let call_id = ticket.call_id().unwrap().to_string();
            let original = Message {
                id: MessageId::assistant(&active.run_id, 0),
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    &call_id,
                    "bash",
                    serde_json::json!({}),
                )],
            };
            let recorded = Arc::new(RecordingCoordinator::default());
            let context = RuntimeRunContext::new()
                .with_reader(recorded.clone())
                .with_commit(recorded.clone());
            commit(
                &context,
                Vec::new(),
                None,
                &active.thread_id,
                RunDisposition::awaiting(ticket.clone()),
                vec![original.clone()],
                Vec::new(),
            )
            .await
            .unwrap();
            let frames = match mode {
                "natural" => vec![r#"{"type":"message","text":"continued without the old request"}"#.into(),
                    r#"{"type":"turn_end","reason":"natural_end"}"#.into()],
                "refusal" => vec![r#"{"type":"turn_end","reason":"refusal"}"#.into()],
                "provider" => vec![r#"{"type":"message","text":"{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream unavailable\"}}"}"#.into(),
                    r#"{"type":"turn_end","reason":"natural_end"}"#.into()],
                "empty" => vec![r#"{"type":"turn_end","reason":"natural_end"}"#.into()],
                _ => vec![],
            };
            let executor = AcpRunExecutor::new(Arc::new(ScriptedSource {
                frames,
                open_error: (mode == "open").then(|| "launch failed".into()),
            }));
            let result = if allow {
                ResumeResult::allow()
            } else {
                ResumeResult::deny(None)
            };
            let state = executor
                .resume(
                    active.clone(),
                    ResumeCommand::from_ticket(&ticket, result, 1),
                    context,
                )
                .await
                .unwrap();
            match mode {
                "natural" => assert_eq!(state, RunState::Ended(EndCause::NaturalEnd)),
                "refusal" => assert_eq!(
                    state,
                    RunState::Ended(EndCause::Stopped("agent refused".into()))
                ),
                _ => assert!(
                    matches!(state, RunState::Ended(EndCause::Error(_))),
                    "{mode}/{allow}: settlement cannot mask the execution error"
                ),
            }
            let commits = recorded.commits.lock().unwrap();
            assert_eq!(
                commits.len(),
                2,
                "E2 settlement is part of the terminal commit"
            );
            assert_eq!(
                commits[0].messages,
                [original],
                "E3 original remains immutable"
            );
            let answers = commits[1]
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } if tool_use_id == &call_id => Some((content, *is_error)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                answers.len(),
                1,
                "E1 {mode}/{allow}: abandoned approval must settle"
            );
            assert!(answers[0].1);
            assert!(answers[0].0.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("not executed"))));
            drop(commits);
            assert!(recorded.resume_ticket_for(&active.run_id).is_none(), "E2");
            assert!(
                awaken_runtime_contract::ActiveToolBatch::load(&Store::rebuild(
                    &recorded.committed_state(&active.thread_id)
                ))
                .unwrap()
                .is_none(),
                "E2"
            );
        }
    }
}

#[tokio::test]
async fn permission_settlement_preserves_an_observed_result_and_an_unknown_outcome() {
    // S1 a result already exists -> retain its exact status/body once, regardless
    // of missing permission notification. S2 an exact grant was consumed but
    // result is absent -> do not fabricate a not-executed/success result. The
    // caller's error disposition remains authoritative for that unknown outcome.
    for observed in [true, false] {
        let decision = PermissionResume {
            call_id: "original".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({}),
            allow: true,
        };
        let resolver = ResumedPermissionResolver::new(&AllowAll, &decision, None);
        let mut appender =
            CollectingAppender::new(RunId("run-1".into()), "test".into(), 0, None, None, None);
        if observed {
            appender
                .append(
                    1,
                    &AcpProjectedEvent::ToolResult {
                        id: "original".into(),
                        content: "observed bytes".into(),
                        is_error: false,
                    },
                )
                .await
                .unwrap();
        } else {
            assert_eq!(
                resolver
                    .resolve(&PermissionAsk {
                        call_id: "regenerated".into(),
                        tool: "bash".into(),
                        arguments: serde_json::json!({})
                    })
                    .await,
                PermissionVerdict::Allow
            );
        }
        let recorded = Arc::new(RecordingCoordinator::default());
        let disposition = RunDisposition::ended(
            RunId("run-1".into()),
            EndCause::Stopped("result unavailable".into()),
        );
        commit(
            &RuntimeRunContext::new().with_commit(recorded.clone()),
            Vec::new(),
            resolver.unconsumed_decision(),
            &ThreadId("thread-1".into()),
            disposition,
            appender.messages.clone(),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(
            recorded.messages(),
            appender.messages,
            "no fabricated or duplicate results"
        );
    }
}

#[tokio::test]
async fn resume_requires_history_then_an_active_ticket() {
    let activation = activation();
    let ticket = permission_ticket(&activation);
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

#[test]
fn restored_session_is_typed_fail_closed_and_backend_scoped() {
    // Causes: the fixtures below establish `restored session` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is typed fail closed and backend scoped` and every asserted
    // state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Decision table for the durable state boundary:
    // C1 absent/removed -> no resume; C2 exact typed identity -> resume;
    // C3 other backend -> no resume; C4 empty or schema-drifted identity -> error.
    // This partitions presence, ownership, and shape so no corrupt fact is
    // silently interpreted as a fresh ACP session.
    let activation = activation();
    let coordinator = Arc::new(RecordingCoordinator::default());
    let context = RuntimeRunContext::new().with_reader(coordinator.clone());
    assert_eq!(
        restored_session_id(
            &context,
            &activation.thread_id,
            activation.snapshot.resolved_spec.model_binding.identity().2,
        )
        .expect("C1 absent state is valid"),
        None
    );

    let commands = [
        acp_session_cell()
            .write(&AcpSessionReference {
                backend_ref: "acp:other".into(),
                session_id: "session-7".into(),
            })
            .expect("C3 fixture serializes"),
        acp_session_cell().remove(),
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
                activation.snapshot.resolved_spec.model_binding.identity().2,
            )
            .expect("C1/C3 are valid non-resume states"),
            None
        );
        coordinator.commits.lock().unwrap().clear();
    }

    let exact = run_state(
        &context,
        &activation.thread_id,
        &TokenUsage::default(),
        "model",
        activation.snapshot.resolved_spec.model_binding.identity().2,
        Some("session-7"),
    )
    .expect("C2 fixture serializes");
    coordinator.commits.lock().unwrap().push(ThreadCommit {
        thread_id: activation.thread_id.clone(),
        run: awaken_agent_contract::thread::commit::RunDisposition::running(
            activation.run_id.clone(),
        ),
        messages: Vec::new(),
        state: exact,
        events: Vec::new(),
    });
    assert_eq!(
        restored_session_id(
            &context,
            &activation.thread_id,
            activation.snapshot.resolved_spec.model_binding.identity().2,
        )
        .expect("C2 exact identity is valid"),
        Some("session-7".into())
    );

    for invalid in [
        serde_json::json!({"backend_ref": "acp:claude", "session_id": ""}),
        serde_json::json!({
            "backend_ref": "acp:claude",
            "session_id": "session-7",
            "unexpected": true
        }),
    ] {
        coordinator.commits.lock().unwrap().clear();
        coordinator.commits.lock().unwrap().push(ThreadCommit {
            thread_id: activation.thread_id.clone(),
            run: awaken_agent_contract::thread::commit::RunDisposition::running(
                activation.run_id.clone(),
            ),
            messages: Vec::new(),
            state: vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                ACP_SESSION_ID_STATE_KEY,
                invalid,
            )],
            events: Vec::new(),
        });
        assert!(
            restored_session_id(
                &context,
                &activation.thread_id,
                activation.snapshot.resolved_spec.model_binding.identity().2,
            )
            .is_err(),
            "C4 malformed durable identity must fail closed"
        );
    }
}

#[test]
fn pending_tool_use_is_deduplicated_or_minted_in_the_canonical_step_family() {
    // Causes: the fixtures below establish `pending tool use` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_protocol_acp::PermissionAsk;

    // Cause/effect graph: C1 the ACP permission request's ToolUse is already in
    // the attempt transcript; C2 it is absent. Effects: E1 C1 appends nothing
    // and does not consume a Step; E2 C2 appends one canonical assistant Step.
    //
    // | Rule | matching ToolUse | Effect |
    // | P1   | yes              | E1     |
    // | P2   | no               | E2     |
    let run = RunId("run-1".into());
    let ask = PermissionAsk {
        tool: "bash".into(),
        call_id: "call-7".into(),
        arguments: serde_json::json!({"cmd": "pwd"}),
    };
    let mut messages = vec![Message {
        id: MessageId::assistant(&run, 0),
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(
            "call-7",
            "bash",
            serde_json::json!({"cmd": "pwd"}),
        )],
    }];
    let mut next_step = 1;
    ensure_pending_tool_use(&run, &mut next_step, &mut messages, 0, &ask).unwrap();
    assert_eq!(messages.len(), 1, "P1/E1");
    assert_eq!(next_step, 1, "P1/E1");

    let absent = PermissionAsk {
        call_id: "call-8".into(),
        ..ask
    };
    ensure_pending_tool_use(&run, &mut next_step, &mut messages, 0, &absent).unwrap();
    assert_eq!(messages.len(), 2, "P2/E2");
    assert_eq!(messages[1].id, MessageId::assistant(&run, 1), "P2/E2");
    assert_eq!(next_step, 2, "P2/E2");
}

/// Pending fact graph: placeholder/full input x staged/committed prefix x
/// matching/drifted permission request -> exact ToolUse + ticket, or rejection.
/// P3 staged placeholder/refinement may change without allocating another Step;
/// P4 committed exact replay stays byte-for-byte unchanged; P5 committed name or
/// argument drift fails before mutation. Absent-call allocation is P2 above.
#[test]
fn pending_permission_refines_only_the_uncommitted_tool_use() {
    let run = RunId("pending-refinement".into());
    let ask = PermissionAsk {
        call_id: "call".into(),
        tool: "mcp__awaken_session__bash".into(),
        arguments: serde_json::json!({"command":"echo reviewed"}),
    };
    for input in [
        serde_json::json!({}),
        serde_json::json!({"command":"partial"}),
        ask.arguments.clone(),
    ] {
        for committed in [false, true] {
            let mut messages = vec![Message {
                id: MessageId::assistant(&run, 0),
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("call", "bash", input.clone())],
            }];
            let before = messages.clone();
            let mut step = 1;
            let result = ensure_pending_tool_use(
                &run,
                &mut step,
                &mut messages,
                usize::from(committed),
                &ask,
            );
            if committed && input != ask.arguments {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("committed tool call"),
                    "P5"
                );
                assert_eq!(messages, before, "P5 no historical rewrite");
            } else {
                result.unwrap();
                assert_eq!(messages.len(), 1, "P3/P4 single occurrence");
                assert!(
                    matches!(&messages[0].content[0], ContentBlock::ToolUse {name,input,..} if name == "bash" && input == &ask.arguments)
                );
                if committed {
                    assert_eq!(messages, before, "P4");
                }
            }
            assert_eq!(step, 1, "no duplicate Step");
        }
    }
    let mut messages = vec![Message {
        id: MessageId::assistant(&run, 0),
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(
            "call",
            "write",
            ask.arguments.clone(),
        )],
    }];
    let before = messages.clone();
    assert!(
        ensure_pending_tool_use(&run, &mut 1, &mut messages, 1, &ask).is_err(),
        "P5 name drift"
    );
    assert_eq!(messages, before);
}

#[test]
fn acp_fact_ids_follow_the_role_specific_identity_authority() {
    // Causes: the fixtures below establish `acp fact ids follow the role specific identity
    // authority` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph:
    // C1 an assistant cursor starts/replays at the same committed prefix; C2 it
    // advances; C3 the Run differs; C4 a Tool-result fact has the same/different
    // activation namespace or suffix. Effects: E1 C1 mints the same canonical
    // Run/Step id; E2 C2/C3 stay unique; E3 C4 remains stable/unique in ACP's
    // private non-assistant family and never classifies as an assistant Step.
    // FMECA FM1: wire sequence restarted on permission resume and collided; the
    // committed assistant cursor now owns that boundary, while Tool results keep
    // their activation namespace because they have no assistant Step semantics.
    //
    // | Rule | role      | Run/cursor or namespace | Effect |
    // | ID1  | Assistant | same prefix replay      | E1     |
    // | ID2  | Assistant | next Step / other Run   | E2     |
    // | ID3  | Tool      | same/different fact key | E3     |
    let run_one = RunId("run-1".into());
    let run_two = RunId("run-2".into());
    let mut first = 0;
    let first_zero = take_assistant_message_id(&run_one, &mut first);
    let first_one = take_assistant_message_id(&run_one, &mut first);
    let mut replay = 0;

    assert_eq!(
        first_zero,
        take_assistant_message_id(&run_one, &mut replay),
        "ID1/E1"
    );
    assert_eq!(first_zero, MessageId::assistant(&run_one, 0), "ID1/E1");
    assert_eq!(first_one, MessageId::assistant(&run_one, 1), "ID2/E2");
    let mut other = 0;
    assert_ne!(
        first_zero,
        take_assistant_message_id(&run_two, &mut other),
        "ID2/E2"
    );

    let tool = acp_tool_message_id(&run_one, "initial:0", 1);
    assert_eq!(
        tool,
        acp_tool_message_id(&run_one, "initial:0", 1),
        "ID3/E3"
    );
    assert_ne!(tool, acp_tool_message_id(&run_one, "resume:0", 1), "ID3/E3");
    assert_ne!(
        tool,
        acp_tool_message_id(&run_one, "initial:0", 2),
        "ID3/E3"
    );
    assert_eq!(tool.assistant_step_of(&run_one), None, "ID3/E3");
}

#[tokio::test]
async fn acp_result_ids_survive_repeated_empty_input_activations() {
    // Cause/effect graph: committed prefix + activation input + restarted wire
    // sequence -> private Tool Message identity -> Managed deduplication.
    // R1 same prefix/input/frame replay is stable; R2 after a committed result,
    // another empty-input activation must mint a distinct result even when the
    // wire sequence and provider call id restart. R3 input-bearing activations
    // obey the same rule. R4 assistant ids remain the canonical Run/Step family.
    // R5 tool-only committed progress also distinguishes the next activation;
    // counting only assistant messages would miss this reachable prefix shape.
    // The old test varied a caller-invented namespace; it never exercised the
    // executor that actually generated "empty-input:0" for every continuation.
    // These are executor-boundary invocations, not proof of dispatch admission.
    for (has_input, has_call) in [(false, false), (false, true), (true, false), (true, true)] {
        let recorded = Arc::new(RecordingCoordinator::default());
        let mut active = activation();
        if !has_input {
            active.input.clear();
        }
        let mut frames = vec![
            r#"{"type":"tool_call","id":"same-wire-id","name":"read","input":{"path":"report.md"}}"#.into(),
            r#"{"type":"tool_result","id":"same-wire-id","content":"read contents","is_error":false}"#.into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ];
        if !has_call {
            frames.remove(0);
        }
        let mut result_ids = std::collections::HashSet::new();
        for _ in 0..3 {
            // Two newly constructed executors see the same immutable prefix;
            // replay commits into a separate recorder and cannot advance it.
            let replay = Arc::new(RecordingCoordinator::default());
            exec(frames.clone())
                .execute(
                    active.clone(),
                    RuntimeRunContext::new()
                        .with_reader(recorded.clone())
                        .with_commit(replay.clone()),
                )
                .await
                .unwrap();
            let result = exec(frames.clone())
                .execute(
                    active.clone(),
                    RuntimeRunContext::new()
                        .with_reader(recorded.clone())
                        .with_commit(recorded.clone()),
                )
                .await
                .unwrap();
            assert_eq!(result, RunState::Ended(EndCause::NaturalEnd));
            let messages = recorded
                .commits
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .clone();
            assert_eq!(messages, replay.messages(), "R1 same-prefix replay");
            let tool = messages.iter().find(|m| m.role == Role::Tool).unwrap();
            assert!(
                result_ids.insert(tool.id.clone()),
                "R2/R3 duplicate result id: {}",
                tool.id.0
            );
            assert_eq!(tool.id.assistant_step_of(&active.run_id), None, "R4");
        }
        assert_eq!(result_ids.len(), 3);
    }
}

#[tokio::test]
async fn regenerated_approved_tool_projects_as_one_logical_call() {
    // C1 the durable transcript owns original O; C2 replacement streams no
    // placeholder/null/empty/partial input before exact T/A; C3 result arrives;
    // C4 later call repeats T/A. R1 every C2 partition + exact T/A -> discard only
    // staged placeholder and remap result to O once. R2 changed arguments after
    // matching -> fail closed. R3 C4 -> normal independent ToolUse. No committed
    // prefix is editable; this appender owns only the current uncommitted turn.
    for placeholder in [
        None,
        Some(serde_json::Value::Null),
        Some(serde_json::json!({})),
        Some(serde_json::json!({"cmd":"partial"})),
    ] {
        let decision = PermissionResume {
            call_id: "original".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"cmd":"echo approved"}),
            allow: true,
        };
        let mut appender = CollectingAppender::new(
            RunId("run-1".into()),
            "resume:0".into(),
            0,
            None,
            None,
            Some(&decision),
        );
        if let Some(input) = placeholder {
            appender
                .append(
                    1,
                    &AcpProjectedEvent::ToolCall {
                        id: "regenerated".into(),
                        name: "bash".into(),
                        input,
                    },
                )
                .await
                .unwrap();
        }
        let exact = AcpProjectedEvent::ToolCall {
            id: "regenerated".into(),
            name: "mcp__awaken_session__bash".into(),
            input: decision.arguments.clone(),
        };
        appender.append(2, &exact).await.unwrap();
        appender.append(3, &exact).await.unwrap();
        assert!(
            appender.messages.is_empty(),
            "R1 only the committed original owns this call"
        );
        assert!(
            appender
                .append(
                    4,
                    &AcpProjectedEvent::ToolCall {
                        id: "regenerated".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"cmd":"different"})
                    }
                )
                .await
                .is_err(),
            "R2 no changed approved request"
        );
        appender
            .append(
                5,
                &AcpProjectedEvent::ToolResult {
                    id: "regenerated".into(),
                    content: "failed original operation".into(),
                    is_error: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(appender.messages.len(), 1, "R1 one result");
        assert!(
            matches!(&appender.messages[0].content[0], ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "original"),
            "R1 resumed failure retains the original id and typed error flag"
        );
        appender
            .append(
                6,
                &AcpProjectedEvent::ToolCall {
                    id: "later".into(),
                    name: "bash".into(),
                    input: decision.arguments,
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(&appender.messages[1].content[0], ContentBlock::ToolUse { id, .. } if id == "later"),
            "R3 one-shot remap consumed"
        );
    }
}

#[tokio::test]
async fn streaming_call_refinements_preserve_one_staged_use_and_terminal_arguments() {
    // R1 ordinary calls stream empty -> partial -> exact input, interleaved with
    // another call: refine only the matching use and retain order/message ids.
    // R2 a result closes that use -> later argument mutation fails, never changes
    // the completed request. This includes AlwaysAllow tools with no Await edge.
    let mut appender =
        CollectingAppender::new(RunId("run".into()), "0".into(), 0, None, None, None);
    for (seq, id, input) in [
        (1, "a", serde_json::json!({})),
        (2, "b", serde_json::json!({"path":"b"})),
        (3, "a", serde_json::json!({"path":"partial"})),
        (4, "a", serde_json::json!({"path":"a"})),
    ] {
        appender
            .append(
                seq,
                &AcpProjectedEvent::ToolCall {
                    id: id.into(),
                    name: "read".into(),
                    input,
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(appender.messages.len(), 2, "R1 no duplicate use");
    for (index, expected) in [(0, "a"), (1, "b")] {
        assert!(
            matches!(&appender.messages[index].content[0], ContentBlock::ToolUse { id, input, .. } if id == expected && input == &serde_json::json!({"path":expected})),
            "R1 exact correspondence"
        );
    }
    appender
        .append(
            5,
            &AcpProjectedEvent::ToolResult {
                id: "a".into(),
                content: "ok".into(),
                is_error: false,
            },
        )
        .await
        .unwrap();
    assert!(
        appender
            .append(
                6,
                &AcpProjectedEvent::ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"changed"})
                }
            )
            .await
            .is_err(),
        "R2 no post-result mutation"
    );
}

#[tokio::test]
async fn opaque_acp_tool_loop_is_bounded_by_the_run_step_budget() {
    // Cause/effect decision table: B1 refinements of one ACP call consume one
    // slot; B2 distinct calls through max_steps are accepted; B3 the next
    // distinct call fails before allocation. Effects: no false charge for
    // streaming updates and a deterministic bound on messages retained while an
    // opaque SDK controls its own provider loop. max_steps remains the one budget
    // authority; this adapter only projects it into the hidden loop.
    let mut appender =
        CollectingAppender::new(RunId("bounded".into()), "0".into(), 0, None, None, None)
            .with_tool_call_limit(2);
    for (seq, id, input) in [
        (1, "one", serde_json::json!({})),
        (2, "one", serde_json::json!({"refined":true})),
        (3, "two", serde_json::json!({})),
    ] {
        appender
            .append(
                seq,
                &AcpProjectedEvent::ToolCall {
                    id: id.into(),
                    name: "desktop_search".into(),
                    input,
                },
            )
            .await
            .expect("B1/B2 stay within the budget");
    }
    let error = appender
        .append(
            4,
            &AcpProjectedEvent::ToolCall {
                id: "three".into(),
                name: "desktop_search".into(),
                input: serde_json::json!({}),
            },
        )
        .await
        .expect_err("B3 rejects the first excess call");
    assert!(error.to_string().contains("max_steps budget of 2"));
    assert_eq!(appender.messages.len(), 2, "B3 allocates no third ToolUse");
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
    // and protocol readiness, so no network/package phase can occur per Run.
    let observer = Arc::new(RecordingObserver::default());
    let e = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: vec![
            r#"{"type":"message","text":"hi"}"#.into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ],
        open_error: None,
    }))
    .with_launch_observer(observer.clone());

    e.execute(
        activation(),
        RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default())),
    )
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

/// Cause/effect design: C1 ACP emits `working`, `done`, then a natural terminal;
/// C2 the activation supplies user input `do it`. Effects: E1 the Run ends
/// NaturalEnd; E2 one commit contains the user message plus one assistant message
/// with concatenated text; E3 that commit carries the terminal state. Decision
/// rule D1=C1+C2=>E1+E2+E3.
#[tokio::test]
async fn drives_a_step_commits_messages_and_returns_natural_end() {
    // Causes: the fixtures below establish `drives a step commits messages and` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
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
    // Causes: the fixtures below establish `an empty natural end` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is a provider failure` and every asserted state transition or
    // side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
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
        "an empty opaque-provider Run must not remain an empty public response"
    );
}

#[tokio::test]
async fn a_structured_provider_error_message_cannot_end_as_success() {
    // Cause/effect decision table:
    // | terminal assistant content                     | end_turn result |
    // | provider error envelope after a warning banner | provider error  |
    // | ordinary JSON containing an error-like field   | natural end     |
    let provider_error = exec(vec![
        serde_json::json!({
            "type": "message",
            "text": "Warning: fallback metadata\n\n{\"error\":{\"message\":\"selected model is unavailable\",\"type\":\"invalid_request_error\",\"code\":\"invalid_request_error\"}}"
        })
        .to_string(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let provider_coordinator = Arc::new(RecordingCoordinator::default());
    let state = provider_error
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(provider_coordinator.clone()),
        )
        .await
        .unwrap();
    let RunState::Ended(EndCause::Error(failure)) = state else {
        panic!("provider envelope must end as an execution failure");
    };
    assert!(failure.message().contains("selected model is unavailable"));

    let ordinary_output = exec(vec![
        serde_json::json!({
            "type": "message",
            "text": "Result: {\"error\":{\"message\":\"documented example\",\"type\":\"example\"}}"
        })
        .to_string(),
        r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
    ]);
    let ordinary_state = ordinary_output
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default())),
        )
        .await
        .unwrap();
    assert_eq!(ordinary_state, RunState::Ended(EndCause::NaturalEnd));
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
    // Causes: the fixtures below establish `acp message id change` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 adjacent chunks without ids remain one legacy
    // stream; C2 a new ACP message id starts another message; C3 repeated id
    // continues it. Effects: E1 diagnostics and deliverables stay distinct; E2
    // token chunks for one deliverable remain coalesced; E3 each new logical
    // assistant Message receives the next canonical Run/Step id.
    //
    // | Rule | previous ACP message id | current id | Effect |
    // | T1   | absent                  | absent     | E2+E3(step 0) |
    // | T2   | absent                  | new        | E1+E3(step 1) |
    // | T3   | same                    | same       | E2, no new Step |
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
    let run = RunId("run-1".into());
    assert_eq!(messages[1].id, MessageId::assistant(&run, 0), "T1/E3");
    assert_eq!(messages[2].id, MessageId::assistant(&run, 1), "T2-T3/E3");
}

#[tokio::test]
async fn live_inbox_steer_folds_into_a_relaunched_run() {
    // Causes: the fixtures below establish `live inbox steer folds into a relaunched run` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // ADR-0054 P4: a steer message queued on the run's live inbox is drained at the
    // Run boundary, folded (re-identified) into the transcript, and drives a second
    // relaunched Run — so steer/redirect reaches an external-CLI Run.
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    let e = exec(vec![
        r#"{"type":"message","text":"Run"}"#.into(),
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
async fn terminal_business_outcome_cleanup_gates_relaunch_without_consuming_steer() {
    // Cause/effect rules: cleanup initially healthy/pending x queued steer x
    // eventual exact exit/owner loss x independent child uncertainty.
    // Healthy+queued => relaunch and consume; pending+* => commit once, retain
    // original future and queued input, never relaunch. Pending+late exit =>
    // return; pending+owner drop => veto. Other-child veto is never cleared.
    // Every combination uses the real executor/supervisor poll and commit path.
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};
    for pending in [false, true] {
        for queued in [false, true] {
            for abort in [false, true] {
                for other_unknown in [false, true] {
                    let gate = Arc::new(AtomicUsize::new(if pending { 2 } else { 0 }));
                    let opens = Arc::new(AtomicUsize::new(0));
                    let prompts = Arc::new(AtomicUsize::new(0));
                    let mut executor = AcpRunExecutor::new(Arc::new(CountingSource {
                        opens: opens.clone(),
                        prompts: prompts.clone(),
                        process_polls: Arc::new(AtomicUsize::new(0)),
                        observation: gate.clone(),
                    }));
                    executor.policy.reap_grace = std::time::Duration::from_millis(1);
                    let inbox = LiveInbox::new();
                    if queued {
                        let _ = inbox.offer_as(
                            MessageOrigin::External,
                            Message::text(MessageId("retained-steer".into()), Role::User, "next"),
                        );
                    }
                    let coordinator = Arc::new(RecordingCoordinator::default());
                    let context = RuntimeRunContext::new()
                        .with_commit(coordinator.clone())
                        .with_live_inbox(inbox.clone());
                    let quiescence = context.physical_quiescence.clone();
                    if other_unknown {
                        quiescence.mark_uncertain();
                    }
                    let mut execution = Box::pin(executor.execute(activation(), context));
                    if pending {
                        assert!(
                            tokio::time::timeout(
                                std::time::Duration::from_millis(100),
                                execution.as_mut()
                            )
                            .await
                            .is_err(),
                            "pending process cannot return"
                        );
                        assert_eq!(coordinator.commits.lock().unwrap().len(), 1);
                        assert_eq!(opens.load(Ordering::SeqCst), 1);
                        assert_eq!(prompts.load(Ordering::SeqCst), 1);
                        assert_eq!(inbox.list().len(), usize::from(queued));
                        if abort {
                            drop(execution);
                            assert!(!quiescence.can_acknowledge_return(), "lost observer veto");
                            continue;
                        }
                        gate.store(0, Ordering::SeqCst);
                    }
                    let state = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
                        .await
                        .expect("late exact exit releases original execution")
                        .unwrap();
                    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
                    assert_eq!(coordinator.commits.lock().unwrap().len(), 1);
                    let relaunch = queued && !pending && !other_unknown;
                    assert_eq!(opens.load(Ordering::SeqCst), 1 + usize::from(relaunch));
                    assert_eq!(prompts.load(Ordering::SeqCst), 1 + usize::from(relaunch));
                    assert_eq!(inbox.list().len(), usize::from(queued && !relaunch));
                    assert_eq!(quiescence.can_acknowledge_return(), !other_unknown);
                }
            }
        }
    }
}

#[tokio::test]
async fn opening_a_session_retains_the_attempt_observer_until_the_typed_owner_returns() {
    // Causes: C1 source.open returns an AgentSession; C2 it returns a launch
    // error; C3 it has entered but never returns; C4 the caller/replacement
    // drops that pending execution; C5 a later observer returns. Effects: E1
    // C1/C2 disarm only the open observer; E2 C3+C4 monotonically vetoes the
    // original attempt's ACK; E3 C5 cannot clear that stale-observer veto.
    // Constraints: AgentSession is the typed process/channel owner handed to
    // CleanupScope; a signal, EOF, timeout or stale replacement is not an exit
    // receipt, and a later observer cannot clear AttemptQuiescence.
    //
    // | Rule | open result | typed owner returned | caller drops | ACK |
    // | O1   | session     | yes                  | no           | yes |
    // | O2   | error       | no launched owner    | no           | yes |
    // | O3   | pending     | no                   | yes          | veto|
    //
    // Normal exit, TERM/KILL without observation, supervisor timeout and
    // post-open Drop are covered by the adjacent cleanup/protocol matrices;
    // O3 is the missing pre-handoff caller-cancellation/stale-observer row.
    let successful = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: Vec::new(),
        open_error: None,
    }));
    let context = RuntimeRunContext::new();
    let session = crate::open_agent_session(
        successful.source.as_ref(),
        &activation_without_session_home(),
        &context,
    )
    .await
    .expect("O1 returns the typed owner");
    drop(session);
    assert!(context.physical_quiescence.can_acknowledge_return(), "O1");

    let failed = AcpRunExecutor::new(Arc::new(ScriptedSource {
        frames: Vec::new(),
        open_error: Some("not launched".into()),
    }));
    let context = RuntimeRunContext::new();
    assert!(
        crate::open_agent_session(
            failed.source.as_ref(),
            &activation_without_session_home(),
            &context,
        )
        .await
        .is_err(),
        "O2"
    );
    assert!(context.physical_quiescence.can_acknowledge_return(), "O2");

    struct PendingOpen(Arc<AtomicBool>);
    #[async_trait]
    impl AgentChannelSource for PendingOpen {
        async fn open(
            &self,
            _activation: &RunActivation,
            _context: &RuntimeRunContext,
            _door: ChannelOpenDoor,
        ) -> std::result::Result<AgentSession, OpenError> {
            self.0.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    let entered = Arc::new(AtomicBool::new(false));
    let executor = AcpRunExecutor::new(Arc::new(PendingOpen(entered.clone())));
    let context = RuntimeRunContext::new().with_ownership(Arc::new(ScriptedOwnership::new([
        OwnershipDecision::Current,
    ])));
    let quiescence = context.physical_quiescence.clone();
    let mut execution = Box::pin(executor.execute(activation_without_session_home(), context));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), execution.as_mut())
            .await
            .is_err(),
        "O3 source remains pending"
    );
    assert!(
        entered.load(Ordering::SeqCst),
        "O3 crossed the source boundary"
    );
    drop(execution);
    assert!(
        !quiescence.can_acknowledge_return(),
        "O3 caller/replacement Drop loses the observer and must veto ACK"
    );
    quiescence.observe_return(async {}).await;
    assert!(
        !quiescence.can_acknowledge_return(),
        "O3 a later observer cannot release the stale attempt"
    );
}

// Reuse the scripted protocol and counting process; change only observation.
struct RecoverableSource {
    script: ScriptedSource,
    observation: Arc<AtomicUsize>,
}
#[async_trait]
impl AgentChannelSource for RecoverableSource {
    async fn open(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
        _door: ChannelOpenDoor,
    ) -> std::result::Result<AgentSession, OpenError> {
        let mut session = self.script.open(activation, context, _door).await?;
        session.process = Arc::new(CountingProcess {
            polls: Arc::new(AtomicUsize::new(0)),
            observation: self.observation.clone(),
        });
        Ok(session)
    }
}

#[tokio::test]
async fn agent_terminal_reason_never_substitutes_for_late_physical_exit() {
    // Rules T1..T9: agent NaturalEnd/Cancelled/TimedOut x live None/poll error/
    // stuck poll => commit the logical outcome but keep outer execution pending.
    // Exact exit on that same handle then releases it without rewriting the fact.
    // EOF from the script and failed TERM/KILL are deliberately not receipts.
    for reason in ["natural_end", "cancelled", "timed_out"] {
        for mode in [1, 2, 3] {
            let observation = Arc::new(AtomicUsize::new(mode));
            let mut executor = AcpRunExecutor::new(Arc::new(RecoverableSource {
                script: ScriptedSource {
                    frames: vec![
                        r#"{"type":"message","text":"done"}"#.into(),
                        format!(r#"{{"type":"turn_end","reason":"{reason}"}}"#),
                    ],
                    open_error: None,
                },
                observation: observation.clone(),
            }));
            executor.policy.reap_grace = std::time::Duration::from_millis(2);
            let coordinator = Arc::new(RecordingCoordinator::default());
            let context = RuntimeRunContext::new().with_commit(coordinator.clone());
            let quiescence = context.physical_quiescence.clone();
            let mut execution = Box::pin(executor.execute(activation(), context));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), execution.as_mut())
                    .await
                    .is_err(),
                "{reason}/{mode}: no exit yet"
            );
            assert_eq!(coordinator.commits.lock().unwrap().len(), 1);
            observation.store(0, Ordering::SeqCst);
            let state = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
                .await
                .expect("original observer recovers")
                .unwrap();
            if reason == "natural_end" {
                assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
            } else if reason == "cancelled" {
                assert_eq!(state, RunState::Ended(EndCause::Cancelled));
            } else {
                assert!(matches!(state, RunState::Ended(EndCause::Stopped(_))));
            }
            assert_eq!(coordinator.commits.lock().unwrap().len(), 1);
            assert!(quiescence.can_acknowledge_return());
        }
    }
}

#[tokio::test]
async fn retained_cleanup_drains_the_original_channel_until_exit() {
    // D1: first cleanup fails, then a peer must write more than pipe capacity
    // before exit can be observed => same-scope draining releases the writer.
    // D2: late bytes look like a terminal frame but cause no fact/commit replay.
    let observation = Arc::new(AtomicUsize::new(2));
    let source = RecoverableSource {
        script: ScriptedSource {
            frames: vec![],
            open_error: None,
        },
        observation: observation.clone(),
    };
    let quiescence = awaken_runtime_contract::runtime_context::AttemptQuiescence::default();
    let mut scope = CleanupScope::new(quiescence.clone());
    // The one door: `open` itself is unreachable without a `ChannelOpenDoor`,
    // which only `open_agent_session` can mint.
    let mut session = crate::open_agent_session(&source, &activation(), &RuntimeRunContext::new())
        .await
        .unwrap();
    let (ours, mut peer) = tokio::io::duplex(32);
    session.channel = Box::new(ours);
    scope.session = Some(session);
    scope.complete = false;
    let grace = std::time::Duration::from_millis(2);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), scope.finish(grace))
            .await
            .is_err()
    );
    observation.store(1, Ordering::SeqCst);
    let writer = async {
        peer.write_all(&vec![b'x'; 4096]).await.unwrap();
        peer.write_all(b"{\"type\":\"turn_end\",\"reason\":\"natural_end\"}\n")
            .await
            .unwrap();
        observation.store(0, Ordering::SeqCst);
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(scope.finish(grace), writer);
    })
    .await
    .expect("retained duplex permits the exit receipt");
    drop(scope);
    assert!(quiescence.can_acknowledge_return());
}

#[tokio::test]
async fn a_requested_pause_awaits_the_run_on_a_resume_ticket() {
    // Causes: the fixtures below establish `a requested pause awaits the run on a resume ticket`
    // with the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `a requested pause awaits the run on a resume ticket` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    // ADR-0054 P5/U2: an operator pause requested by the next boundary awaits the ACP
    // run durably — `RunState::Awaiting` on a no-tool `ManualPause` ticket — rather than
    // ending, even though the Run reached a natural end. The Run's messages commit
    // before the await (clean commit-then-await), mirroring the native engine.
    use awaken_agent_contract::agent::awaiting::AwaitReason;
    use awaken_runtime_contract::pause::PauseSignal;

    let e = exec(vec![
        r#"{"type":"message","text":"Run"}"#.into(),
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
    // The in-flight Run commits before the await.
    assert!(
        commits[0]
            .messages
            .iter()
            .any(|m| m.text_content() == "Run"),
        "the Run's assistant text commits before awaiting"
    );
    // A resumable no-tool `ManualPause` ticket rode the same commit.
    let ticket = commits[0]
        .resume_ticket()
        .expect("an awaiting run commits its awaiting ticket");
    assert_eq!(ticket.reason(), AwaitReason::ManualPause);
    assert_eq!(ticket.run_id, RunId("run-1".into()));
    assert_eq!(ticket.thread_id, ThreadId("thread-1".into()));
    assert!(
        ticket.call_id().is_none() && ticket.pending_tool().is_none(),
        "an operator pause awaits on no tool"
    );
}

#[tokio::test]
async fn a_pause_commits_in_flight_steer_before_awaiting() {
    // Causes: the fixtures below establish `a pause commits in flight steer before awaiting` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `a pause commits in flight steer before awaiting` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    // Pause preempts queued input, but the in-flight steer is not lost: it rides out
    // with the await (fold) and commits before the run awaits (boundary priority is
    // pause > queued-input > idle).
    use awaken_agent_contract::agent::awaiting::AwaitReason;
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};
    use awaken_runtime_contract::pause::PauseSignal;

    let e = exec(vec![
        r#"{"type":"message","text":"Run"}"#.into(),
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
        commits[0].resume_ticket().map(ResumeTicket::reason),
        Some(AwaitReason::ManualPause)
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
    // A terminal ACP commit also consumes its shared permission batch. Usage
    // remains exactly one independently owned Thread cell, not the whole plan.
    let usage_commands = commits[0]
        .state
        .iter()
        .filter(|command| command.key.0 == THREAD_USAGE_STATE_KEY)
        .collect::<Vec<_>>();
    assert_eq!(usage_commands.len(), 1, "one usage cell per commit");
    let cmd = usage_commands[0];
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
    use std::path::PathBuf;

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
            _door: ChannelOpenDoor,
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
            Ok(fixture_session(
                Box::new(ours),
                Arc::new(FakeProcess),
                awaken_protocol_acp::Codec::Newline,
                Some("/workspace".to_string()),
            ))
        }
    }

    let root = std::env::temp_dir().join(format!("acp-recovery-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (store_a, store_b) = (root.join("a"), root.join("b"));
    let dir_a = ConfigHome::open(Some(&store_a), &activation().thread_id.0)
        .unwrap()
        .root()
        .to_path_buf();
    let dir_b = ConfigHome::open(Some(&store_b), &activation().thread_id.0)
        .unwrap()
        .root()
        .to_path_buf();
    let blobs = Arc::new(FsSessionBlobStore::new(root.join("blobs")));

    // Run 1 in dir A: a fresh session; the marker is written and harvested.
    let coord_a = Arc::new(RecordingCoordinator::default());
    AcpRunExecutor::new(Arc::new(PersistingSource { dir: dir_a.clone() }))
        .with_session_home(Arc::new(DirSessionHome::new(Some(store_a), blobs.clone())))
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
        .with_session_home(Arc::new(DirSessionHome::new(Some(store_b), blobs.clone())))
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

    e.execute(
        activation(),
        RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default())),
    )
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

#[tokio::test]
async fn session_home_writes_require_live_attempt_ownership() {
    // Causes: the fixtures below establish `session home` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1=a LocalDir session-home binding exists;
    // C2=authority is current/lost/down before restore; C3=authority is lost
    // after the ACP Step but before harvest. Effects: E1=restore and harvest
    // exactly once; E2=zero session-home/process calls; E3=restore once but
    // never harvest stale process state. The absent compatibility path is owned
    // by `a_local_dir_session_home_is_restored_before_and_harvested_after`.
    //
    // | Rule | Authority sequence          | Home calls       | Effect |
    // | O1   | current x5                  | restore, harvest | E1     |
    // | O2   | lost/down                   | none             | E2     |
    // | O3   | current x4, lost            | restore          | E3     |
    let recorder = Arc::new(RecordingSessionHome::default());
    let (executor, opens, prompts, _) = counting_executor();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(RecordingCoordinator::default()))
        .with_ownership(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
        ])));
    assert_eq!(
        executor
            .with_session_home(recorder.clone())
            .execute(activation(), context)
            .await
            .expect("O1 current authority"),
        RunState::Ended(EndCause::NaturalEnd),
        "O1/E1"
    );
    assert_eq!(opens.load(Ordering::SeqCst), 1, "O1/E1");
    assert_eq!(prompts.load(Ordering::SeqCst), 1, "O1/E1");
    assert_eq!(
        recorder
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, _, _)| kind.as_str())
            .collect::<Vec<_>>(),
        vec!["restore", "harvest"],
        "O1/E1"
    );

    for (label, decision) in [
        ("lost", OwnershipDecision::Lost),
        ("down", OwnershipDecision::Unavailable),
    ] {
        let recorder = Arc::new(RecordingSessionHome::default());
        let (executor, opens, _, _) = counting_executor();
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(RecordingCoordinator::default()))
            .with_ownership(Arc::new(ScriptedOwnership::new([decision])));
        executor
            .with_session_home(recorder.clone())
            .execute(activation(), context)
            .await
            .expect_err("O2 stale authority fences restore");
        assert!(recorder.calls.lock().unwrap().is_empty(), "O2/E2 {label}");
        assert_eq!(opens.load(Ordering::SeqCst), 0, "O2/E2 {label}");
    }

    let recorder = Arc::new(RecordingSessionHome::default());
    let (executor, opens, prompts, _) = counting_executor();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(RecordingCoordinator::default()))
        .with_ownership(Arc::new(ScriptedOwnership::new([
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Current,
            OwnershipDecision::Lost,
        ])));
    executor
        .with_session_home(recorder.clone())
        .execute(activation(), context)
        .await
        .expect_err("O3 stale authority fences harvest");
    assert_eq!(opens.load(Ordering::SeqCst), 1, "O3/E3");
    assert_eq!(prompts.load(Ordering::SeqCst), 1, "O3/E3");
    assert_eq!(recorder.calls.lock().unwrap()[0].0, "restore", "O3/E3");
    assert_eq!(recorder.calls.lock().unwrap().len(), 1, "O3/E3");
}

#[test]
fn session_home_binding_is_none_for_a_non_acp_backend() {
    // A native (non-ACP) backend has no CLI session-home — the binding is absent, so
    // the provider is never engaged (Gateway/stateless adapters skip the same way).
    let mut act = activation();
    act.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
            "prov", "model", "native",
        ));
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
        async fn evaluate(&self, _ctx: &ToolCall) -> ToolPermissionRuling {
            ToolPermissionRuling::policy(
                self.0.clone(),
                awaken_runtime_contract::permission::PermissionBasis::fixed(
                    awaken_runtime_contract::permission::PermissionPolicy::Rule,
                ),
            )
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
            grant_observer: None,
            audit: None,
        };
        assert_eq!(resolver.resolve(&ask).await, want);
    }
}

#[tokio::test]
async fn every_acp_permission_decision_is_recorded_with_the_policy_that_made_it() {
    // Cause/effect graph: C1 the neutral policy decides an ACP tool request and
    // names the authored policy that decided (a fixed one, or `auto` with its
    // grounds); C2 a per-Run narrowing refuses one before the session policy is
    // consulted; C3 an allowing narrowing lets the session policy answer.
    // Effects: E1 one `PermissionDecided` fact per decided call, carrying the
    // gate label, `decided_by: policy` and that policy; E2 the narrowing's own
    // refusal is recorded once, by the narrowing, as `decided_by: run_narrowing`; E3 an allowing narrowing records nothing of its
    // own — the session policy's record is the answer.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | L1 | always_allow → allow | F | F | E1 allow + always_allow |
    // | L2 | auto → ask(indeterminate) | F | F | E1 ask + auto + indeterminate |
    // | L3 | — | T (deny) | F | E2 one deny, no session record |
    // | L4 | always_allow → allow | F | T | E3 exactly one record, the session's |
    //
    // Without this ledger an ACP session commits no decision at all, and the
    // Managed projection — having nothing to read — reports every such call as
    // permitted. That is the fail-open this closes for the ACP backend.
    use awaken_agent_contract::audit::kind::Kind;
    use awaken_agent_contract::audit::permission::{PermissionPolicy, PermissionReasonCode};
    use awaken_protocol_acp::{PermissionAsk, PermissionResolver, PermissionVerdict};
    use awaken_runtime_contract::permission::{
        PermissionBasis, ToolCall, ToolPermissionPolicy, ToolPermissionRuling,
        ToolPermissionVerdict,
    };

    struct RulingPolicy(ToolPermissionRuling);
    #[async_trait]
    impl ToolPermissionPolicy for RulingPolicy {
        async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionRuling {
            self.0.clone()
        }
    }

    fn ask(call_id: &str) -> PermissionAsk {
        PermissionAsk {
            tool: "bash".into(),
            call_id: call_id.into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        }
    }
    type Fact = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    fn facts(audit: &PermissionAudit) -> Vec<Fact> {
        audit
            .drain()
            .into_iter()
            .filter(|draft| draft.kind == Kind::PermissionDecided)
            .map(|draft| {
                let text = |key: &str| {
                    draft.payload[key]
                        .as_str()
                        .map(std::string::ToString::to_string)
                };
                (
                    draft.payload["call_id"].as_str().unwrap_or_default().into(),
                    draft.payload["decision"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    text("decided_by"),
                    text("policy"),
                    text("reason_code"),
                )
            })
            .collect()
    }

    let allowing = RulingPolicy(ToolPermissionRuling::policy(
        ToolPermissionVerdict::Allow,
        PermissionBasis::fixed(PermissionPolicy::AlwaysAllow),
    ));
    let judged = RulingPolicy(ToolPermissionRuling::policy(
        ToolPermissionVerdict::RequireConfirmation {
            correlation_id: "tk".into(),
        },
        PermissionBasis::auto(Some(PermissionReasonCode::INDETERMINATE)),
    ));

    // L1 / L2: the session policy decides and its record names the policy.
    for (rule, policy, call_id, decision, expected_policy, expected_reason) in [
        (
            "L1",
            &allowing,
            "c1",
            "allow",
            PermissionPolicy::AlwaysAllow,
            None,
        ),
        (
            "L2",
            &judged,
            "c2",
            "ask",
            PermissionPolicy::Auto,
            Some(PermissionReasonCode::INDETERMINATE),
        ),
    ] {
        let audit = Arc::new(PermissionAudit::default());
        let resolver = NeutralPermissionResolver {
            policy: Arc::new(RulingPolicy(policy.0.clone())),
            grant_observer: None,
            audit: None,
        }
        .recording_into(audit.clone());
        resolver.resolve(&ask(call_id)).await;
        assert_eq!(
            facts(&audit),
            vec![(
                call_id.to_string(),
                decision.to_string(),
                Some("policy".to_string()),
                Some(expected_policy.as_str().to_string()),
                expected_reason.map(|code: PermissionReasonCode| code.as_str().to_string()),
            )],
            "{rule}/E1"
        );
    }

    // L3: a narrowing that refuses is the decision, recorded by the narrowing.
    let audit = Arc::new(PermissionAudit::default());
    let session_policy = NeutralPermissionResolver {
        policy: Arc::new(RulingPolicy(ToolPermissionRuling::policy(
            ToolPermissionVerdict::Allow,
            PermissionBasis::fixed(PermissionPolicy::AlwaysAllow),
        ))),
        grant_observer: None,
        audit: None,
    }
    .recording_into(audit.clone());
    let deny_all = awaken_runtime_contract::permission::DenyAllTools::new("restricted Run");
    let narrowed = NarrowedPermissionResolver {
        base: &session_policy,
        narrowing: &deny_all,
        audit: &audit,
    };
    assert_eq!(narrowed.resolve(&ask("c3")).await, PermissionVerdict::Deny);
    let recorded = facts(&audit);
    assert_eq!(recorded.len(), 1, "L3/E2 one record, not two");
    assert_eq!(
        (
            recorded[0].1.as_str(),
            recorded[0].2.as_deref(),
            recorded[0].3.as_deref()
        ),
        ("deny", Some("run_narrowing"), None),
        "L3/E2 a per-Run narrowing names itself and no authored policy"
    );

    // L4: an allowing narrowing defers, and only the session policy's record stands.
    let narrowing_allow = RulingPolicy(ToolPermissionRuling::run_narrowing(
        ToolPermissionVerdict::Allow,
    ));
    let narrowed = NarrowedPermissionResolver {
        base: &session_policy,
        narrowing: &narrowing_allow,
        audit: &audit,
    };
    assert_eq!(narrowed.resolve(&ask("c4")).await, PermissionVerdict::Allow);
    assert_eq!(
        facts(&audit),
        vec![(
            "c4".to_string(),
            "allow".to_string(),
            Some("policy".to_string()),
            Some(PermissionPolicy::AlwaysAllow.as_str().to_string()),
            None,
        )],
        "L4/E3"
    );
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
        async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionRuling {
            ToolPermissionRuling::run_narrowing(self.0.clone())
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

    let audit = PermissionAudit::default();
    let narrowed = NarrowedPermissionResolver {
        base: &base_allow,
        narrowing: &deny_all,
        audit: &audit,
    };
    assert_eq!(narrowed.resolve(&ask).await, PermissionVerdict::Deny);

    let cannot_widen = NarrowedPermissionResolver {
        base: &base_deny,
        narrowing: &narrowing_allow,
        audit: &audit,
    };
    assert_eq!(cannot_widen.resolve(&ask).await, PermissionVerdict::Deny);
}

#[tokio::test]
async fn resumed_permission_is_one_shot_and_semantically_exact_across_new_wire_ids() {
    // Cause/effect graph and FMECA:
    // C1=a durable decision freezes tool T and arguments A; C2=replacement ACP
    // reissues T/A with a new wire id; C3=tool or arguments differ; C4=the exact
    // request is repeated after consumption. Effects: E1=the frozen decision is
    // applied once; E2=normal policy remains authoritative. Failure mode FM1 was
    // matching only the obsolete id, which produced a second approval and could
    // duplicate the side effect; semantic equality plus one-shot consumption is
    // the mitigation. FM2 is authority widening; exact T/A equality and fallback
    // to the base resolver mitigate it.
    //
    // | Rule | C2 | C3 | C4 | Effect |
    // | SR1  | T  | F  | F  | E1     |
    // | SR2  | *  | T  | *  | E2     |
    // | SR3  | T  | F  | T  | E2     |
    use awaken_protocol_acp::{PermissionAsk, PermissionResolver, PermissionVerdict};

    struct AskResolver;
    #[async_trait]
    impl PermissionResolver for AskResolver {
        async fn resolve(&self, _ask: &PermissionAsk) -> PermissionVerdict {
            PermissionVerdict::Await {
                correlation_id: "base-policy".into(),
            }
        }
    }

    let decision = PermissionResume {
        call_id: "old-wire-id".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({"cmd": "echo approved"}),
        allow: true,
    };
    let resolver = ResumedPermissionResolver::new(&AskResolver, &decision, None);
    assert!(
        resolver.unconsumed_decision().is_some(),
        "unused authorization is known not dispatched"
    );
    let altered = PermissionAsk {
        tool: "bash".into(),
        call_id: "old-wire-id".into(),
        arguments: serde_json::json!({"cmd": "echo changed"}),
    };
    assert!(matches!(
        resolver.resolve(&altered).await,
        PermissionVerdict::Await { .. }
    ));

    let regenerated = PermissionAsk {
        tool: "bash".into(),
        call_id: "new-wire-id".into(),
        arguments: decision.arguments.clone(),
    };
    assert_eq!(
        resolver.resolve(&regenerated).await,
        PermissionVerdict::Allow
    );
    // A missing response after this point is not evidence of non-execution.
    assert!(
        resolver.unconsumed_decision().is_none(),
        "consumed grant has no not-executed proof"
    );
    assert!(matches!(
        resolver.resolve(&regenerated).await,
        PermissionVerdict::Await { .. }
    ));

    let denied = PermissionResume {
        allow: false,
        ..decision
    };
    assert_eq!(
        ResumedPermissionResolver::new(&AskResolver, &denied, None)
            .resolve(&regenerated)
            .await,
        PermissionVerdict::Deny
    );
}

#[cfg(feature = "real-acp")]
#[tokio::test]
async fn permission_wait_survives_executor_replacement_and_resumes_the_loaded_session() {
    // Causes: the fixtures below establish `permission wait survives executor replacement and` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // FMECA cause-effect graph: C1=ACP agent requests permission; C2=neutral
    // policy requires confirmation; C3=the first executor/process exits after
    // committing the wait; C4=replacement loads the durable ACP session;
    // C5=operator decision. Effects: E1=one ToolPermission ticket with exact
    // correlation/tool identity; E2=first ACP request receives `cancelled` and
    // cannot keep an in-memory authority alive; E3=replacement uses
    // `session/load` and may regenerate the same request under a new wire id;
    // E4=the exact semantic request receives the allow/reject option once;
    // E5=one terminal continuation consumes the ticket; E6=the replacement's
    // assistant output advances from the committed permission ToolUse Step.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5    | E1-E3 | E4     | E5 | E6 |
    // | AR1  | T  | T  | T  | T  | allow | T     | allow  | T  | steps 0,1 |
    // | AR2  | T  | T  | T  | T  | deny  | T     | reject | T  | steps 0,1 |
    //
    // Invalid free-form input and stale/mismatched ticket rules belong to the
    // authoritative runtime resume state machine and are covered in
    // awaken-runtime/tests/awaiting.rs; this adapter test owns only ACP wire
    // projection plus replacement recovery, avoiding a duplicate validator.
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};
    use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};

    struct AskPolicy;
    #[async_trait]
    impl ToolPermissionPolicy for AskPolicy {
        async fn evaluate(&self, call: &ToolCall) -> ToolPermissionRuling {
            let changed = call.arguments == serde_json::json!({"cmd":"echo changed"});
            assert!(
                changed || call.arguments == serde_json::json!({"cmd":"echo ok"}),
                "AR0 permission input supersedes placeholder"
            );
            ToolPermissionRuling::policy(
                ToolPermissionVerdict::RequireConfirmation {
                    correlation_id: if changed { "approval-2" } else { "approval-1" }.to_string(),
                },
                awaken_runtime_contract::permission::PermissionBasis::fixed(
                    awaken_runtime_contract::permission::PermissionPolicy::Rule,
                ),
            )
        }
    }

    struct PermissionSource {
        changed: bool,
        settling_sibling: bool,
        tool_name: &'static str,
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
            _door: ChannelOpenDoor,
        ) -> std::result::Result<AgentSession, OpenError> {
            let tool_name = self.tool_name;
            let settling_sibling = self.settling_sibling;
            let attempt = self.opens.fetch_add(1, Ordering::SeqCst);
            let changed = self.changed && attempt > 0;
            let wire_id = if attempt == 0 {
                "tool-1"
            } else if changed && attempt > 1 {
                "tool-3"
            } else {
                "tool-2"
            };
            let arguments =
                serde_json::json!({"cmd": if changed { "echo changed" } else { "echo ok" }});
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
                            // AR0 Claude streams {} before asking with complete
                            // arguments. The Await commit must refine this same
                            // occurrence, with the exact policy input and ticket.
                            if attempt == 0 {
                                let initial = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"permission-session","update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","title":tool_name,"rawInput":{}}}});
                                output
                                    .write_all(format!("{initial}\n").as_bytes())
                                    .await
                                    .unwrap();
                            }
                            if attempt > 0 {
                                // AR4 actual SDK emits a new placeholder and then
                                // rawInput progress before the regenerated ask.
                                for update in [
                                    serde_json::json!({"sessionUpdate":"tool_call","toolCallId":wire_id,"title":tool_name,"rawInput":{}}),
                                    serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":wire_id,"rawInput":arguments}),
                                ] {
                                    let frame = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"permission-session","update":update}});
                                    output
                                        .write_all(format!("{frame}\n").as_bytes())
                                        .await
                                        .unwrap();
                                }
                            }
                            if attempt == 0 && settling_sibling {
                                let sibling = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"permission-session",
                                    "update":{"sessionUpdate":"tool_call","toolCallId":"sibling-read","title":"mcp__awaken_session__read","rawInput":{"file_path":"/workspace/.skills/example/SKILL.md"}}}});
                                output
                                    .write_all(format!("{sibling}\n").as_bytes())
                                    .await
                                    .unwrap();
                            }
                            // One wire request builder owns original/exact/changed
                            // cases; only call identity and input vary.
                            let permission = serde_json::json!({"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{
                                "sessionId":"permission-session", "toolCall":{"toolCallId":wire_id,"title":tool_name,"rawInput":arguments},
                                "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]
                            }});
                            output
                                .write_all(format!("{permission}\n").as_bytes())
                                .await
                                .unwrap();
                            output.flush().await.unwrap();
                            line.clear();
                            lines.read_line(&mut line).await.unwrap();
                            replies.lock().unwrap().push(line.trim().to_string());
                            events.notify_one();
                            if attempt == 0 || (changed && attempt == 1) {
                                if settling_sibling {
                                    line.clear();
                                    lines.read_line(&mut line).await.unwrap();
                                    assert!(line.contains("session/cancel"));
                                    let sibling = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"permission-session",
                                        "update":{"sessionUpdate":"tool_call_update","toolCallId":"sibling-read","status":"completed",
                                        "content":[{"type":"content","content":{"type":"text","text":"# Exact Skill bytes"}}]}}});
                                    let output = lines.get_mut();
                                    output
                                        .write_all(format!("{sibling}\n").as_bytes())
                                        .await
                                        .unwrap();
                                    output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"cancelled\"}}\n").await.unwrap();
                                    output.flush().await.unwrap();
                                }
                                return;
                            }
                            let rejected = line.contains("reject");
                            let terminal = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"permission-session","update":{
                                "sessionUpdate":"tool_call_update", "toolCallId":wire_id,
                                "status":if rejected {"failed"} else {"completed"},
                                "content":[{"type":"content","content":{"type":"text","text":if rejected {"denied"} else {"ok"}}}]
                            }}});
                            let output = lines.get_mut();
                            output
                                .write_all(format!("{terminal}\n").as_bytes())
                                .await
                                .unwrap();
                            output.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"permission-session\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"continued\"}}}}\n").await.unwrap();
                            output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"stopReason\":\"end_turn\"}}\n").await.unwrap();
                            output.flush().await.unwrap();
                            return;
                        }
                        _ => return,
                    }
                }
            });
            Ok(fixture_session(
                Box::new(ours),
                Arc::new(FakeProcess),
                Codec::Acp,
                None,
            ))
        }
    }

    // AO decision table: installed continuation + canonical owned tool =>
    // RuntimeToolBatch ticket and matching durable batch; unowned tool =>
    // RunAttemptExecutor ticket and no batch. Cross the actual ACP request,
    // policy, await commit and Native batch validator, not a fabricated ticket.
    // AS1 sibling Read starts before Await but completes during cancellation:
    // both owned/unowned pending tool rows commit its exact bytes once in the
    // original ThreadCommit. AS2 pending target still has no result and the
    // original ticket/batch owner survives; projection must not borrow the
    // sibling result as proof that the awaiting operation executed.
    for tool_name in ["write", "desktop_search", "bash"] {
        let source = Arc::new(PermissionSource {
            changed: false,
            settling_sibling: true,
            tool_name,
            opens: AtomicUsize::new(0),
            permission_replies: Default::default(),
            resumed_prompts: Default::default(),
            permission_events: Arc::new(tokio::sync::Notify::new()),
        });
        let committed = Arc::new(RecordingCoordinator::default());
        let executor = AcpRunExecutor::new(source)
            .with_permission_policy(Arc::new(AskPolicy))
            .with_owned_tool_continuation(Some(Arc::new(OwnedContinuationProbe {
                resumes: Default::default(),
                recoveries: AtomicUsize::new(0),
                fail: false,
            })));
        assert_eq!(
            executor
                .execute(
                    activation(),
                    RuntimeRunContext::new()
                        .with_commit(committed.clone())
                        .with_reader(committed.clone())
                )
                .await
                .unwrap(),
            RunState::Awaiting
        );
        let ticket = committed.resume_ticket_for(&RunId("run-1".into())).unwrap();
        let messages = committed.messages();
        let results = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = block
                {
                    Some((tool_use_id, content, *is_error))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1, "AS1 only actual sibling result");
        assert_eq!(results[0].0, "sibling-read");
        assert_eq!(
            results[0].1,
            &vec![ContentBlock::text("# Exact Skill bytes")]
        );
        assert!(!results[0].2);
        assert_eq!(ticket.call_id(), Some("tool-1"), "AS2 original wait target");
        let batch = awaken_runtime_contract::ActiveToolBatch::load(&Store::rebuild(
            &committed.committed_state(&ticket.thread_id),
        ))
        .unwrap();
        assert_eq!(batch.is_some(), tool_name != "bash", "AO owner {tool_name}");
        if let Some(batch) = batch {
            batch
                .validate_awaiting_ticket(&ticket.thread_id, &ticket)
                .unwrap();
            assert_eq!(
                ticket.reason(),
                if tool_name == "desktop_search" {
                    AwaitReason::ExternalEvent
                } else {
                    AwaitReason::ToolPermission
                },
                "AO wait reason {tool_name}"
            );
        } else {
            assert_eq!(
                ticket.tool_call_authority(),
                Some(awaken_agent_contract::agent::awaiting::ToolCallAuthority::RunAttemptExecutor)
            );
        }
    }

    // AR5/AR6 allow/deny O -> changed request N: no transfer of permission;
    // close O as not executed in the same commit that publishes N's new ticket.
    // After approving N, only N's result may settle it, with no dangling O.
    for (allow, changed) in [(true, false), (false, false), (true, true), (false, true)] {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(tokio::sync::Notify::new());
        let source: Arc<dyn AgentChannelSource> = Arc::new(PermissionSource {
            changed,
            settling_sibling: false,
            tool_name: "bash",
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
        // AR1/AR2 external execution => explicit attempt authority, no local
        // Runtime batch. Replacing the executor must retain that same owner.
        assert_eq!(
            ticket.tool_call_authority(),
            Some(awaken_agent_contract::agent::awaiting::ToolCallAuthority::RunAttemptExecutor)
        );
        assert!(
            awaken_runtime_contract::ActiveToolBatch::load(&Store::rebuild(
                &committed.committed_state(&ticket.thread_id),
            ))
            .unwrap()
            .is_none()
        );
        assert_eq!(ticket.reason(), AwaitReason::ToolPermission);
        assert_eq!(ticket.correlation_id, "approval-1");
        assert_eq!(ticket.call_id(), Some("tool-1"));
        assert_eq!(
            ticket.pending_tool().unwrap().arguments,
            serde_json::json!({"cmd":"echo ok"}),
            "AR0"
        );
        let tool_inputs = committed
            .messages()
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, input, .. } if id == "tool-1" => Some(input.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tool_inputs,
            vec![serde_json::json!({"cmd":"echo ok"})],
            "AR0 one complete committed ToolUse"
        );
        assert_eq!(
            ticket.pending_tool().map(|tool| tool.tool_id.as_str()),
            Some("bash")
        );
        tokio::time::timeout(std::time::Duration::from_secs(30), events.notified())
            .await
            .expect("timed out waiting for events.notified()");
        assert!(
            replies.lock().unwrap()[0].contains("cancelled"),
            "the first process is released before durable HITL"
        );

        let result = if allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(Some("operator policy".to_string()))
        };
        let resumed = AcpRunExecutor::new(source.clone())
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
        tokio::time::timeout(std::time::Duration::from_secs(30), events.notified())
            .await
            .expect("timed out waiting for events.notified()");
        if changed {
            assert_eq!(
                resumed,
                RunState::Awaiting,
                "AR5 no grant for changed arguments"
            );
            let replacement = committed.resume_ticket_for(&ticket.run_id).unwrap();
            assert_eq!(replacement.call_id(), Some("tool-2"));
            assert_eq!(replacement.correlation_id, "approval-2");
            assert_eq!(
                replacement.pending_tool().unwrap().arguments,
                serde_json::json!({"cmd":"echo changed"})
            );
            assert_eq!(
                replacement.tool_call_authority(),
                ticket.tool_call_authority()
            );
            assert!(
                awaken_runtime_contract::ActiveToolBatch::load(&Store::rebuild(
                    &committed.committed_state(&ticket.thread_id),
                ))
                .unwrap()
                .is_none()
            );
            {
                let commits = committed.commits.lock().unwrap();
                assert_eq!(commits.len(), 2, "AR5 atomic settlement and replacement");
                let original_results = commits[1]
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } if tool_use_id == "tool-1" => Some((content, *is_error)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(original_results.len(), 1, "AR5 original call is settled");
                assert!(original_results[0].1);
                assert!(original_results[0].0.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("not executed"))));
            }
            assert!(replies.lock().unwrap()[1].contains("cancelled"));
            let executor = AcpRunExecutor::new(source).with_permission_policy(Arc::new(AskPolicy));
            assert!(
                executor
                    .resume(
                        activation(),
                        ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 1),
                        RuntimeRunContext::new()
                            .with_commit(committed.clone())
                            .with_reader(committed.clone())
                    )
                    .await
                    .is_err(),
                "AR5 old decision cannot replace the new ticket"
            );
            let finished = executor
                .resume(
                    activation(),
                    ResumeCommand::from_ticket(&replacement, ResumeResult::allow(), 1),
                    RuntimeRunContext::new()
                        .with_commit(committed.clone())
                        .with_reader(committed.clone()),
                )
                .await
                .unwrap();
            assert_eq!(finished, RunState::Ended(EndCause::NaturalEnd));
            assert!(committed.resume_ticket_for(&ticket.run_id).is_none());
            let history = committed.messages();
            let uses = history
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let answers = history
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => Some((tool_use_id.as_str(), *is_error)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(uses, ["tool-1", "tool-2"]);
            assert_eq!(
                answers,
                [("tool-1", true), ("tool-2", false)],
                "AR6 exact results, no transfer or duplicate"
            );
            assert_eq!(committed.commits.lock().unwrap().len(), 3);
            continue;
        }
        assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd));
        assert!(
            awaken_runtime_contract::ActiveToolBatch::load(&Store::rebuild(
                &committed.committed_state(&ticket.thread_id)
            ))
            .unwrap()
            .is_none(),
            "terminal continuation clears the shared batch"
        );
        let replies = replies.lock().unwrap();
        assert_eq!(replies.len(), 2);
        assert!(replies[1].contains(if allow { "allow" } else { "reject" }));
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains(if allow { "approved" } else { "denied" }));
        // AR3 loaded SDK state may record the prior tool as cancelled. The
        // continuation includes the ticket's exact JSON, never claims execution,
        // and distinguishes reissuing an approval from suppressing a denial.
        assert!(
            prompts[0].contains(r#"{"cmd":"echo ok"}"#),
            "AR3 exact frozen arguments"
        );
        assert!(
            prompts[0].contains(if allow {
                "This tool has not executed. Reissue exactly"
            } else {
                "Do not execute this denied request."
            }),
            "AR3 decision semantics"
        );
        let run = RunId("run-1".into());
        let assistant_steps = committed
            .messages()
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .filter_map(|message| message.id.assistant_step_of(&run))
            .collect::<Vec<_>>();
        assert_eq!(
            assistant_steps,
            vec![0, 2],
            "AR4 discarded staging ids are never reused"
        );
        let messages = committed.messages();
        let uses = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(uses, ["tool-1"], "AR4 one original ToolUse across commits");
        let results = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results,
            ["tool-1"],
            "AR4 allowed/denied result answers original exactly once"
        );
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
    // Causes: the fixtures below establish `a tool call and its result commit as neutral messages`
    // with the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    use awaken_agent_contract::agent::content::ContentBlock;

    // Causal graph:
    // ACP frames -> AcpProjectedEvent staging -> executor ACL -> committed Message.
    //
    // Decision table:
    // | projected input | committed role | committed block | Message identity | correlation |
    // | tool_call       | Assistant      | ToolUse         | canonical Run/Step | ACP id |
    // | tool_result     | Tool           | ToolResult       | private ACP fact   | same ACP id |
    // | turn_end        | no message     | —                | —                  | — |
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
    assert_eq!(
        messages[1].id,
        MessageId::assistant(&RunId("run-1".into()), 0),
        "assistant ToolUse shares the canonical Run/Step identity family"
    );
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
    assert_eq!(
        messages[2].id.assistant_step_of(&RunId("run-1".into())),
        None,
        "Role::Tool remains outside assistant Step classification"
    );
    match &messages[2].content[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => {
            assert_eq!(tool_use_id, "c1");
            assert_eq!(content[0], ContentBlock::text("file body"));
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
}

#[tokio::test]
async fn acp_tool_results_use_the_bound_spiller_and_fail_closed() {
    // Causes: the fixtures below establish `acp tool results use the bound spiller and fail closed`
    // with the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause-effect graph:
    // C1=ACP projects ToolResult; C2=spiller succeeds; C3=spiller fails;
    // C4=the attempt loses ownership before the spiller boundary.
    // C1+C2 -> transformed result alone enters the neutral transcript.
    // C1+C3 -> appender rejects the fact, the Run ends as an error, and no Tool
    // result carrying the unmaterialized payload commits.
    // C1+C4 -> the attempt fails and the spiller receives no call.
    //
    // | Rule | ACP result | spill | Expected effect |
    // | A1 | yes | success | stable run/call sent once; preview committed |
    // | A2 | yes | failure | terminal error; no Tool-role result committed |
    // | A3 | yes | ownership lost | attempt error; zero spill/commit effects |
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

    let stale_seen = Arc::new(Mutex::new(Vec::new()));
    let stale_commits = Arc::new(RecordingCoordinator::default());
    let error = exec(frames())
        .execute(
            activation_without_session_home(),
            RuntimeRunContext::new()
                .with_commit(stale_commits.clone())
                .with_tool_output_spiller(Arc::new(SpillProbe {
                    fail: false,
                    seen: stale_seen.clone(),
                }))
                .with_ownership(Arc::new(ScriptedOwnership::new([
                    OwnershipDecision::Current,
                    OwnershipDecision::Current,
                    OwnershipDecision::Lost,
                    OwnershipDecision::Lost,
                ]))),
        )
        .await
        .expect_err("A3 ownership loss is an attempt error");
    assert!(error.to_string().contains("no longer owns"), "A3: {error}");
    assert!(stale_seen.lock().unwrap().is_empty(), "A3 zero spill");
    assert!(
        stale_commits.commits.lock().unwrap().is_empty(),
        "A3 zero commit"
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
async fn acp_result_status_is_committed_as_the_same_typed_fact_as_native() {
    use awaken_agent_contract::agent::content::ContentBlock;
    // Decision table: terminal ACP success/failure x ordinary/empty/error-like
    // text -> preserve the exact status AND unchanged content in the commit.
    // A literal error-looking success must not be reclassified by text; a
    // failure must not become success or depend on a synthetic text prefix.
    for is_error in [false, true] {
        for body in ["denied", "", "[tool error] literal user document"] {
            let e = exec(vec![
                serde_json::json!({"type":"tool_result", "id":"c9", "content":body, "is_error":is_error}).to_string(),
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
            assert_eq!(
                commits[0].messages[1].content[0],
                ContentBlock::tool_result_with_error(
                    "c9",
                    vec![ContentBlock::text(body)],
                    is_error
                ),
            );
        }
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
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default())),
        )
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

// Fail-open guard (the class found in run-executor-a2a): a clean Run that ends on
// `TerminationReason::Error` — the agent reporting an error as its own terminal frame,
// NOT a driver/IO fault — flows through the `Idle` boundary arm's `end_cause`. It must
// map to a terminal ERROR, never to a success (`NaturalEnd`), or a failed run would be
// recorded as a clean completion. Exercises the `end_cause(Error)` row the truncated-
// stream test (a driver `Err`, i.e. `failure_cause`) never reaches.
#[tokio::test]
async fn a_clean_error_turn_end_maps_to_error_not_natural_end() {
    // Causes: the fixtures below establish `a clean error turn end` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `maps to error not natural end` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `a clean error turn end` is one independent branch selecting `maps to
    // error not natural end`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
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
        "a clean error Run must end in a terminal Error, got {state:?}"
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

// A clean Run that ends on `TerminationReason::TimedOut` (the agent/supervisor
// reporting the Run hit its deadline) maps through `end_cause` to a terminal
// `Stopped`, never to a success — the last untested clean-outcome row.
#[tokio::test]
async fn a_timed_out_turn_end_maps_to_stopped_not_natural_end() {
    // Causes: the fixtures below establish `a timed out turn end` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `maps to stopped not natural end` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `a timed out turn end` is one independent branch selecting `maps to
    // stopped not natural end`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let e = exec(vec![r#"{"type":"turn_end","reason":"timed_out"}"#.into()]);
    let state = e
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(Arc::new(RecordingCoordinator::default())),
        )
        .await
        .unwrap();
    assert!(
        matches!(state, RunState::Ended(EndCause::Stopped(_))),
        "a timed-out Run must end Stopped, got {state:?}"
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

// ── R7: ACP mid-switch relaunches the CLI per Run ────────────────────────────

#[tokio::test]
async fn acp_relaunches_the_cli_every_run_so_a_model_switch_takes_effect() {
    // Causes: the fixtures below establish `acp relaunches the cli every run so a model switch
    // takes effect` with the concrete inputs, state, dependencies, and failure triggers used by
    // this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `acp relaunches the cli every run so a model switch takes effect` is one
    // independent branch selecting `all output, state, side-effect, error, and terminal assertions
    // below hold together`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSource(Arc<AtomicUsize>);
    #[async_trait]
    impl AgentChannelSource for CountingSource {
        async fn open(
            &self,
            _a: &RunActivation,
            _context: &RuntimeRunContext,
            _door: ChannelOpenDoor,
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
            Ok(fixture_session(
                Box::new(ours),
                Arc::new(FakeProcess),
                awaken_protocol_acp::Codec::Newline,
                None,
            ))
        }
    }

    let opens = Arc::new(AtomicUsize::new(0));
    let exec = AcpRunExecutor::new(Arc::new(CountingSource(opens.clone())));
    assert_eq!(exec.model_switch(), ModelSwitch::Relaunch);

    // Two Runs → two launches: an ACP Thread relaunches its CLI each Run, which
    // is how a re-staged model takes effect (R7).
    let commit = Arc::new(RecordingCoordinator::default());
    exec.execute(
        activation(),
        RuntimeRunContext::new().with_commit(commit.clone()),
    )
    .await
    .unwrap();
    exec.execute(activation(), RuntimeRunContext::new().with_commit(commit))
        .await
        .unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 2);
}

/// A host model resolver that returns fixed coordinates (stands in for the
/// config-plane + vault lookup).
struct FixedModel(ResolvedModel);
#[async_trait]
impl LaunchResolver for FixedModel {
    async fn model(
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

#[tokio::test]
async fn projecting_source_plans_launch_from_resolved_model_and_host_env() {
    let cli = *acp_cli("claude").expect("claude in the catalog");
    let resolver = Arc::new(FixedModel(ResolvedModel::Managed {
        base_url: "https://api.kimi.com/coding/".to_string(),
        model: "kimi-k2".to_string(),
        process_secret: Some(ProcessSecretRequirement::new("lease://projected-host")),
        credential_artifact: None,
        acp: None,
        provider_server_tools: Vec::new(),
    }));
    // The resolver supplies the config-home path as non-secret per-run env.
    let source = ProjectingChannelSource::new(cli, resolver);
    let launch = source
        .plan(&activation(), &RuntimeRunContext::new())
        .await
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
/// its ACP plugin config (what the host's `overlay_acp_mcp` produces) makes
/// `open()` stage it as a Session server and `drive()` inject it into the
/// `session/new` request — the D5 seam. The fake agent echoes that it saw the server and
/// that no credential channel is present, proving the whole
/// host→plugin_config→session/new chain is route-only. Gated on `real-acp`: only
/// the official codec serializes `mcpServers` into `session/new`.
#[cfg(feature = "real-acp")]
#[tokio::test]
#[cfg(unix)]
async fn open_and_drive_inject_the_mcp_server_into_session_new_for_an_acp_session_cli() {
    // The canonical Claude row with a cheap JSON-RPC echo executable override.
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
            provider_server_tools: Vec::new(),
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
            provider_server_tools: Vec::new(),
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

/// The ACP `acp_session_id` is carried across the per-Run relaunch loop (R7): the id
/// negotiated on Run 1's `session/new` is threaded into Run 2's config, so the
/// relaunched CLI is resumed via `session/load` with the SAME id (context survives)
/// rather than starting fresh. A fake ACP CLI (JSON-RPC, shell builtins) advertises
/// `loadSession` and, per Run, reports which session verb it received — `new` (Run 1,
/// no prior id) or `load-s1` (Run 2, resumed with the carried id `s1`). Each relaunch
/// is a fresh child (the shell var resets), so the only thing that can carry `s1` into
/// Run 2 is the executor threading it through `config.session_id`. Gated on `real-acp`:
/// only the official codec negotiates/loads a session id (the newline stand-in leaves it
/// `None`).
#[cfg(feature = "real-acp")]
#[tokio::test]
#[cfg(unix)]
async fn acp_session_id_is_carried_across_the_per_run_relaunch() {
    // Causes: the fixtures below establish `acp session id` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `is carried across the per run relaunch` and every asserted
    // state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `acp session id` is one independent branch selecting `is carried across
    // the per run relaunch`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    // id:1 initialize (advertise loadSession) · id:2 session/new|load · id:3 prompt.
    // The id:2 request distinguishes the verb by whether the carried id `s1` is present
    // in it: Run 1's session/new has none (→ `new`, returns sessionId s1); Run 2's
    // session/load carries `s1` (→ `load-s1`, empty result). The Run's agent message
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
            provider_server_tools: Vec::new(),
        })),
    ));
    let e = AcpRunExecutor::new(source);

    // One queued steer forces exactly one relaunch → a second Run (without it the Run
    // ends after Run 1 and never relaunches, so the carry is never exercised).
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
        texts.iter().any(|t| t == "run:new"),
        "Run 1 opened a fresh session via session/new; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "run:load-s1"),
        "Run 2 resumed via session/load carrying the id `s1` — acp_session_id survived \
         the per-Run relaunch; got {texts:?}"
    );
}

/// A durable ACP pause survives executor/process replacement. The replacement
/// validates the committed ticket, restores the protocol session id from thread
/// state, and issues `session/load` before continuing the SAME Run.
#[cfg(feature = "real-acp")]
#[tokio::test]
async fn paused_run_resumes_after_executor_replacement_with_the_committed_session_id() {
    // Causes: the fixtures below establish `paused run` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `resumes after executor replacement with the committed session
    // id` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `paused run` is one independent branch selecting `resumes after executor
    // replacement with the committed session id`; a multi-row decision table is not applicable, and
    // sibling tests own alternate causes.
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
            provider_server_tools: Vec::new(),
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
    assert!(texts.iter().any(|text| text == "run:new"));
    assert!(
        texts.iter().any(|text| text == "run:load-s1"),
        "replacement must restore the durable id and use session/load; got {texts:?}"
    );
    assert!(
        committed
            .resume_ticket_for(&RunId("run-1".into()))
            .is_none(),
        "terminal resume consumes the active ticket"
    );
}

#[tokio::test]
async fn projecting_source_reads_the_cli_compact_window_from_config() {
    let cli = *acp_cli("claude").expect("claude in the catalog");
    let resolver = Arc::new(FixedModel(ResolvedModel::Managed {
        base_url: "u".to_string(),
        model: "m".to_string(),
        process_secret: None,
        credential_artifact: None,
        acp: None,
        provider_server_tools: Vec::new(),
    }));
    let source = ProjectingChannelSource::new(cli, resolver);

    // The run carries an ACP-scoped compaction window in plugin_config.
    let mut act = activation();
    act.snapshot.resolved_spec.plugin_config.insert(
        "acp".to_string(),
        serde_json::json!({ "compact_window": 262144 }),
    );
    let launch = source
        .plan(&act, &RuntimeRunContext::new())
        .await
        .expect("plan");
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

// ── Cancellation, multi-Run usage, and mid-loop relaunch failure ─────────────

/// A pre-cancelled Run token ends the Run `Cancelled` (lease revocation / interrupt
/// at the executor level): the agent hangs with no `turn_end`, so the only way the
/// Run ends is the supervisor's cancel branch → `EndCause::Cancelled`.
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
            _door: ChannelOpenDoor,
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
                // Hold the stream open (never `turn_end`), so only cancel ends the Run.
                std::future::pending::<()>().await;
                drop(theirs);
            });
            Ok(fixture_session(
                Box::new(ours),
                Arc::new(FakeProcess),
                awaken_protocol_acp::Codec::Newline,
                None,
            ))
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

/// Cause/effect design: C1 every launched ACP Run reports usage 10/20/5; C2 one
/// external live-inbox message forces a second launch. Effect E1: the single
/// committed `__usage` Set totals 20/40/10 rather than overwriting the first Run.
/// Decision rule U1=C1+C2=>E1; a single launch is the non-accumulating base row.
#[tokio::test]
async fn usage_accumulates_across_relaunched_runs() {
    // Causes: the fixtures below establish `usage` with the concrete inputs, state, dependencies,
    // and failure triggers used by this case.
    // Effects: the observable result `accumulates across relaunched runs` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    use awaken_agent_contract::agent::state::Action;
    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};
    use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage, TokenUsage};

    // Every launched Run (the ScriptedSource re-emits its frames on each open) reports
    // the same usage; two Runs → the tally must double.
    let e = exec(vec![
        r#"{"type":"message","text":"Run"}"#.into(),
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
        "usage is summed across the two relaunched Runs, not overwritten"
    );
}

/// Cause/effect design for cumulative usage across independent executions.
/// C1 a first ACP Run commits 10/20/5; C2 a fresh executor (the process-restart
/// shape) rebuilds the same Thread prefix through `CommittedThreadView`; C3 a
/// distinct second Run reports the same delta; C4 an ambiguous retry derives a
/// commit twice from one unchanged recovery prefix. Effects: E1 C2 retains C1;
/// E2 C3 commits 20/40/10 through the sole `ThreadUsageKey`; E3 C4 produces an
/// identical command, so the existing operation-id/payload-hash fence can return
/// its original receipt without a second append. Constraints: the local Host's
/// per-Thread execution lock serializes read/commit, while a remote Worker uses
/// `expected_thread_version` plus stable operation id; this executor must not
/// create a second counter or retry authority. Decision rules: U2=C1+C2+C3=>E1+E2;
/// U3=C1+C2+C4=>E3; a stale concurrent contender is rejected by the commit fence
/// before state append and is therefore not simulated as a successful commit here.
#[tokio::test]
async fn usage_accumulates_across_independent_runs_with_a_rebuilt_reader() {
    // Causes: the fixtures below establish `usage` with the concrete inputs, state, dependencies,
    // and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage, TokenUsage};

    let frames = || {
        vec![
            r#"{"type":"message","text":"Run"}"#.into(),
            r#"{"type":"usage","prompt_tokens":10,"completion_tokens":20,"cache_read_tokens":5}"#
                .into(),
            r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
        ]
    };
    let coordinator = Arc::new(RecordingCoordinator::default());
    exec(frames())
        .execute(
            activation(),
            RuntimeRunContext::new()
                .with_commit(coordinator.clone())
                .with_reader(coordinator.clone()),
        )
        .await
        .expect("first ACP Run");

    let mut second = activation();
    second.run_id = RunId("run-2".into());
    second.input = vec![Message::text(
        MessageId("u2".into()),
        Role::User,
        "do it again",
    )];
    let retry_context = || {
        RuntimeRunContext::new()
            .with_commit(coordinator.clone())
            .with_reader(coordinator.clone())
    };
    let delta = TokenUsage {
        prompt_tokens: 10,
        completion_tokens: 20,
        cache_read_tokens: 5,
        cache_creation_tokens: 0,
    };
    assert_eq!(
        usage_state(&retry_context(), &second.thread_id, &delta, "model").unwrap(),
        usage_state(&retry_context(), &second.thread_id, &delta, "model").unwrap(),
        "U3/E3 the same recovery prefix derives an identical retry payload",
    );

    exec(frames())
        .execute(second, retry_context())
        .await
        .expect("fresh executor continues the committed Thread");

    let commands = coordinator.committed_state(&ThreadId("thread-1".into()));
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.key.0 == THREAD_USAGE_STATE_KEY)
            .count(),
        2,
        "U2 each distinct Run commits one cumulative Set",
    );
    assert_eq!(
        ThreadUsage::from_committed_state(&commands).by_model["model"],
        TokenUsage {
            prompt_tokens: 20,
            completion_tokens: 40,
            cache_read_tokens: 10,
            cache_creation_tokens: 0,
        },
        "U2/E2 a fresh executor extends rather than replaces committed usage",
    );
}

/// A relaunch that fails to reopen the channel mid-run (the `BoundaryOutcome::Continue`
/// branch) commits a classified terminal failure — distinct from the initial-open
/// fault. The first open scripts a Run; a steer forces a relaunch; the second open
/// fails, so the Run ends `Error` after exactly two open attempts.
#[tokio::test]
async fn a_relaunch_open_failure_mid_run_classifies_and_ends() {
    // Causes: the fixtures below establish `a relaunch open failure mid run classifies and ends`
    // with the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Coverage rationale: `a relaunch open failure mid run classifies and ends` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_runtime_contract::live_inbox::{LiveInbox, MessageOrigin};

    struct FlakySource(Arc<AtomicUsize>);
    #[async_trait]
    impl AgentChannelSource for FlakySource {
        async fn open(
            &self,
            _a: &RunActivation,
            _context: &RuntimeRunContext,
            _door: ChannelOpenDoor,
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
                    r#"{"type":"message","text":"Run"}"#,
                    r#"{"type":"turn_end","reason":"natural_end"}"#,
                ] {
                    let _ = theirs.write_all(f.as_bytes()).await;
                    let _ = theirs.write_all(b"\n").await;
                    let _ = theirs.flush().await;
                }
            });
            Ok(fixture_session(
                Box::new(ours),
                Arc::new(FakeProcess),
                awaken_protocol_acp::Codec::Newline,
                None,
            ))
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
        "the first open ran the Run; the relaunch attempted a second open and failed"
    );
}

// ── Backend matrix (P1-#4): every catalog CLI drives, not just claude ─────────
//
// The per-row *projection* is covered exhaustively in `acp_cli.rs`
// (`every_cli_*`), and claude is driven end-to-end over the real ACP codec
// (`open_and_drive_inject_*`). What was missing is the parity across the whole
// catalog *through the executor's own launch seam*: that EVERY row
// (claude/codex/gemini/opencode) projects a launchable process through
// `ProjectingChannelSource` and drives a plain Run to a committed reply. This is
// the hermetic analogue of awaken-next's `e2e_external_cli_acp` multi-CLI matrix
// — no real binaries, so the drive path's row-agnosticism is pinned in CI.

/// A model resolver for the matrix: fixed coordinates, no per-run env (a plain
/// Run declares no MCP, so no config-home is needed — keeping it row-agnostic).
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
            .ok_or_else(|| awaken_provisioning_contract::SandboxError::not_found("unknown lease"))
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> std::result::Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        (reference == "lease://matrix")
            .then(|| b"matrix-secret".to_vec())
            .ok_or_else(|| awaken_provisioning_contract::SandboxError::not_found("unknown lease"))
    }

    async fn write_back(
        &self,
        _target: awaken_provisioning_contract::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> std::result::Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "not supported",
        ))
    }
}

#[async_trait]
impl LaunchResolver for MatrixModel {
    async fn model(
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
            provider_server_tools: Vec::new(),
        })
    }

    fn secret_broker(&self) -> Option<Arc<dyn awaken_provisioning_contract::SecretBroker>> {
        Some(Arc::new(MatrixSecretBroker))
    }
}

#[tokio::test]
async fn every_backend_row_projects_a_launchable_process_through_the_source() {
    // The source seam (not just `AcpCli::project`) must be row-agnostic: for each
    // catalog CLI, `ProjectingChannelSource::plan` yields the row's own command as
    // argv[0] and delivers the resolved model under that row's env keys.
    let reference_cli = &known_acp_clis()[0];
    let model = MatrixModel::for_cli(reference_cli)
        .model(&activation(), &RuntimeRunContext::new())
        .await
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
        let planned = source.plan(&activation(), &RuntimeRunContext::new()).await;
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

/// Cause/effect design for every ACP catalog row: C1 `model_delivery` is absent;
/// C2 it is present and the row launches the hermetic `PONG_ECHO` through the
/// production projection/spawn/official-codec seam. Effects: E1 C1 fails before
/// spawn with `credential_driver_required`; E2 C2 ends naturally and commits
/// `pong`. Decision table: R1=C1=>E1; R2=C2=>E2. This covers the same path for
/// claude/codex/gemini/opencode without substituting a codec fake.
#[cfg(feature = "real-acp")]
#[tokio::test]
#[cfg(unix)]
async fn every_backend_row_drives_a_plain_run_to_a_committed_reply() {
    // Causes: the fixtures below establish `every backend row` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the current fenced attempt and committed context are authoritative;
    // remote protocol state cannot become a parallel Run or transcript truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    for row in known_acp_clis() {
        if row.model_delivery.is_none() {
            let error = ProjectingChannelSource::new(*row, Arc::new(MatrixModel::for_cli(row)))
                .plan(&activation(), &RuntimeRunContext::new())
                .await
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
        act.snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                "prov",
                "model",
                format!("acp:{}", row.id),
            ));

        let coord = Arc::new(RecordingCoordinator::default());
        let state = exec
            .execute(act, RuntimeRunContext::new().with_commit(coord.clone()))
            .await
            .unwrap_or_else(|e| panic!("{}: drive failed: {e:?}", row.id));

        assert_eq!(
            state,
            RunState::Ended(EndCause::NaturalEnd),
            "{}: a plain Run ends naturally",
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

#[tokio::test]
async fn canonical_mcp_ticket_and_transcript_survive_wire_name_changes() {
    // Causes: dot/SDK/canonical name x legacy/canonical persisted ticket x
    // changed arguments x consumed grant. R1 same semantic tool and arguments
    // => one allow and canonical transcript; R2 changed args => ask; R3 repeat
    // => ask. Historical dotted tickets remain readable without a second path.
    struct Ask;
    #[async_trait]
    impl PermissionResolver for Ask {
        async fn resolve(&self, _: &PermissionAsk) -> PermissionVerdict {
            PermissionVerdict::Await {
                correlation_id: "again".into(),
            }
        }
    }
    for stored in ["write", "mcp.awaken_session.write"] {
        for wire in [
            "write",
            "mcp.awaken_session.write",
            "mcp__awaken_session__write",
        ] {
            let decision = PermissionResume {
                call_id: "old".into(),
                tool_id: stored.into(),
                arguments: serde_json::json!({"path":"a"}),
                allow: true,
            };
            let resolver = ResumedPermissionResolver::new(&Ask, &decision, None);
            let mut ask = PermissionAsk {
                call_id: "new".into(),
                tool: wire.into(),
                arguments: serde_json::json!({"path":"b"}),
            };
            assert!(matches!(
                resolver.resolve(&ask).await,
                PermissionVerdict::Await { .. }
            ));
            ask.arguments = decision.arguments.clone();
            assert_eq!(resolver.resolve(&ask).await, PermissionVerdict::Allow);
            assert!(matches!(
                resolver.resolve(&ask).await,
                PermissionVerdict::Await { .. }
            ));
            let mut messages = vec![];
            ensure_pending_tool_use(&RunId("run".into()), &mut 0, &mut messages, 0, &ask).unwrap();
            assert!(
                matches!(&messages[0].content[0], ContentBlock::ToolUse { name, .. } if name == "write")
            );
        }
    }
}

struct OwnedContinuationProbe {
    resumes: Mutex<Vec<ResumeCommand>>,
    recoveries: AtomicUsize,
    fail: bool,
}

#[async_trait]
impl awaken_runtime_contract::resume::OwnedToolContinuation for OwnedContinuationProbe {
    fn tool_await_reason(&self, tool_id: &str) -> Option<ToolAwaitReason> {
        match tool_id {
            "write" => Some(ToolAwaitReason::Permission),
            "desktop_search" => Some(ToolAwaitReason::ClientExecution),
            _ => None,
        }
    }

    async fn resume(
        &self,
        _activation: RunActivation,
        command: ResumeCommand,
        _context: RuntimeRunContext,
    ) -> Result<RunState> {
        self.resumes.lock().unwrap().push(command);
        if self.fail {
            Err(Error::Execution("owned-continuation-probe".into()))
        } else {
            Ok(RunState::Awaiting)
        }
    }

    async fn recover(
        &self,
        _activation: RunActivation,
        _context: RuntimeRunContext,
    ) -> Result<RunState> {
        self.recoveries.fetch_add(1, Ordering::SeqCst);
        Err(Error::Execution("owned-recovery-probe".into()))
    }
}

#[tokio::test]
async fn owned_permission_uses_the_host_port_before_opening_an_acp_process() {
    // Causes: O1 exact owned/unowned tool, O2 port returns Awaiting/error,
    // O3 allow/deny. Effects: owned decisions reach the port unchanged and no
    // CLI is opened; errors propagate without a synthetic terminal receipt.
    // Unowned tools retain the original ACP path and never enter the port.
    // | Rule | owned | port outcome | decision | port calls | CLI opens |
    // | P1   | yes   | Awaiting     | allow/deny | 1        | 0 |
    // | P2   | yes   | error        | allow/deny | 1        | 0 |
    // | P3   | no    | unused       | allow/deny | 0        | 1 |
    // Kernel effect/precommit/reclaim semantics belong to Runtime/Host tests;
    // this probe deliberately implements no second tool execution algorithm.
    for owned in [true, false] {
        for fail in [true, false] {
            for allow in [true, false] {
                let committed = Arc::new(RecordingCoordinator::default());
                let activation = activation_without_session_home();
                let tool_id = if owned {
                    "write"
                } else {
                    "mcp__external__write"
                };
                let arguments = serde_json::json!({"file_path":"/workspace/output.txt",
                    "content":"é中文🙂\\\"\n".repeat(3000)});
                let ticket = ResumeTicket::new(
                    "owned-decision",
                    activation.run_id.clone(),
                    activation.thread_id.clone(),
                    activation.snapshot.id.0.clone(),
                    activation.snapshot.fingerprint.0.clone(),
                    AwaitTarget::ToolCall {
                        authority: if owned {
                            awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch
                        } else {
                            awaken_agent_contract::agent::awaiting::ToolCallAuthority::RunAttemptExecutor
                        },
                        reason: ToolAwaitReason::Permission,
                        call_id: "owned-call".into(),
                        tool: PendingTool {
                            tool_id: tool_id.into(),
                            arguments,
                        },
                    },
                );
                super::commit::commit(
                    &RuntimeRunContext::new().with_commit(committed.clone()),
                    Vec::new(),
                    None,
                    &activation.thread_id,
                    RunDisposition::awaiting(ticket.clone()),
                    vec![Message::new(
                        MessageId::assistant(&activation.run_id, 0),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            "owned-call",
                            tool_id,
                            ticket.pending_tool().unwrap().arguments.clone(),
                        )],
                    )],
                    Vec::new(),
                )
                .await
                .unwrap();
                // P1/P2 owned => the exact batch validates the ticket; P3
                // external => no competing Runtime batch is committed.
                let batch = awaken_runtime_contract::ActiveToolBatch::load(&Store::rebuild(
                    &committed.committed_state(&ticket.thread_id),
                ))
                .unwrap();
                assert_eq!(batch.is_some(), owned);
                if let Some(batch) = batch {
                    batch
                        .validate_awaiting_ticket(&ticket.thread_id, &ticket)
                        .unwrap();
                }
                let decision = if allow {
                    ResumeResult::allow()
                } else {
                    ResumeResult::deny(Some("denied by user".into()))
                };
                let command = ResumeCommand::from_ticket(&ticket, decision, 0)
                    .with_operation_id("owned-operation");
                let probe = Arc::new(OwnedContinuationProbe {
                    resumes: Mutex::new(Vec::new()),
                    recoveries: AtomicUsize::new(0),
                    fail,
                });
                let (executor, opens, _, _) = counting_executor();
                let result = executor
                    .with_owned_tool_continuation(Some(probe.clone()))
                    .resume(
                        activation,
                        command.clone(),
                        RuntimeRunContext::new()
                            .with_commit(committed.clone())
                            .with_reader(committed.clone()),
                    )
                    .await;
                if owned {
                    assert_eq!(*probe.resumes.lock().unwrap(), vec![command]);
                    assert_eq!(opens.load(Ordering::SeqCst), 0);
                    assert!(committed.resume_ticket_for(&ticket.run_id).is_some());
                    if fail {
                        assert!(
                            result
                                .unwrap_err()
                                .to_string()
                                .contains("owned-continuation-probe")
                        );
                    } else {
                        assert_eq!(result.unwrap(), RunState::Awaiting);
                    }
                } else {
                    assert!(probe.resumes.lock().unwrap().is_empty());
                    assert_eq!(opens.load(Ordering::SeqCst), 1);
                    assert_eq!(result.unwrap(), RunState::Ended(EndCause::NaturalEnd));
                }
                assert_eq!(probe.recoveries.load(Ordering::SeqCst), 0);
            }
        }
    }
}

#[tokio::test]
async fn owned_receipt_projection_reuses_committed_facts_across_runs_without_sdk_acknowledgements()
{
    // R1 Runtime-minted result + Assistant ToolUse + owned tool => include exact
    // success/error and data. R2 SDK-minted result => exclude (already SDK history).
    // R3 unowned tool, unpaired result or User-authored ToolUse => exclude.
    // R4 later Run / repeated projection => same ordered receipts, zero commits;
    // R5 two valid Assistant occurrences reuse one provider call id => each
    // batch-scoped result pairs with its own tool, while legacy ids still read.
    // R6 forged v2-looking batch coordinate => no Runtime-owned receipt.
    // A finalized ToolBatch publishes every result before the next Assistant
    // inference; a valid old result cannot first appear after the next use.
    // loss before external delivery cannot erase a previously committed result.
    // Context is a read-only projection, never a fabricated SDK ToolResult or
    // durable prompt copy. No tool-result text is interpreted as instructions.
    let committed = Arc::new(RecordingCoordinator::default());
    let probe = OwnedContinuationProbe {
        resumes: Mutex::new(Vec::new()),
        recoveries: AtomicUsize::new(0),
        fail: false,
    };
    let mut activation = activation_without_session_home();
    let mut messages = Vec::new();
    for (call, tool, role, sdk, is_error) in [
        ("ok", "write", Role::Assistant, false, false),
        ("failed", "write", Role::Assistant, false, true),
        ("sdk", "write", Role::Assistant, true, false),
        (
            "external",
            "mcp__external__write",
            Role::Assistant,
            false,
            false,
        ),
        ("user", "write", Role::User, false, false),
    ] {
        messages.push(Message::new(
            MessageId(format!("use-{call}")),
            role,
            vec![ContentBlock::tool_use(call, tool, serde_json::json!({}))],
        ));
        messages.push(Message::new(
            if sdk {
                MessageId(format!("sdk-result-{call}"))
            } else {
                MessageId::tool_result(call)
            },
            Role::Tool,
            vec![ContentBlock::tool_result_with_error(
                call,
                vec![ContentBlock::text(format!(
                    "receipt {call}: é中文🙂\nquoted tool data"
                ))],
                is_error,
            )],
        ));
    }
    messages.push(Message::new(
        MessageId::tool_result("unpaired"),
        Role::Tool,
        vec![ContentBlock::tool_result(
            "unpaired",
            vec![ContentBlock::text("unpaired data")],
        )],
    ));
    messages.push(Message::new(
        MessageId::assistant(&RunId("forged-run".into()), 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "forged",
            "write",
            serde_json::json!({}),
        )],
    ));
    messages.push(Message::new(
        MessageId::tool_result_for_batch("not-a-batch", "forged"),
        Role::Tool,
        vec![ContentBlock::tool_result(
            "forged",
            vec![ContentBlock::text("must not enter ACP context")],
        )],
    ));
    for (run, tool, text) in [
        ("repeat-first", "write", "first owned result"),
        ("repeat-second", "desktop_search", "second owned result"),
    ] {
        messages.push(Message::new(
            MessageId::assistant(&RunId(run.into()), 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "same-provider-id",
                tool,
                serde_json::json!({}),
            )],
        ));
        messages.push(Message::new(
            MessageId::tool_result_for_batch(&format!("tool-batch:{run}:0"), "same-provider-id"),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "same-provider-id",
                vec![ContentBlock::text(text)],
            )],
        ));
    }
    committed
        .commit(ThreadCommit::assemble(
            activation.thread_id.clone(),
            RunDisposition::running(activation.run_id.clone()),
            true,
            messages,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let context = RuntimeRunContext::new()
        .with_reader(committed.clone())
        .with_commit(committed.clone());
    let count = committed.commits.lock().unwrap().len();
    let original =
        crate::tool_continuation::owned_tool_result_context(&probe, &activation, &context)
            .unwrap()
            .unwrap()
            .text_content();
    let (_, json) = original.split_once('\n').unwrap();
    let receipts: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
    assert_eq!(receipts.len(), 4);
    assert_eq!(receipts[0]["call_id"], "ok");
    assert_eq!(receipts[0]["is_error"], false);
    assert_eq!(receipts[1]["call_id"], "failed");
    assert_eq!(receipts[1]["is_error"], true);
    assert_eq!(
        receipts[1]["content"][0]["text"],
        "receipt failed: é中文🙂\nquoted tool data"
    );
    assert_eq!(receipts[2]["tool"], "write", "R5 exact first occurrence");
    assert_eq!(receipts[2]["content"][0]["text"], "first owned result");
    assert_eq!(
        receipts[3]["tool"], "desktop_search",
        "R5 exact second occurrence"
    );
    assert_eq!(receipts[3]["content"][0]["text"], "second owned result");
    activation.run_id = RunId("later-run".into());
    for _ in 0..2 {
        let projected =
            crate::tool_continuation::owned_tool_result_context(&probe, &activation, &context)
                .unwrap()
                .unwrap();
        assert_eq!(projected.text_content(), original);
        assert_eq!(projected.role, Role::System);
    }
    assert_eq!(committed.commits.lock().unwrap().len(), count);
    assert!(probe.resumes.lock().unwrap().is_empty());
    assert_eq!(probe.recoveries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn owned_recovery_selects_only_the_current_runs_exact_execution_owner() {
    // C1 current/other Run; C2 owned/external/mixed batch; C3 open/finalized;
    // C4 durable reader+committer. Effects: R5 current+owned+open -> one recovery
    // before ACP; R6 finalized, R7 other Run, R8 external -> no recovery;
    // R9 mixed -> fail closed with no partial dispatch; R10 missing either
    // durable port -> reject before opening ACP. Recovery errors propagate and
    // do not forge a receipt. Runtime tests own actual NeverReplay execution.
    for case in [
        "owned",
        "finalized",
        "other-run",
        "external",
        "mixed",
        "no-reader",
        "no-commit",
    ] {
        let committed = Arc::new(RecordingCoordinator::default());
        let activation = activation_without_session_home();
        let owner = if case == "other-run" {
            RunId("other-run".into())
        } else {
            activation.run_id.clone()
        };
        let mut calls = vec![ToolCall {
            call_id: "recover-call".into(),
            tool_id: if case == "external" {
                "mcp__external__write".into()
            } else {
                "write".into()
            },
            arguments: serde_json::json!({}),
        }];
        if case == "mixed" {
            calls.push(ToolCall {
                call_id: "external-call".into(),
                tool_id: "mcp__external__write".into(),
                arguments: serde_json::json!({}),
            });
        }
        let mut batch = awaken_runtime_contract::ToolBatch::for_step(
            owner.clone(),
            0,
            calls.into_iter().map(|call| (call, Default::default())),
        )
        .unwrap();
        if case == "finalized" {
            batch.mark_executing("recover-call").unwrap();
            batch
                .complete(awaken_runtime_contract::tool::ToolOutput::ok(
                    "recover-call",
                    "done",
                ))
                .unwrap();
            batch.finalize().unwrap();
        }
        committed
            .commit(ThreadCommit::assemble(
                activation.thread_id.clone(),
                RunDisposition::running(owner),
                true,
                Vec::new(),
                vec![awaken_runtime_contract::ActiveToolBatch::write(&Some(
                    batch,
                ))],
                Vec::new(),
            ))
            .await
            .unwrap();
        let probe = Arc::new(OwnedContinuationProbe {
            resumes: Mutex::new(Vec::new()),
            recoveries: AtomicUsize::new(0),
            fail: true,
        });
        let (executor, opens, _, _) = counting_executor();
        let executor = executor.with_owned_tool_continuation(Some(probe.clone()));
        let mut context = RuntimeRunContext::new();
        if case != "no-reader" {
            context = context.with_reader(committed.clone());
        }
        if case != "no-commit" {
            context = context.with_commit(committed.clone());
        }
        let result = executor
            .prepare_owned_tools(&activation, &mut context)
            .await;
        match case {
            "owned" => {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("owned-recovery-probe")
                );
                assert_eq!(probe.recoveries.load(Ordering::SeqCst), 1);
            }
            "mixed" | "no-reader" | "no-commit" => {
                assert!(result.is_err());
                assert_eq!(probe.recoveries.load(Ordering::SeqCst), 0);
            }
            _ => {
                assert_eq!(result.unwrap(), None);
                assert_eq!(probe.recoveries.load(Ordering::SeqCst), 0);
            }
        }
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert!(probe.resumes.lock().unwrap().is_empty());
        assert_eq!(committed.commits.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn absent_step_limit_does_not_cap_opaque_acp_calls() {
    // A1 absent ceiling + >16 distinct calls -> accepted; A2 refinements remain
    // uncharged. The finite rejection rule stays in opaque_acp_tool_loop above.
    let mut appender =
        CollectingAppender::new(RunId("uncapped".into()), "0".into(), 0, None, None, None)
            .with_tool_call_limit(None);
    for seq in 1..=25 {
        appender
            .append(
                seq,
                &AcpProjectedEvent::ToolCall {
                    id: format!("call-{seq}"),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            )
            .await
            .expect("A1 no implicit ceiling");
    }
    assert_eq!(appender.messages.len(), 25);
    assert_eq!(appender.tool_call_count, 25);
}
