use super::super::workdir_helper_tests::{TeardownProbe, workdir_spec};
use super::super::{LocalProvider, pc};
use pc::Sandbox as _;
use std::sync::Arc;

#[tokio::test]
async fn physical_disposal_rechecks_current_authority_after_every_retained_wait() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    // Cause/effect decision table: C1 wait for participant/quiescence/mount
    // lock/first or last mount; C2 revoke/replace/expire/renew/cancel. Every
    // 5x5 row first polls the REAL future Pending, then changes C2. R1 revoked,
    // replaced or exhausted current proof => no NEXT teardown, shred, removal
    // or terminal success. R2 same-generation renewal => finish once. R3
    // cancellation retains the original participant and exact resource set.
    // R4 retry of R1/R3 under restored explicit fixture authority converges on
    // the SAME Sandbox. An admitted teardown may finish; its late response is
    // not permission to start the next. Cross past/future Control timestamps:
    // UTC cannot reject a fresh proof or revive an exhausted proof.
    for (boundary, expiry) in [
        "participant",
        "quiescence",
        "mount-lock",
        "first-mount",
        "last-mount",
    ]
    .into_iter()
    .flat_map(|boundary| [100, u64::MAX - 1].map(|expiry| (boundary, expiry)))
    {
        for change in ["revoke", "replace", "expire", "renew", "cancel"] {
            let tmp = tempfile::tempdir().unwrap();
            let fence = pc::SandboxEffectFence::new("dispose", "owner", "boot", 2, expiry).unwrap();
            let create =
                pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX).unwrap();
            let sandbox = LocalProvider::new(tmp.path())
                .create_sandbox_for_effect(
                    &workdir_spec("physical-authority", false),
                    &create,
                    None,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
                )
                .await
                .unwrap();
            sandbox
                .prepare_disposal_for_effect(
                    &fence,
                    &pc::test_support::EffectAuthority::new(&fence),
                )
                .await
                .unwrap();
            let authorization = crate::test_disposal_authorization(&fence);
            let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
            let secret = sandbox.workspace_path().join("synthetic-secret");
            std::fs::write(&secret, b"synthetic-material").unwrap();
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let calls = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
            let drops = Arc::new(AtomicUsize::new(0));
            for (index, calls) in calls.iter().enumerate() {
                sandbox
                    .memory_mounts
                    .lock()
                    .await
                    .push(Box::new(TeardownProbe {
                        teardowns: calls.clone(),
                        drops: drops.clone(),
                        failures: 0,
                        wait_until_ms: None,
                        response_gate: ((boundary == "first-mount" && index == 0)
                            || (boundary == "last-mount" && index == 1))
                            .then(|| gate.clone()),
                    }));
            }
            let participant = if boundary == "participant" {
                Some(sandbox.terminal_removal.lock().await)
            } else {
                None
            };
            let mounts = if boundary == "mount-lock" {
                Some(sandbox.memory_mounts.lock().await)
            } else {
                None
            };
            // Ignore cooperative-budget yields in this bounded fixture: Pending
            // must be the selected physical wait, not an earlier scheduler yield.
            let mut pending = Box::pin(tokio::task::unconstrained(
                crate::dispose_terminal_realization(
                    &sandbox.memory_mounts,
                    sandbox.root.root(),
                    &sandbox.secret_paths,
                    &sandbox.terminal_removal,
                    &authorization,
                    &authority,
                    async {
                        if boundary == "quiescence" {
                            let _permit = tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                gate.acquire(),
                            )
                            .await
                            .expect("timed out waiting for gate.acquire()")
                            .unwrap();
                        }
                    },
                ),
            ));
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                    "{boundary}/{change}"
                );
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                calls[0].load(Ordering::SeqCst),
                usize::from(matches!(boundary, "first-mount" | "last-mount")),
                "entered {boundary}"
            );
            assert_eq!(
                calls[1].load(Ordering::SeqCst),
                usize::from(boundary == "last-mount"),
                "entered {boundary}"
            );
            let mut current = authorization.effect_fence().clone();
            current.expires_at_unix_ms += 1;
            if change == "replace" {
                current.epoch += 1;
            }
            *authority.current.lock().unwrap() = if change == "revoke" {
                None
            } else {
                Some((
                    current,
                    if change == "expire" {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(1)
                    },
                ))
            };
            drop(participant);
            drop(mounts);
            gate.add_permits(1);
            if change == "cancel" {
                drop(pending);
            } else {
                assert_eq!(
                    pending.await.is_ok(),
                    change == "renew",
                    "{boundary}/{change}"
                );
            }
            assert_eq!(
                calls[0].load(Ordering::SeqCst),
                usize::from(change == "renew" || matches!(boundary, "first-mount" | "last-mount")),
                "first teardown {boundary}/{change}/{expiry}"
            );
            assert_eq!(
                calls[1].load(Ordering::SeqCst),
                usize::from(change == "renew" || boundary == "last-mount"),
                "second teardown {boundary}/{change}/{expiry}"
            );
            if change != "renew" {
                assert_eq!(std::fs::read(&secret).unwrap(), b"synthetic-material");
                assert!(sandbox.terminal_removal.lock().await.is_some());
                assert_eq!(sandbox.memory_mounts.lock().await.len(), 2);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                *authority.current.lock().unwrap() =
                    Some((authorization.effect_fence().clone(), Duration::from_secs(1)));
                sandbox
                    .dispose_for_effect(&authorization, &authority)
                    .await
                    .unwrap();
            }
            assert!(!sandbox.root.root().exists());
            assert!(sandbox.terminal_removal.lock().await.is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 2);
        }
    }
}

