//! RS-BOOT-001/002: dependency rejection through the actual process entry point.
use crate::config::Role;
use crate::hosted_model_publication::PublicationModelSupply;
use crate::local_process_stores::{in_memory_process_stores, in_memory_split_coordinator};
use crate::process_startup::ProcessStartup;
use crate::process_stores::ProcessStores;
use awaken_session_application::SessionStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

async fn fixture(role: Role) -> (ProcessStores, ProcessStartup, Arc<dyn SessionStore>) {
    let (stores, control_service) = if role == Role::Coordinator {
        let (stores, services) = in_memory_split_coordinator().await;
        (stores, Some(services))
    } else {
        (in_memory_process_stores().await, None)
    };
    let sessions = stores.coordinator.as_ref().unwrap().sessions.clone();
    let mut process = crate::exact_host_model::local_test_process_options(
        &stores,
        Box::new(awaken_cli::LocalExecution::workdir()),
    );
    process.role = role;
    process.control_service = control_service;
    (stores, process, sessions)
}

struct ObservedExecution {
    invoked: Arc<AtomicBool>,
    delegate: Box<dyn crate::ColocatedExecution>,
}

#[async_trait::async_trait]
impl crate::ColocatedExecution for ObservedExecution {
    async fn prepare(
        self: Box<Self>,
        inputs: crate::ColocatedExecutionInputs,
    ) -> Result<crate::PreparedColocatedExecution, String> {
        self.invoked.store(true, Ordering::SeqCst);
        self.delegate.prepare(inputs).await
    }
}

async fn assert_rejected_before_assembly(
    mut stores: ProcessStores,
    mut process: ProcessStartup,
    sessions: Arc<dyn SessionStore>,
    expected: &str,
) {
    let recovery_calls = Arc::new(AtomicUsize::new(0));
    if let Some(coordinator) = stores.coordinator.as_mut() {
        coordinator.deployments = Arc::new(RefuseRecovery(recovery_calls.clone()));
    }
    let lifecycle = process.service_lifecycle.clone();
    let assembled_host = Arc::new(AtomicBool::new(false));
    let observed = assembled_host.clone();
    process.colocated_execution = process.colocated_execution.take().map(|delegate| {
        Box::new(ObservedExecution {
            invoked: observed,
            delegate,
        }) as Box<dyn crate::ColocatedExecution>
    });
    let result = super::prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        process,
    )
    .await;
    let error = result.err().expect("invalid startup must return an error");
    assert!(error.contains(expected), "expected {expected:?}: {error}");
    assert!(
        !assembled_host.load(Ordering::SeqCst),
        "co-located execution construction was not reached"
    );
    assert_eq!(
        recovery_calls.load(Ordering::SeqCst),
        0,
        "preflight precedes Deployment recovery"
    );
    // Starting from an empty lifecycle, failure needs no drain budget. This is
    // a cleanup oracle, not a count of private task registrations.
    lifecycle
        .shutdown(Duration::ZERO)
        .await
        .expect("immediate failed-startup drain");
    let page = sessions.reconciliation_scan(None).await.unwrap();
    assert!(page.sessions.is_empty(), "no Session writes");
    assert!(page.quarantined.is_empty(), "no quarantined partial root");
    assert!(page.next_cursor.is_none(), "the empty result is complete");
}

// Refuse every Deployment operation through the actual startup port. Any
// recovery accidentally moved ahead of dependency preflight changes the named
// error and this observation, without relying on an implementation spy hook.
struct RefuseRecovery(Arc<AtomicUsize>);

impl RefuseRecovery {
    fn attempted<T>(&self) -> Result<T, awaken_deployment_contract::DeploymentRepositoryError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(
            awaken_deployment_contract::DeploymentRepositoryError::Storage(
                "preflight must precede Deployment recovery".into(),
            ),
        )
    }
}

#[async_trait::async_trait]
impl awaken_deployment_contract::DeploymentRepository for RefuseRecovery {
    async fn deployments(
        &self,
    ) -> Result<
        Vec<awaken_deployment_contract::DeploymentView>,
        awaken_deployment_contract::DeploymentRepositoryError,
    > {
        self.attempted()
    }

