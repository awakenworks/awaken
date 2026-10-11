// @formal evidence worker_credential.stale_heartbeat_never_rolls_back_revision
use awaken_dispatch::ClaimedCheckpointTransport as _;
use awaken_worker_contract::WorkerCurrentSource;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_lease_testkit::ManualClock;
use awaken_dispatch::{
    MemoryDispatchStore, PlacementRequirements, RunDispatch, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryStreamCheckpointStore;
use awaken_worker_registry::{
    MemoryWorkerDirectory, RegistryMutation, WorkerCredentialObservation, WorkerCredentialRevision,
    WorkerHeartbeat, WorkerManifest, WorkerState,
};
use awaken_coordinator_client::HttpDispatchQueue;
use awaken_coordinator_client::{WorkerControlClient, WorkerRegistrationError};
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, WorkerUpstream,
};

// Registration requires explicit current protocol evidence. Default/ANY remains
// a negative transport case; positive registry/credential rules use this owner.
fn current_manifest() -> WorkerManifest {
    WorkerManifest {
        runtime_protocol: awaken_worker_contract::VersionRange::exact(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        ),
        ..Default::default()
    }
}

#[derive(Default)]
struct RecordingSessionPreparation {
    claims: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionPreparationControl
    for RecordingSessionPreparation
{
    async fn claim_next_session_preparation(
        &self,
        _target: awaken_session_realization_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionPreparationAssignment>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.claims.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

#[tokio::test]
async fn initial_session_preparation_requires_exact_ready_worker_authority() {
    // Causes: C1 authenticated identity matches the request, C2 current
    // Registry incarnation is Ready, C3 the target owner/incarnation matches
    // it, C4 target has a live non-reassigning lease inside Registry expiry.
    // Effects: E1 call the one Session-root selector; E2 reject before that
    // selector, with no Session or model operation. The closed rules are:
    // R1=all C true=>E1; R2=any C false=>E2. Starting, spoofed owner,
    // reassignment and valid Ready cover independent boundaries.
    let clock = Arc::new(ManualClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let preparation = Arc::new(RecordingSessionPreparation::default());
    let service = WorkerDispatchService::new(
        Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()))
            as Arc<dyn WorkerDispatchAuthority>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 30)
    .with_session_preparation_control(preparation.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            dispatch_transport_router_with_service(Arc::new(service)),
        )
        .await
        .unwrap();
    });
    let client = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-prepare"),
    );
    let registered = client
        .register_classified("boot-prepare", current_manifest())
        .await
        .unwrap();
    let identity = registered.worker.snapshot.identity;
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        lease_expires_at_unix_ms: 120,
        reassign_existing_lease: false,
    };
    assert!(
        client
            .claim_next_session_preparation(&identity, target.clone())
            .await
            .is_err(),
        "R2 Starting"
    );
    assert_eq!(preparation.claims.load(Ordering::SeqCst), 0, "R2/E2");

    clock.set(105);
    client
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .unwrap();
    let mut spoofed = target.clone();
    spoofed.owner = "another-worker".into();
    assert!(
        client
            .claim_next_session_preparation(&identity, spoofed)
            .await
            .is_err(),
        "R2 owner"
    );
    let mut reassigning = target.clone();
    reassigning.reassign_existing_lease = true;
    assert!(
        client
            .claim_next_session_preparation(&identity, reassigning)
            .await
            .is_err(),
        "R2 reassign"
    );
    assert_eq!(preparation.claims.load(Ordering::SeqCst), 0, "R2/E2");
    assert!(
        client
            .claim_next_session_preparation(&identity, target)
            .await
            .unwrap()
            .is_none(),
        "R1/E1"
    );
    assert_eq!(preparation.claims.load(Ordering::SeqCst), 1, "R1/E1");
}

