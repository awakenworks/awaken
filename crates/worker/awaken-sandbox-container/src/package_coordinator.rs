//! Durable package-image build coordination.
//!
//! The OCI registry stores image bytes; this small filesystem journal stores the
//! build state and an atomic lease. A shared directory therefore provides
//! cross-process / cross-worker single-flight without coupling the worker crate
//! to a database. Every state file is disposable: the immutable registry digest
//! remains authoritative and is revalidated before reuse.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use serde::{Deserialize, Serialize};

use crate::{PackageImageProvisioner, RuntimeError};

#[derive(Debug, Clone)]
pub struct PackageCoordinatorPolicy {
    pub lease: Duration,
    pub wait_timeout: Duration,
    pub failure_retry: Duration,
    pub state_ttl: Duration,
}

impl Default for PackageCoordinatorPolicy {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(15 * 60),
            wait_timeout: Duration::from_secs(20 * 60),
            failure_retry: Duration::from_secs(15),
            state_ttl: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BuildState {
    Ready { image: String, updated_at_ms: u64 },
    Failed { message: String, updated_at_ms: u64 },
}

pub struct CoordinatedPackageProvisioner {
    inner: Arc<dyn PackageImageProvisioner>,
    root: PathBuf,
    policy: PackageCoordinatorPolicy,
}

impl CoordinatedPackageProvisioner {
    pub fn new(
        inner: Arc<dyn PackageImageProvisioner>,
        root: impl Into<PathBuf>,
        policy: PackageCoordinatorPolicy,
    ) -> Result<Self, RuntimeError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(backend)?;
        Ok(Self {
            inner,
            root,
            policy,
        })
    }

    async fn request_key(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        let identity = self
            .inner
            .package_image_coordination_key(base_image, packages, network)
            .await?;
        Ok(blake3::hash(identity.as_bytes()).to_hex().to_string())
    }

