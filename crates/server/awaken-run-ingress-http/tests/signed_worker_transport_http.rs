//! The production request identity is wired through lifecycle and dispatch
//! clients over a real socket, including the post-registration incarnation bind.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    DispatchQueue, MemoryDispatchStore, RegisteredWorker, RegistryError, RegistryMutation,
    RunDispatch, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerObservationSource, WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_run_ingress_http::{
    WorkerDispatchService, dispatch_transport_router_with_service,
    worker_environment_warmup_router_with_clock,
};
use awaken_runtime_contract::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_worker_runtime::WorkerControlClient;
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, ManualWorkerClock, SignedWorkerAuthenticator,
    SignedWorkerRequestAuthorizer, WorkerRequestAuthenticator, WorkerSigningCredential,
    WorkerUpstream,
};

#[derive(Default)]
struct RecordingSessionControl {
    projection: Mutex<Option<awaken_session_contract::FrozenSessionProjection>>,
    activations: Mutex<usize>,
    acknowledgements: Mutex<usize>,
    failures: Mutex<usize>,
    begins: Mutex<Vec<awaken_session_contract::BeginSessionRealization>>,
    begin_failure: Mutex<Option<awaken_session_contract::SessionRealizationControlFailure>>,
}

#[derive(Default)]
struct RecordingSessionWorkAuthority {
    owner: Mutex<Option<String>>,
    acquisitions: AtomicUsize,
    releases: AtomicUsize,
    acquired_sessions: Mutex<Vec<String>>,
    released_sessions: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::work_queue::SessionWorkLeaseAuthority
    for RecordingSessionWorkAuthority
{
    async fn acquire_session_work(
        &self,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
        _acquisition: awaken_session_contract::work_queue::SessionWorkAcquisition,
    ) -> Result<
        awaken_session_contract::work_queue::SessionWorkOwnership,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        self.acquired_sessions
            .lock()
            .unwrap()
            .push(session_id.to_string());
        let mut owner = self.owner.lock().unwrap();
        if owner
            .as_deref()
            .is_some_and(|current| current != worker_owner)
        {
            return Ok(awaken_session_contract::work_queue::SessionWorkOwnership::Unowned);
        }
        *owner = Some(worker_owner.to_string());
        Ok(
            awaken_session_contract::work_queue::SessionWorkOwnership::Leased(
                awaken_session_contract::work_queue::SessionWorkLease {
                    work_id: "work-session".into(),
                    environment_id: "env".into(),
                    session_id: session_id.into(),
                    owner: worker_owner.into(),
                    epoch: 1,
                    expires_at_unix_ms: now_ms + 60_000,
                },
            ),
        )
    }

    async fn release_session_work(
        &self,
        session_id: &str,
        worker_owner: &str,
        _now_ms: u64,
    ) -> Result<bool, awaken_session_contract::work_queue::WorkQueueError> {
        self.released_sessions
            .lock()
            .unwrap()
            .push(session_id.to_string());
        let mut owner = self.owner.lock().unwrap();
        if owner.as_deref() != Some(worker_owner) {
            return Ok(false);
        }
        *owner = None;
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn release_worker_session_work(
        &self,
        worker_owner: &str,
    ) -> Result<usize, awaken_session_contract::work_queue::WorkQueueError> {
        let mut owner = self.owner.lock().unwrap();
        if owner.as_deref() != Some(worker_owner) {
            return Ok(0);
        }
        *owner = None;
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(1)
    }
}

fn frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "test.worker",
    );
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment: awaken_session_contract::EnvironmentSnapshot {
                environment_id: "env".into(),
                revision: awaken_session_contract::EnvironmentRevision(1),
                self_hosted: true,
                config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                    "env-fingerprint".into(),
                ),
                sandbox: serde_json::json!({}),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                    inference_holder: holder.clone(),
                    mcp_holder: holder.clone(),
                    resource_holder: holder,
                },
            },
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            model: "model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    awaken_session_contract::FrozenSessionProjection {
        workspace_id: "workspace".into(),
        revision: awaken_session_contract::SessionRevision(2),
        baseline,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        mcp: Vec::new(),
        toolsets: Vec::new(),
        request_context: Vec::new(),
    }
}
#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for RecordingSessionControl {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.begins.lock().unwrap().push(command.clone());
        if let Some(error) = self.begin_failure.lock().unwrap().clone() {
            return Err(error);
        }
        let projection = self
            .projection
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        Ok(awaken_session_contract::SessionRealizationDirective {
            projection,
            lease: awaken_session_contract::SessionRealizationLease {
                owner: command.target.owner,
                runtime_incarnation: command.target.runtime_incarnation,
                epoch: 1,
                expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
            },
            action: awaken_session_contract::SessionRealizationAction::Complete,
        })
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        *self.activations.lock().unwrap() += 1;
        let projection = self
            .projection
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        Ok(awaken_session_contract::SessionRealizationDirective {
            projection,
            lease: command.lease,
            action: awaken_session_contract::SessionRealizationAction::Complete,
        })
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        *self.acknowledgements.lock().unwrap() += 1;
        let projection = self
            .projection
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        Ok(awaken_session_contract::SessionRealizationDirective {
            projection,
            lease: command.lease,
            action: awaken_session_contract::SessionRealizationAction::Complete,
        })
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        *self.failures.lock().unwrap() += 1;
        Ok(())
    }
}

