use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::{CredentialKind, InMemorySecretStore};
use awaken_agent_contract::RedactedString;

#[derive(Default)]
struct RecordingRolloutTarget {
    fail: AtomicBool,
    pending: AtomicBool,
    events: std::sync::Mutex<Vec<ManagedCredentialRollout>>,
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for RecordingRolloutTarget {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ManagedCredentialAdoptionError::Unavailable(
                "service controller unavailable".into(),
            ));
        }
        self.events.lock().unwrap().push(event.clone());
        if self.pending.load(Ordering::SeqCst) {
            return Ok(ManagedCredentialAdoptionProgress::Pending);
        }
        Ok(ManagedCredentialAdoptionProgress::Converged)
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
    reads: AtomicUsize,
    deletes: AtomicUsize,
}

struct AmbiguousManagedPutStore {
    inner: InMemorySecretStore,
    put_landed: tokio::sync::Barrier,
    release_response: tokio::sync::Barrier,
}

struct FailedPutAndCleanupStore {
    inner: InMemorySecretStore,
}

#[derive(Default)]
struct UnopenableManagedWriteStore {
    inner: InMemorySecretStore,
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
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.poison.lock().unwrap().as_ref() == Some(r) {
            return Err(CredentialError::Storage("poison material read".into()));
        }
        self.inner.get(r).await
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
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

#[async_trait::async_trait]
impl SecretStore for FailedPutAndCleanupStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(r, secret).await?;
        Err(CredentialError::Storage("primary put response lost".into()))
    }

    async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        self.inner.get(r).await
    }

    async fn delete(&self, _r: &crate::SecretRef) -> Result<(), CredentialError> {
        Err(CredentialError::Storage(
            "cleanup delete unavailable".into(),
        ))
    }
}

#[async_trait::async_trait]
impl SecretStore for UnopenableManagedWriteStore {
    async fn put(
        &self,
        r: &crate::SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(r, secret).await
    }

    async fn get(&self, _r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
        Err(CredentialError::Seal)
    }

    async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(r).await
    }

    async fn inventory(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        self.inner.inventory().await
    }
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

fn managed_command(url: &str) -> ManagedCredentialCreateCommand {
    ManagedCredentialCreateCommand {
        source: CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("token")),
            oauth_command: None,
        },
        descriptor: None,
        source_id: Some(CredentialSourceId("source-1".into())),
        protocol_endpoint_id: None,
        primary_material_ref: None,
        auxiliary_materials: BTreeMap::new(),
        credential_id: "credential-1".into(),
        vault_id: "vault-1".into(),
        auth: crate::catalog::ManagedCredentialAuth::StaticBearer {
            mcp_server_url: url.into(),
        },
        metadata: BTreeMap::new(),
        display_name: None,
    }
}

#[tokio::test]
async fn managed_creation_publishes_source_and_child_in_one_repository_step() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    let (source, child) =
        create_managed_credential(managed_command("https://mcp.example.com"), &secrets, &repo)
            .await
            .unwrap();

    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert_eq!(
        repo.get_vault_credential("ws", &child.id).await.unwrap(),
        Some(child)
    );
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
}

/// Pending-follower rule: C1 one logical Managed create already owns a durable
/// random attempt; C2 a follower prepares the same secret-free command under a
/// fresh attempt; C3 plaintext cannot be compared in the WAL. C1+C2+C3 yields
/// an explicit transient MutationConflict, zero follower SecretStore writes,
/// and the original WAL/ref remains the sole physical attempt. Any published
/// replay policy remains owned by the stable application command above this
/// generic Managed aggregate writer.
#[tokio::test]
async fn managed_logical_pending_follower_performs_zero_material_writes() {
    let repo = InMemoryCredentialRepo::new();
    let store = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, _secret) =
        prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let owner = PendingManagedCredentialMutation::create(source, child).unwrap();
    assert!(repo.begin_managed_mutation(owner.clone()).await.unwrap());

    let follower =
        create_managed_credential(managed_command("https://mcp.example.com"), &store, &repo).await;

    assert!(matches!(
        follower,
        Err(ManagedCredentialCreationError::Credential(
            CredentialError::MutationConflict(_)
        ))
    ));
    assert!(store.inventory().await.unwrap().is_empty());
    assert_eq!(repo.pending_managed_mutations().await.unwrap(), vec![owner]);
}

