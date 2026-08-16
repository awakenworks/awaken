use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::{CredentialKind, InMemorySecretStore, materialize};
use awaken_agent_contract::RedactedString;

#[derive(Default)]
struct RecordingRolloutTarget {
    fail: AtomicBool,
    events: std::sync::Mutex<Vec<ManagedCredentialRollout>>,
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for RecordingRolloutTarget {
    async fn rollout(&self, event: &ManagedCredentialRollout) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("service controller unavailable".into());
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

struct FaultyDeleteStore {
    inner: InMemorySecretStore,
    fail_before_delete: AtomicBool,
    lose_first_response: AtomicBool,
}

struct PoisonGetStore {
    inner: InMemorySecretStore,
    poison: std::sync::Mutex<Option<crate::SecretRef>>,
}

struct AmbiguousManagedPutStore {
    inner: InMemorySecretStore,
    put_landed: tokio::sync::Barrier,
    release_response: tokio::sync::Barrier,
}

struct GatedCredentialRepo {
    inner: InMemoryCredentialRepo,
    gate_reads: AtomicBool,
    gate: Arc<tokio::sync::Barrier>,
}

impl GatedCredentialRepo {
    fn new() -> Self {
        Self {
            inner: InMemoryCredentialRepo::new(),
            gate_reads: AtomicBool::new(false),
            gate: Arc::new(tokio::sync::Barrier::new(2)),
        }
    }
}

#[async_trait::async_trait]
impl CredentialRepo for GatedCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        self.inner.put(source).await
    }

    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        self.inner.put_if_absent(source).await
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        let result = self.inner.get(id).await;
        if self.gate_reads.load(Ordering::SeqCst) {
            self.gate.wait().await;
        }
        result
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        self.inner.list(workspace_id).await
    }

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        self.inner.begin_mutation(intent).await
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        self.inner.apply_mutation(intent).await
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        self.inner.pending_mutations().await
    }

    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        self.inner.complete_mutation(id).await
    }

    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        self.inner.material_refs().await
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        self.inner.put_pool(pool).await
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        self.inner.get_pool(id).await
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        self.inner.list_pools(workspace_id).await
    }
}

#[async_trait::async_trait]
impl SecretStore for FaultyDeleteStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(r, secret).await
    }

    async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        self.inner.get(r).await
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        if self.fail_before_delete.load(Ordering::SeqCst) {
            return Err(CredentialError::Storage("delete timeout".into()));
        }
        self.inner.delete(r).await?;
        if self.lose_first_response.swap(false, Ordering::SeqCst) {
            return Err(CredentialError::Storage("delete response lost".into()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SecretStore for PoisonGetStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(r, secret).await
    }

    async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        if self.poison.lock().unwrap().as_ref() == Some(r) {
            return Err(CredentialError::Storage("poison material read".into()));
        }
        self.inner.get(r).await
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(r).await
    }
}

#[async_trait::async_trait]
impl SecretStore for AmbiguousManagedPutStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(r, secret).await?;
        self.put_landed.wait().await;
        self.release_response.wait().await;
        Err(CredentialError::Storage(
            "injected lost Managed put response".into(),
        ))
    }

    async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        self.inner.get(r).await
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(r).await
    }
}

#[derive(Default)]
struct RejectingRepo {
    inner: InMemoryCredentialRepo,
}

#[async_trait::async_trait]
impl CredentialRepo for RejectingRepo {
    async fn put(&self, _source: CredentialSource) -> Result<(), CredentialError> {
        Err(CredentialError::Storage("injected row failure".into()))
    }

    async fn put_if_absent(
        &self,
        _source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        Err(CredentialError::Storage("injected row failure".into()))
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        Err(CredentialError::SourceNotFound(id.0.clone()))
    }

