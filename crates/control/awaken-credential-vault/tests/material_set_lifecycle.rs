use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, CredentialMutationIntent, CredentialRepo, CredentialRetirement,
    InMemoryCredentialRepo, enter_credential_with_materials, recover_credential_mutations,
    revoke_credential_exact, rotate_credential_materials_exact,
};
use awaken_credential_vault::{
    CredentialCreateParams, CredentialError, CredentialKind, InMemorySecretStore,
    OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretRef, SecretStore,
};

struct FailNthPutStore {
    inner: InMemorySecretStore,
    puts: AtomicUsize,
    fail_on: AtomicUsize,
}

impl FailNthPutStore {
    fn new() -> Self {
        Self {
            inner: InMemorySecretStore::new(),
            puts: AtomicUsize::new(0),
            fail_on: AtomicUsize::new(usize::MAX),
        }
    }

    fn fail_after_one_more_success(&self) {
        self.fail_on
            .store(self.puts.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SecretStore for FailNthPutStore {
    async fn put(
        &self,
        reference: &SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        let call = self.puts.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on.load(Ordering::SeqCst) {
            return Err(CredentialError::Storage(
                "injected rotation put failure".into(),
            ));
        }
        self.inner.put(reference, secret).await
    }

    async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
        self.inner.get(reference).await
    }

    async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(reference).await
    }

    async fn inventory(&self) -> Result<Vec<SecretRef>, CredentialError> {
        self.inner.inventory().await
    }
}

/// Cause/effect graph and derived decision-table rules:
/// C1 exact active revision; C2 primary supplied; C3 auxiliary slot supplied;
/// C4 auxiliary slot removed; C5 stale revision; C6 terminal retirement.
/// R1 C1+C2+C3 -> one higher revision, fresh refs, old refs reclaimed.
/// R2 C1+C4 -> one higher revision, removed ref reclaimed, retained refs live.
/// R3 C5 -> conflict before secret-store effects.
/// R4 C6 -> archived row owns no refs and every remaining material is reclaimed.
#[tokio::test]
async fn named_material_set_rotates_and_reclaims_as_one_exact_aggregate() {
    let repo = InMemoryCredentialRepo::new();
    let store = InMemorySecretStore::new();
    let created = enter_credential_with_materials(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("mcp".into()),
            env_key: None,
            secret: Some(RedactedString::new("access-1")),
            oauth_command: None,
        },
        BTreeMap::from([
            (
                OAUTH_REFRESH_TOKEN_SLOT.into(),
                RedactedString::new("refresh-1"),
            ),
            (
                OAUTH_CLIENT_SECRET_SLOT.into(),
                RedactedString::new("client-1"),
            ),
        ]),
        &store,
        &repo,
    )
    .await
    .unwrap();
    let old_primary = created.material_ref.clone().unwrap();
    let old_refresh = created
        .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
        .unwrap()
        .clone();