/// Managed publication cause/effect graph: C1 parent admission succeeds; C2
/// material put succeeds; C3 the same SecretStore opens the exact material.
/// Only C1+C2+C3 may atomically publish the Source/child pair. A seal failure at
/// C3 first enters abort-reclaim, deletes the unpublished material, and leaves
/// neither executable aggregate nor pending mutation.
///
/// | Rule | C1 admitted | C2 put | C3 exact open | Effect |
/// |---|---|---|---|---|
/// | M1 | T | success | exact | publish pair |
/// | M2 | T | success | seal failure | publish nothing; reclaim material |
#[tokio::test]
async fn managed_creation_does_not_publish_unopenable_material() {
    let repo = InMemoryCredentialRepo::new();
    let store = UnopenableManagedWriteStore::default();
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    let result =
        create_managed_credential(managed_command("https://mcp.example.com"), &store, &repo).await;

    assert!(
        matches!(
            result,
            Err(ManagedCredentialCreationError::Credential(
                CredentialError::Seal
            ))
        ),
        "M2"
    );
    assert!(
        matches!(
            repo.get(&CredentialSourceId("source-1".into())).await,
            Err(CredentialError::SourceNotFound(_))
        ),
        "M2"
    );
    assert!(
        repo.get_vault_credential("ws", "credential-1")
            .await
            .unwrap()
            .is_none(),
        "M2"
    );
    assert!(store.inventory().await.unwrap().is_empty(), "M2");
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "M2"
    );
}

#[tokio::test]
async fn managed_vault_delete_fences_children_waits_for_rollout_and_tombstones_root() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (source, child) =
        create_managed_credential(managed_command("https://mcp.example.com"), &secrets, &repo)
            .await
            .unwrap();
    let current = repo.get_vault("ws", "vault-1").await.unwrap().unwrap();
    let (requested, changed) =
        crate::catalog::request_managed_vault_deletion(&current, "2026-08-16T00:00:00Z".into())
            .unwrap();
    assert!(changed, "R1 durable delete request is new");
    repo.replace_vault("ws", current.revision, requested.clone())
        .await
        .unwrap();

    assert!(matches!(
        retire_managed_credential(
            "ws",
            "vault-1",
            &child.id,
            ManagedCredentialOperation::Archive,
            "2026-08-16T00:00:01Z".into(),
            &secrets,
            &repo,
        )
        .await,
        Err(ManagedCredentialMutationError::InvalidLifecycle)
    ));
    assert_eq!(
        repo.get_vault_credential("ws", &child.id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle,
        ManagedCredentialLifecycle::Active,
        "R1 every non-delete child mutation is fenced once root deletion starts"
    );

    assert!(
        !reconcile_managed_vault_deletion(&requested, &secrets, &repo)
            .await
            .unwrap(),
        "R2 committed child rollout keeps root pending"
    );
    let retired = repo
        .get_vault_credential("ws", &child.id)
        .await
        .unwrap()
        .unwrap();
    assert!(retired.lifecycle.is_deleted(), "R2 child is tombstoned");
    let retired_source = repo.get(&source.id).await.unwrap();
    assert_eq!(retired_source.status, CredentialStatus::Archived, "R2");
    assert!(retired_source.material_refs().next().is_none(), "R2");
    assert!(secrets.inventory().await.unwrap().is_empty(), "R2");

    let rollout = repo.pending_managed_rollouts().await.unwrap().remove(0);
    repo.complete_managed_rollout(&rollout).await.unwrap();
    let pending = repo.get_vault("ws", "vault-1").await.unwrap().unwrap();
    assert!(
        reconcile_managed_vault_deletion(&pending, &secrets, &repo)
            .await
            .unwrap(),
        "R3 exact rollout acknowledgement permits completion"
    );
    assert!(
        repo.get_vault("ws", "vault-1")
            .await
            .unwrap()
            .unwrap()
            .is_deleted(),
        "R3 root tombstone is absorbing"
    );
    let mut new_child = managed_command("https://new.example.com");
    new_child.source_id = Some(CredentialSourceId("source-2".into()));
    new_child.credential_id = "credential-2".into();
    assert!(matches!(
        create_managed_credential(new_child, &secrets, &repo).await,
        Err(ManagedCredentialCreationError::Admission(
            ManagedCredentialAdmissionError::VaultUnavailable
        ))
    ));
}

