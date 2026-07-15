//! `SandboxManager` — the worker-plane orchestrator for isolation-instance reuse
//! (ADR-0056 §4). It is the missing **caller** of the pure reuse-decision functions:
//! before this crate, `reconcile_adoption` / `LeaseLiveness` / `decide_reap` were
//! written and tested but had no production caller, so there was no reaper, no
//! renew loop, and no orphan reconciliation.
//!
//! The manager owns only **control flow** — it tracks the sandboxes it created, and
//! on each tick delegates the *judgement* to the pure kernel:
//!
//! - **reconcile** a tracked-live set against the set a live run still references
//!   (`reconcile_adoption` → adopt the referenced, reap the unreferenced, surface the
//!   orphaned for re-placement);
//! - **renew-or-reap** each tracked lease by its liveness (`decide_reap`: revoked >
//!   deadline > transport-lost), the dead-man's switch that stops a vanished owner's
//!   sandbox lingering.
//!
//! Per ADR-0056 guardrail G-Pure, this crate contributes NO judgement of its own:
//! every decision stays in the pure functions, which stay exhaustively testable with a
//! fake clock and plain values. First-slice scope is the Workdir tier — create per run,
//! track, reconcile, reap; the Container warm pool + `adopt`/`process` reattach arrive
//! with their driving scenarios behind ADR-0056's G-Y gate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_provisioning_contract::{
    LeaseGrant, LivenessSignals, ReapCause, ReconcileOutcome, Sandbox, SandboxError, SandboxHandle,
    SandboxProvider, SandboxSpec, decide_reap, reconcile_and_apply,
};

/// The `(provider_kind, sandbox_id)` identity a handle reconciles by — the same key
/// the pure `reconcile_adoption` uses (locators in `extra` are not identity).
type Key = (String, String);

fn key(handle: &SandboxHandle) -> Key {
    (handle.provider_kind.clone(), handle.sandbox_id.clone())
}

/// One sandbox this manager created and still owns: its durable handle plus the lease
/// grant whose liveness gates reaping.
#[derive(Clone)]
struct Tracked {
    handle: SandboxHandle,
    lease: LeaseGrant,
}

/// Owns a `SandboxProvider` and the set of sandboxes this worker created, and drives
/// their reuse lifecycle through the pure decision kernel. Cheaply cloneable (the
/// tracked set is shared) so a reaper task and the create path share one view.
#[derive(Clone)]
pub struct SandboxManager {
    provider: Arc<dyn SandboxProvider>,
    tracked: Arc<Mutex<HashMap<Key, Tracked>>>,
}

