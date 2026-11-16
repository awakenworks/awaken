//! Root/Dispatch SQLite conformance for the claimed preparation boundary.
//! Physical MCP receipts are an explicit fixture; the phase reducer and both
//! persistence owners remain real. This is not deployment acceptance evidence.

use super::*;
use awaken_credential_contract::ManagedCredentialRolloutTarget as _;
use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchCommitAuthority as _, DispatchEnqueue as _, DispatchExecution as _,
};
use awaken_session_execution_contract::{
    ClaimedSessionCredentialPreparationContext, SessionRealizationControl as _,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct ExpiringSource;

#[async_trait::async_trait]
impl awaken_credential_contract::SessionCredentialSource for ExpiringSource {
    async fn has_vault(&self, workspace: &str, id: &str) -> Result<bool, String> {
        RolloutCredentialSource.has_vault(workspace, id).await
    }
    async fn mcp_credential_source_for_url(
        &self,
        workspace: &str,
        vaults: &[String],
        url: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialSourceId>, String> {
        RolloutCredentialSource
            .mcp_credential_source_for_url(workspace, vaults, url)
            .await
    }
    async fn mcp_access_for_source(
        &self,
        id: &awaken_credential_contract::CredentialSourceId,
        workspace: &str,
        target: &awaken_agent_contract::McpTarget,
        holder: &awaken_agent_contract::credential::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, String> {
        Ok(RolloutCredentialSource
            .mcp_access_for_source(id, workspace, target, holder, binding)
            .await?
            .with_expiry(40_000))
    }
    async fn credential_access_for_source(
        &self,
        id: &awaken_credential_contract::CredentialSourceId,
        workspace: &str,
        request: awaken_credential_contract::SessionCredentialAccessRequest,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, String> {
        RolloutCredentialSource
            .credential_access_for_source(id, workspace, request)
            .await
    }
}

/// Owns a real SQLite exact-row exclusion. No bool or timer impersonates a
/// stopped predecessor; the original locked physical slot is checked each time.
struct SqlitePreparation<'a> {
    commit: &'a dyn awaken_session_execution_contract::SessionRootCommit,
    guard: Mutex<awaken_dispatch_contract::CommitEpochGuard>,
    request: awaken_dispatch_contract::RunDispatch,
    claim: awaken_worker_contract::RunClaim,
    work: awaken_session_application_contract::SessionWorkAuthorityReceipt,
}

#[async_trait::async_trait]
impl ClaimedSessionCredentialPreparationContext for SqlitePreparation<'_> {
    fn root_commit<'a>(
        &'a self,
        _original: &'a dyn awaken_session_execution_contract::SessionRootCommit,
    ) -> &'a dyn awaken_session_execution_contract::SessionRootCommit {
        self.commit
    }

    async fn validate_root_revision(
        &self,
        id: &SessionId,
        revision: awaken_session_types::SessionRevision,
    ) -> Result<bool, awaken_session_execution_contract::SessionMutationError> {
        self.commit.validate(id, revision).await
    }
    fn claim(&self) -> &awaken_worker_contract::RunClaim {
        &self.claim
    }
    fn session_id(&self) -> &str {
        &self.request.session_thread_id().0
    }
    fn thread_id(&self) -> &awaken_agent_contract::ThreadId {
        self.request.thread_id()
    }
    fn activation(&self) -> &awaken_runtime_contract::activation::RunActivation {
        &self.request.activation
    }
    fn execution_scope(&self) -> Option<&awaken_agent_contract::ExecutionScopeRef> {
        self.request.execution_scope.as_ref()
    }
    fn session_work_authority(
        &self,
    ) -> &awaken_session_application_contract::SessionWorkAuthorityReceipt {
        &self.work
    }
    fn verify_at(
        &self,
        now: u64,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        use awaken_session_execution_contract::SessionRealizationControlFailure as Failure;
        let guard = self.guard.lock().unwrap();
        assert_eq!(guard.request(), &self.request);
        if !guard.is_live_at(now) || guard.cancellation_requested() {
            return Err(Failure::StaleOwnership);
        }
        if !guard.permits_settlement() {
            return Err(Failure::NotReady);
        }
        Ok(())
    }
}

/// Records the original Root boundary while the real SQLite Dispatch guard is held.
struct RecordingRootCommit<'a> {
    original: &'a dyn awaken_session_execution_contract::SessionRootCommit,
    reject: AtomicBool,
    runtime_failure: Mutex<Option<awaken_runtime_contract::runs::RunErrorKind>>,
    commits: AtomicUsize,
    validations: AtomicUsize,
}