/// Rollout delivery cause/effect graph: C1 one exact committed outbox event;
/// C2 an exact primary-id read is present/absent; C3 the target fails, reports
/// pending, or converges. Effects: E1 the bounded read returns only that event,
/// E2 the event is retained, E3 the exact event is acknowledged, E4 no
/// different payload is removed. Decision rules: R0 C1+C2 present -> E1 and an
/// unknown id -> absent; R1 C1+failure -> Pending/E2; R2 C1+pending ->
/// Pending/E2; R3 C1+converged -> Converged/E3+E4 and the same exact read is
/// absent. The public batch reconciler consumes this same single-event result
/// and counts only R3.
#[tokio::test]
async fn managed_update_atomically_publishes_exact_rollout_until_service_acknowledges() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (_, before) =
        create_managed_credential(managed_command("https://mcp.example.com"), &secrets, &repo)
            .await
            .unwrap();

    let after = update_managed_credential(
        before.clone(),
        before.clone(),
        CredentialMaterialPatch {
            primary: Some(RedactedString::new("rotated")),
            auxiliary: BTreeMap::new(),
            descriptor: None,
        },
        false,
        &secrets,
        &repo,
    )
    .await
    .unwrap();
    let events = repo.pending_managed_rollouts().await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].workspace_id, "ws");
    assert_eq!(events[0].vault_id, "vault-1");
    assert_eq!(events[0].source_version, 2);
    assert_eq!(events[0].credential_revision, after.revision);
    assert_eq!(
        repo.managed_rollout(&events[0].id).await.unwrap(),
        Some(events[0].clone()),
        "R0"
    );
    assert_eq!(repo.managed_rollout("missing").await.unwrap(), None, "R0");

    let target = RecordingRolloutTarget::default();
    target.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        reconcile_managed_credential_rollouts(&repo, &target)
            .await
            .unwrap(),
        0
    );
    assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);

    target.fail.store(false, Ordering::SeqCst);
    target.pending.store(true, Ordering::SeqCst);
    assert_eq!(
        reconcile_managed_credential_rollouts(&repo, &target)
            .await
            .unwrap(),
        0
    );
    assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);

    target.pending.store(false, Ordering::SeqCst);
    assert_eq!(
        reconcile_managed_credential_rollouts(&repo, &target)
            .await
            .unwrap(),
        1
    );
    assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    assert_eq!(
        repo.managed_rollout(&events[0].id).await.unwrap(),
        None,
        "R3"
    );
    let attempts = target.events.lock().unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|attempt| attempt == &events[0]));
}

#[tokio::test]
async fn managed_update_rejects_a_conflicting_rollout_id_before_publishing_the_pair() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (source, before) =
        create_managed_credential(managed_command("https://mcp.example.com"), &secrets, &repo)
            .await
            .unwrap();
    let conflicting = ManagedCredentialRollout {
        id: format!(
            "managed-update:{}:{}:{}",
            source.id.0, source.version, before.revision
        ),
        workspace_id: "other-workspace".into(),
        vault_id: "other-vault".into(),
        credential_id: "other-credential".into(),
        source_id: CredentialSourceId("other-source".into()),
        source_version: 99,
        credential_revision: 99,
        operation: ManagedCredentialOperation::Update,
    };
    repo.state
        .lock()
        .unwrap()
        .managed_rollouts
        .insert(conflicting.id.clone(), conflicting.clone());

    let result = update_managed_credential(
        before.clone(),
        before.clone(),
        CredentialMaterialPatch::default(),
        true,
        &secrets,
        &repo,
    )
    .await;

    assert!(matches!(
        result,
        Err(ManagedCredentialMutationError::RevisionConflict)
    ));
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert_eq!(
        repo.get_vault_credential("ws", &before.id).await.unwrap(),
        Some(before)
    );
    assert_eq!(
        repo.pending_managed_rollouts().await.unwrap(),
        vec![conflicting]
    );
}

