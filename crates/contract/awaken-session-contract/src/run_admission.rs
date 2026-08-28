//! Protocol-neutral admission vocabulary for one durable Session-scoped Run.
//!
//! The Session application coordinates the existing Session root and Run
//! dispatch authorities with these values. They own no queue, completion
//! registry, or second Run lifecycle.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;

/// Derive the stable internal Run identity for a public Session mutation.
///
/// Protocol adapters retain their wire identity as `operation_id`; no mapping
/// repository is required because the internal identity is deterministic.
#[must_use]
pub fn session_run_id(session_id: &str, operation_id: &str) -> RunId {
    RunId(format!(
        "session-run-{}",
        crate::stable_fingerprint(&("session-run-v1", session_id, operation_id,))
    ))
}

/// Complete immutable input for one durable Session Run reservation.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmitSessionRun {
    pub session_id: String,
    pub agent_id: String,
    pub operation_id: String,
    pub run_id: RunId,
    pub messages: Vec<Message>,
    pub data_subject_id: Option<String>,
    /// W3C trace context frozen by the admitting edge. Recovery relays this
    /// exact value instead of capturing a supervisor's ambient span.
    pub traceparent: Option<String>,
}

impl AdmitSessionRun {
    /// Message identities are the exact committed-input proof used when a
    /// foreground caller recovers the resulting Step from Thread truth.
    #[must_use]
    pub fn input_message_ids(&self) -> Vec<String> {
        self.messages
            .iter()
            .map(|message| message.id.0.clone())
            .collect()
    }
}

/// Closed result of reserving a dispatch row before root activity admission.
/// No variant authorizes direct execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRunReservation {
    Reserved,
    AlreadyReserved,
    RecoveryClaimed,
    AlreadyActivated { session_activity_epoch: u64 },
    Completed,
}

/// Bind one already-admitted Session activity to its exact reserved Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRunDelivery {
    pub session_id: String,
    pub run_id: RunId,
    pub session_activity_epoch: u64,
}

/// Closed application result after durable reservation and the exact Session
/// activity receipt have both been observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmittedSessionRun {
    Reserved(SessionRunDelivery),
    AlreadyReserved(SessionRunDelivery),
    AlreadyActivated(SessionRunDelivery),
    RecoveryClaimed { session_id: String, run_id: RunId },
    Completed { session_id: String, run_id: RunId },
}

impl AdmittedSessionRun {
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Reserved(delivery)
            | Self::AlreadyReserved(delivery)
            | Self::AlreadyActivated(delivery) => &delivery.session_id,
            Self::RecoveryClaimed { session_id, .. } | Self::Completed { session_id, .. } => {
                session_id
            }
        }
    }

    #[must_use]
    pub fn run_id(&self) -> &RunId {
        match self {
            Self::Reserved(delivery)
            | Self::AlreadyReserved(delivery)
            | Self::AlreadyActivated(delivery) => &delivery.run_id,
            Self::RecoveryClaimed { run_id, .. } | Self::Completed { run_id, .. } => run_id,
        }
    }

    #[must_use]
    pub fn delivery(&self) -> Option<&SessionRunDelivery> {
        match self {
            Self::Reserved(delivery)
            | Self::AlreadyReserved(delivery)
            | Self::AlreadyActivated(delivery) => Some(delivery),
            Self::RecoveryClaimed { .. } | Self::Completed { .. } => None,
        }
    }
}

/// Closed publication result for a Session Run reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRunActivation {
    Activated,
    AlreadyActivated { session_activity_epoch: u64 },
    RecoveryClaimed,
    Completed,
}

#[cfg(test)]
mod tests {
    use super::session_run_id;

    #[test]
    fn session_run_identity_follows_the_idempotency_decision_table() {
        // Cause/effect graph: C1 session identity and C2 operation identity are
        // equal or one differs. Effects: E1 exact retry derives the same RunId;
        // E2 either changed coordinate derives another RunId. Constraints: the
        // derivation is domain-separated and owns no mapping store. Decision
        // rules: R1=C1+C2=>E1; R2=!C1|!C2=>E2.
        let exact = session_run_id("session-1", "operation-1");
        assert_eq!(exact, session_run_id("session-1", "operation-1"), "R1/E1");
        assert_ne!(exact, session_run_id("session-2", "operation-1"), "R2/E2");
        assert_ne!(exact, session_run_id("session-1", "operation-2"), "R2/E2");
    }
}
