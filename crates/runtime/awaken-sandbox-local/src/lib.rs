//! `awaken-sandbox-local` — single-machine per-environment sandbox isolation.
//!
//! Each environment is an [`IsolatedRoot`] (a path jail). Tools execute rooted in
//! it: [`RootedTool`] wraps a native `RawTool` and rewrites its path arguments
//! through the jail (and runs `bash` with `cd <root>`), so a tool cannot touch a
//! path outside its environment. A relay/placement is not a kernel concern
//! (ADR-0034 D6): a rooted tool is *just a `RawTool`* the host composes into a
//! run, so the kernel stays sandbox-agnostic.
//!
//! Provisioning (ADR-0035): `SandboxProvider::create` materializes the whole
//! per-run capability substrate. [`SandboxSpec::mounts`] carries typed
//! provisioning inputs ([`Mount::Skill`] / [`Mount::Resource`]); the provider
//! realizes them and the [`Environment`] exposes a single capability surface —
//! [`Environment::tools`] (hand + skill tools as `RawTool`) plus
//! [`Environment::resources`] (realized refs) — while [`Environment::receipt`]
//! records the host-side pin. A skill surfaces as a tool whose invocation returns
//! its body (progressive disclosure); the kernel never learns "skill". Unknown
//! mount shapes are ignored (forward-compat), so a distributed provider (another
//! repo) can extend the set. This crate ships only the local, in-process side
//! (`LocalSandboxProvider`); a remote relay is another `RawTool` from another repo.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::executable_hand_tools;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use serde_json::Value;

/// The `awaken-provisioning-contract` seam realized locally (ADR-0041). Additive:
/// the pre-contract `Environment`/`SandboxProvider` surface below is unchanged.
mod artifacts;
mod namespace;
mod provider;
// The content-addressed blob store is the canonical `awaken-file-store` (ADR-0041,
// BLAKE3), re-exported here for existing consumers; the provider resolves mount bytes
// from a `FileStore` handle injected at config time (ADR-0038 D6).
pub use awaken_file_store::{FileStore, FileStoreError, FsFileStore, InMemoryFileStore};
pub use namespace::{NamespaceProvider, bubblewrap_argv, sandbox_exec_argv};
pub use provider::{LocalProcess, LocalProvider, LocalSandbox};

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

/// Rewrite one tool call's arguments so its paths are jailed under `root`.
/// Path tools (`read`/`write`/`edit`/`grep`) rebase their `path`; `glob` rebases
/// its `pattern`; `bash` is prefixed with `cd '<root>'` so relative commands run
/// in the environment. Unknown tools pass through unchanged.
fn jail_args(tool_id: &str, mut args: Value, root: &IsolatedRoot) -> Result<Value, ToolError> {
    let escape = |e: EscapeError| ToolError::Execution(e.to_string());
    let rebase = |args: &mut Value, key: &str, root: &IsolatedRoot| -> Result<(), ToolError> {
        if let Some(Value::String(p)) = args.get(key) {
            let jailed = root.resolve(p).map_err(escape)?;
            args[key] = Value::String(jailed.to_string_lossy().into_owned());
        }
        Ok(())
    };
    match tool_id {
        "read" | "write" | "edit" | "grep" => rebase(&mut args, "path", root)?,
        "glob" => rebase(&mut args, "pattern", root)?,
        "bash" => {
            if let Some(Value::String(cmd)) = args.get("command") {
                let rooted = format!("cd '{}' && {}", root.root().display(), cmd);
                args["command"] = Value::String(rooted);
            }
        }
        _ => {}
    }
    Ok(args)
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
    /// The tool's textual result.
    pub content: String,
    /// Whether this is a tool-level error (model-visible; the run continues).
    pub is_error: bool,
}