#[tokio::test]
async fn managed_material_refs_are_unique_to_the_writer_attempt() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    let (source, _) =
        create_managed_credential(managed_command("https://mcp.example.com"), &secrets, &repo)
            .await
            .unwrap();
    let reference = source.material_ref.expect("Managed primary material");
    assert!(reference.0.contains(":attempt:"));
    assert_eq!(
        secrets.get(&reference).await.unwrap().expose_secret(),
        "token"
    );
}

#[tokio::test]
async fn legacy_durable_rows_may_recover_but_cannot_enter_as_new_commands() {
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, _) = prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let mut legacy = PendingManagedCredentialMutation::create(source, child).unwrap();
    legacy.material_fence.format_version = 0;

    assert!(legacy.validate().is_ok());
    assert!(repo.begin_managed_mutation(legacy).await.is_err());
}

/// Managed fresh-command admission uses the same decision rules as source-only
/// publication: new material plus forged Ready/takeover epoch/foreign owner
/// yields rejection before a durable pair or external effect.
#[tokio::test]
async fn managed_begin_rejects_every_forged_fresh_material_owner_shape() {
    let repo = InMemoryCredentialRepo::new();
    let command = managed_command("https://mcp.example.com");
    let (source, _) = prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let canonical = PendingManagedCredentialMutation::create(source, child).unwrap();
    let mut forged_ready = canonical.clone();
    forged_ready.material_fence.phase = CredentialMaterialMutationPhase::Ready;
    let mut forged_epoch = canonical.clone();
    forged_epoch.material_fence.writer_epoch += 1;
    let mut forged_owner = canonical;
    forged_owner.material_fence.writer_token = "foreign-owner".into(); // awaken-allow: secret -- writer identity, not material

    for forged in [forged_ready, forged_epoch, forged_owner] {
        assert!(matches!(
            repo.begin_managed_mutation(forged).await,
            Err(CredentialError::MutationConflict(_))
        ));
    }
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

/// Recovery admission rule: C1 a durable row claims `Ready`; C2 its complete
/// Source/child envelope is malformed. C1+C2 must fail before phase
/// classification causes any SecretStore read/delete, and the exact row remains
/// available for operator repair. Legacy format zero remains valid under the
/// separate generic-envelope rule exercised below.
#[tokio::test]
async fn malformed_ready_managed_recovery_performs_zero_secret_store_io() {
    let repo = InMemoryCredentialRepo::new();
    let store = PoisonGetStore {
        inner: InMemorySecretStore::new(),
        poison: std::sync::Mutex::new(None),
        reads: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
    };
    let command = managed_command("https://mcp.example.com");
    let (source, _) = prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let writing = PendingManagedCredentialMutation::create(source, child).unwrap();
    let mut malformed = writing.with_material_ready().unwrap();
    malformed.after_credential.workspace_id = "forged-workspace".into();
    repo.state
        .lock()
        .unwrap()
        .managed_mutations
        .insert(malformed.after_source.id.0.clone(), malformed.clone());

    assert!(
        recover_managed_credential_mutations_at(&store, &repo, 0)
            .await
            .is_err()
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![malformed]
    );
}

/// Durable-row identity rule: C1 a valid Ready Managed envelope is persisted
/// under a different map/table source key. E1 recovery rejects the entire scan
/// before SecretStore get/delete and retains the row. The envelope source id is
/// the only command identity; its durable key is not an alias.
#[tokio::test]
async fn managed_recovery_rejects_a_mismatched_durable_key_before_secret_io() {
    let repo = InMemoryCredentialRepo::new();
    let store = PoisonGetStore {
        inner: InMemorySecretStore::new(),
        poison: std::sync::Mutex::new(None),
        reads: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
    };
    let command = managed_command("https://mcp.example.com");
    let (source, _) = prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let ready = PendingManagedCredentialMutation::create(source, child)
        .unwrap()
        .with_material_ready()
        .unwrap();
    repo.state
        .lock()
        .unwrap()
        .managed_mutations
        .insert("wrong-managed-row-key".into(), ready.clone());

    assert!(
        recover_managed_credential_mutations_at(&store, &repo, 0)
            .await
            .is_err(),
        "E1"
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), 0, "E1");
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0, "E1");
    assert_eq!(
        repo.state
            .lock()
            .unwrap()
            .managed_mutations
            .values()
            .cloned()
            .collect::<Vec<_>>(),
        vec![ready],
        "E1"
    );
}

