use crate::error::SkillMaterializeError;
use crate::error::{SkillError, SkillRegistryError, SkillRegistryManagerError, SkillWarning};
use crate::materialize::{load_asset_material, load_reference_material, run_script_material};
use crate::skill::{
    ScriptResult, Skill, SkillContext, SkillMeta, SkillResource, SkillResourceKind,
};
use crate::skill_md::{SkillFrontmatter, parse_allowed_tools, parse_skill_md};
use async_trait::async_trait;
use awaken_contract::PeriodicRefresher;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

/// A filesystem-backed skill.
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

// ── FsSkillRegistryManager ──────────────────────────────────────────

#[derive(Clone, Default)]
struct SkillRegistrySnapshot {
    version: u64,
    skills: HashMap<String, Arc<dyn Skill>>,
    warnings: Vec<SkillWarning>,
}

struct SkillRegistryState {
    roots: Vec<PathBuf>,
    snapshot: RwLock<SkillRegistrySnapshot>,
    periodic_refresh: PeriodicRefresher,
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

#[derive(Clone)]
pub struct FsSkillRegistryManager {
    state: Arc<SkillRegistryState>,
}

impl std::fmt::Debug for FsSkillRegistryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.state.snapshot);
        f.debug_struct("FsSkillRegistryManager")
            .field("roots", &self.state.roots.len())
            .field("skills", &snapshot.skills.len())
            .field("warnings", &snapshot.warnings.len())
            .field("version", &snapshot.version)
            .field(
                "periodic_refresh_running",
                &self.state.periodic_refresh.is_running(),
            )
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
                periodic_refresh: PeriodicRefresher::new(),
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
        let weak_state = Arc::downgrade(&self.state);
        self.state
            .periodic_refresh
            .start(interval, move || {
                let weak = weak_state.clone();
                async move {
                    let Some(state) = weak.upgrade() else {
                        return;
                    };
                    if let Err(err) = refresh_state(state.as_ref()).await {
                        tracing::warn!(error = %err, "FS skill periodic refresh failed");
                    }
                }
            })
            .map_err(|msg| match msg.as_str() {
                m if m.contains("non-zero") => SkillRegistryManagerError::InvalidRefreshInterval,
                m if m.contains("already running") => {
                    SkillRegistryManagerError::PeriodicRefreshAlreadyRunning
                }
                _ => SkillRegistryManagerError::RuntimeUnavailable,
            })
    }

    pub fn stop_periodic_refresh(&self) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.state.periodic_refresh.stop())
        })
    }

    pub fn periodic_refresh_running(&self) -> bool {
        self.state.periodic_refresh.is_running()
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

// ── FsSkill discovery & Skill impl ─────────────────────────────────

impl FsSkill {
    /// Discover all valid skills under a single root directory.
    pub fn discover(root: impl Into<PathBuf>) -> Result<DiscoveryResult, SkillError> {
        Self::discover_roots(vec![root.into()])
    }

    /// Discover skills under multiple root directories.
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

    let fm = read_frontmatter_from_skill_md_path(skill_md)?;

    if fm.name != dir_name {
        return Err(format!(
            "frontmatter name '{}' does not match directory '{dir_name}'",
            fm.name
        ));
    }

    let allowed_tools = fm
        .allowed_tools
        .as_deref()
        .map(parse_allowed_tools)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.raw)
        .collect::<Vec<_>>();

    // Parse paths: comma or newline separated, trimmed, deduplicated.
    let paths: Vec<String> = fm
        .paths
        .as_deref()
        .map(|s| {
            s.split([',', '\n'])
                .map(str::trim)
                .filter(|p| !p.is_empty() && *p != "**")
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    let context = match fm.context.as_deref() {
        Some("fork") => SkillContext::Fork,
        _ => SkillContext::Inline,
    };

    let mut meta = SkillMeta::new(fm.name.clone(), fm.name, fm.description, allowed_tools);
    meta.when_to_use = fm.when_to_use;
    meta.arguments = fm.arguments.unwrap_or_default();
    meta.argument_hint = fm.argument_hint;
    meta.user_invocable = fm.user_invocable.unwrap_or(true);
    meta.model_invocable = !fm.disable_model_invocation.unwrap_or(false);
    meta.model_override = fm.model;
    meta.context = context;
    meta.paths = paths;

    Ok(FsSkill {
        meta,
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
            "---\nname: docx-processing\ndescription: Docs\nallowed-tools: read_file\n---\nBody\n",
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
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.reason.contains("directory name"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.reason.contains("missing SKILL.md"))
        );
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
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.reason.contains("does not match directory"))
        );
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
        fs::create_dir_all(skills_root.join("\u{6280}\u{80fd}")).unwrap();
        fs::write(
            skills_root.join("\u{6280}\u{80fd}").join("SKILL.md"),
            "---\nname: \u{6280}\u{80fd}\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(&skills_root).unwrap();
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].meta.id, "\u{6280}\u{80fd}");
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
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]);
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
                meta: SkillMeta::new(id, id, format!("{id} desc"), Vec::new()),
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
            .register(Arc::new(MockSkill::new("s-static")) as Arc<dyn Skill>)
            .expect("register static skill");

        let composite = CompositeSkillRegistry::try_new(vec![
            dynamic.clone() as Arc<dyn SkillRegistry>,
            Arc::new(static_registry) as Arc<dyn SkillRegistry>,
        ])
        .expect("compose registries");

        assert!(composite.ids().contains(&"s1".to_string()));
        assert!(composite.ids().contains(&"s-static".to_string()));

        dynamic.replace_ids(&["s1", "s2"]);
        let ids = composite.ids();
        assert!(ids.contains(&"s1".to_string()));
        assert!(ids.contains(&"s2".to_string()));
        assert!(ids.contains(&"s-static".to_string()));
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

    // ── InMemorySkillRegistry additional tests ──

    #[test]
    fn in_memory_registry_add_get_list() {
        let mut registry = InMemorySkillRegistry::new();
        assert!(registry.is_empty());

        registry
            .register(Arc::new(MockSkill::new("alpha")) as Arc<dyn Skill>)
            .unwrap();
        registry
            .register(Arc::new(MockSkill::new("beta")) as Arc<dyn Skill>)
            .unwrap();

        assert_eq!(registry.len(), 2);
        assert!(registry.get("alpha").is_some());
        assert!(registry.get("gamma").is_none());
        assert_eq!(
            registry.ids(),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn in_memory_registry_rejects_duplicate_id() {
        let mut registry = InMemorySkillRegistry::new();
        registry
            .register(Arc::new(MockSkill::new("s1")) as Arc<dyn Skill>)
            .unwrap();
        let err = registry
            .register(Arc::new(MockSkill::new("s1")) as Arc<dyn Skill>)
            .unwrap_err();
        assert!(matches!(err, SkillRegistryError::DuplicateSkillId(ref id) if id == "s1"));
    }

    #[test]
    fn in_memory_registry_rejects_empty_id() {
        let mut registry = InMemorySkillRegistry::new();
        let skill = Arc::new(MockSkill {
            meta: SkillMeta::new("  ", "empty", "empty", Vec::new()),
        });
        let err = registry.register(skill as Arc<dyn Skill>).unwrap_err();
        assert!(matches!(err, SkillRegistryError::EmptySkillId));
    }

    #[test]
    fn in_memory_registry_extend_upsert_overwrites() {
        let mut registry = InMemorySkillRegistry::new();
        registry
            .register(Arc::new(MockSkill::new("s1")) as Arc<dyn Skill>)
            .unwrap();
        registry.extend_upsert(vec![Arc::new(MockSkill::new("s1")) as Arc<dyn Skill>]);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn in_memory_registry_extend_registry_detects_duplicates() {
        let mut reg_a = InMemorySkillRegistry::new();
        reg_a
            .register(Arc::new(MockSkill::new("dup")) as Arc<dyn Skill>)
            .unwrap();

        let mut reg_b = InMemorySkillRegistry::new();
        reg_b
            .register(Arc::new(MockSkill::new("dup")) as Arc<dyn Skill>)
            .unwrap();

        let err = reg_a.extend_registry(&reg_b).unwrap_err();
        assert!(matches!(err, SkillRegistryError::DuplicateSkillId(_)));
    }

    #[test]
    fn in_memory_registry_snapshot_is_clone() {
        let mut registry = InMemorySkillRegistry::new();
        registry
            .register(Arc::new(MockSkill::new("a")) as Arc<dyn Skill>)
            .unwrap();

        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key("a"));
    }

    // ── CompositeSkillRegistry additional tests ──

    #[test]
    fn composite_registry_delegation_through_get() {
        let mut inner = InMemorySkillRegistry::new();
        inner
            .register(Arc::new(MockSkill::new("s1")) as Arc<dyn Skill>)
            .unwrap();

        let composite =
            CompositeSkillRegistry::try_new(vec![Arc::new(inner) as Arc<dyn SkillRegistry>])
                .unwrap();

        assert!(composite.get("s1").is_some());
        assert!(composite.get("missing").is_none());
        assert_eq!(composite.len(), 1);
        assert!(!composite.is_empty());
    }

    #[test]
    fn composite_registry_fallback_uses_cached_on_conflict() {
        let reg_a = Arc::new(MutableSkillRegistry::default());
        reg_a.replace_ids(&["x"]);

        let composite =
            CompositeSkillRegistry::try_new(vec![reg_a.clone() as Arc<dyn SkillRegistry>]).unwrap();

        assert_eq!(composite.ids(), vec!["x".to_string()]);
        // Cached snapshot is used when refresh succeeds
        assert!(composite.get("x").is_some());
    }

    #[test]
    fn composite_registry_priority_first_registry_wins() {
        let mut reg_a = InMemorySkillRegistry::new();
        reg_a
            .register(Arc::new(MockSkill::new("shared")) as Arc<dyn Skill>)
            .unwrap();
        let mut reg_b = InMemorySkillRegistry::new();
        reg_b
            .register(Arc::new(MockSkill::new("unique")) as Arc<dyn Skill>)
            .unwrap();

        let composite = CompositeSkillRegistry::try_new(vec![
            Arc::new(reg_a) as Arc<dyn SkillRegistry>,
            Arc::new(reg_b) as Arc<dyn SkillRegistry>,
        ])
        .unwrap();

        assert!(composite.get("shared").is_some());
        assert!(composite.get("unique").is_some());
        assert_eq!(composite.len(), 2);
    }

    #[test]
    fn composite_registry_rejects_duplicate_across_registries() {
        let mut reg_a = InMemorySkillRegistry::new();
        reg_a
            .register(Arc::new(MockSkill::new("dup")) as Arc<dyn Skill>)
            .unwrap();
        let mut reg_b = InMemorySkillRegistry::new();
        reg_b
            .register(Arc::new(MockSkill::new("dup")) as Arc<dyn Skill>)
            .unwrap();

        let err = CompositeSkillRegistry::try_new(vec![
            Arc::new(reg_a) as Arc<dyn SkillRegistry>,
            Arc::new(reg_b) as Arc<dyn SkillRegistry>,
        ])
        .unwrap_err();
        assert!(matches!(err, SkillRegistryError::DuplicateSkillId(_)));
    }

    // ── FsSkillRegistryManager additional tests ──

    #[test]
    fn fs_registry_manager_empty_roots_error() {
        let err = FsSkillRegistryManager::discover_roots(Vec::new()).unwrap_err();
        assert!(matches!(err, SkillRegistryManagerError::EmptyRoots));
    }

    #[test]
    fn fs_registry_manager_empty_directory() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        assert!(manager.is_empty());
        assert_eq!(manager.version(), 1);
        assert!(manager.warnings().is_empty());
    }

    #[tokio::test]
    async fn fs_registry_manager_refresh_picks_up_changes() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root.clone()]).unwrap();
        assert!(manager.is_empty());

        fs::create_dir_all(root.join("new-skill")).unwrap();
        fs::write(
            root.join("new-skill").join("SKILL.md"),
            "---\nname: new-skill\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let version = manager.refresh().await.unwrap();
        assert_eq!(version, 2);
        assert_eq!(manager.len(), 1);
        assert!(manager.get("new-skill").is_some());
    }

    #[test]
    fn fs_registry_manager_periodic_refresh_zero_interval_error() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        let err = manager
            .start_periodic_refresh(Duration::from_secs(0))
            .unwrap_err();
        assert!(matches!(
            err,
            SkillRegistryManagerError::InvalidRefreshInterval
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_registry_manager_start_stop_lifecycle() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        assert!(!manager.periodic_refresh_running());

        manager
            .start_periodic_refresh(Duration::from_secs(60))
            .unwrap();
        assert!(manager.periodic_refresh_running());

        let stopped = manager.stop_periodic_refresh();
        assert!(stopped);
        assert!(!manager.periodic_refresh_running());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_registry_manager_double_start_rejected() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        manager
            .start_periodic_refresh(Duration::from_secs(60))
            .unwrap();

        let err = manager
            .start_periodic_refresh(Duration::from_secs(60))
            .unwrap_err();
        assert!(matches!(
            err,
            SkillRegistryManagerError::PeriodicRefreshAlreadyRunning
        ));

        manager.stop_periodic_refresh();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_registry_manager_stop_when_not_running() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        assert!(!manager.stop_periodic_refresh());
    }

    // ── validate_dir_name ──

    #[test]
    fn validate_dir_name_accepts_valid_names() {
        assert!(validate_dir_name("hello").is_ok());
        assert!(validate_dir_name("hello-world").is_ok());
        assert!(validate_dir_name("a1b2").is_ok());
    }

    #[test]
    fn validate_dir_name_rejects_invalid() {
        assert!(validate_dir_name("").is_err());
        assert!(validate_dir_name("-start").is_err());
        assert!(validate_dir_name("end-").is_err());
        assert!(validate_dir_name("a--b").is_err());
        assert!(validate_dir_name("Hello").is_err());
        assert!(validate_dir_name("a/b").is_err());
        assert!(validate_dir_name(&"a".repeat(65)).is_err());
    }

    // ── CompositeSkillRegistry periodic_refresh delegation ──

    #[test]
    fn composite_registry_periodic_refresh_running_returns_false_for_inmemory() {
        let reg = InMemorySkillRegistry::new();
        let composite =
            CompositeSkillRegistry::try_new(vec![Arc::new(reg) as Arc<dyn SkillRegistry>]).unwrap();
        assert!(!composite.periodic_refresh_running());
    }

    #[test]
    fn composite_registry_stop_periodic_refresh_returns_false_when_nothing_running() {
        let reg = InMemorySkillRegistry::new();
        let composite =
            CompositeSkillRegistry::try_new(vec![Arc::new(reg) as Arc<dyn SkillRegistry>]).unwrap();
        assert!(!composite.stop_periodic_refresh());
    }

    #[test]
    fn composite_registry_start_periodic_refresh_no_op_for_inmemory() {
        let reg = InMemorySkillRegistry::new();
        let composite =
            CompositeSkillRegistry::try_new(vec![Arc::new(reg) as Arc<dyn SkillRegistry>]).unwrap();
        // InMemorySkillRegistry start_periodic_refresh returns Ok(())
        composite
            .start_periodic_refresh(Duration::from_secs(10))
            .unwrap();
    }

    #[test]
    fn composite_registry_empty_registries() {
        let composite =
            CompositeSkillRegistry::try_new(Vec::<Arc<dyn SkillRegistry>>::new()).unwrap();
        assert!(composite.is_empty());
        assert_eq!(composite.len(), 0);
        assert!(composite.ids().is_empty());
    }

    #[test]
    fn composite_registry_debug_format() {
        let reg = InMemorySkillRegistry::new();
        let composite =
            CompositeSkillRegistry::try_new(vec![Arc::new(reg) as Arc<dyn SkillRegistry>]).unwrap();
        let debug = format!("{:?}", composite);
        assert!(debug.contains("CompositeSkillRegistry"));
        assert!(debug.contains("registries"));
    }

    // ── FsSkillRegistryManager debug ──

    #[test]
    fn fs_registry_manager_debug_format() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(&root).unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        let debug = format!("{:?}", manager);
        assert!(debug.contains("FsSkillRegistryManager"));
        assert!(debug.contains("version"));
    }

    // ── FsSkillRegistryManager SkillRegistry trait impl ──

    #[test]
    fn fs_registry_manager_get_returns_skill() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("s1")).unwrap();
        fs::write(
            root.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        assert!(manager.get("s1").is_some());
        assert!(manager.get("nonexistent").is_none());
    }

    #[test]
    fn fs_registry_manager_ids_sorted() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        for name in &["z-skill", "a-skill", "m-skill"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(
                root.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: ok\n---\nBody\n"),
            )
            .unwrap();
        }

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        let ids = manager.ids();
        assert_eq!(ids, vec!["a-skill", "m-skill", "z-skill"]);
    }

    #[test]
    fn fs_registry_manager_snapshot_returns_all_skills() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("s1")).unwrap();
        fs::create_dir_all(root.join("s2")).unwrap();
        fs::write(
            root.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            root.join("s2").join("SKILL.md"),
            "---\nname: s2\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        let snap = manager.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.contains_key("s1"));
        assert!(snap.contains_key("s2"));
    }

    #[test]
    fn fs_registry_manager_warnings_reported() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("BadName")).unwrap();
        fs::write(
            root.join("BadName").join("SKILL.md"),
            "---\nname: badname\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let manager = FsSkillRegistryManager::discover_roots(vec![root]).unwrap();
        assert!(!manager.warnings().is_empty());
        assert!(
            manager
                .warnings()
                .iter()
                .any(|w| w.reason.contains("directory name"))
        );
    }

    // ── InMemorySkillRegistry default trait ──

    #[test]
    fn in_memory_registry_default_is_empty() {
        let reg = InMemorySkillRegistry::default();
        assert!(reg.is_empty());
    }

    // ── FsSkill discover with multiple roots ──

    #[test]
    fn discover_roots_merges_multiple_roots() {
        let td = TempDir::new().unwrap();
        let root1 = td.path().join("skills1");
        let root2 = td.path().join("skills2");
        fs::create_dir_all(root1.join("s1")).unwrap();
        fs::create_dir_all(root2.join("s2")).unwrap();
        fs::write(
            root1.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            root2.join("s2").join("SKILL.md"),
            "---\nname: s2\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover_roots(vec![root1, root2]).unwrap();
        assert_eq!(result.skills.len(), 2);
        // Skills should be sorted by id
        assert_eq!(result.skills[0].meta.id, "s1");
        assert_eq!(result.skills[1].meta.id, "s2");
    }

    // ── FsSkill into_arc_skills ──

    #[test]
    fn into_arc_skills_empty() {
        let arcs = FsSkill::into_arc_skills(vec![]);
        assert!(arcs.is_empty());
    }

    // ── DiscoveryResult fields ──

    #[test]
    fn discovery_result_collects_warnings_sorted() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("ZBad")).unwrap();
        fs::create_dir_all(root.join("ABad")).unwrap();
        fs::write(
            root.join("ZBad").join("SKILL.md"),
            "---\nname: zbad\ndescription: ok\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            root.join("ABad").join("SKILL.md"),
            "---\nname: abad\ndescription: ok\n---\nBody\n",
        )
        .unwrap();

        let result = FsSkill::discover(root).unwrap();
        assert!(result.skills.is_empty());
        assert!(result.warnings.len() >= 2);
        // Warnings should be sorted by path
        for i in 1..result.warnings.len() {
            assert!(result.warnings[i - 1].path <= result.warnings[i].path);
        }
    }
}
