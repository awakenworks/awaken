//! Real Docker integration (ADR-0041 Slice 5), gated on the `docker` feature AND a
//! reachable daemon — skips cleanly otherwise (same discipline as the bwrap tests).
//!
//! Run with: `cargo test -p awaken-sandbox-container --features docker --test docker_it`
#![cfg(feature = "docker")]

use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{ContainerPlan, ContainerRuntime, ContainerState, NetworkMode};
use tokio::io::AsyncWriteExt;

const AGENT_PORT: u16 = 8080;

fn plan(cmd: &[&str]) -> ContainerPlan {
    ContainerPlan {
        image: "busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
    }
}

async fn runtime() -> Option<DockerRuntime> {
    let rt = DockerRuntime::connect_local(AGENT_PORT).ok()?;
    if rt.ping().await.is_err() {
        eprintln!("skipping: no reachable Docker daemon");
        return None;
    }
    Some(rt)
}

#[tokio::test]
async fn docker_full_lifecycle_against_a_real_daemon() {
    let Some(rt) = runtime().await else { return };
    let _ = rt.remove("awaken-it-life").await; // clear any leftover

    let id = rt
        .create("it-life", &plan(&["sleep", "30"]))
        .await
        .expect("create+start a real container");

    // process-as-container: the sleep is the container's main process, running.
    assert!(matches!(
        rt.inspect(&id).await.unwrap(),
        ContainerState::Running
    ));
    assert!(rt.poll(&id).await.unwrap().is_none(), "still running");

    // reap + teardown
    rt.signal(&id, pc::Signal::Kill).await.unwrap();
    rt.remove(&id).await.unwrap();
    // once removed, inspect no longer finds it running
    assert!(
        rt.inspect(&id).await.is_err() || matches!(rt.inspect(&id).await, Ok(ContainerState::Gone))
    );
}

#[tokio::test]
async fn docker_open_channel_dials_the_published_agent_port() {
    let Some(rt) = runtime().await else { return };
    let _ = rt.remove("awaken-it-chan").await;

    // A process-as-container agent that listens on AGENT_PORT (re-listen loop).
    let id = rt
        .create(
            "it-chan",
            &plan(&["sh", "-c", "while true; do nc -l -p 8080; done"]),
        )
        .await
        .expect("create listening container");

    // The published port is discovered via inspect and dialed via awaken-connection.
    // Retry while the in-container listener binds.
    let mut opened = None;
    for _ in 0..25 {
        if let Ok(mut chan) = rt.open_channel(&id).await
            && chan.write_all(b"ping").await.is_ok()
        {
            opened = Some(());
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = rt.remove(&id).await;
    assert!(
        opened.is_some(),
        "open_channel must reach the container's published agent port"
    );
}
