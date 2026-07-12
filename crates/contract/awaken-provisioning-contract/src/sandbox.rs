//! The provisioning ports: [`SandboxProvider`] realizes a [`SandboxSpec`] into a
//! live [`Sandbox`]; [`Sandbox`] launches processes and moves files. Concrete
//! backends (lexical / bubblewrap / container) implement these in their own
//! crates and are selected by [`SandboxCapabilities`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::spec::{Command, SandboxSpec};
use crate::vocab::{Artifact, MountRequirement, RealizedMount};

/// Provisioning failure. String-carried at the boundary (like the runtime's other
/// neutral errors); a backend maps its own error into this.
#[derive(Debug, thiserror::Error)]
#[error("sandbox error: {0}")]
pub struct SandboxError(pub String);

impl SandboxError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// A content-addressed byte source a provider consults to resolve a `File`/`Resource`
/// mount's bytes by id. Dependency-inverted so the worker-tier sandbox providers stay
/// free of any durable store: the composition root injects an adapter over the
/// resources-tier content store (A-G17 — the isolated exec tier links no store).
#[async_trait]
pub trait BlobSource: Send + Sync {
    /// The bytes for content id `id`, or `None` if absent (errors are folded to
    /// `None`; a required mount that resolves to nothing fails closed downstream).
    async fn get(&self, id: &str) -> Option<Vec<u8>>;
}

/// A serializable, **durable** reference to a realized sandbox. Persist it the
/// moment a sandbox is created; a live `Box<dyn Sandbox>` cannot survive a host
/// restart, but the handle can be stored and later passed to
/// [`SandboxProvider::adopt`] to reconnect to a still-running remote sandbox
/// (k8s pod / container on another host). For a local sandbox it is just the
/// directory id. `extra` carries provider-specific locators (namespace, node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub provider_kind: String,
    pub sandbox_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

impl SandboxHandle {
    pub fn new(provider_kind: impl Into<String>, sandbox_id: impl Into<String>) -> Self {
        Self {
            provider_kind: provider_kind.into(),
            sandbox_id: sandbox_id.into(),
            extra: None,
        }
    }
}

/// The lifecycle state of a sandbox, queryable idempotently (survives reconnect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// Being realized (image pull, mounts binding).
    Provisioning,
    /// Realized and usable — processes may be spawned.
    Ready,
    /// Torn down, reaped, or lease-expired; no longer usable.
    Terminated,
}

/// Isolation strength, ordered `Workdir < Namespace < Container`. A provider
/// admits a spec only when its class is `>=` the requested one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationClass {
    /// Working-directory selection only; no OS isolation (dev/CI/trusted).
    Workdir,
    /// OS-namespace isolation (bubblewrap / sandbox-exec).
    Namespace,
    /// Full container/VM isolation.
    Container,
}

/// What a backend can actually enforce — the host probes this to pick a provider
/// and to fail closed when a spec asks for more than a backend can give.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub isolation: IsolationClass,
    /// **The load-bearing flag.** True when isolation is OS-enforced on an
    /// arbitrary launched process (so it holds for Claude Code / any CLI); false
    /// for a cooperating-tool-only jail (lexical), which must never host an opaque
    /// agent process.
    pub tool_transparent: bool,
    /// Sandbox-absolute paths are real to launched processes (vs. lexical rewrite).
    pub path_fidelity: bool,
    /// Read-only mounts are OS-enforced.
    pub enforced_readonly: bool,
    /// Egress can be isolated/controlled.
    pub network_isolation: bool,
    /// `EnvVisibility::EgressOnly` secrets can be honored.
    pub secret_egress_substitution: bool,
    /// Resource limits are enforced.
    pub resource_limits: bool,
    /// Provides its own userland/rootfs (vs. borrowing the host's binaries).
    pub custom_rootfs: bool,
}

impl SandboxCapabilities {
    /// Fail-closed backend selection (ADR-0021 §8): does this backend meet
    /// **everything** `spec` requires? A router filters candidate providers by this
    /// before applying any load/region/affinity policy, so a spec is never placed on
    /// a backend that cannot honor it.
    ///
    /// Matches the two load-bearing axes the vocabulary makes selectable: isolation
    /// class (the provider must *meet or exceed* the requested minimum) and network
    /// isolation (required for anything stricter than [`NetworkPolicy::Unrestricted`]).
    #[must_use]
    pub fn satisfies(&self, spec: &crate::spec::SandboxSpec) -> bool {
        use crate::vocab::NetworkPolicy;
        self.isolation >= spec.isolation
            && (matches!(spec.network, NetworkPolicy::Unrestricted) || self.network_isolation)
    }
}

