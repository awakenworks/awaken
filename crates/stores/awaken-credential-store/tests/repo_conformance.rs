// awaken-file-limit-ceiling: 3326 (legacy oversized test file: split, never grow)
//! Conformance suite for [`CredentialRepo`]: the same behavioral assertions run
//! against the in-memory repo and (feature `sqlite`) the sqlite repo, so both
//! backends keep identical semantics — workspace-scoped lists, upsert puts, and
//! the exact NotFound arms.
//!
//! Cause/effect design for the Credential Vault port adapters: C1 in-memory,
//! C2 SQLite, C3 Postgres, C4 mutation interrupted before publication, C5
//! mutation replayed, C6 credential rotation races a stale revision. E1
//! identical repository semantics, E2 durable intent is visible for
//! compensation, E3 replay is idempotent, E4 only the exact `before` revision
//! can publish. Rules: R1 C1|C2|C3 -> E1; R2 C2|C3+C4 -> E2;
//! R3 C2|C3+C4+C5 -> E3; R4 C1|C2|C3+C6 -> E4. Shared helpers keep the
//! behavior contract authoritative while concrete storage lives in this crate.

#[path = "support/cleanup.rs"]
mod cleanup;

use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{CredentialRef, CredentialSourceId};
#[cfg(feature = "postgres")]
use awaken_credential_vault::catalog::ManagedVaultRepo;
use awaken_credential_vault::catalog::{
    ManagedCredentialAuth, ManagedCredentialLifecycle, ManagedVault, ManagedVaultCredential,
};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
use awaken_credential_vault::repo::recover_managed_credential_mutations;
use awaken_credential_vault::repo::{
    CredentialMaterialMutationPhase, CredentialMutationIntent, CredentialRepo,
    InMemoryCredentialRepo, ManagedCredentialOperation, ManagedCredentialRepository,
    PendingManagedCredentialMutation, ensure_worker_local, recover_credential_mutations,
};
use awaken_credential_vault::{
    CredentialError, CredentialKind, CredentialPool, CredentialPoolId, CredentialPoolMember,
    CredentialSource, CredentialStatus, InMemorySecretStore, SecretRef, SecretStore,
    SelectionPolicy, WorkerLocalBinding,
};

#[derive(Default)]
struct DeleteCountingStore {
    inner: InMemorySecretStore,
    reads: AtomicUsize,
    deletes: AtomicUsize,
}

#[async_trait::async_trait]
impl SecretStore for DeleteCountingStore {
    async fn put(
        &self,
        reference: &SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(reference, secret).await
    }

    async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(reference).await
    }

    async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(reference).await
    }

    async fn inventory(&self) -> Result<Vec<SecretRef>, CredentialError> {
        self.inner.inventory().await
    }
}