#[tokio::test]
async fn legacy_writing_json_without_attempt_or_writer_fields_is_claimed_and_aborted() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, secret) =
        prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let mut current = PendingManagedCredentialMutation::create(source, child.clone()).unwrap();
    let legacy_reference = crate::SecretRef(
        current
            .after_source
            .material_ref
            .as_ref()
            .unwrap()
            .0
            .split(":attempt:")
            .next()
            .unwrap()
            .to_string(),
    );
    current.after_source.material_ref = Some(legacy_reference.clone());
    let mut fixture = serde_json::to_value(current).unwrap();
    let object = fixture.as_object_mut().unwrap();
    for field in [
        "format_version",
        "attempt_id",
        "writer_token",
        "writer_epoch",
        "writer_lease_expires_at_unix_ms",
    ] {
        assert!(object.remove(field).is_some(), "fixture field {field}");
    }
    let legacy: PendingManagedCredentialMutation = serde_json::from_value(fixture).unwrap();
    assert_eq!(legacy.material_fence.format_version, 0);
    assert!(legacy.material_fence.attempt_id.is_empty());
    assert!(legacy.material_fence.writer_token.is_empty());
    assert_eq!(legacy.material_fence.writer_epoch, 0);
    assert_eq!(legacy.material_fence.writer_lease_expires_at_unix_ms, 0);
    assert!(legacy.validate().is_ok());

    secrets
        .put(&legacy_reference, secret.unwrap())
        .await
        .unwrap();
    repo.state
        .lock()
        .unwrap()
        .managed_mutations
        .insert(legacy.after_source.id.0.clone(), legacy.clone());

    assert_eq!(
        recover_managed_credential_mutations_at(&secrets, &repo, 0)
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        repo.get(&legacy.after_source.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));
    assert_eq!(
        repo.get_vault_credential("ws", &child.id).await.unwrap(),
        None
    );
    assert!(secrets.get(&legacy_reference).await.is_err());
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn expired_writing_owner_is_aborted_and_cannot_publish() {
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(AmbiguousManagedPutStore {
        inner: InMemorySecretStore::new(),
        put_landed: tokio::sync::Barrier::new(2),
        release_response: tokio::sync::Barrier::new(2),
    });
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, secret) =
        prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let original_reference = source.material_ref.clone().unwrap();
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child).unwrap();
    let recovery_now = pending.material_fence.writer_lease_expires_at_unix_ms + 1;

    let writer_repo = Arc::clone(&repo);
    let writer_secrets = Arc::clone(&secrets);
    let writer = tokio::spawn(async move {
        execute_managed_mutation(
            pending,
            vec![(original_reference, secret.unwrap())],
            writer_secrets.as_ref(),
            writer_repo.as_ref(),
        )
        .await
    });
    secrets.put_landed.wait().await;

    assert_eq!(
        recover_managed_credential_mutations_at(secrets.as_ref(), repo.as_ref(), recovery_now)
            .await
            .unwrap(),
        1
    );
    secrets.release_response.wait().await;
    let stale_error = writer.await.unwrap().unwrap_err();
    assert!(
        matches!(
                stale_error,
                ManagedCredentialMutationError::Compensation { ref primary, ref cleanup }
                if primary.contains("lost Managed put response")
                    && cleanup.contains("no durable pending fact")
        ),
        "a fenced stale writer reports both the ambiguous write and rejected cleanup: {stale_error}"
    );
    assert!(matches!(
        repo.get(&CredentialSourceId("source-1".into())).await,
        Err(CredentialError::SourceNotFound(_))
    ));
    assert!(secrets.inner.inventory().await.unwrap().is_empty());
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejected_managed_creation_reclaims_material_and_pending_fact() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    let error = create_managed_credential(managed_command("file:///secret"), &secrets, &repo)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ManagedCredentialCreationError::Admission(ManagedCredentialAdmissionError::InvalidMcpUrl)
    ));
    assert!(matches!(
        repo.get(&CredentialSourceId("source-1".into())).await,
        Err(CredentialError::SourceNotFound(_))
    ));
    assert!(secrets.inventory().await.unwrap().is_empty());
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn published_managed_identity_rejects_a_new_pending_fact_before_material_write() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    create_managed_credential(managed_command("https://mcp.example.com"), &secrets, &repo)
        .await
        .unwrap();
    let original_inventory = secrets.inventory().await.unwrap();
    let mut replay = managed_command("https://mcp.example.com");
    replay.primary_material_ref = Some(crate::SecretRef("sec:losing-command".into()));
    replay.source.secret = Some(RedactedString::new("replacement"));

    let error = create_managed_credential(replay, &secrets, &repo)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ManagedCredentialCreationError::Credential(CredentialError::MutationConflict(_))
    ));
    assert_eq!(secrets.inventory().await.unwrap(), original_inventory);
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn managed_creation_recovery_publishes_ready_and_aborts_expired_writing() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (mut source, secret) = prepare_source_with_id(
        CredentialSourceId("source-recovery".into()),
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("token")),
            oauth_command: None,
        },
    );
    let child = ManagedVaultCredential {
        id: "credential-recovery".into(),
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
    let complete = PendingManagedCredentialMutation::create(source.clone(), child.clone()).unwrap();
    source = complete.after_source.clone();
    let complete_lease = complete.material_fence.writer_lease_expires_at_unix_ms;
    repo.begin_managed_mutation(complete.clone()).await.unwrap();
    secrets
        .put(source.material_ref.as_ref().unwrap(), secret.unwrap())
        .await
        .unwrap();
    assert_eq!(
        recover_managed_credential_mutations_at(&secrets, &repo, complete_lease - 1)
            .await
            .unwrap(),
        0,
        "periodic recovery must not race a live Writing owner"
    );
    assert_eq!(repo.pending_managed_mutations().await.unwrap().len(), 1);
    repo.mark_managed_mutation_ready(&complete).await.unwrap();
    assert_eq!(
        recover_managed_credential_mutations_at(&secrets, &repo, complete_lease)
            .await
            .unwrap(),
        1
    );
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert_eq!(
        repo.get_vault_credential("ws", &child.id).await.unwrap(),
        Some(child)
    );

    source.id = CredentialSourceId("source-partial".into());
    source.material_ref = Some(crate::SecretRef("sec:source-partial".into()));
    source.auxiliary_material_refs.insert(
        "refresh".into(),
        crate::SecretRef("sec:source-partial:refresh".into()),
    );
    let partial_child = ManagedVaultCredential {
        id: "credential-partial".into(),
        source_id: source.id.clone(),
        ..repo
            .get_vault_credential("ws", "credential-recovery")
            .await
            .unwrap()
            .unwrap()
    };
    let partial =
        PendingManagedCredentialMutation::create(source.clone(), partial_child.clone()).unwrap();
    source = partial.after_source.clone();
    let partial_lease = partial.material_fence.writer_lease_expires_at_unix_ms;
    repo.begin_managed_mutation(partial).await.unwrap();
    secrets
        .put(
            source.material_ref.as_ref().unwrap(),
            RedactedString::new("partial"),
        )
        .await
        .unwrap();
    assert_eq!(
        recover_managed_credential_mutations_at(&secrets, &repo, partial_lease)
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        repo.get(&source.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));
    assert_eq!(
        repo.get_vault_credential("ws", &partial_child.id)
            .await
            .unwrap(),
        None
    );
    assert!(
        secrets
            .get(source.material_ref.as_ref().unwrap())
            .await
            .is_err()
    );
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn in_memory_writing_takeover_rejects_the_stale_owner() {
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, _) = prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let stale = PendingManagedCredentialMutation::create(source, child).unwrap();
    let lease_expires_at = stale.material_fence.writer_lease_expires_at_unix_ms;
    assert!(repo.begin_managed_mutation(stale.clone()).await.unwrap());
    assert!(!repo.begin_managed_mutation(stale.clone()).await.unwrap());

    assert!(
        repo.claim_expired_managed_mutation(&stale, lease_expires_at - 1, lease_expires_at + 100,)
            .await
            .unwrap()
            .is_none()
    );
    let claimed = repo
        .claim_expired_managed_mutation(&stale, lease_expires_at, lease_expires_at + 100)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        claimed.material_fence.writer_token,
        stale.material_fence.writer_token
    );
    assert_eq!(
        claimed.material_fence.writer_epoch,
        stale.material_fence.writer_epoch + 1
    );
    assert!(repo.mark_managed_mutation_ready(&stale).await.is_err());
    let stale_ready = stale.with_material_ready().unwrap();
    assert!(repo.commit_managed_mutation(&stale_ready).await.is_err());
    assert!(repo.mark_managed_mutation_ready(&claimed).await.is_err());
    repo.abort_managed_mutation(&claimed).await.unwrap();
}

