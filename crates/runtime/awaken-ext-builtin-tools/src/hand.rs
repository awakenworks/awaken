// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: This source implements the repository-controlled builtin product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! In-process hand tools (ADR-0007). `read`, `write`, `edit`,
//! `glob`, `grep`, and `bash` run directly in the runtime process and render results as text.
//! Their ids match the descriptors in [`crate::builtin_tools`], so a run that
//! makes a descriptor model-visible can register the matching implementation.
//! Network fetch and the separately configured search plugin live in [`crate::web`].

use std::collections::BTreeMap;
use std::io::Write as _;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError, ToolExecutionTarget};
use serde::Deserialize;

use crate::erasure::erase_for;

mod bash;
pub use bash::{BashArgs, BashTool};

const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024;
const GREP_OUTPUT_LIMIT: usize = 100 * 1024;
const GREP_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const GREP_TRUNCATION_NOTICE: &str = "[output truncated]";
const GLOB_RESULT_LIMIT: usize = 200;
const GLOB_EXPANSION_LIMIT: usize = 256;
const WALK_MAX_DEPTH: usize = 40;
const WALK_MAX_ENTRIES: usize = 50_000;

/// Trusted host configuration for one Managed Agent toolset instance.
/// Instances are scoped to a Hand connection, so Bash state cannot cross Sessions.
#[derive(Clone)]
pub struct HandToolContext {
    workdir: PathBuf,
    allowed_roots: Vec<PathBuf>,
    path_projections: Vec<(PathBuf, PathBuf)>,
    max_file_bytes: Option<u64>,
    bash_env: Option<BTreeMap<String, String>>,
    /// Trusted launcher for the persistent shell. Providers use this to enter a
    /// sandbox once when Bash starts instead of wrapping every command and
    /// losing shell state between calls.
    bash_launcher: Option<Result<(PathBuf, Vec<String>), String>>,
    /// Trusted per-start preparation of that launcher (private descriptors and
    /// the options naming them), applied before the launcher's own arguments.
    bash_launch_preparation: Option<BashLaunchPreparation>,
}

/// Provider-owned preparation of a trusted Bash launcher, run once per shell
/// start. It may only add launcher options and inherited descriptors; it never
/// sees model input.
pub type BashLaunchPreparation =
    Arc<dyn Fn(&mut tokio::process::Command) -> std::io::Result<()> + Send + Sync>;

impl std::fmt::Debug for HandToolContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // This type owns provider-host roots and may be attached to diagnostic
        // context. Preserve useful shape without making workdir, projection,
        // allowed-root, launcher or environment values a second output path.
        formatter
            .debug_struct("HandToolContext")
            .field("allowed_root_count", &self.allowed_roots.len())
            .field("path_projection_count", &self.path_projections.len())
            .field("max_file_bytes", &self.max_file_bytes)
            .field("has_bash_env", &self.bash_env.is_some())
            .field("has_bash_launcher", &self.bash_launcher.is_some())
            .finish()
    }
}

impl HandToolContext {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            allowed_roots: Vec::new(),
            path_projections: Vec::new(),
            max_file_bytes: Some(DEFAULT_MAX_FILE_BYTES),
            bash_env: None,
            bash_launcher: None,
            bash_launch_preparation: None,
        }
    }

    /// Permit an Awaken-owned mount (not a model-selected arbitrary host path).
    #[must_use]
    pub fn with_allowed_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.allowed_roots.push(root.into());
        self
    }

    /// Map one runtime-owned logical mount root to its provider-specific path.
    ///
    /// Linux namespaces and containers normally use identity projections such
    /// as `/mnt` to `/mnt`. macOS Seatbelt has no mount namespace, so the same
    /// logical path maps to the Session's private host projection. Relative
    /// aliases (such as `outputs`) use the same resolver after normalization.
    /// Only trusted composition code can install this mapping.
    #[must_use]
    pub fn with_path_projection(
        mut self,
        logical_root: impl Into<PathBuf>,
        physical_root: impl Into<PathBuf>,
    ) -> Self {
        let physical_root = physical_root.into();
        // A subdirectory of the existing workdir needs a name projection, not
        // another authority root. Keeping the parent capability also permits
        // creating that missing subdirectory; it must not grant a symlink
        // escape just because its logical name has an alias.
        if !physical_root.starts_with(&self.workdir) {
            self.allowed_roots.push(physical_root.clone());
        }
        self.path_projections
            .push((logical_root.into(), physical_root));
        self
    }

    /// Fully replace the inherited Bash environment, matching the SDK helper.
    #[must_use]
    pub fn with_bash_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.bash_env = Some(env);
        self
    }

    /// Replace the default Bash launch with a trusted provider command. The
    /// command must itself end in a persistent POSIX shell; model-authored
    /// arguments never reach this configuration seam.
    #[must_use]
    pub fn with_bash_launcher(
        mut self,
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = String>,
    ) -> Self {
        self.bash_launcher = Some(Ok((program.into(), args.into_iter().collect())));
        self
    }

    /// Retain a provider's failed isolation admission as the shell's launch
    /// outcome. A later Bash call returns this error without selecting another
    /// executable or starting an unconfined shell.
    #[must_use]
    pub fn with_bash_launcher_failure(mut self, cause: impl Into<String>) -> Self {
        self.bash_launcher = Some(Err(cause.into()));
        self
    }

    /// Prepare the trusted launcher on every shell start, before its own
    /// arguments (for example, hand it a sealed syscall-profile descriptor).
    /// Has no effect without [`Self::with_bash_launcher`].
    #[must_use]
    pub fn with_bash_launch_preparation(mut self, prepare: BashLaunchPreparation) -> Self {
        self.bash_launch_preparation = Some(prepare);
        self
    }
}

