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
use awaken_ext_builtin_tools::executable_hand_tools;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use serde_json::Value;

/// The `awaken-provisioning-contract` seam realized locally (ADR-0041).
mod artifacts;
mod blob_cache;
mod namespace;
mod provider;
// The provider resolves mount bytes from an injected [`pc::BlobSource`] port
// (ADR-0038 D6, dependency-inverted) — this worker-tier crate links no durable
// store; the composition root adapts the content-addressed store to the port.
pub use blob_cache::{BlobLru, WorkspaceBlobCache};
pub use namespace::{NamespaceProvider, NamespaceSandbox, bubblewrap_argv, sandbox_exec_argv};
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
fn jail_args(
    tool_id: &str,
    mut args: Value,
    root: &IsolatedRoot,
    deny_egress: bool,
) -> Result<Value, ToolError> {
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
                let rooted = if deny_egress {
                    // Egress denied: run the command inside a bwrap namespace with no
                    // network (`--unshare-net`), rooted at the environment dir. The
                    // shared bash tool still `sh -c`s this string, which execs bwrap.
                    bwrap_no_egress(&root.root().to_string_lossy(), cmd)
                } else {
                    // Legacy lexical jail (host network shared): unchanged.
                    format!("cd '{}' && {}", root.root().display(), cmd)
                };
                args["command"] = Value::String(rooted);
            }
        }
        _ => {}
    }
    Ok(args)
}

/// Single-quote a token for a POSIX shell (`'` → `'\''`), so a token survives the
/// outer `sh -c` verbatim.
fn sh_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build a `bwrap … -- /bin/sh -c '<cmd>'` command string that runs `cmd` under
/// `root` with network egress denied (`--unshare-net`). Every token is shell-quoted
/// so the outer `sh -c` (in the shared bash tool) hands bwrap a clean argv and the
/// user command reaches the inner shell intact. The flag set is the one validated on
/// the target host: read-only host userland, a private `/tmp`, `/dev` and `/proc`,
/// and the environment dir bound read-write as the working directory.
fn bwrap_no_egress(root: &str, cmd: &str) -> String {
    let tokens: [&str; 20] = [
        "bwrap",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--unshare-net",
        "--bind",
        root,
        root,
        "--chdir",
        root,
        "--",
        "/bin/sh",
        "-c",
        cmd,
    ];
    tokens
        .iter()
        .map(|t| sh_squote(t))
        .collect::<Vec<_>>()
        .join(" ")
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
    /// Deny network egress for the `bash` tool (from the environment's spec).
    deny_egress: bool,
}