    async fn deployment_runs(
        &self,
    ) -> Result<
        Vec<awaken_deployment_contract::DeploymentRunView>,
        awaken_deployment_contract::DeploymentRepositoryError,
    > {
        self.attempted()
    }

    async fn write_deployment(
        &self,
        _deployment: awaken_deployment_contract::DeploymentView,
        _expected_revision: Option<u64>,
        _scheduled_limit: usize,
        _lifecycle: Option<awaken_deployment_contract::DeploymentLifecycleFact>,
    ) -> Result<
        awaken_deployment_contract::DeploymentWriteOutcome,
        awaken_deployment_contract::DeploymentRepositoryError,
    > {
        self.attempted()
    }

    async fn upsert_deployment_run(
        &self,
        _run: awaken_deployment_contract::DeploymentRunView,
        _lifecycle: Option<awaken_deployment_contract::DeploymentLifecycleFact>,
    ) -> Result<(), awaken_deployment_contract::DeploymentRepositoryError> {
        self.attempted()
    }

    async fn claim_scheduled_run(
        &self,
        _claim_id: &str,
        _expected_deployment_revision: u64,
        _deployment: awaken_deployment_contract::DeploymentView,
        _run: awaken_deployment_contract::DeploymentRunView,
        _lifecycle: awaken_deployment_contract::DeploymentLifecycleFact,
    ) -> Result<
        awaken_deployment_contract::ScheduledRunClaimOutcome,
        awaken_deployment_contract::DeploymentRepositoryError,
    > {
        self.attempted()
    }
}

type RemoveWiring = fn(&mut ProcessStartup);

#[test]
fn missing_runtime_dependencies_fail_before_host_or_tasks() {
    // Independent causes: role AllInOne/Coordinator and exactly one absent
    // Directory/observations/transport/Agent/Environment port. Each other input
    // stays valid. All 2 x 5 negative rows must return the named error without
    // entering Host assembly or writing a Session; immediate lifecycle drain
    // remains available. Valid-role surface tests own the positive rows.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        let cases: [(&str, RemoveWiring); 5] = [
            ("WorkerDirectory", |process| process.worker_directory = None),
            ("Worker observations", |process| {
                process.worker_observations = None
            }),
            ("Worker transport", |process| {
                process.worker_transport = None
            }),
            ("executable Agent wiring", |process| {
                process.executable_agent_wiring = None
            }),
            ("executable Environment wiring", |process| {
                process.executable_environment_wiring = None
            }),
        ];
        for role in [Role::AllInOne, Role::Coordinator] {
            for (name, remove) in cases {
                let (stores, mut process, sessions) = fixture(role).await;
                remove(&mut process);
                assert_rejected_before_assembly(stores, process, sessions, name).await;
            }
        }
    });
}

#[test]
fn runtime_store_and_service_ownership_fails_closed() {
    // R1 either Runtime role lacks Coordinator stores; R2 AllInOne lacks its
    // Control stores; R3 split Coordinator receives surplus Control authority;
    // R4 split Coordinator lacks its selected Control-service boundary. Every
    // other cause remains valid. None may create a fallback component.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        for role in [Role::AllInOne, Role::Coordinator] {
            let (mut stores, process, sessions) = fixture(role).await;
            stores.coordinator = None;
            assert_rejected_before_assembly(
                stores,
                process,
                sessions,
                "requires Coordinator stores",
            )
            .await;
        }
        let (mut stores, process, sessions) = fixture(Role::AllInOne).await;
        stores.control = None;
        assert_rejected_before_assembly(
            stores,
            process,
            sessions,
            "AllInOne requires Control stores",
        )
        .await;

        let (mut stores, process, sessions) = fixture(Role::Coordinator).await;
        stores.control = in_memory_process_stores().await.control;
        assert_rejected_before_assembly(
            stores,
            process,
            sessions,
            "Coordinator cannot own Control stores",
        )
        .await;

        let (stores, mut process, sessions) = fixture(Role::Coordinator).await;
        process.control_service = None;
        assert_rejected_before_assembly(
            stores,
            process,
            sessions,
            "Coordinator requires Control services",
        )
        .await;
    });
}

