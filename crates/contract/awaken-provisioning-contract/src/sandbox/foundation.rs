//! Repository, Memory-materialization, and sandbox-path contracts.

use std::path::Path;

use async_trait::async_trait;
use awaken_resource_contract::InputResourceId;
use serde::{Deserialize, Serialize};

use super::runtime::SandboxEffectFence;
use super::{
    RepositoryPublicationError, RepositoryPublicationExpectation, RepositoryPublicationReceipt,
};
use crate::spec::SandboxSpec;
use crate::vocab::{MountAccess, Realization};

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

/// One credential write-back command whose authorization and durable
/// idempotency identities are deliberately independent.
///
/// `authorization` is the current aggregate-owned Sandbox fence and must still
/// be live when the broker performs I/O. `writeback_id` is instead derived only
/// from the exact credential reference and immutable physical Sandbox
/// incarnation. A successor aggregate operation for the same physical Sandbox
/// therefore replays the credential authority's existing WAL/CAS command after
/// response loss, while another reference or rebuilt Sandbox cannot alias it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretWritebackEffect {
    reference: String,
    physical_incarnation: String,
    authorization: SandboxEffectFence,
    writeback_id: String,
}

impl SecretWritebackEffect {
    pub fn new(
        reference: impl Into<String>,
        physical_incarnation: impl Into<String>,
        authorization: SandboxEffectFence,
    ) -> Result<Self, SandboxError> {
        let reference = reference.into();
        let physical_incarnation = physical_incarnation.into();
        if reference.trim().is_empty() {
            return Err(SandboxError::new(
                "Secret write-back requires a non-empty credential reference",
            ));
        }
        if physical_incarnation.trim().is_empty() {
            return Err(SandboxError::new(
                "Secret write-back requires a non-empty physical Sandbox incarnation",
            ));
        }
        authorization.validate_identity()?;
        let writeback_id = awaken_agent_contract::stable_fingerprint(&(
            "sandbox-secret-writeback-v1",
            reference.as_str(),
            physical_incarnation.as_str(),
        ));
        Ok(Self {
            reference,
            physical_incarnation,
            authorization,
            writeback_id,
        })
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn physical_incarnation(&self) -> &str {
        &self.physical_incarnation
    }

    #[must_use]
    pub fn authorization(&self) -> &SandboxEffectFence {
        &self.authorization
    }

    #[must_use]
    pub fn writeback_id(&self) -> &str {
        &self.writeback_id
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

    /// Idempotently persist a CLI-refreshed credential file under one typed
    /// physical write-back identity and the current aggregate authorization.
    /// Durable Session cleanup must use this seam so a successor operation can
    /// recognize a lost response for the same exact Sandbox without granting a
    /// rebuilt/foreign Sandbox the predecessor's credential mutation. The
    /// legacy method above remains only for explicitly unmanaged lifecycles.
    async fn write_back_for_effect(
        &self,
        _effect: &SecretWritebackEffect,
        _bytes: Vec<u8>,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "effect-fenced credential write-back is unsupported",
        ))
    }
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
        _operation_reference: &str,
        _evidence: &MemoryMaterializationEvidence,
        _files: &[(String, Vec<u8>)],
        _access: MountAccess,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "recovered Memory copy reconciliation is unsupported",
        ))
    }
}

/// Immutable durable-head identity captured when a copy-backed Memory mount is
/// materialized. Content is deliberately excluded: the surviving Sandbox copy
/// owns the candidate bytes, while these coordinates preserve the original CAS
/// base across Worker loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryMaterializationHead {
    pub path: String,
    pub id: String,
    pub content_sha256: String,
}

/// Provider-neutral evidence for one copy-backed Memory mount. The logical
/// store and mount path correlate this evidence with the already-frozen Session
/// input; transport capabilities and Run claims are never persisted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryMaterializationEvidence {
    pub store_id: String,
    pub mount_path: String,
    pub heads: Vec<MemoryMaterializationHead>,
}

