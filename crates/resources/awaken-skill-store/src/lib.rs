//! Durable Skill aggregate repository for the resources plane.
//!
//! One repository owns definitions, immutable versions, and binary-safe bundles.
//! Runtime and HTTP adapters consume the same truth; there is no process-local
//! version registry beside it. Authorization stays outside this crate: operations
//! receive an already-trusted Workspace id and enforce only intrinsic ownership.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::{PgSkillStore, PgStoreError};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use schema::{BUNDLE_ID, skill_store_bundle};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteSkillStore, StoreError};

/// Reduce `name` to a safe single file stem: keep alphanumerics, `-`, `_`; map every
/// other run to a single `-`; never empty; bounded length. This is an API naming
/// helper, not repository identity normalization: stable ids are stored verbatim.
pub fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// The stable, tagged catalog id for a skill named `name` (e.g. `skill_1a2b…`). The
/// official SDK requires `agent.skills[].skill_id` to be a tagged catalog id, not the
/// skill's name — so both the advertisement (derived from the durable catalog's stems)
/// and the `/v1/skills` registry compute this same id independently from the name, and
/// they line up without a shared map. Deterministic (FNV-1a over the safe stem) so it
/// survives a restart and matches across nodes; taking the stem makes it agree whether
/// fed the raw frontmatter name or the durable key.
#[must_use]
pub fn catalog_id(name: &str) -> String {
    let stem = sanitize_stem(name);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in stem.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("skill_{h:016x}")
}