impl Default for HandToolContext {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

#[derive(Clone)]
struct FileContext(Arc<HandToolContext>);

impl FileContext {
    fn new(context: &HandToolContext) -> Self {
        Self(Arc::new(context.clone()))
    }

    fn ensure_workdir(&self) -> Result<(), ToolError> {
        std::fs::create_dir_all(&self.0.workdir)
            .map_err(|error| ToolError::DefinitiveFailure(format!("workdir: {error}")))
    }

    fn projection_for(&self, input: &Path) -> Option<&(PathBuf, PathBuf)> {
        self.0
            .path_projections
            .iter()
            .filter(|(logical, _)| input.starts_with(logical))
            .max_by_key(|(logical, _)| logical.components().count())
    }

    fn resolve(&self, input: &str) -> Result<ConfinedPath, ToolError> {
        if input.is_empty() {
            return Err(ToolError::InvalidArguments("file path is required".into()));
        }
        let root = canonicalize_or_absolute(&self.0.workdir)?;
        let input_path = lexical_normalize(Path::new(input));
        let projected = self.projection_for(&input_path).map(|(logical, physical)| {
            physical.join(
                input_path
                    .strip_prefix(logical)
                    .expect("matching projection"),
            )
        });
        let candidate = match projected {
            Some(projected) => lexical_normalize(&projected),
            None if input_path.is_absolute() => input_path,
            None => lexical_normalize(&root.join(input_path)),
        };
        let resolved = canonicalize_with_missing(&candidate)
            .map_err(|error| file_error("path", input, &error))?;
        let mut roots = Vec::with_capacity(self.0.allowed_roots.len() + 1);
        roots.push(root);
        for allowed in &self.0.allowed_roots {
            roots.push(canonicalize_or_absolute(allowed)?);
        }
        let root = roots
            .into_iter()
            .filter(|root| path_is_within(root, &resolved))
            .max_by_key(|root| root.components().count())
            .ok_or_else(|| {
                ToolError::DefinitiveFailure(format!("path {input:?} escapes workdir"))
            })?;
        let relative = resolved
            .strip_prefix(&root)
            .expect("selected root contains the resolved path")
            .to_path_buf();
        let directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|error| file_error("path", input, &error))?;
        let logical_absolute = self.logical_path_at_resolution(&resolved);
        Ok(ConfinedPath {
            absolute: resolved,
            logical_absolute,
            relative,
            directory: Arc::new(directory),
        })
    }

    fn max_file_bytes(&self) -> Option<u64> {
        self.0.max_file_bytes
    }

    /// Select the already-authorized root for one glob and return only the
    /// pattern relative to that root. Absolute model paths are accepted only
    /// when they are beneath the workdir or a trusted mount projection; this
    /// gives glob the same logical path vocabulary as read/write/grep.
    fn resolve_glob(
        &self,
        pattern: &str,
        selected_root: Option<&str>,
    ) -> Result<(ConfinedPath, String), ToolError> {
        if !Path::new(pattern).is_absolute() {
            let normalized = lexical_normalize(Path::new(pattern));
            if selected_root.is_none()
                && let Some((logical, _)) = self.projection_for(&normalized)
            {
                let relative = normalized
                    .strip_prefix(logical)
                    .expect("matching projection");
                return Ok((
                    self.resolve(&logical.to_string_lossy())?,
                    if relative.as_os_str().is_empty() {
                        ".".into()
                    } else {
                        relative.to_string_lossy().into_owned()
                    },
                ));
            }
            return Ok((self.resolve(selected_root.unwrap_or("."))?, pattern.into()));
        }
        if selected_root.is_some() {
            return Err(ToolError::InvalidArguments(
                "glob: path must be omitted when pattern is absolute".into(),
            ));
        }

        let pattern = lexical_normalize(Path::new(pattern));
        let mut logical_roots = Vec::with_capacity(self.0.path_projections.len() * 2 + 2);
        logical_roots.push(lexical_absolute(&self.0.workdir)?);
        logical_roots.push(canonicalize_or_absolute(&self.0.workdir)?);
        for (logical, physical) in &self.0.path_projections {
            logical_roots.push(lexical_absolute(logical)?);
            logical_roots.push(canonicalize_or_absolute(physical)?);
        }
        for allowed in &self.0.allowed_roots {
            logical_roots.push(lexical_absolute(allowed)?);
            logical_roots.push(canonicalize_or_absolute(allowed)?);
        }
        let logical_root = logical_roots
            .into_iter()
            .filter(|root| path_is_within(root, &pattern))
            .max_by_key(|root| root.components().count())
            .ok_or_else(|| ToolError::DefinitiveFailure("glob: pattern escapes workdir".into()))?;
        let relative = pattern
            .strip_prefix(&logical_root)
            .expect("selected glob root contains its pattern")
            .to_str()
            .ok_or_else(|| ToolError::InvalidArguments("glob: pattern is not UTF-8".into()))?;
        let root = self.resolve(&logical_root.to_string_lossy())?;
        Ok((
            root,
            if relative.is_empty() { "." } else { relative }.into(),
        ))
    }

    /// Reverse only a trusted provider projection for model-facing output.
    /// Namespace/container mappings are normally identity mappings; Seatbelt
    /// and local adapters must not leak their physical host path through glob.
    fn logical_path_at_resolution(&self, physical: &Path) -> PathBuf {
        self.0
            .path_projections
            .iter()
            .filter_map(|(logical, projected)| {
                let projected = canonicalize_or_absolute(projected).ok()?;
                physical.strip_prefix(&projected).ok().map(|suffix| {
                    (
                        (projected.components().count(), logical.has_root()),
                        lexical_normalize(&logical.join(suffix)),
                    )
                })
            })
            .max_by_key(|(specificity, _)| *specificity)
            .map_or_else(|| physical.to_path_buf(), |(_, logical)| logical)
    }
}