impl MemoryMaterializationEvidence {
    /// Construct the one canonical durable representation for a copy-backed
    /// mount. Providers call this before physical create so malformed mounter
    /// evidence cannot reach a later handle serialization edge.
    pub fn new(
        store_id: impl Into<String>,
        mount_path: impl Into<String>,
        mut heads: Vec<MemoryMaterializationHead>,
    ) -> Result<Self, SandboxError> {
        heads.sort_by(|left, right| left.path.cmp(&right.path));
        let evidence = Self {
            store_id: store_id.into(),
            mount_path: mount_path.into(),
            heads,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    /// Validate deserialized or adapter-provided evidence without normalizing
    /// it. Canonical ordering is part of the durable binding identity.
    pub fn validate(&self) -> Result<(), SandboxError> {
        if self.store_id.trim().is_empty() || self.mount_path.trim().is_empty() {
            return Err(SandboxError::new(
                "Memory materialization evidence requires store and mount identities",
            ));
        }
        let mut previous_path = None;
        for head in &self.heads {
            if head.path.trim().is_empty()
                || head.id.trim().is_empty()
                || head.content_sha256.trim().is_empty()
                || previous_path.is_some_and(|previous| previous >= head.path.as_str())
            {
                return Err(SandboxError::new(
                    "Memory materialization heads are invalid or not canonically ordered",
                ));
            }
            previous_path = Some(head.path.as_str());
        }
        Ok(())
    }

    pub(super) fn canonicalize_all(evidence: &mut [Self]) -> Result<(), SandboxError> {
        for item in evidence.iter() {
            item.validate()?;
        }
        evidence.sort_by(|left, right| {
            (&left.mount_path, &left.store_id).cmp(&(&right.mount_path, &right.store_id))
        });
        if evidence.windows(2).any(|pair| {
            pair[0].mount_path == pair[1].mount_path && pair[0].store_id == pair[1].store_id
        }) {
            return Err(SandboxError::new(
                "Memory materialization evidence contains a duplicate mount",
            ));
        }
        Ok(())
    }

    /// Validate the complete canonical provider evidence sequence without
    /// rewriting it. Cross-context joins use this one ordering/uniqueness rule.
    pub fn validate_all(evidence: &[Self]) -> Result<(), SandboxError> {
        let mut previous_mount: Option<(&str, &str)> = None;
        for item in evidence {
            item.validate()?;
            let current_mount = (item.mount_path.as_str(), item.store_id.as_str());
            if previous_mount.is_some_and(|previous| previous >= current_mount) {
                return Err(SandboxError::new(
                    "Memory materialization evidence is not canonically ordered",
                ));
            }
            previous_mount = Some(current_mount);
        }
        Ok(())
    }
}

/// A live memory-store mount, held for the sandbox's lifetime.
#[async_trait]
pub trait MemoryMount: Send + Sync {
    /// How the store was exposed (`Fuse` where the kernel supports it, else `Copy`).
    fn realization(&self) -> Realization;

    /// Original durable heads captured by a copy materialization. FUSE mounts
    /// return `None`: they write through and have no terminal copy to reconcile.
    /// Providers combine this secret-free evidence with the logical store and
    /// mount path already present in the Sandbox specification.
    fn materialization_heads(&self) -> Option<Vec<MemoryMaterializationHead>> {
        None
    }

    /// Tear down: unmount the FUSE, or (for a writable copy) harvest edits back to
    /// the durable store. The handle remains owned by its Sandbox until this
    /// returns `Ok`, so a transport, CAS, or unmount failure is retryable and can
    /// never be converted into successful Sandbox disposal.
    async fn teardown(&self) -> Result<(), SandboxError>;
}

/// Secret-free, per-Sandbox projection of one already-resolved Repository config.
/// It pins configuration semantics only. Branch is a clone preference; commit is
/// an exact immutable checkout. Principal, role, API key, policy, Workspace
/// hierarchy, and credential bytes are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRealizationPlan {
    pub repository_id: String,
    /// Exact Agent-visible sandbox path selected by the frozen Session input.
    ///
    /// The current substitutable provider set guarantees writable Repository
    /// continuity only below `/workspace`; callers must validate the plan before
    /// any credential or Git effect. This limitation is explicit and
    /// fail-closed: an adapter must never relocate another absolute path into
    /// `/workspace` because the Managed wire, prompt, tools, and Git would then
    /// observe different paths.
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

impl RepositoryRealizationPlan {
    /// Validate the one Repository mount-path contract shared by admission and
    /// every provider adapter.
    ///
    /// This method validates without normalizing or mutating `mount_path` so a
    /// durable caller-supplied value remains the sole Agent-visible truth. A
    /// future provider set may widen the accepted sandbox roots here only after
    /// every provider can preserve the same path and continuity semantics.
    pub fn validate_mount_path(&self) -> Result<(), SandboxError> {
        validate_repository_mount_path(&self.mount_path)
    }
}

/// Provider-neutral layout of runtime-owned paths inside the Agent workspace.
///
/// These roots are lifecycle state, not user content: providers may clear,
/// replace, checkpoint-exclude, or populate them while a Session is active.
/// Every provider and Runtime projection must consume this authority instead of
/// maintaining a local list. Repository trees therefore cannot own one of these
/// roots or any descendant.
pub struct WorkspaceLayout;

impl WorkspaceLayout {
    pub const ROOT: &'static str = "/workspace";

    /// Canonical sandbox-visible root for Session deliverables. This lives
    /// outside the Agent workspace, but belongs to the same provider-neutral
    /// layout authority so Runtime and provider adapters cannot drift.
    pub const OUTPUTS_ROOT: &'static str = "/mnt/session/outputs";

    pub const RESOURCE_PROJECTION_SUBDIR: &'static str = ".mnt";
    pub const RESOURCE_PROJECTION_ROOT: &'static str = "/workspace/.mnt";

    pub const DELIVERED_SKILLS_SUBDIR: &'static str = ".skills";
    pub const DELIVERED_SKILLS_ROOT: &'static str = "/workspace/.skills";

    pub const ACP_CONFIG_SUBDIR: &'static str = ".acp-config";
    pub const ACP_CONFIG_ROOT: &'static str = "/workspace/.acp-config";

    pub const XDG_CONFIG_SUBDIR: &'static str = ".config";
    pub const XDG_CONFIG_ROOT: &'static str = "/workspace/.config";

    pub const XDG_CACHE_SUBDIR: &'static str = ".cache";
    pub const XDG_CACHE_ROOT: &'static str = "/workspace/.cache";

    pub const CODEX_CONFIG_SUBDIR: &'static str = ".codex";
    pub const CODEX_CONFIG_ROOT: &'static str = "/workspace/.codex";

    pub const AWAKEN_STATE_SUBDIR: &'static str = ".awaken";
    pub const AWAKEN_STATE_ROOT: &'static str = "/workspace/.awaken";

    /// Complete set of top-level workspace trees whose contents/lifecycle are
    /// owned by the Runtime rather than by a mounted Repository.
    pub const RUNTIME_OWNED_ROOTS: &'static [&'static str] = &[
        Self::RESOURCE_PROJECTION_ROOT,
        Self::DELIVERED_SKILLS_ROOT,
        Self::ACP_CONFIG_ROOT,
        Self::XDG_CONFIG_ROOT,
        Self::XDG_CACHE_ROOT,
        Self::CODEX_CONFIG_ROOT,
        Self::AWAKEN_STATE_ROOT,
    ];

    /// Process-home variables whose selected workspace subtree is populated by
    /// a runtime/toolchain and therefore cannot also be a Repository tree.
    pub const RUNTIME_DIRECTORY_ENV_KEYS: &'static [&'static str] =
        &["HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "CODEX_HOME"];

    /// Compose one trusted workspace-relative path without restating the root.
    #[must_use]
    pub fn child(relative: &str) -> String {
        let relative = relative.trim_start_matches('/');
        if relative.is_empty() {
            Self::ROOT.to_owned()
        } else {
            format!("{}/{relative}", Self::ROOT)
        }
    }

    /// Return the exact relative suffix for the root or one of its descendants.
    /// Prefix lookalikes such as `/workspace-other` are rejected.
    #[must_use]
    pub fn relative(path: &str) -> Option<&str> {
        if path == Self::ROOT {
            Some("")
        } else {
            path.strip_prefix(Self::ROOT)?.strip_prefix('/')
        }
    }

    /// Return the exact relative suffix for the deliverables root or one of
    /// its descendants. Prefix lookalikes are rejected just like workspace
    /// paths, keeping artifact adapters on this single layout authority.
    #[must_use]
    pub fn outputs_relative(path: &str) -> Option<&str> {
        if path == Self::OUTPUTS_ROOT {
            Some("")
        } else {
            path.strip_prefix(Self::OUTPUTS_ROOT)?.strip_prefix('/')
        }
    }

    #[must_use]
    pub fn contains(path: &str) -> bool {
        Self::relative(path).is_some()
    }

    fn owns(path: &str) -> bool {
        Self::RUNTIME_OWNED_ROOTS.iter().any(|root| {
            path == *root
                || path
                    .strip_prefix(root)
                    .is_some_and(|remainder| remainder.starts_with('/'))
        })
    }
}

/// Canonical sandbox-visible defaults for the closed Resource-input kind set.
///
/// This value is a provider-layout fact: authoring adapters may project it or
/// select from it, but must not persist a second default catalog. An explicitly
/// authored mount path remains authoritative and never passes through this
/// value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceInputDefaultMounts {
    pub file: String,
    pub memory_store: String,
    pub repository: String,
}