impl RecordingRootCommit<'_> {
    fn refuse(&self) -> Result<(), awaken_session_execution_contract::SessionMutationError> {
        if self.reject.load(Ordering::SeqCst) {
            return Err(
                awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership
                    .into(),
            );
        }
        match *self.runtime_failure.lock().unwrap() {
            Some(kind) => Err(
                awaken_session_execution_contract::SessionMutationError::admission(
                    awaken_runtime_contract::runs::RunError {
                        kind,
                        code: "fixture_root_admission".into(),
                        message: "original Root admission fixture refusal".into(),
                    },
                ),
            ),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRootCommit for RecordingRootCommit<'_> {
    async fn validate_claim(
        &self,
    ) -> Result<(), awaken_session_execution_contract::SessionMutationError> {
        self.original.validate_claim().await
    }

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: awaken_session_contract::SessionMutation,
    ) -> Result<
        awaken_session_contract::SessionMutationResult,
        awaken_session_execution_contract::SessionMutationError,
    > {
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.refuse()?;
        self.original.commit(owner_scope, mutation).await
    }

    async fn validate(
        &self,
        session_id: &SessionId,
        revision: awaken_session_types::SessionRevision,
    ) -> Result<bool, awaken_session_execution_contract::SessionMutationError> {
        self.validations.fetch_add(1, Ordering::SeqCst);
        self.refuse()?;
        self.original.validate(session_id, revision).await
    }
}

