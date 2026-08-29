//! Protocol-neutral admission vocabulary for one durable Session-scoped Run.
//!
//! The Session application coordinates the existing Session root and Run
//! dispatch authorities with these values. They own no queue, completion
//! registry, or second Run lifecycle.

use std::collections::BTreeSet;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
use serde::{Deserialize, Serialize};

/// Queue effect requested by one Session Run admission.
///
/// The value is part of the immutable reservation identity: recovery must
/// preserve whether this Run merely follows prior work or replaces every older
/// live dispatch on its logical Thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunReplacement {
    #[default]
    PreservePrior,
    SupersedePrior,
}

impl SessionRunReplacement {
    #[must_use]
    pub const fn supersedes_prior(self) -> bool {
        matches!(self, Self::SupersedePrior)
    }
}

/// Immutable execution restrictions contributed by the application that owns
/// a Session Run. These values can only narrow tool authority or require a
/// more-specific executor; they cannot replace the Session's frozen Agent,
/// Environment, Resource, credential, or tool configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRunExecutionRequirements {
    pub tool_capability_narrowing: ToolCapabilityNarrowing,
    /// Opaque application protocol capabilities advertised by an eligible
    /// registered Worker. The Runtime treats them solely as hard placement
    /// requirements and never interprets their names as authorization.
    pub required_worker_capabilities: BTreeSet<String>,
}

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
    /// Per-Run application requirements intersected with the canonical Session
    /// runtime projection before the reservation becomes durable.
    pub execution_requirements: SessionRunExecutionRequirements,
    /// Explicit newest-wins intent. This is typed domain input, not an HTTP or
    /// queue option inferred from the presence of an awaiting ticket.
    pub replacement: SessionRunReplacement,
}

/// Versioned compact identity of the immutable command admitted by the
/// Session application.
///
/// Runtime projections are deliberately absent: a retry may observe newer
/// Agent, model, Resource, Environment, Skill, or placement facts without
/// changing the command that already owns the reserved Run. Distributed trace
/// context is also excluded because it identifies an attempt, not the command.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionRunCommandFingerprint(String);

impl SessionRunCommandFingerprint {
    pub const CURRENT_PREFIX: &'static str = "session-command-v1:sha256:";

    /// Compute the current identity before any mutable Runtime projection is
    /// applied to the admitted command.
    #[must_use]
    pub fn current(command: &AdmitSessionRun) -> Self {
        let digest = awaken_runtime_contract::content_fingerprint(&(
            "session-command-v1",
            &command.session_id,
            &command.agent_id,
            &command.operation_id,
            &command.run_id,
            &command.messages,
            &command.data_subject_id,
            command.execution_requirements.tool_capability_narrowing,
            &command.execution_requirements.required_worker_capabilities,
            command.replacement,
        ))
        .expect("Session Run command identity has no fallible serializable value");
        Self(format!("{}{digest}", Self::CURRENT_PREFIX))
    }

    /// Whether this value is the exact current wire format. Unknown versions
    /// remain deserializable for durable-row inspection but never authorize a
    /// new reservation or replay.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.0
            .strip_prefix(Self::CURRENT_PREFIX)
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
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
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_runtime_contract::permission::ToolCapabilityNarrowing;

    use super::{
        AdmitSessionRun, SessionRunCommandFingerprint, SessionRunExecutionRequirements,
        SessionRunReplacement, session_run_id,
    };

    fn command() -> AdmitSessionRun {
        AdmitSessionRun {
            session_id: "session-1".into(),
            agent_id: "agent-1".into(),
            operation_id: "operation-1".into(),
            run_id: RunId("run-1".into()),
            messages: vec![Message::text(
                MessageId("message-1".into()),
                Role::User,
                "do the work",
            )],
            data_subject_id: Some("subject-1".into()),
            traceparent: Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into()),
            execution_requirements: SessionRunExecutionRequirements {
                tool_capability_narrowing: ToolCapabilityNarrowing::Configured,
                required_worker_capabilities: ["managed".to_string()].into_iter().collect(),
            },
            replacement: SessionRunReplacement::PreservePrior,
        }
    }

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

    #[test]
    fn session_command_fingerprint_covers_the_complete_admission_decision_table() {
        // Cause/effect graph: C1 every immutable command coordinate is exact
        // or one of session, agent, operation, run, messages, data subject,
        // tool narrowing, required capabilities, and replacement changes; C2
        // only trace context changes. Effects: E1 exact/C2 retries retain one
        // current compact identity; E2 every C1 change gets another identity.
        // Constraint: mutable Runtime projections do not exist in this input.
        // Rules F1=exact=>E1, F2=C2=>E1, F3=any C1 change=>E2.
        let original = command();
        let fingerprint = SessionRunCommandFingerprint::current(&original);
        assert!(fingerprint.is_current(), "F1/E1");
        assert!(
            fingerprint
                .as_str()
                .starts_with(SessionRunCommandFingerprint::CURRENT_PREFIX),
            "F1/E1"
        );
        assert_eq!(
            fingerprint,
            SessionRunCommandFingerprint::current(&original),
            "F1/E1"
        );

        let mut retraced = original.clone();
        retraced.traceparent =
            Some("00-cccccccccccccccccccccccccccccccc-dddddddddddddddd-01".into());
        assert_eq!(
            fingerprint,
            SessionRunCommandFingerprint::current(&retraced),
            "F2/E1"
        );

        let mut changes: Vec<AdmitSessionRun> = Vec::new();
        let mut changed = original.clone();
        changed.session_id = "session-2".into();
        changes.push(changed);
        let mut changed = original.clone();
        changed.agent_id = "agent-2".into();
        changes.push(changed);
        let mut changed = original.clone();
        changed.operation_id = "operation-2".into();
        changes.push(changed);
        let mut changed = original.clone();
        changed.run_id = RunId("run-2".into());
        changes.push(changed);
        let mut changed = original.clone();
        changed.messages = vec![Message::text(
            MessageId("message-2".into()),
            Role::User,
            "different work",
        )];
        changes.push(changed);
        let mut changed = original.clone();
        changed.data_subject_id = Some("subject-2".into());
        changes.push(changed);
        let mut changed = original.clone();
        changed.execution_requirements.tool_capability_narrowing = ToolCapabilityNarrowing::DenyAll;
        changes.push(changed);
        let mut changed = original.clone();
        changed
            .execution_requirements
            .required_worker_capabilities
            .insert("repository-credentials".into());
        changes.push(changed);
        let mut changed = original;
        changed.replacement = SessionRunReplacement::SupersedePrior;
        changes.push(changed);

        for changed in changes {
            assert_ne!(
                fingerprint,
                SessionRunCommandFingerprint::current(&changed),
                "F3/E2"
            );
        }
    }

    #[test]
    fn unknown_session_command_fingerprint_versions_remain_inspectable_but_inactive() {
        // Wire-compatibility rule W1: a stored unknown version must decode for
        // migration inspection (E1) but cannot be accepted as current (E2).
        let unknown: SessionRunCommandFingerprint =
            serde_json::from_str("\"session-command-v2:sha256:abcd\"").expect("W1/E1");
        assert!(!unknown.is_current(), "W1/E2");
    }
}
