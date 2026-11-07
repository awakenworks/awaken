// @formal evidence session_execution.one_physical_attempt_per_thread
//! Physical admission over the unbound transport (no execution bindings, the
//! default for every non-Kubernetes deployment) against a concurrent closure of
//! the claiming Worker incarnation. Real HTTP router, canonical memory Registry
//! kernel and memory Dispatch store sharing one manual clock; the only test
//! double is a barrier around the Registry reads admission performs.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use async_trait::async_trait;
use awaken_dispatch::{MemoryDispatchStore, PlacementRequirements, RunClaim, RunDispatch};
use awaken_dispatch_contract::{DispatchClaims, DispatchControlReads, DispatchEnqueue};
use awaken_dispatch_http::{
    RegisteredDispatchDependencies, WorkerDispatchService, dispatch_transport_router_with_service,
};
use awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies;
use awaken_dispatch_testkit::worker_http::{activation, current_dispatch_codec_manifest};
use awaken_lease_testkit::ManualClock;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerCurrentSource, WorkerDirectory,
    WorkerHeartbeat, WorkerIdentity, WorkerIncarnationGuard, WorkerIncarnationSource,
    WorkerObservationSource, WorkerProcessExit, WorkerRegistration,
};
use awaken_worker_registry::MemoryWorkerDirectory;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tower::ServiceExt;

const REGISTERED_AT_MS: u64 = 1_000;
const REGISTRY_TTL_MS: u64 = 5_000;
const EXPIRES_AT_MS: u64 = REGISTERED_AT_MS + REGISTRY_TTL_MS;
/// The Run claim outlives the Registry lease: the claim alone never fences
/// a closed incarnation, so only the Registry decision can.
const CLAIM_LEASE_MS: u64 = 60_000;

/// Where the request is parked. `AuthorityRead`: the snapshot that
/// `claim_authority` judges has been read, nothing has been written yet.
/// `RegistryExclusion`: the admission path holds the original incarnation
/// exclusion that it carries into the Dispatch admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    AuthorityRead,
    RegistryExclusion,
}

/// The four ways one incarnation stops holding authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closure {
    /// begin_drain + mark_quiesced through the Registry kernel.
    Quiesced,
    /// The store clock reaches exactly `expires_at_ms`; nothing is written.
    Expired,
    /// The incarnation is deregistered (Dead).
    Dead,
    /// The lease lapses and the restarted process registers a new incarnation.
    Replaced,
}

/// Canonical memory Registry with one armed barrier. Closures run against
/// `inner`, so they serialize with admission only through the Registry's own
/// exclusion, never through this wrapper.
struct HeldDirectory {
    inner: Arc<MemoryWorkerDirectory>,
    armed: Mutex<Option<Hold>>,
    reached: Semaphore,
    release: Semaphore,
}

impl HeldDirectory {
    async fn park(&self, at: Hold) {
        let hit = {
            let mut armed = self.armed.lock().unwrap();
            (*armed == Some(at)).then(|| armed.take()).is_some()
        };
        if hit {
            self.reached.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
    }
}

#[async_trait]
impl WorkerObservationSource for HeldDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        self.inner.list().await
    }
}

#[async_trait]
impl WorkerCurrentSource for HeldDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        let current = self.inner.current(worker_id).await;
        self.park(Hold::AuthorityRead).await;
        current
    }
}

#[async_trait]
impl WorkerIncarnationSource for HeldDirectory {
    async fn incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        self.inner.incarnation(identity).await
    }
}

#[async_trait]
impl WorkerDirectory for HeldDirectory {
    async fn lock_incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<WorkerIncarnationGuard>, RegistryError> {
        let guard = self.inner.lock_incarnation(identity).await;
        self.park(Hold::RegistryExclusion).await;
        guard
    }
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        self.inner.register(registration, now_ms, ttl_ms).await
    }
    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.inner
            .heartbeat(identity, heartbeat, now_ms, ttl_ms)
            .await
    }
    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.inner.begin_drain(identity, deadline_ms).await
    }
    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.inner.mark_quiesced(identity).await
    }
    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.inner.deregister(identity).await
    }
    async fn expire_incarnation(
        &self,
        identity: &WorkerIdentity,
        now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.inner.expire_incarnation(identity, now_ms).await
    }
    async fn record_process_exit(
        &self,
        identity: &WorkerIdentity,
        receipt: WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        self.inner.record_process_exit(identity, receipt).await
    }
}

