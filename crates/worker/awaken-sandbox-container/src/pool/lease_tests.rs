//! Warm-capacity lease rules L1–L7 (decision table in [`super::lease`]).
//!
//! Every test runs several pools over one [`FakeDaemon`], because a Worker
//! restart *is* two pools: the first is abandoned without `shutdown` (its value
//! forgotten, exactly as a killed process runs no destructor) and the second —
//! a fresh incarnation — reclaims. Time is a [`ManualClock`]; nothing waits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_lease_contract::UtcTime;
use awaken_lease_testkit::ManualClock;
use awaken_provisioning_contract as pc;

use super::WarmContainerPool;
use super::lease::{ClaimOutcome, Hold, RecordKind, WarmCapacityLeasePolicy, WarmCapacityLeases};
use super::tests::spec;
use crate::{ContainerEnvironmentProvider, ContainerProvider, ContainerRuntime, RuntimeError};

const T0: u64 = 1_700_000_000_000;
const TTL: u64 = 60_000;

/// The shared hand-driven clock, started at `ms`.
fn manual_clock(ms: u64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(ms))
}

/// One container daemon shared by every pool of a test. Containers are keyed
/// by container id (`cid-<scope>`), so the unreceipted-scope port and the
/// by-handle port address the same physical object, as they do on Docker.
struct FakeDaemon {
    lease_root: tempfile::TempDir,
    /// container id -> scope.
    containers: Mutex<BTreeMap<String, String>>,
    creates: Mutex<Vec<String>>,
    /// L4 witness: scopes the daemon was asked to create while no lease record
    /// for them existed anywhere under the lease root.
    created_without_record: Mutex<Vec<String>>,
    unreceipted_removals: Mutex<Vec<String>>,
    handle_removals: Mutex<Vec<String>>,
    fail_handle_removal: Mutex<BTreeSet<String>>,
    hold_create: AtomicBool,
    create_started: tokio::sync::Notify,
    allow_create: tokio::sync::Semaphore,
    hold_removal: AtomicBool,
    removal_started: tokio::sync::Notify,
    allow_removal: tokio::sync::Semaphore,
}

impl FakeDaemon {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            lease_root: tempfile::tempdir().expect("lease root"),
            containers: Mutex::new(BTreeMap::new()),
            creates: Mutex::new(Vec::new()),
            created_without_record: Mutex::new(Vec::new()),
            unreceipted_removals: Mutex::new(Vec::new()),
            handle_removals: Mutex::new(Vec::new()),
            fail_handle_removal: Mutex::new(BTreeSet::new()),
            hold_create: AtomicBool::new(false),
            create_started: tokio::sync::Notify::new(),
            allow_create: tokio::sync::Semaphore::new(0),
            hold_removal: AtomicBool::new(false),
            removal_started: tokio::sync::Notify::new(),
            allow_removal: tokio::sync::Semaphore::new(0),
        })
    }

    fn record_exists(&self, scope: &str) -> bool {
        let name = format!("{scope}.lease");
        std::fs::read_dir(self.lease_root.path())
            .into_iter()
            .flatten()
            .flatten()
            .any(|domain| domain.path().join(&name).is_file())
    }

    /// Every record file name in every lease domain, sorted.
    fn records(&self) -> Vec<String> {
        let mut names = std::fs::read_dir(self.lease_root.path())
            .expect("lease root")
            .flatten()
            .flat_map(|domain| std::fs::read_dir(domain.path()).expect("lease domain"))
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn physical(&self) -> BTreeSet<String> {
        self.containers.lock().unwrap().values().cloned().collect()
    }

    fn unreceipted_removals(&self) -> Vec<String> {
        self.unreceipted_removals.lock().unwrap().clone()
    }

    fn creates(&self) -> usize {
        self.creates.lock().unwrap().len()
    }

    /// Remove a container behind every Worker's back (`docker rm -f`).
    fn vanish(&self, scope: &str) {
        self.containers
            .lock()
            .unwrap()
            .retain(|_, existing| existing != scope);
    }
}

#[async_trait]
impl ContainerRuntime for FakeDaemon {
    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn enforces_network_none(&self) -> bool {
        true
    }

    fn capacity_lease_domain(&self) -> Option<String> {
        Some("fake/lease-tests".into())
    }

    async fn create(&self, id: &str, _plan: &crate::ContainerPlan) -> Result<String, RuntimeError> {
        if !self.record_exists(id) {
            self.created_without_record
                .lock()
                .unwrap()
                .push(id.to_owned());
        }
        self.creates.lock().unwrap().push(id.to_owned());
        // The daemon has the container before it answers: a caller that dies
        // while waiting for the answer strands exactly this object.
        let container_id = format!("cid-{id}");
        self.containers
            .lock()
            .unwrap()
            .insert(container_id.clone(), id.to_owned());
        if self.hold_create.load(Ordering::SeqCst) {
            self.create_started.notify_waiters();
            self.allow_create
                .acquire()
                .await
                .expect("create gate stays open")
                .forget();
        }
        Ok(container_id)
    }

