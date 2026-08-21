//! Product-plane preparation of caller-owned [`pc::MountSource::CacheVolume`]
//! directories. Providers still only mount opaque paths; this module owns the
//! one content-keyed preparation path used by explicit startup warmup and by
//! Session creation.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheVolumeIdentity {
    key: String,
    location: pc::CacheVolumeLocation,
}

/// One caller-owned, rebuildable cache directory to prepare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheVolumeWarmup {
    pub key: String,
    pub location: pc::CacheVolumeLocation,
}

impl CacheVolumeWarmup {
    #[must_use]
    pub fn new(key: impl Into<String>, location: pc::CacheVolumeLocation) -> Self {
        Self {
            key: key.into(),
            location,
        }
    }

    #[must_use]
    pub fn host_path(key: impl Into<String>, path: impl Into<std::path::PathBuf>) -> Self {
        Self::new(
            key,
            pc::CacheVolumeLocation::HostPath {
                path: path.into().to_string_lossy().into_owned(),
            },
        )
    }

    fn identity(&self) -> CacheVolumeIdentity {
        CacheVolumeIdentity {
            key: self.key.clone(),
            location: self.location.clone(),
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
/// deployments that know how to populate it install a richer initializer.
struct FilesystemCacheVolumeInitializer;

#[async_trait]
impl CacheVolumeInitializer for FilesystemCacheVolumeInitializer {
    async fn initialize(&self, volume: &CacheVolumeWarmup) -> Result<(), String> {
        match &volume.location {
            pc::CacheVolumeLocation::HostPath { path } => {
                if path.trim().is_empty() {
                    return Err("host path is empty".into());
                }
                let path = std::path::PathBuf::from(path);
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&path))
                    .await
                    .map_err(|error| format!("directory preparation task failed: {error}"))?
                    .map_err(|error| format!("create cache directory: {error}"))
            }
            pc::CacheVolumeLocation::PersistentVolumeClaim { .. } => {
                Err("Kubernetes CacheVolume requires an installed PVC initializer".into())
            }
        }
    }
}

/// Kubernetes product-plane initializer built on the already-authoritative
/// container provider. It creates one short-lived mounted environment, writes a
/// content-key marker atomically, waits for success, and disposes the Pod. The
/// container adapter still owns only mount/process mechanics; this owner decides
/// when a CacheVolume becomes publishable as prepared.
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
pub(crate) struct SandboxCacheVolumeInitializer {
    provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
impl SandboxCacheVolumeInitializer {
    #[must_use]
    pub(crate) fn new(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
    ) -> Self {
        Self { provider }
    }
}

#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
#[async_trait]
impl CacheVolumeInitializer for SandboxCacheVolumeInitializer {
    async fn initialize(&self, volume: &CacheVolumeWarmup) -> Result<(), String> {
        if !matches!(
            volume.location,
            pc::CacheVolumeLocation::PersistentVolumeClaim { .. }
        ) {
            return Err("sandbox CacheVolume initializer requires a Kubernetes PVC".into());
        }
        let fingerprint =
            awaken_agent_contract::stable_fingerprint(&(volume.key.as_str(), &volume.location));
        let marker = format!("/cache/.awaken/{fingerprint}.ready");
        let spec = pc::SandboxSpec {
            scope: format!("cache-init-{}", &fingerprint[..24]),
            isolation: pc::IsolationClass::Container,
            mounts: vec![pc::MountRequirement {
                mount_id: "cache-volume".into(),
                source: pc::MountSource::CacheVolume {
                    location: volume.location.clone(),
                    key: volume.key.clone(),
                },
                mount_path: "/cache".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::Durable,
                required: true,
            }],
            env: Vec::new(),
            packages: Default::default(),
            // The fixed initializer does not perform network I/O. K8s currently
            // admits only Open mode unless an external NetworkPolicy is proven.
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        };
        let environment = self
            .provider
            .create_environment(&spec)
            .await
            .map_err(|error| format!("create CacheVolume initializer Pod: {error}"))?;
        let script = format!(
            "set -eu; mkdir -p /cache/.awaken; if [ ! -f {marker} ]; then \
             tmp={marker}.tmp.$$; printf ready > \"$tmp\"; mv \"$tmp\" {marker}; fi"
        );
        let outcome = match pc::Sandbox::spawn(
            environment.as_ref(),
            pc::Command {
                argv: vec!["sh".into(), "-c".into(), script],
                cwd: "/cache".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Null,
            },
        )
        .await
        {
            Ok(process) => process
                .wait()
                .await
                .map_err(|error| format!("wait for CacheVolume initializer: {error}"))
                .and_then(|status| {
                    (status.code == Some(0) && !status.signaled)
                        .then_some(())
                        .ok_or_else(|| format!("CacheVolume initializer exited with {status:?}"))
                }),
            Err(error) => Err(format!("spawn CacheVolume initializer: {error}")),
        };
        let disposed = pc::Sandbox::dispose(environment.as_ref())
            .await
            .map_err(|error| format!("dispose CacheVolume initializer Pod: {error}"));
        outcome.and(disposed)
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
            let location = match &volume.location {
                pc::CacheVolumeLocation::HostPath { path } => format!("host path `{path}`"),
                pc::CacheVolumeLocation::PersistentVolumeClaim { claim_name } => {
                    format!("PVC `{claim_name}`")
                }
            };
            format!(
                "prepare cache volume `{}` at {location}: {error}",
                volume.key
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
            if let pc::MountSource::CacheVolume { location, key } = &mount.source {
                self.prewarm(CacheVolumeWarmup::new(key, location.clone()))
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    use std::sync::Mutex;
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

    /// FMECA: FM1 concurrent equal demand starts duplicate initialization and
    /// corrupts or wastes shared cache capacity; FM2 distinct keys collapse to
    /// one Ready result and expose stale contents. Cause/effect graph:
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
        let a = CacheVolumeWarmup::host_path("deps-v1", "/tmp/awaken-cache-a");
        let b = CacheVolumeWarmup::host_path("deps-v2", "/tmp/awaken-cache-a");

        let (first, duplicate, distinct) = tokio::join!(
            prewarmer.prewarm(a.clone()),
            prewarmer.prewarm(a),
            prewarmer.prewarm(b),
        );

        assert!(first.is_ok() && duplicate.is_ok() && distinct.is_ok());
        assert_eq!(initializer.calls.load(Ordering::SeqCst), 2);
    }

    /// FMECA: FM1 a failed initialization is cached as Ready, making later
    /// Sessions consume incomplete contents; FM2 retry never becomes reusable.
    /// Cause/effect graph:
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
        let volume = CacheVolumeWarmup::host_path("repo-v1", "/tmp/awaken-cache-retry");

        assert!(prewarmer.prewarm(volume.clone()).await.is_err());
        assert!(prewarmer.prewarm(volume.clone()).await.is_ok());
        assert!(prewarmer.prewarm(volume).await.is_ok());
        assert_eq!(initializer.calls.load(Ordering::SeqCst), 2);
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    struct FinishedProcess(i32);

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    #[async_trait]
    impl pc::ProcessHandle for FinishedProcess {
        fn id(&self) -> &str {
            "cache-init"
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(self.0),
                signaled: false,
            })
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(Some(self.wait().await?))
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    struct RecordingEnvironment {
        commands: Arc<Mutex<Vec<pc::Command>>>,
        disposals: Arc<AtomicUsize>,
        exit_code: i32,
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    #[async_trait]
    impl pc::Sandbox for RecordingEnvironment {
        fn id(&self) -> &str {
            "cache-init"
        }

        fn handle(&self) -> pc::SandboxHandle {
            pc::SandboxHandle::new("recording", self.id())
        }

        async fn spawn(
            &self,
            command: pc::Command,
        ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
            self.commands.lock().unwrap().push(command);
            Ok(Box::new(FinishedProcess(self.exit_code)))
        }

        async fn attach(
            &self,
            _requirement: pc::MountRequirement,
        ) -> Result<pc::RealizedMount, pc::SandboxError> {
            Err(pc::SandboxError::new("unused"))
        }

        async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
            Ok(Vec::new())
        }

        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::new("unused"))
        }

        fn realized(&self) -> &[pc::RealizedMount] {
            &[]
        }

        async fn process(
            &self,
            _process_id: &str,
        ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
            Ok(Box::new(FinishedProcess(self.exit_code)))
        }

        async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
            Ok(pc::SandboxStatus::Ready)
        }

