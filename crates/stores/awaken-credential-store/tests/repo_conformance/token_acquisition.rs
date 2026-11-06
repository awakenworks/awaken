//! One acquisition/revocation/recovery decision table for every repository.
use super::*;
use awaken_credential_contract::IssuedCredentialTokens;
use awaken_credential_vault::repo::{
    CredentialAcquisitionPhase as Phase, CredentialMaterialPatch, CredentialRetirement,
    TOKEN_ACQUISITION_STATE_SLOT, TokenPublicationState, claim_token_publication,
    publish_acquired_tokens, revoke_credential_exact, rotate_credential_materials_exact,
    token_publication_state,
};
use awaken_time::{WallClock, WallClockExt as _};
use std::collections::BTreeMap;

struct Clock(u64);
impl WallClock for Clock {
    fn now(&self) -> awaken_time::UtcTime {
        awaken_time::UtcTime::from_epoch_ms(self.0)
    }
}
fn tokens() -> IssuedCredentialTokens {
    IssuedCredentialTokens {
        additional_fields: Default::default(),
        access_token: RedactedString::new("issued-access"),
        refresh_token: Some(RedactedString::new("issued-refresh")),
        access_expires_at_unix_ms: Some(500_000),
        refresh_expires_at_unix_ms: Some(5_000_000),
    }
}
async fn create(
    repo: &dyn CredentialRepo,
    store: &dyn SecretStore,
    clock: &Clock,
) -> CredentialSource {
    awaken_credential_vault::repo::enter_credential_with_materials(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: "acquisition".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new("original-access")),
            oauth_command: None,
        },
        BTreeMap::from([(
            awaken_credential_vault::OAUTH_REFRESH_TOKEN_SLOT.into(),
            RedactedString::new("original-refresh"),
        )]),
        store,
        repo,
        clock,
    )
    .await
    .unwrap()
}