impl ResourceInputDefaultMounts {
    /// Select the canonical default for one typed input without converting the
    /// kind back into an adapter-owned string discriminator.
    #[must_use]
    pub fn mount_path(&self, target: &InputResourceId) -> &str {
        match target {
            InputResourceId::File(_) => &self.file,
            InputResourceId::MemoryStore(_) => &self.memory_store,
            InputResourceId::Repository(_) => &self.repository,
        }
    }
}

/// Build the sole Resource-input default mount value.
///
/// Repository placement is derived from [`WorkspaceLayout`] so a future root
/// change cannot leave a stale authoring default behind.
#[must_use]
pub fn resource_input_default_mounts() -> ResourceInputDefaultMounts {
    ResourceInputDefaultMounts {
        file: "/mnt/files/data".to_string(),
        memory_store: "/mnt/memory".to_string(),
        repository: WorkspaceLayout::child("repo"),
    }
}

fn validate_canonical_sandbox_path(path: &str, absolute: bool) -> bool {
    if path.is_empty()
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.ends_with('/')
        || (absolute != path.starts_with('/'))
    {
        return false;
    }
    let relative = path.strip_prefix('/').unwrap_or(path);
    relative
        .split('/')
        .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

/// Validate the sole Repository path invariant shared by new Session
/// resolution, runtime admission, and concrete provider adapters.
///
/// This is intentionally a validator rather than a canonicalizer: callers that
/// own a durable path may retain it for compatibility, but no new realization
/// may silently acquire a different Agent-visible location.
pub fn validate_repository_mount_path(path: &str) -> Result<(), SandboxError> {
    let canonical_workspace_child = WorkspaceLayout::relative(path)
        .is_some_and(|relative| !relative.is_empty())
        && validate_canonical_sandbox_path(path, true);
    if canonical_workspace_child && !WorkspaceLayout::owns(path) {
        Ok(())
    } else {
        Err(SandboxError::new(format!(
            "repository mount path {path:?} is unsupported: current providers require one canonical, non-runtime-owned sandbox-absolute child of `{}`",
            WorkspaceLayout::ROOT
        )))
    }
}

fn mount_trees_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|remainder| remainder.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

/// Validate Repository ownership of complete workspace subtrees.
///
/// `other_mount_paths` must already have crossed its ordinary resource-path
/// grammar. Repositories are directory trees, so neither another Repository nor
/// a File/Memory mount may be their ancestor or descendant. This aggregate
/// preflight is pure and is intended to run before any catalog, credential,
/// projection, provider, or Git effect.
pub fn validate_repository_mount_paths(
    repository_paths: &[&str],
    other_mount_paths: &[&str],
) -> Result<(), SandboxError> {
    for (index, repository) in repository_paths.iter().enumerate() {
        validate_repository_mount_path(repository)?;
        if repository_paths[index + 1..]
            .iter()
            .chain(other_mount_paths)
            .any(|other| mount_trees_overlap(repository, other))
        {
            return Err(SandboxError::new(format!(
                "repository mount path {repository:?} overlaps another resource tree"
            )));
        }
    }
    Ok(())
}

/// Validate the final provider-visible layout before any cache prewarm,
/// environment creation, credential materialization, or Git effect.
///
/// The input must be the provider's effective [`SandboxSpec`] (including
/// container extra mounts). Relative mount requirements retain their existing
/// provider-neutral meaning below `/workspace`; this function projects that
/// meaning only for comparison and never mutates the spec or a durable path.
pub fn validate_repository_sandbox_layout(
    repository_paths: &[&str],
    spec: &SandboxSpec,
) -> Result<(), SandboxError> {
    validate_repository_sandbox_layout_with_history(repository_paths, spec, &[])
}

/// Validate adoption against both the provider's current effective spec and
/// provider-owned path evidence frozen in the durable handle.
///
/// `historical_owned_paths` is a union input, not a replacement: a changed
/// container configuration must protect both old realized mounts recorded by
/// the handle and new provider extra mounts in `spec`. Paths use the same
/// absolute-or-workspace-relative grammar as ordinary mount requirements.
pub fn validate_repository_sandbox_adoption_layout(
    repository_paths: &[&str],
    spec: &SandboxSpec,
    historical_owned_paths: &[&str],
) -> Result<(), SandboxError> {
    validate_repository_sandbox_layout_with_history(repository_paths, spec, historical_owned_paths)
}

fn validate_repository_sandbox_layout_with_history(
    repository_paths: &[&str],
    spec: &SandboxSpec,
    historical_owned_paths: &[&str],
) -> Result<(), SandboxError> {
    let mut mount_ids = std::collections::HashSet::new();
    let mut mount_paths = std::collections::HashSet::new();
    let mut owned_paths = Vec::with_capacity(spec.mounts.len() + spec.env.len() + 1);
    for mount in &spec.mounts {
        if mount.mount_id.trim().is_empty()
            || mount.mount_path.trim().is_empty()
            || !mount_ids.insert(mount.mount_id.as_str())
        {
            return Err(SandboxError::new(
                "sandbox mount ids and paths must be non-empty and unique",
            ));
        }
        let absolute = mount.mount_path.starts_with('/');
        if !validate_canonical_sandbox_path(&mount.mount_path, absolute) {
            return Err(SandboxError::new(format!(
                "sandbox mount path {:?} is not canonical",
                mount.mount_path
            )));
        }
        let visible = if absolute {
            mount.mount_path.clone()
        } else {
            format!("{}/{path}", WorkspaceLayout::ROOT, path = mount.mount_path)
        };
        if !mount_paths.insert(visible.clone()) {
            return Err(SandboxError::new(format!(
                "sandbox mount path {visible:?} is duplicated"
            )));
        }
        owned_paths.push(visible);
    }
    if !validate_canonical_sandbox_path(&spec.outputs_path, true) {
        return Err(SandboxError::new(format!(
            "sandbox outputs path {:?} is not canonical",
            spec.outputs_path
        )));
    }
    owned_paths.push(spec.outputs_path.clone());
    for variable in &spec.env {
        if !WorkspaceLayout::RUNTIME_DIRECTORY_ENV_KEYS.contains(&variable.name.as_str()) {
            continue;
        }
        let crate::vocab::EnvValue::Inline { value } = &variable.value else {
            return Err(SandboxError::new(format!(
                "runtime directory `{}` must be one inline canonical sandbox path",
                variable.name
            )));
        };
        if !validate_canonical_sandbox_path(value, true) {
            return Err(SandboxError::new(format!(
                "runtime directory `{}` has non-canonical path {value:?}",
                variable.name
            )));
        }
        if variable.name != "HOME" || value != WorkspaceLayout::ROOT {
            owned_paths.push(value.clone());
        }
    }
    for path in historical_owned_paths {
        let absolute = path.starts_with('/');
        if !validate_canonical_sandbox_path(path, absolute) {
            return Err(SandboxError::new(format!(
                "historical sandbox-owned path {path:?} is not canonical"
            )));
        }
        let visible = if absolute {
            (*path).to_string()
        } else {
            format!("{}/{path}", WorkspaceLayout::ROOT)
        };
        // A current Repository is necessarily part of the provider-observed
        // owned set after realization. Exact identity is the same tree, not a
        // competing owner; historical ancestors/descendants remain in the set
        // so replacement crash windows still fail closed.
        if repository_paths.contains(&visible.as_str()) {
            continue;
        }
        if !owned_paths.contains(&visible) {
            owned_paths.push(visible);
        }
    }
    let owned_paths = owned_paths.iter().map(String::as_str).collect::<Vec<_>>();
    validate_repository_mount_paths(repository_paths, &owned_paths)
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
