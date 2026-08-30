//! Terminal resource reclamation through the Session aggregate's one durable
//! cleanup operation.

use std::{future::Future, pin::Pin};

use awaken_session_contract::{PersistedSession, RunError};

use super::{internal, mutation_failure, repository_preparation};
use crate::{SessionApplication, SessionPreparationError};

impl SessionApplication {
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

    async fn execute_local_terminal_cleanup_batch(
        &self,
        owner_scope: &str,
        session_id: &str,
        session: &mut PersistedSession,
        commands: Vec<awaken_session_contract::SessionCleanupCommand>,
    ) -> Result<(bool, Option<RunError>), SessionPreparationError> {
        let mut changed = false;
        let mut teardown_error = None;
        for command in commands {
            let command = match session
                .environment
                .restoring_request(owner_scope, session_id)
            {
                Some(request) if command.thread_id == session_id => {
                    command.with_restore_target(request).map_err(internal)?
                }
                _ => command,
            };
            match self
                .runtime()
                .execute_terminal_cleanup(command.clone())
                .await
            {
                Ok(completion) => {
                    let completion = completion
                        .into_aggregate_completion(&command)
                        .map_err(internal)?;
                    match session
                        .record_terminal_cleanup_completion(completion)
                        .map_err(internal)
                    {
                        Ok(recorded) => changed |= recorded,
                        Err(error) => {
                            teardown_error.get_or_insert(RunError::internal(format!(
                                "Session cleanup completion mismatch: {error}"
                            )));
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session = session_id,
                        thread = %command.thread_id,
                        effect_id = %command.effect_id,
                        error = ?error,
                        "Session terminal Runtime teardown remains pending"
                    );
                    teardown_error.get_or_insert(error);
                }
            }
        }
        Ok((changed, teardown_error))
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

                if session.terminal_cleanup.is_requested() {
                    let receipts = if self.requires_external_realization(&session) {
                        session
                            .terminal_cleanup
                            .recorded_receipts(session_id)
                            .map_err(|error| {
                                SessionPreparationError::Rejected(RunError::unavailable_classified(
                                    "session_cleanup_worker_pending",
                                    format!(
                                        "remote Session terminal cleanup remains pending: {error}"
                                    ),
                                ))
                            })?
                    } else {
                        // The cleanup operation projects only child commands until
                        // they are durable. Publication then runs and commits its
                        // receipt before the root finalizer can be projected.
                        let commands = session
                            .terminal_cleanup
                            .pending_commands(session_id)
                            .map_err(internal)?;
                        let (changed, teardown_error) = self
                            .execute_local_terminal_cleanup_batch(
                                owner_scope,
                                session_id,
                                &mut session,
                                commands,
                            )
                            .await?;
                        if changed {
                            session = self
                                .commit_resource_snapshot(
                                    owner_scope,
                                    session,
                                    "terminal-cleanup-local-child-receipts",
                                    Vec::new(),
                                )
                                .await
                                .map_err(mutation_failure)?;
                        }
                        if let Some(error) = teardown_error {
                            return Err(SessionPreparationError::Rejected(error));
                        }

                        if let Some(command) = session
                            .terminal_cleanup
                            .publication_command(session_id)
                            .map_err(internal)?
                        {
                            let effect = self
                                .runtime()
                                .execute_terminal_repository_publication(command)
                                .await
                                .map_err(SessionPreparationError::Rejected)?;
                            match effect {
                                awaken_session_contract::SessionRepositoryPublicationEffect::Published(
                                    receipt,
                                ) => {
                                    session
                                        .terminal_cleanup
                                        .record_repository_publication_receipt(session_id, receipt)
                                        .map_err(internal)?;
                                }
                                awaken_session_contract::SessionRepositoryPublicationEffect::Rejected(
                                    rejection,
                                ) => {
                                    session
                                        .terminal_cleanup
                                        .record_repository_publication_rejection(
                                            session_id,
                                            rejection,
                                        )
                                        .map_err(internal)?;
                                }
                            }
                            session = self
                                .commit_resource_snapshot(
                                    owner_scope,
                                    session,
                                    "terminal-repository-publication-local-outcome",
                                    Vec::new(),
                                )
                                .await
                                .map_err(mutation_failure)?;
                        }

                        let root_commands = session
                            .terminal_cleanup
                            .pending_commands(session_id)
                            .map_err(internal)?;
                        let (changed, teardown_error) = self
                            .execute_local_terminal_cleanup_batch(
                                owner_scope,
                                session_id,
                                &mut session,
                                root_commands,
                            )
                            .await?;
                        if changed {
                            session = self
                                .commit_resource_snapshot(
                                    owner_scope,
                                    session,
                                    "terminal-cleanup-local-root-receipt",
                                    Vec::new(),
                                )
                                .await
                                .map_err(mutation_failure)?;
                        }
                        if let Some(error) = teardown_error {
                            return Err(SessionPreparationError::Rejected(error));
                        }
                        if session
                            .terminal_cleanup
                            .publication_command(session_id)
                            .map_err(internal)?
                            .is_some()
                            || !session
                                .terminal_cleanup
                                .pending_commands(session_id)
                                .map_err(internal)?
                                .is_empty()
                        {
                            return Err(SessionPreparationError::Rejected(
                                RunError::unavailable_classified(
                                    "session_cleanup_local_pending",
                                    "local Session terminal cleanup remains pending",
                                ),
                            ));
                        }
                        session
                            .terminal_cleanup
                            .recorded_receipts(session_id)
                            .map_err(internal)?
                    };
                    if let Some(checkpoint) = session.environment.checkpoint().cloned() {
                        self.runtime()
                            .delete_session_checkpoint(session_id, &checkpoint)
                            .await
                            .map_err(SessionPreparationError::Rejected)?;
                    }
                    if !self
                        .retire_session_repositories(owner_scope, session_id, &session.resources)
                        .await
                    {
                        return Err(internal(
                            "Session-scoped Repository cleanup remains pending",
                        ));
                    }
                    session
                        .complete_terminal_cleanup(
                            &receipts,
                            "Session terminated before activation completed",
                        )
                        .map_err(internal)?;
                }
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