/// A path bound to an already-open directory capability. String resolution is
/// used only to select the least-authoritative trusted root; all subsequent
/// filesystem effects are relative to this handle, so a symlink swap cannot
/// redirect an operation outside that root.
#[derive(Clone)]
struct ConfinedPath {
    absolute: PathBuf,
    /// Model-visible root frozen at the same authorization boundary as the
    /// directory handle. Rendering never canonicalizes an ambient path again.
    logical_absolute: PathBuf,
    relative: PathBuf,
    directory: Arc<cap_std::fs::Dir>,
}

impl ConfinedPath {
    fn logical_path(&self, search_relative: &Path) -> PathBuf {
        lexical_normalize(&self.logical_absolute.join(search_relative))
    }

    fn metadata(&self) -> std::io::Result<cap_std::fs::Metadata> {
        self.directory.metadata(&self.relative)
    }

    fn read_to_string(&self) -> std::io::Result<String> {
        self.directory.read_to_string(&self.relative)
    }

    fn create_parent_dirs(&self) -> std::io::Result<()> {
        match self.relative.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => self.directory.create_dir_all(parent),
            _ => Ok(()),
        }
    }
}

impl Deref for ConfinedPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.absolute
    }
}

fn canonicalize_or_absolute(path: &Path) -> Result<PathBuf, ToolError> {
    let absolute = lexical_absolute(path)?;
    canonicalize_with_missing(&absolute)
        .map_err(|error| ToolError::DefinitiveFailure(format!("workdir: {error}")))
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, ToolError> {
    if path.is_absolute() {
        Ok(lexical_normalize(path))
    } else {
        std::env::current_dir()
            .map(|cwd| lexical_normalize(&cwd.join(path)))
            .map_err(|error| ToolError::DefinitiveFailure(format!("workdir: {error}")))
    }
}

fn canonicalize_with_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut prefix = lexical_normalize(path);
    let mut tail = Vec::new();
    let mut hops = 0;
    loop {
        match std::fs::canonicalize(&prefix) {
            Ok(mut real) => {
                for part in tail.iter().rev() {
                    real.push(part);
                }
                return Ok(lexical_normalize(&real));
            }
            Err(realpath_error) => match std::fs::symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    hops += 1;
                    if hops > 40 {
                        return Err(std::io::Error::from_raw_os_error(40));
                    }
                    let target = std::fs::read_link(&prefix)?;
                    prefix = lexical_normalize(&if target.is_absolute() {
                        target
                    } else {
                        prefix.parent().unwrap_or(Path::new("/")).join(target)
                    });
                }
                Ok(_) => return Err(realpath_error),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    let Some(name) = prefix.file_name().map(ToOwned::to_owned) else {
                        return Err(error);
                    };
                    let Some(parent) = prefix.parent() else {
                        return Err(error);
                    };
                    tail.push(name);
                    prefix = parent.to_path_buf();
                }
                Err(error) => return Err(error),
            },
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    // Preserve leading relative parents until the path has
                    // been resolved against its actual authorized workdir.
                    // Dropping them here silently changes an escape attempt
                    // (or a legitimate sibling mount) into another file.
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn path_is_within(root: &Path, path: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn file_error(operation: &str, input: &str, error: &std::io::Error) -> ToolError {
    let message = match error.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory",
        std::io::ErrorKind::PermissionDenied => "permission denied",
        std::io::ErrorKind::NotADirectory => "not a directory",
        _ if error.raw_os_error() == Some(40) => "too many levels of symbolic links",
        _ => "i/o error",
    };
    ToolError::DefinitiveFailure(format!("{operation}: {input}: {message}"))
}

fn regular_file(
    path: &ConfinedPath,
    input: &str,
    operation: &str,
    limit: Option<u64>,
) -> Result<(), ToolError> {
    let metadata = path
        .metadata()
        .map_err(|error| file_error(operation, input, &error))?;
    if !metadata.is_file() {
        return Err(ToolError::DefinitiveFailure(format!(
            "{operation}: {input} is not a regular file"
        )));
    }
    check_file_size(operation, input, metadata.len(), limit)
}

fn check_file_size(
    operation: &str,
    input: &str,
    size: u64,
    limit: Option<u64>,
) -> Result<(), ToolError> {
    if let Some(limit) = limit
        && size > limit
    {
        let advice = if operation == "read" {
            "Use bash (head/tail/sed) to read a slice."
        } else {
            "Use bash (sed/awk) to edit a large file."
        };
        return Err(ToolError::DefinitiveFailure(format!(
            "{operation}: {input} is {size} bytes, exceeds {limit}-byte limit. {advice}"
        )));
    }
    Ok(())
}

fn atomic_write(path: &ConfinedPath, content: &str) -> std::io::Result<()> {
    path.create_parent_dirs()?;
    let parent = path.relative.parent().unwrap_or(Path::new(""));
    let temporary = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = path.directory.open_with(&temporary, &options)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        path.directory
            .rename(&temporary, &path.directory, &path.relative)
    })();
    if result.is_err() {
        let _ = path.directory.remove_file(&temporary);
    }
    result
}

/// Read a UTF-8 file and return its contents.
pub struct ReadTool(FileContext);

impl ReadTool {
    /// The same default limit applies to filesystem and immutable Resource reads.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = DEFAULT_MAX_FILE_BYTES;

    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Path of the file to read.
    #[serde(rename = "file_path", alias = "path")]
    pub path: String,
    /// Inclusive one-based start and end line; a non-positive end reads to EOF.
    #[serde(default)]
    pub view_range: Option<[i64; 2]>,
}

