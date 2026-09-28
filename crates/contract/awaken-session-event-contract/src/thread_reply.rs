// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the extracted neutral Session Thread target, reply command, activity fence
// @formal reason: and delivery vocabulary shared by Event admission and coordination. Dynamic path: exact
// @formal reason: Session, Run, correlation, tool occurrence and optional committed Thread version remain
// @formal reason: one immutable command; stable identities derive from that command and no protocol
// @formal reason: adapter or mailbox owns a second selector. This is a product-boundary classification;
// @formal reason: adjacent decision tables cover the finite routing partitions.
use crate::SessionUserRunSystemInput;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_types::SessionId;
use serde::{Deserialize, Serialize};

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
    pub fn thread_id(&self, session_id: &SessionId) -> ThreadId {
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
    pub session_id: SessionId,
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
    pub accompanying_system: Option<SessionUserRunSystemInput>,
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
            awaken_agent_contract::stable_fingerprint(&(
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
            awaken_agent_contract::stable_fingerprint(&(
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

/// Existing dispatch ownership, derived while checking the committed Awaiting ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionThreadToolReplyActivity {
    /// Session-owned continuation transfers the prior root activity, if any.
    Session { prior_epoch: Option<u64> },
    /// An extension owns this ordinary Run; transport through a Session does not
    /// grant that Session a second activity or alter the dispatch identity.
    ThreadExtension,
}

impl SessionThreadToolReplyActivity {
    /// Classify routing only; callers still validate the exact committed ticket and owner.
    #[must_use]
    pub fn from_dispatch(
        thread: &ThreadId,
        affinity: Option<&ThreadId>,
        epoch: Option<u64>,
        parent: &ThreadId,
    ) -> Option<Self> {
        if epoch == Some(0) {
            return None;
        }
        match affinity {
            Some(bound) if bound == parent => Some(Self::Session { prior_epoch: epoch }),
            None if thread == parent && epoch.is_none() => Some(Self::ThreadExtension),
            _ => None,
        }
    }

    #[must_use]
    pub fn prior_epoch(&self) -> Option<u64> {
        match self {
            Self::Session { prior_epoch } => *prior_epoch,
            Self::ThreadExtension => None,
        }
    }
}

/// Exact command coordinates remain on the command, not duplicated in this fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionThreadToolReplyFence {
    pub activity: SessionThreadToolReplyActivity,
    /// The exact reply has already been consumed in committed Runtime truth.
    pub already_applied: bool,
}

/// Session-approved delivery handed to Runtime. Session-owned Runs carry a newly
/// opened root activity; extension-owned Runs carry none and retain their owner. This wrapper is internal
/// coordination vocabulary, not a Managed wire shape or another command bus.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionThreadToolReplyDelivery {
    pub command: SessionThreadToolReplyCommand,
    pub fence: SessionThreadToolReplyFence,
    pub session_activity_epoch: Option<u64>,
}