    fn state_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.lock"))
    }

    fn read_state(&self, key: &str) -> Option<BuildState> {
        std::fs::read(self.state_path(key))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    fn write_state(&self, key: &str, state: &BuildState) -> Result<(), RuntimeError> {
        let path = self.state_path(key);
        let tmp = self
            .root
            .join(format!("{key}.json.tmp-{}", std::process::id()));
        let bytes = serde_json::to_vec(state).map_err(backend)?;
        std::fs::write(&tmp, bytes).map_err(backend)?;
        std::fs::rename(&tmp, &path).map_err(backend)
    }

    fn acquire(&self, key: &str) -> Result<Option<BuildLease>, RuntimeError> {
        let path = self.lock_path(key);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{} {}", std::process::id(), now_ms()).map_err(backend)?;
                file.sync_all().map_err(backend)?;
                Ok(Some(BuildLease { path }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path, self.policy.lease) {
                    match std::fs::remove_file(&path) {
                        Ok(()) => return self.acquire(key),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return self.acquire(key);
                        }
                        Err(error) => return Err(backend(error)),
                    }
                }
                Ok(None)
            }
            Err(error) => Err(backend(error)),
        }
    }

    fn prune_states(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if modified_age(&path).is_some_and(|age| age > self.policy.state_ttl) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

struct BuildLease {
    path: PathBuf,
}

impl Drop for BuildLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[async_trait]
impl PackageImageProvisioner for CoordinatedPackageProvisioner {
    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        self.prune_states();
        let key = self.request_key(base_image, packages, network).await?;
        let started = std::time::Instant::now();
        loop {
            if let Some(BuildState::Ready { image, .. }) = self.read_state(&key)
                && self.inner.package_image_available(&image).await?
            {
                return Ok(image);
            }
            if let Some(BuildState::Failed {
                message,
                updated_at_ms,
            }) = self.read_state(&key)
                && now_ms().saturating_sub(updated_at_ms)
                    < self.policy.failure_retry.as_millis() as u64
            {
                return Err(RuntimeError::Backend(format!(
                    "package image build is in retry backoff: {message}"
                )));
            }

            if let Some(_lease) = self.acquire(&key)? {
                // Recheck after winning the lease; another owner may have
                // completed between the prior read and the atomic create.
                if let Some(BuildState::Ready { image, .. }) = self.read_state(&key)
                    && self.inner.package_image_available(&image).await?
                {
                    return Ok(image);
                }
                match self
                    .inner
                    .prepare_package_image(base_image, packages, network)
                    .await
                {
                    Ok(image) => {
                        self.write_state(
                            &key,
                            &BuildState::Ready {
                                image: image.clone(),
                                updated_at_ms: now_ms(),
                            },
                        )?;
                        return Ok(image);
                    }
                    Err(error) => {
                        self.write_state(
                            &key,
                            &BuildState::Failed {
                                message: error.to_string(),
                                updated_at_ms: now_ms(),
                            },
                        )?;
                        return Err(error);
                    }
                }
            }
            if started.elapsed() >= self.policy.wait_timeout {
                return Err(RuntimeError::Backend(
                    "timed out waiting for the package image build lease".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn package_image_available(&self, image: &str) -> Result<bool, RuntimeError> {
        self.inner.package_image_available(image).await
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn modified_age(path: &Path) -> Option<Duration> {
    path.metadata().ok()?.modified().ok()?.elapsed().ok()
}

fn lock_is_stale(path: &Path, lease: Duration) -> bool {
    modified_age(path).is_some_and(|age| age > lease)
}

fn backend(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Builder {
        builds: AtomicUsize,
        base_generation: AtomicUsize,
    }

    #[async_trait]
    impl PackageImageProvisioner for Builder {
        async fn package_image_coordination_key(
            &self,
            base_image: &str,
            packages: &pc::PackageRequirements,
            network: &pc::NetworkPolicy,
        ) -> Result<String, RuntimeError> {
            Ok(format!(
                "{}:{}:{}:{}",
                base_image,
                self.base_generation.load(Ordering::SeqCst),
                serde_json::to_string(packages).unwrap(),
                serde_json::to_string(network).unwrap()
            ))
        }

        async fn prepare_package_image(
            &self,
            _base_image: &str,
            _packages: &pc::PackageRequirements,
            _network: &pc::NetworkPolicy,
        ) -> Result<String, RuntimeError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(format!(
                "registry.test/packages@sha256:{}",
                self.base_generation.load(Ordering::SeqCst)
            ))
        }

        async fn package_image_available(&self, image: &str) -> Result<bool, RuntimeError> {
            Ok(image.starts_with("registry.test/packages@sha256:"))
        }
    }

    #[tokio::test]
    async fn concurrent_requests_build_once_and_persist_the_digest() {
        let root = tempfile::tempdir().unwrap();
        let builder = Arc::new(Builder {
            builds: AtomicUsize::new(0),
            base_generation: AtomicUsize::new(1),
        });
        let coordinator = Arc::new(
            CoordinatedPackageProvisioner::new(
                builder.clone(),
                root.path(),
                PackageCoordinatorPolicy::default(),
            )
            .unwrap(),
        );
        let packages = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let first = coordinator.prepare_package_image(
            "base:1",
            &packages,
            &pc::NetworkPolicy::Unrestricted,
        );
        let second = coordinator.prepare_package_image(
            "base:1",
            &packages,
            &pc::NetworkPolicy::Unrestricted,
        );
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap(), second.unwrap());
        assert_eq!(builder.builds.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "one durable ready state and no leaked lock"
        );
    }

    #[tokio::test]
    async fn moving_a_mutable_base_reference_invalidates_the_ready_state() {
        let root = tempfile::tempdir().unwrap();
        let builder = Arc::new(Builder {
            builds: AtomicUsize::new(0),
            base_generation: AtomicUsize::new(1),
        });
        let coordinator = CoordinatedPackageProvisioner::new(
            builder.clone(),
            root.path(),
            PackageCoordinatorPolicy::default(),
        )
        .unwrap();
        let packages = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let first = coordinator
            .prepare_package_image("base:latest", &packages, &pc::NetworkPolicy::Unrestricted)
            .await
            .unwrap();
        builder.base_generation.store(2, Ordering::SeqCst);
        let second = coordinator
            .prepare_package_image("base:latest", &packages, &pc::NetworkPolicy::Unrestricted)
            .await
            .unwrap();

        assert_ne!(first, second, "the moved base must select a new digest");
        assert_eq!(builder.builds.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            2,
            "each resolved base identity has an independent ready state"
        );
    }
}
