// @formal evidence resource_reclamation.physical_purge_requires_zero_references_leases_and_fence
//! Logical File deletion and recovery share the original durable intent.

use super::{FileApplication, file_catalog_error, logical_file_reference};
use awaken_resource_contract::{
    FileRecord, ResourceKind, ResourcePurgeError, ResourcePurgeIntent, ResourcePurgePreparation,
    ResourceReferenceKind, ResourceTarget,
};

impl FileApplication {
    pub async fn delete(
        &self,
        workspace_id: &str,
        file_id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        // Command recovery must see the retained tombstone: the first attempt
        // may have committed logical deletion before reference removal failed.
        // Ordinary get/bytes keep their active-only visibility contract.
        let Some(record) = self
            .catalog
            .get_file(workspace_id, file_id, true)
            .await
            .map_err(file_catalog_error)?
        else {
            return Ok(None);
        };
        let intent = deletion_intent(&record, requested_at_unix_ms)?;
        self.reclamation.put(intent).await?;
        let deleted = self
            .catalog
            .mark_file_deleted(workspace_id, file_id)
            .await
            .map_err(file_catalog_error)?;
        if deleted.is_some() {
            self.reclamation
                .remove_reference(&logical_file_reference(&record))
                .await?;
        }
        // Replayed deletion still has the public missing-File response, after
        // completing the original durable cleanup prefix instead of skipping it.
        Ok(if record.deleted { None } else { deleted })
    }
}

fn deletion_intent(
    record: &FileRecord,
    requested_at_unix_ms: u64,
) -> Result<ResourcePurgeIntent, ResourcePurgeError> {
    ResourcePurgeIntent::new(
        format!("purge:File:{}:{}", record.workspace_id, record.id),
        format!("file-delete:{}:{}", record.workspace_id, record.id),
        ResourceTarget::new(&record.workspace_id, ResourceKind::File, &record.blob_id),
        None,
        requested_at_unix_ms,
        requested_at_unix_ms,
    )
}

