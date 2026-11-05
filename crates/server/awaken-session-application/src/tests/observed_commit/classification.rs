//! Cause/effect matrix at the real application read/CAS and HTTP seams.
use super::ObservedCommitPort;
use crate::SessionMutationError;
use crate::{SessionMutations, SessionRootReads};
use awaken_session_contract::SessionRepositoryReads;
use awaken_session_contract::{
    IdempotencyRecord, PersistedSession, SessionMutation, SessionMutationPayload,
    SessionMutationResult, SessionRepositoryConflict, SessionRepositoryError,
};
use awaken_session_types::RunErrorKind;
use awaken_session_types::{SessionId, SessionRevision};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct OwnerFailure(SessionRepositoryError);

#[async_trait::async_trait]
impl SessionRepositoryReads for OwnerFailure {
    async fn workspace_content_is_public(
        &self,
        _: &str,
        _: &SessionId,
    ) -> Result<bool, SessionRepositoryError> {
        Err(self.0.clone())
    }

    async fn owner(&self, _: &SessionId) -> Result<String, SessionRepositoryError> {
        Err(self.0.clone())
    }
    async fn get(&self, _: &SessionId) -> Result<PersistedSession, SessionRepositoryError> {
        panic!("failed ownership must not read Session content")
    }
    async fn scan(
        &self,
        _: awaken_session_contract::SessionScanQuery<'_>,
    ) -> Result<awaken_session_contract::SessionScan, SessionRepositoryError> {
        panic!("failed ownership must not scan Sessions")
    }
    async fn deleted_event_admissions(
        &self,
        _: &str,
        _: &SessionId,
    ) -> Result<
        Option<awaken_session_contract::DeletedSessionEventAdmissions>,
        SessionRepositoryError,
    > {
        panic!("failed ownership must not read admissions")
    }
}

#[tokio::test]
async fn repository_error_classes_survive_application_reads_and_cas() {
    // Independent oracle: missing -> NotFound; identity/CAS conflicts -> their
    // conflict variants; only authority outage -> Unavailable; damaged rows
    // and invalid commands -> Internal. A detail string deliberately names a
    // different class: classifying by message or flattening all failures fails.
    let detail = "offline corrupt conflict not found";
    for (cause, expected) in [
        (
            SessionRepositoryError::NotFound,
            SessionMutationError::NotFound,
        ),
        (
            SessionRepositoryError::Conflict(SessionRepositoryConflict::AlreadyExists),
            SessionMutationError::Conflict,
        ),
        (
            SessionRepositoryError::Conflict(SessionRepositoryConflict::Tombstoned),
            SessionMutationError::Conflict,
        ),
        (
            SessionRepositoryError::Conflict(SessionRepositoryConflict::IdempotencyMismatch),
            SessionMutationError::IdempotencyMismatch,
        ),
        (
            SessionRepositoryError::AuthorityUnavailable(detail.into()),
            SessionMutationError::Unavailable(detail.into()),
        ),
        (
            SessionRepositoryError::Corrupt(detail.into()),
            SessionMutationError::Internal(detail.into()),
        ),
        (
            SessionRepositoryError::InvalidMutation(detail.into()),
            SessionMutationError::Internal(detail.into()),
        ),
    ] {
        let id = SessionId::from("classified");
        assert_eq!(
            crate::mutation::session_owner(&OwnerFailure(cause.clone()), &id).await,
            Err(expected.clone()),
            "read: {cause:?}"
        );
        let executor = awaken_session_execution_contract::SessionExecutor::colocated(
            Arc::new(super::super::NoopRuntime),
            Arc::new(super::super::NoopMcpRealizer),
        );
        let queries =
            crate::SessionQueries::new(Arc::new(OwnerFailure(cause.clone())), &executor, None);
        let denied = queries.authorize("workspace", &id).await.unwrap_err();
        match &expected {
            SessionMutationError::NotFound => {
                assert_eq!(denied, crate::SessionAccessError::NotFound)
            }
            SessionMutationError::Internal(message) => {
                assert_eq!(denied, crate::SessionAccessError::Internal(message.clone()))
            }
            _ => assert!(matches!(denied, crate::SessionAccessError::Unavailable(_))),
        }
        let snapshot = super::super::persisted(id.as_str(), false, "idle");
        let mutation = SessionMutation {
            expected_revision: snapshot.revision,
            idempotency: IdempotencyRecord {
                key: "classification".into(),
                payload_hash: "unchanged".into(),
            },
            payload: SessionMutationPayload::Replace(snapshot),
            lifecycle_facts: Vec::new(),
        };
        let port = Arc::new(ObservedCommitPort {
            expected: mutation.clone(),
            pending_once: false,
            calls: AtomicUsize::new(0),
            result: Err(cause.clone()),
        });
        let mutations = SessionMutations::new(port.clone(), awaken_time::system_clock());
        assert_eq!(
            mutations.commit_mutation("workspace", mutation).await,
            Err(expected),
            "CAS: {cause:?}"
        );
        assert_eq!(
            port.calls.load(Ordering::SeqCst),
            1,
            "no hidden retry or success"
        );
    }
}