#[tokio::test]
async fn terminal_preparation_requires_current_authority_before_creating_or_reusing_a_participant()
{
    // C1 no participant / cached prepared participant; C2 revoked / changed
    // operation / changed generation / live renewal. R1 any stale authority
    // denies with zero new marker ownership and unchanged Secret/root bytes.
    // R2 a live same-generation renewal prepares/replays the original durable
    // predecessor, never the proof's newer expiry. Empty Memory is intentional:
    // it must not bypass authority. CT4 also crosses past/future Control expiry
    // without modifying those durable coordinates to fit the Worker clock.
    for (cached, expiry) in [false, true]
        .into_iter()
        .flat_map(|cached| [100, u64::MAX - 1].map(|expiry| (cached, expiry)))
    {
        for change in ["revoke", "operation", "generation", "renew"] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = LocalProvider::new(tmp.path());
            let fence = pc::SandboxEffectFence::new("prepare", "owner", "boot", 2, expiry).unwrap();
            let create =
                pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX).unwrap();
            let sandbox = provider
                .create_sandbox_for_effect(
                    &workdir_spec("prepare-authority", false),
                    &create,
                    None,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
                )
                .await
                .unwrap();
            let secret = sandbox.workspace_path().join("synthetic-secret");
            std::fs::write(&secret, b"synthetic-material").unwrap();
            let authority = pc::test_support::EffectAuthority::new(&fence);
            if cached {
                sandbox
                    .prepare_disposal_for_effect(&fence, &authority)
                    .await
                    .unwrap();
            }
            let mut current = fence.clone();
            current.expires_at_unix_ms += 1;
            match change {
                "operation" => current.operation_id.push('x'),
                "generation" => current.epoch += 1,
                _ => {}
            }
            *authority.current.lock().unwrap() = if change == "revoke" {
                None
            } else {
                Some((current, std::time::Duration::from_secs(1)))
            };
            let result = sandbox
                .prepare_disposal_for_effect(&fence, &authority)
                .await;
            assert_eq!(result.is_ok(), change == "renew", "{cached}/{change}");
            assert_eq!(
                sandbox.terminal_removal.lock().await.is_some(),
                cached || change == "renew"
            );
            assert_eq!(std::fs::read(&secret).unwrap(), b"synthetic-material");
            if let Ok(prepared) = result {
                assert_eq!(prepared, fence);
            }
        }
    }
}

#[tokio::test]
async fn terminal_disposal_teardown_error_retains_every_participant_until_exact_retry() {
    // D8 first FUSE teardown fails => retain Memory guard, prepared marker and
    // unshredded secret/root. D9 exact retry succeeds on that SAME guard =>
    // teardown twice, drop once, then secret/root/marker cleanup. Reuse the
    // creation-compensation probe, not another teardown implementation.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let fence = pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let mut sandbox = provider
        .create_sandbox_for_effect(
            &workdir_spec("teardown-retry", false),
            &fence,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
        )
        .await
        .unwrap();
    let secret = sandbox.workspace_path().join("synthetic-secret");
    std::fs::write(&secret, b"synthetic-material").unwrap();
    sandbox.secret_paths.push(secret.clone());
    sandbox
        .prepare_disposal_for_effect(
            &fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
        )
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    sandbox
        .memory_mounts
        .lock()
        .await
        .push(Box::new(TeardownProbe {
            teardowns: calls.clone(),
            drops: drops.clone(),
            failures: 1,
            wait_until_ms: None,
            response_gate: None,
        }));
    let authorization = crate::test_disposal_authorization(&fence);
    assert!(
        sandbox
            .dispose_for_effect(
                &authorization,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    authorization.effect_fence()
                )
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&secret).unwrap(), b"synthetic-material");
    assert!(sandbox.terminal_removal.lock().await.is_some());
    assert_eq!(sandbox.memory_mounts.lock().await.len(), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    sandbox
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(!sandbox.root.root().exists());
}

