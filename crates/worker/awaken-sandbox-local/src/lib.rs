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
    let rebase = |args: &mut Value, key: &str, root: &IsolatedRoot| -> Result<(), ToolError> {
        if let Some(Value::String(p)) = args.get(key) {
            let jailed = root.resolve(p).map_err(escape)?;
            let relative = jailed
                .strip_prefix(root.root())
                .map_err(|_| ToolError::Execution(format!("path `{p}` escaped its environment")))?;
            // `outputs/...` is an Agent-facing logical alias for the single
            // SandboxSpec output directory. Inspect the normalized relative path
            // so `outputs/../x` stays workspace `x` instead of escaping through a
            // raw `host_outputs.join("../x")`.
            let resolved = if let Ok(suffix) = relative.strip_prefix("outputs") {
                host_outputs.join(suffix)
            } else {
                jailed
            };
            args[key] = Value::String(resolved.to_string_lossy().into_owned());
        }
        Ok(())
    };
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
                if matches!(input.as_str(), "/mnt" | "/workspace" | "/outputs")
                    || input.starts_with("/mnt/")
                    || input.starts_with("/workspace/")
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
        "delete" => rebase(&mut args, "path", root)?,
        "move" => {
            rebase(&mut args, "source", root)?;
            rebase(&mut args, "destination", root)?;
        }
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
pub(crate) fn list_files_at(root: &IsolatedRoot, subdir: &str) -> Vec<(String, Vec<u8>)> {
    let base = root.root().join(subdir);
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

/// Scan `<root>/<subdir>/*/SKILL.md` **live** and return neutral file data (the host
/// parses the skill model). A missing dir / unreadable file / dir without `SKILL.md`
/// is skipped; `dir` is the logical path `"<subdir>/<id>"`. Shared with the Workdir tier.
pub(crate) fn scan_skill_dir_at(root: &IsolatedRoot, subdir: &str) -> Vec<DiscoveredSkillFile> {
    let base = root.root().join(subdir);
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
        // Workdir has path_fidelity=false: public `/mnt` and `/workspace`
        // names must resolve beneath its backing root, while an arbitrary
        // absolute host path must never be translated into reachable content.
        let root = IsolatedRoot::new("/private/session-root");
        for (tool, key, logical, expected) in [
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
    fn jail_rebases_both_move_endpoints_and_rejects_move_or_delete_escape() {
        // Path-boundary decision table: C1 move has two in-root endpoints -> E1
        // both are rebased; C2 either move endpoint traverses above root -> E2
        // reject the whole call; C3 delete traverses above root -> E3 reject.
        // This proves Dream's rename/delete capabilities cannot widen its mount.
        let root = IsolatedRoot::new("/env");
        let moved = jail_args(
            "move",
            serde_json::json!({"source":"old.md", "destination":"topic/new.md"}),
            &root,
            test_outputs(),
            false,
        )
        .unwrap();
        assert_eq!(moved["source"], "/env/old.md");
        assert_eq!(moved["destination"], "/env/topic/new.md");
        for (tool, arguments) in [
            (
                "move",
                serde_json::json!({"source":"old.md", "destination":"../escape.md"}),
            ),
            ("delete", serde_json::json!({"path":"../escape.md"})),
        ] {
            assert!(jail_args(tool, arguments, &root, test_outputs(), false).is_err());
        }
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
        for expected in [
            "read", "write", "edit", "move", "delete", "glob", "grep", "bash",
        ] {
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