pub(super) async fn acquisition_publication_and_preemption_conform(repo: &dyn CredentialRepo) {
    // Causes: issuer phase P; explicit operation O (rotate/revoke); complete
    // sealed pair M; source successor S; writer lease L. Decision rules:
    // A1 same predecessor, different candidate -> no second redemption claim;
    // A2 P=Dispatched + !M + !L -> retain Unknown (never blindly redeem again);
    // A3 O + exact predecessor -> supersede claim, publish O, close candidate;
    // A4 stale phase/Ready/publish after A3 -> deny, no source resurrection;
    // A5 durable Ready+!L+exact predecessor -> recover that generation;
    // complete Writing bytes alone do not acquire Ready authority;
    // A6 cross-id + changed predecessor -> abort candidate, preserve successor.
    // The shared suite runs this table against Memory, SQLite and PostgreSQL.
    let clock = Clock(1000);
    let store = InMemorySecretStore::new();
    for revoke in [false, true] {
        let source = create(repo, &store, &clock).await;
        let intent = claim_token_publication(&source, None, "profile".into(), true, repo, &clock)
            .await
            .unwrap();
        assert!(
            claim_token_publication(
                &source,
                Some(CredentialSourceId(format!("other-{}", source.id.0))),
                "profile".into(),
                true,
                repo,
                &clock
            )
            .await
            .is_err(),
            "A1"
        );
        let dispatched = repo
            .transition_acquisition(&intent, Phase::Dispatched, clock.now_ms())
            .await
            .unwrap();
        assert!(
            repo.transition_acquisition(&intent, Phase::Dispatched, clock.now_ms())
                .await
                .is_err(),
            "exact snapshot CAS"
        );
        assert_eq!(
            repo.pending_mutation(&source.id).await.unwrap(),
            Some(dispatched.clone())
        );
        let expired = Clock(dispatched.material_fence.writer_lease_expires_at_unix_ms);
        recover_credential_mutations(&store, repo, &expired)
            .await
            .unwrap();
        let unknown = repo.pending_mutation(&source.id).await.unwrap().unwrap();
        assert_eq!(
            unknown.acquisition.as_ref().unwrap().phase,
            Phase::OutcomeUnknown,
            "A2"
        );
        for reference in unknown.after.material_refs() {
            store
                .put(reference, RedactedString::new("unpublished"))
                .await
                .unwrap();
        }
        let updated = if revoke {
            revoke_credential_exact(
                &source.id,
                source.version,
                CredentialRetirement::Archive,
                &store,
                repo,
                &expired,
            )
            .await
            .unwrap()
        } else {
            rotate_credential_materials_exact(
                &source.id,
                source.version,
                CredentialMaterialPatch {
                    primary: Some(RedactedString::new("user-new-access")),
                    auxiliary: BTreeMap::new(),
                    descriptor: None,
                },
                &store,
                repo,
                &expired,
            )
            .await
            .unwrap()
        };
        assert_eq!(updated.version, 2, "A3");
        assert!(repo.pending_mutation(&source.id).await.unwrap().is_none());
        for reference in unknown
            .after
            .material_refs()
            .filter(|r| !source.material_refs().any(|v| v == *r))
        {
            assert!(
                matches!(
                    store.get(reference).await,
                    Err(CredentialError::SecretNotFound(_))
                ),
                "A3 tracked candidate reclaimed"
            );
            assert!(
                store
                    .put(reference, RedactedString::new("delayed-writer"))
                    .await
                    .is_err(),
                "A3 delayed puts stay closed"
            );
        }
        assert!(repo.mark_mutation_ready(&dispatched).await.is_err(), "A4");
        assert!(
            publish_acquired_tokens(dispatched, tokens(), None, &store, repo, &expired)
                .await
                .is_err(),
            "A4"
        );
        assert_eq!(repo.get(&source.id).await.unwrap(), updated, "A4");
    }
    let source = create(repo, &store, &clock).await;
    let writing = claim_token_publication(&source, None, "profile".into(), true, repo, &clock)
        .await
        .unwrap();
    let dispatched = repo
        .transition_acquisition(&writing, Phase::Dispatched, clock.now_ms())
        .await
        .unwrap();
    let state = TokenPublicationState {
        version: 1,
        predecessor: Some(CredentialRef {
            id: source.id.0.clone(),
            revision: 1,
        }),
        successor: CredentialRef {
            id: source.id.0.clone(),
            revision: 2,
        },
        profile_fingerprint: "profile".into(),
        issued_at_unix_ms: 1000,
        access_expires_at_unix_ms: Some(500_000),
        refresh_expires_at_unix_ms: Some(5_000_000),
    };
    for (slot, reference) in &dispatched.after.auxiliary_material_refs {
        let value = if slot == TOKEN_ACQUISITION_STATE_SLOT {
            serde_json::to_string(&state).unwrap()
        } else {
            "issued-refresh".into()
        };
        store
            .put(reference, RedactedString::new(value))
            .await
            .unwrap();
    }
    store
        .put(
            dispatched.after.material_ref.as_ref().unwrap(),
            RedactedString::new("issued-access"),
        )
        .await
        .unwrap();
    // The original writer must commit Ready BEFORE its simulated crash.
    let ready = repo.mark_mutation_ready(&dispatched).await.unwrap();
    let expired = Clock(dispatched.material_fence.writer_lease_expires_at_unix_ms);
    recover_credential_mutations(&store, repo, &expired)
        .await
        .unwrap();
    let recovered = repo.get(&source.id).await.unwrap();
    assert_eq!(recovered, ready.after, "A5");
    assert_eq!(
        token_publication_state(&recovered, &store).await.unwrap(),
        Some(state)
    );
    assert!(repo.pending_mutation(&source.id).await.unwrap().is_none());

    let source = create(repo, &store, &clock).await;
    let writing = claim_token_publication(
        &source,
        Some(CredentialSourceId(format!("candidate-{}", source.id.0))),
        "profile".into(),
        true,
        repo,
        &clock,
    )
    .await
    .unwrap();
    let dispatched = repo
        .transition_acquisition(&writing, Phase::Dispatched, clock.now_ms())
        .await
        .unwrap();
    let retired = revoke_credential_exact(
        &source.id,
        1,
        CredentialRetirement::Archive,
        &store,
        repo,
        &clock,
    )
    .await
    .unwrap();
    let expired = Clock(dispatched.material_fence.writer_lease_expires_at_unix_ms);
    recover_credential_mutations(&store, repo, &expired)
        .await
        .unwrap();
    assert!(
        repo.pending_mutation(&dispatched.after.id)
            .await
            .unwrap()
            .is_none(),
        "A6"
    );
    assert_eq!(repo.get(&source.id).await.unwrap(), retired);
    assert!(repo.get(&dispatched.after.id).await.is_err());

    // A7 authority was read before a concurrent exact retirement, but the
    // dispatch marker has not committed. Its CAS must also observe predecessor
    // truth in the SAME storage transaction, so no issuer may be called. A8
    // recording confirmed-unsent remains possible after retirement for cleanup.
    let source = create(repo, &store, &clock).await;
    let candidate = CredentialSourceId(format!("dispatch-fence-{}", source.id.0));
    let prepared = claim_token_publication(
        &source,
        Some(candidate.clone()),
        "profile".into(),
        true,
        repo,
        &clock,
    )
    .await
    .unwrap();
    revoke_credential_exact(
        &source.id,
        1,
        CredentialRetirement::Archive,
        &store,
        repo,
        &clock,
    )
    .await
    .unwrap();
    assert!(
        repo.transition_acquisition(&prepared, Phase::Dispatched, clock.now_ms())
            .await
            .is_err(),
        "A7"
    );
    assert_eq!(
        repo.pending_mutation(&candidate).await.unwrap(),
        Some(prepared.clone())
    );
    let unsent = repo
        .transition_acquisition(&prepared, Phase::ConfirmedUnsent, clock.now_ms())
        .await
        .unwrap();
    awaken_credential_vault::repo::abandon_token_publication(&unsent, &store, repo)
        .await
        .unwrap();
    assert!(
        repo.pending_mutation(&candidate).await.unwrap().is_none(),
        "A8"
    );
}
