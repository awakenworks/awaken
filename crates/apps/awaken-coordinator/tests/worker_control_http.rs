// @formal evidence worker_credential.stale_heartbeat_never_rolls_back_revision
use awaken_dispatch::ClaimedCheckpointTransport as _;
use awaken_worker_contract::WorkerCurrentSource;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::credential::CredentialRef;
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_coordinator_client::HttpDispatchQueue;
use awaken_coordinator_client::{WorkerControlClient, WorkerRegistrationError};
use awaken_dispatch::{
    MemoryDispatchStore, PlacementRequirements, RunDispatch, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_lease_testkit::ManualClock;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryStreamCheckpointStore;
use awaken_worker_registry::{
    MemoryWorkerDirectory, RegistryMutation, WorkerCredentialObservation, WorkerHeartbeat,
    WorkerManifest, WorkerState,
};
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, WorkerUpstream,
};

// Registration requires explicit current protocol evidence. Default/ANY remains
// a negative transport case; positive registry/credential rules use this owner.
// A current Worker also offers the compiled prepared/journal wire codecs a
// current Native request requires.
fn current_manifest() -> WorkerManifest {
    let mut manifest = WorkerManifest {
        runtime_protocol: awaken_worker_contract::VersionRange::exact(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        ),
        ..Default::default()
    };
    manifest.capabilities.extend(
        awaken_dispatch_testkit::worker_http::current_dispatch_codec_manifest().capabilities,
    );
    manifest
}

#[derive(Default)]
struct RecordingSessionPreparation {
    claims: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionPreparationControl for RecordingSessionPreparation {
    async fn claim_next_session_preparation(
        &self,
        _target: awaken_session_execution_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionPreparationAssignment>,
        awaken_session_execution_contract::SessionRealizationControlFailure,
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
    let target = awaken_session_execution_contract::SessionRealizationTarget {
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
                source_retention: Default::default(),
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
    let observed = CredentialRef {
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
                    source_retention: Default::default(),
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
                source_retention: Default::default(),
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
    // A current producer publishes the Worker codecs its request requires.
    RunDispatch::new(activation)
        .with_placement(placement)
        .with_worker_codec_requirements()
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
                source_retention: Default::default(),
            },
        )
        .await
        .unwrap();

    // The claiming client is the same current Worker: it decodes the journal
    // codec its registration offered.
    let client = HttpDispatchQueue::new(
        format!("http://{address}"),
        registered.worker.snapshot.identity.clone(),
    )
    .with_worker_manifest(current_manifest());
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
    let required = CredentialRef {
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
                .with_placement(placement)
                .with_worker_codec_requirements(),
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
        credential: CredentialRef,
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
                    source_retention: Default::default(),
                },
            )
            .await
            .unwrap();
        registered.worker.snapshot.identity
    }

    let wrong = ready_worker(
        address,
        "worker-wrong-revision",
        CredentialRef {
            id: required.id.clone(),
            revision: required.revision - 1,
        },
    )
    .await;
    let wrong_queue = HttpDispatchQueue::new(format!("http://{address}"), wrong)
        .with_worker_manifest(current_manifest());
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
    let exact_queue = HttpDispatchQueue::new(format!("http://{address}"), exact.clone())
        .with_worker_manifest(current_manifest());
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
                        CredentialRef {
                            id: "credential-source-worker-private".to_string(),
                            revision: 12,
                        },
                        130,
                        160,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                    source_retention: Default::default(),
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

    let rotated = CredentialRef {
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
            .with_placement(rotated_placement)
            .with_worker_codec_requirements(),
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
                    source_retention: Default::default(),
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
                        CredentialRef {
                            id: "credential-source-worker-private".into(),
                            revision: 12,
                        },
                        135,
                        165,
                    )]
                    .into_iter()
                    .collect(),
                    acp_capability_observations: Default::default(),
                    source_retention: Default::default(),
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

mod environment_renewal {
    use super::*;

