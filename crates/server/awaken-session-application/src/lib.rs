//! Coordinator-owned Session application state and ports.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use awaken_environment_contract::EnvItem;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_contract::{
    ManagedSessionRepository, McpAttachmentRealizer, McpTarget, PersistedSession, RunError,
    SandboxProvisioning, SessionEnvironmentBindingSink, SessionLifecycleSink, SessionRuntime,
    SessionRuntimePlacement,
};

mod mutation;
pub use mutation::SessionMutationError;
mod ports;
pub use ports::{
    RepositoryCredentialIngress, ResolvedSessionEnvironment, SessionCredentialSource,
    SessionEnvironmentSource,
};
include!("application.rs");
mod activity;
mod live_inbox;
pub use activity::SessionActivityError;
mod contribution;
mod credentials;
mod projection;
pub use credentials::SessionPreparationError;
mod mcp;
mod realization;
mod resource_reconciliation;
pub use mcp::{McpAttachmentCandidate, McpAttachmentCandidateTarget};
pub use realization::{
    SessionRealizationError, SessionReconciliation, SessionReconciliationFailure,
};
pub use resource_reconciliation::SessionResourcePurgeGuard;
mod update;
pub use update::{
    SessionUpdateChanges, SessionUpdateCommand, SessionUpdateError, SessionUpdateOutcome,
};
mod terminal;
pub use terminal::SessionTerminalTransition;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use awaken_environment_realization_contract::EnvironmentImageBuildError;
    use awaken_session_contract::{
        ManagedSessionRepository, McpAttachmentRealizer, PersistedSession, RunError,
        SandboxProvisioning, SessionEnvironmentBindingSink, SessionRuntime,
        SessionRuntimePlacement,
    };

    use super::*;

    struct NoopRuntime;

    #[derive(Default)]
    struct RecordingReferenceIndex {
        fail_replace: AtomicBool,
        records: Mutex<BTreeSet<awaken_resource_contract::ResourceReferenceRecord>>,
    }

    #[async_trait::async_trait]
    impl awaken_resource_contract::ResourceReferenceIndex for RecordingReferenceIndex {
        async fn add_reference(
            &self,
            record: awaken_resource_contract::ResourceReferenceRecord,
        ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
            Ok(self.records.lock().unwrap().insert(record))
        }

        async fn remove_reference(
            &self,
            record: &awaken_resource_contract::ResourceReferenceRecord,
        ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
            Ok(self.records.lock().unwrap().remove(record))
        }

        async fn replace_references(
            &self,
            kind: awaken_resource_contract::ResourceReferenceKind,
            reference_id: &str,
            records: Vec<awaken_resource_contract::ResourceReferenceRecord>,
        ) -> Result<(), awaken_resource_contract::ResourcePurgeError> {
            if self.fail_replace.load(Ordering::SeqCst) {
                return Err(awaken_resource_contract::ResourcePurgeError::Storage(
                    "injected reference projection failure".into(),
                ));
            }
            let mut current = self.records.lock().unwrap();
            current.retain(|record| {
                record.reference.kind != kind || record.reference.reference_id != reference_id
            });
            current.extend(records);
            Ok(())
        }

        async fn references(
            &self,
            target: &awaken_resource_contract::ResourceTarget,
        ) -> Result<
            Vec<awaken_resource_contract::ResourceReference>,
            awaken_resource_contract::ResourcePurgeError,
        > {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|record| &record.target == target)
                .map(|record| record.reference.clone())
                .collect())
        }

        async fn references_for_resource(
            &self,
            kind: awaken_resource_contract::ResourceKind,
            resource_id: &str,
        ) -> Result<
            Vec<awaken_resource_contract::ResourceReferenceRecord>,
            awaken_resource_contract::ResourcePurgeError,
        > {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|record| {
                    record.target.kind == kind && record.target.resource_id == resource_id
                })
                .cloned()
                .collect())
        }
    }

    struct UnusedFileCatalog;

    #[async_trait::async_trait]
    impl awaken_resource_contract::FileCatalog for UnusedFileCatalog {
        async fn create_file(
            &self,
            _record: awaken_resource_contract::FileRecord,
        ) -> Result<
            awaken_resource_contract::CreateFileRecordOutcome,
            awaken_resource_contract::FileCatalogError,
        > {
            unreachable!("Skill-only projection never creates Files")
        }

        async fn get_file(
            &self,
            _workspace_id: &str,
            _file_id: &str,
            _include_deleted: bool,
        ) -> Result<
            Option<awaken_resource_contract::FileRecord>,
            awaken_resource_contract::FileCatalogError,
        > {
            unreachable!("Skill-only projection never resolves Files")
        }

        async fn list_files(
            &self,
            _workspace_id: &str,
            _scope_id: Option<&str>,
        ) -> Result<
            Vec<awaken_resource_contract::FileRecord>,
            awaken_resource_contract::FileCatalogError,
        > {
            Ok(Vec::new())
        }

        async fn mark_file_deleted(
            &self,
            _workspace_id: &str,
            _file_id: &str,
        ) -> Result<
            Option<awaken_resource_contract::FileRecord>,
            awaken_resource_contract::FileCatalogError,
        > {
            Ok(None)
        }

        async fn active_size_bytes(
            &self,
            _workspace_id: &str,
        ) -> Result<u64, awaken_resource_contract::FileCatalogError> {
            Ok(0)
        }
    }

    #[async_trait::async_trait]
    impl SessionRuntime for NoopRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: awaken_session_contract::ToolPermissionDecision,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
            _is_error: bool,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<awaken_session_contract::OutcomeReport, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        fn model(&self) -> String {
            "unused".into()
        }
    }

    struct NoopMcpRealizer;

    #[async_trait::async_trait]
    impl McpAttachmentRealizer for NoopMcpRealizer {}

    #[derive(Default)]
    struct RecordingEnvironmentSource {
        dispatched: Mutex<BTreeSet<String>>,
        failures: Mutex<BTreeSet<String>>,
    }

    impl RecordingEnvironmentSource {
        fn fail_for(&self, session_id: &str) {
            self.failures.lock().unwrap().insert(session_id.into());
        }
    }

    #[async_trait::async_trait]
    impl SessionEnvironmentSource for RecordingEnvironmentSource {
        async fn get(
            &self,
            _environment_id: &str,
        ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
            unreachable!("work dispatch does not reopen the Environment catalog")
        }

        async fn resolve_current_for_session(
            &self,
            _environment_id: &str,
            _runtime: Option<&str>,
            _mcp_targets: &[McpTarget],
        ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
            unreachable!("work dispatch consumes the frozen Session baseline")
        }

        async fn resolve_exact_for_session(
            &self,
            _environment_id: &str,
            _revision: u64,
            _runtime: Option<&str>,
            _mcp_targets: &[McpTarget],
        ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
            unreachable!("work dispatch consumes the frozen Session baseline")
        }

        async fn enqueue_session_work(
            &self,
            _environment_id: &str,
            session_id: &str,
        ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
            if self.failures.lock().unwrap().contains(session_id) {
                return Err(
                    awaken_session_contract::work_queue::WorkQueueError::Storage(format!(
                        "injected failure for {session_id}"
                    )),
                );
            }
            self.dispatched
                .lock()
                .unwrap()
                .insert(session_id.to_string());
            Ok(format!("work:{session_id}"))
        }
    }

    fn persisted(id: &str, self_hosted: bool, application: bool, status: &str) -> PersistedSession {
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env-worker".into(),
            revision: awaken_environment_contract::EnvironmentRevision(7),
            self_hosted,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-7".into()),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        PersistedSession {
            session_id: id.into(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Frozen(
                awaken_session_contract::SessionBaseline::compile(
                    awaken_session_contract::SessionBaselineInputs {
                        environment,
                        runtime_placement: SessionRuntimePlacement::Local,
                        mcp_authoring: Default::default(),
                        agent_id: "agent".into(),
                        model: "model".into(),
                        runtime: None,
                        application: application.then(|| {
                            awaken_session_contract::ApplicationContributionReceipt {
                                plan_fingerprint: "plan".into(),
                                input_fingerprint: "input".into(),
                            }
                        }),
                        delegate_ids: Vec::new(),
                        toolsets: Vec::new(),
                        mounts: Vec::new(),
                        env: Vec::new(),
                        prompts: Vec::new(),
                    },
                ),
            ),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: status.into(),
            archived_at: None,
        }
    }

    async fn create(repo: &dyn ManagedSessionRepository, session: PersistedSession) {
        let id = session.session_id.clone();
        repo.create(
            "workspace",
            session,
            awaken_session_contract::IdempotencyRecord {
                key: format!("create:{id}"),
                payload_hash: format!("payload:{id}"),
            },
            Vec::new(),
        )
        .await
        .expect("persist fixture");
    }

    fn application(
        repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
    ) -> SessionApplication {
        application_with_configuration(
            repo,
            environments,
            SessionApplicationConfiguration::default(),
        )
    }

    fn application_with_configuration(
        repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
        configuration: SessionApplicationConfiguration,
    ) -> SessionApplication {
        SessionApplication::new_with_configuration(
            Arc::new(NoopRuntime),
            Arc::new(NoopMcpRealizer),
            repo,
            environments,
            configuration,
        )
    }

    fn skill_resources(id: &str) -> awaken_session_contract::ResolvedSessionResources {
        awaken_session_contract::ResolvedSessionResources {
            inputs: Vec::new(),
            skills: Some(vec![awaken_session_contract::ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: id.into(),
                version: 1,
                bundle_sha256: format!("sha-{id}"),
            }]),
        }
    }

    fn file_resources(id: &str) -> awaken_session_contract::ResolvedSessionResources {
        awaken_session_contract::ResolvedSessionResources {
            inputs: vec![awaken_session_contract::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::from("input"),
                source: awaken_session_contract::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from(id),
                },
                mount_path: "/workspace/input".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            skills: Some(Vec::new()),
        }
    }

    /// Local-restart ownership FMECA and cause/effect decision table. C1 the
    /// configured logical owner is unchanged or different; C2 the Runtime
    /// incarnation is the original process or a replacement. Effects are E1 an
    /// exact replay, E2 immediate epoch-fenced takeover, E3 stale-owner
    /// rejection, or E4 claim-authorized topology reassignment. C3 renewal and
    /// reassignment are mutually exclusive so one command cannot obscure its
    /// authority class.
    ///
    /// | Rule | Logical owner | Incarnation | Reassign | Renew | Effect |
    /// |---|---|---|---|---|---|
    /// | L1 | same | same | false | false | E1 replay |
    /// | L2 | same | replacement | false | false | E2 epoch advances immediately |
    /// | L3 | different | replacement | false | false | E3 stale ownership |
    /// | L4 | different | replacement | true | false | E4 epoch advances immediately |
    /// | L5 | any | any | true | true | invalid, no mutation |
    #[tokio::test]
    async fn local_realization_uses_stable_owner_and_process_incarnation() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(
            repo.as_ref(),
            persisted("local-restart", false, false, "idle"),
        )
        .await;
        let configured = |owner: &str| SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            local_realization_owner: owner.into(),
        };

        let first = application_with_configuration(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            configured("embedded-worker"),
        );
        let first_session = first.realize_session("local-restart").await.expect("L1");
        let first_lease = first_session.realization.expect("L1 lease");
        assert_eq!(first_lease.owner, "embedded-worker", "L1/E1");
        assert_eq!(
            first
                .realize_session("local-restart")
                .await
                .expect("L1 exact replay")
                .realization
                .expect("L1 replay lease"),
            first_lease,
            "L1/E1"
        );

        let replacement = application_with_configuration(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            configured("embedded-worker"),
        );
        let replaced = replacement
            .realize_session("local-restart")
            .await
            .expect("L2 same logical owner may fence a crashed incarnation");
        let replaced_lease = replaced.realization.expect("L2 lease");
        assert_eq!(replaced_lease.owner, first_lease.owner, "L2/E2");
        assert_ne!(
            replaced_lease.runtime_incarnation, first_lease.runtime_incarnation,
            "L2/E2"
        );
        assert_eq!(replaced_lease.epoch, first_lease.epoch + 1, "L2/E2");

        let other = application_with_configuration(
            repo,
            Arc::new(RecordingEnvironmentSource::default()),
            configured("other-worker"),
        );
        assert!(
            matches!(
                other.realize_session("local-restart").await,
                Err(crate::SessionRealizationError::Control(
                    awaken_session_contract::SessionRealizationControlFailure::StaleOwnership
                ))
            ),
            "L3/E3"
        );
        let reassigned =
            awaken_session_contract::SessionRealizationControl::begin_session_realization(
                &other,
                awaken_session_contract::BeginSessionRealization {
                    session_id: "local-restart".into(),
                    target: awaken_session_contract::SessionRealizationTarget {
                        owner: "other-worker".into(),
                        runtime_incarnation: "other-worker/boot-2".into(),
                        lease_expires_at_unix_ms: u64::MAX,
                        renew_existing_lease: false,
                        reassign_existing_lease: true,
                    },
                },
            )
            .await
            .expect("L4 claim-authorized reassignment");
        assert_eq!(reassigned.lease.epoch, replaced_lease.epoch + 1, "L4/E4");
        assert_eq!(reassigned.lease.owner, "other-worker", "L4/E4");

        let invalid =
            awaken_session_contract::SessionRealizationControl::begin_session_realization(
                &other,
                awaken_session_contract::BeginSessionRealization {
                    session_id: "local-restart".into(),
                    target: awaken_session_contract::SessionRealizationTarget {
                        owner: "other-worker".into(),
                        runtime_incarnation: "other-worker/boot-2".into(),
                        lease_expires_at_unix_ms: u64::MAX,
                        renew_existing_lease: true,
                        reassign_existing_lease: true,
                    },
                },
            )
            .await;
        assert!(
            matches!(
                invalid,
                Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
            ),
            "L5"
        );
    }

    /// Reference-projection FMECA and cause/effect decision table. C1 the
    /// Resource index accepts or rejects a replacement; C2 the Session root CAS
    /// wins or conflicts after projection. Effects are E1 no durable intent is
    /// exposed without its safety edge, E2 a losing candidate is repaired back
    /// to repository truth, and E3 a winning pending replacement retains both
    /// installed and desired targets until settlement.
    ///
    /// | Rule | C1 index | C2 root CAS | Effect |
    /// |---|---|---|---|
    /// | R1 | rejects | not attempted | E1 unchanged Session |
    /// | R2 | accepts | conflicts | E2 installed target only |
    /// | R3 | accepts | wins | E3 installed + desired targets |
    #[tokio::test]
    async fn resource_reference_projection_fails_closed_and_repairs_cas_losers() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let environments = Arc::new(RecordingEnvironmentSource::default());
        let mut application = application(repo.clone(), environments);
        let references = Arc::new(RecordingReferenceIndex::default());
        application
            .set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));

        let mut original = persisted("reference-fence", false, false, "idle");
        original.resources =
            awaken_session_contract::SessionResourceState::from_legacy(skill_resources("old"));
        create(repo.as_ref(), original).await;

        let mut rejected = repo.get("reference-fence").await.unwrap();
        let original_revision = rejected.revision;
        rejected
            .resources
            .prepare("reference-fence", skill_resources("new"))
            .unwrap();
        references.fail_replace.store(true, Ordering::SeqCst);
        assert!(
            application
                .commit_resource_snapshot("workspace", rejected, "R1", Vec::new())
                .await
                .is_err(),
            "R1"
        );
        assert_eq!(
            repo.get("reference-fence").await.unwrap().revision,
            original_revision,
            "R1/E1"
        );

        references.fail_replace.store(false, Ordering::SeqCst);
        let mut stale = repo.get("reference-fence").await.unwrap();
        let mut winner = stale.clone();
        winner.title = Some("concurrent winner".into());
        application
            .commit_session_snapshot("workspace", winner, "concurrent", Vec::new())
            .await
            .unwrap();
        stale
            .resources
            .prepare("reference-fence", skill_resources("new"))
            .unwrap();
        assert!(matches!(
            application
                .commit_resource_snapshot("workspace", stale, "R2", Vec::new())
                .await,
            Err(SessionMutationError::Conflict)
        ));
        let ids = || {
            references
                .records
                .lock()
                .unwrap()
                .iter()
                .map(|record| record.target.resource_id.clone())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(ids(), BTreeSet::from(["old".to_string()]), "R2/E2");

        let mut accepted = repo.get("reference-fence").await.unwrap();
        accepted
            .resources
            .prepare("reference-fence", skill_resources("new"))
            .unwrap();
        application
            .commit_resource_snapshot("workspace", accepted, "R3", Vec::new())
            .await
            .unwrap();
        assert_eq!(
            ids(),
            BTreeSet::from(["new".to_string(), "old".to_string()]),
            "R3/E3"
        );
    }

    /// Legacy Resource adoption FMECA and cause/effect decision table. C1 the
    /// retained manifest has no activation records; C2 the Worker reports that
    /// it prepared the exact active revision; C3 the reported revision is stale.
    /// Effects are E1 one Active/attempted record is committed, E2 no second
    /// physical-state authority is created, and E3 a mismatched generation is
    /// rejected without mutating durable truth.
    ///
    /// | Rule | Legacy inputs | Prepared revision | Effect |
    /// |---|---|---|---|
    /// | L1 | present | exact | E1 adopt once |
    /// | L2 | present | stale | E3 reject, unchanged |
    /// | L3 | absent/already adopted | any | E2 normal realization path |
    #[tokio::test]
    async fn remote_realization_adopts_only_the_exact_legacy_resource_generation() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let application = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        for id in ["legacy-exact", "legacy-stale"] {
            let mut session = persisted(id, false, false, "preparing");
            session.resources =
                awaken_session_contract::SessionResourceState::from_legacy(file_resources(id));
            session.realization = Some(lease.clone());
            create(repo.as_ref(), session).await;
        }

        let exact =
            awaken_session_contract::SessionRealizationControl::activate_session_realization(
                &application,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "legacy-exact".into(),
                    lease: lease.clone(),
                    prepared_resource_revision: Some(1),
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .expect("L1 exact legacy generation");
        assert!(
            matches!(
                exact.action,
                awaken_session_contract::SessionRealizationAction::Publish { .. }
            ),
            "L1/E1"
        );
        let adopted = repo.get("legacy-exact").await.expect("L1 durable truth");
        assert_eq!(adopted.resources.activations.len(), 1, "L1/E1");
        assert_eq!(
            adopted.resources.activations[0].state,
            awaken_session_contract::ActivationState::Active,
            "L1/E1"
        );
        assert_eq!(adopted.resources.activations[0].attempts, 1, "L1/E1");

        let stale =
            awaken_session_contract::SessionRealizationControl::activate_session_realization(
                &application,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "legacy-stale".into(),
                    lease,
                    prepared_resource_revision: Some(2),
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .expect_err("L2 stale generation must fail closed");
        assert!(
            matches!(
                stale,
                awaken_session_contract::SessionRealizationControlFailure::Invalid(_)
            ),
            "L2/E3: {stale}"
        );
        assert!(
            repo.get("legacy-stale")
                .await
                .expect("L2 durable truth")
                .resources
                .activations
                .is_empty(),
            "L2/E3"
        );
    }

    /// Terminal Worker-placement FMECA and cause/effect decision table. C1 the
    /// Session placement delegates live realization to a Worker; C2 the Session
    /// is live with pending work; C3 it is terminal with Releasing work. Effects
    /// are E1 no Coordinator execution effect for a live generation and E2 the
    /// sole terminal cleanup path converges because no future Run can claim it.
    ///
    /// | Rule | Placement | Lifecycle | Resource state | Effect |
    /// |---|---|---|---|---|
    /// | W1 | Worker | idle | Prepared | E1 remain unattempted |
    /// | W2 | Worker | terminal | Releasing | E2 release durably |
    #[tokio::test]
    async fn worker_placement_defers_live_effects_but_not_terminal_cleanup() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let worker_baseline = |id: &str, status: &str| {
            let mut session = persisted(id, false, false, status);
            let awaken_session_contract::SessionBaselineState::Frozen(baseline) =
                &mut session.baseline
            else {
                unreachable!("fixture is frozen")
            };
            baseline.runtime_placement = SessionRuntimePlacement::Worker;
            session
        };

        let mut live = worker_baseline("worker-live", "idle");
        live.resources
            .prepare("worker-live", file_resources("live"))
            .expect("W1 prepared generation");
        create(repo.as_ref(), live).await;

        let mut terminal = worker_baseline("worker-terminal", "terminated");
        terminal.resources =
            awaken_session_contract::SessionResourceState::from_legacy(file_resources("terminal"));
        terminal.resources.adopt_legacy_active("worker-terminal");
        terminal
            .resources
            .begin_release()
            .expect("W2 release intent");
        create(repo.as_ref(), terminal).await;

        let application = application_with_configuration(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
        );
        let report = application.reconcile_resource_activations().await;
        assert!(report.failures.is_empty(), "W1/W2: {:#?}", report.failures);
        assert_eq!(report.settled.len(), 1, "W2/E2");

        let live = repo.get("worker-live").await.expect("W1 durable truth");
        assert!(live.resources.pending.is_some(), "W1/E1");
        assert_eq!(live.resources.activations[0].attempts, 0, "W1/E1");
        let terminal = repo.get("worker-terminal").await.expect("W2 durable truth");
        assert!(
            terminal
                .resources
                .activations
                .iter()
                .all(|activation| activation.state
                    == awaken_session_contract::ActivationState::Released),
            "W2/E2 durable={:?} settled={:?}",
            terminal.resources.activations,
            report.settled[0].resources.activations
        );
    }

    /// Cause/effect graph: C1 a baseline is frozen; C2 its explicit Runtime
    /// placement is Local or Worker; C3 a retained pre-placement row is marked
    /// LegacyUnspecified; C4 the process composition is local or registered;
    /// C5 an application contribution independently requires Worker custody.
    /// The realization lease is intentionally absent from the causes: it is an
    /// assignment fence, never placement policy. Effects are E1 local physical
    /// realization or E2 dispatch-only Coordinator projection.
    ///
    /// | Rule | Frozen placement | Process placement | Application | Effect |
    /// |---|---|---|---|---|
    /// | P1 | preparing | any | n/a | E1 (not yet realizable) |
    /// | P2 | local | local/registered | absent | E1 |
    /// | P3 | worker | local/registered | absent | E2 |
    /// | P4 | local/worker | any | present | E2 |
    /// | P5 | legacy | local | absent | E1 |
    /// | P6 | legacy | registered | absent | E2 |
    ///
    /// P5/P6 are the one-way upgrade interpretation for rows serialized before
    /// placement existed. New creation is separately asserted never to emit the
    /// legacy value.
    #[test]
    fn realization_owner_follows_the_application_placement_decision_table() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let environments = Arc::new(RecordingEnvironmentSource::default());
        let local = application_with_configuration(
            repo.clone(),
            environments.clone(),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::LocalWorker,
                ..Default::default()
            },
        );
        let registered = application_with_configuration(
            repo,
            environments,
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
        );

        let frozen = |placement: SessionRuntimePlacement, application: bool| {
            let mut value = persisted("placement", false, false, "idle");
            let awaken_session_contract::SessionBaselineState::Frozen(baseline) =
                &mut value.baseline
            else {
                unreachable!("fixture is frozen")
            };
            baseline.runtime_placement = placement;
            baseline.application =
                application.then(|| awaken_session_contract::ApplicationContributionReceipt {
                    plan_fingerprint: "plan".into(),
                    input_fingerprint: "input".into(),
                });
            value
        };
        let preparing = {
            let mut value = frozen(SessionRuntimePlacement::Local, false);
            let baseline = value.frozen_baseline().expect("frozen").clone();
            value.baseline = awaken_session_contract::SessionBaselineState::Preparing(
                awaken_session_contract::SessionCreationIntent {
                    control: awaken_session_contract::ControlSessionCreationInputs {
                        environment: baseline.environment,
                        runtime_placement: SessionRuntimePlacement::Local,
                        agent_id: baseline.agent_id,
                        model: baseline.model,
                        execution_model_ref: baseline.execution_model_ref,
                        runtime: baseline.runtime,
                        mcp_authoring: baseline.mcp_authoring,
                        delegate_ids: baseline.delegate_ids,
                        toolsets: baseline.toolsets,
                        mounts: baseline.mounts,
                        env: baseline.env,
                        prompts: baseline.prompts,
                        resources: Default::default(),
                        initial_mcp: Vec::new(),
                    },
                    application: awaken_session_contract::ApplicationContributionState::Absent,
                },
            );
            value
        };

        for (rule, value, local_expected, registered_expected) in [
            ("P1", preparing, false, false),
            (
                "P2",
                frozen(SessionRuntimePlacement::Local, false),
                false,
                false,
            ),
            (
                "P3",
                frozen(SessionRuntimePlacement::Worker, false),
                true,
                true,
            ),
            (
                "P4a",
                frozen(SessionRuntimePlacement::Local, true),
                true,
                true,
            ),
            (
                "P4b",
                frozen(SessionRuntimePlacement::Worker, true),
                true,
                true,
            ),
            (
                "P5/P6",
                frozen(SessionRuntimePlacement::LegacyUnspecified, false),
                false,
                true,
            ),
        ] {
            assert_eq!(
                local.requires_external_realization(&value),
                local_expected,
                "{rule}/local"
            );
            assert_eq!(
                registered.requires_external_realization(&value),
                registered_expected,
                "{rule}/registered"
            );
        }
        assert_eq!(local.runtime_placement(), SessionRuntimePlacement::Local);
        assert_eq!(
            registered.runtime_placement(),
            SessionRuntimePlacement::Worker
        );
    }

    #[tokio::test]
    async fn publication_acknowledgement_follows_the_renewal_decision_table() {
        // Cause/effect graph: C1 acknowledgement names the exact active
        // generation; C2 it names the same owner/incarnation/epoch under a
        // shorter expiry; C3 any immutable generation coordinate differs.
        // Effects: E1 acknowledge and make the Session idle; E2 commit nothing
        // and return the current Stage for canonical catch-up; E3 fail closed
        // without changing publication state.
        //
        // | Rule | exact | monotonic predecessor | replacement | Effect |
        // |---|---|---|---|---|
        // | A1 | yes | no | no | E1 |
        // | A2 | no | yes | no | E2 |
        // | A3 | no | no | yes | E3 |
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let environments = Arc::new(RecordingEnvironmentSource::default());
        let application = application(repo.clone(), environments);
        let current_expiry = u64::MAX - 1;

        let fixture = |id: &str| {
            let mut session = persisted(id, false, false, "activating");
            let lease = awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a/boot-1".into(),
                epoch: 7,
                expires_at_unix_ms: current_expiry,
            };
            let attachment_id = awaken_session_contract::McpAttachmentId("browser".into());
            let generation = awaken_session_contract::McpGeneration(1);
            session.realization = Some(lease.clone());
            session
                .mcp
                .attachments
                .push(awaken_session_contract::SessionMcpAttachment {
                    attachment_id: attachment_id.clone(),
                    name: "browser".into(),
                    generation,
                    target: awaken_session_contract::McpTarget::parse_http(
                        "https://browser.example.test/mcp",
                    )
                    .expect("MCP target"),
                    prompts_as_skills: false,
                    origin: awaken_session_contract::McpAttachmentOrigin::Agent,
                    credential: None,
                    selected_plaintext_holder: None,
                    state: awaken_session_contract::McpAttachmentState::Active,
                    publication_acknowledged: false,
                    realization: Some(awaken_session_contract::McpRealizationClaim {
                        realization_id: "realize-browser-1".into(),
                        runtime_incarnation: lease.runtime_incarnation.clone(),
                        lease_epoch: lease.epoch,
                        lease_expires_at_unix_ms: current_expiry,
                        stage_idempotency_key: "renew-browser-1".into(),
                    }),
                    attempts: 1,
                    last_error: None,
                });
            let generation_ref = awaken_session_contract::McpGenerationRef {
                session_id: id.into(),
                attachment_id,
                generation,
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: current_expiry,
            };
            (session, lease, generation_ref)
        };

        let (exact, exact_lease, exact_generation) = fixture("ack-exact");
        create(repo.as_ref(), exact).await;
        let exact_result =
            awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
                &application,
                awaken_session_contract::AcknowledgeSessionRealization {
                    session_id: "ack-exact".into(),
                    lease: exact_lease,
                    published: vec![exact_generation],
                    drained: Vec::new(),
                },
            )
            .await
            .expect("A1 exact acknowledgement");
        assert!(
            matches!(
                exact_result.action,
                awaken_session_contract::SessionRealizationAction::Complete
            ),
            "A1/E1"
        );
        assert_eq!(repo.get("ack-exact").await.unwrap().status, "idle", "A1/E1");

        let (renewed, renewed_lease, mut predecessor) = fixture("ack-renewed");
        create(repo.as_ref(), renewed).await;
        predecessor.lease_expires_at_unix_ms -= 1;
        let mut predecessor_lease = renewed_lease.clone();
        predecessor_lease.expires_at_unix_ms -= 1;
        let renewed_result =
            awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
                &application,
                awaken_session_contract::AcknowledgeSessionRealization {
                    session_id: "ack-renewed".into(),
                    lease: predecessor_lease,
                    published: vec![predecessor],
                    drained: Vec::new(),
                },
            )
            .await
            .expect("A2 monotonic predecessor");
        assert!(
            matches!(
                renewed_result.action,
                awaken_session_contract::SessionRealizationAction::Stage { .. }
            ),
            "A2/E2"
        );
        let renewed_truth = repo.get("ack-renewed").await.unwrap();
        assert_eq!(renewed_truth.status, "activating", "A2/E2");
        assert!(
            !renewed_truth.mcp.attachments[0].publication_acknowledged,
            "A2/E2"
        );

        let (replacement, replacement_lease, mut wrong_generation) = fixture("ack-replaced");
        create(repo.as_ref(), replacement).await;
        wrong_generation.attachment_id = awaken_session_contract::McpAttachmentId("other".into());
        let error =
            awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
                &application,
                awaken_session_contract::AcknowledgeSessionRealization {
                    session_id: "ack-replaced".into(),
                    lease: replacement_lease,
                    published: vec![wrong_generation],
                    drained: Vec::new(),
                },
            )
            .await
            .expect_err("A3 replacement must fail closed");
        assert!(
            matches!(
                error,
                awaken_session_contract::SessionRealizationControlFailure::Invalid(_)
            ),
            "A3/E3: {error}"
        );
        assert!(
            !repo.get("ack-replaced").await.unwrap().mcp.attachments[0].publication_acknowledged,
            "A3/E3"
        );
    }

    #[tokio::test]
    async fn root_mutation_cause_effect_decision_table() {
        // Cause-effect graph: C1 expected revision is current; C2 idempotency key
        // and payload hash replay exactly; C3 expected revision is stale; C4 an
        // existing key is reused with another hash. Effects: E1 apply once and
        // advance revision; E2 replay current truth without another advance; E3
        // conflict; E4 idempotency mismatch. No interface adapter owns a second
        // CAS or retry algorithm.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|---|
        // | M1 | yes | no | no | no | E1 applied |
        // | M2 | no | yes | no | no | E2 replayed |
        // | M3 | no | no | yes | no | E3 conflict |
        // | M4 | no | no | no | yes | E4 mismatch |
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(repo.as_ref(), persisted("mutation", false, false, "idle")).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let original = repo.get("mutation").await.expect("fixture");
        let mut candidate = original.clone();
        candidate.title = Some("applied".into());
        let payload = awaken_session_contract::SessionMutationPayload::Replace(candidate.clone());
        let record = awaken_session_contract::IdempotencyRecord {
            key: "mutation:one".into(),
            payload_hash: payload.stable_hash(),
        };

        let (applied, changed) = app
            .commit_session_snapshot_with_record(
                "workspace",
                candidate.clone(),
                record.clone(),
                Vec::new(),
            )
            .await
            .expect("M1");
        assert!(changed, "M1");
        assert!(applied.revision > original.revision, "M1");

        let (replayed, changed) = app
            .commit_session_snapshot_with_record(
                "workspace",
                candidate.clone(),
                record.clone(),
                Vec::new(),
            )
            .await
            .expect("M2");
        assert!(!changed, "M2");
        assert_eq!(replayed.revision, applied.revision, "M2");

        let stale = app
            .commit_session_snapshot("workspace", candidate.clone(), "stale", Vec::new())
            .await;
        assert_eq!(stale, Err(SessionMutationError::Conflict), "M3");

        let mismatch = app
            .commit_session_snapshot_with_record(
                "workspace",
                candidate,
                awaken_session_contract::IdempotencyRecord {
                    key: record.key,
                    payload_hash: "another-payload".into(),
                },
                Vec::new(),
            )
            .await;
        assert_eq!(
            mismatch,
            Err(SessionMutationError::IdempotencyMismatch),
            "M4"
        );
    }

    /// Session-root insertion graph. C1 identity is unused; C2 key/hash/payload
    /// exactly replay; C3 an existing key carries another hash; C4 another key
    /// targets an existing identity. Effects are E1 one insert/revision advance,
    /// E2 replay of the same revision, E3 idempotency mismatch, and E4 identity
    /// conflict. The application is the only repository-result classifier.
    ///
    /// | Rule | Identity | Key/hash | Payload | Effect |
    /// |---|---|---|---|---|
    /// | C1 | unused | new | original | E1 |
    /// | C2 | existing | exact | exact | E2 |
    /// | C3 | existing | same key/different hash | changed | E3 |
    /// | C4 | existing | another key | any | E4 |
    #[tokio::test]
    async fn create_session_root_classifies_insert_replay_and_conflicts() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let original = persisted("create-root", false, false, "preparing");
        let payload = awaken_session_contract::SessionMutationPayload::Replace(original.clone());
        let record = awaken_session_contract::IdempotencyRecord {
            key: "create-root:one".into(),
            payload_hash: payload.stable_hash(),
        };

        let inserted = app
            .create_session_root("workspace", original.clone(), record.clone(), Vec::new())
            .await
            .expect("C1");
        assert_eq!(
            inserted.revision,
            awaken_session_contract::SessionRevision(1),
            "C1"
        );
        let replayed = app
            .create_session_root("workspace", original.clone(), record.clone(), Vec::new())
            .await
            .expect("C2");
        assert_eq!(replayed.revision, inserted.revision, "C2");

        let mut changed = original.clone();
        changed.title = Some("changed".into());
        assert_eq!(
            app.create_session_root(
                "workspace",
                changed,
                awaken_session_contract::IdempotencyRecord {
                    key: record.key,
                    payload_hash: "changed-hash".into(),
                },
                Vec::new(),
            )
            .await,
            Err(SessionMutationError::IdempotencyMismatch),
            "C3"
        );
        assert_eq!(
            app.create_session_root(
                "workspace",
                original,
                awaken_session_contract::IdempotencyRecord {
                    key: "create-root:another".into(),
                    payload_hash: "another-hash".into(),
                },
                Vec::new(),
            )
            .await,
            Err(SessionMutationError::Conflict),
            "C4"
        );
    }

    /// Terminal-transition cause/effect graph. C1 the durable Session is live;
    /// C2 two archive commands race; C3 the same archive is replayed; C4 delete
    /// targets a live Session; C5 delete targets another terminal state. Effects:
    /// E1 exactly one archive CAS/fact and one idempotent observation; E2 replay
    /// does not advance revision; E3 delete atomically records hidden terminal
    /// status plus recoverable cleanup classification; E4 terminal-to-terminal mutation is rejected.
    ///
    /// | Rule | Durable state | Command | Race/replay | Effect |
    /// |---|---|---|---|---|
    /// | L1 | idle | archive | race | E1 |
    /// | L2 | terminated | archive | replay | E2 |
    /// | L3 | idle | delete | none | E3 |
    /// | L4 | terminated | delete | none | E4 |
    #[tokio::test]
    async fn terminal_transition_decision_table_is_durable_and_idempotent() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(
            repo.as_ref(),
            persisted("archive-race", false, false, "idle"),
        )
        .await;
        create(
            repo.as_ref(),
            persisted("delete-live", false, false, "idle"),
        )
        .await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let fact = |id: &str, event_type: &str| awaken_session_contract::ManagedLifecycleFact {
            id: format!("{id}:{event_type}"),
            object_id: id.into(),
            workspace_id: Some("workspace".into()),
            event_type: event_type.into(),
            timestamp: 1,
        };

        let archive_fact = fact("archive-race", "session.terminated");
        let (first, second) = tokio::join!(
            app.begin_archive("archive-race", "2026-08-06T00:00:00Z", archive_fact.clone()),
            app.begin_archive("archive-race", "2026-08-06T00:00:00Z", archive_fact)
        );
        let first = first.expect("L1 first");
        let second = second.expect("L1 second");
        assert_ne!(first.transitioned, second.transitioned, "L1");
        let archived = repo.get("archive-race").await.expect("L1 durable");
        assert_eq!(archived.status, "terminated", "L1");
        let revision = archived.revision;
        let replay = app
            .begin_archive(
                "archive-race",
                "another-timestamp",
                fact("archive-race", "session.terminated"),
            )
            .await
            .expect("L2");
        assert!(!replay.transitioned, "L2");
        assert_eq!(replay.session.revision, revision, "L2");

        let deleted = app
            .begin_delete("delete-live", fact("delete-live", "session.deleted"))
            .await
            .expect("L3");
        assert!(deleted.transitioned, "L3");
        assert_eq!(deleted.session.status, "deleted", "L3");
        assert!(deleted.session.needs_resource_reconciliation(), "L3");
        assert!(
            matches!(
                app.begin_delete("archive-race", fact("archive-race", "session.deleted"))
                    .await,
                Err(SessionPreparationError::NotFound)
            ),
            "L4"
        );
    }

    /// Activity-fence FMECA cause/effect graph. Causes: C1 the Session exists;
    /// C2 it is nonterminal; C3 the epoch can advance; C4 settlement presents
    /// the current epoch; C5 a later admission or terminal transition has
    /// fenced that settlement; C6 initial realization is still preparing.
    /// Effects: E1 an idle admission commits `running` with one unique monotonic
    /// epoch; E2 only the current running completion commits `idle`; E3 stale or
    /// terminal completions are no-ops; E4 missing, terminal-admission, and
    /// exhausted-epoch failures do not mutate durable truth; E5 a queued first
    /// turn advances its epoch without masking realization state.
    ///
    /// | Rule | Exists | Status | Epoch available | Current settle | Fence | Effect |
    /// |---|---|---|---|---|---|---|
    /// | A1 | yes | idle | yes | n/a | concurrent admit | E1, distinct epochs |
    /// | A2 | yes | running | n/a | no | newer epoch | E3, remains running |
    /// | A3 | yes | running | n/a | yes | none | E2, idle |
    /// | A4 | yes | terminal | n/a | any | terminal | E3, terminal preserved |
    /// | A5 | yes | terminal | any | n/a | n/a | E4, reject admission |
    /// | A6 | yes | idle | no | n/a | n/a | E4, reject exhaustion |
    /// | A7 | no | n/a | n/a | n/a | n/a | E4, not found |
    /// | A8 | yes | preparing | yes | yes | realization pending | E5, preserve preparing |
    /// | A9 | yes | activation_failed | n/a | any | realization failed | E3, preserve failed |
    #[tokio::test]
    async fn activity_fence_decision_table_preserves_monotonic_and_terminal_truth() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(repo.as_ref(), persisted("activity", false, false, "idle")).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );

        let (first, second) = tokio::join!(
            app.begin_activity("activity"),
            app.begin_activity("activity")
        );
        let first = first.expect("A1 first admission");
        let second = second.expect("A1 concurrent admission");
        let mut epochs = [first.activity_epoch, second.activity_epoch];
        epochs.sort_unstable();
        assert_eq!(epochs, [1, 2], "A1");
        let active = repo.get("activity").await.expect("A1 durable Session");
        assert_eq!(active.status, "running", "A1");

        let stale = app
            .settle_activity("activity", epochs[0])
            .await
            .expect("A2 stale settlement");
        assert_eq!(stale.status, "running", "A2");
        assert_eq!(stale.activity_epoch, epochs[1], "A2");

        let idle = app
            .settle_activity("activity", epochs[1])
            .await
            .expect("A3 current settlement");
        assert_eq!(idle.status, "idle", "A3");

        let running = app
            .begin_activity("activity")
            .await
            .expect("A4 activity before terminal transition");
        let mut terminated = running.clone();
        terminated.status = "terminated".into();
        let terminated = app
            .commit_session_snapshot(
                "workspace",
                terminated,
                "activity-test-terminal",
                Vec::new(),
            )
            .await
            .expect("A4 terminal transition");
        let fenced = app
            .settle_activity("activity", running.activity_epoch)
            .await
            .expect("A4 terminal settlement is idempotent");
        assert_eq!(fenced, terminated, "A4");
        assert_eq!(
            app.begin_activity("activity").await,
            Err(SessionActivityError::Terminal),
            "A5"
        );

        let mut exhausted = persisted("activity-exhausted", false, false, "idle");
        exhausted.activity_epoch = u64::MAX;
        create(repo.as_ref(), exhausted).await;
        let exhausted_before = repo
            .get("activity-exhausted")
            .await
            .expect("A6 durable Session before admission");
        assert_eq!(
            app.begin_activity("activity-exhausted").await,
            Err(SessionActivityError::EpochExhausted),
            "A6"
        );
        assert_eq!(
            repo.get("activity-exhausted")
                .await
                .expect("A6 durable Session"),
            exhausted_before,
            "A6"
        );
        assert_eq!(
            app.begin_activity("missing").await,
            Err(SessionActivityError::NotFound),
            "A7"
        );

        create(
            repo.as_ref(),
            persisted("activity-preparing", false, false, "preparing"),
        )
        .await;
        let preparing = app
            .begin_activity("activity-preparing")
            .await
            .expect("A8 queued activity");
        assert_eq!(preparing.activity_epoch, 1, "A8/E5");
        assert_eq!(preparing.status, "preparing", "A8/E5");
        let still_preparing = app
            .settle_activity("activity-preparing", preparing.activity_epoch)
            .await
            .expect("A8 settlement");
        assert_eq!(still_preparing.status, "preparing", "A8/E5");

        let mut failed = still_preparing;
        failed.status = "activation_failed".into();
        let failed = app
            .commit_session_snapshot(
                "workspace",
                failed,
                "activity-test-realization-failed",
                Vec::new(),
            )
            .await
            .expect("A9 terminal realization");
        assert_eq!(
            app.settle_activity("activity-preparing", preparing.activity_epoch)
                .await
                .expect("A9 stale settlement"),
            failed,
            "A9/E3"
        );
    }

    /// Update-admission authority graph. C1 durable status is idle; C2 durable
    /// status is running/terminal; C3 an interface cache is absent or stale.
    /// Only C1 permits mutation (E1); C2 always rejects without a root revision
    /// change (E2), independently of C3. This prevents another process's wire
    /// projection from becoming a parallel lifecycle authority.
    ///
    /// | Rule | Durable status | Wire cache | Effect |
    /// |---|---|---|---|
    /// | U1 | idle | any/absent | apply through root CAS |
    /// | U2 | running | any/absent | reject, no mutation |
    /// | U3 | terminal | any/absent | reject, no mutation |
    #[tokio::test]
    async fn update_admission_uses_only_durable_session_status() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(
            repo.as_ref(),
            persisted("update-idle", false, false, "idle"),
        )
        .await;
        create(
            repo.as_ref(),
            persisted("update-running", false, false, "running"),
        )
        .await;
        create(
            repo.as_ref(),
            persisted("update-terminal", false, false, "terminated"),
        )
        .await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let command = |title: &str| SessionUpdateCommand {
            title: Some(Some(title.into())),
            metadata: None,
            tools: None,
            mcp_candidates: None,
            idempotency_key: None,
            request_fingerprint: awaken_session_contract::stable_fingerprint(&title),
            if_match: None,
        };

        let updated = app
            .update_session("update-idle", command("accepted"))
            .await
            .expect("U1");
        assert_eq!(updated.session.title.as_deref(), Some("accepted"), "U1");

        for (rule, id) in [("U2", "update-running"), ("U3", "update-terminal")] {
            let before = repo.get(id).await.expect(rule);
            assert!(
                matches!(
                    app.update_session(id, command("rejected")).await,
                    Err(SessionUpdateError::NotIdle)
                ),
                "{rule}"
            );
            assert_eq!(repo.get(id).await.expect(rule), before, "{rule}");
        }
    }

    /// Cause/effect graph: C1 the Runtime presents the exact durable realization
    /// lease; C2 the binding is new or an idempotent replay; C3 the same epoch
    /// has been renewed monotonically; C4 a replacement owner/epoch has fenced
    /// the Runtime. C1 permits the ordinary root CAS; C3 preserves in-flight
    /// work admitted before renewal; C4 rejects a binding change while an equal
    /// durable binding remains a side-effect-free recovery replay.
    ///
    /// | Rule | Asserted lease | Binding | Effect |
    /// |---|---|---|---|
    /// | B1 | exact | new | persist once |
    /// | B2 | exact | equal | idempotent success |
    /// | B3 | shorter same-epoch assertion under live renewal | new/equal | authorized |
    /// | B4 | stale owner/epoch | equal | idempotent success, no mutation |
    /// | B5 | stale owner/epoch | different | fenced, no mutation |
    /// | B6 | aggregate/assertion both absent | new/equal | legacy CAS path |
    /// | B7 | current lease expired | equal | idempotent success, no mutation |
    /// | B8 | current lease expired | different | fenced, no mutation |
    #[tokio::test]
    async fn environment_binding_persistence_is_fenced_by_exact_realization() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let mut session = persisted("binding-fence", false, false, "idle");
        let current = awaken_session_contract::SessionRealizationLease {
            owner: "runtime-a".into(),
            runtime_incarnation: "runtime-a/boot-1".into(),
            epoch: 3,
            expires_at_unix_ms: u64::MAX,
        };
        session.realization = Some(current.clone());
        create(repo.as_ref(), session).await;
        let sink = RepositoryEnvironmentBindingSink::new(repo.clone());

        sink.persist("binding-fence", "sandbox-a", Some(&current))
            .await
            .expect("B1");
        sink.persist("binding-fence", "sandbox-a", Some(&current))
            .await
            .expect("B2");
        let mut renewed_session = persisted("binding-renewed", false, false, "idle");
        renewed_session.realization = Some(awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: u64::MAX,
            ..current.clone()
        });
        create(repo.as_ref(), renewed_session).await;
        let admitted_before_renewal = awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: u64::MAX - 1,
            ..current.clone()
        };
        sink.persist(
            "binding-renewed",
            "sandbox-renewed",
            Some(&admitted_before_renewal),
        )
        .await
        .expect("B3 monotonic renewal authorizes admitted work");
        create(
            repo.as_ref(),
            persisted("binding-unassigned", false, false, "idle"),
        )
        .await;
        sink.persist("binding-unassigned", "sandbox-legacy", None)
            .await
            .expect("B6");
        let stale = awaken_session_contract::SessionRealizationLease {
            owner: "runtime-b".into(),
            runtime_incarnation: "runtime-b/boot-1".into(),
            epoch: 4,
            expires_at_unix_ms: u64::MAX,
        };
        let revision_before_stale_replay = repo.get("binding-fence").await.expect("B4").revision;
        sink.persist("binding-fence", "sandbox-a", Some(&stale))
            .await
            .expect("B4 stale replay of an equal binding is idempotent");
        assert_eq!(
            repo.get("binding-fence").await.expect("B4").revision,
            revision_before_stale_replay,
            "B4 must not write"
        );
        let error = sink
            .persist("binding-fence", "sandbox-b", Some(&stale))
            .await
            .expect_err("B5 stale binding replacement is fenced");
        assert_eq!(error.code, "session_realization_stale", "B5");
        let expired = awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: 0,
            ..current.clone()
        };
        let mut expired_session = repo.get("binding-fence").await.expect("B6 fixture");
        expired_session.realization = Some(expired.clone());
        let expected_revision = expired_session.revision;
        let payload = awaken_session_contract::SessionMutationPayload::Replace(expired_session);
        let payload_hash = payload.stable_hash();
        assert!(matches!(
            repo.commit_mutation(
                "workspace",
                awaken_session_contract::SessionMutation {
                    expected_revision,
                    idempotency: awaken_session_contract::IdempotencyRecord {
                        key: "binding-fence:expire".into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await
            .expect("B7 fixture mutation"),
            awaken_session_contract::SessionMutationResult::Applied { .. }
        ));
        sink.persist("binding-fence", "sandbox-a", Some(&expired))
            .await
            .expect("B7 expired-lease replay of an equal binding is idempotent");
        assert_eq!(
            sink.persist("binding-fence", "sandbox-b", Some(&expired))
                .await
                .expect_err("B8")
                .code,
            "session_realization_stale",
            "B8"
        );
        assert_eq!(
            repo.get("binding-fence")
                .await
                .and_then(|session| session.environment.binding().map(str::to_owned))
                .as_deref(),
            Some("sandbox-a"),
            "B4/B5/B7/B8"
        );
    }

    /// In-flight renewal cause/effect graph: C1 one MCP generation is Realizing
    /// under an admitted lease; C2 the same owner/incarnation/epoch is durably
    /// extended before its Stage receipt returns. E1 accepts the old exact
    /// receipt under the monotonic lease fence; E2 activates the durable
    /// generation but returns one renewal Stage; E3 the renewal receipt leads
    /// to Publish with the extended generation. Different owner/epoch is A2 in
    /// the contract table; conflicting receipt bindings remain rejected by the
    /// canonical receipt verification gate.
    ///
    /// | Rule | C1 | C2 | First effect | Next effect |
    /// |---|---|---|---|---|
    /// | F1 | yes | yes | E1 + E2, no publish | E3 |
    #[tokio::test]
    async fn activation_restages_a_generation_renewed_while_its_stage_was_in_flight() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let admitted_expiry = now + 60_000;
        let renewed_expiry = now + 120_000;
        let asserted_lease = awaken_session_contract::SessionRealizationLease {
            owner: "runtime-a".into(),
            runtime_incarnation: "runtime-a/boot-1".into(),
            epoch: 7,
            expires_at_unix_ms: admitted_expiry,
        };
        let current_lease = awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: renewed_expiry,
            ..asserted_lease.clone()
        };
        let mut session = persisted("in-flight-renewal", false, false, "activating");
        session.realization = Some(current_lease.clone());
        session.mcp = awaken_session_contract::SessionMcpAttachmentSet::from_initial(
            vec![awaken_session_contract::McpAttachmentDraft {
                name: "browser".into(),
                target: McpTarget::parse_http("https://browser.example.test/mcp").unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
        let attachment_id = session.mcp.attachments[0].attachment_id.clone();
        session
            .mcp
            .claim_realization(
                &attachment_id,
                awaken_session_contract::McpGeneration(1),
                awaken_session_contract::McpRealizationClaim {
                    realization_id: "realization-1".into(),
                    runtime_incarnation: asserted_lease.runtime_incarnation.clone(),
                    lease_epoch: asserted_lease.epoch,
                    lease_expires_at_unix_ms: admitted_expiry,
                    stage_idempotency_key: "stage-1".into(),
                },
            )
            .unwrap();
        let admitted_request = projection::stage_mcp_request(
            "workspace",
            &session.session_id,
            &session.mcp.attachments[0],
        )
        .unwrap();
        create(repo.as_ref(), session).await;
        let app = application(repo, Arc::new(RecordingEnvironmentSource::default()));
        let admitted_receipt = awaken_session_contract::McpRealizationReceipt {
            receipt_fingerprint: admitted_request.fingerprint(),
            generation: admitted_request.generation.clone(),
            realization_id: admitted_request.realization_id.clone(),
            selected_plaintext_holder: admitted_request.selected_plaintext_holder.clone(),
            actual_realization_kind: None,
        };

        let directive =
            awaken_session_contract::SessionRealizationControl::activate_session_realization(
                &app,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "in-flight-renewal".into(),
                    lease: asserted_lease,
                    prepared_resource_revision: None,
                    mcp_receipts: vec![admitted_receipt],
                },
            )
            .await
            .expect("F1/E1");
        let awaken_session_contract::SessionRealizationAction::Stage {
            prepare_session,
            mut mcp_stages,
        } = directive.action
        else {
            panic!("F1/E2 must restage the extended exact fence before publish")
        };
        assert!(!prepare_session, "F1/E2 does not recreate the Environment");
        let renewed_request = mcp_stages.remove(0);
        assert_eq!(
            renewed_request.renewal_binding_fingerprint(),
            admitted_request.renewal_binding_fingerprint(),
            "F1/E2"
        );
        assert_eq!(
            renewed_request.generation.lease_expires_at_unix_ms, renewed_expiry,
            "F1/E2"
        );
        let renewed_receipt = awaken_session_contract::McpRealizationReceipt {
            receipt_fingerprint: renewed_request.fingerprint(),
            generation: renewed_request.generation.clone(),
            realization_id: renewed_request.realization_id,
            selected_plaintext_holder: renewed_request.selected_plaintext_holder,
            actual_realization_kind: None,
        };
        let directive =
            awaken_session_contract::SessionRealizationControl::activate_session_realization(
                &app,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "in-flight-renewal".into(),
                    lease: current_lease,
                    prepared_resource_revision: None,
                    mcp_receipts: vec![renewed_receipt],
                },
            )
            .await
            .expect("F1/E3");
        let awaken_session_contract::SessionRealizationAction::Publish { publish, .. } =
            directive.action
        else {
            panic!("F1/E3 must publish after exact renewal restage")
        };
        assert_eq!(publish, vec![renewed_request.generation], "F1/E3");
    }

    #[test]
    fn lifecycle_supervisor_claim_is_one_shot() {
        // Cause/effect decision table: C1=unclaimed fence, C2=already claimed.
        // R1 C1 -> E1 first caller becomes owner; R2 C2 -> E2 every later caller
        // is rejected. This proves moving the fence out of the protocol adapter
        // cannot start parallel Session lifecycle supervisors.
        let fence = AtomicBool::new(false);
        assert!(claim_once(&fence), "R1");
        assert!(!claim_once(&fence), "R2");
    }

    #[test]
    fn native_only_lazy_provisioning_decision_table() {
        // Causes: C1 eager policy; C2 lazy policy; C3 implicit/native backend;
        // C4 ACP/A2A backend. Effects: E1 accept; E2 reject before Session
        // realization. Environment policy tests own disabled/exact-version rules.
        //
        // | Rule | policy | runtime | effect |
        // | R1 | eager | any | accept |
        // | R2 | lazy | implicit/native | accept |
        // | R3 | lazy | ACP/A2A | reject |
        use SandboxProvisioning::{Eager, OnToolUse};
        for (case, provisioning, runtime, accepted) in [
            ("R1 eager native", Eager, None, true),
            ("R1 eager ACP", Eager, Some("acp:claude"), true),
            ("R2 lazy implicit native", OnToolUse, None, true),
            ("R2 lazy explicit native", OnToolUse, Some("awaken"), true),
            ("R2 lazy genai native", OnToolUse, Some("genai"), true),
            (
                "R2 lazy custom native",
                OnToolUse,
                Some("provider-native"),
                true,
            ),
            ("R3 lazy ACP", OnToolUse, Some("acp:claude"), false),
            (
                "R3 lazy A2A",
                OnToolUse,
                Some("a2a:https://agent.example"),
                false,
            ),
        ] {
            assert_eq!(
                validate_sandbox_provisioning_runtime(provisioning, runtime).is_ok(),
                accepted,
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn durable_session_truth_owns_one_work_projection_path() {
        // Cause/effect graph: C1 frozen Environment is self-hosted; C2 Session
        // has no Application-owned execution; C3 Session is nonterminal; C4 the
        // projection command is replayed. Effects: E1 only C1+C2+C3 dispatches;
        // E2 replay uses the same idempotent port and creates no second identity.
        //
        // | Rule | self-hosted | application | terminal | replay | effect |
        // | R1 | yes | no | no | no | project one |
        // | R2 | yes | no | no | yes | retain one |
        // | R3 | no | no | no | any | skip |
        // | R4 | yes | yes | no | any | skip |
        // | R5 | yes | no | yes | any | skip |
        let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository");
        create(&repo, persisted("external", true, false, "idle")).await;
        create(&repo, persisted("local", false, false, "idle")).await;
        create(&repo, persisted("application", true, true, "idle")).await;
        create(&repo, persisted("terminal", true, false, "terminated")).await;
        let environments = RecordingEnvironmentSource::default();

        let first = reconcile_work_dispatches(&repo, &environments).await;
        assert_eq!(first.settled, 1, "R1/R3/R4/R5");
        assert!(first.failures.is_empty());
        assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R1");

        let replay = reconcile_work_dispatches(&repo, &environments).await;
        assert_eq!(replay.settled, 1, "R2");
        assert!(replay.failures.is_empty());
        assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R2");
    }

    #[tokio::test]
    async fn work_dispatch_reconciliation_isolates_each_session_failure() {
        // Work-projection FMECA decision table. Causes: C1 a durable Session needs
        // external work; C2 its queue write succeeds; C3 a sibling queue write
        // fails; C4 an unrelated Session needs no dispatch. Effects: E1 every
        // eligible Session is attempted; E2 successes settle independently; E3
        // failures retain exact Session/Environment diagnostics for later retry;
        // E4 ineligible Sessions cause no side effect. Rules: W1 C1+C2=>E1+E2;
        // W2 C1+C3=>E1+E3 without aborting W1; W3 C4=>E4.
        let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository");
        create(&repo, persisted("external-failed", true, false, "idle")).await;
        create(&repo, persisted("external-settled", true, false, "idle")).await;
        create(&repo, persisted("local-skip", false, false, "idle")).await;
        let environments = RecordingEnvironmentSource::default();
        environments.fail_for("external-failed");

        let report = reconcile_work_dispatches(&repo, &environments).await;
        assert_eq!(report.settled, 1, "W1/W2");
        assert_eq!(
            environments
                .dispatched
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            ["external-settled".to_string()],
            "W1/W3"
        );
        assert_eq!(report.failures.len(), 1, "W2");
        assert_eq!(report.failures[0].session_id, "external-failed", "W2");
        assert_eq!(report.failures[0].environment_id, "env-worker", "W2");
        assert!(
            report.failures[0].message.contains("injected failure"),
            "W2 preserves the retry diagnostic"
        );
    }
}
