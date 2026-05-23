//! Test fixture: launches a PostgreSQL server in a testcontainer.

#![cfg(feature = "postgres")]
#![allow(dead_code)]

use std::time::Duration;

use sqlx::PgPool;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

pub struct PostgresFixture {
    _container: ContainerAsync<GenericImage>,
    pub url: String,
    pub pool: PgPool,
}

impl PostgresFixture {
    pub async fn start() -> Self {
        let image = GenericImage::new("postgres", "16-alpine")
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "awaken_test")
            .with_env_var("POSTGRES_USER", "awaken")
            .with_env_var("POSTGRES_DB", "awaken_test");
        let container = image
            .start()
            .await
            .expect("failed to start postgres container");
        let host_port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("postgres port");
        let url = format!("postgres://awaken:awaken_test@127.0.0.1:{host_port}/awaken_test");

        // The wait condition fires when the server logs "ready" — but the
        // first accept loop may need a moment more to take connections.
        // Retry the initial connect a few times.
        let mut last_err = None;
        for _ in 0..30 {
            match PgPool::connect(&url).await {
                Ok(pool) => {
                    return Self {
                        _container: container,
                        url,
                        pool,
                    };
                }
                Err(err) => {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        panic!(
            "failed to connect to postgres testcontainer: {:?}",
            last_err
        );
    }
}
