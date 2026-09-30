//! Warm-capacity leases across a Worker restart against REAL rootless Podman
//! (rules L1/L2/L3/L6 in `awaken_sandbox_container::pool::lease`).
//!
//! Gated on the `podman` feature AND a Podman that can start a container
//! (self-skips otherwise). Run with:
//! `cargo test -p awaken-sandbox-container --features podman --test pool_lease_podman -- --test-threads=1`
#![cfg(all(unix, feature = "podman"))]

#[path = "common/residue.rs"]
mod residue;
#[path = "common/warm_lease_restart.rs"]
mod warm_lease_restart;

use awaken_sandbox_container::podman::PodmanRuntime;

const BUSYBOX: &str = "docker.io/library/busybox:latest";

fn podman_ready() -> bool {
    std::process::Command::new("podman")
        .args(["run", "--rm", "--network=none", BUSYBOX, "true"])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn a_restarted_podman_worker_reclaims_only_expired_never_checked_out_capacity() {
    if !podman_ready() {
        eprintln!("skipping: rootless Podman cannot start {BUSYBOX}");
        return;
    }
    warm_lease_restart::a_restarted_worker_reclaims_only_expired_never_checked_out_capacity(
        "podman",
        BUSYBOX,
        |namespace| PodmanRuntime::for_realization(namespace, 8080),
    );
}
