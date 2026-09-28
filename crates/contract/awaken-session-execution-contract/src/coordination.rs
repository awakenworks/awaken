// @formal entrypoint session.agent_message_missing_lifecycle_projection_relinquishes_without_settlement
// @formal entrypoint session.agent_message_target_boundary_not_source_receipt_settles_activity
//! Neutral Session-owned Agent coordination vocabulary.
//!
//! A coordinated child is still an ordinary Thread containing ordinary Runs.
//! These values describe admission and a derived relationship; they are not a
//! Subagent aggregate, registry, mailbox, or persistence model.

use crate::SessionId;
#[cfg(test)]
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::classify_run_lifecycle_event;
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

/// Coordinates supplied by the Runtime tool boundary and accepted only after
/// refinement against the source Thread's committed `ActiveToolBatch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAgentMessageCommand {
    pub session_id: crate::SessionId,
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
/// Cancellation uses `RecoverOnly`: deleting a never-admitted reservation is valid,
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
pub fn session_run_activity_operation_id(session_id: &SessionId, run_id: &RunId) -> String {
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
    session_id: &SessionId,
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
/// already-derived one. Spawn derives a logical Thread and follow-up reuses it;
/// both freeze input in their own fresh ordinary Run activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatedRunIntent {
    Spawn,
    FollowUp,
}

pub use awaken_session_event_contract::SessionAgentRunBoundary;

/// Classify only committed states that form an Agent-message activity boundary.
#[must_use]
pub fn session_agent_run_boundary(state: &RunState) -> Option<SessionAgentRunBoundary> {
    SessionAgentRunBoundary::of_lifecycle_kind(classify_run_lifecycle_event(state, None))
}

/// Close-rule class of one coordinated Thread.
///
/// Consumers: the Managed lifecycle projection (which only sees the wire
/// `is_advisor` bit), Managed interruption recovery, and the Session/Host
/// follow-up admission fences. They previously split the rule: only the
/// projection knew that Advisor consultations close at every terminal Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatedThreadKind {
    /// Ordinary Agent Thread reused by follow-up messages.
    Agent,
    /// One-shot Advisor consultation.
    Advisor,
}

impl CoordinatedThreadKind {
    #[must_use]
    pub const fn from_is_advisor(is_advisor: bool) -> Self {
        if is_advisor {
            Self::Advisor
        } else {
            Self::Agent
        }
    }
}

/// Whether the latest committed Run permanently closes a coordinated Thread to
/// further input.
///
/// This is a projection of the existing Run lifecycle authority, not another
/// Thread disposition cell. For an ordinary Agent Thread, Completed and
/// externally cancelled Runs leave the logical Thread reusable; a `Failed`
/// lifecycle classification is absorbing because no later Run may be admitted
/// after it. An Advisor consultation is one-shot, so every ended Run closes it.
#[must_use]
pub fn coordinated_thread_closed(kind: CoordinatedThreadKind, state: &RunState) -> bool {
    match kind {
        CoordinatedThreadKind::Agent => {
            session_agent_run_boundary(state) == Some(SessionAgentRunBoundary::Failed)
        }
        CoordinatedThreadKind::Advisor => state.is_terminal(),
    }
}

/// Logical position of the claimed Run inside Agent-message settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAgentBoundaryRole {
    Child,
    PrimaryContinuation,
}

/// Whether the lifecycle feed has published the exact committed Run boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAgentBoundaryVisibility {
    Missing,
    Visible,
}

/// Whether the committed child snapshot contains the canonical non-empty
/// report selected by [`session_agent_report_text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAgentCommittedReport {
    Absent,
    Present,
}

/// Complete repository-controlled action for one claimed Agent-message Run.
/// Ports execute the selected effects; this value owns no persistence or queue
/// state of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAgentBoundaryAction {
    RejectNonBoundary,
    RetryMissingProjection,
    InterruptThenSettle,
    Settle,
    ContinueReport,
}

/// Select the only legal next action from the existing committed Thread,
/// lifecycle projection, and immutable claimed-dispatch provenance.
#[must_use]
pub fn session_agent_boundary_action(
    role: SessionAgentBoundaryRole,
    state: &RunState,
    visibility: SessionAgentBoundaryVisibility,
    cancellation_requested: bool,
    report: SessionAgentCommittedReport,
) -> SessionAgentBoundaryAction {
    let Some(boundary) = session_agent_run_boundary(state) else {
        return SessionAgentBoundaryAction::RejectNonBoundary;
    };
    if visibility == SessionAgentBoundaryVisibility::Missing {
        return SessionAgentBoundaryAction::RetryMissingProjection;
    }
    if role == SessionAgentBoundaryRole::Child && boundary == SessionAgentRunBoundary::Failed {
        return SessionAgentBoundaryAction::InterruptThenSettle;
    }
    if role == SessionAgentBoundaryRole::Child
        && boundary == SessionAgentRunBoundary::Completed
        && !cancellation_requested
        && report == SessionAgentCommittedReport::Present
    {
        return SessionAgentBoundaryAction::ContinueReport;
    }
    SessionAgentBoundaryAction::Settle
}