#[async_trait::async_trait]
impl ResourcePurgePreparation for FileApplication {
    async fn prepare(&self, intent: &ResourcePurgeIntent) -> Result<(), ResourcePurgeError> {
        if intent.target.kind != ResourceKind::File {
            return Ok(());
        }
        // The existing reference index locates possible logical owners. Never
        // parse an opaque intent id, scan another Workspace, or infer authority
        // from a blob shared with another File. Re-derive the original command
        // from the retained record and require its complete immutable identity.
        for reference in self.reclamation.references(&intent.target).await? {
            if !matches!(
                reference.kind,
                ResourceReferenceKind::WorkspaceOwnership | ResourceReferenceKind::Artifact
            ) {
                continue;
            }
            let Some(record) = self
                .catalog
                .get_file(&intent.target.workspace_id, &reference.reference_id, true)
                .await
                .map_err(file_catalog_error)?
            else {
                continue;
            };
            if logical_file_reference(&record).reference == reference
                && deletion_intent(&record, intent.requested_at_unix_ms)?.same_request(intent)
            {
                self.delete(
                    &record.workspace_id,
                    &record.id,
                    intent.requested_at_unix_ms,
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{application, artifact_publication};
    use super::*;
    use awaken_resource_contract::FileStore;
    use std::sync::Arc;
    #[tokio::test]
    async fn delete_replay_resumes_durable_prefixes_without_losing_other_owners() {
        // Cause/effect table: D1 purge-intent insert fails; D2 tombstone update
        // fails after intent commit; D3 reference removal fails after tombstone;
        // D4 all effects commit but the caller retries without using the receipt.
        // D1/D2 remain readable; D3/D4 deny ordinary reads. Reopening the actual
        // SQLite adapters then replaying must finish this File's reference
        // removal and preserve the first durable intent, without another queue.
        // An already tombstoned File still returns None (HTTP 404 compatibility).
        // Every rule crosses upload/artifact ownership and C5 another Workspace
        // owns the same physical blob. Cross manual DELETE replay with the
        // production reclaimer: 16 cases, including Cowork outputs. Automatic
        // D1 has no committed intent and must do nothing until a fresh DELETE;
        // D2/D3 resume after reopen without another user command; D4 is idempotent.
        // its File, bytes and reference must survive; no inline blob deletion.
        // Missing/wrong-Workspace IDs produce no effects. SQLite triggers abort
        // the real statements, not a parallel fake deletion implementation.
        use awaken_resource_contract::{ResourcePurgeRepository, ResourceReferenceIndex};
        let cases = [
            (
                "D1",
                Some("BEFORE INSERT ON resource_lifecycle_purge_intents"),
                false,
            ),
            (
                "D2",
                Some("BEFORE UPDATE OF deleted ON file_store_file"),
                false,
            ),
            (
                "D3",
                Some("BEFORE DELETE ON resource_lifecycle_references"),
                true,
            ),
            ("D4", None, true),
        ];
        for (((rule, trigger, tombstoned), artifact), automatic) in cases
            .into_iter()
            .flat_map(|case| [false, true].map(|artifact| (case, artifact)))
            .flat_map(|case| [false, true].map(|automatic| (case, automatic)))
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("resources.sqlite");
            let open = || {
                let path = path.clone();
                async move {
                    let clock = awaken_time::system_clock();
                    let files = Arc::new(
                        awaken_file_store::sqlite::SqliteFileStore::open(path.to_str().unwrap())
                            .await
                            .unwrap(),
                    );
                    let lifecycle = Arc::new(
                        awaken_resource_store::SqliteResourceStore::open(&path)
                            .await
                            .unwrap(),
                    );
                    let resources =
                        crate::ResourcesApplication::new(crate::ResourceAuthorities::new(
                            Arc::new(crate::RegistryApplication::new(
                                lifecycle.clone(),
                                clock.clone(),
                            )),
                            files.clone(),
                            files.clone(),
                            Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                                clock.clone(),
                            )),
                            Arc::new(awaken_skill_store::InMemorySkillStore::new()),
                            lifecycle.clone(),
                            clock,
                        ));
                    let reclaimer = awaken_resource_reclaimer::ResourceReclaimer::new(
                        "file-recovery",
                        100,
                        lifecycle.clone(),
                        resources.physical_cleanup(),
                    )
                    .unwrap()
                    .with_preparation(resources.purge_preparation())
                    .with_guard(resources.lifecycle_guard());
                    (resources.files.clone(), files, lifecycle, reclaimer)
                }
            };
            let (app, files, lifecycle, reclaimer) = open().await;
            let file = if artifact {
                app.create_artifact(&artifact_publication("a.txt", b"shared", None))
                    .await
                    .unwrap()
            } else {
                app.create_uploaded_file(
                    "workspace-a",
                    "a.txt".into(),
                    "text/plain".into(),
                    b"shared",
                )
                .await
                .unwrap()
            };
            let other = app
                .create_uploaded_file(
                    "workspace-b",
                    "b.txt".into(),
                    "text/plain".into(),
                    b"shared",
                )
                .await
                .unwrap();
            assert_eq!(file.blob_id, other.blob_id);
            let intent_id = format!("purge:File:workspace-a:{}", file.id);
            let sql = rusqlite::Connection::open(&path).unwrap();
            if let Some(event) = trigger {
                sql.execute_batch(&format!("CREATE TRIGGER deletion_fault {event} BEGIN SELECT RAISE(ABORT, 'injected deletion failure'); END;")).unwrap();
            }
            let first = app.delete("workspace-a", &file.id, 7).await;
            if trigger.is_some() {
                assert!(
                    first
                        .unwrap_err()
                        .to_string()
                        .contains("injected deletion failure"),
                    "{rule}"
                );
                assert!(
                    app.delete("workspace-a", &file.id, 8)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("injected deletion failure"),
                    "{rule}: persistent failure is not masked by the tombstone"
                );
            } else {
                assert!(first.unwrap().is_some(), "{rule}");
            }
            assert_eq!(
                app.bytes("workspace-a", &file.id).await.unwrap().is_none(),
                tombstoned,
                "{rule}"
            );
            let original_intent = ResourcePurgeRepository::get(lifecycle.as_ref(), &intent_id)
                .await
                .unwrap();
            assert_eq!(original_intent.is_some(), rule != "D1", "{rule}");
            if automatic && trigger.is_some() {
                let result = reclaimer.reconcile(9, 10).await.unwrap();
                assert_eq!(
                    result.retryable_failures,
                    usize::from(rule != "D1"),
                    "{rule}: preparation failure"
                );
                assert_eq!(
                    result.completed, 0,
                    "{rule}: no purge on failed preparation"
                );
            }
            let references = lifecycle
                .references_for_resource(ResourceKind::File, &file.blob_id)
                .await
                .unwrap();
            assert_eq!(references.len(), if rule == "D4" { 1 } else { 2 }, "{rule}");
            if trigger.is_some() {
                sql.execute_batch("DROP TRIGGER deletion_fault").unwrap();
            }
            drop(sql);
            drop(app);
            drop(files);
            drop(lifecycle);
            drop(reclaimer);

            let (app, files, lifecycle, reclaimer) = open().await;
            assert!(
                app.delete("workspace-b", &file.id, 9)
                    .await
                    .unwrap()
                    .is_none(),
                "{rule}: scope"
            );
            assert!(
                app.delete("workspace-a", "missing-file", 9)
                    .await
                    .unwrap()
                    .is_none(),
                "{rule}: missing"
            );
            if automatic {
                let result = reclaimer.reconcile(11, 10).await.unwrap();
                assert_eq!(result.completed, 0, "{rule}: shared blob still owned");
                assert_eq!(
                    result.deferred,
                    usize::from(rule != "D1"),
                    "{rule}: only the other owner remains"
                );
            }
            if !automatic || rule == "D1" {
                let replay = app.delete("workspace-a", &file.id, 11).await.unwrap();
                assert_eq!(
                    replay.is_none(),
                    tombstoned,
                    "{rule}: original HTTP semantics"
                );
            }
            assert!(
                app.bytes("workspace-a", &file.id).await.unwrap().is_none(),
                "{rule}"
            );
            let references = lifecycle
                .references_for_resource(ResourceKind::File, &file.blob_id)
                .await
                .unwrap();
            assert_eq!(
                references,
                vec![logical_file_reference(&other)],
                "{rule}: exact remaining owner"
            );
            assert_eq!(
                app.bytes("workspace-b", &other.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .1,
                b"shared",
                "{rule}"
            );
            assert_eq!(
                files.get(&file.blob_id).await.unwrap().unwrap(),
                b"shared",
                "{rule}: no inline purge"
            );
            let after = ResourcePurgeRepository::get(lifecycle.as_ref(), &intent_id)
                .await
                .unwrap()
                .unwrap();
            if let Some(before) = original_intent {
                assert!(after.same_request(&before), "{rule}: same durable command");
                assert_eq!(after.requested_at_unix_ms, before.requested_at_unix_ms);
                if !automatic {
                    assert_eq!(after, before, "{rule}: no reset of durable intent");
                }
            } else {
                assert_eq!(after.requested_at_unix_ms, 11, "{rule}");
            }
            assert!(
                app.delete("workspace-a", &file.id, 13)
                    .await
                    .unwrap()
                    .is_none(),
                "{rule}: exact replay"
            );
            assert_eq!(
                ResourcePurgeRepository::get(lifecycle.as_ref(), &intent_id)
                    .await
                    .unwrap(),
                Some(after),
                "{rule}: repeated replay"
            );
            if automatic {
                // The final independent owner is explicitly deleted. The same
                // reclaimer now removes the actual SQLite blob and commits both
                // receipts; a later scan has no work. This exercises physical
                // cleanup only after all ownership edges are gone.
                app.delete("workspace-b", &other.id, 14).await.unwrap();
                assert_eq!(reclaimer.reconcile(15, 10).await.unwrap().completed, 2);
                assert!(files.get(&file.blob_id).await.unwrap().is_none());
                assert_eq!(reclaimer.reconcile(16, 10).await.unwrap().completed, 0);
            }
        }
    }

    #[tokio::test]
    async fn preparation_requires_the_exact_logical_file_command() {
        // P1 exact intent + active/tombstoned source -> resume only that File.
        // P2 change one immutable authority field (intent id, key, Workspace,
        // blob, kind, config) -> no logical or reference effects. A second File
        // in the SAME Workspace shares the blob and must survive every rule.
        // This checks why a generic physical purge is not logical-delete authority.
        for tombstoned in [false, true] {
            for drift in ["none", "id", "key", "workspace", "blob", "kind", "config"] {
                let app = application().await;
                let file = app
                    .create_artifact(&artifact_publication("a.txt", b"shared", None))
                    .await
                    .unwrap();
                let other = app
                    .create_artifact(&artifact_publication("b.txt", b"shared", None))
                    .await
                    .unwrap();
                if tombstoned {
                    app.catalog
                        .mark_file_deleted("workspace-a", &file.id)
                        .await
                        .unwrap();
                }
                let before = app
                    .reclamation
                    .references_for_resource(ResourceKind::File, &file.blob_id)
                    .await
                    .unwrap();
                let mut intent = deletion_intent(&file, 7).unwrap();
                match drift {
                    "id" => intent.intent_id.push_str("-changed"),
                    "key" => intent.idempotency_key.push_str("-changed"),
                    "workspace" => intent.target.workspace_id = "workspace-b".into(),
                    "blob" => intent.target.resource_id = "different-blob".into(),
                    "kind" => intent.target.kind = ResourceKind::Skill,
                    "config" => intent.config_version = Some(1),
                    _ => {}
                }
                app.prepare(&intent).await.unwrap();
                assert_eq!(
                    app.get("workspace-a", &file.id).await.unwrap().is_none(),
                    tombstoned || drift == "none",
                    "{drift}"
                );
                assert!(
                    app.get("workspace-a", &other.id).await.unwrap().is_some(),
                    "{drift}: shared owner"
                );
                let after = app
                    .reclamation
                    .references_for_resource(ResourceKind::File, &file.blob_id)
                    .await
                    .unwrap();
                if drift == "none" {
                    assert_eq!(after, vec![logical_file_reference(&other)]);
                } else {
                    assert_eq!(
                        after, before,
                        "{drift}: no compensation for another command"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn decision_table_logical_delete_denies_reads_without_deleting_bytes_inline() {
        // Cause/effect design:
        // C1 active logical File -> E1 metadata and bytes are readable;
        // C2 delete command -> E2 tombstone + purge intent are durable before the
        // ownership reference is removed;
        // C3 post-delete read -> E3 ordinary reads fail closed while physical blob
        // reclamation remains an independent, retryable reclamation operation.
        let app = application().await;
        let file = app
            .create_uploaded_file("workspace-a", "input.txt".into(), "text/plain".into(), b"x")
            .await
            .unwrap();
        assert!(
            app.bytes("workspace-a", &file.id).await.unwrap().is_some(),
            "E1"
        );
        assert!(
            app.delete("workspace-a", &file.id, 7)
                .await
                .unwrap()
                .is_some(),
            "E2"
        );
        assert!(
            app.bytes("workspace-a", &file.id).await.unwrap().is_none(),
            "E3"
        );
    }
}
