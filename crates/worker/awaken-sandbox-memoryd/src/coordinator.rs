//! Reference-counted shared mounts (ADR-0053, D5).
//!
//! Concurrent mounts of the *same* `store_id` on one host share **one**
//! [`MemoryFuse`](crate::fuse::MemoryFuse) instance — one cache, one inode table —
//! so reads are coherent by construction (there is no second cache to go stale) and
//! writes stay CAS-serialized. The [`MountCoordinator`] enforces "mounted exactly
//! once per store", reference-counting sandbox acquire/release and unmounting only
//! when the last reference drops.
//!
//! The mount primitive is injected as a [`MountFactory`] so the refcount lifecycle
//! is unit-testable without `/dev/fuse`; [`FuseMountFactory`] is the real one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::FuseError;

/// One live mount that can be torn down. Real impl is
/// [`MemoryMountHandle`](crate::fuse::MemoryMountHandle).
pub trait Mount: Send {
    /// Tear the mount down (draining open fds up to a bounded timeout).
    fn unmount(self: Box<Self>);
}

/// Materializes one FUSE mount of a store at a host path. Injected so the
/// coordinator's refcount logic is testable with a fake.
pub trait MountFactory: Send + Sync {
    /// Mount `store_id`; return its host mountpoint and a teardown handle.
    fn mount(&self, store_id: &str) -> Result<(PathBuf, Box<dyn Mount>), FuseError>;
}

struct Entry {
    mountpoint: PathBuf,
    handle: Box<dyn Mount>,
    refcount: usize,
}

/// Owns the one-shared-mount-per-store invariant.
pub struct MountCoordinator {
    factory: Box<dyn MountFactory>,
    mounts: Mutex<HashMap<String, Entry>>,
}

impl MountCoordinator {
    #[must_use]
    pub fn new(factory: Box<dyn MountFactory>) -> Self {
        Self {
            factory,
            mounts: Mutex::new(HashMap::new()),
        }
    }

    /// Ensure `store_id` is mounted (mounting it once if not), bump its reference
    /// count, and return the shared host mountpoint (the provider binds/exposes this
    /// into each sandbox).
    pub fn acquire(&self, store_id: &str) -> Result<PathBuf, FuseError> {
        let mut mounts = self.mounts.lock().expect("mounts mutex poisoned");
        if let Some(entry) = mounts.get_mut(store_id) {
            entry.refcount += 1;
            return Ok(entry.mountpoint.clone());
        }
        // First reference: mount once.
        let (mountpoint, handle) = self.factory.mount(store_id)?;
        let result = mountpoint.clone();
        mounts.insert(
            store_id.to_string(),
            Entry {
                mountpoint,
                handle,
                refcount: 1,
            },
        );
        Ok(result)
    }

    /// Drop one reference; unmount (outside the lock) when the last one releases.
    pub fn release(&self, store_id: &str) {
        let handle = {
            let mut mounts = self.mounts.lock().expect("mounts mutex poisoned");
            match mounts.get_mut(store_id) {
                Some(entry) if entry.refcount <= 1 => mounts.remove(store_id).map(|e| e.handle),
                Some(entry) => {
                    entry.refcount -= 1;
                    None
                }
                None => None,
            }
        };
        if let Some(handle) = handle {
            handle.unmount();
        }
    }

    /// The number of distinct stores currently mounted.
    #[must_use]
    pub fn active_mounts(&self) -> usize {
        self.mounts.lock().expect("mounts mutex poisoned").len()
    }

    /// The current reference count for `store_id` (0 if not mounted).
    #[must_use]
    pub fn refcount(&self, store_id: &str) -> usize {
        self.mounts
            .lock()
            .expect("mounts mutex poisoned")
            .get(store_id)
            .map_or(0, |e| e.refcount)
    }
}

/// The real mount factory: each store is mounted under `root/<store_id>` via
/// [`spawn_mount`](crate::fuse::spawn_mount) over the shared [`MemoryFs`].
#[cfg(feature = "fuse")]
pub struct FuseMountFactory {
    fs: std::sync::Arc<dyn awaken_memory_store::MemoryFs>,
    root: PathBuf,
}

#[cfg(feature = "fuse")]
impl FuseMountFactory {
    #[must_use]
    pub fn new(
        fs: std::sync::Arc<dyn awaken_memory_store::MemoryFs>,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            fs,
            root: root.into(),
        }
    }
}