fn source(id: &str, ws: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        replacement_of: None,
        workspace_id: ws.into(),
        kind: CredentialKind::Vault,
        descriptor: None,
        provider_id: Some("anthropic".into()),
        protocol_endpoint_id: None,
        env_key: Some("ANTHROPIC_API_KEY".into()),
        material_ref: None,
        auxiliary_material_refs: Default::default(),
        oauth_command: None,
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

fn replacement_source(before: &CredentialSource, id: &str, material_ref: &str) -> CredentialSource {
    let mut after = before.clone();
    after.id = CredentialSourceId(id.into());
    after.replacement_of = Some(CredentialRef {
        id: before.id.0.clone(),
        revision: u64::try_from(before.version).expect("positive predecessor revision"),
    });
    after.material_ref = Some(SecretRef(material_ref.into()));
    after.version = 1;
    after
}

fn pool(id: &str, ws: &str) -> CredentialPool {
    CredentialPool {
        id: CredentialPoolId(id.into()),
        workspace_id: ws.into(),
        members: vec![CredentialPoolMember {
            credential_source_id: CredentialSourceId("cred:a".into()),
            ordinal: 0,
            enabled: true,
            selection_weight: 0,
        }],
        policy: SelectionPolicy::FirstHealthy,
    }
}

async fn sources_round_trip_and_scope_by_workspace(repo: &dyn CredentialRepo) {
    repo.put(source("cred:a", "ws")).await.unwrap();
    repo.put(source("cred:b", "ws")).await.unwrap();
    repo.put(source("cred:c", "other")).await.unwrap();

    let got = repo
        .get(&CredentialSourceId("cred:a".into()))
        .await
        .unwrap();
    assert_eq!(got, source("cred:a", "ws"));
    assert_eq!(repo.list("ws").await.unwrap().len(), 2);
    assert_eq!(repo.list("other").await.unwrap().len(), 1);
    assert_eq!(repo.list("empty").await.unwrap().len(), 0);
}

/// Secret-free replacement provenance is part of the source row itself, so
/// every adapter must round-trip it without a second lineage table.
async fn replacement_lineage_round_trips(repo: &dyn CredentialRepo) {
    let mut before = source("cred:lineage-old", "ws");
    before.material_ref = Some(SecretRef("sec:lineage:old".into()));
    let after = replacement_source(&before, "cred:lineage-new", "sec:lineage:new");
    repo.put(before).await.unwrap();
    repo.put(after.clone()).await.unwrap();
    assert_eq!(repo.get(&after.id).await.unwrap(), after);
}

async fn pools_round_trip_and_scope_by_workspace(repo: &dyn CredentialRepo) {
    repo.put_pool(pool("pool:a", "ws")).await.unwrap();
    repo.put_pool(pool("pool:b", "ws")).await.unwrap();
    repo.put_pool(pool("pool:c", "other")).await.unwrap();

    let got = repo
        .get_pool(&CredentialPoolId("pool:a".into()))
        .await
        .unwrap();
    assert_eq!(got, pool("pool:a", "ws"));
    assert_eq!(repo.list_pools("ws").await.unwrap().len(), 2);
    assert_eq!(repo.list_pools("other").await.unwrap().len(), 1);
    assert_eq!(repo.list_pools("empty").await.unwrap().len(), 0);
}

async fn missing_rows_are_not_found(repo: &dyn CredentialRepo) {
    assert!(matches!(
        repo.get(&CredentialSourceId("cred:absent".into())).await,
        Err(CredentialError::SourceNotFound(id)) if id == "cred:absent"
    ));
    assert!(matches!(
        repo.get_pool(&CredentialPoolId("pool:absent".into())).await,
        Err(CredentialError::PoolNotFound(id)) if id == "pool:absent"
    ));
}

async fn put_is_upsert(repo: &dyn CredentialRepo) {
    repo.put(source("cred:a", "ws")).await.unwrap();
    let mut v2 = source("cred:a", "ws");
    v2.status = CredentialStatus::Disabled;
    v2.version = 2;
    repo.put(v2.clone()).await.unwrap();
    let got = repo
        .get(&CredentialSourceId("cred:a".into()))
        .await
        .unwrap();
    assert_eq!(got, v2);
    assert_eq!(repo.list("ws").await.unwrap().len(), 1);

    repo.put_pool(pool("pool:a", "ws")).await.unwrap();
    let mut p2 = pool("pool:a", "ws");
    p2.members.clear();
    repo.put_pool(p2.clone()).await.unwrap();
    let got = repo
        .get_pool(&CredentialPoolId("pool:a".into()))
        .await
        .unwrap();
    assert_eq!(got, p2);
    assert_eq!(repo.list_pools("ws").await.unwrap().len(), 1);
}

async fn worker_local_registration_is_atomic_idempotent_and_secret_free(repo: &dyn CredentialRepo) {
    // Cause graph: (workspace, driver, subject) -> canonical id -> put-if-absent
    // -> one durable non-secret source; a conflicting durable winner fails closed.
    //
    // Decision table:
    // D1 first ensure       -> version 1 source
    // D2 concurrent ensure  -> one identical durable source
    // D3 repeated ensure    -> identical source
    // D4 different subject  -> different source
    // D5 conflicting winner -> InvalidSource
    // D6 empty identity      -> InvalidSource, no row
    let binding = WorkerLocalBinding::new("acp:codex", "default");
    let first = ensure_worker_local(repo, "ws", binding.clone(), Some("openai".into()))
        .await
        .expect("D1");
    assert_eq!(first.kind, CredentialKind::WorkerLocal, "D1");
    assert_eq!(first.worker_local_binding.as_ref(), Some(&binding), "D1");
    assert!(
        first.material_ref.is_none() && first.env_key.is_none(),
        "D1"
    );

    let concurrent_binding = WorkerLocalBinding::new("acp:codex", "concurrent");
    let (left, right) = tokio::join!(
        ensure_worker_local(
            repo,
            "ws",
            concurrent_binding.clone(),
            Some("openai".into())
        ),
        ensure_worker_local(repo, "ws", concurrent_binding, Some("openai".into()))
    );
    assert_eq!(left.expect("D2 left"), right.expect("D2 right"), "D2");

    let repeated = ensure_worker_local(repo, "ws", binding, Some("openai".into()))
        .await
        .expect("D3");
    assert_eq!(repeated, first, "D3");

    let other = ensure_worker_local(
        repo,
        "ws",
        WorkerLocalBinding::new("acp:codex", "secondary"),
        Some("openai".into()),
    )
    .await
    .expect("D4");
    assert_ne!(other.id, first.id, "D4");

    let mut conflict = first.clone();
    conflict.kind = CredentialKind::Vault;
    conflict.worker_local_binding = None;
    repo.put(conflict).await.unwrap();
    assert!(
        matches!(
            ensure_worker_local(
                repo,
                "ws",
                WorkerLocalBinding::new("acp:codex", "default"),
                Some("openai".into())
            )
            .await,
            Err(CredentialError::InvalidSource(_))
        ),
        "D5"
    );

    assert!(
        matches!(
            ensure_worker_local(
                repo,
                " ",
                WorkerLocalBinding::new("acp:codex", "default"),
                None
            )
            .await,
            Err(CredentialError::InvalidSource(_))
        ),
        "D6"
    );
}

/// Cause/effect decision table for the mutation WAL:
/// R0 forged Ready, epoch, or owner => reject with zero WAL; R1 first canonical
/// intent => begin records the single-writer claim; R2 exact or
/// logically identical fresh-attempt replay => no second claim or physical ref;
/// R3 current==before and phase Ready => publish after while retaining
/// Reclaiming WAL; R4 exact Reclaiming replay is idempotent; R5 completion =>
/// WAL is removed idempotently. Cleanup sits between R4 and R5, so a process
/// crash cannot hide unreclaimed material.
async fn mutation_wal_publish_and_completion_are_idempotent(repo: &dyn CredentialRepo) {
    let mut source = source("cred:intent", "ws");
    source.material_ref = Some(SecretRef("sec:intent:r1:primary".into()));
    let intent = CredentialMutationIntent::prepare(None, source.clone()).unwrap();
    let follower = CredentialMutationIntent::prepare(None, source).unwrap();

    let mut forged_ready = intent.clone();
    forged_ready.material_fence.phase = CredentialMaterialMutationPhase::Ready;
    assert!(
        matches!(
            repo.begin_mutation(forged_ready).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R0 forged Ready"
    );
    let mut forged_epoch = intent.clone();
    forged_epoch.material_fence.writer_epoch = 2;
    assert!(
        matches!(
            repo.begin_mutation(forged_epoch).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R0 forged epoch"
    );
    let mut forged_owner = intent.clone();
    forged_owner.material_fence.writer_token = "forged-owner".into(); // awaken-allow: secret -- writer identity, not material
    assert!(
        matches!(
            repo.begin_mutation(forged_owner).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R0 forged owner"
    );
    assert!(repo.pending_mutations().await.unwrap().is_empty(), "R0");

    assert!(repo.begin_mutation(intent.clone()).await.unwrap(), "R1");
    assert!(!repo.begin_mutation(intent.clone()).await.unwrap(), "R2");
    assert_ne!(
        intent.material_fence.attempt_id(),
        follower.material_fence.attempt_id(),
        "R2 exercises a fresh random physical attempt"
    );
    assert!(!repo.begin_mutation(follower).await.unwrap(), "R2");
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        vec![intent.clone()]
    );
    assert!(matches!(
        repo.get(&intent.after.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));

    let ready = repo.mark_mutation_ready(&intent).await.unwrap();
    let reclaiming = repo.apply_mutation(&ready).await.unwrap();
    assert_eq!(repo.apply_mutation(&ready).await.unwrap(), reclaiming, "R4");
    assert_eq!(repo.get(&ready.after.id).await.unwrap(), ready.after);
    assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);

    repo.complete_mutation(&reclaiming).await.unwrap();
    repo.complete_mutation(&reclaiming).await.unwrap();
    assert_eq!(
        repo.get(&reclaiming.after.id).await.unwrap(),
        reclaiming.after
    );
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

/// Cause/effect rule R5A: exact unpublished truth must first become durable
/// ReclaimingAbort; only that terminal phase may remove the WAL, and repeating
/// completion is safe.
async fn completing_unpublished_mutation_is_idempotent(repo: &dyn CredentialRepo) {
    let source = source("cred:abort", "ws");
    let intent = CredentialMutationIntent::prepare(None, source.clone()).unwrap();
    repo.begin_mutation(intent.clone()).await.unwrap();
    let abort = repo.abort_mutation(&intent).await.unwrap();
    repo.complete_mutation(&abort).await.unwrap();
    repo.complete_mutation(&abort).await.unwrap();
    assert!(repo.pending_mutations().await.unwrap().is_empty());
    assert!(matches!(
        repo.get(&source.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));

    cleanup::source_cleanup_rechecks_durable_truth(repo).await;
}

/// Durable rotation FMECA/decision table. C1 current row equals the intent's
/// `before`; C2 apply is retried after publication; C3 a delayed writer carries
/// the retired `before` revision. Effects: E1 atomically install the higher row
/// while retaining the WAL; E2 idempotent replay; E3 conflict without changing
/// the committed row or its material reference. Rules R6 C1=>E1; R7 C1+C2=>E2;
/// R8 C3=>E3. The same rules run on in-memory, SQLite, and live Postgres.
async fn rotation_publication_is_revision_cas_and_idempotent(repo: &dyn CredentialRepo) {
    let mut before = source("cred:rotation-cas", "ws");
    before.material_ref = Some(SecretRef("sec:rotation:r1:primary".into()));
    repo.put(before.clone()).await.unwrap();

    let mut after = before.clone();
    after.version = 2;
    after.material_ref = Some(SecretRef("sec:rotation:r2:primary".into()));
    let exact = CredentialMutationIntent::prepare(Some(before.clone()), after).unwrap();
    assert!(
        repo.begin_mutation(exact.clone()).await.unwrap(),
        "R6 owner"
    );
    assert!(
        !repo.begin_mutation(exact.clone()).await.unwrap(),
        "R6 pending replay"
    );
    let ready = repo.mark_mutation_ready(&exact).await.unwrap();
    let reclaiming = repo.apply_mutation(&ready).await.unwrap();
    assert_eq!(repo.get(&ready.after.id).await.unwrap(), ready.after, "R6");
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        vec![reclaiming.clone()],
        "R6"
    );
    assert_eq!(repo.apply_mutation(&ready).await.unwrap(), reclaiming, "R7");
    repo.complete_mutation(&reclaiming).await.unwrap();

    let mut stale_before = source("cred:rotation-stale", "ws");
    stale_before.material_ref = Some(SecretRef("sec:rotation:stale:r1:primary".into()));
    repo.put(stale_before.clone()).await.unwrap();
    let mut stale_after = stale_before.clone();
    stale_after.version = 2;
    stale_after.material_ref = Some(SecretRef("sec:rotation:stale:primary".into()));
    let stale = CredentialMutationIntent::prepare(Some(stale_before.clone()), stale_after).unwrap();
    repo.begin_mutation(stale.clone()).await.unwrap();
    let stale_ready = repo.mark_mutation_ready(&stale).await.unwrap();
    let mut winner = stale_before;
    winner.version = 3;
    repo.put(winner.clone()).await.unwrap();
    assert!(
        matches!(
            repo.apply_mutation(&stale_ready).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R8"
    );
    assert_eq!(repo.get(&winner.id).await.unwrap(), winner, "R8");

    // R9-R11 are one full-Source decision table, not independent field-level
    // authorization rules. Cause axes: all thirteen non-key Source fields x
    // begin / Ready apply / committed replay. Exactly one field changes; the
    // endpoint case starts provider-scoped so it remains a valid domain row.
    // Effect: reject the stale complete image, preserving the current Source,
    // every pending WAL fact and an unrelated Source. Identity is the physical
    // lookup key and has its absent/wrong-key witness R12 below. The two extra
    // replacement rows check changed predecessor rejects first publication but
    // cannot invalidate an exact destination/WAL replay after publication. Repository put
    // constructs the competing snapshot; this does not claim that an ordinary
    // rotation command permits changing every tested field at the same version.
    fn drift(row: &mut CredentialSource, coordinate: u8) {
        match coordinate {
            0 => {
                row.replacement_of = Some(CredentialRef {
                    id: "cred:ancestor".into(),
                    revision: 1,
                })
            }
            1 => row.workspace_id.push_str("-other"),
            2 => row.kind = CredentialKind::Oauth,
            3 => {
                use awaken_credential_contract::{
                    CredentialDescriptor, CredentialMaterialDescriptor, CredentialPurpose,
                    CredentialTarget, CredentialTargetContract, CredentialUsage,
                };
                row.descriptor = Some(CredentialDescriptor::new(
                    "github",
                    CredentialMaterialDescriptor::secret("awaken.secret/v1"),
                    [CredentialTargetContract::new(
                        CredentialTarget::new(
                            CredentialPurpose::RepositoryTransport,
                            awaken_credential_contract::repository_transport_audience(
                                "https://github.com/awaken/example.git",
                            )
                            .unwrap(),
                        ),
                        CredentialUsage::HttpBasicAuth,
                    )],
                ));
            }
            4 => row.provider_id = Some("other-provider".into()),
            5 => row.protocol_endpoint_id = Some("other-endpoint".into()),
            6 => row.env_key = Some("OTHER_KEY".into()),
            7 => row.material_ref = Some(SecretRef("sec:other-current".into())),
            8 => {
                row.auxiliary_material_refs
                    .insert("other".into(), SecretRef("sec:other-slot".into()));
            }
            9 => row.oauth_command = Some(vec!["other-command".into()]),
            10 => {
                row.worker_local_binding =
                    Some(WorkerLocalBinding::new("other-driver", "other-subject"))
            }
            11 => row.status = CredentialStatus::Disabled,
            12 => row.version += 1,
            _ => unreachable!(),
        }
    }
    let spectator = source("cred:rotation-spectator", "ws");
    repo.put(spectator.clone()).await.unwrap();
    for stage in 0..5 {
        for coordinate in 0..13 {
            let rule = format!("Source stage {stage} field {coordinate}");
            let mut before = source(&format!("cred:rotation-drift:{stage}:{coordinate}"), "ws");
            before.provider_id = (coordinate == 5).then(|| "endpoint-provider".into());
            before.env_key = None;
            before.material_ref = Some(SecretRef("sec:old-current".into()));
            repo.put(before.clone()).await.unwrap();
            let mut after = before.clone();
            after.version += 1;
            after.material_ref = Some(SecretRef("sec:new-current".into()));
            if stage >= 3 {
                after.id.0.push_str("-replacement");
                after.version = 1;
                after.replacement_of = Some(CredentialRef {
                    id: before.id.0.clone(),
                    revision: 1,
                });
            }
            let writing = CredentialMutationIntent::prepare(Some(before.clone()), after).unwrap();
            let ready = if stage > 0 {
                assert!(
                    repo.begin_mutation(writing.clone()).await.unwrap(),
                    "{rule}"
                );
                Some(repo.mark_mutation_ready(&writing).await.unwrap())
            } else {
                None
            };
            let reclaiming = if stage == 2 || stage == 4 {
                Some(repo.apply_mutation(ready.as_ref().unwrap()).await.unwrap())
            } else {
                None
            };
            let original = if stage == 2 { &writing.after } else { &before };
            let mut changed = original.clone();
            drift(&mut changed, coordinate);
            changed
                .validate_authority()
                .expect("drift must reach CAS, not validation");
            assert_ne!(changed, *original, "{rule}");
            if coordinate != 12 {
                assert_eq!(changed.version, original.version, "{rule}");
            }
            repo.put(changed.clone()).await.unwrap();
            let pending = repo.pending_mutations().await.unwrap();
            let rejected = match ready.as_ref() {
                None => repo.begin_mutation(writing.clone()).await.map(|_| ()),
                Some(ready) => repo.apply_mutation(ready).await.map(|_| ()),
            };
            if stage == 4 {
                assert!(
                    rejected.is_ok(),
                    "replacement replay ignores changed predecessor"
                );
            } else {
                assert!(
                    matches!(rejected, Err(CredentialError::MutationConflict(_))),
                    "{rule} full Source CAS"
                );
            }
            if stage == 3 {
                assert!(
                    matches!(
                        repo.get(&writing.after.id).await,
                        Err(CredentialError::SourceNotFound(_))
                    ),
                    "replacement CAS cannot insert destination"
                );
            } else if stage == 4 {
                assert_eq!(
                    repo.get(&writing.after.id).await.unwrap(),
                    writing.after,
                    "replacement replay retains complete published destination"
                );
            }
            assert_eq!(
                repo.get(&changed.id).await.unwrap(),
                changed,
                "{rule} current Source frame"
            );
            assert_eq!(
                repo.pending_mutations().await.unwrap(),
                pending,
                "{rule} complete WAL frame"
            );
            assert_eq!(
                repo.get(&spectator.id).await.unwrap(),
                spectator,
                "{rule} spectator frame"
            );
            // Restore the constructed competing snapshot to retire only this
            // case's exact WAL through the production abort/completion port.
            repo.put(original.clone()).await.unwrap();
            if let Some(reclaiming) = reclaiming {
                repo.complete_mutation(&reclaiming).await.unwrap();
            } else if let Some(ready) = ready {
                let abort = repo.abort_mutation(&ready).await.unwrap();
                repo.complete_mutation(&abort).await.unwrap();
            }
        }
    }

    // R12: the same payload/revision at another physical id is not this row's
    // preimage. A missing positive-version target cannot claim a creation WAL.
    let mut missing_before = spectator.clone();
    missing_before.id = CredentialSourceId("cred:rotation-missing-key".into());
    let mut missing_after = missing_before.clone();
    missing_after.version += 1;
    let missing = CredentialMutationIntent::prepare(Some(missing_before), missing_after).unwrap();
    let pending = repo.pending_mutations().await.unwrap();
    assert!(
        matches!(
            repo.begin_mutation(missing.clone()).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R12 exact lookup key"
    );
    assert!(
        matches!(
            repo.get(&missing.after.id).await,
            Err(CredentialError::SourceNotFound(_))
        ),
        "R12 absent stays absent"
    );
    assert_eq!(
        repo.get(&spectator.id).await.unwrap(),
        spectator,
        "R12 other key frame"
    );
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        pending,
        "R12 WAL frame"
    );

    // R13/R14: independently vary the request's complete WAL identity while
    // durable truth is Ready/Reclaiming and Source is its exact before/after.
    // All requests validate; token/epoch/lease/before/after changes must fail
    // actual WAL equality. Non-Ready phases fail the phase gate even for replay.
    let mut before = source("cred:rotation-wal-gates", "ws");
    before.material_ref = Some(SecretRef("sec:wal-old".into()));
    repo.put(before.clone()).await.unwrap();
    let mut after = before.clone();
    after.version += 1;
    after.material_ref = Some(SecretRef("sec:wal-new".into()));
    let writing = CredentialMutationIntent::prepare(Some(before), after).unwrap();
    repo.begin_mutation(writing.clone()).await.unwrap();
    let ready = repo.mark_mutation_ready(&writing).await.unwrap();
    for published in [false, true] {
        if published {
            repo.apply_mutation(&ready).await.unwrap();
        }
        let current = repo.get(&ready.after.id).await.unwrap();
        let pending = repo.pending_mutations().await.unwrap();
        for coordinate in 0..8 {
            let mut stale = ready.clone();
            match coordinate {
                0 => stale.material_fence.writer_token.push('x'),
                1 => stale.material_fence.writer_epoch += 1,
                2 => stale.material_fence.writer_lease_expires_at_unix_ms -= 1,
                3 => stale.before.as_mut().unwrap().env_key = Some("OTHER_BEFORE".into()),
                4 => stale.after.env_key = Some("OTHER_AFTER".into()),
                5 => stale.material_fence.phase = CredentialMaterialMutationPhase::Writing,
                6 => stale.material_fence.phase = CredentialMaterialMutationPhase::Reclaiming,
                7 => stale.material_fence.phase = CredentialMaterialMutationPhase::ReclaimingAbort,
                _ => unreachable!(),
            }
            stale
                .validate()
                .expect("request must reach publication admission");
            assert!(
                matches!(
                    repo.apply_mutation(&stale).await,
                    Err(CredentialError::MutationConflict(_))
                ),
                "R13/R14 published={published} field={coordinate}"
            );
            assert_eq!(
                repo.get(&ready.after.id).await.unwrap(),
                current,
                "R13/R14 Source frame"
            );
            assert_eq!(
                repo.pending_mutations().await.unwrap(),
                pending,
                "R13/R14 WAL frame"
            );
        }
    }
    let reclaiming = repo.apply_mutation(&ready).await.unwrap();
    repo.complete_mutation(&reclaiming).await.unwrap();
    // R15/R16: old Ready cannot infer replay from matching Source alone after
    // its WAL is gone or a successor's Ready fact occupies the same key.
    for successor_pending in [false, true] {
        if successor_pending {
            let mut successor = ready.after.clone();
            successor.version += 1;
            successor.material_ref = Some(SecretRef("sec:successor".into()));
            let intent =
                CredentialMutationIntent::prepare(Some(ready.after.clone()), successor).unwrap();
            repo.begin_mutation(intent.clone()).await.unwrap();
            repo.mark_mutation_ready(&intent).await.unwrap();
        }
        let pending = repo.pending_mutations().await.unwrap();
        assert!(
            matches!(
                repo.apply_mutation(&ready).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "R15/R16 requires exact WAL"
        );
        assert_eq!(
            repo.get(&ready.after.id).await.unwrap(),
            ready.after,
            "R15/R16 Source frame"
        );
        assert_eq!(
            repo.pending_mutations().await.unwrap(),
            pending,
            "R15/R16 successor WAL frame"
        );
        assert_eq!(
            repo.get(&spectator.id).await.unwrap(),
            spectator,
            "R15/R16 spectator frame"
        );
    }
}

/// Distinct replacement cause/effect table. C1 the old full row equals the
/// intent's exact `before`; C2 the deterministic new id is absent, exact, or
/// foreign; C3 the old row races to a different revision; C4 old/new material
/// ownership is disjoint and the authority projection is unchanged. Effects:
/// E1 atomically insert the new version-1 row while preserving old; E2 exact
/// replay; E3 conflict without inserting/overwriting either row; E4 malformed
/// cross-id intent is rejected before its WAL claim. Every backend runs the
/// same rules so the repository port, not an adapter, owns these semantics.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | X1 | T | absent | F | T | E1 |
/// | X2 | - | exact | - | T | E2 |
/// | X3 | F | absent | T | T | E3 |
/// | X4 | T | foreign | F | T | E3 |
/// | X5 | T | absent | F | F | E4 |
async fn distinct_replacement_is_old_row_cas_and_idempotent(repo: &dyn CredentialRepo) {
    let mut old = source("cred:replacement-old", "ws");
    old.material_ref = Some(SecretRef("sec:replacement:old:primary".into()));
    repo.put(old.clone()).await.unwrap();
    let replacement =
        replacement_source(&old, "cred:replacement-new", "sec:replacement:new:primary");
    let exact = CredentialMutationIntent::prepare(Some(old.clone()), replacement).unwrap();
    repo.begin_mutation(exact.clone()).await.unwrap();
    let exact_ready = repo.mark_mutation_ready(&exact).await.unwrap();
    let exact_reclaiming = repo.apply_mutation(&exact_ready).await.unwrap();
    assert_eq!(repo.get(&old.id).await.unwrap(), old, "X1/E1");
    assert_eq!(
        repo.get(&exact.after.id).await.unwrap(),
        exact.after,
        "X1/E1"
    );
    repo.apply_mutation(&exact_ready).await.unwrap();
    assert_eq!(
        repo.get(&exact.after.id).await.unwrap(),
        exact.after,
        "X2/E2"
    );
    repo.complete_mutation(&exact_reclaiming).await.unwrap();

    let mut stale_old = source("cred:replacement-stale-old", "ws");
    stale_old.material_ref = Some(SecretRef("sec:replacement:stale-old:primary".into()));
    repo.put(stale_old.clone()).await.unwrap();
    let stale_new = replacement_source(
        &stale_old,
        "cred:replacement-stale-new",
        "sec:replacement:stale-new:primary",
    );
    let stale =
        CredentialMutationIntent::prepare(Some(stale_old.clone()), stale_new.clone()).unwrap();
    repo.begin_mutation(stale.clone()).await.unwrap();
    let stale_ready = repo.mark_mutation_ready(&stale).await.unwrap();
    let mut winner = stale_old.clone();
    winner.version = 2;
    repo.put(winner.clone()).await.unwrap();
    assert!(
        matches!(
            repo.apply_mutation(&stale_ready).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "X3/E3"
    );
    assert_eq!(repo.get(&stale_old.id).await.unwrap(), winner, "X3/E3");
    assert!(matches!(
        repo.get(&stale_new.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));
    let stale_abort = repo.abort_mutation(&stale_ready).await.unwrap();
    repo.complete_mutation(&stale_abort).await.unwrap();

    let mut occupied_old = source("cred:replacement-occupied-old", "ws");
    occupied_old.material_ref = Some(SecretRef("sec:replacement:occupied-old:primary".into()));
    repo.put(occupied_old.clone()).await.unwrap();
    let occupied_new = replacement_source(
        &occupied_old,
        "cred:replacement-occupied-new",
        "sec:replacement:occupied-new:primary",
    );
    let occupied =
        CredentialMutationIntent::prepare(Some(occupied_old.clone()), occupied_new.clone())
            .unwrap();
    repo.begin_mutation(occupied.clone()).await.unwrap();
    let occupied_ready = repo.mark_mutation_ready(&occupied).await.unwrap();
    let mut foreign = occupied_new.clone();
    foreign.version = 2;
    repo.put(foreign.clone()).await.unwrap();
    assert!(
        matches!(
            repo.apply_mutation(&occupied_ready).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "X4/E3"
    );
    assert_eq!(
        repo.get(&occupied_old.id).await.unwrap(),
        occupied_old,
        "X4/E3"
    );
    assert_eq!(repo.get(&occupied_new.id).await.unwrap(), foreign, "X4/E3");
    assert!(matches!(
        repo.abort_mutation(&occupied_ready).await,
        Err(CredentialError::MutationConflict(_))
    ));

    let mut invalid_after = replacement_source(
        &old,
        "cred:replacement-invalid-new",
        "sec:replacement:invalid-new:primary",
    );
    invalid_after.workspace_id = "other-workspace".into();
    assert!(
        matches!(
            CredentialMutationIntent::prepare(Some(old), invalid_after),
            Err(CredentialError::InvalidSource(_))
        ),
        "X5/E4"
    );
}

/// Cross-id recovery decision table. C1 the replacement row committed before
/// the crash; C2 its material was sealed; C3 the predecessor remains pinned.
/// Effects: E1 an unpublished replacement loses only its own material; E2 a
/// published replacement keeps both rows and both generations of material;
/// E3 recovery completes the one existing WAL idempotently. These rules run
/// against InMemory, SQLite, and Postgres repositories with the same Vault
/// reconciler and SecretStore port.
///
/// | Rule | C1 | C2 | C3 | Effect |
/// |---|---|---|---|---|
/// | XR1 | F | T | T | E1+E3 |
/// | XR2 | T | T | T | E2+E3 |
/// | XR3 | foreign same ref | T | T | zero delete, retain WAL/bytes |
async fn distinct_replacement_recovery_is_directional(repo: &dyn CredentialRepo) {
    let store = InMemorySecretStore::new();

    let mut aborted_old = source("cred:recovery-aborted-old", "ws");
    aborted_old.material_ref = Some(SecretRef("sec:recovery:aborted-old:primary".into()));
    repo.put(aborted_old.clone()).await.unwrap();
    store
        .put(
            aborted_old.material_ref.as_ref().unwrap(),
            RedactedString::new("aborted-old-material"),
        )
        .await
        .unwrap();
    let aborted_new = replacement_source(
        &aborted_old,
        "cred:recovery-aborted-new",
        "sec:recovery:aborted-new:primary",
    );
    let aborted =
        CredentialMutationIntent::prepare(Some(aborted_old.clone()), aborted_new.clone()).unwrap();
    repo.begin_mutation(aborted.clone()).await.unwrap();
    store
        .put(
            aborted.after.material_ref.as_ref().unwrap(),
            RedactedString::new("aborted-new-material"),
        )
        .await
        .unwrap();
    let claim_now = aborted.material_fence.writer_lease_expires_at_unix_ms + 1;
    let claimed = repo
        .claim_expired_mutation(&aborted, claim_now, claim_now + 120_000)
        .await
        .unwrap()
        .expect("XR1 expired Writing claim");
    repo.abort_mutation(&claimed).await.unwrap();
    assert_eq!(
        recover_credential_mutations(&store, repo).await.unwrap(),
        1,
        "XR1"
    );
    assert_eq!(
        repo.get(&aborted_old.id).await.unwrap(),
        aborted_old,
        "XR1/E1"
    );
    assert!(
        store
            .get(aborted_old.material_ref.as_ref().unwrap())
            .await
            .is_ok()
    );
    assert!(
        store
            .get(aborted.after.material_ref.as_ref().unwrap())
            .await
            .is_err()
    );
    assert!(matches!(
        repo.get(&aborted_new.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));

    let mut committed_old = source("cred:recovery-committed-old", "ws");
    committed_old.material_ref = Some(SecretRef("sec:recovery:committed-old:primary".into()));
    repo.put(committed_old.clone()).await.unwrap();
    store
        .put(
            committed_old.material_ref.as_ref().unwrap(),
            RedactedString::new("committed-old-material"),
        )
        .await
        .unwrap();
    let committed_new = replacement_source(
        &committed_old,
        "cred:recovery-committed-new",
        "sec:recovery:committed-new:primary",
    );
    let committed =
        CredentialMutationIntent::prepare(Some(committed_old.clone()), committed_new.clone())
            .unwrap();
    repo.begin_mutation(committed.clone()).await.unwrap();
    store
        .put(
            committed.after.material_ref.as_ref().unwrap(),
            RedactedString::new("committed-new-material"),
        )
        .await
        .unwrap();
    let committed_ready = repo.mark_mutation_ready(&committed).await.unwrap();
    repo.apply_mutation(&committed_ready).await.unwrap();
    assert_eq!(
        recover_credential_mutations(&store, repo).await.unwrap(),
        1,
        "XR2"
    );
    assert_eq!(
        repo.get(&committed_old.id).await.unwrap(),
        committed_old,
        "XR2/E2"
    );
    assert_eq!(
        repo.get(&committed.after.id).await.unwrap(),
        committed.after,
        "XR2/E2"
    );
    assert!(
        store
            .get(committed_old.material_ref.as_ref().unwrap())
            .await
            .is_ok()
    );
    assert!(
        store
            .get(committed.after.material_ref.as_ref().unwrap())
            .await
            .is_ok()
    );
    assert!(
        repo.pending_mutations().await.unwrap().is_empty(),
        "XR1/XR2/E3"
    );

    let guarded = DeleteCountingStore::default();
    let mut foreign_old = source("cred:recovery-foreign-old", "ws");
    foreign_old.material_ref = Some(SecretRef("sec:recovery:foreign-old:primary".into()));
    repo.put(foreign_old.clone()).await.unwrap();
    guarded
        .put(
            foreign_old.material_ref.as_ref().unwrap(),
            RedactedString::new("foreign-old-material"),
        )
        .await
        .unwrap();
    let foreign_after = replacement_source(
        &foreign_old,
        "cred:recovery-foreign-new",
        "sec:recovery:foreign-new:primary",
    );
    let foreign_intent =
        CredentialMutationIntent::prepare(Some(foreign_old), foreign_after).unwrap();
    assert!(repo.begin_mutation(foreign_intent.clone()).await.unwrap());
    let candidate_ref = foreign_intent.after.material_ref.clone().unwrap();
    guarded
        .put(&candidate_ref, RedactedString::new("candidate-material"))
        .await
        .unwrap();
    let foreign_ready = repo.mark_mutation_ready(&foreign_intent).await.unwrap();
    let mut foreign_row = foreign_ready.after.clone();
    foreign_row.replacement_of = None;
    repo.put(foreign_row.clone()).await.unwrap();

    assert!(matches!(
        recover_credential_mutations(&guarded, repo).await,
        Err(CredentialError::MutationConflict(_))
    ));
    assert_eq!(guarded.deletes.load(Ordering::SeqCst), 0, "XR3");
    assert_eq!(repo.get(&foreign_row.id).await.unwrap(), foreign_row, "XR3");
    assert_eq!(
        guarded.get(&candidate_ref).await.unwrap().expose_secret(),
        "candidate-material",
        "XR3"
    );
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        vec![foreign_ready],
        "XR3"
    );
}

// CONTRACT: `CredentialRepo::get` is a deliberate unscoped by-id PRIMITIVE — it is
// keyed by source id only, while `list` is the workspace-scoped enumeration face.
// Tenant isolation for secret *materialization* is enforced one layer up, in
// `awaken-config-resolver::resolve_credential` (a pool member / Exact binding whose
// source `workspace_id` differs from the pool's is fenced there), and a caller audit
// confirms every `get` caller either goes through that fence or only reads secret-free
// management rows. This test pins the primitive's contract across EVERY backend
// (in-memory, sqlite, postgres); it mirrors the `get_is_an_unscoped_by_id_primitive_
// fenced_at_resolution` unit in `src/repo.rs`, extending it to the durable backends.
async fn get_is_an_unscoped_by_id_primitive(repo: &dyn CredentialRepo) {
    repo.put(source("cred:owned", "ws-owner")).await.unwrap();

    // `list` for an unrelated workspace correctly hides the row...
    assert_eq!(repo.list("ws-other").await.unwrap().len(), 0);
    // ...but a direct `get` by id returns it regardless of workspace.
    let cross = repo
        .get(&CredentialSourceId("cred:owned".into()))
        .await
        .unwrap();
    assert_eq!(cross.workspace_id, "ws-owner");
}

/// Legacy wire inputs retain or omit attempt_id independently of the missing
/// owner fields. Both valid epoch-zero envelopes must recover without gaining
/// epoch-one publication authority; they remain forbidden as fresh commands.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
fn legacy_mutation_wires(
    retain_attempt: bool,
    phase: CredentialMaterialMutationPhase,
) -> [(&'static str, serde_json::Value, &'static str); 2] {
    let mut source = source("cred:legacy", "ws");
    source.material_ref = Some(SecretRef("sec:legacy".into()));
    let plain = CredentialMutationIntent::prepare(None, source.clone()).unwrap();
    source.id = CredentialSourceId("cred:legacy:managed".into());
    let child = ManagedVaultCredential {
        id: "child:legacy".into(),
        vault_id: "vault:legacy".into(),
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
    let managed = PendingManagedCredentialMutation::create(source, child).unwrap();
    let mut rows = [
        (
            "credential_creation_intent",
            serde_json::to_value(plain).unwrap(),
            "after",
        ),
        (
            "credential_managed_credential_mutation",
            serde_json::to_value(managed).unwrap(),
            "after_source",
        ),
    ];
    for (_, wire, source_field) in &mut rows {
        let object = wire.as_object_mut().unwrap();
        object.insert("format_version".into(), serde_json::json!(0));
        object.insert("phase".into(), serde_json::to_value(phase).unwrap());
        for field in [
            "writer_token",
            "writer_epoch",
            "writer_lease_expires_at_unix_ms",
        ] {
            assert!(object.remove(field).is_some());
        }
        if !retain_attempt {
            assert!(object.remove("attempt_id").is_some());
            object.get_mut(*source_field).unwrap()["material_ref"] = serde_json::json!(format!(
                "sec:{}",
                object[*source_field]["id"].as_str().unwrap()
            ));
        }
    }
    rows
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn legacy_mutation_recovery_conforms(
    repo: &dyn ManagedCredentialRepository,
    retain_attempt: bool,
) {
    // L1 missing owner fields deserialize as zero; L2 absence/retention of the
    // old attempt does not affect admission. Both => exact CAS to epoch two,
    // no Ready/publication, stale-owner rejection, unchanged remaining fields,
    // and abort cleanup. These are WAL witnesses with no external material IO.
    macro_rules! check_legacy {
        ($pending:ident, $begin:ident, $claim:ident, $ready:ident, $abort:ident, $complete:ident) => {{
            let rows = repo.$pending().await.unwrap();
            assert_eq!(rows.len(), 1);
            let original = &rows[0];
            original.validate().unwrap();
            assert_eq!(original.material_fence.writer_epoch, 0);
            assert_eq!(original.material_fence.writer_lease_expires_at_unix_ms, 0);
            assert!(original.material_fence.writer_token.is_empty());
            assert_eq!(
                !original.material_fence.attempt_id().is_empty(),
                retain_attempt
            );
            assert!(
                repo.$begin(original.clone()).await.is_err(),
                "legacy is not a fresh command"
            );
            if original.material_fence.phase == CredentialMaterialMutationPhase::Writing {
                let claimed = repo.$claim(original, 0, 10).await.unwrap().unwrap();
                assert_eq!(claimed.material_fence.writer_epoch, 2);
                assert_eq!(claimed.material_fence.writer_lease_expires_at_unix_ms, 10);
                let mut normalized = claimed.clone();
                normalized.material_fence.writer_token =
                    original.material_fence.writer_token.clone();
                normalized.material_fence.writer_epoch = 0;
                normalized.material_fence.writer_lease_expires_at_unix_ms = 0;
                assert_eq!(&normalized, original);
                assert!(
                    repo.$ready(&claimed).await.is_err(),
                    "legacy takeover is abort-only"
                );
                assert!(claimed.with_material_ready().is_err());
                assert!(claimed.with_material_reclaiming().is_err());
                assert!(repo.$claim(original, 0, 10).await.unwrap().is_none());
                assert!(repo.$abort(original).await.is_err());
                assert_eq!(repo.$pending().await.unwrap(), vec![claimed.clone()]);
                let next = repo.$claim(&claimed, 10, 20).await.unwrap().unwrap();
                assert_eq!(
                    next.material_fence.writer_epoch, 3,
                    "recovery lease can itself expire"
                );
                assert!(repo.$ready(&next).await.is_err());
                assert!(repo.$claim(&claimed, 10, 20).await.unwrap().is_none());
                let aborted = repo.$abort(&next).await.unwrap();
                repo.$complete(&aborted).await.unwrap();
            } else {
                // L3 pre-existing Ready may abort; L4 an exact legacy abort WAL
                // may complete. Re-encoding omitted defaults is not a CAS fence.
                let aborted =
                    if original.material_fence.phase == CredentialMaterialMutationPhase::Ready {
                        repo.$abort(original).await.unwrap()
                    } else {
                        assert_eq!(
                            original.material_fence.phase,
                            CredentialMaterialMutationPhase::ReclaimingAbort
                        );
                        original.clone()
                    };
                repo.$complete(&aborted).await.unwrap();
            }
            assert!(repo.$pending().await.unwrap().is_empty());
        }};
    }
    check_legacy!(
        pending_mutations,
        begin_mutation,
        claim_expired_mutation,
        mark_mutation_ready,
        abort_mutation,
        complete_mutation
    );
    check_legacy!(
        pending_managed_mutations,
        begin_managed_mutation,
        claim_expired_managed_mutation,
        mark_managed_mutation_ready,
        abort_managed_mutation,
        complete_managed_mutation
    );
}

// E1 stored JSON explicitly spells or omits defaults (and SQLite changes whitespace), but
// decodes to the exact before row => Source and Managed updates publish and
// retire their WAL/outbox. E2 a genuinely changed before row remains a conflict,
// covered by the shared stale-snapshot/collision tables in this same suite.
// SQL wrappers rewrite only representation; these helpers own both assertions.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn prepare_root_encoding_updates(
    repo: &dyn ManagedCredentialRepository,
) -> (CredentialMutationIntent, PendingManagedCredentialMutation) {
    let plain = source("cred:encoding:plain", "ws");
    repo.put(plain.clone()).await.unwrap();
    let mut after_plain = plain.clone();
    after_plain.version += 1;
    let plain_update = CredentialMutationIntent::prepare(Some(plain), after_plain).unwrap();
    let vault = ManagedVault {
        id: "vault:encoding".into(),
        workspace_id: "ws".into(),
        display_name: "encoding".into(),
        metadata: Default::default(),
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        archived_at: None,
        deletion: None,
        revision: 1,
    };
    repo.insert_vault("ws", vault.clone()).await.unwrap();
    let managed_source = source("cred:encoding:managed", "ws");
    let child = ManagedVaultCredential {
        id: "child:encoding".into(),
        vault_id: vault.id,
        workspace_id: "ws".into(),
        source_id: managed_source.id.clone(),
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
    let create =
        PendingManagedCredentialMutation::create(managed_source.clone(), child.clone()).unwrap();
    assert!(repo.begin_managed_mutation(create.clone()).await.unwrap());
    let committed = repo.commit_managed_mutation(&create).await.unwrap();
    repo.complete_managed_mutation(&committed).await.unwrap();
    let mut after_child = child.clone();
    after_child.revision += 1;
    after_child.display_name = Some("updated encoding fixture".into());
    let update = PendingManagedCredentialMutation::change(
        "managed-update:encoding".into(),
        ManagedCredentialOperation::Update,
        managed_source.clone(),
        managed_source,
        child,
        after_child,
        false,
    )
    .unwrap();
    (plain_update, update)
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
fn root_encoding_wires(
    plain: &CredentialMutationIntent,
    managed: &PendingManagedCredentialMutation,
) -> [(&'static str, serde_json::Value); 3] {
    let mut rows = [
        (
            "credential_source",
            serde_json::to_value(plain.before.as_ref().unwrap()).unwrap(),
        ),
        (
            "credential_source",
            serde_json::to_value(managed.before_source.as_ref().unwrap()).unwrap(),
        ),
        (
            "credential_managed_vault_credential",
            serde_json::to_value(managed.before_credential.as_ref().unwrap()).unwrap(),
        ),
    ];
    for (table, wire) in &mut rows {
        let object = wire.as_object_mut().unwrap();
        if *table == "credential_source" {
            assert!(
                object
                    .insert("descriptor".into(), serde_json::Value::Null)
                    .is_none(),
                "E1 explicit None differs from omitted None"
            );
        } else {
            assert_eq!(
                object.remove("metadata"),
                Some(serde_json::json!({})),
                "E1 omitted empty map differs from serialized empty map"
            );
        }
    }
    rows
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn root_encoding_updates_conform(
    repo: &dyn ManagedCredentialRepository,
    plain: CredentialMutationIntent,
    managed: PendingManagedCredentialMutation,
) {
    assert_eq!(
        repo.get(&plain.after.id).await.unwrap(),
        *plain.before.as_ref().unwrap()
    );
    assert_eq!(
        repo.get(&managed.after_source.id).await.unwrap(),
        *managed.before_source.as_ref().unwrap()
    );
    assert_eq!(
        repo.get_vault_credential("ws", &managed.after_credential.id)
            .await
            .unwrap()
            .as_ref(),
        managed.before_credential.as_ref()
    );
    assert!(repo.begin_mutation(plain.clone()).await.unwrap());
    let committed = repo.apply_mutation(&plain).await.unwrap();
    repo.complete_mutation(&committed).await.unwrap();
    assert_eq!(
        repo.get(&plain.after.id).await.unwrap(),
        plain.after,
        "E1 Source"
    );
    assert!(repo.begin_managed_mutation(managed.clone()).await.unwrap());
    let committed = repo.commit_managed_mutation(&managed).await.unwrap();
    repo.complete_managed_mutation(&committed).await.unwrap();
    assert_eq!(
        repo.get(&managed.after_source.id).await.unwrap(),
        managed.after_source,
        "E1 Managed source"
    );
    assert_eq!(
        repo.get_vault_credential("ws", &managed.after_credential.id)
            .await
            .unwrap(),
        Some(managed.after_credential),
        "E1 Managed child"
    );
    for rollout in repo.pending_managed_rollouts().await.unwrap() {
        repo.complete_managed_rollout(&rollout).await.unwrap();
    }
    assert!(repo.pending_mutations().await.unwrap().is_empty());
    assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
    assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
}

/// Shared takeover table for Source/Managed x InMemory/SQLite/PostgreSQL.
/// T1 absent WAL or live/non-Writing snapshot => no claim; T2 invalid successor
/// lease => error without a WAL change; T3 same key but stale token/epoch/lease/payload
/// => no claim; T4 two expired-snapshot callers => one successor; T5 stale owner
/// cannot Ready/publish/abort/claim; T6 repeated takeover advances epoch, retains every
/// other envelope field, and permits only durable abort/cleanup. No material IO
/// is performed: these are repository CAS witnesses, not SecretStore recovery.
async fn expired_writer_claims_are_exact_snapshot_cas(repo: &dyn ManagedCredentialRepository) {
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
            assert!(repo.$begin(writing.clone()).await.unwrap());
            assert!(
                !repo.$begin(writing.clone()).await.unwrap(),
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
                    0 => stale.material_fence.writer_token.push('x'),
                    1 => stale.material_fence.writer_epoch += 1,
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
            for (expected_epoch, claim_time) in [(2, now), (3, deadline)] {
                if expected_epoch == 3 {
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
                assert_eq!(claimed.material_fence.writer_token.len(), 32);
                assert!(
                    claimed
                        .material_fence
                        .writer_token
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                );
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
        CredentialMutationIntent::prepare(None, source.clone()).unwrap(),
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
        PendingManagedCredentialMutation::create(source, child).unwrap(),
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

/// Cause/effect table V1-V3: proposed Workspace matches or differs from the
/// request; the durable id is absent, same-owner, or foreign-owner; operation
/// is insert or ensure. These 2x3x2 rules cover authority errors before effects,
/// insert conflict on an owned id, ensure replay preserving durable metadata,
/// and creation only when absent. Every rejection preserves both Workspaces.
async fn managed_vault_workspace_authority_conforms(repo: &dyn ManagedCredentialRepository) {
    for ensure in [false, true] {
        for proposed_matches in [false, true] {
            for existing_owner in [None, Some("a"), Some("b")] {
                let id = format!("authority-{ensure}-{proposed_matches}-{existing_owner:?}");
                let proposed = ManagedVault {
                    id: id.clone(),
                    workspace_id: if proposed_matches { "a" } else { "b" }.into(),
                    display_name: "proposed".into(),
                    metadata: Default::default(),
                    created_at: "2026-09-17T00:00:00Z".into(),
                    updated_at: "2026-09-17T00:00:00Z".into(),
                    archived_at: None,
                    deletion: None,
                    revision: 1,
                };
                let durable = existing_owner.map(|owner| ManagedVault {
                    workspace_id: owner.into(),
                    display_name: "durable".into(),
                    revision: 7,
                    ..proposed.clone()
                });
                if let Some(durable) = &durable {
                    repo.insert_vault(&durable.workspace_id, durable.clone())
                        .await
                        .unwrap();
                }
                let result = if ensure {
                    repo.ensure_vault("a", proposed.clone()).await
                } else {
                    repo.insert_vault("a", proposed.clone())
                        .await
                        .map(|()| proposed.clone())
                };
                if !proposed_matches || existing_owner == Some("b") {
                    assert!(
                        matches!(result, Err(CredentialError::InvalidSource(_))),
                        "V1/V3"
                    );
                } else if !ensure && existing_owner.is_some() {
                    assert!(
                        matches!(result, Err(CredentialError::MutationConflict(_))),
                        "V2 insert"
                    );
                } else {
                    assert_eq!(
                        result.unwrap(),
                        durable.clone().unwrap_or_else(|| proposed.clone()),
                        "V2 ensure/create"
                    );
                }
                let expected = durable
                    .or_else(|| (proposed_matches && existing_owner.is_none()).then_some(proposed));
                for owner in ["a", "b"] {
                    assert_eq!(
                        repo.get_vault(owner, &id).await.unwrap(),
                        expected.clone().filter(|row| row.workspace_id == owner),
                        "no foreign projection or changed durable row"
                    );
                }
            }
        }
    }
}

/// Managed begin/phase cause-effect table shared by all three adapters. C1 the
/// fresh fence is canonical or forged; C2 no WAL or one logically identical WAL
/// already exists; C3 the caller is the durable physical owner. Effects: M1
/// forged Ready/epoch/owner/replacement lineage rejects with zero WAL; M2 first begin owns exactly
/// one attempt; M3 a fresh random follower attaches as non-owner and creates no
/// second WAL/ref; M4 only the exact owner may mark Ready, publish the pair, and
/// complete Reclaiming; M5 a caller whose before-material projection differs
/// from the exact durable Reclaiming envelope is rejected and cannot drive
/// cleanup. Rules M1 !C1=>E1; M2 C1+no WAL=>E2; M3 C1+same logical WAL=>E3;
/// M4 C3=>E4; M5 !exact-envelope=>conflict and durable WAL retained.
/// M6 independent source/child identity collisions: neither present admits;
/// source-only, child-only or both present reject before creating a WAL. Reuse
/// the published IDs with fresh opposite IDs, without fabricating torn storage.
async fn managed_material_mutation_begin_and_phases_conform(
    repo: &dyn ManagedCredentialRepository,
) {
    expired_writer_claims_are_exact_snapshot_cas(repo).await;
    managed_vault_workspace_authority_conforms(repo).await;
    let vault = ManagedVault {
        id: "vault-begin-owner".into(),
        workspace_id: "ws".into(),
        display_name: "vault-begin-owner".into(),
        metadata: Default::default(),
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        archived_at: None,
        deletion: None,
        revision: 1,
    };
    repo.insert_vault("ws", vault.clone()).await.unwrap();
    let mut source = source("cred:managed-begin-owner", "ws");
    source.material_ref = Some(SecretRef("sec:managed-begin-owner:r1:primary".into()));
    let child = ManagedVaultCredential {
        id: "child-begin-owner".into(),
        vault_id: vault.id,
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
    let owner = PendingManagedCredentialMutation::create(source.clone(), child.clone()).unwrap();
    let follower = PendingManagedCredentialMutation::create(source, child).unwrap();

    let mut forged_ready = owner.clone();
    forged_ready.material_fence.phase = CredentialMaterialMutationPhase::Ready;
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_ready).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged Ready"
    );
    let mut forged_epoch = owner.clone();
    forged_epoch.material_fence.writer_epoch = 2;
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_epoch).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged epoch"
    );
    let mut forged_owner = owner.clone();
    forged_owner.material_fence.writer_token = "forged-owner".into(); // awaken-allow: secret -- writer identity, not material
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_owner).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged owner"
    );
    let mut forged_lineage = owner.clone();
    forged_lineage.after_source.replacement_of = Some(CredentialRef {
        id: "predecessor-not-fenced-by-managed-create".into(),
        revision: 1,
    });
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_lineage).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged replacement lineage"
    );
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "M1"
    );

    assert!(
        repo.begin_managed_mutation(owner.clone()).await.unwrap(),
        "M2"
    );
    assert_ne!(
        owner.material_fence.attempt_id(),
        follower.material_fence.attempt_id(),
        "M3 exercises a fresh random physical attempt"
    );
    assert!(!repo.begin_managed_mutation(follower).await.unwrap(), "M3");
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![owner.clone()],
        "M3"
    );

    let ready = repo.mark_managed_mutation_ready(&owner).await.unwrap();
    let reclaiming = repo.commit_managed_mutation(&ready).await.unwrap();
    assert_eq!(
        reclaiming.material_fence.phase,
        CredentialMaterialMutationPhase::Reclaiming,
        "M4"
    );
    repo.complete_managed_mutation(&reclaiming).await.unwrap();
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "M4"
    );

    let before_source = repo.get(&owner.after_source.id).await.unwrap();
    let before_child = repo
        .get_vault_credential("ws", &owner.after_credential.id)
        .await
        .unwrap()
        .unwrap();
    let after_source = before_source.clone();
    let mut after_child = before_child.clone();
    after_child.revision += 1;
    after_child
        .metadata
        .insert("generation".into(), "two".into());
    let update = PendingManagedCredentialMutation::change(
        "managed-update:exact-reclaiming-envelope".into(),
        ManagedCredentialOperation::Update,
        before_source,
        after_source,
        before_child,
        after_child,
        false,
    )
    .unwrap();
    assert!(repo.begin_managed_mutation(update.clone()).await.unwrap());
    let update_reclaiming = repo.commit_managed_mutation(&update).await.unwrap();
    // Rollout identity decision table, shared by all three real repositories.
    // C1 event key exists; C2 every payload field matches; C3 exact mutation is
    // replayed. R1 C1+C2+C3 => same event and committed pair; R2 C1+!C2 =>
    // conflict with unchanged event/pair; R3 !C1 => idempotent no-op; R4 C1+C2
    // acknowledgement => remove exactly this event; repeating R4 => R3.
    // Single-field collisions cover each of the seven non-key fields; all
    // eight-field combinations are independently covered in the contract's
    // native/Kani table. The SQL transaction/JSON boundary is exercised here.
    let events = repo.pending_managed_rollouts().await.unwrap();
    assert_eq!(events.len(), 1, "rollout R1");
    let event = events[0].clone();
    assert_eq!(
        repo.commit_managed_mutation(&update).await.unwrap(),
        update_reclaiming,
        "rollout R1 exact commit replay"
    );
    assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
    for field in 0..7 {
        let mut collision = event.clone();
        match field {
            0 => collision.workspace_id.push('x'),
            1 => collision.vault_id.push('x'),
            2 => collision.credential_id.push('x'),
            3 => collision.source_id.0.push('x'),
            4 => collision.source_version ^= 1,
            5 => collision.credential_revision ^= 1,
            6 => collision.operation = ManagedCredentialOperation::Delete,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                repo.complete_managed_rollout(&collision).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "rollout R2 field {field}"
        );
        assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
        assert_eq!(
            repo.get(&update.after_source.id).await.unwrap(),
            update.after_source
        );
        assert_eq!(
            repo.get_vault_credential("ws", &update.after_credential.id)
                .await
                .unwrap(),
            Some(update.after_credential.clone())
        );
    }
    let mut forged_update = update.clone();
    forged_update
        .before_source
        .as_mut()
        .unwrap()
        .auxiliary_material_refs
        .insert("foreign".into(), SecretRef("sec:foreign".into()));
    assert!(
        repo.commit_managed_mutation(&forged_update).await.is_err(),
        "M5"
    );
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![update_reclaiming.clone()],
        "M5"
    );
    repo.complete_managed_mutation(&update_reclaiming)
        .await
        .unwrap();

    // R5 a later valid mutation reuses the still-durable event id but advances
    // the child revision => insert collision rejects the entire publication.
    // Public repository commands seed the collision, so this same rollback
    // witness runs on all adapters without a parallel test-only insertion port.
    let mut next_child = update.after_credential.clone();
    next_child.revision += 1;
    let collision_update = PendingManagedCredentialMutation::change(
        update.operation_id.clone(),
        ManagedCredentialOperation::Update,
        update.after_source.clone(),
        update.after_source.clone(),
        update.after_credential.clone(),
        next_child,
        false,
    )
    .unwrap();
    assert!(
        repo.begin_managed_mutation(collision_update.clone())
            .await
            .unwrap()
    );
    assert!(
        matches!(
            repo.commit_managed_mutation(&collision_update).await,
            Err(awaken_credential_vault::catalog::ManagedCredentialMutationError::RevisionConflict)
        ),
        "rollout R5"
    );
    assert_eq!(
        repo.get(&update.after_source.id).await.unwrap(),
        update.after_source
    );
    assert_eq!(
        repo.get_vault_credential("ws", &update.after_credential.id)
            .await
            .unwrap(),
        Some(update.after_credential.clone())
    );
    assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![collision_update.clone()]
    );
    let aborted = repo
        .abort_managed_mutation(&collision_update)
        .await
        .unwrap();
    repo.complete_managed_mutation(&aborted).await.unwrap();
    let mut absent = event.clone();
    absent.id.push('x');
    repo.complete_managed_rollout(&absent).await.unwrap();
    assert_eq!(
        repo.pending_managed_rollouts().await.unwrap(),
        events,
        "rollout R3"
    );
    repo.complete_managed_rollout(&event).await.unwrap();
    assert!(
        repo.pending_managed_rollouts().await.unwrap().is_empty(),
        "rollout R4"
    );
    repo.complete_managed_rollout(&event).await.unwrap();

    // R6-R8 cross independent source/child transactions, a shared event key,
    // and left-first/right-first/concurrent scheduling. Each candidate is
    // valid against its OWN unchanged before-pair: only the outbox key can
    // cause rejection. Exactly one pair and Reclaiming WAL publish; the loser
    // retains its complete before-pair and Ready WAL. A third committed event
    // is a spectator for every commit and ACK (frame, not just cardinality).
    // R9 losing-payload ACK rejects while the winner exists; after exact ACK
    // it is an absent-key no-op. R10 retries of the winner preserve its WAL
    // and event. These are native transaction witnesses, not schedule-complete
    // Rust/SQL refinement; join! overlaps yielding SQL calls, while synchronous
    // adapters serialize at their existing command lock.
    for schedule in ["left-first", "right-first", "concurrent"] {
        let shared_id = format!("rollout-race:{schedule}");
        let mut candidates = Vec::new();
        for index in 0..3 {
            let mut initial_source = owner.after_source.clone();
            initial_source.id = CredentialSourceId(format!("cred:rollout:{schedule}:{index}"));
            // Distinct vaults avoid serializing PostgreSQL contenders at the
            // parent row lock before they reach the shared outbox unique key.
            let mut candidate_vault = repo
                .get_vault("ws", &owner.after_credential.vault_id)
                .await
                .unwrap()
                .unwrap();
            candidate_vault.id = format!("vault:rollout:{schedule}:{index}");
            repo.insert_vault("ws", candidate_vault.clone())
                .await
                .unwrap();
            let mut initial_child = owner.after_credential.clone();
            initial_child.vault_id = candidate_vault.id;
            initial_child.id = format!("child:rollout:{schedule}:{index}");
            initial_child.source_id = initial_source.id.clone();
            let create = PendingManagedCredentialMutation::create(
                initial_source.clone(),
                initial_child.clone(),
            )
            .unwrap();
            assert!(repo.begin_managed_mutation(create.clone()).await.unwrap());
            let ready = repo.mark_managed_mutation_ready(&create).await.unwrap();
            let committed = repo.commit_managed_mutation(&ready).await.unwrap();
            repo.complete_managed_mutation(&committed).await.unwrap();
            // Create namespaces material references to its real attempt; use
            // the committed pair as the next command's before-image.
            let initial_source = committed.after_source.clone();
            let mut after_source = initial_source.clone();
            after_source.version += 1;
            let mut after_child = initial_child.clone();
            after_child.revision += 1;
            after_child.display_name = Some(format!("published-{schedule}-{index}"));
            let candidate = PendingManagedCredentialMutation::change(
                if index == 2 {
                    format!("spectator:{schedule}")
                } else {
                    shared_id.clone()
                },
                ManagedCredentialOperation::Update,
                initial_source,
                after_source,
                initial_child,
                after_child,
                false,
            )
            .unwrap();
            assert!(
                repo.begin_managed_mutation(candidate.clone())
                    .await
                    .unwrap()
            );
            candidates.push(candidate);
        }
        let spectator_wal = repo.commit_managed_mutation(&candidates[2]).await.unwrap();
        let spectator = repo
            .managed_rollout(&candidates[2].operation_id)
            .await
            .unwrap()
            .unwrap();
        let (left, right) = match schedule {
            "left-first" => (
                repo.commit_managed_mutation(&candidates[0]).await,
                repo.commit_managed_mutation(&candidates[1]).await,
            ),
            "right-first" => {
                let right = repo.commit_managed_mutation(&candidates[1]).await;
                (repo.commit_managed_mutation(&candidates[0]).await, right)
            }
            "concurrent" => tokio::join!(
                repo.commit_managed_mutation(&candidates[0]),
                repo.commit_managed_mutation(&candidates[1]),
            ),
            _ => unreachable!(),
        };
        let (winner, winner_wal, rejected) = match (left, right) {
            (Ok(wal), Err(error)) => (0, wal, error),
            (Err(error), Ok(wal)) => (1, wal, error),
            outcomes => panic!("R6 one atomic winner: {schedule}: {outcomes:?}"),
        };
        assert!(
            matches!(
                rejected,
                awaken_credential_vault::catalog::ManagedCredentialMutationError::RevisionConflict
            ),
            "R6 exact event collision: {schedule}"
        );
        if schedule != "concurrent" {
            assert_eq!(
                winner,
                usize::from(schedule == "right-first"),
                "R7 both serial winners"
            );
        }
        let loser = 1 - winner;
        let event = repo.managed_rollout(&shared_id).await.unwrap().unwrap();
        // Independent event oracle: expected identities/revisions come from
        // the requested winning pair, never from the production constructor.
        assert_eq!(event.id, shared_id);
        assert_eq!(event.workspace_id, "ws");
        assert_eq!(event.vault_id, candidates[winner].after_credential.vault_id);
        assert_eq!(event.credential_id, candidates[winner].after_credential.id);
        assert_eq!(event.source_id, candidates[winner].after_source.id);
        assert_eq!(event.source_version, 2);
        assert_eq!(event.credential_revision, 2);
        assert_eq!(event.operation, ManagedCredentialOperation::Update);
        let mut expected_events = vec![event.clone(), spectator.clone()];
        expected_events.sort_by(|left, right| left.id.cmp(&right.id));
        let mut durable_events = repo.pending_managed_rollouts().await.unwrap();
        durable_events.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(durable_events, expected_events, "R8 outbox frame");
        assert_eq!(
            repo.commit_managed_mutation(&candidates[winner])
                .await
                .unwrap(),
            winner_wal,
            "R10"
        );
        let mut colliding_ack = event.clone();
        colliding_ack.vault_id = candidates[loser].after_credential.vault_id.clone();
        colliding_ack.credential_id = candidates[loser].after_credential.id.clone();
        colliding_ack.source_id = candidates[loser].after_source.id.clone();
        assert!(
            matches!(
                repo.complete_managed_rollout(&colliding_ack).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "R9 losing ACK"
        );
        let mut durable_events = repo.pending_managed_rollouts().await.unwrap();
        durable_events.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(durable_events, expected_events, "R9 outbox frame");
        for (index, candidate) in candidates.iter().enumerate() {
            let (expected_source, expected_child) = if index == loser {
                (
                    candidate.before_source.as_ref().unwrap(),
                    candidate.before_credential.as_ref().unwrap(),
                )
            } else {
                (&candidate.after_source, &candidate.after_credential)
            };
            assert_eq!(
                &repo.get(&candidate.after_source.id).await.unwrap(),
                expected_source,
                "R8 source frame"
            );
            assert_eq!(
                repo.get_vault_credential("ws", &candidate.after_credential.id)
                    .await
                    .unwrap()
                    .as_ref(),
                Some(expected_child),
                "R8 child frame"
            );
        }
        let mut expected_wals = vec![
            winner_wal.clone(),
            candidates[loser].clone(),
            spectator_wal.clone(),
        ];
        expected_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        let mut durable_wals = repo.pending_managed_mutations().await.unwrap();
        durable_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        assert_eq!(durable_wals, expected_wals, "R8 complete WAL frame");
        repo.complete_managed_rollout(&event).await.unwrap();
        repo.complete_managed_rollout(&colliding_ack).await.unwrap();
        repo.complete_managed_rollout(&event).await.unwrap();
        assert_eq!(
            repo.pending_managed_rollouts().await.unwrap(),
            vec![spectator.clone()],
            "R9 absent replay preserves spectator"
        );
        // R11 the key becomes available only after exact removal. The same
        // previously rejected Ready transaction can now publish its own pair.
        // Delayed ACK of the OLD payload must reject, even though the id is
        // identical. This crosses key reuse with payload comparison (ABA),
        // without inventing a second event-insertion path or a permanent id
        // tombstone that the production contract does not have.
        let successor_wal = repo
            .commit_managed_mutation(&candidates[loser])
            .await
            .unwrap();
        assert_eq!(
            repo.managed_rollout(&shared_id).await.unwrap(),
            Some(colliding_ack.clone()),
            "R11 exact successor event"
        );
        assert!(
            matches!(
                repo.complete_managed_rollout(&event).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "R11 delayed old ACK"
        );
        assert_eq!(
            repo.commit_managed_mutation(&candidates[winner])
                .await
                .unwrap(),
            winner_wal,
            "R11 old commit replay"
        );
        let mut durable_events = repo.pending_managed_rollouts().await.unwrap();
        durable_events.sort_by(|left, right| left.id.cmp(&right.id));
        let mut successor_events = vec![colliding_ack.clone(), spectator.clone()];
        successor_events.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(
            durable_events, successor_events,
            "R11 replay preserves successor and spectator"
        );
        for candidate in &candidates {
            assert_eq!(
                repo.get(&candidate.after_source.id).await.unwrap(),
                candidate.after_source,
                "R11 committed source frame"
            );
            assert_eq!(
                repo.get_vault_credential("ws", &candidate.after_credential.id)
                    .await
                    .unwrap(),
                Some(candidate.after_credential.clone()),
                "R11 committed child frame"
            );
        }
        let mut durable_wals = repo.pending_managed_mutations().await.unwrap();
        durable_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        let mut completed_wals = vec![
            winner_wal.clone(),
            successor_wal.clone(),
            spectator_wal.clone(),
        ];
        completed_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        assert_eq!(durable_wals, completed_wals, "R11 WAL frame");
        repo.complete_managed_rollout(&colliding_ack).await.unwrap();
        for finished in [&winner_wal, &successor_wal, &spectator_wal] {
            repo.complete_managed_mutation(finished).await.unwrap();
        }
        repo.complete_managed_rollout(&spectator).await.unwrap();
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    }

    for source_exists in [false, true] {
        for child_exists in [false, true] {
            let mut source = owner.after_source.clone();
            let mut child = owner.after_credential.clone();
            if !source_exists {
                source.id = CredentialSourceId("cred:managed-begin-fresh".into());
            }
            if !child_exists {
                child.id = "child-begin-fresh".into();
            }
            child.source_id = source.id.clone();
            let candidate = PendingManagedCredentialMutation::create(source, child).unwrap();
            let result = repo.begin_managed_mutation(candidate.clone()).await;
            if source_exists || child_exists {
                assert!(
                    matches!(result, Err(CredentialError::MutationConflict(_))),
                    "M6"
                );
            } else {
                assert!(result.unwrap(), "M6");
                // This begin-only case wrote no SecretStore material. Retire
                // its exact abort envelope before inspecting the next case.
                let aborted = repo.abort_managed_mutation(&candidate).await.unwrap();
                repo.complete_managed_mutation(&aborted).await.unwrap();
            }
            assert!(
                repo.pending_managed_mutations().await.unwrap().is_empty(),
                "M6"
            );
        }
    }
}

/// Run every suite, each on a fresh repo from `make`.
async fn run_all(make: impl Fn() -> Box<dyn CredentialRepo>) {
    sources_round_trip_and_scope_by_workspace(&*make()).await;
    replacement_lineage_round_trips(&*make()).await;
    pools_round_trip_and_scope_by_workspace(&*make()).await;
    missing_rows_are_not_found(&*make()).await;
    put_is_upsert(&*make()).await;
    worker_local_registration_is_atomic_idempotent_and_secret_free(&*make()).await;
    mutation_wal_publish_and_completion_are_idempotent(&*make()).await;
    completing_unpublished_mutation_is_idempotent(&*make()).await;
    rotation_publication_is_revision_cas_and_idempotent(&*make()).await;
    distinct_replacement_is_old_row_cas_and_idempotent(&*make()).await;
    distinct_replacement_recovery_is_directional(&*make()).await;
    get_is_an_unscoped_by_id_primitive(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryCredentialRepo::new())).await;
    managed_material_mutation_begin_and_phases_conform(&InMemoryCredentialRepo::new()).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_repo_conforms() {
    use awaken_credential_store::sqlite::SqliteCredentialRepo;
    run_all(|| Box::new(SqliteCredentialRepo::open_in_memory().unwrap())).await;
    managed_material_mutation_begin_and_phases_conform(
        &SqliteCredentialRepo::open_in_memory().unwrap(),
    )
    .await;
}

/// Durable-key cause/effect table for the persistent recovery scans. C1 a
/// decoded Ready source or Managed envelope is internally valid; C2 its SQL
/// `source_id` differs from the envelope source id. C1+C2 => one batch storage
/// error, zero SecretStore read/delete, and both repairable rows retained.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_recovery_scans_reject_mismatched_durable_keys_before_secret_io() {
    use awaken_credential_store::sqlite::SqliteCredentialRepo;
    use rusqlite::{Connection, params};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential-key-fence.db");
    let path = path.to_str().unwrap();
    let repo = SqliteCredentialRepo::open(path).unwrap();

    let mut source_after = source("cred:sqlite:key-envelope", "ws");
    source_after.env_key = None;
    source_after.material_ref = Some(SecretRef("sec:sqlite:key-envelope".into()));
    let source_ready = CredentialMutationIntent::prepare(None, source_after)
        .unwrap()
        .with_material_ready()
        .unwrap();

    let mut managed_source = source("cred:sqlite:managed-key-envelope", "ws");
    managed_source.env_key = None;
    managed_source.material_ref = Some(SecretRef("sec:sqlite:managed-key-envelope".into()));
    let managed_child = ManagedVaultCredential {
        id: "child:sqlite:managed-key-envelope".into(),
        vault_id: "vault:sqlite:managed-key-envelope".into(),
        workspace_id: "ws".into(),
        source_id: managed_source.id.clone(),
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
    let managed_ready = PendingManagedCredentialMutation::create(managed_source, managed_child)
        .unwrap()
        .with_material_ready()
        .unwrap();

    let connection = Connection::open(path).unwrap();
    connection
        .execute(
            "INSERT INTO credential_creation_intent (source_id, data) VALUES (?1, ?2)",
            params![
                "wrong-source-row-key",
                serde_json::to_string(&source_ready).unwrap()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO credential_managed_credential_mutation (source_id, data) VALUES (?1, ?2)",
            params![
                "wrong-managed-row-key",
                serde_json::to_string(&managed_ready).unwrap()
            ],
        )
        .unwrap();

    let store = DeleteCountingStore::default();
    assert!(recover_credential_mutations(&store, &repo).await.is_err());
    assert!(
        recover_managed_credential_mutations(&store, &repo)
            .await
            .is_err()
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), 0);
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
    let retained: i64 = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM credential_creation_intent) + \
             (SELECT COUNT(*) FROM credential_managed_credential_mutation)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 2);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_legacy_mutation_recovery_conforms() {
    // Run L1-L4/E1 over actual SQLite JSON decoding and transaction CAS. Raw fixture
    // insertion represents pre-existing storage; no fresh-command bypass is added.
    use awaken_credential_store::sqlite::SqliteCredentialRepo;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy-writer.db");
    let repo = SqliteCredentialRepo::open(path.to_str().unwrap()).unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    for retain_attempt in [false, true] {
        for phase in [
            CredentialMaterialMutationPhase::Writing,
            CredentialMaterialMutationPhase::Ready,
            CredentialMaterialMutationPhase::ReclaimingAbort,
        ] {
            for (table, wire, source_field) in legacy_mutation_wires(retain_attempt, phase) {
                connection
                    .execute(
                        &format!("INSERT INTO {table} (source_id, data) VALUES (?1, ?2)"),
                        rusqlite::params![
                            wire[source_field]["id"].as_str().unwrap(),
                            serde_json::to_string(&wire).unwrap()
                        ],
                    )
                    .unwrap();
            }
            legacy_mutation_recovery_conforms(&repo, retain_attempt).await;
        }
    }
    let (plain, managed) = prepare_root_encoding_updates(&repo).await;
    for (table, wire) in root_encoding_wires(&plain, &managed) {
        assert_eq!(
            connection
                .execute(
                    &format!("UPDATE {table} SET data = ?1 WHERE id = ?2"),
                    rusqlite::params![
                        serde_json::to_string_pretty(&wire).unwrap(),
                        wire["id"].as_str().unwrap()
                    ],
                )
                .unwrap(),
            1
        );
    }
    root_encoding_updates_conform(&repo, plain, managed).await;
}

/// Live Postgres conformance: the same suites as the other backends, each on a
/// fresh schema (so the four independent suites never see each other's rows).
/// Only the default development URL may skip; an explicit fixture must work.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_credential_store::postgres::PostgresCredentialRepo;
    use awaken_credential_vault::catalog::ManagedCredentialMutationError;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use std::collections::BTreeMap;

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        })
    }

    async fn schema_pool(schema: &'static str) -> Option<PgPool> {
        // Pool-budget cause/effect table for this integration-test binary:
        // C1 Rust runs the independent schema tests concurrently; C2 each
        // fixture uses an unconstrained sqlx pool; C3 PostgreSQL has a finite
        // connection budget. C1+C2+C3 previously allowed sibling fixtures to
        // consume the budget and made an unrelated CREATE SCHEMA time out.
        // The admin phase needs exactly one connection; a scenario pool needs
        // at most four (the largest lock/race witness is bounded below that).
        // R1 admin => max 1; R2 scenario => max 4; R3 explicit in-test races
        // remain concurrent and must not be serialized by the harness.
        let admin = match PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url())
            .await
        {
            Ok(pool) => pool,
            Err(err) => {
                // R3: an explicitly configured durable-backend gate must fail,
                // never count an unreachable database as passing coverage.
                assert!(
                    std::env::var_os("AWAKEN_TEST_DATABASE_URL").is_none(),
                    "explicit PostgreSQL fixture is unavailable: {err}"
                );
                println!("[skip] no Postgres reachable: {err}");
                return None;
            }
        };
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        Some(
            PgPoolOptions::new()
                .max_connections(4)
                .after_connect(move |conn, _meta| {
                    Box::pin(async move {
                        conn.execute(format!("SET search_path = {schema}").as_str())
                            .await?;
                        conn.execute(format!("SET application_name = '{schema}'").as_str())
                            .await?;
                        Ok(())
                    })
                })
                .connect(&database_url())
                .await
                .expect("PostgreSQL scenario fixture must initialize after admin connection"),
        )
    }

    async fn repo(schema: &'static str) -> Option<PostgresCredentialRepo> {
        let pool = schema_pool(schema).await?;
        let repo = PostgresCredentialRepo::with_pool(pool)
            .await
            .expect("store");
        Some(repo)
    }

    fn managed_source(id: &str, workspace_id: &str) -> CredentialSource {
        CredentialSource {
            provider_id: None,
            env_key: None,
            ..source(id, workspace_id)
        }
    }

    fn managed_child(
        id: &str,
        vault_id: &str,
        workspace_id: &str,
        source_id: CredentialSourceId,
    ) -> ManagedVaultCredential {
        ManagedVaultCredential {
            id: id.into(),
            vault_id: vault_id.into(),
            workspace_id: workspace_id.into(),
            source_id,
            auth: ManagedCredentialAuth::StaticBearer {
                mcp_server_url: "https://mcp.example.test".into(),
            },
            metadata: BTreeMap::new(),
            display_name: None,
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        }
    }

    fn managed_vault(id: &str, workspace_id: &str) -> ManagedVault {
        ManagedVault {
            id: id.into(),
            workspace_id: workspace_id.into(),
            display_name: id.into(),
            metadata: BTreeMap::new(),
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            archived_at: None,
            deletion: None,
            revision: 1,
        }
    }

    async fn ready_create(
        repo: &PostgresCredentialRepo,
        source: CredentialSource,
        child: ManagedVaultCredential,
    ) -> PendingManagedCredentialMutation {
        // Managed create material-state cause/effect table. The production
        // constructor is the sole phase owner; this fixture may only drive the
        // transition that its result requires.
        //
        // | Rule | new material | initial phase | fixture effect |
        // |---|---|---|---|
        // | MC1 | yes | Writing | exact owner advances once to Ready |
        // | MC2 | no  | Ready   | retain constructor truth; no fake write |
        // | MC3 | either | either | logical begin replay owns no attempt |
        let writing = PendingManagedCredentialMutation::create(source, child).unwrap();
        assert!(repo.begin_managed_mutation(writing.clone()).await.unwrap());
        assert!(!repo.begin_managed_mutation(writing.clone()).await.unwrap());
        if writing.material_fence.phase == CredentialMaterialMutationPhase::Writing {
            repo.mark_managed_mutation_ready(&writing).await.unwrap()
        } else {
            assert_eq!(
                writing.material_fence.phase,
                CredentialMaterialMutationPhase::Ready,
                "MC2 constructor owns the material-free phase"
            );
            writing
        }
    }

    async fn wait_for_lock_waiters(pool: &PgPool, application_name: &str, expected: i64) {
        for _ in 0..200 {
            let waiters = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE application_name = $1 AND wait_event_type = 'Lock'",
            )
            .bind(application_name)
            .fetch_one(pool)
            .await
            .unwrap();
            if waiters >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("writers did not reach the expected PostgreSQL lock boundary");
    }

    #[tokio::test]
    async fn postgres_repo_conforms() {
        let Some(r) = repo("t_cred_sources").await else {
            return;
        };
        sources_round_trip_and_scope_by_workspace(&r).await;
        replacement_lineage_round_trips(&repo("t_cred_lineage").await.unwrap()).await;
        pools_round_trip_and_scope_by_workspace(&repo("t_cred_pools").await.unwrap()).await;
        missing_rows_are_not_found(&repo("t_cred_missing").await.unwrap()).await;
        put_is_upsert(&repo("t_cred_upsert").await.unwrap()).await;
        mutation_wal_publish_and_completion_are_idempotent(
            &repo("t_cred_creation_intent").await.unwrap(),
        )
        .await;
        completing_unpublished_mutation_is_idempotent(&repo("t_cred_abort_intent").await.unwrap())
            .await;
        rotation_publication_is_revision_cas_and_idempotent(
            &repo("t_cred_rotation_cas").await.unwrap(),
        )
        .await;
        distinct_replacement_is_old_row_cas_and_idempotent(
            &repo("t_cred_replacement_cas").await.unwrap(),
        )
        .await;
        distinct_replacement_recovery_is_directional(
            &repo("t_cred_replacement_recovery").await.unwrap(),
        )
        .await;
        managed_material_mutation_begin_and_phases_conform(
            &repo("t_cred_managed_begin_phases").await.unwrap(),
        )
        .await;
        get_is_an_unscoped_by_id_primitive(&repo("t_cred_xtenant").await.unwrap()).await;
    }

    #[tokio::test]
    async fn postgres_legacy_mutation_recovery_conforms() {
        // Same L1-L4/E1 table, including PostgreSQL jsonb decoding and locked CAS.
        let Some(pool) = schema_pool("t_cred_legacy_writer").await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .unwrap();
        for retain_attempt in [false, true] {
            for phase in [
                CredentialMaterialMutationPhase::Writing,
                CredentialMaterialMutationPhase::Ready,
                CredentialMaterialMutationPhase::ReclaimingAbort,
            ] {
                for (table, wire, source_field) in legacy_mutation_wires(retain_attempt, phase) {
                    sqlx::query(&format!(
                        "INSERT INTO {table} (source_id, data) VALUES ($1, $2)"
                    ))
                    .bind(wire[source_field]["id"].as_str().unwrap())
                    .bind(sqlx::types::Json(&wire))
                    .execute(&pool)
                    .await
                    .unwrap();
                }
                legacy_mutation_recovery_conforms(&repo, retain_attempt).await;
            }
        }
        let (plain, managed) = prepare_root_encoding_updates(&repo).await;
        for (table, wire) in root_encoding_wires(&plain, &managed) {
            assert_eq!(
                sqlx::query(&format!("UPDATE {table} SET data = $1 WHERE id = $2"))
                    .bind(sqlx::types::Json(&wire))
                    .bind(wire["id"].as_str().unwrap())
                    .execute(&pool)
                    .await
                    .unwrap()
                    .rows_affected(),
                1
            );
        }
        root_encoding_updates_conform(&repo, plain, managed).await;
    }

    /// Same durable-key rule as the SQLite cause/effect test, executed against
    /// PostgreSQL JSON rows: valid Ready payload + foreign SQL key => batch
    /// error, zero SecretStore I/O, both rows retained.
    #[tokio::test]
    async fn postgres_recovery_scans_reject_mismatched_durable_keys_before_secret_io() {
        use sqlx::types::Json;

        const SCHEMA: &str = "t_cred_recovery_key_fence";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let mut source_after = source("cred:pg:key-envelope", "ws");
        source_after.env_key = None;
        source_after.material_ref = Some(SecretRef("sec:pg:key-envelope".into()));
        let source_ready = CredentialMutationIntent::prepare(None, source_after)
            .unwrap()
            .with_material_ready()
            .unwrap();
        let mut managed_source = source("cred:pg:managed-key-envelope", "ws");
        managed_source.env_key = None;
        managed_source.material_ref = Some(SecretRef("sec:pg:managed-key-envelope".into()));
        let managed_child = managed_child(
            "child:pg:managed-key-envelope",
            "vault:pg:managed-key-envelope",
            "ws",
            managed_source.id.clone(),
        );
        let managed_ready = PendingManagedCredentialMutation::create(managed_source, managed_child)
            .unwrap()
            .with_material_ready()
            .unwrap();

        sqlx::query("INSERT INTO credential_creation_intent (source_id, data) VALUES ($1, $2)")
            .bind("wrong-source-row-key")
            .bind(Json(&source_ready))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO credential_managed_credential_mutation (source_id, data) VALUES ($1, $2)",
        )
        .bind("wrong-managed-row-key")
        .bind(Json(&managed_ready))
        .execute(&pool)
        .await
        .unwrap();

        let store = DeleteCountingStore::default();
        assert!(recover_credential_mutations(&store, &repo).await.is_err());
        assert!(
            recover_managed_credential_mutations(&store, &repo)
                .await
                .is_err()
        );
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
        let retained = sqlx::query_scalar::<_, i64>(
            "SELECT (SELECT COUNT(*) FROM credential_creation_intent) + \
             (SELECT COUNT(*) FROM credential_managed_credential_mutation)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained, 2);
    }

    /// PostgreSQL recovery decode cause/effect table. C1 each source and
    /// Managed table contains one healthy envelope plus one valid-JSON row that
    /// cannot decode as the current envelope. E1 each scan fails as a batch
    /// before SecretStore I/O, and E2 all four rows remain durable for operator
    /// repair. This is the PostgreSQL counterpart of the SQLite batch boundary.
    #[tokio::test]
    async fn postgres_undecodable_recovery_row_blocks_each_batch_before_secret_io() {
        use sqlx::types::Json;

        const SCHEMA: &str = "t_cred_recovery_decode_fence";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let mut source_after = source("cred:pg:decode-healthy", "ws");
        source_after.env_key = None;
        source_after.material_ref = Some(SecretRef("sec:pg:decode-healthy".into()));
        let source_writing = CredentialMutationIntent::prepare(None, source_after).unwrap();
        let mut managed_source = source("cred:pg:managed-decode-healthy", "ws");
        managed_source.env_key = None;
        managed_source.material_ref = Some(SecretRef("sec:pg:managed-decode-healthy".into()));
        let managed_child = managed_child(
            "crd-pg-managed-decode-healthy",
            "vlt-pg-managed-decode-healthy",
            "ws",
            managed_source.id.clone(),
        );
        let managed_writing =
            PendingManagedCredentialMutation::create(managed_source, managed_child).unwrap();
        let undecodable = serde_json::json!({"format_version": u64::MAX});

        sqlx::query(
            "INSERT INTO credential_creation_intent (source_id, data) VALUES ($1, $2), ($3, $4)",
        )
        .bind(&source_writing.after.id.0)
        .bind(Json(&source_writing))
        .bind("cred:pg:decode-undecodable")
        .bind(Json(&undecodable))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO credential_managed_credential_mutation (source_id, data) \
             VALUES ($1, $2), ($3, $4)",
        )
        .bind(&managed_writing.after_source.id.0)
        .bind(Json(&managed_writing))
        .bind("cred:pg:managed-decode-undecodable")
        .bind(Json(&undecodable))
        .execute(&pool)
        .await
        .unwrap();

        let store = DeleteCountingStore::default();
        assert!(recover_credential_mutations(&store, &repo).await.is_err());
        assert!(
            recover_managed_credential_mutations(&store, &repo)
                .await
                .is_err()
        );
        assert_eq!(store.reads.load(Ordering::SeqCst), 0, "E1");
        assert_eq!(store.deletes.load(Ordering::SeqCst), 0, "E1");
        let retained = sqlx::query_scalar::<_, i64>(
            "SELECT (SELECT COUNT(*) FROM credential_creation_intent) + \
             (SELECT COUNT(*) FROM credential_managed_credential_mutation)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained, 4, "E2");
    }

    /// PostgreSQL unique-key race rule. C1 two fresh physical attempts carry
    /// the same logical create and cross the start barrier together. E1 exactly
    /// one insert owns the SecretStore attempt, the loser attaches as
    /// non-owner, and one durable WAL row remains. This is the concurrent form
    /// of the sequential begin conformance rule.
    #[tokio::test]
    async fn postgres_concurrent_begin_has_one_physical_attempt_owner() {
        use std::sync::Arc;

        const SCHEMA: &str = "t_cred_concurrent_begin";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = Arc::new(
            PostgresCredentialRepo::with_pool(pool)
                .await
                .expect("store"),
        );
        let mut after = source("cred:pg:concurrent-begin", "ws");
        after.env_key = None;
        after.material_ref = Some(SecretRef("sec:pg:concurrent-begin".into()));
        let left = CredentialMutationIntent::prepare(None, after.clone()).unwrap();
        let right = CredentialMutationIntent::prepare(None, after).unwrap();
        assert_ne!(
            left.material_fence.attempt_id(),
            right.material_fence.attempt_id()
        );
        let start = Arc::new(tokio::sync::Barrier::new(3));
        let left_task = {
            let repo = repo.clone();
            let start = start.clone();
            tokio::spawn(async move {
                start.wait().await;
                repo.begin_mutation(left).await
            })
        };
        let right_task = {
            let repo = repo.clone();
            let start = start.clone();
            tokio::spawn(async move {
                start.wait().await;
                repo.begin_mutation(right).await
            })
        };
        start.wait().await;
        let mut ownership = vec![
            left_task.await.unwrap().unwrap(),
            right_task.await.unwrap().unwrap(),
        ];
        ownership.sort_unstable();
        assert_eq!(ownership, vec![false, true], "E1");
        assert_eq!(repo.pending_mutations().await.unwrap().len(), 1, "E1");
    }

    /// Managed PostgreSQL unique-key race rule. C1 two fresh physical attempts
    /// carry the same logical source+child create and cross the start barrier
    /// together. E1 exactly one insert owns the SecretStore attempt, the loser
    /// attaches as non-owner, and one exact Managed WAL envelope remains. This
    /// freezes the independent Managed table path, not only the source WAL.
    #[tokio::test]
    async fn postgres_concurrent_managed_begin_has_one_physical_attempt_owner() {
        use std::sync::Arc;

        const SCHEMA: &str = "t_cred_managed_concurrent_begin";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = Arc::new(
            PostgresCredentialRepo::with_pool(pool)
                .await
                .expect("store"),
        );
        let mut after_source = managed_source("cred:pg:managed-concurrent-begin", "ws");
        after_source.material_ref = Some(SecretRef("sec:pg:managed-concurrent-begin".into()));
        let after_child = managed_child(
            "crd-pg-managed-concurrent-begin",
            "vlt-pg-managed-concurrent-begin",
            "ws",
            after_source.id.clone(),
        );
        let left =
            PendingManagedCredentialMutation::create(after_source.clone(), after_child.clone())
                .unwrap();
        let right = PendingManagedCredentialMutation::create(after_source, after_child).unwrap();
        assert_ne!(
            left.material_fence.attempt_id(),
            right.material_fence.attempt_id()
        );
        let start = Arc::new(tokio::sync::Barrier::new(3));
        let left_task = {
            let repo = repo.clone();
            let start = start.clone();
            tokio::spawn(async move {
                start.wait().await;
                repo.begin_managed_mutation(left).await
            })
        };
        let right_task = {
            let repo = repo.clone();
            let start = start.clone();
            tokio::spawn(async move {
                start.wait().await;
                repo.begin_managed_mutation(right).await
            })
        };
        start.wait().await;
        let mut ownership = vec![
            left_task.await.unwrap().unwrap(),
            right_task.await.unwrap().unwrap(),
        ];
        ownership.sort_unstable();
        assert_eq!(ownership, vec![false, true], "E1");
        assert_eq!(
            repo.pending_managed_mutations().await.unwrap().len(),
            1,
            "E1"
        );
    }

    /// Exact rollout lookup cause/effect table. C1 a non-create mutation commits
    /// its rollout through the production pair transaction; C2 the queried
    /// primary id is present or absent; C3 the durable JSON is decodable or
    /// malformed. Effects are E1 the exact event, E2 `None`, and E3 a storage
    /// error while the poison row remains repairable.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | MR-PG1 | committed | present | valid | E1 exact event |
    /// | MR-PG2 | committed | absent | n/a | E2 `None` |
    /// | MR-PG3 | independent row | present | malformed | E3 error, retain row |
    #[tokio::test]
    async fn postgres_managed_rollout_exact_lookup_conforms() {
        use awaken_credential_vault::InMemorySecretStore;
        use awaken_credential_vault::repo::{CredentialMaterialPatch, update_managed_credential};

        const SCHEMA: &str = "t_cred_managed_rollout_lookup";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let vault = managed_vault("vault-rollout", "ws");
        repo.insert_vault("ws", vault.clone()).await.unwrap();
        let before_source = managed_source("cred:rollout", "ws");
        let before_child =
            managed_child("child-rollout", &vault.id, "ws", before_source.id.clone());
        let ready = ready_create(&repo, before_source.clone(), before_child.clone()).await;
        let reclaiming = repo.commit_managed_mutation(&ready).await.unwrap();
        repo.complete_managed_mutation(&reclaiming).await.unwrap();

        let mut after_child = before_child.clone();
        after_child.display_name = Some("updated".into());
        let after_child = update_managed_credential(
            before_child,
            after_child,
            CredentialMaterialPatch::default(),
            true,
            "2026-09-17T00:00:01Z".into(),
            &InMemorySecretStore::new(),
            &repo,
        )
        .await
        .unwrap();
        let events = repo.pending_managed_rollouts().await.unwrap();
        assert_eq!(events.len(), 1, "MR-PG1");
        let event = events
            .into_iter()
            .next()
            .expect("MR-PG1 production commit publishes rollout");
        assert_eq!(event.source_version, 2, "MR-PG1");
        assert_eq!(event.credential_revision, after_child.revision, "MR-PG1");
        let event_id = event.id.clone();
        assert_eq!(
            repo.managed_rollout(&event_id).await.unwrap(),
            Some(event),
            "MR-PG1"
        );
        assert_eq!(
            repo.managed_rollout("managed-update:missing")
                .await
                .unwrap(),
            None,
            "MR-PG2"
        );

        const POISON_ID: &str = "managed-update:malformed";
        sqlx::query(
            "INSERT INTO credential_managed_credential_rollout (event_id, data) \
             VALUES ($1, $2::jsonb)",
        )
        .bind(POISON_ID)
        .bind(r#"{"format_version":2}"#)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            matches!(
                repo.managed_rollout(POISON_ID).await,
                Err(CredentialError::Storage(_))
            ),
            "MR-PG3"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM credential_managed_credential_rollout WHERE event_id = $1",
            )
            .bind(POISON_ID)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1,
            "MR-PG3 poison remains available for repair"
        );
    }

    #[tokio::test]
    async fn postgres_managed_absent_child_cas_has_one_atomic_winner() {
        const SCHEMA: &str = "t_cred_managed_absent_cas";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let vault = managed_vault("vault-shared", "ws");
        repo.insert_vault("ws", vault.clone()).await.unwrap();

        let left_source = managed_source("cred:left", "ws");
        let right_source = managed_source("cred:right", "ws");
        let left = ready_create(
            &repo,
            left_source.clone(),
            managed_child("child-shared", &vault.id, "ws", left_source.id.clone()),
        )
        .await;
        let right = ready_create(
            &repo,
            right_source.clone(),
            managed_child("child-shared", &vault.id, "ws", right_source.id.clone()),
        )
        .await;

        // Hold the root so both writers queue at the aggregate's first lock.
        // Once released, the second writer must re-read the first writer's child,
        // not continue from an absent-row snapshot and overwrite it.
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM credential_managed_vault WHERE id = $1 FOR UPDATE")
            .bind(&vault.id)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        let left_repo = repo.clone();
        let left_pending = left.clone();
        let left_task =
            tokio::spawn(async move { left_repo.commit_managed_mutation(&left_pending).await });
        let right_repo = repo.clone();
        let right_pending = right.clone();
        let right_task =
            tokio::spawn(async move { right_repo.commit_managed_mutation(&right_pending).await });
        wait_for_lock_waiters(&pool, SCHEMA, 2).await;
        blocker.commit().await.unwrap();

        let results = [left_task.await.unwrap(), right_task.await.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(ManagedCredentialMutationError::RevisionConflict)
                ))
                .count(),
            1
        );
        let winner = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .expect("one winner");
        let durable_child = repo
            .get_vault_credential("ws", "child-shared")
            .await
            .unwrap()
            .expect("winner child");
        assert_eq!(durable_child, winner.after_credential);
        assert_eq!(
            repo.get(&winner.after_source.id).await.unwrap(),
            winner.after_source
        );
        let loser_id = if winner.after_source.id == left_source.id {
            right_source.id
        } else {
            left_source.id
        };
        assert!(matches!(
            repo.get(&loser_id).await,
            Err(CredentialError::SourceNotFound(_))
        ));
    }

    #[tokio::test]
    async fn postgres_managed_absent_child_cas_never_cross_workspace_overwrites() {
        const SCHEMA: &str = "t_cred_managed_cross_workspace_cas";
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        repo.insert_vault("ws-a", managed_vault("vault-a", "ws-a"))
            .await
            .unwrap();
        repo.insert_vault("ws-b", managed_vault("vault-b", "ws-b"))
            .await
            .unwrap();
        let left_source = managed_source("cred:cross-left", "ws-a");
        let right_source = managed_source("cred:cross-right", "ws-b");
        let left = ready_create(
            &repo,
            left_source.clone(),
            managed_child("child-global", "vault-a", "ws-a", left_source.id.clone()),
        )
        .await;
        let right = ready_create(
            &repo,
            right_source.clone(),
            managed_child("child-global", "vault-b", "ws-b", right_source.id.clone()),
        )
        .await;

        // Different roots cannot serialize the globally unique child id for us.
        // Holding both roots forces the old child-before-root ordering to retain
        // two absent snapshots; root-first plus the child INSERT CAS must still
        // admit only one workspace after both roots are released.
        let mut blocker = pool.begin().await.unwrap();
        for vault_id in ["vault-a", "vault-b"] {
            sqlx::query("SELECT id FROM credential_managed_vault WHERE id = $1 FOR UPDATE")
                .bind(vault_id)
                .fetch_one(&mut *blocker)
                .await
                .unwrap();
        }
        let left_repo = repo.clone();
        let left_pending = left.clone();
        let left_task =
            tokio::spawn(async move { left_repo.commit_managed_mutation(&left_pending).await });
        let right_repo = repo.clone();
        let right_pending = right.clone();
        let right_task =
            tokio::spawn(async move { right_repo.commit_managed_mutation(&right_pending).await });
        wait_for_lock_waiters(&pool, SCHEMA, 2).await;
        blocker.commit().await.unwrap();
        let (left_result, right_result) = (left_task.await.unwrap(), right_task.await.unwrap());
        let results = [left_result, right_result];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .expect("one winner");
        assert_eq!(
            repo.get_vault_credential(
                &winner.after_credential.workspace_id,
                &winner.after_credential.id,
            )
            .await
            .unwrap(),
            Some(winner.after_credential.clone())
        );
        assert_eq!(
            repo.get(&winner.after_source.id).await.unwrap(),
            winner.after_source
        );
        assert_eq!(
            repo.list("ws-a").await.unwrap().len() + repo.list("ws-b").await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn postgres_managed_abort_retains_exact_cleanup_authority_until_completion() {
        let Some(pool) = schema_pool("t_cred_managed_abort_cleanup").await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let source = managed_source("cred:abort-cleanup", "ws");
        let writing = PendingManagedCredentialMutation::create(
            source.clone(),
            managed_child(
                "child-abort-cleanup",
                "vault-abort-cleanup",
                "ws",
                source.id,
            ),
        )
        .unwrap();
        repo.begin_managed_mutation(writing.clone()).await.unwrap();

        let reclaiming = repo.abort_managed_mutation(&writing).await.unwrap();
        assert_eq!(
            reclaiming.material_fence.phase,
            CredentialMaterialMutationPhase::ReclaimingAbort
        );
        assert_eq!(
            repo.pending_managed_mutations().await.unwrap(),
            vec![reclaiming.clone()]
        );
        assert!(matches!(
            repo.abort_managed_mutation(&writing).await,
            Err(CredentialError::MutationConflict(_))
        ));

        repo.put(reclaiming.after_source.clone()).await.unwrap();
        assert!(matches!(
            repo.complete_managed_mutation(&reclaiming).await,
            Err(CredentialError::MutationConflict(_))
        ));
        sqlx::query("DELETE FROM credential_source WHERE id = $1")
            .bind(&reclaiming.after_source.id.0)
            .execute(&pool)
            .await
            .unwrap();

        // A restarted worker resumes from the exact durable cleanup fact. The
        // fact disappears only after completion verifies the unpublished
        // before-pair truth.
        repo.complete_managed_mutation(&reclaiming).await.unwrap();
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        assert!(matches!(
            repo.get(&reclaiming.after_source.id).await,
            Err(CredentialError::SourceNotFound(_))
        ));
        assert_eq!(
            repo.get_vault_credential("ws", &reclaiming.after_credential.id)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn postgres_plain_absent_source_cas_does_not_overwrite_concurrent_winner() {
        const SCHEMA: &str = "t_cred_plain_absent_cas";
        const ADVISORY_KEY: i64 = 8_216_041;
        let Some(pool) = schema_pool(SCHEMA).await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let mut proposed = source("cred:plain-race", "ws-proposed");
        proposed.provider_id = Some("blocked-proposal".into());
        let intent = CredentialMutationIntent::prepare(None, proposed.clone()).unwrap();
        repo.begin_mutation(intent.clone()).await.unwrap();

        sqlx::query(
            "CREATE FUNCTION block_proposed_source() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF NEW.data->>'provider_id' = 'blocked-proposal' THEN \
                 PERFORM pg_advisory_xact_lock(8216041); \
               END IF; \
               RETURN NEW; \
             END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER block_proposed_source_before_insert \
             BEFORE INSERT ON credential_source FOR EACH ROW \
             EXECUTE FUNCTION block_proposed_source()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut blocker = tokio::time::timeout(std::time::Duration::from_secs(30), pool.acquire())
            .await
            .expect("timed out waiting for pool.acquire()")
            .unwrap();
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(ADVISORY_KEY)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let apply_repo = repo.clone();
        let apply_intent = intent.clone();
        let apply_task =
            tokio::spawn(async move { apply_repo.apply_mutation(&apply_intent).await });
        wait_for_lock_waiters(&pool, SCHEMA, 1).await;

        let mut winner = source("cred:plain-race", "ws-winner");
        winner.provider_id = Some("winner".into());
        repo.put(winner.clone()).await.unwrap();
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(ADVISORY_KEY)
            .execute(&mut *blocker)
            .await
            .unwrap();

        assert!(matches!(
            apply_task.await.unwrap(),
            Err(CredentialError::MutationConflict(_))
        ));
        assert_eq!(repo.get(&winner.id).await.unwrap(), winner);
    }

    /// The durable secret path on Postgres: AEAD sealing composed over the
    /// Postgres blob store. Deliberately no bare (plaintext) SecretStore exists.
    #[cfg(feature = "sealed-aead")]
    #[tokio::test]
    async fn postgres_sealed_secret_round_trips_and_never_stores_plaintext() {
        use std::sync::Arc;

        use awaken_agent_contract::RedactedString;
        use awaken_credential_store::{PostgresSealedBlobStore, SealedAeadSecretStore};
        use awaken_credential_vault::{
            CredentialCreateParams, SecretStore, create_source, materialize,
        };

        let Some(pool) = schema_pool("t_cred_sealed").await else {
            return;
        };
        const KEY: [u8; 32] = [7u8; 32];
        let blob = Arc::new(
            PostgresSealedBlobStore::with_pool(pool.clone())
                .await
                .expect("blob store"),
        );
        let store = SealedAeadSecretStore::over(&KEY, blob);

        let row = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-super-secret-value")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();

        // Materializes back through the AEAD layer…
        assert_eq!(
            materialize(&row, &store).await.unwrap().expose_secret(),
            "sk-super-secret-value"
        );

        // …and the at-rest bytea column never contains the plaintext.
        let secret_ref = row.material_ref.clone().unwrap();
        let sealed: Vec<u8> =
            sqlx::query_scalar("SELECT sealed FROM credential_secret WHERE secret_ref = $1")
                .bind(&secret_ref.0)
                .fetch_one(&pool)
                .await
                .unwrap();
        let plaintext = b"sk-super-secret-value";
        assert!(!sealed.windows(plaintext.len()).any(|w| w == plaintext));

        // A wrong key fails closed.
        let wrong = SealedAeadSecretStore::over(
            &[8u8; 32],
            Arc::new(PostgresSealedBlobStore::with_pool(pool).await.unwrap()),
        );
        assert!(matches!(
            wrong.get(&secret_ref).await,
            Err(awaken_credential_vault::CredentialError::Seal)
        ));
    }
}
