//! In-process hand tools (ADR-0007). `read`, `write`, `edit`, `move`, `delete`,
//! `glob`, `grep`, and `bash` run directly in the runtime process and render results as text.
//! Their ids match the descriptors in [`crate::builtin_tools`], so a run that
//! makes a descriptor model-visible can register the matching implementation.
//! Network fetch and the separately configured search plugin live in [`crate::web`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError, ToolExecutionTarget};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::erasure::erase_for;

const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024;
const BASH_OUTPUT_LIMIT: usize = 100 * 1024;
const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const GREP_OUTPUT_LIMIT: usize = 100 * 1024;
const GREP_MAX_LINE_LENGTH: usize = 2_000;
const GLOB_RESULT_LIMIT: usize = 200;
const GLOB_EXPANSION_LIMIT: usize = 256;
const WALK_MAX_DEPTH: usize = 40;
const WALK_MAX_ENTRIES: usize = 50_000;

/// Trusted host configuration for one Managed Agent toolset instance.
/// Instances are scoped to a Hand connection, so Bash state cannot cross Sessions.
#[derive(Clone, Debug)]
pub struct HandToolContext {
    workdir: PathBuf,
    allowed_roots: Vec<PathBuf>,
    path_projections: Vec<(PathBuf, PathBuf)>,
    max_file_bytes: Option<u64>,
    bash_env: Option<BTreeMap<String, String>>,
    /// Trusted launcher for the persistent shell. Providers use this to enter a
    /// sandbox once when Bash starts instead of wrapping every command and
    /// losing shell state between calls.
    bash_launcher: Option<(PathBuf, Vec<String>)>,
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
    /// logical path maps to the Session's private host projection. Only trusted
    /// composition code can install this mapping; model-authored paths cannot.
    #[must_use]
    pub fn with_path_projection(
        mut self,
        logical_root: impl Into<PathBuf>,
        physical_root: impl Into<PathBuf>,
    ) -> Self {
        let physical_root = physical_root.into();
        self.allowed_roots.push(physical_root.clone());
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
        self.bash_launcher = Some((program.into(), args.into_iter().collect()));
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
            .map_err(|error| ToolError::Execution(format!("workdir: {error}")))
    }

