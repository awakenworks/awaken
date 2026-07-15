//! Conformance suite for [`CatalogRepo`]: the same behavioral assertions run
//! against the in-memory repo and (feature `sqlite`) the sqlite repo, so both
//! backends keep identical semantics; plus sqlite-only reopen-from-file tests.

use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo, RepoError};
use awaken_model_catalog::{
    ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};

fn provider(id: &str) -> Provider {
    Provider {
        id: ProviderId::new(id),
        slug: id.into(),
        display_name: id.into(),
        version: 1,
    }
}

fn endpoint(id: &str, provider: &str, dialect: ApiDialect) -> ProtocolEndpoint {
    ProtocolEndpoint {
        id: ProtocolEndpointId::new(id),
        provider_id: ProviderId::new(provider),
        dialect,
        base_url: None,
        timeout_secs: 300,
        display_name: id.into(),
        version: 1,
    }
}

fn offering(model: &str, ep: &str, dialect: ApiDialect) -> Offering {
    Offering {
        model_id: model.into(),
        provider_id: ProviderId::new("anthropic"),
        protocol_endpoint_id: ProtocolEndpointId::new(ep),
        dialect,
        upstream_model: None,
    }
}

async fn crud_round_trip_and_snapshot(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    repo.put_endpoint(endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    repo.put_offering(offering(
        "claude-opus-4-8",
        "ep1",
        ApiDialect::AnthropicMessages,
    ))
    .await
    .unwrap();

    assert_eq!(
        repo.get_provider(&ProviderId::new("anthropic"))
            .await
            .unwrap()
            .slug,
        "anthropic"
    );
    assert_eq!(
        repo.get_endpoint(&ProtocolEndpointId::new("ep1"))
            .await
            .unwrap()
            .timeout_secs,
        300
    );
    let snap = repo.snapshot().await.unwrap();
    assert_eq!(snap.providers.len(), 1);
    assert_eq!(snap.endpoints.len(), 1);
    assert_eq!(snap.offerings.len(), 1);
    assert!(
        snap.resolve_offering("claude-opus-4-8", ApiDialect::AnthropicMessages)
            .is_some()
    );
}

async fn missing_rows_are_not_found(repo: &dyn CatalogRepo) {
    assert!(matches!(
        repo.get_provider(&ProviderId::new("ghost")).await,
        Err(RepoError::ProviderNotFound(id)) if id == "ghost"
    ));
    assert!(matches!(
        repo.get_endpoint(&ProtocolEndpointId::new("ghost")).await,
        Err(RepoError::EndpointNotFound(id)) if id == "ghost"
    ));
}

async fn endpoint_needs_existing_provider(repo: &dyn CatalogRepo) {
    assert!(matches!(
        repo.put_endpoint(endpoint("ep1", "ghost", ApiDialect::OpenAiChat))
            .await,
        Err(RepoError::ProviderNotFound(id)) if id == "ghost"
    ));
}

async fn offering_needs_existing_endpoint(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    assert!(matches!(
        repo.put_offering(offering("m", "ghost", ApiDialect::AnthropicMessages))
            .await,
        Err(RepoError::EndpointNotFound(id)) if id == "ghost"
    ));
}

async fn rejected_offering_leaves_no_trace(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    repo.put_endpoint(endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    // Dialect mismatch with the endpoint → fail-closed…
    let bad = offering("m", "ep1", ApiDialect::OpenAiChat);
    assert!(matches!(
        repo.put_offering(bad).await,
        Err(RepoError::Invariant(_))
    ));
    // …and the rejected write is fully rolled back: later snapshots stay valid.
    let snap = repo.snapshot().await.unwrap();
    assert_eq!(snap.offerings.len(), 0);
}

async fn put_is_upsert(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    let mut v2 = provider("anthropic");
    v2.display_name = "Anthropic".into();
    v2.version = 2;
    repo.put_provider(v2).await.unwrap();
    let got = repo
        .get_provider(&ProviderId::new("anthropic"))
        .await
        .unwrap();
    assert_eq!(got.display_name, "Anthropic");
    assert_eq!(got.version, 2);
    assert_eq!(repo.snapshot().await.unwrap().providers.len(), 1);

    repo.put_endpoint(endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    let mut ep2 = endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages);
    ep2.timeout_secs = 60;
    ep2.version = 2;
    repo.put_endpoint(ep2).await.unwrap();
    let got = repo
        .get_endpoint(&ProtocolEndpointId::new("ep1"))
        .await
        .unwrap();
    assert_eq!(got.timeout_secs, 60);
    assert_eq!(repo.snapshot().await.unwrap().endpoints.len(), 1);
}

/// Re-putting an offering on its primary key `(model_id, protocol_endpoint_id)`
/// replaces the row rather than accumulating a duplicate — the durable backends
/// key on exactly that pair, so an in-memory push must not diverge.
async fn offering_put_is_upsert(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    repo.put_endpoint(endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    let mut first = offering("claude-opus-4-8", "ep1", ApiDialect::AnthropicMessages);
    first.upstream_model = Some("v1".into());
    repo.put_offering(first).await.unwrap();
    let mut second = offering("claude-opus-4-8", "ep1", ApiDialect::AnthropicMessages);
    second.upstream_model = Some("v2".into());
    repo.put_offering(second).await.unwrap();

    let snap = repo.snapshot().await.unwrap();
    // Same key ⇒ one offering, carrying the second write's payload.
    assert_eq!(snap.offerings.len(), 1);
    assert_eq!(snap.offerings[0].upstream_model.as_deref(), Some("v2"));

    // A distinct endpoint is a distinct key ⇒ a second offering, not a replace.
    repo.put_endpoint(endpoint("ep2", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    repo.put_offering(offering(
        "claude-opus-4-8",
        "ep2",
        ApiDialect::AnthropicMessages,
    ))
    .await
    .unwrap();
    assert_eq!(repo.snapshot().await.unwrap().offerings.len(), 2);
}

/// Run every suite, each on a fresh repo from `make`.
async fn run_all(make: impl Fn() -> Box<dyn CatalogRepo>) {
    crud_round_trip_and_snapshot(&*make()).await;
    missing_rows_are_not_found(&*make()).await;
    endpoint_needs_existing_provider(&*make()).await;
    offering_needs_existing_endpoint(&*make()).await;
    rejected_offering_leaves_no_trace(&*make()).await;
    put_is_upsert(&*make()).await;
    offering_put_is_upsert(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryCatalogRepo::new())).await;
}

/// Live Postgres conformance: the same suites as the other backends, each on a
/// fresh schema (so the six independent suites never see each other's rows).
/// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`).
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_model_catalog::postgres::PostgresCatalogRepo;
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

    async fn repo(schema: &'static str) -> Option<PostgresCatalogRepo> {
        let pool = schema_pool(schema).await?;
        Some(PostgresCatalogRepo::with_pool(pool).await.expect("store"))
    }

    #[tokio::test]
    async fn postgres_repo_conforms() {
        // Each suite gets its own schema — the runner's per-schema ledger keeps the
        // migrations isolated, and an empty schema is a fresh repo.
        let Some(r) = repo("t_cat_crud").await else {
            return;
        };
        crud_round_trip_and_snapshot(&r).await;
        missing_rows_are_not_found(&repo("t_cat_missing").await.unwrap()).await;
        endpoint_needs_existing_provider(&repo("t_cat_ep").await.unwrap()).await;
        offering_needs_existing_endpoint(&repo("t_cat_off").await.unwrap()).await;
        rejected_offering_leaves_no_trace(&repo("t_cat_reject").await.unwrap()).await;
        put_is_upsert(&repo("t_cat_upsert").await.unwrap()).await;
        offering_put_is_upsert(&repo("t_cat_off_upsert").await.unwrap()).await;
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;
    use awaken_model_catalog::sqlite::SqliteCatalogRepo;

    #[tokio::test]
    async fn sqlite_repo_conforms() {
        run_all(|| Box::new(SqliteCatalogRepo::open_in_memory().unwrap())).await;
    }

    #[tokio::test]
    async fn sqlite_rows_survive_a_reopen_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let path = path.to_str().unwrap();
        {
            let repo = SqliteCatalogRepo::open(path).unwrap();
            repo.put_provider(provider("anthropic")).await.unwrap();
            repo.put_endpoint(endpoint("ep1", "anthropic", ApiDialect::AnthropicMessages))
                .await
                .unwrap();
            repo.put_offering(offering(
                "claude-opus-4-8",
                "ep1",
                ApiDialect::AnthropicMessages,
            ))
            .await
            .unwrap();
        }
        // A fresh handle on the same file sees the identical validated catalog.
        let repo = SqliteCatalogRepo::open(path).unwrap();
        let snap = repo.snapshot().await.unwrap();
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.endpoints.len(), 1);
        assert_eq!(snap.offerings.len(), 1);
        assert_eq!(
            repo.get_provider(&ProviderId::new("anthropic"))
                .await
                .unwrap()
                .slug,
            "anthropic"
        );
    }
}