impl RootedTool {
    pub(crate) fn new(inner: Arc<dyn RawTool>, root: IsolatedRoot, deny_egress: bool) -> Self {
        Self {
            inner,
            root,
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
pub(crate) fn rooted_hand_tools(root: IsolatedRoot, deny_egress: bool) -> Vec<Arc<dyn HandTool>> {
    executable_hand_tools()
        .into_iter()
        .map(|inner| {
            Arc::new(RootedTool::new(inner, root.clone(), deny_egress)) as Arc<dyn HandTool>
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

/// Clone a git repository into `<root>/<logical>` **host-side** (ADR-0038). The
/// credential never enters the jail: git runs as a host process, the token is used
/// only for the clone transport, and the persisted `origin` is rewritten tokenless.
/// Fail-closed: a bad `logical` or non-zero git exit is an error. Shared by the
/// legacy `Environment` and the `pc::Sandbox` Workdir tier.
pub(crate) fn provision_repo_at(
    root: &IsolatedRoot,
    logical: &str,
    url: &str,
    git_ref: Option<&str>,
    token: Option<&str>,
) -> Result<(), SandboxError> {
    let dest = jailed_at(root, logical)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SandboxError(e.to_string()))?;
    }
    let mut args = vec!["clone".to_string()];
    if let Some(r) = git_ref {
        args.push("--branch".into());
        args.push(r.to_string());
    }
    args.push(authed_url(url, token));
    args.push(dest.to_string_lossy().into_owned());
    run_git(None, &args)?;
    if token.is_some() {
        // Scrub the token from the jail's origin: the agent inside never sees the credential
        // (the host re-injects it only on the harvest push transport).
        run_git(Some(&dest), &["remote", "set-url", "origin", url])?;
    }
    // The committer identity is the AGENT's to set (its own name/email on its own commits),
    // not provision's — a harvest can neither author a meaningful message nor a real user.
    Ok(())
}

/// Push the repo at `<root>/<logical>` to its origin **host-side** (ADR-0038 write-back).
///
/// Commit is the AGENT's job — it authors its own commits (message + identity) in the jail;
/// the host only pushes, because it alone holds the token (injected on the push transport,
/// never persisted). A harvest never fabricates a commit: it would have no meaningful message
/// and no real committer. So this pushes whatever the agent committed and pushes NOTHING when
/// the agent authored nothing (uncommitted working-tree changes are the agent's to commit).
///
/// `Ok(true)` when the agent's branch was ahead of its upstream and was pushed; `Ok(false)`
/// when it was already up to date. Shared with the Workdir tier.
///
/// The branch is not guessed: `HEAD` pushes the agent's *current* branch to the same-named
/// branch on origin, and `@{u}..HEAD` counts commits ahead of *that* branch's upstream. So
/// the agent owns the branch too — whichever branch it checked out or created is what ships.
/// A branch the agent newly created has no upstream; that reads as "ahead", so the push
/// creates it on the remote. A detached HEAD (a commit checkout) has no branch to push — the
/// push fails loudly rather than inventing a target.
pub(crate) fn push_repo_at(
    root: &IsolatedRoot,
    logical: &str,
    token: Option<&str>,
) -> Result<bool, SandboxError> {
    let dest = jailed_at(root, logical)?;
    // Push only when the agent's branch has commits ahead of its upstream — so an agent that
    // committed cleanly (empty working tree, real commits) IS pushed, and a re-harvest of an
    // already up-to-date branch is a cheap no-op. No upstream (a new branch) → treat as ahead.
    let ahead = git_stdout(Some(&dest), &["rev-list", "--count", "@{u}..HEAD"])
        .map(|c| c.trim() != "0")
        .unwrap_or(true);
    if !ahead {
        return Ok(false);
    }
    let url = git_stdout(Some(&dest), &["remote", "get-url", "origin"])?;
    run_git(
        Some(&dest),
        &["push", &authed_url(url.trim(), token), "HEAD"],
    )?;
    // Advance the remote-tracking ref ourselves: the push targets origin's URL (not the named
    // remote — so the token can ride the transport), which does NOT move `refs/remotes/origin/*`.
    // Syncing it makes a re-harvest of an already-pushed branch a true no-op (`@{u}..HEAD` == 0).
    let branch =
        git_stdout(Some(&dest), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let branch = branch.trim();
    if !branch.is_empty() && branch != "HEAD" {
        let _ = run_git(
            Some(&dest),
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                "HEAD",
            ],
        );
    }
    Ok(true)
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
pub(crate) fn rooted_raw_tools(root: IsolatedRoot, deny_egress: bool) -> Vec<Arc<dyn RawTool>> {
    rooted_hand_tools(root, deny_egress)
        .into_iter()
        .map(hand_tool_as_raw)
        .collect()
}

/// A stable content id over provisioning bytes — the pin identity a mount declares
/// and the provider verifies. BLAKE3 (the same hash the content-addressed store
/// assigns), so the id is identical to what that store computes and is
/// stable across processes, Rust versions, and nodes (unlike the old 64-bit
/// `DefaultHasher`), which distributed reference-passing (ADR-0038 D6) requires.
pub fn content_fingerprint(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
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

/// Splice a bearer token into an `https://` URL for a single git transport op,
/// so it lands in the process's transient argv and never in `.git/config`. Only
/// `https://` is rewritten (GitHub uses the `x-access-token` username convention);
/// a local path, `file://`, or an already-authed URL passes through unchanged, and
/// an absent token is a no-op — the tokenless path used by local remotes and tests.
fn authed_url(url: &str, token: Option<&str>) -> String {
    match token {
        Some(t) if url.starts_with("https://") && !url.contains('@') => {
            format!("https://x-access-token:{t}@{}", &url["https://".len()..])
        }
        _ => url.to_string(),
    }
}

/// Run `git <args>` (optionally in `cwd`) with prompts disabled, returning its
/// captured output. Any spawn failure or non-zero exit is a fail-closed
/// [`SandboxError`] carrying stderr — never a silent partial success.
fn git_run(cwd: Option<&Path>, args: &[&str]) -> Result<std::process::Output, SandboxError> {
    let mut cmd = std::process::Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0").args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .map_err(|e| SandboxError(format!("git {}: {e}", args.first().unwrap_or(&""))))?;
    if !out.status.success() {
        return Err(SandboxError(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out)
}

/// [`git_run`] with owned-string args (for the transient authed URL).
fn run_git(cwd: Option<&Path>, args: &[impl AsRef<str>]) -> Result<(), SandboxError> {
    let borrowed: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    git_run(cwd, &borrowed).map(|_| ())
}

/// Run a git command and return its trimmed stdout.
fn git_stdout(cwd: Option<&Path>, args: &[&str]) -> Result<String, SandboxError> {
    Ok(String::from_utf8_lossy(&git_run(cwd, args)?.stdout).into_owned())
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
            blake3::hash(bytes).to_hex().to_string()
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
        let g = jail_args(
            "glob",
            serde_json::json!({ "pattern": "src/*.rs" }),
            &root,
            false,
        )
        .unwrap();
        assert_eq!(g["pattern"], "/env/src/*.rs");

        let b = jail_args("bash", serde_json::json!({ "command": "ls" }), &root, false).unwrap();
        assert_eq!(b["command"], "cd '/env' && ls");
    }

    #[test]
    fn jail_passes_unknown_tools_through_and_rejects_escapes() {
        let root = IsolatedRoot::new("/env");
        let u = jail_args("weird", serde_json::json!({ "path": "../x" }), &root, false).unwrap();
        assert_eq!(u["path"], "../x"); // unknown tool: untouched

        assert!(
            jail_args(
                "read",
                serde_json::json!({ "path": "../escape" }),
                &root,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn deny_egress_bash_is_wrapped_in_a_no_network_bwrap_and_shell_quoted() {
        // The egress-denied bash path: instead of the legacy `cd '<root>' && <cmd>`
        // lexical jail (host network shared), the command must be re-rendered to run
        // inside a `bwrap --unshare-net` namespace rooted at the env dir, with every
        // token single-quoted so the outer `sh -c` hands bwrap a clean argv. This is the
        // deterministic construction the gated isolation e2e can only assert behaviorally.
        let root = IsolatedRoot::new("/env");
        let out = jail_args("bash", serde_json::json!({ "command": "id" }), &root, true).unwrap();
        let cmd = out["command"].as_str().unwrap();
        // Runs under bwrap with the network namespace unshared (egress denied).
        assert!(cmd.starts_with("'bwrap' '--ro-bind' '/' '/'"), "got: {cmd}");
        assert!(
            cmd.contains("'--unshare-net'"),
            "egress must be denied: {cmd}"
        );
        // Rooted at the env dir, and the user command reaches the inner shell.
        assert!(cmd.contains("'--chdir' '/env'"));
        assert!(cmd.contains("'--bind' '/env' '/env'"));
        assert!(cmd.ends_with("'/bin/sh' '-c' 'id'"), "got: {cmd}");
        // It is NOT the legacy lexical `cd && ...` form.
        assert!(!cmd.contains("cd '/env' &&"));
    }

    #[test]
    fn deny_egress_bash_escapes_an_embedded_quote_so_the_command_cannot_break_out() {
        // Injection safety: a single quote inside the user command must be escaped
        // (`'` → `'\''`) so it cannot terminate the outer `sh -c` quoting and smuggle
        // tokens past the bwrap wrapper.
        let root = IsolatedRoot::new("/env");
        let out = jail_args("bash", serde_json::json!({ "command": "a'b" }), &root, true).unwrap();
        let cmd = out["command"].as_str().unwrap();
        // The user command lands as a single fully-quoted token with the quote escaped.
        assert!(cmd.ends_with(r#"'-c' 'a'\''b'"#), "got: {cmd}");
    }

    #[test]
    fn mount_from_value_ignores_unknown_kinds_and_missing_fields_and_round_trips() {
        // Forward-compat contract (ADR-0035 D1): an unknown `kind` or a resource missing
        // a required field parses to `None` (ignored, never an error); `content_hash` is
        // optional (defaults empty); and a well-formed resource round-trips through the
        // opaque `Value` carrier byte-for-byte.
        assert_eq!(
            Mount::from_value(&serde_json::json!({ "kind": "future_thing", "x": 1 })),
            None,
            "an unknown kind is ignored, not an error"
        );
        // kind=resource but no `id` / `logical_path` / `content` → None (skipped).
        assert_eq!(
            Mount::from_value(&serde_json::json!({ "kind": "resource", "id": "r" })),
            None,
            "a resource missing logical_path/content is ignored"
        );
        // A hashless resource is admitted with an empty content_hash (unwrap_or_default).
        let hashless = Mount::from_value(&serde_json::json!({
            "kind": "resource", "id": "r", "logical_path": "a.txt", "content": "hi",
        }))
        .unwrap();
        assert_eq!(
            hashless,
            Mount::Resource(ResourceMount {
                id: "r".into(),
                content_hash: String::new(),
                logical_path: "a.txt".into(),
                content: "hi".into(),
            })
        );
        // Round-trip: to_value → from_value is the identity on a full resource.
        let full = Mount::Resource(ResourceMount {
            id: "r1".into(),
            content_hash: "h".into(),
            logical_path: "dir/a.txt".into(),
            content: "bytes".into(),
        });
        assert_eq!(Mount::from_value(&full.to_value()), Some(full));
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
        let good = hand_tool_as_raw(Arc::new(CustomHand {
            id: "good".into(),
            fail: false,
        }));
        let bad = hand_tool_as_raw(Arc::new(CustomHand {
            id: "bad".into(),
            fail: true,
        }));

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

    // ---- rooted_hand_tools ----

    #[test]
    fn rooted_hand_tools_wraps_every_builtin_hand_tool() {
        let tools = rooted_hand_tools(IsolatedRoot::new("/env"), false);
        let ids: Vec<_> = tools.iter().map(|t| t.id().to_string()).collect();
        for expected in ["read", "write", "edit", "glob", "grep", "bash"] {
            assert!(ids.contains(&expected.to_string()), "missing {expected}");
        }
    }
}
