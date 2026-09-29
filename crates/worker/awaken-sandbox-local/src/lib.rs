// @formal entrypoint sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! `awaken-sandbox-local` — single-machine sandbox isolation realizing the neutral
//! [`awaken_provisioning_contract`] seam (ADR-0041).
//!
//! Each sandbox is an [`IsolatedRoot`] (a path jail). Rooted tools execute in it:
//! [`HandToolContext`] owns path projection and directory-capability confinement
//! (with `bwrap --unshare-net` for network-denied Bash). A rooted tool is a `RawTool` the
//! host composes into a run (ADR-0034 D6), so the kernel stays sandbox-agnostic.
//!
//! The provider surface is the pc contract: [`LocalProvider`] (Workdir tier) and
//! [`NamespaceProvider`] (bubblewrap tier) implement
//! [`awaken_provisioning_contract::SandboxProvider`], realizing a `SandboxSpec` into
//! a [`LocalSandbox`] / [`NamespaceSandbox`]. The Workdir [`LocalSandbox`] carries the
//! host-tier helpers the host composes — [`LocalSandbox::rooted_tools`] (the built-in
//! capability surface), repo clone/write-back, and artifact/skill scanning — while
//! `spawn` launches opaque agent processes. Mount bytes resolve through an injected
//! [`BlobSource`] port (ADR-0038 D6, dependency-inverted): this worker-tier crate
//! links no durable store; the composition root adapts the content-addressed store.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::{HandToolContext, all_hand_tools_in};
use awaken_runtime_contract::ContentBlock;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolExecutionTarget, ToolOutput};

#[path = "namespace/memory_mount.rs"]
mod memory_mount;
mod memory_mount_owner;
pub(crate) use memory_mount::{
    AdoptionMemoryReplay, TerminalMemoryReplay, mount_memory_requirement,
    prepare_adoption_memory_replay, prepare_terminal_memory_replay, replay_memory_mount_guards,
};
pub(crate) use memory_mount_owner::{MemoryMountOwner, MemoryMountRegistry, MemoryMountState};

/// Validate and project the one provider-neutral Ready receipt against the
/// frozen effective spec. Local and Namespace intentionally share this code so
/// response-loss recovery cannot grow provider-specific reconstruction rules.
pub(crate) fn replay_completion_receipt(
    spec: &awaken_provisioning_contract::SandboxSpec,
    receipt: &realization_marker::RealizationCompletionReceipt,
    ordinary_realization: awaken_provisioning_contract::Realization,
) -> Result<
    (
        Vec<awaken_provisioning_contract::RealizedMount>,
        Vec<awaken_provisioning_contract::MemoryMaterializationEvidence>,
    ),
    awaken_provisioning_contract::SandboxError,
> {
    use awaken_provisioning_contract as pc;

    let mounts = receipt.mounts();
    let materializations = receipt.memory_materializations().to_vec();
    if mounts.len() != spec.mounts.len() {
        return Err(pc::SandboxError::invalid(
            "filesystem completion receipt does not cover the frozen mount set",
        ));
    }

    let mut expected_materializations = 0_usize;
    for (required, realized) in spec.mounts.iter().zip(&mounts) {
        if required.mount_id != realized.mount_id
            || required.mount_path != realized.mount_path
            || required.access != realized.access
        {
            return Err(pc::SandboxError::conflict(
                "filesystem completion receipt mount identity or access drifted from the frozen spec",
            ));
        }
        match &required.source {
            pc::MountSource::MemoryStore {
                store_id,
                write_consistency,
                ..
            } => {
                if realized.content_hash.is_some()
                    || !matches!(
                        realized.realization,
                        pc::Realization::Copy | pc::Realization::Fuse
                    )
                    || (*write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
                        && realized.realization != pc::Realization::Fuse)
                {
                    return Err(pc::SandboxError::invalid(
                        "filesystem completion receipt has an invalid Memory realization",
                    ));
                }
                let evidence_count = materializations
                    .iter()
                    .filter(|evidence| {
                        evidence.store_id == *store_id && evidence.mount_path == required.mount_path
                    })
                    .count();
                match realized.realization {
                    pc::Realization::Copy if evidence_count == 1 => {
                        expected_materializations += 1;
                    }
                    pc::Realization::Fuse if evidence_count == 0 => {}
                    _ => {
                        return Err(pc::SandboxError::invalid(
                            "filesystem completion receipt does not bind exact Memory materialization evidence",
                        ));
                    }
                }
            }
            _ if realized.realization == ordinary_realization => {
                if materializations
                    .iter()
                    .any(|evidence| evidence.mount_path == required.mount_path)
                {
                    return Err(pc::SandboxError::invalid(
                        "filesystem completion receipt attached Memory evidence to a non-Memory mount",
                    ));
                }
            }
            _ => {
                return Err(pc::SandboxError::invalid(
                    "filesystem completion receipt has a provider-incompatible realization",
                ));
            }
        }
    }
    if expected_materializations != materializations.len() {
        return Err(pc::SandboxError::invalid(
            "filesystem completion receipt contains extra Memory materialization evidence",
        ));
    }
    Ok((mounts, materializations))
}

/// Validate the only Memory shape that a guard-free terminal reconstruction
/// can safely expose. A current handle must carry one canonical copy-base item
/// for every frozen Memory mount and no others. Write-through requires the lost
/// live FUSE guard and therefore always fails closed here.
///
/// This is deliberately pure and runs before marker admission. The Host owns
/// the one recovered-copy CAS reconciliation; the returned evidence is only
/// cached into the reconstructed handle, never converted into a second mount or
/// teardown participant by Local/Namespace providers.
pub(crate) fn terminal_copy_materializations(
    spec: &awaken_provisioning_contract::SandboxSpec,
    handle: Option<&awaken_provisioning_contract::SandboxHandle>,
) -> Result<
    Vec<awaken_provisioning_contract::MemoryMaterializationEvidence>,
    awaken_provisioning_contract::SandboxError,
