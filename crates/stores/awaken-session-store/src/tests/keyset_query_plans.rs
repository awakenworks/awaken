use sqlx::Executor as _;

use super::*;

#[tokio::test]
async fn sqlite_recovery_cursor_is_an_index_range_for_each_selection() {
    // Causes: C1 a subsequent keyset page; C2 general/preparation selection.
    // Effect: SQLite's actual planner seeks the exclusive key rather than
    // scanning/filtering its prefix. R1=C1+general and R2=C1+preparation.
    // This consumes the production query producer, not a copied SQL oracle.
    // Existing page conformance owns first-page, EOF and 256/257 semantics.
    let repo = memory_repository().await;
    for preparation_only in [false, true] {
        let sql = reconciliation_scan_sql(preparation_only, true, "?1");
        let rows =
            sqlx::query_as::<_, (i64, i64, i64, String)>(&format!("EXPLAIN QUERY PLAN {sql}"))
                .bind("plan-after")
                .fetch_all(&repo.pool)
                .await
                .unwrap();
        let plan = rows
            .into_iter()
            .map(|row| row.3)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plan.contains("SEARCH work") && plan.contains("session_id>?"),
            "R1/R2: {plan}"
        );
    }
}

#[tokio::test]
async fn postgres_cached_recovery_and_startup_cursors_keep_the_index_condition() {
    // Causes: C1 PostgreSQL chooses a cached generic plan; C2 a subsequent
    // general/preparation page; C3 startup crosses its 256-root page boundary.
    // Effects: E1 C1+C2 retains the bound key in Index Cond, not Filter;
    // E2 C1+C3 does the same for the actual prepared startup query.
    // R1/R2 cover both recovery selections; R3 reads the query PostgreSQL
    // prepared while the production Server audit traversed 257 canonical roots.
    // Constraint: planner settings and schema are owned by this one fixture
    // connection; disabling sequential/bitmap alternatives exposes whether the
    // key is eligible for a B-tree range independently of tiny fixture costs.
    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    let schema = format!("t_session_keyset_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&scoped_postgres_url(&url, &schema))
        .await
        .unwrap();
    let repo = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .unwrap();
    for index in 0..257 {
        create_fixture(
            &repo,
            "ws-plan",
            sample(&format!("plan-{index:03}")),
            Vec::new(),
        )
        .await;
    }
    for setting in [
        "SET plan_cache_mode = force_generic_plan",
        "SET enable_seqscan = off",
        "SET enable_bitmapscan = off",
    ] {
        pool.execute(setting).await.unwrap();
    }
    for preparation_only in [false, true] {
        let name = if preparation_only {
            "keyset_prepare"
        } else {
            "keyset_general"
        };
        let sql = reconciliation_scan_sql(preparation_only, true, "$1::text");
        pool.execute(format!("PREPARE {name}(text) AS {sql}").as_str())
            .await
            .unwrap();
        let plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) EXECUTE {name}('plan-after')"
        ))
        .fetch_all(&pool)
        .await
        .unwrap()
        .join("\n");
        assert!(
            plan.contains("Index Cond: (session_id > $1)"),
            "R1/R2: {plan}"
        );
    }
    drop(
        PostgresManagedSessionRepository::with_existing_pool(pool.clone())
            .await
            .unwrap(),
    );
    let prepared = sqlx::query_as::<_, (String, String)>(
        "SELECT name, statement FROM pg_prepared_statements WHERE statement LIKE 'SELECT root.session_id,%'"
    ).fetch_all(&pool).await.unwrap();
    let mut checked = 0;
    for (name, statement) in prepared {
        if !statement.contains("WHERE root.session_id > $1::text") {
            continue;
        }
        assert!(
            name.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        );
        let plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) EXECUTE \"{name}\"('plan-after')"
        ))
        .fetch_all(&pool)
        .await
        .unwrap()
        .join("\n");
        assert!(plan.contains("Index Cond: (session_id > $1)"), "R3: {plan}");
        checked += 1;
    }
    assert!(
        checked > 0,
        "R3 must inspect the production subsequent-page statement"
    );
    drop(repo);
    pool.close().await;
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}