impl ReadArgs {
    /// Decode and slice bytes independently of their storage. Path authority is
    /// checked by the caller; this is the one text-read semantics for Hand and
    /// a frozen Skill bundle served before Hand exists.
    pub fn read_content(&self, bytes: &[u8], limit: Option<u64>) -> Result<String, ToolError> {
        check_file_size("read", &self.path, bytes.len() as u64, limit)?;
        let content = std::str::from_utf8(bytes).map_err(|_| {
            file_error(
                "read",
                &self.path,
                &std::io::Error::from(std::io::ErrorKind::InvalidData),
            )
        })?;
        let Some(range) = self.view_range else {
            return Ok(content.to_owned());
        };
        let lines = content.split('\n').collect::<Vec<_>>();
        let start = usize::try_from(range[0].saturating_sub(1).max(0)).unwrap_or(usize::MAX);
        let end = if range[1] > 0 {
            usize::try_from(range[1]).unwrap_or(usize::MAX)
        } else {
            lines.len()
        };
        Ok(lines
            .get(start..end.min(lines.len()))
            .unwrap_or(&[])
            .join("\n"))
    }
}

#[async_trait]
impl Tool for ReadTool {
    type Args = ReadArgs;
    type Output = String;
    const ID: &'static str = "read";
    const DESCRIPTION: &'static str = "Read a UTF-8 text file, including Markdown instructions, SKILL.md, CSV, source code and scripts. Use the given file_path directly; no shell cat, head, encoding probe or path lookup is needed for a known text file. Omit view_range to receive the complete file within the environment's file-size limit; use view_range only when a partial read is intended. This tool does not decode images, PDF or Office binary formats; use an appropriate document or OCR tool for those files.";

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        if args.path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "read: file_path is required".into(),
            ));
        }
        let path = self.0.resolve(&args.path)?;
        regular_file(&path, &args.path, "read", self.0.max_file_bytes())?;
        let content = path
            .directory
            .read(&path.relative)
            .map_err(|error| file_error("read", &args.path, &error))?;
        args.read_content(&content, self.0.max_file_bytes())
    }
}

/// List the paths matching a glob pattern, newline-joined.
pub struct GlobTool(FileContext);

impl GlobTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobArgs {
    /// Doublestar glob pattern relative to `path`, or an absolute pattern under
    /// the sandbox workdir or one of its trusted logical mount roots.
    pub pattern: String,
    /// Optional directory root to search under.
    #[serde(default)]
    pub path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    type Args = GlobArgs;
    type Output = String;
    const ID: &'static str = "glob";
    const DESCRIPTION: &'static str = "Find files matching a glob";

