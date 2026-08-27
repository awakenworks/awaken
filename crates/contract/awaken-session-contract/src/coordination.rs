//! Neutral Session-owned Agent coordination vocabulary.
//!
//! A coordinated child is still an ordinary Thread containing ordinary Runs.
//! These values describe admission and a derived relationship; they are not a
//! Subagent aggregate, registry, mailbox, or persistence model.

use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{RunLifecycleEventKind, classify_run_lifecycle_event};
use awaken_runtime_contract::ExecutableAgentSnapshot;
use serde::{Deserialize, Serialize};

/// One exact member of the roster frozen by the coordinator Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAgentRosterEntry {
    pub agent_id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Model-independent target of one coordination command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionAgentTarget {
    Spawn { agent_id: String },
    ExistingThread { thread_id: ThreadId },
}

/// Trusted coordinates supplied by the Runtime tool boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAgentMessageCommand {
    pub session_id: String,
    pub source_thread_id: ThreadId,
    pub source_run_id: RunId,
    pub source_call_id: String,
    pub operation_id: String,
    pub target: SessionAgentTarget,
    pub message: String,
}

/// Accepted identity returned to the model-facing builtin adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAgentMessageReceipt {
    pub thread_id: ThreadId,
}

/// Definitive Session-root decision for one exact durable Run reservation.
/// Dependency failure remains a run error; this value is safe for the
/// claim-fenced dispatch row to persist as its admission outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunActivityAdmission {
    Admitted { session_activity_epoch: u64 },
    Rejected,
}

/// Whether reservation repair may create a fresh Session activity receipt.
/// Cancellation uses `RecoverOnly`: deleting a never-admitted intent is valid,
/// but an already-committed receipt must first follow the ordinary cancellation
/// settlement path so its epoch cannot leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunActivityAdmissionMode {
    RecoverOrAdmit,
    RecoverOnly,
}

/// Stable activity-operation identity for a self-affine Session Run. The Run
/// id is already the canonical dispatch identity, so exact crash recovery
/// reuses one Session receipt without another registry or counter.
#[must_use]
pub fn session_run_activity_operation_id(session_id: &str, run_id: &RunId) -> String {
    format!(
        "session-run-activity:{}",
        crate::stable_fingerprint(&("session-run-activity-v1", session_id, run_id.0.as_str(),))
    )
}

/// Canonical logical Thread identity for one accepted spawn operation. The
/// ordinary Thread remains the storage unit; this helper only prevents the
/// admission and committed-link projector from maintaining parallel codecs.
#[must_use]
pub fn coordinated_thread_id(
    session_id: &str,
    source_run_id: &RunId,
    operation_id: &str,
) -> ThreadId {
    ThreadId(format!(
        "sthr_{}",
        crate::stable_fingerprint(&(
            "managed-coordinated-thread-v1",
            session_id,
            source_run_id.0.as_str(),
            operation_id,
        ))
    ))
}

/// Whether an admitted Run creates a coordinated Thread or follows up on an
/// already-derived one. Spawn input belongs to the ordinary activation;
/// follow-up input belongs to the existing cross-Thread message path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatedRunIntent {
    Spawn,
    FollowUp,
}

/// Whether the latest committed Run permanently closes an ordinary coordinated
/// Thread to follow-up input.
///
/// This is a projection of the existing Run lifecycle authority, not another
/// Thread disposition cell. Completed and externally cancelled Runs leave the
/// logical Thread reusable; a `Failed` lifecycle classification is absorbing
/// because no later Run may be admitted after it.
#[must_use]
pub fn coordinated_thread_failed(state: &RunState) -> bool {
    classify_run_lifecycle_event(state, None) == RunLifecycleEventKind::Failed
}

/// Durable-dispatch input produced only after Session policy has admitted it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoordinatedRunCommand {
    pub intent: CoordinatedRunIntent,
    pub session_id: String,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub operation_id: String,
    pub snapshot: ExecutableAgentSnapshot,
    pub message: String,
    pub session_activity_epoch: u64,
    pub max_unarchived_threads: usize,
}

