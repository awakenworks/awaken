use super::*;
use crate::provider::workdir_helper_tests::TeardownProbe;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn admitted<'a>(
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    effect: &pc::SandboxEffectFence,
    phase: &str,
    authority: &'a dyn pc::CurrentExecutionAuthority,
) -> ProviderCreationGuard<'a> {
    if phase == "legacy" {
        return ProviderCreationGuard::Legacy(begin_legacy(root).unwrap());
    }
    let mut guard = begin(root, fingerprint, effect, None, None).unwrap();
    if phase == "ready" && guard.completed_receipt().unwrap().is_none() {
        guard
            .recorded_admission(|held, admitted| held.complete(&empty_receipt(), admitted))
            .unwrap();
        guard = begin(root, fingerprint, effect, None, None).unwrap();
    }
    ProviderCreationGuard::Current {
        guard: Box::new(guard),
        authority,
    }
}

#[tokio::test]
async fn compensation_preserves_phase_and_all_failure_evidence() {
    // Cause/effect graph: retained Memory release + incomplete-only Secret
    // shredding -> exact-root abort iff BOTH succeeded. C1 Creating/Ready/legacy;
    // C2 teardown succeeds/fails once; C3 Secret path is exact/outside the root.
    // Full 3x2x2 table. R1 either failure retains the exact root/marker and cause;
    // Memory failure retains its original guard, even when shredding succeeds.
    // R2 successful incomplete compensation zeros the Secret then removes its
    // root. R3 Ready ignores even an invalid shred path and never removes or
    // rewrites completed bytes. R4 current-marker retries reuse exact evidence;
    // legacy has no durable retry authority and rejects an occupied root.
    // Recreating's distinct source-marker rollback is exercised below through
    // the same marker owner, not a second rebuild state machine.
    for phase in ["creating", "ready", "legacy"] {
        for failures in [0, 1] {
            for foreign_path in [false, true] {
                let base = tempfile::tempdir().unwrap();
                let root = base.path().join("root");
                let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("root"));
                let effect = fence("create", 1);
                let authority = pc::test_support::EffectAuthority::new(&effect);
                let mut guard = admitted(&root, &fingerprint, &effect, phase, &authority);
                guard.prepare_root().unwrap();
                let secret = root.join("credential");
                let foreign = base.path().join("foreign");
                std::fs::write(&secret, b"synthetic credential").unwrap();
                std::fs::write(&foreign, b"foreign bytes").unwrap();
                let identity = awaken_sandbox_fs::directory_identity_nofollow(&root).unwrap();
                let marker = read_marker(&root).unwrap();
                let teardowns = Arc::new(AtomicUsize::new(0));
                let drops = Arc::new(AtomicUsize::new(0));
                let mut memory = crate::MemoryMountState::from(vec![Box::new(TeardownProbe {
                    teardowns: teardowns.clone(),
                    drops: drops.clone(),
                    failures,
                    wait_until_ms: None,
                    response_gate: None,
                })
                    as Box<dyn pc::MemoryMount>]);
                let isolated = crate::IsolatedRoot::new(root.clone());
                let error = crate::compensate_creation_failure(
                    guard,
                    &mut memory,
                    &isolated,
                    &[if foreign_path {
                        foreign.clone()
                    } else {
                        secret.clone()
                    }],
                    "injected creation phase",
                    pc::SandboxError::backend(
                        "compensation_preserves_phase_and_all_failure_evidence",
                        "original failure",
                    ),
                )
                .await;
                let retained = phase == "ready" || failures != 0 || foreign_path;
                assert!(
                    error.to_string().contains("original failure"),
                    "R1 cause retained"
                );
                assert_eq!(
                    root.exists(),
                    retained,
                    "R1/R2/R3 {phase}/{failures}/{foreign_path}"
                );
                assert_eq!(
                    std::fs::read(&foreign).unwrap(),
                    b"foreign bytes",
                    "R1 no foreign write"
                );
                assert_eq!(teardowns.load(Ordering::SeqCst), 1);
                assert_eq!(drops.load(Ordering::SeqCst), usize::from(failures == 0));
                assert_eq!(
                    memory.require_empty().is_ok(),
                    failures == 0,
                    "R1 exact Memory owner"
                );
                if retained {
                    assert_eq!(
                        awaken_sandbox_fs::directory_identity_nofollow(&root).unwrap(),
                        identity
                    );
                    assert_eq!(
                        read_marker(&root).unwrap(),
                        marker,
                        "R1/R3 no marker rewrite"
                    );
                    let expected = if phase == "ready" || foreign_path {
                        b"synthetic credential".to_vec()
                    } else {
                        vec![0; b"synthetic credential".len()]
                    };
                    assert_eq!(
                        std::fs::read(&secret).unwrap(),
                        expected,
                        "R2/R3 shred selection"
                    );
                    if phase == "legacy" {
                        assert!(
                            begin_legacy(&root).is_err(),
                            "R4 no invented legacy recovery"
                        );
                    } else {
                        let guard = admitted(&root, &fingerprint, &effect, phase, &authority);
                        let error = crate::compensate_creation_failure(
                            guard,
                            &mut memory,
                            &isolated,
                            std::slice::from_ref(&secret),
                            "exact retry",
                            pc::SandboxError::backend(
                                "compensation_preserves_phase_and_all_failure_evidence",
                                "retry cause",
                            ),
                        )
                        .await;
                        assert_eq!(
                            error.to_string(),
                            pc::SandboxError::backend(
                                "compensation_preserves_phase_and_all_failure_evidence",
                                "retry cause"
                            )
                            .to_string(),
                            "R4 no repeated cleanup failure"
                        );
                        assert_eq!(root.exists(), phase == "ready", "R3/R4");
                        assert!(memory.require_empty().is_ok(), "R4 original guard released");
                        assert_eq!(drops.load(Ordering::SeqCst), 1, "R4 one guard retired");
                    }
                } else {
                    assert!(read_marker(&root).unwrap().is_none(), "R2 marker removed");
                }
            }
        }
    }
}