#[derive(Default)]
struct TestWorkerDirectory(Mutex<Option<RegisteredWorker>>);

struct StaticEnvironmentWarmups(Vec<awaken_session_contract::EnvironmentSnapshot>);

#[async_trait::async_trait]
impl awaken_session_contract::EnvironmentWarmupSource for StaticEnvironmentWarmups {
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_contract::EnvironmentSnapshot>, String> {
        Ok(self.0.clone())
    }
}

#[async_trait::async_trait]
impl WorkerObservationSource for TestWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(self.0.lock().unwrap().clone().into_iter().collect())
    }
}

#[async_trait::async_trait]
impl WorkerDirectory for TestWorkerDirectory {
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let record = RegisteredWorker {
            snapshot: WorkerSnapshot {
                identity: WorkerIdentity::new(
                    registration.worker_id,
                    registration.incarnation_id,
                    1,
                ),
                state: WorkerState::Starting,
                capability_fingerprint: registration.manifest.fingerprint().unwrap(),
                manifest: registration.manifest,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
            heartbeat_sequence: 0,
            registered_at_ms: now_ms,
            heartbeat_at_ms: now_ms,
            drain_deadline_ms: None,
        };
        *self.0.lock().unwrap() = Some(record.clone());
        Ok(record)
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        let mut current = self.0.lock().unwrap();
        let Some(record) = current.as_mut() else {
            return Ok(RegistryMutation::NotFound);
        };
        if &record.snapshot.identity != identity {
            return Ok(RegistryMutation::StaleIncarnation);
        }
        record.snapshot.state = if heartbeat.ready {
            WorkerState::Ready
        } else {
            WorkerState::Starting
        };
        record.snapshot.in_flight = heartbeat.in_flight;
        record.snapshot.warm_environment_shapes = heartbeat.warm_environment_shapes;
        record.snapshot.credential_observations = heartbeat.credential_observations;
        record.snapshot.expires_at_ms = now_ms.saturating_add(ttl_ms);
        record.heartbeat_sequence = heartbeat.sequence;
        record.heartbeat_at_ms = now_ms;
        Ok(RegistryMutation::Applied)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .clone()
            .filter(|record| record.snapshot.identity.worker_id == worker_id))
    }

    async fn expire(&self, _now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        Ok(Vec::new())
    }
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("signed-run".into()),
        ThreadId("signed-thread".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("signed-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("signed-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("signed-catalog".into()),
                instructions: String::new(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "genai"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("signed-snapshot-fingerprint".into()),
        },
        vec![Message::text(
            MessageId("signed-input".into()),
            Role::User,
            "run",
        )],
    )
}

