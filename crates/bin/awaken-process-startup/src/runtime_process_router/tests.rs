//! RS-BOOT-001/002: dependency rejection through the actual process entry point.
use crate::config::Role;
use crate::hosted_model_publication::PublicationModelSupply;
use crate::local_process_stores::{in_memory_process_stores, in_memory_split_coordinator};
use crate::process_startup::ProcessStartup;
use crate::process_stores::ProcessStores;
use awaken_session_repository_contract::SessionStore;
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
        std::sync::Arc::new(crate::colocated_worker::WorkdirExecution),
    );
    process.role = role;
    process.control_service = control_service;
    (stores, process, sessions)
}

async fn assert_rejected_before_assembly(
    mut stores: ProcessStores,
    process: ProcessStartup,
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
    let result = super::prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        process,
        Some(Box::new(move |host| {
            observed.store(true, Ordering::SeqCst);
            host
        })),
    )
    .await;
    let error = result.err().expect("invalid startup must return an error");
    assert!(error.contains(expected), "expected {expected:?}: {error}");
    assert!(
        !assembled_host.load(Ordering::SeqCst),
        "Host customization was not reached"
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
