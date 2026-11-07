// @formal entrypoint session_event_batch.exact_retry_replays_and_changed_retry_conflicts
// @formal entrypoint session_event_batch.final_non_admission_excludes_late_acceptance
// @formal entrypoint session_event_batch.idempotency_key_appends_at_most_once
// @formal entrypoint session_event_batch.idempotent_receipt_is_admission_stable
// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint tool_result.one_pending_occurrence_has_one_admitted_reply
//! Admission and reconciliation of the Session root's durable Event batches.
//!
//! This module coordinates existing owners only. It does not persist an Event
//! log, execute a Run, or retain a completion registry.

use crate::mutation::ROOT_CAS_ATTEMPTS;
use crate::{SessionProjectionReads as _, SessionRootReads as _};
use awaken_agent_contract::agent::message::{Id as MessageId, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::SessionThreadTarget;
use awaken_session_contract::{
    OUTCOME_BUSY_CODE, SessionEventBatch, SessionEventBatchAdmissionAction,
    SessionEventBatchAdmissionClass, SessionEventBatchAdmissionPlan, SessionEventCommand,
    SessionEventInput, SessionEventProjectionAnchor, SessionUserRunCommand,
    classify_session_event_batch_admission, resolve_session_event_batch_admission,
    session_event_batch_id,
};
use awaken_session_contract::{PersistedSession, SessionEventHistory as EventHistory};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionAgentCoordination, SessionRunActivation,
    admit_event_batch_topology,
};
use awaken_session_types::SessionRevision;
use awaken_time::WallClockExt as _;

use std::sync::Arc;

use super::{SessionMutationError, mutation::repository_failure};

mod selection;

use selection::{adjacent_system_input, preceding_event_runtime_target, select_session_event};

/// Maximum number of durable Event commands advanced in one supervisor slice.
/// This is a recovery scheduling bound, independent of create-time admission.
const EVENT_BATCH_RECONCILIATION_STEPS: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub enum SessionEventBatchIdempotency {
    Absent,
    Exact(SessionEventBatch),
    NoOp,
    Conflict,
}

/// A no-op has an accepted command receipt but no retained Event batch.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEventBatchResult {
    Batch(SessionEventBatch),
    NoOp,
}

impl SessionEventBatchResult {
    pub fn as_batch(&self) -> Option<&SessionEventBatch> {
        match self {
            Self::Batch(batch) => Some(batch),
            Self::NoOp => None,
        }
    }

    pub fn into_batch(self) -> Option<SessionEventBatch> {
        match self {
            Self::Batch(batch) => Some(batch),
            Self::NoOp => None,
        }
    }

    fn require_accepted(&self) -> Result<(), RunError> {
        match self {
            Self::Batch(batch) => batch
                .require_accepted()
                .map_err(|_| awaken_session_contract::event_admission_closed()),
            Self::NoOp => Ok(()),
        }
    }
}

impl From<&SessionEventBatchResult> for awaken_session_contract::SessionEventAdmission {
    fn from(result: &SessionEventBatchResult) -> Self {
        match result {
            SessionEventBatchResult::Batch(batch) => batch.into(),
            SessionEventBatchResult::NoOp => Self::NoOp,
        }
    }
}

enum EventBatchAdmission {
    Accept {
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    },
    Close {
        key: String,
        request_fingerprint: String,
    },
}

impl EventBatchAdmission {
    fn record(
        &self,
        mutations: &crate::SessionMutations,
        runtime_incarnation: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> awaken_session_contract::IdempotencyRecord {
        if let Some((key, fingerprint)) = self.idempotency() {
            return awaken_session_contract::IdempotencyRecord::event_admission(key, fingerprint);
        }
        let fresh = mutations.fresh_mutation_record(
            runtime_incarnation,
            session_id.as_str(),
            "event-batch",
        );
        awaken_session_contract::IdempotencyRecord::event_admission(&fresh.key, &fresh.payload_hash)
    }

    fn action(&self) -> SessionEventBatchAdmissionAction {
        match self {
            Self::Accept { .. } => SessionEventBatchAdmissionAction::Accept,
            Self::Close { .. } => SessionEventBatchAdmissionAction::Close,
        }
    }

    fn idempotency(&self) -> Option<(&str, &str)> {
        match self {
            Self::Accept { idempotency, .. } => idempotency
                .as_ref()
                .map(|(key, fingerprint)| (key.as_str(), fingerprint.as_str())),
            Self::Close {
                key,
                request_fingerprint,
            } => Some((key, request_fingerprint)),
        }
    }
}

struct RetainedEventBatchAdmission<'a> {
    batch: Option<&'a SessionEventBatch>,
    fingerprint_matches: bool,
    admission_closed: bool,
    no_op: bool,
}

impl RetainedEventBatchAdmission<'_> {
    fn classification(&self) -> SessionEventBatchAdmissionClass {
        classify_session_event_batch_admission(
            self.batch.is_some() || self.no_op,
            self.fingerprint_matches,
            self.admission_closed,
        )
    }

    fn resolution(
        &self,
        action: SessionEventBatchAdmissionAction,
    ) -> SessionEventBatchAdmissionPlan {
        resolve_session_event_batch_admission(
            self.batch.is_some() || self.no_op,
            self.fingerprint_matches,
            self.admission_closed,
            action,
        )
    }
}

/// The inputs a retained batch keeps once the budget pause has absorbed its
/// Interrupts.
fn retained_event_inputs(
    inputs: &[SessionEventInput],
    interrupts_absorbed: bool,
) -> Vec<SessionEventInput> {
    inputs
        .iter()
        .filter(|input| !(interrupts_absorbed && matches!(input, SessionEventInput::Interrupt(_))))
        .cloned()
        .collect()
}

fn snapshot_has_no_live_run(
    snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    known_absent: bool,
) -> Result<bool, RunError> {
    let Some(snapshot) = snapshot else {
        return Ok(known_absent);
    };
    let _ = snapshot.current_run().map_err(|error| {
        RunError::classified("session_event_snapshot_invalid", error.to_string())
    })?;
    if snapshot.runs.is_empty() {
        return Ok(known_absent && snapshot.resume_tickets.is_empty());
    }
    Ok(snapshot
        .runs
        .iter()
        .all(|run| matches!(run.state, RunState::Ended(_))))
}

fn retained_event_batch_admission<'a>(
    batches: &'a [SessionEventBatch],
    external_key: Option<&str>,
    record: &awaken_session_contract::IdempotencyRecord,
    receipt: Option<&awaken_session_contract::SessionIdempotencyReceipt>,
    revision: SessionRevision,
) -> Result<RetainedEventBatchAdmission<'a>, RunError> {
    use awaken_session_contract::SessionCommandReceiptKind as Kind;
    let invalid = || {
        RunError::classified(
            "session_event_receipt_invalid",
            "Event receipt contradicts its retained Session facts",
        )
    };
    let batch = if let Some(key) = external_key {
        batches
            .iter()
            .find(|batch| batch.idempotency_key.as_deref() == Some(key))
    } else {
        receipt
            .filter(|receipt| receipt.kind() == Kind::EventBatch)
            .and_then(|receipt| {
                batches
                    .iter()
                    .find(|batch| batch.admitted_revision == receipt.committed_revision)
            })
    };
    let no_op = receipt.is_some_and(|receipt| receipt.kind() == Kind::EventNoOp);
    if let Some(receipt) = receipt
        && (receipt.committed_revision.0 == 0
            || receipt.committed_revision > revision
            || receipt.kind() == Kind::Root
            || (no_op && batch.is_some())
            || (receipt.kind() == Kind::EventBatch && batch.is_none())
            || batch.is_some_and(|batch| batch.admitted_revision != receipt.committed_revision))
    {
        return Err(invalid());
    }
    Ok(RetainedEventBatchAdmission {
        batch,
        fingerprint_matches: receipt.map_or_else(
            || {
                batch.is_some_and(|batch| {
                    batch.request_fingerprint.as_deref() == Some(record.payload_hash.as_str())
                })
            },
            |receipt| receipt.payload_hash == record.payload_hash,
        ),
        admission_closed: batch.is_some_and(|batch| batch.admission_closed),
        no_op,
    })
}

