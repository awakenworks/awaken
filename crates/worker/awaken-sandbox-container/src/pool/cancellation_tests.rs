//! What survives a warm create whose caller is cancelled mid-await.
//!
//! These use the crate's own `ContainerRuntime` fake, not a daemon: the leak is
//! a Worker-side ordering defect, and a fake is the only way to hold a create
//! open at the exact instant the caller is dropped.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::tests::{RecordingRuntime, recording_pool, spec};
use super::{WarmContainerPool, next_warm_scope};

/// Drive `prewarm` to the point where the runtime has begun creating, then hand
/// the caller's future back so the test can decide its fate.
async fn prewarm_blocked_in_create(
    runtime: &Arc<RecordingRuntime>,
    pool: &Arc<WarmContainerPool<RecordingRuntime>>,
    spec: &awaken_provisioning_contract::SandboxSpec,
) -> tokio::task::JoinHandle<()> {
    runtime.block_create.store(true, Ordering::SeqCst);
    let started = runtime.create_started.notified();
    let warming = {
        let pool = pool.clone();
        let spec = spec.clone();
        tokio::spawn(async move {
            // Abort lands here as a drop of this future while `prewarm` is
            // suspended inside the claimed create — the exact shape a cancelled
            // request or a dropped timeout produces in production.
            let _ = pool.prewarm(&spec, 1).await;
        })
    };
    started.await;
    warming
}

/// Cause/effect design — the leak this closes:
///
/// C1 = a warm create is in flight; C2 = the caller's future is dropped before
/// the create returns; C3 = `shutdown` runs; C4 = the create returned normally.
/// E1 = the warm scope is on record as created-by-nobody; E2 = `shutdown`
/// disposes that scope through the unreceipted-capacity port; E3 = the record
/// is empty afterwards; E4 = nothing is recorded at all.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect    |
/// | W1   | 1  | 1  | 0  | 0  | E1        |
/// | W2   | 1  | 1  | 1  | 0  | E1,E2,E3  |
/// | W3   | 1  | 0  | -  | 1  | E4        |
///
/// W3 is the half that keeps the record honest: a settled claim never retains,
/// so a create that came back does not leave a phantom scope for `shutdown` to
/// delete a live warm container by.
#[tokio::test]
async fn a_prewarm_dropped_mid_create_leaves_its_warm_scope_for_shutdown_to_dispose() {
    let runtime = Arc::new(RecordingRuntime::ready());
    let pool = Arc::new(recording_pool(runtime.clone(), 1));
    let spec = spec("cancelled-prewarm", &[]);

    let warming = prewarm_blocked_in_create(&runtime, &pool, &spec).await;
    assert!(
        pool.unreceipted_warm_scopes().is_empty(),
        "W1 nothing is on record while the create is still owned"
    );
    warming.abort();
    let _ = warming.await;

    let recorded = pool.unreceipted_warm_scopes();
    assert_eq!(recorded.len(), 1, "W1");
    assert!(recorded[0].starts_with("warmpool-"), "W1 {recorded:?}");
    assert_eq!(
        pool.ready_len(&spec),
        0,
        "W1 the container never reached ready capacity, which is the leak"
    );

    // Let the abandoned create finish inside the runtime, exactly as a real
    // daemon would: the container exists and no Worker value refers to it.
    runtime.allow_create.add_permits(1);
    runtime.block_create.store(false, Ordering::SeqCst);
    pool.shutdown().await;

    assert_eq!(
        runtime.unreceipted_removals.lock().unwrap().as_slice(),
        recorded.as_slice(),
        "W2 shutdown disposes exactly the scope the claim recorded"
    );
    assert!(pool.unreceipted_warm_scopes().is_empty(), "W3");
}

/// The settled half of the same table (W3), kept separate so a failure names
/// which direction broke. A completed create must leave no record, or every
/// `shutdown` would try to delete a container that is legitimately in capacity.
#[tokio::test]
async fn a_warm_create_that_returns_records_nothing_to_dispose() {
    let runtime = Arc::new(RecordingRuntime::ready());
    let pool = recording_pool(runtime.clone(), 1);
    let spec = spec("settled-prewarm", &[]);

    assert_eq!(pool.prewarm(&spec, 1).await.unwrap(), 1);
    assert!(pool.unreceipted_warm_scopes().is_empty(), "W3");
    pool.shutdown().await;
    assert!(
        runtime.unreceipted_removals.lock().unwrap().is_empty(),
        "W3 a container in hand is disposed as a container, not as a name"
    );
    assert_eq!(runtime.removes.load(Ordering::SeqCst), 1);
}

/// A dropped create inside the readiness probe is the same leak one await
/// later: `ready_candidate` disposes a not-ready candidate across an await, and
/// a caller dropped there strands the identical container. The claim spans both
/// awaits, so the record covers both.
///
/// C1 = the runtime creates; C2 = the readiness probe fails the candidate;
/// C3 = the caller is dropped during the probe. E1 = the scope is on record.
/// Rule: (C1,C2,C3) -> E1.
#[tokio::test]
async fn a_prewarm_dropped_during_the_readiness_probe_is_also_on_record() {
    let runtime = Arc::new(RecordingRuntime::ready());
    let pool = Arc::new(recording_pool(runtime.clone(), 1));
    let spec = spec("cancelled-probe", &[]);

    let warming = prewarm_blocked_in_create(&runtime, &pool, &spec).await;
    // Release the create and immediately abort: the task resumes inside the
    // claimed effect, past `create_container` and into the probe.
    runtime.allow_create.add_permits(1);
    warming.abort();
    let _ = warming.await;

    assert_eq!(pool.unreceipted_warm_scopes().len(), 1, "E1");
    pool.shutdown().await;
    assert!(pool.unreceipted_warm_scopes().is_empty());
}

/// Two warm scopes minted by the same pool are distinct, and neither is keyed
/// by the bare pid the kernel recycles.
///
/// C1 = two scopes from one incarnation; C2 = two incarnations. E1 = distinct
/// names; E2 = no bare-pid prefix a restarted Worker could redraw.
/// Rule: (C1) -> E1; (C2) -> (E1,E2).
#[test]
fn warm_scopes_are_unique_per_incarnation_rather_than_per_pid() {
    let first = next_warm_scope("owner-a");
    let second = next_warm_scope("owner-a");
    assert_ne!(first, second, "E1");
    assert_ne!(next_warm_scope("owner-b"), next_warm_scope("owner-a"), "E1");
    let pid_prefix = format!("warmpool-{}-", std::process::id());
    assert!(!first.starts_with(&pid_prefix), "E2 {first}");
}