    async fn call(&self, args: GlobArgs) -> Result<String, ToolError> {
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArguments(
                "glob: pattern is required".into(),
            ));
        }
        if args
            .pattern
            .split(['/', '\\'])
            .any(|component| component == "..")
        {
            return Err(ToolError::DefinitiveFailure(
                "glob: \"..\" is not permitted in the pattern".into(),
            ));
        }
        let (root, relative_pattern) = self.0.resolve_glob(&args.pattern, args.path.as_deref())?;
        let patterns = expand_glob_alternatives(&relative_pattern)?
            .into_iter()
            .map(|pattern| RelativeGlob::compile(&pattern))
            .collect::<Result<Vec<_>, _>>()?;
        let mut paths = collect_confined_entries(&root, WalkPurpose::Glob)?
            .into_iter()
            .filter(|entry| {
                patterns
                    .iter()
                    .any(|pattern| pattern.matches(&entry.search_relative))
            })
            .map(|entry| {
                (
                    entry.modified,
                    root.logical_path(&entry.search_relative)
                        .display()
                        .to_string(),
                )
            })
            .collect::<Vec<_>>();
        paths.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        if paths.is_empty() {
            return Ok("no matches".into());
        }
        Ok(paths
            .into_iter()
            .take(GLOB_RESULT_LIMIT)
            .map(|(_, path)| path)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkPurpose {
    Glob,
    Grep,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkedEntryKind {
    File,
    Directory,
}

struct WalkedEntry {
    capability_relative: PathBuf,
    search_relative: PathBuf,
    kind: WalkedEntryKind,
    modified: Option<cap_std::time::SystemTime>,
    len: u64,
}

/// One traversal authority for Glob and Grep. Every lookup remains relative to
/// the directory handle captured by `ConfinedPath`; returned paths are data for
/// matching/rendering and are never reparsed for filesystem access.
fn collect_confined_entries(
    root: &ConfinedPath,
    purpose: WalkPurpose,
) -> Result<Vec<WalkedEntry>, ToolError> {
    let capability_root = if root.relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &root.relative
    };
    let metadata = root
        .directory
        .symlink_metadata(capability_root)
        .map_err(|error| {
            file_error("walk", &root.logical_absolute.display().to_string(), &error)
        })?;
    if metadata.file_type().is_symlink() {
        return Ok(Vec::new());
    }
    if metadata.is_file() {
        return Ok(vec![WalkedEntry {
            capability_relative: capability_root.to_path_buf(),
            search_relative: PathBuf::new(),
            kind: WalkedEntryKind::File,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }]);
    }
    if !metadata.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut remaining = WALK_MAX_ENTRIES;
    let mut ignore_stack = Vec::new();
    collect_confined_directory(
        root,
        capability_root,
        Path::new(""),
        purpose,
        0,
        &mut remaining,
        &mut ignore_stack,
        &mut entries,
    )?;
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn collect_confined_directory(
    root: &ConfinedPath,
    capability_directory: &Path,
    search_directory: &Path,
    purpose: WalkPurpose,
    depth: usize,
    remaining: &mut usize,
    ignore_stack: &mut Vec<ignore::gitignore::Gitignore>,
    output: &mut Vec<WalkedEntry>,
) -> Result<(), ToolError> {
    if depth > WALK_MAX_DEPTH || *remaining == 0 {
        return Ok(());
    }

    let inherited_ignore_count = ignore_stack.len();
    if purpose == WalkPurpose::Grep {
        load_confined_ignore_files(root, capability_directory, search_directory, ignore_stack);
    }
    let directory = root.directory.read_dir(capability_directory);
    let mut directory_entries = match directory {
        Ok(directory) => directory.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(error) => {
            ignore_stack.truncate(inherited_ignore_count);
            return Err(file_error(
                "walk",
                &root.logical_path(search_directory).display().to_string(),
                &error,
            ));
        }
    };
    directory_entries.sort_by_key(cap_std::fs::DirEntry::file_name);

    for entry in directory_entries {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        let name = entry.file_name();
        if purpose == WalkPurpose::Grep && matches!(name.to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        if purpose == WalkPurpose::Grep && name.to_str().is_some_and(|name| name.starts_with('.')) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let capability_relative = capability_directory.join(&name);
        let search_relative = search_directory.join(&name);
        let kind = if file_type.is_dir() {
            WalkedEntryKind::Directory
        } else if file_type.is_file() {
            WalkedEntryKind::File
        } else {
            continue;
        };
        if purpose == WalkPurpose::Grep
            && path_is_ignored(
                ignore_stack,
                &search_relative,
                kind == WalkedEntryKind::Directory,
            )
        {
            continue;
        }
        let Ok(metadata) = root.directory.symlink_metadata(&capability_relative) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        output.push(WalkedEntry {
            capability_relative: capability_relative.clone(),
            search_relative: search_relative.clone(),
            kind,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        });
        if kind == WalkedEntryKind::Directory {
            // An unreadable descendant is omitted like `WalkDir`/ripgrep; the
            // already-resolved search root itself remains a typed error above.
            let _ = collect_confined_directory(
                root,
                &capability_relative,
                &search_relative,
                purpose,
                depth + 1,
                remaining,
                ignore_stack,
                output,
            );
        }
    }
    ignore_stack.truncate(inherited_ignore_count);
    Ok(())
}

fn load_confined_ignore_files(
    root: &ConfinedPath,
    capability_directory: &Path,
    search_directory: &Path,
    ignore_stack: &mut Vec<ignore::gitignore::Gitignore>,
) {
    for file_name in [".gitignore", ".ignore"] {
        let capability_path = capability_directory.join(file_name);
        let Ok(metadata) = root.directory.symlink_metadata(&capability_path) else {
            continue;
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > DEFAULT_MAX_FILE_BYTES
        {
            continue;
        }
        let Ok(contents) = root.directory.read_to_string(&capability_path) else {
            continue;
        };
        let source = search_directory.join(file_name);
        let mut builder = ignore::gitignore::GitignoreBuilder::new(search_directory);
        for line in contents.lines() {
            let _ = builder.add_line(Some(source.clone()), line);
        }
        if let Ok(matcher) = builder.build() {
            ignore_stack.push(matcher);
        }
    }
}

fn path_is_ignored(
    ignore_stack: &[ignore::gitignore::Gitignore],
    path: &Path,
    is_directory: bool,
) -> bool {
    let mut ignored = false;
    for matcher in ignore_stack {
        let matched = matcher.matched_path_or_any_parents(path, is_directory);
        if matched.is_ignore() {
            ignored = true;
        } else if matched.is_whitelist() {
            ignored = false;
        }
    }
    ignored
}

enum RelativeGlobSegment {
    Recursive,
    Pattern(glob::Pattern),
}

struct RelativeGlob {
    segments: Vec<RelativeGlobSegment>,
}

impl RelativeGlob {
    fn compile(pattern: &str) -> Result<Self, ToolError> {
        let segments = pattern
            .split('/')
            .map(|segment| {
                if segment == "**" {
                    Ok(RelativeGlobSegment::Recursive)
                } else {
                    glob::Pattern::new(segment)
                        .map(RelativeGlobSegment::Pattern)
                        .map_err(|error| {
                            ToolError::InvalidArguments(format!("glob {pattern}: {error}"))
                        })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { segments })
    }

    fn matches(&self, relative: &Path) -> bool {
        let Some(segments) = relative
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        self.matches_from(0, 0, &segments, &mut BTreeMap::new())
    }

    fn matches_from(
        &self,
        pattern_index: usize,
        path_index: usize,
        path: &[&str],
        memo: &mut BTreeMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, path_index)) {
            return *result;
        }
        let result = match self.segments.get(pattern_index) {
            None => path_index == path.len(),
            Some(RelativeGlobSegment::Recursive) => {
                self.matches_from(pattern_index + 1, path_index, path, memo)
                    || (path_index < path.len()
                        && self.matches_from(pattern_index, path_index + 1, path, memo))
            }
            Some(RelativeGlobSegment::Pattern(pattern)) => {
                path.get(path_index)
                    .is_some_and(|name| pattern.matches(name))
                    && self.matches_from(pattern_index + 1, path_index + 1, path, memo)
            }
        };
        memo.insert((pattern_index, path_index), result);
        result
    }
}

/// Expand the alternation forms accepted by Node's native `fs.glob` but not by
/// Rust's `glob` crate. The cap makes a model-authored combinatorial pattern a
/// deterministic argument error instead of unbounded work.
fn expand_glob_alternatives(pattern: &str) -> Result<Vec<String>, ToolError> {
    fn first_alternation(pattern: &str) -> Option<(usize, usize, char, Vec<&str>)> {
        let brace = pattern.find('{').map(|index| (index, '{', '}', ','));
        let extglob = pattern.find("@(").map(|index| (index, '(', ')', '|'));
        let (start, open, close, separator) = match (brace, extglob) {
            (Some(left), Some(right)) => {
                if left.0 <= right.0 {
                    left
                } else {
                    right
                }
            }
            (Some(found), None) | (None, Some(found)) => found,
            (None, None) => return None,
        };
        let content_start = if open == '(' { start + 2 } else { start + 1 };
        let mut depth = 0_usize;
        let mut end = None;
        for (offset, character) in pattern[content_start..].char_indices() {
            if character == open {
                depth += 1;
            } else if character == close {
                if depth == 0 {
                    end = Some(content_start + offset);
                    break;
                }
                depth -= 1;
            }
        }
        let end = end?;
        let content = &pattern[content_start..end];
        let mut choices = Vec::new();
        let mut choice_start = 0;
        depth = 0;
        for (offset, character) in content.char_indices() {
            if character == open {
                depth += 1;
            } else if character == close {
                depth = depth.saturating_sub(1);
            } else if character == separator && depth == 0 {
                choices.push(&content[choice_start..offset]);
                choice_start = offset + character.len_utf8();
            }
        }
        choices.push(&content[choice_start..]);
        (choices.len() > 1).then_some((start, end + 1, separator, choices))
    }

    fn visit(pattern: String, output: &mut Vec<String>) -> Result<(), ToolError> {
        if output.len() >= GLOB_EXPANSION_LIMIT {
            return Err(ToolError::InvalidArguments(format!(
                "glob: pattern expands to more than {GLOB_EXPANSION_LIMIT} alternatives"
            )));
        }
        let Some((start, end, _, choices)) = first_alternation(&pattern) else {
            output.push(pattern);
            return Ok(());
        };
        for choice in choices {
            visit(
                format!("{}{}{}", &pattern[..start], choice, &pattern[end..]),
                output,
            )?;
        }
        Ok(())
    }

    let mut expanded = Vec::new();
    visit(pattern.to_string(), &mut expanded)?;
    Ok(expanded)
}

/// Search a file's lines for a regex, returning `path:line:text` for each match.
pub struct GrepTool(FileContext);

impl GrepTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    /// Regular expression to search for.
    pub pattern: String,
    /// Optional directory root to search under.
    #[serde(default)]
    pub path: String,
}

#[async_trait]
impl Tool for GrepTool {
    type Args = GrepArgs;
    type Output = String;
    const ID: &'static str = "grep";
    const DESCRIPTION: &'static str = "Search file contents";

    async fn call(&self, args: GrepArgs) -> Result<String, ToolError> {
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArguments(
                "grep: pattern is required".into(),
            ));
        }
        let root = self.0.resolve(if args.path.is_empty() {
            "."
        } else {
            &args.path
        })?;
        let re = regex::Regex::new(&args.pattern)
            .map_err(|err| ToolError::InvalidArguments(format!("grep: invalid regex: {err}")))?;
        let root_is_file = root.metadata().is_ok_and(|metadata| metadata.is_file());
        let files = collect_confined_entries(&root, WalkPurpose::Grep)?;
        let mut hits = Vec::new();
        let mut output_bytes = 0;
        for file in files {
            if file.kind != WalkedEntryKind::File || file.len > GREP_MAX_FILE_BYTES {
                continue;
            }
            let Ok(content) = root.directory.read_to_string(&file.capability_relative) else {
                continue;
            };
            if content
                .as_bytes()
                .get(..content.len().min(512))
                .is_some_and(|prefix| prefix.contains(&0))
            {
                continue;
            }
            let logical_path = root.logical_path(&file.search_relative);
            for (index, line) in content.split('\n').enumerate() {
                if re.is_match(line) {
                    let hit = if root_is_file {
                        format!("{}:{}", index + 1, line)
                    } else {
                        format!("{}:{}:{}", logical_path.display(), index + 1, line)
                    };
                    if output_bytes + hit.len() + 1 > GREP_OUTPUT_LIMIT {
                        hits.push(GREP_TRUNCATION_NOTICE.to_owned());
                        return Ok(hits.join("\n"));
                    }
                    output_bytes += hit.len() + 1;
                    hits.push(hit);
                }
            }
        }
        if hits.is_empty() {
            Ok("no matches".into())
        } else {
            Ok(hits.join("\n"))
        }
    }
}

/// Write `content` to a file, creating or truncating it. Returns a confirmation.
pub struct WriteTool(FileContext);

impl WriteTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// Path of the file to write.
    #[serde(rename = "file_path", alias = "path")]
    pub path: String,
    /// Complete replacement content.
    pub content: String,
}