/// Realizes environments. The local impl lives in `awaken-sandbox-local`; a
/// remote/container impl lives in a distributed repo and plugs in here.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// What this backend can enforce (probed at startup for selection).
    fn capabilities(&self) -> SandboxCapabilities;

    /// Realize a validated spec into a live sandbox (bind mounts, apply ro/env/
    /// network/limits). Callers should validate first via `prepare_environment`.
    async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError>;

    /// Reconnect to an already-realized sandbox from a persisted [`SandboxHandle`]
    /// — the recovery path after a host restart, and the takeover path across
    /// hosts. For a local backend this re-opens the directory; for a remote one it
    /// rebuilds a client against the still-running pod/container.
    async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError>;
}

/// A live sandbox environment. **Execute** (`spawn`), **mount/inject** (`attach`),
/// **retrieve** (`artifacts`/`read_artifact`), and — for sandboxes that outlive the
/// owning host — **reconnect** (`handle`/`process`), **observe** (`status`), and
/// **keep alive** (`renew_lease`). `spawn` is primary and tool-transparent: the
/// runtime's `RawTool` model is a separate crate's adapter over `spawn`, not a
/// method here.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The environment id (= the spec scope).
    fn id(&self) -> &str;

    /// A durable, serializable reference for reconnecting later (persist this).
    fn handle(&self) -> SandboxHandle;

    /// **EXECUTE** — launch any process under OS-enforced isolation. Isolation is
    /// transparent to what the process does inside.
    async fn spawn(&self, command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError>;

    /// **INJECT** — attach a mount after creation (mirrors adding a session
    /// resource). Fails closed when the backend cannot honor the access mode.
    async fn attach(&self, req: MountRequirement) -> Result<RealizedMount, SandboxError>;

    /// **RETRIEVE (list)** — artifacts the agent wrote under the outputs path.
    /// The backend decides how (directory scan / copy-out / volume read).
    async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError>;

    /// **RETRIEVE (read)** — the bytes of one artifact by id.
    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, SandboxError>;

    /// The mounts realized so far — logical refs + content hashes (G3), for audit
    /// and replay.
    fn realized(&self) -> &[RealizedMount];

    /// Reconnect to a process launched earlier in this sandbox, by its id — the
    /// recovery path after a dropped connection or host restart (pair with
    /// [`ProcessHandle::poll`] to learn its outcome idempotently).
    async fn process(&self, process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError>;

    /// The sandbox's current lifecycle state — an idempotent query, safe to call
    /// from any host after a reconnect.
    async fn status(&self) -> Result<SandboxStatus, SandboxError>;

    /// Renew the lease (the dead-man's switch). The owner calls this within the
    /// spec's `lease_ttl_secs`; if the owner vanishes and the lease expires, the
    /// backend reaps the sandbox. A local backend implements this as a no-op.
    async fn renew_lease(&self) -> Result<(), SandboxError>;

    /// Tear down the environment. Idempotent; `Durable` mounts persist.
    async fn dispose(&self) -> Result<(), SandboxError>;
}

/// A handle to a process launched by [`Sandbox::spawn`]. Lifecycle only — piped
/// stdio (for a protocol bridge such as ACP) is exposed by the provider's own
/// handle type, so the neutral contract needn't bind an async-IO abstraction.
#[async_trait]
pub trait ProcessHandle: Send + Sync {
    /// Provider-assigned process id.
    fn id(&self) -> &str;

    /// Await exit. Over a lossy transport this connection may drop mid-run; treat a
    /// transport error as "unknown" and re-establish via [`Sandbox::process`] +
    /// [`ProcessHandle::poll`] rather than assuming failure.
    async fn wait(&self) -> Result<ExitStatus, SandboxError>;

    /// Non-blocking, idempotent status: `None` while still running, `Some(status)`
    /// once exited. Safe to call repeatedly from any host after a reconnect — this
    /// is how you resolve an indeterminate outcome without re-running the process.
    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError>;

    /// Deliver a signal (terminate/kill/interrupt). Tearing down the sandbox reaps
    /// the whole process group regardless.
    async fn signal(&self, signal: Signal) -> Result<(), SandboxError>;
}

/// How a launched process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Exit code, when it exited normally.
    pub code: Option<i32>,
    /// True when terminated by a signal.
    pub signaled: bool,
}

/// A signal to deliver to a launched process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Graceful terminate (SIGTERM).
    Term,
    /// Force kill (SIGKILL).
    Kill,
    /// Interrupt (SIGINT).
    Int,
}

#[cfg(test)]
mod tests {
    //! A trivial fake exercises the full lifecycle — create → persist handle →
    //! (simulated host restart) adopt → spawn → poll → renew_lease → dispose —
    //! which also proves the ports stay object-safe (`Box<dyn …>`).

