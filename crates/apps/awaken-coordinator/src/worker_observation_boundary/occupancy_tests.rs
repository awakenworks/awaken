// @formal surface exclude=test_only
// @formal owner crates/apps/awaken-coordinator/src/worker_observation_boundary.rs
// @formal reason: Exercises the real private domain Router with the existing read-only Work SPI and original Session/Registry owners.
//! Work occupancy is a required read capability, independent of Environment commands.
use super::*;
use axum::{body::Body, http::Request};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt as _;

#[derive(Default)]
struct OccupancyOnly {
    mode: AtomicUsize,
    owners: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::WorkerLeaseObservation for OccupancyOnly {
    async fn has_owner_claims(
        &self,
        owner: &str,
    ) -> Result<bool, awaken_executable_environment_contract::WorkQueueError> {
        self.owners.lock().unwrap().push(owner.to_owned());
        match self.mode.load(Ordering::SeqCst) {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(
                awaken_executable_environment_contract::WorkQueueError::Storage(
                    "private-work-claim-store diagnostic".into(),
                ),
            ),
        }
    }
}

async fn request(
    app: &Router,
    identity: &awaken_worker_contract::WorkerIdentity,
    authenticated: bool,
) -> axum::response::Response {
    let mut request = Request::get(format!(
        "{WORKER_DOMAIN_PATH}?worker_id={}&incarnation_id={}&generation={}",
        identity.worker_id, identity.incarnation_id, identity.generation,
    ));
    if authenticated {
        request = request.header(header::AUTHORIZATION, "Bearer domain-token");
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        app.clone().oneshot(request.body(Body::empty()).unwrap()),
    )
    .await
    .expect("domain response deadline")
    .unwrap()
}

#[tokio::test]
async fn domain_reads_require_only_work_observation_and_preserve_exact_owner_and_errors() {
    // O1 selected SPI says empty -> false; O2 that same SPI says held -> true.
    // O3 its read fails ->503 with original classification, never false/success.
    // O4 missing service auth and O5 foreign incarnation -> deny before the SPI.
    // Every admitted row must actually reach the same incarnation-qualified port.
    // This fake implements no Work claim, mutation or Environment registration API.
    let selected = Arc::new(OccupancyOnly::default());
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let resources = awaken_resource_persistence::ephemeral(
        clock.clone(),
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .unwrap();
    let assembled = crate::test_session_fixture::session_read_fixture(
        resources.authorities().resource_registry(),
        clock.clone(),
    )
    .await;
    let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
    let registered = directory
        .register(
            awaken_worker_contract::WorkerRegistration {
                worker_id: "occupancy-worker".into(),
                incarnation_id: "occupancy-boot".into(),
                manifest: awaken_worker_contract::WorkerManifest {
                    runtime_protocol: awaken_worker_contract::VersionRange::exact(
                        awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                    ),
                    ..Default::default()
                },
                create_ledger: None,
            },
            100,
            10_000,
        )
        .await
        .unwrap();
    let identity = registered.snapshot.identity;
    let app = domain_router_with_authenticator(DomainState {
        directory,
        dispatch: assembled.dispatch,
        work_claims: selected.clone(),
        session_queries: assembled.sessions.queries(),
        worker_occupancy: assembled.sessions.worker_occupancy(),
        relay_targets: None,
        bindings: None,
        clock,
        authenticator: awaken_service_auth_contract::static_token_authenticator("domain-token")
            .unwrap(),
        relay_authenticator: None,
        retirement_explanations: Default::default(),
    });
    for (mode, expected, calls) in [(0, false, 1), (1, true, 2)] {
        selected.mode.store(mode, Ordering::SeqCst);
        let response = request(&app, &identity, true).await;
        assert_eq!(response.status(), StatusCode::OK, "O1/O2");
        let bytes = tokio::time::timeout(
            Duration::from_secs(5),
            axum::body::to_bytes(response.into_body(), 1_000_000),
        )
        .await
        .expect("domain body deadline")
        .unwrap();
        let observation: WorkerDomainObservation = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(observation.registered.snapshot.identity, identity);
        assert_eq!(observation.work_claims_active, expected, "O1/O2");
        assert_eq!(
            selected.owners.lock().unwrap().len(),
            calls,
            "non-vacuous read"
        );
    }
    selected.mode.store(2, Ordering::SeqCst);
    let response = request(&app, &identity, true).await;
    assert_eq!(selected.owners.lock().unwrap().len(), 3, "O3 SPI reached");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "O3");
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 1_000_000),
    )
    .await
    .expect("domain fault body deadline")
    .unwrap();
    let fault: WorkerDomainReadFailure = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(fault.identity, identity);
    assert_eq!(fault.stage, "work_claims");
    assert_eq!(fault.kind, "unavailable");
    assert_eq!(fault.code, "worker_work_read_unavailable");
    assert!(!String::from_utf8_lossy(&bytes).contains("private-work-claim-store"));
    assert!(serde_json::from_slice::<WorkerDomainObservation>(&bytes).is_err());
    assert_eq!(
        request(&app, &identity, false).await.status(),
        StatusCode::UNAUTHORIZED,
        "O4"
    );
    let mut foreign = identity.clone();
    foreign.generation = foreign.generation.checked_add(1).unwrap();
    assert_eq!(
        request(&app, &foreign, true).await.status(),
        StatusCode::CONFLICT,
        "O5"
    );
    assert_eq!(
        selected.owners.lock().unwrap().as_slice(),
        vec![identity.lease_owner(); 3],
        "exact selected owner; denied requests add no read",
    );
}