#[tokio::test]
async fn invalid_applied_revision_is_permanent_and_never_reloaded() {
    // C1 correct next revision, C2 same revision, C3 skipped revision.
    // E1 only C1 succeeds; E2 C2/C3 are the aggregate's typed revision
    // refusal (the permanent Internal class), no repair read/second CAS.
    for (revision, valid) in [(1, true), (0, false), (2, false)] {
        let mut aggregate = crate::mutation::aggregate_from_snapshot(super::super::persisted(
            "revision", false, "idle",
        ))
        .unwrap();
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
        let record = IdempotencyRecord {
            key: "revision".into(),
            payload_hash: "unchanged".into(),
        };
        let expected = aggregate.compile_mutation("workspace", record.clone(), Vec::new(), 100);
        let port = Arc::new(ObservedCommitPort {
            expected,
            pending_once: false,
            calls: AtomicUsize::new(0),
            result: Ok(SessionMutationResult::Applied {
                new_revision: SessionRevision(revision),
            }),
        });
        let mutations = SessionMutations::new(port.clone(), clock);
        let result = mutations
            .commit_session_snapshot_with_record("workspace", aggregate, record, Vec::new())
            .await;
        if valid {
            assert_eq!(result.unwrap().committed_revision(), SessionRevision(1));
        } else {
            assert!(
                matches!(
                    result,
                    Err(SessionMutationError::Invalid(
                        crate::SessionStateViolation::Aggregate(
                            awaken_session_contract::SessionMutationValidationError::ReplacementRevisionMismatch
                        )
                    ))
                ),
                "revision={revision}"
            );
            if let Err(error) = result {
                let error: awaken_session_types::RunError = error.into();
                assert_eq!(error.kind, RunErrorKind::Internal, "revision={revision}");
                assert!(!error.is_unclassified_internal(), "revision={revision}");
            }
        }
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn permanent_mutation_failures_do_not_become_retryable_domain_errors() {
    // A paired control prevents making every error permanent. The same detail
    // must preserve its cause through activity, preparation, manifest, archive,
    // access/recovery and realization-control adapters.
    assert_eq!(
        crate::SessionActivityError::mutation(SessionMutationError::IdempotencyMismatch)
            .run_error()
            .kind,
        RunErrorKind::Internal
    );
    assert_eq!(
        crate::run_admission::preparation_error(crate::credentials::mutation_failure(
            SessionMutationError::IdempotencyMismatch
        ))
        .kind,
        RunErrorKind::Internal
    );
    for (cause, kind) in [
        (
            SessionMutationError::Internal("same detail".into()),
            RunErrorKind::Internal,
        ),
        (
            SessionMutationError::Invalid(crate::SessionStateViolation::Aggregate(
                awaken_session_contract::SessionMutationValidationError::ReplacementRevisionMismatch,
            )),
            RunErrorKind::Internal,
        ),
        (
            SessionMutationError::Invalid(crate::SessionStateViolation::Budget(
                awaken_session_contract::ManagedListPriceError::MissingModel("same detail".into()),
            )),
            RunErrorKind::Internal,
        ),
        (
            SessionMutationError::Unavailable("same detail".into()),
            RunErrorKind::Unavailable,
        ),
    ] {
        let activity = crate::SessionActivityError::mutation(cause.clone()).run_error();
        assert_eq!(activity.kind, kind);
        assert!(!activity.is_unclassified_internal(), "known activity cause");
        let preparation = crate::credentials::mutation_failure(cause.clone());
        let error = crate::run_admission::preparation_error(preparation);
        assert_eq!(error.kind, kind);
        let manifest = crate::SessionResourceManifestError::mutation(cause.clone());
        let archive = crate::SessionArchiveWithRepositoryPublicationError::from(cause.clone());
        let control = crate::realization::mutation_control(cause);
        let disposition = control.disposition();
        if kind == RunErrorKind::Internal {
            assert!(matches!(
                control,
                awaken_session_execution_contract::SessionRealizationControlFailure::Internal(_)
            ));
            assert!(
                !error.is_unclassified_internal(),
                "known permanent failure must not enter the unknown-effect retry budget"
            );
            assert!(
                matches!(manifest, crate::SessionResourceManifestError::Rejected(error) if error.kind == kind && !error.is_unclassified_internal())
            );
            assert!(
                matches!(archive, crate::SessionArchiveWithRepositoryPublicationError::Rejected(error) if error.kind == kind && !error.is_unclassified_internal())
            );
            assert_eq!(
                disposition,
                awaken_session_execution_contract::SessionRealizationControlDisposition::Terminal
            );
            let error: awaken_session_types::RunError =
                crate::SessionAccessError::Internal("same detail".into()).into();
            assert_eq!(error.kind, kind);
            assert!(!error.is_unclassified_internal(), "known access cause");
            assert!(
                matches!(crate::SessionProjectionRecoveryError::from(crate::SessionAccessError::Internal("same detail".into())), crate::SessionProjectionRecoveryError::Rejected(error) if error.kind == kind && !error.is_unclassified_internal())
            );
        } else {
            assert!(matches!(
                manifest,
                crate::SessionResourceManifestError::Unavailable(_)
            ));
            assert!(matches!(
                archive,
                crate::SessionArchiveWithRepositoryPublicationError::Unavailable(_)
            ));
            assert_eq!(disposition, awaken_session_execution_contract::SessionRealizationControlDisposition::Retryable);
        }
    }
}

#[test]
fn mutation_run_errors_preserve_cause_and_permanence() {
    use awaken_session_types::RunError;
    // Every input class has an independent kind oracle. Unknown internal errors
    // are a positive control: known corruption must not share their retry code.
    assert!(RunError::internal("same detail").is_unclassified_internal());
    let exhausted = crate::SessionActivityError::EpochExhausted.run_error();
    assert_eq!(exhausted.kind, RunErrorKind::Internal);
    assert!(
        !exhausted.is_unclassified_internal(),
        "an exhausted durable epoch cannot recover by retry"
    );
    for (cause, kind) in [
        (SessionMutationError::NotFound, RunErrorKind::BadRequest),
        (SessionMutationError::Conflict, RunErrorKind::Conflict),
        (
            SessionMutationError::IdempotencyMismatch,
            RunErrorKind::BadRequest,
        ),
        (
            SessionMutationError::Unavailable("same detail".into()),
            RunErrorKind::Unavailable,
        ),
        (
            SessionMutationError::Internal("same detail".into()),
            RunErrorKind::Internal,
        ),
    ] {
        let error: RunError = cause.into();
        assert_eq!(error.kind, kind);
        assert!(!error.is_unclassified_internal());
        if kind == RunErrorKind::Internal {
            assert_eq!(error.code, "session_application_invalid");
            assert_eq!(error.message, "same detail");
        }
    }
}

struct UnreachedInbox;

#[async_trait::async_trait]
impl awaken_session_execution_contract::LiveInboxAccess for UnreachedInbox {
    async fn live_inbox_snapshot(
        &self,
        _: &str,
    ) -> Result<
        awaken_session_application_contract::LiveInboxSnapshot,
        awaken_session_application_contract::LiveInboxError,
    > {
        panic!("failed authority must not read the attempt queue")
    }
    async fn live_inbox_queue(
        &self,
        _: &str,
        _: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<u64, awaken_session_application_contract::LiveInboxError> {
        panic!("failed authority must not queue content")
    }
    async fn live_inbox_remove(
        &self,
        _: &str,
        _: u64,
    ) -> Result<(), awaken_session_application_contract::LiveInboxError> {
        panic!("failed authority must not remove content")
    }
    async fn live_inbox_replace(
        &self,
        _: &str,
        _: u64,
        _: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<(), awaken_session_application_contract::LiveInboxError> {
        panic!("failed authority must not replace content")
    }
    async fn live_inbox_reorder(
        &self,
        _: &str,
        _: Vec<u64>,
    ) -> Result<(), awaken_session_application_contract::LiveInboxError> {
        panic!("failed authority must not reorder content")
    }
}

#[tokio::test]
async fn every_live_inbox_operation_preserves_authority_failure_before_runtime_effects() {
    use awaken_session_application_contract::{
        LiveInboxApplication, LiveInboxApplicationError as E,
    };
    // C1 absent => NotFound; C2 outage => retryable authority error;
    // C3/C4 damaged durable state => permanent Internal. All five public
    // operations must fail before a read or write on the attempt-local inbox.
    for (cause, expected) in [
        (SessionRepositoryError::NotFound, "missing"),
        (
            SessionRepositoryError::AuthorityUnavailable("same detail".into()),
            "unavailable",
        ),
        (
            SessionRepositoryError::Corrupt("same detail".into()),
            "internal",
        ),
        (
            SessionRepositoryError::InvalidMutation("same detail".into()),
            "internal",
        ),
    ] {
        let inputs = crate::SessionInputs::new(
            Arc::new(OwnerFailure(cause)),
            Arc::new(UnreachedInbox),
            None,
        );
        let id = SessionId::from("inbox");
        for error in [
            inputs.snapshot("workspace", &id).await.unwrap_err(),
            inputs
                .queue("workspace", &id, Vec::new())
                .await
                .unwrap_err(),
            inputs.remove("workspace", &id, 1).await.unwrap_err(),
            inputs
                .replace("workspace", &id, 1, Vec::new())
                .await
                .unwrap_err(),
            inputs
                .reorder("workspace", &id, Vec::new())
                .await
                .unwrap_err(),
        ] {
            match (expected, error) {
                ("missing", E::NotFound) => {}
                ("unavailable", E::AuthorityUnavailable { detail }) => assert_eq!(
                    detail,
                    "Session mutation repository is unavailable: same detail"
                ),
                ("internal", E::InvalidSessionState { detail }) => {
                    assert_eq!(detail, "same detail")
                }
                (expected, error) => panic!("expected {expected}, got {error:?}"),
            }
        }
    }
}

#[test]
fn mutation_control_conversion_preserves_each_authority_failure_class() {
    use awaken_session_execution_contract::SessionRealizationControlFailure as Control;
    use serde_json::json;
    // Closed table of every mutation cause at the existing control interface:
    // missing/conflict preserve identity; outage preserves its detail; invalid
    // durable state and a mismatched internal receipt remain permanent faults.
    for (mutation, expected) in [
        (SessionMutationError::NotFound, json!({"kind":"not_found"})),
        (SessionMutationError::Conflict, json!({"kind":"conflict"})),
        (
            SessionMutationError::Unavailable("temporary".into()),
            json!({"kind":"unavailable","detail":"temporary"}),
        ),
        (
            SessionMutationError::Internal("broken root".into()),
            json!({"kind":"internal","detail":"broken root"}),
        ),
        (
            SessionMutationError::IdempotencyMismatch,
            json!({"kind":"internal","detail":"Session idempotency key was reused with another payload"}),
        ),
    ] {
        let actual: Control = mutation.into();
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
}

#[test]
fn a_list_price_outage_is_the_one_retryable_state_violation() {
    use awaken_session_contract::{ManagedListPriceError, SessionMutationValidationError};
    // Cause: the Session state violation. Effect: its one classification at
    // the mutation boundary, kept by creation and activity.
    // | Rule | Violation | Mutation | RunError |
    // | V1 | Budget(Unavailable) | Unavailable | Unavailable (retryable) |
    // | V2 | Budget(MissingModel) | Invalid | Internal |
    // | V3 | Budget(InvalidSnapshot) | Invalid | Internal |
    // | V4 | Aggregate(..) | Invalid | Internal |
    for (rule, violation, retryable) in [
        (
            "V1",
            crate::SessionStateViolation::Budget(ManagedListPriceError::Unavailable(
                "price authority down".into(),
            )),
            true,
        ),
        (
            "V2",
            crate::SessionStateViolation::Budget(ManagedListPriceError::MissingModel(
                "model".into(),
            )),
            false,
        ),
        (
            "V3",
            crate::SessionStateViolation::Budget(ManagedListPriceError::InvalidSnapshot(
                "list cost overflow".into(),
            )),
            false,
        ),
        (
            "V4",
            crate::SessionStateViolation::Aggregate(
                SessionMutationValidationError::ReplacementRevisionMismatch,
            ),
            false,
        ),
    ] {
        let mutation = SessionMutationError::from(violation.clone());
        assert_eq!(
            matches!(mutation, SessionMutationError::Unavailable(_)),
            retryable,
            "{rule} mutation class"
        );
        assert_eq!(
            matches!(mutation, SessionMutationError::Invalid(_)),
            !retryable,
            "{rule} permanent violations keep their typed source"
        );
        let expected = if retryable {
            RunErrorKind::Unavailable
        } else {
            RunErrorKind::Internal
        };
        assert_eq!(
            awaken_session_types::RunError::from(mutation).kind,
            expected,
            "{rule} run error"
        );
        assert_eq!(
            crate::SessionActivityError::mutation(violation.clone().into())
                .run_error()
                .kind,
            expected,
            "{rule} activity keeps the class"
        );
        assert_eq!(
            matches!(
                crate::SessionCreationError::from(violation),
                crate::SessionCreationError::Unavailable(_)
            ),
            retryable,
            "{rule} creation keeps the class"
        );
    }
}
