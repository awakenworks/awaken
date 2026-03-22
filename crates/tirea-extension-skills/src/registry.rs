use crate::materialize::{load_asset_material, load_reference_material, run_script_material};
use crate::skill_md::{parse_allowed_tools, parse_skill_md, SkillFrontmatter};
use crate::{
    ScriptResult, Skill, SkillError, SkillMaterializeError, SkillMeta, SkillResource,
    SkillResourceKind, SkillWarning,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

/// A filesystem-backed skill.
///
/// Each `FsSkill` owns its root directory and SKILL.md path. Resource loading
/// and script execution are performed relative to `root_dir`.
///
/// Use [`FsSkill::discover`] to scan a directory for skills, or
/// [`FsSkill::discover_roots`] for multiple directories.
#[derive(Debug, Clone)]
pub struct FsSkill {
    meta: SkillMeta,
    root_dir: PathBuf,
    skill_md_path: PathBuf,
}

/// Result of a discovery scan: found skills and any warnings.
#[derive(Debug, Clone)]
pub struct DiscoveryResult {
    pub skills: Vec<FsSkill>,
    pub warnings: Vec<SkillWarning>,
}

pub trait SkillRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>>;

    fn start_periodic_refresh(&self, _interval: Duration) -> Result<(), SkillRegistryManagerError> {
        Ok(())
    }

    fn stop_periodic_refresh(&self) -> bool {
        false
    }

    fn periodic_refresh_running(&self) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SkillRegistryError {
    #[error("skill id must be non-empty")]
    EmptySkillId,

    #[error("duplicate skill id: {0}")]
    DuplicateSkillId(String),
}

#[derive(Clone, Default)]
pub struct InMemorySkillRegistry {
    skills: HashMap<String, Arc<dyn Skill>>,
}

impl std::fmt::Debug for InMemorySkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemorySkillRegistry")
            .field("len", &self.skills.len())
            .finish()
    }
}

impl InMemorySkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn from_skills(skills: Vec<Arc<dyn Skill>>) -> Self {
        let mut registry = Self::new();
        registry.extend_upsert(skills);
        registry
    }

    pub fn register(&mut self, skill: Arc<dyn Skill>) -> Result<(), SkillRegistryError> {
        let id = skill.meta().id.trim().to_string();
        if id.is_empty() {
            return Err(SkillRegistryError::EmptySkillId);
        }
        if self.skills.contains_key(&id) {
            return Err(SkillRegistryError::DuplicateSkillId(id));
        }
        self.skills.insert(id, skill);
        Ok(())
    }

    pub fn extend_upsert(&mut self, skills: Vec<Arc<dyn Skill>>) {
        for skill in skills {
            let id = skill.meta().id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            self.skills.insert(id, skill);
        }
    }

    pub fn extend_registry(&mut self, other: &dyn SkillRegistry) -> Result<(), SkillRegistryError> {
        for (_, skill) in other.snapshot() {
            self.register(skill)?;
        }
        Ok(())
    }
}

impl SkillRegistry for InMemorySkillRegistry {
    fn len(&self) -> usize {
        self.skills.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        self.skills.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.skills.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
        self.skills.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeSkillRegistry {
    registries: Vec<Arc<dyn SkillRegistry>>,
    cached_snapshot: Arc<RwLock<HashMap<String, Arc<dyn Skill>>>>,
}

impl std::fmt::Debug for CompositeSkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.cached_snapshot);
        f.debug_struct("CompositeSkillRegistry")
            .field("registries", &self.registries.len())
            .field("len", &snapshot.len())
            .finish()
    }
}

impl CompositeSkillRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn SkillRegistry>>,
    ) -> Result<Self, SkillRegistryError> {
        let registries: Vec<Arc<dyn SkillRegistry>> = regs.into_iter().collect();
        let merged = Self::merge_snapshots(&registries)?;
        Ok(Self {
            registries,
            cached_snapshot: Arc::new(RwLock::new(merged)),
        })
    }

    fn merge_snapshots(
        registries: &[Arc<dyn SkillRegistry>],
    ) -> Result<HashMap<String, Arc<dyn Skill>>, SkillRegistryError> {
        let mut merged = InMemorySkillRegistry::new();
        for reg in registries {
            merged.extend_registry(reg.as_ref())?;
        }
        Ok(merged.snapshot())
    }

    fn refresh_snapshot(&self) -> Result<HashMap<String, Arc<dyn Skill>>, SkillRegistryError> {
        Self::merge_snapshots(&self.registries)
    }

    fn read_cached_snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
        read_lock(&self.cached_snapshot).clone()
    }

    fn write_cached_snapshot(&self, snapshot: HashMap<String, Arc<dyn Skill>>) {
        *write_lock(&self.cached_snapshot) = snapshot;
    }
}