    async fn preflight_create_for_effect(
        &self,
        _context: &crate::ContainerRealizationContext<'_>,
        _plan: &crate::ContainerPlan,
        _realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn prepare_for_effect(
        &self,
        context: &crate::ContainerRealizationContext<'_>,
        plan: &crate::ContainerPlan,
        _realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<crate::PreparedContainer, RuntimeError> {
        self.create(context.scope, plan).await.map(|locator| {
            crate::PreparedContainer::fresh(locator, context.attempt.as_str().to_owned())
        })
    }

    async fn activate_for_effect(
        &self,
        _context: &crate::ContainerRealizationContext<'_>,
        _plan: &crate::ContainerPlan,
        _realization_fingerprint: &pc::SandboxRealizationFingerprint,
        prepared_locator: &str,
    ) -> Result<String, RuntimeError> {
        Ok(prepared_locator.to_owned())
    }

    async fn open_channel(
        &self,
        _container_id: &str,
    ) -> Result<Box<dyn crate::AgentChannel>, RuntimeError> {
        Err(RuntimeError::Backend("not used by lease tests".into()))
    }

    async fn inspect(&self, container_id: &str) -> Result<crate::ContainerState, RuntimeError> {
        Ok(
            if self.containers.lock().unwrap().contains_key(container_id) {
                crate::ContainerState::Running
            } else {
                crate::ContainerState::Gone
            },
        )
    }

    async fn wait(&self, _container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        Ok(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }

    async fn poll(&self, _container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        Ok(None)
    }

    async fn signal(&self, _container_id: &str, _signal: pc::Signal) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        _container_id: &str,
        _artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        Err(RuntimeError::Backend("not used by lease tests".into()))
    }

    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError> {
        if self.containers.lock().unwrap().contains_key(container_id) {
            Ok(())
        } else {
            Err(RuntimeError::NotFound(container_id.into()))
        }
    }

    /// Docker's by-id removal: an absent container is a 404 error.
    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        if self
            .fail_handle_removal
            .lock()
            .unwrap()
            .contains(container_id)
        {
            return Err(RuntimeError::Backend("daemon refused removal".into()));
        }
        self.handle_removals
            .lock()
            .unwrap()
            .push(container_id.to_owned());
        match self.containers.lock().unwrap().remove(container_id) {
            Some(_) => Ok(()),
            None => Err(RuntimeError::NotFound(container_id.into())),
        }
    }

    /// Docker's by-name removal: absence is success.
    async fn remove_unreceipted_scope(&self, scope: &str) -> Result<(), RuntimeError> {
        if self.hold_removal.load(Ordering::SeqCst) {
            self.removal_started.notify_waiters();
            self.allow_removal
                .acquire()
                .await
                .expect("removal gate stays open")
                .forget();
        }
        self.unreceipted_removals
            .lock()
            .unwrap()
            .push(scope.to_owned());
        self.vanish(scope);
        Ok(())
    }
}

fn policy() -> WarmCapacityLeasePolicy {
    WarmCapacityLeasePolicy::new(
        Duration::from_millis(TTL),
        Duration::from_secs(15),
        Duration::from_secs(30),
    )
    .unwrap()
}

fn leases(daemon: &FakeDaemon, clock: &Arc<ManualClock>) -> WarmCapacityLeases {
    WarmCapacityLeases::new(daemon.lease_root.path(), policy(), clock.clone())
}

/// A Worker incarnation over the shared daemon. Maintenance is off so every
/// renewal and reclamation in a test is one the test performed.
async fn worker(
    daemon: &Arc<FakeDaemon>,
    clock: &Arc<ManualClock>,
    size: usize,
) -> WarmContainerPool<FakeDaemon> {
    WarmContainerPool::new(
        Arc::new(ContainerProvider::for_test(daemon.clone(), "agent:test")),
        size,
    )
    .with_capacity_leases(leases(daemon, clock).without_maintenance())
    .await
    .expect("leased warm pool")
}

/// Process death: no `shutdown`, no destructor, no further renewal.
fn kill(pool: WarmContainerPool<FakeDaemon>) {
    std::mem::forget(pool);
}

fn ready_scopes(pool: &WarmContainerPool<FakeDaemon>) -> BTreeSet<String> {
    pool.warm
        .lock()
        .unwrap()
        .values()
        .flat_map(|entry| entry.ready.iter().map(|sandbox| sandbox.id.clone()))
        .collect()
}

/// Wait until every off-path replenishment of `pool` has returned, on the
/// pool's own in-flight counter rather than on elapsed time.
async fn replenishment_settled(pool: &WarmContainerPool<FakeDaemon>) {
    loop {
        let changed = pool.create_changed.notified();
        if pool.in_flight_creates.load(Ordering::SeqCst) == 0 {
            return;
        }
        changed.await;
    }
}

fn container_id(environment: &Arc<dyn crate::ContainerEnvironment>) -> String {
    crate::DecodedContainerHandle::decode(&pc::Sandbox::handle(environment.as_ref()))
        .expect("container handle")
        .payload
        .container_id
}

/// L1: while its owner renews, a peer never reclaims — one millisecond before
/// the deadline, and after the original deadline once it was renewed.
#[tokio::test]
async fn l1_a_live_peers_lease_is_never_reclaimed() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let owner = worker(&daemon, &clock, 1).await;
    assert_eq!(owner.prewarm(&spec("l1", &[]), 1).await.unwrap(), 1);
    let scope = ready_scopes(&owner).into_iter().next().unwrap();
    let peer = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL - 1);
    assert_eq!(
        peer.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L1 one millisecond before the deadline"
    );
    owner.renew_capacity_leases().await.unwrap();
    clock.set(T0 + TTL + 5);
    assert_eq!(
        peer.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L1 a renewed record outlives its original deadline"
    );
    assert!(daemon.unreceipted_removals().is_empty(), "L1");
    assert_eq!(daemon.physical(), BTreeSet::from([scope.clone()]), "L1");

    let creates = daemon.creates();
    let environment = owner
        .create_unattached_environment(&spec("session", &[]))
        .await
        .unwrap();
    assert_eq!(
        container_id(&environment),
        format!("cid-{scope}"),
        "L1 still usable"
    );
    assert_eq!(
        daemon.creates(),
        creates,
        "L1 the checkout was warm, not cold"
    );
    replenishment_settled(&owner).await;
    owner.shutdown().await;
    peer.shutdown().await;
}

