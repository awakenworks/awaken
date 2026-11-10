use sqlx::Executor as _;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Fixture {
    pub pool: PgPool,
    schema: String,
    url: String,
}

impl Fixture {
    pub async fn new(tag: &str) -> Option<Self> {
        assert!(
            !tag.is_empty()
                && tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
        let url = awaken_reliability_testkit::postgres_fixture_url()?;
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(3))
            .connect(&url)
            .await
            .expect("configured DataSubject PostgreSQL fixture must connect");
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!(
            "t_ds_{tag}_{}_{nonce:x}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // The checked tag plus integer process/time/counter coordinates form
        // only this fixture's schema identifier; no business values enter DDL.
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .await
            .expect("create owned schema");
        admin.close().await;
        let pool = Self::connect_schema(&url, &schema).await;
        Some(Self { pool, schema, url })
    }

    async fn connect_schema(url: &str, schema: &str) -> PgPool {
        let schema = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(3))
            .after_connect(move |connection, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("connect to owned DataSubject schema")
    }

    pub async fn reconnect(&self) -> PgPool {
        Self::connect_schema(&self.url, &self.schema).await
    }

    pub async fn cleanup(self) {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.url)
            .await
            .expect("cleanup connection");
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .await
            .expect("drop exact owned schema");
        admin.close().await;
        self.pool.close().await;
    }
}
