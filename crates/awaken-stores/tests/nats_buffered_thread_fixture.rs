#![cfg(feature = "nats")]
#![allow(dead_code)]

use std::time::Duration;

use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

pub struct NatsFixture {
    _container: ContainerAsync<GenericImage>,
    pub url: String,
}

impl NatsFixture {
    pub async fn start() -> Self {
        let image = GenericImage::new("nats", "2.10-alpine")
            .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
            .with_cmd(vec!["-js"]);
        let container = image.start().await.expect("failed to start nats container");
        let host_port = container.get_host_port_ipv4(4222).await.expect("port");
        let url = format!("nats://127.0.0.1:{host_port}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        Self {
            _container: container,
            url,
        }
    }
}

pub fn unique_config(fixture: &NatsFixture) -> awaken_stores::NatsBufferedThreadConfig {
    let mut config = awaken_stores::NatsBufferedThreadConfig::new(fixture.url.clone());
    config.stream_name = format!("THREADLOG_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.hot_bucket = format!("hot_{}", uuid::Uuid::now_v7().simple());
    config
}
