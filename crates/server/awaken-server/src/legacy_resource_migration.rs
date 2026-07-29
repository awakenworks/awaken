//! Startup-only data migrations from the retired `resource-api.db` projections.
//!
//! These migrations run before routers are exposed. Target writes are restart-safe,
//! and a durable receipt is committed only after every row reaches the canonical
//! repository.

use std::path::{Path, PathBuf};

use awaken_skill_store::{SkillBundleFile, SkillDefinition, SkillStore, SkillVersion};

const SKILL_RECEIPT: &str = "awaken.skill_store.legacy-resource-api.v1.receipt";

fn load_legacy_rows(database: PathBuf) -> Result<Vec<(String, String, String)>, String> {
    if !database.exists() {
        return Ok(Vec::new());
    }
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| format!("open {}: {error}", database.display()))?;
    let has_table = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'skill_records')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| format!("inspect {}: {error}", database.display()))?;
    if !has_table {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT workspace_id, skill_id, data FROM skill_records ORDER BY workspace_id, skill_id",
        )
        .map_err(|error| format!("read {}: {error}", database.display()))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| format!("query {}: {error}", database.display()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("decode {} row: {error}", database.display()))
}

fn decode_versions(
    workspace: &str,
    id: &str,
    data: &str,
) -> Result<(SkillDefinition, Vec<SkillVersion>), String> {
    let record: serde_json::Value = serde_json::from_str(data)
        .map_err(|error| format!("legacy Skill `{workspace}/{id}` is malformed: {error}"))?;
    let legacy_versions = record
        .get("versions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("legacy Skill `{workspace}/{id}` has no version list"))?;
    let display_title = match record.get("display_title") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| {
                    format!("legacy Skill `{workspace}/{id}` has an invalid display title")
                })?
                .to_string(),
        ),
    };
    let required = |legacy: &serde_json::Value, field: &str| {
        legacy
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                format!("legacy Skill `{workspace}/{id}` version field `{field}` is invalid")
            })
    };
    let mut versions = Vec::with_capacity(legacy_versions.len());
    for (index, legacy) in legacy_versions.iter().enumerate() {
        let version = required(legacy, "version")?;
        let ordinal = version.parse::<u64>().map_err(|error| {
            format!("legacy Skill `{workspace}/{id}` has an invalid version: {error}")
        })?;
        let expected = (index + 1) as u64;
        if ordinal != expected {
            return Err(format!(
                "legacy Skill `{workspace}/{id}` versions are not contiguous at {ordinal}; expected {expected}"
            ));
        }
        let fallback_content = required(legacy, "content")?;
        let mut files = Vec::new();
        if let Some(entries) = legacy.get("files") {
            for (path, content) in entries.as_object().ok_or_else(|| {
                format!("legacy Skill `{workspace}/{id}` version files are invalid")
            })? {
                let content = content.as_str().ok_or_else(|| {
                    format!("legacy Skill `{workspace}/{id}` file `{path}` content is invalid")
                })?;
                files.push(SkillBundleFile {
                    path: path.clone(),
                    content: content.as_bytes().to_vec(),
                    executable: false,
                });
            }
        }
        if !files
            .iter()
            .any(|file| file.path == "SKILL.md" || file.path.ends_with("/SKILL.md"))
        {
            files.push(SkillBundleFile {
                path: "SKILL.md".into(),
                content: fallback_content.into_bytes(),
                executable: false,
            });
        }
        versions.push(SkillVersion {
            id: required(legacy, "id")?.into(),
            skill_id: id.to_string().into(),
            version: ordinal,
            name: required(legacy, "name")?,
            description: required(legacy, "description")?,
            directory: required(legacy, "directory")?,
            bundle_sha256: awaken_skill_store::bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        });
    }
    if versions.is_empty() {
        return Err(format!(
            "legacy Skill `{workspace}/{id}` has no version to import"
        ));
    }
    Ok((
        SkillDefinition {
            id: id.to_string().into(),
            workspace_id: workspace.to_string(),
            display_title,
            latest_version: 1,
            last_version: 1,
            timestamps: Default::default(),
        },
        versions,
    ))
}

