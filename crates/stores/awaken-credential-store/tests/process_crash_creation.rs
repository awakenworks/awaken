#![cfg(all(feature = "sqlite", feature = "sealed-aead"))]

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;

use awaken_credential_vault::repo::{
    CredentialMutationIntent, CredentialRepo, recover_credential_mutations,
};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, SecretRef, SecretStore,
};
use awaken_reliability_testkit::CrashProcess;

const CHILD_MODE: &str = "AWAKEN_CREDENTIAL_CRASH_CHILD";
const DB_PATH: &str = "AWAKEN_CREDENTIAL_CRASH_DB";
const MARKER_PATH: &str = "AWAKEN_CREDENTIAL_CRASH_MARKER";
const KEY: [u8; 32] = [19; 32];

fn source() -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId("cred:ws:process-crash".into()),
        replacement_of: None,
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: Some("anthropic".into()),
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: Some(SecretRef("sec:cred:ws:process-crash".into())),
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

#[path = "support/crash_secrets.rs"]
mod crash_secrets;
use crash_secrets::{CrashSecrets, checkpoint};

#[tokio::test]
async fn secret_write_survives_kill_and_is_compensated_from_the_intent() {
    // Cause/effect rule CR1: before=None, unpublished source, and a durable new
    // secret after process death => recovery deletes the unpublished secret,
    // completes the WAL record, and a second recovery is a no-op.
    if let Ok(phase) = std::env::var(CHILD_MODE) {
        let db = std::env::var(DB_PATH).unwrap();
        let marker = std::env::var(MARKER_PATH).unwrap();
        let (repo, secrets) = crash_secrets::stores(&db, &KEY);
        let intent = CredentialMutationIntent::prepare(None, source(), 1_000).unwrap();
        repo.begin_mutation(intent.clone(), 1_000).await.unwrap();
        secrets
            .put(
                intent.after.material_ref.as_ref().unwrap(),
                RedactedString::new("process-crash-secret"),
            )
            .await
            .unwrap();
        if phase == "writing" {
            checkpoint(&marker).await;
        }
        repo.abort_mutation(&intent).await.unwrap();
        let faults = CrashSecrets {
            inner: &secrets,
            phase: &phase,
            marker: &marker,
        };
        assert_eq!(
            recover_credential_mutations(
                &faults,
                &repo,
                awaken_lease_contract::system_clock().as_ref()
            )
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
        let db = dir.path().join("credential.db");
        let marker = dir.path().join("secret-written.marker");
        let status = CrashProcess::new(
            "secret_write_survives_kill_and_is_compensated_from_the_intent",
            &marker,
        )
        .env(CHILD_MODE, phase)
        .env(DB_PATH, &db)
        .env(MARKER_PATH, &marker)
        .run()
        .expect("child reaches the secret-write boundary and is killed");
        assert!(!status.success());

        let (repo, secrets) = crash_secrets::stores(db.to_str().unwrap(), &KEY);
        assert!(
            matches!(
                repo.get(&source().id).await,
                Err(awaken_credential_vault::CredentialError::SourceNotFound(_))
            ),
            "unpublished source stays absent at every crash prefix"
        );
        let pending = repo.pending_mutations().await.unwrap();
        assert_eq!(
            pending.len(),
            usize::from(phase != "retired"),
            "{phase} WAL"
        );
        if phase == "retired" {
            assert!(secrets.inventory().await.unwrap().is_empty());
            assert_eq!(
                recover_credential_mutations(
                    &secrets,
                    &repo,
                    awaken_lease_contract::system_clock().as_ref()
                )
                .await
                .unwrap(),
                0
            );
            continue;
        }
        let attempted_ref = pending[0].after.material_ref.clone().unwrap();
        assert_eq!(
            secrets.get(&attempted_ref).await.is_ok(),
            matches!(phase, "writing" | "before-delete"),
            "{phase} material"
        );
        if phase == "writing" {
            let claim_now = pending[0].material_fence.writer_lease_expires_at_unix_ms + 1;
            let claimed = repo
                .claim_expired_mutation(&pending[0], claim_now, claim_now + 120_000)
                .await
                .unwrap()
                .expect("expired crash writer claim");
            repo.abort_mutation(&claimed).await.unwrap();
        }
        assert_eq!(
            recover_credential_mutations(
                &secrets,
                &repo,
                awaken_lease_contract::system_clock().as_ref()
            )
            .await
            .unwrap(),
            1
        );
        assert!(repo.pending_mutations().await.unwrap().is_empty());
        assert!(secrets.get(&attempted_ref).await.is_err());
        assert_eq!(
            recover_credential_mutations(
                &secrets,
                &repo,
                awaken_lease_contract::system_clock().as_ref()
            )
            .await
            .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn published_material_retirement_survives_process_crash() {
    crash_secrets::published_retirement(
        "published_material_retirement_survives_process_crash",
        source(),
        None,
        &KEY,
    )
    .await;
}
