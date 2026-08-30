//! Real Docker integration (ADR-0041 Slice 5), gated on the `docker` feature AND a
//! reachable daemon — skips cleanly otherwise (same discipline as the bwrap tests).
//!
//! Run with: `cargo test -p awaken-sandbox-container --features docker --test docker_it`
#![cfg(feature = "docker")]

#[path = "common/restore.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{
    ContainerEnvironmentProvider, ContainerPlan, ContainerProvider, ContainerRuntime,
    ContainerState, NetworkMode, RootfsPlan,
};
use tokio::io::AsyncWriteExt;

const AGENT_PORT: u16 = 8080;

fn plan(cmd: &[&str]) -> ContainerPlan {
    ContainerPlan {
        image: "busybox:latest".into(),
        command: cmd.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: Vec::new(),
        rootfs: RootfsPlan::HostUserland,
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
async fn docker_exact_restore_survives_provider_and_wrapper_replacement() {
    /* Live exact-restore table. C1 target absent; C2 provider wrapper is
     * dropped before aggregate CAS; C3 a fresh Docker client retries the exact
     * tuple; C4 the same effect carries another generation. Effects: E1 create
     * one target and retain its bind source; E2 C2+C3 recover the same handle;
     * E3 C4 fails without replacing it; E4 terminal disposal removes both
     * container and retained source. Rules D1=C1=>E1; D2=C2+C3=>E2;
     * D3=C4=>E3; D4=dispose=>E4. */
    let Some(first_runtime) = runtime().await else {
        return;
    };
    let scope = format!("docker-restore-{}", std::process::id());
    let spec = common::exact_restore_spec(&scope);
    let request = common::exact_restore_request(&scope);
    let first = ContainerProvider::new(Arc::new(first_runtime), "busybox:latest")
        .acquire_restore_environment(&spec, &request)
        .await
        .expect("D1 exact restore create");
    assert_eq!(
        first.disposition(),
        pc::SandboxRestoreTargetDisposition::Created,
        "D1/E1",
    );
    let handle = pc::Sandbox::handle(first.target().as_ref());
    let payload = handle
        .container_payload()
        .expect("D1/E1 typed container handle");
    let staging_root = match payload.runtime_handle.as_ref() {
        Some(pc::ContainerContinuationHandle::HostBindRestoration(locator)) => {
            std::path::PathBuf::from(locator.staging_root())
        }
        other => panic!("D1/E1 missing host staging evidence: {other:?}"),
    };
    drop(first);
    assert!(staging_root.is_dir(), "D2 retained after wrapper drop");

    let Some(retry_runtime) = runtime().await else {
        panic!("D2 daemon disappeared after exact target creation");
    };
    let retry = ContainerProvider::new(Arc::new(retry_runtime), "busybox:latest");
    let mut mismatch = request.clone();
    mismatch.generation_id.push_str("-other");
    assert!(
        retry
            .acquire_restore_environment(&spec, &mismatch)
            .await
            .is_err(),
        "D3/E3",
    );
    let recovered = retry
        .acquire_restore_environment(&spec, &request)
        .await
        .expect("D2 fresh provider recovery");
    assert_eq!(
        recovered.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "D2/E2",
    );
    assert_eq!(
        pc::Sandbox::handle(recovered.target().as_ref()),
        handle,
        "D2/E2"
    );
    drop(recovered);
    retry
        .dispose_restored_environment(&spec, &request)
        .await
        .expect("D4 provider exact terminal cleanup");
    retry
        .dispose_restored_environment(&spec, &request)
        .await
        .expect("D4 provider absent replay");
    assert!(!staging_root.exists(), "D4/E4");
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

#[tokio::test]
async fn docker_exec_can_be_polled_reattached_signaled_and_waited() {
    let Some(rt) = runtime().await else { return };
    let _ = rt.remove("awaken-it-exec").await;
    let id = rt
        .create("it-exec", &plan(&["sleep", "30"]))
        .await
        .expect("create Session container");

    let command = pc::Command {
        argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        cwd: "/tmp".into(),
        env: vec![pc::EnvVar {
            name: "EXEC_MARKER".into(),
            value: pc::EnvValue::Inline { value: "ok".into() },
            visibility: pc::EnvVisibility::Process,
        }],
        stdio: pc::Stdio::Null,
    };
    let command = pc::materialize_process_command(&[], command, None)
        .await
        .expect("materialize public exec environment");
    let process = rt.spawn(&id, command).await.expect("spawn detached exec");
    assert!(process.poll().await.unwrap().is_none());
    let public_id = process.id().to_string();
    let recovered = rt
        .process(&id, &public_id)
        .await
        .expect("reattach the durable exec handle");
    recovered.signal(pc::Signal::Term).await.unwrap();
    assert!(recovered.wait().await.unwrap().code.is_some());
    assert!(rt.process("wrong-container", &public_id).await.is_err());

    let empty = pc::MaterializedCommand::new(Vec::<String>::new());
    assert!(rt.spawn(&id, empty.clone()).await.is_err());
    assert!(rt.spawn_agent(&id, empty).await.is_err());
    let _ = rt.remove(&id).await;
}
