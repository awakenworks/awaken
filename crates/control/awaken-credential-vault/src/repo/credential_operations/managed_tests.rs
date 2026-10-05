// @formal evidence credential.failed_create_compensates_material
// @formal evidence credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
// @formal evidence credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
// @formal evidence credential.managed_rollout_failure_or_busy_session_remains_pending
// @formal evidence credential.restart_recovery_compensates_unpublished_material
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::repo::cleanup::tests::FaultyDeleteStore;
use crate::{CredentialKind, InMemorySecretStore};
use awaken_agent_contract::RedactedString;

#[derive(Default)]
struct RecordingRolloutTarget {
    failure: std::sync::Mutex<Option<ManagedCredentialAdoptionError>>,
    pending: AtomicBool,
    events: std::sync::Mutex<Vec<ManagedCredentialRollout>>,
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for RecordingRolloutTarget {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        if let Some(error) = self.failure.lock().unwrap().clone() {
            return Err(error);
        }
        self.events.lock().unwrap().push(event.clone());
        if self.pending.load(Ordering::SeqCst) {
            return Ok(ManagedCredentialAdoptionProgress::Pending);
        }
        Ok(ManagedCredentialAdoptionProgress::Converged)
    }
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
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
        created_at: "2026-09-17T00:00:00Z".into(),
    }
}

#[tokio::test]
async fn managed_creation_publishes_source_and_child_in_one_repository_step() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    let (source, child) = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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