// The aggregate port + values live in the port-only contract crate.
pub use awaken_resource_contract::{
    SkillBundleFile, SkillDefinition, SkillStore, SkillStoreError, SkillVersion,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SkillAggregate {
    pub definition: SkillDefinition,
    pub versions: BTreeMap<u64, SkillVersion>,
    #[serde(default)]
    pub retired_versions: std::collections::BTreeSet<u64>,
}

pub(crate) fn legacy_aggregate(workspace: &str, id: &str, content: &[u8]) -> SkillAggregate {
    let files = vec![SkillBundleFile {
        path: "SKILL.md".into(),
        content: content.to_vec(),
    }];
    SkillAggregate {
        definition: SkillDefinition {
            id: id.into(),
            workspace_id: workspace.into(),
            display_title: None,
            latest_version: 1,
            last_version: 1,
        },
        versions: BTreeMap::from([(
            1,
            SkillVersion {
                id: format!("skver_{}_1", sanitize_stem(id)),
                skill_id: id.into(),
                version: 1,
                name: id.into(),
                description: String::new(),
                directory: format!("/skills/{}", sanitize_stem(id)),
                bundle_sha256: bundle_sha256(&files),
                files,
            },
        )]),
        retired_versions: Default::default(),
    }
}

/// Canonical SHA-256 of the complete bundle. Paths are ordered and length-framed so
/// distinct path/content partitions cannot hash to the same byte stream.
#[must_use]
pub fn bundle_sha256(files: &[SkillBundleFile]) -> String {
    let mut ordered = files.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hash = Sha256::new();
    for file in ordered {
        hash.update((file.path.len() as u64).to_be_bytes());
        hash.update(file.path.as_bytes());
        hash.update((file.content.len() as u64).to_be_bytes());
        hash.update(&file.content);
    }
    format!("sha256:{:x}", hash.finalize())
}

pub(crate) fn validate_create(
    definition: &SkillDefinition,
    version: &SkillVersion,
) -> Result<(), SkillStoreError> {
    if definition.id.trim().is_empty() || definition.workspace_id.trim().is_empty() {
        return Err(SkillStoreError::Invalid(
            "skill and Workspace ids must be non-empty".into(),
        ));
    }
    if definition.latest_version != version.version
        || definition.last_version != version.version
        || version.skill_id != definition.id
        || version.version == 0
        || version.files.is_empty()
        || version.skill_md().is_none()
    {
        return Err(SkillStoreError::Invalid(
            "definition and initial Skill version are inconsistent".into(),
        ));
    }
    if version.bundle_sha256 != bundle_sha256(&version.files) {
        return Err(SkillStoreError::Invalid(
            "Skill bundle SHA-256 does not match its files".into(),
        ));
    }
    Ok(())
}

pub(crate) fn append_to(
    aggregate: &mut SkillAggregate,
    version: SkillVersion,
) -> Result<(), SkillStoreError> {
    if version.skill_id != aggregate.definition.id
        || version.version != aggregate.definition.last_version.saturating_add(1)
        || version.bundle_sha256 != bundle_sha256(&version.files)
        || version.skill_md().is_none()
    {
        return Err(SkillStoreError::Invalid(
            "Skill version must be the next valid immutable bundle".into(),
        ));
    }
    if aggregate.versions.contains_key(&version.version) {
        return Err(SkillStoreError::VersionConflict(
            version.version.to_string(),
        ));
    }
    aggregate.definition.latest_version = version.version;
    aggregate.definition.last_version = version.version;
    aggregate.versions.insert(version.version, version);
    Ok(())
}

pub(crate) fn remove_version_from(
    aggregate: &mut SkillAggregate,
    version: u64,
) -> Result<bool, SkillStoreError> {
    if !aggregate.versions.contains_key(&version) || aggregate.retired_versions.contains(&version) {
        return Ok(false);
    }
    let visible = aggregate
        .versions
        .keys()
        .filter(|candidate| !aggregate.retired_versions.contains(candidate))
        .count();
    if visible == 1 {
        return Err(SkillStoreError::Invalid(
            "the only Skill version cannot be deleted; delete the Skill instead".into(),
        ));
    }
    aggregate.retired_versions.insert(version);
    if aggregate.definition.latest_version == version {
        aggregate.definition.latest_version = *aggregate
            .versions
            .keys()
            .rev()
            .find(|candidate| !aggregate.retired_versions.contains(candidate))
            .expect("at least one visible version remains");
    }
    Ok(true)
}

/// In-memory [`SkillStore`] (tests / ephemeral single-process).
#[derive(Default)]
pub struct InMemorySkillStore {
    // workspace_id → (id → complete aggregate)
    inner: Mutex<BTreeMap<String, BTreeMap<String, SkillAggregate>>>,
}

impl InMemorySkillStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SkillStore for InMemorySkillStore {
    async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        validate_create(&definition, &initial_version)?;
        let mut inner = self.inner.lock().unwrap();
        let workspace = inner.entry(definition.workspace_id.clone()).or_default();
        if workspace.contains_key(&definition.id) {
            return Err(SkillStoreError::AlreadyExists(definition.id));
        }
        workspace.insert(
            definition.id.clone(),
            SkillAggregate {
                definition,
                versions: BTreeMap::from([(initial_version.version, initial_version)]),
                retired_versions: Default::default(),
            },
        );
        Ok(())
    }

    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let aggregate = inner
            .get_mut(workspace_id)
            .and_then(|workspace| workspace.get_mut(skill_id))
            .ok_or_else(|| SkillStoreError::NotFound(skill_id.into()))?;
        append_to(aggregate, version)
    }

    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .and_then(|ws| ws.get(skill_id))
            .map(|aggregate| aggregate.definition.clone()))
    }

    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .map(|ws| {
                ws.values()
                    .map(|aggregate| aggregate.definition.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<Option<SkillVersion>, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .and_then(|ws| ws.get(skill_id))
            .and_then(|aggregate| aggregate.versions.get(&version).cloned()))
    }

    async fn list_versions(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workspace_id)
            .and_then(|ws| ws.get(skill_id))
            .map(|aggregate| {
                aggregate
                    .versions
                    .iter()
                    .filter(|(version, _)| !aggregate.retired_versions.contains(version))
                    .map(|(_, version)| version.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn delete_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<bool, SkillStoreError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(aggregate) = inner
            .get_mut(workspace_id)
            .and_then(|workspace| workspace.get_mut(skill_id))
        else {
            return Ok(false);
        };
        remove_version_from(aggregate, version)
    }

    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get_mut(workspace_id)
            .is_some_and(|ws| ws.remove(skill_id).is_some()))
    }
}

/// Filesystem repository: one JSON aggregate per Skill. Identity components are
/// encoded byte-for-byte as hex, avoiding traversal *and* lossy sanitization
/// collisions between Workspaces or Skill ids.
pub struct FsSkillStore {
    root: PathBuf,
    gate: Mutex<()>,
}

