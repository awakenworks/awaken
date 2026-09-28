// @formal evidence credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
// @formal evidence credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
// @formal evidence credential.managed_creation_publishes_source_and_child_atomically
// @formal evidence credential.managed_material_admission_predicate_requires_owner_and_namespace_match
// @formal evidence credential.restart_recovery_compensates_unpublished_material
#![cfg(all(feature = "sqlite", feature = "sealed-aead"))]

use std::collections::BTreeMap;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_store::sqlite::SqliteCredentialRepo;
use awaken_credential_vault::catalog::{
    ManagedCredentialAuth, ManagedCredentialLifecycle, ManagedVault, ManagedVaultCredential,
    ManagedVaultRepo,
};
use awaken_credential_vault::repo::{
    CredentialRepo, ManagedCredentialRepository, PendingManagedCredentialMutation,
    recover_managed_credential_mutations,
};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, SecretRef, SecretStore,
};
use awaken_reliability_testkit::CrashProcess;

const CHILD_MODE: &str = "AWAKEN_MANAGED_CREDENTIAL_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_MANAGED_CREDENTIAL_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_MANAGED_CREDENTIAL_CRASH_MARKER";
const KEY: [u8; 32] = [23; 32];

fn source() -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId("cred:ws:managed-process-crash".into()),
        replacement_of: None,
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: None,
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(SecretRef("sec:cred:ws:managed-process-crash".into())),
        auxiliary_material_refs: BTreeMap::new(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

fn child() -> ManagedVaultCredential {
    ManagedVaultCredential {
        id: "credential-process-crash".into(),
        vault_id: "vault-process-crash".into(),
        workspace_id: "ws".into(),
        source_id: source().id,
        auth: ManagedCredentialAuth::StaticBearer {
            mcp_server_url: "https://mcp.example.com".into(),
        },
        metadata: BTreeMap::new(),
        display_name: None,
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    }
}

#[path = "support/crash_secrets.rs"]
mod crash_secrets;
use crash_secrets::{CrashSecrets, checkpoint};

#[tokio::test]
async fn writing_material_survives_kill_but_expired_owner_is_aborted() {
    if let Ok(phase) = std::env::var(CHILD_MODE) {
        let db = std::env::var(DB_PATH).unwrap();
        let marker = std::env::var(MARKER_PATH).unwrap();
        let (repo, secrets) = crash_secrets::stores(&db, &KEY);
        repo.insert_vault(
            "ws",
            ManagedVault {
                id: "vault-process-crash".into(),
                workspace_id: "ws".into(),
                display_name: "Crash".into(),
                metadata: BTreeMap::new(),
                created_at: "2026-09-17T00:00:00Z".into(),
                updated_at: "2026-09-17T00:00:00Z".into(),
                archived_at: None,
                deletion: None,
                revision: 1,
            },
        )
        .await
        .unwrap();
        let pending = PendingManagedCredentialMutation::create(source(), child()).unwrap();
        let material_ref = pending.after_source.material_ref.clone().unwrap();
        repo.begin_managed_mutation(pending.clone()).await.unwrap();
        secrets
            .put(&material_ref, RedactedString::new("process-crash-secret"))
            .await
            .unwrap();
        if phase == "writing" {
            checkpoint(&marker).await;
        }
        repo.abort_managed_mutation(&pending).await.unwrap();
        let faults = CrashSecrets {
            inner: &secrets,
            phase: &phase,
            marker: &marker,
        };
        assert_eq!(
            recover_managed_credential_mutations(&faults, &repo)
                .await
                .unwrap(),
            1
        );
        assert_eq!(phase, "retired");
        checkpoint(&marker).await;
    }

    // C1 writing/before-delete => material present; C2 deleted or absence
    // observed => material absent but WAL retained; C3 retired => neither.
    // Cross actual process kill with separate metadata/material databases.
    // Recovery must preserve the unpublished projection and converge exactly
    // once; only a Writing prefix needs lease takeover before compensation.
    for phase in crash_secrets::PHASES {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("managed-credential.db");
        let marker = dir.path().join("material-ready.marker");
        let status = CrashProcess::new(
            "writing_material_survives_kill_but_expired_owner_is_aborted",
            &marker,
        )
        .env(CHILD_MODE, phase)
        .env(DB_PATH, &db)
        .env(MARKER_PATH, &marker)
        .run()
        .expect("child reaches the material-write boundary and is killed");
        assert!(!status.success());

        let (repo, secrets) = crash_secrets::stores(db.to_str().unwrap(), &KEY);
        assert!(
            matches!(
                repo.get(&source().id).await,
                Err(awaken_credential_vault::CredentialError::SourceNotFound(_))
            ),
            "unpublished source stays absent at every crash prefix"
        );
        assert!(
            repo.get_vault_credential("ws", &child().id)
                .await
                .unwrap()
                .is_none(),
            "unpublished child stays absent at every crash prefix"
        );
        let pending = repo.pending_managed_mutations().await.unwrap();
        assert_eq!(
            pending.len(),
            usize::from(phase != "retired"),
            "{phase} WAL"
        );
        if phase == "retired" {
            assert!(secrets.inventory().await.unwrap().is_empty());
            assert_eq!(
                recover_managed_credential_mutations(&secrets, &repo)
                    .await
                    .unwrap(),
                0
            );
            continue;
        }
        let attempted_ref = pending[0].after_source.material_ref.clone().unwrap();
        assert_eq!(
            secrets.get(&attempted_ref).await.is_ok(),
            matches!(phase, "writing" | "before-delete"),
            "{phase} material"
        );
        if phase == "writing" {
            let claim_now = pending[0].material_fence.writer_lease_expires_at_unix_ms + 1;
            let claimed = repo
                .claim_expired_managed_mutation(&pending[0], claim_now, claim_now + 120_000)
                .await
                .unwrap()
                .expect("expired crash writer claim");
            repo.abort_managed_mutation(&claimed).await.unwrap();
        }
        assert_eq!(
            recover_managed_credential_mutations(&secrets, &repo)
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            repo.get(&source().id).await,
            Err(awaken_credential_vault::CredentialError::SourceNotFound(_))
        ));
        assert!(secrets.get(&attempted_ref).await.is_err());
        assert_eq!(
            repo.get_vault_credential("ws", &child().id).await.unwrap(),
            None
        );
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        repo.begin_managed_mutation(
            PendingManagedCredentialMutation::create(source(), child()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(repo.pending_managed_mutations().await.unwrap().len(), 1);
        assert_eq!(
            recover_managed_credential_mutations(&secrets, &repo)
                .await
                .unwrap(),
            0,
            "the fresh retry remains live Writing and cannot be stolen"
        );
    }
}

#[tokio::test]
async fn sqlite_writing_lease_fences_live_and_stale_owners() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lease-fence.db");
    let repo = SqliteCredentialRepo::open(db.to_str().unwrap()).unwrap();
    repo.insert_vault(
        "ws",
        ManagedVault {
            id: "vault-process-crash".into(),
            workspace_id: "ws".into(),
            display_name: "Lease".into(),
            metadata: BTreeMap::new(),
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            archived_at: None,
            deletion: None,
            revision: 1,
        },
    )
    .await
    .unwrap();
    let stale = PendingManagedCredentialMutation::create(source(), child()).unwrap();
    let lease_expires_at = stale.material_fence.writer_lease_expires_at_unix_ms;
    assert!(repo.begin_managed_mutation(stale.clone()).await.unwrap());
    assert!(!repo.begin_managed_mutation(stale.clone()).await.unwrap());

    assert!(
        repo.claim_expired_managed_mutation(&stale, lease_expires_at - 1, lease_expires_at + 100)
            .await
            .unwrap()
            .is_none(),
        "a live writer lease must not be stolen"
    );
    let claimed = repo
        .claim_expired_managed_mutation(&stale, lease_expires_at, lease_expires_at + 100)
        .await
        .unwrap()
        .expect("expired writer must be atomically fenced");
    assert_eq!(
        claimed.material_fence.writer_epoch,
        stale
            .material_fence
            .writer_epoch
            .and_then(awaken_lease_contract::fence::Epoch::next)
    );
    assert_ne!(
        claimed.material_fence.writer_token,
        stale.material_fence.writer_token
    );
    assert!(repo.mark_managed_mutation_ready(&stale).await.is_err());

    let stale_ready = stale.with_material_ready().unwrap();
    assert!(repo.commit_managed_mutation(&stale_ready).await.is_err());

    assert!(repo.mark_managed_mutation_ready(&claimed).await.is_err());
    let abort = repo.abort_managed_mutation(&claimed).await.unwrap();
    repo.complete_managed_mutation(&abort).await.unwrap();
}

#[tokio::test]
async fn published_material_retirement_survives_process_crash() {
    crash_secrets::published_retirement(
        "published_material_retirement_survives_process_crash",
        source(),
        Some(child()),
        &KEY,
    )
    .await;
}
