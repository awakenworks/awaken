// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the canonical Session aggregate admission for one frozen terminal
// @formal reason: Repository publication intent. Dynamic path: an active Session atomically validates the
// @formal reason: exact active input, freezes the publication intent, and enters the existing archive
// @formal reason: transition, while exact replay is inert and every mismatch leaves the aggregate
// @formal reason: unchanged. Evidence: session.lifecycle.session_state and the adjacent aggregate
// @formal reason: decision-table test; this is a product-requirement classification, not a direct
// @formal reason: formal-proof claim.
//! Atomic archive transition with one frozen Repository publication intent.

use super::{PersistedSession, SessionDisposition, SessionDispositionTransitionError};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionArchiveWithRepositoryPublicationError {
    #[error(transparent)]
    Disposition(#[from] SessionDispositionTransitionError),
    #[error(transparent)]
    Cleanup(#[from] crate::state::SessionCleanupError),
}

/// Operational capacity projection of one authoritative Session root. It is a
/// read hint only: neither this value nor an index derived from it can grant a
/// realization lease, Run claim, or physical effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWorkerObligation<'a> {
    None,
    RetainedOwner(&'a str),
    UnboundInitialPreparation,
    UnboundTerminalCleanup,
}

#[must_use]
fn publication_input_is_exactly_active(
    active_inputs: &[crate::state::ResolvedInput],
    asserted: &crate::state::ResolvedInput,
) -> bool {
    active_inputs
        .iter()
        .filter(|input| *input == asserted)
        .count()
        == 1
}

impl PersistedSession {
    /// Admit negative publication evidence through the aggregate's exact source
    /// before the sidecar mutation. Once retained, byte-identical replay remains
    /// valid after ordinary cleanup has cleared the Environment binding.
    pub fn record_repository_publication_rejection(
        &mut self,
        rejection: crate::state::SessionRepositoryPublicationRejection,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        let durable = self
            .terminal_cleanup
            .repository_publication_rejection(&self.session_id)?;
        if durable != Some(&rejection)
            && !rejection
                .effect_rejection
                .matches_source_binding(self.environment.terminal_repository_publication_binding())
        {
            return Err(crate::state::SessionCleanupError::FrozenRepositoryPublicationMismatch);
        }
        self.terminal_cleanup
            .record_repository_publication_rejection(&self.session_id, rejection)
    }

    /// Return terminal cleanup authority only after binding the retained
    /// operation and any publication sidecar to this aggregate's Session identity.
    pub fn verified_terminal_cleanup(
        &self,
    ) -> Result<&crate::state::SessionCleanupOperation, crate::state::SessionCleanupError> {
        self.terminal_cleanup.verify_for(&self.session_id)?;
        Ok(&self.terminal_cleanup)
    }

    /// Whether this terminal root still requires its assigned Worker to finish
    /// cleanup. This is an operational demand projection, never permission to
    /// reassign a lost physical owner or to synthesize an exit receipt.
    pub fn needs_terminal_cleanup_worker(&self) -> Result<bool, crate::state::SessionCleanupError> {
        let cleanup = self.verified_terminal_cleanup()?;
        Ok(self.is_terminal() && !cleanup.is_not_requested() && !cleanup.is_completed())
    }

    /// A realization remains an obligation until this root proves terminal
    /// cleanup. Actor-retirement source retention does not clear it; actual
    /// exit and lease succession may transfer the same root to its next owner.
    /// Lease expiry and a settled Run do not release the obligation.
    pub fn has_unfinished_worker_realization(
        &self,
    ) -> Result<bool, crate::state::SessionCleanupError> {
        let cleanup = self.verified_terminal_cleanup()?;
        if !self.is_terminal()
            && (matches!(
                self.environment,
                crate::state::SessionEnvironmentState::Hibernated { .. }
            ) || self
                .environment_materialization()
                .is_some_and(|facts| facts.target_not_admitted(&self.environment)))
        {
            return Ok(false);
        }
        Ok(self.realization.is_some() && !cleanup.is_completed())
    }

    /// Initial opt-in or an Idle MCP update is preparation work on this root,
    /// without a model Run. Self-hosted initial placement stays WorkQueue-owned;
    /// only its already-bound Runtime owner may enter the MCP-update lane.
    /// Exact placement and root-CAS admission remain with SessionApplication.
    #[must_use]
    pub fn is_run_free_preparation_candidate(&self) -> bool {
        let Some(baseline) = self.frozen_baseline() else {
            return false;
        };
        let initial = self.realization_progress.run_free_initial_preparation
            && self.needs_initial_realization_recovery()
            && !baseline.environment.self_hosted;
        let mcp_update = self.execution == crate::state::SessionExecutionState::Idle
            && self.environment.admits_realization_effects()
            && !self.has_active_activities()
            && self.mcp.needs_realization()
            && (!baseline.environment.self_hosted || self.realization.is_some());
        let continuation = self.execution == crate::state::SessionExecutionState::Idle
            && !self.has_active_activities()
            && !baseline.environment.self_hosted
            && matches!(
                self.environment,
                crate::state::SessionEnvironmentState::Suspending { .. }
                    | crate::state::SessionEnvironmentState::Restoring { .. }
                    | crate::state::SessionEnvironmentState::Hibernated { .. }
            );
        !self.is_terminal()
            && self.requires_external_environment_owner()
            && (initial || mcp_update || continuation)
    }

    #[must_use]
    pub fn needs_run_free_preparation_worker(&self) -> bool {
        !matches!(
            self.environment,
            crate::state::SessionEnvironmentState::Hibernated { .. }
        ) && (self.realization.is_none()
            || self
                .environment_materialization()
                .is_some_and(|facts| facts.target_not_admitted(&self.environment)))
            && self.is_run_free_preparation_candidate()
    }

    /// Classify the one Worker capacity cause retained by this root. A bound
    /// owner wins over pending work because its exact physical identity must
    /// survive a settled or delayed Run until verified cleanup completes.
    pub fn worker_obligation(
        &self,
    ) -> Result<SessionWorkerObligation<'_>, crate::state::SessionCleanupError> {
        if self.has_unfinished_worker_realization()? {
            let owner = self
                .realization
                .as_ref()
                .ok_or(crate::state::SessionCleanupError::MissingPhysicalOwner)?
                .owner
                .as_str();
            if owner.trim().is_empty() {
                return Err(crate::state::SessionCleanupError::InvalidWorkerOwner);
            }
            return Ok(SessionWorkerObligation::RetainedOwner(owner));
        }
        if self.needs_run_free_preparation_worker() {
            return Ok(SessionWorkerObligation::UnboundInitialPreparation);
        }
        if self.needs_terminal_cleanup_worker()? {
            if !matches!(
                self.environment,
                crate::state::SessionEnvironmentState::Unmaterialized
                    | crate::state::SessionEnvironmentState::Hibernated { .. }
            ) && !self
                .environment_materialization()
                .is_some_and(|facts| facts.target_not_admitted(&self.environment))
            {
                return Err(crate::state::SessionCleanupError::MissingPhysicalOwner);
            }
            return Ok(SessionWorkerObligation::UnboundTerminalCleanup);
        }
        Ok(SessionWorkerObligation::None)
    }

    pub(super) fn has_verified_completed_cleanup(&self) -> bool {
        self.verified_terminal_cleanup()
            .is_ok_and(crate::state::SessionCleanupOperation::is_completed)
    }

    pub(super) fn verified_cleanup_needs_reconciliation(&self) -> bool {
        self.verified_terminal_cleanup().map_or(
            true,
            crate::state::SessionCleanupOperation::needs_reconciliation,
        )
    }

    /// Atomically freeze one exact Repository publication intent before the
    /// existing archive transition installs its terminal fence. Adapters call
    /// this single aggregate API so none can accidentally archive first and then
    /// attempt to upgrade an already-frozen no-publication operation. An already
    /// Archived exact replay is admitted from that immutable intent even after
    /// terminal Resource release cleared the formerly active manifest.
    pub fn archive_with_repository_publication(
        &mut self,
        archived_at: impl Into<String>,
        archived_at_unix_ms: Option<u64>,
        intent: crate::state::SessionRepositoryPublicationIntent,
    ) -> Result<bool, SessionArchiveWithRepositoryPublicationError> {
        match self.disposition {
            SessionDisposition::Deleting | SessionDisposition::Deleted => {
                return Err(SessionDispositionTransitionError::ArchiveAfterDelete.into());
            }
            SessionDisposition::Archived { .. } => {
                self.terminal_cleanup
                    .request_with_publication(&self.session_id, intent)?;
                return Ok(false);
            }
            SessionDisposition::Active => {}
        }
        if self
            .environment
            .terminal_repository_publication_binding()
            .is_none()
        {
            return Err(
                crate::state::SessionCleanupError::InvalidRepositoryPublicationIntent(
                    "Repository publication requires an existing live Session Environment source"
                        .into(),
                )
                .into(),
            );
        }
        if !publication_input_is_exactly_active(self.resources.active.inputs(), &intent.input) {
            return Err(
                crate::state::SessionCleanupError::InvalidRepositoryPublicationIntent(
                    "publication input does not match exactly one active Session input".into(),
                )
                .into(),
            );
        }
        let archived_at_unix_ms =
            archived_at_unix_ms.ok_or(SessionDispositionTransitionError::ArchiveTimeRequired)?;
        self.terminal_cleanup
            .request_with_publication(&self.session_id, intent)?;
        self.archive(archived_at, archived_at_unix_ms)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::session_repo::mutation_tests::session;
    use crate::state::{SessionExecutionState, SessionRevision};

    #[test]
    fn worker_demand_requires_terminal_requested_verified_cleanup() {
        // Causes: C1 realization absent/present; C2 live/terminal execution;
        // C3 cleanup unrequested/requested/in-progress. Effects: R1 absent
        // realization -> no retained owner; R2 present, live or unfinished
        // terminal -> retained owner; R3 terminal cleanup needs a Worker only
        // after request. Completed cleanup and expired leases are covered by
        // the real Session store/application tests in worker_affinity.rs.
        let mut root = session("terminal-worker-demand", SessionRevision(1));
        assert!(!root.needs_terminal_cleanup_worker().unwrap(), "R1");
        assert!(!root.has_unfinished_worker_realization().unwrap(), "R1");
        root.realization = Some(
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "worker-root".into(),
                runtime_incarnation: "worker-root:pod-uid".into(),
                epoch: 1,
                expires_at_unix_ms: 1,
            },
        );
        assert!(root.has_unfinished_worker_realization().unwrap(), "R2");
        let id = root.session_id.clone();
        assert!(root.terminal_cleanup.request(&id));
        assert!(!root.needs_terminal_cleanup_worker().unwrap(), "R1 live");
        root.archive("2026-10-05T00:00:00Z", 1).unwrap();
        assert!(root.needs_terminal_cleanup_worker().unwrap(), "R2");
        assert!(root.has_unfinished_worker_realization().unwrap(), "R2");
        root.freeze_terminal_cleanup_targets([], 0, 0).unwrap();
        assert!(
            root.needs_terminal_cleanup_worker().unwrap(),
            "R2 in progress"
        );
    }

    fn repository_publication_intent() -> crate::state::SessionRepositoryPublicationIntent {
        serde_json::from_value(serde_json::json!({
            "input": {
                "binding_id": "source",
                "source": {
                    "kind": "repository",
                    "repository_id": "repo-1",
                    "config": {
                        "repository_id": "repo-1",
                        "version": 7,
                        "remote_url": "https://example.test/repo.git"
                    }
                },
                "mount_path": "/workspace/source",
                "access": "read_write"
            },
            "expectation": {
                "branch": "awf/work",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }
        }))
        .unwrap()
    }

    fn install_publication_input(
        session: &mut PersistedSession,
        intent: &crate::state::SessionRepositoryPublicationIntent,
    ) {
        session.resources = crate::state::SessionResourceState::from_active(
            crate::state::ResolvedSessionResources::try_new(vec![intent.input.clone()], Vec::new())
                .unwrap(),
        );
    }

    fn install_live_publication_source(session: &mut PersistedSession) {
        session.environment = crate::state::SessionEnvironmentState::Resident {
            binding: "resident-publication-source".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    }

    #[test]
    fn source_closure_matches_the_aggregate_before_admission_and_replays_after_release() {
        // Source admission table: A9 exact command + exact source -> retain
        // one negative outcome and expose ordinary root cleanup; A10 foreign
        // source/missing source -> no mutation; A11 an already-recorded exact
        // negative replays after source release -> no mutation/no new command;
        // A12 a different closure cannot replace the retained one. This pure
        // aggregate fixture does not assert a physical cleanup receipt.
        let id = crate::state::SessionId::from("source-closure");
        let mut pending = session(id.as_str(), SessionRevision(1));
        let intent = repository_publication_intent();
        install_publication_input(&mut pending, &intent);
        install_live_publication_source(&mut pending);
        pending
            .archive_with_repository_publication("2026-09-28T00:00:00Z", Some(100), intent)
            .unwrap();
        pending.freeze_terminal_cleanup_targets([], 0, 0).unwrap();
        let command = pending
            .terminal_cleanup
            .publication_command(&id)
            .unwrap()
            .unwrap();
        let observation =
            awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: None,
            };
        let rejection = crate::state::SessionRepositoryPublicationRejection::source_closed(
            &command,
            "resident-publication-source",
            observation.clone(),
        )
        .unwrap();
        let mut foreign = pending.clone();
        foreign.environment.set_resident("foreign-source");
        let before = foreign.clone();
        assert!(
            foreign
                .record_repository_publication_rejection(rejection.clone())
                .is_err(),
            "A10 foreign source"
        );
        assert_eq!(foreign, before, "A10 no mutation");
        foreign.environment = crate::state::SessionEnvironmentState::Unmaterialized;
        let before = foreign.clone();
        assert!(
            foreign
                .record_repository_publication_rejection(rejection.clone())
                .is_err(),
            "A10 missing source"
        );
        assert_eq!(foreign, before);
        assert_eq!(
            pending.record_repository_publication_rejection(rejection.clone()),
            Ok(true),
            "A9"
        );
        assert!(
            pending
                .terminal_cleanup
                .publication_command(&id)
                .unwrap()
                .is_none()
        );
        assert!(
            pending
                .terminal_cleanup
                .command_for(&id, id.as_str())
                .is_some(),
            "A9 root cleanup unblocked"
        );
        assert!(
            pending
                .terminal_cleanup
                .repository_publication_receipt(&id)
                .unwrap()
                .is_none(),
            "A9 no success receipt"
        );
        pending.environment = crate::state::SessionEnvironmentState::Unmaterialized;
        let before = pending.clone();
        assert_eq!(
            pending.record_repository_publication_rejection(rejection),
            Ok(false),
            "A11"
        );
        assert_eq!(pending, before);
        let other = crate::state::SessionRepositoryPublicationRejection::source_closed(
            &command,
            "foreign-source",
            observation,
        )
        .unwrap();
        assert!(
            pending
                .record_repository_publication_rejection(other)
                .is_err(),
            "A12"
        );
        assert_eq!(pending, before);
    }

    #[test]
    fn archive_with_repository_publication_is_one_atomic_aggregate_transition() {
        // Cause/effect graph: C1 disposition admits archive; C2 publication
        // intent is valid and either new/exact replay; C3 no incompatible cleanup
        // fence exists; C4 the asserted input has exactly one equality match in
        // the active manifest. Effects: E1 freeze publication before
        // archive/termination; E2 exact replay is a no-op; E3 any invalid
        // disposition, intent, prior fence, absent input, or corrupt duplicate
        // leaves the complete aggregate unchanged.
        //
        // | Rule | disposition | intent | cleanup | active matches | Effect |
        // | A1 | Active | valid new + typed time | NotRequested | one | E1 |
        // | A2 | Archived | exact replay, no new time | Completed publication | zero | E2 |
        // | A3 | Active | invalid | NotRequested | one | E3 |
        // | A4 | Deleting | valid | any | one | E3 |
        // | A5 | Active | valid | legacy no-publication fence | one | E3 |
        // | A6 | Active | valid | NotRequested | zero | E3 |
        // | A7 | Active/corrupt | valid | NotRequested | duplicate | E3 |
        // | A8 | Active + Unmaterialized | valid | NotRequested | one | E3 |
        // | A13 | Active | valid, no Application time | NotRequested | one | E3 |
        let intent = repository_publication_intent();
        let mut active = session("publish-archive", SessionRevision(1));
        install_live_publication_source(&mut active);
        install_publication_input(&mut active, &intent);
        let before = active.clone();
        assert_eq!(
            active.archive_with_repository_publication("ignored", None, intent.clone()),
            Err(SessionArchiveWithRepositoryPublicationError::Disposition(
                SessionDispositionTransitionError::ArchiveTimeRequired,
            )),
            "A13/E3"
        );
        assert_eq!(active, before, "A13/E3 no partial publication intent");
        assert_eq!(
            active.archive_with_repository_publication(
                "2026-08-28T00:00:00Z",
                Some(100),
                intent.clone()
            ),
            Ok(true),
            "A1/E1"
        );
        assert_eq!(active.execution, SessionExecutionState::Terminated, "A1/E1");
        assert_eq!(active.archived_at(), Some("2026-08-28T00:00:00Z"), "A1/E1");
        assert_eq!(active.archived_at_unix_ms(), Some(100), "A1/E1");
        assert_eq!(
            active.terminal_cleanup.repository_publication_intent(),
            Some(&intent),
            "A1/E1"
        );
        active.freeze_terminal_cleanup_targets([], 0, 0).unwrap();
        let publication = active
            .terminal_cleanup
            .publication_command(&crate::state::SessionId::from("publish-archive"))
            .unwrap()
            .unwrap();
        let publication_receipt = crate::state::SessionRepositoryPublicationReceipt::new(
            &publication,
            awaken_provisioning_contract::RepositoryPublicationReceipt {
                repository_id: "repo-1".into(),
                source_remote_url: "https://example.test/repo.git".into(),
                branch: publication.intent.expectation.branch.clone(),
                commit: publication.intent.expectation.commit.clone(),
            },
        );
        active
            .terminal_cleanup
            .record_repository_publication_receipt(
                &crate::state::SessionId::from("publish-archive"),
                publication_receipt,
            )
            .unwrap();
        let root = active
            .terminal_cleanup
            .command_for(
                &crate::state::SessionId::from("publish-archive"),
                "publish-archive",
            )
            .unwrap();
        let receipt_fingerprint = crate::state::stable_fingerprint(&(
            "session-terminal-cleanup-thread-receipt-v1",
            root.session_id.as_str(),
            root.thread_id.as_str(),
            root.effect_id.as_str(),
            Vec::<(&str, &str)>::new(),
        ));
        let completion = serde_json::json!({
            "session_id": root.session_id,
            "thread_id": root.thread_id,
            "effect_id": root.effect_id,
            "artifact_receipts": [],
            "receipt_fingerprint": receipt_fingerprint,
        });
        let mut aggregate_wire = serde_json::to_value(&active).unwrap();
        aggregate_wire["terminal_cleanup"]["cleanup"]
            .as_object_mut()
            .expect("publication cleanup wire is an object")
            .entry("completions")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("legacy completions wire is an object")
            .insert(root.thread_id.clone(), completion);
        active = serde_json::from_value(aggregate_wire).unwrap();
        active
            .normalize_legacy_terminal_cleanup("released")
            .unwrap();
        assert!(active.resources.active.inputs().is_empty(), "A2 setup");
        let archived = active.clone();
        assert_eq!(
            active.archive_with_repository_publication(
                "2026-08-29T00:00:00Z",
                None,
                intent.clone()
            ),
            Ok(false),
            "A2/E2"
        );
        assert_eq!(active, archived, "A2/E2");

        let mut foreign_completed = active.clone();
        foreign_completed.session_id = "foreign-publish-archive".into();
        let foreign_before = foreign_completed.clone();
        assert_eq!(
            foreign_completed
                .request_delete("workspace", 0)
                .unwrap_err(),
            crate::state::SessionCleanupError::OperationMismatch,
            "A2 foreign cleanup identity rejects Delete before mutation"
        );
        assert_eq!(foreign_completed, foreign_before, "A2 fail-closed stutter");
        assert!(
            foreign_completed.verified_terminal_cleanup().is_err(),
            "A2 foreign sidecar cannot bind the rewritten outer Session"
        );
        assert!(
            !foreign_completed.admits_tombstone(
                "foreign-publish-archive",
                SessionRevision(foreign_completed.revision.0 + 1),
            ),
            "A2 foreign Completed evidence cannot authorize a tombstone"
        );

        let mut invalid = session("invalid-archive", SessionRevision(1));
        install_live_publication_source(&mut invalid);
        install_publication_input(&mut invalid, &intent);
        let before = invalid.clone();
        let mut invalid_expectation = intent.clone();
        invalid_expectation.expectation.branch.clear();
        assert!(
            matches!(
                invalid.archive_with_repository_publication(
                    "ignored",
                    Some(0),
                    invalid_expectation
                ),
                Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                    crate::state::SessionCleanupError::InvalidRepositoryPublicationIntent(_)
                ))
            ),
            "A3/E3"
        );
        assert_eq!(invalid, before, "A3/E3");

        let mut deleting = session("deleting-archive", SessionRevision(1));
        install_live_publication_source(&mut deleting);
        install_publication_input(&mut deleting, &intent);
        deleting.disposition = SessionDisposition::Deleting;
        let before = deleting.clone();
        assert_eq!(
            deleting.archive_with_repository_publication("ignored", None, intent.clone()),
            Err(SessionArchiveWithRepositoryPublicationError::Disposition(
                SessionDispositionTransitionError::ArchiveAfterDelete,
            )),
            "A4/E3"
        );
        assert_eq!(deleting, before, "A4/E3");

        let mut legacy_fenced = session("legacy-fenced", SessionRevision(1));
        install_live_publication_source(&mut legacy_fenced);
        install_publication_input(&mut legacy_fenced, &intent);
        assert!(legacy_fenced.ensure_terminal_cleanup_fence());
        let before = legacy_fenced.clone();
        assert_eq!(
            legacy_fenced.archive_with_repository_publication("ignored", Some(0), intent.clone()),
            Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                crate::state::SessionCleanupError::FrozenRepositoryPublicationMismatch,
            )),
            "A5/E3"
        );
        assert_eq!(legacy_fenced, before, "A5/E3");

        let mut absent = session("absent-input", SessionRevision(1));
        install_live_publication_source(&mut absent);
        let before = absent.clone();
        assert!(
            matches!(
                absent.archive_with_repository_publication("ignored", Some(0), intent.clone()),
                Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                    crate::state::SessionCleanupError::InvalidRepositoryPublicationIntent(_)
                ))
            ),
            "A6/E3"
        );
        assert_eq!(absent, before, "A6/E3");

        assert!(
            publication_input_is_exactly_active(std::slice::from_ref(&intent.input), &intent.input,),
            "A1 one exact active match"
        );
        assert!(
            !publication_input_is_exactly_active(&[], &intent.input),
            "A6 zero matches"
        );
        assert!(
            !publication_input_is_exactly_active(
                &[intent.input.clone(), intent.input.clone()],
                &intent.input,
            ),
            "A7 duplicate corrupt matches"
        );

        let mut unmaterialized = session("unmaterialized-publication", SessionRevision(1));
        install_publication_input(&mut unmaterialized, &intent);
        let before = unmaterialized.clone();
        assert!(
            matches!(
                unmaterialized.archive_with_repository_publication("ignored", Some(0), intent),
                Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                    crate::state::SessionCleanupError::InvalidRepositoryPublicationIntent(_)
                ))
            ),
            "A8/E3 terminal publication cannot invent a never-materialized worktree"
        );
        assert_eq!(unmaterialized, before, "A8/E3");
    }
}