    fn resolve(&self, input: &str) -> Result<ConfinedPath, ToolError> {
        if input.is_empty() {
            return Err(ToolError::InvalidArguments("file path is required".into()));
        }
        let root = canonicalize_or_absolute(&self.0.workdir)?;
        let input_path = lexical_normalize(Path::new(input));
        let candidate = if input_path.is_absolute() {
            self.0
                .path_projections
                .iter()
                .filter_map(|(logical, physical)| {
                    input_path
                        .strip_prefix(logical)
                        .ok()
                        .map(|suffix| (logical.components().count(), physical.join(suffix)))
                })
                .max_by_key(|(specificity, _)| *specificity)
                .map_or(input_path, |(_, projected)| lexical_normalize(&projected))
        } else {
            lexical_normalize(&root.join(input_path))
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
            .ok_or_else(|| ToolError::Execution(format!("path {input:?} escapes workdir")))?;
        let relative = resolved
            .strip_prefix(&root)
            .expect("selected root contains the resolved path")
            .to_path_buf();
        let directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|error| file_error("path", input, &error))?;
        Ok(ConfinedPath {
            absolute: resolved,
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
            .ok_or_else(|| ToolError::Execution("glob: pattern escapes workdir".into()))?;
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
    fn logical_path(&self, physical: &Path) -> PathBuf {
        self.0
            .path_projections
            .iter()
            .filter_map(|(logical, projected)| {
                let projected = canonicalize_or_absolute(projected).ok()?;
                physical.strip_prefix(&projected).ok().map(|suffix| {
                    (
                        projected.components().count(),
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
    relative: PathBuf,
    directory: Arc<cap_std::fs::Dir>,
}

impl ConfinedPath {
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

    fn rename_to(&self, destination: &Self) -> std::io::Result<()> {
        self.directory.rename(
            &self.relative,
            &destination.directory,
            &destination.relative,
        )
    }

    fn remove_file(&self) -> std::io::Result<()> {
        self.directory.remove_file(&self.relative)
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
        .map_err(|error| ToolError::Execution(format!("workdir: {error}")))
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, ToolError> {
    if path.is_absolute() {
        Ok(lexical_normalize(path))
    } else {
        std::env::current_dir()
            .map(|cwd| lexical_normalize(&cwd.join(path)))
            .map_err(|error| ToolError::Execution(format!("workdir: {error}")))
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
                out.pop();
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
    ToolError::Execution(format!("{operation}: {input}: {message}"))
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
        return Err(ToolError::Execution(format!(
            "{operation}: {input} is not a regular file"
        )));
    }
    if let Some(limit) = limit
        && metadata.len() > limit
    {
        let advice = if operation == "read" {
            "Use bash (head/tail/sed) to read a slice."
        } else {
            "Use bash (sed/awk) to edit a large file."
        };
        return Err(ToolError::Execution(format!(
            "{operation}: {input} is {} bytes, exceeds {limit}-byte limit. {advice}",
            metadata.len()
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

#[async_trait]
impl Tool for ReadTool {
    type Args = ReadArgs;
    type Output = String;
    const ID: &'static str = "read";
    const DESCRIPTION: &'static str = "Read a file";

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        if args.path.is_empty() {
            return Err(ToolError::InvalidArguments(
                "read: file_path is required".into(),
            ));
        }
        let path = self.0.resolve(&args.path)?;
        regular_file(&path, &args.path, "read", self.0.max_file_bytes())?;
        let content = path
            .read_to_string()
            .map_err(|error| file_error("read", &args.path, &error))?;
        let Some(range) = args.view_range else {
            return Ok(content);
        };
        let lines = content.split('\n').collect::<Vec<_>>();
        let start = usize::try_from((range[0] - 1).max(0)).unwrap_or(usize::MAX);
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
            return Err(ToolError::Execution(
                "glob: \"..\" is not permitted in the pattern".into(),
            ));
        }
        let (root, relative_pattern) = self.0.resolve_glob(&args.pattern, args.path.as_deref())?;
        let patterns = expand_glob_alternatives(&relative_pattern)?;
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();
        for relative_pattern in patterns {
            let pattern = root.join(&relative_pattern).display().to_string();
            let entries = glob::glob(&pattern)
                .map_err(|err| ToolError::InvalidArguments(format!("glob {pattern}: {err}")))?;
            for entry in entries {
                let path =
                    entry.map_err(|err| ToolError::Execution(format!("glob walk: {err}")))?;
                if path
                    .components()
                    .any(|component| matches!(component, Component::Normal(name) if name == ".git" || name == "node_modules"))
                {
                    continue;
                }
                let Ok(real) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !path_is_within(&root, &real) || !seen.insert(real.clone()) {
                    continue;
                }
                let Ok(metadata) = std::fs::metadata(&real) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let modified = metadata
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                paths.push((modified, self.0.logical_path(&path).display().to_string()));
            }
        }
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
        if let Some(output) = run_ripgrep(&args.pattern, &root).await? {
            return Ok(output);
        }
        let re = regex::Regex::new(&args.pattern)
            .map_err(|err| ToolError::InvalidArguments(format!("grep: invalid regex: {err}")))?;
        let mut files = Vec::new();
        let mut remaining = WALK_MAX_ENTRIES;
        collect_files(&root, &mut files, 0, &mut remaining)?;
        files.sort();
        let mut hits = Vec::new();
        let mut output_bytes = 0;
        for path in files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (index, line) in content.split('\n').enumerate() {
                if line.len() > GREP_MAX_LINE_LENGTH {
                    continue;
                }
                if re.is_match(line) {
                    let hit = format!("{}:{}:{}", path.display(), index + 1, line);
                    if output_bytes + hit.len() + 1 > GREP_OUTPUT_LIMIT {
                        hits.push(format!("[output truncated at {GREP_OUTPUT_LIMIT} bytes]"));
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

/// Match the official Node helper's preferred path: use ripgrep when it is on
/// PATH, and return None only when the executable is absent so the bounded
/// in-process walker can take over.
async fn run_ripgrep(pattern: &str, path: &Path) -> Result<Option<String>, ToolError> {
    let mut child = match tokio::process::Command::new("rg")
        .args(["-n", "--no-heading", "-e", pattern, "--"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ToolError::Execution(format!("grep: rg failed: {error}")));
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout
            .take((GREP_OUTPUT_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr
            .take((GREP_OUTPUT_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    });
    let output = stdout_task
        .await
        .map_err(|error| ToolError::Execution(format!("grep: rg stdout: {error}")))?
        .map_err(|error| ToolError::Execution(format!("grep: rg stdout: {error}")))?;
    let truncated = output.len() > GREP_OUTPUT_LIMIT;
    if truncated {
        let _ = child.kill().await;
    }
    let status = child
        .wait()
        .await
        .map_err(|error| ToolError::Execution(format!("grep: rg failed: {error}")))?;
    let error = stderr_task
        .await
        .map_err(|error| ToolError::Execution(format!("grep: rg stderr: {error}")))?
        .map_err(|error| ToolError::Execution(format!("grep: rg stderr: {error}")))?;
    if truncated {
        let prefix = String::from_utf8_lossy(&output[..GREP_OUTPUT_LIMIT]);
        return Ok(Some(format!(
            "{prefix}\n[output truncated at {GREP_OUTPUT_LIMIT} bytes]"
        )));
    }
    match status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&output).into_owned())),
        Some(1) => Ok(Some("no matches".into())),
        _ => {
            let detail = String::from_utf8_lossy(&error);
            let detail = if detail.is_empty() {
                format!("exit {status}")
            } else {
                detail.into_owned()
            };
            Err(ToolError::Execution(format!("grep: rg failed: {detail}")))
        }
    }
}

fn collect_files(
    path: &Path,
    files: &mut Vec<PathBuf>,
    depth: usize,
    remaining: &mut usize,
) -> Result<(), ToolError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| file_error("grep", &path.display().to_string(), &err))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() || depth > WALK_MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(path)
        .map_err(|err| ToolError::Execution(format!("read directory {}: {err}", path.display())))?;
    for entry in entries {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        let entry =
            entry.map_err(|err| ToolError::Execution(format!("walk {}: {err}", path.display())))?;
        if matches!(entry.file_name().to_str(), Some(".git" | "node_modules")) {
            continue;
        }
        let file_type = entry.file_type().map_err(|err| {
            ToolError::Execution(format!("stat {}: {err}", entry.path().display()))
        })?;
        if file_type.is_dir() {
            collect_files(&entry.path(), files, depth + 1, remaining)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
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
            args.path
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
            0 => Err(ToolError::Execution(format!(
                "edit: old_string not found in {}",
                args.path
            ))),
            1 => {
                let updated = content.replacen(&args.old, &args.new, 1);
                atomic_write(&path, &updated)
                    .map_err(|error| file_error("edit: write", &args.path, &error))?;
                Ok(format!("edited {} (1 replacement(s))", args.path))
            }
            n if args.replace_all => {
                let updated = content.replace(&args.old, &args.new);
                atomic_write(&path, &updated)
                    .map_err(|error| file_error("edit: write", &args.path, &error))?;
                Ok(format!("edited {} ({n} replacements)", args.path))
            }
            n => Err(ToolError::Execution(format!(
                "edit: old_string appears {n} times in {} (must be unique)",
                args.path,
            ))),
        }
    }
}

/// Move or rename one file. Directory trees are intentionally unsupported so
/// callers cannot turn a narrowly-scoped file operation into a recursive move.
pub struct MoveTool(FileContext);

impl MoveTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    /// Absolute source file path.
    pub source: String,
    /// Absolute destination file path.
    pub destination: String,
}

#[async_trait]
impl Tool for MoveTool {
    type Args = MoveArgs;
    type Output = String;
    const ID: &'static str = "move";
    const DESCRIPTION: &'static str = "Move or rename a file";

    async fn call(&self, args: MoveArgs) -> Result<String, ToolError> {
        let source = self.0.resolve(&args.source)?;
        let destination = self.0.resolve(&args.destination)?;
        if !source
            .metadata()
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err(ToolError::Execution(format!(
                "move {}: source is not a file",
                args.source
            )));
        }
        destination
            .create_parent_dirs()
            .map_err(|error| ToolError::Execution(format!("move {}: {error}", args.destination)))?;
        source.rename_to(&destination).map_err(|error| {
            ToolError::Execution(format!(
                "move {} to {}: {error}",
                args.source, args.destination
            ))
        })?;
        Ok(format!("moved {} to {}", args.source, args.destination))
    }
}

/// Delete exactly one regular file. Directories are rejected; recursive deletion
/// remains outside the model-callable capability surface.
pub struct DeleteTool(FileContext);

impl DeleteTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self(FileContext::new(context))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    /// Absolute file path to delete.
    pub path: String,
}

#[async_trait]
impl Tool for DeleteTool {
    type Args = DeleteArgs;
    type Output = String;
    const ID: &'static str = "delete";
    const DESCRIPTION: &'static str = "Delete one file";

    async fn call(&self, args: DeleteArgs) -> Result<String, ToolError> {
        let path = self.0.resolve(&args.path)?;
        if !path
            .metadata()
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err(ToolError::Execution(format!(
                "delete {}: path is not a file",
                args.path
            )));
        }
        path.remove_file()
            .map_err(|error| ToolError::Execution(format!("delete {}: {error}", args.path)))?;
        Ok(format!("deleted {}", args.path))
    }
}

/// A persistent Bash scoped to one Hand/toolset instance.
pub struct BashTool {
    context: HandToolContext,
    session: tokio::sync::Mutex<Option<BashSession>>,
}

impl BashTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self {
            context: context.clone(),
            session: tokio::sync::Mutex::new(None),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// Shell command to execute.
    #[serde(default)]
    pub command: String,
    /// Restart the runner-side Bash session before executing the command.
    #[serde(default)]
    pub restart: bool,
    /// Invocation timeout in milliseconds; zero selects the default timeout.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    type Args = BashArgs;
    type Output = String;
    const ID: &'static str = "bash";
    const DESCRIPTION: &'static str = "Run a shell command";

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        let mut session = self.session.lock().await;
        if args.restart {
            session.take();
            if args.command.is_empty() {
                return Ok("bash session restarted".into());
            }
        }
        if args.command.is_empty() {
            return Err(ToolError::InvalidArguments(
                "bash: command is required".into(),
            ));
        }
        if session
            .as_ref()
            .is_some_and(|existing| existing.invalidated.load(Ordering::SeqCst))
        {
            session.take();
        }
        if session.is_none() {
            *session = Some(BashSession::spawn(&self.context)?);
        }
        let timeout_ms = args
            .timeout_ms
            .filter(|timeout| *timeout > 0)
            .unwrap_or(BASH_DEFAULT_TIMEOUT_MS);
        let result = session
            .as_mut()
            .expect("Bash session was initialized")
            .exec(&args.command, timeout_ms)
            .await;
        let (output, exit_code) = match result {
            Ok(result) => result,
            Err(error) => {
                session.take();
                return Err(error);
            }
        };
        if exit_code == 0 {
            Ok(output)
        } else {
            Err(ToolError::Execution(if output.is_empty() {
                format!("exit {exit_code}")
            } else {
                output
            }))
        }
    }
}

struct BashSession {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    invalidated: Arc<AtomicBool>,
    process_group: ProcessGroupGuard,
}

impl BashSession {
    fn spawn(context: &HandToolContext) -> Result<Self, ToolError> {
        let mut command = if let Some((program, args)) = &context.bash_launcher {
            let mut command = tokio::process::Command::new(program);
            command.args(args);
            command
        } else {
            persistent_bash_command()
        };
        command
            .current_dir(&context.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = &context.bash_env {
            command.env_clear().envs(env);
        } else {
            for key in std::env::vars_os()
                .map(|(key, _)| key)
                .filter(|key| key.to_string_lossy().starts_with("ANTHROPIC_"))
            {
                command.env_remove(key);
            }
        }
        command.env("PS1", "").env("PS2", "").env("TERM", "dumb");
        configure_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Execution(format!("spawn /bin/bash: {error}")))?;
        let process_group = ProcessGroupGuard::new(child.id());
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Execution("capture /bin/bash stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("capture /bin/bash stdout".into()))?;
        let stderr = child.stderr.take();
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            stderr,
            invalidated: Arc::new(AtomicBool::new(false)),
            process_group,
        })
    }

    async fn exec(&mut self, command: &str, timeout_ms: u64) -> Result<(String, i32), ToolError> {
        let sentinel = format!("__ANT_CMD_{}_DONE__", uuid::Uuid::new_v4());
        let split = format!("{}''{}", &sentinel[..8], &sentinel[8..]);
        let wrapped = format!("{{ {command}\n}} </dev/null 2>&1; printf '\\n{split}%d\\n' $?\n");
        self.stdin
            .write_all(wrapped.as_bytes())
            .await
            .map_err(|error| ToolError::Execution(format!("bash: {error}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| ToolError::Execution(format!("bash: {error}")))?;

        let invalidated = self.invalidated.clone();
        let mut cancellation_guard =
            InvocationProcessGuard::new(self.process_group.pid(), invalidated.clone());
        let read = async {
            let mut collected = Vec::new();
            let mut truncated = false;
            let mut stdout_chunk = [0_u8; 8192];
            let mut stderr_chunk = [0_u8; 8192];
            loop {
                let (read, bytes) = if let Some(stderr) = self.stderr.as_mut() {
                    tokio::select! {
                        result = self.stdout.read(&mut stdout_chunk) => {
                            result.map(|read| (read, &stdout_chunk[..read]))
                        },
                        result = stderr.read(&mut stderr_chunk) => {
                            result.map(|read| (read, &stderr_chunk[..read]))
                        },
                    }
                } else {
                    self.stdout
                        .read(&mut stdout_chunk)
                        .await
                        .map(|read| (read, &stdout_chunk[..read]))
                }
                .map_err(|error| ToolError::Execution(format!("bash: {error}")))?;
                if read == 0 {
                    return Err(ToolError::Execution("bash: bash session terminated".into()));
                }
                collected.extend_from_slice(bytes);
                if collected.len() > BASH_OUTPUT_LIMIT {
                    let drain = collected.len() - BASH_OUTPUT_LIMIT;
                    collected.drain(..drain);
                    truncated = true;
                }
                if let Some(index) = find_bytes(&collected, sentinel.as_bytes()) {
                    let tail = &collected[index + sentinel.len()..];
                    let digits = tail
                        .iter()
                        .take_while(|byte| byte.is_ascii_digit() || **byte == b'-')
                        .copied()
                        .collect::<Vec<_>>();
                    let exit_code = String::from_utf8_lossy(&digits).parse().unwrap_or(-1);
                    let mut output = strip_ansi(&String::from_utf8_lossy(&collected[..index]));
                    while output.ends_with('\n') {
                        output.pop();
                    }
                    if truncated {
                        output.insert_str(0, "[output truncated]\n");
                    }
                    return Ok((output, exit_code));
                }
            }
        };
        let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read).await;
        match result {
            Ok(result) => {
                cancellation_guard.disarm();
                result
            }
            Err(_) => {
                invalidated.store(true, Ordering::SeqCst);
                Err(ToolError::Execution(format!(
                    "bash: bash command timed out after {timeout_ms}ms"
                )))
            }
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            if bytes[index] != 0 {
                output.push(bytes[index]);
            }
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

struct InvocationProcessGuard {
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
    invalidated: Arc<AtomicBool>,
}

#[cfg(unix)]
impl InvocationProcessGuard {
    fn new(group: Option<nix::unistd::Pid>, invalidated: Arc<AtomicBool>) -> Self {
        Self { group, invalidated }
    }

    fn disarm(&mut self) {
        self.group.take();
    }
}

#[cfg(windows)]
impl InvocationProcessGuard {
    fn new(_group: Option<()>, invalidated: Arc<AtomicBool>) -> Self {
        Self { invalidated }
    }

    fn disarm(&mut self) {}
}

#[cfg(unix)]
impl Drop for InvocationProcessGuard {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            self.invalidated.store(true, Ordering::SeqCst);
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

#[cfg(windows)]
impl Drop for InvocationProcessGuard {
    fn drop(&mut self) {
        self.invalidated.store(true, Ordering::SeqCst);
    }
}

#[cfg(all(test, not(windows)))]
mod bash_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn bash_wait_does_not_starve_runtime_control_tasks() {
        // A long local command must yield the sole async runtime thread. Worker
        // heartbeat and dispatch renewal use the same scheduling path in the
        // served runtime, so blocking here previously expired both authorities.
        let control_task_ran = Arc::new(AtomicBool::new(false));
        let observed = control_task_ran.clone();
        let control = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            observed.store(true, Ordering::SeqCst);
        });

        BashTool::new(&HandToolContext::default())
            .call(BashArgs {
                command: "sleep 0.15".into(),
                restart: false,
                timeout_ms: None,
            })
            .await
            .expect("sleep command");

        assert!(control_task_ran.load(Ordering::SeqCst));
        control.await.expect("control task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_bash_call_kills_its_descendant_process_group() {
        // Cancellation previously dropped only the blocking-task join handle,
        // leaving the shell and descendants alive beneath the Worker service.
        // A descendant reports readiness, would write `leaked` after cancellation,
        // and is required to disappear with its process group instead.
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let ready = directory.path().join("ready");
        let leaked = directory.path().join("leaked");
        let command = format!(
            "sh -c 'printf ready > {}; sleep 0.4; printf leaked > {}' & wait",
            ready.display(),
            leaked.display()
        );
        let tool = BashTool::new(&HandToolContext::new(directory.path()));
        let call = tokio::spawn(async move {
            tool.call(BashArgs {
                command,
                restart: false,
                timeout_ms: None,
            })
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !ready.is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant becomes ready");

        call.abort();
        let _ = call.await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !leaked.exists(),
            "a descendant survived cancellation and wrote {}",
            leaked.display()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_bash_call_keeps_background_jobs_until_restart() {
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let leaked = directory.path().join("leaked");
        let tool = BashTool::new(&HandToolContext::new(directory.path()));
        let command = format!(
            "sh -c 'sleep 0.4; printf leaked > {}' & printf done",
            leaked.display()
        );

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tool.call(BashArgs {
                command,
                restart: false,
                timeout_ms: None,
            }),
        )
        .await
        .expect("an unredirected background child must not hold the tool pipe open")
        .expect("foreground shell succeeds");
        assert_eq!(output, "done");

        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            leaked.exists(),
            "persistent Bash must preserve background jobs across calls"
        );
        tool.call(BashArgs {
            command: String::new(),
            restart: true,
            timeout_ms: None,
        })
        .await
        .expect("restart closes the old process group");
    }
}

#[cfg(windows)]
fn persistent_bash_command() -> tokio::process::Command {
    let shell = windows_posix_shell();
    let mut process = tokio::process::Command::new(shell.as_deref().unwrap_or("cmd.exe"));
    if shell.is_some() {
        process.args(["--noprofile", "--norc"]);
    } else {
        process.args(["/D", "/Q"]);
    }
    process
}

#[cfg(windows)]
fn windows_posix_shell() -> Option<String> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let shell = directory.join("sh.exe");
        if shell.is_file() {
            return Some(shell.to_string_lossy().into_owned());
        }
        let git = directory.join("git.exe");
        if git.is_file() && directory.file_name().is_some_and(|name| name == "cmd") {
            let shell = directory.parent()?.join("bin").join("sh.exe");
            if shell.is_file() {
                return Some(shell.to_string_lossy().into_owned());
            }
        }
    }
    std::env::var_os("ProgramFiles")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("Git").join("bin").join("sh.exe"))
        .filter(|shell| shell.is_file())
        .map(|shell| shell.to_string_lossy().into_owned())
}

#[cfg(not(windows))]
fn persistent_bash_command() -> tokio::process::Command {
    let mut process = tokio::process::Command::new("/bin/bash");
    process.args(["--noprofile", "--norc"]);
    process
}

fn configure_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// Synchronously terminates a Unix process group when a tool future finishes or
/// is dropped. Tokio's `kill_on_drop` covers the direct child on every platform;
/// this guard extends that guarantee to background descendants on Unix.
struct ProcessGroupGuard {
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            group: pid.map(|value| nix::unistd::Pid::from_raw(value as i32)),
        }
    }

    #[cfg(unix)]
    fn pid(&self) -> Option<nix::unistd::Pid> {
        self.group
    }

    #[cfg(windows)]
    fn pid(&self) -> Option<()> {
        None
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// The local hand tools, erased for `Runtime::with_tool` registration. Both
/// network tools are owned exclusively by their configured plugin paths.
pub fn executable_hand_tools() -> Vec<Arc<dyn RawTool>> {
    executable_hand_tools_in(HandToolContext::default())
}

/// Create a fresh, environment-scoped toolset. The Bash session and filesystem
/// confinement share this trusted workdir.
pub fn executable_hand_tools_in(context: HandToolContext) -> Vec<Arc<dyn RawTool>> {
    vec![
        erase_for(ReadTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(WriteTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(EditTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(MoveTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(DeleteTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(GlobTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(GrepTool::new(&context), ToolExecutionTarget::Sandbox),
        erase_for(BashTool::new(&context), ToolExecutionTarget::Sandbox),
    ]
}

#[cfg(test)]
mod write_tests {
    use super::*;

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

    #[tokio::test]
    async fn move_and_delete_are_single_file_operations() {
        // Causes: M1 a regular source file and nested destination; M2 a regular
        // destination file; M3 a directory passed to delete.
        // Constraints: move/delete operate on one file and never recurse.
        // Effects: M1 preserves bytes at the new path, M2 removes that file, and
        // M3 fails without changing the directory tree.
        // Decision rules: M1 move success; M2 delete success; M3 directory reject.
        let base = std::env::temp_dir().join(format!("awaken-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let source = base.join("source.md");
        let destination = base.join("nested/destination.md");
        std::fs::write(&source, "durable").unwrap();
        let context = HandToolContext::new(&base);
        MoveTool::new(&context)
            .call(MoveArgs {
                source: source.to_string_lossy().into_owned(),
                destination: destination.to_string_lossy().into_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "durable",
            "M1"
        );
        DeleteTool::new(&context)
            .call(DeleteArgs {
                path: destination.to_string_lossy().into_owned(),
            })
            .await
            .unwrap();
        assert!(!destination.exists(), "M2");
        assert!(
            DeleteTool::new(&context)
                .call(DeleteArgs {
                    path: base.to_string_lossy().into_owned(),
                })
                .await
                .is_err(),
            "M3"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