#[test]
fn non_runtime_roles_cannot_enter_runtime_assembly() {
    // R1 Control and R2 Worker supplied with otherwise complete Runtime inputs
    // must return an error in release and debug builds, rather than rely on a
    // debug assertion or inherit another role's startup authority.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        for role in [Role::Control, Role::Worker] {
            let (stores, mut process, sessions) = fixture(Role::AllInOne).await;
            process.role = role;
            assert_rejected_before_assembly(
                stores,
                process,
                sessions,
                "does not own Runtime process assembly",
            )
            .await;
        }
    });
}

struct ExecutionFixture {
    base: super::ProcessBase,
    resources: super::DeploymentAndResources,
    cloud: super::CloudPlane,
    control: super::ControlSurface,
    execution: super::ExecutionStores,
    sessions: Arc<dyn SessionStore>,
}

async fn execution_fixture(role: Role, registered: bool) -> ExecutionFixture {
    let (stores, mut process, sessions) = fixture(role).await;
    let deployment = process.deployment.as_mut().unwrap();
    deployment.disable_local_pool = registered;
    deployment.durable = true;
    let mut base = super::open_process_base(stores, None, None, process)
        .await
        .unwrap();
    let resources = super::assemble_deployment_and_resources(&mut base)
        .await
        .unwrap();
    let cloud = super::assemble_cloud_plane(&mut base, PublicationModelSupply::PublishedProviders)
        .await
        .unwrap();
    let component = super::assemble_control_component(&base, &resources, &cloud, None).await;
    let execution = super::assemble_execution_stores(&mut base, &resources)
        .await
        .unwrap();
    let control = super::assemble_control_surface(&mut base, component).unwrap();
    // The production stage functions own all assembly. This fixture only stops
    // at their boundary so the test can observe the selected construction.
    ExecutionFixture {
        base,
        resources,
        cloud,
        control,
        execution,
        sessions,
    }
}

#[test]
fn process_placement_selects_shared_worker_delivery_and_releases_process_services() {
    // Causes: C1 AllInOne / split Coordinator, C2 registered / local placement,
    // C3 the complete Session application stays alive after its service ports
    // drop. Effects: P1=C2(registered) => WorkerPlacedSessions and no local pool,
    // P2=C2(local) => original executor and pool; both use the exact selected
    // dispatch and preparation compiler. P3=P1+C3 => ProcessDispatch releases:
    // neither publication, placement, reads nor settlement closes a back edge.
    // P4 either placement => original start/drain law, no Session is created by
    // construction. The four C1 x C2 rows exercise production stage functions.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        for role in [Role::AllInOne, Role::Coordinator] {
            for registered in [false, true] {
                let mut f = execution_fixture(role, registered).await;
                let original = f.base.runtime_authority.as_ref().unwrap().dispatch_store();
                let lifecycle = f.base.process.service_lifecycle.clone();
                let prepared = super::assemble_execution_host(
                    &mut f.base,
                    &f.resources,
                    &mut f.cloud,
                    &f.control,
                    &f.execution,
                )
                .await
                .unwrap();
                assert_eq!(
                    matches!(&prepared.selected, super::SelectedExecution::Registered(_)),
                    registered,
                    "P1/P2 role={role:?}"
                );
                let dispatch = match &prepared.selected {
                    super::SelectedExecution::Colocated(execution) => execution.dispatch.clone(),
                    super::SelectedExecution::Registered(execution) => execution.dispatch.clone(),
                };
                let process = Arc::downgrade(&dispatch);
                let compiler = dispatch.preparation_placement();
                let plane = super::assemble_session_plane(
                    &mut f.base,
                    &f.resources,
                    &f.cloud,
                    &mut f.control,
                    &f.execution,
                    prepared,
                )
                .unwrap();
                assert!(
                    Arc::ptr_eq(&plane.execution.dispatch, &dispatch),
                    "P1/P2 original process"
                );
                assert!(
                    Arc::ptr_eq(&plane.execution.coordinator_dispatch, &original),
                    "P1/P2 original authority"
                );
                let mounted = plane.execution.dispatch.preparation_placement();
                assert!(Arc::ptr_eq(&compiler, &mounted), "P1/P2 exact compiler");
                plane.execution.dispatch_lifecycle.start().unwrap();
                assert_eq!(
                    plane.execution.dispatch.pool().is_some(),
                    !registered,
                    "P1/P2 selected physical owner"
                );
                assert!(
                    plane
                        .execution
                        .dispatch_lifecycle
                        .drain(Duration::from_secs(2))
                        .await,
                    "P4 original bounded drain"
                );
                let application = plane.session_application.clone();
                drop(plane);
                drop(dispatch);
                if registered {
                    assert!(
                        process.upgrade().is_none(),
                        "P3 retained application releases services"
                    );
                }
                let page = f.sessions.reconciliation_scan(None).await.unwrap();
                assert!(
                    page.sessions.is_empty(),
                    "P4 composition creates no business Session"
                );
                assert!(page.quarantined.is_empty());
                drop(application);
                lifecycle.shutdown(Duration::from_secs(2)).await.unwrap();
            }
        }
    });
}

