//! Product-plane preparation of caller-owned [`pc::MountSource::CacheVolume`]
//! directories. Providers still only mount opaque paths; this module owns the
//! one content-keyed preparation path used by explicit startup warmup and by
//! Session creation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheVolumeIdentity {
    key: String,
    host_path: PathBuf,
}

/// One caller-owned, rebuildable cache directory to prepare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheVolumeWarmup {
    pub key: String,
    pub host_path: PathBuf,
}

impl CacheVolumeWarmup {
    #[must_use]
    pub fn new(key: impl Into<String>, host_path: impl Into<PathBuf>) -> Self {
        Self {
            key: key.into(),
            host_path: host_path.into(),
        }
    }

    fn identity(&self) -> CacheVolumeIdentity {
        CacheVolumeIdentity {
            key: self.key.clone(),
            host_path: self.host_path.clone(),
        }
    }
}

/// Product-specific population of a rebuildable cache directory. Implementations
/// may clone a repository, restore a package cache, or prepare derived artifacts.
/// They must publish the directory contents before returning success.
#[async_trait]
pub trait CacheVolumeInitializer: Send + Sync {
    async fn initialize(&self, volume: &CacheVolumeWarmup) -> Result<(), String>;
}

/// Default preparation: make the caller-declared directory mountable. Product
/// compositions that know how to populate it install a richer initializer.
struct FilesystemCacheVolumeInitializer;

#[async_trait]
impl CacheVolumeInitializer for FilesystemCacheVolumeInitializer {
    async fn initialize(&self, volume: &CacheVolumeWarmup) -> Result<(), String> {
        if volume.host_path.as_os_str().is_empty() {
            return Err("host path is empty".into());
        }
        let path = volume.host_path.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&path))
            .await
            .map_err(|error| format!("directory preparation task failed: {error}"))?
            .map_err(|error| format!("create cache directory: {error}"))
    }
}

type Preparation = tokio::sync::OnceCell<Result<(), String>>;

struct CacheVolumePrewarmerInner {
    initializer: Arc<dyn CacheVolumeInitializer>,
    preparations: tokio::sync::Mutex<HashMap<CacheVolumeIdentity, Arc<Preparation>>>,
}

/// Content-keyed single-flight cache-volume preparation.
///
/// A successful `(key, host_path)` is reused for the process lifetime. Failures are
/// removed so a later Session can retry; changing content requires a new caller key.
#[derive(Clone)]
pub(crate) struct CacheVolumePrewarmer {
    inner: Arc<CacheVolumePrewarmerInner>,
}

impl Default for CacheVolumePrewarmer {
    fn default() -> Self {
        Self::new(Arc::new(FilesystemCacheVolumeInitializer))
    }
}

impl CacheVolumePrewarmer {
    #[must_use]
    pub(crate) fn new(initializer: Arc<dyn CacheVolumeInitializer>) -> Self {
        Self {
            inner: Arc::new(CacheVolumePrewarmerInner {
                initializer,
                preparations: tokio::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Prepare one cache volume, joining an identical concurrent request.
    pub(crate) async fn prewarm(&self, volume: CacheVolumeWarmup) -> Result<(), String> {
        let identity = volume.identity();
        let preparation = {
            let mut preparations = self.inner.preparations.lock().await;
            preparations
                .entry(identity.clone())
                .or_insert_with(|| Arc::new(Preparation::new()))
                .clone()
        };
        let result = preparation
            .get_or_init(|| self.inner.initializer.initialize(&volume))
            .await
            .clone();
        if result.is_err() {
            let mut preparations = self.inner.preparations.lock().await;
            if preparations
                .get(&identity)
                .is_some_and(|current| Arc::ptr_eq(current, &preparation))
            {
                preparations.remove(&identity);
            }
        }
        result.map_err(|error| {
            format!(
                "prepare cache volume `{}` at `{}`: {error}",
                volume.key,
                volume.host_path.display()
            )
        })
    }

    /// Prepare every CacheVolume in a Session spec before the provider binds it.
    /// Repeated mounts of one identity share the same single-flight cell.
    pub(crate) async fn prepare_mounts(
        &self,
        mounts: &[pc::MountRequirement],
    ) -> Result<(), String> {
        for mount in mounts {
            if let pc::MountSource::CacheVolume { host_path, key } = &mount.source {
                self.prewarm(CacheVolumeWarmup::new(key, Path::new(host_path)))
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CountingInitializer {
        calls: AtomicUsize,
        failures_left: AtomicUsize,
    }

    #[async_trait]
    impl CacheVolumeInitializer for CountingInitializer {
        async fn initialize(&self, _volume: &CacheVolumeWarmup) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            if self
                .failures_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                Err("injected cold-build failure".into())
            } else {
                Ok(())
            }
        }
    }

    /// Cause/effect design:
    /// C1=same key+path overlaps, C2=different key or path, C3=initializer succeeds.
    /// E1=C1 joins one initialization, E2=C2 owns independent initialization,
    /// E3=all successful waiters observe Ready.
    /// Decision rules exercised: (C1,C3)->(E1,E3), (C2,C3)->(E2,E3).
    #[tokio::test]
    async fn identical_warmups_are_single_flight_but_distinct_identities_are_not() {
        let initializer = Arc::new(CountingInitializer {
            calls: AtomicUsize::new(0),
            failures_left: AtomicUsize::new(0),
        });
        let prewarmer = CacheVolumePrewarmer::new(initializer.clone());
        let a = CacheVolumeWarmup::new("deps-v1", "/tmp/awaken-cache-a");
        let b = CacheVolumeWarmup::new("deps-v2", "/tmp/awaken-cache-a");

        let (first, duplicate, distinct) = tokio::join!(
            prewarmer.prewarm(a.clone()),
            prewarmer.prewarm(a),
            prewarmer.prewarm(b),
        );

        assert!(first.is_ok() && duplicate.is_ok() && distinct.is_ok());
        assert_eq!(initializer.calls.load(Ordering::SeqCst), 2);
    }

    /// Cause/effect design:
    /// C1=initializer fails, C2=a later request retries, C3=retry succeeds.
    /// E1=failure is not cached as Ready, E2=the later call runs the initializer
    /// again, E3=the successful result becomes reusable.
    /// Decision rules: (C1)->E1; (C1,C2,C3)->(E2,E3).
    #[tokio::test]
    async fn failed_preparation_is_retryable_and_success_is_reused() {
        let initializer = Arc::new(CountingInitializer {
            calls: AtomicUsize::new(0),
            failures_left: AtomicUsize::new(1),
        });
        let prewarmer = CacheVolumePrewarmer::new(initializer.clone());
        let volume = CacheVolumeWarmup::new("repo-v1", "/tmp/awaken-cache-retry");

        assert!(prewarmer.prewarm(volume.clone()).await.is_err());
        assert!(prewarmer.prewarm(volume.clone()).await.is_ok());
        assert!(prewarmer.prewarm(volume).await.is_ok());
        assert_eq!(initializer.calls.load(Ordering::SeqCst), 2);
    }
}
