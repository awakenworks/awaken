//! One real SecretStore crash boundary fixture for both publication envelopes.
use awaken_agent_contract::RedactedString;
use awaken_credential_store::{
    SealedAeadSecretStore,
    sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore},
};
use awaken_credential_vault::{CredentialError, SecretRef, SecretStore};
use std::{path::Path, sync::Arc, time::Duration};

pub const PHASES: [&str; 5] = [
    "writing",
    "before-delete",
    "after-delete",
    "after-readback",
    "retired",
];

pub fn stores(path: &str, key: &[u8; 32]) -> (SqliteCredentialRepo, SealedAeadSecretStore) {
    let repo = SqliteCredentialRepo::open(path).unwrap();
    // Separate durability domains: metadata and encrypted bytes cannot share
    // an accidental SQLite transaction or connection lifecycle.
    let material = Path::new(path).with_extension("secrets.db");
    let blob = SqliteSealedBlobStore::open(material.to_str().unwrap()).unwrap();
    (repo, SealedAeadSecretStore::over(key, Arc::new(blob)))
}

pub async fn checkpoint(marker: &str) -> ! {
    std::fs::write(marker, b"durable-boundary").unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    panic!("parent failed to kill crash child");
}

pub struct CrashSecrets<'a> {
    pub inner: &'a dyn SecretStore,
    pub phase: &'a str,
    pub marker: &'a str,
}

#[async_trait::async_trait]
impl SecretStore for CrashSecrets<'_> {
    async fn put(
        &self,
        reference: &SecretRef,
        value: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(reference, value).await
    }
    async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
        let result = self.inner.get(reference).await;
        if self.phase == "after-readback" {
            assert!(
                matches!(&result, Err(CredentialError::SecretNotFound(id)) if id == &reference.0)
            );
            checkpoint(self.marker).await;
        }
        result
    }
    async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
        if self.phase == "before-delete" {
            checkpoint(self.marker).await;
        }
        self.inner.delete(reference).await?;
        if self.phase == "after-delete" {
            checkpoint(self.marker).await;
        }
        Ok(())
    }
}

