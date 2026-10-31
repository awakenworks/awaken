//! One explicit test composition around the production Session Root and CAS.

use super::compile_complete_test_session_projection;
use awaken_session_contract::SessionRepositoryReads;
use awaken_time::WallClockExt as _;
use std::sync::Arc;

mod environment_ports;

/// One explicit test composition over the existing SQLite Session authority.
/// Select it before the Host is built; no Host-id registry or read-side seeding
/// can substitute for this owner. The production sink remains the only admit/
/// binding writer, including first-effect CAS and immutable receipt replay.
pub(crate) struct ManagedSessionRootFixture {
    pub(super) repository: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    persistence: std::sync::Mutex<Option<awaken_session_application::SessionPersistence>>,
    pub(super) binding_sink:
        Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    application: std::sync::OnceLock<Arc<awaken_session_application::SessionApplication>>,
    environments: Arc<environment_ports::UnusedEnvironmentPorts>,
    clock: Arc<dyn awaken_time::WallClock>,
    pub(super) incarnation: String,
}

impl ManagedSessionRootFixture {
    pub(crate) async fn new() -> Self {
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let clock = awaken_time::system_clock();
        let persistence =
            awaken_session_application::SessionPersistence::new(repository.clone(), clock.clone());
        Self {
            repository,
            binding_sink: persistence.environment_binding_sink(),
            persistence: std::sync::Mutex::new(Some(persistence)),
            application: std::sync::OnceLock::new(),
            environments: Arc::default(),
            clock,
            incarnation: format!("runtime-host-fixture-{}", uuid::Uuid::new_v4()),
        }
    }

    /// Sessions whose Work projection the application requested.
    pub(crate) fn enqueued_work(&self) -> Vec<awaken_session_types::SessionId> {
        self.environments.enqueued()
    }

    pub(crate) fn configure(
        &self,
        dependencies: crate::SessionExecutorBuilder,
    ) -> crate::SessionExecutorBuilder {
        dependencies.with_environment_binding_sink(self.binding_sink.clone())
    }

    pub(super) async fn install_complete_test_session(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
        init: awaken_session_configuration_contract::SessionInit,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        self.install_projection(
            managed,
            thread,
            compile_complete_test_session_projection(managed, init),
        )
        .await
    }

