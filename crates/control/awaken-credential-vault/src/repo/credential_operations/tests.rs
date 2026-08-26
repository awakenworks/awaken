use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::{CredentialKind, InMemorySecretStore, encode_structured_material, materialize};
use awaken_agent_contract::RedactedString;

struct FaultyDeleteStore {
    inner: InMemorySecretStore,
    fail_before_delete: AtomicBool,
    lose_first_response: AtomicBool,
}

#[derive(Clone, Copy)]
enum RotationReadbackFault {
    Seal,
    Mismatch,
}

struct RejectedRotationReadbackStore {
    inner: InMemorySecretStore,
    fault: RotationReadbackFault,
}

#[async_trait::async_trait]
impl SecretStore for RejectedRotationReadbackStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(r, secret).await
    }

    async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        if r.0.ends_with(":r2:primary") {
            return match self.fault {
                RotationReadbackFault::Seal => Err(CredentialError::Seal),
                RotationReadbackFault::Mismatch => Ok(RedactedString::new("different-material")),
            };
        }
        self.inner.get(r).await
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(r).await
    }

    async fn inventory(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        self.inner.inventory().await
    }
}

struct GatedCredentialRepo {
    inner: InMemoryCredentialRepo,
    gated_reads_remaining: AtomicUsize,
    gate: Arc<tokio::sync::Barrier>,
}

impl GatedCredentialRepo {
    fn new() -> Self {
        Self {
            inner: InMemoryCredentialRepo::new(),
            gated_reads_remaining: AtomicUsize::new(0),
            gate: Arc::new(tokio::sync::Barrier::new(2)),
        }
    }

    fn gate_next_read_pair(&self) {
        self.gated_reads_remaining.store(2, Ordering::SeqCst);
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
        if self
            .gated_reads_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
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
    ) -> Result<bool, CredentialError> {
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

#[derive(Default)]
struct ObservedSecretStore {
    inner: InMemorySecretStore,
    puts: AtomicUsize,
    deletes: AtomicUsize,
}

#[async_trait::async_trait]
impl SecretStore for ObservedSecretStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(r, secret).await
    }