pub async fn published_retirement(
    test_name: &str,
    mut source: awaken_credential_vault::CredentialSource,
    child: Option<awaken_credential_vault::catalog::ManagedVaultCredential>,
    key: &[u8; 32],
) {
    use awaken_credential_vault::catalog::{ManagedVault, ManagedVaultRepo};
    use awaken_credential_vault::repo::{
        CredentialMutationIntent, CredentialRepo, ManagedCredentialOperation,
        ManagedCredentialRepository, PendingManagedCredentialMutation,
        recover_credential_mutations, recover_managed_credential_mutations,
    };
    const ROOT: &str = "AWAKEN_RETIREMENT_CRASH_ROOT";
    const PHASE: &str = "AWAKEN_RETIREMENT_CRASH_PHASE";
    // C4 published retirement crosses before-delete/after-delete/after-absence/
    // after-WAL-retirement for BOTH envelopes. C5 one auxiliary ref is shared
    // by before/after. Outcomes: old primary is absent after recovery; shared
    // material and exact published source/child remain live; repeat is a no-op.
    source
        .auxiliary_material_refs
        .insert("retained".into(), SecretRef("shared-retirement-ref".into()));
    if let Ok(root) = std::env::var(ROOT) {
        let root = Path::new(&root);
        let phase = std::env::var(PHASE).unwrap();
        let marker = root.join("marker");
        let (repo, secrets) = stores(root.join("metadata.db").to_str().unwrap(), key);
        if let Some(child) = &child {
            repo.insert_vault(
                &source.workspace_id,
                ManagedVault {
                    id: child.vault_id.clone(),
                    workspace_id: source.workspace_id.clone(),
                    display_name: "Crash retirement".into(),
                    metadata: Default::default(),
                    created_at: "2026-09-17T00:00:00Z".into(),
                    updated_at: "2026-09-17T00:00:00Z".into(),
                    archived_at: None,
                    deletion: None,
                    revision: 1,
                },
            )
            .await
            .unwrap();
            let create = PendingManagedCredentialMutation::create(source, child.clone()).unwrap();
            repo.begin_managed_mutation(create.clone()).await.unwrap();
            for reference in create.after_source.material_refs() {
                secrets
                    .put(reference, RedactedString::new("retirement-fixture"))
                    .await
                    .unwrap();
            }
            let ready = repo.mark_managed_mutation_ready(&create).await.unwrap();
            repo.commit_managed_mutation(&ready).await.unwrap();
            assert_eq!(
                recover_managed_credential_mutations(&secrets, &repo)
                    .await
                    .unwrap(),
                1
            );
            source = create.after_source;
        } else {
            repo.put(source.clone()).await.unwrap();
            for reference in source.material_refs() {
                secrets
                    .put(reference, RedactedString::new("retirement-fixture"))
                    .await
                    .unwrap();
            }
        }
        std::fs::write(
            root.join("retired-ref"),
            &source.material_ref.as_ref().unwrap().0,
        )
        .unwrap();
        let mut after = source.clone();
        after.version += 1;
        after.material_ref = None;
        if let Some(before_child) = child {
            let mut after_child = before_child.clone();
            after_child.revision += 1;
            let update = PendingManagedCredentialMutation::change(
                "retirement".into(),
                ManagedCredentialOperation::Update,
                source,
                after,
                before_child,
                after_child,
                false,
            )
            .unwrap();
            repo.begin_managed_mutation(update.clone()).await.unwrap();
            repo.commit_managed_mutation(&update).await.unwrap();
        } else {
            let update = CredentialMutationIntent::prepare(Some(source), after).unwrap();
            repo.begin_mutation(update.clone()).await.unwrap();
            repo.apply_mutation(&update).await.unwrap();
        }
        let faults = CrashSecrets {
            inner: &secrets,
            phase: &phase,
            marker: marker.to_str().unwrap(),
        };
        let completed = recover_credential_mutations(&faults, &repo).await.unwrap()
            + recover_managed_credential_mutations(&faults, &repo)
                .await
                .unwrap();
        assert_eq!(completed, 1);
        assert_eq!(phase, "retired");
        checkpoint(marker.to_str().unwrap()).await;
    }
    for phase in PHASES.into_iter().filter(|phase| *phase != "writing") {
        let root = tempfile::tempdir().unwrap();
        let status =
            awaken_reliability_testkit::CrashProcess::new(test_name, root.path().join("marker"))
                .env(ROOT, root.path())
                .env(PHASE, phase)
                .run()
                .unwrap();
        assert!(!status.success());
        let (repo, secrets) = stores(root.path().join("metadata.db").to_str().unwrap(), key);
        let retired = SecretRef(std::fs::read_to_string(root.path().join("retired-ref")).unwrap());
        let published = repo.get(&source.id).await.unwrap();
        assert_eq!(published.version, 2);
        assert!(published.material_ref.is_none());
        let shared = published.auxiliary_material_refs.get("retained").unwrap();
        assert!(secrets.get(shared).await.is_ok(), "C5 before recovery");
        assert_eq!(
            secrets.get(&retired).await.is_ok(),
            phase == "before-delete",
            "C4 material prefix"
        );
        let pending = repo.pending_mutations().await.unwrap().len()
            + repo.pending_managed_mutations().await.unwrap().len();
        assert_eq!(pending, usize::from(phase != "retired"), "C4 WAL prefix");
        for repeat in 0..2 {
            let completed = recover_credential_mutations(&secrets, &repo).await.unwrap()
                + recover_managed_credential_mutations(&secrets, &repo)
                    .await
                    .unwrap();
            assert_eq!(completed, usize::from(repeat == 0 && phase != "retired"));
        }
        assert!(repo.pending_mutations().await.unwrap().is_empty());
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        assert_eq!(
            repo.get(&source.id).await.unwrap(),
            published,
            "C4 published source preserved"
        );
        if let Some(child) = &child {
            let mut expected = child.clone();
            expected.revision += 1;
            assert_eq!(
                repo.get_vault_credential(&source.workspace_id, &child.id)
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert!(
            matches!(secrets.get(&retired).await, Err(CredentialError::SecretNotFound(id)) if id == retired.0)
        );
        assert_eq!(
            secrets.inventory().await.unwrap(),
            vec![shared.clone()],
            "C5 only shared material remains"
        );
    }
}