/// Trusted committed Session activity boundary delivered by the dispatch
/// settlement choke. A child Awaiting, Cancelled, or Failed boundary settles its
/// activity directly. Only a normally Completed child transfers that same epoch
/// to the deterministic primary report continuation, whose own boundary settles
/// it. A cancellation-requested child also settles directly: the dispatch claim
/// is the authority for that cause, so no transcript text or terminal-cause
/// heuristic participates. No process-local waiter or second activity registry
/// participates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAgentBoundaryCommand {
    pub session_id: String,
    pub source_thread_id: ThreadId,
    pub source_run_id: RunId,
    /// Trusted immutable execution identity copied from the exact claimed
    /// dispatch snapshot. It lets crash settlement validate the child against
    /// the frozen Session roster even when the parent tool-result receipt did
    /// not commit before the process stopped. It is transient provenance, not
    /// a relationship registry or another Agent-publication authority.
    #[serde(default)]
    pub source_agent_id: String,
    pub session_activity_epoch: u64,
    /// Trusted transient provenance copied from the claimed dispatch. This is
    /// not another cancellation state; remote admission verifies it against the
    /// queue row protected by the same claim epoch.
    #[serde(default)]
    pub cancellation_requested: bool,
}

/// Session-approved report handed to the Runtime owner. The message id is the
/// agent-contract provenance minted from `source_run_id`; the Host persists it
/// through the ordinary Outbox/Inbox and root dispatch owners.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionAgentReportContinuation {
    pub session_id: String,
    pub source_thread_id: ThreadId,
    pub source_run_id: RunId,
    pub session_activity_epoch: u64,
    pub message: awaken_agent_contract::agent::message::Message,
}

/// Select the one terminal assistant response that becomes a coordinated
/// child's report to its Session coordinator.
///
/// A child Run may commit several ordinary assistant steps before it ends. The
/// highest complete assistant step is the report boundary; any `MaxTokens`
/// partials for that same step belong to the same response and are returned in
/// transcript order. Earlier steps remain ordinary child-Thread messages. This
/// pure classifier is shared by settlement and protocol projection so report
/// delivery and live-preview eligibility cannot diverge.
#[must_use]
pub fn session_agent_report_messages<'a>(
    messages: &'a [Message],
    run_id: &RunId,
) -> Vec<&'a Message> {
    let Some(report_step) = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .filter_map(|message| message.id.assistant_step_of(run_id))
        .max()
    else {
        return Vec::new();
    };
    messages
        .iter()
        .filter(|message| {
            message.role == Role::Assistant
                && (message.id.assistant_step_of(run_id) == Some(report_step)
                    || message
                        .id
                        .assistant_truncated_response_of(run_id)
                        .is_some_and(|(step, _)| step == report_step))
        })
        .collect()
}

/// Render the text delivered by the report continuation from the exact same
/// response selection used by protocol projection.
#[must_use]
pub fn session_agent_report_text(messages: &[Message], run_id: &RunId) -> String {
    session_agent_report_messages(messages, run_id)
        .into_iter()
        .map(Message::text_content)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Canonical logical Thread target for a Session tool reply.
///
/// This is the neutral lower-boundary topology vocabulary shared by protocol
/// admission, Session activity coordination, and Runtime delivery. Adapters
/// must not maintain a second Primary/child selector for the same command.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionThreadTarget {
    Primary,
    Child(ThreadId),
}

impl SessionThreadTarget {
    /// Resolve the logical Thread id without consulting topology state. Child
    /// membership is still validated by the Session application before use.
    #[must_use]
    pub fn thread_id(&self, session_id: &str) -> ThreadId {
        match self {
            Self::Primary => ThreadId(session_id.to_string()),
            Self::Child(thread_id) => thread_id.clone(),
        }
    }

    #[must_use]
    pub fn child_thread_id(&self) -> Option<&ThreadId> {
        match self {
            Self::Primary => None,
            Self::Child(thread_id) => Some(thread_id),
        }
    }
}