    async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        self.inner.get(r).await
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(r).await
    }

    async fn inventory(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        self.inner.inventory().await
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
    ) -> Result<bool, CredentialError> {
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
        descriptor: None,
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

    let after = rotate_credential_materials_exact(
        &before.id,
        before.version,
        CredentialMaterialPatch {
            primary: Some(RedactedString::new("new")),
            auxiliary: BTreeMap::new(),
            descriptor: None,
        },
        &store,
        &repo,
    )
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

/// Rotation material-publication cause/effect graph: C1 the exact active
/// revision is current; C2 the replacement write returns success; C3 the same
/// SecretStore can open the exact replacement before CAS. C1+C2+C3 publishes
/// revision 2 and retires revision 1. C1+C2+!C3 returns the typed seal error,
/// keeps revision 1 executable, and reclaims only the unpublished reference;
/// the same holds when C3 opens bytes different from the submitted material.
///
/// | Rule | C1 current | C2 put | C3 exact open | Effect |
/// |---|---|---|---|---|
/// | V1 | T | success | exact | publish r2; retire r1 |
/// | V2 | T | success | seal failure | keep r1; remove r2 material |
/// | V3 | T | success | different bytes | keep r1; remove r2 material |
#[tokio::test]
async fn rotation_does_not_publish_material_without_exact_readback() {
    for (rule, fault) in [
        ("V2", RotationReadbackFault::Seal),
        ("V3", RotationReadbackFault::Mismatch),
    ] {
        let store = RejectedRotationReadbackStore {
            inner: InMemorySecretStore::new(),
            fault,
        };
        let repo = InMemoryCredentialRepo::new();
        let before = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("github.com/api".into()),
                env_key: None,
                secret: Some(RedactedString::new("old-token")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let old_ref = before.material_ref.clone().unwrap();

        let result = rotate_credential_materials_exact(
            &before.id,
            before.version,
            CredentialMaterialPatch {
                primary: Some(RedactedString::new("new-token")),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            &store,
            &repo,
        )
        .await;

        assert!(matches!(result, Err(CredentialError::Seal)), "{rule}");
        assert_eq!(repo.get(&before.id).await.unwrap(), before, "{rule}");
        assert_eq!(
            store.get(&old_ref).await.unwrap().expose_secret(),
            "old-token",
            "{rule}"
        );
        assert_eq!(store.inventory().await.unwrap(), vec![old_ref], "{rule}");
        assert!(repo.pending_mutations().await.unwrap().is_empty(), "{rule}");
    }
}

#[tokio::test]
async fn retirement_fails_closed_and_reclaims_material() {
    // Retirement cause/effect decision table:
    // R1 active source + stale expected revision => MutationConflict before
    //    WAL publication or secret deletion; the current source stays active.
    // R2 active source + exact expected revision + Archive => one higher
    //    archived revision, no material refs, and material is reclaimed.
    // This same test observes both rules so R1 also proves a failed command
    // cannot consume the material needed by the following exact command.
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

    assert!(matches!(
        revoke_credential_exact(
            &before.id,
            before.version + 1,
            CredentialRetirement::Archive,
            &store,
            &repo,
        )
        .await,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(repo.get(&before.id).await.unwrap(), before);
    assert!(store.get(&old_ref).await.is_ok());
    assert!(repo.pending_mutations().await.unwrap().is_empty());

    let after = revoke_credential_exact(
        &before.id,
        before.version,
        CredentialRetirement::Archive,
        &store,
        &repo,
    )
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
        descriptor: None,
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
        descriptor: None,
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
        descriptor: None,
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

/// Concurrent verified-create cause/effect graph: C1 both callers observe the
/// stable identity as absent; C2 their public source facts match; C3 material
/// matches. The source-keyed WAL admits one owner (E1) and classifies the other
/// as an in-flight replay (E2): only E1 may put/apply/delete, exactly one source
/// is published, and an E2 retry after publication returns the existing source
/// without another material effect. Constraint: E2 may temporarily report a
/// mutation conflict while E1 is pending.
#[tokio::test]
async fn concurrent_verified_create_with_same_material_has_one_writer_and_replays() {
    let store = ObservedSecretStore::default();
    let repo = GatedCredentialRepo::new();
    let id = CredentialSourceId("cred:ws:verified-same-race".into());
    let params = || CredentialCreateParams {
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: Some("domain-pack/provider".into()),
        env_key: None,
        secret: Some(RedactedString::new("same-material")),
        oauth_command: None,
    };
    repo.gate_next_read_pair();

    let left = enter_credential_idempotent_verified(id.clone(), params(), None, &store, &repo);
    let right = enter_credential_idempotent_verified(id.clone(), params(), None, &store, &repo);
    let (left, right) = tokio::join!(left, right);

    assert_eq!(
        usize::from(left.as_ref().is_ok_and(|entry| entry.created))
            + usize::from(right.as_ref().is_ok_and(|entry| entry.created)),
        1,
        "E1"
    );
    assert!(
        matches!(&left, Ok(_) | Err(CredentialError::MutationConflict(_))),
        "E1/E2 left"
    );
    assert!(
        matches!(&right, Ok(_) | Err(CredentialError::MutationConflict(_))),
        "E1/E2 right"
    );
    let replay = enter_credential_idempotent_verified(id.clone(), params(), None, &store, &repo)
        .await
        .unwrap();
    assert!(!replay.created, "E2 retry");
    assert_eq!(repo.list("ws").await.unwrap().len(), 1, "E1");
    assert_eq!(store.puts.load(Ordering::SeqCst), 1, "E1 only");
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0, "E2 no delete");
    assert_eq!(
        store
            .get(replay.source.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "same-material",
        "E1 bytes"
    );
}

/// Concurrent verified-create decision rule for different material: C1 both
/// callers observe one absent stable identity; C2 public source facts match; C3
/// material differs. The WAL admits exactly one writer (E1); the in-flight
/// caller returns pending conflict without put/apply/delete (E2). Once E1 is
/// published, retrying C3 reaches the existing exact-byte verifier and remains
/// a conflict (E3), while E1's bytes and the single source stay unchanged.
#[tokio::test]
async fn concurrent_verified_create_with_different_material_retains_winner_bytes() {
    let store = ObservedSecretStore::default();
    let repo = GatedCredentialRepo::new();
    let id = CredentialSourceId("cred:ws:verified-different-race".into());
    let params = |material: &str| CredentialCreateParams {
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: Some("domain-pack/provider".into()),
        env_key: None,
        secret: Some(RedactedString::new(material)),
        oauth_command: None,
    };
    repo.gate_next_read_pair();

    let left = enter_credential_idempotent_verified(
        id.clone(),
        params("left-material"),
        None,
        &store,
        &repo,
    );
    let right = enter_credential_idempotent_verified(
        id.clone(),
        params("right-material"),
        None,
        &store,
        &repo,
    );
    let (left, right) = tokio::join!(left, right);

    assert_eq!(
        usize::from(left.is_ok()) + usize::from(right.is_ok()),
        1,
        "E1"
    );
    assert_eq!(
        usize::from(matches!(&left, Err(CredentialError::MutationConflict(_))))
            + usize::from(matches!(&right, Err(CredentialError::MutationConflict(_)))),
        1,
        "E2"
    );
    let (winner_material, loser_material) = if left.is_ok() {
        ("left-material", "right-material")
    } else {
        ("right-material", "left-material")
    };
    let retry = enter_credential_idempotent_verified(
        id.clone(),
        params(loser_material),
        None,
        &store,
        &repo,
    )
    .await;
    assert!(
        matches!(retry, Err(CredentialError::MutationConflict(_))),
        "E3"
    );
    let durable = repo.get(&id).await.unwrap();
    assert_eq!(repo.list("ws").await.unwrap().len(), 1, "E1");
    assert_eq!(store.puts.load(Ordering::SeqCst), 1, "E1 only");
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0, "E2 no delete");
    assert_eq!(
        store
            .get(durable.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        winner_material,
        "E1 bytes retained"
    );
}

/// Cause/effect graph for the hosted application bearer aggregate:
/// C1 source is absent/present; C2 tuple matches; C3 generation is
/// zero/equal/newer/older; C4 key and payload match the current material.
/// Effects are E0 reject without source/secret, E1 create one source/revision,
/// E2 exact replay/no write, E3 reject without mutation, and E4 rotate only
/// material while retaining source identity. Constraints: generation zero is
/// never durable; only equal generation plus exact key/payload replays; every
/// other equal/older command rejects before the WAL or SecretStore.
///
/// | rule | source | tuple | generation | key/payload | effect |
/// |---|---|---|---|---|---|
/// | A0 | absent | yes | zero | any | E0 |
/// | A1 | absent | yes | positive | any | E1/revision 1 |
/// | A2 | present | yes | equal | exact | E2/same source and revision |
/// | A3 | present | yes | equal | different | E3/conflict |
/// | A4 | present | yes | newer | any | E4/same id, revision + 1 |
/// | A5 | present | yes | older | prior exact | E3/conflict/no rollback |
/// | A6 | present | no | newer | any | E3/conflict |
#[tokio::test]
async fn application_mcp_bearer_create_replay_rotate_and_conflict_are_one_aggregate() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let source_id = CredentialSourceId("cred:app-mcp:test".into());
    macro_rules! command {
        ($workspace:expr, $target:expr, $generation:expr, $key:expr, $token:expr) => {
            enter_or_rotate_application_mcp_bearer(
                ApplicationMcpBearerCommand {
                    source_id: source_id.clone(),
                    workspace_id: $workspace.into(),
                    target_fingerprint: $target.into(),
                    command_key_fingerprint: $key.into(),
                    credential_generation: $generation,
                    bearer: RedactedString::new($token),
                },
                &store,
                &repo,
            )
        };
    }

    assert!(matches!(
        command!("ws", "target-a", 0, "key-zero", "token-zero").await,
        Err(CredentialError::InvalidSource(_))
    ));
    assert!(repo.list("ws").await.unwrap().is_empty(), "A0");
    assert!(store.inventory().await.unwrap().is_empty(), "A0");

    let first = command!("ws", "target-a", 1, "key-1", "token-1")
        .await
        .unwrap();
    let replay = command!("ws", "target-a", 1, "key-1", "token-1")
        .await
        .unwrap();
    let mismatched_replay = command!("ws", "target-a", 1, "key-1", "token-other").await;
    let same_generation_conflict = command!("ws", "target-a", 1, "key-other", "token-other").await;
    let rotated = command!("ws", "target-a", 2, "key-2", "token-2")
        .await
        .unwrap();
    let delayed_older_replay = command!("ws", "target-a", 1, "key-1", "token-1").await;
    let current_generation_conflict =
        command!("ws", "target-a", 2, "key-other", "token-other").await;
    let workspace_conflict = command!("other", "target-a", 3, "key-3", "token-3").await;
    let target_conflict = command!("ws", "target-b", 3, "key-3", "token-3").await;

    assert_eq!(first.version, 1);
    assert_eq!(first.id, replay.id);
    assert_eq!(first.version, replay.version);
    assert!(matches!(
        mismatched_replay,
        Err(CredentialError::MutationConflict(_))
    ));
    assert!(matches!(
        same_generation_conflict,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(rotated.id, first.id);
    assert_eq!(rotated.version, first.version + 1);
    assert!(
        rotated
            .material_ref
            .as_ref()
            .unwrap()
            .0
            .contains(":generation:2")
    );
    assert_eq!(
        store
            .get(rotated.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "token-2"
    );
    assert!(matches!(
        delayed_older_replay,
        Err(CredentialError::MutationConflict(_))
    ));
    assert!(matches!(
        current_generation_conflict,
        Err(CredentialError::MutationConflict(_))
    ));
    assert!(matches!(
        workspace_conflict,
        Err(CredentialError::MutationConflict(_))
    ));
    assert!(matches!(
        target_conflict,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(repo.get(&source_id).await.unwrap(), rotated, "A3/A5/A6");
    assert_eq!(store.inventory().await.unwrap().len(), 1, "A3/A5/A6");
}

/// Legacy-upgrade cause/effect graph: C1 an existing application-MCP material
/// ref has no generation but retains its valid Managed attempt fence -> E1
/// parse it as generation zero; C2 a positive generation arrives -> E2 rotate
/// through the existing WAL/CAS and reclaim the legacy material; C3 the exact
/// upgraded tuple replays -> E3 no additional source, secret, or mutation.
///
/// | rule | current encoding | incoming generation | effect |
/// |---|---|---|---|
/// | L1 | legacy/0 | 1 | one revision advance; one generation-1 material |
/// | L2 | generation 1 | exact 1 | replay; no extra state |
#[tokio::test]
async fn application_mcp_legacy_material_identity_upgrades_once() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let source_id = CredentialSourceId("cred:app-mcp:legacy".into());
    let command_key_fingerprint = "legacy-key";
    let legacy_ref = crate::SecretRef(format!(
        "sec:{}:application-mcp:{}:{command_key_fingerprint}:attempt:legacyattempt",
        source_id.0,
        command_key_fingerprint.len(),
    ));
    store
        .put(&legacy_ref, RedactedString::new("legacy-token"))
        .await
        .unwrap();
    repo.put(CredentialSource {
        id: source_id.clone(),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: Some(APPLICATION_MCP_PROVIDER_ID.into()),
        protocol_endpoint_id: Some("target".into()),
        env_key: None,
        material_ref: Some(legacy_ref.clone()),
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    })
    .await
    .unwrap();

    let command = || ApplicationMcpBearerCommand {
        source_id: source_id.clone(),
        workspace_id: "ws".into(),
        target_fingerprint: "target".into(),
        command_key_fingerprint: command_key_fingerprint.into(),
        credential_generation: 1,
        bearer: RedactedString::new("legacy-token"),
    };
    let upgraded = enter_or_rotate_application_mcp_bearer(command(), &store, &repo)
        .await
        .unwrap();
    assert_eq!(upgraded.version, 2, "L1");
    assert!(
        upgraded
            .material_ref
            .as_ref()
            .unwrap()
            .0
            .contains(":generation:1"),
        "L1"
    );
    assert!(
        matches!(
            store.get(&legacy_ref).await,
            Err(CredentialError::SecretNotFound(_))
        ),
        "L1 reclaims legacy material"
    );
    let replay = enter_or_rotate_application_mcp_bearer(command(), &store, &repo)
        .await
        .unwrap();
    assert_eq!(replay, upgraded, "L2");
    assert_eq!(store.inventory().await.unwrap().len(), 1, "L1/L2");
    assert!(repo.pending_mutations().await.unwrap().is_empty(), "L1/L2");
}

/// Concurrent-rotation decision rule: C1 two valid commands read the same
/// revision and C2 both carry the same next generation with different command
/// fingerprints. The one source-keyed WAL accepts exactly one intent (E1), the
/// other returns MutationConflict (E2), and the durable source/generation
/// advances exactly once (E3). Equal generation+command+payload is constrained
/// to the replay rule above and may safely share one intent.
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
            credential_generation: 1,
            bearer: RedactedString::new("initial-token"),
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    repo.gate_next_read_pair();

    let left = enter_or_rotate_application_mcp_bearer(
        ApplicationMcpBearerCommand {
            source_id: source_id.clone(),
            workspace_id: "ws".into(),
            target_fingerprint: "target".into(),
            command_key_fingerprint: "left-key".into(),
            credential_generation: 2,
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
            credential_generation: 2,
            bearer: RedactedString::new("right-token"),
        },
        &store,
        &repo,
    );
    let (left, right) = tokio::join!(left, right);
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert_eq!(
        usize::from(matches!(left, Err(CredentialError::MutationConflict(_))))
            + usize::from(matches!(right, Err(CredentialError::MutationConflict(_)))),
        1
    );
    let durable = repo.get(&source_id).await.unwrap();
    assert_eq!(durable.version, 2);
    assert!(
        durable
            .material_ref
            .as_ref()
            .unwrap()
            .0
            .contains(":generation:2")
    );
    assert_eq!(store.inventory().await.unwrap().len(), 1);
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

/// Cause/effect graph: C1 a described source carries an exact secret-free
/// material shape; C2 create material matches/mismatches that shape; C3 rotation
/// keeps/changes the type or complete field set; C4 descriptor-only rotation
/// changes subject/expiry while retaining the same material shape; C5 a new
/// described write also supplies legacy `provider_id`. Effects: E1 matching
/// create is published once; E2 mismatched create writes neither source nor
/// material; E3 matching exact rotation advances one revision; E4 mismatched
/// rotation fails before any new sealed reference is written; E5 metadata and
/// material remain one source revision after descriptor-only rotation; E6 a
/// dual-provider write is rejected before effects; C6 material shape and target
/// usage are individually valid but mutually incompatible. E7 create and rotate
/// reject that descriptor/material contract before effects, including a
/// descriptor-only rotation against the currently sealed material.
/// C7 replacement descriptor changes the canonical provider. E8 provider
/// identity and source revision remain unchanged.
///
/// | Rule | create shape | rotation shape | Effect |
/// |---|---|---|---|
/// | D1 | exact | - | E1 |
/// | D2 | mismatch | - | E2 |
/// | D3 | exact | exact | E3 |
/// | D4 | exact | mismatch | E4 |
/// | D5 | exact | same shape, new metadata | E5 |
/// | D6 | exact + descriptor/provider_id | - | E6 |
/// | D7 | shape exact, usage incompatible | - | E7 |
/// | D8 | exact | replacement shape exact, usage incompatible | E7 |
/// | D9 | exact | descriptor-only usage incompatible | E7 |
/// | D10 | exact | descriptor provider changes | E8 |
/// | D11 | unsupported target-less consumer purpose | - | E7, zero writes |
#[tokio::test]
async fn described_material_is_validated_before_create_and_rotation_effects() {
    use awaken_credential_contract::{
        CredentialDescriptor, CredentialMaterialDescriptor, CredentialPurpose, CredentialUsage,
        HTTP_BASIC_MATERIAL_TYPE, http_basic_material,
    };

    let descriptor = CredentialDescriptor::new(
        "github",
        CredentialMaterialDescriptor::structured(
            HTTP_BASIC_MATERIAL_TYPE,
            ["password", "username"],
        ),
        [awaken_credential_contract::CredentialTargetContract::new(
            awaken_credential_contract::CredentialTarget::new(
                CredentialPurpose::RepositoryTransport,
                awaken_credential_contract::repository_transport_audience(
                    "https://github.com/awaken/example.git",
                )
                .unwrap(),
            ),
            CredentialUsage::HttpBasicAuth,
        )],
    )
    .with_subject("installation-1")
    .with_expiry(u64::MAX - 1);
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let params = |material| CredentialCreateParams {
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: None,
        env_key: None,
        secret: Some(encode_structured_material(material).unwrap()),
        oauth_command: None,
    };
    let material = || {
        http_basic_material(
            RedactedString::new("x-access-token"),
            RedactedString::new("token"),
        )
    };
    let source_id = CredentialSourceId("cred:github:described".into());
    let created = enter_credential_idempotent_described(
        source_id.clone(),
        params(material()),
        None,
        descriptor.clone(),
        &store,
        &repo,
    )
    .await
    .expect("D1/E1");
    assert!(created.created, "D1/E1");
    assert_eq!(created.source.descriptor, Some(descriptor.clone()), "D1/E1");

    let bad_id = CredentialSourceId("cred:github:bad-shape".into());
    let before = store.inventory().await.unwrap();
    assert!(
        enter_credential_idempotent_described(
            bad_id.clone(),
            params(awaken_agent_contract::StructuredCredentialMaterial {
                type_id: "acme.other/v1".into(),
                fields: std::collections::BTreeMap::from([
                    ("password".into(), RedactedString::new("token")),
                    ("username".into(), RedactedString::new("x-access-token")),
                ]),
            }),
            None,
            descriptor.clone(),
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D2/E2"
    );
    assert!(
        matches!(
            repo.get(&bad_id).await,
            Err(CredentialError::SourceNotFound(_))
        ),
        "D2/E2"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D2/E2");

    let unsupported_id = CredentialSourceId("cred:github:unsupported-purpose".into());
    let unsupported_descriptor = CredentialDescriptor::new(
        "github",
        descriptor.material.clone(),
        [awaken_credential_contract::CredentialTargetContract::new(
            awaken_credential_contract::CredentialTarget::new(
                CredentialPurpose::ProviderAdapter,
                "provider://github",
            ),
            CredentialUsage::ProviderAdapter,
        )],
    );
    let before = store.inventory().await.unwrap();
    assert!(
        enter_credential_idempotent_described(
            unsupported_id.clone(),
            params(material()),
            None,
            unsupported_descriptor,
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D11/E7"
    );
    assert!(
        matches!(
            repo.get(&unsupported_id).await,
            Err(CredentialError::SourceNotFound(_))
        ),
        "D11/E7 no source"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D11/E7");

    let dual_provider_id = CredentialSourceId("cred:github:dual-provider".into());
    let before = store.inventory().await.unwrap();
    assert!(
        enter_credential_idempotent_described(
            dual_provider_id.clone(),
            CredentialCreateParams {
                provider_id: Some("github".into()),
                ..params(material())
            },
            None,
            descriptor.clone(),
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D6/E6"
    );
    assert!(
        matches!(
            repo.get(&dual_provider_id).await,
            Err(CredentialError::SourceNotFound(_))
        ),
        "D6/E6"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D6/E6");

    let incompatible_descriptor = CredentialDescriptor::new(
        "github",
        CredentialMaterialDescriptor::secret("awaken.secret/v1"),
        descriptor.targets.clone(),
    );
    let incompatible_id = CredentialSourceId("cred:github:incompatible-create".into());
    let before = store.inventory().await.unwrap();
    assert!(
        enter_credential_idempotent_described(
            incompatible_id.clone(),
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("scalar-token")),
                oauth_command: None,
            },
            None,
            incompatible_descriptor.clone(),
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D7/E7"
    );
    assert!(
        matches!(
            repo.get(&incompatible_id).await,
            Err(CredentialError::SourceNotFound(_))
        ),
        "D7/E7"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D7/E7");

    let rotated = rotate_credential_materials_exact(
        &source_id,
        1,
        CredentialMaterialPatch {
            primary: Some(encode_structured_material(material()).unwrap()),
            auxiliary: std::collections::BTreeMap::new(),
            descriptor: None,
        },
        &store,
        &repo,
    )
    .await
    .expect("D3/E3");
    assert_eq!(rotated.version, 2, "D3/E3");

    let updated_descriptor = descriptor
        .clone()
        .with_subject("installation-2")
        .with_expiry(u64::MAX);
    let metadata_rotated = rotate_credential_materials_exact(
        &source_id,
        2,
        CredentialMaterialPatch {
            primary: None,
            auxiliary: std::collections::BTreeMap::new(),
            descriptor: Some(updated_descriptor.clone()),
        },
        &store,
        &repo,
    )
    .await
    .expect("D5/E5");
    assert_eq!(metadata_rotated.version, 3, "D5/E5");
    assert_eq!(
        metadata_rotated.descriptor,
        Some(updated_descriptor),
        "D5/E5"
    );

    let before = store.inventory().await.unwrap();
    assert!(
        rotate_credential_materials_exact(
            &source_id,
            3,
            CredentialMaterialPatch {
                primary: Some(
                    encode_structured_material(
                        awaken_agent_contract::StructuredCredentialMaterial {
                            type_id: "acme.other/v1".into(),
                            fields: std::collections::BTreeMap::from([
                                ("password".into(), RedactedString::new("token")),
                                ("username".into(), RedactedString::new("x-access-token")),
                            ]),
                        }
                    )
                    .unwrap(),
                ),
                auxiliary: std::collections::BTreeMap::new(),
                descriptor: None,
            },
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D4/E4 mismatched type"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D4/E4");

    let before = store.inventory().await.unwrap();
    assert!(
        rotate_credential_materials_exact(
            &source_id,
            3,
            CredentialMaterialPatch {
                primary: Some(RedactedString::new("scalar-token")),
                auxiliary: std::collections::BTreeMap::new(),
                descriptor: Some(incompatible_descriptor),
            },
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D8/E7"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D8/E7");

    let descriptor_only_incompatible = CredentialDescriptor::new(
        "github",
        descriptor.material.clone(),
        [awaken_credential_contract::CredentialTargetContract::new(
            descriptor.targets[0].target.clone(),
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        )],
    );
    let before = store.inventory().await.unwrap();
    assert!(
        rotate_credential_materials_exact(
            &source_id,
            3,
            CredentialMaterialPatch {
                primary: None,
                auxiliary: std::collections::BTreeMap::new(),
                descriptor: Some(descriptor_only_incompatible),
            },
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D9/E7"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D9/E7");

    let changed_provider = CredentialDescriptor {
        provider: awaken_credential_contract::CredentialProviderRef("github-enterprise".into()),
        ..descriptor.clone()
    };
    let before = store.inventory().await.unwrap();
    assert!(
        rotate_credential_materials_exact(
            &source_id,
            3,
            CredentialMaterialPatch {
                primary: None,
                auxiliary: std::collections::BTreeMap::new(),
                descriptor: Some(changed_provider),
            },
            &store,
            &repo,
        )
        .await
        .is_err(),
        "D10/E8"
    );
    assert_eq!(store.inventory().await.unwrap(), before, "D10/E8");
    assert_eq!(repo.get(&source_id).await.unwrap().version, 3, "D10/E8");
}
