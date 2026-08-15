use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_service_lifecycle::ServiceLifecycle;

#[tokio::test]
async fn critical_task_outcomes_drive_one_health_and_shutdown_contract() {
    // Cause/effect graph:
    // C1 a critical task remains pending; C2 it returns an error; C3 it panics;
    // C4 service cancellation is requested. Effects: E1 health remains ready;
    // E2 health becomes failed and wait_for_failure reports the task name/cause;
    // E3 cancellation is not classified as failure; E4 shutdown joins all tasks.
    // Constraints: the first terminal fault is authoritative, and every task
    // receives a child of the one service cancellation token.
    //
    // Decision table:
    // | Rule | C1 pending | C2 error | C3 panic | C4 cancel | Effects |
    // | R1   | yes        | no       | no       | no        | E1      |
    // | R2   | no         | yes      | no       | no        | E2      |
    // | R3   | no         | no       | yes      | no        | E2      |
    // | R4   | any        | no       | no       | yes       | E3+E4   |
    let group = ServiceLifecycle::new();
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let observed = cancellation_observed.clone();
    group.spawn("steady", move |cancel| async move {
        cancel.cancelled().await;
        observed.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert!(group.is_healthy(), "R1");

    group.spawn("failed", |_| async { Err("offline".to_owned()) });
    let failure = tokio::time::timeout(Duration::from_secs(1), group.wait_for_failure())
        .await
        .expect("R2 fault must be observable")
        .expect("R2 fault exists");
    assert_eq!(failure.task, "failed", "R2");
    assert!(failure.cause.contains("offline"), "R2");
    assert!(!group.is_healthy(), "R2");

    group
        .shutdown(Duration::from_secs(1))
        .await
        .expect("R4 cooperative tasks join");
    assert!(cancellation_observed.load(Ordering::SeqCst), "R4");

    let panicking = ServiceLifecycle::new();
    panicking.spawn("panicked", |_| async {
        panic!("boom");
        #[allow(unreachable_code)]
        Ok(())
    });
    let panic_failure = panicking
        .wait_for_failure()
        .await
        .expect("R3 panic must be observable");
    assert_eq!(panic_failure.task, "panicked", "R3");
    assert!(panic_failure.cause.contains("panicked"), "R3");
    panicking
        .shutdown(Duration::from_secs(1))
        .await
        .expect("R3 already-failed watcher joins");
}

#[tokio::test]
async fn shutdown_is_idempotent_and_aborts_only_after_the_drain_deadline() {
    // Cause/effect graph: C1 a task cooperates with cancellation; C2 a task
    // ignores cancellation; C3 shutdown is called again. Effects: E1 cooperative
    // completion is joined; E2 the deadline aborts and names only the stuck task;
    // E3 a repeated call observes the already-drained group and succeeds.
    // Decision table: R1=C1+!C2 -> E1; R2=C1+C2 -> E1+E2;
    // R3=(R1|R2)+C3 -> E3. No drop-only cleanup is accepted as a test oracle.
    let group = ServiceLifecycle::new();
    group.spawn("stuck", |_| async {
        std::future::pending::<()>().await;
        Ok(())
    });
    group.spawn("cooperative", |cancel| async move {
        cancel.cancelled().await;
        Ok(())
    });

    let error = group
        .shutdown(Duration::from_millis(20))
        .await
        .expect_err("R2 stuck task must cross the deadline");
    assert_eq!(error.timed_out, vec!["stuck"], "R1+R2");
    group
        .shutdown(Duration::from_millis(20))
        .await
        .expect("R3 repeated shutdown has no second task set");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdown_cannot_return_before_the_active_drain_finishes() {
    // C1 the leader owns a non-cooperative task; C2 a follower calls shutdown
    // after cancellation is visible but before the leader's deadline. E1 the
    // follower remains blocked; E2 the leader reports/aborts the stuck task;
    // E3 only then may the follower observe the drained registry and return.
    // The old check-and-take implementation violated E1 by returning Ok from
    // the follower while the task was still live in the leader's local set.
    let group = ServiceLifecycle::new();
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    group.spawn("stuck", move |cancel| async move {
        cancel.cancelled().await;
        let _ = cancelled_tx.send(());
        std::future::pending::<()>().await;
        Ok(())
    });

    let leader_group = group.clone();
    let leader =
        tokio::spawn(async move { leader_group.shutdown(Duration::from_millis(80)).await });
    cancelled_rx
        .await
        .expect("leader transferred and cancelled the task set");

    let follower_group = group.clone();
    let follower =
        tokio::spawn(async move { follower_group.shutdown(Duration::from_secs(1)).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        !follower.is_finished(),
        "a concurrent shutdown must wait for the active drain"
    );

    let leader_error = leader
        .await
        .expect("leader task")
        .expect_err("stuck task crosses the leader deadline");
    assert_eq!(leader_error.timed_out, vec!["stuck"]);
    follower
        .await
        .expect("follower task")
        .expect("follower returns only after the registry is drained");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_shutdown_have_one_atomic_ownership_boundary() {
    // FMECA cause/effect graph:
    // C1 registration linearizes before shutdown; C2 shutdown linearizes first;
    // C3 a rejected builder would create an observable side effect. Effects:
    // E1 C1 task is cancelled and joined; E2 C2 registration is rejected; E3
    // rejected builders are never invoked; E4 no task can be appended after the
    // shutdown task set was drained. Severity: orphan recurring tasks can mutate
    // state after process shutdown (S=9); occurrence was scheduler-dependent
    // (O=4); detection by ordinary tests was weak (D=8), RPN=288.
    // Mitigation: one mutex is the registration/shutdown linearization point.
    // Decision rules: R1=C1 -> E1+E4; R2=C2+C3 -> E2+E3+E4.
    let group = ServiceLifecycle::new();
    let ran = Arc::new(AtomicBool::new(false));
    let ran_before = ran.clone();
    group.spawn("owned", move |cancel| {
        ran_before.store(true, Ordering::SeqCst);
        async move {
            cancel.cancelled().await;
            Ok(())
        }
    });
    group.shutdown(Duration::from_secs(1)).await.expect("R1");
    assert!(ran.load(Ordering::SeqCst), "R1: registered future ran");

    let rejected_builder_ran = Arc::new(AtomicBool::new(false));
    let marker = rejected_builder_ran.clone();
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        group.spawn("late", move |_| {
            marker.store(true, Ordering::SeqCst);
            async { Ok(()) }
        });
    }));
    assert!(rejected.is_err(), "R2: late registration rejected");
    assert!(!rejected_builder_ran.load(Ordering::SeqCst), "R3");
    group.shutdown(Duration::from_millis(10)).await.expect("R4");
}