        async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), pc::SandboxError> {
            self.disposals.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironment for RecordingEnvironment {
        async fn spawn_agent_process(
            &self,
            _command: pc::Command,
        ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
            Err(pc::SandboxError::new("unused"))
        }

        async fn read_files(
            &self,
            _root: &str,
        ) -> Result<Vec<awaken_sandbox_container::EnvironmentFile>, pc::SandboxError> {
            Ok(Vec::new())
        }
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    struct RecordingProvider {
        specs: Arc<Mutex<Vec<pc::SandboxSpec>>>,
        commands: Arc<Mutex<Vec<pc::Command>>>,
        disposals: Arc<AtomicUsize>,
        exit_code: i32,
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironmentProvider for RecordingProvider {
        async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
            Ok(())
        }

        async fn create_environment(
            &self,
            spec: &pc::SandboxSpec,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            self.specs.lock().unwrap().push(spec.clone());
            Ok(Arc::new(RecordingEnvironment {
                commands: self.commands.clone(),
                disposals: self.disposals.clone(),
                exit_code: self.exit_code,
            }))
        }

        async fn adopt_environment(
            &self,
            _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            Err(pc::SandboxError::new("unused"))
        }
    }

    #[cfg(any(
        feature = "container-docker",
        feature = "container-podman",
        feature = "container-k8s"
    ))]
    #[tokio::test]
    async fn pvc_initializer_publishes_only_after_exact_mounted_process_success() {
        // FMECA: F1 success is published before the PVC process exits
        // (S9,O4,D3,RPN108); F2 a host path is sent to the K8s initializer
        // (S8,O3,D2,RPN48); F3 a failed process leaks a Pod or caches Ready
        // (S8,O5,D3,RPN120). Causes: C1=PVC; C2=host path; C3=exit zero;
        // C4=exit nonzero. Effects: E1=exact PVC mounted and atomic marker command
        // completes; E2=reject before create; E3=error; E4=always dispose.
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | K1   | 1  | 0  | 1  | 0  | E1,E4  |
        // | K2   | 0  | 1  | -  | -  | E2     |
        // | K3   | 1  | 0  | 0  | 1  | E3,E4  |
        let specs = Arc::new(Mutex::new(Vec::new()));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let disposals = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(RecordingProvider {
            specs: specs.clone(),
            commands: commands.clone(),
            disposals: disposals.clone(),
            exit_code: 0,
        });
        let initializer = SandboxCacheVolumeInitializer::new(provider);
        let pvc = CacheVolumeWarmup::new(
            "deps-v1",
            pc::CacheVolumeLocation::PersistentVolumeClaim {
                claim_name: "cache-pvc".into(),
            },
        );
        initializer.initialize(&pvc).await.expect("K1");
        assert_eq!(specs.lock().unwrap().len(), 1, "K1");
        assert!(matches!(
            &specs.lock().unwrap()[0].mounts[0].source,
            pc::MountSource::CacheVolume {
                location: pc::CacheVolumeLocation::PersistentVolumeClaim { claim_name },
                ..
            } if claim_name == "cache-pvc"
        ));
        assert!(commands.lock().unwrap()[0].argv[2].contains(".ready"), "K1");
        assert_eq!(disposals.load(Ordering::SeqCst), 1, "K1 E4");

        assert!(
            initializer
                .initialize(&CacheVolumeWarmup::host_path("deps-v1", "/tmp/cache"))
                .await
                .is_err(),
            "K2"
        );
        assert_eq!(specs.lock().unwrap().len(), 1, "K2 before create");

        let failing_disposals = Arc::new(AtomicUsize::new(0));
        let failing = SandboxCacheVolumeInitializer::new(Arc::new(RecordingProvider {
            specs: Arc::new(Mutex::new(Vec::new())),
            commands: Arc::new(Mutex::new(Vec::new())),
            disposals: failing_disposals.clone(),
            exit_code: 7,
        }));
        assert!(failing.initialize(&pvc).await.is_err(), "K3 E3");
        assert_eq!(failing_disposals.load(Ordering::SeqCst), 1, "K3 E4");
    }

