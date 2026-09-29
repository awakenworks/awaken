// @formal evidence session_execution.one_physical_attempt_per_thread
use super::*;
use awaken_dispatch::{
    ApplicationError, AttemptExecutionBinding, RunClaim, WorkerExecutionBindingSource,
};
use awaken_dispatch_contract::ControlDispatchQueue;
use awaken_dispatch_testkit::worker_http as support;
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

struct BindingSource {
    result: Mutex<Result<AttemptExecutionBinding, String>>,
    calls: AtomicUsize,
    entered: Notify,
    release: Semaphore,
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for BindingSource {
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
    directory: Arc<TestWorkerDirectory>,
    clock: Arc<ManualClock>,
    /// The dispatch store's own clock, held at the same reading as `clock`.
    dispatch_clock: Arc<awaken_lease_testkit::ManualClock>,
    claim: RunClaim,
    upstream: WorkerUpstream,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(configured: bool, request_budget_ms: u64) -> Self {
        let (ready, identity) = support::ready_worker("binding-http").await;
        let record = ready.current(&identity.worker_id).await.unwrap().unwrap();
        let directory = Arc::new(TestWorkerDirectory(Mutex::new(Some(record.clone()))));
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
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .snapshot
                        .identity
                        .incarnation_id = "replacement".into()
                }
                "registry-dead" => {
                    f.directory
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .snapshot
                        .state = WorkerState::Dead
                }
                "registry-expired" => {
                    f.directory
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .snapshot
                        .expires_at_ms = 10_001;
                    f.clock.set(10_001);
                    f.dispatch_clock.set(10_001);
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
                        .0
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .snapshot
                        .clone();
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