#[tokio::test]
async fn terminal_cleanup_uses_current_proof_not_provider_wall_time() {
    // Clock/authority cause-effect table CT: C1 past/future Control timestamp;
    // C2 current/revoked/exhausted local proof; C3 resident/cold/absent participant;
    // C4 Local/Namespace adapter. CT1 current => prepare and dispose exact bytes,
    // including a current proof whose Control timestamp is behind Worker UTC.
    // CT2 revoked/exhausted => zero terminal writes even with far-future expiry.
    // CT3 preparation/replay retain original Control coordinates and source
    // identity. The existing 5x5 Pending table owns all async loss/retry cuts;
    // it replaces the old duplicate sleep-until-UTC table's authority premise.
    // Namespace rows exercise filesystem lifecycle only, not OS confinement.
    for namespace in [false, true] {
        if namespace && !cfg!(any(target_os = "linux", target_os = "macos")) {
            continue;
        }
        for expiry in [100, u64::MAX - 1] {
            for proof in ["current", "revoked", "exhausted"] {
                for phase in ["resident", "cold", "absent"] {
                    let tmp = tempfile::tempdir().unwrap();
                    let provider: Box<dyn pc::SandboxProvider> = if namespace {
                        Box::new(crate::NamespaceProvider::new(tmp.path()))
                    } else {
                        Box::new(LocalProvider::new(tmp.path()))
                    };
                    let mut spec = workdir_spec("clock-authority", false);
                    if namespace {
                        spec.isolation = pc::IsolationClass::Namespace;
                    }
                    let create =
                        pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX)
                            .unwrap();
                    let terminal =
                        pc::SandboxEffectFence::new("terminal", "owner", "boot", 2, expiry)
                            .unwrap();
                    let authority = pc::test_support::EffectAuthority::new(&terminal);
                    if proof == "revoked" {
                        *authority.current.lock().unwrap() = None;
                    } else if proof == "exhausted" {
                        authority.current.lock().unwrap().as_mut().unwrap().1 =
                            std::time::Duration::ZERO;
                    }
                    let root = crate::sandbox_dir(tmp.path(), &spec.scope);
                    let secret = root.join("synthetic-retained");
                    let mut sandbox = if phase == "absent" {
                        None
                    } else {
                        let sandbox = provider
                            .claim_for_effect(
                                &spec,
                                &create,
                                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                    &create,
                                ),
                            )
                            .await
                            .unwrap();
                        std::fs::write(&secret, b"unchanged-by-denial").unwrap();
                        Some(sandbox)
                    };
                    let handle = sandbox.as_ref().map(|sandbox| sandbox.handle());
                    let admitted = if phase == "resident" {
                        sandbox
                            .as_ref()
                            .unwrap()
                            .prepare_disposal_for_effect(&terminal, &authority)
                            .await
                            .map(|_| ())
                    } else {
                        drop(sandbox.take());
                        match provider
                            .prepare_terminal_for_effect(
                                &spec,
                                handle.as_ref(),
                                if phase == "absent" {
                                    Some(&create)
                                } else {
                                    None
                                },
                                &terminal,
                                &authority,
                            )
                            .await
                        {
                            Ok(recovered) => {
                                sandbox = recovered;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    };
                    assert_eq!(
                        admitted.is_ok(),
                        proof == "current",
                        "CT {namespace}/{expiry}/{proof}/{phase}: {admitted:?}"
                    );
                    if proof != "current" {
                        if phase != "absent" {
                            assert_eq!(std::fs::read(&secret).unwrap(), b"unchanged-by-denial");
                            assert_eq!(
                                provider
                                    .observe_for_effect(&spec, handle.as_ref().unwrap(), &create)
                                    .await
                                    .unwrap(),
                                pc::SandboxObservation::Ready,
                                "CT2 no terminal marker transition",
                            );
                        } else {
                            assert!(!root.exists());
                            assert!(
                                std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
                                "CT2 no lock or tombstone publication"
                            );
                        }
                        continue;
                    }
                    if let Some(sandbox) = sandbox {
                        let prepared = sandbox
                            .prepare_disposal_for_effect(&terminal, &authority)
                            .await
                            .unwrap();
                        assert_eq!(prepared, terminal, "CT3 no synthetic UTC projection");
                        let authorization = crate::test_disposal_authorization(&prepared);
                        sandbox
                            .dispose_for_effect(
                                &authorization,
                                &pc::test_support::EffectAuthority::new(
                                    authorization.effect_fence(),
                                ),
                            )
                            .await
                            .unwrap();
                        assert!(
                            !root.exists() && !secret.exists(),
                            "CT1 exact terminal outcome"
                        );
                    } else {
                        assert_eq!(phase, "absent");
                        assert!(!root.exists());
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn terminal_disposal_cancellation_retains_the_original_participant_and_serializes_retry() {
    // D1 exact prepared guard + Memory-list lock held => disposal waits while
    // retaining the original marker lock. D2 a second disposer queues; D3 the
    // first future is cancelled => same participant survives, never None.
    // D4 sync acknowledgement/preparation during the wait => busy, no blocking
    // lock-order cycle with Memory. D5 unlock => queued retry alone completes
    // mounts -> root -> Removed; only then forget the resident participant.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("cancelled-terminal-disposal", false);
    let create = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let sandbox = provider
        .create_sandbox_for_effect(
            &spec,
            &create,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
        )
        .await
        .unwrap();
    sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .unwrap();
    let root = sandbox.root.root().to_path_buf();
    let authorization = crate::test_disposal_authorization(&terminal);
    let mounts = sandbox.memory_mounts.lock().await;
    let disposal_authority = awaken_provisioning_contract::test_support::EffectAuthority::new(
        authorization.effect_fence(),
    );
    let mut first = Box::pin(sandbox.dispose_for_effect(&authorization, &disposal_authority));
    tokio::select! {
        biased;
        result = &mut first => panic!("D1 premature disposal: {result:?}"),
        () = std::future::ready(()) => {}
    }
    assert!(
        sandbox.terminal_removal.try_lock().is_err(),
        "D1 same async slot held"
    );
    assert!(
        sandbox
            .prepare_disposal_for_effect(
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal)
            )
            .await
            .is_err(),
        "D4 fail busy, not deadlock"
    );
    let disposal_authority = awaken_provisioning_contract::test_support::EffectAuthority::new(
        authorization.effect_fence(),
    );
    let mut second = Box::pin(sandbox.dispose_for_effect(&authorization, &disposal_authority));
    tokio::select! {
        biased;
        result = &mut second => panic!("D2 bypassed original participant: {result:?}"),
        () = std::future::ready(()) => {}
    }
    drop(first);
    tokio::select! {
        biased;
        result = &mut second => panic!("D3 queued retry lost the retained participant: {result:?}"),
        () = std::future::ready(()) => {}
    }
    // Tokio hands the lock to its queued waiter; do not mistake that fair
    // reservation for a missing participant. Cancel this waiter too to inspect
    // the retained value, then run the final exact retry.
    drop(second);
    assert!(
        sandbox.terminal_removal.try_lock().unwrap().is_some(),
        "D3 guard survives cancellation"
    );
    assert!(root.exists(), "D1-D4 no early root deletion");
    drop(mounts);
    sandbox
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
    assert!(!root.exists(), "D5 root deleted");
    assert!(
        sandbox.terminal_removal.lock().await.is_none(),
        "D5 completion releases participant"
    );
}

#[tokio::test]
async fn disposal_requires_authorization_before_quiescence_and_retains_cancelled_quiescence() {
    // D6 foreign original preparation => no quiesce callback, no physical
    // mutation, retain guard. D7 valid authorization + pending quiescence =>
    // no mounts/root cleanup; cancellation retains guard; exact retry finishes.
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("quiesce-terminal-disposal", false);
    let create = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let foreign = pc::SandboxEffectFence::new("foreign", "owner", "runtime", 1, u64::MAX).unwrap();
    let sandbox = provider
        .create_sandbox_for_effect(
            &spec,
            &create,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
        )
        .await
        .unwrap();
    sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .unwrap();
    let entered = AtomicBool::new(false);
    assert!(
        crate::dispose_terminal_realization(
            &sandbox.memory_mounts,
            sandbox.root.root(),
            &sandbox.secret_paths,
            &sandbox.terminal_removal,
            &crate::test_disposal_authorization(&foreign),
            &pc::test_support::EffectAuthority::new(
                crate::test_disposal_authorization(&foreign).effect_fence()
            ),
            async {
                entered.store(true, Ordering::SeqCst);
            },
        )
        .await
        .is_err()
    );
    assert!(
        !entered.load(Ordering::SeqCst),
        "D6 unauthorized cannot close service"
    );
    let authorization = crate::test_disposal_authorization(&terminal);
    let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
    let mut pending = Box::pin(crate::dispose_terminal_realization(
        &sandbox.memory_mounts,
        sandbox.root.root(),
        &sandbox.secret_paths,
        &sandbox.terminal_removal,
        &authorization,
        &authority,
        async {
            entered.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        },
    ));
    tokio::select! {
        biased;
        result = &mut pending => panic!("D7 premature completion: {result:?}"),
        () = std::future::ready(()) => {}
    }
    assert!(entered.load(Ordering::SeqCst));
    drop(pending);
    assert!(
        sandbox.root.root().exists(),
        "D7 no delete before quiescence"
    );
    assert!(
        sandbox.terminal_removal.lock().await.is_some(),
        "D7 guard retained"
    );
    sandbox
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
}
