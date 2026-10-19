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

use crate::SessionRootReads as _;
use crate::mutation::ROOT_CAS_ATTEMPTS;
use awaken_agent_contract::agent::message::{Id as MessageId, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_session_event_contract::{
    OUTCOME_BUSY_CODE, SessionEventBatch, SessionEventBatchAdmissionAction,
    SessionEventBatchAdmissionClass, SessionEventBatchAdmissionPlan, SessionEventCommand,
    SessionEventInput, SessionEventProjectionAnchor, SessionUserRunCommand,
    SessionUserRunSystemInput, classify_session_event_batch_admission,
    resolve_session_event_batch_admission, session_event_batch_id,
};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionAgentCoordination, SessionRunActivation,
    SessionThreadTarget,
};
use awaken_session_state_contract::{PersistedSession, SessionEventHistory as EventHistory};
use awaken_session_types::{RunError, SessionRevision};
use awaken_time::WallClockExt as _;

use super::{SessionApplication, SessionMutationError, mutation::repository_failure};

/// Maximum number of durable Event commands advanced in one supervisor slice.
/// This is a recovery scheduling bound, independent of create-time admission.
const EVENT_BATCH_RECONCILIATION_STEPS: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub enum SessionEventBatchIdempotency {
    Absent,
    Exact(SessionEventBatch),
    Conflict,
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
}

impl RetainedEventBatchAdmission<'_> {
    fn classification(&self) -> SessionEventBatchAdmissionClass {
        classify_session_event_batch_admission(
            self.batch.is_some(),
            self.fingerprint_matches,
            self.admission_closed,
        )
    }

    fn resolution(
        &self,
        action: SessionEventBatchAdmissionAction,
    ) -> SessionEventBatchAdmissionPlan {
        resolve_session_event_batch_admission(
            self.batch.is_some(),
            self.fingerprint_matches,
            self.admission_closed,
            action,
        )
    }
}

fn retained_event_batch_admission<'a>(
    batches: &'a [SessionEventBatch],
    key: &str,
    request_fingerprint: &str,
) -> RetainedEventBatchAdmission<'a> {
    let batch = batches
        .iter()
        .find(|batch| batch.idempotency_key.as_deref() == Some(key));
    RetainedEventBatchAdmission {
        batch,
        fingerprint_matches: batch
            .is_some_and(|batch| batch.request_fingerprint.as_deref() == Some(request_fingerprint)),
        admission_closed: batch.is_some_and(|batch| batch.admission_closed),
    }
}