impl HandOutput {
    /// A successful result carrying `content`.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A tool-level error carrying `content` (model-visible; the run continues).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// A [`HandTool`] that runs an inner `RawTool` jailed to an environment root. The
/// jail rewrites path arguments; the inner result is narrowed to [`HandOutput`], so
/// any runtime state the inner tool might carry is dropped at the boundary (G13).
pub(crate) struct RootedTool {
    inner: Arc<dyn RawTool>,
    root: IsolatedRoot,
}

impl RootedTool {
    pub(crate) fn new(inner: Arc<dyn RawTool>, root: IsolatedRoot) -> Self {
        Self { inner, root }
    }
}

#[async_trait]
impl HandTool for RootedTool {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn run(&self, mut call: ToolCall) -> Result<HandOutput, ToolError> {
        call.arguments = jail_args(self.inner.id(), call.arguments, &self.root)?;
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

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let call_id = call.call_id.clone();
        let out = self.0.run(call).await?;
        Ok(if out.is_error {
            ToolOutput::error(call_id, out.content)
        } else {
            ToolOutput::ok(call_id, out.content)
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
pub(crate) fn rooted_hand_tools(root: IsolatedRoot) -> Vec<Arc<dyn HandTool>> {
    executable_hand_tools()
        .into_iter()
        .map(|inner| Arc::new(RootedTool::new(inner, root.clone())) as Arc<dyn HandTool>)
        .collect()
}

/// A stable content id over provisioning bytes — the pin identity a mount declares
/// and the provider verifies. Delegates to [`awaken_file_store::content_id`] (BLAKE3),
/// so the id is identical to what the content-addressed [`FileStore`] assigns and is
/// stable across processes, Rust versions, and nodes (unlike the old 64-bit
/// `DefaultHasher`), which distributed reference-passing (ADR-0038 D6) requires.
pub fn content_fingerprint(bytes: &[u8]) -> String {
    awaken_file_store::content_id(bytes)
}

/// A typed provisioning input carried in [`SandboxSpec::mounts`] as an opaque
/// `Value`. The local provider realizes the variants it understands
/// ([`Mount::from_value`]) and ignores the rest (forward-compat), so another
/// repo's provider can add mount kinds without a breaking change (ADR-0035 D1).
/// Conversion is explicit (not derived) so this crate depends only on
/// `serde_json`, honoring its crate-boundary allow-list.
#[derive(Debug, Clone, PartialEq)]
pub enum Mount {
    /// A read-only resource file, realized under the environment's `.mnt/` root
    /// and referenced by logical path (no host path crosses the boundary, G3).
    Resource(ResourceMount),
}

impl Mount {
    /// Serialize to the opaque `Value` carried in [`SandboxSpec::mounts`].
    pub fn to_value(&self) -> Value {
        match self {
            Mount::Resource(r) => serde_json::json!({
                "kind": "resource",
                "id": r.id,
                "content_hash": r.content_hash,
                "logical_path": r.logical_path,
                "content": r.content,
            }),
        }
    }

    /// Parse a carried mount, or `None` when the `kind` is unknown or a required
    /// field is missing — both are ignored (forward-compat, ADR-0035 D1).
    pub fn from_value(v: &Value) -> Option<Mount> {
        let field = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        match v.get("kind").and_then(Value::as_str)? {
            "resource" => Some(Mount::Resource(ResourceMount {
                id: field("id")?,
                content_hash: field("content_hash").unwrap_or_default(),
                logical_path: field("logical_path")?,
                content: field("content")?,
            })),
            _ => None,
        }
    }
}

/// A resource file provisioned into the environment. `content` is realized under
/// `.mnt/<logical_path>`; `content_hash`, when non-empty, is verified fail-closed.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceMount {
    pub id: String,
    pub content_hash: String,
    pub logical_path: String,
    pub content: String,
}

/// A realized resource reference the environment exposes. Carries the logical path
/// under the environment root and the content hash — never a host absolute path
/// (G3): the host receives a reference, not a filesystem location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRef {
    pub id: String,
    pub content_hash: String,
    pub logical_path: String,
}

/// Which kind of capability a [`ProvisionEntry`] pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionKind {
    Resource,
}

/// One pinned provisioning entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionEntry {
    pub id: String,
    pub kind: ProvisionKind,
    pub content_hash: String,
}

/// The host-side pin for an environment: the content-addressed set that was
/// provisioned (ADR-0035 D3). It is recorded with the run and replayed to
/// re-provision an identical environment; it is not the kernel's presentation
/// fingerprint (ADR-0034 D5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionReceipt {
    pub entries: Vec<ProvisionEntry>,
}

