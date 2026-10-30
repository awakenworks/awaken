// @formal entrypoint session_event_batch.run_reservation_precedes_distinct_activity_receipt
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint session_run.reservation_precedes_activity
//! Protocol-neutral admission vocabulary for one durable Session-scoped Run.
//!
//! The Session application coordinates the existing Session root and Run
//! dispatch authorities with these values. They own no queue, completion
//! registry, or second Run lifecycle.

use std::collections::BTreeSet;

use crate::SessionId;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
use serde::{Deserialize, Serialize};

use awaken_session_types::{SessionRunCommandFingerprint, SessionRunReplacement};

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
pub fn session_run_id(session_id: &SessionId, operation_id: &str) -> RunId {
    RunId(format!(
        "session-run-{}",
        crate::stable_fingerprint(&("session-run-v1", session_id, operation_id,))
    ))
}

/// Complete immutable input for one durable Session Run reservation.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmitSessionRun {
    pub session_id: crate::SessionId,
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

impl AdmitSessionRun {
    /// Compute the command's current compact identity before any mutable
    /// Runtime projection is applied to it.
    #[must_use]
    pub fn command_fingerprint(&self) -> SessionRunCommandFingerprint {
        let digest = awaken_agent_contract::content_fingerprint(&(
            "session-command-v1",
            &self.session_id,
            &self.agent_id,
            &self.operation_id,
            &self.run_id,
            &self.messages,
            &self.data_subject_id,
            self.execution_requirements.tool_capability_narrowing,
            &self.execution_requirements.required_worker_capabilities,
            self.replacement,
        ))
        .expect("Session Run command identity has no fallible serializable value");
        SessionRunCommandFingerprint::from_sha256_hex(&digest)
            .expect("content_fingerprint yields a lowercase SHA-256 hex digest")
    }

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

/// Trusted, non-wire input whose caller already froze the entire activation.
/// No current Session Agent, Skill or model projection may replace its fields.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmitPreparedSessionRun {
    pub session_id: crate::SessionId,
    pub operation_id: String,
    pub activation: awaken_runtime_contract::RunActivation,
    pub traceparent: Option<String>,
}

impl AdmitPreparedSessionRun {
    #[must_use]
    pub fn command_fingerprint(&self) -> SessionRunCommandFingerprint {
        let digest = awaken_agent_contract::content_fingerprint(&(
            "session-prepared-command-v1",
            &self.session_id,
            &self.operation_id,
            &self.activation,
        ))
        .expect("prepared activation has no fallible serializable value");
        SessionRunCommandFingerprint::from_sha256_hex(&digest)
            .expect("content_fingerprint yields a lowercase SHA-256 hex digest")
    }
}

/// Borrowed admission of a complete prepared root Run into its Session.
/// The application owns admission and its durable refusal evidence; Execution
/// adapts this neutral port to the SDK executor required by the Outcome controller.
/// No caller may retain this borrow or substitute an activation field.
#[async_trait::async_trait]
pub trait PreparedSessionRunAdmission: Send + Sync {
    async fn admit(
        &self,
        activation: awaken_runtime_contract::RunActivation,
    ) -> awaken_runtime_contract::diagnostic::Result<awaken_agent_contract::agent::run::RunState>;
}

/// Borrowed admission into the sole Session activity owner. Delivery calls
/// this after reservation and, for legacy adoption, under admission tenure.
/// Projection and continuation preparation must precede this callback.
#[async_trait::async_trait]
pub trait SessionRunActivityAdmitter: Send + Sync {
    async fn admit(
        &self,
        reservation: SessionRunReservation,
        recover_only: bool,
        admitted_after_cursor: Option<u64>,
    ) -> Result<AdmittedSessionRun, awaken_session_types::RunError>;
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

/// The sole reservation-to-Session-activity decision. The Runtime reservation
/// is already durable when this plan is evaluated; Reserved variants must bind
/// one exact non-zero root receipt before activation, while an activated row
/// carries its previously bound receipt and never opens another activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRunActivityReceiptPlan {
    CommitActivity {
        reservation_replayed: bool,
    },
    UseReceipt {
        session_activity_epoch: u64,
        reservation_replayed: bool,
    },
    UseActivatedReceipt {
        session_activity_epoch: u64,
    },
    RecoveryClaimed,
    Completed,
    RejectInvalidReceipt,
}

