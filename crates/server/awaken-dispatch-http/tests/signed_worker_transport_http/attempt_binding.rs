// @formal evidence session_execution.one_physical_attempt_per_thread
use super::*;
use awaken_dispatch::DispatchAttemptRecordReads as _;
use awaken_dispatch::{AttemptExecutionBinding, RunClaim, WorkerExecutionBindingSource};
use awaken_session_execution_contract::ApplicationError;
use awaken_worker_contract::{WorkerCurrentSource, WorkerIncarnationSource};

use awaken_dispatch_testkit::worker_http as support;
use serde_json::{Value, json};
use std::future::Future;
use tokio::sync::{Notify, Semaphore};

struct BindingSource {
    result: Mutex<Result<AttemptExecutionBinding, String>>,
    calls: AtomicUsize,
    entered: Notify,
    release: Semaphore,
    preparation: Mutex<Result<(), String>>,
    retention: Mutex<Result<(), String>>,
    retained: Mutex<Vec<WorkerIdentity>>,
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for BindingSource {
    async fn prepare_registration_physical_evidence(
        &self,
        _: &awaken_dispatch::WorkerRegistration,
    ) -> Result<Option<awaken_dispatch::WorkerCreateLedger>, ApplicationError> {
        self.preparation
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)?;
        Ok(None)
    }
    async fn retain_registered_worker_evidence(
        &self,
        worker: &WorkerIdentity,
        _create_ledger: Option<&awaken_dispatch::WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        self.retained.lock().unwrap().push(worker.clone());
        self.retention
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)
    }
    async fn binding_for(
        &self,
        _: &WorkerIdentity,
    ) -> Result<AttemptExecutionBinding, ApplicationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        self.result
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)
    }
}

struct Fixture {
    source: Arc<BindingSource>,
    store: Arc<MemoryDispatchStore>,
    directory: Arc<awaken_worker_registry::MemoryWorkerDirectory>,
    clock: Arc<ManualClock>,
    /// The dispatch store's own clock, held at the same reading as `clock`.
    dispatch_clock: Arc<awaken_lease_testkit::ManualClock>,
    claim: RunClaim,
    upstream: WorkerUpstream,
    server: tokio::task::JoinHandle<()>,
}

#[tokio::test]
async fn ordinary_return_uses_historical_protocol_identity_without_authorizing_other_verbs() {
    use awaken_dispatch::{AttemptPhysicalFact, AttemptReturnCause, DispatchCursor};
    // O1 exact current-protocol incarnation becomes superseded/expired; O2
    // typed cause appears on begin/handoff/close; O3 ordinary Some is repeated
    // or changed. R1 O1+finish -> original binding+local cause only, no current
    // Ready/TTL requirement; R2 O2 -> reject before Provider/slot IO; R3 exact
    // retry is one event, different cause rejects. The original manifest covers
    // CURRENT (5), not an exact old version. U5 covers v3/exact4 Some denial
    // plus None compatibility. Worker JSON cannot carry All/URI.
    let mut f = Fixture::new(true, 5_000).await;
    let identity = f.upstream.worker_identity().unwrap().clone();
    let begin = f.request("begin", f.body());
    f.entered().await;
    f.source.release.add_permits(1);
    assert_eq!(begin.await.unwrap().0, reqwest::StatusCode::OK);
    let original = f
        .store
        .load_physical_attempt(&f.claim.run_id)
        .await
        .unwrap()
        .unwrap();
    let mut observed = f.body();
    observed["return_cause"] = json!(AttemptReturnCause::StructuredReturnObserved);
    let source_calls = f.source.calls.load(Ordering::SeqCst);
    for action in ["begin", "handoff", "close"] {
        let (status, _) = f.request(action, observed.clone()).await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "R2 {action}");
        assert_eq!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap(),
            Some(original.clone())
        );
        assert_eq!(
            f.source.calls.load(Ordering::SeqCst),
            source_calls,
            "R2 no Provider observation"
        );
    }
    let manifest = f
        .directory
        .incarnation(&identity)
        .await
        .unwrap()
        .unwrap()
        .snapshot
        .manifest;
    let next = f
        .directory
        .register(
            WorkerRegistration {
                worker_id: identity.worker_id.clone(),
                incarnation_id: "replacement-boot".into(),
                manifest,
                create_ledger: None,
            },
            50_000,
            30_000,
        )
        .await
        .unwrap();
    assert_ne!(next.snapshot.identity, identity);
    assert!(
        f.directory
            .incarnation(&identity)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .manifest
            .supports_current_runtime_protocol(),
        "R1 immutable original record"
    );
    f.clock.set(50_000);
    let (status, body) = f.request("finish", observed.clone()).await.unwrap();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "R1 late original observation"
    );
    assert_eq!(body["finished"], true);
    let retained = f
        .store
        .attempt_closure_records_after(DispatchCursor::default(), usize::MAX, Some(&f.claim.run_id))
        .await
        .unwrap();
    assert_eq!(retained.events.len(), 1);
    let AttemptPhysicalFact::Acknowledgement { record } = &retained.events[0].record else {
        panic!("R1 ordinary observation cannot become independent closure")
    };
    assert_eq!(record.attempt(), &original);
    assert_eq!(record.cause(), AttemptReturnCause::StructuredReturnObserved);
    assert_eq!(
        f.request("finish", observed.clone()).await.unwrap().0,
        reqwest::StatusCode::OK,
        "R3 response-loss retry"
    );
    observed["return_cause"] = json!(AttemptReturnCause::OperationFutureNotEntered);
    assert!(
        !f.request("finish", observed).await.unwrap().0.is_success(),
        "R3 immutable cause"
    );
    assert_eq!(
        f.store
            .attempt_closure_records_after(
                DispatchCursor::default(),
                usize::MAX,
                Some(&f.claim.run_id)
            )
            .await
            .unwrap(),
        retained
    );
    f.server.abort();
    let _ = (&mut f.server).await;
}