/// Fail closed when a declared content hash does not match realized bytes. An
/// empty declaration means the caller did not pin, so the check is skipped.
fn verify_hash(content: &str, declared: &str) -> Result<(), SandboxError> {
    if !declared.is_empty() {
        let got = content_fingerprint(content.as_bytes());
        if got != declared {
            return Err(SandboxError(format!(
                "content hash mismatch: declared {declared}, realized {got}"
            )));
        }
    }
    Ok(())
}

/// The request to create an environment. `mounts` carries typed provisioning
/// inputs ([`Mount`]) as opaque `Value`s so a distributed provider (another repo)
/// can extend the set; `constraints` stays reserved, forward-compatible data.
#[derive(Debug, Clone, Default)]
pub struct SandboxSpec {
    pub id: String,
    pub mounts: Vec<Value>,
    pub constraints: Option<Value>,
}

impl SandboxSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }

    /// Declare a typed provisioning input. Serialized into the opaque `mounts`
    /// carrier so the wire stays forward-compatible.
    pub fn with_mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount.to_value());
        self
    }
}

/// A provisioned environment: an id and the hand tools bound to it. The host
/// composes [`hand_tools`](Environment::hand_tools) into a run without knowing
/// *where* they execute — a local provider yields rooted in-process tools, a
/// distributed provider (another repo) yields relay tools bound to a container or
/// remote root. The environment's realized path never crosses this boundary (G3):
/// the host receives tools, not a host path.
pub struct Environment {
    id: String,
    hand_tools: Vec<Arc<dyn HandTool>>,
    resources: Vec<ResourceRef>,
    receipt: ProvisionReceipt,
    /// The environment's realized root. **Never exposed** as a path (G3): it is
    /// used internally to scan for skill files, which are returned as neutral data.
    root: Option<PathBuf>,
}

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

impl Environment {
    /// Build an environment from its id and the isolation [`HandTool`]s a provider
    /// bound to it. Resources and the receipt are added by the `with_*` builders
    /// during provisioning. Because a `HandTool` returns [`HandOutput`] (no state
    /// field), every tool bound to an environment **cannot** author runtime state
    /// (G13) — structural, whether it runs in-process or relays into a
    /// container/remote root.
    pub fn new(id: impl Into<String>, hand_tools: Vec<Arc<dyn HandTool>>) -> Self {
        Self {
            id: id.into(),
            hand_tools,
            resources: Vec::new(),
            receipt: ProvisionReceipt::default(),
            root: None,
        }
    }

    /// Attach the realized root (kept private; used only to scan skill files).
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Scan `<root>/<subdir>/*/SKILL.md` **live** and return neutral file data.
    /// Live (re-scanned each call) so a skill the agent authored this run is seen.
    /// A missing dir, an unreadable file, or a dir without `SKILL.md` is skipped.
    /// The returned `dir` is the logical path `"<subdir>/<id>"`, never a host path.
    pub fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        let base = root.join(subdir);
        let Ok(entries) = std::fs::read_dir(&base) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let md = entry.path().join("SKILL.md");
            let Ok(content) = std::fs::read_to_string(&md) else {
                continue;
            };
            out.push(DiscoveredSkillFile {
                id: id.clone(),
                content,
                dir: format!("{subdir}/{id}"),
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// List regular files under `<root>/<subdir>` (recursively), returning each
    /// file's path relative to `subdir` and its bytes, sorted by path. Empty when
    /// the dir is absent. Collects a session's output artifacts (agent-written
    /// files) for retrieval; the returned paths are logical (never a host path, G3).
    pub fn list_files(&self, subdir: &str) -> Vec<(String, Vec<u8>)> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        let base = root.join(subdir);
        let mut out = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(path),
                    Ok(t) if t.is_file() => {
                        if let Ok(bytes) = std::fs::read(&path) {
                            let rel = path
                                .strip_prefix(&base)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            out.push((rel, bytes));
                        }
                    }
                    _ => {}
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Attach realized resource references.
    pub fn with_resources(mut self, resources: Vec<ResourceRef>) -> Self {
        self.resources = resources;
        self
    }

    /// Attach the host-side provisioning receipt (the pin).
    pub fn with_receipt(mut self, receipt: ProvisionReceipt) -> Self {
        self.receipt = receipt;
        self
    }

    /// This environment's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The isolation hand tools only, adapted for `Runtime::with_tool`. Retained
    /// for callers that compose just the built-in tool set; new callers should
    /// prefer [`tools`](Environment::tools), the full capability surface.
    pub fn hand_tools(&self) -> Vec<Arc<dyn RawTool>> {
        self.hand_tools
            .iter()
            .cloned()
            .map(hand_tool_as_raw)
            .collect()
    }

    /// The full provisioned **capability surface** (ADR-0035 D8): the environment's
    /// tools, each adapted to a `RawTool` carrying empty state (G13). This is what
    /// the host composes into a run; the kernel sees a uniform tool set with no
    /// mount/provision concept. Cheap to call repeatedly (clones `Arc` handles).
    pub fn tools(&self) -> Vec<Arc<dyn RawTool>> {
        self.hand_tools
            .iter()
            .cloned()
            .map(hand_tool_as_raw)
            .collect()
    }

    /// The realized resource references. Each names a logical path under the
    /// environment root and a content hash — never a host absolute path (G3).
    pub fn resources(&self) -> &[ResourceRef] {
        &self.resources
    }

    /// The host-side provisioning receipt — the content-addressed pin used to
    /// replay an identical environment (ADR-0035 D3).
    pub fn receipt(&self) -> &ProvisionReceipt {
        &self.receipt
    }
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("id", &self.id)
            .field("hand_tools", &self.hand_tools.len())
            .field("resources", &self.resources.len())
            .finish()
    }
}

/// Why provisioning failed.
#[derive(Debug, thiserror::Error)]
#[error("sandbox provisioning failed: {0}")]
pub struct SandboxError(pub String);

/// The provisioning port. The local impl is here; a remote/container impl lives
/// in a distributed repository and plugs in through this trait.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    async fn create(&self, spec: &SandboxSpec) -> Result<Environment, SandboxError>;
    async fn teardown(&self, id: &str) -> Result<(), SandboxError>;
}