/// Durable-dispatch input produced only after Session policy has admitted it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoordinatedRunCommand {
    pub intent: CoordinatedRunIntent,
    pub session_id: crate::SessionId,
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
    pub session_id: crate::SessionId,
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
/// agent-contract provenance minted from `source_run_id`; the Host freezes it
/// directly into the deterministic root Run activation. Thread commits remain
/// the message authority, so no parallel Outbox/Inbox fact is created.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionAgentReportContinuation {
    pub session_id: crate::SessionId,
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

#[cfg(kani)]
/// @obligation session.agent_message_completed_target_allows_late_source_receipt_without_reexecution
/// @obligation session.agent_message_missing_lifecycle_projection_relinquishes_without_settlement
/// @obligation session.agent_message_target_boundary_not_source_receipt_settles_activity
#[kani::proof]
fn session_agent_boundary_requires_a_visible_committed_target() {
    use awaken_agent_contract::agent::run::EndCause;

    // Causes: C1 the actual committed Run state is Running/Awaiting/Completed/
    // Failed/Cancelled; C2 the lifecycle projection is missing/visible; C3 the
    // claimed dispatch is a child/root continuation and is/is-not cancelled;
    // C4 the canonical committed report is absent/present. Effects: E1 a
    // non-boundary rejects; E2 a missing projection retries without settlement;
    // E3 only a visible normal Completed child with a report continues; E4 a
    // visible failed child interrupts before settlement; E5 every other visible
    // boundary settles directly. The source tool receipt is deliberately not an
    // input: it has no authority to choose any settlement action.
    let state = match kani::any::<u8>() % 5 {
        0 => RunState::Running,
        1 => RunState::Awaiting,
        2 => RunState::Ended(EndCause::NaturalEnd),
        3 => RunState::Ended(EndCause::MaxSteps),
        _ => RunState::Ended(EndCause::Cancelled),
    };
    let role = if kani::any() {
        SessionAgentBoundaryRole::Child
    } else {
        SessionAgentBoundaryRole::PrimaryContinuation
    };
    let visibility = if kani::any() {
        SessionAgentBoundaryVisibility::Visible
    } else {
        SessionAgentBoundaryVisibility::Missing
    };
    let cancellation_requested = kani::any();
    let report = if kani::any() {
        SessionAgentCommittedReport::Present
    } else {
        SessionAgentCommittedReport::Absent
    };
    let action =
        session_agent_boundary_action(role, &state, visibility, cancellation_requested, report);
    let boundary = session_agent_run_boundary(&state);

    assert_eq!(
        action == SessionAgentBoundaryAction::RejectNonBoundary,
        boundary.is_none()
    );
    assert_eq!(
        action == SessionAgentBoundaryAction::RetryMissingProjection,
        boundary.is_some() && visibility == SessionAgentBoundaryVisibility::Missing
    );
    assert_eq!(
        action == SessionAgentBoundaryAction::ContinueReport,
        boundary == Some(SessionAgentRunBoundary::Completed)
            && visibility == SessionAgentBoundaryVisibility::Visible
            && role == SessionAgentBoundaryRole::Child
            && !cancellation_requested
            && report == SessionAgentCommittedReport::Present
    );
    assert_eq!(
        action == SessionAgentBoundaryAction::InterruptThenSettle,
        boundary == Some(SessionAgentRunBoundary::Failed)
            && visibility == SessionAgentBoundaryVisibility::Visible
            && role == SessionAgentBoundaryRole::Child
    );
    assert_eq!(
        matches!(
            action,
            SessionAgentBoundaryAction::Settle
                | SessionAgentBoundaryAction::InterruptThenSettle
                | SessionAgentBoundaryAction::ContinueReport
        ),
        boundary.is_some() && visibility == SessionAgentBoundaryVisibility::Visible
    );
}