impl SkillRegistry for CompositeSkillRegistry {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        self.snapshot().get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
        match self.refresh_snapshot() {
            Ok(snapshot) => {
                self.write_cached_snapshot(snapshot.clone());
                snapshot
            }
            Err(_) => self.read_cached_snapshot(),
        }
    }

    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), SkillRegistryManagerError> {
        let mut started: Vec<Arc<dyn SkillRegistry>> = Vec::new();
        for registry in &self.registries {
            match registry.start_periodic_refresh(interval) {
                Ok(()) => started.push(registry.clone()),
                Err(SkillRegistryManagerError::PeriodicRefreshAlreadyRunning) => {}
                Err(err) => {
                    for reg in started {
                        let _ = reg.stop_periodic_refresh();
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn stop_periodic_refresh(&self) -> bool {
        let mut stopped = false;
        for registry in &self.registries {
            stopped |= registry.stop_periodic_refresh();
        }
        stopped
    }

    fn periodic_refresh_running(&self) -> bool {
        self.registries
            .iter()
            .any(|registry| registry.periodic_refresh_running())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SkillRegistryManagerError {
    #[error(transparent)]
    Skill(#[from] SkillError),

    #[error(transparent)]
    Registry(#[from] SkillRegistryError),

    #[error(transparent)]
    Mcp(#[from] tirea_extension_mcp::McpToolRegistryError),

    #[error("skill roots list must be non-empty")]
    EmptyRoots,

    #[error("periodic refresh interval must be > 0")]
    InvalidRefreshInterval,

    #[error("periodic refresh loop is already running")]
    PeriodicRefreshAlreadyRunning,

    #[error("tokio runtime is required to start periodic refresh")]
    RuntimeUnavailable,

    #[error("periodic refresh join failed: {0}")]
    Join(String),
}

#[derive(Clone, Default)]
struct SkillRegistrySnapshot {
    version: u64,
    skills: HashMap<String, Arc<dyn Skill>>,
    warnings: Vec<SkillWarning>,
}

struct PeriodicRefreshRuntime {
    stop_tx: Option<mpsc::Sender<()>>,
    join: JoinHandle<()>,
}

struct SkillRegistryState {
    roots: Vec<PathBuf>,
    snapshot: RwLock<SkillRegistrySnapshot>,
    periodic_refresh: Mutex<Option<PeriodicRefreshRuntime>>,
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn is_periodic_refresh_running(state: &SkillRegistryState) -> bool {
    let mut runtime = mutex_lock(&state.periodic_refresh);
    if runtime
        .as_ref()
        .is_some_and(|running| running.join.is_finished())
    {
        if let Some(finished) = runtime.take() {
            let _ = finished.join.join();
        }
        return false;
    }
    runtime.is_some()
}

type DiscoverSnapshot = (HashMap<String, Arc<dyn Skill>>, Vec<SkillWarning>);

fn discover_snapshot_from_roots(
    roots: &[PathBuf],
) -> Result<DiscoverSnapshot, SkillRegistryManagerError> {
    let discovered = FsSkill::discover_roots(roots.to_vec())?;
    let mut map: HashMap<String, Arc<dyn Skill>> = HashMap::new();
    for skill in discovered.skills {
        let arc = Arc::new(skill) as Arc<dyn Skill>;
        let id = arc.meta().id.trim().to_string();
        if id.is_empty() {
            return Err(SkillRegistryError::EmptySkillId.into());
        }
        if map.insert(id.clone(), arc).is_some() {
            return Err(SkillRegistryError::DuplicateSkillId(id).into());
        }
    }
    Ok((map, discovered.warnings))
}

async fn refresh_state(state: &SkillRegistryState) -> Result<u64, SkillRegistryManagerError> {
    let roots = state.roots.clone();
    let handle = tokio::task::spawn_blocking(move || discover_snapshot_from_roots(&roots));
    let (skills, warnings) = handle
        .await
        .map_err(|e| SkillRegistryManagerError::Join(e.to_string()))??;
    Ok(apply_snapshot(state, skills, warnings))
}

fn apply_snapshot(
    state: &SkillRegistryState,
    skills: HashMap<String, Arc<dyn Skill>>,
    warnings: Vec<SkillWarning>,
) -> u64 {
    let mut snapshot = write_lock(&state.snapshot);
    let version = snapshot.version.saturating_add(1);
    *snapshot = SkillRegistrySnapshot {
        version,
        skills,
        warnings,
    };
    version
}

fn refresh_state_blocking(state: &SkillRegistryState) -> Result<u64, SkillRegistryManagerError> {
    let (skills, warnings) = discover_snapshot_from_roots(&state.roots)?;
    Ok(apply_snapshot(state, skills, warnings))
}

fn periodic_refresh_loop(
    state: Weak<SkillRegistryState>,
    interval: Duration,
    stop_rx: mpsc::Receiver<()>,
) {
    loop {
        match stop_rx.recv_timeout(interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(state) = state.upgrade() else {
                    break;
                };
                let _ = refresh_state_blocking(state.as_ref());
            }
        }
    }
}

#[derive(Clone)]
pub struct FsSkillRegistryManager {
    state: Arc<SkillRegistryState>,
}

impl std::fmt::Debug for FsSkillRegistryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.state.snapshot);
        let periodic_running = is_periodic_refresh_running(self.state.as_ref());
        f.debug_struct("FsSkillRegistryManager")
            .field("roots", &self.state.roots.len())
            .field("skills", &snapshot.skills.len())
            .field("warnings", &snapshot.warnings.len())
            .field("version", &snapshot.version)
            .field("periodic_refresh_running", &periodic_running)
            .finish()
    }
}

impl FsSkillRegistryManager {
    pub fn discover_roots(roots: Vec<PathBuf>) -> Result<Self, SkillRegistryManagerError> {
        if roots.is_empty() {
            return Err(SkillRegistryManagerError::EmptyRoots);
        }
        let (skills, warnings) = discover_snapshot_from_roots(&roots)?;
        let snapshot = SkillRegistrySnapshot {
            version: 1,
            skills,
            warnings,
        };
        Ok(Self {
            state: Arc::new(SkillRegistryState {
                roots,
                snapshot: RwLock::new(snapshot),
                periodic_refresh: Mutex::new(None),
            }),
        })
    }

    pub async fn refresh(&self) -> Result<u64, SkillRegistryManagerError> {
        refresh_state(self.state.as_ref()).await
    }

    pub fn start_periodic_refresh(
        &self,
        interval: Duration,
    ) -> Result<(), SkillRegistryManagerError> {
        if interval.is_zero() {
            return Err(SkillRegistryManagerError::InvalidRefreshInterval);
        }
        let mut runtime = mutex_lock(&self.state.periodic_refresh);
        if runtime
            .as_ref()
            .is_some_and(|running| !running.join.is_finished())
        {
            return Err(SkillRegistryManagerError::PeriodicRefreshAlreadyRunning);
        }
        if let Some(finished) = runtime.take() {
            let _ = finished.join.join();
        }
        let (stop_tx, stop_rx) = mpsc::channel();
        let weak_state = Arc::downgrade(&self.state);
        let join = std::thread::Builder::new()
            .name("tirea-skills-registry-refresh".to_string())
            .spawn(move || periodic_refresh_loop(weak_state, interval, stop_rx))
            .map_err(|e| SkillRegistryManagerError::Join(e.to_string()))?;
        *runtime = Some(PeriodicRefreshRuntime {
            stop_tx: Some(stop_tx),
            join,
        });
        Ok(())
    }

    pub fn stop_periodic_refresh(&self) -> bool {
        let runtime = {
            let mut guard = mutex_lock(&self.state.periodic_refresh);
            guard.take()
        };
        let Some(mut runtime) = runtime else {
            return false;
        };
        if let Some(stop_tx) = runtime.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        let _ = runtime.join.join();
        true
    }

    pub fn periodic_refresh_running(&self) -> bool {
        is_periodic_refresh_running(self.state.as_ref())
    }

    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }

    pub fn warnings(&self) -> Vec<SkillWarning> {
        read_lock(&self.state.snapshot).warnings.clone()
    }
}

impl SkillRegistry for FsSkillRegistryManager {
    fn len(&self) -> usize {
        read_lock(&self.state.snapshot).skills.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        read_lock(&self.state.snapshot).skills.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let snapshot = read_lock(&self.state.snapshot);
        let mut ids: Vec<String> = snapshot.skills.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
        read_lock(&self.state.snapshot).skills.clone()
    }

    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), SkillRegistryManagerError> {
        FsSkillRegistryManager::start_periodic_refresh(self, interval)
    }

    fn stop_periodic_refresh(&self) -> bool {
        FsSkillRegistryManager::stop_periodic_refresh(self)
    }

    fn periodic_refresh_running(&self) -> bool {
        FsSkillRegistryManager::periodic_refresh_running(self)
    }
}

impl FsSkill {
    /// Discover all valid skills under a single root directory.
    pub fn discover(root: impl Into<PathBuf>) -> Result<DiscoveryResult, SkillError> {
        Self::discover_roots(vec![root.into()])
    }

    /// Discover skills under multiple root directories.
    ///
    /// Returns an error if duplicate skill IDs are found across roots.
    pub fn discover_roots(roots: Vec<PathBuf>) -> Result<DiscoveryResult, SkillError> {
        let mut skills: Vec<FsSkill> = Vec::new();
        let mut warnings: Vec<SkillWarning> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for root in roots {
            let (found, w) = discover_under_root(&root)?;
            warnings.extend(w);

            for skill in found {
                if !seen_ids.insert(skill.meta.id.clone()) {
                    return Err(SkillError::DuplicateSkillId(skill.meta.id));
                }
                skills.push(skill);
            }
        }

        skills.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
        warnings.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(DiscoveryResult { skills, warnings })
    }

    /// Collect discovered skills into a vec of trait objects.
    pub fn into_arc_skills(skills: Vec<FsSkill>) -> Vec<Arc<dyn Skill>> {
        skills
            .into_iter()
            .map(|s| Arc::new(s) as Arc<dyn Skill>)
            .collect()
    }
}

#[async_trait]
impl Skill for FsSkill {
    fn meta(&self) -> &SkillMeta {
        &self.meta
    }

    async fn read_instructions(&self) -> Result<String, SkillError> {
        fs::read_to_string(&self.skill_md_path).map_err(|e| {
            SkillError::Io(format!(
                "failed to read SKILL.md for skill '{}': {e}",
                self.meta.id
            ))
        })
    }

    async fn load_resource(
        &self,
        kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError> {
        let root = self.root_dir.clone();
        let skill_id = self.meta.id.clone();
        let path = path.to_string();

        let materialized: Result<SkillResource, SkillMaterializeError> =
            tokio::task::spawn_blocking(move || match kind {
                SkillResourceKind::Reference => {
                    load_reference_material(&skill_id, &root, &path).map(SkillResource::Reference)
                }
                SkillResourceKind::Asset => {
                    load_asset_material(&skill_id, &root, &path).map(SkillResource::Asset)
                }
            })
            .await
            .map_err(|e| SkillError::Io(e.to_string()))?;

        materialized.map_err(SkillError::from)
    }

    async fn run_script(&self, script: &str, args: &[String]) -> Result<ScriptResult, SkillError> {
        let result: Result<ScriptResult, SkillMaterializeError> =
            run_script_material(&self.meta.id, &self.root_dir, script, args).await;
        result.map_err(SkillError::from)
    }
}

fn discover_under_root(root: &Path) -> Result<(Vec<FsSkill>, Vec<SkillWarning>), SkillError> {
    let mut skills: Vec<FsSkill> = Vec::new();
    let mut warnings: Vec<SkillWarning> = Vec::new();

    let root = fs::canonicalize(root)
        .map_err(|e| SkillError::Io(format!("failed to access skills root: {e}")))?;

    let entries = fs::read_dir(&root)
        .map_err(|e| SkillError::Io(format!("failed to read skills root: {e}")))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let dir_name_raw = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if dir_name_raw.starts_with('.') {
            continue;
        }

        let dir_name = normalize_name(&dir_name_raw);
        if let Err(reason) = validate_dir_name(&dir_name) {
            warnings.push(SkillWarning {
                path: path.clone(),
                reason,
            });
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            warnings.push(SkillWarning {
                path: path.clone(),
                reason: "missing SKILL.md".to_string(),
            });
            continue;
        }

        match build_fs_skill(&dir_name, &skill_md) {
            Ok(skill) => skills.push(skill),
            Err(reason) => warnings.push(SkillWarning {
                path: skill_md,
                reason,
            }),
        }
    }

    Ok((skills, warnings))
}

fn build_fs_skill(dir_name: &str, skill_md: &Path) -> Result<FsSkill, String> {
    let root_dir = skill_md
        .parent()
        .ok_or_else(|| "invalid skill path".to_string())?
        .to_path_buf();

    let SkillFrontmatter {
        name,
        description,
        allowed_tools,
        ..
    } = read_frontmatter_from_skill_md_path(skill_md)?;

    // Spec: name must match the parent directory name.
    if name != dir_name {
        return Err(format!(
            "frontmatter name '{name}' does not match directory '{dir_name}'"
        ));
    }

    let allowed_tools = allowed_tools
        .as_deref()
        .map(parse_allowed_tools)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.raw)
        .collect::<Vec<_>>();

    Ok(FsSkill {
        meta: SkillMeta {
            id: name.clone(),
            name,
            description,
            allowed_tools,
        },
        root_dir,
        skill_md_path: skill_md.to_path_buf(),
    })
}

fn read_frontmatter_from_skill_md_path(skill_md: &Path) -> Result<SkillFrontmatter, String> {
    let file = fs::File::open(skill_md).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file);

    let mut first = String::new();
    let n = reader.read_line(&mut first).map_err(|e| e.to_string())?;
    if n == 0 || trim_line_ending(&first) != "---" {
        return Err("missing YAML frontmatter (expected leading '---' fence)".to_string());
    }

    let mut fm = String::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("unterminated YAML frontmatter (missing closing '---' fence)".to_string());
        }
        if trim_line_ending(&line) == "---" {
            break;
        }
        fm.push_str(&line);
    }

    // Reuse the same parser/validator to keep behavior consistent.
    let synthetic = format!("---\n{}---\n", fm);
    parse_skill_md(&synthetic)
        .map(|doc| doc.frontmatter)
        .map_err(|e| e.to_string())
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn normalize_name(s: &str) -> String {
    s.trim().nfkc().collect::<String>()
}

fn validate_dir_name(dir_name: &str) -> Result<(), String> {
    if dir_name.is_empty() {
        return Err("directory name must be non-empty".to_string());
    }
    if dir_name.chars().count() > 64 {
        return Err("directory name must be <= 64 characters".to_string());
    }
    if dir_name != dir_name.to_lowercase() {
        return Err("directory name must be lowercase".to_string());
    }
    if dir_name.starts_with('-') {
        return Err("directory name must not start with '-'".to_string());
    }
    if dir_name.ends_with('-') {
        return Err("directory name must not end with '-'".to_string());
    }
    if dir_name.contains("--") {
        return Err("directory name must not contain consecutive '-'".to_string());
    }
    if !dir_name.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(
            "directory name contains invalid characters (only letters, digits, and '-' are allowed)"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    #[test]
    fn discover_skills_and_parses_frontmatter() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("docx-processing")).unwrap();
        fs::write(
            skills_root.join("docx-processing").join("SKILL.md"),
            r#"---
name: docx-processing
description: Docs
allowed-tools: read_file
---
Body
"#,
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].meta.id, "docx-processing");
        assert_eq!(result.skills[0].meta.name, "docx-processing");
        assert_eq!(
            result.skills[0].meta.allowed_tools,
            vec!["read_file".to_string()]
        );
    }

    #[test]
    fn discover_skips_invalid_skills_and_reports_warnings() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("good-skill")).unwrap();
        fs::create_dir_all(skills_root.join("BadSkill")).unwrap();
        fs::create_dir_all(skills_root.join("missing-skill-md")).unwrap();
        fs::write(
            skills_root.join("good-skill").join("SKILL.md"),
            "---\nname: good-skill\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            skills_root.join("BadSkill").join("SKILL.md"),
            "---\nname: badskill\ndescription: x\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].meta.id, "good-skill");
        assert!(!result.warnings.is_empty());
        assert!(result
            .warnings
            .iter()
            .any(|w| w.reason.contains("directory name")));
        assert!(result
            .warnings
            .iter()
            .any(|w| w.reason.contains("missing SKILL.md")));
    }

    #[test]
    fn discover_skips_name_mismatch() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("good-skill")).unwrap();
        fs::write(
            skills_root.join("good-skill").join("SKILL.md"),
            "---\nname: other-skill\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert!(result.skills.is_empty());
        assert!(result
            .warnings
            .iter()
            .any(|w| w.reason.contains("does not match directory")));
    }

    #[test]
    fn discover_does_not_recurse_into_nested_dirs() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("good-skill").join("nested")).unwrap();
        fs::write(
            skills_root.join("good-skill").join("SKILL.md"),
            "---\nname: good-skill\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            skills_root
                .join("good-skill")
                .join("nested")
                .join("SKILL.md"),
            "---\nname: nested-skill\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].meta.id, "good-skill");
    }

    #[test]
    fn discover_skips_hidden_dirs_and_root_files() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join(".hidden")).unwrap();
        fs::write(
            skills_root.join(".hidden").join("SKILL.md"),
            "---\nname: hidden\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            skills_root.join("not-a-skill"),
            "---\nname: not-a-skill\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert!(result.skills.is_empty());
    }

    #[test]
    fn discover_allows_i18n_directory_names() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("技能")).unwrap();
        fs::write(
            skills_root.join("技能").join("SKILL.md"),
            "---\nname: 技能\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].meta.id, "技能");
    }

    #[tokio::test]
    async fn read_instructions_reads_from_disk_lazily() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("s1")).unwrap();
        let skill_md = skills_root.join("s1").join("SKILL.md");
        fs::write(&skill_md, "---\nname: s1\ndescription: ok\n---\nBody\n").unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        let skill = &result.skills[0];
        fs::remove_file(&skill_md).unwrap();

        let err = skill.read_instructions().await.unwrap_err();
        assert!(matches!(err, SkillError::Io(_)));
    }

    #[test]
    fn discover_does_not_parse_markdown_body() {
        let td = TempDir::new().unwrap();
        let skills_root = td.path().join("skills");
        fs::create_dir_all(skills_root.join("s1")).unwrap();
        let skill_md = skills_root.join("s1").join("SKILL.md");
        let mut bytes = b"---\nname: s1\ndescription: ok\n---\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]); // invalid UTF-8 body
        fs::write(&skill_md, bytes).unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].meta.id, "s1");
    }

    #[test]
    fn discover_errors_for_missing_root() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join("missing");
        let err = FsSkill::discover(&missing).unwrap_err();
        assert!(matches!(err, SkillError::Io(_)));
    }

    #[test]
    fn discover_rejects_duplicate_ids_across_roots() {
        let td = TempDir::new().unwrap();
        let root1 = td.path().join("skills1");
        let root2 = td.path().join("skills2");
        fs::create_dir_all(root1.join("s1")).unwrap();
        fs::create_dir_all(root2.join("s1")).unwrap();
        fs::write(
            root1.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            root2.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let err = FsSkill::discover_roots(vec![root1, root2]).unwrap_err();
        assert!(matches!(err, SkillError::DuplicateSkillId(ref id) if id == "s1"));
    }

    #[test]
    fn normalize_relative_name_nfkc() {
        let s = "a\u{FF0D}b";
        let norm = normalize_name(s);
        assert!(!norm.is_empty());
    }

    #[test]
    fn validate_dir_name_rejects_parent_dir_like_segments() {
        assert!(validate_dir_name("a/b").is_err());
        assert!(validate_dir_name("..").is_err());
    }

    #[tokio::test]
    async fn registry_manager_refresh_discovers_new_skill_without_rebuild() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("s1")).unwrap();
        fs::write(
            root.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root.clone()]).unwrap();
        assert_eq!(manager.version(), 1);
        assert_eq!(manager.ids(), vec!["s1".to_string()]);

        fs::create_dir_all(root.join("s2")).unwrap();
        fs::write(
            root.join("s2").join("SKILL.md"),
            "---\nname: s2\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let version = manager.refresh().await.unwrap();
        assert_eq!(version, 2);
        assert_eq!(manager.ids(), vec!["s1".to_string(), "s2".to_string()]);
    }

    #[tokio::test]
    async fn registry_manager_failed_refresh_keeps_last_good_snapshot() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("s1")).unwrap();
        fs::write(
            root.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root.clone()]).unwrap();
        let initial_ids = manager.ids();
        fs::remove_dir_all(&root).unwrap();

        let err = manager.refresh().await.err().unwrap();
        assert!(matches!(err, SkillRegistryManagerError::Skill(_)));
        assert_eq!(manager.version(), 1);
        assert_eq!(manager.ids(), initial_ids);
    }

    #[derive(Debug)]
    struct MockSkill {
        meta: SkillMeta,
    }

    impl MockSkill {
        fn new(id: &str) -> Self {
            Self {
                meta: SkillMeta {
                    id: id.to_string(),
                    name: id.to_string(),
                    description: format!("{id} desc"),
                    allowed_tools: Vec::new(),
                },
            }
        }
    }

    #[async_trait]
    impl Skill for MockSkill {
        fn meta(&self) -> &SkillMeta {
            &self.meta
        }

        async fn read_instructions(&self) -> Result<String, SkillError> {
            Ok(format!(
                "---\nname: {}\ndescription: ok\n---\nBody\n",
                self.meta.id
            ))
        }

        async fn load_resource(
            &self,
            _kind: SkillResourceKind,
            _path: &str,
        ) -> Result<SkillResource, SkillError> {
            Err(SkillError::Unsupported("not used".to_string()))
        }

        async fn run_script(
            &self,
            _script: &str,
            _args: &[String],
        ) -> Result<ScriptResult, SkillError> {
            Err(SkillError::Unsupported("not used".to_string()))
        }
    }

    #[derive(Default)]
    struct MutableSkillRegistry {
        skills: RwLock<HashMap<String, Arc<dyn Skill>>>,
    }

    impl MutableSkillRegistry {
        fn replace_ids(&self, ids: &[&str]) {
            let mut map: HashMap<String, Arc<dyn Skill>> = HashMap::new();
            for id in ids {
                map.insert(
                    (*id).to_string(),
                    Arc::new(MockSkill::new(id)) as Arc<dyn Skill>,
                );
            }
            match self.skills.write() {
                Ok(mut guard) => *guard = map,
                Err(poisoned) => *poisoned.into_inner() = map,
            }
        }
    }

    impl SkillRegistry for MutableSkillRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
            match self.skills.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[test]
    fn composite_skill_registry_reads_live_updates_from_source_registries() {
        let dynamic = Arc::new(MutableSkillRegistry::default());
        dynamic.replace_ids(&["s1"]);

        let mut static_registry = InMemorySkillRegistry::new();
        static_registry
            .register(Arc::new(MockSkill::new("s_static")) as Arc<dyn Skill>)
            .expect("register static skill");

        let composite = CompositeSkillRegistry::try_new(vec![
            dynamic.clone() as Arc<dyn SkillRegistry>,
            Arc::new(static_registry) as Arc<dyn SkillRegistry>,
        ])
        .expect("compose registries");

        assert!(composite.ids().contains(&"s1".to_string()));
        assert!(composite.ids().contains(&"s_static".to_string()));

        dynamic.replace_ids(&["s1", "s2"]);
        let ids = composite.ids();
        assert!(ids.contains(&"s1".to_string()));
        assert!(ids.contains(&"s2".to_string()));
        assert!(ids.contains(&"s_static".to_string()));
    }

    #[test]
    fn composite_skill_registry_keeps_last_good_snapshot_on_runtime_conflict() {
        let reg_a = Arc::new(MutableSkillRegistry::default());
        reg_a.replace_ids(&["s1"]);
        let reg_b = Arc::new(MutableSkillRegistry::default());
        reg_b.replace_ids(&["s2"]);

        let composite = CompositeSkillRegistry::try_new(vec![
            reg_a.clone() as Arc<dyn SkillRegistry>,
            reg_b.clone() as Arc<dyn SkillRegistry>,
        ])
        .expect("compose registries");

        let initial_ids = composite.ids();
        assert_eq!(initial_ids, vec!["s1".to_string(), "s2".to_string()]);

        reg_b.replace_ids(&["s1"]);
        assert_eq!(composite.ids(), initial_ids);
        assert!(composite.get("s2").is_some());
    }
}
