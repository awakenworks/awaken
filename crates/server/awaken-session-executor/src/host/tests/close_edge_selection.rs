// @formal surface requirement=runtime.cancellation.terminal_cancellation
// @formal reason: Test-only original Root retirement and real DirectAttemptDriver observations.
// @formal reason: A synchronous scheduling seam exposes a possible cross-thread gap; no cancellation token is manufactured.
//! A local retirement selection cannot interrupt a rebuilt Session allocation.

use super::*;

struct ReleaseRetirementCheck(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

impl ReleaseRetirementCheck {
    fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

impl Drop for ReleaseRetirementCheck {
    fn drop(&mut self) {
        self.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn closed_realization_retirement_cannot_interrupt_a_session_rebuilt_after_its_match_check() {
    rebuilt_successor_is_not_interrupted(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn closed_realization_retirement_keeps_the_original_token_after_its_run_completes() {
    rebuilt_successor_is_not_interrupted(true).await;
}

async fn rebuilt_successor_is_not_interrupted(original_run_active: bool) {
    // S1 close and check original allocation A; S2 pause at that synchronous
    // check boundary, modeling another OS thread's preemption; S3 remove A
    // through SessionRealizations and reconstruct B through the real Host;
    // S4 B starts a real SQLite-backed DirectAttemptDriver Run; S5 resume A.
    // A's completion CAS already protects B's root. Its cancellation must use
    // the same original selection, rather than selecting B again by Session ID.
    let entered_model = Arc::new(tokio::sync::Notify::new());
    let release_model = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: release_model.clone(),
        reached: entered_model.clone(),
        calls: AtomicUsize::new(2),
    });
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let mut dependencies = crate::HostDependencies::ephemeral(model.clone(), "scripted")
        .await
        .with_ephemeral_dispatch(dispatch);
    dependencies.execution.ingress = crate::RunIngress::Direct;
    let host = dependencies.build().unwrap();
    let _managed = install_test_dispatch_runtime(&host);
    let thread = if original_run_active {
        "retirement-completed-original-rebuilt-successor"
    } else {
        "retirement-post-check-rebuilt-successor"
    };
    let original_ctx = host.ctx_for(thread, None).await.unwrap();
    let original = crate::session_resource::SessionRealizationLease {
        owner: "local".into(),
        runtime_incarnation: "boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 300_000,
    };
    host.install_session_realization_lease(thread, original.clone())
        .unwrap();
    let phase = host
        .installed_sessions
        .read(thread, |slot| slot.realization.execution().phase.clone())
        .unwrap();
    let first_run = if original_run_active {
        let mut run = tokio::spawn({
            let host = host.clone();
            async move { host.run(None, thread, user("original")).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                () = entered_model.notified() => {},
                result = &mut run => match result {
                    Ok(Ok(_)) => panic!("original ended before inference"),
                    Ok(Err(error)) => panic!("original failed before inference: {error}"),
                    Err(error) => panic!("original task failed before inference: {error}"),
                },
            }
        })
        .await
        .expect("original actual Run entered inference");
        let token = original_ctx
            .foreground
            .observe()
            .unwrap()
            .cancellation
            .unwrap();
        assert!(!token.is_cancelled());
        Some((run, token))
    } else {
        None
    };
    let selected = host
        .installed_sessions
        .close_realization(thread, Some((&original, &phase)))
        .expect("close and select actual original root");
    let checked = Arc::new(tokio::sync::Notify::new());
    let release = ReleaseRetirementCheck(Arc::new((
        std::sync::Mutex::new(false),
        std::sync::Condvar::new(),
    )));
    let retirement = tokio::spawn({
        let host = host.clone();
        let checked = checked.clone();
        let release = release.0.clone();
        async move {
            host.retire_selected_session_realization_with_post_check(
                thread,
                selected,
                true,
                move || {
                    checked.notify_one();
                    let (mutex, changed) = &*release;
                    let mut released = mutex.lock().unwrap();
                    while !*released {
                        released = changed.wait(released).unwrap();
                    }
                },
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), checked.notified())
        .await
        .expect("S2 actual original root matched before the scheduling seam");
    let original_token = if let Some((run, token)) = first_run {
        release_model.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("original Run returned")
            .unwrap()
            .unwrap();
        assert!(
            !token.is_cancelled(),
            "original completed before retirement signals it"
        );
        model.calls.store(2, Ordering::SeqCst);
        Some(token)
    } else {
        None
    };
    assert!(
        host.realization_executor
            .sessions()
            .remove_session(thread)
            .expect("installed sessions readable")
    );
    // Release the original real Workdir through its lifecycle before the
    // next actual incarnation creates its own root; never adopt an unowned
    // legacy directory or weaken the provider's fail-closed admission.
    original_ctx
        .env
        .as_ref()
        .expect("original Workdir")
        .lifecycle()
        .dispose()
        .await
        .expect("dispose only the original fixture");
    let successor_ctx = host.ctx_for(thread, None).await.unwrap();
    assert!(!Arc::ptr_eq(&original_ctx, &successor_ctx));
    let successor = crate::session_resource::SessionRealizationLease {
        epoch: 2,
        ..original.clone()
    };
    host.install_session_realization_lease(thread, successor.clone())
        .unwrap();
    let mut run = tokio::spawn({
        let host = host.clone();
        async move { host.run(None, thread, user("successor")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = entered_model.notified() => {},
            result = &mut run => match result {
                Ok(Ok(_)) => panic!("S4 real successor ended before inference"),
                Ok(Err(error)) => panic!("S4 real successor failed: {error}"),
                Err(error) => panic!("S4 real successor task failed: {error}"),
            },
        }
    })
    .await
    .expect("S4 real successor entered inference");
    let token = successor_ctx
        .foreground
        .observe()
        .unwrap()
        .cancellation
        .unwrap();
    assert!(!token.is_cancelled());
    release.release();
    let retired = tokio::time::timeout(std::time::Duration::from_secs(5), retirement)
        .await
        .expect("S5 original retirement completed")
        .unwrap()
        .unwrap();
    let cancelled = token.is_cancelled();
    let retained = host
        .installed_sessions
        .read(thread, |slot| {
            slot.realization.execution().authority.lease() == Some(&successor)
                && slot
                    .run
                    .context
                    .as_ref()
                    .is_some_and(|ctx| Arc::ptr_eq(ctx, &successor_ctx))
        })
        .unwrap_or(false);
    release_model.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("real successor completed")
        .unwrap()
        .unwrap();
    assert!(
        !retired && retained,
        "original completion CAS preserves rebuilt root"
    );
    assert!(
        !cancelled,
        "old realization retirement interrupted the real successor reconstructed after its root match check"
    );
    if let Some(original) = original_token {
        assert!(
            original.is_cancelled(),
            "retirement still signals its selected original token"
        );
    }
}