pub use awaken_session_event_contract::{
    SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyActivity,
    SessionThreadToolReplyCommand, SessionThreadToolReplyDelivery, SessionThreadToolReplyFence,
};

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    #[test]
    fn reply_activity_preserves_dispatch_ownership() {
        // Cause/effect table: exact affinity -> Session; no affinity+primary+no epoch
        // -> extension; foreign affinity, orphan child, zero epoch or orphan epoch -> reject.
        let parent = ThreadId("parent".into());
        let child = ThreadId("child".into());
        for thread in [&parent, &child] {
            for affinity in [None, Some(&parent), Some(&child)] {
                for epoch in [None, Some(0), Some(7)] {
                    let result = SessionThreadToolReplyActivity::from_dispatch(
                        thread, affinity, epoch, &parent,
                    );
                    let expected = if epoch == Some(0) {
                        None
                    } else if affinity == Some(&parent) {
                        Some(SessionThreadToolReplyActivity::Session { prior_epoch: epoch })
                    } else if affinity.is_none() && thread == &parent && epoch.is_none() {
                        Some(SessionThreadToolReplyActivity::ThreadExtension)
                    } else {
                        None
                    };
                    assert_eq!(
                        result, expected,
                        "thread={thread:?} affinity={affinity:?} epoch={epoch:?}"
                    );
                }
            }
        }
    }

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
        let exact =
            coordinated_thread_id(&crate::SessionId::from("session-1"), &run, "operation-1");
        assert_eq!(
            exact,
            coordinated_thread_id(&crate::SessionId::from("session-1"), &run, "operation-1"),
            "T1/E1"
        );
        for changed in [
            coordinated_thread_id(&crate::SessionId::from("session-2"), &run, "operation-1"),
            coordinated_thread_id(
                &crate::SessionId::from("session-1"),
                &RunId("run-2".into()),
                "operation-1",
            ),
            coordinated_thread_id(&crate::SessionId::from("session-1"), &run, "operation-2"),
        ] {
            assert_ne!(changed, exact, "T2/E2");
        }
    }

    #[test]
    fn coordinated_thread_close_rule_covers_agent_and_advisor_kinds() {
        use awaken_agent_contract::agent::run::{EndCause, Failure};

        // Cause/effect graph: C1 Thread kind is an ordinary Agent or a one-shot
        // Advisor; C2 latest committed state is active/Awaiting or one of the
        // closed EndCause variants. Effects: E1 closed (follow-up rejected,
        // Thread projected Terminated); E2 open (Thread reusable). The
        // lifecycle classifier is the sole authority; no parallel flag.
        //
        // Decision table:
        // | Rule | Kind | Latest state | Lifecycle class | Effect |
        // |---|---|---|---|---|
        // | F1 | Agent | Running/Awaiting | active | E2 |
        // | F2 | Agent | NaturalEnd | Completed | E2 |
        // | F3 | Agent | Cancelled | Cancelled | E2 |
        // | F4 | Agent | MaxSteps/Stopped/Error/Indeterminate | Failed | E1 |
        // | A1 | Advisor | Running/Awaiting | active | E2 |
        // | A2 | Advisor | any Ended | Completed/Cancelled/Failed | E1 |
        let active = [RunState::Running, RunState::Awaiting];
        let reusable = [
            RunState::Ended(EndCause::NaturalEnd),
            RunState::Ended(EndCause::Cancelled),
        ];
        let failed = [
            RunState::Ended(EndCause::MaxSteps),
            RunState::Ended(EndCause::Stopped("budget".into())),
            RunState::Ended(EndCause::Error(Failure::StateConflict)),
            RunState::Ended(EndCause::Indeterminate),
        ];
        for state in &active {
            assert!(
                !coordinated_thread_closed(CoordinatedThreadKind::Agent, state),
                "F1/E2: {state:?}"
            );
            assert!(
                !coordinated_thread_closed(CoordinatedThreadKind::Advisor, state),
                "A1/E2: {state:?}"
            );
        }
        for state in &reusable {
            assert!(
                !coordinated_thread_closed(CoordinatedThreadKind::Agent, state),
                "F2-F3/E2: {state:?}"
            );
            assert!(
                coordinated_thread_closed(CoordinatedThreadKind::Advisor, state),
                "A2/E1: {state:?}"
            );
        }
        for state in &failed {
            assert!(
                coordinated_thread_closed(CoordinatedThreadKind::Agent, state),
                "F4/E1: {state:?}"
            );
            assert!(
                coordinated_thread_closed(CoordinatedThreadKind::Advisor, state),
                "A2/E1: {state:?}"
            );
        }
        assert_eq!(
            CoordinatedThreadKind::from_is_advisor(true),
            CoordinatedThreadKind::Advisor
        );
        assert_eq!(
            CoordinatedThreadKind::from_is_advisor(false),
            CoordinatedThreadKind::Agent
        );
    }

    #[test]
    fn agent_message_boundary_action_uses_only_visible_target_truth() {
        use awaken_agent_contract::agent::run::EndCause;

        // Cause/effect graph: C1 committed source is Running or a closed target
        // boundary; C2 the exact lifecycle projection is missing/visible; C3
        // the claim is a child/primary continuation; C4 cancellation is set;
        // C5 the canonical committed report is absent/present. Effects: E1
        // Running rejects; E2 missing projection retries without settlement;
        // E3 Failed child interrupts then settles; E4 only normal Completed
        // child with report continues; E5 all remaining visible boundaries
        // settle. Source receipt is not a cause and cannot select E3-E5.
        //
        // | Rule | State | Projection | Role/cancel/report | Effect |
        // |---|---|---|---|---|
        // | P1 | Running | any | any | E1 reject |
        // | P2 | boundary | missing | any | E2 retry |
        // | P3 | Failed | visible | child | E3 interrupt+settle |
        // | P4 | Completed | visible | child/no/present | E4 continue |
        // | P5 | boundary | visible | every other tuple | E5 settle |
        assert_eq!(
            session_agent_boundary_action(
                SessionAgentBoundaryRole::Child,
                &RunState::Running,
                SessionAgentBoundaryVisibility::Visible,
                false,
                SessionAgentCommittedReport::Present,
            ),
            SessionAgentBoundaryAction::RejectNonBoundary,
            "P1/E1"
        );
        assert_eq!(
            session_agent_boundary_action(
                SessionAgentBoundaryRole::Child,
                &RunState::Awaiting,
                SessionAgentBoundaryVisibility::Missing,
                false,
                SessionAgentCommittedReport::Absent,
            ),
            SessionAgentBoundaryAction::RetryMissingProjection,
            "P2/E2"
        );
        assert_eq!(
            session_agent_boundary_action(
                SessionAgentBoundaryRole::Child,
                &RunState::Ended(EndCause::MaxSteps),
                SessionAgentBoundaryVisibility::Visible,
                false,
                SessionAgentCommittedReport::Absent,
            ),
            SessionAgentBoundaryAction::InterruptThenSettle,
            "P3/E3"
        );
        assert_eq!(
            session_agent_boundary_action(
                SessionAgentBoundaryRole::Child,
                &RunState::Ended(EndCause::NaturalEnd),
                SessionAgentBoundaryVisibility::Visible,
                false,
                SessionAgentCommittedReport::Present,
            ),
            SessionAgentBoundaryAction::ContinueReport,
            "P4/E4"
        );
        for (role, cancelled, report) in [
            (
                SessionAgentBoundaryRole::PrimaryContinuation,
                false,
                SessionAgentCommittedReport::Present,
            ),
            (
                SessionAgentBoundaryRole::Child,
                true,
                SessionAgentCommittedReport::Present,
            ),
            (
                SessionAgentBoundaryRole::Child,
                false,
                SessionAgentCommittedReport::Absent,
            ),
        ] {
            assert_eq!(
                session_agent_boundary_action(
                    role,
                    &RunState::Ended(EndCause::NaturalEnd),
                    SessionAgentBoundaryVisibility::Visible,
                    cancelled,
                    report,
                ),
                SessionAgentBoundaryAction::Settle,
                "P5/E5"
            );
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
            activity: SessionThreadToolReplyActivity::Session {
                prior_epoch: Some(7),
            },
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

    /// Close-rule class consumed by [`coordinated_thread_closed`].
    #[must_use]
    pub const fn kind(&self) -> CoordinatedThreadKind {
        match self {
            Self::Agent { .. } => CoordinatedThreadKind::Agent,
            Self::Advisor { .. } => CoordinatedThreadKind::Advisor,
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
    pub session_id: crate::SessionId,
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
        session_id: &crate::SessionId,
        agent_id: &str,
        run_id: &RunId,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, crate::RunError>;

    /// Reconcile cumulative committed Session usage and decide whether the
    /// identified Run may start its next logical model request. Implementations
    /// must evaluate the root aggregate on every call and must not cache this
    /// decision in a Worker or Runtime.
    async fn admit_session_model_request(
        &self,
        session_id: &crate::SessionId,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<bool, crate::RunError>;

    async fn list_session_agents(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<Vec<SessionAgentRosterEntry>, crate::RunError>;

    async fn send_session_agent_message(
        &self,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, crate::RunError>;

    async fn settle_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), crate::RunError>;

    /// Apply only after the exact Dispatch settlement CAS succeeded. The
    /// default fails closed so a coordination adapter cannot report full
    /// Session settlement while omitting the post-Dispatch root receipt.
    async fn finalize_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), crate::RunError>;

    async fn interrupt_session_thread(
        &self,
        session_id: &crate::SessionId,
        child_thread_id: &ThreadId,
    ) -> Result<(), crate::RunError>;

    async fn reply_session_thread_tool(
        &self,
        command: SessionThreadToolReplyCommand,
    ) -> Result<(), crate::RunError>;
}