#[must_use]
pub const fn session_run_activity_receipt_plan(
    reservation: SessionRunReservation,
    existing_activity_epoch: Option<u64>,
) -> SessionRunActivityReceiptPlan {
    match reservation {
        SessionRunReservation::Reserved | SessionRunReservation::AlreadyReserved => {
            let reservation_replayed =
                matches!(reservation, SessionRunReservation::AlreadyReserved);
            match existing_activity_epoch {
                Some(0) => SessionRunActivityReceiptPlan::RejectInvalidReceipt,
                Some(session_activity_epoch) => SessionRunActivityReceiptPlan::UseReceipt {
                    session_activity_epoch,
                    reservation_replayed,
                },
                None => SessionRunActivityReceiptPlan::CommitActivity {
                    reservation_replayed,
                },
            }
        }
        SessionRunReservation::AlreadyActivated {
            session_activity_epoch: 0,
        } => SessionRunActivityReceiptPlan::RejectInvalidReceipt,
        SessionRunReservation::AlreadyActivated {
            session_activity_epoch,
        } => SessionRunActivityReceiptPlan::UseActivatedReceipt {
            session_activity_epoch,
        },
        SessionRunReservation::RecoveryClaimed => SessionRunActivityReceiptPlan::RecoveryClaimed,
        SessionRunReservation::Completed => SessionRunActivityReceiptPlan::Completed,
    }
}

#[cfg(kani)]
/// @obligation session_event_batch.run_reservation_precedes_distinct_activity_receipt
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
/// @obligation session_run.recovery_claim_is_admission_only
/// @obligation session_run.reservation_precedes_activity
#[kani::proof]
fn session_run_reservation_binds_exactly_one_nonzero_activity_receipt() {
    // Cause/effect table exhausts five reservation classes, optional prior
    // receipt and arbitrary u64 receipt values. Reserved/AlreadyReserved with
    // no receipt select one commit; a non-zero receipt is reused exactly;
    // zero fails closed. AlreadyActivated uses only its bound non-zero receipt.
    // Recovery/Completed never create or reuse an activity. This is the
    // production decision called only after the Runtime reservation returns.
    let reservation = match kani::any::<u8>() % 5 {
        0 => SessionRunReservation::Reserved,
        1 => SessionRunReservation::AlreadyReserved,
        2 => SessionRunReservation::RecoveryClaimed,
        3 => SessionRunReservation::AlreadyActivated {
            session_activity_epoch: kani::any(),
        },
        _ => SessionRunReservation::Completed,
    };
    let existing = kani::any::<bool>().then_some(kani::any());
    let plan = session_run_activity_receipt_plan(reservation, existing);
    let reserved = matches!(
        reservation,
        SessionRunReservation::Reserved | SessionRunReservation::AlreadyReserved
    );
    let invalid = (reserved && existing == Some(0))
        || matches!(
            reservation,
            SessionRunReservation::AlreadyActivated {
                session_activity_epoch: 0
            }
        );
    assert_eq!(
        plan == SessionRunActivityReceiptPlan::RejectInvalidReceipt,
        invalid
    );
    assert_eq!(
        matches!(plan, SessionRunActivityReceiptPlan::CommitActivity { .. }),
        reserved && existing.is_none()
    );
    assert_eq!(
        matches!(plan, SessionRunActivityReceiptPlan::UseReceipt { .. }),
        reserved && existing.is_some_and(|epoch| epoch != 0)
    );
    match (reservation, existing, plan) {
        (
            SessionRunReservation::Reserved | SessionRunReservation::AlreadyReserved,
            None,
            SessionRunActivityReceiptPlan::CommitActivity {
                reservation_replayed,
            },
        ) => assert_eq!(
            reservation_replayed,
            reservation == SessionRunReservation::AlreadyReserved
        ),
        (
            SessionRunReservation::Reserved | SessionRunReservation::AlreadyReserved,
            Some(epoch),
            SessionRunActivityReceiptPlan::UseReceipt {
                session_activity_epoch,
                reservation_replayed,
            },
        ) => {
            assert_ne!(epoch, 0);
            assert_eq!(session_activity_epoch, epoch);
            assert_eq!(
                reservation_replayed,
                reservation == SessionRunReservation::AlreadyReserved
            );
        }
        (
            SessionRunReservation::AlreadyActivated {
                session_activity_epoch,
            },
            _,
            SessionRunActivityReceiptPlan::UseActivatedReceipt {
                session_activity_epoch: planned,
            },
        ) => {
            assert_ne!(session_activity_epoch, 0);
            assert_eq!(planned, session_activity_epoch);
        }
        (
            SessionRunReservation::RecoveryClaimed,
            _,
            SessionRunActivityReceiptPlan::RecoveryClaimed,
        )
        | (SessionRunReservation::Completed, _, SessionRunActivityReceiptPlan::Completed) => {}
        (_, _, SessionRunActivityReceiptPlan::RejectInvalidReceipt) => assert!(invalid),
        _ => panic!("reservation/activity plan violated its closed decision table"),
    }
}