    async fn list(&self, _workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        Ok(Vec::new())
    }

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        self.inner.begin_mutation(intent).await
    }

    async fn apply_mutation(
        &self,
        _intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Storage("injected row failure".into()))
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        self.inner.pending_mutations().await
    }

    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        self.inner.complete_mutation(id).await
    }

    async fn put_pool(&self, _pool: CredentialPool) -> Result<(), CredentialError> {
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        Err(CredentialError::PoolNotFound(id.0.clone()))
    }

    async fn list_pools(
        &self,
        _workspace_id: &str,
    ) -> Result<Vec<CredentialPool>, CredentialError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn enter_stores_row_and_secret_separately() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new("sk-xyz")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();

    // The row is retrievable and secret-free; the secret materializes from the store.
    let got = repo.get(&source.id).await.unwrap();
    assert!(!serde_json::to_string(&got).unwrap().contains("sk-xyz"));
    assert_eq!(
        materialize(&got, &store).await.unwrap().expose_secret(),
        "sk-xyz"
    );
    assert_eq!(repo.list("ws").await.unwrap().len(), 1);
    assert_eq!(repo.list("other").await.unwrap().len(), 0);
}

#[tokio::test]
async fn ambiguous_row_commit_is_reconciled_from_the_durable_intent() {
    let store = InMemorySecretStore::new();
    let repo = RejectingRepo::default();
    let result = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: None,
            secret: Some(RedactedString::new("must-not-be-orphaned")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await;

    assert!(
        matches!(result, Err(CredentialError::Storage(message)) if message == "injected row failure")
    );
    // An ambiguous commit error is not compensated inline: deleting here could
    // break a row whose commit succeeded but whose response was lost.
    assert_eq!(store.map.lock().expect("secret store mutex").len(), 1);
    assert_eq!(
        recover_credential_mutations(&store, &repo).await.unwrap(),
        1
    );
    assert!(store.map.lock().expect("secret store mutex").is_empty());
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

async fn interrupted_creation(store: &dyn SecretStore, repo: &dyn CredentialRepo) {
    let source = CredentialSource {
        id: CredentialSourceId("cred:ws:interrupted".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: Some("anthropic".into()),
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(crate::SecretRef("sec:cred:ws:interrupted".into())),
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: crate::CredentialStatus::Active,
        version: 1,
    };
    repo.begin_mutation(CredentialMutationIntent {
        before: None,
        after: source.clone(),
    })
    .await
    .unwrap();
    store
        .put(
            source.material_ref.as_ref().unwrap(),
            RedactedString::new("orphan candidate"),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_timeout_keeps_the_intent_for_a_later_reconciliation() {
    let store = FaultyDeleteStore {
        inner: InMemorySecretStore::new(),
        fail_before_delete: AtomicBool::new(true),
        lose_first_response: AtomicBool::new(false),
    };
    let repo = InMemoryCredentialRepo::new();
    interrupted_creation(&store, &repo).await;
    assert!(recover_credential_mutations(&store, &repo).await.is_err());
    assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);
    store.fail_before_delete.store(false, Ordering::SeqCst);
    assert_eq!(
        recover_credential_mutations(&store, &repo).await.unwrap(),
        1
    );
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn lost_delete_response_is_retried_idempotently() {
    let store = FaultyDeleteStore {
        inner: InMemorySecretStore::new(),
        fail_before_delete: AtomicBool::new(false),
        lose_first_response: AtomicBool::new(true),
    };
    let repo = InMemoryCredentialRepo::new();
    interrupted_creation(&store, &repo).await;
    assert!(recover_credential_mutations(&store, &repo).await.is_err());
    assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);
    assert_eq!(
        recover_credential_mutations(&store, &repo).await.unwrap(),
        1
    );
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn rotation_publishes_a_new_revision_and_reclaims_the_old_material() {
    // Causes: L2 active Vault source + replacement material. Effects: source
    // revision/ref advance together, new material resolves, old ref is erased,
    // and no WAL remains. This is decision-table rule rotate/success.
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let before = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("old")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    let old_ref = before.material_ref.clone().unwrap();

    let after = rotate_credential(&before.id, RedactedString::new("new"), &store, &repo)
        .await
        .unwrap();

    assert_eq!(after.version, before.version + 1);
    assert_ne!(after.material_ref, before.material_ref);
    assert_eq!(
        materialize(&after, &store).await.unwrap().expose_secret(),
        "new"
    );
    assert!(store.get(&old_ref).await.is_err());
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn retirement_fails_closed_and_reclaims_material() {
    // Causes: L3 active source + Archive retirement. Effects: a higher
    // archived revision with no material ref is durable, the secret is erased,
    // and both direct and pinned materialization fail closed.
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let before = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("retire-me")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    let old_ref = before.material_ref.clone().unwrap();

    let after = revoke_credential(&before.id, CredentialRetirement::Archive, &store, &repo)
        .await
        .unwrap();

    assert_eq!(after.status, CredentialStatus::Archived);
    assert_eq!(after.version, before.version + 1);
    assert!(after.material_ref.is_none());
    assert!(store.get(&old_ref).await.is_err());
    assert!(matches!(
        materialize(&after, &store).await,
        Err(CredentialError::NotActive(_))
    ));
}

#[tokio::test]
async fn reversible_status_transition_retains_material_and_advances_revision() {
    // Decision rule L4: active source + reversible Disabled transition =>
    // higher fail-closed revision, unchanged material ref/bytes, no pending
    // WAL. A repeated identical transition is an idempotent no-op.
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let before = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("retained")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    let disabled = transition_credential_status(&before.id, CredentialStatus::Disabled, &repo)
        .await
        .unwrap();
    assert_eq!(disabled.version, before.version + 1);
    assert_eq!(disabled.material_ref, before.material_ref);
    assert_eq!(
        store
            .get(disabled.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "retained"
    );
    assert_eq!(
        transition_credential_status(&disabled.id, CredentialStatus::Disabled, &repo,)
            .await
            .unwrap(),
        disabled
    );
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn provider_scope_widening_is_exact_and_retains_material_authority() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let before = enter_credential_idempotent(
        CredentialSourceId("cred:ws:provider-scope".into()),
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("provider".into()),
            env_key: None,
            secret: Some(RedactedString::new("retained")),
            oauth_command: None,
        },
        Some("provider.chat".into()),
        &store,
        &repo,
    )
    .await
    .unwrap()
    .source;

    assert!(
        widen_credential_to_provider_scope_exact(
            &before.id,
            before.version,
            "other-provider",
            "provider.chat",
            &repo,
        )
        .await
        .is_err(),
        "a different Provider cannot consume the source"
    );
    let widened = widen_credential_to_provider_scope_exact(
        &before.id,
        before.version,
        "provider",
        "provider.chat",
        &repo,
    )
    .await
    .unwrap();
    assert_eq!(widened.protocol_endpoint_id, None);
    assert_eq!(widened.version, before.version + 1);
    assert_eq!(widened.material_ref, before.material_ref);
    assert_eq!(
        materialize(&widened, &store).await.unwrap().expose_secret(),
        "retained"
    );
    assert!(
        widen_credential_to_provider_scope_exact(
            &widened.id,
            before.version,
            "provider",
            "provider.chat",
            &repo,
        )
        .await
        .is_err(),
        "a stale command cannot widen again"
    );
}

#[tokio::test]
async fn recovery_uses_the_durable_side_of_a_rotation_as_cleanup_authority() {
    // Mutation recovery decision table:
    // R6 current==before => delete unpublished after-ref and keep old-ref.
    // R7 current==after  => delete retired old-ref and keep new-ref.
    for publish_after in [false, true] {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let before = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("old")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let mut after = before.clone();
        after.version += 1;
        after.material_ref = Some(crate::SecretRef(format!(
            "sec:{}:r{}",
            after.id.0, after.version
        )));
        let intent = CredentialMutationIntent {
            before: Some(before.clone()),
            after: after.clone(),
        };
        repo.begin_mutation(intent.clone()).await.unwrap();
        store
            .put(
                after.material_ref.as_ref().unwrap(),
                RedactedString::new("new"),
            )
            .await
            .unwrap();
        if publish_after {
            repo.apply_mutation(&intent).await.unwrap();
        }

        assert_eq!(
            recover_credential_mutations(&store, &repo).await.unwrap(),
            1
        );
        let durable = repo.get(&before.id).await.unwrap();
        if publish_after {
            assert_eq!(durable, after);
            assert!(
                store
                    .get(before.material_ref.as_ref().unwrap())
                    .await
                    .is_err()
            );
            assert!(
                store
                    .get(after.material_ref.as_ref().unwrap())
                    .await
                    .is_ok()
            );
        } else {
            assert_eq!(durable, before);
            assert!(
                store
                    .get(before.material_ref.as_ref().unwrap())
                    .await
                    .is_ok()
            );
            assert!(
                store
                    .get(after.material_ref.as_ref().unwrap())
                    .await
                    .is_err()
            );
        }
    }
}

#[tokio::test]
async fn mutation_rejects_a_competing_revision_and_retains_recovery_evidence() {
    // Causes: R8 matching WAL but current is neither exact before nor after.
    // Effects: publication fails with MutationConflict and the WAL remains so
    // no material can be silently reclaimed against ambiguous metadata.
    let repo = InMemoryCredentialRepo::new();
    let before = CredentialSource {
        id: CredentialSourceId("cred:ws:conflict".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: None,
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(crate::SecretRef("sec:cred:ws:conflict".into())),
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    };
    repo.put(before.clone()).await.unwrap();
    let mut after = before.clone();
    after.version = 2;
    let intent = CredentialMutationIntent {
        before: Some(before.clone()),
        after,
    };
    repo.begin_mutation(intent.clone()).await.unwrap();
    let mut competing = before;
    competing.version = 3;
    repo.put(competing).await.unwrap();

    assert!(matches!(
        repo.apply_mutation(&intent).await,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(repo.pending_mutations().await.unwrap(), vec![intent]);
}

#[tokio::test]
async fn inventory_reports_but_never_deletes_a_ref_a_new_pending_can_reuse() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let committed = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("kept")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    let orphan = crate::SecretRef("sec:cred:ws:legacy-orphan".into());
    let webhook = crate::SecretRef("whsec:shared-store".into());
    store
        .put(&orphan, RedactedString::new("delete"))
        .await
        .unwrap();
    store
        .put(&webhook, RedactedString::new("preserve"))
        .await
        .unwrap();

    let report = inspect_credential_inventory(&store, &repo).await.unwrap();
    assert_eq!(report.orphaned_detected, vec![orphan.clone()]);
    assert!(report.missing_material.is_empty());
    assert!(store.get(&orphan).await.is_ok());
    assert!(store.get(&webhook).await.is_ok());
    assert!(
        store
            .get(committed.material_ref.as_ref().unwrap())
            .await
            .is_ok()
    );
    // A retry may begin after the inventory protection snapshot and reuse a
    // deterministic reference. Since the generic pass is report-only, that
    // candidate remains available when the new source publishes.
    let source = CredentialSource {
        id: CredentialSourceId("cred:ws:legacy-retry".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: None,
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(orphan.clone()),
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    };
    let intent = CredentialMutationIntent {
        before: None,
        after: source.clone(),
    };
    repo.begin_mutation(intent.clone()).await.unwrap();
    repo.apply_mutation(&intent).await.unwrap();
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert!(store.get(&orphan).await.is_ok());
    assert!(
        inspect_credential_inventory(&store, &repo)
            .await
            .unwrap()
            .orphaned_detected
            .is_empty()
    );
}

#[tokio::test]
async fn inventory_reports_metadata_whose_secret_is_missing() {
    let repo = InMemoryCredentialRepo::new();
    let store = InMemorySecretStore::new();
    let reference = crate::SecretRef("sec:cred:ws:missing".into());
    let source = CredentialSource {
        id: CredentialSourceId("cred:ws:missing".into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: None,
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(reference.clone()),
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: crate::CredentialStatus::Active,
        version: 1,
    };
    repo.put(source.clone()).await.unwrap();

    let report = inspect_credential_inventory(&store, &repo).await.unwrap();
    assert_eq!(report.missing_material, vec![reference]);
    assert!(materialize(&source, &store).await.is_err());
}

fn managed_vault() -> ManagedVault {
    ManagedVault {
        id: "vault-1".into(),
        workspace_id: "ws".into(),
        display_name: "Vault".into(),
        metadata: BTreeMap::new(),
        archived_at: None,
        deletion: None,
        revision: 1,
    }
}

#[tokio::test]
async fn inventory_protects_material_referenced_by_a_managed_pending_fact() {
    let repo = InMemoryCredentialRepo::new();
    let store = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (source, secret) = prepare_source_with_id(
        CredentialSourceId("cred:ws:managed-pending".into()),
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("pending")),
            oauth_command: None,
        },
    );
    let child = ManagedVaultCredential {
        id: "credential-managed-pending".into(),
        vault_id: "vault-1".into(),
        workspace_id: "ws".into(),
        source_id: source.id.clone(),
        auth: crate::catalog::ManagedCredentialAuth::StaticBearer {
            mcp_server_url: "https://mcp.example.com".into(),
        },
        metadata: BTreeMap::new(),
        display_name: None,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();
    repo.begin_managed_mutation(pending).await.unwrap();
    store.put(&reference, secret.unwrap()).await.unwrap();

    let report = inspect_credential_inventory(&store, &repo).await.unwrap();
    assert!(report.orphaned_detected.is_empty());
    assert!(store.get(&reference).await.is_ok());
}

#[tokio::test]
async fn pools_round_trip_and_list_scopes_by_workspace() {
    let repo = InMemoryCredentialRepo::new();
    let pool = |id: &str, ws: &str| CredentialPool {
        id: CredentialPoolId(id.into()),
        workspace_id: ws.into(),
        members: Vec::new(),
        policy: crate::SelectionPolicy::FirstHealthy,
    };
    repo.put_pool(pool("pool:a", "ws")).await.unwrap();
    repo.put_pool(pool("pool:b", "ws")).await.unwrap();
    repo.put_pool(pool("pool:c", "other")).await.unwrap();

    let got = repo
        .get_pool(&CredentialPoolId("pool:a".into()))
        .await
        .unwrap();
    assert_eq!(got.workspace_id, "ws");
    assert_eq!(repo.list_pools("ws").await.unwrap().len(), 2);
    assert_eq!(repo.list_pools("other").await.unwrap().len(), 1);
    assert_eq!(repo.list_pools("empty").await.unwrap().len(), 0);
}

/// F11(d): `get` is a deliberate **unscoped by-id primitive** — keyed by source
/// id only, never by workspace (only `list`/`list_pools` filter by workspace).
/// Tenancy is NOT enforced on this low-level read; it is enforced one layer up,
/// at credential *resolution* (`awaken_config_resolver::resolve_credential`),
/// which is where both the binding's and the source's workspace are known: a
/// pool member whose source belongs to another workspace is skipped, and an
/// `Exact` binding on a cross-workspace source fails closed (`SourceMissing`).
///
/// Audit of every `CredentialRepo::get` caller confirms none performs an
/// unfenced cross-workspace *secret materialization*:
/// - `awaken-runtime-host::PrefetchedSourceLookup::for_defs` (managed MCP)
///   prefetches by id, then materializes only through the fenced resolver;
/// - the admin-config-api routes (`get_credential`, `archive_credential`,
///   `put_mcp_server` existence-check) are IAM-gated by-id management ops that
///   read a **secret-free** row / mutate status — they never materialize a
///   secret; the sealed material stays behind `SecretStore`.
///
/// So keeping `get` unscoped is correct: it is the shared read primitive, and
/// the tenant fence lives at the resolution seam. This test pins that primitive.
#[tokio::test]
async fn get_is_an_unscoped_by_id_primitive_fenced_at_resolution() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let owned = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws-owner".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: Some("ANTHROPIC_API_KEY".into()),
            secret: Some(RedactedString::new("secret")),
            oauth_command: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();

    // `list` for an unrelated workspace correctly hides the row...
    assert_eq!(repo.list("ws-other").await.unwrap().len(), 0);
    // ...but a direct `get` with the id returns it regardless of workspace.
    let cross_read = repo.get(&owned.id).await.unwrap();
    assert_eq!(cross_read.workspace_id, "ws-owner");
}

// Idempotent-entry decision table:
// R1 missing identity + valid command -> create one row and one material ref.
// R2 same identity + same non-secret facts -> return the existing row without
//    creating another material ref.
// R3 same identity + different provider facts -> fail closed as a conflict.
// R4 same identity/provider + different endpoint scope -> fail closed.
#[tokio::test]
async fn idempotent_entry_has_one_identity_and_rejects_conflicting_facts() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let id = CredentialSourceId("cred:ws:provider-command".into());
    let params = |provider: &str, secret: &str| CredentialCreateParams {
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: Some(provider.into()),
        env_key: None,
        secret: Some(RedactedString::new(secret)),
        oauth_command: None,
    };

    let first = enter_credential_idempotent(
        id.clone(),
        params("anthropic", "first"),
        Some("anthropic.anthropic_messages".into()),
        &store,
        &repo,
    )
    .await
    .unwrap();
    let replay = enter_credential_idempotent(
        id.clone(),
        params("anthropic", "ignored-on-replay"),
        Some("anthropic.anthropic_messages".into()),
        &store,
        &repo,
    )
    .await
    .unwrap();
    let conflict = enter_credential_idempotent(
        id,
        params("openai", "different-command"),
        Some("openai.open_ai_chat".into()),
        &store,
        &repo,
    )
    .await;
    let endpoint_conflict = enter_credential_idempotent(
        CredentialSourceId("cred:ws:provider-command".into()),
        params("anthropic", "different-endpoint"),
        Some("anthropic.anthropic_messages.backup".into()),
        &store,
        &repo,
    )
    .await;

    assert!(first.created);
    assert!(!replay.created);
    assert_eq!(first.source.id, replay.source.id);
    assert_eq!(repo.list("ws").await.unwrap().len(), 1);
    assert_eq!(store.inventory().await.unwrap().len(), 1);
    assert!(matches!(conflict, Err(CredentialError::InvalidSource(_))));
    assert!(matches!(
        endpoint_conflict,
        Err(CredentialError::InvalidSource(_))
    ));
}

/// Verified-idempotency cause/effect graph: C1 stable source identity and
/// metadata match; C2 submitted material matches the sealed first write.
/// C1+C2 is an exact no-write replay; C1+!C2 fails with a mutation conflict.
/// This stricter command is opt-in so ordinary non-secret idempotent callers
/// retain their established replay semantics.
#[tokio::test]
async fn verified_idempotent_entry_rejects_different_material() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let id = CredentialSourceId("cred:ws:verified-operation".into());
    let params = |secret: &str| CredentialCreateParams {
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: Some("domain-pack/provider".into()),
        env_key: None,
        secret: Some(RedactedString::new(secret)),
        oauth_command: None,
    };

    let first =
        enter_credential_idempotent_verified(id.clone(), params("first"), None, &store, &repo)
            .await
            .unwrap();
    let replay =
        enter_credential_idempotent_verified(id.clone(), params("first"), None, &store, &repo)
            .await
            .unwrap();
    let conflict =
        enter_credential_idempotent_verified(id, params("second"), None, &store, &repo).await;

    assert!(first.created);
    assert!(!replay.created);
    assert_eq!(first.source, replay.source);
    assert!(matches!(
        conflict,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(store.inventory().await.unwrap().len(), 1);
}

/// Cause/effect graph for the hosted application bearer aggregate:
/// C1 source absent; C2 tuple matches the durable source; C3 command key
/// matches; C4 payload/material matches; C5 a new command key is supplied.
/// Effects are E1 create one source/revision, E2 exact replay/no write, E3
/// reject an idempotency or tuple conflict, and E4 rotate only material while
/// retaining source identity. Constraints: C3 excludes C5; C4 is relevant
/// only with C3.
///
/// | rule | C1 | C2 | C3 | C4 | C5 | effect |
/// |---|---|---|---|---|---|---|
/// | A1 | yes | - | - | - | - | E1/revision 1 |
/// | A2 | no | yes | yes | yes | no | E2/same ids and revision |
/// | A3 | no | yes | yes | no | no | E3/conflict |
/// | A4 | no | yes | no | - | yes | E4/same id, revision + 1 |
/// | A5 | no | no | - | - | - | E3/conflict |
#[tokio::test]
async fn application_mcp_bearer_create_replay_rotate_and_conflict_are_one_aggregate() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let source_id = CredentialSourceId("cred:app-mcp:test".into());
    macro_rules! command {
        ($workspace:expr, $target:expr, $key:expr, $token:expr) => {
            enter_or_rotate_application_mcp_bearer(
                ApplicationMcpBearerCommand {
                    source_id: source_id.clone(),
                    workspace_id: $workspace.into(),
                    target_fingerprint: $target.into(),
                    command_key_fingerprint: $key.into(),
                    bearer: RedactedString::new($token),
                },
                &store,
                &repo,
            )
        };
    }

    let first = command!("ws", "target-a", "key-1", "token-1")
        .await
        .unwrap();
    let replay = command!("ws", "target-a", "key-1", "token-1")
        .await
        .unwrap();
    let mismatched_replay = command!("ws", "target-a", "key-1", "token-other").await;
    let rotated = command!("ws", "target-a", "key-2", "token-2")
        .await
        .unwrap();
    let workspace_conflict = command!("other", "target-a", "key-3", "token-3").await;
    let target_conflict = command!("ws", "target-b", "key-3", "token-3").await;

    assert_eq!(first.version, 1);
    assert_eq!(first.id, replay.id);
    assert_eq!(first.version, replay.version);
    assert!(matches!(
        mismatched_replay,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(rotated.id, first.id);
    assert_eq!(rotated.version, first.version + 1);
    assert_eq!(
        store
            .get(rotated.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "token-2"
    );
    assert!(matches!(
        workspace_conflict,
        Err(CredentialError::MutationConflict(_))
    ));
    assert!(matches!(
        target_conflict,
        Err(CredentialError::MutationConflict(_))
    ));
}

/// Concurrent-rotation decision rule: C1 two valid commands read the same
/// revision and C2 their command identities differ. The one source-keyed WAL
/// accepts exactly one intent (E1), the other returns MutationConflict (E2),
/// and the durable source advances exactly once (E3). Equal command+payload
/// is constrained to the replay rule above and may safely share one intent.
#[tokio::test]
async fn application_mcp_bearer_concurrent_rotation_has_one_winner() {
    let store = InMemorySecretStore::new();
    let repo = GatedCredentialRepo::new();
    let source_id = CredentialSourceId("cred:app-mcp:race".into());
    enter_or_rotate_application_mcp_bearer(
        ApplicationMcpBearerCommand {
            source_id: source_id.clone(),
            workspace_id: "ws".into(),
            target_fingerprint: "target".into(),
            command_key_fingerprint: "initial-key".into(),
            bearer: RedactedString::new("initial-token"),
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    repo.gate_reads.store(true, Ordering::SeqCst);

    let left = enter_or_rotate_application_mcp_bearer(
        ApplicationMcpBearerCommand {
            source_id: source_id.clone(),
            workspace_id: "ws".into(),
            target_fingerprint: "target".into(),
            command_key_fingerprint: "left-key".into(),
            bearer: RedactedString::new("left-token"),
        },
        &store,
        &repo,
    );
    let right = enter_or_rotate_application_mcp_bearer(
        ApplicationMcpBearerCommand {
            source_id: source_id.clone(),
            workspace_id: "ws".into(),
            target_fingerprint: "target".into(),
            command_key_fingerprint: "right-key".into(),
            bearer: RedactedString::new("right-token"),
        },
        &store,
        &repo,
    );
    let (left, right) = tokio::join!(left, right);
    repo.gate_reads.store(false, Ordering::SeqCst);

    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert_eq!(
        usize::from(matches!(left, Err(CredentialError::MutationConflict(_))))
            + usize::from(matches!(right, Err(CredentialError::MutationConflict(_)))),
        1
    );
    assert_eq!(repo.get(&source_id).await.unwrap().version, 2);
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_source_or_pool_is_not_found() {
    let repo = InMemoryCredentialRepo::new();
    assert!(matches!(
        repo.get(&CredentialSourceId("cred:absent".into())).await,
        Err(CredentialError::SourceNotFound(id)) if id == "cred:absent"
    ));
    assert!(matches!(
        repo.get_pool(&CredentialPoolId("pool:absent".into())).await,
        Err(CredentialError::PoolNotFound(id)) if id == "pool:absent"
    ));
}