/// Provisions environments as directories under a base path.
pub struct LocalSandboxProvider {
    base: PathBuf,
}

impl LocalSandboxProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }
}

#[async_trait]
impl SandboxProvider for LocalSandboxProvider {
    async fn create(&self, spec: &SandboxSpec) -> Result<Environment, SandboxError> {
        let dir = self.base.join(&spec.id);
        std::fs::create_dir_all(&dir).map_err(|e| SandboxError(e.to_string()))?;
        let root = IsolatedRoot::new(dir.clone());

        let mut resources: Vec<ResourceRef> = Vec::new();
        let mut entries: Vec<ProvisionEntry> = Vec::new();

        for raw in &spec.mounts {
            // Forward-compat: a mount shape this provider does not understand is
            // ignored, not an error (ADR-0035 D1).
            let Some(mount) = Mount::from_value(raw) else {
                continue;
            };
            match mount {
                Mount::Resource(resource) => {
                    verify_hash(&resource.content, &resource.content_hash)?;
                    // Realize under a read-only `.mnt/` root, jailed: a logical path
                    // that tries to escape fails closed.
                    let logical_path = format!(".mnt/{}", resource.logical_path);
                    let path = root
                        .resolve(&logical_path)
                        .map_err(|e| SandboxError(e.to_string()))?;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| SandboxError(e.to_string()))?;
                    }
                    std::fs::write(&path, &resource.content)
                        .map_err(|e| SandboxError(e.to_string()))?;
                    resources.push(ResourceRef {
                        id: resource.id.clone(),
                        content_hash: resource.content_hash.clone(),
                        logical_path,
                    });
                    entries.push(ProvisionEntry {
                        id: resource.id,
                        kind: ProvisionKind::Resource,
                        content_hash: resource.content_hash,
                    });
                }
            }
        }

        Ok(Environment::new(spec.id.clone(), rooted_hand_tools(root))
            .with_root(dir)
            .with_resources(resources)
            .with_receipt(ProvisionReceipt { entries }))
    }

    async fn teardown(&self, id: &str) -> Result<(), SandboxError> {
        let dir = self.base.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| SandboxError(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_fingerprint_is_blake3_and_matches_file_store() {
        // Unified on the content-addressed store's id (BLAKE3): a blob's mount id is
        // identical whichever crate computed it — the precondition for swapping the
        // FileStore impl at config time (ADR-0038 D6). BLAKE3 hex is 64 chars, so this
        // also proves we left the old unstable 16-hex DefaultHasher fingerprint.
        let bytes = b"provisioned bytes";
        assert_eq!(
            content_fingerprint(bytes),
            awaken_file_store::content_id(bytes)
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
    fn jail_rebases_glob_pattern_and_cds_bash() {
        let root = IsolatedRoot::new("/env");
        let g = jail_args("glob", serde_json::json!({ "pattern": "src/*.rs" }), &root).unwrap();
        assert_eq!(g["pattern"], "/env/src/*.rs");

        let b = jail_args("bash", serde_json::json!({ "command": "ls" }), &root).unwrap();
        assert_eq!(b["command"], "cd '/env' && ls");
    }

    #[test]
    fn jail_passes_unknown_tools_through_and_rejects_escapes() {
        let root = IsolatedRoot::new("/env");
        let u = jail_args("weird", serde_json::json!({ "path": "../x" }), &root).unwrap();
        assert_eq!(u["path"], "../x"); // unknown tool: untouched

        assert!(jail_args("read", serde_json::json!({ "path": "../escape" }), &root).is_err());
    }

    // ---- HandOutput ----

    #[test]
    fn hand_output_constructors() {
        let ok = HandOutput::ok("a");
        assert_eq!(ok.content, "a");
        assert!(!ok.is_error);
        assert!(HandOutput::error("b").is_error);
    }

    // ---- the environment boundary carries no runtime state (G13) ----

    // A `HandTool` cannot author state (`HandOutput` has no state field); this
    // confirms the `RawTool` the runtime sees always carries empty state, on both
    // the success and error paths.
    #[tokio::test]
    async fn adapter_maps_ok_and_error_with_empty_state() {
        let raw = Environment::new(
            "e",
            vec![
                Arc::new(CustomHand {
                    id: "good".into(),
                    fail: false,
                }),
                Arc::new(CustomHand {
                    id: "bad".into(),
                    fail: true,
                }),
            ],
        )
        .hand_tools();
        let good = raw.iter().find(|t| t.id() == "good").unwrap();
        let bad = raw.iter().find(|t| t.id() == "bad").unwrap();

        let o = good
            .invoke(call("good", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(o.content, "done");
        assert!(!o.is_error && o.state.is_empty());

        let e = bad
            .invoke(call("bad", serde_json::json!({})))
            .await
            .unwrap();
        assert!(e.is_error && e.state.is_empty());
    }

    // ---- Environment ----

    #[test]
    fn environment_id_count_and_debug() {
        let env = Environment::new(
            "env-7",
            vec![Arc::new(CustomHand {
                id: "x".into(),
                fail: false,
            })],
        );
        assert_eq!(env.id(), "env-7");
        assert_eq!(env.hand_tools().len(), 1);
        let dbg = format!("{env:?}");
        assert!(dbg.contains("env-7") && dbg.contains("hand_tools"));
    }

    // ---- rooted_hand_tools ----

    #[test]
    fn rooted_hand_tools_wraps_every_builtin_hand_tool() {
        let tools = rooted_hand_tools(IsolatedRoot::new("/env"));
        let ids: Vec<_> = tools.iter().map(|t| t.id().to_string()).collect();
        for expected in ["read", "write", "edit", "glob", "grep", "bash"] {
            assert!(ids.contains(&expected.to_string()), "missing {expected}");
        }
    }

    // ---- spec / error / provider surfaces ----

    #[test]
    fn sandbox_spec_and_error_surfaces() {
        let spec = SandboxSpec::new("id-1");
        assert_eq!(spec.id, "id-1");
        assert!(spec.mounts.is_empty() && spec.constraints.is_none());
        assert!(SandboxError("nope".into()).to_string().contains("nope"));
    }

    #[tokio::test]
    async fn local_provider_create_yields_tools_and_teardown_is_idempotent() {
        let base = std::env::temp_dir().join(format!("awaken-sbx-unit-{}", std::process::id()));
        let provider = LocalSandboxProvider::new(&base);
        let env = provider.create(&SandboxSpec::new("u")).await.unwrap();
        assert_eq!(env.id(), "u");
        assert_eq!(env.hand_tools().len(), 6);
        provider.teardown("u").await.unwrap();
        // second teardown is a no-op (dir already gone), still Ok.
        provider.teardown("u").await.unwrap();
    }
}