    /// Admit only an initial declaration; Store replay supplies all current
    /// Environment/origin/assignment facts for another Host over this same
    /// Actor. Resident input cannot be used to construct a fresh Root, and a
    /// different assignment remains an error until the original command changes it.
    pub(crate) async fn install_projection(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_contract::SessionRepository as _;
        use awaken_session_contract::{
            PersistedSession, SessionCreateResult, SessionMutationPayload, SessionResourceState,
        };
        if projection.resource_revision != 0
            || !matches!(
                projection.environment,
                awaken_session_contract::SessionEnvironmentState::Unmaterialized
            )
            || projection
                .environment_materialization
                .as_deref()
                .is_some_and(
                    awaken_session_contract::SessionEnvironmentMaterialization::has_admitted,
                )
        {
            return Err(awaken_session_types::RunError::bad_request(
                "fresh Root fixture requires an unmaterialized initial declaration; recovery reads the existing Root and updates require its original commands",
            ));
        }
        let session_id = awaken_session_types::SessionId::from(thread);
        let mut resources = SessionResourceState::default();
        resources
            .prepare(&session_id, projection.resources.clone())
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let fresh = PersistedSession::frozen_with_budget(
            session_id,
            projection.baseline.clone(),
            resources,
            Default::default(),
            None,
            Default::default(),
            projection.tools.clone(),
            Default::default(),
        );
        let identity = SessionMutationPayload::default_create_idempotency(&fresh);
        // Explicit Create is before Runtime installation/provider I/O. An
        // exact replay returns the real current Root; changed inputs or an old
        // occupied identity are rejected by the Store, never replaced/seeded.
        let created = self
            .repository
            .create(&projection.workspace_id, fresh, identity, Vec::new())
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let mut root = match created {
            SessionCreateResult::Applied(root) | SessionCreateResult::Replayed(root) => root,
        };
        let lease = crate::session_resource::SessionRealizationLease {
            owner: "runtime-host-test".into(),
            runtime_incarnation: self.incarnation.clone(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        if root.realization.is_none() {
            root = self
                .commit_assignment(&projection.workspace_id, root, lease)
                .await?;
        }
        self.install_committed_projection(managed, thread, projection, root)
            .await
    }

    /// An explicit test reassignment after the caller revoked the old Hand.
    /// Closing a generation is not a physical-domain completion proof; no read
    /// method calls this command, and it never changes the original Create fact.
    pub(crate) async fn advance_assignment(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        let id = awaken_session_types::SessionId::from(thread);
        let root = self
            .repository
            .get(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let owner = self
            .repository
            .owner(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let current = root.realization.as_ref().ok_or_else(|| {
            awaken_session_types::RunError::unavailable("fixture assignment is absent")
        })?;
        self.check_actor(current)?;
        let (resource_revision, resources) = root.resources.desired_generation();
        let frozen = managed
            .run_assembly
            .installed_sessions
            .realizations()
            .frozen(thread)
            .expect("installed Sessions");
        let projection = awaken_session_realization_contract::FrozenSessionProjection {
            workspace_id: owner,
            revision: root.revision,
            baseline: root.frozen_baseline().cloned().ok_or_else(|| {
                awaken_session_types::RunError::unavailable("fixture Root has no frozen baseline")
            })?,
            agent_publication: frozen.published_snapshot.clone(),
            environment: root.environment.clone(),
            environment_materialization: root.environment_materialization().cloned().map(Box::new),
            resource_revision,
            resources: resources.clone(),
            previous_resource_manifest: None,
            tools: root.tools.clone(),
            mcp: root.mcp.attachments.clone(),
            request_context: frozen.request_context.clone(),
        };
        self.reassign_closed(
            managed,
            managed,
            thread,
            projection,
            ("runtime-host-test", &self.incarnation),
        )
        .await
    }

    /// Explicit fixture command for a cold consumer after exact old closure.
    /// It preserves the original Create identity; the supplied new Actor owns
    /// only the next test assignment, never a physical-domain takeover proof.
    pub(crate) async fn reassign_closed(
        &self,
        source: &Arc<crate::ExecutorComponents>,
        target: &Arc<crate::ExecutorComponents>,
        thread: &str,
        template: awaken_session_realization_contract::FrozenSessionProjection,
        target_actor: (&str, &str),
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        let id = awaken_session_types::SessionId::from(thread);
        let owner = self
            .repository
            .owner(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        if owner != template.workspace_id || target_actor.0.is_empty() || target_actor.1.is_empty()
        {
            return Err(awaken_session_types::RunError::bad_request(
                "fixture reassignment requires the original Workspace and an explicit Actor",
            ));
        }
        let root = self
            .repository
            .get(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let current = root.realization.as_ref().ok_or_else(|| {
            awaken_session_types::RunError::unavailable("fixture assignment is absent")
        })?;
        let closed = source.run_assembly.installed_sessions.realizations().read_realization(thread, |r| {
            matches!(&r.execution().authority, crate::realization_state::SessionRealizationAuthority::Closed(Some(old)) if old == current)
        }).expect("installed Sessions").unwrap_or(false);
        if !closed {
            return Err(awaken_session_types::RunError::unavailable(
                "fixture advance requires the explicitly revoked exact generation",
            ));
        }
        let epoch = awaken_lease_contract::fence::Epoch::from_persisted(current.epoch)
            .and_then(awaken_lease_contract::fence::Epoch::next)
            .ok_or_else(|| {
                awaken_session_types::RunError::unavailable("fixture epoch is exhausted")
            })?;
        let mut next = current.clone();
        next.owner = target_actor.0.into();
        next.runtime_incarnation = target_actor.1.into();
        next.epoch = epoch.get();
        let root = self.commit_assignment(&owner, root, next.clone()).await?;
        self.install_projection_for_assignment(target, thread, template, root, &next)
            .await
    }

    fn check_actor(
        &self,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        if lease.owner != "runtime-host-test" || lease.runtime_incarnation != self.incarnation {
            return Err(awaken_session_types::RunError::unavailable(
                "fixture does not own the Root's current Actor",
            ));
        }
        Ok(())
    }

    async fn commit_assignment(
        &self,
        owner: &str,
        root: awaken_session_contract::PersistedSession,
        lease: crate::session_resource::SessionRealizationLease,
    ) -> Result<awaken_session_contract::PersistedSession, awaken_session_types::RunError> {
        use awaken_session_contract::SessionAggregate;
        let mut aggregate = SessionAggregate::try_from(root)
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        aggregate
            .assignment()
            .claim(lease, false)
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        self.commit_aggregate(owner, aggregate).await
    }

    /// The original selected Persistence is consumed once by the actual
    /// application. Resource fixtures use its existing activation and manifest
    /// commands, not another reducer or receipt writer.
    fn application(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
    ) -> Arc<awaken_session_application::SessionApplication> {
        self.application_with_configuration(managed, Default::default())
    }

    fn application_with_configuration(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        configuration: awaken_session_application::SessionApplicationConfiguration,
    ) -> Arc<awaken_session_application::SessionApplication> {
        self.application
            .get_or_init(|| {
                // These private tests install their exact frozen projection below.
                // Environment/Work composition is exercised by ScenarioHost.
                // Any other use of an Environment role fails here; a requested
                // Work enqueue is only recorded.
                let environments = self.environments.clone();
                let runtime = managed.session_execution_roles();
                Arc::new(
                    awaken_session_application::SessionApplicationBuilder::new(
                        runtime,
                        self.persistence
                            .lock()
                            .unwrap()
                            .take()
                            .expect("one selected Persistence"),
                        environments,
                        awaken_session_application::SessionExecutionAdmission::fixture(),
                        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                    )
                    .configuration(configuration)
                    .build()
                    .expect("valid fixture Session application"),
                )
            })
            .clone()
    }

    /// Settle actual Stage through the original Control receiver, then issue
    /// the existing manifest command. Unchanged desired input stays unchanged.
    pub(crate) async fn prepare_resource_update(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        desired: crate::session_resource::ResolvedSessionResources,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        let projection = self
            .settle_resource_stage(managed, thread, projection)
            .await?;
        let id = awaken_session_types::SessionId::from(thread);
        let fingerprint = awaken_agent_contract::stable_fingerprint(&desired);
        self.application(managed)
            .resource_manifests()
            .replace_session_resource_manifest(
                &id,
                awaken_session_application::ReplaceSessionResourceManifest {
                    resources: desired,
                    expected_resource_revision: Some(projection.resource_revision),
                    idempotency_key: Some(format!("fixture-resource:{thread}:{fingerprint}")),
                    request_fingerprint: fingerprint,
                },
            )
            .await
            .map_err(|error| awaken_session_types::RunError::unavailable(error.to_string()))?;
        let root = self
            .repository
            .get(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let installed = self
            .install_committed_projection(managed, thread, projection, root.clone())
            .await?;
        let lease = root.realization.as_ref().ok_or_else(|| {
            awaken_session_types::RunError::unavailable("fixture assignment is absent")
        })?;
        let transition = installed.resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )?;
        managed
            .realization_executor
            .apply_session_inputs_with_context(thread, &transition, None, Some(lease))
            .await?;
        self.settle_resource_stage(managed, thread, installed.clone())
            .await?;
        Ok(installed)
    }

    /// The Runtime's exact prepared manifest is the Stage result for this
    /// bounded test. The production Control validates and commits that result;
    /// an Environment binding or staged-only desired manifest is insufficient.
    pub(crate) async fn settle_resource_stage(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_realization_contract::SessionRealizationControl as _;
        let id = awaken_session_types::SessionId::from(thread);
        let root = self
            .repository
            .get(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        let lease = root.realization.as_ref().ok_or_else(|| {
            awaken_session_types::RunError::unavailable("fixture assignment is absent")
        })?;
        self.check_actor(lease)?;
        let (revision, resources) = root.resources.desired_generation();
        let prepared = crate::session_resource::SessionResourceManifest::at_revision(
            projection.workspace_id.clone(),
            revision,
            resources.clone(),
        );
        let exact_stage = managed
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| {
                r.staging().manifest.as_ref() == Some(&prepared)
                    && r.execution().authority.lease() == Some(lease)
                    && r.execution().authority.remaining_for(lease).is_some()
            })
            .expect("installed Sessions")
            .unwrap_or(false);
        if !exact_stage {
            return Err(awaken_session_types::RunError::unavailable(
                "fixture Resource settlement requires the exact current physically prepared generation",
            ));
        }
        self.application(managed)
            .realizations()
            .activate_session_realization(
                awaken_session_realization_contract::ActivateSessionRealization {
                    session_id: id,
                    lease: lease.clone(),
                    prepared_resource_revision: Some(revision),
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .map_err(|error| awaken_session_types::RunError::unavailable(error.to_string()))?;
        self.read_projection(thread, &projection).await
    }

    async fn commit_aggregate(
        &self,
        owner: &str,
        mut aggregate: awaken_session_contract::SessionAggregate,
    ) -> Result<awaken_session_contract::PersistedSession, awaken_session_types::RunError> {
        use awaken_session_contract::SessionRepository as _;
        use awaken_session_contract::{
            IdempotencyRecord, SessionMutationPayload, SessionMutationResult,
        };
        let id = aggregate.session_id.clone();
        let payload_hash =
            SessionMutationPayload::Replace(aggregate.snapshot().clone()).stable_hash();
        let mutation = aggregate.compile_mutation(
            owner,
            IdempotencyRecord {
                key: format!("fixture:root:{id}:{}:{payload_hash}", aggregate.revision.0),
                payload_hash,
            },
            Vec::new(),
            self.clock.now_ms(),
        );
        match self
            .repository
            .commit(owner, mutation)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?
        {
            SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {}
            other => {
                return Err(awaken_session_types::RunError::unavailable(format!(
                    "fixture Root command did not commit: {other:?}"
                )));
            }
        }
        self.repository
            .get(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))
    }

    pub(super) async fn install_committed_projection(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        root: awaken_session_contract::PersistedSession,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        let lease = root
            .realization
            .as_ref()
            .ok_or_else(|| {
                awaken_session_types::RunError::unavailable("fixture assignment is absent")
            })?
            .clone();
        self.check_actor(&lease)?;
        self.install_projection_for_assignment(managed, thread, projection, root, &lease)
            .await
    }

    /// Read only the existing Root; never Create, claim or install on recovery.
    pub(crate) async fn read_projection(
        &self,
        thread: &str,
        template: &awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        let id = awaken_session_types::SessionId::from(thread);
        let owner = self
            .repository
            .owner(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        if owner != template.workspace_id {
            return Err(awaken_session_types::RunError::bad_request(
                "fixture template does not name the original Workspace",
            ));
        }
        let root = self
            .repository
            .get(&id)
            .await
            .map_err(|error| awaken_session_types::RunError::bad_request(error.to_string()))?;
        Self::project_root(template.clone(), &root)
    }

    fn project_root(
        mut projection: awaken_session_realization_contract::FrozenSessionProjection,
        root: &awaken_session_contract::PersistedSession,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        projection.baseline = root.frozen_baseline().cloned().ok_or_else(|| {
            awaken_session_types::RunError::unavailable("fixture Root has no frozen baseline")
        })?;
        let (resource_revision, desired) = root.resources.desired_generation();
        let (previous_revision, previous) = root.resources.active_generation();
        projection.revision = root.revision;
        projection.environment = root.environment.clone();
        projection.environment_materialization =
            root.environment_materialization().cloned().map(Box::new);
        projection.resource_revision = resource_revision;
        projection.resources = desired.clone();
        projection.previous_resource_manifest = Some(
            crate::session_resource::SessionResourceManifest::at_revision(
                projection.workspace_id.clone(),
                previous_revision,
                previous.clone(),
            ),
        );
        projection.tools = root.tools.clone();
        projection.mcp = root.mcp.attachments.clone();
        Ok(projection)
    }

    async fn install_projection_for_assignment(
        &self,
        managed: &Arc<crate::ExecutorComponents>,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        root: awaken_session_contract::PersistedSession,
        expected: &crate::session_resource::SessionRealizationLease,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        if root.realization.as_ref() != Some(expected) {
            return Err(awaken_session_types::RunError::unavailable(
                "fixture installation differs from the explicitly committed Actor",
            ));
        }
        let projection = Self::project_root(projection, &root)?;
        // Cold Resource requirements are staged by Dispatch; the following
        // lease-only mode validates those same inputs and cannot cold-stage or
        // adopt. Both modes carry the same Store-read snapshot, and the tested
        // Run remains the only owner of new physical Environment I/O.
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*managed.run_assembly,
            thread,
            projection.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await?;
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*managed.run_assembly,
            thread,
            projection.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease:
                    awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                        expected.clone(),
                        self.clock.now_ms(),
                    )
                    .map_err(|error| {
                        awaken_session_types::RunError::bad_request(error.to_string())
                    })?
                    .observed(
                        awaken_authority_clock::authority_proof_started().map_err(|error| {
                            awaken_session_types::RunError::unavailable(error.to_string())
                        })?,
                    ),
                // Fixture installation establishes Root/lease inputs only.
                // The tested Run owns the first physical effect and its CAS.
                prepare_session: false,
            },
        )
        .await?;
        Ok(projection)
    }
}

#[cfg(test)]
mod retry_hint_tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{
        available_local_environment_binding, eager_environment,
        empty_frozen_projection_for_snapshot_at_placement, managed_test_host, test_activation,
    };
    use awaken_session_environment::SandboxEnvironmentProvider;
    use awaken_session_environment::test_support::{
        FakeContainerProvider, FakeHandExecutorFactory,
    };
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn idle_no_create_retirement_uses_real_coordination_reads_without_terminal_cleanup() {
        // Cause/effect graph: C1 modern explicit no-Create root, Idle, exact
        // closed Worker assignment and no retained Resources/activities; C2
        // production CoordinatedThreads returns NotTerminal for its terminal
        // source-Dispatch port; C3 the same complete Session read is used for
        // retention subjects and qualification. R1 C1+C2+C3 -> retained, still
        // occupied, no provider subject or root write. A scripted Settled reply
        // for a live root violates the real port contract and hid the defect.
        // Worker-domain physical/Work/Registry gates remain separately required;
        // the existing Coordinator decision table covers each refusal.
        use awaken_session_contract::SessionRepository as _;
        use awaken_session_realization_contract::WorkerSourceRetentionSubjects as _;
        let roots = ManagedSessionRootFixture::new().await;
        let host = roots
            .configure(
                crate::SessionExecutorBuilder::ephemeral(
                    Arc::new(crate::no_model::NoModelConfiguredExecutor),
                    "stub",
                )
                .await,
            )
            .build()
            .unwrap();
        let identity = awaken_worker_contract::WorkerIdentity::new("idle-worker", "boot", 1);
        let mut baseline = super::super::remote_terminal_cleanup_projection().baseline;
        baseline.environment.self_hosted = false;
        baseline.environment.sandbox_provisioning =
            awaken_session_configuration_contract::SandboxProvisioning::OnToolUse;
        let mut root = awaken_session_contract::PersistedSession::frozen_with_budget(
            "live-no-create-composition",
            baseline,
            Default::default(),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        root.execution = awaken_session_contract::SessionExecutionState::Idle;
        root.realization = Some(
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: identity.worker_id.clone(),
                runtime_incarnation: identity.lease_owner(),
                epoch: 7,
                expires_at_unix_ms: 0,
            },
        );
        let id = root.session_id.clone();
        roots
            .repository
            .create(
                "workspace",
                root,
                awaken_session_contract::IdempotencyRecord {
                    key: "create-live-no-create-composition".into(),
                    payload_hash: "same-create".into(),
                },
                Vec::new(),
            )
            .await
            .unwrap();
        let before = roots.repository.get(&id).await.unwrap();
        let application = roots.application(&host);
        let source = host
            .session_execution_roles()
            .coordination_reads
            .source_dispatch_observation(&before)
            .await;
        assert_eq!(
            source,
            awaken_session_execution_contract::SessionSourceDispatchObservation::NotProven {
                reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::NotTerminal,
            },
            "the real port is deliberately terminal-only"
        );
        let occupancy = application.worker_occupancy();
        assert!(
            occupancy
                .source_retention_subjects(&identity)
                .await
                .unwrap()
                .is_empty()
        );
        let observed = occupancy
            .observe_worker_realizations(
                awaken_session_application::WorkerRealizationRead::Retirement {
                    identity: &identity,
                    reported: &Default::default(),
                },
            )
            .await
            .unwrap();
        assert!(
            observed.occupied && observed.sources_retained,
            "live no-Create retirement cannot require terminal cleanup or its frozen Dispatch scope"
        );
        assert_eq!(roots.repository.get(&id).await.unwrap(), before);
    }

    #[tokio::test]
    async fn local_install_preserves_the_real_provider_hint_before_neutral_error_projection() {
        // L3-L6 use the actual provider adapter, R adoption, RunAssembly install,
        // LocalProjectionSynchronizer, phase driver and Root failure CAS.
        // C1 the same Root has a committed first Create/binding; C2 provider
        // observation has a live / absent / elapsed provisioning deadline, or
        // returns a capacity-typed observation error without a hint.
        // L3 live => original Defer gets Some(wait<=6s); L4/L5 absent/elapsed
        // => bounded Retry and None; L6 failed observation => Retry/None with
        // ObservationIndeterminate, never proof of a Create capacity refusal. Every row
        // preserves the exact driver hint at the original Defer, observes
        // once, creates nothing
        // beyond its original Create, adopts/disposes nothing, retains the
        // original binding and cannot install a Session context. The frozen
        // provider observation deliberately uses another wall-time epoch:
        // wrapping it must not resample a host clock or classify it again.
        use awaken_provisioning_contract::{
            ProvisioningDeadline, SandboxError, SandboxObservation,
        };
        use awaken_session_types::{SessionId, SessionRuntimePlacement};
        for (rule, observation, hinted, deferred) in [
            (
                "L3",
                Ok(SandboxObservation::Provisioning {
                    deadline: Some(ProvisioningDeadline::new(10_000, 4_000)),
                }),
                true,
                true,
            ),
            (
                "L4",
                Ok(SandboxObservation::Provisioning { deadline: None }),
                false,
                false,
            ),
            (
                "L5",
                Ok(SandboxObservation::Provisioning {
                    deadline: Some(ProvisioningDeadline::new(10_000, 10_000)),
                }),
                false,
                false,
            ),
            (
                "L6",
                Err(SandboxError::capacity_unavailable("provider quota")),
                false,
                false,
            ),
        ] {
            let fake = Arc::new(FakeContainerProvider::default());
            let provider = SandboxEnvironmentProvider::container(
                fake.clone(),
                Vec::new(),
                Arc::new(FakeHandExecutorFactory),
                "awaken-sandbox",
            )
            .into_provider_roles();
            let roots = ManagedSessionRootFixture::new().await;
            let host = roots
                .configure(
                    crate::SessionExecutorBuilder::ephemeral(
                        Arc::new(crate::no_model::NoModelConfiguredExecutor),
                        "stub",
                    )
                    .await
                    .with_session_environment_provider(provider, None),
                )
                .build()
                .unwrap();
            let managed = managed_test_host(host.clone());
            // Each row has its own Provider, Host and Store. Use the original
            // container fixture's exact Session identity rather than forging a
            // binding for another Session, which correctly rejects before I/O.
            let thread = "session-container".to_string();
            let activation = test_activation(&thread, "initial-create");
            let mut projection = empty_frozen_projection_for_snapshot_at_placement(
                "workspace",
                eager_environment(),
                &activation.snapshot,
                SessionRuntimePlacement::Local,
            );
            projection.agent_publication = Some(activation.snapshot.clone());
            let initial = roots
                .install_projection(&managed, &thread, projection.clone())
                .await
                .unwrap();
            let binding = available_local_environment_binding(&host, &thread, &initial).await;
            *fake.observation.lock() = Some(observation);
            let app = roots.application_with_configuration(
                &managed,
                awaken_session_application::SessionApplicationConfiguration {
                    execution_placement:
                        awaken_session_application::SessionExecutionPlacement::LocalWorker,
                    local_realization_owner: "runtime-host-test".into(),
                    ..Default::default()
                },
            );
            let error = app
                .realizations()
                .realize_session(&SessionId::from(thread.clone()))
                .await
                .unwrap_err();
            let awaken_session_realization_contract::SessionRealizationDriveError::Effect(effect) =
                error
            else {
                panic!("{rule} expected original provider effect: {error}");
            };
            assert_eq!(
                fake.observations.lock().len(),
                1,
                "{rule} real provider observation entered; original effect: {effect:?}"
            );
            assert_eq!(
                effect.retry_after().is_some(),
                hinted,
                "{rule} original provider hint was not flattened through executor error: {effect:?}"
            );
            if let Some(wait) = effect.retry_after() {
                assert!(
                    wait <= std::time::Duration::from_secs(6),
                    "{rule} reporting only shortens the original hint"
                );
                assert_eq!(
                    effect.run_error().code,
                    awaken_session_realization_contract::ENVIRONMENT_PROVISIONING_CODE,
                    "{rule} original NotReady classification"
                );
            }
            if rule == "L6" {
                assert_eq!(
                    effect.run_error().code,
                    awaken_session_realization::realization_error::PendingState::ObservationIndeterminate.code(),
                    "L6 a failed read remains indeterminate regardless of its provider error variant"
                );
            }
            let driver_hint = effect.retry_after();
            let claim = awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(effect),
            );
            match (deferred, claim) {
                (true, awaken_dispatch_execution::Error::ResolutionNotReady(wait)) => assert_eq!(
                    wait.retry_after(),
                    driver_hint,
                    "{rule} original Defer receives exact driver hint"
                ),
                (false, awaken_dispatch_execution::Error::Execution(_)) => {}
                (_, other) => panic!("{rule} wrong original claim disposition: {other:?}"),
            }
            assert_eq!(
                fake.creates.load(Ordering::SeqCst),
                1,
                "{rule} only original Create"
            );
            assert_eq!(
                fake.adopts.load(Ordering::SeqCst),
                0,
                "{rule} no adoption effect"
            );
            assert_eq!(
                fake.disposals.load(Ordering::SeqCst),
                0,
                "{rule} no disposal"
            );
            let root = roots
                .repository
                .get(&SessionId::from(thread.clone()))
                .await
                .unwrap();
            assert_eq!(
                root.environment.binding(),
                Some(binding.as_str()),
                "{rule} original durable binding retained"
            );
            assert!(
                !root.terminal_cleanup.is_fenced(),
                "{rule} no cleanup success invented"
            );
            assert_eq!(
                host.run_assembly
                    .installed_sessions
                    .read(&thread, |installed| installed.run.context.is_none())
                    .unwrap(),
                Some(true),
                "{rule} failed adoption never installs context"
            );
            // M1-M3 adapter routing uses this SAME actual persisted Root and
            // original Host/ProcessDispatch, not another policy/Root fixture.
            // The canonical physical decision table remains in Dispatch's
            // preparation_tests. Here M1 actual trait -> canonical output;
            // M2 selected Arc -> accepted; M3 another real ProcessDispatch with
            // equal values/output -> original identity refusal, no added
            // provider effect or durable Root mutation. Existing App construction
            // probes separately own zero-call first-refusal for every installed Session.
            let effects = (
                fake.observations.lock().len(),
                fake.creates.load(Ordering::SeqCst),
                fake.adopts.load(Ordering::SeqCst),
                fake.disposals.load(Ordering::SeqCst),
            );
            assert_process_dispatch_placement_uses_the_selected_instance(&host, &root);
            assert_eq!(
                (
                    fake.observations.lock().len(),
                    fake.creates.load(Ordering::SeqCst),
                    fake.adopts.load(Ordering::SeqCst),
                    fake.disposals.load(Ordering::SeqCst),
                ),
                effects,
                "M3 selection checks add no provider effect"
            );
            assert_eq!(
                roots
                    .repository
                    .get(&SessionId::from(thread))
                    .await
                    .unwrap(),
                root,
                "M3 pure placement and identity checks cannot write the Root"
            );
        }
    }

    fn assert_process_dispatch_placement_uses_the_selected_instance(
        host: &Arc<crate::ExecutorComponents>,
        root: &awaken_session_contract::PersistedSession,
    ) {
        use awaken_session_realization_contract::SessionPreparationPlacement;
        let selected: Arc<dyn SessionPreparationPlacement> =
            host.dispatch().preparation_placement();
        let canonical =
            awaken_dispatch_execution::placement::session_preparation_requirements(root)
                .expect("M1 this real committed Root has a frozen baseline");
        assert_eq!(
            selected.requirements(root).unwrap(),
            canonical,
            "M1 real trait route"
        );
        let workers: Arc<dyn awaken_worker_contract::WorkerIncarnationSource> =
            Arc::new(awaken_worker_registry::MemoryWorkerDirectory::default());
        let feed = host
            .coordination()
            .dispatch_observation_source()
            .expect("M2 existing process dispatch feed");
        let admission = awaken_session_application::SessionExecutionAdmission::new(
            feed,
            workers.clone(),
            selected.clone(),
        );
        assert!(
            admission
                .verify_worker_admission(&workers, &selected)
                .is_ok(),
            "M2 selected Arc"
        );
        // Construct a real equal-value process using the original constructor,
        // Clock and CompletionSink. No pool/task/provider is started here.
        let foreign = Arc::new(awaken_dispatch_execution::ProcessDispatch::new(
            host.dispatch().topology().clone(),
            host.clock(),
            awaken_dispatch_execution::DispatchHint::new(Arc::new(
                awaken_dispatch_execution::LocalWakeSignal::new(),
            )),
        ));
        assert_eq!(
            foreign.topology(),
            host.dispatch().topology(),
            "M3 same topology values"
        );
        let foreign: Arc<dyn SessionPreparationPlacement> = foreign.preparation_placement();
        assert_eq!(
            foreign.requirements(root).unwrap(),
            canonical,
            "M3 same canonical output"
        );
        assert_eq!(
            admission.verify_worker_admission(&workers, &foreign),
            Err("Session preparation compiler differs from the canonical Host"),
            "M3 equal values cannot replace the frozen process instance"
        );
        assert!(
            admission
                .verify_worker_admission(&workers, &selected)
                .is_ok(),
            "M3 original retained"
        );
    }
}
