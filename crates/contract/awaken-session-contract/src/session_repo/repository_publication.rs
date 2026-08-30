//! Atomic archive transition with one frozen Repository publication intent.

use super::{PersistedSession, SessionDisposition, SessionDispositionTransitionError};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionArchiveWithRepositoryPublicationError {
    #[error(transparent)]
    Disposition(#[from] SessionDispositionTransitionError),
    #[error(transparent)]
    Cleanup(#[from] crate::SessionCleanupError),
}

#[must_use]
fn publication_input_is_exactly_active(
    active_inputs: &[crate::ResolvedInput],
    asserted: &crate::ResolvedInput,
) -> bool {
    active_inputs
        .iter()
        .filter(|input| *input == asserted)
        .count()
        == 1
}

impl PersistedSession {
    /// Return terminal cleanup authority only after binding any Repository
    /// publication sidecar to this aggregate's outer Session identity.
    pub fn verified_terminal_cleanup(
        &self,
    ) -> Result<&crate::SessionCleanupOperation, crate::SessionCleanupError> {
        self.terminal_cleanup.verify_for(&self.session_id)?;
        Ok(&self.terminal_cleanup)
    }

    pub(super) fn has_verified_completed_cleanup(&self) -> bool {
        self.verified_terminal_cleanup()
            .is_ok_and(crate::SessionCleanupOperation::is_completed)
    }

    pub(super) fn verified_cleanup_needs_reconciliation(&self) -> bool {
        self.verified_terminal_cleanup()
            .map_or(true, crate::SessionCleanupOperation::needs_reconciliation)
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
        intent: crate::SessionRepositoryPublicationIntent,
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
        if !publication_input_is_exactly_active(self.resources.active.inputs(), &intent.input) {
            return Err(
                crate::SessionCleanupError::InvalidRepositoryPublicationIntent(
                    "publication input does not match exactly one active Session input".into(),
                )
                .into(),
            );
        }
        self.terminal_cleanup
            .request_with_publication(&self.session_id, intent)?;
        self.archive(archived_at).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_repo::mutation_tests::session;
    use crate::{SessionExecutionState, SessionRevision};

    fn repository_publication_intent() -> crate::SessionRepositoryPublicationIntent {
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
        intent: &crate::SessionRepositoryPublicationIntent,
    ) {
        session.resources = crate::SessionResourceState::from_active(
            crate::ResolvedSessionResources::try_new(vec![intent.input.clone()], Vec::new())
                .unwrap(),
        );
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
        // | A1 | Active | valid new | NotRequested | one | E1 |
        // | A2 | Archived | exact replay | Completed publication | zero | E2 |
        // | A3 | Active | invalid | NotRequested | one | E3 |
        // | A4 | Deleting | valid | any | one | E3 |
        // | A5 | Active | valid | legacy no-publication fence | one | E3 |
        // | A6 | Active | valid | NotRequested | zero | E3 |
        // | A7 | Active/corrupt | valid | NotRequested | duplicate | E3 |
        let intent = repository_publication_intent();
        let mut active = session("publish-archive", SessionRevision(1));
        install_publication_input(&mut active, &intent);
        assert_eq!(
            active.archive_with_repository_publication("2026-08-28T00:00:00Z", intent.clone()),
            Ok(true),
            "A1/E1"
        );
        assert_eq!(active.execution, SessionExecutionState::Terminated, "A1/E1");
        assert_eq!(active.archived_at(), Some("2026-08-28T00:00:00Z"), "A1/E1");
        assert_eq!(
            active.terminal_cleanup.repository_publication_intent(),
            Some(&intent),
            "A1/E1"
        );
        active.freeze_terminal_cleanup_targets([], 0, 0).unwrap();
        let publication = active
            .terminal_cleanup
            .publication_command("publish-archive")
            .unwrap()
            .unwrap();
        let publication_receipt = crate::SessionRepositoryPublicationReceipt::new(
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
            .record_repository_publication_receipt("publish-archive", publication_receipt)
            .unwrap();
        let root = active
            .terminal_cleanup
            .command_for("publish-archive", "publish-archive")
            .unwrap();
        let root_receipt = crate::SessionCleanupCompletion::new(&root, Vec::new())
            .verify(&root)
            .unwrap();
        active
            .complete_terminal_cleanup(&[root_receipt], "released")
            .unwrap();
        assert!(active.resources.active.inputs().is_empty(), "A2 setup");
        let archived = active.clone();
        assert_eq!(
            active.archive_with_repository_publication("2026-08-29T00:00:00Z", intent.clone()),
            Ok(false),
            "A2/E2"
        );
        assert_eq!(active, archived, "A2/E2");

        let mut foreign_completed = active.clone();
        foreign_completed.session_id = "foreign-publish-archive".into();
        assert!(
            foreign_completed.request_delete(),
            "A2 foreign setup hidden"
        );
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
        install_publication_input(&mut invalid, &intent);
        let before = invalid.clone();
        let mut invalid_expectation = intent.clone();
        invalid_expectation.expectation.branch.clear();
        assert!(
            matches!(
                invalid.archive_with_repository_publication("ignored", invalid_expectation),
                Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                    crate::SessionCleanupError::InvalidRepositoryPublicationIntent(_)
                ))
            ),
            "A3/E3"
        );
        assert_eq!(invalid, before, "A3/E3");

        let mut deleting = session("deleting-archive", SessionRevision(1));
        install_publication_input(&mut deleting, &intent);
        deleting.disposition = SessionDisposition::Deleting;
        let before = deleting.clone();
        assert_eq!(
            deleting.archive_with_repository_publication("ignored", intent.clone()),
            Err(SessionArchiveWithRepositoryPublicationError::Disposition(
                SessionDispositionTransitionError::ArchiveAfterDelete,
            )),
            "A4/E3"
        );
        assert_eq!(deleting, before, "A4/E3");

        let mut legacy_fenced = session("legacy-fenced", SessionRevision(1));
        install_publication_input(&mut legacy_fenced, &intent);
        assert!(legacy_fenced.ensure_terminal_cleanup_fence());
        let before = legacy_fenced.clone();
        assert_eq!(
            legacy_fenced.archive_with_repository_publication("ignored", intent.clone()),
            Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                crate::SessionCleanupError::FrozenRepositoryPublicationMismatch,
            )),
            "A5/E3"
        );
        assert_eq!(legacy_fenced, before, "A5/E3");

        let mut absent = session("absent-input", SessionRevision(1));
        let before = absent.clone();
        assert!(
            matches!(
                absent.archive_with_repository_publication("ignored", intent.clone()),
                Err(SessionArchiveWithRepositoryPublicationError::Cleanup(
                    crate::SessionCleanupError::InvalidRepositoryPublicationIntent(_)
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
    }
}
