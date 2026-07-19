//! Conformance suite for [`CredentialRepo`]: the same behavioral assertions run
//! against the in-memory repo and (feature `sqlite`) the sqlite repo, so both
//! backends keep identical semantics — workspace-scoped lists, upsert puts, and
//! the exact NotFound arms.

use awaken_credential_vault::repo::{
    CredentialCreationIntent, CredentialRepo, InMemoryCredentialRepo,
};
use awaken_credential_vault::{
    CredentialError, CredentialKind, CredentialPool, CredentialPoolId, CredentialPoolMember,
    CredentialSource, CredentialSourceId, CredentialStatus, SelectionPolicy,
};

fn source(id: &str, ws: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        workspace_id: ws.into(),
        kind: CredentialKind::Vault,
        provider_id: Some("anthropic".into()),
        env_key: Some("ANTHROPIC_API_KEY".into()),
        material_ref: None,
        oauth_command: None,
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

/// Publishing metadata and retiring the durable creation intent is one repository
/// transaction. A successful return may never expose both the source and its old
/// pending intent, and replaying the commit must remain harmless.
async fn creation_intent_commit_is_atomic_and_idempotent(repo: &dyn CredentialRepo) {
    let source = source("cred:intent", "ws");
    let intent = CredentialCreationIntent {
        source: source.clone(),
    };

    repo.begin_creation(intent.clone()).await.unwrap();
    repo.begin_creation(intent).await.unwrap();
    assert_eq!(repo.pending_creations().await.unwrap().len(), 1);
    assert!(matches!(
        repo.get(&source.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));

    repo.commit_creation(source.clone()).await.unwrap();
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert!(repo.pending_creations().await.unwrap().is_empty());

    repo.commit_creation(source.clone()).await.unwrap();
    assert_eq!(repo.get(&source.id).await.unwrap(), source);
    assert!(repo.pending_creations().await.unwrap().is_empty());
}

async fn abort_creation_is_idempotent(repo: &dyn CredentialRepo) {
    let source = source("cred:abort", "ws");
    repo.begin_creation(CredentialCreationIntent {
        source: source.clone(),
    })
    .await
    .unwrap();
    repo.abort_creation(&source.id).await.unwrap();
    repo.abort_creation(&source.id).await.unwrap();
    assert!(repo.pending_creations().await.unwrap().is_empty());
    assert!(matches!(
        repo.get(&source.id).await,
        Err(CredentialError::SourceNotFound(_))
    ));
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
    creation_intent_commit_is_atomic_and_idempotent(&*make()).await;
    abort_creation_is_idempotent(&*make()).await;
    get_is_an_unscoped_by_id_primitive(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryCredentialRepo::new())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_repo_conforms() {
    use awaken_credential_vault::sqlite::SqliteCredentialRepo;
    run_all(|| Box::new(SqliteCredentialRepo::open_in_memory().unwrap())).await;
}

/// Live Postgres conformance: the same suites as the other backends, each on a
/// fresh schema (so the four independent suites never see each other's rows).
/// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`).
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_credential_vault::postgres::PostgresCredentialRepo;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

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

    #[tokio::test]
    async fn postgres_repo_conforms() {
        let Some(r) = repo("t_cred_sources").await else {
            return;
        };
        sources_round_trip_and_scope_by_workspace(&r).await;
        pools_round_trip_and_scope_by_workspace(&repo("t_cred_pools").await.unwrap()).await;
        missing_rows_are_not_found(&repo("t_cred_missing").await.unwrap()).await;
        put_is_upsert(&repo("t_cred_upsert").await.unwrap()).await;
        creation_intent_commit_is_atomic_and_idempotent(
            &repo("t_cred_creation_intent").await.unwrap(),
        )
        .await;
        abort_creation_is_idempotent(&repo("t_cred_abort_intent").await.unwrap()).await;
        get_is_an_unscoped_by_id_primitive(&repo("t_cred_xtenant").await.unwrap()).await;
    }

    /// The durable secret path on Postgres: AEAD sealing composed over the
    /// Postgres blob store. Deliberately no bare (plaintext) SecretStore exists.
    #[cfg(feature = "sealed-aead")]
    #[tokio::test]
    async fn postgres_sealed_secret_round_trips_and_never_stores_plaintext() {
        use std::sync::Arc;

        use awaken_agent_contract::RedactedString;
        use awaken_credential_vault::postgres::PostgresSealedBlobStore;
        use awaken_credential_vault::{
            CredentialCreateParams, SealedAeadSecretStore, SecretStore, create_source, materialize,
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