/// L1 at the boundary: a record renewed at exactly its deadline was never
/// claimed, so the renewal holds it and the peer finds it live.
#[tokio::test]
async fn l1_a_lease_renewed_exactly_at_its_deadline_is_not_reclaimed() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let owner = worker(&daemon, &clock, 1).await;
    owner.prewarm(&spec("boundary", &[]), 1).await.unwrap();
    let peer = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    owner.renew_capacity_leases().await.unwrap();
    assert_eq!(
        peer.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L1 renewed at the boundary"
    );
    assert_eq!(owner.lease_ledger_for_test().held_scopes().len(), 1, "L1");
    owner.shutdown().await;
    peer.shutdown().await;
}

/// L2 at the boundary: an unrenewed record is live one millisecond before its
/// deadline and reclaimed at exactly its deadline, once.
#[tokio::test]
async fn l2_an_unrenewed_lease_is_reclaimed_at_exactly_its_deadline_and_only_once() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dead = worker(&daemon, &clock, 1).await;
    dead.prewarm(&spec("expiry", &[]), 1).await.unwrap();
    let scope = ready_scopes(&dead).into_iter().next().unwrap();
    kill(dead);
    let successor = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL - 1);
    assert!(
        successor
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed
            .is_empty(),
        "L1 before the deadline"
    );
    clock.set(T0 + TTL);
    let reclaimed = successor.reclaim_expired_capacity().await.unwrap();
    assert_eq!(reclaimed.reclaimed, vec![scope.clone()], "L2");
    assert_eq!(daemon.unreceipted_removals(), vec![scope], "L2");
    assert!(daemon.physical().is_empty(), "L2");
    assert!(daemon.records().is_empty(), "L2 the claim is deleted");
    assert_eq!(
        successor.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L2 exactly once"
    );
    assert_eq!(daemon.unreceipted_removals().len(), 1, "L2 exactly once");
    successor.shutdown().await;
}