#[tokio::test]
async fn authenticated_client_drives_the_registry_lifecycle_over_real_http() {
    let clock = Arc::new(ManualClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let service = WorkerDispatchService::new(
        Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()))
            as Arc<dyn WorkerDispatchAuthority>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory.clone(), 30);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-http"),
    );
    let registered = client
        .register_classified("boot-http", current_manifest())
        .await
        .unwrap();
    // Registration/lifecycle cause-effect graph: C1 same Worker id; C2
    // incarnation differs; C3 prior lease is live; C4 this registry-only
    // composition has no Session Work authority. R1 C1+C2+C3 => HTTP conflict
    // classified as SlotOccupied, preserving the old authority. R2 C4 + graceful
    // drain/quiesce/deregister => the original Registry lifecycle completes with
    // no synthetic Work dependency. A configured Work authority's exact-owner
    // release is covered by signed_worker_transport_http T21.
    // FMECA: treating optional C4 as an error makes the reusable registry router
    // unable to deregister and leaks the Worker slot; conditional cleanup retains
    // one Registry protocol while production composition still releases Work.
    assert!(matches!(
        client
            .register_classified("boot-http-conflict", current_manifest())
            .await,
        Err(WorkerRegistrationError::SlotOccupied(_))
    ));
    assert_eq!(registered.worker.snapshot.state, WorkerState::Starting);
    let identity = registered.worker.snapshot.identity;

    clock.set(110);
    let observed = WorkerCredentialRevision {
        id: "cred:worker".into(),
        revision: 9,
    };
    let observation = WorkerCredentialObservation::available(observed, 110, 140);
    assert_eq!(
        client
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 1,
                    warm_environment_shapes: Default::default(),
                    credential_observations: std::collections::BTreeSet::from([
                        observation.clone(),
                    ]),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap()
            .mutation,
        RegistryMutation::Applied
    );
    assert_eq!(
        directory
            .current("worker-http")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        WorkerState::Ready
    );
    assert_eq!(
        directory
            .current("worker-http")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .credential_observations,
        std::collections::BTreeSet::from([observation])
    );

    assert_eq!(
        client.begin_drain(&identity, Some(200)).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        client.mark_quiesced(&identity).await.unwrap(),
        RegistryMutation::InvalidTransition
    );
    client
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        client.mark_quiesced(&identity).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        client.deregister(&identity).await.unwrap(),
        RegistryMutation::Applied
    );
    assert_eq!(
        directory
            .current("worker-http")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        WorkerState::Dead
    );
}