/// Typed reply to a client/permission tool blocking a Session Thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionThreadToolReply {
    Confirm(PermissionDecision),
    Custom {
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    },
    Result {
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionThreadToolReplyCommand {
    pub session_id: String,
    /// Managed `agent.tool_use`/`agent.custom_tool_use` request Event answered by
    /// this reply. Older/internal callers may omit it; current Managed lowering
    /// retains the occurrence identity so a provider call id reused in a later
    /// step is a new ingress operation rather than an exact-retry collision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_request_event_id: Option<String>,
    /// Thread commit version that owned the selected active ticket. This fences
    /// delayed processing when the same Run/call id is reused by a later await.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_thread_version: Option<u64>,
    pub target: SessionThreadTarget,
    /// Exact committed Run selected when the inbound Event was admitted. A
    /// delayed command must never bind a reused tool id to a later Run.
    pub expected_run_id: RunId,
    /// Exact committed ticket correlation selected with `expected_run_id`.
    pub expected_correlation_id: String,
    pub tool_use_id: String,
    pub reply: SessionThreadToolReply,
    /// Optional System input adjacent to this reply in the accepted Event
    /// batch. Runtime freezes it as a stable Role::System Message in the same
    /// pending/resume payload; it is never a Session-level context aggregate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accompanying_system: Option<crate::SessionUserRunSystemInput>,
}

impl SessionThreadToolReplyCommand {
    /// Stable Session mutation identity for this exact committed await and
    /// reply payload. The fence is read from Runtime truth, never accepted from
    /// the public/claimed command, so reuse of a provider call id in a later Run
    /// cannot replay an already-settled activity receipt.
    #[must_use]
    pub fn activity_operation_id(&self) -> String {
        format!(
            "coordinated-reply:{}",
            crate::stable_fingerprint(&(
                "managed-session-thread-reply-activity-v3",
                self.session_id.as_str(),
                self.tool_request_event_id.as_deref(),
                self.expected_thread_version,
                &self.target,
                self.expected_run_id.0.as_str(),
                self.expected_correlation_id.as_str(),
                self.tool_use_id.as_str(),
                &self.reply,
                &self.accompanying_system,
            ))
        )
    }

    /// Stable durable-ingress identity used by the existing Outbox/Inbox and
    /// by Runtime's committed resume receipt. It is derived from the complete
    /// immutable reply command, so exact retry is stable and changed payload is
    /// a distinct operation against the same correlation.
    #[must_use]
    pub fn delivery_operation_id(&self) -> String {
        format!(
            "session-thread-reply-{}",
            crate::stable_fingerprint(&(
                "managed-session-thread-reply-v2",
                self.activity_operation_id(),
            ))
        )
    }
}

impl SessionThreadToolReply {
    #[must_use]
    pub fn client_executed(&self) -> bool {
        matches!(self, Self::Custom { .. } | Self::Result { .. })
    }
}

/// Activity coordinate read while validating the command's exact committed
/// Awaiting ticket. Run/correlation identity lives only on the command so the
/// delayed intent and delivery cannot drift between two copies of that fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionThreadToolReplyFence {
    /// Current durable dispatch activity coordinate, when the Run was admitted
    /// through the Session application. An epochless but exact Session-affine
    /// foreground row is adopted by the first continuation transaction. The
    /// Session root removes an existing coordinate while opening the new epoch
    /// in the same CAS, so staging may safely race a finishing Worker lease.
    pub prior_session_activity_epoch: Option<u64>,
    /// The exact reply was already consumed in committed Runtime truth. The
    /// Session may finish its retained EventBatch without requiring a now-gone
    /// active ticket or restaging the durable input.
    pub already_applied: bool,
}