impl FsSkillStore {
    /// Open (creating if absent) the catalog rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let store = Self {
            root,
            gate: Mutex::new(()),
        };
        store.import_legacy_files()?;
        Ok(store)
    }

    /// The catalog's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn encoded(value: &str) -> String {
        value
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn ws_dir(&self, workspace_id: &str) -> PathBuf {
        self.root.join(Self::encoded(workspace_id))
    }

    fn aggregate_path(&self, workspace_id: &str, skill_id: &str) -> PathBuf {
        self.ws_dir(workspace_id)
            .join(format!("{}.json", Self::encoded(skill_id)))
    }

    fn read_aggregate(
        &self,
        workspace: &str,
        id: &str,
    ) -> Result<Option<SkillAggregate>, SkillStoreError> {
        match std::fs::read(self.aggregate_path(workspace, id)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| SkillStoreError::Storage(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(SkillStoreError::Io(error.to_string())),
        }
    }

    fn write_aggregate(&self, aggregate: &SkillAggregate) -> Result<(), SkillStoreError> {
        let dir = self.ws_dir(&aggregate.definition.workspace_id);
        std::fs::create_dir_all(&dir).map_err(|error| SkillStoreError::Io(error.to_string()))?;
        let path =
            self.aggregate_path(&aggregate.definition.workspace_id, &aggregate.definition.id);
        let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let bytes = serde_json::to_vec(aggregate)
            .map_err(|error| SkillStoreError::Storage(error.to_string()))?;
        std::fs::write(&temp, bytes).map_err(|error| SkillStoreError::Io(error.to_string()))?;
        std::fs::rename(&temp, &path).map_err(|error| SkillStoreError::Io(error.to_string()))
    }

    fn import_legacy_files(&self) -> std::io::Result<()> {
        for workspace in std::fs::read_dir(&self.root)? {
            let workspace = workspace?;
            if !workspace.file_type()?.is_dir() {
                continue;
            }
            let Some(workspace_id) = workspace.file_name().to_str().map(str::to_string) else {
                continue;
            };
            for entry in std::fs::read_dir(workspace.path())? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "md") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let target = self.aggregate_path(&workspace_id, id);
                if target.exists() {
                    continue;
                }
                let aggregate = legacy_aggregate(&workspace_id, id, &std::fs::read(&path)?);
                self.write_aggregate(&aggregate)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SkillStore for FsSkillStore {
    async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        validate_create(&definition, &initial_version)?;
        let _guard = self.gate.lock().unwrap();
        if self
            .read_aggregate(&definition.workspace_id, &definition.id)?
            .is_some()
        {
            return Err(SkillStoreError::AlreadyExists(definition.id));
        }
        self.write_aggregate(&SkillAggregate {
            definition,
            versions: BTreeMap::from([(initial_version.version, initial_version)]),
            retired_versions: Default::default(),
        })
    }

    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        let _guard = self.gate.lock().unwrap();
        let mut aggregate = self
            .read_aggregate(workspace_id, skill_id)?
            .ok_or_else(|| SkillStoreError::NotFound(skill_id.into()))?;
        append_to(&mut aggregate, version)?;
        self.write_aggregate(&aggregate)
    }

    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError> {
        Ok(self
            .read_aggregate(workspace_id, skill_id)?
            .map(|aggregate| aggregate.definition))
    }

    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        let dir = self.ws_dir(workspace_id);
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SkillStoreError::Io(error.to_string())),
        };
        let mut out = Vec::new();
        for entry in read_dir {
            let path = entry
                .map_err(|error| SkillStoreError::Io(error.to_string()))?
                .path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let bytes =
                    std::fs::read(path).map_err(|error| SkillStoreError::Io(error.to_string()))?;
                let aggregate: SkillAggregate = serde_json::from_slice(&bytes)
                    .map_err(|error| SkillStoreError::Storage(error.to_string()))?;
                if aggregate.definition.workspace_id == workspace_id {
                    out.push(aggregate.definition);
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<Option<SkillVersion>, SkillStoreError> {
        Ok(self
            .read_aggregate(workspace_id, skill_id)?
            .and_then(|aggregate| aggregate.versions.get(&version).cloned()))
    }

    async fn list_versions(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        Ok(self
            .read_aggregate(workspace_id, skill_id)?
            .map(|aggregate| {
                aggregate
                    .versions
                    .into_iter()
                    .filter(|(version, _)| !aggregate.retired_versions.contains(version))
                    .map(|(_, version)| version)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn delete_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<bool, SkillStoreError> {
        let _guard = self.gate.lock().unwrap();
        let Some(mut aggregate) = self.read_aggregate(workspace_id, skill_id)? else {
            return Ok(false);
        };
        let removed = remove_version_from(&mut aggregate, version)?;
        if removed {
            self.write_aggregate(&aggregate)?;
        }
        Ok(removed)
    }

    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError> {
        let _guard = self.gate.lock().unwrap();
        match std::fs::remove_file(self.aggregate_path(workspace_id, skill_id)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(SkillStoreError::Io(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregate(workspace: &str, id: &str) -> (SkillDefinition, SkillVersion) {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\ndescription: test\n---\nbody".to_vec(),
        }];
        (
            SkillDefinition {
                id: id.into(),
                workspace_id: workspace.into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
            },
            SkillVersion {
                id: format!("skver-{id}-1"),
                skill_id: id.into(),
                version: 1,
                name: id.into(),
                description: "test".into(),
                directory: format!("/skills/{id}"),
                bundle_sha256: bundle_sha256(&files),
                files,
            },
        )
    }

    fn scratch(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("awaken-skillstore-{tag}-{stamp}"))
    }

    #[test]
    fn sanitize_stem_edge_cases() {
        // Safe characters survive verbatim, including case, `_`, `-`, and digits.
        assert_eq!(sanitize_stem("greet"), "greet");
        assert_eq!(sanitize_stem("a_b-C9"), "a_b-C9");
        // Every run of unsafe chars collapses to a single `-`.
        assert_eq!(sanitize_stem("a b"), "a-b");
        assert_eq!(sanitize_stem("a....b"), "a-b");
        // Leading/trailing separators are trimmed off the stem.
        assert_eq!(sanitize_stem("--foo_bar--"), "foo_bar");
        // Empty and all-unsafe ids both collapse to the shared "skill" fallback.
        assert_eq!(sanitize_stem(""), "skill");
        assert_eq!(sanitize_stem("***"), "skill");
        assert_eq!(sanitize_stem("/"), "skill");
        // Collision domain: distinct raw ids can sanitize onto the SAME stem.
        assert_eq!(sanitize_stem("a b"), sanitize_stem("a/b"));
        assert_ne!(sanitize_stem("a"), sanitize_stem("b"));
        // Length is bounded so a crafted long id cannot blow up a filename.
        assert!(sanitize_stem(&"x".repeat(300)).len() <= 120);
    }

    #[test]
    fn catalog_id_is_stable_and_tagged() {
        // Tagged catalog form, deterministic across calls (survives a restart).
        let a = catalog_id("Greeter");
        assert!(a.starts_with("skill_"), "tagged form: {a}");
        assert_eq!(a, catalog_id("Greeter"));
        // Agrees whether fed the raw frontmatter name or the durable stem it sanitizes
        // to — so advertisement (from stems) and the registry (from names) line up.
        assert_eq!(catalog_id("Greeter"), catalog_id(&sanitize_stem("Greeter")));
        assert_eq!(catalog_id("my skill!"), catalog_id("my-skill"));
        // Distinct skills get distinct ids.
        assert_ne!(catalog_id("greeter"), catalog_id("farewell"));
    }

    #[tokio::test]
    async fn crafted_ids_cannot_escape_root() {
        let root = scratch("escape");
        let store = FsSkillStore::open(&root).unwrap();
        let (definition, version) = aggregate("../workspace", "../../etc/passwd");
        store.create(definition, version).await.unwrap();
        assert!(
            store
                .definition("../workspace", "../../etc/passwd")
                .await
                .unwrap()
                .is_some()
        );
        assert!(!root.parent().unwrap().join("passwd.json").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Listing only projects valid aggregate documents and ignores unrelated files.
    #[tokio::test]
    async fn list_ignores_non_aggregate_files_in_the_workspace_dir() {
        let root = scratch("stray");
        let store = FsSkillStore::open(&root).unwrap();
        for id in ["greet", "review"] {
            let (definition, version) = aggregate("ws", id);
            store.create(definition, version).await.unwrap();
        }
        let ws_dir = store.ws_dir("ws");
        std::fs::write(ws_dir.join("notes.txt"), "not a skill").unwrap();
        std::fs::write(ws_dir.join("README"), "no extension at all").unwrap();

        let ids: Vec<String> = store
            .list_definitions("ws")
            .await
            .unwrap()
            .into_iter()
            .map(|definition| definition.id)
            .collect();
        assert_eq!(ids, vec!["greet", "review"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn filesystem_open_imports_legacy_skill_md_once() {
        let root = scratch("legacy");
        std::fs::create_dir_all(root.join("workspace-a")).unwrap();
        std::fs::write(
            root.join("workspace-a/greet.md"),
            "---\ndescription: old\n---\nlegacy",
        )
        .unwrap();
        let store = FsSkillStore::open(&root).unwrap();
        let version = store
            .version("workspace-a", "greet", 1)
            .await
            .unwrap()
            .unwrap();
        assert!(version.skill_md().unwrap().ends_with(b"legacy"));
        drop(store);
        let reopened = FsSkillStore::open(&root).unwrap();
        assert_eq!(
            reopened
                .list_versions("workspace-a", "greet")
                .await
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(root).ok();
    }
}