#[tokio::test]
async fn stable_managed_creation_replays_the_published_pair_and_rejects_changed_material() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();

    let first = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let replay = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(replay, first);
    assert_eq!(secrets.inventory().await.unwrap().len(), 1);
    assert_eq!(
        repo.list_vault_credentials("ws", "vault-1")
            .await
            .unwrap()
            .len(),
        1
    );

    let mut changed = managed_command("https://mcp.example.com");
    changed.source.secret = Some(RedactedString::new("different-token"));
    let error = create_managed_credential(
        changed,
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ManagedCredentialCreationError::Credential(CredentialError::MutationConflict(_))
    ));
    assert_eq!(secrets.inventory().await.unwrap().len(), 1);
    assert_eq!(
        repo.list_vault_credentials("ws", "vault-1")
            .await
            .unwrap()
            .len(),
        1
    );
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let owner = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    assert!(
        repo.begin_managed_mutation(owner.clone(), 1_000)
            .await
            .unwrap()
    );

    let follower = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &store,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
    .await;

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

    let result = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &store,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
    .await;

    assert!(
        matches!(
            result,
            Err(ManagedCredentialCreationError::Credential(
                CredentialError::MutationConflict(_)
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
    // M2: failed readback also prevents confirmation of compensating deletion.
    // Physical absence alone (seen only by this test) cannot retire the WAL.
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap().len(),
        1,
        "M2"
    );
    assert_eq!(
        recover_managed_credential_mutations(
            &store.inner,
            &repo,
            awaken_time::system_clock().as_ref()
        )
        .await
        .unwrap(),
        1
    );
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "M2 recovered"
    );
}

#[tokio::test]
async fn managed_vault_delete_fences_children_waits_for_rollout_and_tombstones_root() {
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (source, child) = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
            awaken_time::system_clock().as_ref(),
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
        !reconcile_managed_vault_deletion(
            &requested,
            &secrets,
            &repo,
            awaken_time::system_clock().as_ref(),
        )
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
        reconcile_managed_vault_deletion(
            &pending,
            &secrets,
            &repo,
            awaken_time::system_clock().as_ref(),
        )
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
        create_managed_credential(
            new_child,
            &secrets,
            &repo,
            awaken_time::system_clock().as_ref()
        )
        .await,
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
    let (_, before) = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
        "2026-09-17T00:00:01Z".into(),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
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

    // R1 additionally requires correlated structured diagnostics, without the
    // untrusted participant's secret-bearing error text. Future-local capture
    // keeps this assertion independent of parallel test subscribers.
    use tracing::instrument::WithSubscriber;
    let target = RecordingRolloutTarget::default();
    // Every closed port failure shares the durable Pending rule but retains a
    // distinct stable diagnostic; payload-bearing variants must be redacted.
    for (error, code) in [
        (
            ManagedCredentialAdoptionError::Unavailable("secret-diagnostic-canary".into()),
            "unavailable",
        ),
        (
            ManagedCredentialAdoptionError::InvalidEvent("secret-diagnostic-canary".into()),
            "invalid_event",
        ),
        (
            ManagedCredentialAdoptionError::IdentityCollision,
            "identity_collision",
        ),
        (ManagedCredentialAdoptionError::Unauthorized, "unauthorized"),
    ] {
        let captured = awaken_reliability_testkit::events::CapturedEvents::default();
        *target.failure.lock().unwrap() = Some(error);
        assert_eq!(
            reconcile_managed_credential_rollouts(&repo, &target)
                .with_subscriber(captured.clone())
                .await
                .unwrap(),
            0
        );
        let diagnostics = captured.snapshot();
        let delivery = diagnostics
            .iter()
            .find(|fields| fields.get("phase").is_some_and(|v| v == "delivery"))
            .unwrap();
        assert_eq!(delivery["rollout_id"], events[0].id);
        assert_eq!(delivery["failure"], code);
        assert!(!format!("{diagnostics:?}").contains("secret-diagnostic-canary"));
        assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
    }
    *target.failure.lock().unwrap() = None;
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
    let (source, before) = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
        "2026-09-17T00:00:01Z".into(),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
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

    let (source, _) = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let mut legacy = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    legacy.material_fence.format_version = 0;

    assert!(legacy.validate().is_ok());
    assert!(repo.begin_managed_mutation(legacy, 1_000).await.is_err());
}

/// Managed fresh-command admission uses the same decision rules as source-only
/// publication. Causes: a new material ref, forged Ready/takeover/foreign
/// writer, and begin reading before or exactly at expiry. R1 forged -> reject
/// before WAL/SecretStore; R2 exact expiry -> reject; R3 live original writer
/// -> one pending pair, never a second material owner.
#[tokio::test]
async fn managed_begin_rejects_every_forged_fresh_material_owner_shape() {
    let repo = InMemoryCredentialRepo::new();
    let clock = awaken_lease_testkit::ManualClock::new(1_000);
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let canonical = PendingManagedCredentialMutation::create(
        source,
        child,
        credential_material_now_unix_ms(&clock).unwrap(),
    )
    .unwrap();
    let mut forged_ready = canonical.clone();
    forged_ready.material_fence.phase = CredentialMaterialMutationPhase::Ready;
    let mut forged_epoch = canonical.clone();
    forged_epoch.material_fence.writer_epoch = forged_epoch
        .material_fence
        .writer_epoch
        .and_then(awaken_lease_contract::fence::Epoch::next);
    let mut forged_owner = canonical.clone();
    forged_owner.material_fence.writer_token = Some("foreign-owner".into()); // awaken-allow: secret -- writer identity, not material

    for forged in [forged_ready, forged_epoch, forged_owner] {
        assert!(matches!(
            repo.begin_managed_mutation(forged, credential_material_now_unix_ms(&clock).unwrap())
                .await,
            Err(CredentialError::MutationConflict(_))
        ));
    }
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    let expires_at = canonical.material_fence.writer_lease_expires_at_unix_ms;
    clock.set(expires_at);
    assert!(
        matches!(
            repo.begin_managed_mutation(
                canonical.clone(),
                credential_material_now_unix_ms(&clock).unwrap(),
            )
            .await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R2 exact expiry"
    );
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "R2"
    );
    clock.set(expires_at - 1);
    assert!(
        repo.begin_managed_mutation(canonical, credential_material_now_unix_ms(&clock).unwrap())
            .await
            .unwrap(),
        "R3"
    );
}

#[tokio::test]
async fn injected_recovery_clock_keeps_live_managed_writer_then_aborts_at_expiry() {
    // Causes: C1 one unpublished Managed Source/child Writing intent is durable;
    // C2 the injected clock is before or exactly at the writer deadline.
    // Effects: E1 live owner retains the pending pair and publishes nothing;
    // E2 the expired owner is fenced for abort and the pair remains invisible.
    // Decision rules: R1 C1+C2(before) -> E1; R2 C1+C2(exact) -> E2.
    let repo = InMemoryCredentialRepo::new();
    let secrets = InMemorySecretStore::new();
    let (pending, _) =
        prepare_managed_credential_create(managed_command("https://mcp.example.com"), 1_000)
            .unwrap();
    assert!(
        repo.begin_managed_mutation(pending.clone(), 1_000)
            .await
            .unwrap()
    );
    let expires_at = pending.material_fence.writer_lease_expires_at_unix_ms;
    let clock = awaken_lease_testkit::ManualClock::new(expires_at - 1);

    assert_eq!(
        recover_managed_credential_mutations(&secrets, &repo, &clock)
            .await
            .unwrap(),
        0,
        "R1"
    );
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap().len(),
        1,
        "R1/E1"
    );
    assert!(repo.get(&pending.after_source.id).await.is_err(), "R1/E1");

    clock.set(expires_at);
    assert_eq!(
        recover_managed_credential_mutations(&secrets, &repo, &clock)
            .await
            .unwrap(),
        1,
        "R2"
    );
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "R2/E2"
    );
    assert!(repo.get(&pending.after_source.id).await.is_err(), "R2/E2");
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let writing = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let ready = PendingManagedCredentialMutation::create(source, child, 1_000)
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let mut current =
        PendingManagedCredentialMutation::create(source, child.clone(), 1_000).unwrap();
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
    assert!(legacy.material_fence.writer_token.is_none());
    assert_eq!(legacy.material_fence.writer_epoch, None);
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    let recovery_now = pending.material_fence.writer_lease_expires_at_unix_ms + 1;

    let writer_repo = Arc::clone(&repo);
    let writer_secrets = Arc::clone(&secrets);
    let writer_clock = awaken_lease_testkit::ManualClock::new(1_000);
    let writer = tokio::spawn(async move {
        execute_managed_mutation(
            pending,
            vec![(original_reference, secret.unwrap())],
            writer_secrets.as_ref(),
            writer_repo.as_ref(),
            &writer_clock,
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

    let error = create_managed_credential(
        managed_command("file:///secret"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
    create_managed_credential(
        managed_command("https://mcp.example.com"),
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let original_inventory = secrets.inventory().await.unwrap();
    let mut replay = managed_command("https://mcp.example.com");
    replay.primary_material_ref = Some(crate::SecretRef("sec:losing-command".into()));
    replay.source.secret = Some(RedactedString::new("replacement"));

    let error = create_managed_credential(
        replay,
        &secrets,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let complete =
        PendingManagedCredentialMutation::create(source.clone(), child.clone(), 1_000).unwrap();
    source = complete.after_source.clone();
    let complete_lease = complete.material_fence.writer_lease_expires_at_unix_ms;
    repo.begin_managed_mutation(complete.clone(), 1_000)
        .await
        .unwrap();
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
        PendingManagedCredentialMutation::create(source.clone(), partial_child.clone(), 1_000)
            .unwrap();
    source = partial.after_source.clone();
    let partial_lease = partial.material_fence.writer_lease_expires_at_unix_ms;
    repo.begin_managed_mutation(partial, 1_000).await.unwrap();
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
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        };
        let pending =
            PendingManagedCredentialMutation::create(source.clone(), child, 1_000).unwrap();
        let reference = pending.after_source.material_ref.clone().unwrap();
        repo.begin_managed_mutation(pending.clone(), 1_000)
            .await
            .unwrap();
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
    // C1 legacy epoch missing/zero; C2 writer token/deadline absent. E1 decode
    // is an expired unowned envelope (not a grant); E2 takeover starts at epoch
    // two even with a reused attempt namespace; E3 neither Ready nor publish
    // is allowed. This table preserves persisted compatibility without giving
    // missing identity a dispatch-style grant.
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    let wire = serde_json::to_value(pending).unwrap();
    for epoch in [None, Some(0)] {
        let mut legacy = wire.clone();
        let object = legacy.as_object_mut().unwrap();
        object.remove("writer_token");
        object.remove("writer_epoch");
        object.remove("writer_lease_expires_at_unix_ms");
        if let Some(epoch) = epoch {
            object.insert("writer_epoch".into(), epoch.into());
        }

        let decoded: PendingManagedCredentialMutation = serde_json::from_value(legacy).unwrap();
        assert!(
            decoded.material_fence.writer_token.is_none(),
            "E1 {epoch:?}"
        );
        assert_eq!(decoded.material_fence.writer_epoch, None, "E1 {epoch:?}");
        assert_eq!(
            decoded.material_fence.writer_lease_expires_at_unix_ms, 0,
            "E1 {epoch:?}"
        );
        let claimed = decoded.claim_after_expiry(0, 1).unwrap().unwrap();
        assert_eq!(
            claimed.material_fence.writer_epoch,
            awaken_lease_contract::fence::Epoch::from_persisted(2),
            "E2 {epoch:?}"
        );
        claimed.validate().unwrap();
        assert!(claimed.with_material_ready().is_err(), "E3 {epoch:?}");
        assert!(claimed.with_material_reclaiming().is_err(), "E3 {epoch:?}");
    }
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();
    repo.begin_managed_mutation(pending.clone(), 1_000)
        .await
        .unwrap();
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
        1_000,
    )
    .unwrap();
    assert!(
        !repo.begin_managed_mutation(competing, 1_000).await.unwrap(),
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
        ..Default::default()
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();
    repo.begin_managed_mutation(pending.clone(), 1_000)
        .await
        .unwrap();
    store.put(&reference, secret.unwrap()).await.unwrap();

    assert!(
        abort_managed_mutation_before_cleanup(
            &pending,
            CredentialMaterialRecoverySignal::OwnerWriteFailed,
            &store,
            &repo,
        )
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
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let pending = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
    let reference = pending.after_source.material_ref.clone().unwrap();

    let error = execute_managed_mutation(
        pending,
        vec![(reference.clone(), secret.unwrap())],
        &store,
        &repo,
        &awaken_lease_testkit::ManualClock::new(1_000),
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
        ..Default::default()
    };
    let repo = InMemoryCredentialRepo::new();
    repo.insert_vault("ws", managed_vault()).await.unwrap();
    let (before_source, before_child) = create_managed_credential(
        managed_command("https://mcp.example.com"),
        &store,
        &repo,
        awaken_time::system_clock().as_ref(),
    )
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
        "2026-09-17T00:00:01Z".into(),
        &store,
        &repo,
        awaken_time::system_clock().as_ref(),
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

#[tokio::test]
async fn candidate_readiness_preserves_unknown_after_a_missing_reference() {
    // Causes: two new references, each present(0), missing(1), unavailable(2).
    // R1 both present => publish candidate; R2 missing without any unavailable
    // read => abort candidate; R3 any unavailable => retain WAL for retry, even
    // after an earlier missing reference. R4 stop only at an unavailable read.
    // The existing targeted-read fault fixture supplies observations; the
    // independent 3x3 oracle covers the interaction and both encounter orders.
    for primary in 0..3 {
        for auxiliary in 0..3 {
            let refs = [
                crate::SecretRef("candidate-primary".into()),
                crate::SecretRef("candidate-auxiliary".into()),
            ];
            let outcomes = [primary, auxiliary];
            let store = PoisonGetStore {
                inner: InMemorySecretStore::new(),
                poison: std::sync::Mutex::new(
                    outcomes
                        .iter()
                        .position(|v| *v == 2)
                        .map(|index| refs[index].clone()),
                ),
                reads: AtomicUsize::new(0),
                deletes: AtomicUsize::new(0),
            };
            for (reference, outcome) in refs.iter().zip(outcomes) {
                if outcome == 0 {
                    store
                        .inner
                        .put(reference, RedactedString::new("fixture"))
                        .await
                        .unwrap();
                }
            }
            let mut after = super::super::publication_proofs::source();
            after.material_ref = Some(refs[0].clone());
            after
                .auxiliary_material_refs
                .insert("k".into(), refs[1].clone());
            let result = cleanup::candidate_material_complete(None, &after, &store).await;
            if primary == 2 || auxiliary == 2 {
                assert!(matches!(result, Err(CredentialError::Storage(_))), "R3");
            } else {
                assert_eq!(result.unwrap(), primary == 0 && auxiliary == 0, "R1-R2");
            }
            assert_eq!(
                store.reads.load(Ordering::SeqCst),
                if primary == 2 { 1 } else { 2 },
                "R4"
            );
            assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
        }
    }
}