/// L2: two reclaimers race over the same expired record. The first holds its
/// claim across a removal that is still in flight; the second finds only a
/// live claim and removes nothing. Neither fails.
#[tokio::test]
async fn l2_two_racing_reclaimers_remove_expired_capacity_exactly_once() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dead = worker(&daemon, &clock, 1).await;
    dead.prewarm(&spec("race", &[]), 1).await.unwrap();
    let scope = ready_scopes(&dead).into_iter().next().unwrap();
    kill(dead);
    let first = Arc::new(worker(&daemon, &clock, 1).await);
    let second = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    daemon.hold_removal.store(true, Ordering::SeqCst);
    let started = daemon.removal_started.notified();
    let in_flight = {
        let first = first.clone();
        tokio::spawn(async move { first.reclaim_expired_capacity().await })
    };
    started.await;
    assert_eq!(
        second.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L2 the losing reclaimer neither removes nor fails"
    );
    daemon.allow_removal.add_permits(1);
    let won = in_flight.await.unwrap().unwrap();
    assert_eq!(won.reclaimed, vec![scope.clone()], "L2");
    assert!(won.failed.is_empty(), "L2");
    assert_eq!(
        daemon.unreceipted_removals(),
        vec![scope],
        "L2 exactly once"
    );
    assert!(daemon.records().is_empty(), "L2");
    daemon.hold_removal.store(false, Ordering::SeqCst);
    first.shutdown().await;
    second.shutdown().await;
}

/// L2: the rename is the linearization point. A reclaimer that observed the
/// record but lost the rename gets `Lost` and has nothing to remove.
#[tokio::test]
async fn l2_a_reclaimer_that_loses_the_rename_removes_nothing() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dead = worker(&daemon, &clock, 1).await;
    dead.prewarm(&spec("rename", &[]), 1).await.unwrap();
    kill(dead);
    let winner = worker(&daemon, &clock, 1).await;
    let loser = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    let seen = loser.lease_ledger_for_test().store().observe().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        winner
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed
            .len(),
        1
    );
    assert_eq!(
        loser
            .lease_ledger_for_test()
            .store()
            .claim(&seen[0])
            .unwrap(),
        ClaimOutcome::Lost,
        "L2"
    );
    assert_eq!(daemon.unreceipted_removals().len(), 1, "L2");
    winner.shutdown().await;
    loser.shutdown().await;
}

/// L1 under a race: the owner renews after the reclaimer observed the record
/// as expired but before it renamed it. The post-claim re-read sees the
/// renewal and puts the record back; the owner keeps and can hand out its
/// container.
#[tokio::test]
async fn l1_a_renewal_that_lands_between_observation_and_claim_keeps_the_lease() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let owner = worker(&daemon, &clock, 1).await;
    owner.prewarm(&spec("renew-race", &[]), 1).await.unwrap();
    let scope = ready_scopes(&owner).into_iter().next().unwrap();
    let reclaimer = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    let seen = reclaimer.lease_ledger_for_test().store().observe().unwrap();
    assert_eq!(seen[0].kind, RecordKind::Lease);
    assert!(
        seen[0]
            .deadline
            .is_expired_at(UtcTime::from_epoch_ms(T0 + TTL)),
        "observed as expired"
    );
    owner.renew_capacity_leases().await.unwrap();
    assert_eq!(
        reclaimer
            .lease_ledger_for_test()
            .store()
            .claim(&seen[0])
            .unwrap(),
        ClaimOutcome::Live,
        "L1 the raced claim is put back"
    );
    assert_eq!(daemon.records(), vec![format!("{scope}.lease")], "L1");
    assert!(daemon.unreceipted_removals().is_empty(), "L1");
    assert_eq!(
        owner.lease_ledger_for_test().store().renew(&scope).unwrap(),
        Hold::Held,
        "L1 the owner still holds it"
    );
    let creates = daemon.creates();
    let environment = owner
        .create_unattached_environment(&spec("session", &[]))
        .await
        .unwrap();
    assert_eq!(container_id(&environment), format!("cid-{scope}"), "L1");
    assert_eq!(daemon.creates(), creates, "L1 warm checkout");
    replenishment_settled(&owner).await;
    owner.shutdown().await;
    reclaimer.shutdown().await;
}

/// L2 under the other race: the owner renews *after* the claim. Its renewal
/// finds the record gone, so it forgets the container without removing it; the
/// claimant alone removes it, once.
#[tokio::test]
async fn l2_a_renewal_after_the_claim_is_lost_and_the_owner_forgets_the_container() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let stalled = worker(&daemon, &clock, 1).await;
    stalled.prewarm(&spec("stalled", &[]), 1).await.unwrap();
    let scope = ready_scopes(&stalled).into_iter().next().unwrap();
    let reclaimer = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    let seen = reclaimer.lease_ledger_for_test().store().observe().unwrap();
    let ClaimOutcome::Claimed(_) = reclaimer
        .lease_ledger_for_test()
        .store()
        .claim(&seen[0])
        .unwrap()
    else {
        panic!("an expired record is claimable");
    };
    stalled.renew_capacity_leases().await.unwrap();
    assert!(ready_scopes(&stalled).is_empty(), "L2 the owner forgot it");
    assert!(
        stalled.lease_ledger_for_test().held_scopes().is_empty(),
        "L2"
    );
    assert!(
        daemon.handle_removals.lock().unwrap().is_empty(),
        "L2 the owner does not remove what the claimant owns"
    );

    // The claimant's own deadline passes (it was, say, killed after claiming);
    // the next pass re-claims and finishes.
    clock.set(T0 + 2 * TTL);
    assert_eq!(
        reclaimer
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed,
        vec![scope.clone()]
    );
    assert_eq!(daemon.unreceipted_removals(), vec![scope], "L2 once");
    stalled.shutdown().await;
    reclaimer.shutdown().await;
}