#[tokio::test]
async fn claimed_busy_credential_rotation_reaches_exact_ack_and_blocks_peer_activity() {
    use awaken_session_configuration_contract::mcp::McpAttachmentState;
    use awaken_session_execution_contract::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        SessionRealizationAction,
    };
    // Root and repair share the same parent Session but retain different raw
    // Run/Thread identities. Expired v1 must advance through the same phase
    // owner, never by blessing a Requested pin as adoption.
    for (case, repair) in [
        ("root", false),
        ("repair", true),
        ("failed_replay", false),
        ("peer", false),
        ("unknown", false),
        ("foreign_run", false),
        ("physical", false),
        ("predecessor", false),
        ("expired", false),
        ("cas_denied", false),
        ("runtime_unavailable", false),
        ("runtime_invalid", false),
        ("runtime_conflict", false),
    ] {
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
        let repo: Arc<dyn awaken_session_application::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let session_id = SessionId::from("preparing");
        let mut activation = awaken_dispatch_testkit::worker_http::activation("claimed-credential");
        activation.run_id = awaken_agent_contract::RunId("current-run".into());
        activation.thread_id = awaken_agent_contract::ThreadId(
            if repair { "repair-thread" } else { "preparing" }.into(),
        );
        let mut dto = awaken_protocol_managed_testkit::preparing_session_with_mcp_source(false);
        dto.mcp.attachments[0]
            .credential
            .as_mut()
            .unwrap()
            .expires_at_unix_ms = Some(900);
        let mut root = awaken_session_contract::SessionAggregate::try_from(dto).unwrap();
        root.assignment()
            .begin(
                awaken_session_contract::SessionRealizationBeginRequest::new(
                    "workspace-a",
                    "worker",
                    "worker/boot",
                    100_000,
                    false,
                    None,
                ),
                1_000,
            )
            .unwrap();
        let old = root.mcp.attachments[0].clone();
        let realization_id = old.realization.as_ref().unwrap().realization_id.clone();
        root.mcp()
            .activate(&old.generation, &realization_id)
            .unwrap();
        root.mcp()
            .acknowledge_publication(&old.generation, &realization_id)
            .unwrap();
        root.activity().realization_ready(1_000).unwrap();
        assert_eq!(
            root.activity().open(
                None,
                false,
                false,
                1_000,
                0,
                Some((
                    "current-operation",
                    &activation.thread_id,
                    &activation.run_id
                ))
            ),
            awaken_session_contract::SessionActivityAdmissionPlan::Open(1)
        );
        if case == "peer" {
            assert_eq!(
                root.activity().open(
                    None,
                    false,
                    false,
                    1_000,
                    0,
                    Some((
                        "peer",
                        &awaken_agent_contract::ThreadId("peer".into()),
                        &awaken_agent_contract::RunId("peer".into())
                    ))
                ),
                awaken_session_contract::SessionActivityAdmissionPlan::Open(2)
            );
        }
        let mut dto = root.into_snapshot();
        if case == "unknown" {
            let mut wire = serde_json::to_value(dto).unwrap();
            wire.as_object_mut().unwrap().remove("run_activities");
            dto = serde_json::from_value(wire).unwrap();
        }
        create_session_fixture(repo.as_ref(), "workspace-a", dto).await;
        let runtime = Arc::new(RehydrateFake::default());
        let managed = Arc::new(
            awaken_protocol_managed_testkit::managed_state_from_parts(
                runtime.clone(),
                Some(runtime),
                repo.clone(),
                awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
                clock.clone(),
                Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .with_credential_source(Arc::new(ExpiringSource)),
        );
        let app = managed.session_application();
        let target = SessionCredentialRolloutAdoption::over_managed(&app, &managed);
        let event = rollout_event(
            "claimed-current",
            2,
            awaken_credential_contract::ManagedCredentialOperation::Update,
        );
        let before = repo.get(&session_id).await.unwrap();
        assert_eq!(
            target.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
        );
        assert_eq!(
            repo.get(&session_id).await.unwrap(),
            before,
            "ordinary busy rule is retained"
        );
        let queue = awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .unwrap();
        let mut request = awaken_dispatch_contract::RunDispatch::new(activation.clone());
        if case == "foreign_run" {
            request.activation.run_id = awaken_agent_contract::RunId("another-run".into());
        }
        if repair {
            request.session_thread_id =
                Some(awaken_agent_contract::ThreadId(session_id.to_string()));
        }
        queue.enqueue(request).await.unwrap();
        let mut claimed = queue
            .claim("worker/boot", 10_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        if case == "physical" || case == "predecessor" {
            let first = awaken_worker_contract::RunClaim::from(&claimed.lease);
            assert_eq!(
                queue.begin_attempt(&first, None).await.unwrap(),
                awaken_dispatch_contract::AttemptAdmission::Applied
            );
            if case == "predecessor" {
                clock.set(11_000);
                claimed = queue
                    .claim("replacement/boot", 10_000, &Default::default())
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
        if case == "expired" {
            clock.set(11_000);
        }
        let claim = awaken_worker_contract::RunClaim::from(&claimed.lease);
        let work = awaken_session_application_contract::acquire_session_work_receipt(
            app.work_assignments().as_ref(),
            &session_id,
            &claim.owner,
            awaken_session_application_contract::SessionWorkAcquisition::RealizationRenewal,
        )
        .await
        .unwrap()
        .unwrap();
        let gate = RecordingRootCommit {
            original: app.mutations(),
            reject: AtomicBool::new(case == "cas_denied"),
            runtime_failure: Mutex::new(match case {
                "runtime_unavailable" => {
                    Some(awaken_runtime_contract::runs::RunErrorKind::Unavailable)
                }
                "runtime_invalid" => Some(awaken_runtime_contract::runs::RunErrorKind::BadRequest),
                "runtime_conflict" => Some(awaken_runtime_contract::runs::RunErrorKind::Conflict),
                _ => None,
            }),
            commits: AtomicUsize::new(0),
            validations: AtomicUsize::new(0),
        };
        let context = SqlitePreparation {
            commit: &gate,
            request: claimed.request,
            guard: Mutex::new(queue.lock_commit_epoch(&claim).await.unwrap().unwrap()),
            claim,
            work,
        };
        let scoped = target.for_claimed_preparation(&context);
        assert_eq!(
            scoped.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
        );
        if !matches!(case, "root" | "repair" | "failed_replay") {
            assert_eq!(
                repo.get(&session_id).await.unwrap(),
                before,
                "{case}: no desired update without native authority"
            );
            assert_eq!(
                scoped.outcome(),
                Some(Err(
                    if matches!(case, "expired" | "cas_denied" | "runtime_unavailable") {
                        crate::ClaimedCredentialPreparationFailure::Unavailable
                    } else if matches!(
                        case,
                        "unknown" | "foreign_run" | "runtime_invalid" | "runtime_conflict"
                    ) {
                        crate::ClaimedCredentialPreparationFailure::Invalid
                    } else {
                        crate::ClaimedCredentialPreparationFailure::NotReady
                    }
                )),
                "{case}: typed failure survives Vault Pending"
            );
            if matches!(
                case,
                "cas_denied" | "runtime_unavailable" | "runtime_invalid" | "runtime_conflict"
            ) {
                assert_eq!(
                    gate.commits.load(Ordering::SeqCst),
                    1,
                    "typed admission refusal is not retried as a CAS conflict"
                );
            }
            context.guard.into_inner().unwrap().release().await.unwrap();
            continue;
        }
        assert_eq!(
            scoped.outcome(),
            Some(Ok(
                crate::ClaimedCredentialPreparationProgress::DesiredCommitted
            ))
        );
        let pending = repo.get(&session_id).await.unwrap();
        let first_commit_count = gate.commits.load(Ordering::SeqCst);
        assert!(
            first_commit_count > 0,
            "desired update uses selected final Root CAS"
        );
        assert!(
            gate.validations.load(Ordering::SeqCst) > 0,
            "result validates current Root revision"
        );
        gate.reject.store(true, Ordering::SeqCst);
        assert_eq!(
            scoped.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
        );
        assert_eq!(
            scoped.outcome(),
            Some(Err(crate::ClaimedCredentialPreparationFailure::Unavailable)),
            "receipt/no-change result keeps typed final admission refusal"
        );
        assert_eq!(repo.get(&session_id).await.unwrap(), pending);
        assert_eq!(
            gate.commits.load(Ordering::SeqCst),
            first_commit_count,
            "receipt replay cannot write before its current-revision check"
        );
        gate.reject.store(false, Ordering::SeqCst);
        assert_eq!(
            scoped.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
        );
        assert_eq!(
            repo.get(&session_id).await.unwrap(),
            pending,
            "exact duplicate preserves desired generation and original receipt"
        );
        let desired = pending.mcp.desired_attachments()[0];
        assert_eq!(desired.state, McpAttachmentState::Requested);
        assert_eq!(desired.credential.as_ref().unwrap().credential.revision, 2);
        assert_eq!(
            desired.credential.as_ref().unwrap().expires_at_unix_ms,
            Some(40_000)
        );
        assert_eq!(
            pending.active_activity_epochs,
            before.active_activity_epochs
        );
        assert_eq!(pending.run_activities(), before.run_activities());
        let mut peer = awaken_session_contract::SessionAggregate::try_from(pending).unwrap();
        assert_eq!(
            peer.activity().open(
                None,
                false,
                false,
                1_000,
                0,
                Some((
                    "peer",
                    &awaken_agent_contract::ThreadId("peer".into()),
                    &awaken_agent_contract::RunId("peer".into())
                ))
            ),
            awaken_session_contract::SessionActivityAdmissionPlan::NotReady
        );
        let control = app.realizations();
        let mut directive = control
            .begin_claimed_session_realization(
                &context,
                BeginSessionRealization {
                    session_id: session_id.clone(),
                    expected_resources: None,
                    target: awaken_session_execution_contract::SessionRealizationTarget {
                        owner: "worker".into(),
                        runtime_incarnation: "worker/boot".into(),
                        lease_expires_at_unix_ms: 100_000,
                        reassign_existing_lease: false,
                    },
                },
            )
            .await
            .unwrap();
        if case == "failed_replay" {
            // Fail the MCP generation through its aggregate law while the
            // original current Run remains nonterminal. Terminal Session failure
            // has a separate owner and correctly removes it from adoption.
            let SessionRealizationAction::Stage { mcp_stages, .. } = &directive.action else {
                panic!("fixture needs the issued MCP generation");
            };
            let stage = &mcp_stages[0];
            let mut failed_root = awaken_session_contract::SessionAggregate::try_from(
                repo.get(&session_id).await.unwrap(),
            )
            .unwrap();
            let failed_generation = failed_root.mcp.desired_attachments()[0].generation.clone();
            failed_root
                .mcp()
                .fail_realization(
                    &failed_generation,
                    &stage.realization_id,
                    "permanent fixture MCP failure",
                )
                .unwrap();
            let failed_commit = app
                .mutations()
                .commit_session_snapshot_with_outcome(
                    "workspace-a",
                    failed_root,
                    "fixture-failed-mcp-generation",
                    Vec::new(),
                )
                .await
                .unwrap();
            assert!(failed_commit.transitioned());
            let failed = repo.get(&session_id).await.unwrap();
            assert_eq!(
                failed.mcp.desired_attachments()[0].state,
                McpAttachmentState::Failed
            );
            let commits_before = gate.commits.load(Ordering::SeqCst);
            gate.reject.store(true, Ordering::SeqCst);
            assert_eq!(
                scoped.rollout(&event).await.unwrap(),
                awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
            );
            assert_eq!(
                scoped.outcome(),
                Some(Err(crate::ClaimedCredentialPreparationFailure::Unavailable)),
                "retry-generation CAS preserves final admission failure"
            );
            assert_eq!(
                gate.commits.load(Ordering::SeqCst),
                commits_before + 1,
                "retry-generation uses the original final gate once"
            );
            assert_eq!(repo.get(&session_id).await.unwrap(), failed);
            gate.reject.store(false, Ordering::SeqCst);
            assert_eq!(
                scoped.rollout(&event).await.unwrap(),
                awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
            );
            let retried = repo.get(&session_id).await.unwrap();
            assert!(
                retried.mcp.desired_attachments()[0].generation.version()
                    > failed.mcp.desired_attachments()[0].generation.version()
            );
            assert_eq!(
                retried.mcp.desired_attachments()[0]
                    .credential
                    .as_ref()
                    .unwrap()
                    .credential
                    .revision,
                2
            );
            directive = control
                .begin_claimed_session_realization(
                    &context,
                    BeginSessionRealization {
                        session_id: session_id.clone(),
                        expected_resources: None,
                        target: awaken_session_execution_contract::SessionRealizationTarget {
                            owner: "worker".into(),
                            runtime_incarnation: "worker/boot".into(),
                            lease_expires_at_unix_ms: 100_000,
                            reassign_existing_lease: false,
                        },
                    },
                )
                .await
                .unwrap();
        }
        let SessionRealizationAction::Stage { mcp_stages, .. } = directive.action else {
            panic!("new exact credential needs Stage")
        };
        assert_eq!(mcp_stages.len(), 1);
        let stage = &mcp_stages[0];
        assert_eq!(stage.credential.as_ref().unwrap().credential.revision, 2);
        let receipt = awaken_session_configuration_contract::mcp::McpRealizationReceipt {
            generation: stage.generation.clone(),
            realization_id: stage.realization_id.clone(),
            selected_plaintext_holder: stage.selected_plaintext_holder.clone(),
            actual_realization_kind: None,
            receipt_fingerprint: stage.fingerprint(),
        };
        let published = control
            .activate_session_realization(ActivateSessionRealization {
                session_id: session_id.clone(),
                lease: directive.authority.lease,
                prepared_resource_revision: None,
                mcp_receipts: vec![receipt],
            })
            .await
            .unwrap();
        assert_eq!(
            scoped.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending,
            "Active without exact publication acknowledgement remains pending"
        );
        let SessionRealizationAction::Publish { publish, drain } = published.action else {
            panic!("exact Publish expected")
        };
        let complete = control
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: session_id.clone(),
                lease: published.authority.lease,
                published: publish,
                drained: drain,
                continuation: None,
            })
            .await
            .unwrap();
        assert!(matches!(
            complete.action,
            SessionRealizationAction::Complete
        ));
        assert_eq!(
            scoped.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Converged
        );
        assert_eq!(
            scoped.outcome(),
            Some(Ok(
                crate::ClaimedCredentialPreparationProgress::AlreadyAdopted
            ))
        );
        let after = repo.get(&session_id).await.unwrap();
        assert!(!after.mcp.needs_realization());
        assert!(
            after
                .mcp
                .desired_attachments()
                .iter()
                .all(|a| a.state == McpAttachmentState::Active && a.publication_acknowledged)
        );
        for (kind, expected) in [
            (
                awaken_runtime_contract::runs::RunErrorKind::Unavailable,
                crate::ClaimedCredentialPreparationFailure::Unavailable,
            ),
            (
                awaken_runtime_contract::runs::RunErrorKind::BadRequest,
                crate::ClaimedCredentialPreparationFailure::Invalid,
            ),
            (
                awaken_runtime_contract::runs::RunErrorKind::Conflict,
                crate::ClaimedCredentialPreparationFailure::Invalid,
            ),
        ] {
            *gate.runtime_failure.lock().unwrap() = Some(kind);
            assert_eq!(
                scoped.rollout(&event).await.unwrap(),
                awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending
            );
            assert_eq!(
                scoped.outcome(),
                Some(Err(expected)),
                "AlreadyAdopted and desired updates preserve the same admission class"
            );
            assert_eq!(repo.get(&session_id).await.unwrap(), after);
        }
        *gate.runtime_failure.lock().unwrap() = None;
        // An older event observes the current pin without moving it backward.
        let older = rollout_event(
            "older-claimed-event",
            1,
            awaken_credential_contract::ManagedCredentialOperation::Update,
        );
        assert_eq!(
            scoped.rollout(&older).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Converged,
        );
        assert_eq!(repo.get(&session_id).await.unwrap(), after);
        // AlreadyAdopted still belongs to this exact current claim. Reaching
        // its expiry equality cannot acknowledge the old event or change Root.
        clock.set(11_000);
        assert_eq!(
            scoped.rollout(&event).await.unwrap(),
            awaken_credential_contract::ManagedCredentialAdoptionProgress::Pending,
        );
        assert_eq!(
            scoped.outcome(),
            Some(Err(crate::ClaimedCredentialPreparationFailure::Unavailable)),
        );
        assert_eq!(repo.get(&session_id).await.unwrap(), after);
        context.guard.into_inner().unwrap().release().await.unwrap();
    }
}
