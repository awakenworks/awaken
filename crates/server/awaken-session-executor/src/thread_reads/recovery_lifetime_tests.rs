//! Retained recovery is read-only and cannot become an owner of Session state.
use super::*;
use awaken_agent_contract::agent::run::Id as RunId;

#[tokio::test]
async fn retained_worker_recovery_releases_original_slots_and_idle_context() {
    // L1 installed idle Context + retained Worker reader + live composition =>
    // exact original cached adapter. L2 drop the composition and Context => the
    // actual slot table, Context and realization executor are released while
    // the reader lives. L3 post-release recovery => same configured committed
    // prefix, without recreating slots or realizing an Environment.
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("real executor composition");
    let session = "retained-recovery-owner";
    let context = host
        .run_assembly
        .ctx_for_session_coordination(session, None)
        .await
        .expect("real cached idle Context");
    assert!(context.env.is_none() && !context.has_active_run());
    let queries = host.thread_reads.clone();
    assert!(
        queries
            .coordinated_threads(session, None)
            .await
            .unwrap()
            .is_empty()
    );
    let recovery = host.worker_recovery_source();
    let thread = ThreadId(session.into());
    let run = RunId("unstarted-recovery-run".into());
    let expected = recovery
        .recovery_snapshot(&thread, &run)
        .await
        .expect("original committed prefix");
    let slots = Arc::downgrade(&host.run_assembly.installed_sessions);
    let realization = Arc::downgrade(&host.realization_executor);
    let cached = Arc::downgrade(&context);
    drop(context);
    drop(host);
    assert!(
        slots.upgrade().is_none(),
        "retained reader must release original slots"
    );
    assert!(
        cached.upgrade().is_none(),
        "idle cached Context must be released"
    );
    assert!(
        realization.upgrade().is_none(),
        "no retained realization machinery"
    );
    let actual = recovery
        .recovery_snapshot(&thread, &run)
        .await
        .expect("same selected commit source after owner release");
    assert_eq!(actual, expected);
    assert!(
        queries
            .coordinated_threads(session, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        slots.upgrade().is_none(),
        "readback must not resurrect Session state"
    );
}

#[tokio::test]
async fn retained_recovery_waits_on_original_lifecycle_and_selects_exact_cached_commit() {
    // C1 original lifecycle held => the canonical reader waits; C2 released =>
    // it returns the exact cached Arc, including Worker-projected adapters,
    // rather than constructing a second read path or partition.
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let session = "retained-recovery-lock";
    let context = host
        .run_assembly
        .ctx_for_session_coordination(session, None)
        .await
        .unwrap();
    let original = context.commit.clone();
    let guard = host
        .run_assembly
        .installed_sessions
        .realizations()
        .lock_lifecycle(session)
        .await
        .unwrap();
    let mut reading = Box::pin(host.thread_reads.commit_for_read(session));
    tokio::select! {
        biased;
        _ = &mut reading => panic!("read bypassed the original lifecycle lock"),
        _ = tokio::task::yield_now() => {}
    }
    drop(guard);
    let selected = reading.await.unwrap();
    assert!(
        Arc::ptr_eq(&selected, &original),
        "one canonical cached commit adapter"
    );
}

#[tokio::test]
async fn retained_recovery_preserves_poison_and_original_source_refusal() {
    // P1 original Worker source refuses unbound committed recovery. P2 poison
    // the real installed registry => the typed poison refusal, not absence or
    // fallback. P3 release its owner => the original Worker-source refusal is
    // preserved exactly; the retained read role cannot create a local prefix.
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let slots = host.run_assembly.installed_sessions.clone();
    slots.install("poisoned-recovery").unwrap();
    let reader = ThreadReads::new(
        Arc::new(crate::host::tests::fixtures::test_upstream_authority()),
        slots.clone(),
        host.realization_executor.clone(),
    );
    let thread = ThreadId("poisoned-recovery".into());
    let run = RunId("unbound-original-run".into());
    let refused = reader
        .recovery_snapshot(&thread, &run)
        .await
        .expect_err("original upstream commit has no claimed projection")
        .to_string();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slots.modify("poisoned-recovery", |_| -> () {
                panic!("poison original registry")
            })
        }))
        .is_err()
    );
    let error = reader
        .commit_for_read("poisoned-recovery")
        .await
        .err()
        .expect("typed refusal");
    assert_eq!(
        error.code,
        awaken_session_realization::realization_error::POISONED_CODE
    );
    let weak_slots = Arc::downgrade(&slots);
    drop(slots);
    drop(host);
    assert!(weak_slots.upgrade().is_none());
    let after = reader
        .recovery_snapshot(&thread, &run)
        .await
        .expect_err("original upstream refusal survives owner release")
        .to_string();
    assert_eq!(after, refused);
    assert!(weak_slots.upgrade().is_none());
}
