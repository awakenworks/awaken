//! One explicit test composition around the production Session Root and CAS.

use super::compile_complete_test_session_projection;
use awaken_time::WallClockExt as _;
use std::sync::Arc;

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
            clock,
            incarnation: format!("runtime-host-fixture-{}", uuid::Uuid::new_v4()),
        }
    }

    pub(crate) fn configure(
        &self,
        dependencies: crate::HostDependencies,
    ) -> crate::HostDependencies {
        dependencies.with_environment_binding_sink(self.binding_sink.clone())
    }

    pub(super) async fn install_complete_test_session(
        &self,
        managed: &crate::ManagedHost,
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
        managed: &crate::ManagedHost,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_repository_contract::SessionRepository as _;
        use awaken_session_state_contract::{
            PersistedSession, SessionCreateResult, SessionMutationPayload, SessionResourceState,
        };
        if projection.resource_revision != 0
            || !matches!(
                projection.environment,
                awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
            )
            || projection
                .environment_materialization
                .as_deref()
                .is_some_and(
                    awaken_session_state_contract::SessionEnvironmentMaterialization::has_admitted,
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
        managed: &crate::ManagedHost,
        thread: &str,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_repository_contract::SessionRepository as _;
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
            .host
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
        source: &crate::ManagedHost,
        target: &crate::ManagedHost,
        thread: &str,
        template: awaken_session_realization_contract::FrozenSessionProjection,
        target_actor: (&str, &str),
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_repository_contract::SessionRepository as _;
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
        let closed = source.host.installed_sessions.realizations().read_realization(thread, |r| {
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
        root: awaken_session_state_contract::PersistedSession,
        lease: crate::session_resource::SessionRealizationLease,
    ) -> Result<awaken_session_state_contract::PersistedSession, awaken_session_types::RunError>
    {
        use awaken_session_state_contract::SessionAggregate;
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
        managed: &crate::ManagedHost,
    ) -> Arc<awaken_session_application::SessionApplication> {
        self.application
            .get_or_init(|| {
                let catalog = Arc::new(
                    awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new(),
                );
                catalog
                    .install_seed(
                        awaken_environment_application::default_environment_registration(),
                    )
                    .expect("install canonical local Environment");
                let environments = Arc::new(
                    awaken_environment_application::execution::EnvironmentExecutionApplication::new(
                        Arc::new(awaken_work_store::InMemoryWorkQueue::new(
                            self.clock.clone(),
                        )),
                        catalog,
                    ),
                );
                let runtime = crate::session_execution_roles(managed);
                Arc::new(
                    awaken_session_application::SessionApplicationBuilder::fixture(
                        runtime,
                        Arc::new(managed.clone()),
                        self.persistence
                            .lock()
                            .unwrap()
                            .take()
                            .expect("one selected Persistence"),
                        environments,
                        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                    )
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
        managed: &crate::ManagedHost,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        desired: crate::session_resource::ResolvedSessionResources,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_repository_contract::SessionRepository as _;
        let projection = self
            .settle_resource_stage(managed, thread, projection)
            .await?;
        let id = awaken_session_types::SessionId::from(thread);
        let fingerprint = awaken_agent_contract::stable_fingerprint(&desired);
        self.application(managed)
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
            .host
            .realization_executor
            .apply_session_inputs(thread, &transition, None, Some(lease))
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
        managed: &crate::ManagedHost,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_types::RunError,
    > {
        use awaken_session_realization_contract::SessionRealizationControl as _;
        use awaken_session_repository_contract::SessionRepository as _;
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
            .host
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
        mut aggregate: awaken_session_state_contract::SessionAggregate,
    ) -> Result<awaken_session_state_contract::PersistedSession, awaken_session_types::RunError>
    {
        use awaken_session_repository_contract::SessionRepository as _;
        use awaken_session_state_contract::{
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
        managed: &crate::ManagedHost,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        root: awaken_session_state_contract::PersistedSession,
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
        use awaken_session_repository_contract::SessionRepository as _;
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
        root: &awaken_session_state_contract::PersistedSession,
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
        managed: &crate::ManagedHost,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        root: awaken_session_state_contract::PersistedSession,
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
            managed,
            thread,
            projection.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await?;
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            managed,
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