#[tokio::test]
async fn authenticated_header_cannot_register_a_different_worker_id() {
    let directory = Arc::new(MemoryWorkerDirectory::new());
    let service = WorkerDispatchService::new(
        Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()))
            as Arc<dyn WorkerDispatchAuthority>,
        Arc::new(HeaderWorkerAuthenticator),
        Arc::new(ManualClock::new(0)),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 30);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/worker/register")
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", "authenticated")
        .body(Body::from(
            serde_json::json!({
                "registration": {
                    "worker_id": "impersonated",
                    "incarnation_id": "boot",
                    "manifest": current_manifest()
                }
            })
            .to_string(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

fn dispatch_with_capability(run: &str, capability: &str) -> RunDispatch {
    let activation = RunActivation::new(
        RunId(run.to_string()),
        ThreadId(format!("thread-{run}")),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("catalog".to_string()),
                instructions: String::new(),
                max_steps: Some(1),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "native"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("catalog".to_string()),
        },
        vec![Message::text(
            MessageId("message".to_string()),
            Role::User,
            "run",
        )],
    );
    let mut placement = PlacementRequirements::remote_required();
    placement
        .required_capabilities
        .insert(capability.to_string());
    RunDispatch::new(activation).with_placement(placement)
}

#[tokio::test]
async fn registered_http_claim_skips_incompatible_work_and_uses_incarnation_owner() {
    // Causes: the fixtures below establish `registered http claim skips incompatible work and` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `uses incarnation owner` and every asserted state transition
    // or side effect must hold.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Coverage rationale: `registered http claim skips incompatible work and` is one independent
    // branch selecting `uses incarnation owner`; a multi-row decision table is not applicable, and
    // sibling tests own alternate causes.
    let clock = Arc::new(ManualClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    // The dispatch port reads "now" from its store; the Worker registry keeps
    // its own clock, so this test drives both authorities explicitly.
    let dispatch_clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
    dispatch
        .enqueue(dispatch_with_capability("gpu", "gpu"))
        .await
        .unwrap();
    dispatch
        .enqueue(dispatch_with_capability("cpu", "cpu"))
        .await
        .unwrap();
    let service = WorkerDispatchService::new(
        dispatch as Arc<dyn WorkerDispatchAuthority>,
        Arc::new(HeaderWorkerAuthenticator),
        clock,
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 1_000)
    .with_checkpoint_store(checkpoints.clone());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let upstream = WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-cpu");
    let control = WorkerControlClient::new(upstream);
    let mut manifest = current_manifest();
    manifest.capabilities.insert("cpu".to_string());
    let registered = control
        .register_classified("boot-cpu", manifest)
        .await
        .unwrap();
    control
        .heartbeat(
            &registered.worker.snapshot.identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .unwrap();

    let client = HttpDispatchQueue::new(
        format!("http://{address}"),
        registered.worker.snapshot.identity.clone(),
    );
    dispatch_clock.set(99);
    let claimed = client
        .claim("ignored-local-owner", 99, &Default::default())
        .await
        .unwrap()
        .expect("compatible work");
    assert_eq!(claimed.request.run_id().0, "cpu");
    assert_eq!(
        claimed.lease.owner(),
        registered.worker.snapshot.identity.lease_owner()
    );
    assert_eq!(
        claimed.assignment.unwrap().identity,
        registered.worker.snapshot.identity
    );
    let claim = awaken_dispatch::RunClaim::from(&claimed.lease);
    assert!(
        client
            .bind_sandbox(&claim, "sandbox-cpu")
            .await
            .unwrap()
            .applied()
    );
    let partial = StreamCheckpoint {
        run_id: claimed.lease.run_id().0.clone(),
        thread_id: "thread-cpu".to_string(),
        model: "model".to_string(),
        partial_text: "partial".to_string(),
        partial_tools: Vec::new(),
        retry_count: 0,
    };
    assert!(
        client
            .put_stream_checkpoint(&claim, partial.clone())
            .await
            .unwrap()
            .applied()
    );
    assert_eq!(
        client.load_stream_checkpoint(&claim).await.unwrap(),
        Some(partial)
    );
    assert_eq!(
        control
            .begin_drain(&registered.worker.snapshot.identity, Some(1_000))
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    dispatch_clock.set(99);
    assert!(
        client
            .claim("ignored", 99, &Default::default())
            .await
            .is_err(),
        "a draining incarnation cannot receive new work"
    );
    assert!(matches!(
        checkpoints.get(&claim.run_id.0).await,
        Observed::Present(_)
    ));
}

#[tokio::test]
async fn http_claim_requires_the_exact_worker_private_credential_revision() {
    // Worker-observation rotation FMECA and cause/effect table. C1 dispatch pins
    // revision N/N+1; C2 Worker reports N/N+1; C3 observation live/expired; C4
    // heartbeat sequence current/stale. Effects: E1 only exact live revision may
    // claim; E2 rotation becomes eligible after one authoritative heartbeat; E3
    // a delayed pre-rotation heartbeat cannot roll the observation back.
    // Rules: R1 N/N/live=>E1; R2 N+1/N/live=>reject; R3 N+1/N+1/live=>E2;
    // R4 any/exact/expired=>reject; R5 stale sequence carrying N=>E3.
    let clock = Arc::new(ManualClock::new(100));
    let directory = Arc::new(MemoryWorkerDirectory::new());
    // The dispatch port reads "now" from its store; the Worker registry keeps
    // its own clock, so this test drives both authorities to the same reading.
    let dispatch_clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let required = WorkerCredentialRevision {
        id: "credential-source-worker-private".to_string(),
        revision: 12,
    };
    let mut placement = PlacementRequirements::remote_required();
    placement
        .required_capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    placement.required_credentials.insert(required.clone());
    dispatch
        .enqueue(
            dispatch_with_capability("worker-private", WORKER_LOCAL_CREDENTIALS_CAPABILITY)
                .with_placement(placement),
        )
        .await
        .unwrap();

    let service = WorkerDispatchService::new(
        dispatch.clone() as Arc<dyn WorkerDispatchAuthority>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(1_000)),
    )
    .with_worker_directory(directory, 1_000);
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    async fn ready_worker(
        address: std::net::SocketAddr,
        worker_id: &str,
        credential: WorkerCredentialRevision,
    ) -> awaken_worker_registry::WorkerIdentity {
        let control = WorkerControlClient::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_id(worker_id),
        );
        let mut manifest = current_manifest();
        manifest
            .capabilities
            .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
        let registered = control
            .register_classified(format!("boot-{worker_id}"), manifest)
            .await
            .unwrap();
        control
            .heartbeat(
                &registered.worker.snapshot.identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: [WorkerCredentialObservation::available(
                        credential, 100, 130,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap();
        registered.worker.snapshot.identity
    }

    let wrong = ready_worker(
        address,
        "worker-wrong-revision",
        WorkerCredentialRevision {
            id: required.id.clone(),
            revision: required.revision - 1,
        },
    )
    .await;
    let wrong_queue = HttpDispatchQueue::new(format!("http://{address}"), wrong);
    clock.set(100);
    dispatch_clock.set(100);
    assert!(
        wrong_queue
            .claim("ignored", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a nearby credential revision is not equivalent to the published revision"
    );

    let exact = ready_worker(address, "worker-exact-revision", required.clone()).await;
    let exact_queue = HttpDispatchQueue::new(format!("http://{address}"), exact.clone());
    clock.set(130);
    dispatch_clock.set(130);
    assert!(
        exact_queue
            .claim("ignored", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "an expired credential observation cannot claim while the worker lease is live"
    );
    let control = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("worker-exact-revision"),
    );
    assert_eq!(
        control
            .heartbeat(
                &exact,
                WorkerHeartbeat {
                    sequence: 2,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: [WorkerCredentialObservation::available(
                        WorkerCredentialRevision {
                            id: "credential-source-worker-private".to_string(),
                            revision: 12,
                        },
                        130,
                        160,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap()
            .mutation,
        RegistryMutation::Applied
    );
    let claimed = exact_queue
        .claim("ignored", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("the worker reporting the exact revision can claim the run");
    assert_eq!(claimed.request.run_id().0, "worker-private");
    assert_eq!(claimed.assignment.as_ref().unwrap().identity, exact, "R1");
    dispatch
        .settle(
            claimed.lease.run_id(),
            claimed.lease.epoch(),
            awaken_dispatch::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    let rotated = WorkerCredentialRevision {
        id: required.id,
        revision: required.revision + 1,
    };
    let mut rotated_placement = PlacementRequirements::remote_required();
    rotated_placement
        .required_capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    rotated_placement
        .required_credentials
        .insert(rotated.clone());
    dispatch
        .enqueue(
            dispatch_with_capability(
                "worker-private-rotated",
                WORKER_LOCAL_CREDENTIALS_CAPABILITY,
            )
            .with_placement(rotated_placement),
        )
        .await
        .unwrap();
    assert!(
        exact_queue
            .claim("ignored", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "R2: the pre-rotation observation cannot claim the successor revision"
    );

    clock.set(135);
    dispatch_clock.set(135);
    assert_eq!(
        control
            .heartbeat(
                &exact,
                WorkerHeartbeat {
                    sequence: 3,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: [WorkerCredentialObservation::available(
                        rotated, 135, 165,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap()
            .mutation,
        RegistryMutation::Applied,
        "R3"
    );
    assert_eq!(
        control
            .heartbeat(
                &exact,
                WorkerHeartbeat {
                    sequence: 2,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: [WorkerCredentialObservation::available(
                        WorkerCredentialRevision {
                            id: "credential-source-worker-private".into(),
                            revision: 12,
                        },
                        135,
                        165,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                },
            )
            .await
            .unwrap()
            .mutation,
        RegistryMutation::StaleSequence,
        "R5"
    );
    assert_eq!(
        exact_queue
            .claim("ignored", 1_000, &Default::default())
            .await
            .unwrap()
            .expect("R3 rotated revision")
            .request
            .run_id()
            .0,
        "worker-private-rotated",
        "R3+R5"
    );
}

use awaken_dispatch::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchExecution as _, DispatchSettlement as _,
};

use awaken_dispatch_http::WorkerDispatchAuthority;
