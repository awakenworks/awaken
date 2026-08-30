use super::*;


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

/// Last-mile credential broker shared by secret files and process-secret
/// requirements. The two operations are intentionally distinct: a durable file
/// reference may support refresh/write-back, while a process reference is normally
/// short-lived, claim-fenced, one-shot, and never valid as a file identifier. Secret
/// bytes never enter a [`SandboxSpec`].
#[async_trait]
pub trait SecretBroker: Send + Sync {
    /// Materialize the current credential file bytes for `reference`.
    async fn materialize(&self, reference: &str) -> Result<Vec<u8>, SandboxError>;

    /// Consume a process-scoped requirement immediately before launch. An
    /// implementation must validate the reference as a process capability; it must
    /// not silently reinterpret an arbitrary durable file/credential id.
    async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, SandboxError>;

    /// Atomically persist a CLI-refreshed credential file under `reference`.
    async fn write_back(&self, reference: &str, bytes: Vec<u8>) -> Result<(), SandboxError>;
}

/// Realizes a [`MountSource::MemoryStore`](crate::vocab::MountSource::MemoryStore)
/// into a sandbox at a provider-resolved host path — the path-addressed counterpart
/// of [`BlobSource`] (a store is a keyed filesystem, not one blob). The FUSE / copy
/// impl lives in the worker tier (`awaken-sandbox-memoryd`); the composition root
/// injects it, so the providers stay free of the store and FUSE deps (A-G17). A
/// provider with no mounter fails a `MemoryStore` mount loud rather than fake it.
#[async_trait]
pub trait MemoryMounter: Send + Sync {
    /// Expose `store_id` at `host_path` with `access`, returning a live handle whose
    /// [`realization`](MemoryMount::realization) the provider records. The handle is
    /// held for the sandbox's life; dropping it (via [`teardown`](MemoryMount::teardown))
    /// unmounts a FUSE mount or harvests a writable copy back to the store.
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: MountAccess,
    ) -> Result<Box<dyn MemoryMount>, SandboxError>;

    /// Reconcile the current files read from an adopted copy-backed sandbox.
    /// A hard process crash loses the original in-process [`MemoryMount`] guard,
    /// while the long-lived sandbox and its files remain. Providers call this only
    /// at the recovered Session's terminal edge; implementations retain the same
    /// conflict-safe durable-head rules as ordinary copy teardown.
    async fn reconcile_recovered_copy(
        &self,
        _store_id: &str,
        _files: &[(String, Vec<u8>)],
        _access: MountAccess,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "recovered Memory copy reconciliation is unsupported",
        ))
    }
}

/// A live memory-store mount, held for the sandbox's lifetime.
#[async_trait]
pub trait MemoryMount: Send + Sync {
    /// How the store was exposed (`Fuse` where the kernel supports it, else `Copy`).
    fn realization(&self) -> Realization;

    /// Tear down: unmount the FUSE, or (for a writable copy) harvest edits back to
    /// the durable store. Idempotent and best-effort — a teardown fault is logged,
    /// not surfaced, since the sandbox is already being disposed.
    async fn teardown(self: Box<Self>);
}

/// Secret-free, per-Sandbox projection of one already-resolved Repository config.
/// It pins configuration semantics only. Branch is a clone preference; commit is
/// an exact immutable checkout. Principal, role, API key, policy, Workspace
/// hierarchy, and credential bytes are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRealizationPlan {
    pub repository_id: String,
    pub mount_path: String,
    /// Frozen upstream/source identity from the resolved Repository config.
    /// This is the canonical URL recorded in publication receipts.
    pub source_remote_url: String,
    /// Already-authorized effect endpoint. Direct transports equal
    /// `source_remote_url`; mediated transports use the Gateway endpoint.
    pub transport_url: String,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
    pub access: MountAccess,
}

/// Ephemeral Basic-auth value translated from an already-admitted credential at
/// the Repository boundary. It is deliberately non-serializable and absent from
/// [`RepositoryRealizationPlan`]; only the target adapter may expose its fields.
#[derive(Debug, Clone)]
pub struct RepositoryHttpBasicCredential {
    username: awaken_agent_contract::RedactedString,
    password: awaken_agent_contract::RedactedString,
    source: RepositoryHttpBasicCredentialSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepositoryHttpBasicCredentialSource {
    Upstream,
    GatewayCapability,
}

impl RepositoryHttpBasicCredential {
    #[must_use]
    pub fn new(
        username: impl Into<awaken_agent_contract::RedactedString>,
        password: impl Into<awaken_agent_contract::RedactedString>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            source: RepositoryHttpBasicCredentialSource::Upstream,
        }
    }

    /// A short-lived platform Gateway capability, not an upstream Repository
    /// credential. The distinction lets the target adapter admit an in-cluster
    /// Gateway endpoint without weakening HTTPS for upstream secrets.
    #[must_use]
    pub fn gateway_capability(
        capability: impl Into<awaken_agent_contract::RedactedString>,
    ) -> Self {
        Self {
            username: "git".to_owned().into(),
            password: capability.into(),
            source: RepositoryHttpBasicCredentialSource::GatewayCapability,
        }
    }

    #[must_use]
    pub fn is_gateway_capability(&self) -> bool {
        self.source == RepositoryHttpBasicCredentialSource::GatewayCapability
    }

    #[must_use]
    pub fn expose_username(&self) -> &str {
        self.username.expose_secret()
    }

    #[must_use]
    pub fn expose_password(&self) -> &str {
        self.password.expose_secret()
    }
}

/// Environment-side adapter for a mutable Repository input. Authorization and
/// configuration resolution happen before this port is called; implementations
/// only construct/use a working tree. A credential value is injected ephemerally
/// for the transport operation and must never be persisted in the plan, origin URL,
/// or sandbox.
#[async_trait]
pub trait RepositoryRealizer: Send + Sync {
    /// Clone the current remote content into this realizer's Sandbox.
    async fn realize_repository(
        &self,
        plan: &RepositoryRealizationPlan,
        credential: Option<&RepositoryHttpBasicCredential>,
    ) -> Result<(), SandboxError>;

    /// Publish one exact Agent-authored branch/commit coordinate. The caller
    /// decides whether publishing is allowed/required; this adapter owns only Git
    /// transport mechanics. First publication and exact replay return the same
    /// deterministic, secret-free receipt.
    async fn publish_repository(
        &self,
        plan: &RepositoryRealizationPlan,
        expectation: &RepositoryPublicationExpectation,
        credential: Option<&RepositoryHttpBasicCredential>,
    ) -> Result<RepositoryPublicationReceipt, RepositoryPublicationError>;
}