#[cfg(feature = "fuse")]
impl MountFactory for FuseMountFactory {
    fn mount(&self, store_id: &str) -> Result<(PathBuf, Box<dyn Mount>), FuseError> {
        let mountpoint = self.root.join(awaken_memory_store::sanitize_stem(store_id));
        std::fs::create_dir_all(&mountpoint).map_err(|e| FuseError::Internal(e.to_string()))?;
        let handle =
            crate::fuse::spawn_mount(self.fs.clone(), store_id.to_string(), mountpoint.clone())?;
        Ok((mountpoint, Box::new(handle)))
    }
}

#[cfg(feature = "fuse")]
impl Mount for crate::fuse::MemoryMountHandle {
    fn unmount(self: Box<Self>) {
        (*self).unmount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingMount {
        unmounts: Arc<AtomicUsize>,
    }
    impl Mount for CountingMount {
        fn unmount(self: Box<Self>) {
            self.unmounts.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CountingFactory {
        mounts: Arc<AtomicUsize>,
        unmounts: Arc<AtomicUsize>,
    }
    impl MountFactory for CountingFactory {
        fn mount(&self, store_id: &str) -> Result<(PathBuf, Box<dyn Mount>), FuseError> {
            self.mounts.fetch_add(1, Ordering::SeqCst);
            Ok((
                PathBuf::from(format!("/run/awaken/memory/{store_id}")),
                Box::new(CountingMount {
                    unmounts: self.unmounts.clone(),
                }),
            ))
        }
    }

    fn coordinator() -> (MountCoordinator, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let mounts = Arc::new(AtomicUsize::new(0));
        let unmounts = Arc::new(AtomicUsize::new(0));
        let factory = CountingFactory {
            mounts: mounts.clone(),
            unmounts: unmounts.clone(),
        };
        (MountCoordinator::new(Box::new(factory)), mounts, unmounts)
    }

    #[test]
    fn a_store_mounts_once_and_is_shared_across_acquirers() {
        let (coord, mounts, unmounts) = coordinator();
        let first = coord.acquire("memstore_1").unwrap();
        let second = coord.acquire("memstore_1").unwrap();
        assert_eq!(first, second, "both acquirers share one mountpoint");
        assert_eq!(mounts.load(Ordering::SeqCst), 1, "mounted exactly once");
        assert_eq!(coord.refcount("memstore_1"), 2);
        assert_eq!(coord.active_mounts(), 1);

        // Releasing one keeps the shared mount alive for the other.
        coord.release("memstore_1");
        assert_eq!(
            unmounts.load(Ordering::SeqCst),
            0,
            "not unmounted while referenced"
        );
        assert_eq!(coord.refcount("memstore_1"), 1);

        // The last release unmounts.
        coord.release("memstore_1");
        assert_eq!(unmounts.load(Ordering::SeqCst), 1);
        assert_eq!(coord.active_mounts(), 0);
    }

    #[test]
    fn distinct_stores_get_distinct_mounts() {
        let (coord, mounts, _) = coordinator();
        let a = coord.acquire("memstore_1").unwrap();
        let b = coord.acquire("memstore_2").unwrap();
        assert_ne!(a, b);
        assert_eq!(mounts.load(Ordering::SeqCst), 2);
        assert_eq!(coord.active_mounts(), 2);
    }

    #[test]
    fn release_of_an_unknown_store_is_a_noop() {
        let (coord, _, unmounts) = coordinator();
        coord.release("never-acquired");
        assert_eq!(unmounts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failing_mount_propagates_and_registers_no_entry() {
        // When the factory cannot mount (e.g. /dev/fuse refused, spawn failed),
        // `acquire` must surface that error AND leave the coordinator clean: no
        // half-registered entry, refcount 0, so a later retry starts from scratch
        // rather than inheriting a phantom reference.
        struct FailingFactory;
        impl MountFactory for FailingFactory {
            fn mount(&self, _store_id: &str) -> Result<(PathBuf, Box<dyn Mount>), FuseError> {
                Err(FuseError::Internal("mount refused".into()))
            }
        }
        let coord = MountCoordinator::new(Box::new(FailingFactory));
        assert!(matches!(
            coord.acquire("memstore_x"),
            Err(FuseError::Internal(_))
        ));
        assert_eq!(
            coord.active_mounts(),
            0,
            "a failed mount registers no entry"
        );
        assert_eq!(coord.refcount("memstore_x"), 0);
    }
}