#[tokio::test]
async fn recreating_compensation_restores_exact_source_only_after_memory_cleanup() {
    // C1 Ready source physically lost; C2 exact authorized Recreating attempt;
    // C3 teardown succeeds/fails once. R1 failure retains the NEW attempt and its
    // original guard; R2 success (including retry) removes only the new root and
    // restores the complete typed SOURCE marker without changing its fields.
    // It must not remove all evidence like Creating, nor preserve the failed
    // attempt as though it were Ready. Missing source bytes are not fabricated.
    for failures in [0, 1] {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("root"));
        let mut original = begin(&root, &fingerprint, &fence("source", 1), None, None).unwrap();
        let evidence = original
            .recorded_admission(|held, admitted| held.complete(&empty_receipt(), admitted))
            .unwrap();
        let source_marker = read_marker(&root).unwrap().unwrap();
        std::fs::remove_dir(&root).unwrap(); // Fixture: exact empty source was lost.
        let effect = fence("rebuild", 2);
        let authority = pc::test_support::EffectAuthority::new(&effect);
        let guard = ProviderCreationGuard::Current {
            guard: Box::new(
                begin(
                    &root,
                    &fingerprint,
                    &effect,
                    Some(source(&fingerprint, &evidence)),
                    None,
                )
                .unwrap(),
            ),
            authority: &authority,
        };
        let rebuilding = read_marker(&root).unwrap();
        let teardowns = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut memory = crate::MemoryMountState::from(vec![Box::new(TeardownProbe {
            teardowns: teardowns.clone(),
            drops: drops.clone(),
            failures,
            wait_until_ms: None,
            response_gate: None,
        })
            as Box<dyn pc::MemoryMount>]);
        let isolated = crate::IsolatedRoot::new(root.clone());
        let error = crate::compensate_creation_failure(
            guard,
            &mut memory,
            &isolated,
            &[],
            "rebuild",
            pc::SandboxError::backend(
                "recreating_compensation_restores_exact_source_only_after_memory_cleanup",
                "original failure",
            ),
        )
        .await;
        assert!(error.to_string().contains("original failure"));
        if failures != 0 {
            assert!(root.exists(), "R1 failed attempt retained");
            assert_eq!(
                read_marker(&root).unwrap(),
                rebuilding,
                "R1 no early source restore"
            );
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "R1 original Memory guard retained"
            );
            let guard = ProviderCreationGuard::Current {
                guard: Box::new(
                    begin(
                        &root,
                        &fingerprint,
                        &effect,
                        Some(source(&fingerprint, &evidence)),
                        None,
                    )
                    .unwrap(),
                ),
                authority: &authority,
            };
            let error = crate::compensate_creation_failure(
                guard,
                &mut memory,
                &isolated,
                &[],
                "rebuild retry",
                pc::SandboxError::backend(
                    "recreating_compensation_restores_exact_source_only_after_memory_cleanup",
                    "retry cause",
                ),
            )
            .await;
            assert_eq!(
                error.to_string(),
                pc::SandboxError::backend(
                    "recreating_compensation_restores_exact_source_only_after_memory_cleanup",
                    "retry cause"
                )
                .to_string()
            );
        }
        assert!(!root.exists(), "R2 remove only failed rebuilt bytes");
        assert_eq!(
            read_marker(&root).unwrap(),
            Some(source_marker),
            "R2 exact source restored"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1, "R2 one guard retired");
        assert_eq!(
            teardowns.load(Ordering::SeqCst),
            failures + 1,
            "R2 exact retry count"
        );
        assert!(memory.require_empty().is_ok(), "R2 owner released");
    }
}