#[async_trait]
impl Tool for WriteTool {
    type Args = WriteArgs;
    type Output = String;
    const ID: &'static str = "write";
    const DESCRIPTION: &'static str = "Write a file";

    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        if args.path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "write: file_path is required".into(),
            ));
        }
        self.0.ensure_workdir()?;
        let path = self.0.resolve(&args.path)?;
        atomic_write(&path, &args.content)
            .map_err(|error| file_error("write", &args.path, &error))?;
        Ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            path.logical_absolute.display()
        ))
    }
}

/// Replace one exact occurrence of `old` with `new` in a file. Fails closed when
/// `old` is absent or ambiguous, so an edit never silently changes the wrong
/// span.
pub struct EditTool(FileContext);

impl EditTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Path of the file to edit.
    #[serde(rename = "file_path", alias = "path")]
    pub path: String,
    /// Exact text to replace.
    #[serde(rename = "old_string", alias = "old")]
    pub old: String,
    /// Replacement text.
    #[serde(rename = "new_string", alias = "new")]
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    type Args = EditArgs;
    type Output = String;
    const ID: &'static str = "edit";
    const DESCRIPTION: &'static str = "Edit a file by replacing text";

    async fn call(&self, args: EditArgs) -> Result<String, ToolError> {
        // An empty `old` is a degenerate anchor: `"".matches("")` is 1, so on an
        // empty file the "exactly one occurrence" arm would silently *insert*
        // `new`. Reject it up front so an edit always replaces a real, located
        // substring rather than mutating on a no-op anchor.
        if args.path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "edit: file_path is required".into(),
            ));
        }
        if args.old.is_empty() {
            return Err(ToolError::InvalidArguments(
                "edit: old_string is required".into(),
            ));
        }
        let path = self.0.resolve(&args.path)?;
        regular_file(&path, &args.path, "edit", self.0.max_file_bytes())?;
        let content = path
            .read_to_string()
            .map_err(|error| file_error("edit", &args.path, &error))?;
        let matches = content.matches(&args.old).count();
        match matches {
            0 => Err(ToolError::DefinitiveFailure(format!(
                "edit: old_string not found in {}",
                args.path
            ))),
            1 => {
                let updated = content.replacen(&args.old, &args.new, 1);
                atomic_write(&path, &updated)
                    .map_err(|error| file_error("edit: write", &args.path, &error))?;
                Ok(format!(
                    "edited {} (1 replacement(s))",
                    path.logical_absolute.display()
                ))
            }
            n if args.replace_all => {
                let updated = content.replace(&args.old, &args.new);
                atomic_write(&path, &updated)
                    .map_err(|error| file_error("edit: write", &args.path, &error))?;
                Ok(format!(
                    "edited {} ({n} replacements)",
                    path.logical_absolute.display()
                ))
            }
            n => Err(ToolError::DefinitiveFailure(format!(
                "edit: old_string appears {n} times in {} (must be unique)",
                args.path,
            ))),
        }
    }
}