/// Bind one already-admitted Session activity to its exact reserved Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRunDelivery {
    pub session_id: crate::SessionId,
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
    RecoveryClaimed {
        session_id: crate::SessionId,
        run_id: RunId,
    },
    Completed {
        session_id: crate::SessionId,
        run_id: RunId,
    },
}

impl AdmittedSessionRun {
    #[must_use]
    pub fn session_id(&self) -> &crate::SessionId {
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
        AdmitSessionRun, SessionRunActivityReceiptPlan, SessionRunCommandFingerprint,
        SessionRunExecutionRequirements, SessionRunReplacement, SessionRunReservation,
        session_run_activity_receipt_plan, session_run_id,
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
    fn prepared_identity_preserves_all_activation_fields_without_changing_ordinary_identity() {
        use super::AdmitPreparedSessionRun;
        use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
        // Cause/effect table: F1 each immutable prepared activation/operation
        // coordinate differs => another digest; F2 trace alone differs => same
        // digest; F3 ordinary input => the original literal v1 tuple/hash.
        // Constraint: prepared input is non-wire; absent inherited sources are
        // distinct from explicit sources and never expanded from current Skills.
        let ordinary = command();
        let expected_ordinary = awaken_agent_contract::content_fingerprint(&(
            "session-command-v1",
            &ordinary.session_id,
            &ordinary.agent_id,
            &ordinary.operation_id,
            &ordinary.run_id,
            &ordinary.messages,
            &ordinary.data_subject_id,
            ordinary.execution_requirements.tool_capability_narrowing,
            &ordinary.execution_requirements.required_worker_capabilities,
            ordinary.replacement,
        ))
        .unwrap();
        assert_eq!(
            ordinary.command_fingerprint(),
            SessionRunCommandFingerprint::from_sha256_hex(&expected_ordinary).unwrap(),
            "F3 old tuple unchanged"
        );
        let original = AdmitPreparedSessionRun {
            session_id: ordinary.session_id.clone(),
            operation_id: ordinary.operation_id.clone(),
            activation: RunActivation::new(
                ordinary.run_id,
                awaken_agent_contract::ThreadId(ordinary.session_id.to_string()),
                ExecutableAgentSnapshot::builder("frozen-worker")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "model", "native",
                        )
                        .expect("valid native immutable input fixture"),
                    )
                    .build(),
                ordinary.messages,
            ),
            traceparent: ordinary.traceparent,
        };
        let mut changes = Vec::new();
        let mut changed = original.clone();
        changed.session_id = "other-session".into();
        changes.push(changed);
        let mut changed = original.clone();
        changed.operation_id = "other-operation".into();
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.run_id.0.push('2');
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.thread_id.0.push('2');
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.snapshot = ExecutableAgentSnapshot::builder("other-worker")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "model", "native")
                    .expect("valid native immutable input fixture"),
            )
            .build();
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.input[0].id.0.push('2');
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.delegation_origin = Some(
            awaken_agent_contract::agent::delegation::DelegationOrigin::root_for_agent(
                RunId("parent".into()),
                "call",
                "coordinator",
            ),
        );
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.model_ref_override = Some("frozen-model".into());
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation.data_subject_id =
            Some(awaken_runtime_contract::DataSubjectId("subject".into()));
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation = changed.activation.without_tools();
        changes.push(changed);
        let mut changed = original.clone();
        changed.activation = changed.activation.with_inherited_toolsets(Some(
            awaken_agent_contract::toolset::InheritedToolsets::with_caller(Vec::new(), None),
        ));
        changes.push(changed);
        for changed in changes {
            assert_ne!(
                original.command_fingerprint(),
                changed.command_fingerprint(),
                "F1 full prepared coordinate"
            );
        }
        let mut retraced = original.clone();
        retraced.traceparent = None;
        assert_eq!(
            original.command_fingerprint(),
            retraced.command_fingerprint(),
            "F2 trace excluded"
        );
    }

    #[test]
    fn session_run_identity_follows_the_idempotency_decision_table() {
        // Cause/effect graph: C1 session identity and C2 operation identity are
        // equal or one differs. Effects: E1 exact retry derives the same RunId;
        // E2 either changed coordinate derives another RunId. Constraints: the
        // derivation is domain-separated and owns no mapping store. Decision
        // rules: R1=C1+C2=>E1; R2=!C1|!C2=>E2.
        let exact = session_run_id(&crate::SessionId::from("session-1"), "operation-1");
        assert_eq!(
            exact,
            session_run_id(&crate::SessionId::from("session-1"), "operation-1"),
            "R1/E1"
        );
        assert_ne!(
            exact,
            session_run_id(&crate::SessionId::from("session-2"), "operation-1"),
            "R2/E2"
        );
        assert_ne!(
            exact,
            session_run_id(&crate::SessionId::from("session-1"), "operation-2"),
            "R2/E2"
        );
    }

    #[test]
    fn reservation_activity_receipt_decision_table_is_closed() {
        // Causes: fresh/replayed reservation, missing/exact/zero receipt, or an
        // already activated/recovery/complete row. Effects: only a reserved row
        // with no receipt requests one commit; exact non-zero receipts are
        // reused byte-for-byte; zero rejects; non-reserved terminal/recovery
        // classes never open activity. The Kani companion exhausts all u64s.
        assert_eq!(
            session_run_activity_receipt_plan(SessionRunReservation::Reserved, None),
            SessionRunActivityReceiptPlan::CommitActivity {
                reservation_replayed: false,
            }
        );
        assert_eq!(
            session_run_activity_receipt_plan(SessionRunReservation::AlreadyReserved, Some(7),),
            SessionRunActivityReceiptPlan::UseReceipt {
                session_activity_epoch: 7,
                reservation_replayed: true,
            }
        );
        assert_eq!(
            session_run_activity_receipt_plan(SessionRunReservation::Reserved, Some(0)),
            SessionRunActivityReceiptPlan::RejectInvalidReceipt
        );
        assert_eq!(
            session_run_activity_receipt_plan(
                SessionRunReservation::AlreadyActivated {
                    session_activity_epoch: 9,
                },
                Some(7),
            ),
            SessionRunActivityReceiptPlan::UseActivatedReceipt {
                session_activity_epoch: 9,
            }
        );
        assert_eq!(
            session_run_activity_receipt_plan(SessionRunReservation::RecoveryClaimed, None),
            SessionRunActivityReceiptPlan::RecoveryClaimed
        );
        assert_eq!(
            session_run_activity_receipt_plan(SessionRunReservation::Completed, None),
            SessionRunActivityReceiptPlan::Completed
        );
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
        let fingerprint = original.command_fingerprint();
        assert!(fingerprint.is_current(), "F1/E1");
        assert!(
            fingerprint
                .as_str()
                .starts_with(SessionRunCommandFingerprint::CURRENT_PREFIX),
            "F1/E1"
        );
        assert_eq!(fingerprint, original.command_fingerprint(), "F1/E1");

        let mut retraced = original.clone();
        retraced.traceparent =
            Some("00-cccccccccccccccccccccccccccccccc-dddddddddddddddd-01".into());
        assert_eq!(fingerprint, retraced.command_fingerprint(), "F2/E1");

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
            assert_ne!(fingerprint, changed.command_fingerprint(), "F3/E2");
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
