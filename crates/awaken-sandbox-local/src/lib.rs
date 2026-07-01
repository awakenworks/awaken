//! `awaken-sandbox-local` — single-machine per-environment sandbox isolation.
//!
//! Each environment is an [`IsolatedRoot`] (a path jail). Tools execute rooted in
//! it: [`RootedTool`] wraps a native `RawTool` and rewrites its path arguments
//! through the jail (and runs `bash` with `cd <root>`), so a tool cannot touch a
//! path outside its environment. A relay/placement is not a kernel concern
//! (ADR-0034 D6): a rooted tool is *just a `RawTool`* the host composes into a
//! run, so the kernel stays sandbox-agnostic.
//!
//! Distribution stays out of this repo: `SandboxSpec` reserves `mounts` /
//! `constraints` as forward-compatible data, and a remote relay is another
//! `RawTool` from another repository. This crate ships only the local, in-process
//! side (`LocalSandboxProvider`).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::executable_hand_tools;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use serde_json::Value;

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

/// A `RawTool` that runs an inner tool jailed to an environment root. The kernel
/// invokes it like any other tool; isolation is entirely inside this wrapper.
pub struct RootedTool {
    inner: Arc<dyn RawTool>,
    root: IsolatedRoot,
}

impl RootedTool {
    pub fn new(inner: Arc<dyn RawTool>, root: IsolatedRoot) -> Self {
        Self { inner, root }
    }
}

#[async_trait]
impl RawTool for RootedTool {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn invoke(&self, mut call: ToolCall) -> Result<ToolOutput, ToolError> {
        call.arguments = jail_args(self.inner.id(), call.arguments, &self.root)?;
        self.inner.invoke(call).await
    }
}

/// The built-in hand tools, each jailed to `root` — the executable tool set the
/// host composes into a run for one environment.
pub fn rooted_hand_tools(root: IsolatedRoot) -> Vec<Arc<dyn RawTool>> {
    executable_hand_tools()
        .into_iter()
        .map(|inner| Arc::new(RootedTool::new(inner, root.clone())) as Arc<dyn RawTool>)
        .collect()
}

/// The request to create an environment. `mounts` / `constraints` are reserved,
/// forward-compatible data a distributed provider (another repo) fills in; the
/// local provider ignores them.
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
}

/// A provisioned environment: an id and its isolated root.
#[derive(Debug, Clone)]
pub struct Environment {
    pub id: String,
    pub root: IsolatedRoot,
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
        Ok(Environment {
            id: spec.id.clone(),
            root: IsolatedRoot::new(dir),
        })
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
}