impl RetainedEventBatchAdmission<'_> {
    fn idempotency(&self) -> SessionEventBatchIdempotency {
        match self.classification() {
            SessionEventBatchAdmissionClass::Absent => SessionEventBatchIdempotency::Absent,
            SessionEventBatchAdmissionClass::Conflict => SessionEventBatchIdempotency::Conflict,
            SessionEventBatchAdmissionClass::Accepted if self.no_op => {
                SessionEventBatchIdempotency::NoOp
            }
            SessionEventBatchAdmissionClass::Accepted | SessionEventBatchAdmissionClass::Closed => {
                SessionEventBatchIdempotency::Exact(
                    self.batch.expect("retained batch receipt").clone(),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EventBatchReconciliation {
    pub settled: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
    pub quarantined: usize,
}

#[derive(Clone, Copy)]
enum EventObservation {
    InputCommitted(SessionEventProjectionAnchor),
    Processed(SessionEventProjectionAnchor),
}

impl EventObservation {
    fn anchor(self) -> SessionEventProjectionAnchor {
        match self {
            Self::InputCommitted(anchor) | Self::Processed(anchor) => anchor,
        }
    }
}

enum EventBatchProgress {
    Advanced,
    Pending,
}

#[derive(Clone)]
struct SelectedSessionEvent {
    batch_id: String,
    event: SessionEventCommand,
    traceparent: Option<String>,
    processed_anchor: Option<SessionEventProjectionAnchor>,
    superseded_tool_reply: Option<FailedToolReplyCoordinate>,
}

#[derive(Clone)]
struct FailedToolReplyCoordinate {
    batch_id: String,
    operation_id: String,
    activity_operation_id: String,
}

#[derive(Clone, Copy)]
enum SessionEventSelection<'a> {
    Normal {
        preferred_batch_id: Option<&'a str>,
    },
    InterruptAfterFailedToolReply {
        batch_id: &'a str,
        operation_id: &'a str,
    },
}

fn committed_message_anchor<'a>(
    snapshot: &'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    message_id: &MessageId,
) -> Result<
    Option<(
        &'a awaken_agent_contract::agent::message::Message,
        SessionEventProjectionAnchor,
    )>,
    RunError,
> {
    if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
        return Err(RunError::unavailable(
            "committed Session Event message has no durable commit coordinate",
        ));
    }
    let Some((message, cursor)) = snapshot
        .messages
        .iter()
        .zip(snapshot.message_commit_cursors.iter().copied())
        .find(|(message, _)| &message.id == message_id)
    else {
        return Ok(None);
    };
    if cursor == 0 || cursor > snapshot.store_cursor {
        return Err(RunError::unavailable(
            "committed Session Event message anchor is outside its snapshot",
        ));
    }
    Ok(Some((
        message,
        SessionEventProjectionAnchor {
            source_commit_cursor: cursor,
        },
    )))
}

/// Session Event batches: append a batch of user, system, tool-reply,
/// Outcome and interrupt events through the root CAS with its exact
/// idempotency, then drive each committed event to its effect and settle it,
/// both on request and from recovery.
///
/// It holds the CAS kernel, the clock and lifecycle signal, the read, Run
/// admission, activity and coordination components an event drives, and the Thread,
/// coordination, streaming and Outcome runtime roles.
pub struct SessionEventBatchCommands {
    runtime_incarnation: String,
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    lifecycle: Arc<crate::SessionLifecycle>,
    queries: Arc<crate::SessionQueries>,
    activities: Arc<crate::SessionActivities>,
    run_admissions: Arc<crate::SessionRunAdmissions>,
    coordination: Arc<crate::SessionCoordination>,
    thread_reads: Arc<dyn awaken_session_execution_contract::ThreadReads>,
    coordination_runtime: Arc<dyn awaken_session_execution_contract::ChildRunCoordination>,
    coordination_reads: Arc<dyn awaken_session_execution_contract::SessionCoordinationReads>,
    streaming_runs: Arc<dyn awaken_session_execution_contract::StreamingRuns>,
    outcomes: Arc<dyn awaken_session_execution_contract::Outcomes>,
}

/// The collaborators one [`SessionEventBatchCommands`] is built from.
pub(crate) struct SessionEventBatchCollaborators {
    pub(crate) runtime_incarnation: String,
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) lifecycle: Arc<crate::SessionLifecycle>,
    pub(crate) queries: Arc<crate::SessionQueries>,
    pub(crate) activities: Arc<crate::SessionActivities>,
    pub(crate) run_admissions: Arc<crate::SessionRunAdmissions>,
    pub(crate) coordination: Arc<crate::SessionCoordination>,
    pub(crate) runtime: crate::SessionExecutionRoles,
}

impl SessionEventBatchCommands {
    pub(crate) fn new(collaborators: SessionEventBatchCollaborators) -> Self {
        let SessionEventBatchCollaborators {
            runtime_incarnation,
            mutations,
            clock,
            lifecycle,
            queries,
            activities,
            run_admissions,
            coordination,
            runtime,
        } = collaborators;
        Self {
            runtime_incarnation,
            mutations,
            clock,
            lifecycle,
            queries,
            activities,
            run_admissions,
            coordination,
            thread_reads: runtime.thread_reads.clone(),
            coordination_runtime: runtime.coordination.clone(),
            coordination_reads: runtime.coordination_reads.clone(),
            streaming_runs: runtime.streaming_runs.clone(),
            outcomes: runtime.outcomes.clone(),
        }
    }