#[tokio::test]
async fn final_incarnation_guard_rechecks_closure_after_provider_and_authority_reads() {
    // G1 provider returns while the original Registry exclusion is held and
    // closure is already queued -> admission cannot pass the final guard.
    // G2 closure commits first -> BAD_REQUEST and no physical slot. The test
    // establishes the closure queue order, without claiming the HTTP task has
    // reached its second read. Guard release is an interleaving gate, never a
    // termination or application readiness receipt.
    let f = Fixture::new(true, 5_000).await;
    let identity = f.upstream.worker_identity().unwrap().clone();
    let request = f.request("begin", f.body());
    f.entered().await;
    let held = f
        .directory
        .lock_incarnation(&identity)
        .await
        .unwrap()
        .unwrap();
    f.source.release.add_permits(1);
    // The closure queues behind the exact guard before admission can own it.
    let directory = f.directory.clone();
    let closing_identity = identity.clone();
    let mut closing = Box::pin(directory.deregister(&closing_identity));
    std::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    held.release().await.unwrap();
    assert_eq!(closing.await.unwrap(), RegistryMutation::Applied);
    let (status, _) = request.await.unwrap();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        f.store
            .load_physical_attempt(&f.claim.run_id)
            .await
            .unwrap()
            .is_none()
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(configured: bool, request_budget_ms: u64) -> Self {
        let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
        let mut manifest = WorkerManifest::default();
        manifest.runtime_protocol.min = 1;
        manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
        let identity = directory
            .register(
                WorkerRegistration {
                    worker_id: "binding-http".into(),
                    incarnation_id: "binding-http-boot".into(),
                    manifest,
                    create_ledger: None,
                },
                10_000,
                30_000,
            )
            .await
            .unwrap()
            .snapshot
            .identity;
        directory
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
                10_000,
                30_000,
            )
            .await
            .unwrap();
        let record = directory
            .current(&identity.worker_id)
            .await
            .unwrap()
            .unwrap();
        let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(10_000));
        let store = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
        store
            .enqueue(
                RunDispatch::new(support::activation("binding-http"))
                    .with_placement(awaken_dispatch::PlacementRequirements::remote_required()),
            )
            .await
            .unwrap();
        let claimed = store
            .claim_compatible(&record.snapshot, 1_000)
            .await
            .unwrap()
            .unwrap();
        let claim = RunClaim::from(&claimed.lease);
        let binding =
            AttemptExecutionBinding::new(identity.clone(), "trusted-cluster", "exact-pod-process")
                .unwrap();
        let source = Arc::new(BindingSource {
            result: Mutex::new(Ok(binding)),
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Semaphore::new(0),
            preparation: Mutex::new(Ok(())),
            retention: Mutex::new(Ok(())),
            retained: Mutex::new(Vec::new()),
        });
        let clock = Arc::new(ManualClock::new(10_000));
        let credential = WorkerSigningCredential::new(
            "binding-http",
            "key",
            "credential",
            b"fixture-only-binding-secret".to_vec(),
        )
        .unwrap();
        let auth = SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
            .with_time_policy(60_000, 0);
        let service = WorkerDispatchService::new(
            store.clone(),
            Arc::new(auth),
            clock.clone(),
            Arc::new(FixedWorkerLeasePolicy::new(request_budget_ms)),
        )
        .with_worker_directory(directory.clone(), 30_000);
        let service = if configured {
            service.with_execution_binding_source(source.clone())
        } else {
            service
        };
        let router = dispatch_transport_router_with_service(Arc::new(service));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let upstream = WorkerUpstream::new(format!("http://{address}"))
            .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
                credential,
                clock.clone(),
            )))
            .with_worker_identity(identity);
        Self {
            source,
            store,
            directory,
            clock,
            dispatch_clock,
            claim,
            upstream,
            server,
        }
    }

    fn body(&self) -> Value {
        json!({"claim": self.claim, "identity": self.upstream.worker_identity()})
    }

    fn request(
        &self,
        verb: &str,
        body: Value,
    ) -> tokio::task::JoinHandle<(reqwest::StatusCode, Value)> {
        let upstream = self.upstream.clone();
        let path = format!("/v1/worker/dispatch/attempt/{verb}");
        tokio::spawn(async move {
            let request = upstream
                .client()
                .post(format!("{}{path}", upstream.base_url()))
                .json(&body);
            let response = upstream
                .authorize("POST", &path, request)
                .unwrap()
                .send()
                .await
                .unwrap();
            let status = response.status();
            let value = response.json().await.unwrap_or(Value::Null);
            (status, value)
        })
    }

    async fn entered(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.source.entered.notified(),
        )
        .await
        .unwrap();
        assert!(
            self.store
                .load_physical_attempt(&self.claim.run_id)
                .await
                .unwrap()
                .is_none(),
            "lookup precedes installation"
        );
    }
}

