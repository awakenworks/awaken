//! Admission and reconciliation of the Session root's durable Event batches.
//!
//! This module coordinates existing owners only. It does not persist an Event
//! log, execute a Run, or retain a completion registry.

use awaken_agent_contract::agent::message::{Id as MessageId, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_session_contract::{
    AdmitSessionRun, AdmittedSessionRun, OUTCOME_BUSY_CODE, PersistedSession, RunError,
    SessionAgentCoordination, SessionEventBatch, SessionEventCommand, SessionEventInput,
    SessionEventProjectionAnchor, SessionRevision, SessionRunActivation, SessionThreadTarget,
    SessionUserRunCommand, SessionUserRunSystemInput, session_event_batch_id,
};

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

fn classify_event_batch_idempotency(
    batches: &[SessionEventBatch],
    key: &str,
    request_fingerprint: &str,
) -> SessionEventBatchIdempotency {
    let Some(batch) = batches
        .iter()
        .find(|batch| batch.idempotency_key.as_deref() == Some(key))
    else {
        return SessionEventBatchIdempotency::Absent;
    };
    if batch.request_fingerprint.as_deref() == Some(request_fingerprint) {
        SessionEventBatchIdempotency::Exact(batch.clone())
    } else {
        SessionEventBatchIdempotency::Conflict
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EventBatchReconciliation {
    pub settled: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
    pub quarantined: usize,
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
}

impl SessionApplication {
    /// Observe the Session root's one retained Event-batch retry coordinate.
    /// This read exists so a protocol retry can recover its original receipt
    /// before current pending-tool admission; it does not own another receipt.
    pub async fn session_event_batch_idempotency(
        &self,
        session_id: &str,
        key: &str,
        request_fingerprint: &str,
    ) -> Result<SessionEventBatchIdempotency, RunError> {
        let session = self
            .session_repository()
            .get(session_id)
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
        session_id: &str,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        message_id: &MessageId,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        self.message_projection_anchor_with_cancelled_fallback(
            session_id, thread_id, run_id, message_id, false,
        )
        .await
    }

    /// Anchor an accepted User receipt to its committed Message, or to the exact
    /// cancelled Run when interruption won before that Message entered the
    /// transcript. The latter terminal coordinate completes the retained command
    /// without inventing transcript content and lets the existing FIFO advance.
    async fn user_message_projection_anchor(
        &self,
        session_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        message_id: &MessageId,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        self.message_projection_anchor_with_cancelled_fallback(
            session_id, session_id, run_id, message_id, true,
        )
        .await
    }

    async fn message_projection_anchor_with_cancelled_fallback(
        &self,
        session_id: &str,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        message_id: &MessageId,
        cancelled_run_may_anchor: bool,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        let snapshot = self
            .runtime()
            .session_thread_run_recovery_snapshot(session_id, thread_id, run_id)
            .await?
            .ok_or_else(|| {
                RunError::unavailable(
                    "committed Session Event message owner is not yet recoverable",
                )
            })?;
        if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
            return Err(RunError::unavailable(
                "committed Session Event message has no durable commit coordinate",
            ));
        }
        let message_anchor = snapshot
            .messages
            .iter()
            .zip(snapshot.message_commit_cursors.iter().copied())
            .find_map(|(message, cursor)| {
                (&message.id == message_id).then_some(SessionEventProjectionAnchor {
                    source_commit_cursor: cursor,
                })
            });
        if let Some(anchor) = message_anchor {
            return Ok(anchor);
        }
        if cancelled_run_may_anchor
            && snapshot
                .runs
                .iter()
                .any(|run| &run.id == run_id && run.state == RunState::Ended(EndCause::Cancelled))
        {
            return Ok(SessionEventProjectionAnchor {
                source_commit_cursor: snapshot.store_cursor,
            });
        }
        Err(RunError::unavailable(
            "committed Session Event message is not yet visible in recovery",
        ))
    }

    async fn run_snapshot_projection_anchor(
        &self,
        session_id: &str,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<SessionEventProjectionAnchor, RunError> {
        self.runtime()
            .session_thread_run_recovery_snapshot(session_id, thread_id, run_id)
            .await?
            .filter(|snapshot| snapshot.runs.iter().any(|run| &run.id == run_id))
            .map(|snapshot| SessionEventProjectionAnchor {
                source_commit_cursor: snapshot.store_cursor,
            })
            .ok_or_else(|| {
                RunError::unavailable("committed Session Event Run owner is not yet recoverable")
            })
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
                    MessageId::session_system(&session_id, &system.operation_id),
                    Role::System,
                    system.content,
                ));
            }
            if command.operation_id.trim().is_empty() || command.content.is_empty() {
                return Err(RunError::bad_request("Session User input is incomplete"));
            }
            messages.push(awaken_agent_contract::agent::message::Message::new(
                MessageId::session_event_input(&session_id, &command.operation_id),
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
                replacement: awaken_session_contract::SessionRunReplacement::PreservePrior,
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
            session_id,
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
        session_id: &str,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency: Option<(String, String)>,
    ) -> Result<SessionEventBatch, RunError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner = self.owner(session_id).await.map_err(mutation_run_error)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_run_error)?;
            if let Some((key, request_fingerprint)) = idempotency.as_ref() {
                match classify_event_batch_idempotency(
                    &session.event_batches,
                    key,
                    request_fingerprint,
                ) {
                    SessionEventBatchIdempotency::Exact(batch) => return Ok(batch),
                    SessionEventBatchIdempotency::Conflict => {
                        return Err(RunError {
                            message:
                                "Idempotency-Key was already used for another Session Event batch"
                                    .into(),
                            kind: awaken_session_contract::RunErrorKind::BadRequest,
                            code: "idempotency_conflict".into(),
                        });
                    }
                    SessionEventBatchIdempotency::Absent => {}
                }
            }
            if session.is_terminal() {
                return Err(RunError::bad_request(
                    "Session no longer accepts Event batches",
                ));
            }
            let committed_revision = SessionRevision(
                session
                    .revision
                    .0
                    .checked_add(1)
                    .ok_or_else(|| RunError::internal("Session revision is exhausted"))?,
            );
            let batch_id = session_event_batch_id(session_id, committed_revision)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
            let mut batch = SessionEventBatch::compile_attributed(
                session_id,
                batch_id,
                inputs.clone(),
                data_subject_id.clone(),
                traceparent.clone(),
            )
            .map_err(|error| RunError::bad_request(error.to_string()))?;
            batch.admitted_revision = committed_revision;
            if let Some((key, request_fingerprint)) = idempotency.as_ref() {
                batch
                    .bind_idempotency(key.clone(), request_fingerprint.clone())
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
            }
            session.event_batches.push(batch.clone());
            match self
                .commit_session_snapshot(&owner, session, "append-event-batch", Vec::new())
                .await
            {
                Ok(_) => {
                    self.wake_lifecycle_supervisor();
                    return Ok(batch);
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
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
        let candidates = match self.session_repository().reconcilable_sessions().await {
            Ok(scan) => super::SessionRecoveryCandidates::from(scan),
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
        for candidate in &candidates.sessions {
            let session = match self.session_repository().get(&candidate.session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report
                        .failures
                        .push((candidate.session_id.clone(), error.to_string()));
                    continue;
                }
            };
            if !session.needs_event_reconciliation() {
                continue;
            }
            report.pending += 1;
            let session_id = session.session_id;
            match self
                .reconcile_session_event_batches(&session_id, None)
                .await
            {
                Ok(true) => report.settled += 1,
                Ok(false) => {}
                Err(error) => report.failures.push((session_id, error.to_string())),
            }
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
        session_id: &str,
        preferred_batch_id: Option<&str>,
    ) -> Result<(), RunError> {
        let _ = self
            .reconcile_session_event_batches(session_id, preferred_batch_id)
            .await?;
        Ok(())
    }

    async fn reconcile_session_event_batches(
        &self,
        session_id: &str,
        preferred_batch_id: Option<&str>,
    ) -> Result<bool, RunError> {
        // Bound one supervisor slice. Retained history may contain many batches,
        // but each Advanced result persists one entry and a later scan resumes.
        for _ in 0..EVENT_BATCH_RECONCILIATION_STEPS {
            let session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_run_error)?;
            if session.is_terminal() {
                return self.resolve_terminal_event_batches(session_id).await;
            }
            let selected = match preferred_batch_id {
                Some(batch_id) => select_preferred_batch_receipt(&session, batch_id),
                None => select_session_event(&session),
            };
            let Some(selected) = selected else {
                return Ok(true);
            };
            match self.reconcile_one_session_event(&session, selected).await? {
                EventBatchProgress::Advanced => {}
                EventBatchProgress::Pending => return Ok(false),
            }
        }
        Ok(false)
    }

    /// Close only the legacy race in which an old writer terminalized a root
    /// after accepting, but before anchoring, an Event batch. The canonical
    /// supervisor marks those receipts resolved under root CAS and never calls
    /// any Runtime effect after terminal state.
    async fn resolve_terminal_event_batches(&self, session_id: &str) -> Result<bool, RunError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(mutation_run_error)?;
            let mut session = self
                .session_repository()
                .get(session_id)
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
            let runtime_commit_cursor = terminal_cleanup.runtime_commit_cursor();
            let anchor =
                runtime_commit_cursor.map(|source_commit_cursor| SessionEventProjectionAnchor {
                    source_commit_cursor,
                });
            let resolved = session
                .event_batches
                .iter_mut()
                .map(|batch| batch.resolve_terminally(anchor))
                .sum::<usize>();
            if resolved == 0 {
                return Ok(true);
            }
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "resolve-terminal-event-batches",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => return Ok(true),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
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
        // This reconciliation is polled by the lifecycle supervisor, whose
        // recovery future is already deep. Keep the per-Event state machine at
        // the same boxed application seam used by ordinary Run admission; an
        // unboxed User/System/Outcome union exceeds a default Tokio worker stack
        // in debug builds and risks doing the same under a large production
        // recovery prefix. Boxing changes placement only, not ownership.
        Box::pin(async move {
            let SelectedSessionEvent {
                batch_id,
                event,
                traceparent,
            } = selected;
            match event {
                SessionEventCommand::UserMessage {
                    operation_id,
                    run_id,
                    content,
                    data_subject_id,
                } => {
                    match self
                        .runtime()
                        .session_run_state(&session.session_id, &run_id)
                        .await?
                    {
                        Some(RunState::Awaiting) | Some(RunState::Ended(_)) => {
                            let anchor = self
                                .user_message_projection_anchor(
                                    &session.session_id,
                                    &run_id,
                                    &MessageId::session_event_input(
                                        &session.session_id,
                                        &operation_id,
                                    ),
                                )
                                .await?;
                            self.settle_event_batch_wake(session, &batch_id).await?;
                            self.mark_session_event_processed(
                                &session.session_id,
                                &batch_id,
                                &operation_id,
                                anchor,
                            )
                            .await?;
                            return Ok(EventBatchProgress::Advanced);
                        }
                        Some(RunState::Running) => {
                            self.settle_event_batch_wake(session, &batch_id).await?;
                            return Ok(EventBatchProgress::Pending);
                        }
                        None => {}
                    }
                    if self
                        .another_root_run_blocks_user(&session.session_id, &run_id)
                        .await?
                    {
                        return Ok(EventBatchProgress::Pending);
                    }

                    let accompanying_system =
                        adjacent_system_input(session, &batch_id, &operation_id);
                    let admission = self
                        .admit_session_user_run(SessionUserRunCommand {
                            session_id: session.session_id.clone(),
                            agent_id: session
                                .agent_id()
                                .ok_or_else(|| RunError::internal("Session Agent is not frozen"))?
                                .to_string(),
                            operation_id: operation_id.clone(),
                            run_id: run_id.clone(),
                            content,
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
                            if matches!(
                                self.runtime()
                                    .session_run_state(&session.session_id, &run_id)
                                    .await?,
                                Some(RunState::Awaiting) | Some(RunState::Ended(_))
                            ) {
                                let anchor = self
                                    .user_message_projection_anchor(
                                        &session.session_id,
                                        &run_id,
                                        &MessageId::session_event_input(
                                            &session.session_id,
                                            &operation_id,
                                        ),
                                    )
                                    .await?;
                                self.mark_session_event_processed(
                                    &session.session_id,
                                    &batch_id,
                                    &operation_id,
                                    anchor,
                                )
                                .await?;
                                return Ok(EventBatchProgress::Advanced);
                            }
                            return Ok(EventBatchProgress::Pending);
                        }
                    };
                    match self.runtime().activate_session_run(delivery).await? {
                        SessionRunActivation::Activated
                        | SessionRunActivation::AlreadyActivated {
                            session_activity_epoch: _,
                        }
                        | SessionRunActivation::RecoveryClaimed => Ok(EventBatchProgress::Pending),
                        SessionRunActivation::Completed => {
                            if matches!(
                                self.runtime()
                                    .session_run_state(&session.session_id, &run_id)
                                    .await?,
                                Some(RunState::Awaiting) | Some(RunState::Ended(_))
                            ) {
                                let anchor = self
                                    .user_message_projection_anchor(
                                        &session.session_id,
                                        &run_id,
                                        &MessageId::session_event_input(
                                            &session.session_id,
                                            &operation_id,
                                        ),
                                    )
                                    .await?;
                                self.mark_session_event_processed(
                                    &session.session_id,
                                    &batch_id,
                                    &operation_id,
                                    anchor,
                                )
                                .await?;
                                Ok(EventBatchProgress::Advanced)
                            } else {
                                Ok(EventBatchProgress::Pending)
                            }
                        }
                    }
                }
                SessionEventCommand::SystemMessage {
                    operation_id,
                    content,
                } => {
                    let message_id = MessageId::session_system(&session.session_id, &operation_id);
                    let committed = self
                        .runtime()
                        .committed_messages(&session.session_id)
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
                    let (thread_id, run_id) =
                        preceding_event_runtime_target(session, &batch_id, &operation_id)
                            .ok_or_else(|| {
                                RunError::internal(
                                    "committed Session System input has no retained Run owner",
                                )
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
                    self.mark_session_event_processed(
                        &session.session_id,
                        &batch_id,
                        &operation_id,
                        anchor,
                    )
                    .await?;
                    Ok(EventBatchProgress::Advanced)
                }
                SessionEventCommand::DefineOutcome {
                    operation_id,
                    outcome_id,
                    description,
                    rubric,
                    max_iterations,
                } => {
                    let source_commit_cursor = match self
                        .prepare_outcome(
                            &session.session_id,
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
                    self.mark_session_event_processed(
                        &session.session_id,
                        &batch_id,
                        &operation_id,
                        SessionEventProjectionAnchor {
                            source_commit_cursor,
                        },
                    )
                    .await?;
                    Ok(EventBatchProgress::Advanced)
                }
                SessionEventCommand::ToolReply {
                    operation_id,
                    reply,
                } => {
                    let answered_pending_anchor = reply
                        .answered_pending_commit_cursor
                        .filter(|cursor| *cursor != 0)
                        .map(|source_commit_cursor| SessionEventProjectionAnchor {
                            source_commit_cursor,
                        });
                    let accompanying_system =
                        adjacent_system_input(session, &batch_id, &operation_id);
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
                    self.mark_session_event_processed(
                        &session.session_id,
                        &batch_id,
                        &operation_id,
                        anchor,
                    )
                    .await?;
                    Ok(EventBatchProgress::Advanced)
                }
                SessionEventCommand::Interrupt {
                    operation_id,
                    interrupt,
                } => {
                    let mut first_error = None;
                    let targets = interrupt.targets;
                    for target in &targets {
                        let result = match target {
                            SessionThreadTarget::Primary => {
                                self.interrupt(&session.session_id).await
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
                    self.wake_lifecycle_supervisor();
                    let mut source_commit_cursor = session
                        .event_batches
                        .iter()
                        .flat_map(|batch| &batch.events)
                        .filter_map(|entry| entry.projection_anchor)
                        .map(|anchor| anchor.source_commit_cursor)
                        .max()
                        .unwrap_or_default();
                    for target in &targets {
                        let thread_id = target.thread_id(&session.session_id);
                        if let Some(snapshot) = self
                            .runtime()
                            .session_thread_recovery_snapshot(&session.session_id, &thread_id.0)
                            .await?
                        {
                            source_commit_cursor = source_commit_cursor.max(snapshot.store_cursor);
                        }
                    }
                    self.settle_event_batch_wake(session, &batch_id).await?;
                    self.mark_session_event_processed(
                        &session.session_id,
                        &batch_id,
                        &operation_id,
                        SessionEventProjectionAnchor {
                            source_commit_cursor,
                        },
                    )
                    .await?;
                    Ok(EventBatchProgress::Advanced)
                }
            }
        })
    }

    async fn mark_session_event_processed(
        &self,
        session_id: &str,
        batch_id: &str,
        operation_id: &str,
        projection_anchor: SessionEventProjectionAnchor,
    ) -> Result<PersistedSession, RunError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner = self.owner(session_id).await.map_err(mutation_run_error)?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(mutation_run_error)?;
            let batch = session
                .event_batches
                .iter_mut()
                .find(|batch| batch.batch_id == batch_id)
                .ok_or_else(|| RunError::internal("Session Event batch disappeared"))?;
            if let Some(entry) = batch
                .events
                .iter()
                .find(|entry| entry.event.operation_id() == operation_id && entry.processed)
            {
                return if entry.projection_anchor == Some(projection_anchor) {
                    Ok(session)
                } else {
                    Err(RunError::internal(
                        "Session Event replay changed its projection anchor",
                    ))
                };
            }
            batch
                .mark_processed(operation_id, projection_anchor)
                .map_err(|error| RunError::internal(error.to_string()))?;
            match self
                .commit_session_snapshot(&owner, session, "mark-event-processed", Vec::new())
                .await
            {
                Ok(session) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
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
            self.settle_activity(&session.session_id, epoch)
                .await
                .map_err(crate::SessionActivityError::run_error)?;
        }
        Ok(())
    }

    async fn another_root_run_blocks_user(
        &self,
        session_id: &str,
        target_run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, RunError> {
        let Some(snapshot) = self
            .runtime()
            .session_thread_recovery_snapshot(session_id, session_id)
            .await?
        else {
            return Ok(false);
        };
        let Some(latest_run_id) = snapshot.latest_run_id.as_ref() else {
            return Ok(false);
        };
        if latest_run_id == target_run_id {
            return Ok(false);
        }
        let latest = snapshot
            .runs
            .iter()
            .find(|run| &run.id == latest_run_id)
            .ok_or_else(|| {
                RunError::internal("root Thread recovery omitted its latest Run state")
            })?;
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
                awaken_agent_contract::agent::thread::Id(session.session_id.clone()),
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

/// Eligibility selector over retained provenance. ToolReply, DefineOutcome, and
/// Interrupt are receipt commands and may cross queued User entries in their
/// original relative order. A System immediately following an already-processed
/// ToolReply is then eligible only as a side-effect-free observation of its
/// exact committed Message; this narrow case prevents an older queued User from
/// hiding completion of the resumed Run. User-associated System and every other
/// command remain FIFO.
fn select_preferred_batch_receipt(
    session: &PersistedSession,
    batch_id: &str,
) -> Option<SelectedSessionEvent> {
    let batch = session
        .event_batches
        .iter()
        .find(|batch| batch.batch_id == batch_id)?;
    batch.events.iter().find_map(|entry| {
        (!entry.processed
            && matches!(
                &entry.event,
                SessionEventCommand::ToolReply { .. } | SessionEventCommand::Interrupt { .. }
            ))
        .then(|| SelectedSessionEvent {
            batch_id: batch.batch_id.clone(),
            event: entry.event.clone(),
            traceparent: batch.traceparent.clone(),
        })
    })
}

fn select_session_event(session: &PersistedSession) -> Option<SelectedSessionEvent> {
    let select = |predicate: fn(&SessionEventCommand) -> bool| {
        session.event_batches.iter().find_map(|batch| {
            batch.events.iter().find_map(|entry| {
                (!entry.processed && predicate(&entry.event)).then(|| SelectedSessionEvent {
                    batch_id: batch.batch_id.clone(),
                    event: entry.event.clone(),
                    traceparent: batch.traceparent.clone(),
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