#[tokio::test]
async fn managed_mutation_batch_continues_after_its_first_item_fails() {
    let repo = InMemoryCredentialRepo::new();
    let store = PoisonGetStore {
        inner: InMemorySecretStore::new(),
        poison: std::sync::Mutex::new(None),
        reads: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
    };
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    for suffix in ["a", "b"] {
        let source = CredentialSource {
            id: CredentialSourceId(format!("source-batch-{suffix}")),
            replacement_of: None,
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(crate::SecretRef(format!("sec:source-batch-{suffix}"))),
            auxiliary_material_refs: BTreeMap::new(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        let child = ManagedVaultCredential {
            id: format!("credential-batch-{suffix}"),
            vault_id: "vault-1".into(),
            workspace_id: "ws".into(),
            source_id: source.id.clone(),
            auth: crate::catalog::ManagedCredentialAuth::StaticBearer {
                mcp_server_url: format!("https://{suffix}.example.com"),
            },
            metadata: BTreeMap::new(),
            display_name: None,
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        };
        let pending = PendingManagedCredentialMutation::create(source.clone(), child).unwrap();
        let reference = pending.after_source.material_ref.clone().unwrap();
        repo.begin_managed_mutation(pending.clone()).await.unwrap();
        store
            .put(&reference, RedactedString::new(format!("token-{suffix}")))
            .await
            .unwrap();
        repo.mark_managed_mutation_ready(&pending).await.unwrap();
    }

    let ordered = repo.pending_managed_mutations().await.unwrap();
    assert_eq!(ordered.len(), 2);
    let poison = ordered[0].after_source.material_ref.clone().unwrap();
    let later_source = ordered[1].after_source.id.clone();
    *store.poison.lock().unwrap() = Some(poison);

    assert!(
        recover_managed_credential_mutations_at(&store, &repo, 0)
            .await
            .is_err(),
        "the batch must report its first item error"
    );
    assert_eq!(repo.get(&later_source).await.unwrap().id, later_source);
    assert_eq!(repo.pending_managed_mutations().await.unwrap().len(), 1);
}

#[test]
fn legacy_writing_json_defaults_to_an_expired_unowned_lease() {
    let command = managed_command("https://mcp.example.com");
    let (source, _) = prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child).unwrap();
    let mut legacy = serde_json::to_value(pending).unwrap();
    let object = legacy.as_object_mut().unwrap();
    object.remove("writer_token");
    object.remove("writer_epoch");
    object.remove("writer_lease_expires_at_unix_ms");

    let decoded: PendingManagedCredentialMutation = serde_json::from_value(legacy).unwrap();
    assert!(decoded.material_fence.writer_token.is_empty());
    assert_eq!(decoded.material_fence.writer_epoch, 0);
    assert_eq!(decoded.material_fence.writer_lease_expires_at_unix_ms, 0);
    let claimed = decoded.claim_after_expiry(0, 1).unwrap().unwrap();
    assert_eq!(claimed.material_fence.writer_epoch, 1);
    claimed.validate().unwrap();
}

#[tokio::test]
async fn aborted_managed_cleanup_survives_the_crash_prefix_before_delete() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, secret) =
        prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();
    repo.begin_managed_mutation(pending.clone()).await.unwrap();
    store.put(&reference, secret.unwrap()).await.unwrap();

    // This is the crash prefix: the database CAS landed, but no external
    // delete or completion did.
    let reclaiming = repo.abort_managed_mutation(&pending).await.unwrap();
    assert_eq!(
        reclaiming.material_fence.phase,
        CredentialMaterialMutationPhase::ReclaimingAbort
    );
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![reclaiming.clone()]
    );
    assert!(store.get(&reference).await.is_ok());

    let competing = PendingManagedCredentialMutation::create(
        pending.after_source.clone(),
        pending.after_credential.clone(),
    )
    .unwrap();
    assert!(
        !repo.begin_managed_mutation(competing).await.unwrap(),
        "the durable cleanup attempt remains the sole non-owner attachment"
    );

    assert_eq!(
        recover_managed_credential_mutations_at(&store, &repo, 1_000)
            .await
            .unwrap(),
        1
    );
    assert!(store.get(&reference).await.is_err());
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn ambiguous_aborted_delete_keeps_exact_cleanup_fact_for_retry() {
    let store = FaultyDeleteStore {
        inner: InMemorySecretStore::new(),
        fail_before_delete: AtomicBool::new(false),
        lose_first_response: AtomicBool::new(true),
    };
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, secret) =
        prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();
    repo.begin_managed_mutation(pending.clone()).await.unwrap();
    store.put(&reference, secret.unwrap()).await.unwrap();

    assert!(
        abort_managed_mutation_before_cleanup(&pending, &store, &repo)
            .await
            .is_err()
    );
    assert!(store.get(&reference).await.is_err());
    let durable = repo.pending_managed_mutations().await.unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].material_fence.phase,
        CredentialMaterialMutationPhase::ReclaimingAbort
    );

    // SecretStore deletion is idempotent: recovery can safely retry after
    // an ambiguous response, then retire only the exact durable fact.
    assert_eq!(
        recover_managed_credential_mutations_at(&store, &repo, 1_000)
            .await
            .unwrap(),
        1
    );
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_material_write_and_failed_compensation_preserve_both_failures() {
    // Failure-product test: primary write {ok, err} × compensation {ok, err}.
    // This covers the err×err corner: the caller receives both causes and the
    // durable ReclaimingAbort fact remains the sole retry authority.
    let store = FailedPutAndCleanupStore {
        inner: InMemorySecretStore::new(),
    };
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let command = managed_command("https://mcp.example.com");
    let (source, secret) =
        prepare_source_with_id(command.source_id.clone().unwrap(), command.source);
    let child = ManagedVaultCredential {
        id: command.credential_id,
        vault_id: command.vault_id,
        workspace_id: source.workspace_id.clone(),
        source_id: source.id.clone(),
        auth: command.auth,
        metadata: command.metadata,
        display_name: command.display_name,
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();

    let error = execute_managed_mutation(
        pending,
        vec![(reference.clone(), secret.unwrap())],
        &store,
        &repo,
    )
    .await
    .unwrap_err();

    match error {
        ManagedCredentialMutationError::Compensation { primary, cleanup } => {
            assert!(primary.contains("primary put response lost"));
            assert!(cleanup.contains("cleanup delete unavailable"));
        }
        other => panic!("expected composite compensation error, got {other}"),
    }
    let durable = repo.pending_managed_mutations().await.unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].material_fence.phase,
        CredentialMaterialMutationPhase::ReclaimingAbort
    );
    let durable_reference = durable[0].after_source.material_ref.as_ref().unwrap();
    assert!(store.get(durable_reference).await.is_ok());
}

