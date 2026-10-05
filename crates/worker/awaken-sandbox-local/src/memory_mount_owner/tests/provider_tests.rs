// @formal evidence memory_participant.live_provider_retains_pending_and_failed_operations
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
use super::*;
use pc::Sandbox as _;
use std::future::IntoFuture;

#[tokio::test]
async fn creation_and_adoption_recheck_authority_and_retain_pending_memory() {
    // A1-A120: Local/Namespace x guard allocated before/after mount Pending x
    // revoke/replace/exhaust/renew/cancel x past/future Control timestamps x
    // fresh creation/Ready creation replay/cold adoption.
    // Entry denial -> zero mounts. Loss after a REAL Pending mount -> no
    // successful response or unauthenticated teardown; cancellation retains the
    // pending owner. Renewal -> one confirmed participant. Exact retry retires
    // an unconfirmed participant once. Ready keeps marker/Agent bytes and its
    // handle; incomplete creation clears partial bytes only on admitted retry.
    // Inline AFTER Memory detects unauthorized later writes/Ready publication.
    // This is port/FS/ownership qualification;
    // the fake FUSE guard does not prove kernel mount or Worker-death recovery.
    use awaken_provisioning_contract::test_support::FenceTamper as _;
    use std::time::Duration;
    async fn reopen(
        provider: &dyn pc::SandboxProvider,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        match handle {
            Some(handle) => {
                provider
                    .adopt_for_effect(spec, handle, fence, authority)
                    .await
            }
            None => provider.claim_for_effect(spec, fence, authority).await,
        }
    }
    for operation in ["adopt", "create-replay", "create"] {
        for namespace in [false, true] {
            for before_pause in [false, true] {
                for expiry in [100, u64::MAX - 1] {
                    for change in ["revoke", "replace", "exhaust", "renew", "cancel"] {
                        let root = tempfile::tempdir().unwrap();
                        let mounter = Arc::new(PausedMounter::new(before_pause, 0));
                        let make_provider = || -> Box<dyn pc::SandboxProvider> {
                            if namespace {
                                Box::new(
                                    crate::NamespaceProvider::new(root.path())
                                        .with_memory_mounter(mounter.clone()),
                                )
                            } else {
                                Box::new(
                                    crate::LocalProvider::trusted_host(root.path())
                                        .with_memory_mounter(mounter.clone()),
                                )
                            }
                        };
                        let mut spec = workdir_spec("adoption-proof", false);
                        if namespace {
                            spec.isolation = pc::IsolationClass::Namespace;
                        }
                        spec.mounts.push(requirement("memory"));
                        spec.mounts.push(pc::MountRequirement {
                            mount_id: "after-memory".into(),
                            mount_path: "/after-memory".into(),
                            source: pc::MountSource::Inline {
                                contents: "declared".into(),
                            },
                            access: pc::MountAccess::ReadWrite,
                            lifetime: pc::MountLifetime::Session,
                            required: true,
                        });
                        let create =
                            pc::SandboxEffectFence::new("create", "owner", "boot", 1, expiry)
                                .unwrap();
                        let handle = if operation != "create" {
                            mounter.mount_gate.add_permits(1);
                            let old = make_provider();
                            let first = old
                        .claim_for_effect(
                            &spec,
                            &create,
                            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                &create,
                            ),
                        )
                        .await
                        .unwrap();
                            let handle = first.handle();
                            drop(first);
                            drop(old);
                            assert_eq!(mounter.mount_gate.forget_permits(1), 1);
                            Some(handle)
                        } else {
                            None
                        };
                        let baseline = usize::from(handle.is_some());
                        let proof = root.path().join("adoption-proof/agent-proof");
                        if handle.is_some() {
                            std::fs::write(&proof, b"unchanged Agent bytes").unwrap();
                        }
                        let provider = make_provider();
                        let fence = if operation == "adopt" {
                            pc::SandboxEffectFence::new("adopt", "owner", "boot", 2, expiry)
                                .unwrap()
                        } else {
                            create
                        };
                        let adoption_handle =
                            (operation == "adopt").then(|| handle.as_ref().unwrap());
                        let authority = pc::test_support::EffectAuthority::new(&fence);
                        assert!(
                            reopen(
                                provider.as_ref(),
                                &spec,
                                adoption_handle,
                                &fence,
                                &pc::test_support::EffectAuthority::denied()
                            )
                            .await
                            .is_err()
                        );
                        assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), baseline);
                        if operation == "create" {
                            assert!(
                                !proof.parent().unwrap().exists(),
                                "entry denial creates no root"
                            );
                        }
                        let mut pending = Box::pin(reopen(
                            provider.as_ref(),
                            &spec,
                            adoption_handle,
                            &fence,
                            &authority,
                        ));
                        assert_pending(pending.as_mut()).await;
                        assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), baseline + 1);
                        std::fs::write(&proof, b"unchanged Agent bytes").unwrap();
                        let root_identity =
                            awaken_sandbox_fs::directory_identity_nofollow(proof.parent().unwrap())
                                .unwrap();
                        {
                            let mut current = authority.current.lock().unwrap();
                            match change {
                                "revoke" => *current = None,
                                "replace" => current.as_mut().unwrap().0.reassign(),
                                "exhaust" => current.as_mut().unwrap().1 = Duration::ZERO,
                                "renew" => current.as_mut().unwrap().0.renew(),
                                _ => {}
                            }
                        }
                        if change == "cancel" {
                            drop(pending);
                        } else {
                            mounter.mount_gate.add_permits(1);
                            assert_eq!(
                                pending.await.is_ok(),
                                change == "renew",
                                "{operation}/{namespace}/{before_pause}/{expiry}/{change}"
                            );
                        }
                        assert_eq!(
                            mounter.teardowns.load(Ordering::SeqCst),
                            0,
                            "no stale compensation"
                        );
                        assert_eq!(
                            mounter.drops.load(Ordering::SeqCst),
                            baseline,
                            "new participant remains owned"
                        );
                        assert_eq!(std::fs::read(&proof).unwrap(), b"unchanged Agent bytes");
                        assert_eq!(
                            proof.parent().unwrap().join("after-memory").exists(),
                            operation != "create" || change == "renew",
                            "lost proof cannot admit the next file write"
                        );
                        *authority.current.lock().unwrap() =
                            Some((fence.clone(), Duration::from_secs(1)));
                        if change == "cancel" {
                            mounter.mount_gate.add_permits(1);
                        }
                        let replay = reopen(
                            provider.as_ref(),
                            &spec,
                            adoption_handle,
                            &fence,
                            &authority,
                        )
                        .await
                        .unwrap();
                        if let Some(handle) = handle {
                            assert_eq!(replay.handle(), handle);
                        }
                        assert_eq!(
                            awaken_sandbox_fs::directory_identity_nofollow(proof.parent().unwrap())
                                .unwrap(),
                            root_identity
                        );
                        if operation != "create" || change == "renew" {
                            assert_eq!(std::fs::read(&proof).unwrap(), b"unchanged Agent bytes");
                        } else {
                            assert!(
                                !proof.exists(),
                                "admitted incomplete retry resets partial bytes"
                            );
                        }
                        assert_eq!(
                            std::fs::read(proof.parent().unwrap().join("after-memory")).unwrap(),
                            b"declared"
                        );
                        assert_eq!(
                            mounter.mount_calls.load(Ordering::SeqCst),
                            baseline + if change == "renew" { 1 } else { 2 }
                        );
                        assert_eq!(
                            mounter.teardowns.load(Ordering::SeqCst),
                            usize::from(change != "renew")
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn copy_replay_keeps_original_bytes_and_ack_prevents_second_harvest() {
    use awaken_resource_contract::MemoryRepository as _;
    use pc::MemoryMounter as _;
    // P13-P16: Local/Namespace x create replay/fenced adoption. The same live
    // Provider owns the original Copy guard after wrapper Drop. Replay must
    // preserve Agent bytes AND original durable heads without early writeback.
    // The existing Memoryd recovered-copy CAS path is the Host-side reconciler
    // seam; only its successful exact acknowledgement retires the Copy guard.
    // Editing the still-present file after ACK is a negative control: terminal
    // disposal must not harvest again. This is a real store/mounter/FS test,
    // not a full distributed Host integration or credential qualification.
    for namespace in [false, true] {
        for adoption in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                awaken_time::system_clock(),
            ));
            store
                .create("memory", "/note.md", "original")
                .await
                .unwrap();
            let mounter = Arc::new(awaken_sandbox_memoryd::MemoryStoreMounter::copy_only(
                store.clone(),
            ));
            let provider: Box<dyn pc::SandboxProvider> = if namespace {
                Box::new(
                    crate::NamespaceProvider::new(root.path()).with_memory_mounter(mounter.clone()),
                )
            } else {
                Box::new(
                    crate::LocalProvider::trusted_host(root.path())
                        .with_memory_mounter(mounter.clone()),
                )
            };
            let mut spec = workdir_spec("copy-owner", false);
            if namespace {
                spec.isolation = pc::IsolationClass::Namespace;
            }
            spec.mounts.push(requirement("memory"));
            let create =
                pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX).unwrap();
            let first = provider
                .claim_for_effect(
                    &spec,
                    &create,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
                )
                .await
                .unwrap();
            let handle = first.handle();
            let heads = handle.memory_materializations().unwrap().unwrap().to_vec();
            let file = root.path().join("copy-owner/workspace/memory/note.md");
            std::fs::write(&file, b"agent edit").unwrap();
            drop(first);
            let replay = if adoption {
                provider
                    .adopt_for_effect(
                        &spec,
                        &handle,
                        &create,
                        &pc::test_support::EffectAuthority::new(&create),
                    )
                    .await
                    .unwrap()
            } else {
                provider
                    .claim_for_effect(
                        &spec,
                        &create,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
                    )
                    .await
                    .unwrap()
            };
            assert_eq!(replay.handle(), handle);
            assert_eq!(std::fs::read(&file).unwrap(), b"agent edit");
            assert_eq!(
                store
                    .get_by_path("memory", "/note.md")
                    .await
                    .unwrap()
                    .unwrap()
                    .content
                    .as_deref(),
                Some("original")
            );
            mounter
                .reconcile_recovered_copy(
                    "memory",
                    &heads[0],
                    &[("note.md".into(), b"agent edit".to_vec())],
                    pc::MountAccess::ReadWrite,
                )
                .await
                .unwrap();
            let terminal =
                pc::SandboxEffectFence::new("dispose", "owner", "boot", 2, u64::MAX).unwrap();
            let authority = pc::test_support::EffectAuthority::new(&terminal);
            replay
                .acknowledge_memory_reconciliation(&terminal, &heads, &authority)
                .await
                .unwrap();
            replay
                .acknowledge_memory_reconciliation(&terminal, &heads, &authority)
                .await
                .unwrap();
            std::fs::write(&file, b"must not harvest twice").unwrap();
            replay
                .prepare_disposal_for_effect(&terminal, &authority)
                .await
                .unwrap();
            let authorization = crate::test_disposal_authorization(&terminal);
            let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
            replay
                .dispose_for_effect(&authorization, &authority)
                .await
                .unwrap();
            assert!(!file.exists());
            assert_eq!(
                store
                    .get_by_path("memory", "/note.md")
                    .await
                    .unwrap()
                    .unwrap()
                    .content
                    .as_deref(),
                Some("agent edit")
            );
        }
    }
}