#[tokio::test]
async fn compensation_reports_abort_conflict_without_deleting_foreign_stage() {
    // C1 Memory release and Secret shred succeed; C2 a foreign directory occupies
    // the marker's private-stage name beside the exact published root. R1 abort
    // must fail, retain the marker/root, preserve foreign bytes, and report BOTH
    // the original creation cause and cleanup conflict. R2 after the fixture
    // moves only the foreign directory away, exact retry removes the owned root
    // without touching that moved directory. This reaches the post-shred abort
    // failure edge independently of the teardown/shred failure table above.
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("root"));
    let effect = fence("create", 1);
    let authority = pc::test_support::EffectAuthority::new(&effect);
    let guard = admitted(&root, &fingerprint, &effect, "creating", &authority);
    let marker = read_marker(&root).unwrap().unwrap();
    let stage = stage_path(&root, &marker.root).unwrap();
    std::fs::create_dir(&stage).unwrap();
    std::fs::write(stage.join("foreign"), b"keep foreign bytes").unwrap();
    let secret = root.join("credential");
    std::fs::write(&secret, b"synthetic credential").unwrap();
    let isolated = crate::IsolatedRoot::new(root.clone());
    let mut memory = crate::MemoryMountState::default();
    let error = crate::compensate_creation_failure(
        guard,
        &mut memory,
        &isolated,
        std::slice::from_ref(&secret),
        "conflicted creation",
        pc::SandboxError::backend(
            "compensation_reports_abort_conflict_without_deleting_foreign_stage",
            "original failure",
        ),
    )
    .await
    .to_string();
    assert!(error.contains("original failure"), "R1 original cause");
    assert!(
        error.contains("exact-owned cleanup failed"),
        "R1 abort failure"
    );
    assert_eq!(
        read_marker(&root).unwrap(),
        Some(marker),
        "R1 marker retained"
    );
    assert_eq!(
        std::fs::read(&secret).unwrap(),
        vec![0; b"synthetic credential".len()],
        "C1 shredded"
    );
    assert_eq!(
        std::fs::read(stage.join("foreign")).unwrap(),
        b"keep foreign bytes",
        "R1 foreign untouched"
    );
    let preserved = base.path().join("foreign-preserved");
    std::fs::rename(&stage, &preserved).unwrap();
    let guard = admitted(&root, &fingerprint, &effect, "creating", &authority);
    let error = crate::compensate_creation_failure(
        guard,
        &mut memory,
        &isolated,
        &[secret],
        "exact retry",
        pc::SandboxError::backend(
            "compensation_reports_abort_conflict_without_deleting_foreign_stage",
            "retry cause",
        ),
    )
    .await;
    assert_eq!(
        error.to_string(),
        pc::SandboxError::backend(
            "compensation_reports_abort_conflict_without_deleting_foreign_stage",
            "retry cause"
        )
        .to_string()
    );
    assert!(!root.exists(), "R2 owned root removed");
    assert!(read_marker(&root).unwrap().is_none(), "R2 marker removed");
    assert_eq!(
        std::fs::read(preserved.join("foreign")).unwrap(),
        b"keep foreign bytes",
        "R2 no foreign cleanup"
    );
}

