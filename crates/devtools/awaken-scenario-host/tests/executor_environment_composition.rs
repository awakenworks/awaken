//! Real Environment/Work assembly stays in the existing scenario composition.
use std::sync::Arc;

use awaken_executable_environment_contract::{
    SessionEnvironmentProjectionSource, WorkPayload, WorkQueries as _, WorkState,
};
use awaken_session_application_contract::{SessionWorkerAssignment, SessionWorkerLeases};
use awaken_session_types::SessionId;
use awaken_time::{WallClock, WallClockExt as _};

#[tokio::test]
async fn executor_environment_roles_use_the_selected_work_queue_and_clock() {
    // C1 the original Root fixture selects one queue, clock and local catalog;
    // C2 repeated assignment names the same Session; C3 its lease role claims
    // that work. E1 the actual row is in the selected queue with its clock's
    // creation instant; E2 replay leaves one original row; E3 the lease role
    // activates that same row. A separate queue is the negative control: it
    // remains empty. These are real in-memory owner adapters, not PG evidence.
    let now_ms = 1_000_000;
    let clock: Arc<dyn WallClock> = Arc::new(awaken_lease_testkit::ManualClock::new(now_ms));
    let expected_created_at = clock.observed_at();
    let queue = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock.clone()));
    let other_queue = awaken_work_store::InMemoryWorkQueue::new(clock.clone());
    let catalog =
        Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
    let registration = awaken_environment_application::default_environment_registration();
    let environment_id = registration.definition.id.clone();
    let revision = registration.definition.revision;
    catalog.install_seed(registration).unwrap();
    let environments = Arc::new(
        awaken_environment_application::execution::EnvironmentExecutionApplication::new(
            queue.clone(),
            catalog,
        ),
    );
    // The Session application consumes all three roles from this one source.
    let _roles: awaken_session_application::SessionEnvironments = environments.clone().into();
    let current = SessionEnvironmentProjectionSource::get(environments.as_ref(), &environment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.id, environment_id, "E1 original catalog");
    assert_eq!(current.revision, revision, "E1 original revision");

    let session = SessionId::from("executor-environment-composition");
    let work_id =
        SessionWorkerAssignment::enqueue_session(environments.as_ref(), &environment_id, &session)
            .await
            .unwrap();
    let repeated =
        SessionWorkerAssignment::enqueue_session(environments.as_ref(), &environment_id, &session)
            .await
            .unwrap();
    assert_eq!(repeated, work_id, "E2 original assignment");
    let rows = queue.list(&environment_id).await.unwrap();
    assert_eq!(rows.len(), 1, "E2 no second queue row");
    assert_eq!(rows[0].id, work_id, "E1 selected queue");
    assert_eq!(rows[0].created_at, expected_created_at, "E1 selected clock");
    assert!(matches!(&rows[0].data, WorkPayload::Session { id } if id == session.as_str()));
    assert_eq!(rows[0].state, WorkState::Queued);

    let lease = SessionWorkerLeases::acquire_session(
        environments.as_ref(),
        &environment_id,
        &session,
        "scenario-worker",
        now_ms,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(lease.work_id(), work_id, "E3 original row");
    assert_eq!(lease.session_id(), &session, "E3 original Session");
    assert_eq!(lease.owner(), "scenario-worker", "E3 original owner");
    assert_eq!(
        queue.list(&environment_id).await.unwrap()[0].state,
        WorkState::Active
    );
    assert!(
        other_queue.list(&environment_id).await.unwrap().is_empty(),
        "E1 distinct queue"
    );
    assert!(
        SessionWorkerLeases::release_session(environments.as_ref(), &lease)
            .await
            .unwrap()
    );
}

// This is the unchanged Host/Resources composition case moved from Testkit.
// Core-only Testkit qualification must not compile its Host-only dev adapters.
mod testkit_host {
    use std::sync::Arc;

    use awaken_deployment_config::DeploymentConfig;
    use awaken_session_executor::SessionExecutorBuilder;
    use awaken_session_executor_testkit::{
        local_resource_ports, memory_extraction_repository, volatile_runtime_authority,
    };

    #[derive(Default)]
    struct RecordingMemoryInvalidator(std::sync::Mutex<Vec<(String, String)>>);
    impl awaken_resource_contract::MemoryInvalidator for RecordingMemoryInvalidator {
        fn publish(&self, store: &str, path: &str) {
            self.0
                .lock()
                .expect("test Memory invalidations")
                .push((store.into(), path.into()));
        }
    }

    /// Cause/effect graph: C1 one Resources authority selection supplies the
    /// File/Memory/Skill/reclamation ports; C2 its clock fills the existing
    /// process value; C3 the sole local constructor consumes those inputs.
    /// P1 C1 -> the original Skill/reclamation/Memory identities are present.
    /// P2 C1+C2+C3 -> the actual Builder clock is that same Resources Arc,
    /// rather than a second clock copied into LocalResourcePorts. This row
    /// observes the constructed Builder; comparing two inputs is insufficient.
    /// Existing topology/kernel and root suites own invalid-deployment panic,
    /// provider/capture and authority rules; this row performs no model I/O and
    /// adds no parallel model fake, clock selection policy or permission path.
    #[tokio::test]
    async fn resource_ports_carry_every_authority_of_one_selection() {
        let clock = awaken_time::system_clock();
        let resources = awaken_resource_persistence::ephemeral(
            clock,
            Arc::new(RecordingMemoryInvalidator::default()),
        )
        .await
        .expect("ephemeral Resources");
        let authorities = resources.authorities();
        let ports = local_resource_ports(&authorities);
        assert!(ports.skills.is_some(), "P1 Skill catalog present");
        assert!(ports.reclamation.is_some(), "P1 reclamation present");
        assert!(
            Arc::ptr_eq(&ports.memory.queries(), &authorities.memory_queries()),
            "P1 the same Memory authority"
        );
        let process = awaken_session_executor::SessionExecutionProcess::from_deployment(
            &DeploymentConfig::ephemeral(),
            awaken_deployment_config::mint_local_workspace(),
            authorities.clock(),
        )
        .expect("a local fixture selects a valid process");
        let dependencies = SessionExecutorBuilder::for_local_resources(
            Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF,
            ports,
            memory_extraction_repository(None).await,
            process,
            volatile_runtime_authority(authorities.clock()),
        );
        assert!(
            Arc::ptr_eq(&dependencies.clock(), &authorities.clock()),
            "P2 clock"
        );
    }
}