#[tokio::test]
async fn registration_retention_failure_retries_the_same_durable_identity_before_receipt() {
    // Causes: C1 pre-registration physical preparation fails; C2 it succeeds
    // but the post-registration readback fails; C3 both succeed on retry; C4
    // the original incarnation is then closed. Effects: E1 C1 => no new
    // Registry generation; E2 C2 => one Starting identity and retryable error;
    // E3 C3 => same identity returned after retention; E4 C4 => no reinstall.
    // Rules R0=C1->E1, R1=!C1+C2->E2, R2=!C1+!C2+C3->E3,
    // R3=R2+C4->E4. Actual signed HTTP and Registry own the observations.
    let f = Fixture::new(true, 5_000).await;
    let bootstrap = f.upstream.clone();
    // A bootstrap has the same signer and no incarnation header.
    let credential = WorkerSigningCredential::new(
        "binding-http",
        "key",
        "credential",
        b"fixture-only-binding-secret".to_vec(),
    )
    .unwrap();
    let upstream = WorkerUpstream::new(bootstrap.base_url())
        .with_worker_id("binding-http")
        .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
            credential,
            f.clock.clone(),
        )));
    let control = awaken_coordinator_client::WorkerControlClient::new(upstream);
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = 1;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    f.directory
        .deregister(f.upstream.worker_identity().unwrap())
        .await
        .unwrap();
    let predecessor = f.directory.current("binding-http").await.unwrap().unwrap();
    *f.source.preparation.lock().unwrap() = Err("provider unavailable".into());
    assert!(
        matches!(
            control
                .register_classified("retention-boot", manifest.clone())
                .await,
            Err(awaken_coordinator_client::WorkerRegistrationError::Unavailable(_))
        ),
        "R0 preparation failure is retryable"
    );
    assert_eq!(
        f.directory.current("binding-http").await.unwrap().unwrap(),
        predecessor,
        "R0 no new Registry incarnation before provider binding"
    );
    *f.source.preparation.lock().unwrap() = Ok(());
    *f.source.retention.lock().unwrap() = Err("API unavailable".into());
    assert!(
        matches!(
            control
                .register_classified("retention-boot", manifest.clone())
                .await,
            Err(awaken_coordinator_client::WorkerRegistrationError::Unavailable(_))
        ),
        "R1 exact retryable outcome"
    );
    let original = f.directory.current("binding-http").await.unwrap().unwrap();
    assert_eq!(original.snapshot.state, WorkerState::Starting);
    *f.source.retention.lock().unwrap() = Ok(());
    let receipt = control
        .register_classified("retention-boot", manifest.clone())
        .await
        .unwrap();
    assert_eq!(receipt.worker.snapshot.identity, original.snapshot.identity);
    assert_eq!(
        *f.source.retained.lock().unwrap(),
        [
            original.snapshot.identity.clone(),
            original.snapshot.identity.clone()
        ]
    );
    assert_eq!(f.source.calls.load(Ordering::SeqCst), 0);
    f.directory
        .deregister(&original.snapshot.identity)
        .await
        .unwrap();
    control
        .register_classified("retention-boot", manifest)
        .await
        .unwrap();
    assert_eq!(
        f.source.retained.lock().unwrap().len(),
        2,
        "R3 no reinstall"
    );
}