struct BoundaryMemoryProbe {
    fail: Option<&'static str>,
    calls: std::sync::Mutex<Vec<(&'static str, bool)>>,
}

impl BoundaryMemoryProbe {
    fn observe(
        &self,
        phase: &'static str,
        dispatch: &awaken_dispatch::RunDispatch,
        claim: &awaken_dispatch::RunClaim,
        cancelled: bool,
    ) -> Result<(), awaken_dispatch::DispatchSettlementError> {
        assert_eq!(dispatch.run_id(), &claim.run_id, "original guarded Run");
        self.calls.lock().unwrap().push((phase, cancelled));
        if self.fail == Some(phase) {
            return Err(awaken_dispatch::DispatchSettlementError(format!(
                "original-memory-{phase}-refusal",
            )));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_dispatch::DispatchSettlementObserver for BoundaryMemoryProbe {
    async fn before_settle(
        &self,
        dispatch: &awaken_dispatch::RunDispatch,
        claim: &awaken_dispatch::RunClaim,
        _: &awaken_agent_contract::agent::run::RunState,
        cancelled: bool,
    ) -> Result<(), awaken_dispatch::DispatchSettlementError> {
        self.observe("before", dispatch, claim, cancelled)
    }
    async fn after_settle(
        &self,
        dispatch: &awaken_dispatch::RunDispatch,
        claim: &awaken_dispatch::RunClaim,
        _: &awaken_agent_contract::agent::run::RunState,
        cancelled: bool,
    ) -> Result<(), awaken_dispatch::DispatchSettlementError> {
        self.observe("after", dispatch, claim, cancelled)
    }
}

#[test]
fn registered_boundary_observation_preserves_original_memory_and_session_refusals() {
    // Causes: C1 before/after Dispatch CAS callback, C2 Memory succeeds/fails,
    // C3 Session boundary absent/present for an unknown primary Session.
    // D1=C2(fail) => original Memory refusal first, without Session admission;
    // D2=C2(ok)+C3(present) => real Session application refusal unchanged;
    // D3=C2(ok)+C3(absent) => success. All 2 x 2 x 2 rows preserve the exact
    // guarded Run and cancellation flag; callbacks own no Dispatch fence/CAS.
    // The existing Session observer and HTTP tests own successful Session CAS,
    // replay and post-CAS finalization. This adapter only orders those effects.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        use awaken_agent_contract::agent::{
            run::{EndCause, Id as RunId, RunState},
            thread::Id as ThreadId,
        };
        use awaken_dispatch::DispatchSettlementObserver as _;
        let mut f = execution_fixture(Role::Coordinator, true).await;
        let lifecycle = f.base.process.service_lifecycle.clone();
        let prepared = super::assemble_execution_host(
            &mut f.base,
            &f.resources,
            &mut f.cloud,
            &f.control,
            &f.execution,
        )
        .await
        .unwrap();
        let plane = super::assemble_session_plane(
            &mut f.base,
            &f.resources,
            &f.cloud,
            &mut f.control,
            &f.execution,
            prepared,
        )
        .unwrap();
        let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("frozen-agent")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse(
                    "provider", "model", "native",
                )
                .unwrap(),
            )
            .build();
        for phase in ["before", "after"] {
            for memory_fails in [false, true] {
                for coordinated in [false, true] {
                    let memory = Arc::new(BoundaryMemoryProbe {
                        fail: memory_fails.then_some(phase),
                        calls: Default::default(),
                    });
                    let observer = super::registered_execution::CoordinatorBoundaryEffects {
                        sessions: Arc::new(
                            awaken_session_run_delivery::SessionDispatchObserver::new(
                                plane.session_application.coordination(),
                            ),
                        ),
                        memory: memory.clone(),
                    };
                    let mut dispatch = awaken_dispatch::RunDispatch::new(
                        awaken_runtime_contract::RunActivation::new(
                            RunId("guarded-run".into()),
                            ThreadId("unknown-primary".into()),
                            snapshot.clone(),
                            Vec::new(),
                        ),
                    )
                    .for_session(ThreadId("unknown-primary".into()));
                    dispatch.session_activity_epoch = coordinated.then_some(1);
                    let claim = awaken_dispatch::RunClaim {
                        run_id: dispatch.run_id().clone(),
                        owner: "claiming-worker".into(),
                        epoch: 5,
                    };
                    let state = RunState::Ended(EndCause::NaturalEnd);
                    assert!(!observer.owns_dispatch_fence());
                    let result = if phase == "before" {
                        observer
                            .before_settle(&dispatch, &claim, &state, true)
                            .await
                    } else {
                        observer.after_settle(&dispatch, &claim, &state, true).await
                    };
                    assert_eq!(memory.calls.lock().unwrap().as_slice(), &[(phase, true)]);
                    match (memory_fails, coordinated) {
                        (true, _) => assert_eq!(
                            result.unwrap_err().0,
                            format!("original-memory-{phase}-refusal"),
                            "D1"
                        ),
                        (false, true) => {
                            assert!(!result.unwrap_err().0.contains("original-memory"), "D2")
                        }
                        (false, false) => result.expect("D3 no Session boundary requested"),
                    }
                }
            }
        }
        assert!(
            f.sessions
                .reconciliation_scan(None)
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
        drop(plane);
        lifecycle.shutdown(Duration::from_secs(2)).await.unwrap();
    });
}

#[test]
fn registered_process_adopts_initial_credentials_without_losing_reserved_activity() {
    // Causes: C1 the production registered-Worker role set; C2 frozen Preparing
    // with known-empty materialization and no admitted physical/MCP effect;
    // C3 newer same-source credential; C4 logical activity absent/already reserved.
    // R1=C1+C2+C3+!C4 and R2=C1+C2+C3+C4 => the real update CAS advances the
    // desired pin, retires pristine old requests and preserves every activity
    // coordinate. No Worker has executed it, so pin adoption is not convergence.
    // The shared fixture is the Coordinator adoption test's exact input law;
    // this case replaces its physical fake with the production composition.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        use awaken_credential_contract::ManagedCredentialRolloutTarget as _;
        use awaken_protocol_managed_testkit::{
            RolloutCredentialSource, create_session_fixture, preparing_session_with_mcp_source,
        };
        use awaken_session_types::SessionId;
        for with_activity in [false, true] {
            let mut f = execution_fixture(Role::Coordinator, true).await;
            f.control.credential_source = Arc::new(RolloutCredentialSource);
            let lifecycle = f.base.process.service_lifecycle.clone();
            create_session_fixture(
                f.sessions.as_ref(),
                "workspace-a",
                preparing_session_with_mcp_source(with_activity),
            )
            .await;
            let id = SessionId::from("preparing");
            let before = f.sessions.get(&id).await.unwrap();
            let prepared = super::assemble_execution_host(
                &mut f.base,
                &f.resources,
                &mut f.cloud,
                &f.control,
                &f.execution,
            )
            .await
            .unwrap();
            let plane = super::assemble_session_plane(
                &mut f.base,
                &f.resources,
                &f.cloud,
                &mut f.control,
                &f.execution,
                prepared,
            )
            .unwrap();
            let target = awaken_coordinator::SessionCredentialRolloutAdoption::over_managed(
                &plane.session_application,
                &plane.managed_state,
            );
            let event = awaken_credential_contract::ManagedCredentialRollout {
                id: "registered-preparation-rotation".into(),
                workspace_id: "workspace-a".into(),
                vault_id: "vault-event".into(),
                credential_id: "credential-a".into(),
                source_id: awaken_credential_contract::CredentialSourceId("source-a".into()),
                source_version: 2,
                credential_revision: 2,
                operation: awaken_credential_contract::ManagedCredentialOperation::Update,
            };
            let progress = target.rollout(&event).await.unwrap();
            let after = f.sessions.get(&id).await.unwrap();
            let desired = after.mcp.desired_attachments();
            assert_eq!(desired.len(), 1);
            assert_eq!(
                desired[0].credential.as_ref().unwrap().credential.revision,
                2,
                "desired pin advances before the first Worker can realize it"
            );
            assert!(
                after
                    .mcp
                    .attachments
                    .iter()
                    .filter(|attachment| attachment
                        .credential
                        .as_ref()
                        .is_some_and(|access| access.credential.revision == 1))
                    .all(|attachment| attachment.state
                        == awaken_session_configuration_contract::mcp::McpAttachmentState::Removed)
            );
            assert_eq!(after.active_activity_epochs, before.active_activity_epochs);
            assert_eq!(after.run_activities(), before.run_activities());
            assert_eq!(
                after.environment_materialization(),
                before.environment_materialization()
            );
            assert_eq!(
                after.execution,
                awaken_session_contract::SessionExecutionState::Preparing
            );
            assert_eq!(desired[0].attempts, 0, "no initial MCP effect admitted");
            assert!(desired[0].realization.is_none());
            assert!(!desired[0].publication_acknowledged);
            assert!(
                !progress.is_converged(),
                "no initial MCP publication is acknowledged"
            );
            let generation = desired[0].generation.clone();
            target.rollout(&event).await.unwrap();
            let replay = f.sessions.get(&id).await.unwrap();
            assert_eq!(
                replay.mcp.desired_attachments()[0].generation,
                generation,
                "same adoption replay keeps the exact desired generation"
            );
            assert_eq!(replay.run_activities(), before.run_activities());
            assert!(
                plane.execution.dispatch.pool().is_none(),
                "registered placement executes no local Run"
            );
            drop(plane);
            lifecycle.shutdown(Duration::from_secs(2)).await.unwrap();
        }
    });
}

