// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Test-only regression observations of the original component and its effects.
// @formal reason: This classifies the assertions; it is not a formal-proof claim.
//! An old terminal-cleanup lookup cannot retire a successor realization.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_terminal_not_found_preserves_the_current_real_run_and_realization() {
    missing_terminal_work_preserves_successor(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_terminal_not_found_preserves_a_realization_reassigned_during_lookup() {
    missing_terminal_work_preserves_successor(true).await;
}

async fn missing_terminal_work_preserves_successor(delayed: bool) {
    // C1 recovery polls the original lease; C2 the Session has a successor
    // generation before entry, or acquires it while Control is suspended;
    // C3 a real DirectAttemptDriver Run enters inference under that generation;
    // C4 the old lookup returns NotFound. E1 its token stays live, E2 the same
    // physical resident/root survive, and E3 that real Run commits normally.
    // The fake Control response is an explicit L1 I/O fault seam. The Run,
    // Runtime and SQLite dispatch are real; this is not a backend CAS proof.
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
    let mut dependencies = crate::HostDependencies::ephemeral(model, "scripted")
        .await
        .with_ephemeral_dispatch(dispatch);
    dependencies.execution.ingress = crate::RunIngress::Direct;
    let host = dependencies.build().expect("complete Host composition");
    let _managed = install_test_dispatch_runtime(&host);
    let thread = if delayed {
        "terminal-missing-delayed-successor"
    } else {
        "terminal-missing-current-successor"
    };
    let session = SessionId::new(thread);
    let ctx = host
        .ctx_for(thread, None)
        .await
        .expect("original idle resident");
    let original = crate::session_resource::SessionRealizationLease {
        owner: "local".into(),
        runtime_incarnation: "boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
            + 300_000,
    };
    host.install_session_realization_lease(thread, original.clone())
        .expect("original Control-observed lease");
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    control
        .terminal_work_not_found
        .store(true, Ordering::SeqCst);
    let recovery = if delayed {
        *control.terminal_work_block_for.lock().unwrap() = Some(thread.into());
        let task = tokio::spawn({
            let host = host.clone();
            let control = control.clone();
            let session = session.clone();
            let original = original.clone();
            async move {
                host.reconcile_terminal_cleanup_for_lease(control.as_ref(), &session, &original)
                    .await
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            control.terminal_work_started.notified(),
        )
        .await
        .expect("C1 original lookup is suspended at the real Control call");
        Some(task)
    } else {
        None
    };
    let successor = crate::session_resource::SessionRealizationLease {
        epoch: 2,
        ..original.clone()
    };
    host.install_session_realization_lease(thread, successor.clone())
        .expect("C2 successor Control-observed generation");
    let mut run = tokio::spawn({
        let host = host.clone();
        async move { host.run(None, thread, user("successor")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = provider_entered.notified() => {}
            result = &mut run => match result {
                Ok(Ok(_)) => panic!("successor ended before its inference barrier"),
                Ok(Err(error)) => panic!("successor failed before inference: {error}"),
                Err(error) => panic!("successor task failed: {error}"),
            },
        }
    })
    .await
    .expect("C3 real successor entered inference");
    let token = ctx
        .foreground
        .observe()
        .unwrap()
        .cancellation
        .expect("C3 real attempt token");
    assert!(!token.is_cancelled(), "C3 initially live");
    if let Some(recovery) = recovery {
        control.terminal_work_release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), recovery)
            .await
            .expect("C4 original lookup returned")
            .expect("join recovery")
            .expect("old terminal lookup was handled");
    } else {
        host.reconcile_terminal_cleanup_for_lease(control.as_ref(), &session, &original)
            .await
            .expect("C4 stale terminal lookup was handled");
    }
    let cancelled = token.is_cancelled();
    let retained = host
        .installed_sessions
        .read(thread, |slot| {
            slot.realization.execution.authority.lease() == Some(&successor)
                && slot
                    .run
                    .context
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &ctx))
        })
        .unwrap_or(false);
    // Always release and join the actual driver before asserting the old fault.
    provider_gate.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("real successor returned")
        .expect("join successor");
    assert!(
        !cancelled,
        "old terminal NotFound cancelled the real successor Run"
    );
    assert!(
        retained,
        "old terminal NotFound retired the successor realization"
    );
    outcome.expect("E3 real successor committed its outcome");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_terminal_not_found_cancels_and_retires_its_original_real_run() {
    missing_current_terminal_work_retires_original(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewed_original_owner_is_still_retired_after_terminal_not_found() {
    missing_current_terminal_work_retires_original(true).await;
}

async fn missing_current_terminal_work_retires_original(renew: bool) {
    // Positive decision rows: exact original owner, with/without a same-owner
    // monotonic renewal during I/O, must still stop its admitted real Run.
    // Rejecting a successor must not turn terminal absence into a blanket no-op.
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
    let mut dependencies = crate::HostDependencies::ephemeral(model, "scripted")
        .await
        .with_ephemeral_dispatch(dispatch);
    dependencies.execution.ingress = crate::RunIngress::Direct;
    let host = dependencies.build().expect("complete Host composition");
    let _managed = install_test_dispatch_runtime(&host);
    let thread = if renew {
        "terminal-original-renewed"
    } else {
        "terminal-original-current"
    };
    let ctx = host.ctx_for(thread, None).await.expect("original resident");
    let original = crate::session_resource::SessionRealizationLease {
        owner: "local".into(),
        runtime_incarnation: "boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
            + 300_000,
    };
    host.install_session_realization_lease(thread, original.clone())
        .unwrap();
    let mut run = tokio::spawn({
        let host = host.clone();
        async move { host.run(None, thread, user("original")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = provider_entered.notified() => {}
            result = &mut run => match result {
                Ok(Ok(_)) => panic!("original ended before its inference barrier"),
                Ok(Err(error)) => panic!("original failed before inference: {error}"),
                Err(error) => panic!("original task failed: {error}"),
            },
        }
    })
    .await
    .expect("original real Run entered inference");
    let token = ctx
        .foreground
        .observe()
        .unwrap()
        .cancellation
        .expect("actual original token");
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    control
        .terminal_work_not_found
        .store(true, Ordering::SeqCst);
    *control.terminal_work_block_for.lock().unwrap() = Some(thread.into());
    let recovery = tokio::spawn({
        let host = host.clone();
        let control = control.clone();
        let original = original.clone();
        async move {
            host.reconcile_terminal_cleanup_for_lease(
                control.as_ref(),
                &SessionId::new(thread),
                &original,
            )
            .await
        }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        control.terminal_work_started.notified(),
    )
    .await
    .expect("original lookup reached Control");
    if renew {
        host.install_session_realization_lease(
            thread,
            crate::session_resource::SessionRealizationLease {
                expires_at_unix_ms: original.expires_at_unix_ms + 5_000,
                ..original
            },
        )
        .expect("same-generation monotonic renewal");
    }
    control.terminal_work_release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), recovery)
        .await
        .expect("original cleanup returned")
        .expect("join original cleanup")
        .expect("original cleanup succeeded");
    let cancelled = token.is_cancelled();
    let closed_and_detached = host
        .installed_sessions
        .read(thread, |slot| {
            slot.realization.execution.authority.is_closed() && slot.run.context.is_none()
        })
        .unwrap_or(true);
    provider_gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("original driver returned")
        .expect("join original driver")
        .expect("original driver committed cancellation");
    assert!(
        cancelled,
        "terminal absence must still cancel the selected original Run"
    );
    assert!(
        closed_and_detached,
        "terminal absence closes original admission and detaches its Runtime"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_terminal_projection_not_found_cancels_its_original_real_run() {
    // Row T1 of the terminal-absence table in application.rs: C1 a real
    // DirectAttemptDriver Run is in inference under lease L; C2 this process
    // holds L's completed terminal-cleanup projection; C3 Control reports the
    // cleanup work NotFound. E1 the projection is retired (the Session slot is
    // gone) and E2 the Run controls captured before the lookup are signalled,
    // so no Run keeps executing on a retired terminal Session.
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
    let mut dependencies = crate::HostDependencies::ephemeral(model, "scripted")
        .await
        .with_ephemeral_dispatch(dispatch);
    dependencies.execution.ingress = crate::RunIngress::Direct;
    let host = dependencies.build().expect("complete Host composition");
    let _managed = install_test_dispatch_runtime(&host);
    let thread = "terminal-completed-projection";
    let ctx = host.ctx_for(thread, None).await.expect("original resident");
    let original = crate::session_resource::SessionRealizationLease {
        owner: "local".into(),
        runtime_incarnation: "boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
            + 300_000,
    };
    host.install_session_realization_lease(thread, original.clone())
        .unwrap();
    let mut run = tokio::spawn({
        let host = host.clone();
        async move { host.run(None, thread, user("original")).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = provider_entered.notified() => {}
            result = &mut run => match result {
                Ok(Ok(_)) => panic!("original ended before its inference barrier"),
                Ok(Err(error)) => panic!("original failed before inference: {error}"),
                Err(error) => panic!("original task failed: {error}"),
            },
        }
    })
    .await
    .expect("C1 original real Run entered inference");
    let token = ctx
        .foreground
        .observe()
        .unwrap()
        .cancellation
        .expect("C1 actual original token");
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| {
            r.cleanup.assignment = Some(
                awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                    environment_creation_not_attempted: false,
                    session_id: thread.into(),
                    projection: remote_terminal_cleanup_projection(),
                    authority: awaken_session_realization::test_support::test_lease_receipt(
                        original.clone(),
                    ),
                },
            );
        });
    let control = RemoteTerminalCleanupControl::default();
    control
        .terminal_work_not_found
        .store(true, Ordering::SeqCst);
    let handled = host
        .reconcile_terminal_cleanup_for_lease(&control, &SessionId::new(thread), &original)
        .await
        .expect("C3 terminal absence was handled");
    let cancelled = token.is_cancelled();
    let retired = !host.installed_sessions.contains(thread);
    provider_gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("original driver returned")
        .expect("join original driver")
        .expect("original driver committed its cancellation");
    assert!(handled, "terminal absence is owned by the terminal fence");
    assert!(retired, "E1 the completed terminal projection is retired");
    assert!(
        cancelled,
        "E2 the original Run captured before lookup is signalled"
    );
}

#[test]
fn terminal_absence_selection_requires_the_complete_generation_and_original_phase() {
    // Complete local decision table. These are ten aggregate-state rows in one
    // test, not ten backend/runtime qualifications. No external effect is inferred.
    for rule in [
        "exact",
        "renewed",
        "future-expiry",
        "owner",
        "incarnation",
        "older-epoch",
        "newer-epoch",
        "phase",
        "closed-retry",
        "absent",
    ] {
        let slots = Arc::new(crate::installed_session::InstalledSessions::default());
        let current = crate::session_resource::SessionRealizationLease {
            owner: "local".into(),
            runtime_incarnation: "boot".into(),
            epoch: 2,
            expires_at_unix_ms: awaken_session_realization::test_support::runtime_unix_now_ms()
                + 300_000,
        };
        let mut expected = current.clone();
        let actual_phase = if rule == "absent" {
            Arc::new(tokio::sync::Mutex::new(()))
        } else {
            slots.update(rule, |slot| {
                assert!(slot.realization.install_lease(
                    awaken_session_realization::test_support::test_observed_lease(current.clone()),
                ));
                if rule == "closed-retry" {
                    assert!(slot.realization.close_authority(Some(&current)));
                }
                slot.realization.execution.phase.clone()
            })
        };
        let phase = if rule == "phase" {
            Arc::new(tokio::sync::Mutex::new(()))
        } else {
            actual_phase
        };
        match rule {
            "renewed" => expected.expires_at_unix_ms -= 1,
            "future-expiry" => expected.expires_at_unix_ms += 1,
            "owner" => expected.owner = "foreign".into(),
            "incarnation" => expected.runtime_incarnation = "other-boot".into(),
            "older-epoch" => expected.epoch -= 1,
            "newer-epoch" => {
                expected.epoch = awaken_lease_contract::fence::Epoch::try_from(expected.epoch)
                    .expect("positive fixture epoch")
                    .next()
                    .expect("fixture epoch has a successor")
                    .get();
            }
            _ => {}
        }
        let before = slots.read(rule, |slot| {
            crate::session_realizations::SessionRealizationRevocationSelection::from_realization(
                &slot.realization,
            )
        });
        let selected = slots
            .close_realization_generation(rule, &expected, &phase)
            .map(|closed| closed.root);
        let should_close = matches!(rule, "exact" | "renewed" | "closed-retry");
        assert_eq!(selected.is_some(), should_close, "decision row {rule}");
        if let Some(selected) = selected {
            assert!(
                slots
                    .read(rule, |slot| {
                        selected.matches(&slot.realization)
                            && slot.realization.execution.authority.is_closed()
                    })
                    .unwrap(),
                "{rule}: close and selection are one canonical mutation"
            );
        } else if let Some(before) = before {
            assert!(
                slots
                    .read(rule, |slot| before.matches(&slot.realization))
                    .unwrap(),
                "{rule}: rejected input preserves the exact root"
            );
        } else {
            assert!(
                slots.session_ids().is_empty(),
                "absence never creates a replacement root"
            );
        }
    }
}