/// L2c: a reclaimer killed between claiming and removing leaves a claim with
/// its own deadline. It is honoured until then and finished afterwards.
#[tokio::test]
async fn l2c_a_crashed_reclaimers_claim_is_finished_by_the_next_reclaimer() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dead = worker(&daemon, &clock, 1).await;
    dead.prewarm(&spec("crashed-claim", &[]), 1).await.unwrap();
    let scope = ready_scopes(&dead).into_iter().next().unwrap();
    kill(dead);
    let crashed = worker(&daemon, &clock, 1).await;
    let next = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    let seen = crashed.lease_ledger_for_test().store().observe().unwrap();
    assert!(matches!(
        crashed
            .lease_ledger_for_test()
            .store()
            .claim(&seen[0])
            .unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    kill(crashed);

    clock.set(T0 + 2 * TTL - 1);
    assert_eq!(
        next.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L2c a live claim is honoured"
    );
    clock.set(T0 + 2 * TTL);
    assert_eq!(
        next.reclaim_expired_capacity().await.unwrap().reclaimed,
        vec![scope.clone()],
        "L2c"
    );
    assert_eq!(daemon.unreceipted_removals(), vec![scope], "L2c once");
    assert!(daemon.records().is_empty(), "L2c");
    next.shutdown().await;
}

/// L3: a container checked out to a Session left warm capacity by consuming
/// its record, so no reclaimer can reach it — whatever time it is. The warm
/// capacity the dead Worker still held is reclaimed around it.
#[tokio::test]
async fn l3_a_checked_out_container_is_never_reclaimed_whatever_its_lease() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dead = worker(&daemon, &clock, 2).await;
    dead.prewarm(&spec("checkout", &[]), 2).await.unwrap();
    let environment = dead
        .create_unattached_environment(&spec("session", &[]))
        .await
        .unwrap();
    let checked_out = container_id(&environment)
        .strip_prefix("cid-")
        .unwrap()
        .to_owned();
    assert!(
        !daemon.record_exists(&checked_out),
        "L3 checkout consumed the record before hand-out"
    );
    replenishment_settled(&dead).await;
    let warm = ready_scopes(&dead);
    assert_eq!(warm.len(), 2, "one prewarmed + one replenished");
    kill(dead);

    let successor = worker(&daemon, &clock, 2).await;
    clock.set(T0 + 10 * TTL);
    let reclaimed = successor.reclaim_expired_capacity().await.unwrap();
    assert_eq!(
        reclaimed.reclaimed.into_iter().collect::<BTreeSet<_>>(),
        warm,
        "L2 the dead Worker's warm capacity is reclaimed"
    );
    assert_eq!(
        daemon.physical(),
        BTreeSet::from([checked_out.clone()]),
        "L3 the Session's container survives"
    );
    assert!(!daemon.unreceipted_removals().contains(&checked_out), "L3");
    assert!(daemon.records().is_empty());
    successor.shutdown().await;
}