/// Session-approved delivery handed to the Runtime owner after the root
/// aggregate has durably opened the next activity. This wrapper is internal
/// coordination vocabulary, not a Managed wire shape or another command bus.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionThreadToolReplyDelivery {
    pub command: SessionThreadToolReplyCommand,
    pub fence: SessionThreadToolReplyFence,
    pub session_activity_epoch: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    #[test]
    fn coordinated_thread_identity_is_stable_and_scoped() {
        // Cause/effect graph: C1 Session, C2 source Run, and C3 Runtime operation
        // are identical or one coordinate differs. Effects: E1 exact retry and
        // committed reconstruction choose one Thread; E2 any changed coordinate
        // chooses a distinct Thread. Decision table: T1(all same)->E1;
        // T2(!C1|!C2|!C3)->E2.
        // Constraint/invariant: this domain-separated fingerprint is the sole
        // identity derivation; retries must not allocate a registry entry.
        let run = RunId("run-1".into());
        let exact = coordinated_thread_id("session-1", &run, "operation-1");
        assert_eq!(
            exact,
            coordinated_thread_id("session-1", &run, "operation-1"),
            "T1/E1"
        );
        for changed in [
            coordinated_thread_id("session-2", &run, "operation-1"),
            coordinated_thread_id("session-1", &RunId("run-2".into()), "operation-1"),
            coordinated_thread_id("session-1", &run, "operation-2"),
        ] {
            assert_ne!(changed, exact, "T2/E2");
        }
    }

    #[test]
    fn failed_run_alone_absorbingly_terminates_coordinated_follow_up() {
        use awaken_agent_contract::agent::run::{EndCause, Failure};

        // Cause/effect graph: C1 latest committed state is active/Awaiting or
        // one of the closed EndCause variants. Effects: E1 Failed lifecycle
        // classes permanently reject follow-up; E2 Completed and Cancelled
        // leave the logical Thread reusable; E3 nonterminal states remain
        // admissible for queued input. The lifecycle classifier is the sole
        // authority; this test stores no parallel terminal flag.
        //
        // Decision table:
        // | Rule | Latest state | Lifecycle class | Effect |
        // |---|---|---|---|
        // | F1 | Running/Awaiting | active | E3 accepts |
        // | F2 | NaturalEnd | Completed | E2 accepts |
        // | F3 | Cancelled | Cancelled | E2 accepts |
        // | F4 | MaxSteps/Stopped/Error/Indeterminate | Failed | E1 rejects |
        // Constraint/invariant: the canonical lifecycle classifier is the
        // only terminal authority; coordination stores no parallel failed bit.
        for state in [RunState::Running, RunState::Awaiting] {
            assert!(!coordinated_thread_failed(&state), "F1/E3: {state:?}");
        }
        for state in [
            RunState::Ended(EndCause::NaturalEnd),
            RunState::Ended(EndCause::Cancelled),
        ] {
            assert!(!coordinated_thread_failed(&state), "F2-F3/E2: {state:?}");
        }
        for state in [
            RunState::Ended(EndCause::MaxSteps),
            RunState::Ended(EndCause::Stopped("budget".into())),
            RunState::Ended(EndCause::Error(Failure::StateConflict)),
            RunState::Ended(EndCause::Indeterminate),
        ] {
            assert!(coordinated_thread_failed(&state), "F4/E1: {state:?}");
        }
    }

    #[test]
    fn terminal_report_selects_only_the_last_complete_step_and_its_partials() {
        // Cause/effect graph: C1 one Run has an earlier complete assistant
        // step; C2 its last complete step has a MaxTokens partial and final
        // Message; C3 foreign/non-assistant Messages are interleaved. Effects:
        // E1 C1 stays an ordinary Thread message; E2 C2 is the report in
        // transcript order; E3 C3 is excluded. Constraint: without a complete
        // final Message no report boundary can be inferred.
        //
        // Decision table:
        // | Rule | Complete final | Earlier step | Partial in final | Effect |
        // |---|---|---|---|---|
        // | R1 | yes | yes | yes | E1,E2,E3 |
        // | R2 | no | any | any | empty / fail closed |
        let run = RunId("child-run".into());
        let messages = vec![
            Message::text(MessageId::assistant(&run, 0), Role::Assistant, "ordinary"),
            Message::text(
                MessageId::assistant_truncated(&run, 1, 0),
                Role::Assistant,
                "report part ",
            ),
            Message::text(MessageId("foreign".into()), Role::Assistant, "foreign"),
            Message::text(MessageId::assistant(&run, 1), Role::Assistant, "report end"),
            Message::text(MessageId("tool".into()), Role::Tool, "ignored"),
        ];
        assert_eq!(
            session_agent_report_messages(&messages, &run)
                .into_iter()
                .map(Message::text_content)
                .collect::<Vec<_>>(),
            vec!["report part ", "report end"],
            "R1/E1-E3"
        );
        assert_eq!(
            session_agent_report_text(&messages, &run),
            "report part \nreport end",
            "R1 canonical settlement/projection text"
        );
        assert!(
            session_agent_report_messages(&messages, &RunId("unknown".into())).is_empty(),
            "R2 fail closed"
        );
    }

    #[test]
    fn reply_activity_identity_is_scoped_to_the_committed_await() {
        // Cause/effect graph: C1 public reply and optional System payload are
        // identical/changed; C2 committed Run/correlation is identical or a
        // later await; C3 target is Primary/child. Effects: E1 exact retry
        // reuses one activity receipt; E2 a reused provider tool id in a later
        // Run cannot reuse that receipt; E3 changed System/target is a distinct
        // payload and therefore cannot pass as an exact replay.
        //
        // Decision table:
        // | Rule | Reply/System | Run/correlation | Target | Effect |
        // | I1 | same | same | same | E1 same id |
        // | I2 | same | changed | same | E2 distinct id |
        // | I3 | changed | same | same | E3 distinct id |
        // | I4 | same | same | changed | E3 distinct id |
        // | I5 | same call reused in same Run | same | same | distinct public occurrence/id |
        // Constraint/invariant: the committed Run/correlation fence and full
        // accepted payload bind identity; the transient prior epoch does not.
        let command = SessionThreadToolReplyCommand {
            session_id: "session".into(),
            tool_request_event_id: Some("evt-tool-1".into()),
            expected_thread_version: Some(7),
            target: SessionThreadTarget::Child(ThreadId("child".into())),
            expected_run_id: RunId("run-1".into()),
            expected_correlation_id: "correlation-1".into(),
            tool_use_id: "provider-reused-call".into(),
            reply: SessionThreadToolReply::Confirm(PermissionDecision::Allow { note: None }),
            accompanying_system: Some(crate::SessionUserRunSystemInput {
                operation_id: "system-1".into(),
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "context",
                )],
            }),
        };
        let first = SessionThreadToolReplyFence {
            prior_session_activity_epoch: Some(7),
            already_applied: false,
        };
        let replay = first.clone();
        assert_eq!(
            command.activity_operation_id(),
            command.clone().activity_operation_id(),
            "I1/E1"
        );
        assert_eq!(first, replay, "I1 activity coordinate is not identity");
        let mut later = command.clone();
        later.expected_run_id = RunId("run-2".into());
        later.expected_correlation_id = "correlation-2".into();
        assert_ne!(
            command.activity_operation_id(),
            later.activity_operation_id(),
            "I2/E2"
        );
        let mut changed_system = command.clone();
        changed_system
            .accompanying_system
            .as_mut()
            .expect("system")
            .content = vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "changed",
        )];
        assert_ne!(
            command.activity_operation_id(),
            changed_system.activity_operation_id(),
            "I3/E3"
        );
        let mut primary = command.clone();
        primary.target = SessionThreadTarget::Primary;
        assert_ne!(
            command.activity_operation_id(),
            primary.activity_operation_id(),
            "I4/E3"
        );
        let mut later_same_run = command.clone();
        later_same_run.tool_request_event_id = Some("evt-tool-2".into());
        assert_ne!(
            command.activity_operation_id(),
            later_same_run.activity_operation_id(),
            "I5 provider call ids are not occurrence identities"
        );
    }

    #[test]
    fn reply_execution_classification_distinguishes_confirmation_from_supplied_result() {
        // Cause/effect graph: C1 the closed reply is a permission decision or
        // externally supplied result; C2 a result uses the custom or generic
        // family; C3 permission is allow/deny with optional explanatory text;
        // C4 supplied content is normal/error. Effects: E1 permission replies
        // require a permission ticket; E2 both result families require a
        // client-executed ticket. Exact decision/content/error projection is
        // covered at the Host boundary; protocol provenance remains upstream.
        //
        // Decision table:
        // | Rule | Reply family | Effect |
        // |---|---|---|
        // | R1-R3 | Confirm closed allow/deny decision | E1 false |
        // | R4 | Custom normal/error result | E2 true |
        // | R5 | Generic normal/error result | E2 true |
        // Constraint/invariant: this closed reply enum is the sole Runtime
        // classification; protocol provenance validation remains upstream.
        for reply in [
            SessionThreadToolReply::Confirm(PermissionDecision::Allow { note: None }),
            SessionThreadToolReply::Confirm(PermissionDecision::Deny {
                reason: Some("blocked".into()),
            }),
        ] {
            assert!(!reply.client_executed(), "R1-R3/E1");
        }
        for reply in [
            SessionThreadToolReply::Custom {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "custom",
                )],
                is_error: false,
            },
            SessionThreadToolReply::Result {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "generic",
                )],
                is_error: true,
            },
        ] {
            assert!(reply.client_executed(), "R4-R5/E2");
        }
    }

    #[test]
    fn boundary_dispatch_provenance_is_backward_compatible_and_round_trips() {
        // Cause/effect graph: C1 an older trusted transport omits cancellation
        // and frozen Agent provenance; C2 a current transport carries an exact
        // Agent plus true/false cancellation. Effects: E1 omission decodes as
        // ordinary completion plus a blank identity that settlement rejects if
        // it needs a child report; E2 current values round-trip exactly. These
        // compatibility defaults do not authorize remote input: the
        // claim-guarded HTTP edge independently compares both with queue truth.
        //
        // Decision table:
        // | Rule | Provenance fields | Values | Effect |
        // |---|---|---|---|
        // | B1 | absent | - | E1 false + blank/fail-closed identity |
        // | B2 | present | agent-a + false | E2 exact |
        // | B3 | present | agent-a + true | E2 exact |
        // Constraint/invariant: serde defaults preserve old trusted records
        // only; they cannot bypass the claim-guarded provenance comparison.
        let legacy = serde_json::json!({
            "session_id": "session",
            "source_thread_id": "child",
            "source_run_id": "run",
            "session_activity_epoch": 7,
        });
        let decoded: SessionAgentBoundaryCommand = serde_json::from_value(legacy).unwrap();
        assert!(!decoded.cancellation_requested, "B1/E1");
        assert!(decoded.source_agent_id.is_empty(), "B1/E1");

        for expected in [false, true] {
            let command = SessionAgentBoundaryCommand {
                session_id: "session".into(),
                source_thread_id: ThreadId("child".into()),
                source_run_id: RunId("run".into()),
                source_agent_id: "agent-a".into(),
                session_activity_epoch: 7,
                cancellation_requested: expected,
            };
            let round_trip: SessionAgentBoundaryCommand =
                serde_json::from_value(serde_json::to_value(&command).unwrap()).unwrap();
            assert_eq!(round_trip.cancellation_requested, expected, "B2-B3/E2");
            assert_eq!(round_trip.source_agent_id, "agent-a", "B2-B3/E2");
        }
    }
}

