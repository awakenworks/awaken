//! The one Work-backend PostgreSQL fixture for unit and port-conformance callers.

use std::str::FromStr as _;

use sqlx::Executor as _;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

/// Unit `connect`, concurrent claim, and generic WorkQueue conformance all
/// consume one explicit URL but own different, randomly named schemas.
pub struct WorkPostgresFixture {
    pub pool: PgPool,
    pub scoped_url: String,
    schema: String,
}

impl WorkPostgresFixture {
    pub async fn new(url: &str, maximum_connections: u32) -> Self {
        assert!(maximum_connections > 0, "fixture pool bound is positive");
        assert!(
            !url.trim().is_empty(),
            "explicit Work PostgreSQL URL is empty"
        );
        let options =
            PgConnectOptions::from_str(url).expect("explicit Work PostgreSQL URL is valid");
        let admin = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect_with(options.clone())
            .await
            .expect("explicit Work PostgreSQL URL must connect");
        let schema = format!("work_fixture_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create exact owned Work schema");
        admin.close().await;
        // SQLx 0.8.6's `to_url_lossy` omits startup `options`. The Work
        // `connect(url)` entrypoint reparses a URI, so add the documented
        // `options[search_path]` query parameter to the exact supplied URI.
        // This retains its original TLS/principal/session configuration.
        let mut scoped = url::Url::parse(url).expect("explicit Work PostgreSQL URL is absolute");
        scoped
            .query_pairs_mut()
            .append_pair("options[search_path]", &schema);
        let scoped_url = scoped.to_string();
        let options = PgConnectOptions::from_str(&scoped_url)
            .expect("Work schema parameter round-trips through SQLx URL parsing");
        let pool = PgPoolOptions::new()
            .max_connections(maximum_connections)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect_with(options)
            .await
            .expect("connect exact owned Work schema");
        let actual_schema: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&pool)
            .await
            .expect("read Work fixture's actual schema");
        assert_eq!(
            actual_schema, schema,
            "Work fixture must never write into another schema"
        );
        Self {
            pool,
            scoped_url,
            schema,
        }
    }

    pub async fn cleanup(self) {
        self.pool
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .expect("drop only this Work fixture schema");
        self.pool.close().await;
    }
}