    use awaken_session_application_contract::SessionWorkGrant;
    use awaken_session_application_contract::SessionWorkerLeaseAuthority;
    use awaken_session_application_contract::{SessionWorkAcquisition, SessionWorkOwnership};
    use awaken_session_configuration_contract::resource::SessionRealizationLease;
    use awaken_session_contract::{
        IdempotencyRecord, PersistedSession, SessionEnvironmentEffectAuthorization,
        SessionEnvironmentEffectIntent, SessionEnvironmentEffectKind, SessionEnvironmentReceipt,
        SessionEnvironmentState,
    };
    use awaken_session_contract::{SessionRepository, SessionRepositoryReads};
    use {
        awaken_session_configuration_contract::{
            EnvironmentFingerprint, EnvironmentSnapshot, SessionBaseline, SessionBaselineInputs,
        },
        awaken_session_types::SessionRuntimePlacement,
    };

    // This fixture supplies a selected Work-port observation. It owns no Work
    // queue or authorization algorithm; the production permits_effect_for law
    // decides whether the exact or foreign lease permits entering the real Root.
    struct ObservedWork(SessionWorkOwnership);

    #[async_trait::async_trait]
    impl SessionWorkerLeaseAuthority for ObservedWork {
        async fn acquire_session_work(
            &self,
            _session_id: &awaken_session_types::SessionId,
            _worker_owner: &str,
            _acquisition: SessionWorkAcquisition,
        ) -> Result<SessionWorkOwnership, awaken_runtime_contract::runs::RunError> {
            Ok(self.0.clone())
        }
        async fn release_session_work(
            &self,
            _lease: &SessionWorkGrant,
        ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
            Ok(false)
        }
        async fn release_worker_session_work(
            &self,
            _worker_owner: &str,
        ) -> Result<usize, awaken_runtime_contract::runs::RunError> {
            Ok(0)
        }
    }