#[tokio::test]
async fn cancelled_compensation_retains_original_memory_and_root_until_exact_retry() {
    // C1 incomplete/Ready marker; C2 the actual teardown response is Pending;
    // C3 caller cancels. R1 before and after cancellation: credential bytes and
    // exact marker remain untouched, original Memory guard remains retained.
    // R2 exact retry reuses that guard and invokes idempotent teardown again,
    // then removes only an incomplete root. No wall-clock sleeps or replacement
    // participant fixture. MemoryParticipantOwnership supplies the bounded
    // two-caller retain/exclusive-borrow proof, not kernel teardown/Worker death
    // or a proof of the filesystem compensation sequence itself.
    for phase in ["creating", "ready"] {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("root"));
        let effect = fence("create", 1);
        let authority = pc::test_support::EffectAuthority::new(&effect);
        let mut guard = admitted(&root, &fingerprint, &effect, phase, &authority);
        guard.prepare_root().unwrap();
        let secret = root.join("credential");
        std::fs::write(&secret, b"original credential").unwrap();
        let marker = read_marker(&root).unwrap();
        let teardowns = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut memory = crate::MemoryMountState::from(vec![Box::new(TeardownProbe {
            teardowns: teardowns.clone(),
            drops: drops.clone(),
            failures: 0,
            wait_until_ms: None,
            response_gate: Some(gate.clone()),
        })
            as Box<dyn pc::MemoryMount>]);
        let isolated = crate::IsolatedRoot::new(root.clone());
        let paths = [secret.clone()];
        let mut pending = Box::pin(crate::compensate_creation_failure(
            guard,
            &mut memory,
            &isolated,
            &paths,
            "cancelled creation",
            pc::SandboxError::backend(
                "cancelled_compensation_retains_original_memory_and_root_until_exact_retry",
                "original failure",
            ),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(teardowns.load(Ordering::SeqCst), 1, "C2 reached teardown");
        assert_eq!(
            std::fs::read(&secret).unwrap(),
            b"original credential",
            "R1 before cancel"
        );
        drop(pending);
        assert_eq!(
            std::fs::read(&secret).unwrap(),
            b"original credential",
            "R1 after cancel"
        );
        assert_eq!(read_marker(&root).unwrap(), marker, "R1 marker retained");
        assert_eq!(drops.load(Ordering::SeqCst), 0, "R1 guard retained");
        assert!(
            memory.require_empty().is_err(),
            "R1 original owner remains nonempty"
        );
        gate.add_permits(1);
        let guard = admitted(&root, &fingerprint, &effect, phase, &authority);
        let error = crate::compensate_creation_failure(
            guard,
            &mut memory,
            &isolated,
            &paths,
            "exact retry",
            pc::SandboxError::backend(
                "cancelled_compensation_retains_original_memory_and_root_until_exact_retry",
                "retry cause",
            ),
        )
        .await;
        assert_eq!(
            error.to_string(),
            pc::SandboxError::backend(
                "cancelled_compensation_retains_original_memory_and_root_until_exact_retry",
                "retry cause"
            )
            .to_string()
        );
        assert_eq!(
            root.exists(),
            phase == "ready",
            "R2 completed root preserved"
        );
        assert_eq!(teardowns.load(Ordering::SeqCst), 2, "R2 idempotent retry");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "R2 same guard retired once"
        );
    }
}