/// L3 from the owner's side: a candidate whose record a reclaimer already
/// claimed is never handed to a Session — the checkout cold-creates instead,
/// and the claimant still removes the candidate exactly once.
#[tokio::test]
async fn l3_a_candidate_whose_record_was_claimed_is_never_handed_out() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let stalled = worker(&daemon, &clock, 1).await;
    stalled.prewarm(&spec("claimed", &[]), 1).await.unwrap();
    let candidate = ready_scopes(&stalled).into_iter().next().unwrap();
    let reclaimer = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    let seen = reclaimer.lease_ledger_for_test().store().observe().unwrap();
    assert!(matches!(
        reclaimer
            .lease_ledger_for_test()
            .store()
            .claim(&seen[0])
            .unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    let creates = daemon.creates();
    let environment = stalled
        .create_unattached_environment(&spec("session", &[]))
        .await
        .unwrap();
    let session_container = container_id(&environment);
    assert_ne!(
        session_container,
        format!("cid-{candidate}"),
        "L3 the claimed candidate is not handed out"
    );
    assert_eq!(
        daemon.creates(),
        creates + 1,
        "L3 the checkout cold-created"
    );
    replenishment_settled(&stalled).await;
    assert!(!ready_scopes(&stalled).contains(&candidate), "L3 forgotten");

    clock.set(T0 + 2 * TTL);
    reclaimer.reclaim_expired_capacity().await.unwrap();
    assert_eq!(
        daemon
            .unreceipted_removals()
            .iter()
            .filter(|scope| **scope == candidate)
            .count(),
        1,
        "L3 the claimant removes it once"
    );
    assert!(
        daemon
            .physical()
            .contains(session_container.strip_prefix("cid-").unwrap()),
        "L3 the Session's cold container is untouched"
    );
    stalled.shutdown().await;
    reclaimer.shutdown().await;
}

/// L4: the daemon never sees a warm create whose record is not already on
/// disk, on both the startup and the replenishment path.
#[tokio::test]
async fn l4_the_record_exists_before_the_runtime_sees_the_create() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let pool = worker(&daemon, &clock, 2).await;
    pool.prewarm(&spec("ordering", &[]), 2).await.unwrap();
    let _environment = pool
        .create_unattached_environment(&spec("session", &[]))
        .await
        .unwrap();
    replenishment_settled(&pool).await;
    assert_eq!(
        daemon.creates(),
        3,
        "two startup creates and one replenishment"
    );
    assert!(
        daemon.created_without_record.lock().unwrap().is_empty(),
        "L4 {:?}",
        daemon.created_without_record.lock().unwrap()
    );
    pool.shutdown().await;
}

/// L4: no record, no create. A mint that cannot publish fails the warmup
/// before the daemon is asked for anything.
#[tokio::test]
async fn l4_a_failed_mint_creates_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let pool = worker(&daemon, &clock, 1).await;
    let domain = pool.lease_ledger_for_test().store().directory().to_owned();
    std::fs::set_permissions(&domain, std::fs::Permissions::from_mode(0o500)).unwrap();
    let warmed = pool.prewarm(&spec("unpublishable", &[]), 1).await;
    std::fs::set_permissions(&domain, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(warmed.is_err(), "L4");
    assert_eq!(daemon.creates(), 0, "L4");
    pool.shutdown().await;
}

/// A domain directory removed by a peer's shutdown (it was empty) is
/// re-established by the next mint rather than failing it, and a pool that
/// shuts down with nothing left removes its empty domain directory.
#[tokio::test]
async fn an_empty_domain_directory_is_retired_and_re_established() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let peer = worker(&daemon, &clock, 1).await;
    let pool = worker(&daemon, &clock, 1).await;
    let domain = pool.lease_ledger_for_test().store().directory().to_owned();
    peer.shutdown().await;
    assert!(!domain.exists(), "an empty domain is retired");
    assert_eq!(pool.prewarm(&spec("again", &[]), 1).await.unwrap(), 1);
    assert_eq!(daemon.records().len(), 1, "L4 the record was published");
    pool.shutdown().await;
    assert!(!domain.exists());
}

/// L4 across process death: a Worker killed while its create was still in
/// flight at the daemon leaves the container *and* its record. The durable
/// claim names the reconciler; a successor reclaims once the record expires.
#[tokio::test]
async fn l4_a_create_in_flight_when_its_worker_died_is_reclaimed_from_its_record() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dying = Arc::new(worker(&daemon, &clock, 1).await);
    daemon.hold_create.store(true, Ordering::SeqCst);
    let started = daemon.create_started.notified();
    let warming = {
        let dying = dying.clone();
        tokio::spawn(async move {
            let _ = dying.prewarm(&spec("in-flight", &[]), 1).await;
        })
    };
    started.await;
    let scope = daemon.creates.lock().unwrap()[0].clone();
    assert!(
        daemon.record_exists(&scope),
        "L4 enumerable while in flight"
    );
    warming.abort();
    let _ = warming.await;
    std::mem::forget(dying);
    daemon.hold_create.store(false, Ordering::SeqCst);
    assert_eq!(daemon.physical(), BTreeSet::from([scope.clone()]));

    let successor = worker(&daemon, &clock, 1).await;
    clock.set(T0 + TTL);
    assert_eq!(
        successor
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed,
        vec![scope],
        "L4 the stranded create is reclaimed from its record"
    );
    assert!(daemon.physical().is_empty());
    successor.shutdown().await;
}

/// L5: renewal gets at least three chances per TTL, and reclamation grace
/// covers a TTL — validated by the shared lease-timing policy.
#[test]
fn l5_the_policy_refuses_a_renew_interval_above_a_third_of_the_ttl() {
    let ttl = Duration::from_secs(60);
    assert!(
        WarmCapacityLeasePolicy::new(ttl, Duration::from_secs(21), Duration::from_secs(30))
            .is_err(),
        "L5"
    );
    assert!(
        WarmCapacityLeasePolicy::new(ttl, Duration::from_secs(20), Duration::from_secs(30)).is_ok()
    );
    let default = WarmCapacityLeasePolicy::default();
    assert!(default.renew_interval() * 3 <= default.ttl(), "L5 default");
}