#[test]
fn only_a_local_placement_installs_the_colocated_execution() {
    // Constructor decision table carried from the old SDK hook:
    // L1 LocalWorker + supplied constructor => one installation;
    // L2 LocalWorker + missing constructor => original refusal, zero effects;
    // L3 RegisteredWorker + any constructor => zero local installation.
    // The registered root's SDK-free signature and binary closure guard own
    // backend independence; this spy observes the actual production stage.
    awaken_service_lifecycle::run_composed_async_test(|| async {
        for (registered, supplied) in [(false, true), (false, false), (true, true), (true, false)] {
            let mut f = execution_fixture(Role::AllInOne, registered).await;
            let lifecycle = f.base.process.service_lifecycle.clone();
            let invoked = Arc::new(AtomicBool::new(false));
            f.base.process.colocated_execution = supplied.then(|| {
                Box::new(ObservedExecution {
                    invoked: invoked.clone(),
                    delegate: Box::new(awaken_cli::LocalExecution::workdir()),
                }) as Box<dyn crate::ColocatedExecution>
            });
            let result = super::assemble_execution_host(
                &mut f.base,
                &f.resources,
                &mut f.cloud,
                &f.control,
                &f.execution,
            )
            .await;
            assert_eq!(invoked.load(Ordering::SeqCst), !registered && supplied);
            if !registered && !supplied {
                assert!(
                    result.err().unwrap().contains("co-locates their execution"),
                    "L2"
                );
            } else {
                drop(result.unwrap());
            }
            assert!(
                f.sessions
                    .reconciliation_scan(None)
                    .await
                    .unwrap()
                    .sessions
                    .is_empty()
            );
            lifecycle.shutdown(Duration::from_secs(2)).await.unwrap();
        }
    });
}
