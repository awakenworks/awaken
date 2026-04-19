//! Test fixture: launches a NATS server in a testcontainer.

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
        let host_port = container.get_host_port_ipv4(4222).await.expect("nats port");
        let url = format!("nats://127.0.0.1:{host_port}");
        // Give JetStream a moment to initialize.
        tokio::time::sleep(Duration::from_millis(500)).await;
        Self {
            _container: container,
            url,
        }
    }
}
