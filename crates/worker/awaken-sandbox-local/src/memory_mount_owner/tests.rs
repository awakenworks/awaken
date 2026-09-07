use super::*;
use crate::provider::workdir_helper_tests::{TeardownProbe, workdir_spec};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct PausedMounter {
    mount_calls: Arc<AtomicUsize>,
    teardowns: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    mount_gate: Arc<tokio::sync::Semaphore>,
    teardown_gate: Option<Arc<tokio::sync::Semaphore>>,
    before_pause: bool,
    failures: usize,
}

impl PausedMounter {
    fn new(before_pause: bool, failures: usize) -> Self {
        Self {
            mount_calls: Arc::new(AtomicUsize::new(0)),
            teardowns: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            mount_gate: Arc::new(tokio::sync::Semaphore::new(0)),
            teardown_gate: None,
            before_pause,
            failures,
        }
    }

    fn guard(&self) -> Box<dyn pc::MemoryMount> {
        Box::new(TeardownProbe {
            teardowns: self.teardowns.clone(),
            drops: self.drops.clone(),
            failures: self.failures,
            wait_until_ms: None,
            response_gate: self.teardown_gate.clone(),
        })
    }
}

#[async_trait::async_trait]
impl pc::MemoryMounter for PausedMounter {
    async fn mount(
        &self,
        _: &str,
        _: &Path,
        _: pc::MountAccess,
    ) -> Result<Box<dyn pc::MemoryMount>, pc::SandboxError> {
        self.mount_calls.fetch_add(1, Ordering::SeqCst);
        let created = self.before_pause.then(|| self.guard());
        let _permit = self.mount_gate.acquire().await.unwrap();
        Ok(created.unwrap_or_else(|| self.guard()))
    }
}

fn requirement(id: &str) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: id.into(),
        mount_path: format!("/workspace/{id}"),
        source: pc::MountSource::MemoryStore {
            store_id: id.into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    }
}

async fn assert_pending<F: Future>(future: Pin<&mut F>) {
    let mut future = Box::pin(tokio::task::unconstrained(future));
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

async fn acquire(
    owner: &MemoryMountOwner,
    mounter: Arc<dyn pc::MemoryMounter>,
    req: &pc::MountRequirement,
    path: &Path,
) {
    owner
        .lock()
        .await
        .acquire(
            mounter,
            req.clone(),
            match &req.source {
                pc::MountSource::MemoryStore {
                    store_id,
                    materialization_reference,
                    ..
                } => materialization_reference
                    .as_ref()
                    .unwrap_or(store_id)
                    .clone(),
                _ => unreachable!("Memory fixture"),
            },
            path.to_owned(),
            req.access,
        )
        .await
        .unwrap();
}

mod provider_tests;

#[tokio::test]
async fn cancelled_mount_and_failed_compensation_keep_one_resumable_operation() {
    // M1-M8: backend guard created before/after Pending x cleanup failure 0/1
    // x wrapper retained/dropped. Drop cancels observation, not the stored mount
    // future. Denied retry does not resume it; admitted retry resumes once, keeps
    // failures reachable, and drops the guard only after successful teardown.
    for before_pause in [false, true] {
        for failures in [0, 1] {
            for drop_wrapper in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let registry = MemoryMountRegistry::default();
                let owner = registry.owner(root.path());
                let weak = Arc::downgrade(&owner);
                let req = requirement("memory");
                let path = root.path().join("memory");
                let mounter = Arc::new(PausedMounter::new(before_pause, failures));
                let mut pending = Box::pin(acquire(&owner, mounter.clone(), &req, &path));
                assert_pending(pending.as_mut()).await;
                assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), 1);
                drop(pending);
                assert_eq!(mounter.drops.load(Ordering::SeqCst), 0);
                let wrapper = if drop_wrapper {
                    drop(owner);
                    None
                } else {
                    Some(owner)
                };
                let retry = registry.owner(root.path());
                assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), &retry));
                assert!(retry.lock().await.require_no_pending().is_err());
                assert!(
                    retry
                        .lock()
                        .await
                        .release(|| Err(pc::SandboxError::new("denied")))
                        .await
                        .is_err()
                );
                assert_eq!(mounter.teardowns.load(Ordering::SeqCst), 0);
                mounter.mount_gate.add_permits(1);
                let result = retry.lock().await.release(|| Ok(())).await;
                assert_eq!(result.is_err(), failures != 0);
                assert_eq!(
                    mounter.drops.load(Ordering::SeqCst),
                    usize::from(failures == 0)
                );
                if failures != 0 {
                    drop(retry);
                    let retry = registry.owner(root.path());
                    retry.lock().await.release(|| Ok(())).await.unwrap();
                } else {
                    drop(retry);
                }
                assert_eq!(
                    mounter.mount_calls.load(Ordering::SeqCst),
                    1,
                    "no duplicate mount on cleanup retry"
                );
                assert_eq!(mounter.teardowns.load(Ordering::SeqCst), failures + 1);
                assert_eq!(mounter.drops.load(Ordering::SeqCst), 1);
                drop(wrapper);
                drop(registry.owner(root.path()));
                assert!(
                    weak.upgrade().is_none(),
                    "empty unreferenced owner is reclaimed"
                );
            }
        }
    }
}

