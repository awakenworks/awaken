//! Real-Docker end-to-end for the cross-restart reaper: prove `list_managed` discovers
//! awaken-labeled containers with faithful running/age signals, and a `SandboxReaper`
//! sweep reaps the finished (exited) one while keeping a live one — against a real
//! daemon, not the fake runtime the decision-table unit tests use. This closes the
//! `list_managed` label-filter + state/age parsing gap that only a real daemon exercises.
//!
//! Gated on the `docker` feature AND a reachable daemon (self-skips otherwise).
//! Run: `cargo test -p awaken-sandbox-container --features docker --test reaper_docker`
#![cfg(feature = "docker")]

use std::sync::Arc;
use std::time::Duration;

use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::reaper::{ReapReason, SandboxReaper, should_reap};
use awaken_sandbox_container::{ContainerPlan, ContainerRuntime, NetworkMode, RootfsPlan};

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn plan(cmd: Vec<String>) -> ContainerPlan {
    ContainerPlan {
        image: "busybox:latest".into(),
        command: cmd,
        env: Vec::new(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::None,
        limits: Default::default(),
        memory_mounts: Vec::new(),
        rootfs: RootfsPlan::Image("busybox:latest".into()),
    }
}

#[tokio::test]
async fn reaper_sweeps_a_finished_awaken_container_and_keeps_a_live_one() {
    if !docker_available() {
        eprintln!("skipping: no reachable Docker daemon");
        return;
    }
    let _ = std::process::Command::new("docker")
        .args(["pull", "-q", "busybox:latest"])
        .status();

    let owner_runtime = Arc::new(DockerRuntime::connect_local(8080).expect("docker client"));
    let pid = std::process::id();
    let live_id = format!("reaper-live-{pid}");
    let done_id = format!("reaper-done-{pid}");

    // A live agent: sleeps, staying Running. A finished agent: `true` exits at once.
    let live = owner_runtime
        .create(&live_id, &plan(vec!["sleep".into(), "300".into()]))
        .await
        .expect("create live");
    let done = owner_runtime
        .create(&done_id, &plan(vec!["true".into()]))
        .await
        .expect("create done");

    // Let the `true` container exit (and confirm the label discovery sees both).
    let mut managed = Vec::new();
    for _ in 0..40 {
        managed = owner_runtime.list_managed().await.expect("list_managed");
        let done_exited = managed.iter().any(|m| m.id == done && !m.running);
        let live_running = managed.iter().any(|m| m.id == live && m.running);
        if done_exited && live_running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // list_managed discovered BOTH awaken-labeled containers (the label filter works),
    // with faithful signals: the `true` one exited, the `sleep` one still running.
    let done_mc = managed
        .iter()
        .find(|m| m.id == done)
        .expect("done discovered");
    let live_mc = managed
        .iter()
        .find(|m| m.id == live)
        .expect("live discovered");
    assert!(
        !done_mc.running,
        "the finished agent is discovered as exited"
    );
    assert!(live_mc.running, "the live agent is discovered as running");
    // The creating runtime retains ownership: its reaper cannot race the normal
    // channel-drain/process/write-back lifecycle, even after the process exits.
    assert!(done_mc.owned_by_current_runtime);
    assert_eq!(should_reap(done_mc, 3600), None);
    assert_eq!(should_reap(live_mc, 3600), None);

    // A new runtime instance models a restarted worker. It sees the old owner's
    // containers as stale; the finished one is reaped while the young live one stays.
    let restarted_runtime = Arc::new(DockerRuntime::connect_local(8080).expect("docker client"));
    let stale = restarted_runtime
        .list_managed()
        .await
        .expect("list managed after restart");
    assert!(
        stale
            .iter()
            .all(|container| !container.owned_by_current_runtime)
    );
    let reaper = SandboxReaper::new(restarted_runtime.clone(), 3600);
    let reaped = reaper.sweep().await;
    assert!(
        reaped
            .iter()
            .any(|(id, r)| *id == done && *r == ReapReason::Exited),
        "the finished container was reaped: {reaped:?}"
    );
    assert!(
        !reaped.iter().any(|(id, _)| *id == live),
        "the live container was NOT reaped"
    );

    // The reaped container is gone; the live one still exists.
    let after = restarted_runtime
        .list_managed()
        .await
        .expect("list_managed after");
    assert!(
        !after.iter().any(|m| m.id == done),
        "reaped container removed"
    );
    assert!(after.iter().any(|m| m.id == live), "live container remains");

    // Cleanup.
    let _ = owner_runtime.remove(&live).await;
    let _ = done; // already removed by the sweep
}