#[tokio::test]
async fn signed_admission_freezes_only_coordinator_selected_bindings() {
    // Causes: provider configured/absent; first/retry/changed/omitted response;
    // begin/handoff; raw Worker attempts to submit binding/stop evidence.
    // R1 configured+valid => persist exact binding, same-request retry stutters.
    // R2 changed provider coordinate => Conflict, old slot unchanged.
    // R3 unconfigured => legacy None (never a stop proof).
    // R4 unknown wire fields => reject before provider or Dispatch mutation.
    for verb in ["begin", "handoff"] {
        let f = Fixture::new(true, 5_000).await;
        // R11 authenticated body owner/incarnation mismatch or non-current
        // claim => reject before provider I/O. Signed identity is not a right
        // to make provider lookups for arbitrary Run ids/epochs.
        for field in ["owner", "epoch", "run_id"] {
            let mut body = f.body();
            body["claim"][field] = if field == "epoch" {
                json!(
                    awaken_lease_contract::fence::Epoch::from_persisted(f.claim.epoch)
                        .unwrap()
                        .next()
                        .unwrap()
                        .get()
                )
            } else {
                json!("foreign")
            };
            let (status, body) = f.request(verb, body).await.unwrap();
            if field == "owner" {
                assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
            } else {
                assert_eq!(body["admission"], "fenced");
            }
        }
        // R11b crosses begin/handoff with zero/missing grant: wire decoding
        // refuses before provider I/O or physical-slot installation. R11
        // above still covers a well-formed but non-current positive epoch.
        for epoch in [None, Some(0)] {
            let mut body = f.body();
            body["claim"].as_object_mut().unwrap().remove("epoch");
            if let Some(epoch) = epoch {
                body["claim"]["epoch"] = epoch.into();
            }
            assert_eq!(
                f.request(verb, body).await.unwrap().0,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "R11b {verb} {epoch:?}"
            );
            assert!(
                f.store
                    .load_physical_attempt(&f.claim.run_id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let mut body = f.body();
        body["identity"]["incarnation_id"] = json!("foreign-boot");
        assert_eq!(
            f.request(verb, body).await.unwrap().0,
            reqwest::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            f.source.calls.load(Ordering::SeqCst),
            0,
            "R11 no source lookup"
        );
        f.source.release.add_permits(1);
        assert_eq!(
            f.request(verb, f.body()).await.unwrap().1["admission"],
            "applied",
            "R1"
        );
        let saved = f
            .store
            .load_physical_attempt(&f.claim.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.binding,
            Some(f.source.result.lock().unwrap().clone().unwrap())
        );
        f.source.release.add_permits(1);
        assert_eq!(
            f.request(verb, f.body()).await.unwrap().1["admission"],
            "already_applied",
            "R1 lost-response retry"
        );
        *f.source.result.lock().unwrap() = Ok(AttemptExecutionBinding::new(
            f.upstream.worker_identity().unwrap().clone(),
            "trusted-cluster",
            "other-pod",
        )
        .unwrap());
        f.source.release.add_permits(1);
        assert_eq!(
            f.request(verb, f.body()).await.unwrap().0,
            reqwest::StatusCode::CONFLICT,
            "R2"
        );
        assert_eq!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap(),
            Some(saved.clone())
        );
        *f.source.result.lock().unwrap() = Err("provider offline after admission".into());
        // R4 also covers the destructive ACK boundary: invented evidence fields
        // reject the whole request and cannot clear the admitted obligation.
        for field in ["binding", "execution_binding", "stopped"] {
            let mut body = f.body();
            body[field] = json!(true);
            assert_eq!(
                f.request("finish", body).await.unwrap().0,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY
            );
            assert_eq!(
                f.store
                    .load_physical_attempt(&f.claim.run_id)
                    .await
                    .unwrap(),
                Some(saved.clone())
            );
        }
        assert_eq!(
            f.request("finish", f.body()).await.unwrap().0,
            reqwest::StatusCode::OK
        );
        assert_eq!(
            f.source.calls.load(Ordering::SeqCst),
            3,
            "ACK does not depend on binding provider availability"
        );

        let f = Fixture::new(false, 5_000).await;
        for field in ["binding", "execution_binding", "stopped"] {
            let mut body = f.body();
            body[field] = json!(true);
            assert_eq!(
                f.request(verb, body).await.unwrap().0,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "R4"
            );
            assert!(
                f.store
                    .load_physical_attempt(&f.claim.run_id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            f.request(verb, f.body()).await.unwrap().1["admission"],
            "applied",
            "R3"
        );
        assert!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap()
                .unwrap()
                .binding
                .is_none()
        );
        assert_eq!(f.source.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn provider_configuration_changes_cannot_rewrite_an_existing_slot() {
    // R12 enable source over a historical unbound slot => conflict, no backfill;
    // R13 disable source over a bound slot => conflict, no binding erasure.
    // Both ordinary admission and handoff retain exact saved obligation. This
    // models configuration rollout without inventing a migration/repair path.
    for verb in ["begin", "handoff"] {
        for configured in [false, true] {
            let f = Fixture::new(configured, 5_000).await;
            let binding = f.source.result.lock().unwrap().clone().unwrap();
            let prior_binding = if configured { None } else { Some(&binding) };
            assert_eq!(
                f.store
                    .begin_attempt(&f.claim, prior_binding)
                    .await
                    .unwrap(),
                AttemptAdmission::Applied
            );
            let prior = f
                .store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap();
            f.source.release.add_permits(1);
            assert_eq!(
                f.request(verb, f.body()).await.unwrap().0,
                reqwest::StatusCode::CONFLICT
            );
            assert_eq!(
                f.store
                    .load_physical_attempt(&f.claim.run_id)
                    .await
                    .unwrap(),
                prior
            );
        }
    }
}

#[tokio::test]
async fn provider_wait_cannot_refresh_stale_authority_or_downgrade_failure() {
    // Causal graph: authenticated current claim -> provider await -> changing
    // Registry/claim/clock -> provider reply -> revalidation -> atomic admission.
    // Cross every rule with both verbs (no parallel handoff authorization path).
    // R5 provider error => unavailable + no slot; R6 mismatched identity => error
    // + no slot; R7 Registry replacement/death/expiry => reject + no slot.
    // R8 expired/replaced/cancelled claim => Fenced + no slot. R9 independently
    // renewed claim after the original deadline => Applied; lookup holds no DB
    // authority lock. R10 provider timeout => bounded unavailable, no fallback.
    for verb in ["begin", "handoff"] {
        for rule in [
            "error",
            "foreign",
            "registry-replaced",
            "registry-dead",
            "registry-expired",
            "claim-expired",
            "claim-replaced",
            "claim-cancelled",
            "renewed",
        ] {
            let f = Fixture::new(true, 5_000).await;
            let pending = f.request(verb, f.body());
            f.entered().await;
            match rule {
                "error" => *f.source.result.lock().unwrap() = Err("provider unreachable".into()),
                "foreign" => {
                    *f.source.result.lock().unwrap() = Ok(AttemptExecutionBinding::new(
                        WorkerIdentity::new("other", "boot", 1),
                        "trusted-cluster",
                        "pod",
                    )
                    .unwrap())
                }
                "registry-replaced" => {
                    f.directory
                        .deregister(&f.upstream.worker_identity().unwrap().clone())
                        .await
                        .unwrap();
                    let record = f.directory.current("binding-http").await.unwrap().unwrap();
                    f.directory
                        .register(
                            WorkerRegistration {
                                worker_id: "binding-http".into(),
                                incarnation_id: "replacement".into(),
                                manifest: record.snapshot.manifest,
                                create_ledger: None,
                            },
                            10_000,
                            30_000,
                        )
                        .await
                        .unwrap();
                }
                "registry-dead" => {
                    f.directory
                        .deregister(f.upstream.worker_identity().unwrap())
                        .await
                        .unwrap();
                }
                "registry-expired" => {
                    f.clock.set(40_000);
                    f.dispatch_clock.set(40_000);
                }
                "claim-expired" => {
                    f.clock.set(11_001);
                    f.dispatch_clock.set(11_001);
                }
                "claim-replaced" => {
                    f.clock.set(11_001);
                    f.dispatch_clock.set(11_001);
                    let mut snapshot = f
                        .directory
                        .current("binding-http")
                        .await
                        .unwrap()
                        .unwrap()
                        .snapshot;
                    snapshot.identity = WorkerIdentity::new("replacement", "new-boot", 1);
                    f.store
                        .claim_compatible(&snapshot, 10_000)
                        .await
                        .unwrap()
                        .unwrap();
                }
                "claim-cancelled" => {
                    f.store.cancel(&f.claim.run_id).await.unwrap();
                }
                "renewed" => {
                    // This timeout also detects holding the store lock while
                    // awaiting the provider. Renew before expiry, finish after.
                    tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        f.store.renew_lease(&f.claim, 10_000),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    f.clock.set(11_001);
                    f.dispatch_clock.set(11_001);
                }
                _ => unreachable!(),
            }
            f.source.release.add_permits(1);
            let (status, body) = pending.await.unwrap();
            if rule == "renewed" {
                assert_eq!(status, reqwest::StatusCode::OK, "{verb}/{rule}: {body}");
                assert_eq!(body["admission"], "applied");
                assert!(
                    f.store
                        .load_physical_attempt(&f.claim.run_id)
                        .await
                        .unwrap()
                        .is_some()
                );
            } else {
                let expected_status = match rule {
                    "error" => reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    "foreign" => reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    "registry-replaced" | "registry-dead" | "registry-expired" => {
                        reqwest::StatusCode::BAD_REQUEST
                    }
                    _ => reqwest::StatusCode::OK,
                };
                assert_eq!(status, expected_status, "{verb}/{rule}: {body}");
                if status == reqwest::StatusCode::OK {
                    assert_eq!(body["admission"], "fenced", "{verb}/{rule}");
                }
                assert!(
                    f.store
                        .load_physical_attempt(&f.claim.run_id)
                        .await
                        .unwrap()
                        .is_none(),
                    "{verb}/{rule}"
                );
            }
        }
        let f = Fixture::new(true, 20).await;
        let pending = f.request(verb, f.body());
        let (status, _) = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "R10");
        assert!(
            f.store
                .load_physical_attempt(&f.claim.run_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

use awaken_dispatch_contract::{
    DispatchAdministration as _, DispatchClaims as _, DispatchControlReads as _,
    DispatchEnqueue as _, DispatchExecution as _,
};

/// The signed physical-attempt protocol family belongs beside its original
/// binding/Registry tests; all P1-P5/U1-U4 assertions move intact from parent.
pub(super) async fn verify_ordinary_return_compatibility(
    queue: &awaken_coordinator_client::HttpDispatchQueue,
    upstream: &WorkerUpstream,
    directory: &TestWorkerDirectory,
    dispatch: &MemoryDispatchStore,
    claim: &RunClaim,
) {
    // Physical-slot transport rules: exact live identity begins; exact begin
    // and continuation-handoff response-loss retries are idempotent; a body
    // identity different from the signed incarnation can neither hand off nor
    // acknowledge; the exact owner can finish. The
    // final ACK endpoint intentionally authenticates the signed predecessor but
    // does not require it to remain the current registry/Run owner, because that
    // would make safe handoff impossible after reclaim.
    assert_eq!(
        queue.begin_attempt(claim, None).await.expect("P1 begin"),
        AttemptAdmission::Applied,
        "P1 exact identity"
    );
    assert_eq!(
        queue.begin_attempt(claim, None).await.expect("P2 retry"),
        AttemptAdmission::AlreadyApplied,
        "P2 exact response-loss retry"
    );
    assert_eq!(
        queue
            .handoff_attempt(claim, None)
            .await
            .expect("P2 handoff retry"),
        AttemptAdmission::AlreadyApplied,
        "P2 exact durable-continuation retry"
    );
    let mismatched_queue = awaken_coordinator_client::dispatch_transport_with_upstream(
        upstream,
        WorkerIdentity::new("signed-http-worker", "different-boot", 2),
    );
    assert!(
        mismatched_queue.handoff_attempt(claim, None).await.is_err(),
        "P3 mismatched signed-body incarnation cannot replace the slot"
    );
    assert!(
        mismatched_queue.finish_attempt(claim, None).await.is_err(),
        "P3 mismatched signed-body incarnation cannot clear the slot"
    );
    assert!(
        mismatched_queue
            .finish_attempt(
                claim,
                Some(awaken_dispatch::AttemptReturnCause::StructuredReturnObserved)
            )
            .await
            .is_err(),
        "P3 Some also binds the signed original identity"
    );
    // Upgrade table U1: an already-occupied claim belongs to a persisted v3
    // incarnation. New begin/handoff requests are rejected without touching its
    // slot; U2 its authenticated exact ACK still retires that slot; U3 an ACK
    // retry is idempotent. Seed historical manifest state only in this fixture
    // (production cannot mutate an incarnation's immutable manifest).
    let before_upgrade = directory.0.lock().unwrap().clone();
    {
        let mut record = directory.0.lock().unwrap();
        let snapshot = &mut record.as_mut().unwrap().snapshot;
        snapshot.manifest.runtime_protocol.max = 3;
        snapshot.capability_fingerprint = snapshot.manifest.fingerprint().unwrap();
    }
    let occupied_before = dispatch.list_dispatches().await.unwrap();
    assert!(queue.begin_attempt(claim, None).await.is_err(), "U1 begin");
    assert!(
        queue.handoff_attempt(claim, None).await.is_err(),
        "U1 handoff"
    );
    assert_eq!(
        dispatch.list_dispatches().await.unwrap(),
        occupied_before,
        "U1 no mutation"
    );
    // U5 new local facts require the exact immutable protocol record; the
    // legacy None ACK below remains legal and cannot backfill this observation.
    assert!(
        queue
            .finish_attempt(
                claim,
                Some(awaken_dispatch::AttemptReturnCause::StructuredReturnObserved)
            )
            .await
            .is_err(),
        "U5 v3 cannot attest the new local observation"
    );
    assert_eq!(
        dispatch.list_dispatches().await.unwrap(),
        occupied_before,
        "U5 preserve original slot"
    );
    {
        let mut record = directory.0.lock().unwrap();
        let snapshot = &mut record.as_mut().unwrap().snapshot;
        snapshot.manifest.runtime_protocol.min = 4;
        snapshot.manifest.runtime_protocol.max = 4;
        snapshot.capability_fingerprint = snapshot.manifest.fingerprint().unwrap();
    }
    assert!(
        queue
            .finish_attempt(
                claim,
                Some(awaken_dispatch::AttemptReturnCause::StructuredReturnObserved)
            )
            .await
            .is_err(),
        "U5 exact4 does not cover CURRENT5"
    );
    assert_eq!(
        dispatch.list_dispatches().await.unwrap(),
        occupied_before,
        "U5 exact4 leaves slot unchanged"
    );
    {
        let mut record = directory.0.lock().unwrap();
        let snapshot = &mut record.as_mut().unwrap().snapshot;
        snapshot.manifest.runtime_protocol.min = 1;
        snapshot.manifest.runtime_protocol.max = 3;
        snapshot.capability_fingerprint = snapshot.manifest.fingerprint().unwrap();
    }
    assert_eq!(
        queue
            .finish_attempt(claim, None)
            .await
            .expect("P4 exact ACK"),
        SettleOutcome::Applied,
        "P4 exact owner quiescence"
    );
    // P5: an ACK committed but its HTTP response was lost. Retrying the same
    // authenticated command sees Empty -> Applied, with no executor re-entry.
    assert_eq!(
        queue
            .finish_attempt(claim, None)
            .await
            .expect("P5 ACK retry"),
        SettleOutcome::Applied,
        "P5 transport preserves the store's idempotent empty-slot ACK"
    );
    // U4: neither an old peer nor a current replacement may re-admit the same
    // acknowledged identity; protocol rejection and epoch retirement compose.
    assert!(
        queue.begin_attempt(claim, None).await.is_err(),
        "U4 old peer"
    );
    *directory.0.lock().unwrap() = before_upgrade;
    assert_eq!(
        queue.begin_attempt(claim, None).await.unwrap(),
        AttemptAdmission::Fenced,
        "U4 retired epoch"
    );
}
