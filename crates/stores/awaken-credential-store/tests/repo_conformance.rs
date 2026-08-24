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

use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::{
    CredentialMutationIntent, CredentialRepo, InMemoryCredentialRepo, ensure_worker_local,
};
use awaken_credential_vault::{
    CredentialError, CredentialKind, CredentialPool, CredentialPoolId, CredentialPoolMember,
    CredentialSource, CredentialStatus, SecretRef, SelectionPolicy, WorkerLocalBinding,
};

fn source(id: &str, ws: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        workspace_id: ws.into(),
        kind: CredentialKind::Vault,
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
/// R1 same pending intent => begin is idempotent; R2 current==before => publish
/// after while retaining WAL; R3 current==after => apply is idempotent; R4
/// completion => WAL is removed idempotently. Cleanup deliberately sits between
/// R3 and R4, so a process crash cannot hide unreclaimed material.
async fn mutation_wal_publish_and_completion_are_idempotent(repo: &dyn CredentialRepo) {
    let source = source("cred:intent", "ws");
    let intent = CredentialMutationIntent {
        before: None,
        after: source.clone(),
    };

    repo.begin_mutation(intent.clone()).await.unwrap();
    repo.begin_mutation(intent.clone()).await.unwrap();
    assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);
    assert!(matches!(
        repo.get(&source.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));

    repo.apply_mutation(&intent).await.unwrap();
    repo.apply_mutation(&intent).await.unwrap();
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert_eq!(repo.pending_mutations().await.unwrap().len(), 1);

    repo.complete_mutation(&source.id).await.unwrap();
    repo.complete_mutation(&source.id).await.unwrap();
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert!(repo.pending_mutations().await.unwrap().is_empty());
}

/// Cause/effect rule R5: completing a mutation before publication removes only
/// its WAL record; no source row is synthesized, and repeating completion is safe.
async fn completing_unpublished_mutation_is_idempotent(repo: &dyn CredentialRepo) {
    let source = source("cred:abort", "ws");
    repo.begin_mutation(CredentialMutationIntent {
        before: None,
        after: source.clone(),
    })
    .await
    .unwrap();
    repo.complete_mutation(&source.id).await.unwrap();
    repo.complete_mutation(&source.id).await.unwrap();
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
    let exact = CredentialMutationIntent {
        before: Some(before.clone()),
        after: after.clone(),
    };
    repo.begin_mutation(exact.clone()).await.unwrap();
    repo.apply_mutation(&exact).await.unwrap();
    assert_eq!(repo.get(&after.id).await.unwrap(), after, "R6");
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        vec![exact.clone()],
        "R6"
    );
    repo.apply_mutation(&exact).await.unwrap();
    assert_eq!(repo.get(&after.id).await.unwrap(), after, "R7");
    repo.complete_mutation(&after.id).await.unwrap();

    let mut stale_after = before.clone();
    stale_after.version = 2;
    stale_after.material_ref = Some(SecretRef("sec:rotation:stale:primary".into()));
    let stale = CredentialMutationIntent {
        before: Some(before),
        after: stale_after,
    };
    repo.begin_mutation(stale.clone()).await.unwrap();
    assert!(
        matches!(
            repo.apply_mutation(&stale).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R8"
    );
    assert_eq!(repo.get(&after.id).await.unwrap(), after, "R8");
    repo.complete_mutation(&after.id).await.unwrap();
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

/// Run every suite, each on a fresh repo from `make`.
async fn run_all(make: impl Fn() -> Box<dyn CredentialRepo>) {
    sources_round_trip_and_scope_by_workspace(&*make()).await;
    pools_round_trip_and_scope_by_workspace(&*make()).await;
    missing_rows_are_not_found(&*make()).await;
    put_is_upsert(&*make()).await;
    worker_local_registration_is_atomic_idempotent_and_secret_free(&*make()).await;
    mutation_wal_publish_and_completion_are_idempotent(&*make()).await;
    completing_unpublished_mutation_is_idempotent(&*make()).await;
    rotation_publication_is_revision_cas_and_idempotent(&*make()).await;
    get_is_an_unscoped_by_id_primitive(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryCredentialRepo::new())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_repo_conforms() {
    use awaken_credential_store::sqlite::SqliteCredentialRepo;
    run_all(|| Box::new(SqliteCredentialRepo::open_in_memory().unwrap())).await;
}

/// Live Postgres conformance: the same suites as the other backends, each on a
/// fresh schema (so the four independent suites never see each other's rows).
/// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`).
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_credential_store::postgres::PostgresCredentialRepo;
    use awaken_credential_vault::catalog::{
        ManagedCredentialAuth, ManagedCredentialLifecycle, ManagedCredentialMutationError,
        ManagedVault, ManagedVaultCredential, ManagedVaultRepo,
    };
    use awaken_credential_vault::repo::{
        ManagedCredentialMutationPhase, ManagedCredentialRepository,
        PendingManagedCredentialMutation,
    };
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
            workspace_id: workspace_id.into(),
            kind: CredentialKind::Vault,
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
        let writing = PendingManagedCredentialMutation::create(source, child).unwrap();
        repo.begin_managed_mutation(writing.clone()).await.unwrap();
        repo.mark_managed_mutation_ready(&writing).await.unwrap()
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
        get_is_an_unscoped_by_id_primitive(&repo("t_cred_xtenant").await.unwrap()).await;
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
        let Some(pool) = schema_pool("t_cred_managed_writer_claim").await else {
            return;
        };
        let repo = PostgresCredentialRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let source = managed_source("cred:claim", "ws");
        let writing = PendingManagedCredentialMutation::create(
            source.clone(),
            managed_child("child-claim", "vault-claim", "ws", source.id),
        )
        .unwrap();
        repo.begin_managed_mutation(writing.clone()).await.unwrap();
        let now = writing.writer_lease_expires_at_unix_ms;
        let deadline = now.checked_add(10_000).unwrap();

        let (left, right) = tokio::join!(
            repo.claim_expired_managed_mutation(&writing, now, deadline),
            repo.claim_expired_managed_mutation(&writing, now, deadline)
        );
        let claims = [left.unwrap(), right.unwrap()];
        assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
        let claimed = claims.into_iter().flatten().next().unwrap();
        assert_eq!(claimed.writer_epoch, writing.writer_epoch + 1);
        assert_ne!(claimed.writer_token, writing.writer_token);
        assert_eq!(claimed.writer_lease_expires_at_unix_ms, deadline);
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
            reclaiming.phase,
            ManagedCredentialMutationPhase::ReclaimingAbort
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
            reclaiming.phase,
            ManagedCredentialMutationPhase::ReclaimingAbort
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
        let intent = CredentialMutationIntent {
            before: None,
            after: proposed.clone(),
        };
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