async fn apply_skill(
    store: &dyn SkillStore,
    definition: SkillDefinition,
    versions: Vec<SkillVersion>,
) -> Result<bool, String> {
    let workspace = definition.workspace_id.clone();
    let id = definition.id.clone();
    let existing = store
        .definition(workspace.as_str(), id.as_str())
        .await
        .map_err(|error| error.to_string())?;
    let next;
    let mut changed = false;
    if let Some(existing) = existing {
        for legacy in versions
            .iter()
            .take(existing.last_version.min(versions.len() as u64) as usize)
        {
            let stored = store
                .version(workspace.as_str(), id.as_str(), legacy.version)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "canonical Skill `{workspace}/{id}` is missing version {}",
                        legacy.version
                    )
                })?;
            if stored.bundle_sha256 != legacy.bundle_sha256 {
                return Err(format!(
                    "canonical Skill `{workspace}/{id}` version {} conflicts with legacy input",
                    legacy.version
                ));
            }
        }
        next = existing.last_version.saturating_add(1);
    } else {
        store
            .create(definition, versions[0].clone())
            .await
            .map_err(|error| error.to_string())?;
        next = 2;
        changed = true;
    }
    for version in versions.into_iter().skip((next - 1) as usize) {
        store
            .append_version(workspace.as_str(), id.as_str(), version)
            .await
            .map_err(|error| error.to_string())?;
        changed = true;
    }
    Ok(changed)
}

/// Import every Workspace-scoped Skill row before the HTTP/runtime adapters are
/// assembled. Re-running after a crash verifies already-written versions and
/// continues from the first missing version.
pub async fn migrate_legacy_skill_registry(
    storage_root: &Path,
    store: &dyn SkillStore,
) -> Result<usize, String> {
    let ledger = storage_root.join("migration-ledger");
    let receipt = ledger.join(SKILL_RECEIPT);
    if receipt.exists() {
        return Ok(0);
    }
    let database = storage_root.join("resource-api.db");
    let rows = tokio::task::spawn_blocking(move || load_legacy_rows(database))
        .await
        .map_err(|error| format!("legacy Skill migration task failed: {error}"))??;
    let mut changed = 0;
    for (workspace, id, data) in rows {
        let (definition, versions) = decode_versions(&workspace, &id, &data)?;
        changed += usize::from(apply_skill(store, definition, versions).await?);
    }
    std::fs::create_dir_all(&ledger)
        .map_err(|error| format!("create migration ledger: {error}"))?;
    let temporary = receipt.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, format!("imported={changed}\n"))
        .map_err(|error| format!("write migration receipt: {error}"))?;
    std::fs::rename(&temporary, &receipt)
        .map_err(|error| format!("commit migration receipt: {error}"))?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn imports_all_workspaces_once_and_commits_a_receipt() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("resource-api.db");
        let connection = rusqlite::Connection::open(database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE skill_records(
                    workspace_id TEXT NOT NULL,
                    skill_id TEXT NOT NULL,
                    data TEXT NOT NULL
                 );",
            )
            .unwrap();
        let data = serde_json::json!({
            "display_title": "Greeter",
            "versions": [{
                "id": "skver_greet_1",
                "version": "1",
                "name": "greet",
                "description": "greets",
                "directory": "/skills/greet",
                "content": "---\nname: greet\n---\nhello",
                "files": {}
            }]
        });
        for workspace in ["ws-a", "ws-b"] {
            connection
                .execute(
                    "INSERT INTO skill_records(workspace_id, skill_id, data) VALUES (?1, 'greet', ?2)",
                    rusqlite::params![workspace, data.to_string()],
                )
                .unwrap();
        }
        drop(connection);

        let store = awaken_skill_store::InMemorySkillStore::new();
        assert_eq!(
            migrate_legacy_skill_registry(root.path(), &store)
                .await
                .unwrap(),
            2
        );
        assert!(store.definition("ws-a", "greet").await.unwrap().is_some());
        assert!(store.definition("ws-b", "greet").await.unwrap().is_some());
        assert_eq!(
            migrate_legacy_skill_registry(root.path(), &store)
                .await
                .unwrap(),
            0
        );
        assert!(
            root.path()
                .join("migration-ledger")
                .join(SKILL_RECEIPT)
                .exists()
        );
    }
}