impl SandboxManager {
    /// Wire the manager to the isolation provider it orchestrates.
    #[must_use]
    pub fn new(provider: Arc<dyn SandboxProvider>) -> Self {
        Self {
            provider,
            tracked: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a sandbox and START tracking it under `lease`, so a later `reconcile` /
    /// `renew_or_reap` sees it as a live instance this worker owns. The returned live
    /// `Sandbox` is the caller's to drive; the manager keeps only the durable handle.
    pub async fn create(
        &self,
        spec: &SandboxSpec,
        lease: LeaseGrant,
    ) -> Result<Box<dyn Sandbox>, SandboxError> {
        let sandbox = self.provider.create(spec).await?;
        let handle = sandbox.handle();
        self.tracked
            .lock()
            .expect("sandbox manager tracked map")
            .insert(key(&handle), Tracked { handle, lease });
        Ok(sandbox)
    }

    /// Reconcile THIS manager's own tracked-live set against the sandboxes a live run
    /// still `referenced`. Correct for a **node-local** tier (Workdir): a sandbox that
    /// this worker did not create is not live for it (it died with its owner), so it is
    /// surfaced as orphaned for re-placement. For a **shared-substrate** tier whose
    /// sandboxes outlive their creator (Container/k8s), use [`reconcile_against`] with
    /// the provider-discovered live set so a peer can re-adopt a still-running sandbox.
    pub async fn reconcile(&self, referenced: &[SandboxHandle]) -> ReconcileOutcome {
        self.reconcile_against(&self.live_handles(), referenced)
            .await
    }

    /// Reconcile a caller-supplied `live` set (the provider's discovered live sandboxes,
    /// which on a shared substrate span workers) against the `referenced` set, actuating
    /// through the pure kernel: adopt the still-referenced (reconnect — this is how a
    /// PEER worker re-adopts a sandbox whose creator crashed, so the worker is not a
    /// data-loss single point of failure), reap the unreferenced, surface the orphaned
    /// (referenced-but-not-live) for re-placement. Adopted handles this worker did not
    /// track become tracked; reaped/orphaned are dropped.
    pub async fn reconcile_against(
        &self,
        live: &[SandboxHandle],
        referenced: &[SandboxHandle],
    ) -> ReconcileOutcome {
        let outcome = reconcile_and_apply(self.provider.as_ref(), live, referenced).await;
        let mut tracked = self.tracked.lock().expect("sandbox manager tracked map");
        // A peer re-adopting a crashed worker's sandbox now owns its renewal.
        for handle in &outcome.adopted {
            tracked.entry(key(handle)).or_insert_with(|| Tracked {
                handle: handle.clone(),
                lease: LeaseGrant::indefinite(),
            });
        }
        for handle in outcome.reaped.iter().chain(outcome.orphaned.iter()) {
            tracked.remove(&key(handle));
        }
        outcome
    }

    /// The dead-man's switch: for each tracked sandbox, collapse its liveness signals
    /// (`signals(handle)` supplies revoked / transport-lost / now) into a reap decision
    /// via the pure `decide_reap` (priority revoked > deadline > transport-lost), and
    /// tear down every reapable one (reconnect, then `dispose`). Returns each reaped
    /// handle with its cause; a dispose failure leaves the handle tracked for a later
    /// tick (idempotent). Judgement stays entirely in `decide_reap`.
    pub async fn renew_or_reap<F>(&self, mut signals: F) -> Vec<(SandboxHandle, ReapCause)>
    where
        F: FnMut(&SandboxHandle) -> LivenessSignals,
    {
        let due: Vec<(Tracked, ReapCause)> = {
            let tracked = self.tracked.lock().expect("sandbox manager tracked map");
            tracked
                .values()
                .filter_map(|t| decide_reap(&t.lease, signals(&t.handle)).map(|c| (t.clone(), c)))
                .collect()
        };
        let mut reaped = Vec::new();
        for (t, cause) in due {
            // Reconnect and dispose; only drop from tracking once the teardown lands,
            // so a transient failure is retried on the next tick rather than leaked.
            if let Ok(sandbox) = self.provider.adopt(&t.handle).await
                && sandbox.dispose().await.is_ok()
            {
                self.tracked
                    .lock()
                    .expect("sandbox manager tracked map")
                    .remove(&key(&t.handle));
                reaped.push((t.handle, cause));
            }
        }
        reaped
    }

    /// The durable handles of every sandbox this manager currently tracks.
    #[must_use]
    pub fn live_handles(&self) -> Vec<SandboxHandle> {
        self.tracked
            .lock()
            .expect("sandbox manager tracked map")
            .values()
            .map(|t| t.handle.clone())
            .collect()
    }

    /// How many sandboxes this manager currently tracks.
    #[must_use]
    pub fn tracked_count(&self) -> usize {
        self.tracked
            .lock()
            .expect("sandbox manager tracked map")
            .len()
    }

    /// Tear down every tracked sandbox (a graceful worker drain). Best-effort per
    /// handle; a failed teardown is left tracked (a supervisor may retry).
    pub async fn shutdown(&self) {
        for handle in self.live_handles() {
            if let Ok(sandbox) = self.provider.adopt(&handle).await
                && sandbox.dispose().await.is_ok()
            {
                self.tracked
                    .lock()
                    .expect("sandbox manager tracked map")
                    .remove(&key(&handle));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Cause-effect + state-transition coverage for the manager's control flow, with
    //! every judgement delegated to the pure kernel. A recording fake provider lets us
    //! assert exactly which sandboxes are torn down, and a caller-supplied clock/signal
    //! function drives `decide_reap` deterministically (no wall clock).
    use super::*;
    use async_trait::async_trait;
    use awaken_provisioning_contract::{
        Artifact, Command, IsolationClass, MountRequirement, NetworkPolicy, ProcessHandle,
        RealizedMount, SandboxCapabilities, SandboxStatus,
    };
    use std::sync::Mutex as StdMutex;

    /// Records every `dispose` so a test can prove a reap/reconcile actually tore a
    /// sandbox down (not merely dropped it from tracking).
    #[derive(Default)]
    struct Recorder {
        disposed: StdMutex<Vec<String>>,
    }

    struct RecProvider {
        rec: Arc<Recorder>,
    }
    struct RecSandbox {
        id: String,
        rec: Arc<Recorder>,
    }

    #[async_trait]
    impl Sandbox for RecSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        fn handle(&self) -> SandboxHandle {
            SandboxHandle::new("fake", &self.id)
        }
        async fn spawn(&self, _c: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Err(SandboxError::new("unused in manager tests"))
        }
        async fn attach(&self, _r: MountRequirement) -> Result<RealizedMount, SandboxError> {
            Err(SandboxError::new("unused"))
        }
        async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
            Ok(Vec::new())
        }
        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, SandboxError> {
            Ok(Vec::new())
        }
        fn realized(&self) -> &[RealizedMount] {
            &[]
        }
        async fn process(&self, _p: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Err(SandboxError::new("unused"))
        }
        async fn status(&self) -> Result<SandboxStatus, SandboxError> {
            Ok(SandboxStatus::Ready)
        }
        async fn renew_lease(&self) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn dispose(&self) -> Result<(), SandboxError> {
            self.rec.disposed.lock().unwrap().push(self.id.clone());
            Ok(())
        }
    }

    #[async_trait]
    impl SandboxProvider for RecProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                isolation: IsolationClass::Workdir,
                tool_transparent: false,
                path_fidelity: false,
                enforced_readonly: false,
                network_isolation: false,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
            }
        }
        async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(RecSandbox {
                id: spec.scope.clone(),
                rec: self.rec.clone(),
            }))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(RecSandbox {
                id: handle.sandbox_id.clone(),
                rec: self.rec.clone(),
            }))
        }
    }

    fn spec(scope: &str) -> SandboxSpec {
        SandboxSpec {
            scope: scope.into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: Some(60),
            extra: None,
        }
    }

    fn manager() -> (SandboxManager, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        let provider = Arc::new(RecProvider { rec: rec.clone() });
        (SandboxManager::new(provider), rec)
    }

    fn href(id: &str) -> SandboxHandle {
        SandboxHandle::new("fake", id)
    }

    // All non-reaping signals — decide_reap fires only on a passed deadline.
    fn alive_signals(now_ms: u64) -> impl FnMut(&SandboxHandle) -> LivenessSignals {
        move |_h| LivenessSignals {
            now_ms,
            revoked: false,
            transport_lost: false,
        }
    }

    #[tokio::test]
    async fn create_tracks_the_sandbox() {
        let (mgr, _rec) = manager();
        let sb = mgr
            .create(&spec("t-1"), LeaseGrant::indefinite())
            .await
            .unwrap();
        assert_eq!(sb.id(), "t-1");
        assert_eq!(mgr.tracked_count(), 1);
        assert_eq!(mgr.live_handles(), vec![href("t-1")]);
    }

    #[tokio::test]
    async fn reconcile_adopts_a_referenced_sandbox_and_reaps_an_unreferenced_one() {
        // Two live sandboxes; a run still references only t-keep. reconcile must adopt
        // t-keep (stays tracked) and reap t-drop (disposed, dropped) — the judgement is
        // entirely reconcile_adoption's.
        let (mgr, rec) = manager();
        mgr.create(&spec("t-keep"), LeaseGrant::indefinite())
            .await
            .unwrap();
        mgr.create(&spec("t-drop"), LeaseGrant::indefinite())
            .await
            .unwrap();

        let outcome = mgr.reconcile(&[href("t-keep")]).await;
        assert_eq!(outcome.adopted, vec![href("t-keep")]);
        assert_eq!(outcome.reaped, vec![href("t-drop")]);
        assert!(outcome.orphaned.is_empty());
        assert_eq!(*rec.disposed.lock().unwrap(), vec!["t-drop".to_string()]);
        assert_eq!(
            mgr.live_handles(),
            vec![href("t-keep")],
            "only the referenced stays tracked"
        );
    }

    #[tokio::test]
    async fn reconcile_surfaces_an_orphan_for_a_referenced_but_dead_sandbox() {
        // A run references t-gone, which this manager never created (its sandbox died).
        // reconcile must surface it as orphaned for re-placement, not adopt it.
        let (mgr, _rec) = manager();
        let outcome = mgr.reconcile(&[href("t-gone")]).await;
        assert_eq!(outcome.orphaned, vec![href("t-gone")]);
        assert!(outcome.adopted.is_empty() && outcome.reaped.is_empty());
    }

    #[tokio::test]
    async fn renew_or_reap_tears_down_a_past_deadline_lease_and_keeps_a_live_one() {
        // t-live's lease outlasts now; t-dead's has passed. decide_reap (deadline) fires
        // only on t-dead → it is disposed and dropped; t-live is kept.
        let (mgr, rec) = manager();
        mgr.create(&spec("t-live"), LeaseGrant::until(1_000))
            .await
            .unwrap();
        mgr.create(&spec("t-dead"), LeaseGrant::until(100))
            .await
            .unwrap();

        let reaped = mgr.renew_or_reap(alive_signals(500)).await; // now=500: t-dead expired
        assert_eq!(reaped, vec![(href("t-dead"), ReapCause::Expired)]);
        assert_eq!(*rec.disposed.lock().unwrap(), vec!["t-dead".to_string()]);
        assert_eq!(mgr.live_handles(), vec![href("t-live")]);
    }

    #[tokio::test]
    async fn renew_or_reap_honors_a_revoke_over_a_still_live_deadline() {
        // The lease deadline is far in the future, but the signal says revoked →
        // decide_reap's priority (revoked > deadline) reaps it anyway.
        let (mgr, rec) = manager();
        mgr.create(&spec("t-revoked"), LeaseGrant::until(10_000))
            .await
            .unwrap();
        let reaped = mgr
            .renew_or_reap(|_h| LivenessSignals {
                now_ms: 1,
                revoked: true,
                transport_lost: false,
            })
            .await;
        assert_eq!(reaped, vec![(href("t-revoked"), ReapCause::Revoked)]);
        assert_eq!(*rec.disposed.lock().unwrap(), vec!["t-revoked".to_string()]);
        assert_eq!(mgr.tracked_count(), 0);
    }

    #[tokio::test]
    async fn a_peer_worker_re_adopts_a_crashed_workers_still_live_sandbox() {
        // The SPOF-elimination proof at the component level (ADR-0056). One shared
        // substrate (the provider): worker A creates a sandbox and binds the run to it,
        // then A "crashes" (its manager is gone). Worker B recovers the run — the
        // sandbox OUTLIVED A (shared-substrate tier) so the provider still discovers it
        // live. B reconciles the run's binding against the discovered-live set and
        // ADOPTS the SAME sandbox (same handle) rather than orphaning it. No dispose
        // happens: the in-flight sandbox state is preserved across the worker crash.
        let rec = Arc::new(Recorder::default());
        let provider_a = Arc::new(RecProvider { rec: rec.clone() });
        let provider_b = Arc::new(RecProvider { rec: rec.clone() }); // same substrate

        let worker_a = SandboxManager::new(provider_a);
        worker_a
            .create(&spec("run-42"), LeaseGrant::indefinite())
            .await
            .unwrap();
        let bound = href("run-42"); // the run's durable sandbox binding

        // A crashes. B recovers: the provider discovers the sandbox still live (it
        // outlived A), so B reconciles the binding against that discovered-live set.
        drop(worker_a);
        let worker_b = SandboxManager::new(provider_b);
        let discovered_live = vec![bound.clone()];
        let outcome = worker_b
            .reconcile_against(&discovered_live, std::slice::from_ref(&bound))
            .await;

        assert_eq!(
            outcome.adopted,
            vec![bound.clone()],
            "B re-adopts the same sandbox"
        );
        assert!(
            outcome.orphaned.is_empty(),
            "a still-live sandbox is not orphaned"
        );
        assert!(
            rec.disposed.lock().unwrap().is_empty(),
            "the sandbox is NOT torn down — its state survives the crash"
        );
        assert_eq!(
            worker_b.live_handles(),
            vec![bound],
            "B now tracks (and will renew) the adopted sandbox"
        );
    }

    #[tokio::test]
    async fn shutdown_disposes_every_tracked_sandbox() {
        let (mgr, rec) = manager();
        mgr.create(&spec("a"), LeaseGrant::indefinite())
            .await
            .unwrap();
        mgr.create(&spec("b"), LeaseGrant::indefinite())
            .await
            .unwrap();
        mgr.shutdown().await;
        assert_eq!(mgr.tracked_count(), 0);
        let mut disposed = rec.disposed.lock().unwrap().clone();
        disposed.sort();
        assert_eq!(disposed, vec!["a".to_string(), "b".to_string()]);
    }
}
