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
async fn managed_material_mutation_begin_and_phases_conform(
    repo: &dyn ManagedCredentialRepository,
) {
    let vault = ManagedVault {
        id: "vault-begin-owner".into(),
        workspace_id: "ws".into(),
        display_name: "vault-begin-owner".into(),
        metadata: Default::default(),
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

/// Live Postgres conformance: the same suites as the other backends, each on a
/// fresh schema (so the four independent suites never see each other's rows).
/// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`).
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
        let admin = match PgPool::connect(&database_url()).await {
            Ok(pool) => pool,
            Err(err) => {
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
        PgPoolOptions::new()
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
            .ok()
    }

    async fn repo(schema: &'static str) -> Option<PostgresCredentialRepo> {
        let pool = schema_pool(schema).await?;
        Some(
            PostgresCredentialRepo::with_pool(pool)
                .await
                .expect("store"),
        )
    }

    fn managed_source(id: &str, workspace_id: &str) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId(id.into()),
            replacement_of: None,
            workspace_id: workspace_id.into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: BTreeMap::new(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
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
    async fn postgres_expired_writer_claim_is_exact_snapshot_cas() {
        // Writer-claim cause/effect table: a material-bearing command starts in
        // Writing (C1); two recovery callers present the same expired snapshot
        // (C2). Exactly one claims epoch N+1 (E1), the other observes the CAS
        // loss (E2), and the former owner cannot mutate or abort afterward (E3).
        let Some(pool) = schema_pool("t_cred_managed_writer_claim").await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let mut source = managed_source("cred:claim", "ws");
        source.material_ref = Some(SecretRef("sec:claim".into()));
        let writing = PendingManagedCredentialMutation::create(
            source.clone(),
            managed_child("child-claim", "vault-claim", "ws", source.id),
        )
        .unwrap();
        assert_eq!(
            writing.material_fence.phase,
            CredentialMaterialMutationPhase::Writing,
            "C1 material-bearing create owns a writer lease"
        );
        repo.begin_managed_mutation(writing.clone()).await.unwrap();
        let now = writing.material_fence.writer_lease_expires_at_unix_ms;
        let deadline = now.checked_add(10_000).unwrap();

        let (left, right) = tokio::join!(
            repo.claim_expired_managed_mutation(&writing, now, deadline),
            repo.claim_expired_managed_mutation(&writing, now, deadline)
        );
        let claims = [left.unwrap(), right.unwrap()];
        assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
        let claimed = claims.into_iter().flatten().next().unwrap();
        assert_eq!(
            claimed.material_fence.writer_epoch,
            writing.material_fence.writer_epoch + 1
        );
        assert_ne!(
            claimed.material_fence.writer_token,
            writing.material_fence.writer_token
        );
        assert_eq!(
            claimed.material_fence.writer_lease_expires_at_unix_ms,
            deadline
        );
        assert_eq!(
            repo.pending_managed_mutations().await.unwrap(),
            vec![claimed.clone()]
        );
        assert!(matches!(
            repo.abort_managed_mutation(&writing).await,
            Err(CredentialError::MutationConflict(_))
        ));
        let reclaiming = repo.abort_managed_mutation(&claimed).await.unwrap();
        assert_eq!(
            reclaiming.material_fence.phase,
            CredentialMaterialMutationPhase::ReclaimingAbort
        );
        repo.complete_managed_mutation(&reclaiming).await.unwrap();
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
        let mut blocker = pool.acquire().await.unwrap();
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