    #[cfg(feature = "container-k8s")]
    #[tokio::test]
    async fn k8s_live_pvc_initialization_is_readable_and_reused() {
        // Live FMECA/decision rule K4 extends the unit table above:
        // C1=real PVC bound; C2=initializer exits zero; C3=same key is requested
        // again; C4=a Session Pod mounts that PVC. Effects: E1=atomic marker is
        // visible inside the Session; E2=the in-process success is reused; E3=no
        // initializer Pod survives. K4=(C1,C2,C3,C4)->(E1,E2,E3).
        if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
            eprintln!("skipping: set AWAKEN_K8S_E2E=1 to require live PVC initialization");
            return;
        }
        let status = std::process::Command::new("kubectl")
            .args(["get", "nodes"])
            .status()
            .expect("AWAKEN_K8S_E2E=1 requires kubectl");
        assert!(status.success(), "AWAKEN_K8S_E2E=1 requires a cluster");

        let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
        let claim = format!("awaken-cache-e2e-{}", std::process::id());
        let manifest = format!(
            "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: {claim}\n  namespace: {namespace}\nspec:\n  accessModes: [ReadWriteOnce]\n  resources:\n    requests:\n      storage: 16Mi\n"
        );
        let mut apply = std::process::Command::new("kubectl")
            .args(["apply", "-f", "-"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("start kubectl apply");
        use std::io::Write as _;
        apply
            .stdin
            .take()
            .unwrap()
            .write_all(manifest.as_bytes())
            .unwrap();
        assert!(apply.wait().unwrap().success(), "create test PVC");

        let runtime = Arc::new(
            awaken_sandbox_container::k8s::K8sRuntime::connect(
                namespace.clone(),
                "127.0.0.1:1".parse().unwrap(),
            )
            .await
            .expect("connect to Kubernetes"),
        );
        let image =
            std::env::var("AWAKEN_K8S_FIXTURE_IMAGE").unwrap_or_else(|_| "awaken-bb:1".into());
        let provider = Arc::new(awaken_sandbox_container::ContainerProvider::new(
            runtime, image,
        ));
        let location = pc::CacheVolumeLocation::PersistentVolumeClaim {
            claim_name: claim.clone(),
        };
        let warmup = CacheVolumeWarmup::new("live-v1", location.clone());
        let prewarmer = CacheVolumePrewarmer::new(Arc::new(SandboxCacheVolumeInitializer::new(
            provider.clone(),
        )));
        prewarmer.prewarm(warmup.clone()).await.expect("K4 init");
        prewarmer.prewarm(warmup.clone()).await.expect("K4 reuse");

        let fingerprint =
            awaken_agent_contract::stable_fingerprint(&(warmup.key.as_str(), &warmup.location));
        let session_spec = pc::SandboxSpec {
            scope: format!("cache-reader-{}", std::process::id()),
            isolation: pc::IsolationClass::Container,
            mounts: vec![pc::MountRequirement {
                mount_id: "cache".into(),
                source: pc::MountSource::CacheVolume {
                    location,
                    key: warmup.key,
                },
                mount_path: "/cache".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::Durable,
                required: true,
            }],
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        };
        let environment =
            awaken_sandbox_container::ContainerEnvironmentProvider::create_environment(
                provider.as_ref(),
                &session_spec,
            )
            .await
            .expect("K4 Session mounts initialized PVC");
        let process = pc::Sandbox::spawn(
            environment.as_ref(),
            pc::Command {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("test \"$(cat /cache/.awaken/{fingerprint}.ready)\" = ready"),
                ],
                cwd: "/cache".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Null,
            },
        )
        .await
        .expect("spawn marker reader");
        assert_eq!(process.wait().await.unwrap().code, Some(0), "K4 E1");
        environment.dispose().await.unwrap();
        assert!(
            std::process::Command::new("kubectl")
                .args(["delete", "pvc", &claim, "-n", &namespace, "--wait=true",])
                .status()
                .unwrap()
                .success(),
            "cleanup PVC"
        );
    }
}