#[tokio::test]
async fn root_replacement_cannot_resume_or_release_a_retained_mount() {
    // M9-M14: Pending acquisition / acquired guard x root absent/file/new-dir.
    // Every foreign root denies before mounter resumption, teardown or ACK. Restoring
    // the ORIGINAL directory permits exact retry; the replacement is untouched.
    for pending_mount in [false, true] {
        for replacement in ["absent", "file", "directory"] {
            let base = tempfile::tempdir().unwrap();
            let root = base.path().join("root");
            let parked = base.path().join("parked");
            std::fs::create_dir(&root).unwrap();
            let registry = MemoryMountRegistry::default();
            let owner = registry.owner(&root);
            let req = requirement("memory");
            let path = root.join("memory");
            let mounter = Arc::new(PausedMounter::new(true, 0));
            if pending_mount {
                let mut pending = Box::pin(acquire(&owner, mounter.clone(), &req, &path));
                assert_pending(pending.as_mut()).await;
                drop(pending);
            } else {
                mounter.mount_gate.add_permits(1);
                acquire(&owner, mounter.clone(), &req, &path).await;
                owner.lock().await.confirm_all();
            }
            std::fs::rename(&root, &parked).unwrap();
            match replacement {
                "file" => std::fs::write(&root, b"foreign").unwrap(),
                "directory" => std::fs::create_dir(&root).unwrap(),
                _ => (),
            }
            assert!(owner.lock().await.release(|| Ok(())).await.is_err());
            assert!(
                owner
                    .lock()
                    .await
                    .acknowledge_copy_reconciliation()
                    .is_err()
            );
            assert_eq!(mounter.teardowns.load(Ordering::SeqCst), 0);
            assert_eq!(mounter.drops.load(Ordering::SeqCst), 0);
            match replacement {
                "file" => {
                    assert_eq!(std::fs::read(&root).unwrap(), b"foreign");
                    std::fs::remove_file(&root).unwrap();
                }
                "directory" => std::fs::remove_dir(&root).unwrap(),
                _ => (),
            }
            std::fs::rename(&parked, &root).unwrap();
            mounter.mount_gate.add_permits(1);
            owner.lock().await.release(|| Ok(())).await.unwrap();
            assert_eq!(mounter.mount_calls.load(Ordering::SeqCst), 1);
            assert_eq!(mounter.teardowns.load(Ordering::SeqCst), 1);
            assert_eq!(mounter.drops.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn concurrent_cleanup_reenters_authority_and_retains_cancelled_waits() {
    // M15-M17: two cleanup callers + first teardown Pending; then first caller
    // revoked/renewed/cancelled. The second waits for the SAME owner mutex.
    // Revocation/cancellation retains guards; renewed success lets the waiting
    // caller observe empty completion. A different root is never affected.
    for change in ["revoke", "renew", "cancel"] {
        let root = tempfile::tempdir().unwrap();
        let peer_root = tempfile::tempdir().unwrap();
        let registry = MemoryMountRegistry::default();
        let owner = registry.owner(root.path());
        let peer = registry.owner(peer_root.path());
        assert!(!Arc::ptr_eq(&owner, &peer));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut probe = PausedMounter::new(true, 0);
        probe.teardown_gate = Some(gate.clone());
        let mounter = Arc::new(probe);
        mounter.mount_gate.add_permits(1);
        let req = requirement("memory");
        acquire(&owner, mounter.clone(), &req, &root.path().join("memory")).await;
        let allowed = AtomicBool::new(true);
        let mut first = Box::pin(async {
            owner
                .lock()
                .await
                .release(|| {
                    if allowed.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(pc::SandboxError::new("revoked"))
                    }
                })
                .await
        });
        assert_pending(first.as_mut()).await;
        assert_eq!(mounter.teardowns.load(Ordering::SeqCst), 1);
        let mut second = Box::pin(async { owner.lock().await.release(|| Ok(())).await });
        assert_pending(second.as_mut()).await;
        peer.lock().await.require_empty().unwrap();
        if change == "cancel" {
            drop(first);
        } else {
            allowed.store(change == "renew", Ordering::SeqCst);
            gate.add_permits(1);
            assert_eq!(first.await.is_ok(), change == "renew");
        }
        if change == "cancel" {
            gate.add_permits(1);
        }
        second.await.unwrap();
        assert_eq!(
            mounter.teardowns.load(Ordering::SeqCst),
            if change == "renew" { 1 } else { 2 }
        );
        assert_eq!(mounter.drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn replay_reuses_fuse_but_keeps_copy_edits_until_existing_reconciliation() {
    use awaken_resource_contract::MemoryRepository as _;
    // M18: exact Ready FUSE is reused without a second external mount. M19:
    // reference renewal tears down only the old FUSE; confirmed Copy edits and
    // original heads survive BOTH replay paths. Actual copy-only Memoryd/store
    // verifies bytes, not just a mock realization enum. Ordinary release then
    // publishes that edit through the unchanged copy teardown implementation.
    let root = tempfile::tempdir().unwrap();
    let registry = MemoryMountRegistry::default();
    let owner = registry.owner(root.path());
    let copy_req = requirement("copy");
    let mut fuse_req = requirement("fuse");
    let copy_path = root.path().join("copy");
    let fuse_path = root.path().join("fuse");
    let store = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
    store.create("copy", "/note.md", "original").await.unwrap();
    let copy = Arc::new(awaken_sandbox_memoryd::MemoryStoreMounter::copy_only(
        store.clone(),
    ));
    let fuse = Arc::new(PausedMounter::new(true, 0));
    fuse.mount_gate.add_permits(1);
    acquire(&owner, copy, &copy_req, &copy_path).await;
    acquire(&owner, fuse.clone(), &fuse_req, &fuse_path).await;
    owner.lock().await.confirm_all();
    std::fs::write(copy_path.join("note.md"), b"agent edit").unwrap();
    let mut spec = workdir_spec("replay", false);
    spec.mounts = vec![copy_req, fuse_req.clone()];
    let paths = [copy_path.clone(), fuse_path.clone()];
    owner
        .lock()
        .await
        .prepare_replay(&spec, &paths, || Ok(()))
        .await
        .unwrap();
    acquire(&owner, fuse.clone(), &fuse_req, &fuse_path).await;
    assert_eq!(fuse.mount_calls.load(Ordering::SeqCst), 1);
    if let pc::MountSource::MemoryStore {
        materialization_reference,
        ..
    } = &mut fuse_req.source
    {
        *materialization_reference = Some("renewed-reference".into());
    }
    spec.mounts[1] = fuse_req.clone();
    owner
        .lock()
        .await
        .prepare_replay(&spec, &paths, || Ok(()))
        .await
        .unwrap();
    assert_eq!(fuse.teardowns.load(Ordering::SeqCst), 1);
    assert_eq!(
        owner.lock().await.len(),
        1,
        "the original Copy participant remains"
    );
    assert_eq!(
        std::fs::read(copy_path.join("note.md")).unwrap(),
        b"agent edit"
    );
    assert_eq!(
        store
            .get_by_path("copy", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("original")
    );
    acquire(&owner, fuse.clone(), &fuse_req, &fuse_path).await;
    owner.lock().await.release(|| Ok(())).await.unwrap();
    assert_eq!(
        store
            .get_by_path("copy", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("agent edit")
    );
    assert_eq!(fuse.mount_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fuse.drops.load(Ordering::SeqCst), 2);
}
