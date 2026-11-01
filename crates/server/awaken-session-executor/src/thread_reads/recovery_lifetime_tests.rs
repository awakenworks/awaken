//! Retained recovery is read-only and cannot become an owner of Session state.
use super::*;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;

#[tokio::test]
async fn retained_worker_recovery_releases_original_installed_sessions_and_idle_context() {
    // L1 installed idle Context + retained Worker reader + live composition =>
    // exact original cached adapter. L2 drop the composition and Context => the
    // actual installed Session table, Context and realization executor are released while
    // the reader lives. L3 post-release recovery => same configured committed
    // prefix, without recreating installed Sessions or realizing an Environment.
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
        .ctx_for_session_coordination(session, None, None)
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
    let installed_sessions = Arc::downgrade(&host.run_assembly.installed_sessions);
    let realization = Arc::downgrade(&host.realization_executor);
    let cached = Arc::downgrade(&context);
    drop(context);
    drop(host);
    assert!(
        installed_sessions.upgrade().is_none(),
        "retained reader must release original installed Sessions"
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
        installed_sessions.upgrade().is_none(),
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
        .ctx_for_session_coordination(session, None, None)
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
    let installed_sessions = host.run_assembly.installed_sessions.clone();
    installed_sessions.install("poisoned-recovery").unwrap();
    let reader = CommittedReads::new(
        Arc::new(InstalledCommitSource::new(
            Arc::new(crate::host::tests::fixtures::test_upstream_authority()),
            &installed_sessions,
        )),
        Arc::new(ResolvedAdvisorModels::new(
            &installed_sessions,
            &host.realization_executor,
        )),
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
            installed_sessions.modify("poisoned-recovery", |_| -> () {
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
    let weak_installed_sessions = Arc::downgrade(&installed_sessions);
    drop(installed_sessions);
    drop(host);
    assert!(weak_installed_sessions.upgrade().is_none());
    let after = reader
        .recovery_snapshot(&thread, &run)
        .await
        .expect_err("original upstream refusal survives owner release")
        .to_string();
    assert_eq!(after, refused);
    assert!(weak_installed_sessions.upgrade().is_none());
}