> {
    use awaken_provisioning_contract as pc;

    let memory_mounts = spec
        .mounts
        .iter()
        .filter_map(|mount| match &mount.source {
            pc::MountSource::MemoryStore {
                store_id,
                write_consistency,
                ..
            } => Some((
                store_id.as_str(),
                mount.mount_path.as_str(),
                *write_consistency,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    if memory_mounts
        .iter()
        .any(|(_, _, consistency)| *consistency == pc::MemoryWriteConsistency::WriteThroughRequired)
    {
        return Err(pc::SandboxError::unsupported(
            "terminal reconstruction cannot recover a write-through Memory mount without its live FUSE guard",
        ));
    }

    let materializations = match handle {
        Some(handle) => handle.memory_materializations()?.unwrap_or(&[]),
        None if memory_mounts.is_empty() => &[],
        None => {
            return Err(pc::SandboxError::invalid(
                "terminal reconstruction of a copy-backed Memory mount requires its exact current handle evidence",
            ));
        }
    };
    if memory_mounts.len() != materializations.len()
        || memory_mounts.iter().any(|(store_id, mount_path, _)| {
            materializations
                .iter()
                .filter(|evidence| {
                    evidence.store_id.as_str() == *store_id
                        && evidence.mount_path.as_str() == *mount_path
                })
                .count()
                != 1
        })
        || materializations.iter().any(|evidence| {
            memory_mounts
                .iter()
                .filter(|(store_id, mount_path, _)| {
                    evidence.store_id.as_str() == *store_id
                        && evidence.mount_path.as_str() == *mount_path
                })
                .count()
                != 1
        })
    {
        return Err(pc::SandboxError::invalid(
            "terminal Memory materialization evidence is missing, extra, or ambiguous for the frozen Sandbox spec",
        ));
    }
    Ok(materializations.to_vec())
}

pub(crate) async fn teardown_memory_mounts(
    mounts: &[&dyn awaken_provisioning_contract::MemoryMount],
    mut authorize: impl FnMut() -> Result<(), awaken_provisioning_contract::SandboxError>,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let mut failure = None;
    for mount in mounts {
        authorize()?;
        if let Err(error) = mount.teardown().await
            && failure.is_none()
        {
            failure = Some(error);
        }
    }
    // The next iteration checks after each non-final await. This final check
    // also covers the empty set and precedes forgetting any retained guards.
    authorize()?;
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod memory_mount_release_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use awaken_provisioning_contract as pc;

    fn retained(
        mounts: Vec<Box<dyn pc::MemoryMount>>,
    ) -> tokio::sync::Mutex<crate::MemoryMountState> {
        tokio::sync::Mutex::new(mounts.into())
    }

    struct CountedMount {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        fail_first: bool,
        realization: pc::Realization,
    }

    struct ReplayMounter {
        mount_calls: Arc<AtomicUsize>,
        teardown_calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for CountedMount {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl pc::MemoryMount for CountedMount {
        fn realization(&self) -> pc::Realization {
            self.realization
        }

        async fn teardown(&self) -> Result<(), pc::SandboxError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && call == 0 {
                Err(pc::SandboxError::backend("teardown", "retry teardown"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl pc::MemoryMounter for ReplayMounter {
        async fn mount(
            &self,
            _store_id: &str,
            host_path: &std::path::Path,
            _access: pc::MountAccess,
        ) -> pc::MemoryMountOutcome {
            let result: Result<Box<dyn pc::MemoryMount>, pc::SandboxError> = (|| {
                self.mount_calls.fetch_add(1, Ordering::SeqCst);
                std::fs::create_dir_all(host_path)
                    .map_err(|error| pc::SandboxError::backend("mount", error))?;
                Ok(Box::new(CountedMount {
                    calls: self.teardown_calls.clone(),
                    drops: self.drops.clone(),
                    fail_first: true,
                    realization: pc::Realization::Fuse,
                }) as Box<dyn pc::MemoryMount>)
            })();
            pc::MemoryMountOutcome::from_no_effect(result)
        }

        async fn recover_unknown_mount(
            &self,
            _: &str,
            _: &std::path::Path,
            _: pc::MountAccess,
        ) -> Result<(), pc::SandboxError> {
            Ok(())
        }

        async fn reconcile_recovered_copy(
            &self,
            reference: &str,
            evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
            files: &[(String, Vec<u8>)],
            access: awaken_provisioning_contract::MountAccess,
        ) -> Result<(), awaken_provisioning_contract::SandboxError> {
            let _ = (reference, evidence, files, access);
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "recovered Memory copy reconciliation is outside this fixture",
            ))
        }
    }

    #[tokio::test]
    async fn teardown_failure_retains_the_complete_guard_set_until_retry() {
        // Cause/effect table: C1 every teardown succeeds/fails, C2 retry occurs.
        // R1 any failure returns Err and retains *all* guards (including those
        // already reporting Ok); R2 retry re-invokes the same complete set; R3
        // only an all-Ok pass clears it, which is the prerequisite for root and
        // marker removal in both Local and Namespace providers.
        let successful_calls = Arc::new(AtomicUsize::new(0));
        let retry_calls = Arc::new(AtomicUsize::new(0));
        let mounts = retained(vec![
            Box::new(CountedMount {
                calls: successful_calls.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
                fail_first: false,
                realization: pc::Realization::Copy,
            }),
            Box::new(CountedMount {
                calls: retry_calls.clone(),
                drops: Arc::new(AtomicUsize::new(0)),
                fail_first: true,
                realization: pc::Realization::Copy,
            }),
        ]);

        assert!(
            super::release_memory_mounts(&mounts, || Ok(()))
                .await
                .is_err(),
            "R1"
        );
        assert_eq!(mounts.lock().await.len(), 2, "R1");
        super::release_memory_mounts(&mounts, || Ok(()))
            .await
            .expect("R2/R3");
        assert!(mounts.lock().await.is_empty(), "R3");
        assert_eq!(successful_calls.load(Ordering::SeqCst), 2, "R2");
        assert_eq!(retry_calls.load(Ordering::SeqCst), 2, "R2");
    }

    #[tokio::test]
    async fn authorization_denial_retains_empty_or_populated_mount_sets() {
        // C1 set size=0/1/2; C2 authorization denied before mount k or after
        // the final await; C3 all checks succeed. R1 denial at k => only the
        // preceding k mounts were entered, all guards retained, no Drop.
        // R2 all-Ok => each mount once, then complete set Drop. Even an empty
        // set must check authorization before returning a release receipt.
        // Real async expiry/lock waits are covered by the Provider D10-D14
        // matrix; this table isolates every callback branch without sleeps.
        for count in 0..=2 {
            for denied in (0..=count).map(Some).chain(std::iter::once(None)) {
                let calls: Vec<_> = (0..count).map(|_| Arc::new(AtomicUsize::new(0))).collect();
                let drops = Arc::new(AtomicUsize::new(0));
                let mounts = retained(
                    calls
                        .iter()
                        .map(|calls| {
                            Box::new(CountedMount {
                                calls: calls.clone(),
                                drops: drops.clone(),
                                fail_first: false,
                                realization: pc::Realization::Fuse,
                            }) as Box<dyn pc::MemoryMount>
                        })
                        .collect(),
                );
                let mut checks = 0;
                let result = super::release_memory_mounts(&mounts, || {
                    let current = checks;
                    checks += 1;
                    if denied == Some(current) {
                        Err(pc::SandboxError::fence_rejected(
                            "expired callback evidence",
                        ))
                    } else {
                        Ok(())
                    }
                })
                .await;
                assert_eq!(result.is_err(), denied.is_some());
                assert_eq!(checks, denied.unwrap_or(count) + 1);
                for (index, calls) in calls.iter().enumerate() {
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        usize::from(index < denied.unwrap_or(count))
                    );
                }
                assert_eq!(
                    mounts.lock().await.len(),
                    if denied.is_some() { count } else { 0 }
                );
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    if denied.is_some() { 0 } else { count }
                );
            }
        }
    }

    #[tokio::test]
    async fn failed_compensation_survives_wrapper_drop_and_retries_the_same_guard_set() {
        // Temporary-compensation decision table: C1 every teardown is Ok / at
        // least one is Err; E1 all-Ok permits normal guard drop; E2 any Err
        // returns failure and retains the complete set in the Provider. E3
        // dropping the temporary wrapper cannot lose that set; a fresh lookup
        // retries the original guards and all-Ok drops each exactly once.
        let successful_calls = Arc::new(AtomicUsize::new(0));
        let failed_calls = Arc::new(AtomicUsize::new(0));
        let successful_drops = Arc::new(AtomicUsize::new(0));
        let failed_drops = Arc::new(AtomicUsize::new(0));
        let mounts: Vec<Box<dyn pc::MemoryMount>> = vec![
            Box::new(CountedMount {
                calls: successful_calls.clone(),
                drops: successful_drops.clone(),
                fail_first: false,
                realization: pc::Realization::Fuse,
            }),
            Box::new(CountedMount {
                calls: failed_calls.clone(),
                drops: failed_drops.clone(),
                fail_first: true,
                realization: pc::Realization::Fuse,
            }),
        ];

        let root = tempfile::tempdir().unwrap();
        let registry = crate::MemoryMountRegistry::default();
        let owner = registry.owner(root.path());
        *owner.lock().await = mounts.into();
        assert!(
            super::release_memory_mounts(&owner, || Ok(()))
                .await
                .is_err(),
            "E2"
        );
        drop(owner);
        assert_eq!(successful_calls.load(Ordering::SeqCst), 1, "E2 all tried");
        assert_eq!(
            failed_calls.load(Ordering::SeqCst),
            1,
            "E2 failure observed"
        );
        assert_eq!(
            successful_drops.load(Ordering::SeqCst),
            0,
            "E2 full set retained"
        );
        assert_eq!(
            failed_drops.load(Ordering::SeqCst),
            0,
            "E2 full set retained"
        );
        let retry = registry.owner(root.path());
        super::release_memory_mounts(&retry, || Ok(()))
            .await
            .unwrap();
        assert_eq!(
            successful_calls.load(Ordering::SeqCst),
            2,
            "E3 same successful guard"
        );
        assert_eq!(
            failed_calls.load(Ordering::SeqCst),
            2,
            "E3 same failed guard"
        );
        assert_eq!(successful_drops.load(Ordering::SeqCst), 1, "E3");
        assert_eq!(failed_drops.load(Ordering::SeqCst), 1, "E3");
    }

    #[tokio::test]
    async fn replay_revalidates_after_mount_and_retains_failed_compensation() {
        // Replay decision table: C1 the held lifecycle validator is exact
        // before mount and remains exact / changes while the external mount is
        // in flight; C2 compensation succeeds/fails. R1 stable+exact publishes
        // the guard; R2 post-mount drift rejects with zero unauthorized teardown;
        // R3 a newly admitted retry may compensate, but a teardown failure still
        // retains the same guard. This row proves post-await admission is real.
        let mount_calls = Arc::new(AtomicUsize::new(0));
        let teardown_calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mounter: Arc<dyn pc::MemoryMounter> = Arc::new(ReplayMounter {
            mount_calls: mount_calls.clone(),
            teardown_calls: teardown_calls.clone(),
            drops: drops.clone(),
        });
        let mounter = Arc::new(std::sync::RwLock::new(Some(mounter)));
        let requirement = pc::MountRequirement {
            mount_id: "memory".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "store".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/mnt/memory/test".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        };
        let spec = pc::SandboxSpec {
            process_tree_supervision: false,
            scope: "memory-replay-post-validate".into(),
            isolation: pc::IsolationClass::Workdir,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: vec![requirement],
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: Default::default(),
        };
        let realized = vec![pc::RealizedMount {
            mount_id: "memory".into(),
            mount_path: "/mnt/memory/test".into(),
            access: pc::MountAccess::ReadWrite,
            realization: pc::Realization::Fuse,
            content_hash: None,
        }];
        let root = tempfile::tempdir().unwrap();
        let registry = crate::MemoryMountRegistry::default();
        let validations = AtomicUsize::new(0);
        let error = match super::replay_memory_mount_guards(
            &mounter,
            registry.owner(root.path()),
            &spec,
            &realized,
            &[root.path().join("memory")],
            || {
                if validations.fetch_add(1, Ordering::SeqCst) < 2 {
                    Ok(())
                } else {
                    Err(pc::SandboxError::fence_rejected("post-mount fence changed"))
                }
            },
        )
        .await
        {
            Ok(_) => panic!("R2 post-mount drift must fail"),
            Err(error) => error,
        };
        // R2 the fence drift keeps its class; R3 the failed compensation is
        // appended evidence, not a reclassification.
        assert!(
            matches!(error, pc::SandboxError::FenceRejected(_)),
            "R2 {error}"
        );
        assert!(
            error.detail().contains("Memory replay teardown failed"),
            "R3",
        );
        assert_eq!(mount_calls.load(Ordering::SeqCst), 1, "R2");
        assert_eq!(
            teardown_calls.load(Ordering::SeqCst),
            0,
            "R2 no lost-authority teardown"
        );
        let retry = registry.owner(root.path());
        assert!(
            super::release_memory_mounts(&retry, || Ok(()))
                .await
                .is_err(),
            "R3"
        );
        assert_eq!(teardown_calls.load(Ordering::SeqCst), 1, "R3");
        assert_eq!(drops.load(Ordering::SeqCst), 0, "R3 retained");
    }

    #[tokio::test]
    async fn memory_ack_rechecks_authority_after_mount_wait_and_on_replay() {
        // Causes: C1 lock wait vs completed ack replay; C2 current authority
        // live/renewed/revoked/zero/replaced; C3 caller cancelled while waiting.
        // W1 live/renewed => drain Copy once, no teardown, authority guard held.
        // W2 revoked/zero/replaced => deny, no new drain or ack; W3 cancellation
        // => no effect. W4 successful ack followed by revocation => replay must
        // fail, not return cached success. An explicit Pending poll proves the
        // authorization change occurs AFTER request initiation and BEFORE grant.
        use awaken_provisioning_contract::test_support::FenceTamper as _;
        use std::{future::Future, task::Poll, time::Duration};
        for rule in [
            "live",
            "renewed",
            "revoked",
            "zero",
            "replaced",
            "cancelled",
        ] {
            let fence = pc::SandboxEffectFence::new("effect", "owner", "boot", 1, 100).unwrap();
            let authority = pc::test_support::EffectAuthority::new(&fence);
            let evidence =
                vec![pc::MemoryMaterializationEvidence::new("store", "/memory", vec![]).unwrap()];
            let ack = pc::MemoryReconciliationAck::default();
            let drops = Arc::new(AtomicUsize::new(0));
            let teardowns = Arc::new(AtomicUsize::new(0));
            let mounts = retained(vec![Box::new(CountedMount {
                calls: teardowns.clone(),
                drops: drops.clone(),
                fail_first: false,
                realization: pc::Realization::Copy,
            })]);
            let locked = mounts.lock().await;
            let mut pending = Box::pin(super::acknowledge_memory_reconciliation(
                &ack,
                &mounts,
                &fence,
                &authority,
                &evidence,
                || Ok(evidence.clone()),
                |_| {
                    assert!(authority.current.try_lock().is_err(), "W1 held authority");
                    Ok(())
                },
            ));
            std::future::poll_fn(|cx| {
                assert!(pending.as_mut().poll(cx).is_pending(), "W wait");
                Poll::Ready(())
            })
            .await;
            assert_eq!(drops.load(Ordering::SeqCst), 0, "W wait");
            match rule {
                "revoked" => *authority.current.lock().unwrap() = None,
                "zero" => authority.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO,
                "replaced" => {
                    let mut guard = authority.current.lock().unwrap();
                    guard.as_mut().unwrap().0.reassign();
                }
                "renewed" => {
                    let mut guard = authority.current.lock().unwrap();
                    guard
                        .as_mut()
                        .unwrap()
                        .0
                        .tamper(|w| w.expires_at_unix_ms += 100);
                }
                "cancelled" => {
                    drop(pending);
                    drop(locked);
                    assert_eq!(drops.load(Ordering::SeqCst), 0, "W3");
                    continue;
                }
                "live" => {}
                _ => unreachable!(),
            }
            drop(locked);
            let allowed = matches!(rule, "live" | "renewed");
            assert_eq!(pending.await.is_ok(), allowed, "W1/W2 {rule}");
            assert_eq!(
                drops.load(Ordering::SeqCst),
                usize::from(allowed),
                "W1/W2 {rule}"
            );
            assert_eq!(teardowns.load(Ordering::SeqCst), 0, "W1/W2");
            if allowed {
                *authority.current.lock().unwrap() = None;
                assert!(
                    super::acknowledge_memory_reconciliation(
                        &ack,
                        &mounts,
                        &fence,
                        &authority,
                        &evidence,
                        || Ok(evidence.clone()),
                        |_| panic!("W4 cached acknowledgement cannot bypass revocation"),
                    )
                    .await
                    .is_err(),
                    "W4"
                );
            }
        }
    }

    #[tokio::test]
    async fn terminal_ack_is_exact_effect_scoped_and_retires_only_copy_guards() {
        // Terminal-memory acknowledgement decision table. Causes: C1 supplied
        // evidence equals the complete canonical provider evidence (including
        // order); C2 effect-bound current proof is live; C3 this process recorded no ack / the same
        // effect / a same-lease takeover / a foreign lease; C4 guard is
        // Copy/FUSE. Rules: A1 !C1 or
        // !C2 => reject before authorization or guard mutation; A2 C1+C2+no ack
        // => authorize once, drop every Copy without teardown, retain FUSE, and
        // record the full effect identity; A3 same-effect replay => success with
        // current authority rechecked but no repeated marker mutation/drop; A4 same-lease takeover reauthorizes,
        // drains idempotently, and rebinds the ack; A5 foreign lease rejects
        // with the retained FUSE and ack unchanged. Disposal gating admits only
        // the latest effect recorded by A2/A4.
        let first = pc::MemoryMaterializationEvidence::new(
            "store-a",
            "/a",
            vec![pc::MemoryMaterializationHead {
                path: "value".into(),
                id: "head-a".into(),
                version: 1,
                content_sha256: "digest-a".into(),
            }],
        )
        .unwrap();
        let second = pc::MemoryMaterializationEvidence::new(
            "store-b",
            "/b",
            vec![pc::MemoryMaterializationHead {
                path: "value".into(),
                id: "head-b".into(),
                version: 1,
                content_sha256: "digest-b".into(),
            }],
        )
        .unwrap();
        let expected = vec![first.clone(), second.clone()];
        let copy_drops = Arc::new(AtomicUsize::new(0));
        let fuse_drops = Arc::new(AtomicUsize::new(0));
        let teardown_calls = Arc::new(AtomicUsize::new(0));
        let mounts = retained(vec![
            Box::new(CountedMount {
                calls: teardown_calls.clone(),
                drops: copy_drops.clone(),
                fail_first: false,
                realization: pc::Realization::Copy,
            }),
            Box::new(CountedMount {
                calls: teardown_calls.clone(),
                drops: fuse_drops.clone(),
                fail_first: false,
                realization: pc::Realization::Fuse,
            }),
            Box::new(CountedMount {
                calls: teardown_calls.clone(),
                drops: copy_drops.clone(),
                fail_first: false,
                realization: pc::Realization::Copy,
            }),
        ]);
        let ack = pc::MemoryReconciliationAck::default();
        let live =
            pc::SandboxEffectFence::new("terminal", "owner", "runtime", 2, u64::MAX).unwrap();
        let expired = pc::SandboxEffectFence::new("terminal", "owner", "runtime", 2, 0).unwrap();
        let takeover =
            pc::SandboxEffectFence::new("takeover", "owner", "runtime", 2, u64::MAX).unwrap();
        let foreign =
            pc::SandboxEffectFence::new("foreign", "other-owner", "runtime", 2, u64::MAX).unwrap();
        let authorization_calls = Arc::new(AtomicUsize::new(0));

        let reversed = vec![second, first];
        assert!(
            super::acknowledge_memory_reconciliation(
                &ack,
                &mounts,
                &live,
                &pc::test_support::EffectAuthority::new(&live),
                &reversed,
                || Ok(expected.clone()),
                |_| {
                    authorization_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .is_err(),
            "A1 order mismatch"
        );
        assert!(
            super::acknowledge_memory_reconciliation(
                &ack,
                &mounts,
                &expired,
                &pc::test_support::EffectAuthority::denied(),
                &expected,
                || Ok(expected.clone()),
                |_| {
                    authorization_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .is_err(),
            "A1 expired"
        );
        assert_eq!(authorization_calls.load(Ordering::SeqCst), 0, "A1");
        assert_eq!(mounts.lock().await.len(), 3, "A1");
        assert_eq!(copy_drops.load(Ordering::SeqCst), 0, "A1");

        super::acknowledge_memory_reconciliation(
            &ack,
            &mounts,
            &live,
            &pc::test_support::EffectAuthority::new(&live),
            &expected,
            || Ok(expected.clone()),
            |_| {
                authorization_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("A2");
        assert_eq!(authorization_calls.load(Ordering::SeqCst), 1, "A2");
        assert_eq!(copy_drops.load(Ordering::SeqCst), 2, "A2");
        assert_eq!(teardown_calls.load(Ordering::SeqCst), 0, "A2");
        assert_eq!(mounts.lock().await.len(), 1, "A2 retains FUSE");

        super::acknowledge_memory_reconciliation(
            &ack,
            &mounts,
            &live,
            &pc::test_support::EffectAuthority::new(&live),
            &expected,
            || Ok(expected.clone()),
            |_| {
                authorization_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("A3");
        super::acknowledge_memory_reconciliation(
            &ack,
            &mounts,
            &takeover,
            &pc::test_support::EffectAuthority::new(&takeover),
            &expected,
            || Ok(expected.clone()),
            |_| {
                authorization_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("A4");
        assert_eq!(authorization_calls.load(Ordering::SeqCst), 2, "A3/A4");
        assert_eq!(mounts.lock().await.len(), 1, "A3/A4");
        assert_eq!(fuse_drops.load(Ordering::SeqCst), 0, "A3/A4");
        assert!(
            ack.require_for_disposal(
                &expected,
                &live,
                &pc::test_support::EffectAuthority::new(&live),
                |_| Ok(())
            )
            .is_err(),
            "A4 supersedes prior effect"
        );
        ack.require_for_disposal(
            &expected,
            &takeover,
            &pc::test_support::EffectAuthority::new(&takeover),
            |_| Ok(()),
        )
        .expect("A4 disposal gate");
        assert!(
            super::acknowledge_memory_reconciliation(
                &ack,
                &mounts,
                &foreign,
                &pc::test_support::EffectAuthority::new(&foreign),
                &expected,
                || Ok(expected.clone()),
                |_| {
                    authorization_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .is_err(),
            "A5"
        );
        assert_eq!(authorization_calls.load(Ordering::SeqCst), 2, "A5");
    }
}

/// Compensate failed materialization or Ready publication through the same
/// provider-owned participants. The caller retains the creation/root guard and
/// the Memory owner lock throughout; this function adds no lifecycle state.
/// Ready response-loss replay never shreds or rolls back the completed root.
pub(crate) async fn compensate_creation_failure(
    guard: realization_marker::ProviderCreationGuard<'_>,
    memory: &mut MemoryMountState,
    root: &IsolatedRoot,
    secret_paths: &[PathBuf],
    phase: &str,
    cause: awaken_provisioning_contract::SandboxError,
) -> awaken_provisioning_contract::SandboxError {
    let teardown = memory.release(|| guard.validate_before_mutation()).await;
    let shredding = if guard.is_incomplete() {
        let identity = match guard.root_identity() {
            Ok(identity) => identity,
            Err(error) => return error,
        };
        guard.with_mutation(|| shred_secret_paths_at(root, identity, secret_paths))
    } else {
        Ok(())
    };
    if teardown.is_err() || shredding.is_err() {
        // Retaining the root is a statement, not silence. Say so durably before
        // returning, so the next attempt at this scope refuses the evidence
        // this path just chose to preserve instead of clearing it as an
        // abandonment. A fenced guard already says it with its marker phase.
        let retention = guard.retain_failed_compensation();
        return cause.map_detail(|detail| format!("{phase} failed: {detail}; memory teardown: {}; secret shredding: {}; exact root evidence was retained{}",
            teardown
                .as_ref()
                .err()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
            shredding
                .as_ref()
                .err()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
            retention
                .err()
                .map_or_else(String::new, |error| format!("; retention record: {error}")),
        ));
    }
    if let Err(cleanup) = guard.abort_creation() {
        return cause.map_detail(|detail| {
            format!("{phase} failed: {detail}; exact-owned cleanup failed: {cleanup}")
        });
    }
    cause
}

/// Tear down an all-or-retry set. No guard is cleared until every teardown
/// succeeds, so a partial transport/unmount failure remains retryable and can
/// never be converted into successful root deletion.
pub(crate) async fn release_memory_mounts(
    mounts: &tokio::sync::Mutex<MemoryMountState>,
    authorize: impl FnMut() -> Result<(), awaken_provisioning_contract::SandboxError>,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let mut retained = mounts.lock().await;
    retained.release(authorize).await
}

/// Serialize the provider's complete Copy-guard snapshot with the shared
/// acknowledgement kernel. The snapshot callback runs while the async guard
/// list is locked, so Namespace dynamic attachment can publish its guard and
/// evidence atomically in the same lock order.
pub(crate) async fn acknowledge_memory_reconciliation(
    acknowledgement: &awaken_provisioning_contract::MemoryReconciliationAck,
    mounts: &tokio::sync::Mutex<MemoryMountState>,
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
    authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    supplied_materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
    expected_materializations: impl FnOnce() -> Result<
        Vec<awaken_provisioning_contract::MemoryMaterializationEvidence>,
        awaken_provisioning_contract::SandboxError,
    >,
    authorize: impl FnOnce(
        &awaken_provisioning_contract::SandboxEffectGuard<'_>,
    ) -> Result<(), awaken_provisioning_contract::SandboxError>,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let mut retained = mounts.lock().await;
    let expected_materializations = expected_materializations()?;
    acknowledgement.acknowledge(
        effect_fence,
        authority,
        &expected_materializations,
        supplied_materializations,
        |admitted| {
            authorize(admitted)?;
            retained.acknowledge_copy_reconciliation()
        },
    )
}

pub(crate) use provider::terminal_disposal::{
    authorize_terminal_disposal, dispose_terminal_realization, prepare_terminal_disposal,
};

/// Shred provider-selected credential paths through the one exact-root leaf.
/// `None` is valid only for a marker-free legacy admission whose root is still
/// absent; it never authorizes deriving an identity from an occupied pathname.
pub(crate) fn shred_secret_paths_at(
    root: &IsolatedRoot,
    root_identity: Option<awaken_sandbox_fs::DirectoryIdentity>,
    paths: &[PathBuf],
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let Some(root_identity) = root_identity else {
        return match awaken_sandbox_fs::classify_nofollow(root.root()).map_err(|error| {
            awaken_provisioning_contract::SandboxError::backend("shred_secret_paths_at", error)
        })? {
            awaken_sandbox_fs::PathEntry::Absent => Ok(()),
            _ => Err(awaken_provisioning_contract::SandboxError::conflict(
                "marker-free secret shredding found an occupied sandbox root",
            )),
        };
    };
    for path in paths {
        let relative = path.strip_prefix(root.root()).map_err(|_| {
            awaken_provisioning_contract::SandboxError::invalid(format!(
                "secret path `{}` is outside sandbox root `{}`",
                path.display(),
                root.root().display()
            ))
        })?;
        awaken_sandbox_fs::zero_relative_regular_file_nofollow(
            root.root(),
            root_identity,
            relative,
        )
        .map_err(|error| {
            awaken_provisioning_contract::SandboxError::backend("shred_secret_paths_at", error)
        })?;
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct RuntimePathEnv {
    project_dir: String,
    outputs_dir: String,
}

impl RuntimePathEnv {
    pub(crate) fn new(project_dir: impl Into<String>, outputs_dir: impl Into<String>) -> Self {
        Self {
            project_dir: project_dir.into(),
            outputs_dir: outputs_dir.into(),
        }
    }

    pub(crate) fn apply(&self, command: &mut tokio::process::Command) {
        command.envs(self.entries());
    }

    fn entries(&self) -> [(&str, &str); 2] {
        [
            ("AWAKEN_PROJECT_DIR", &self.project_dir),
            ("AWAKEN_OUTPUTS_DIR", &self.outputs_dir),
        ]
    }

    fn bash_env(&self) -> std::collections::BTreeMap<String, String> {
        let mut env = std::env::vars()
            .filter(|(key, _)| !key.starts_with("ANTHROPIC_"))
            .collect::<std::collections::BTreeMap<_, _>>();
        env.extend(
            self.entries()
                .map(|(name, value)| (name.into(), value.into())),
        );
        env
    }
}

pub(crate) fn sandbox_dir(base: &Path, id: &str) -> PathBuf {
    // A WorkUnit id is a logical identity, not a portable filesystem component.
    // In particular Flow ids contain `:`; although Linux accepts that byte in a
    // filename, Rust/Cargo and pkg-config use colon-delimited path lists and can
    // no longer compile from such a root. Preserve short portable ids for
    // operator readability and map every other identity to one deterministic,
    // collision-resistant component.
    let invalid = id.is_empty()
        || matches!(id, "." | "..")
        || id.len() > 96
        || id.ends_with([' ', '.'])
        || id.chars().any(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '_')
        });
    let reserved = awaken_sandbox_fs::is_reserved_component(id);
    if !invalid && !reserved {
        return base.join(id);
    }
    base.join(format!("scope-{}", blake3::hash(id.as_bytes()).to_hex()))
}

/// The `awaken-provisioning-contract` seam realized locally (ADR-0041).
mod abandoned_realization;
mod artifacts;
mod auxiliary;
mod blob_cache;
mod git_transport;
mod handle;
mod namespace;
mod provider;
#[cfg(target_os = "linux")]
pub use provider::execution::LocalExecutionDeployment;
mod read_only_tree;
mod realization_marker;
mod repo_bundle;
#[cfg(target_os = "linux")]
pub mod repository_effect;
// The provider resolves mount bytes from an injected [`pc::BlobSource`] port
// (ADR-0038 D6, dependency-inverted) — this worker-tier crate links no durable
// store; the composition root adapts the content-addressed store to the port.
pub use abandoned_realization::AbandonedRealizations;
pub use auxiliary::LocalAuxiliarySandboxes;
pub use awaken_local_process::LocalProcess;
pub use blob_cache::{BlobLru, WorkspaceBlobCache};
pub(crate) use git_transport::{git_bytes, provision_repo_at, push_repo_to_at, run_git};
pub use handle::{
    FilesystemHandleCodec, LocalSandboxHandleV1, LocalSandboxHandleV2, NamespaceProviderKind,
    NamespaceSandboxHandleV1, NamespaceSandboxHandleV2,
};
pub use namespace::{NamespaceProvider, NamespaceSandbox, bubblewrap_argv, sandbox_exec_argv};
pub use provider::{LocalProvider, LocalSandbox};
pub use repo_bundle::{clone_repo_bundle, push_repo_bundle};

/// A logical path escaped its environment root.
#[derive(Debug, thiserror::Error)]
#[error("path {0:?} escapes the sandbox root")]
pub struct EscapeError(pub String);

/// A path jail. Every logical path a tool names is resolved *under* the root;
/// `..` that would climb above the root, and absolute paths, fail closed. Resolution
/// is lexical (no symlink following), so a path need not exist yet (for writes).
#[derive(Debug, Clone)]
pub struct IsolatedRoot {
    root: PathBuf,
}

impl IsolatedRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `logical` to an absolute path under the root, or reject an escape.
    /// A leading `/` is treated as root-relative (rebased under the jail), never
    /// as the host filesystem root.
    pub fn resolve(&self, logical: &str) -> Result<PathBuf, EscapeError> {
        let rebased = logical.trim_start_matches('/');
        let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
        for component in Path::new(rebased).components() {
            match component {
                Component::Normal(part) => stack.push(part),
                Component::ParentDir => {
                    if stack.pop().is_none() {
                        return Err(EscapeError(logical.to_string()));
                    }
                }
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            }
        }
        let mut out = self.root.clone();
        for part in stack {
            out.push(part);
        }
        Ok(out)
    }
}

/// Build the trusted launcher argv for one persistent Bash process in a
/// networkless bubblewrap namespace rooted at the environment workdir.
fn bwrap_persistent_bash(root: &str) -> Vec<String> {
    [
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--bind",
        root,
        root,
        "--chdir",
        root,
        "--unshare-net",
        "--",
        "/bin/bash",
        "--noprofile",
        "--norc",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// A hand tool bound to a sandbox environment. Unlike a [`RawTool`], its result is
/// content-or-error **only** — [`HandOutput`] has no state field, so an environment
/// tool executes side effects (filesystem, process) but can never author runtime
/// state (G13): the runtime stays the sole author of its own state. Whether the
/// tool runs in-process (rooted) or relays into a container/remote root, this makes
/// the boundary invariant *unrepresentable*, not merely conventional.
#[async_trait]
pub trait HandTool: Send + Sync {
    /// The tool id, matching the model-visible descriptor.
    fn id(&self) -> &str;
    /// Execute the call, returning content or a tool-level error — never state.
    async fn run(&self, call: ToolCall) -> Result<HandOutput, ToolError>;
}

/// The result of a [`HandTool`]: content or a tool-level error, with no runtime
/// state (G13). This is the shape that structurally forbids an environment tool
/// from mutating runtime state.
#[derive(Debug, Clone)]
pub struct HandOutput {
    /// The tool's structured model-visible result.
    pub content: Vec<ContentBlock>,
    /// Whether this is a tool-level error (model-visible; the run continues).
    pub is_error: bool,
}

impl HandOutput {
    /// A successful result carrying `content`.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(content)],
            is_error: false,
        }
    }

    /// A tool-level error carrying `content` (model-visible; the run continues).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(content)],
            is_error: true,
        }
    }

    /// Derived text for diagnostics and text-only relay assertions.
    #[must_use]
    pub fn text(&self) -> String {
        awaken_runtime_contract::extract_text(&self.content)
    }
}

/// Narrow the canonical Hand tools to the environment's content-only boundary.
/// Path resolution belongs to HandToolContext; this adapter never rewrites args.
struct StateFreeHand(Arc<dyn RawTool>);

#[async_trait]
impl HandTool for StateFreeHand {
    fn id(&self) -> &str {
        self.0.id()
    }

    async fn run(&self, call: ToolCall) -> Result<HandOutput, ToolError> {
        let out = self.0.invoke(call).await?;
        Ok(HandOutput {
            content: out.content,
            is_error: out.is_error,
        })
    }
}

/// Adapts a state-less [`HandTool`] into the runtime's `RawTool`. This is the single
/// place the two shapes meet, and the produced [`ToolOutput`] always carries empty
/// state (G13): state cannot cross the environment boundary.
struct HandToolAsRaw(Arc<dyn HandTool>);

#[async_trait]
impl RawTool for HandToolAsRaw {
    fn id(&self) -> &str {
        self.0.id()
    }

    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Sandbox
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let call_id = call.call_id.clone();
        let out = self.0.run(call).await?;
        Ok(if out.is_error {
            ToolOutput::error_blocks(call_id, out.content)
        } else {
            ToolOutput::ok_blocks(call_id, out.content)
        })
    }
}

/// Adapt a state-less [`HandTool`] into a runtime `RawTool` (empty state, G13).
fn hand_tool_as_raw(tool: Arc<dyn HandTool>) -> Arc<dyn RawTool> {
    Arc::new(HandToolAsRaw(tool))
}

/// The built-in hand tools, each jailed to `root`. Internal: the local provider's
/// way to bind [`HandTool`]s to an environment; a distributed provider builds its
/// own relay `HandTool`s instead.
pub(crate) fn rooted_hand_tools(
    root: IsolatedRoot,
    host_outputs: PathBuf,
    runtime_paths: RuntimePathEnv,
    deny_egress: bool,
) -> Vec<Arc<dyn HandTool>> {
    let mut context = HandToolContext::new(root.root())
        .with_path_projection("/workspace", root.root().join("workspace"))
        .with_path_projection("/mnt", root.root().join("mnt"))
        .with_path_projection("/outputs", &host_outputs)
        .with_path_projection("outputs", &host_outputs)
        .with_bash_env(runtime_paths.bash_env());
    if deny_egress {
        context = context.with_bash_launcher(
            "bwrap",
            bwrap_persistent_bash(&root.root().to_string_lossy()),
        );
    }
    all_hand_tools_in(context)
        .into_iter()
        .map(|inner| Arc::new(StateFreeHand(inner)) as Arc<dyn HandTool>)
        .collect()
}

/// Resolve a logical path under a realized `root`, fail-closed on escape (G3). The
/// free-function form of the jail the repo helpers share; `.`/`..` segments and an
/// empty path are rejected so a mount never lands outside the sandbox root.
pub(crate) fn jailed_at(root: &IsolatedRoot, logical: &str) -> Result<PathBuf, SandboxError> {
    let logical = logical.trim_start_matches('/');
    if logical.is_empty() || logical.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(SandboxError::invalid(format!(
            "unsafe repo mount path `{logical}`"
        )));
    }
    Ok(root.root().join(logical))
}

/// List regular files under `<root>/<subdir>` (recursively) as `(logical_path, bytes)`
/// sorted by path — a session's output artifacts / memory harvest. Paths are logical
/// (never a host path, G3). Shared with the Workdir tier.
pub(crate) fn list_files_at(
    root: &IsolatedRoot,
    root_identity: awaken_sandbox_fs::DirectoryIdentity,
    subdir: &str,
) -> Result<Vec<(String, Vec<u8>)>, SandboxError> {
    awaken_sandbox_fs::read_regular_tree_nofollow(
        root.root(),
        root_identity,
        std::path::Path::new(subdir),
    )
    .map_err(|error| SandboxError::backend("list_files_at", error))?
    .into_iter()
    .map(|file| {
        let relative = file.relative_path.to_str().ok_or_else(|| {
            SandboxError::invalid("sandbox tree contains a non-UTF-8 logical path")
        })?;
        Ok((relative.replace('\\', "/"), file.bytes))
    })
    .collect()
}

/// Scan `<root>/<subdir>/*/SKILL.md` **live** and return neutral file data (the host
/// parses the skill model). A missing directory is empty; every other tree or
/// UTF-8 fault is surfaced. Directories without `SKILL.md` remain neutral and
/// are skipped. `tree_subdir` selects the directory below the exact sandbox
/// root, while `logical_subdir` owns the caller-visible `"<subdir>/<id>"` path.
pub(crate) fn scan_skill_dir_at(
    root: &IsolatedRoot,
    root_identity: awaken_sandbox_fs::DirectoryIdentity,
    tree_subdir: &str,
    logical_subdir: &str,
) -> Result<Vec<DiscoveredSkillFile>, SandboxError> {
    let mut out = Vec::new();
    for file in awaken_sandbox_fs::read_regular_tree_nofollow(
        root.root(),
        root_identity,
        std::path::Path::new(tree_subdir),
    )
    .map_err(|error| SandboxError::backend("scan_skill_dir_at", error))?
    {
        let mut components = file.relative_path.components();
        let Some(std::path::Component::Normal(id)) = components.next() else {
            continue;
        };
        let Some(std::path::Component::Normal(name)) = components.next() else {
            continue;
        };
        if components.next().is_some() || name != "SKILL.md" {
            continue;
        }
        let id = id
            .to_str()
            .ok_or_else(|| SandboxError::invalid("Skill directory id is not UTF-8"))?
            .to_owned();
        let content = String::from_utf8(file.bytes).map_err(|error| {
            SandboxError::invalid(format!("Skill `{id}` is not UTF-8: {error}"))
        })?;
        out.push(DiscoveredSkillFile {
            id: id.clone(),
            content,
            dir: format!("{}/{id}", logical_subdir.trim_end_matches('/')),
        });
    }
    out.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(out)
}

/// Build the rooted in-process tools for a Workdir-tier root as `RawTool`s ready for
/// `Runtime::with_tool` — the full capability surface the host composes (ADR-0035 D8),
/// path-jailed to `root` with egress optionally denied. The pc-model counterpart of
/// [`Environment::tools`]; the kernel sees a uniform `RawTool` set with no mount concept.
pub(crate) fn rooted_raw_tools(
    root: IsolatedRoot,
    host_outputs: PathBuf,
    runtime_paths: RuntimePathEnv,
    deny_egress: bool,
) -> Vec<Arc<dyn RawTool>> {
    rooted_hand_tools(root, host_outputs, runtime_paths, deny_egress)
        .into_iter()
        .map(hand_tool_as_raw)
        .collect()
}

/// Canonical Resource content identity used to verify provisioning bytes.
pub use awaken_resource_contract::content_id as content_fingerprint;

/// A `SKILL.md`-bearing directory discovered under the environment; the
/// Session environment contract owns the value (G3: `dir` is logical).
pub use awaken_session_environment_contract::DiscoveredSkillFile;

/// Local helpers report the one provisioning error class set; there is no
/// second provider-private string error to lose a class through conversion.
pub(crate) use awaken_provisioning_contract::SandboxError;

impl From<EscapeError> for SandboxError {
    /// A logical path that climbs out of its jail is a malformed request.
    fn from(error: EscapeError) -> Self {
        Self::invalid(error.to_string())
    }
}

#[cfg(test)]
pub(crate) fn test_disposal_authorization(
    prepared: &awaken_provisioning_contract::SandboxEffectFence,
) -> awaken_provisioning_contract::SandboxDisposalAuthorization {
    test_disposal_authorization_for_current(prepared, prepared)
}

#[cfg(test)]
pub(crate) fn test_disposal_authorization_for_current(
    prepared: &awaken_provisioning_contract::SandboxEffectFence,
    current: &awaken_provisioning_contract::SandboxEffectFence,
) -> awaken_provisioning_contract::SandboxDisposalAuthorization {
    use awaken_provisioning_contract as pc;

    let fingerprint = format!("local-test-preparation:{}", prepared.operation_id());
    let preparation = pc::SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
    let operation_id = preparation.operation_id().unwrap();
    let successor = pc::SandboxEffectFence::new(
        operation_id,
        current.owner(),
        current.runtime_incarnation(),
        current.epoch(),
        current.expires_at_unix_ms(),
    )
    .unwrap();
    preparation.authorize(successor).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_outputs() -> &'static Path {
        Path::new("/env/mnt/session/outputs")
    }

    #[test]
    fn content_fingerprint_is_blake3_and_matches_file_store() {
        // Cause/effect graph: C1 identical bytes enter the Resource and Sandbox
        // paths; C2 the shared contract selects BLAKE3. Effects: E1 both paths
        // produce one stable identity; E2 the legacy 16-hex hash cannot recur.
        // Decision rule H1: C1+C2 -> E1+E2 (64-hex canonical digest).
        let bytes = b"provisioned bytes";
        assert_eq!(
            content_fingerprint(bytes),
            awaken_resource_contract::content_id(bytes)
        );
        assert_eq!(content_fingerprint(bytes).len(), 64);
    }

    #[test]
    fn resolve_stays_under_root() {
        let root = IsolatedRoot::new("/env");
        assert_eq!(
            root.resolve("a/b.txt").unwrap(),
            PathBuf::from("/env/a/b.txt")
        );
        // absolute is rebased, not the host root
        assert_eq!(
            root.resolve("/etc/passwd").unwrap(),
            PathBuf::from("/env/etc/passwd")
        );
        // interior `..` that stays under root is fine
        assert_eq!(root.resolve("a/../b").unwrap(), PathBuf::from("/env/b"));
    }

    #[test]
    fn escapes_fail_closed() {
        let root = IsolatedRoot::new("/env");
        assert!(root.resolve("../secret").is_err());
        assert!(root.resolve("a/../../secret").is_err());
        assert!(root.resolve("..").is_err());
    }

    #[test]
    fn resolve_handles_curdir_and_empty() {
        let root = IsolatedRoot::new("/env");
        assert_eq!(root.resolve("./a").unwrap(), PathBuf::from("/env/a"));
        assert_eq!(root.resolve("").unwrap(), PathBuf::from("/env"));
    }

    // ---- helpers ----

    fn call(tool_id: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".into(),
            tool_id: tool_id.into(),
            arguments: args,
        }
    }

    /// A custom `HandTool` — the shape an external / relay provider builds.
    struct CustomHand {
        id: String,
        fail: bool,
    }

    #[async_trait]
    impl HandTool for CustomHand {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(&self, _c: ToolCall) -> Result<HandOutput, ToolError> {
            Ok(if self.fail {
                HandOutput::error("boom")
            } else {
                HandOutput::ok("done")
            })
        }
    }

    // ---- portable roots and canonical Hand path projection ----

    #[test]
    fn sandbox_directories_escape_nonportable_ids_deterministically() {
        let base = Path::new("/awaken");
        assert_eq!(sandbox_dir(base, "valid.scope"), base.join("valid.scope"));

        let reserved = sandbox_dir(base, "CON");
        assert_ne!(reserved, base.join("CON"));
        assert!(
            !reserved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with('.')
        );

        let trailing_dot = sandbox_dir(base, "session.");
        assert_ne!(trailing_dot, base.join("session."));
        assert!(
            !trailing_dot
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with('.')
        );

        // Flow WorkUnit ids contain colons. Keeping those bytes in a local
        // sandbox root breaks colon-delimited compiler and pkg-config paths even
        // on Unix, so the same logical id must always resolve to one short hash.
        let work_unit = "state-entry:issue-1:deliver:3";
        let hashed = sandbox_dir(base, work_unit);
        assert_eq!(hashed, sandbox_dir(base, work_unit));
        assert_ne!(hashed, base.join(work_unit));
        let component = hashed.file_name().unwrap().to_string_lossy();
        assert!(component.starts_with("scope-"));
        assert_eq!(component.len(), "scope-".len() + 64);
        assert_ne!(hashed, sandbox_dir(base, "state-entry:issue-2:deliver:3"));
    }

    #[tokio::test]
    async fn local_paths_use_the_canonical_hand_projection_and_confinement() {
        // Causes: logical root={workspace,mnt,outputs}, alias={absolute,relative},
        // normalization={ordinary,parent,escape}, host path={inside,outside}.
        // Effects: W1 logical workspace/mnt map once; W2 both output aliases
        // use the ONE configured backing directory; W3 normalized outputs/..
        // stays in workspace, not output parent; W4 escape/outside rejects.
        // Read/write/edit/glob/grep must share FileContext, not per-tool JSON
        // rewriting. Existing Bash state test owns persistent-shell behavior.
        let root = tempfile::tempdir().unwrap();
        let output_parent = tempfile::tempdir().unwrap();
        let outputs = output_parent.path().join("canonical outputs");
        std::fs::create_dir(&outputs).unwrap();
        let tools = rooted_raw_tools(
            IsolatedRoot::new(root.path()),
            outputs.clone(),
            RuntimePathEnv::new(root.path().to_string_lossy(), outputs.to_string_lossy()),
            false,
        );
        let find = |id: &str| tools.iter().find(|tool| tool.id() == id).unwrap();
        for (logical, physical) in [
            (
                "/workspace/note.txt",
                root.path().join("workspace/note.txt"),
            ),
            ("/mnt/input.txt", root.path().join("mnt/input.txt")),
            ("outputs/a.txt", outputs.join("a.txt")),
            ("/outputs/b.txt", outputs.join("b.txt")),
            ("outputs/../note.txt", root.path().join("note.txt")),
        ] {
            find("write")
                .invoke(call(
                    "write",
                    serde_json::json!({"path": logical, "content":"before"}),
                ))
                .await
                .unwrap();
            assert_eq!(std::fs::read(&physical).unwrap(), b"before", "{logical}");
            find("edit").invoke(call("edit", serde_json::json!({"file_path": logical, "old_string":"before", "new_string":"after"}))).await.unwrap();
            let read = find("read")
                .invoke(call("read", serde_json::json!({"path": logical})))
                .await
                .unwrap();
            assert!(read.text().contains("after"), "{logical}");
            assert_eq!(std::fs::read(&physical).unwrap(), b"after");
        }
        for bad in ["../escape", "outputs/../../escape"] {
            assert!(
                find("write")
                    .invoke(call(
                        "write",
                        serde_json::json!({"path":bad,"content":"forbidden"})
                    ))
                    .await
                    .is_err()
            );
        }
        let secret = output_parent.path().join("outside.txt");
        std::fs::write(&secret, b"outside").unwrap();
        assert!(
            find("read")
                .invoke(call("read", serde_json::json!({"path":secret})))
                .await
                .is_err()
        );
        let glob = find("glob")
            .invoke(call(
                "glob",
                serde_json::json!({"pattern":"/workspace/*.txt"}),
            ))
            .await
            .unwrap();
        assert!(glob.text().contains("/workspace/note.txt"));
        assert!(
            !glob
                .text()
                .contains(&root.path().to_string_lossy().to_string())
        );
        let output_glob = find("glob")
            .invoke(call("glob", serde_json::json!({"pattern":"outputs/*.txt"})))
            .await
            .unwrap();
        assert!(output_glob.text().contains("a.txt"));
        assert!(output_glob.text().contains("b.txt"));
        assert!(
            !output_glob
                .text()
                .contains(&output_parent.path().to_string_lossy().to_string())
        );
        let grep = find("grep")
            .invoke(call(
                "grep",
                serde_json::json!({"path":"/mnt","pattern":"after"}),
            ))
            .await
            .unwrap();
        assert!(grep.text().contains("input.txt"));
    }

    #[test]
    fn persistent_bwrap_launcher_has_no_network_and_exact_root() {
        // The root is passed as an argv token, not interpolated into shell text,
        // while the shell process itself lives in the no-network namespace.
        let args = bwrap_persistent_bash("/env/a'b");
        assert!(args.iter().any(|arg| arg == "--unshare-net"));
        assert!(
            args.windows(3)
                .any(|part| part == ["--bind", "/env/a'b", "/env/a'b"])
        );
        assert!(args.ends_with(&[
            "--".to_owned(),
            "/bin/bash".to_owned(),
            "--noprofile".to_owned(),
            "--norc".to_owned(),
        ]));
    }

    // ---- HandOutput ----

    #[test]
    fn hand_output_constructors() {
        let ok = HandOutput::ok("a");
        assert_eq!(ok.text(), "a");
        assert!(!ok.is_error);
        assert!(HandOutput::error("b").is_error);
    }

    // ---- the environment boundary carries no runtime state (G13) ----

    // Causes: C1 a sandbox-bound HandTool crosses the sole HandTool -> RawTool
    // adapter; C2 its invocation succeeds; C3 its invocation returns an error
    // output. Effects: E1 the post-adapter execution target remains Sandbox; E2
    // success carries no runtime state; E3 error carries no runtime state.
    // Constraint K1: the adapter may translate shape only; it cannot move a Hand
    // capability to Brain or introduce a second state-authoring boundary.
    // Decision rules: R1=C1+C2 -> E1+E2; R2=C1+C3 -> E1+E3.
    #[tokio::test]
    async fn adapter_maps_ok_and_error_with_empty_state() {
        let good = hand_tool_as_raw(Arc::new(CustomHand {
            id: "good".into(),
            fail: false,
        }));
        let bad = hand_tool_as_raw(Arc::new(CustomHand {
            id: "bad".into(),
            fail: true,
        }));

        assert_eq!(
            good.execution_target(),
            ToolExecutionTarget::Sandbox,
            "R1/E1"
        );
        assert_eq!(
            bad.execution_target(),
            ToolExecutionTarget::Sandbox,
            "R2/E1"
        );

        let o = good
            .invoke(call("good", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(o.text(), "done");
        assert!(!o.is_error && o.state.is_empty(), "R1/E2");

        let e = bad
            .invoke(call("bad", serde_json::json!({})))
            .await
            .unwrap();
        assert!(e.is_error && e.state.is_empty(), "R2/E3");
    }

    // ---- rooted_hand_tools ----

    #[test]
    fn rooted_hand_tools_wraps_every_builtin_hand_tool() {
        let tools = rooted_hand_tools(
            IsolatedRoot::new("/env"),
            test_outputs().to_path_buf(),
            RuntimePathEnv::new("/env", "/env/mnt/session/outputs"),
            false,
        );
        let ids: Vec<_> = tools.iter().map(|t| t.id().to_string()).collect();
        for expected in ["read", "write", "edit", "glob", "grep", "bash"] {
            assert!(ids.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn rooted_tools_accept_official_file_path_and_preserve_bash_state() {
        let workspace = tempfile::tempdir().unwrap();
        let outputs = tempfile::tempdir().unwrap();
        let tools = rooted_raw_tools(
            IsolatedRoot::new(workspace.path()),
            outputs.path().to_path_buf(),
            RuntimePathEnv::new(
                workspace.path().to_string_lossy(),
                outputs.path().to_string_lossy(),
            ),
            false,
        );
        let find = |id: &str| tools.iter().find(|tool| tool.id() == id).cloned().unwrap();

        find("write")
            .invoke(call(
                "write",
                serde_json::json!({
                    "file_path": "nested/note.txt",
                    "content": "official-shape"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("nested/note.txt")).unwrap(),
            "official-shape"
        );
        find("write")
            .invoke(call(
                "write",
                serde_json::json!({
                    "file_path": "outputs/result.txt",
                    "content": "mounted-output"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(outputs.path().join("result.txt")).unwrap(),
            "mounted-output"
        );

        let bash = find("bash");
        bash.invoke(call(
            "bash",
            serde_json::json!({ "command": "mkdir state; cd state; export PERSISTED=yes" }),
        ))
        .await
        .unwrap();
        let state = bash
            .invoke(call(
                "bash",
                serde_json::json!({ "command": "printf '%s:%s' \"$PWD\" \"$PERSISTED\"" }),
            ))
            .await
            .unwrap();
        assert!(state.text().ends_with("/state:yes"), "{}", state.text());

        let outside = tempfile::NamedTempFile::new().unwrap();
        let error = find("read")
            .invoke(call(
                "read",
                serde_json::json!({ "file_path": outside.path() }),
            ))
            .await
            .expect_err("absolute host path outside the workdir must fail");
        assert!(error.to_string().contains("escapes workdir"));
    }
}