#[tokio::test]
async fn provider_retry_replay_and_adoption_share_the_original_participants() {
    // P1-P8: Local/Namespace x mount Pending before/after guard creation x
    // compensation succeeds/fails once. Cancel the REAL fenced create entry,
    // then retry that same effect. Cleanup must resume the original mount,
    // never reset its root first, and a failed teardown cannot lose its guard.
    // Ready create replay + fenced adoption (after all old wrappers Drop)
    // must reuse the resulting FUSE participant. Terminal disposal releases
    // it exactly once. This is adapter wiring, not an actual FUSE kernel test.
    for namespace in [false, true] {
        for before_pause in [false, true] {
            for failures in [0, 1] {
                let root = tempfile::tempdir().unwrap();
                let mounter = Arc::new(PausedMounter::new(before_pause, failures));
                let provider: Box<dyn pc::SandboxProvider> = if namespace {
                    Box::new(
                        crate::NamespaceProvider::new(root.path())
                            .with_memory_mounter(mounter.clone()),
                    )
                } else {
                    Box::new(
                        crate::LocalProvider::trusted_host(root.path())
                            .with_memory_mounter(mounter.clone()),
                    )
                };
                let mut spec = workdir_spec("retained-provider", false);
                if namespace {
                    spec.isolation = pc::IsolationClass::Namespace;
                }
                spec.mounts.push(requirement("memory"));
                let effect =
                    pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX).unwrap();
                let create_authority = pc::test_support::EffectAuthority::new(&effect);
                let mut first = Box::pin(
                    provider
                        .claim_for_effect(&spec, &effect, &create_authority)
                        .into_future(),
                );
                assert_pending(first.as_mut()).await;
                assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), 1);
                drop(first);
                assert_eq!(mounter.drops.load(Ordering::SeqCst), 0);
                mounter.mount_gate.add_permits(1);
                if failures != 0 {
                    assert!(
                        provider
                            .claim_for_effect(
                                &spec,
                                &effect,
                                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                    &effect
                                )
                            )
                            .await
                            .is_err()
                    );
                    assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(mounter.drops.load(Ordering::SeqCst), 0);
                }
                let first = provider
                    .claim_for_effect(
                        &spec,
                        &effect,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(&effect),
                    )
                    .await
                    .unwrap();
                assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), 2);
                assert_eq!(mounter.drops.load(Ordering::SeqCst), 1);
                let handle = first.handle();
                drop(first);
                let replay = provider
                    .claim_for_effect(
                        &spec,
                        &effect,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(&effect),
                    )
                    .await
                    .unwrap();
                assert_eq!(replay.handle(), handle);
                drop(replay);
                let adopted = provider
                    .adopt_for_effect(
                        &spec,
                        &handle,
                        &effect,
                        &pc::test_support::EffectAuthority::new(&effect),
                    )
                    .await
                    .unwrap();
                assert_eq!(adopted.handle(), handle);
                assert_eq!(
                    mounter.mount_calls.load(Ordering::SeqCst),
                    2,
                    "no third mount"
                );
                assert_eq!(
                    mounter.drops.load(Ordering::SeqCst),
                    1,
                    "wrapper Drop is not teardown"
                );
                let terminal =
                    pc::SandboxEffectFence::new("dispose", "owner", "boot", 2, u64::MAX).unwrap();
                let authority = pc::test_support::EffectAuthority::new(&terminal);
                adopted
                    .prepare_disposal_for_effect(&terminal, &authority)
                    .await
                    .unwrap();
                let authorization = crate::test_disposal_authorization(&terminal);
                let authority =
                    pc::test_support::EffectAuthority::new(authorization.effect_fence());
                adopted
                    .dispose_for_effect(&authorization, &authority)
                    .await
                    .unwrap();
                assert_eq!(mounter.teardowns.load(Ordering::SeqCst), failures + 2);
                assert_eq!(mounter.drops.load(Ordering::SeqCst), 2);
            }
        }
    }
}