    /// Observe the Session root's one retained Event-batch retry coordinate.
    /// This read exists so a protocol retry can recover its original receipt
    /// before current pending-tool admission; it does not own another receipt.
    pub async fn session_event_batch_idempotency(
        &self,
        session_id: &awaken_session_types::SessionId,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<SessionEventBatchIdempotency, RunError> {
        let record =
            awaken_session_contract::IdempotencyRecord::event_admission(key, request_fingerprint);
        let receipt = self
            .mutations
            .repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_run_error)?;
        let session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_run_error)?;
        Ok(retained_event_batch_admission(
            &session.event_batches,
            Some(key),
            &record,
            receipt.as_ref(),
            session.revision,
        )?
        .idempotency())
    }

    async fn message_projection_anchor(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        message_id: &MessageId,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        let thread_reads = self.thread_reads.as_ref();

        let snapshot = thread_reads
            .session_thread_run_recovery_snapshot(session_id, thread_id, run_id)
            .await?
            .ok_or_else(|| {
                RunError::unavailable(
                    "committed Session Event message owner is not yet recoverable",
                )
            })?;
        if let Some((_, anchor)) = committed_message_anchor(&snapshot, message_id)? {
            return Ok(anchor);
        }
        Err(RunError::unavailable(
            "committed Session Event message is not yet visible in recovery",
        ))
    }

    async fn run_snapshot_projection_anchor(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        let thread_reads = self.thread_reads.as_ref();

        let snapshot = thread_reads
            .session_thread_run_recovery_snapshot(session_id, thread_id, run_id)
            .await?;
        match snapshot {
            Some(snapshot)
                if snapshot
                    .run_record(run_id)
                    .map_err(|error| RunError::internal(error.to_string()))?
                    .is_some() =>
            {
                Ok(SessionEventProjectionAnchor {
                    source_commit_cursor: snapshot.store_cursor,
                })
            }
            _ => Err(RunError::unavailable(
                "committed Session Event Run owner is not yet recoverable",
            )),
        }
    }

    /// Recover the canonical disposable projection, reserve one stable User
    /// Run, then commit/recover its exact Session activity receipt without
    /// making the reservation executable.
    ///
    /// This is the first half of the durable User Run boundary shared by
    /// create-time reconciliation and internal foreground commands. Activation
    /// and optional committed-state observation remain a distinct second stage.
    pub fn admit_session_user_run(
        &self,
        command: SessionUserRunCommand,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AdmittedSessionRun, RunError>> + Send + '_>,
    > {
        Box::pin(async move {
            let session_id = command.session_id.clone();
            let mut messages =
                Vec::with_capacity(1 + usize::from(command.accompanying_system.is_some()));
            if let Some(system) = command.accompanying_system {
                if system.operation_id.trim().is_empty() || system.content.is_empty() {
                    return Err(RunError::bad_request("Session System input is incomplete"));
                }
                messages.push(awaken_agent_contract::agent::message::Message::new(
                    MessageId::session_system(session_id.as_str(), &system.operation_id),
                    Role::System,
                    system.content,
                ));
            }
            if command.operation_id.trim().is_empty() || command.content.is_empty() {
                return Err(RunError::bad_request("Session User input is incomplete"));
            }
            messages.push(awaken_agent_contract::agent::message::Message::new(
                MessageId::session_event_input(session_id.as_str(), &command.operation_id),
                Role::User,
                command.content,
            ));
            self.run_admissions
                .admit_session_run(AdmitSessionRun {
                    session_id,
                    agent_id: command.agent_id,
                    operation_id: command.operation_id,
                    run_id: command.run_id,
                    messages,
                    data_subject_id: command.data_subject_id,
                    traceparent: command.traceparent,
                    execution_requirements: Default::default(),
                    replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
                    private_task_input: None,
                })
                .await
        })
    }

    /// The Event append every caller outside this crate uses. Ownership was
    /// decided once, when `access` was minted; only this crate's own
    /// reconcilers append without it.
    pub async fn append_owned_session_event_batch(
        &self,
        access: &crate::SessionAccess,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatchResult, RunError> {
        self.admit_session_event_batch_idempotent(
            access.session_id(),
            inputs,
            data_subject_id,
            traceparent,
            idempotency,
        )
        .await
    }

    /// The owned form of the request-local append-and-drive.
    pub async fn append_and_drive_owned_session_event_batch(
        &self,
        access: &crate::SessionAccess,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<(SessionEventBatchResult, Option<RunError>), RunError> {
        self.append_and_drive_session_event_batch_idempotent(
            access.session_id(),
            inputs,
            data_subject_id,
            traceparent,
            idempotency,
        )
        .await
    }

    /// Atomically accept one complete ordinary Event batch in the Session root.
    /// Its prospective committed revision is the stable linearization identity.
    /// Acceptance opens no activity: queued work may wait while the Thread is
    /// idle or Awaiting without fabricating a Running Session.
    #[cfg(test)]
    pub(crate) async fn append_session_event_batch(
        &self,
        session_id: &str,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
    ) -> Result<SessionEventBatch, RunError> {
        self.admit_session_event_batch_idempotent(
            &awaken_session_types::SessionId::from(session_id),
            inputs,
            data_subject_id,
            traceparent,
            None,
        )
        .await?
        .into_batch()
        .ok_or_else(|| {
            RunError::classified(
                "session_event_batch_missing",
                "ordinary batch fixture received an accepted no-op",
            )
        })
    }

    /// Atomically append or replay one HTTP-idempotent Event batch. The key and
    /// fingerprint live on the existing root command, so concurrent callers
    /// either observe that exact batch or a conflict under the same root CAS.
    pub(crate) async fn admit_session_event_batch_idempotent(
        &self,
        session_id: &awaken_session_types::SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatchResult, RunError> {
        let batch = self
            .resolve_session_event_batch(
                session_id,
                EventBatchAdmission::Accept {
                    inputs,
                    data_subject_id,
                    traceparent,
                    idempotency,
                },
                true,
            )
            .await?;
        batch
            .require_accepted()
            .map_err(|_| awaken_session_contract::event_admission_closed())?;
        Ok(batch)
    }

    /// Accept through the same root CAS, then perform the request-local drive
    /// before notifying the full recovery sweep. This removes a duplicate race
    /// between the synchronous driver and its own wake while retaining the
    /// periodic recovery authority if this future is lost after acceptance.
    // Request-path state machines in this file return their future already
    // boxed. An unoptimized poll frame reserves a slot for every inline callee
    // it awaits and never reuses it; nested inline, one Managed request needed
    // more than a 2 MiB thread stack (see `tests/stack_boundaries.rs`).
    pub(crate) fn append_and_drive_session_event_batch_idempotent<'a>(
        &'a self,
        session_id: &'a awaken_session_types::SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<(SessionEventBatchResult, Option<RunError>), RunError>,
    > {
        Box::pin(async move {
            let batch = self
                .resolve_session_event_batch(
                    session_id,
                    EventBatchAdmission::Accept {
                        inputs,
                        data_subject_id,
                        traceparent,
                        idempotency,
                    },
                    false,
                )
                .await?;
            batch
                .require_accepted()
                .map_err(|_| awaken_session_contract::event_admission_closed())?;
            let Some(retained) = batch.as_batch() else {
                return Ok((batch, None));
            };
            let drive_error = self
                .drive_session_event_batches(session_id, Some(&retained.batch_id))
                .await
                .err();
            // The full sweep remains the one retry owner. Waking after the local
            // attempt preserves queued predecessors and retryable failures without
            // racing this request's append/admission/activation root mutations.
            self.lifecycle.request_reconciliation();
            Ok((batch, drive_error))
        })
    }

    /// Resolve response loss without replaying execution. If acceptance already
    /// won, return its exact receipt. Otherwise close the SAME identity under
    /// root CAS so a delayed request can never arrive after quota is released.
    pub async fn close_session_event_admission(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        key: String,
        request_fingerprint: String,
    ) -> Result<awaken_session_contract::SessionEventAdmission, RunError> {
        if let Some(deleted) = self
            .mutations
            .repository()
            .deleted_event_admissions(owner_scope, session_id)
            .await
            .map_err(repository_run_error)?
        {
            return awaken_session_contract::resolve_deleted_event_admission(
                &deleted,
                &key,
                &request_fingerprint,
            );
        }
        self.queries.authorize(owner_scope, session_id).await?;
        let result = self
            .resolve_session_event_batch(
                session_id,
                EventBatchAdmission::Close {
                    key: key.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                },
                true,
            )
            .await;
        match result {
            Ok(batch) => Ok((&batch).into()),
            Err(error) => {
                // Delete may win between the live lookup and root CAS. Read
                // the SAME owner-bound deletion authority before classifying.
                match self
                    .mutations
                    .repository()
                    .deleted_event_admissions(owner_scope, session_id)
                    .await
                    .map_err(repository_run_error)?
                {
                    Some(deleted) => awaken_session_contract::resolve_deleted_event_admission(
                        &deleted,
                        &key,
                        &request_fingerprint,
                    ),
                    None => Err(error),
                }
            }
        }
    }

    /// Read through the one mutation owner on each caller's CAS attempt.
    /// These remain two ordered reads, not an atomic owner/root snapshot.
    async fn load_event_root(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(String, awaken_session_contract::SessionAggregate), RunError> {
        let owner = self
            .mutations
            .owner(session_id)
            .await
            .map_err(mutation_run_error)?;
        let session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_run_error)?;
        Ok((owner, session))
    }

    /// The Interrupts of `inputs` that the budget pause absorbs on this root:
    /// all of them or none. A batch of only absorbed Interrupts is an Event
    /// no-op; any other batch is retained without them.
    async fn budget_pause_absorbed_interrupts(
        &self,
        session: &PersistedSession,
        inputs: &[SessionEventInput],
    ) -> Result<Vec<awaken_session_contract::SessionEventInterrupt>, RunError> {
        let interrupts = inputs
            .iter()
            .filter_map(|input| match input {
                SessionEventInput::Interrupt(interrupt) => Some(interrupt.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if interrupts.is_empty() || !self.budget_pause_absorbs_interrupt(session).await? {
            return Ok(Vec::new());
        }
        Ok(interrupts)
    }

    /// The budget-pause interrupt law, decided by the aggregate over the Thread
    /// evidence gathered here. No snapshot is read while the root alone refuses.
    async fn budget_pause_absorbs_interrupt(
        &self,
        session: &PersistedSession,
    ) -> Result<bool, RunError> {
        if !awaken_session_contract::session_budget_pause_may_absorb_interrupt(session) {
            return Ok(false);
        }
        let no_live_run_proven = self.no_live_run_proven(session).await?;
        Ok(
            awaken_session_contract::session_budget_pause_absorbs_interrupt(
                session,
                no_live_run_proven,
            ),
        )
    }

    /// Committed root and child snapshots show no live Run, and the full prefix
    /// validator proves they stayed current. Unknown sources prove nothing.
    async fn no_live_run_proven(&self, session: &PersistedSession) -> Result<bool, RunError> {
        let root = self
            .queries
            .projection_thread_snapshot(&session.session_id, session.session_id.as_str(), None)
            .await?;
        let (links, children) = self
            .queries
            .projection_coordinated_prefix(&session.session_id, root.as_ref())
            .await?;
        let known_absent = |thread: &str| {
            session
                .run_activities()
                .is_some_and(|(records, unrecorded)| {
                    !unrecorded
                        && records
                            .iter()
                            .all(|record| record.thread_id().0.as_str() != thread)
                })
        };
        if !snapshot_has_no_live_run(root.as_ref(), known_absent(session.session_id.as_str()))? {
            return Ok(false);
        }
        for link in &links {
            if !snapshot_has_no_live_run(
                children.get(&link.thread_id.0),
                link.latest_run_id.is_none() && known_absent(&link.thread_id.0),
            )? {
                return Ok(false);
            }
        }
        Ok(!self
            .queries
            .projection_prefix_moved(
                session,
                awaken_session_execution_contract::SessionProjectionPrefix {
                    root: root.as_ref(),
                    links: &links,
                    children: &children,
                },
            )
            .await?)
    }

    fn resolve_session_event_batch<'a>(
        &'a self,
        session_id: &'a awaken_session_types::SessionId,
        admission: EventBatchAdmission,
        wake_after_accept: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<SessionEventBatchResult, RunError>> {
        Box::pin(async move {
            let mutations = &self.mutations;
            let (thread_reads, coordination) =
                (self.thread_reads.as_ref(), &*self.coordination_reads);
            let record = admission.record(mutations, &self.runtime_incarnation, session_id);

            for attempt in 0..ROOT_CAS_ATTEMPTS {
                let receipt = mutations
                    .repository()
                    .idempotency_receipt(session_id, &record.key)
                    .await
                    .map_err(repository_run_error)?;
                let (owner, mut session) = self.load_event_root(session_id).await?;
                let retained = retained_event_batch_admission(
                    &session.event_batches,
                    admission.idempotency().map(|(key, _)| key),
                    &record,
                    receipt.as_ref(),
                    session.revision,
                )?;
                let plan = retained.resolution(admission.action());
                match plan {
                    SessionEventBatchAdmissionPlan::ReplayAccepted
                    | SessionEventBatchAdmissionPlan::ReplayClosed => {
                        if retained.no_op {
                            return Ok(SessionEventBatchResult::NoOp);
                        }
                        return Ok(SessionEventBatchResult::Batch(
                            retained
                                .batch
                                .expect("replay admission plan has one retained batch")
                                .clone(),
                        ));
                    }
                    SessionEventBatchAdmissionPlan::RejectClosed => {
                        return Err(awaken_session_contract::event_admission_closed());
                    }
                    SessionEventBatchAdmissionPlan::Conflict => {
                        return Err(awaken_session_contract::event_admission_conflict());
                    }
                    SessionEventBatchAdmissionPlan::AppendAccepted
                    | SessionEventBatchAdmissionPlan::AppendClosed => {}
                }
                if session.is_terminal() && plan == SessionEventBatchAdmissionPlan::AppendAccepted {
                    return Err(RunError::bad_request(
                        "Session no longer accepts Event batches",
                    ));
                }
                let mut interrupts_absorbed = false;
                if let EventBatchAdmission::Accept { inputs, .. } = &admission {
                    for input in inputs {
                        if let SessionEventInput::Interrupt(interrupt) = input {
                            interrupt.validate()?;
                        }
                    }
                    // The shared Thread law applies to every internal/protocol
                    // caller before either retaining a batch or acknowledging no-op.
                    admit_event_batch_topology(coordination, thread_reads, session_id, inputs)
                        .await?;
                    let interrupts = self
                        .budget_pause_absorbed_interrupts(&session, inputs)
                        .await?;
                    interrupts_absorbed = !interrupts.is_empty();
                    if interrupts_absorbed && interrupts.len() == inputs.len() {
                        let result = mutations
                            .commit_mutation(
                                &owner,
                                awaken_session_contract::SessionMutation {
                                    expected_revision: session.revision,
                                    idempotency: record.clone(),
                                    payload:
                                        awaken_session_contract::SessionMutationPayload::EventNoOp {
                                            session_id: session_id.clone(),
                                            interrupts,
                                        },
                                    lifecycle_facts: Vec::new(),
                                },
                            )
                            .await
                            .map_err(mutation_run_error)?;
                        match result {
                            awaken_session_contract::SessionMutationResult::AcknowledgedNoOp {
                                observed_revision,
                            } if observed_revision.0 > 0 => {
                                return Ok(SessionEventBatchResult::NoOp);
                            }
                            awaken_session_contract::SessionMutationResult::Replayed { .. }
                            | awaken_session_contract::SessionMutationResult::Conflict { .. } => {
                                continue;
                            }
                            awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
                                return Err(awaken_session_contract::event_admission_conflict());
                            }
                            _ => {
                                return Err(RunError::classified(
                                    "session_event_receipt_invalid",
                                    "Repository returned a root mutation for an Event no-op",
                                ));
                            }
                        }
                    }
                }
                let admission_projection_anchor =
                    if plan == SessionEventBatchAdmissionPlan::AppendAccepted {
                        let source_commit_cursor = thread_reads
                            .session_thread_recovery_snapshot(session_id, session_id.as_str())
                            .await?
                            .map_or(0, |snapshot| snapshot.store_cursor);
                        Some(SessionEventProjectionAnchor {
                            source_commit_cursor,
                        })
                    } else {
                        None
                    };
                let committed_revision = SessionRevision(
                    session
                        .revision
                        .0
                        .checked_add(1)
                        .ok_or_else(|| RunError::internal("Session revision is exhausted"))?,
                );
                let batch_id = session_event_batch_id(session_id, committed_revision)?;
                let mut batch = match (plan, &admission) {
                    (
                        SessionEventBatchAdmissionPlan::AppendAccepted,
                        EventBatchAdmission::Accept {
                            inputs,
                            data_subject_id,
                            traceparent,
                            ..
                        },
                    ) => SessionEventBatch::compile_attributed(
                        session_id,
                        batch_id,
                        retained_event_inputs(inputs, interrupts_absorbed),
                        data_subject_id.clone(),
                        traceparent.clone(),
                    ),
                    (
                        SessionEventBatchAdmissionPlan::AppendClosed,
                        EventBatchAdmission::Close {
                            key,
                            request_fingerprint,
                        },
                    ) => SessionEventBatch::closed_admission(
                        batch_id,
                        committed_revision,
                        key.clone(),
                        request_fingerprint.clone(),
                    ),
                    _ => unreachable!("admission plan is derived from the requested action"),
                }?;
                batch.admitted_revision = committed_revision;
                if let Some(anchor) = admission_projection_anchor {
                    batch
                        .anchor_admission_projection(anchor)
                        .map_err(|error| RunError::internal(error.to_string()))?;
                }
                if !batch.admission_closed
                    && let Some((key, request_fingerprint)) = admission.idempotency()
                {
                    batch.bind_idempotency(key.to_owned(), request_fingerprint.to_owned())?;
                }
                batch.validate_tool_reply_occupancy(&session.event_batches)?;
                session
                    .events()
                    .append_batch(batch.clone())
                    .map_err(|error| RunError::internal(error.to_string()))?;
                match Box::pin(crate::SessionMutations::commit_observed_snapshot(
                    mutations.repository().as_ref(),
                    &owner,
                    &mut session,
                    record.clone(),
                    Vec::new(),
                    self.clock.now_ms(),
                ))
                .await
                {
                    Ok(awaken_session_contract::SessionMutationResult::Applied {
                        new_revision,
                    }) if new_revision == committed_revision => {
                        if !batch.admission_closed && wake_after_accept {
                            self.lifecycle.request_reconciliation();
                        }
                        return Ok(SessionEventBatchResult::Batch(batch));
                    }
                    Ok(awaken_session_contract::SessionMutationResult::AcknowledgedNoOp {
                        observed_revision,
                    }) if observed_revision.0 > 0 => return Ok(SessionEventBatchResult::NoOp),
                    Ok(awaken_session_contract::SessionMutationResult::Replayed { .. }) => {
                        continue;
                    }
                    Ok(awaken_session_contract::SessionMutationResult::Conflict { .. })
                        if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                    Ok(awaken_session_contract::SessionMutationResult::IdempotencyMismatch) => {
                        return Err(awaken_session_contract::event_admission_conflict());
                    }
                    Ok(awaken_session_contract::SessionMutationResult::Conflict { .. }) => {
                        return Err(RunError::unavailable(
                            "Session Event state changed concurrently",
                        ));
                    }
                    Ok(_) => {
                        return Err(RunError::classified(
                            "session_event_receipt_invalid",
                            "Repository returned an invalid Event receipt revision",
                        ));
                    }
                    Err(error) => return Err(repository_run_error(error)),
                }
            }
            Err(RunError::unavailable(
                "Session Event batch admission changed concurrently",
            ))
        })
    }

    /// Revisit every unfinished root-owned Event batch through the one Session
    /// recovery scan. One Session is advanced until it reaches an external Run
    /// boundary; no process-local task owns later progress.
    #[cfg(test)]
    pub(crate) async fn reconcile_event_batches(&self) -> EventBatchReconciliation {
        let candidates = match super::recovery_scan::recovery_candidates(
            self.mutations.repository().as_ref(),
            self.clock.as_ref(),
            true,
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                let mut report = EventBatchReconciliation::default();
                report
                    .failures
                    .push(("<repository>".into(), error.to_string()));
                return report;
            }
        };
        self.reconcile_event_batches_from(&candidates).await
    }

    pub(crate) async fn reconcile_event_batches_from(
        &self,
        candidates: &super::SessionRecoveryCandidates,
    ) -> EventBatchReconciliation {
        let mut report = EventBatchReconciliation {
            quarantined: candidates.quarantined.len(),
            ..Default::default()
        };
        // Shared bounded iteration owns only scheduling; this selector retains
        // per-Session FIFO, root CAS, Runtime admission and interruption order.
        let completed = super::recovery_sweep::map_candidates(
            candidates,
            super::recovery_sweep::EVENT_RECOVERY_CONCURRENCY,
            |candidate| async move {
                let mut result = EventBatchReconciliation::default();
                if !candidate.interests.event_batches {
                    return result;
                }
                let session = match self
                    .mutations
                    .session_aggregate(&candidate.session_id)
                    .await
                {
                    Ok(session) => session,
                    Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                        return result;
                    }
                    Err(error) => {
                        result
                            .failures
                            .push((candidate.session_id.to_string(), error.to_string()));
                        return result;
                    }
                };
                if !session.needs_event_reconciliation() {
                    return result;
                }
                result.pending = 1;
                let session_id = session.session_id.clone();
                match self
                    .reconcile_session_event_batches(
                        &session_id,
                        None,
                        Some(session.into_snapshot()),
                    )
                    .await
                {
                    Ok(true) => result.settled = 1,
                    Ok(false) => {}
                    Err(error) => result
                        .failures
                        .push((String::from(session_id), error.to_string())),
                }
                result
            },
        )
        .await;
        for result in completed {
            report.pending += result.pending;
            report.settled += result.settled;
            report.failures.extend(result.failures);
        }
        report
    }

    /// Opportunistically drive receipt commands from one just-admitted batch.
    /// `None` is reserved for recovery/tests that intentionally exercise the
    /// lifecycle selector over all retained batches. A concrete batch never
    /// re-drives an older Outcome command and therefore cannot wait behind that
    /// Outcome's Worker/Judge execution before delivering its own interrupt.
    pub async fn drive_session_event_batches(
        &self,
        session_id: &awaken_session_types::SessionId,
        preferred_batch_id: Option<&str>,
    ) -> Result<(), RunError> {
        let _ = self
            .reconcile_session_event_batches(session_id, preferred_batch_id, None)
            .await?;
        Ok(())
    }

    fn reconcile_session_event_batches<'a>(
        &'a self,
        session_id: &'a awaken_session_types::SessionId,
        preferred_batch_id: Option<&'a str>,
        mut initial_session: Option<PersistedSession>,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RunError>> {
        Box::pin(async move {
            // Bound one supervisor slice. Retained history may contain many batches,
            // but each Advanced result persists one entry and a later scan resumes.
            for _ in 0..EVENT_BATCH_RECONCILIATION_STEPS {
                let session = match initial_session.take() {
                    Some(session) => session,
                    None => self
                        .mutations
                        .repository()
                        .get(session_id)
                        .await
                        .map_err(repository_run_error)?,
                };
                if session.is_terminal() {
                    return self.resolve_terminal_event_batches(session_id).await;
                }
                let selected = select_session_event(
                    &session,
                    SessionEventSelection::Normal { preferred_batch_id },
                );
                let Some(selected) = selected else {
                    return Ok(true);
                };
                let failed_tool_reply = match &selected.event {
                    SessionEventCommand::ToolReply { operation_id, .. }
                        if preferred_batch_id.is_none() =>
                    {
                        Some((selected.batch_id.clone(), operation_id.clone()))
                    }
                    _ => None,
                };
                match self.reconcile_one_session_event(&session, selected).await {
                    Ok(EventBatchProgress::Advanced) => {}
                    Ok(EventBatchProgress::Pending) => return Ok(false),
                    Err(reply_error) => {
                        let Some((batch_id, operation_id)) = failed_tool_reply else {
                            return Err(reply_error);
                        };
                        // A failed ToolReply can depend on damaged Runtime reply
                        // authority forever. Reload the root after that exact
                        // failure and let only a later retained Interrupt cross it.
                        // The normal selector remains retained-order FIFO; this is
                        // a recovery escape, not an alternate event scheduler.
                        let fresh = self
                            .mutations
                            .repository()
                            .get(session_id)
                            .await
                            .map_err(repository_run_error)?;
                        if fresh.is_terminal() {
                            let _ = self.resolve_terminal_event_batches(session_id).await?;
                            return Err(reply_error);
                        }
                        let Some(interrupt) = select_session_event(
                            &fresh,
                            SessionEventSelection::InterruptAfterFailedToolReply {
                                batch_id: &batch_id,
                                operation_id: &operation_id,
                            },
                        ) else {
                            return Err(reply_error);
                        };
                        match self.reconcile_one_session_event(&fresh, interrupt).await {
                            Ok(EventBatchProgress::Advanced) => {
                                // Preserve the failed reply's retry signal only after
                                // the Interrupt effect and processed-root CAS succeed.
                                return Err(reply_error);
                            }
                            Ok(EventBatchProgress::Pending) => {
                                return Err(RunError::unavailable(
                                    "recovery Interrupt remained pending after a failed ToolReply",
                                ));
                            }
                            Err(interrupt_error) => return Err(interrupt_error),
                        }
                    }
                }
            }
            Ok(false)
        })
    }

    /// Close only the legacy race in which an old writer terminalized a root
    /// after accepting, but before anchoring, an Event batch. The canonical
    /// supervisor marks those receipts resolved under root CAS and never calls
    /// any Runtime effect after terminal state.
    fn resolve_terminal_event_batches<'a>(
        &'a self,
        session_id: &'a awaken_session_types::SessionId,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RunError>> {
        Box::pin(async move {
            let mutations = &self.mutations;
            for attempt in 0..ROOT_CAS_ATTEMPTS {
                let (owner_scope, mut session) = self.load_event_root(session_id).await?;
                if !session.is_terminal() {
                    return Ok(false);
                }
                let terminal_cleanup = session
                    .verified_terminal_cleanup()
                    .map_err(|error| RunError::internal(error.to_string()))?;
                if !terminal_cleanup.is_requested() && !terminal_cleanup.is_completed() {
                    // The fence has not yet joined every root/child Runtime
                    // writer. Resolving now would misclassify a current row as
                    // legacy and could place accepted input before an unstable
                    // terminal high-water.
                    return Ok(false);
                }
                let resolved = session
                    .events()
                    .resolve_terminally(self.clock.now_ms())
                    .map_err(|error| RunError::internal(error.to_string()))?;
                if resolved == 0 {
                    return Ok(true);
                }
                match mutations
                    .commit_session_snapshot_with_outcome(
                        &owner_scope,
                        session,
                        "resolve-terminal-event-batches",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(_) => return Ok(true),
                    Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                    Err(error) => return Err(mutation_run_error(error)),
                }
            }
            Err(RunError::unavailable(
                "terminal Session Event-batch resolution conflicted repeatedly",
            ))
        })
    }

    fn reconcile_one_session_event<'a>(
        &'a self,
        session: &'a PersistedSession,
        selected: SelectedSessionEvent,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<EventBatchProgress, RunError>> + Send + 'a>,
    > {
        let coordination_runtime = self.coordination_runtime.as_ref();

        let thread_reads = self.thread_reads.as_ref();

        // This reconciliation is polled by the lifecycle supervisor, whose
        // recovery future is already deep. Select the exact Event kind before
        // constructing its boxed future: one async block spanning every Event
        // variant makes Rust's debug poll frame reserve the largest temporary
        // union on every path, even when only a User message is running. The
        // variant futures retain this sole selector and all existing durable
        // authorities; only their execution-shaped stack frames are isolated.
        let SelectedSessionEvent {
            batch_id,
            event,
            traceparent,
            processed_anchor,
            superseded_tool_reply,
        } = selected;
        if let Some(anchor) = processed_anchor {
            return Box::pin(async move {
                // A prior recovery attempt may have durably anchored the exact
                // later Interrupt before closing the failed reply. Reuse that
                // root provenance without repeating the Runtime effect. Settle
                // the reply's one exact transferred activity before closing
                // its pending root provenance; a crash between these mutations
                // therefore leaves a retryable command rather than a leaked
                // Running epoch with no remaining recovery trigger.
                if let Some(failed) = superseded_tool_reply.as_ref() {
                    self.settle_superseded_tool_reply_activity(session.session_id.as_str(), failed)
                        .await?;
                }
                self.observe_session_event(
                    &session.session_id,
                    &batch_id,
                    event.operation_id(),
                    EventObservation::Processed(anchor),
                    superseded_tool_reply.as_ref(),
                )
                .await?;
                Ok(EventBatchProgress::Advanced)
            });
        }
        match event {
            SessionEventCommand::UserMessage {
                operation_id,
                run_id,
                content,
                data_subject_id,
            } => Box::pin(async move {
                if let Some(progress) = self
                    .observe_user_run(session, &batch_id, &operation_id, &run_id, &content)
                    .await?
                {
                    return Ok(progress);
                }
                if self
                    .reconcile_blocking_root_run(&session.session_id, &run_id)
                    .await?
                {
                    return Ok(EventBatchProgress::Pending);
                }

                let accompanying_system = adjacent_system_input(session, &batch_id, &operation_id);
                let admission = self
                    .admit_session_user_run(SessionUserRunCommand {
                        session_id: session.session_id.clone(),
                        agent_id: session
                            .agent_id()
                            .ok_or_else(|| RunError::internal("Session Agent is not frozen"))?
                            .to_string(),
                        operation_id: operation_id.clone(),
                        run_id: run_id.clone(),
                        content: content.clone(),
                        accompanying_system,
                        data_subject_id,
                        traceparent,
                    })
                    .await?;
                self.settle_event_batch_wake(session, &batch_id).await?;

                let activation = match admission {
                    AdmittedSessionRun::Reserved(delivery)
                    | AdmittedSessionRun::AlreadyReserved(delivery)
                    | AdmittedSessionRun::AlreadyActivated(delivery) => {
                        coordination_runtime.activate_session_run(delivery).await?
                    }
                    AdmittedSessionRun::RecoveryClaimed { .. } => {
                        return Ok(EventBatchProgress::Pending);
                    }
                    AdmittedSessionRun::Completed { .. } => SessionRunActivation::Completed,
                };
                match activation {
                    SessionRunActivation::Activated
                    | SessionRunActivation::AlreadyActivated {
                        session_activity_epoch: _,
                    }
                    | SessionRunActivation::RecoveryClaimed => Ok(EventBatchProgress::Pending),
                    SessionRunActivation::Completed => Ok(self
                        .observe_user_run(session, &batch_id, &operation_id, &run_id, &content)
                        .await?
                        .unwrap_or(EventBatchProgress::Pending)),
                }
            }),
            SessionEventCommand::SystemMessage {
                operation_id,
                content,
            } => Box::pin(async move {
                let message_id =
                    MessageId::session_system(session.session_id.as_str(), &operation_id);
                let committed = thread_reads
                    .committed_messages(session.session_id.as_str())
                    .await?;
                let Some(message) = committed.iter().find(|message| message.id == message_id)
                else {
                    return Ok(EventBatchProgress::Pending);
                };
                if message.role != Role::System || message.content != content {
                    return Err(RunError::internal(
                        "committed Session System input conflicts with root command intent",
                    ));
                }
                let (thread_id, run_id) = preceding_event_runtime_target(
                    session,
                    &batch_id,
                    &operation_id,
                )
                .ok_or_else(|| {
                    RunError::internal("committed Session System input has no retained Run owner")
                })?;
                let anchor = self
                    .message_projection_anchor(
                        &session.session_id,
                        &thread_id.0,
                        &run_id,
                        &message_id,
                    )
                    .await?;
                self.observe_processed_event_after_wake(session, &batch_id, &operation_id, anchor)
                    .await
            }),
            SessionEventCommand::DefineOutcome {
                operation_id,
                outcome_id,
                description,
                rubric,
                max_iterations,
            } => Box::pin(async move {
                let source_commit_cursor = match self
                    .outcomes
                    .prepare_outcome(
                        session.session_id.as_str(),
                        &outcome_id,
                        &description,
                        rubric.execution_reference(),
                        max_iterations.unwrap_or(3),
                    )
                    .await
                {
                    Ok(source_commit_cursor) => source_commit_cursor,
                    Err(error) if error.code == OUTCOME_BUSY_CODE => {
                        return Ok(EventBatchProgress::Pending);
                    }
                    Err(error) => return Err(error),
                };
                self.observe_processed_event_after_wake(
                    session,
                    &batch_id,
                    &operation_id,
                    SessionEventProjectionAnchor {
                        source_commit_cursor,
                    },
                )
                .await
            }),
            SessionEventCommand::ToolReply {
                operation_id,
                reply,
            } => Box::pin(async move {
                let answered_pending_anchor = reply
                    .answered_pending_commit_cursor
                    .filter(|cursor| *cursor != 0)
                    .map(|source_commit_cursor| SessionEventProjectionAnchor {
                        source_commit_cursor,
                    });
                let accompanying_system = adjacent_system_input(session, &batch_id, &operation_id);
                SessionAgentCoordination::reply_session_thread_tool(
                    self.coordination.as_ref(),
                    reply.delivery_command(&session.session_id, accompanying_system),
                )
                .await?;
                let target_thread = reply.target.thread_id(&session.session_id);
                // Current admissions freeze the Awaiting commit before
                // delivery. Legacy rows lack it and retain the old
                // post-delivery recovery fallback for compatibility.
                let anchor = match answered_pending_anchor {
                    Some(anchor) => anchor,
                    None => {
                        self.run_snapshot_projection_anchor(
                            &session.session_id,
                            &target_thread.0,
                            &reply.expected_run_id,
                        )
                        .await?
                    }
                };
                self.observe_processed_event_after_wake(session, &batch_id, &operation_id, anchor)
                    .await
            }),
            SessionEventCommand::Interrupt {
                operation_id,
                interrupt,
            } => Box::pin(async move {
                let mut first_error = None;
                let targets = interrupt.targets;
                for target in &targets {
                    let result = match target {
                        SessionThreadTarget::Primary => {
                            self.streaming_runs
                                .interrupt(session.session_id.as_str())
                                .await
                        }
                        SessionThreadTarget::Child(child_thread_id) => {
                            SessionAgentCoordination::interrupt_session_thread(
                                self.coordination.as_ref(),
                                &session.session_id,
                                child_thread_id,
                            )
                            .await
                        }
                    };
                    if let Err(error) = result
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
                if let Some(error) = first_error {
                    return Err(error);
                }
                // Cancellation is accepted independently of the in-flight
                // Runtime future. It can make a retained Outcome
                // terminalizable without producing a dispatch completion
                // event (for example an in-process grader), so nudge the
                // sole reconciler now; Notify retains the permit until the
                // currently blocked drive yields.
                self.lifecycle.request_reconciliation();
                let mut source_commit_cursor = EventHistory::from(session).projection_cursor();
                for target in &targets {
                    let thread_id = target.thread_id(&session.session_id);
                    if let Some(snapshot) = thread_reads
                        .session_thread_recovery_snapshot(&session.session_id, &thread_id.0)
                        .await?
                    {
                        source_commit_cursor = source_commit_cursor.max(snapshot.store_cursor);
                    }
                }
                self.settle_event_batch_wake(session, &batch_id).await?;
                if let Some(failed) = superseded_tool_reply.as_ref() {
                    self.settle_superseded_tool_reply_activity(session.session_id.as_str(), failed)
                        .await?;
                }
                self.observe_session_event(
                    &session.session_id,
                    &batch_id,
                    &operation_id,
                    EventObservation::Processed(SessionEventProjectionAnchor {
                        source_commit_cursor,
                    }),
                    superseded_tool_reply.as_ref(),
                )
                .await?;
                Ok(EventBatchProgress::Advanced)
            }),
        }
    }

    /// Complete only the shared System/Outcome/ToolReply suffix. Interrupt
    /// supersession and already-processed recovery retain their distinct order.
    async fn observe_processed_event_after_wake(
        &self,
        session: &PersistedSession,
        batch_id: &str,
        operation_id: &str,
        anchor: SessionEventProjectionAnchor,
    ) -> Result<EventBatchProgress, RunError> {
        self.settle_event_batch_wake(session, batch_id).await?;
        self.observe_session_event(
            &session.session_id,
            batch_id,
            operation_id,
            EventObservation::Processed(anchor),
            None,
        )
        .await?;
        Ok(EventBatchProgress::Advanced)
    }

    /// Close the sole activity receipt opened by the exact failed ToolReply.
    /// A missing receipt means delivery failed before activity transfer; an
    /// already-settled receipt is an idempotent no-op in the canonical activity
    /// owner. Callers must complete this before removing root retry provenance.
    async fn settle_superseded_tool_reply_activity(
        &self,
        session_id: &str,
        failed: &FailedToolReplyCoordinate,
    ) -> Result<(), RunError> {
        let Some((_, activity_epoch)) = self
            .activities
            .recover_activity_for_operation(
                &awaken_session_types::SessionId::from(session_id),
                &failed.activity_operation_id,
                None,
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?
        else {
            return Ok(());
        };
        self.lifecycle
            .settle_activity_observed(
                &awaken_session_types::SessionId::from(session_id),
                activity_epoch,
                None,
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        Ok(())
    }

    /// Input visibility and command completion are separate observations of
    /// the same retained command. Running input keeps FIFO blocked and carries
    /// no processed time; terminal fallback remains owned by the exact Run.
    fn observe_user_run<'a>(
        &'a self,
        session: &'a PersistedSession,
        batch_id: &'a str,
        operation_id: &'a str,
        run_id: &'a awaken_agent_contract::agent::run::Id,
        content: &'a [awaken_agent_contract::agent::content::ContentBlock],
    ) -> futures_util::future::BoxFuture<'a, Result<Option<EventBatchProgress>, RunError>> {
        Box::pin(async move {
            let coordination_reads = self.coordination_reads.as_ref();
            let thread_reads = self.thread_reads.as_ref();

            let Some(state) = coordination_reads
                .session_run_state(&session.session_id, run_id)
                .await?
            else {
                return Ok(None);
            };
            let message_id =
                MessageId::session_event_input(session.session_id.as_str(), operation_id);
            let observation = if state == RunState::Running {
                // The durable root already retained the immutable input coordinate.
                // Repeated supervisor scans must not reload a growing transcript
                // merely to re-prove a completed visibility observation.
                if session.event_batches.iter().any(|batch| {
                    batch.batch_id == batch_id
                        && batch.events.iter().any(|entry| {
                            entry.event.operation_id() == operation_id
                                && entry.projection_anchor.is_some()
                        })
                }) {
                    self.settle_event_batch_wake(session, batch_id).await?;
                    return Ok(Some(EventBatchProgress::Pending));
                }
                let snapshot = thread_reads
                    .session_thread_run_recovery_snapshot(
                        &session.session_id,
                        session.session_id.as_str(),
                        run_id,
                    )
                    .await?;
                match snapshot
                    .as_ref()
                    .map(|snapshot| committed_message_anchor(snapshot, &message_id))
                    .transpose()?
                    .flatten()
                {
                    Some((message, anchor)) => {
                        if message.role != Role::User || message.content != content {
                            return Err(RunError::internal(
                                "committed Session User input conflicts with root command intent",
                            ));
                        }
                        Some(EventObservation::InputCommitted(anchor))
                    }
                    None => None,
                }
            } else {
                // Terminal Thread truth alone cannot publish a processed User
                // receipt. The Dispatch owner invokes the typed post-settlement
                // callback only after its CAS returns Applied; that callback joins
                // the commit anchor, exact activity removal and processed marker in
                // one Session root mutation.
                None
            };
            self.settle_event_batch_wake(session, batch_id).await?;
            if let Some(observation) = observation {
                self.observe_session_event(
                    &session.session_id,
                    batch_id,
                    operation_id,
                    observation,
                    None,
                )
                .await?;
            }
            // Running and terminal Runtime observations both wait here. Running
            // still needs its eventual terminal boundary; terminal truth still
            // needs the Dispatch-Applied callback that alone joins the commit
            // anchor, activity removal and processed marker. Returning Advanced
            // for that pre-callback window would immediately select the same
            // unprocessed command again and hot-loop the bounded supervisor slice.
            Ok(Some(EventBatchProgress::Pending))
        })
    }

    async fn observe_session_event(
        &self,
        session_id: &awaken_session_types::SessionId,
        batch_id: &str,
        operation_id: &str,
        observation: EventObservation,
        superseded_tool_reply: Option<&FailedToolReplyCoordinate>,
    ) -> Result<PersistedSession, RunError> {
        let mutations = &self.mutations;
        let projection_anchor = observation.anchor();
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let (owner, mut session) = self.load_event_root(session_id).await?;
            let changed = session
                .events()
                .observe(
                    batch_id,
                    operation_id,
                    projection_anchor,
                    matches!(observation, EventObservation::Processed(_)),
                    self.clock.now_ms(),
                    superseded_tool_reply
                        .map(|failed| (failed.batch_id.as_str(), failed.operation_id.as_str())),
                )
                .map_err(|error| RunError::internal(error.to_string()))?;
            if !changed {
                return Ok(session.into_snapshot());
            }
            match mutations
                .commit_session_snapshot_with_outcome(
                    &owner,
                    session,
                    match observation {
                        EventObservation::InputCommitted(_) => "anchor-event-input",
                        EventObservation::Processed(_) => "mark-event-processed",
                    },
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(mutation_run_error(error)),
            }
        }
        Err(RunError::unavailable(
            "Session Event progress changed concurrently",
        ))
    }

    fn settle_event_batch_wake<'a>(
        &'a self,
        session: &'a PersistedSession,
        batch_id: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<(), RunError>> {
        Box::pin(async move {
            let wake = session
                .event_batches
                .iter()
                .find(|batch| batch.batch_id == batch_id)
                .and_then(|batch| batch.wake_activity_epoch);
            if let Some(epoch) = wake
                && session.active_activity_epochs.contains(&epoch)
            {
                self.lifecycle
                    .settle_activity_observed(&session.session_id, epoch, None)
                    .await
                    .map_err(crate::SessionActivityError::run_error)?;
            }
            Ok(())
        })
    }

    async fn reconcile_blocking_root_run(
        &self,
        session_id: &awaken_session_types::SessionId,
        target_run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, RunError> {
        let thread_reads = self.thread_reads.as_ref();

        let Some(snapshot) = thread_reads
            .session_thread_recovery_snapshot(session_id, session_id.as_str())
            .await?
        else {
            return Ok(false);
        };
        let Some(latest) = snapshot
            .current_run()
            .map_err(|error| RunError::internal(format!("root Thread recovery: {error}")))?
        else {
            return Ok(false);
        };
        if &latest.id == target_run_id {
            return Ok(false);
        }
        if latest.state == RunState::Awaiting {
            // The queued input is already a durable recovery trigger. Reuse
            // its exact predecessor interval/Dispatch receipt instead of a
            // second repair queue or synthetic tool acknowledgement.
            let root = self
                .queries
                .session(session_id)
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?;
            if root.execution == awaken_session_contract::SessionExecutionState::Running
                && root.active_activity_epochs.is_empty()
                && let Some(observation) = root.running_interval.as_ref().and_then(|interval| {
                    interval.observations.iter().find(|observation| {
                        observation.activity_epoch == root.activity_epoch
                            && observation.run_id == latest.id
                            && observation.thread_id.0 == session_id
                    })
                })
            {
                self.lifecycle
                    .settle_user_event_dispatch_observed(
                        session_id,
                        root.activity_epoch,
                        observation.clone(),
                        &latest.id,
                        awaken_session_contract::SessionAgentRunBoundary::Awaiting,
                    )
                    .await
                    .map_err(crate::SessionActivityError::run_error)?;
            }
        }
        Ok(matches!(
            latest.state,
            RunState::Running | RunState::Awaiting
        ))
    }
}

fn repository_run_error(error: awaken_session_contract::SessionRepositoryError) -> RunError {
    mutation_run_error(repository_failure(error))
}

fn mutation_run_error(error: SessionMutationError) -> RunError {
    match error {
        SessionMutationError::NotFound => RunError::bad_request("Session was not found"),
        SessionMutationError::Conflict => {
            RunError::unavailable("Session Event state changed concurrently")
        }
        SessionMutationError::IdempotencyMismatch => {
            SessionMutationError::Internal("Session Event mutation identity changed payload".into())
                .into()
        }
        SessionMutationError::Unavailable(message) | SessionMutationError::NotApplied(message) => {
            RunError::unavailable(message)
        }
        error @ (SessionMutationError::Internal(_) | SessionMutationError::Invalid(_)) => {
            error.into()
        }
    }
}

#[async_trait::async_trait]
impl crate::SessionEventBatches for SessionEventBatchCommands {
    async fn append_and_drive_session_event_batch_idempotent(
        &self,
        access: &crate::SessionAccess,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<(SessionEventBatchResult, Option<RunError>), RunError> {
        SessionEventBatchCommands::append_and_drive_owned_session_event_batch(
            self,
            access,
            inputs,
            data_subject_id,
            traceparent,
            idempotency,
        )
        .await
    }

    async fn append_session_event_batch_idempotent(
        &self,
        access: &crate::SessionAccess,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatchResult, RunError> {
        SessionEventBatchCommands::append_owned_session_event_batch(
            self,
            access,
            inputs,
            data_subject_id,
            traceparent,
            idempotency,
        )
        .await
    }

    async fn session_event_batch_idempotency(
        &self,
        session_id: &awaken_session_types::SessionId,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<SessionEventBatchIdempotency, RunError> {
        SessionEventBatchCommands::session_event_batch_idempotency(
            self,
            session_id,
            key,
            request_fingerprint,
        )
        .await
    }

    async fn close_session_event_admission(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        key: String,
        request_fingerprint: String,
    ) -> Result<awaken_session_contract::SessionEventAdmission, RunError> {
        SessionEventBatchCommands::close_session_event_admission(
            self,
            owner_scope,
            session_id,
            key,
            request_fingerprint,
        )
        .await
    }

    fn agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.queries.agent_unavailable(workspace_id, agent_id)
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> Result<bool, RunError> {
        let streaming_runtime = self.streaming_runs.as_ref();

        streaming_runtime
            .supports_mid_conversation_system(thread)
            .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn persisted_batch_effect_ownership_is_nonoverlapping() {
        // Cause/effect inventory and selector decision table. Causes: C1 batch
        // is create/ordinary; C2 target User absent/Running/Awaiting/Ended; C3
        // another latest Run is Running/Awaiting; C4 accompanying System is
        // absent/committed and follows User/processed ToolReply; C5 ToolReply,
        // DefineOutcome, or Interrupt is unprocessed anywhere in retained
        // batches; C6 crash before/after effect admission or processed CAS.
        // Effects: E1 ordinary acceptance opens no activity; E2 create wake
        // settles once the first effect has its own durable owner; E3 User is
        // processed on Awaiting/Ended; E4 later User remains queued under C3;
        // E5 System completes only from its stable committed Message; E6 all
        // receipt commands cross queued User in order; E7 a System crosses User
        // only as the stable observation paired with a processed ToolReply; E8
        // retained entries remain provenance and exact replay skips them.
        //
        // | Rule | Durable observation | Effect |
        // | R1 | ordinary append | E1 |
        // | R2 | User admitted/observed | E2; Running waits, Awaiting/Ended E3 |
        // | R3 | different latest Running/Awaiting | E4, no new Run activity |
        // | R4 | User-paired System missing/committed | FIFO wait/E5 |
        // | R5 | reply/outcome/interrupt behind User | E6 in retained order |
        // | R6 | processed ToolReply + exact System missing/committed | wait/E7 |
        // | R7 | crash at any CAS boundary | existing owner dedup + E8 |
        // Constraints/invariants: every effect family has exactly one durable
        // owner; retained batch entries are provenance only, and replay cannot
        // admit a second effect or reorder a later command across its owner.
        let variants = [
            SessionEventProgressOwner::UserDispatchAndThread,
            SessionEventProgressOwner::AccompanyingRunDispatchAndThread,
            SessionEventProgressOwner::ThreadOutcomeState,
            SessionEventProgressOwner::SessionThreadReplyCoordination,
            SessionEventProgressOwner::RuntimeThreadInterruption,
        ];
        assert_eq!(variants.len(), 5, "R1-R7 one owner per effect family");
    }

    enum SessionEventProgressOwner {
        UserDispatchAndThread,
        AccompanyingRunDispatchAndThread,
        ThreadOutcomeState,
        SessionThreadReplyCoordination,
        RuntimeThreadInterruption,
    }
}