/// Create a fresh, environment-scoped toolset. The Bash session and filesystem
/// confinement share this trusted workdir. The public catalog is owned by
/// `all_hand_tools_in`; this private constructor cannot form a parallel API.
pub(crate) fn hand_tools_in(context: HandToolContext) -> Vec<Arc<dyn RawTool>> {
    vec![
        erase_for(ReadTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(WriteTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(EditTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(GlobTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(GrepTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(BashTool::new(&context), ToolExecutionTarget::Sandbox),
    ]
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[test]
    fn relative_parents_and_aliases_share_one_authorized_resolution() {
        // Decision table P2: normalize BEFORE matching aliases; preserve
        // unmatched relative parents until the actual workdir is known.
        // Sibling mount => allowed only if explicitly granted; outside sibling
        // => reject; overlapping aliases => most specific; similarly named
        // non-alias => ordinary workspace. Missing nested projection remains
        // covered by the already-openable workdir, not another missing root.
        // On macOS /var/folders names /private/var/folders; compare the same
        // physical coordinate the existing resolver authorizes, not a lexical
        // spelling that differs while naming the identical directory.
        let fixture = tempfile::tempdir().unwrap();
        let workdir = fixture.path().join("workspace");
        let mount = fixture.path().join("mnt");
        let specific = fixture.path().join("special");
        for dir in [&workdir, &mount, &specific] {
            std::fs::create_dir(dir).unwrap();
        }
        let context = HandToolContext::new(&workdir)
            .with_path_projection("/workspace", &workdir)
            .with_path_projection("/nested", workdir.join("not-yet-created"))
            .with_path_projection("/mnt", &mount)
            .with_path_projection("outputs", &mount)
            .with_path_projection("outputs/specific", &specific);
        let files = FileContext::new(&context);
        for (input, expected) in [
            ("../mnt/file", mount.join("file")),
            ("outputs/file", mount.join("file")),
            ("outputs/specific/file", specific.join("file")),
            ("outputs/specific/../file", mount.join("file")),
            ("outputs/../file", workdir.join("file")),
            ("outputs-similar/file", workdir.join("outputs-similar/file")),
            ("/nested/file", workdir.join("not-yet-created/file")),
        ] {
            assert_eq!(
                files.resolve(input).unwrap().absolute,
                canonicalize_with_missing(&expected).unwrap(),
                "{input}"
            );
        }
        for input in ["../outside/file", "../../file", "outputs/../../file", ""] {
            assert!(files.resolve(input).is_err(), "{input}");
        }
        for (input, expected) in [
            ("../a", "../a"),
            ("a/../../b", "../b"),
            ("../../b", "../../b"),
        ] {
            assert_eq!(lexical_normalize(Path::new(input)), Path::new(expected));
        }
    }

    #[cfg(unix)]
    #[test]
    fn nested_projection_does_not_grant_an_escaping_symlink() {
        // P3 a projected descendant already belongs to workdir authority.
        // Replacing it by a symlink to an ungranted sibling must still deny;
        // adding an alias must not silently grant the symlink target.
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("nested")).unwrap();
        let context = HandToolContext::new(root.path())
            .with_path_projection("/managed", root.path().join("nested"));
        assert!(FileContext::new(&context).resolve("/managed/file").is_err());
    }

    #[tokio::test]
    async fn write_creates_missing_parent_directories() {
        // A write to a nested path (e.g. `outputs/x.txt`) must succeed without a prior
        // mkdir — the artifact-write path (ADR-0038) relies on this.
        let base = std::env::temp_dir().join(format!("awaken-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join("outputs/deep/result.txt");
        let out = WriteTool::new(&HandToolContext::new(&base))
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "artifact-bytes".into(),
            })
            .await
            .expect("write into a missing dir tree");
        assert!(out.contains("wrote"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "artifact-bytes");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_workdir_beneath_a_symlink_is_confined_by_its_real_root() {
        use std::os::unix::fs::symlink;

        // macOS exposes /var through /private/var. The same shape can occur in
        // sandbox mount projections on Linux, so compare both the trusted root
        // and candidate after resolving their longest existing prefix.
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let workdir = alias.join("missing");
        let context = HandToolContext::new(&workdir);
        let inside = workdir.join("nested/result.txt");

        WriteTool::new(&context)
            .call(WriteArgs {
                path: inside.to_string_lossy().into_owned(),
                content: "inside".into(),
            })
            .await
            .expect("symlinked missing workdir remains writable");
        assert_eq!(
            std::fs::read_to_string(real.join("missing/nested/result.txt")).unwrap(),
            "inside"
        );

        let outside = directory.path().join("outside.txt");
        assert!(
            WriteTool::new(&context)
                .call(WriteArgs {
                    path: outside.to_string_lossy().into_owned(),
                    content: "outside".into(),
                })
                .await
                .is_err(),
            "a canonical root must not broaden the trusted boundary"
        );
        assert!(!outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn directory_capability_closes_check_use_symlink_races() {
        use std::os::unix::fs::symlink;

        // Causal graph for the check/use race:
        // R1 resolve an in-root read, then replace its parent with an escaping
        //    symlink -> the capability-relative read fails closed.
        // R2 resolve an in-root write, perform the same replacement -> atomic
        //    creation fails and the outside directory remains untouched.
        // The adversarial swap is deliberately between policy resolution and
        // I/O, so this tests the handle boundary rather than canonicalization.
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let pivot = root.path().join("pivot");
        let held = root.path().join("held");
        std::fs::create_dir(&pivot).unwrap();
        std::fs::write(pivot.join("secret"), "inside").unwrap();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        let files = FileContext::new(&HandToolContext::new(root.path()));

        let read = files.resolve("pivot/secret").unwrap();
        std::fs::rename(&pivot, &held).unwrap();
        symlink(outside.path(), &pivot).unwrap();
        assert!(read.read_to_string().is_err(), "R1");

        std::fs::remove_file(&pivot).unwrap();
        std::fs::rename(&held, &pivot).unwrap();
        let write = files.resolve("pivot/new.txt").unwrap();
        std::fs::rename(&pivot, &held).unwrap();
        symlink(outside.path(), &pivot).unwrap();
        assert!(atomic_write(&write, "must-not-escape").is_err(), "R2");
        assert!(!outside.path().join("new.txt").exists(), "R2");
    }
    #[cfg(unix)]
    #[test]
    fn search_walker_stays_bound_to_the_open_root_after_path_swap() {
        use std::os::unix::fs::symlink;

        // Search-walker cause/effect graph: C1 FileContext captured the trusted
        // directory capability; C2 its ambient pathname is unchanged/swapped;
        // C3 the replacement is an escaping symlink. E1 both Glob and Grep see
        // the originally opened tree; E2 neither observes replacement bytes.
        //
        // | Rule | C1 | C2 | C3 | Purpose | Effect |
        // |---|---|---|---|---|---|
        // | W1 | T | unchanged | F | Glob | E1 |
        // | W2 | T | swapped | T | Glob | E1+E2 |
        // | W3 | T | swapped | T | Grep | E1+E2 |
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root_path = parent.path().join("root");
        let held_path = parent.path().join("held");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("inside.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("outside.txt"), "outside").unwrap();
        let files = FileContext::new(&HandToolContext::new(&root_path));
        let confined = files.resolve(".").unwrap();

        let before = collect_confined_entries(&confined, WalkPurpose::Glob).unwrap();
        assert_eq!(before.len(), 1, "W1/E1");
        assert_eq!(before[0].search_relative, Path::new("inside.txt"), "W1/E1");

        std::fs::rename(&root_path, &held_path).unwrap();
        symlink(outside.path(), &root_path).unwrap();
        for (rule, purpose) in [("W2", WalkPurpose::Glob), ("W3", WalkPurpose::Grep)] {
            let after = collect_confined_entries(&confined, purpose).unwrap();
            assert_eq!(after.len(), 1, "{rule}/E1+E2");
            assert_eq!(
                after[0].search_relative,
                Path::new("inside.txt"),
                "{rule}/E1+E2"
            );
            assert_eq!(
                confined
                    .directory
                    .read_to_string(&after[0].capability_relative)
                    .unwrap(),
                "inside",
                "{rule}/E1+E2"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn search_rendering_keeps_the_authorized_logical_projection_after_path_swap() {
        use std::os::unix::fs::symlink;

        // Rendering cause/effect graph: C1 `/managed` is mapped to one trusted
        // physical root; C2 resolution captures its capability and logical
        // base; C3 the ambient physical path is replaced by an escaping
        // symlink. E1 traversal still reads the captured tree; E2 model-visible
        // output remains `/managed/...`; E3 no physical host path is exposed.
        // Decision rule P1=C1+C2+C3 => E1+E2+E3.
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let physical = parent.path().join("physical");
        let held = parent.path().join("held");
        std::fs::create_dir(&physical).unwrap();
        std::fs::write(physical.join("inside.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("outside.txt"), "outside").unwrap();
        let context = HandToolContext::new(parent.path().join("workdir"))
            .with_path_projection("/managed", &physical);
        let confined = FileContext::new(&context).resolve("/managed").unwrap();

        std::fs::rename(&physical, &held).unwrap();
        symlink(outside.path(), &physical).unwrap();
        let entries = collect_confined_entries(&confined, WalkPurpose::Glob).unwrap();
        assert_eq!(entries.len(), 1, "P1/E1");
        let visible = confined.logical_path(&entries[0].search_relative);
        assert_eq!(visible, Path::new("/managed/inside.txt"), "P1/E2");
        assert!(!visible.starts_with(parent.path()), "P1/E3");
        assert!(!visible.starts_with(outside.path()), "P1/E3");
    }
}