/// L5: the pool's own maintenance loop renews without being asked. After one
/// round at `T0 + TTL - 1`, the record outlives its original deadline.
#[tokio::test(start_paused = true)]
async fn l5_the_maintenance_loop_renews_every_held_record() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let owner = WarmContainerPool::new(
        Arc::new(ContainerProvider::for_test(daemon.clone(), "agent:test")),
        1,
    )
    .with_capacity_leases(leases(&daemon, &clock))
    .await
    .unwrap();
    owner.prewarm(&spec("maintained", &[]), 1).await.unwrap();
    let mut rounds = owner.maintenance_rounds();
    let before = *rounds.borrow_and_update();
    clock.set(T0 + TTL - 1);
    rounds
        .wait_for(|completed| *completed > before)
        .await
        .expect("maintenance keeps running");

    let peer = worker(&daemon, &clock, 1).await;
    clock.set(T0 + TTL + 1);
    assert_eq!(
        peer.reclaim_expired_capacity().await.unwrap(),
        Default::default(),
        "L5 the loop renewed the record"
    );
    owner.shutdown().await;
    assert!(
        owner.maintenance.lock().unwrap().is_none(),
        "L5 shutdown stops the loop"
    );
    peer.shutdown().await;
}

/// L5: shutdown releases the record of every container it disposed and leaves
/// the record of one it could not, which a later reclaimer then finishes.
#[tokio::test]
async fn l5_shutdown_releases_only_the_records_of_capacity_it_disposed() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let pool = worker(&daemon, &clock, 2).await;
    pool.prewarm(&spec("drain", &[]), 2).await.unwrap();
    let scopes = ready_scopes(&pool).into_iter().collect::<Vec<_>>();
    daemon
        .fail_handle_removal
        .lock()
        .unwrap()
        .insert(format!("cid-{}", scopes[1]));
    pool.shutdown().await;
    let remaining = daemon.records();
    assert_eq!(remaining.len(), 1, "L5 only the undisposed record remains");
    assert!(
        remaining[0].starts_with(&format!("{}.claim.", scopes[1])),
        "L5 disposal claimed its own record first: {remaining:?}"
    );
    assert!(
        pool.lease_ledger_for_test().held_scopes().is_empty(),
        "L5 no longer renewed"
    );

    let successor = worker(&daemon, &clock, 1).await;
    clock.set(T0 + TTL);
    assert_eq!(
        successor
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed,
        vec![scopes[1].clone()],
        "L5 a later reclaimer finishes it"
    );
    assert!(daemon.physical().is_empty());
    successor.shutdown().await;
}

/// L6: a record whose container is already gone (removed behind the Worker's
/// back) is collected by reclamation: absence is success, the record goes.
#[tokio::test]
async fn l6_a_record_whose_container_is_gone_is_collected() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let dead = worker(&daemon, &clock, 1).await;
    dead.prewarm(&spec("orphan", &[]), 1).await.unwrap();
    let scope = ready_scopes(&dead).into_iter().next().unwrap();
    kill(dead);
    daemon.vanish(&scope);

    let successor = worker(&daemon, &clock, 1).await;
    clock.set(T0 + TTL);
    assert_eq!(
        successor
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed,
        vec![scope],
        "L6"
    );
    assert!(daemon.records().is_empty(), "L6 not retained forever");
    successor.shutdown().await;
}

/// L6 in a live pool: a ready container that stopped existing fails its
/// liveness probe at renewal, leaves ready capacity, stops being renewed, and
/// its record is collected once it expires.
#[tokio::test]
async fn l6_a_live_pool_drops_a_ready_container_that_vanished() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let pool = worker(&daemon, &clock, 1).await;
    pool.prewarm(&spec("vanished", &[]), 1).await.unwrap();
    let scope = ready_scopes(&pool).into_iter().next().unwrap();
    daemon.vanish(&scope);

    pool.renew_capacity_leases().await.unwrap();
    assert!(ready_scopes(&pool).is_empty(), "L6 dropped from capacity");
    assert!(
        pool.lease_ledger_for_test().held_scopes().is_empty(),
        "L6 no longer renewed"
    );
    clock.set(T0 + TTL);
    assert_eq!(
        pool.reclaim_expired_capacity().await.unwrap().reclaimed,
        vec![scope],
        "L6"
    );
    assert!(daemon.records().is_empty(), "L6 collected");
    pool.shutdown().await;
}

