// @formal surface requirement=runtime.cancellation.terminal_cancellation
// @formal reason: Test-only regression observations of the original component and its effects.
// @formal reason: This classifies the assertions; it is not a formal-proof claim.
//! A Session control request cannot select a Run admitted during its read.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_interruption_cannot_cancel_a_run_admitted_during_its_commit_read() {
    for fail_read in [false, true] {
        delayed_interruption_preserves_the_successor(false, fail_read).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_interruption_cannot_cancel_the_successor_of_a_completed_run() {
    for fail_read in [false, true] {
        delayed_interruption_preserves_the_successor(true, fail_read).await;
    }
}

async fn delayed_interruption_preserves_the_successor(
    start_with_active_run: bool,
    fail_read: bool,
) {
    // C1 the resident Session is idle when interruption selects local control;
    // C2 its real commit-read seam waits; C3 a real later Run enters inference
    // in the same resident Runtime; C4 C2 resumes. E1 C4 must not cancel C3.
    // No claim/token is manufactured: Host admission and DirectAttemptDriver
    // install the later Run and its token through their production path.
    // C5 the original read fails. Decision table: R1 idle + successful read ->
    // no cancellation; R2 selected original + successful read -> only original
    // token cancelled; R3/R4 either selection + C5 -> propagate the error and
    // signal neither token. All four rows preserve the real successor.
    // Each row owns a distinct sandbox identity: a new authority cannot adopt
    // the previous row's retained filesystem through an unfenced name reuse.
    let provider_gate = Arc::new(tokio::sync::Notify::new());
    let provider_entered = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: provider_gate.clone(),
        reached: provider_entered.clone(),
        calls: AtomicUsize::new(2),
    });
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(awaken_time::system_clock())
            .await
            .expect("real SQLite dispatch authority"),
    );
    let mut dependencies = crate::SessionExecutorBuilder::ephemeral(model.clone(), "scripted")
        .await
        .with_ephemeral_dispatch(dispatch);
    dependencies.execution.ingress = crate::RunIngress::Direct;
    let host = dependencies.build().expect("complete Host composition");
    let _managed = host.clone();
    let thread = match (start_with_active_run, fail_read) {
        (false, false) => "idle-interrupt-later-run",
        (false, true) => "idle-interrupt-failed-read-later-run",
        (true, false) => "completed-interrupt-later-run",
        (true, true) => "completed-interrupt-failed-read-later-run",
    };
    let ctx = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .expect("idle resident");
    assert!(!ctx.foreground.has_run().unwrap(), "C1 idle Run");
    assert!(
        ctx.foreground.observe().unwrap().cancellation.is_none(),
        "C1 no attempt token"
    );

    let original_run = if start_with_active_run {
        let mut run = tokio::spawn({
            let host = host.clone();
            async move { host.session_runs.run(None, thread, user("original")).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                () = provider_entered.notified() => {}
                result = &mut run => match result {
                    Ok(Ok(_)) => panic!("original Run ended before its inference gate"),
                    Ok(Err(error)) => panic!("original Run failed: {error}"),
                    Err(error) => panic!("original Run task failed: {error}"),
                },
            }
        })
        .await
        .expect("original Run entered real inference");
        let token = ctx
            .foreground
            .observe()
            .unwrap()
            .cancellation
            .expect("original real attempt token");
        Some((run, token))
    } else {
        None
    };

    let read_entered = Arc::new(tokio::sync::Notify::new());
    let release_read = Arc::new(tokio::sync::Notify::new());
    let interrupt = tokio::spawn({
        let host = host.clone();
        let commit = ctx.commit.clone();
        let read_entered = read_entered.clone();
        let release_read = release_read.clone();
        async move {
            host.session_runs
                .interrupt_with_commit_read(thread, async move {
                    read_entered.notify_one();
                    release_read.notified().await;
                    if fail_read {
                        Err(SessionExecutionError::unavailable(
                            "injected original commit read failure",
                        ))
                    } else {
                        Ok(commit)
                    }
                })
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), read_entered.notified())
        .await
        .expect("C2 original interruption entered its commit-read seam");

    let selected_original_token = if let Some((run, token)) = original_run {
        provider_gate.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("original Run returned")
            .expect("join original Run")
            .expect("original Run committed its real outcome");
        assert!(
            !token.is_cancelled(),
            "original Run completed before cancellation delivery"
        );
        assert!(
            !ctx.foreground.has_run().unwrap(),
            "original Run is no longer active"
        );
        model.calls.store(2, Ordering::SeqCst);
        Some(token)
    } else {
        None
    };

    let mut later = tokio::spawn({
        let host = host.clone();
        async move { host.session_runs.run(None, thread, user("go")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = provider_entered.notified() => {}
            result = &mut later => match result {
                Ok(Ok(_)) => panic!("C3 later Run ended before inference"),
                Ok(Err(error)) => panic!("C3 later Run failed before inference: {error}"),
                Err(error) => panic!("C3 later Run task failed: {error}"),
            },
        }
    })
    .await
    .expect("C3 later Run physically entered inference");
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run
                .context
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &ctx)))
            .expect("installed Sessions")
            .unwrap(),
        "C3 same original resident Runtime, not a replacement fake context"
    );
    let actual_later_token = ctx
        .foreground
        .observe()
        .unwrap()
        .cancellation
        .expect("C3 actual attempt token");
    assert!(!actual_later_token.is_cancelled(), "C3 initially live");
    release_read.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), interrupt)
        .await
        .expect("C4 interruption returned")
        .expect("join interruption");
    if fail_read {
        result.expect_err("R3/R4 propagate the original read failure");
    } else {
        result.expect("R1/R2 interruption succeeded");
    }
    let cancelled_later = actual_later_token.is_cancelled();
    // Join the real Run on either implementation before checking the fault.
    provider_gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), later)
        .await
        .expect("later Run returned")
        .expect("join later Run")
        .expect("later Run committed its real outcome");
    assert!(
        !cancelled_later,
        "idle interruption cancelled the real successor admitted during its commit read"
    );
    if let Some(original) = selected_original_token {
        assert_eq!(
            original.is_cancelled(),
            !fail_read,
            "R2 signals the selected original only after a successful read; R4 does not"
        );
    }
}
