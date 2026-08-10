//! Atomic logical-lifecycle projection for the Skill aggregate.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_resource_contract::{
    ResourceKind, ResourcePurgeError, ResourceReference, ResourceReferenceIndex,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget, SkillDefinition, SkillStore,
    SkillStoreError, SkillVersion,
};

pub(crate) struct ReferenceIndexedSkillStore {
    inner: Arc<dyn SkillStore>,
    references: Arc<dyn ResourceReferenceIndex>,
}

impl ReferenceIndexedSkillStore {
    pub(crate) fn new(
        inner: Arc<dyn SkillStore>,
        references: Arc<dyn ResourceReferenceIndex>,
    ) -> Self {
        Self { inner, references }
    }

    fn record(workspace_id: &str, skill_id: &str) -> ResourceReferenceRecord {
        ResourceReferenceRecord {
            target: ResourceTarget::new(workspace_id, ResourceKind::Skill, skill_id),
            reference: ResourceReference {
                kind: ResourceReferenceKind::LogicalLifecycle,
                reference_id: format!("skill:{workspace_id}:{skill_id}"),
            },
        }
    }

    async fn ensure_active(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<(), SkillStoreError> {
        self.references
            .add_reference(Self::record(workspace_id, skill_id))
            .await
            .map_err(skill_storage)?;
        Ok(())
    }

    pub(crate) async fn synchronize_all(&self) -> Result<(), SkillStoreError> {
        for workspace_id in self.inner.workspace_ids().await? {
            for definition in self.inner.list_definitions(&workspace_id).await? {
                self.ensure_active(&workspace_id, definition.id.as_str())
                    .await?;
            }
        }
        Ok(())
    }
}

fn skill_storage(error: ResourcePurgeError) -> SkillStoreError {
    SkillStoreError::Storage(error.to_string())
}

#[async_trait]
impl SkillStore for ReferenceIndexedSkillStore {
    async fn workspace_ids(&self) -> Result<Vec<String>, SkillStoreError> {
        self.inner.workspace_ids().await
    }

    async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        self.ensure_active(&definition.workspace_id, definition.id.as_str())
            .await?;
        self.inner.create(definition, initial_version).await
    }

    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        self.ensure_active(workspace_id, skill_id).await?;
        self.inner
            .append_version(workspace_id, skill_id, version)
            .await
    }

    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError> {
        self.inner.definition(workspace_id, skill_id).await
    }

    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        self.inner.list_definitions(workspace_id).await
    }

    async fn snapshot_latest_versions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        self.inner.snapshot_latest_versions(workspace_id).await
    }

    async fn version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<Option<SkillVersion>, SkillStoreError> {
        self.inner.version(workspace_id, skill_id, version).await
    }

    async fn list_versions(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        self.inner.list_versions(workspace_id, skill_id).await
    }

    async fn delete_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<bool, SkillStoreError> {
        self.inner
            .delete_version(workspace_id, skill_id, version)
            .await
    }

    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError> {
        let deleted = self.inner.delete_skill(workspace_id, skill_id).await?;
        self.references
            .remove_reference(&Self::record(workspace_id, skill_id))
            .await
            .map_err(skill_storage)?;
        Ok(deleted)
    }

    async fn purge_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<u64, SkillStoreError> {
        self.inner.purge_skill(workspace_id, skill_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{
        ResourceReclamationFence, ResourceReferenceIndex, ResourceTimestamps, SkillBundleFile,
        skill_bundle_sha256,
    };

    fn skill() -> (SkillDefinition, SkillVersion) {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: safe\n---\nbody".to_vec(),
            executable: false,
        }];
        (
            SkillDefinition {
                id: "skill-a".into(),
                workspace_id: "workspace-a".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: ResourceTimestamps::created(1),
            },
            SkillVersion {
                id: "skill-version-a".into(),
                skill_id: "skill-a".into(),
                version: 1,
                name: "safe".into(),
                description: String::new(),
                directory: "/skills/skill-a".into(),
                bundle_sha256: skill_bundle_sha256(&files),
                files,
                created_unix_nanos: 1,
            },
        )
    }

    #[tokio::test]
    async fn skill_lifecycle_reference_is_atomic_repairable_and_removed_after_tombstone() {
        // FMECA cause/effect graph: C1 active Skill is created; C2 it is
        // tombstoned; C3 reclamation fence already exists; C4 restart restores
        // pre-projection Skill rows. Effects: E1 lifecycle reference precedes
        // visibility; E2 reference removal follows tombstone visibility; E3 C3
        // rejects creation; E4 startup reconciliation rebuilds active rows.
        // Skill recreate/purge TOCTOU has S=9,O=3,D=9,RPN=243. Rules:
        // S1=C1->E1; S2=C1+C2->E2; S3=C3+C1->E3; S4=C4->E4.
        let inner = Arc::new(awaken_skill_store::InMemorySkillStore::new());
        let references = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
        let store = ReferenceIndexedSkillStore::new(inner.clone(), references.clone());
        let target = ResourceTarget::new("workspace-a", ResourceKind::Skill, "skill-a");
        let (definition, version) = skill();
        store.create(definition, version).await.unwrap();
        assert_eq!(references.references(&target).await.unwrap().len(), 1, "S1");
        assert!(store.delete_skill("workspace-a", "skill-a").await.unwrap());
        assert!(
            references.references(&target).await.unwrap().is_empty(),
            "S2"
        );

        let fenced = ResourceTarget::new("workspace-a", ResourceKind::Skill, "skill-fenced");
        assert!(matches!(
            references
                .acquire_reclamation("purge-fenced", &fenced)
                .await
                .unwrap(),
            awaken_resource_contract::AcquireResourceReclamationOutcome::Acquired
        ));
        let (mut blocked_definition, mut blocked_version) = skill();
        blocked_definition.id = "skill-fenced".into();
        blocked_version.id = "skill-version-fenced".into();
        blocked_version.skill_id = "skill-fenced".into();
        assert!(
            store
                .create(blocked_definition, blocked_version)
                .await
                .is_err(),
            "S3"
        );
        assert!(
            inner
                .definition("workspace-a", "skill-fenced")
                .await
                .unwrap()
                .is_none(),
            "S3"
        );

        let restored_inner = Arc::new(awaken_skill_store::InMemorySkillStore::new());
        let (definition, version) = skill();
        restored_inner.create(definition, version).await.unwrap();
        let restored_references =
            Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
        let restored = ReferenceIndexedSkillStore::new(restored_inner, restored_references.clone());
        restored.synchronize_all().await.unwrap();
        assert_eq!(
            restored_references.references(&target).await.unwrap().len(),
            1,
            "S4"
        );
    }
}