/// L6: a staged mint whose creator died before publishing it names no
/// container (the create starts only after the link) and is collected.
#[tokio::test]
async fn l6_a_stale_staged_mint_is_collected() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let pool = worker(&daemon, &clock, 1).await;
    let staged: PathBuf = pool
        .lease_ledger_for_test()
        .store()
        .directory()
        .join("warmpool-dead-0.mint");
    std::fs::File::create(&staged)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_millis(T0))
        .unwrap();
    clock.set(T0 + TTL - 1);
    pool.reclaim_expired_capacity().await.unwrap();
    assert!(staged.exists(), "L6 a recent mint may still be publishing");
    clock.set(T0 + TTL);
    pool.reclaim_expired_capacity().await.unwrap();
    assert!(!staged.exists(), "L6");
    assert!(
        daemon.unreceipted_removals().is_empty(),
        "a mint names no container"
    );
    pool.shutdown().await;
}

/// A restarted Worker is a new incarnation: it cannot tell its predecessor's
/// records from a live peer's, so it waits out their deadline — then reclaims
/// them, and never its own fresh capacity.
#[tokio::test]
async fn l1_l2_a_restarted_worker_waits_out_its_predecessors_lease_then_reclaims_it() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let predecessor = worker(&daemon, &clock, 2).await;
    predecessor.prewarm(&spec("restart", &[]), 2).await.unwrap();
    let old = ready_scopes(&predecessor);
    kill(predecessor);

    let restarted = worker(&daemon, &clock, 2).await;
    assert_eq!(
        daemon.physical(),
        old,
        "L1 startup reclamation left live records"
    );
    clock.set(T0 + TTL - 1);
    assert!(
        restarted
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed
            .is_empty()
    );
    clock.set(T0 + TTL);
    restarted.prewarm(&spec("restart", &[]), 1).await.unwrap();
    let fresh = ready_scopes(&restarted);
    let reclaimed = restarted.reclaim_expired_capacity().await.unwrap();
    assert_eq!(
        reclaimed.reclaimed.into_iter().collect::<BTreeSet<_>>(),
        old,
        "L2"
    );
    assert_eq!(
        daemon.physical(),
        fresh,
        "L1 its own fresh capacity survives"
    );
    restarted.shutdown().await;
}

/// L7: the production default is maintained (renewal + periodic reclamation
/// run without being asked), uses the validated default timing, and lives in a
/// per-user directory beside the retained host staging.
#[test]
fn l7_the_host_default_is_maintained_and_per_user() {
    let leases = WarmCapacityLeases::host_default(awaken_lease_contract::system_clock()).unwrap();
    assert!(leases.maintained(), "L7 production leases are maintained");
    assert_eq!(leases.policy(), WarmCapacityLeasePolicy::default());
    let name = leases
        .root()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with("awaken-warm-capacity-leases-")
            && name["awaken-warm-capacity-leases-".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_digit()),
        "{name}"
    );
    assert_eq!(leases.root().parent(), Some(std::env::temp_dir().as_path()));
}

/// L2/L5: an owner disposing capacity and a reclaimer finishing the same
/// expired record never both remove one container. The owner takes its own
/// record through the same rename a reclaimer uses, so whoever loses it does
/// not touch the daemon.
#[tokio::test]
async fn l2_an_owner_and_a_reclaimer_never_both_remove_one_container() {
    let daemon = FakeDaemon::new();
    let clock = manual_clock(T0);
    let stalled = worker(&daemon, &clock, 1).await;
    let shape = spec("both-remove", &[]);
    stalled.prewarm(&shape, 1).await.unwrap();
    let scope = ready_scopes(&stalled).into_iter().next().unwrap();
    let reclaimer = worker(&daemon, &clock, 1).await;

    clock.set(T0 + TTL);
    let seen = reclaimer.lease_ledger_for_test().store().observe().unwrap();
    assert!(matches!(
        reclaimer
            .lease_ledger_for_test()
            .store()
            .claim(&seen[0])
            .unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    // The stalled owner now discards the very shape the reclaimer holds.
    stalled.discard(&shape).await;
    assert!(
        daemon.handle_removals.lock().unwrap().is_empty(),
        "L5 the owner that lost its record removes nothing"
    );
    assert!(daemon.unreceipted_removals().is_empty());

    clock.set(T0 + 2 * TTL);
    assert_eq!(
        reclaimer
            .reclaim_expired_capacity()
            .await
            .unwrap()
            .reclaimed,
        vec![scope.clone()],
        "L2 the claimant finishes it"
    );
    assert_eq!(
        daemon.unreceipted_removals(),
        vec![scope],
        "L2 exactly once"
    );
    assert!(daemon.physical().is_empty());
    stalled.shutdown().await;
    reclaimer.shutdown().await;
}