#[tokio::test]
async fn committed_managed_pair_returns_success_and_retries_failed_cleanup() {
    let store = FaultyDeleteStore {
        inner: InMemorySecretStore::new(),
        fail_before_delete: AtomicBool::new(false),
        lose_first_response: AtomicBool::new(false),
    };
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (before_source, before_child) =
        create_managed_credential(managed_command("https://mcp.example.com"), &store, &repo)
            .await
            .unwrap();

    store.fail_before_delete.store(true, Ordering::SeqCst);
    let updated = update_managed_credential(
        before_child.clone(),
        before_child,
        CredentialMaterialPatch {
            primary: Some(RedactedString::new("replacement")),
            auxiliary: BTreeMap::new(),
            descriptor: None,
        },
        false,
        &store,
        &repo,
    )
    .await
    .expect("after-commit cleanup failure must not fail the logical command");

    let committed = repo.get(&before_source.id).await.unwrap();
    assert_eq!(committed.version, 2);
    assert_eq!(updated.revision, 2);
    let pending = repo.pending_managed_mutations().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].material_fence.phase,
        CredentialMaterialMutationPhase::Reclaiming
    );

    store.fail_before_delete.store(false, Ordering::SeqCst);
    assert_eq!(
        recover_managed_credential_mutations_at(&store, &repo, 1_000)
            .await
            .unwrap(),
        1
    );
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    assert!(
        store
            .get(before_source.material_ref.as_ref().unwrap())
            .await
            .is_err()
    );
}