    let rotated = rotate_credential_materials_exact(
        &created.id,
        created.version,
        CredentialMaterialPatch {
            primary: Some(RedactedString::new("access-2")),
            auxiliary: BTreeMap::from([(
                OAUTH_REFRESH_TOKEN_SLOT.into(),
                Some(RedactedString::new("refresh-2")),
            )]),
            descriptor: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    assert_eq!(rotated.version, created.version + 1);
    assert_eq!(store.inventory().await.unwrap().len(), 3);
    assert!(store.get(&old_primary).await.is_err());
    assert!(store.get(&old_refresh).await.is_err());
    assert_eq!(
        store
            .get(
                rotated
                    .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                    .unwrap(),
            )
            .await
            .unwrap()
            .expose_secret(),
        "client-1"
    );

    let inventory_before_stale = store.inventory().await.unwrap();
    assert!(matches!(
        rotate_credential_materials_exact(
            &created.id,
            created.version,
            CredentialMaterialPatch {
                primary: Some(RedactedString::new("must-not-write")),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            &store,
            &repo,
        )
        .await,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(store.inventory().await.unwrap(), inventory_before_stale);

    let without_client = rotate_credential_materials_exact(
        &rotated.id,
        rotated.version,
        CredentialMaterialPatch {
            primary: None,
            auxiliary: BTreeMap::from([(OAUTH_CLIENT_SECRET_SLOT.into(), None)]),
            descriptor: None,
        },
        &store,
        &repo,
    )
    .await
    .unwrap();
    assert!(
        without_client
            .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
            .is_none()
    );
    assert_eq!(store.inventory().await.unwrap().len(), 2);

    let archived = revoke_credential_exact(
        &without_client.id,
        without_client.version,
        CredentialRetirement::Archive,
        &store,
        &repo,
    )
    .await
    .unwrap();
    assert_eq!(archived.material_refs().count(), 0);
    assert!(store.inventory().await.unwrap().is_empty());
}

/// Rotation failure FMECA and cause/effect decision table:
///
/// | Rule | source/revision | patch | injected failure | Effect |
/// |---|---|---|---|---|
/// | F1 | active/current | empty | none | idempotent no-op; no new revision/ref |
/// | F2 | active/current | invalid slot | none | reject before WAL/secret write |
/// | F3 | active/current | primary + auxiliary | second put | preserve old row/material; remove every candidate ref |
/// | F4 | disabled/current | material | none | reject before secret write |
/// | F5 | active/i64::MAX | material | none | fail revision exhaustion before WAL/secret write |
///
/// These rules close partial-write, invalid-input, retired-source and counter-
/// exhaustion modes around the same WAL/CAS authority. Recovery of ambiguous
/// pre/post-publication intents is covered by `recovery_reconciles_*` below.
#[tokio::test]
async fn rotation_failures_preserve_one_committed_revision_and_material_set() {
    async fn inventory(store: &dyn SecretStore) -> BTreeSet<String> {
        store
            .inventory()
            .await
            .unwrap()
            .into_iter()
            .map(|reference| reference.0)
            .collect()
    }

    let repo = InMemoryCredentialRepo::new();
    let store = FailNthPutStore::new();
    let created = enter_credential_with_materials(
        CredentialCreateParams {
            workspace_id: "ws-failure".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("mcp".into()),
            env_key: None,
            secret: Some(RedactedString::new("primary-old")),
            oauth_command: None,
        },
        BTreeMap::from([(
            OAUTH_REFRESH_TOKEN_SLOT.into(),
            RedactedString::new("refresh-old"),
        )]),
        &store,
        &repo,
    )
    .await
    .unwrap();
    let original_inventory = inventory(&store).await;

    let unchanged = rotate_credential_materials_exact(
        &created.id,
        created.version,
        CredentialMaterialPatch::default(),
        &store,
        &repo,
    )
    .await
    .expect("F1");
    assert_eq!(unchanged, created, "F1");
    assert_eq!(inventory(&store).await, original_inventory, "F1");

    let invalid = rotate_credential_materials_exact(
        &created.id,
        created.version,
        CredentialMaterialPatch {
            primary: None,
            auxiliary: BTreeMap::from([(
                "../escape".into(),
                Some(RedactedString::new("must-not-write")),
            )]),
            descriptor: None,
        },
        &store,
        &repo,
    )
    .await;
    assert!(
        matches!(invalid, Err(CredentialError::InvalidSource(_))),
        "F2"
    );
    assert_eq!(inventory(&store).await, original_inventory, "F2");

    store.fail_after_one_more_success();
    let partial = rotate_credential_materials_exact(
        &created.id,
        created.version,
        CredentialMaterialPatch {
            primary: Some(RedactedString::new("primary-new")),
            auxiliary: BTreeMap::from([(
                OAUTH_REFRESH_TOKEN_SLOT.into(),
                Some(RedactedString::new("refresh-new")),
            )]),
            descriptor: None,
        },
        &store,
        &repo,
    )
    .await;
    assert!(matches!(partial, Err(CredentialError::Storage(_))), "F3");
    assert_eq!(repo.get(&created.id).await.unwrap(), created, "F3");
    assert_eq!(inventory(&store).await, original_inventory, "F3");
    assert!(repo.pending_mutations().await.unwrap().is_empty(), "F3");

    let disabled = awaken_credential_vault::repo::transition_credential_status(
        &created.id,
        awaken_credential_vault::CredentialStatus::Disabled,
        &repo,
    )
    .await
    .unwrap();
    let before_disabled = inventory(&store).await;
    assert!(
        matches!(
            rotate_credential_materials_exact(
                &disabled.id,
                disabled.version,
                CredentialMaterialPatch {
                    primary: Some(RedactedString::new("must-not-write")),
                    auxiliary: BTreeMap::new(),
                    descriptor: None,
                },
                &store,
                &repo,
            )
            .await,
            Err(CredentialError::NotActive(_))
        ),
        "F4"
    );
    assert_eq!(inventory(&store).await, before_disabled, "F4");

    let mut exhausted = disabled.clone();
    exhausted.status = awaken_credential_vault::CredentialStatus::Active;
    exhausted.version = i64::MAX;
    repo.put(exhausted.clone()).await.unwrap();
    assert!(
        matches!(
            rotate_credential_materials_exact(
                &exhausted.id,
                exhausted.version,
                CredentialMaterialPatch {
                    primary: Some(RedactedString::new("must-not-write")),
                    auxiliary: BTreeMap::new(),
                    descriptor: None,
                },
                &store,
                &repo,
            )
            .await,
            Err(CredentialError::InvalidSource(message)) if message.contains("overflow")
        ),
        "F5"
    );
    assert_eq!(inventory(&store).await, before_disabled, "F5");
    assert!(repo.pending_mutations().await.unwrap().is_empty(), "F5");
}

/// Recovery decision table: C1 intent unpublished -> E1 delete every new ref
/// and preserve every old ref; C2 intent published -> E2 delete every retired
/// old ref and preserve every new ref. Both rules end by removing the WAL row.
#[tokio::test]
async fn recovery_reconciles_the_complete_named_material_set() {
    for committed in [false, true] {
        let repo = InMemoryCredentialRepo::new();
        let store = InMemorySecretStore::new();
        let before = enter_credential_with_materials(
            CredentialCreateParams {
                workspace_id: format!("ws-{committed}"),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("access-old")),
                oauth_command: None,
            },
            BTreeMap::from([(
                OAUTH_REFRESH_TOKEN_SLOT.into(),
                RedactedString::new("refresh-old"),
            )]),
            &store,
            &repo,
        )
        .await
        .unwrap();
        let old_refs = before.material_refs().cloned().collect::<Vec<_>>();
        let mut after = before.clone();
        after.version += 1;
        after.material_ref = Some(SecretRef(format!("sec:{}:r2:primary", after.id.0)));
        after.auxiliary_material_refs.insert(
            OAUTH_REFRESH_TOKEN_SLOT.into(),
            SecretRef(format!("sec:{}:r2:{OAUTH_REFRESH_TOKEN_SLOT}", after.id.0)),
        );
        let new_refs = after.material_refs().cloned().collect::<Vec<_>>();
        for reference in &new_refs {
            store
                .put(reference, RedactedString::new("new"))
                .await
                .unwrap();
        }
        let intent = CredentialMutationIntent {
            before: Some(before),
            after,
        };
        repo.begin_mutation(intent.clone()).await.unwrap();
        if committed {
            repo.apply_mutation(&intent).await.unwrap();
        }

        assert_eq!(
            recover_credential_mutations(&store, &repo).await.unwrap(),
            1
        );
        for reference in if committed { &old_refs } else { &new_refs } {
            assert!(store.get(reference).await.is_err());
        }
        for reference in if committed { &new_refs } else { &old_refs } {
            assert!(store.get(reference).await.is_ok());
        }
        assert!(repo.pending_mutations().await.unwrap().is_empty());
    }
}
