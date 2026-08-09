//! The production request identity is wired through lifecycle and dispatch
//! clients over a real socket, including the post-registration incarnation bind.

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
struct RecordingApplicationContributions {
    contributions: Mutex<Vec<awaken_session_contract::ApplicationSessionContribution>>,
    projection: Mutex<Option<awaken_session_contract::FrozenSessionProjection>>,
    activations: Mutex<usize>,
    acknowledgements: Mutex<usize>,
    failures: Mutex<usize>,
    begins: Mutex<Vec<awaken_session_contract::BeginSessionRealization>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::ApplicationSessionContributionApi
    for RecordingApplicationContributions
{
    async fn contribute_application(
        &self,
        contribution: awaken_session_contract::ApplicationSessionContribution,
    ) -> Result<
        awaken_session_contract::ApplicationSessionContributionReceipt,
        awaken_session_contract::ApplicationSessionContributionFailure,
    > {
        self.contributions
            .lock()
            .unwrap()
            .push(contribution.clone());
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let input_receipt = awaken_session_contract::ApplicationContributionReceipt::from_input(
            contribution.application_fingerprint,
            &contribution.input,
        );
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: serde_json::json!({}),
                    sandbox_provisioning: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "agent".into(),
                model: "model".into(),
                runtime: None,
                application: Some(input_receipt),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: contribution.input.mounts,
                env: contribution.input.env,
                prompts: contribution.input.prompts,
            },
        );
        let projection = awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            mcp: Vec::new(),
            toolsets: Vec::new(),
        };
        *self.projection.lock().unwrap() = Some(projection.clone());
        Ok(
            awaken_session_contract::ApplicationSessionContributionReceipt {
                outcome: awaken_session_contract::ApplicationContributionOutcome::Committed,
                projection,
            },
        )
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for RecordingApplicationContributions {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.begins.lock().unwrap().push(command.clone());
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
    let contributions = Arc::new(RecordingApplicationContributions::default());
    let service = WorkerDispatchService::new(
        dispatch.clone(),
        authenticator.clone(),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )
    .with_worker_directory(directory.clone(), 30_000)
    .with_application_session_control(contributions.clone());
    let warmup = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "signed-env".into(),
        revision: awaken_session_contract::EnvironmentRevision(3),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("signed-shape".into()),
        sandbox: serde_json::json!({}),
        sandbox_provisioning: Default::default(),
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

    // Cause graph: signed exact incarnation -> live registry lease -> identity
    // owns exact Run claim -> guarded Run thread equals Session -> invoke the one
    // Control contribution port while the epoch guard is held. Every failed cause
    // rejects before the port and therefore before any Session mutation.
    //
    // | Rule | Identity | Claim owner/epoch | Session=thread | Effect |
    // |---|---|---|---|---|
    // | T1 | exact/live | exact/live | T | one contribution + receipt |
    // | T2 | exact/live | wrong owner | T | reject, no contribution |
    // | T3 | exact/live | exact/live | F | reject, no contribution |
    // | T4 | exact/live | stale/expired | T | reject, no contribution |
    // | T5 | exact/live | Session lease exact | - | activate reaches same control |
    // | T6 | wrong incarnation | Session lease exact | - | reject before control |
    // | T7 | exact/live | Session lease expired | - | reject before control |
    // | T8 | exact/live | Session lease exact | - | acknowledge reaches same control |
    // | T9 | exact/live | Session lease exact | - | failure reaches same control |
    // | T10 | exact/live | explicit renewal within registry lease | - | begin reaches control |
    // | T11 | exact/live | implicit/non-renew begin | - | reject before control |
    // | T12 | exact/live | renewal beyond registry lease | - | reject before control |
    // | T13 | exact/live | exact/live | frozen Session | resume without contribution |
    // | T14 | exact/live | exact/live | wrong Session | reject resume before control |
    // | T15 | exact/live | exact/live | contribution/resume | mark claim-authorized reassignment |
    // | T16 | exact/live | renew+reassign | - | reject contradictory authority before control |
    let client = WorkerControlClient::new(upstream.clone());
    let contribution = awaken_session_contract::ApplicationSessionContribution {
        session_id: "signed-thread".into(),
        application_fingerprint: "plan-a".into(),
        input: awaken_session_contract::ApplicationSessionInput::default(),
    };
    let receipt = client
        .contribute_application(&registered.snapshot.identity, &claim, contribution.clone())
        .await
        .expect("T1 exact signed claim contributes");
    assert_eq!(
        receipt.contribution.outcome,
        awaken_session_contract::ApplicationContributionOutcome::Committed
    );
    assert_eq!(contributions.contributions.lock().unwrap().len(), 1, "T1");
    let resumed = client
        .resume_application_session(&registered.snapshot.identity, &claim, "signed-thread")
        .await
        .expect("T13 claim-bound frozen Session resume")
        .expect("T13 frozen Session has a realization directive");
    assert_eq!(resumed.projection, receipt.contribution.projection, "T13");
    assert_eq!(contributions.contributions.lock().unwrap().len(), 1, "T13");
    {
        let begins = contributions.begins.lock().unwrap();
        assert!(begins[0].target.reassign_existing_lease, "T15 contribution");
        assert!(begins[1].target.reassign_existing_lease, "T15 resume");
    }
    assert!(
        client
            .resume_application_session(&registered.snapshot.identity, &claim, "another-session",)
            .await
            .is_err(),
        "T14"
    );
    let mut wrong_owner = claim.clone();
    wrong_owner.owner = "another-owner".into();
    assert!(
        client
            .contribute_application(
                &registered.snapshot.identity,
                &wrong_owner,
                contribution.clone(),
            )
            .await
            .is_err(),
        "T2"
    );
    let mut wrong_session = contribution.clone();
    wrong_session.session_id = "another-session".into();
    assert!(
        client
            .contribute_application(&registered.snapshot.identity, &claim, wrong_session)
            .await
            .is_err(),
        "T3"
    );
    assert_eq!(
        contributions.contributions.lock().unwrap().len(),
        1,
        "T2/T3"
    );
    let realization_lease = receipt.realization.lease.clone();
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
    assert_eq!(contributions.begins.lock().unwrap().len(), 3, "T10");
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
    assert_eq!(contributions.begins.lock().unwrap().len(), 3, "T11/T12");
    let mut contradictory = excessive;
    contradictory.target.lease_expires_at_unix_ms = realization_lease.expires_at_unix_ms;
    contradictory.target.reassign_existing_lease = true;
    assert!(
        client
            .begin_session_realization(&registered.snapshot.identity, contradictory)
            .await
            .is_err(),
        "T16"
    );
    assert_eq!(contributions.begins.lock().unwrap().len(), 3, "T16");
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
    assert_eq!(*contributions.activations.lock().unwrap(), 1, "T5");

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
    assert_eq!(*contributions.activations.lock().unwrap(), 1, "T6");

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
    assert_eq!(*contributions.activations.lock().unwrap(), 1, "T7");

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
    assert_eq!(*contributions.acknowledgements.lock().unwrap(), 1, "T8");

    client
        .fail_session_realization(
            &registered.snapshot.identity,
            awaken_session_contract::FailSessionRealization {
                session_id: "signed-thread".into(),
                lease: realization_lease,
                prepared_resource_revision: None,
                reason: "test failure".into(),
            },
        )
        .await
        .expect("T9");
    assert_eq!(*contributions.failures.lock().unwrap(), 1, "T9");

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
        WorkerControlClient::new(upstream)
            .contribute_application(&registered.snapshot.identity, &claim, contribution)
            .await
            .is_err(),
        "T4"
    );
    assert_eq!(contributions.contributions.lock().unwrap().len(), 1, "T4");
}