fn registration(worker_id: &str, incarnation_id: &str) -> WorkerRegistration {
    let mut manifest = current_dispatch_codec_manifest();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    WorkerRegistration {
        worker_id: worker_id.into(),
        incarnation_id: incarnation_id.into(),
        manifest,
        create_ledger: None,
    }
}

async fn register_ready(
    directory: &dyn WorkerDirectory,
    worker_id: &str,
    incarnation_id: &str,
    now_ms: u64,
) -> WorkerIdentity {
    let identity = directory
        .register(
            registration(worker_id, incarnation_id),
            now_ms,
            REGISTRY_TTL_MS,
        )
        .await
        .unwrap()
        .snapshot
        .identity;
    assert_eq!(
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
                now_ms,
                REGISTRY_TTL_MS,
            )
            .await
            .unwrap(),
        RegistryMutation::Applied
    );
    identity
}

struct Fixture {
    clock: Arc<ManualClock>,
    directory: Arc<HeldDirectory>,
    store: Arc<MemoryDispatchStore>,
    router: Router,
    identity: WorkerIdentity,
    claim: RunClaim,
}

impl Fixture {
    async fn new(tag: &str) -> Self {
        let clock = Arc::new(ManualClock::new(REGISTERED_AT_MS));
        let directory = Arc::new(HeldDirectory {
            inner: Arc::new(MemoryWorkerDirectory::new()),
            armed: Mutex::new(None),
            reached: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let identity = register_ready(
            directory.as_ref(),
            &format!("{tag}-worker"),
            "first-boot",
            REGISTERED_AT_MS,
        )
        .await;
        let request = RunDispatch::new(activation(tag))
            .with_placement(PlacementRequirements::remote_required())
            .with_worker_codec_requirements();
        let run_id = request.run_id().clone();
        store.enqueue(request).await.unwrap();
        let snapshot = directory
            .inner
            .current(&identity.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        let claim = RunClaim::from(
            &store
                .claim_run_compatible(&run_id, &snapshot, CLAIM_LEASE_MS)
                .await
                .unwrap()
                .expect("the Ready incarnation claims its Run")
                .lease,
        );
        let service = WorkerDispatchService::new(RegisteredDispatchDependencies {
            registry_ttl_ms: REGISTRY_TTL_MS,
            ..worker_dispatch_dependencies(store.clone(), directory.clone(), clock.clone())
        });
        Self {
            clock,
            directory,
            store,
            router: dispatch_transport_router_with_service(Arc::new(service)),
            identity,
            claim,
        }
    }

    async fn installed(&self) -> bool {
        self.store
            .load_physical_attempt(&self.claim.run_id)
            .await
            .unwrap()
            .is_some_and(|attempt| attempt.claim == self.claim)
    }
}

async fn begin(router: Router, worker: WorkerIdentity, claim: RunClaim) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/worker/dispatch/attempt/begin")
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", &worker.worker_id)
        .body(Body::from(
            serde_json::to_vec(&json!({"identity": worker, "claim": claim})).unwrap(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Apply `closure` to the canonical Registry. Clock movement is immediate and
/// therefore closes authority before anything that waits on the exclusion.
async fn close(
    closure: Closure,
    clock: Arc<ManualClock>,
    directory: Arc<MemoryWorkerDirectory>,
    identity: WorkerIdentity,
) -> RegistryMutation {
    match closure {
        Closure::Quiesced => {
            assert_eq!(
                directory
                    .begin_drain(&identity, EXPIRES_AT_MS)
                    .await
                    .unwrap(),
                RegistryMutation::Applied
            );
            directory.mark_quiesced(&identity).await.unwrap()
        }
        Closure::Expired => {
            clock.set(EXPIRES_AT_MS);
            RegistryMutation::Applied
        }
        Closure::Dead => directory.deregister(&identity).await.unwrap(),
        Closure::Replaced => {
            let restarted = directory
                .register(
                    registration(&identity.worker_id, "second-boot"),
                    EXPIRES_AT_MS,
                    REGISTRY_TTL_MS,
                )
                .await
                .unwrap();
            assert_eq!(restarted.snapshot.identity.generation, 2);
            RegistryMutation::Applied
        }
    }
}

/// Whether the closure's effect needs no Registry exclusion (it is in force
/// as soon as it starts, even while admission holds the incarnation lock).
const fn closes_without_exclusion(closure: Closure) -> bool {
    matches!(closure, Closure::Expired | Closure::Replaced)
}

/// One interleaving. Returns a violation description, or None.
async fn interleave(hold: Hold, closure: Closure) -> Option<String> {
    let fixture = Fixture::new(&format!("closure-{hold:?}-{closure:?}").to_lowercase()).await;
    *fixture.directory.armed.lock().unwrap() = Some(hold);
    let mut request = tokio::spawn(begin(
        fixture.router.clone(),
        fixture.identity.clone(),
        fixture.claim.clone(),
    ));
    let early = tokio::select! {
        biased;
        permit = fixture.directory.reached.acquire() => {
            permit.unwrap().forget();
            None
        }
        response = &mut request => Some(response.unwrap()),
    };
    if let Some((status, body)) = early {
        // The parked point was never reached: admission took no Registry
        // exclusion. Close afterwards only to keep the row comparable.
        return Some(format!(
            "{hold:?}/{closure:?}: admission never held the Registry exclusion \
             (returned {status} {body} without reaching it)"
        ));
    }
    if closure == Closure::Replaced {
        // Replacement needs a replaceable slot: the lease lapses first.
        fixture.clock.set(EXPIRES_AT_MS);
    }
    let mut closing = Box::pin(close(
        closure,
        fixture.clock.clone(),
        fixture.directory.inner.clone(),
        fixture.identity.clone(),
    ));
    let first = std::future::poll_fn(|cx| Poll::Ready(closing.as_mut().poll(cx))).await;
    let closed_before_release = match first {
        Poll::Ready(mutation) => {
            assert_eq!(mutation, RegistryMutation::Applied, "{hold:?}/{closure:?}");
            true
        }
        Poll::Pending => {
            // Only the exclusion this admission holds can park a closure.
            assert_eq!(hold, Hold::RegistryExclusion, "{closure:?}");
            closes_without_exclusion(closure)
        }
    };
    fixture.directory.release.add_permits(1);
    let (status, body) = request.await.unwrap();
    if !matches!(first, Poll::Ready(_)) {
        assert_eq!(
            closing.await,
            RegistryMutation::Applied,
            "{hold:?}/{closure:?}: the waiting closure applies after admission"
        );
    }
    let installed = fixture.installed().await;
    // Invariant: no attempt is installed for an incarnation whose closure was
    // in force before installation.
    (closed_before_release && installed).then(|| {
        format!(
            "{hold:?}/{closure:?}: attempt installed for an incarnation closed \
             before installation (response {status} {body})"
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbound_physical_admission_never_installs_for_a_closed_incarnation() {
    // Causes: C1 parked point (authority read, Registry exclusion); C2 closure
    // (quiesce, exact expiry, Dead, replacement). Effect: E1 no installed
    // attempt for an incarnation closed before installation; a closure that
    // waits on the exclusion applies after the installation instead.
    let mut violations = Vec::new();
    for hold in [Hold::AuthorityRead, Hold::RegistryExclusion] {
        for closure in [
            Closure::Quiesced,
            Closure::Expired,
            Closure::Dead,
            Closure::Replaced,
        ] {
            violations.extend(interleave(hold, closure).await);
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}
