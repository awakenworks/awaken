//! `awaken-sandbox-local` — single-machine sandbox isolation realizing the neutral
//! [`awaken_provisioning_contract`] seam (ADR-0041).
//!
//! Each sandbox is an [`IsolatedRoot`] (a path jail). Rooted tools execute in it:
//! [`RootedTool`] wraps a native `RawTool` and rewrites its path arguments through
//! the jail (and runs `bash` with `cd <root>` / `bwrap --unshare-net`), so a tool
//! cannot touch a path outside its sandbox. A rooted tool is *just a `RawTool`* the
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
use serde_json::Value;

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
        return Err(pc::SandboxError::new(
            "filesystem completion receipt does not cover the frozen mount set",
        ));
    }

    let mut expected_materializations = 0_usize;
    for (required, realized) in spec.mounts.iter().zip(&mounts) {
        if required.mount_id != realized.mount_id
            || required.mount_path != realized.mount_path
            || required.access != realized.access
        {
            return Err(pc::SandboxError::new(
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
                    return Err(pc::SandboxError::new(
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
                        return Err(pc::SandboxError::new(
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
                    return Err(pc::SandboxError::new(
                        "filesystem completion receipt attached Memory evidence to a non-Memory mount",
                    ));
                }
            }
            _ => {
                return Err(pc::SandboxError::new(
                    "filesystem completion receipt has a provider-incompatible realization",
                ));
            }
        }
    }
    if expected_materializations != materializations.len() {
        return Err(pc::SandboxError::new(
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
        return Err(pc::SandboxError::new(
            "terminal reconstruction cannot recover a write-through Memory mount without its live FUSE guard",
        ));
    }

    let materializations = match handle {
        Some(handle) => handle.memory_materializations()?.unwrap_or(&[]),
        None if memory_mounts.is_empty() => &[],
        None => {
            return Err(pc::SandboxError::new(
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
        return Err(pc::SandboxError::new(
            "terminal Memory materialization evidence is missing, extra, or ambiguous for the frozen Sandbox spec",
        ));
    }
    Ok(materializations.to_vec())
}

pub(crate) async fn teardown_memory_mounts(
    mounts: &[Box<dyn awaken_provisioning_contract::MemoryMount>],
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let mut failure = None;
    for mount in mounts {
        if let Err(error) = mount.teardown().await
            && failure.is_none()
        {
            failure = Some(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod memory_mount_release_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use awaken_provisioning_contract as pc;

    struct CountedMount {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        fail_first: bool,
        realization: pc::Realization,
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
                Err(pc::SandboxError::new("retry teardown"))
            } else {
                Ok(())
            }
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
        let mounts: tokio::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>> =
            tokio::sync::Mutex::new(vec![
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

        assert!(super::release_memory_mounts(&mounts).await.is_err(), "R1");
        assert_eq!(mounts.lock().await.len(), 2, "R1");
        super::release_memory_mounts(&mounts).await.expect("R2/R3");
        assert!(mounts.lock().await.is_empty(), "R3");
        assert_eq!(successful_calls.load(Ordering::SeqCst), 2, "R2");
        assert_eq!(retry_calls.load(Ordering::SeqCst), 2, "R2");
    }

    #[tokio::test]
    async fn terminal_ack_is_exact_effect_scoped_and_retires_only_copy_guards() {
        // Terminal-memory acknowledgement decision table. Causes: C1 supplied
        // evidence equals the complete canonical provider evidence (including
        // order); C2 fence is live; C3 this process recorded no ack / the same
        // effect / a same-lease takeover / a foreign lease; C4 guard is
        // Copy/FUSE. Rules: A1 !C1 or
        // !C2 => reject before authorization or guard mutation; A2 C1+C2+no ack
        // => authorize once, drop every Copy without teardown, retain FUSE, and
        // record the full effect identity; A3 same-effect replay => success with
        // no repeated authorization/drop; A4 same-lease takeover reauthorizes,
        // drains idempotently, and rebinds the ack; A5 foreign lease rejects
        // with the retained FUSE and ack unchanged. Disposal gating admits only
        // the latest effect recorded by A2/A4.
        let first = pc::MemoryMaterializationEvidence::new(
            "store-a",
            "/a",
            vec![pc::MemoryMaterializationHead {
                path: "value".into(),
                id: "head-a".into(),
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
                content_sha256: "digest-b".into(),
            }],
        )
        .unwrap();
        let expected = vec![first.clone(), second.clone()];
        let copy_drops = Arc::new(AtomicUsize::new(0));
        let fuse_drops = Arc::new(AtomicUsize::new(0));
        let teardown_calls = Arc::new(AtomicUsize::new(0));
        let mounts: tokio::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>> =
            tokio::sync::Mutex::new(vec![
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
                &reversed,
                || Ok(expected.clone()),
                || {
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
                &expected,
                || Ok(expected.clone()),
                || {
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
            &expected,
            || Ok(expected.clone()),
            || {
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
            &expected,
            || Ok(expected.clone()),
            || {
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
            &expected,
            || Ok(expected.clone()),
            || {
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
            super::require_memory_reconciliation_ack(&ack, &expected, &live).is_err(),
            "A4 supersedes prior effect"
        );
        super::require_memory_reconciliation_ack(&ack, &expected, &takeover)
            .expect("A4 disposal gate");
        assert!(
            super::acknowledge_memory_reconciliation(
                &ack,
                &mounts,
                &foreign,
                &expected,
                || Ok(expected.clone()),
                || {
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

/// Tear down an all-or-retry set. No guard is cleared until every teardown
/// succeeds, so a partial transport/unmount failure remains retryable and can
/// never be converted into successful root deletion.
pub(crate) async fn release_memory_mounts(
    mounts: &tokio::sync::Mutex<Vec<Box<dyn awaken_provisioning_contract::MemoryMount>>>,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let mut retained = mounts.lock().await;
    teardown_memory_mounts(&retained).await?;
    retained.clear();
    Ok(())
}

/// Serialize the provider's complete Copy-guard snapshot with the shared
/// acknowledgement kernel. The snapshot callback runs while the async guard
/// list is locked, so Namespace dynamic attachment can publish its guard and
/// evidence atomically in the same lock order.
pub(crate) async fn acknowledge_memory_reconciliation(
    acknowledgement: &awaken_provisioning_contract::MemoryReconciliationAck,
    mounts: &tokio::sync::Mutex<Vec<Box<dyn awaken_provisioning_contract::MemoryMount>>>,
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
    supplied_materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
    expected_materializations: impl FnOnce() -> Result<
        Vec<awaken_provisioning_contract::MemoryMaterializationEvidence>,
        awaken_provisioning_contract::SandboxError,
    >,
    authorize: impl FnOnce() -> Result<(), awaken_provisioning_contract::SandboxError>,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    use awaken_provisioning_contract as pc;

    let mut retained = mounts.lock().await;
    let expected_materializations = expected_materializations()?;
    acknowledgement.acknowledge(
        effect_fence,
        &expected_materializations,
        supplied_materializations,
        || {
            authorize()?;
            retained.retain(|mount| mount.realization() != pc::Realization::Copy);
            Ok(())
        },
    )
}

pub(crate) fn require_memory_reconciliation_ack(
    acknowledgement: &awaken_provisioning_contract::MemoryReconciliationAck,
    expected_materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    acknowledgement.require_for_disposal(expected_materializations, effect_fence)
}

/// Admit or refresh one exact filesystem removal participant. Both Local and
/// Namespace use this sole provider authorization path before Copy guards are
/// retired or disposal preparation is reported. Same-operation renewals retain
/// the first durable predecessor; another operation cannot reuse it.
pub(crate) fn authorize_terminal_disposal(
    root: &Path,
    realization: &realization_marker::LiveRealization,
    terminal_removal: &std::sync::Mutex<Option<realization_marker::RemovalGuard>>,
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
) -> Result<
    awaken_provisioning_contract::SandboxEffectFence,
    awaken_provisioning_contract::SandboxError,
> {
    use awaken_provisioning_contract as pc;

    let evidence = realization.current().ok_or_else(|| {
        pc::SandboxError::new("legacy sandbox handle cannot authorize fenced Memory reconciliation")
    })?;
    let mut slot = terminal_removal
        .lock()
        .map_err(|_| pc::SandboxError::new("terminal removal lock poisoned"))?;
    let mut removal = slot.take();
    if removal.is_none() {
        removal = realization_marker::begin_terminal_takeover(
            root,
            evidence.fingerprint(),
            Some(realization_marker::RebuildSource {
                fingerprint: evidence.fingerprint(),
                effect_fence: evidence.effect_fence(),
                physical_incarnation: evidence.physical_incarnation(),
            }),
            None,
            effect_fence,
        )?
        .map(|(_, removal)| removal);
    }
    let Some(mut removal) = removal else {
        return Err(pc::SandboxError::new(
            "Memory reconciliation lost its exact physical Sandbox participant",
        ));
    };
    let prepared_effect_fence = match removal.prepare_disposal_effect(effect_fence) {
        Ok(prepared_effect_fence) => prepared_effect_fence,
        Err(error) => {
            *slot = Some(removal);
            return Err(error);
        }
    };
    *slot = Some(removal);
    Ok(prepared_effect_fence)
}

/// Validate the provider-neutral Memory acknowledgement and bind the exact
/// filesystem removal participant without deleting or shredding any bytes.
/// The aggregate persists successful preparation before invoking physical
/// authorization; this process-local guard is only its provider projection.
pub(crate) fn prepare_terminal_disposal(
    acknowledgement: &awaken_provisioning_contract::MemoryReconciliationAck,
    expected_materializations: &[awaken_provisioning_contract::MemoryMaterializationEvidence],
    root: &Path,
    realization: &realization_marker::LiveRealization,
    terminal_removal: &std::sync::Mutex<Option<realization_marker::RemovalGuard>>,
    effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
) -> Result<
    awaken_provisioning_contract::SandboxEffectFence,
    awaken_provisioning_contract::SandboxError,
> {
    require_memory_reconciliation_ack(acknowledgement, expected_materializations, effect_fence)?;
    authorize_terminal_disposal(root, realization, terminal_removal, effect_fence)
}

/// Consume one previously prepared exact filesystem participant. This is the
/// sole Local/Namespace physical authorization owner: remaining FUSE mounts,
/// credential bytes, the exact root, and finally the Removed marker are handled
/// in that order. Any pre-delete failure restores the guard for exact replay.
pub(crate) async fn dispose_terminal_realization(
    memory_mounts: &tokio::sync::Mutex<Vec<Box<dyn awaken_provisioning_contract::MemoryMount>>>,
    sandbox_root: &Path,
    secret_paths: &[PathBuf],
    terminal_removal: &std::sync::Mutex<Option<realization_marker::RemovalGuard>>,
    authorization: &awaken_provisioning_contract::SandboxDisposalAuthorization,
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    use awaken_provisioning_contract as pc;

    let mut removal = terminal_removal
        .lock()
        .map_err(|_| pc::SandboxError::new("terminal removal lock poisoned"))?
        .take()
        .ok_or_else(|| {
            pc::SandboxError::new(
                "physical disposal requires an exact prepared filesystem participant",
            )
        })?;
    if let Err(error) = removal.authorize_disposal(authorization) {
        *terminal_removal
            .lock()
            .map_err(|_| pc::SandboxError::new("terminal removal lock poisoned"))? = Some(removal);
        return Err(error);
    }
    if let Err(error) = release_memory_mounts(memory_mounts).await {
        *terminal_removal
            .lock()
            .map_err(|_| pc::SandboxError::new("terminal removal lock poisoned"))? = Some(removal);
        return Err(error);
    }
    if let Err(error) = shred_secret_paths_for_removal(&removal, sandbox_root, secret_paths) {
        *terminal_removal
            .lock()
            .map_err(|_| pc::SandboxError::new("terminal removal lock poisoned"))? = Some(removal);
        return Err(error);
    }
    if let Err(error) = removal.remove_root() {
        *terminal_removal
            .lock()
            .map_err(|_| pc::SandboxError::new("terminal removal lock poisoned"))? = Some(removal);
        return Err(error);
    }
    removal.finish()
}

fn shred_secret_paths_for_removal(
    removal: &realization_marker::RemovalGuard,
    sandbox_root: &Path,
    secret_paths: &[PathBuf],
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let Some((owned_root, identity)) = removal.owned_root()? else {
        return Ok(());
    };
    let owned = IsolatedRoot::new(owned_root);
    let paths = secret_paths
        .iter()
        .map(|path| {
            path.strip_prefix(sandbox_root)
                .map(|relative| owned.root().join(relative))
                .map_err(|_| {
                    awaken_provisioning_contract::SandboxError::new(format!(
                        "secret path `{}` is outside sandbox root `{}`",
                        path.display(),
                        sandbox_root.display()
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    shred_secret_paths_at(&owned, Some(identity), &paths)
}

/// Shred provider-selected credential paths through the one exact-root leaf.
/// `None` is valid only for a marker-free legacy admission whose root is still
/// absent; it never authorizes deriving an identity from an occupied pathname.
pub(crate) fn shred_secret_paths_at(
    root: &IsolatedRoot,
    root_identity: Option<awaken_sandbox_fs::DirectoryIdentity>,
    paths: &[PathBuf],
) -> Result<(), awaken_provisioning_contract::SandboxError> {
    let Some(root_identity) = root_identity else {
        return match awaken_sandbox_fs::classify_nofollow(root.root())
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?
        {
            awaken_sandbox_fs::PathEntry::Absent => Ok(()),
            _ => Err(awaken_provisioning_contract::SandboxError::new(
                "marker-free secret shredding found an occupied sandbox root",
            )),
        };
    };
    for path in paths {
        let relative = path.strip_prefix(root.root()).map_err(|_| {
            awaken_provisioning_contract::SandboxError::new(format!(
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
        .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
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
        command
            .env("AWAKEN_PROJECT_DIR", &self.project_dir)
            .env("AWAKEN_OUTPUTS_DIR", &self.outputs_dir);
    }

    fn bash_env(&self) -> std::collections::BTreeMap<String, String> {
        let mut env = std::env::vars()
            .filter(|(key, _)| !key.starts_with("ANTHROPIC_"))
            .collect::<std::collections::BTreeMap<_, _>>();
        env.insert("AWAKEN_PROJECT_DIR".into(), self.project_dir.clone());
        env.insert("AWAKEN_OUTPUTS_DIR".into(), self.outputs_dir.clone());
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
    let stem = id.split('.').next().unwrap_or_default();
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if !invalid && !reserved {
        return base.join(id);
    }
    base.join(format!("scope-{}", blake3::hash(id.as_bytes()).to_hex()))
}

/// The `awaken-provisioning-contract` seam realized locally (ADR-0041).
mod artifacts;
mod blob_cache;
mod git_transport;
mod namespace;
mod provider;
mod read_only_tree;
mod realization_marker;
mod repo_bundle;
// The provider resolves mount bytes from an injected [`pc::BlobSource`] port
// (ADR-0038 D6, dependency-inverted) — this worker-tier crate links no durable
// store; the composition root adapts the content-addressed store to the port.
pub use awaken_local_process::LocalProcess;
pub use blob_cache::{BlobLru, WorkspaceBlobCache};
pub(crate) use git_transport::{git_bytes, provision_repo_at, push_repo_to_at, run_git};
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

/// Rewrite one tool call's file arguments so paths remain jailed under `root`.
/// Bash confinement is configured once when its persistent process starts, so
/// Bash arguments intentionally pass through unchanged here.
fn jail_args(
    tool_id: &str,
    mut args: Value,
    root: &IsolatedRoot,
    host_outputs: &Path,
    _deny_egress: bool,
) -> Result<Value, ToolError> {
    let escape = |e: EscapeError| ToolError::Execution(e.to_string());
    let map_output_alias =
        |args: &mut Value, key: &str, root: &IsolatedRoot| -> Result<(), ToolError> {
            let Some(Value::String(input)) = args.get(key) else {
                return Ok(());
            };
            if Path::new(input).is_absolute() {
                // Managed Agents use sandbox-absolute `/mnt/...` paths. A
                // Workdir tier has no kernel path fidelity, so translate only
                // these public logical roots into its private backing root.
                // Arbitrary host-absolute paths remain untouched and are then
                // rejected by `FileContext`, preserving the escape boundary.
                if matches!(input.as_str(), "/mnt" | "/outputs")
                    || input.starts_with("/mnt/")
                    || awaken_provisioning_contract::WorkspaceLayout::contains(input)
                    || input.starts_with("/outputs/")
                {
                    let jailed = root.resolve(input).map_err(escape)?;
                    args[key] = Value::String(jailed.to_string_lossy().into_owned());
                }
                return Ok(());
            }
            let jailed = root.resolve(input).map_err(escape)?;
            let relative = jailed.strip_prefix(root.root()).map_err(|_| {
                ToolError::Execution(format!("path `{input}` escaped its environment"))
            })?;
            if let Ok(suffix) = relative.strip_prefix("outputs") {
                args[key] = Value::String(host_outputs.join(suffix).to_string_lossy().into_owned());
            }
            Ok(())
        };
    match tool_id {
        "read" | "write" | "edit" => {
            map_output_alias(&mut args, "file_path", root)?;
            map_output_alias(&mut args, "path", root)?;
        }
        "grep" => map_output_alias(&mut args, "path", root)?,
        "glob" => map_output_alias(&mut args, "path", root)?,
        // Bash is confined when its persistent process is launched. Wrapping an
        // individual command here would create a fresh inner shell and lose
        // cross-call state such as `cd`, `export`, aliases, and functions.
        "bash" => {}
        _ => {}
    }
    Ok(args)
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

/// A [`HandTool`] that runs an inner `RawTool` jailed to an environment root. The
/// jail rewrites path arguments; the inner result is narrowed to [`HandOutput`], so
/// any runtime state the inner tool might carry is dropped at the boundary (G13).
pub(crate) struct RootedTool {
    inner: Arc<dyn RawTool>,
    root: IsolatedRoot,
    /// Concrete backing path for the one SandboxSpec output directory.
    host_outputs: PathBuf,
    /// Deny network egress for the `bash` tool (from the environment's spec).
    deny_egress: bool,
}

impl RootedTool {
    pub(crate) fn new(
        inner: Arc<dyn RawTool>,
        root: IsolatedRoot,
        host_outputs: PathBuf,
        deny_egress: bool,
    ) -> Self {
        Self {
            inner,
            root,
            host_outputs,
            deny_egress,
        }
    }
}

#[async_trait]
impl HandTool for RootedTool {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn run(&self, mut call: ToolCall) -> Result<HandOutput, ToolError> {
        call.arguments = jail_args(
            self.inner.id(),
            call.arguments,
            &self.root,
            &self.host_outputs,
            self.deny_egress,
        )?;
        let out = self.inner.invoke(call).await?;
        // Narrow to content/error: an environment tool never authors runtime state.
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
        .with_allowed_root(&host_outputs)
        .with_bash_env(runtime_paths.bash_env());
    if deny_egress {
        context = context.with_bash_launcher(
            "bwrap",
            bwrap_persistent_bash(&root.root().to_string_lossy()),
        );
    }
    all_hand_tools_in(context)
        .into_iter()
        .map(|inner| {
            Arc::new(RootedTool::new(
                inner,
                root.clone(),
                host_outputs.clone(),
                deny_egress,
            )) as Arc<dyn HandTool>
        })
        .collect()
}

/// Resolve a logical path under a realized `root`, fail-closed on escape (G3). The
/// free-function form of the jail the repo helpers share; `.`/`..` segments and an
/// empty path are rejected so a mount never lands outside the sandbox root.
pub(crate) fn jailed_at(root: &IsolatedRoot, logical: &str) -> Result<PathBuf, SandboxError> {
    let logical = logical.trim_start_matches('/');
    if logical.is_empty() || logical.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(SandboxError(format!("unsafe repo mount path `{logical}`")));
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
    .map_err(|error| SandboxError::new(error.to_string()))?
    .into_iter()
    .map(|file| {
        let relative = file
            .relative_path
            .to_str()
            .ok_or_else(|| SandboxError::new("sandbox tree contains a non-UTF-8 logical path"))?;
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
    .map_err(|error| SandboxError::new(error.to_string()))?
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
            .ok_or_else(|| SandboxError::new("Skill directory id is not UTF-8"))?
            .to_owned();
        let content = String::from_utf8(file.bytes)
            .map_err(|error| SandboxError::new(format!("Skill `{id}` is not UTF-8: {error}")))?;
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

/// A `SKILL.md`-bearing directory discovered under the environment. Neutral file
/// data — no skill semantics — so the sandbox stays unaware of the skill model
/// (the host parses it). `dir` is a **logical** path under the root (usable by
/// jailed tools and for `${SKILL_DIR}`), never a host absolute path (G3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkillFile {
    pub id: String,
    pub content: String,
    pub dir: String,
}

/// Why provisioning failed.
#[derive(Debug, thiserror::Error)]
#[error("sandbox provisioning failed: {0}")]
pub struct SandboxError(pub String);

impl SandboxError {
    fn new(error: impl Into<String>) -> Self {
        Self(error.into())
    }
}

impl From<SandboxError> for awaken_provisioning_contract::SandboxError {
    fn from(error: SandboxError) -> Self {
        Self::new(error.0)
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

    let fingerprint = format!("local-test-preparation:{}", prepared.operation_id);
    let preparation = pc::SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
    let operation_id = preparation.operation_id().unwrap();
    let successor = pc::SandboxEffectFence::new(
        operation_id,
        current.owner.clone(),
        current.runtime_incarnation.clone(),
        current.epoch,
        current.expires_at_unix_ms,
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

    // ---- jail_args branches ----

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

    #[test]
    fn jail_preserves_glob_pattern_and_persistent_bash_command() {
        let root = IsolatedRoot::new("/env");
        let g = jail_args(
            "glob",
            serde_json::json!({ "pattern": "src/*.rs" }),
            &root,
            test_outputs(),
            false,
        )
        .unwrap();
        assert_eq!(g["pattern"], "src/*.rs");

        let b = jail_args(
            "bash",
            serde_json::json!({ "command": "ls" }),
            &root,
            test_outputs(),
            false,
        )
        .unwrap();
        assert_eq!(b["command"], "ls");
    }

    #[test]
    fn logical_output_alias_targets_only_the_canonical_sandbox_directory() {
        // Output-path FMECA / cause-effect decision table. C1 is an Agent-facing
        // `outputs/...` path, C2 is an ordinary workspace path, and C3 contains a
        // normalized parent segment. Effects: E1 maps to the one SandboxSpec
        // output backing directory; E2 remains under the workspace jail; E3 never
        // reaches the parent of that backing directory.
        //
        // | Rule | logical path | Effect |
        // | O1 | outputs/result.txt | E1 canonical output |
        // | O2 | notes/result.txt | E2 workspace file |
        // | O3 | outputs/../secret | E2 workspace secret, not output-parent escape |
        let root = IsolatedRoot::new("/env/workspace");
        let outputs = Path::new("/env/mnt/session/outputs");
        for (rule, logical, expected) in [
            (
                "O1",
                "outputs/result.txt",
                "/env/mnt/session/outputs/result.txt",
            ),
            ("O2", "notes/result.txt", "notes/result.txt"),
            ("O3", "outputs/../secret", "outputs/../secret"),
        ] {
            let call = jail_args(
                "write",
                serde_json::json!({ "path": logical }),
                &root,
                outputs,
                false,
            )
            .unwrap();
            assert_eq!(call["path"], expected, "{rule}");
        }
    }

    #[test]
    fn managed_absolute_mount_paths_rebase_but_host_paths_still_fail_closed() {
        // Cause/effect decision table for Workdir (path_fidelity=false): C1 is
        // the exact WorkspaceLayout root, C2 is one of its descendants, C3 is a
        // public `/mnt` mount, and C4 is an arbitrary host-absolute path. E1
        // rebases below the private Session root; E2 leaves the host path
        // untouched so FileContext rejects it. Rules: W1 C1=>E1; W2 C2=>E1;
        // W3 C3=>E1; W4 C4=>E2. The exact-root rule also proves the shared
        // WorkspaceLayout::contains authority covers ROOT without a second
        // equality branch in this adapter.
        let root = IsolatedRoot::new("/private/session-root");
        for (tool, key, logical, expected) in [
            (
                "glob",
                "path",
                awaken_provisioning_contract::WorkspaceLayout::ROOT,
                "/private/session-root/workspace",
            ),
            (
                "read",
                "file_path",
                "/mnt/dream/input-memory/MEMORY.md",
                "/private/session-root/mnt/dream/input-memory/MEMORY.md",
            ),
            (
                "write",
                "file_path",
                "/mnt/dream/output-memory/MEMORY.md",
                "/private/session-root/mnt/dream/output-memory/MEMORY.md",
            ),
            (
                "glob",
                "path",
                "/workspace/project",
                "/private/session-root/workspace/project",
            ),
        ] {
            let mut arguments = serde_json::json!({});
            arguments[key] = serde_json::Value::String(logical.into());
            let mapped = jail_args(tool, arguments, &root, test_outputs(), false).unwrap();
            assert_eq!(mapped[key], expected, "{tool}:{key}");
        }
        let outside = jail_args(
            "read",
            serde_json::json!({"file_path":"/etc/passwd"}),
            &root,
            test_outputs(),
            false,
        )
        .unwrap();
        assert_eq!(outside["file_path"], "/etc/passwd");
    }

    #[test]
    fn jail_passes_unknown_tools_through_and_rejects_escapes() {
        let root = IsolatedRoot::new("/env");
        let u = jail_args(
            "weird",
            serde_json::json!({ "path": "../x" }),
            &root,
            test_outputs(),
            false,
        )
        .unwrap();
        assert_eq!(u["path"], "../x"); // unknown tool: untouched

        assert!(
            jail_args(
                "read",
                serde_json::json!({ "path": "../escape" }),
                &root,
                test_outputs(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn deny_egress_bash_commands_are_not_wrapped_per_call() {
        // Per-call wrapping would start an inner shell and discard `cd`,
        // exports, aliases, and functions after every invocation.
        let root = IsolatedRoot::new("/env");
        let out = jail_args(
            "bash",
            serde_json::json!({ "command": "cd nested && export ANSWER=42" }),
            &root,
            test_outputs(),
            true,
        )
        .unwrap();
        assert_eq!(out["command"], "cd nested && export ANSWER=42");
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
