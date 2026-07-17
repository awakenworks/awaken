//! Conformance suite for [`CatalogRepo`]: the same behavioral assertions run
//! against the in-memory repo and (feature `sqlite`) the sqlite repo, so both
//! backends keep identical semantics; plus sqlite-only reopen-from-file tests.

use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo, RepoError};
use awaken_model_catalog::{
    ApiDialect, CatalogError, ModelAttributes, Offering, ProtocolEndpoint, ProtocolEndpointId,
    Provider, ProviderCatalog, ProviderId, ValidCatalog,
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

/// Model attributes publish independently of any offering (they carry no
/// provider/endpoint reference), upsert on `model_id`, and round-trip through the
/// snapshot on EVERY backend. Previously only the in-mem and sqlite unit tests
/// covered this — folding it into the shared suite verifies the Postgres round-trip
/// (jsonb column) too, and pins the upsert-replaces-on-model_id semantics.
async fn put_model_attributes_round_trips(repo: &dyn CatalogRepo) {
    repo.put_model_attributes(
        "claude-opus-4-8".into(),
        ModelAttributes {
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
        },
    )
    .await
    .unwrap();
    // Re-publishing the same model id upserts (replaces), never accumulates.
    repo.put_model_attributes(
        "claude-opus-4-8".into(),
        ModelAttributes {
            context_window: Some(190_000),
            max_output_tokens: None,
        },
    )
    .await
    .unwrap();

    let snap = repo.snapshot().await.unwrap();
    assert_eq!(snap.model_attributes.len(), 1);
    let attrs = &snap.model_attributes["claude-opus-4-8"];
    assert_eq!(attrs.context_window, Some(190_000));
    // The absent second write's max_output_tokens overwrites the first ⇒ None.
    assert_eq!(attrs.max_output_tokens, None);
    assert_eq!(snap.context_window("claude-opus-4-8"), Some(190_000));
    // Publishing attributes adds no offering and needs no provider/endpoint.
    assert!(snap.offerings.is_empty());
}

/// `resolve_offering` is a linear first-match over the snapshot's stored offering
/// order, so for two offerings sharing one `model_id`+dialect it returns whichever
/// the backend lists first — and that choice is stable across independent snapshots.
/// Offerings are INSERTED in non-sorted order (`ep-z` before `ep-a`) to exercise the
/// ordering, and the assertion is phrased against each backend's own stored order so
/// it holds on all three (see the cross-backend divergence test for the caveat that
/// the in-mem push order and the durable sorted order are NOT the same first-match).
async fn resolve_first_match_follows_stored_order_deterministically(repo: &dyn CatalogRepo) {
    repo.put_provider(provider("anthropic")).await.unwrap();
    repo.put_endpoint(endpoint("ep-a", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    repo.put_endpoint(endpoint("ep-z", "anthropic", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    // Non-sorted insertion order: ep-z first, then ep-a. Both offer the same model+dialect.
    repo.put_offering(offering("m", "ep-z", ApiDialect::AnthropicMessages))
        .await
        .unwrap();
    repo.put_offering(offering("m", "ep-a", ApiDialect::AnthropicMessages))
        .await
        .unwrap();

    let snap = repo.snapshot().await.unwrap();
    let resolved = snap
        .resolve_offering("m", ApiDialect::AnthropicMessages)
        .expect("a matching offering");
    // First-match == the first entry of this backend's own stored order.
    let stored_first = snap
        .offerings
        .iter()
        .find(|o| o.model_id == "m" && o.dialect == ApiDialect::AnthropicMessages)
        .expect("stored offering");
    assert_eq!(
        resolved.protocol_endpoint_id,
        stored_first.protocol_endpoint_id
    );
    // Deterministic: a second independent snapshot resolves to the identical endpoint.
    let snap2 = repo.snapshot().await.unwrap();
    assert_eq!(
        snap2
            .resolve_offering("m", ApiDialect::AnthropicMessages)
            .unwrap()
            .protocol_endpoint_id,
        resolved.protocol_endpoint_id
    );
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
    put_model_attributes_round_trips(&*make()).await;
    resolve_first_match_follows_stored_order_deterministically(&*make()).await;
}

/// Reload-time integrity guard, at the pure `ValidCatalog::parse` boundary: the
/// endpoint→provider dangling variant (the crate's own unit test only covers the
/// offering→endpoint variant at this boundary). A catalog whose endpoint references
/// an unknown provider cannot be sealed — parse fails closed.
#[test]
fn valid_catalog_parse_rejects_a_dangling_endpoint_provider() {
    let mut cat = ProviderCatalog::default();
    cat.endpoints.insert(
        "bad".into(),
        endpoint("bad", "ghost", ApiDialect::OpenAiChat),
    );
    assert!(matches!(
        ValidCatalog::parse(cat),
        Err(CatalogError::EndpointProviderUnknown(ep, prov)) if ep == "bad" && prov == "ghost"
    ));
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
        put_model_attributes_round_trips(&repo("t_cat_attrs").await.unwrap()).await;
        resolve_first_match_follows_stored_order_deterministically(
            &repo("t_cat_resolve").await.unwrap(),
        )
        .await;
    }

    /// Reload-time integrity guard on the network backend: an offering row that
    /// references an unknown endpoint — a dangling reference the public write path
    /// can never commit, but external corruption could plant — makes `snapshot()`
    /// fail closed through the same `ValidCatalog::parse` boundary. Skip-on-unreachable.
    #[tokio::test]
    async fn postgres_snapshot_rejects_a_dangling_offering_row() {
        let Some(pool) = schema_pool("t_cat_dangling").await else {
            return;
        };
        let repo = PostgresCatalogRepo::with_pool(pool.clone())
            .await
            .expect("store");
        let off = offering("orphan", "ghost", ApiDialect::AnthropicMessages);
        sqlx::query(
            "INSERT INTO catalog_offering (model_id, protocol_endpoint_id, data) \
             VALUES ($1, $2, $3::jsonb)",
        )
        .bind("orphan")
        .bind("ghost")
        .bind(serde_json::to_string(&off).unwrap())
        .execute(&pool)
        .await
        .expect("inject dangling offering");

        let err = repo.snapshot().await.unwrap_err();
        assert!(
            matches!(
                err,
                RepoError::Invariant(CatalogError::OfferingEndpointUnknown { ref model, ref endpoint })
                    if model == "orphan" && endpoint == "ghost"
            ),
            "expected OfferingEndpointUnknown, got {err:?}"
        );
    }

    /// The `Storage` error path on the network backend: a row whose (valid jsonb)
    /// `data` does not match the aggregate's shape makes the reload's serde fail and
    /// surface as `CatalogError::Storage` — fail-closed, not a panic. Skip-on-unreachable.
    #[tokio::test]
    async fn postgres_snapshot_surfaces_a_corrupt_row_as_a_storage_error() {
        let Some(pool) = schema_pool("t_cat_corrupt").await else {
            return;
        };
        let repo = PostgresCatalogRepo::with_pool(pool.clone())
            .await
            .expect("store");
        sqlx::query("INSERT INTO catalog_provider (id, data) VALUES ($1, $2::jsonb)")
            .bind("x")
            .bind("{\"wrong\":1}")
            .execute(&pool)
            .await
            .expect("inject corrupt provider");

        let err = repo.snapshot().await.unwrap_err();
        assert!(
            matches!(err, RepoError::Invariant(CatalogError::Storage(_))),
            "expected Storage, got {err:?}"
        );
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

    /// KNOWN BUG (adjudicate): `resolve_offering`'s first-match is NOT identical
    /// across backends for the SAME writes. The in-memory repo lists offerings in
    /// Vec-push (insertion) order; the durable backends reload them ORDER BY
    /// `(model_id, protocol_endpoint_id)`. Inserting two offerings for one
    /// model+dialect in non-sorted order (`ep-z` then `ep-a`) therefore resolves to a
    /// DIFFERENT endpoint per backend — the durable sqlite/postgres pair agree with
    /// each other (both sorted), but the in-mem repo diverges. Only the sqlite==pg
    /// guarantee is documented in src; the in-mem↔durable disagreement is unenforced.
    /// This test PINS the current divergent behavior; a future convergence flips it.
    #[tokio::test]
    async fn first_match_diverges_between_in_memory_and_durable_backend() {
        async fn setup(repo: &dyn CatalogRepo) {
            repo.put_provider(provider("anthropic")).await.unwrap();
            repo.put_endpoint(endpoint("ep-a", "anthropic", ApiDialect::AnthropicMessages))
                .await
                .unwrap();
            repo.put_endpoint(endpoint("ep-z", "anthropic", ApiDialect::AnthropicMessages))
                .await
                .unwrap();
            // Non-sorted insertion order: ep-z first, then ep-a.
            repo.put_offering(offering("m", "ep-z", ApiDialect::AnthropicMessages))
                .await
                .unwrap();
            repo.put_offering(offering("m", "ep-a", ApiDialect::AnthropicMessages))
                .await
                .unwrap();
        }
        let mem = InMemoryCatalogRepo::new();
        setup(&mem).await;
        let sql = SqliteCatalogRepo::open_in_memory().unwrap();
        setup(&sql).await;

        let mem_first = mem
            .snapshot()
            .await
            .unwrap()
            .resolve_offering("m", ApiDialect::AnthropicMessages)
            .unwrap()
            .protocol_endpoint_id
            .0
            .clone();
        let sql_first = sql
            .snapshot()
            .await
            .unwrap()
            .resolve_offering("m", ApiDialect::AnthropicMessages)
            .unwrap()
            .protocol_endpoint_id
            .0
            .clone();

        assert_eq!(mem_first, "ep-z", "in-memory resolves to insertion-first");
        assert_eq!(sql_first, "ep-a", "durable resolves to sorted-first");
        assert_ne!(
            mem_first, sql_first,
            "KNOWN BUG: backends disagree on resolve_offering first-match"
        );
    }

    /// Reload-time integrity guard against a corrupt row the public write path can
    /// never produce: an offering row referencing an endpoint that doesn't exist,
    /// injected via a raw connection to the DB file. `snapshot()` funnels the reload
    /// through `ValidCatalog::parse`, so it fails closed rather than serving a
    /// catalog with a dangling reference.
    #[tokio::test]
    async fn snapshot_rejects_a_dangling_offering_row_reloaded_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let path = path.to_str().unwrap();
        // Create the `catalog` schema (empty, valid) through the repo.
        SqliteCatalogRepo::open(path).unwrap();
        // Inject the dangling offering directly, bypassing the fail-closed write path.
        {
            let raw = rusqlite::Connection::open(path).unwrap();
            let off = offering("orphan", "ghost", ApiDialect::AnthropicMessages);
            raw.execute(
                "INSERT INTO catalog_offering (model_id, protocol_endpoint_id, data) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params!["orphan", "ghost", serde_json::to_string(&off).unwrap()],
            )
            .unwrap();
        }
        let repo = SqliteCatalogRepo::open(path).unwrap();
        let err = repo.snapshot().await.unwrap_err();
        assert!(
            matches!(
                err,
                RepoError::Invariant(CatalogError::OfferingEndpointUnknown { ref model, ref endpoint })
                    if model == "orphan" && endpoint == "ghost"
            ),
            "expected OfferingEndpointUnknown, got {err:?}"
        );
    }

    /// The `Storage` error path: a row whose `data` is not valid JSON for the
    /// aggregate makes the reload's serde fail, surfaced as `CatalogError::Storage`
    /// (fail-closed) — never a panic and never a silently-dropped row.
    #[tokio::test]
    async fn snapshot_surfaces_a_corrupt_row_as_a_storage_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let path = path.to_str().unwrap();
        SqliteCatalogRepo::open(path).unwrap();
        {
            let raw = rusqlite::Connection::open(path).unwrap();
            raw.execute(
                "INSERT INTO catalog_provider (id, data) VALUES (?1, ?2)",
                rusqlite::params!["x", "this is not json"],
            )
            .unwrap();
        }
        let repo = SqliteCatalogRepo::open(path).unwrap();
        let err = repo.snapshot().await.unwrap_err();
        assert!(
            matches!(err, RepoError::Invariant(CatalogError::Storage(_))),
            "expected Storage, got {err:?}"
        );
    }
}