    use super::*;
    use crate::spec::{Command, SandboxSpec};
    use crate::vocab::NetworkPolicy;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FakeProcess {
        id: String,
    }

    #[async_trait]
    impl ProcessHandle for FakeProcess {
        fn id(&self) -> &str {
            &self.id
        }
        async fn wait(&self) -> Result<ExitStatus, SandboxError> {
            Ok(ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }
        async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
            Ok(Some(ExitStatus {
                code: Some(0),
                signaled: false,
            }))
        }
        async fn signal(&self, _signal: Signal) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    struct FakeSandbox {
        id: String,
        renews: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        fn handle(&self) -> SandboxHandle {
            SandboxHandle::new("fake", &self.id)
        }
        async fn spawn(&self, _command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Ok(Box::new(FakeProcess {
                id: "proc-1".into(),
            }))
        }
        async fn attach(&self, _req: MountRequirement) -> Result<RealizedMount, SandboxError> {
            Err(SandboxError::new("fake has no mounts"))
        }
        async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
            Ok(Vec::new())
        }
        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, SandboxError> {
            Ok(Vec::new())
        }
        fn realized(&self) -> &[RealizedMount] {
            &[]
        }
        async fn process(&self, process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Ok(Box::new(FakeProcess {
                id: process_id.into(),
            }))
        }
        async fn status(&self) -> Result<SandboxStatus, SandboxError> {
            Ok(SandboxStatus::Ready)
        }
        async fn renew_lease(&self) -> Result<(), SandboxError> {
            self.renews.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn dispose(&self) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    struct FakeProvider {
        renews: Arc<AtomicU32>,
    }

    #[async_trait]
    impl SandboxProvider for FakeProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                isolation: IsolationClass::Workdir,
                tool_transparent: false,
                path_fidelity: false,
                enforced_readonly: false,
                network_isolation: false,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
            }
        }
        async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: spec.scope.clone(),
                renews: self.renews.clone(),
            }))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            Ok(Box::new(FakeSandbox {
                id: handle.sandbox_id.clone(),
                renews: self.renews.clone(),
            }))
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "thread-1".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: Some(60),
            extra: None,
        }
    }

    fn caps(isolation: IsolationClass, network_isolation: bool) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: false,
        }
    }

    #[test]
    fn satisfies_requires_meeting_or_exceeding_isolation() {
        let mut s = spec();
        s.isolation = IsolationClass::Namespace;
        // exact and stronger classes satisfy
        assert!(caps(IsolationClass::Namespace, false).satisfies(&s));
        assert!(caps(IsolationClass::Container, false).satisfies(&s));
        // weaker fails closed
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
    }

    #[test]
    fn satisfies_requires_network_isolation_for_restricted_egress() {
        let mut s = spec();
        s.network = NetworkPolicy::None;
        assert!(!caps(IsolationClass::Workdir, false).satisfies(&s));
        assert!(caps(IsolationClass::Workdir, true).satisfies(&s));
        // unrestricted egress needs no network isolation
        s.network = NetworkPolicy::Unrestricted;
        assert!(caps(IsolationClass::Workdir, false).satisfies(&s));
    }

    #[tokio::test]
    async fn create_persist_adopt_poll_lease_lifecycle() {
        let renews = Arc::new(AtomicU32::new(0));
        let provider: Box<dyn SandboxProvider> = Box::new(FakeProvider {
            renews: renews.clone(),
        });

        // Create, then persist the durable handle (as a host would to its store).
        let sandbox = provider.create(&spec()).await.unwrap();
        let handle = sandbox.handle();
        let wire = serde_json::to_string(&handle).unwrap(); // handle is serializable
        drop(sandbox); // simulate the owning host process going away

        // Recovery: reconnect from the persisted handle alone.
        let recovered: SandboxHandle = serde_json::from_str(&wire).unwrap();
        let sandbox = provider.adopt(&recovered).await.unwrap();
        assert_eq!(sandbox.id(), "thread-1");
        assert!(matches!(
            sandbox.status().await.unwrap(),
            SandboxStatus::Ready
        ));

        // Launch an opaque process (e.g. Claude Code), then resolve its outcome
        // idempotently via poll — the reconnect-safe path.
        let proc = sandbox
            .spawn(Command::new(["claude", "--acp"]))
            .await
            .unwrap();
        let reattached = sandbox.process(proc.id()).await.unwrap();
        assert!(matches!(
            reattached.poll().await.unwrap(),
            Some(ExitStatus { code: Some(0), .. })
        ));

        sandbox.renew_lease().await.unwrap();
        assert_eq!(renews.load(Ordering::SeqCst), 1);
        sandbox.dispose().await.unwrap();
    }
}