/// Rebuildable relationship obtained from committed parent tool facts and
/// ordinary child dispatch/Thread facts. Nothing in this value is stored as a
/// second relationship authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatedThreadTarget {
    Agent { agent_id: String },
    Advisor { model: String },
}

impl CoordinatedThreadTarget {
    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Agent { agent_id } => Some(agent_id),
            Self::Advisor { .. } => None,
        }
    }

    #[must_use]
    pub fn advisor_model(&self) -> Option<&str> {
        match self {
            Self::Agent { .. } => None,
            Self::Advisor { model } => Some(model),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatedThreadLink {
    pub session_id: String,
    pub thread_id: ThreadId,
    pub target: CoordinatedThreadTarget,
    pub created_by_operation_id: String,
    pub latest_run_id: Option<RunId>,
}

/// Application port used by the fixed builtin tools. The implementation is the
/// existing Session application, so model tools cannot bypass Session terminal,
/// roster, budget, or concurrency policy.
#[async_trait::async_trait]
pub trait SessionAgentCoordination: Send + Sync {
    /// Admit the durable activity for one exact self-affine Session Run. Exact
    /// operation replay must return its committed epoch before evaluating fresh
    /// terminal, budget, or roster policy.
    async fn admit_session_run_activity(
        &self,
        _session_id: &str,
        _agent_id: &str,
        _run_id: &RunId,
        _mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, crate::RunError> {
        Err(crate::RunError::unavailable(
            "Session Run activity admission is unsupported",
        ))
    }

    /// Reconcile cumulative committed Session usage and decide whether the
    /// identified Run may start its next logical model request. Implementations
    /// must evaluate the root aggregate on every call and must not cache this
    /// decision in a Worker or Runtime.
    async fn admit_session_model_request(
        &self,
        _session_id: &str,
        _thread_id: &ThreadId,
        _run_id: &RunId,
    ) -> Result<bool, crate::RunError> {
        Err(crate::RunError::unavailable(
            "Session model-request admission is unsupported",
        ))
    }

    async fn list_session_agents(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, crate::RunError>;

    async fn send_session_agent_message(
        &self,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, crate::RunError>;

    async fn settle_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), crate::RunError>;

    async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &ThreadId,
    ) -> Result<(), crate::RunError>;

    async fn reply_session_thread_tool(
        &self,
        command: SessionThreadToolReplyCommand,
    ) -> Result<(), crate::RunError>;
}
