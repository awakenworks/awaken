//! Exact writer-takeover conformance shared by Source and Managed stores.

use super::*;

/// Shared takeover table for Source/Managed x InMemory/SQLite/PostgreSQL.
/// T1 absent WAL or live/non-Writing snapshot => no claim; T2 invalid successor
/// lease => error without a WAL change; T3 same key but stale token/epoch/lease/payload
/// => no claim; T4 two expired-snapshot callers => one successor; T5 stale owner
/// cannot Ready/publish/abort/claim; T6 repeated takeover advances epoch, retains every
/// other envelope field, and permits only durable abort/cleanup. No material IO
/// is performed: these are repository CAS witnesses, not SecretStore recovery.
pub(super) async fn expired_writer_claims_are_exact_snapshot_cas(
    repo: &dyn ManagedCredentialRepository,
) {
    // Both ports have distinct envelopes; the same rule body prevents a second
    // assertion table drifting between source-only and Managed publication.
    macro_rules! check_claim {
        ($writing:expr, $begin:ident, $claim:ident, $ready:ident, $publish:ident, $abort:ident,
         $complete:ident, $pending:ident, $changed_payload:expr) => {{
            let writing = $writing;
            let now = writing.material_fence.writer_lease_expires_at_unix_ms;
            let deadline = now.checked_add(10_000).unwrap();
            assert!(
                repo.$claim(&writing, now, deadline)
                    .await
                    .unwrap()
                    .is_none(),
                "T1 absent"
            );
            assert!(repo.$begin(writing.clone(), 1_000).await.unwrap());
            assert!(
                !repo.$begin(writing.clone(), 1_000).await.unwrap(),
                "begin replay retains the sole writer"
            );
            assert!(
                repo.$claim(&writing, now - 1, deadline)
                    .await
                    .unwrap()
                    .is_none(),
                "T1 live"
            );
            assert!(
                repo.$claim(&writing.with_material_ready().unwrap(), now, deadline)
                    .await
                    .unwrap()
                    .is_none(),
                "T1 non-Writing"
            );
            assert!(
                matches!(
                    repo.$claim(&writing, now, now).await,
                    Err(CredentialError::MutationConflict(_))
                ),
                "T2"
            );
            for coordinate in 0..4 {
                let mut stale = writing.clone();
                match coordinate {
                    0 => stale.material_fence.writer_token = Some("x".repeat(33)),
                    1 => stale.material_fence.writer_epoch = epoch(2),
                    2 => stale.material_fence.writer_lease_expires_at_unix_ms -= 1,
                    _ => ($changed_payload)(&mut stale),
                }
                stale.validate().unwrap(); // T3 must reach CAS, not fail command validation.
                assert!(
                    repo.$claim(&stale, now, deadline).await.unwrap().is_none(),
                    "T3"
                );
            }
            assert_eq!(
                repo.$pending().await.unwrap(),
                vec![writing.clone()],
                "T1-T3 preserve WAL"
            );
            let (left, right) = tokio::join!(
                repo.$claim(&writing, now, deadline),
                repo.$claim(&writing, now, deadline)
            );
            let claims = [left.unwrap(), right.unwrap()];
            assert_eq!(
                claims.iter().filter(|claim| claim.is_some()).count(),
                1,
                "T4"
            );
            let mut claimed = claims.into_iter().flatten().next().unwrap();
            for (expected_epoch, claim_time) in [(epoch(2), now), (epoch(3), deadline)] {
                if expected_epoch == epoch(3) {
                    claimed = repo
                        .$claim(&claimed, claim_time, deadline + 10_000)
                        .await
                        .unwrap()
                        .unwrap();
                }
                assert_eq!(claimed.material_fence.writer_epoch, expected_epoch, "T4/T6");
                assert_ne!(
                    claimed.material_fence.writer_token, writing.material_fence.writer_token,
                    "UUID wrapper smoke witness; Kani also allows collisions"
                );
                let token = claimed.material_fence.writer_token.as_deref().unwrap();
                assert_eq!(token.len(), 32);
                assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
                assert_eq!(
                    claimed.material_fence.writer_lease_expires_at_unix_ms,
                    claim_time + 10_000
                );
                let mut normalized = claimed.clone();
                normalized.material_fence.writer_token =
                    writing.material_fence.writer_token.clone();
                normalized.material_fence.writer_epoch = writing.material_fence.writer_epoch;
                normalized.material_fence.writer_lease_expires_at_unix_ms = now;
                assert_eq!(normalized, writing, "T6 complete envelope preservation");
                assert_eq!(repo.$pending().await.unwrap(), vec![claimed.clone()]);
                assert!(
                    matches!(
                        repo.$ready(&writing).await,
                        Err(CredentialError::MutationConflict(_))
                    ),
                    "T5 stale Ready"
                );
                assert!(
                    matches!(
                        repo.$abort(&writing).await,
                        Err(CredentialError::MutationConflict(_))
                    ),
                    "T5 stale abort"
                );
                assert!(
                    repo.$claim(&writing, deadline, deadline + 10_000)
                        .await
                        .unwrap()
                        .is_none(),
                    "T5 stale claim"
                );
                assert!(
                    repo.$publish(&writing.with_material_ready().unwrap())
                        .await
                        .is_err(),
                    "T5 stale Ready cannot publish"
                );
                assert!(
                    repo.$ready(&claimed).await.is_err(),
                    "T6 successor is abort-only"
                );
            }
            let aborted = repo.$abort(&claimed).await.unwrap();
            assert_eq!(
                aborted.material_fence.phase,
                CredentialMaterialMutationPhase::ReclaimingAbort
            );
            assert!(
                repo.$claim(&aborted, deadline + 10_000, deadline + 20_000)
                    .await
                    .unwrap()
                    .is_none(),
                "T1 terminal"
            );
            assert!(
                repo.$claim(&claimed, deadline + 10_000, deadline + 20_000)
                    .await
                    .unwrap()
                    .is_none(),
                "T3 phase-only durable drift fences the otherwise exact expired snapshot"
            );
            repo.$complete(&aborted).await.unwrap();
            assert!(repo.$pending().await.unwrap().is_empty());
        }};
    }
    let mut source = source("cred:claim", "ws");
    source.provider_id = None;
    source.env_key = None;
    source.material_ref = Some(SecretRef("sec:claim".into()));
    check_claim!(
        CredentialMutationIntent::prepare(None, source.clone(), 1_000).unwrap(),
        begin_mutation,
        claim_expired_mutation,
        mark_mutation_ready,
        apply_mutation,
        abort_mutation,
        complete_mutation,
        pending_mutations,
        |pending: &mut CredentialMutationIntent| pending.after.version += 1
    );
    let child = ManagedVaultCredential {
        id: "child-claim".into(),
        vault_id: "vault-claim".into(),
        workspace_id: "ws".into(),
        source_id: source.id.clone(),
        auth: ManagedCredentialAuth::StaticBearer {
            mcp_server_url: "https://mcp.example.test".into(),
        },
        metadata: Default::default(),
        display_name: None,
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    check_claim!(
        PendingManagedCredentialMutation::create(source, child, 1_000).unwrap(),
        begin_managed_mutation,
        claim_expired_managed_mutation,
        mark_managed_mutation_ready,
        commit_managed_mutation,
        abort_managed_mutation,
        complete_managed_mutation,
        pending_managed_mutations,
        |pending: &mut PendingManagedCredentialMutation| pending.after_credential.display_name =
            Some("changed".into())
    );
}
