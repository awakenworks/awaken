use super::*;
use awaken_session_run_delivery::{AuthorityCommits, SessionCommit};

#[tokio::test]
async fn raw_local_commit_selection_reuses_runtime_authority_without_session_context() {
    // Cause/effect graph: C1 one original Runtime authority; C2 SDK and shared
    // readers open the same physical Session; C3 a child partition was never
    // opened. L1 before C2 -> no partition; L2 C1+C2 -> identical underlying
    // LocalCommit and committed authority, not a second store; L3 C3 -> false.
    // No resident context, Environment, model or lifecycle registry is created.
    let runtime = Arc::new(EphemeralRuntimeAuthority::new(awaken_time::system_clock()));
    let selected: Arc<dyn SessionAuthority> = Arc::new(LocalAuthority::new(runtime.clone()));
    assert!(
        !selected.durable_thread_exists("parent").await.unwrap(),
        "L1"
    );
    let shared: Arc<dyn SessionCommitSource> = selected.clone();
    assert_eq!(
        Arc::as_ptr(&selected) as *const (),
        Arc::as_ptr(&shared) as *const (),
        "L2 same authority through the shared role"
    );
    let commit = shared.open_for_read("parent").await.unwrap();
    let expected = AuthorityCommits::new(runtime)
        .open_for_read("parent")
        .await
        .unwrap();
    let SessionCommit::Local(actual) = commit.as_ref() else {
        panic!("L2 local source");
    };
    let SessionCommit::Local(original) = expected.as_ref() else {
        panic!("L2 shared source");
    };
    assert!(
        Arc::ptr_eq(actual, original),
        "L2 one original physical commit"
    );
    assert!(
        commit.committed_authority().is_some(),
        "L2 actual committed authority"
    );
    assert!(
        selected.durable_thread_exists("parent").await.unwrap(),
        "L2 original partition"
    );
    assert!(
        !selected.durable_thread_exists("child").await.unwrap(),
        "L3 no child partition"
    );
}

#[tokio::test]
async fn raw_remote_commit_selection_keeps_fresh_projection_and_no_local_authority() {
    // R1 an explicit database-less authority -> one fresh recovery projection
    // per raw open, with no committed write authority. R2 a repeated open of
    // the same physical Session -> another cache, not the resident cache.
    // R3 durable existence stays false; no transport or provider is consulted.
    // InstalledCommitSource's original native tests own resident reuse/locking;
    // these rows must remain distinct from that higher-level read contract.
    let selected: Arc<dyn SessionAuthority> = Arc::new(crate::host::test_upstream_authority());
    let shared: Arc<dyn SessionCommitSource> = selected.clone();
    assert_eq!(
        Arc::as_ptr(&selected) as *const (),
        Arc::as_ptr(&shared) as *const (),
        "R1 same selected Worker authority"
    );
    let first = shared.open_for_read("parent").await.unwrap();
    let second = selected.open_for_read("parent").await.unwrap();
    assert!(
        first.committed_authority().is_none(),
        "R1 no local write authority"
    );
    assert!(
        second.committed_authority().is_none(),
        "R2 no local write authority"
    );
    let first = first.recovery_projection().expect("R1 recovery projection");
    let second = second
        .recovery_projection()
        .expect("R2 recovery projection");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "R2 each raw open keeps a fresh cache"
    );
    assert!(
        !selected.durable_thread_exists("parent").await.unwrap(),
        "R3 no local partition"
    );
}