/// Shared Worker-auth middleware decision table and FMECA:
/// F1 warmup projection accepts bootstrap/stale identity and leaks Environment
/// configuration (S8 O3 D3, RPN72); F2 warmup route invents a second auth decision
/// path (S8 O3 D4, RPN96). Both are mitigated by the existing signed middleware and
/// current-directory identity verifier used by all Worker control routes.
/// C1 valid route-bound signed bootstrap assertion -> registration only; C2 the
/// same bootstrap assertion used as an allocated incarnation -> reject before
/// dispatch; C3 valid incarnation-bound assertion -> heartbeat and dispatch
/// and warmup handlers receive one verified context; C4 invalid/replayed assertion
/// -> HTTP 401 before a handler. Effects: E1 register only, E2 reject, E3 return
/// current warmup projection. Decision table: A1 C1->E1; A2 C2->E2;
/// A3 C3->E3; A4 C4->E2. The assertions below cover A1-A4 over real HTTP.
#[tokio::test(flavor = "multi_thread")]
async fn signed_identity_covers_register_heartbeat_and_dispatch() {
    let clock = Arc::new(ManualWorkerClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "signed-http-worker",
        "rotation-1",
        "credential-1",
        b"test-only-signing-secret".to_vec(),
    )
    .unwrap();
    let authenticator = SignedWorkerAuthenticator::new(credential.clone())
        .with_clock(clock.clone())
        .with_time_policy(60_000, 0);
    let authenticator: Arc<dyn WorkerRequestAuthenticator> = Arc::new(authenticator);
    let directory = Arc::new(TestWorkerDirectory::default());
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let session_control = Arc::new(RecordingSessionControl::default());
    *session_control.projection.lock().unwrap() = Some(frozen_projection());
    let session_work = Arc::new(RecordingSessionWorkAuthority::default());
    let service = WorkerDispatchService::new(
        dispatch.clone(),
        authenticator.clone(),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )
    .with_worker_directory(directory.clone(), 30_000)
    .with_session_control(session_control.clone())
    .with_session_work_authority(session_work.clone());
    let warmup = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "signed-env".into(),
        revision: awaken_session_contract::EnvironmentRevision(3),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("signed-shape".into()),
        sandbox: serde_json::json!({}),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: Some("registry.example/env@sha256:signed".into()),
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let router = dispatch_transport_router_with_service(Arc::new(service)).merge(
        worker_environment_warmup_router_with_clock(
            Arc::new(StaticEnvironmentWarmups(vec![warmup])),
            directory,
            authenticator,
            clock.clone(),
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let unauthenticated = reqwest::Client::new()
        .post(format!("http://{address}/v1/worker/register"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("C4 unauthenticated request reaches middleware");
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "C4"
    );

    let authorizer = Arc::new(
        SignedWorkerRequestAuthorizer::new(credential)
            .with_clock(clock.clone())
            .with_assertion_ttl_ms(10_000),
    );
    let bootstrap = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id("signed-http-worker")
        .with_request_authorizer(authorizer);
    let manifest = WorkerManifest {
        build_digest: "signed-http-build".into(),
        ..WorkerManifest::default()
    };
    let registered = WorkerControlClient::new(bootstrap.clone())
        .register("signed-http-boot", manifest)
        .await
        .expect("bootstrap assertion registers the Worker");
    assert!(
        WorkerControlClient::new(bootstrap.clone())
            .heartbeat(
                &registered.snapshot.identity,
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
            .is_err(),
        "a bootstrap assertion cannot act as an allocated incarnation"
    );
    assert!(
        WorkerControlClient::new(bootstrap.clone())
            .current_environment_warmups(&registered.snapshot.identity)
            .await
            .is_err(),
        "A2 bootstrap assertion cannot read the Worker warmup projection"
    );

    let upstream = bootstrap.with_worker_identity(registered.snapshot.identity.clone());
    let heartbeat = WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
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
        .expect("incarnation-bound assertion heartbeats");
    assert_eq!(heartbeat, RegistryMutation::Applied);
    let warmups = WorkerControlClient::new(upstream.clone())
        .current_environment_warmups(&registered.snapshot.identity)
        .await
        .expect("A3 incarnation-bound assertion reads current warmups");
    assert_eq!(warmups.len(), 1, "A3");
    assert_eq!(warmups[0].environment_id, "signed-env", "A3");

    let queue = awaken_worker_runtime::dispatch_transport_with_upstream(
        &upstream,
        registered.snapshot.identity.clone(),
    );
    queue
        .enqueue(RunDispatch::new(activation()))
        .await
        .expect("signed dispatch enqueue");
    let claimed = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            10_000,
            &Default::default(),
        )
        .await
        .expect("signed dispatch claim")
        .expect("queued run is claimable");
    assert_eq!(
        claimed.lease.owner,
        registered.snapshot.identity.lease_owner()
    );
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
    assert!(
        queue
            .claim_is_current(&claim, 10_000)
            .await
            .expect("signed exact-claim verification")
    );
    assert_eq!(session_work.acquisitions.load(Ordering::SeqCst), 1);

    // Cause graph: signed exact incarnation -> live registry lease -> identity
    // owns the exact Run claim -> guarded Run thread equals Session -> resume the
    // already-frozen projection while the epoch guard is held. Every failed
    // cause rejects before Control mutation.
    //
    // | Rule | Identity | Claim owner/epoch | Session=thread | Effect |
    // |---|---|---|---|---|
    // | T1 | exact/live | exact/live | yes | resume frozen projection |
    // | T2 | exact/live | wrong owner | yes | reject before Control |
    // | T3 | exact/live | exact/live | no | reject before Control |
    // | T4 | exact/live | stale/expired | yes | reject before Control |
    // | T5 | exact/live | Session lease exact | - | activate reaches Control |
    // | T6 | wrong incarnation | Session lease exact | - | reject before Control |
    // | T7 | exact/live | Session lease expired | - | reject before Control |
    // | T8 | exact/live | Session lease exact | - | acknowledge reaches Control |
    // | T9 | exact/live | Session lease exact | - | failure reaches Control |
    // | T10 | exact/live | explicit renewal within registry lease | - | begin reaches Control |
    // | T11 | exact/live | implicit/non-renew begin | - | reject before Control |
    // | T12 | exact/live | renewal beyond registry lease | - | reject before Control |
    // | T13 | exact/live | exact/live | wrong Session | reject resume before Control |
    // | T14 | exact/live | exact/live | frozen Session | mark claim-authorized reassignment |
    // | T15 | exact/live | renew+reassign | - | reject contradictory authority |
    // | T16 | exact/live | explicit renewal | Control NotReady | preserve typed reply |
    // | T17 | exact/live | Work owned by other Worker | yes | reject resume before Control |
    // | T18 | exact/live | Work owner changes before phase | - | reject phase before Control |
    // | T19 | exact/live | exact Work owner | claim check | atomically renew Work |
    // | T20 | exact/live | exact Work owner | settle | release Work, then settle Run |
    // | T21 | exact/live | active Work remains | deregister | release exact incarnation |
    // | T22 | child Run | parent Work exact | verify/resume | use parent Session affinity |
    // | T23 | child Run | borrowed parent Work | settle | retain Work for waiting parent |
    let client = WorkerControlClient::new(upstream.clone());
    *session_work.owner.lock().unwrap() = Some("another-worker-incarnation".into());
    assert!(
        client
            .resume_session(&registered.snapshot.identity, &claim, "signed-thread")
            .await
            .is_err(),
        "T17"
    );
    assert!(session_control.begins.lock().unwrap().is_empty(), "T17");
    *session_work.owner.lock().unwrap() = None;
    let resumed = client
        .resume_session(&registered.snapshot.identity, &claim, "signed-thread")
        .await
        .expect("T1 exact signed claim resumes")
        .expect("T1 frozen Session has a realization directive");
    assert_eq!(resumed.projection, frozen_projection(), "T1");
    assert!(
        session_control.begins.lock().unwrap()[0]
            .target
            .reassign_existing_lease,
        "T14"
    );
    assert!(
        client
            .resume_session(&registered.snapshot.identity, &claim, "another-session")
            .await
            .is_err(),
        "T13"
    );
    let mut wrong_owner = claim.clone();
    wrong_owner.owner = "another-owner".into();
    assert!(
        client
            .resume_session(&registered.snapshot.identity, &wrong_owner, "signed-thread")
            .await
            .is_err(),
        "T2"
    );
    assert_eq!(session_control.begins.lock().unwrap().len(), 1, "T2/T13");
    let realization_lease = resumed.lease.clone();
    let renewal = awaken_session_contract::BeginSessionRealization {
        session_id: "signed-thread".into(),
        target: awaken_session_contract::SessionRealizationTarget {
            owner: registered.snapshot.identity.worker_id.clone(),
            runtime_incarnation: registered.snapshot.identity.lease_owner(),
            lease_expires_at_unix_ms: realization_lease.expires_at_unix_ms,
            renew_existing_lease: true,
            reassign_existing_lease: false,
        },
    };
    client
        .begin_session_realization(&registered.snapshot.identity, renewal.clone())
        .await
        .expect("T10");
    assert_eq!(session_control.begins.lock().unwrap().len(), 2, "T10");
    *session_control.begin_failure.lock().unwrap() =
        Some(awaken_session_contract::SessionRealizationControlFailure::NotReady);
    assert!(
        matches!(
            client
                .begin_session_realization(&registered.snapshot.identity, renewal.clone())
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        ),
        "T16"
    );
    *session_control.begin_failure.lock().unwrap() = None;
    let mut implicit = renewal.clone();
    implicit.target.renew_existing_lease = false;
    assert!(
        client
            .begin_session_realization(&registered.snapshot.identity, implicit)
            .await
            .is_err(),
        "T11"
    );
    let mut excessive = renewal;
    excessive.target.lease_expires_at_unix_ms = u64::MAX;
    assert!(
        client
            .begin_session_realization(&registered.snapshot.identity, excessive.clone())
            .await
            .is_err(),
        "T12"
    );
    assert_eq!(
        session_control.begins.lock().unwrap().len(),
        3,
        "T11/T12/T16"
    );
    let mut contradictory = excessive;
    contradictory.target.lease_expires_at_unix_ms = realization_lease.expires_at_unix_ms;
    contradictory.target.reassign_existing_lease = true;
    assert!(
        client
            .begin_session_realization(&registered.snapshot.identity, contradictory)
            .await
            .is_err(),
        "T15"
    );
    assert_eq!(session_control.begins.lock().unwrap().len(), 3, "T15/T16");
    *session_work.owner.lock().unwrap() = Some("another-worker-incarnation".into());
    assert!(
        client
            .activate_session_realization(
                &registered.snapshot.identity,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "signed-thread".into(),
                    lease: realization_lease.clone(),
                    prepared_resource_revision: None,
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .is_err(),
        "T18"
    );
    assert_eq!(*session_control.activations.lock().unwrap(), 0, "T18");
    *session_work.owner.lock().unwrap() = Some(registered.snapshot.identity.lease_owner());
    client
        .activate_session_realization(
            &registered.snapshot.identity,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease.clone(),
                prepared_resource_revision: None,
                mcp_receipts: Vec::new(),
            },
        )
        .await
        .expect("T5");
    assert_eq!(*session_control.activations.lock().unwrap(), 1, "T5");

    let wrong_identity = WorkerIdentity::new("signed-http-worker", "another-boot", 999);
    assert!(
        client
            .activate_session_realization(
                &wrong_identity,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "signed-thread".into(),
                    lease: realization_lease.clone(),
                    prepared_resource_revision: None,
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .is_err(),
        "T6"
    );
    assert_eq!(*session_control.activations.lock().unwrap(), 1, "T6");

    let mut expired_lease = realization_lease.clone();
    expired_lease.expires_at_unix_ms = 0;
    assert!(
        client
            .activate_session_realization(
                &registered.snapshot.identity,
                awaken_session_contract::ActivateSessionRealization {
                    session_id: "signed-thread".into(),
                    lease: expired_lease,
                    prepared_resource_revision: None,
                    mcp_receipts: Vec::new(),
                },
            )
            .await
            .is_err(),
        "T7"
    );
    assert_eq!(*session_control.activations.lock().unwrap(), 1, "T7");

    client
        .acknowledge_session_realization(
            &registered.snapshot.identity,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease.clone(),
                published: Vec::new(),
                drained: Vec::new(),
            },
        )
        .await
        .expect("T8");
    assert_eq!(*session_control.acknowledgements.lock().unwrap(), 1, "T8");

    client
        .fail_session_realization(
            &registered.snapshot.identity,
            awaken_session_contract::FailSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease,
                prepared_resource_revision: None,
                retryable: false,
                reason: "test failure".into(),
            },
        )
        .await
        .expect("T9");
    assert_eq!(*session_control.failures.lock().unwrap(), 1, "T9");

    // FMECA T22/T23: an outcome/delegated child has its own Run/thread
    // lifecycle but borrows the parent's Session Environment. Addressing Work
    // by the child thread deadlocks behind the waiting parent's one active
    // Environment lease; releasing the borrowed Work at child settlement
    // fences the still-running parent. The existing session_thread_id affinity
    // is therefore used for admission/resume, while only a root Run releases.
    let mut child_activation = activation();
    child_activation.run_id = RunId("signed-child-run".into());
    child_activation.thread_id = ThreadId("signed-child-thread".into());
    queue
        .enqueue(RunDispatch::new(child_activation).for_session(ThreadId("signed-thread".into())))
        .await
        .expect("T22 child dispatch");
    let child = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            10_000,
            &Default::default(),
        )
        .await
        .expect("T22 child claim")
        .expect("T22 child is independently claimable");
    let child_claim = awaken_run_ingress::RunClaim::from(&child.lease);
    session_work.acquired_sessions.lock().unwrap().clear();
    assert!(
        queue
            .claim_is_current(&child_claim, 10_000)
            .await
            .expect("T22 child claim verification"),
        "T22"
    );
    client
        .resume_session(&registered.snapshot.identity, &child_claim, "signed-thread")
        .await
        .expect("T22 child resumes parent Session")
        .expect("T22 parent projection remains frozen");
    assert!(
        session_work
            .acquired_sessions
            .lock()
            .unwrap()
            .iter()
            .all(|session_id| session_id == "signed-thread"),
        "T22 every child Work check uses the parent Session"
    );
    let releases_before_child = session_work.releases.load(Ordering::SeqCst);
    queue
        .settle(
            &child_claim.run_id,
            child_claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("T23 child settlement");
    assert_eq!(
        session_work.releases.load(Ordering::SeqCst),
        releases_before_child,
        "T23 child settlement retains the parent's Work"
    );

    // FMECA T20: leaving the outer Session Work active after its subordinate Run
    // settles blocks every queued Session in the same Environment. The exact
    // owner release is part of the private registered-Worker settlement chain;
    // public custom Workers retain their official explicit stop call.
    assert_eq!(
        queue
            .settle(
                &claim.run_id,
                claim.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("T20 signed settlement"),
        awaken_run_ingress::SettleOutcome::Applied,
        "T20"
    );
    assert_eq!(session_work.releases.load(Ordering::SeqCst), 1, "T20");
    assert_eq!(
        session_work.released_sessions.lock().unwrap().as_slice(),
        ["signed-thread"],
        "T20 only the root Run releases its Session Work"
    );

    let mut next_activation = activation();
    next_activation.run_id = RunId("signed-run-next".into());
    next_activation.thread_id = ThreadId("signed-thread-next".into());
    queue
        .enqueue(RunDispatch::new(next_activation))
        .await
        .expect("post-settlement claim fixture");
    let claimed = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            10_001,
            &Default::default(),
        )
        .await
        .expect("post-settlement claim")
        .expect("released Session Work permits the next Run");
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
    assert!(
        queue
            .claim_is_current(&claim, 10_001)
            .await
            .expect("T20 next Session acquires released Work"),
        "T20"
    );

    let begins_before_expired_claim = session_control.begins.lock().unwrap().len();
    clock.set(20_000);
    WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 1,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .expect("registry lease remains live while the dispatch lease expires");
    clock.set(claimed.lease.expires_ms + 1);
    assert!(
        !queue
            .claim_is_current(&claim, claimed.lease.expires_ms + 1)
            .await
            .expect("signed expired-claim verification")
    );
    assert!(
        WorkerControlClient::new(upstream.clone())
            .resume_session(&registered.snapshot.identity, &claim, "signed-thread")
            .await
            .is_err(),
        "T4"
    );
    assert_eq!(
        session_control.begins.lock().unwrap().len(),
        begins_before_expired_claim,
        "T4"
    );

    // T22: a registered Worker that loses subordinate admission can return its
    // exact unstarted claim immediately; a replay from the prior epoch is fenced.
    assert!(
        queue
            .relinquish_claim(&claim)
            .await
            .expect("signed relinquish")
            .applied(),
        "T22"
    );
    WorkerControlClient::new(upstream.clone())
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 3,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
        )
        .await
        .expect("relinquish removes the Run from Worker in-flight capacity");
    let replacement = queue
        .claim(
            &registered.snapshot.identity.lease_owner(),
            30_000,
            claimed.lease.expires_ms + 1,
            &Default::default(),
        )
        .await
        .expect("reclaim relinquished Run")
        .expect("relinquished Run is pending without waiting for lease expiry");
    assert_eq!(replacement.lease.epoch, claim.epoch + 1, "T22");
    assert!(
        !queue
            .relinquish_claim(&claim)
            .await
            .expect("stale relinquish is a fenced no-op")
            .applied(),
        "T22"
    );

    // FMECA T21: graceful restart can begin after the committed answer becomes
    // visible but before asynchronous Run settlement. Deregistration is the last
    // exact-incarnation boundary and releases that residual Work immediately;
    // crash recovery deliberately remains TTL-based.
    *session_work.owner.lock().unwrap() = Some(registered.snapshot.identity.lease_owner());
    WorkerControlClient::new(upstream.clone())
        .deregister(&registered.snapshot.identity)
        .await
        .expect("T21 exact deregistration");
    assert_eq!(session_work.releases.load(Ordering::SeqCst), 2, "T21");
}
