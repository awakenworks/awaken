//! Terminal resource reclamation through the Session aggregate's one durable
//! cleanup operation.

use std::{future::Future, pin::Pin};

use awaken_session_contract::{
    PersistedSession, RenewSessionRealization, RunError, SandboxRestoreRequest,
    SessionRealizationControlFailure, SessionRealizationDriveError, SessionRuntime,
    SessionTerminalCleanupAction, SessionTerminalCleanupAssignment,
    SessionTerminalCleanupDriveOutcome,
};

use super::{internal, mutation_failure, repository_preparation};
use crate::{SessionApplication, SessionPreparationError};

pub(super) fn terminal_restore_target_for_thread(
    workspace_id: &str,
    session: &PersistedSession,
    thread_id: &str,
) -> Option<SandboxRestoreRequest> {
    (thread_id == session.session_id)
        .then(|| {
            session
                .environment
                .restoring_request(workspace_id, &session.session_id)
        })
        .flatten()
}

pub(super) fn bind_terminal_restore_target(
    workspace_id: &str,
    session: &PersistedSession,
    action: SessionTerminalCleanupAction,
) -> Result<SessionTerminalCleanupAction, SessionRealizationControlFailure> {
    match action {
        SessionTerminalCleanupAction::Prepare { mut commands } => {
            if let Some(root) = commands
                .iter_mut()
                .find(|command| command.thread_id == session.session_id)
                && let Some(request) =
                    terminal_restore_target_for_thread(workspace_id, session, &root.thread_id)
            {
                *root = root.clone().with_restore_target(request).map_err(|error| {
                    SessionRealizationControlFailure::Invalid(error.to_string())
                })?;
            }
            Ok(SessionTerminalCleanupAction::Prepare { commands })
        }
        SessionTerminalCleanupAction::Dispose { command } => {
            let command = match terminal_restore_target_for_thread(
                workspace_id,
                session,
                &session.session_id,
            ) {
                Some(request) => command.with_restore_target(request).map_err(|error| {
                    SessionRealizationControlFailure::Invalid(error.to_string())
                })?,
                None => command,
            };
            Ok(SessionTerminalCleanupAction::Dispose { command })
        }
        SessionTerminalCleanupAction::Waiting => Ok(SessionTerminalCleanupAction::Waiting),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalCleanupPath {
    Completed,
    NormalizeLegacy,
    WaitForExternalWorker,
    DriveLocal,
}

fn terminal_cleanup_path(
    completed: bool,
    legacy_receipts_complete: bool,
    requires_external_realization: bool,
) -> TerminalCleanupPath {
    if completed {
        TerminalCleanupPath::Completed
    } else if legacy_receipts_complete {
        TerminalCleanupPath::NormalizeLegacy
    } else if requires_external_realization {
        TerminalCleanupPath::WaitForExternalWorker
    } else {
        TerminalCleanupPath::DriveLocal
    }
}

fn terminal_driver_failure(
    error: awaken_session_contract::SessionRealizationDriveError,
) -> SessionPreparationError {
    use awaken_session_contract::{
        SessionRealizationControlFailure as ControlFailure,
        SessionRealizationDriveError as DriveError,
    };

    match error {
        DriveError::Effect(error) => SessionPreparationError::Rejected(error),
        DriveError::Control(ControlFailure::NotFound) => SessionPreparationError::NotFound,
        DriveError::Control(
            ControlFailure::NotReady
            | ControlFailure::Retired
            | ControlFailure::Terminal
            | ControlFailure::StaleOwnership
            | ControlFailure::Conflict,
        ) => SessionPreparationError::Conflict,
        DriveError::Control(ControlFailure::Invalid(detail)) => SessionPreparationError::Rejected(
            RunError::classified("session_terminal_cleanup_projection_invalid", detail),
        ),
        DriveError::Control(ControlFailure::Unavailable(detail)) => {
            SessionPreparationError::Unavailable(detail)
        }
        DriveError::DidNotConverge => SessionPreparationError::Unavailable(
            "Session terminal cleanup protocol did not converge".into(),
        ),
    }
}

async fn supervise_local_terminal_cleanup_lease<Drive, Renew, RenewFuture, Clock>(
    session_id: &str,
    initial_assignment: &SessionTerminalCleanupAssignment,
    runtime: &dyn SessionRuntime,
    drive: Drive,
    mut renew: Renew,
    now_unix_ms: Clock,
) -> Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError>
where
    Drive:
        Future<Output = Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError>>,
    Renew: FnMut(RenewSessionRealization) -> RenewFuture,
    RenewFuture: Future<
        Output = Result<
            awaken_session_contract::SessionRealizationLease,
            SessionRealizationControlFailure,
        >,
    >,
    Clock: Fn() -> u64,
{
    if initial_assignment.session_id != session_id {
        return Err(SessionRealizationDriveError::Control(
            SessionRealizationControlFailure::Invalid(
                "local terminal cleanup assignment names another Session".into(),
            ),
        ));
    }
    let mut asserted_lease = initial_assignment.lease.clone();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        crate::realization::LOCAL_TERMINAL_CLEANUP_RENEW_INTERVAL_MS,
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Tokio intervals have one immediately-ready first tick. Consume it before
    // entering the biased race so a short cleanup never writes a needless root
    // renewal merely because its drive and the synthetic first tick are ready
    // together.
    interval.tick().await;
    let mut drive = std::pin::pin!(drive);

    loop {
        tokio::select! {
            biased;
            outcome = &mut drive => return outcome,
            _ = interval.tick() => {
                // This companion owns no effect or retry state. While renewal
                // is in flight the canonical drive is deliberately not polled;
                // only a verified same-generation CAS readback lets it resume.
                let requested_expiry_unix_ms = now_unix_ms()
                    .checked_add(crate::realization::LOCAL_SESSION_REALIZATION_LEASE_MS)
                    .map(|requested| requested.max(asserted_lease.expires_at_unix_ms))
                    .ok_or_else(|| {
                        SessionRealizationDriveError::Control(
                            SessionRealizationControlFailure::Unavailable(
                                "local terminal cleanup renewal expiry overflow".into(),
                            ),
                        )
                    })?;
                let command = RenewSessionRealization {
                    session_id: session_id.to_string(),
                    asserted_lease: asserted_lease.clone(),
                    requested_expires_at_unix_ms: requested_expiry_unix_ms,
                };
                let renewed_lease = renew(command)
                    .await
                    .map_err(SessionRealizationDriveError::Control)?;
                if !awaken_session_contract::realization_lease_generation_authorizes(
                        &renewed_lease,
                        &asserted_lease,
                    )
                    || renewed_lease.expires_at_unix_ms < requested_expiry_unix_ms
                {
                    return Err(SessionRealizationDriveError::Control(
                        SessionRealizationControlFailure::Invalid(
                            "local terminal cleanup renewal returned another or regressed generation"
                                .into(),
                        ),
                    ));
                }
                // Renewal is deliberately lease-only. Reinstall the original
                // aggregate-frozen terminal projection under that monotonic
                // successor; neither Control nor this companion rebuilds a
                // second projection or terminal-specific renewal read model.
                let renewed_assignment = SessionTerminalCleanupAssignment {
                    session_id: initial_assignment.session_id.clone(),
                    projection: initial_assignment.projection.clone(),
                    lease: renewed_lease.clone(),
                };
                runtime
                    .install_terminal_cleanup_assignment(&renewed_assignment)
                    .await
                    .map_err(SessionRealizationDriveError::Effect)?;
                asserted_lease = renewed_lease;
            }
        }
    }
}

impl SessionApplication {
    async fn drive_local_terminal_cleanup(
        &self,
        assignment: &SessionTerminalCleanupAssignment,
    ) -> Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError> {
        let session_id = assignment.session_id.as_str();
        supervise_local_terminal_cleanup_lease(
            session_id,
            assignment,
            self.runtime(),
            awaken_session_contract::drive_session_terminal_cleanup(
                session_id,
                &assignment.lease,
                self,
                self.runtime(),
            ),
            |command| async move {
                let lease =
                    awaken_session_contract::SessionRealizationControl::renew_session_realization(
                        self, command,
                    )
                    .await?;
                Ok(lease)
            },
            crate::activity::now_unix_ms,
        )
        .await
    }

    /// The sole terminal cleanup implementation shared by archive/delete edges
    /// and background recovery. The target set comes only from the Runtime's
    /// durable delegation authority after the terminal fence has stopped the
    /// parent; protocol projections are never accepted as cleanup authority.
    /// Every frozen child Runtime is attempted even when another teardown fails;
    /// durable completion commits only when all external effects succeed.
    pub async fn release_terminal_resources(
        &self,
        owner_scope: &str,
        session_id: &str,
    ) -> Result<Option<PersistedSession>, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            match self
                .release_terminal_resources_once(owner_scope, session_id)
                .await
            {
                Err(SessionPreparationError::NotFound) => {
                    // Delete removes the aggregate only after the durable
                    // cleanup operation is complete. An eager actor can hold a
                    // stale snapshot while the lifecycle supervisor wins the
                    // final CAS and tombstones the Session; that loser observes
                    // NotFound at its next phase CAS. Normalize the same
                    // terminal truth as the entry read instead of reporting a
                    // recoverable cleanup failure after cleanup already won.
                    return Ok(None);
                }
                Err(SessionPreparationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    // Delete admission deliberately wakes the durable lifecycle
                    // driver and also starts an eager cleanup attempt. Another
                    // process may do the same after failover. Every external
                    // effect below has a durable idempotency identity, so a root
                    // CAS loser must re-read the aggregate and resume from the
                    // winning phase instead of stranding a Deleting Session.
                    continue;
                }
                result => return result,
            }
        }
        Err(SessionPreparationError::Conflict)
    }

    fn release_terminal_resources_once<'a>(
        &'a self,
        owner_scope: &'a str,
        session_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<PersistedSession>, SessionPreparationError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut session = match self.session_repository().get(session_id).await {
                Ok(session) => session,
                Err(awaken_session_contract::SessionRepositoryError::NotFound) => return Ok(None),
                Err(error) => return Err(repository_preparation(error)),
            };
            let cleanup_completed = session
                .verified_terminal_cleanup()
                .map_err(internal)?
                .is_completed();
            let mut drove_local_cleanup = false;
            if !cleanup_completed {
                // Persist the admission fence before *any* external cleanup effect.
                // This also upgrades legacy terminal rows that predate the explicit
                // cleanup operation.
                if session.ensure_terminal_cleanup_fence() {
                    session = self
                        .commit_resource_snapshot(
                            owner_scope,
                            session,
                            "terminal-cleanup-fence",
                            Vec::new(),
                        )
                        .await
                        .map_err(mutation_failure)?;
                }
                // Work retirement belongs to the recoverable cleanup operation.
                // Completed archive cleanup is absorbing: a replay must not issue a
                // second retirement attempt through this or any other reconciler.
                self.retire_terminal_work(&session).await?;

                // Phase 2 interrupts and waits for the parent to settle, then freezes the
                // complete durable delegated-Run set and its committed watermark. A retry
                // after this commit reuses exactly these targets.
                let mut intent_changed = false;
                if session.terminal_cleanup.is_fenced() {
                    let snapshot = self
                        .runtime()
                        .quiesce_terminal_delegations(session_id)
                        .await
                        .map_err(SessionPreparationError::Rejected)?;
                    let thread_ids = snapshot
                        .delegated_runs
                        .into_iter()
                        .map(|delegated| delegated.run_id.0)
                        .chain(
                            snapshot
                                .coordinated_thread_ids
                                .into_iter()
                                .map(|thread| thread.0),
                        );
                    intent_changed = session
                        .freeze_terminal_cleanup_targets(
                            thread_ids,
                            snapshot.watermark,
                            snapshot.runtime_commit_cursor,
                        )
                        .map_err(internal)?;
                }
                if intent_changed {
                    session = self
                        .commit_resource_snapshot(
                            owner_scope,
                            session,
                            "resource-release-intent",
                            Vec::new(),
                        )
                        .await
                        .map_err(mutation_failure)?;
                }

                let legacy_receipts_complete = session
                    .has_complete_legacy_terminal_cleanup_evidence()
                    .map_err(internal)?;
                match terminal_cleanup_path(
                    false,
                    legacy_receipts_complete,
                    self.requires_external_realization(&session),
                ) {
                    TerminalCleanupPath::NormalizeLegacy => {
                        // Historical one-stage receipts already prove physical
                        // deletion. Reuse the canonical Repository participant
                        // reconciler, then normalize only those exact bytes;
                        // never replay a Runtime effect or revive its old
                        // per-thread acknowledgement path.
                        let (next_session, _repository_preparation) = self
                            .prepare_terminal_repository_participants(owner_scope, session)
                            .await?;
                        session = next_session;
                        session
                            .normalize_legacy_terminal_cleanup(
                                "Session terminated before activation completed",
                            )
                            .map_err(internal)?;
                        session = self
                            .commit_resource_snapshot(
                                owner_scope,
                                session,
                                "resource-release-complete",
                                Vec::new(),
                            )
                            .await
                            .map_err(mutation_failure)?;
                    }
                    TerminalCleanupPath::WaitForExternalWorker => {
                        // The aggregate itself is the remote queue. A Worker
                        // claims this frozen root and runs the same driver; the
                        // local lifecycle owns no shadow completion loop.
                    }
                    TerminalCleanupPath::DriveLocal => {
                        let assignment = self
                            .claim_local_terminal_cleanup_assignment(session_id)
                            .await
                            .map_err(|error| terminal_driver_failure(error.into()))?;
                        self.drive_local_terminal_cleanup(&assignment)
                            .await
                            .map_err(terminal_driver_failure)?;
                        drove_local_cleanup = true;
                        session = match self.session_repository().get(session_id).await {
                            Ok(session) => session,
                            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                                return Ok(None);
                            }
                            Err(error) => return Err(repository_preparation(error)),
                        };
                    }
                    TerminalCleanupPath::Completed => {
                        unreachable!("the incomplete branch cannot classify cleanup as completed")
                    }
                }
            }
            let cleanup_completed = session
                .verified_terminal_cleanup()
                .map_err(internal)?
                .is_completed();
            if cleanup_completed
                && !self.requires_external_realization(&session)
                && !drove_local_cleanup
                && let Some(lease) = session.realization.as_ref()
            {
                // The disposal receipt CAS may lose its response. One
                // aggregate-level acknowledgement drains the retained local
                // projection without reconstructing the deleted one-stage
                // per-thread effect registry.
                self.runtime()
                    .acknowledge_completed_terminal_cleanup(session_id, lease)
                    .await;
            }
            if !cleanup_completed {
                // Pending local or remote work remains represented by this
                // root. In particular, a hidden Delete must not compact away
                // the aggregate that is the Work queue and receipt authority.
                return Ok(Some(session));
            }
            if session.is_hidden() {
                if session.has_incomplete_event_batches() {
                    // The canonical Event-batch supervisor must first preserve and
                    // resolve every accepted receipt under the same root CAS. A
                    // compact tombstone cannot carry that provenance and resource
                    // reconciliation never executes or cancels those commands.
                    return Ok(Some(session));
                }
                self.commit_delete_tombstone(owner_scope, &session)
                    .await
                    .map_err(mutation_failure)?;
                return Ok(None);
            }
            Ok(Some(session))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_contract::SessionRealizationLease;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    struct DriveDropGuard(Arc<AtomicBool>);

    impl Drop for DriveDropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct RecordingTerminalProjectionRuntime {
        installed: Mutex<Vec<awaken_session_contract::SessionTerminalCleanupAssignment>>,
        fail_install: AtomicBool,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRuntime for RecordingTerminalProjectionRuntime {
        async fn install_terminal_cleanup_assignment(
            &self,
            assignment: &awaken_session_contract::SessionTerminalCleanupAssignment,
        ) -> Result<(), RunError> {
            if self.fail_install.swap(false, Ordering::SeqCst) {
                return Err(RunError::unavailable(
                    "injected terminal projection installation failure",
                ));
            }
            self.installed.lock().unwrap().push(assignment.clone());
            Ok(())
        }

        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("terminal renewal fixture"))
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: awaken_session_contract::ToolPermissionDecision,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("terminal renewal fixture"))
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
            _is_error: bool,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("terminal renewal fixture"))
        }

        fn model(&self) -> String {
            "terminal-renewal-fixture".into()
        }
    }

    fn local_lease(now_unix_ms: u64) -> SessionRealizationLease {
        SessionRealizationLease {
            owner: "local-runtime".into(),
            runtime_incarnation: "local-runtime/boot".into(),
            epoch: 7,
            expires_at_unix_ms: now_unix_ms
                .checked_add(crate::realization::LOCAL_SESSION_REALIZATION_LEASE_MS)
                .expect("test lease horizon"),
        }
    }

    fn local_assignment(now_unix_ms: u64) -> SessionTerminalCleanupAssignment {
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "environment".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "environment-v1".into(),
                    ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization:
                        awaken_credential_contract::CredentialRealizationProfile {
                            inference_holder: awaken_credential_contract::PlaintextHolder::new(
                                awaken_credential_contract::PlaintextBoundary::Workload,
                                "awaken.workload.acp",
                            ),
                            mcp_holder: awaken_credential_contract::PlaintextHolder::new(
                                awaken_credential_contract::PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            resource_holder: awaken_credential_contract::PlaintextHolder::new(
                                awaken_credential_contract::PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                        },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "agent".into(),
                agent_revision: None,
                model: "model".into(),
                model_override: None,
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        );
        SessionTerminalCleanupAssignment {
            session_id: "session".into(),
            projection: awaken_session_contract::FrozenSessionProjection {
                workspace_id: "workspace".into(),
                revision: awaken_session_contract::SessionRevision(2),
                baseline,
                agent_publication: None,
                environment: Default::default(),
                resource_revision: 0,
                resources: Default::default(),
                previous_resource_manifest: Some(
                    awaken_session_contract::SessionResourceManifest::at_revision(
                        "workspace",
                        0,
                        Default::default(),
                    ),
                ),
                tools: Default::default(),
                mcp: Vec::new(),
                request_context: Vec::new(),
            },
            lease: local_lease(now_unix_ms),
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum RenewalRejection {
        Error,
        ForeignOwner,
        SuccessorEpoch,
        RegressedExpiry,
        InstallationError,
    }
}