fn classify_event_batch_idempotency(
    batches: &[SessionEventBatch],
    key: &str,
    request_fingerprint: &str,
) -> SessionEventBatchIdempotency {
    let retained = retained_event_batch_admission(batches, key, request_fingerprint);
    match retained.classification() {
        SessionEventBatchAdmissionClass::Absent => SessionEventBatchIdempotency::Absent,
        SessionEventBatchAdmissionClass::Accepted | SessionEventBatchAdmissionClass::Closed => {
            SessionEventBatchIdempotency::Exact(
                retained
                    .batch
                    .expect("exact classification has a retained batch")
                    .clone(),
            )
        }
        SessionEventBatchAdmissionClass::Conflict => SessionEventBatchIdempotency::Conflict,
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

impl SessionApplication {
    /// Observe the Session root's one retained Event-batch retry coordinate.
    /// This read exists so a protocol retry can recover its original receipt
    /// before current pending-tool admission; it does not own another receipt.
    pub async fn session_event_batch_idempotency(
        &self,
        session_id: &awaken_session_types::SessionId,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<SessionEventBatchIdempotency, RunError> {
        let session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_failure)
            .map_err(mutation_run_error)?;
        Ok(classify_event_batch_idempotency(
            &session.event_batches,
            key,
            request_fingerprint,
        ))
    }

    async fn message_projection_anchor(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        message_id: &MessageId,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        let thread_reads = self.runtime().thread_reads();

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
        let thread_reads = self.runtime().thread_reads();

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
            self.admit_session_run(AdmitSessionRun {
                session_id,
                agent_id: command.agent_id,
                operation_id: command.operation_id,
                run_id: command.run_id,
                messages,
                data_subject_id: command.data_subject_id,
                traceparent: command.traceparent,
                execution_requirements: Default::default(),
                replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
            })
            .await
        })
    }

    /// Atomically accept one complete ordinary Event batch in the Session root.
    /// Its prospective committed revision is the stable linearization identity.
    /// Acceptance opens no activity: queued work may wait while the Thread is
    /// idle or Awaiting without fabricating a Running Session.
    pub async fn append_session_event_batch(
        &self,
        session_id: &str,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
    ) -> Result<SessionEventBatch, RunError> {
        self.append_session_event_batch_idempotent(
            &awaken_session_types::SessionId::from(session_id),
            inputs,
            data_subject_id,
            traceparent,
            None,
        )
        .await
    }

    /// Atomically append or replay one HTTP-idempotent Event batch. The key and
    /// fingerprint live on the existing root command, so concurrent callers
    /// either observe that exact batch or a conflict under the same root CAS.
    pub async fn append_session_event_batch_idempotent(
        &self,
        session_id: &awaken_session_types::SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatch, RunError> {
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
            .map_err(|_| awaken_session_application_contract::event_admission_closed())?;
        Ok(batch)
    }

    /// Accept through the same root CAS, then perform the request-local drive
    /// before notifying the full recovery sweep. This removes a duplicate race
    /// between the synchronous driver and its own wake while retaining the
    /// periodic recovery authority if this future is lost after acceptance.
    pub async fn append_and_drive_session_event_batch_idempotent(
        &self,
        session_id: &awaken_session_types::SessionId,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<(SessionEventBatch, Option<RunError>), RunError> {
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
            .map_err(|_| awaken_session_application_contract::event_admission_closed())?;
        let drive_error = self
            .drive_session_event_batches(session_id, Some(&batch.batch_id))
            .await
            .err();
        // The full sweep remains the one retry owner. Waking after the local
        // attempt preserves queued predecessors and retryable failures without
        // racing this request's append/admission/activation root mutations.
        self.lifecycle.request_reconciliation();
        Ok((batch, drive_error))
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
    ) -> Result<awaken_session_application_contract::SessionEventAdmission, RunError> {
        if let Some(deleted) = self
            .session_repository()
            .deleted_event_admissions(owner_scope, session_id)
            .await
            .map_err(repository_failure)
            .map_err(mutation_run_error)?
        {
            return awaken_session_application_contract::resolve_deleted_event_admission(
                &deleted,
                &key,
                &request_fingerprint,
            );
        }
        match self.mutations.owner(session_id).await {
            Ok(owner) if owner == owner_scope => {}
            Ok(_) | Err(SessionMutationError::NotFound) => {
                return Err(RunError {
                    message: "Session was not found".into(),
                    kind: awaken_session_types::RunErrorKind::BadRequest,
                    code: "session_not_found".into(),
                });
            }
            Err(error) => return Err(mutation_run_error(error)),
        }
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
                    .session_repository()
                    .deleted_event_admissions(owner_scope, session_id)
                    .await
                    .map_err(repository_failure)
                    .map_err(mutation_run_error)?
                {
                    Some(deleted) => {
                        awaken_session_application_contract::resolve_deleted_event_admission(
                            &deleted,
                            &key,
                            &request_fingerprint,
                        )
                    }
                    None => Err(error),
                }
            }
        }
    }

    async fn resolve_session_event_batch(
        &self,
        session_id: &awaken_session_types::SessionId,
        admission: EventBatchAdmission,
        wake_after_accept: bool,
    ) -> Result<SessionEventBatch, RunError> {
        let mutations = &self.mutations;
        let thread_reads = self.runtime().thread_reads();

        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner = mutations
                .owner(session_id)
                .await
                .map_err(mutation_run_error)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_run_error)?;
            let retained = admission.idempotency().map(|(key, request_fingerprint)| {
                retained_event_batch_admission(&session.event_batches, key, request_fingerprint)
            });
            let plan = match retained.as_ref() {
                Some(retained) => retained.resolution(admission.action()),
                None => SessionEventBatchAdmissionPlan::AppendAccepted,
            };
            match plan {
                SessionEventBatchAdmissionPlan::ReplayAccepted
                | SessionEventBatchAdmissionPlan::ReplayClosed => {
                    return Ok(retained
                        .as_ref()
                        .and_then(|retained| retained.batch)
                        .expect("replay admission plan has one retained batch")
                        .clone());
                }
                SessionEventBatchAdmissionPlan::RejectClosed => {
                    return Err(awaken_session_application_contract::event_admission_closed());
                }
                SessionEventBatchAdmissionPlan::Conflict => {
                    return Err(awaken_session_application_contract::event_admission_conflict());
                }
                SessionEventBatchAdmissionPlan::AppendAccepted
                | SessionEventBatchAdmissionPlan::AppendClosed => {}
            }
            if session.is_terminal() && plan == SessionEventBatchAdmissionPlan::AppendAccepted {
                return Err(RunError::bad_request(
                    "Session no longer accepts Event batches",
                ));
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
            let batch_id = session_event_batch_id(session_id, committed_revision)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
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
                    inputs.clone(),
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
            }
            .map_err(|error| RunError::bad_request(error.to_string()))?;
            batch.admitted_revision = committed_revision;
            if let Some(anchor) = admission_projection_anchor {
                batch
                    .anchor_admission_projection(anchor)
                    .map_err(|error| RunError::internal(error.to_string()))?;
            }
            if !batch.admission_closed
                && let Some((key, request_fingerprint)) = admission.idempotency()
            {
                batch
                    .bind_idempotency(key.to_owned(), request_fingerprint.to_owned())
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
            }
            // Protocol validation can race another key before either root CAS.
            // Recheck on EVERY retry; the durable batch owns occupancy even
            // after Runtime delivery, without a second registry or local lock.
            batch
                .validate_tool_reply_occupancy(&session.event_batches)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
            session
                .events()
                .append_batch(batch.clone())
                .map_err(|error| RunError::internal(error.to_string()))?;
            match mutations
                .commit_session_snapshot_with_outcome(
                    &owner,
                    session,
                    "append-event-batch",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {
                    if !batch.admission_closed && wake_after_accept {
                        self.lifecycle.request_reconciliation();
                    }
                    return Ok(batch);
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(mutation_run_error(error)),
            }
        }
        Err(RunError::unavailable(
            "Session Event batch admission changed concurrently",
        ))
    }

    /// Revisit every unfinished root-owned Event batch through the one Session
    /// recovery scan. One Session is advanced until it reaches an external Run
    /// boundary; no process-local task owns later progress.
    #[cfg(test)]
    pub(crate) async fn reconcile_event_batches(&self) -> EventBatchReconciliation {
        let candidates = match super::recovery_scan::recovery_candidates(self, true).await {
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

    pub(super) async fn reconcile_event_batches_from(
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
                    Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => {
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

    async fn reconcile_session_event_batches(
        &self,
        session_id: &awaken_session_types::SessionId,
        preferred_batch_id: Option<&str>,
        mut initial_session: Option<PersistedSession>,
    ) -> Result<bool, RunError> {
        // Bound one supervisor slice. Retained history may contain many batches,
        // but each Advanced result persists one entry and a later scan resumes.
        for _ in 0..EVENT_BATCH_RECONCILIATION_STEPS {
            let session = match initial_session.take() {
                Some(session) => session,
                None => self
                    .session_repository()
                    .get(session_id)
                    .await
                    .map_err(repository_failure)
                    .map_err(mutation_run_error)?,
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
                        .session_repository()
                        .get(session_id)
                        .await
                        .map_err(repository_failure)
                        .map_err(mutation_run_error)?;
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
    }

    /// Close only the legacy race in which an old writer terminalized a root
    /// after accepting, but before anchoring, an Event batch. The canonical
    /// supervisor marks those receipts resolved under root CAS and never calls
    /// any Runtime effect after terminal state.
    async fn resolve_terminal_event_batches(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<bool, RunError> {
        let mutations = &self.mutations;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(session_id)
                .await
                .map_err(mutation_run_error)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_run_error)?;
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
    }

    fn reconcile_one_session_event<'a>(
        &'a self,
        session: &'a PersistedSession,
        selected: SelectedSessionEvent,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<EventBatchProgress, RunError>> + Send + 'a>,
    > {
        let coordination_runtime = self.runtime().coordination();

        let thread_reads = self.runtime().thread_reads();

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

                let delivery = match admission {
                    AdmittedSessionRun::Reserved(delivery)
                    | AdmittedSessionRun::AlreadyReserved(delivery)
                    | AdmittedSessionRun::AlreadyActivated(delivery) => delivery,
                    AdmittedSessionRun::RecoveryClaimed { .. } => {
                        return Ok(EventBatchProgress::Pending);
                    }
                    AdmittedSessionRun::Completed { .. } => {
                        return Ok(self
                            .observe_user_run(session, &batch_id, &operation_id, &run_id, &content)
                            .await?
                            .unwrap_or(EventBatchProgress::Pending));
                    }
                };
                match coordination_runtime.activate_session_run(delivery).await? {
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
                self.settle_event_batch_wake(session, &batch_id).await?;
                self.observe_session_event(
                    &session.session_id,
                    &batch_id,
                    &operation_id,
                    EventObservation::Processed(anchor),
                    None,
                )
                .await?;
                Ok(EventBatchProgress::Advanced)
            }),
            SessionEventCommand::DefineOutcome {
                operation_id,
                outcome_id,
                description,
                rubric,
                max_iterations,
            } => Box::pin(async move {
                let source_commit_cursor = match self
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
                self.settle_event_batch_wake(session, &batch_id).await?;
                self.observe_session_event(
                    &session.session_id,
                    &batch_id,
                    &operation_id,
                    EventObservation::Processed(SessionEventProjectionAnchor {
                        source_commit_cursor,
                    }),
                    None,
                )
                .await?;
                Ok(EventBatchProgress::Advanced)
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
                    self,
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
                self.settle_event_batch_wake(session, &batch_id).await?;
                self.observe_session_event(
                    &session.session_id,
                    &batch_id,
                    &operation_id,
                    EventObservation::Processed(anchor),
                    None,
                )
                .await?;
                Ok(EventBatchProgress::Advanced)
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
                            self.interrupt(session.session_id.as_str()).await
                        }
                        SessionThreadTarget::Child(child_thread_id) => {
                            SessionAgentCoordination::interrupt_session_thread(
                                self,
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
        self.settle_activity(session_id, activity_epoch)
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        Ok(())
    }

    /// Input visibility and command completion are separate observations of
    /// the same retained command. Running input keeps FIFO blocked and carries
    /// no processed time; terminal fallback remains owned by the exact Run.
    async fn observe_user_run(
        &self,
        session: &PersistedSession,
        batch_id: &str,
        operation_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        content: &[awaken_agent_contract::agent::content::ContentBlock],
    ) -> Result<Option<EventBatchProgress>, RunError> {
        let coordination_reads = self.runtime().coordination_reads();
        let thread_reads = self.runtime().thread_reads();

        let Some(state) = coordination_reads
            .session_run_state(&session.session_id, run_id)
            .await?
        else {
            return Ok(None);
        };
        let message_id = MessageId::session_event_input(session.session_id.as_str(), operation_id);
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
            let owner = mutations
                .owner(session_id)
                .await
                .map_err(mutation_run_error)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_run_error)?;
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

    async fn settle_event_batch_wake(
        &self,
        session: &PersistedSession,
        batch_id: &str,
    ) -> Result<(), RunError> {
        let wake = session
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == batch_id)
            .and_then(|batch| batch.wake_activity_epoch);
        if let Some(epoch) = wake
            && session.active_activity_epochs.contains(&epoch)
        {
            self.settle_activity(session.session_id.as_str(), epoch)
                .await
                .map_err(crate::SessionActivityError::run_error)?;
        }
        Ok(())
    }

    async fn reconcile_blocking_root_run(
        &self,
        session_id: &awaken_session_types::SessionId,
        target_run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, RunError> {
        let thread_reads = self.runtime().thread_reads();

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
            if root.execution == awaken_session_state_contract::SessionExecutionState::Running
                && root.active_activity_epochs.is_empty()
                && let Some(observation) = root.running_interval.as_ref().and_then(|interval| {
                    interval.observations.iter().find(|observation| {
                        observation.activity_epoch == root.activity_epoch
                            && observation.run_id == latest.id
                            && observation.thread_id.0 == session_id
                    })
                })
            {
                self.settle_user_event_dispatch_observed(
                    session_id,
                    root.activity_epoch,
                    observation.clone(),
                    &latest.id,
                    awaken_session_execution_contract::SessionAgentRunBoundary::Awaiting,
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

fn adjacent_system_input(
    session: &PersistedSession,
    batch_id: &str,
    operation_id: &str,
) -> Option<SessionUserRunSystemInput> {
    session
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == batch_id)
        .and_then(|batch| {
            batch
                .events
                .iter()
                .position(|entry| entry.event.operation_id() == operation_id)
                .and_then(|ordinal| batch.events.get(ordinal + 1))
        })
        .and_then(|entry| match &entry.event {
            SessionEventCommand::SystemMessage {
                operation_id,
                content,
            } => Some(SessionUserRunSystemInput {
                operation_id: operation_id.clone(),
                content: content.clone(),
            }),
            _ => None,
        })
}

fn preceding_event_runtime_target(
    session: &PersistedSession,
    batch_id: &str,
    operation_id: &str,
) -> Option<(
    awaken_agent_contract::agent::thread::Id,
    awaken_agent_contract::agent::run::Id,
)> {
    let batch = session
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == batch_id)?;
    let ordinal = batch
        .events
        .iter()
        .position(|entry| entry.event.operation_id() == operation_id)?;
    batch.events[..ordinal]
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            SessionEventCommand::UserMessage { run_id, .. } => Some((
                awaken_agent_contract::agent::thread::Id(session.session_id.to_string()),
                run_id.clone(),
            )),
            SessionEventCommand::ToolReply { reply, .. } => Some((
                reply.target.thread_id(&session.session_id),
                reply.expected_run_id.clone(),
            )),
            SessionEventCommand::SystemMessage { .. }
            | SessionEventCommand::DefineOutcome { .. }
            | SessionEventCommand::Interrupt { .. } => None,
        })
}

/// Select the next command through the durable aggregate's ordinary priority
/// and FIFO rules. Request-local acceleration must compare against this exact
/// result rather than implementing another ordering rule.
fn select_normal_session_event(session: &PersistedSession) -> Option<SelectedSessionEvent> {
    let select = |predicate: fn(&SessionEventCommand) -> bool| {
        session.event_batches.iter().find_map(|batch| {
            batch.events.iter().find_map(|entry| {
                (!entry.processed && predicate(&entry.event)).then(|| SelectedSessionEvent {
                    batch_id: batch.batch_id.clone(),
                    event: entry.event.clone(),
                    traceparent: batch.traceparent.clone(),
                    processed_anchor: None,
                    superseded_tool_reply: None,
                })
            })
        })
    };
    select(|event| {
        matches!(
            event,
            SessionEventCommand::ToolReply { .. }
                | SessionEventCommand::DefineOutcome { .. }
                | SessionEventCommand::Interrupt { .. }
        )
    })
    .or_else(|| {
        session.event_batches.iter().find_map(|batch| {
            batch
                .events
                .windows(2)
                .find(|pair| {
                    pair[0].processed
                        && matches!(&pair[0].event, SessionEventCommand::ToolReply { .. })
                        && !pair[1].processed
                        && matches!(&pair[1].event, SessionEventCommand::SystemMessage { .. })
                })
                .map(|pair| SelectedSessionEvent {
                    batch_id: batch.batch_id.clone(),
                    event: pair[1].event.clone(),
                    traceparent: batch.traceparent.clone(),
                    processed_anchor: None,
                    superseded_tool_reply: None,
                })
        })
    })
    .or_else(|| {
        select(|event| {
            !matches!(
                event,
                SessionEventCommand::ToolReply { .. }
                    | SessionEventCommand::DefineOutcome { .. }
                    | SessionEventCommand::Interrupt { .. }
            )
        })
    })
}

/// One eligibility selector over retained provenance. Normal selection keeps
/// ToolReply, DefineOutcome, and Interrupt receipt commands in their original
/// relative order while letting them cross queued User entries. A request-local
/// preferred batch narrows that same authority to its ToolReply/Interrupt
/// receipt, or starts its User command only when the ordinary selector confirms
/// it is already the global FIFO head. Only after an exact ToolReply delivery has failed may recovery select
/// a later retained Interrupt that includes the failed reply's exact Thread
/// target; it must rediscover the same unprocessed failed operation in a fresh
/// root before crossing it. An already-processed Interrupt supplies its root
/// anchor without repeating the Runtime effect, closing the crash window between
/// Interrupt anchoring and reply supersession. A System
/// immediately following an already-processed ToolReply is otherwise eligible
/// only as a side-effect-free observation of its exact committed Message.
/// User-associated System and every other command remain FIFO.
fn select_session_event(
    session: &PersistedSession,
    selection: SessionEventSelection<'_>,
) -> Option<SelectedSessionEvent> {
    if let SessionEventSelection::InterruptAfterFailedToolReply {
        batch_id,
        operation_id,
    } = selection
    {
        let mut failed_tool_reply = None;
        for batch in &session.event_batches {
            for entry in &batch.events {
                if failed_tool_reply.is_none() {
                    if batch.batch_id == batch_id
                        && entry.event.operation_id() == operation_id
                        && !entry.processed
                        && let SessionEventCommand::ToolReply { reply, .. } = &entry.event
                    {
                        let activity_operation_id = reply
                            .delivery_command(
                                &session.session_id,
                                adjacent_system_input(session, batch_id, operation_id),
                            )
                            .activity_operation_id();
                        failed_tool_reply = Some((
                            reply.target.clone(),
                            FailedToolReplyCoordinate {
                                batch_id: batch_id.to_string(),
                                operation_id: operation_id.to_string(),
                                activity_operation_id,
                            },
                        ));
                    }
                    continue;
                }
                let Some((failed_target, failed)) = failed_tool_reply.as_ref() else {
                    continue;
                };
                if let SessionEventCommand::Interrupt { interrupt, .. } = &entry.event
                    && interrupt.targets.contains(failed_target)
                {
                    let processed_anchor = if entry.processed {
                        let Some(anchor) = entry.projection_anchor else {
                            // Legacy processed rows can lack projection
                            // provenance. They cannot own supersession, but
                            // neither may they starve a later exact Interrupt.
                            continue;
                        };
                        Some(anchor)
                    } else {
                        None
                    };
                    return Some(SelectedSessionEvent {
                        batch_id: batch.batch_id.clone(),
                        event: entry.event.clone(),
                        traceparent: batch.traceparent.clone(),
                        processed_anchor,
                        superseded_tool_reply: Some(failed.clone()),
                    });
                }
            }
        }
        return None;
    }

    let SessionEventSelection::Normal { preferred_batch_id } = selection else {
        unreachable!("failed-reply selection returned above")
    };
    if let Some(batch_id) = preferred_batch_id {
        let batch = session
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == batch_id)?;
        if let Some(receipt) = batch.events.iter().find_map(|entry| {
            (!entry.processed
                && matches!(
                    &entry.event,
                    SessionEventCommand::ToolReply { .. } | SessionEventCommand::Interrupt { .. }
                ))
            .then(|| SelectedSessionEvent {
                batch_id: batch.batch_id.clone(),
                event: entry.event.clone(),
                traceparent: batch.traceparent.clone(),
                processed_anchor: None,
                superseded_tool_reply: None,
            })
        }) {
            return Some(receipt);
        }
        return select_normal_session_event(session).filter(|selected| {
            selected.batch_id == batch_id
                && matches!(&selected.event, SessionEventCommand::UserMessage { .. })
        });
    }
    select_normal_session_event(session)
}

fn mutation_run_error(error: SessionMutationError) -> RunError {
    match error {
        SessionMutationError::NotFound => RunError::bad_request("Session was not found"),
        SessionMutationError::Conflict => {
            RunError::unavailable("Session Event state changed concurrently")
        }
        SessionMutationError::IdempotencyMismatch => {
            RunError::internal("Session Event mutation identity changed payload")
        }
        SessionMutationError::Unavailable(message) => RunError::unavailable(message),
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
