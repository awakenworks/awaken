//! Warm-capacity leases across a Worker restart against a REAL Docker daemon
//! (rules L1/L2/L3/L6 in `awaken_sandbox_container::pool::lease`).
//!
//! Gated on the `docker` feature AND a reachable daemon (self-skips otherwise).
//! Run with:
//! `cargo test -p awaken-sandbox-container --features docker --test pool_lease_docker -- --test-threads=1`
#![cfg(feature = "docker")]

#[path = "common/residue.rs"]
mod residue;
#[path = "common/warm_lease_restart.rs"]
mod warm_lease_restart;

use awaken_sandbox_container::docker::DockerRuntime;

const BUSYBOX: &str = "busybox:latest";

fn docker_ready() -> bool {
    std::process::Command::new("docker")
        .args(["image", "inspect", BUSYBOX])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        || std::process::Command::new("docker")
            .args(["pull", "-q", BUSYBOX])
            .status()
            .is_ok_and(|status| status.success())
}

#[test]
fn a_restarted_docker_worker_reclaims_only_expired_never_checked_out_capacity() {
    if !docker_ready() {
        eprintln!("skipping: no reachable Docker daemon with {BUSYBOX}");
        return;
    }
    warm_lease_restart::a_restarted_worker_reclaims_only_expired_never_checked_out_capacity(
        "docker",
        BUSYBOX,
        |namespace| {
            DockerRuntime::connect_local_for_realization(namespace, 8080).expect("docker client")
        },
    );
}