#[tokio::test]
async fn namespace_attach_serializes_destination_admission_and_cancelled_mount_retry() {
    // P9-P12: first attach returns/is cancelled x guard before/after Pending.
    // The second caller must wait BEFORE destination admission. Success makes
    // it reject without another mount; cancellation makes it resume/compensate
    // the old operation, then attach once. Dispose releases the sole published
    // guard. Fixed destination is deliberately identical for both callers.
    for cancel in [false, true] {
        for before_pause in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mounter = Arc::new(PausedMounter::new(before_pause, 0));
            let provider =
                crate::NamespaceProvider::new(root.path()).with_memory_mounter(mounter.clone());
            let mut spec = workdir_spec("dynamic-memory", false);
            spec.isolation = pc::IsolationClass::Namespace;
            let sandbox = provider.create_sandbox(&spec).await.unwrap();
            let req = requirement("memory");
            let mut first = Box::pin(sandbox.attach(req.clone()));
            assert_pending(first.as_mut()).await;
            let mut second = Box::pin(sandbox.attach(req.clone()));
            assert_pending(second.as_mut()).await;
            assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), 1);
            mounter.mount_gate.add_permits(1);
            if cancel {
                drop(first);
            } else {
                first.await.unwrap();
            }
            assert_eq!(second.await.is_ok(), cancel);
            assert_eq!(
                mounter.mount_calls.load(Ordering::SeqCst),
                if cancel { 2 } else { 1 }
            );
            assert_eq!(
                mounter.teardowns.load(Ordering::SeqCst),
                usize::from(cancel)
            );
            assert!(sandbox.attach(req).await.is_err());
            sandbox.dispose_unattached().await.unwrap();
            assert_eq!(
                mounter.drops.load(Ordering::SeqCst),
                if cancel { 2 } else { 1 }
            );
        }
    }
}