    fn root(session_id: &str, lease: SessionRealizationLease) -> PersistedSession {
        let environment = EnvironmentSnapshot {
            environment_id: "environment-renewal".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: EnvironmentFingerprint("environment-context".into()),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment,
            runtime_placement: SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model_override: None,
            model: "model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        let mut root = PersistedSession::frozen_with_budget(
            session_id,
            baseline,
            Default::default(),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        root.realization = Some(lease);
        root
    }

    #[tokio::test]
    async fn environment_http_preserves_original_expiry_under_current_root_renewal() {
        // Causes: A authenticated exact Worker identity; R current Registry is
        // live; W exact Work ownership; L Root owner/incarnation/epoch is exact;
        // M Root expiry is nonregressed; C Root lease is live; O original expiry
        // is live/expired. Both original intent and receipt use the same bytes.
        // Effects: E1 only A+R+W+L+M+C admits the original intent with one CAS,
        // then persists the binding with one separate CAS; replay leaves
        // Root revision/identity unchanged; E2 every missing condition rejects
        // authorization and persistence without a Root mutation. O is NOT a
        // precondition for these Environment operations. Decision rules:
        // | rule | O | negated cause | effect |
        // | E1 | expired | none | authorize, persist, exact replay |
        // | E2 | expired | C (Root expired) | reject |
        // | E3 | live | M (Root regressed) | reject |
        // | E4 | expired | L incarnation | reject |
        // | E5 | expired | L epoch | reject |
        // | E8 | expired | L owner | reject |
        // | E6 | expired | R (Registry expired) | reject |
        // | E7 | expired | W (foreign Work) | reject |
        // The real SQLite RepositoryEnvironmentBindingSink owns Root law/CAS;
        // no test sink, renewed receipt, fabricated Grant or second timer exists.
        for rule in [
            "renewed",
            "root-expired",
            "regressed",
            "incarnation",
            "epoch",
            "owner",
            "registry-expired",
            "work-foreign",
        ] {
            let clock = Arc::new(ManualClock::new(100));
            let directory = Arc::new(MemoryWorkerDirectory::new());
            let registry = awaken_worker_registry::WorkerDirectory::register(
                directory.as_ref(),
                awaken_worker_contract::WorkerRegistration {
                    worker_id: format!("worker-{rule}"),
                    incarnation_id: "original-boot".into(),
                    manifest: current_manifest(),
                    create_ledger: None,
                },
                100,
                if rule == "registry-expired" {
                    40
                } else {
                    1_000
                },
            )
            .await
            .unwrap();
            let identity = registry.snapshot.identity;
            let session_id = format!("environment-{rule}");
            let original = SessionRealizationLease {
                owner: identity.worker_id.clone(),
                runtime_incarnation: identity.lease_owner(),
                epoch: 7,
                expires_at_unix_ms: if rule == "regressed" { 300 } else { 120 },
            };
            let mut current = SessionRealizationLease {
                expires_at_unix_ms: 500,
                ..original.clone()
            };
            match rule {
                "root-expired" => current.expires_at_unix_ms = 150,
                "regressed" => current.expires_at_unix_ms = 200,
                "incarnation" => current.runtime_incarnation = "new-root-incarnation".into(),
                "epoch" => {
                    current.epoch =
                        awaken_lease_contract::fence::Epoch::from_persisted(current.epoch)
                            .unwrap()
                            .next()
                            .unwrap()
                            .get();
                }
                "owner" => current.owner = "foreign-root-owner".into(),
                _ => {}
            }
            let repository = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            repository
                .create(
                    "workspace",
                    root(&session_id, current.clone()),
                    IdempotencyRecord {
                        key: format!("create:{session_id}"),
                        payload_hash: format!("payload:{session_id}"),
                    },
                    Vec::new(),
                )
                .await
                .unwrap();
            let before = repository.get(&session_id.clone().into()).await.unwrap();
            let sink = Arc::new(
                awaken_session_application::RepositoryEnvironmentBindingSink::new(
                    repository.clone(),
                    clock.clone(),
                ),
            );
            let work = awaken_session_types::SessionWorkLease::from_persisted(
                format!("work:{session_id}"),
                "environment-renewal",
                session_id.clone().into(),
                if rule == "work-foreign" {
                    "foreign-owner".into()
                } else {
                    identity.lease_owner()
                },
                (awaken_lease_contract::fence::Epoch::FIRST).get(),
                1_000,
            )
            .map(SessionWorkGrant::from_transfer)
            .expect("canonical grant has a nonzero epoch");
            let service = WorkerDispatchService::new(
                Arc::new(MemoryDispatchStore::new(clock.clone()))
                    as Arc<dyn WorkerDispatchAuthority>,
                Arc::new(HeaderWorkerAuthenticator),
                clock.clone(),
                Arc::new(FixedWorkerLeasePolicy::new(1_000)),
            )
            .with_worker_directory(directory, 1_000)
            .with_session_work_authority(Arc::new(ObservedWork(SessionWorkOwnership::Leased(work))))
            .with_session_environment_bindings(sink);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    dispatch_transport_router_with_service(Arc::new(service)),
                )
                .await
                .unwrap();
            });
            let client = WorkerControlClient::new(
                WorkerUpstream::new(format!("http://{address}"))
                    .with_worker_id(identity.worker_id.clone()),
            );
            clock.set(150);
            assert_eq!(
                awaken_session_configuration_contract::resource::realization_lease_is_live_at(
                    original.expires_at_unix_ms,
                    150
                ),
                rule == "regressed",
                "{rule}: original expiry truly crosses the authority clock"
            );
            let intent = SessionEnvironmentEffectIntent::new(
                session_id.clone(),
                SessionEnvironmentEffectKind::Create,
                Some(original.clone()),
            )
            .for_environment("environment-context");
            let receipt =
                SessionEnvironmentReceipt::from_intent(&intent, "original-binding").unwrap();
            let original_receipt = serde_json::to_value(&receipt).unwrap();
            let authorization = client
                .authorize_session_environment_effect(&identity, &intent)
                .await;
            let admitted = repository.get(&session_id.clone().into()).await.unwrap();
            let persistence = client
                .persist_session_environment_receipt(&identity, &receipt)
                .await;
            let committed = repository.get(&session_id.clone().into()).await.unwrap();
            let replay = if rule == "renewed" {
                Some(
                    client
                        .persist_session_environment_receipt(&identity, &receipt)
                        .await,
                )
            } else {
                None
            };
            let replayed = repository.get(&session_id.clone().into()).await.unwrap();
            server.abort();
            let _ = server.await;
            if rule == "renewed" {
                assert_eq!(
                    authorization.unwrap(),
                    SessionEnvironmentEffectAuthorization::Authorized {
                        intent: Box::new(intent.clone())
                    },
                    "E1"
                );
                assert_eq!(
                    admitted.revision.0,
                    before.revision.0 + 1,
                    "E1 first admission CAS precedes persistence"
                );
                assert_eq!(
                    admitted.environment_materialization().unwrap().pending(),
                    Some(&intent),
                    "E1 exact first intent saved before binding"
                );
                assert!(
                    matches!(persistence.unwrap(), SessionEnvironmentState::Resident { ref binding, .. } if binding == "original-binding"),
                    "E1 persisted exact binding"
                );
                replay.unwrap().unwrap();
                assert_eq!(
                    committed.revision.0,
                    admitted.revision.0 + 1,
                    "E1 one binding CAS after the separate admission CAS"
                );
                assert_eq!(
                    committed.environment_materialization().unwrap().origin(),
                    Some(&intent),
                    "E1 original origin survives"
                );
                assert_eq!(
                    replayed, committed,
                    "E1 replay does not rewrite Root identity or expiry"
                );
                assert_eq!(
                    committed.realization,
                    Some(current),
                    "E1 current Root renewal is retained"
                );
            } else {
                assert!(authorization.is_err(), "{rule}: E2 authorize rejects");
                assert!(persistence.is_err(), "{rule}: E2 persist rejects");
                assert_eq!(admitted, before, "{rule}: E2 no admission mutation");
                assert_eq!(committed, before, "{rule}: E2 no Root mutation");
            }
            assert_eq!(
                serde_json::to_value(&receipt).unwrap(),
                original_receipt,
                "{rule}: original receipt bytes/expiry/identity are never refreshed"
            );
            assert_eq!(
                receipt.realization,
                Some(original),
                "{rule}: retained original lease"
            );
        }
    }

    #[tokio::test]
    async fn active_realization_http_still_rejects_expired_assertions() {
        // C1 exact Registry/Worker identity is live; C2 the submitted realization
        // lease is already expired; C3 activate/ack/fail share the same boundary.
        // Rule A1=C1+C2 for each C3 => HTTP400 before any Control port call.
        // A missing Control port would instead yield HTTP500, so this rejects
        // widening the Environment exception to ordinary active controls.
        let clock = Arc::new(ManualClock::new(100));
        let directory = Arc::new(MemoryWorkerDirectory::new());
        let registered = awaken_worker_registry::WorkerDirectory::register(
            directory.as_ref(),
            awaken_worker_contract::WorkerRegistration {
                worker_id: "active-worker".into(),
                incarnation_id: "boot".into(),
                manifest: current_manifest(),
                create_ledger: None,
            },
            100,
            1_000,
        )
        .await
        .unwrap();
        let identity = registered.snapshot.identity;
        let service = WorkerDispatchService::new(
            Arc::new(MemoryDispatchStore::new(clock.clone())) as Arc<dyn WorkerDispatchAuthority>,
            Arc::new(HeaderWorkerAuthenticator),
            clock.clone(),
            Arc::new(FixedWorkerLeasePolicy::new(1_000)),
        )
        .with_worker_directory(directory, 1_000)
        .with_session_work_authority(Arc::new(ObservedWork(SessionWorkOwnership::NotRequired)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                dispatch_transport_router_with_service(Arc::new(service)),
            )
            .await
            .unwrap();
        });
        let lease = SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 1,
            expires_at_unix_ms: 100,
        };
        clock.set(150);
        let http = awaken_outbound_http::InternalHttpClient::new(std::time::Duration::from_secs(5))
            .unwrap();
        let mut responses = Vec::new();
        for operation in ["activate", "acknowledge", "fail"] {
            let mut command = serde_json::json!({"session_id":"active-session", "lease":lease});
            if operation == "fail" {
                command["reason"] = serde_json::json!("failure");
            }
            let response = http
                .send(
                    &awaken_outbound_http::JsonRequest::post(format!(
                        "http://{address}/v1/worker/session/realization/{operation}"
                    ))
                    .header("x-awaken-worker-id", identity.worker_id.clone())
                    .json_body(
                        serde_json::json!({"identity":identity,"command":command}).to_string(),
                    ),
                )
                .await
                .unwrap();
            responses.push((operation, response.status(), response.body().to_owned()));
        }
        server.abort();
        let _ = server.await;
        for (operation, status, body) in responses {
            assert_eq!(status, 400, "A1 {operation}: {body}");
            assert!(
                body.contains("Session realization lease is not owned"),
                "A1 {operation}: authorization, not DTO failure"
            );
        }
    }
}
