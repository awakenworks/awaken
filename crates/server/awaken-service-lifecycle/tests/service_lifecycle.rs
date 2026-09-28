// @formal evidence service_lifecycle.concurrent_shutdown_waits_for_active_drain
// @formal evidence service_lifecycle.return_requires_all_tasks_stopped_or_aborted
// @formal evidence service_lifecycle.shutdown_fences_task_registration
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
async fn timeout_requests_task_abort_and_joins_watcher_before_task_stops() {
    struct DropSignal {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
        }
    }

    // Cause/effect graph: C1 a registered task ignores cancellation; C2 the
    // shared deadline expires; C3 task abort is requested; C4 watcher abort is
    // requested while the task is synchronously non-yielding. Effects: E1 C3
    // eventually drops a pending task Future; E2 C4 lets shutdown join the
    // watcher and return while the non-yielding task is still live; E3 the exact
    // task is named. Decision table: R1=C1+C2+!C3 -> !E1;
    // R2=C1+C2+C3+!C4 -> E1 but no bounded E2; R3=C1+C2+C3+C4 -> E1+E2+E3.
    // This establishes an abort request plus watcher completion, not task
    // termination at shutdown return; Tokio scheduling and AbortHandle semantics
    // remain TCB.

    // R1/R3: a pending Future is eventually dropped only because the timeout
    // branch requests abort of the exact task.
    let group = ServiceLifecycle::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (drop_entered_tx, drop_entered_rx) = tokio::sync::oneshot::channel();
    group.spawn("pending", |_| async move {
        let _drop_signal = DropSignal {
            entered: Some(drop_entered_tx),
        };
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
        Ok(())
    });
    started_rx.await.expect("R1 task installed its Drop signal");
    let result = group
        .shutdown(Duration::from_millis(10))
        .await
        .expect_err("R3 pending task crosses the deadline");
    let drop_observed = tokio::time::timeout(Duration::from_secs(1), drop_entered_rx)
        .await
        .is_ok_and(|entered| entered.is_ok());
    assert!(
        drop_observed,
        "R3/E1 task abort eventually drops the Future"
    );
    assert_eq!(result.timed_out, vec!["pending"], "R3/E3");

    // R2/R3: this task never yields while the observer holds it. Its task abort
    // cannot finish it, so only watcher abort permits bounded shutdown return.
    let group = ServiceLifecycle::new();
    let release = Arc::new(AtomicBool::new(false));
    let task_release = release.clone();
    let (busy_started_tx, busy_started_rx) = tokio::sync::oneshot::channel();
    group.spawn("busy", |_| async move {
        let _ = busy_started_tx.send(());
        while !task_release.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        Ok(())
    });
    busy_started_rx.await.expect("R2 busy task started");

    let shutdown_done = Arc::new(AtomicBool::new(false));
    let shutdown_done_task = shutdown_done.clone();
    let shutdown_group = group.clone();
    let shutdown = tokio::spawn(async move {
        let result = shutdown_group.shutdown(Duration::from_millis(10)).await;
        shutdown_done_task.store(true, Ordering::Release);
        result
    });
    let observer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let returned_before_release = shutdown_done.load(Ordering::Acquire);
        release.store(true, Ordering::Release);
        returned_before_release
    });
    let returned_before_release = tokio::task::spawn_blocking(move || observer.join().unwrap())
        .await
        .expect("R2/R3 observer joins");
    let result = tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("R2 completes after the observer releases the task")
        .expect("R2/R3 shutdown task does not panic")
        .expect_err("R3 reports the deadline timeout");
    assert!(returned_before_release, "R3/E2 watcher abort bounds return");
    assert_eq!(result.timed_out, vec!["busy"], "R3/E3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdown_cannot_return_before_the_active_drain_finishes() {
    // C1 the leader owns a non-cooperative task; C2 a follower calls shutdown
    // after cancellation is visible but before the leader's deadline. E1 the
    // follower remains blocked; E2 the leader reports/aborts the stuck task;
    // E3 only then may the follower observe the drained registry and return Ok,
    // preserving the established follower/idempotent API rather than copying the
    // leader-only timeout diagnostic.
    // Decision rules: R1=C1+C2+active deadline -> E1; R2=R1+deadline ->
    // E2+E3. The cooperative-success and multiple-follower row is separate.
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
async fn concurrent_shutdown_followers_wait_for_successful_active_drain() {
    // Causes: C1 the leader has transferred one cooperative task and that task
    // has observed cancellation but is held before returning; C2 two followers
    // independently call shutdown while C1 holds; C3 the task is released.
    // Effects: E1 neither follower returns during the active drain; E2 release
    // lets the leader join the task and return Ok; E3 both followers then acquire
    // the same guard in turn, observe the drained registry, and return Ok.
    // Decision rules: R1=C1+C2+!C3 -> E1; R2=C1+C2+C3 -> E2+E3. Together with
    // the timeout row above this crosses both leader terminal outcomes without
    // introducing a second lifecycle state or test-only drain owner.
    let group = ServiceLifecycle::new();
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    group.spawn("cooperative", move |cancel| async move {
        cancel.cancelled().await;
        let _ = cancelled_tx.send(());
        let _ = release_rx.await;
        Ok(())
    });

    let leader_group = group.clone();
    let leader = tokio::spawn(async move { leader_group.shutdown(Duration::from_secs(1)).await });
    cancelled_rx
        .await
        .expect("R1 leader transferred and cancelled the task set");

    let mut followers = Vec::new();
    for _ in 0..2 {
        let follower_group = group.clone();
        followers.push(tokio::spawn(async move {
            follower_group.shutdown(Duration::from_secs(1)).await
        }));
    }
    for follower in &mut followers {
        assert!(
            tokio::time::timeout(Duration::from_millis(20), follower)
                .await
                .is_err(),
            "R1/E1 every follower remains behind the active drain"
        );
    }

    release_tx.send(()).expect("R2 release cooperative task");
    leader
        .await
        .expect("R2 leader task")
        .expect("R2 cooperative drain succeeds");
    for follower in followers {
        follower
            .await
            .expect("R3 follower task")
            .expect("R3 follower returns after the drained registry");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_shutdown_caller_cannot_orphan_the_active_drain() {
    // Causes: C1 the first caller transfers a cooperative task; C2 that caller's
    // shutdown Future is cancelled while the task is still held; C3 a follower
    // calls shutdown; C4 the task is released. Effects: E1 the unique drain task
    // retains ownership after C2; E2 the follower remains waiting through C3;
    // E3 C4 lets the retained owner join the task and publish completion; E4 the
    // follower then returns. Decision rules: R1=C1+C2 -> E1;
    // R2=R1+C3+!C4 -> E2; R3=R1+C3+C4 -> E3+E4. This is the cancellation row
    // omitted by a caller-owned local task Vec, which detached both handles when
    // the leader Future was dropped.
    let group = ServiceLifecycle::new();
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    group.spawn("retained", move |cancel| async move {
        cancel.cancelled().await;
        let _ = cancelled_tx.send(());
        let _ = release_rx.await;
        Ok(())
    });

    let leader_group = group.clone();
    let leader = tokio::spawn(async move { leader_group.shutdown(Duration::from_secs(1)).await });
    cancelled_rx
        .await
        .expect("R1 leader transferred and cancelled the task set");
    leader.abort();
    assert!(
        leader
            .await
            .expect_err("R1 leader is cancelled")
            .is_cancelled(),
        "R1 caller cancellation is observed"
    );

    let follower_group = group.clone();
    let mut follower =
        tokio::spawn(async move { follower_group.shutdown(Duration::from_secs(1)).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut follower)
            .await
            .is_err(),
        "R2 follower remains behind the retained drain owner"
    );

    release_tx.send(()).expect("R3 release retained task");
    follower
        .await
        .expect("R3 follower task")
        .expect("R4 follower returns after retained drain completion");
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
