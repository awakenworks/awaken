//! In-crate COMPOSITION test for the DEFAULT (non-gateway) worker posture — the
//! production path where `run()` ALSO opens the shared control-plane stores and wires
//! the per-run `ConfigExecutorProvider` + the warm-loaded config service.
//!
//! Distinct from `composition_readyz_drain.rs` (gateway-only): here
//! `AWAKEN_WORKER_GATEWAY_ONLY` is UNSET, so `run()` takes the `if !gateway_only { … }`
//! branch — `awaken_control::open_shared_config_stores_from_env()` +
//! `SharedHost::with_executor_provider` + `warm_config_service_from_env()`. With
//! `AWAKEN_MGMT_DIR` unset those stores are in-memory and need NO seal key, so the
//! production-default composition must boot cleanly and become routable offline. If any
//! of those steps panicked (e.g. a spurious seal-key demand on the in-memory path),
//! `/readyz` would never reach 200. A minimal worker-control HTTP peer satisfies the
//! mandatory registration/heartbeat lifecycle while returning an empty dispatch queue.
//!
//! Same subprocess rationale as the sibling file: `run()` is env-configured and its
//! future is non-`Send`, and this crate `forbid`s unsafe, so we launch the crate's own
//! binary (a shell over `run()`) via `Command::env` and observe the admin surface. The
//! upstream is a dead port — no real server/model, far short of the mjs e2e.

use std::io::{Read, Write};
use std::process::{Child, Command};
use std::time::Duration;

mod support;
use support::FakeWorkerUpstream;

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn http_status(addr: &str, method: &str, path: &str) -> u16 {
    let mut stream = match std::net::TcpStream::connect(addr) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return 0;
    }
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

fn poll_until(addr: &str, method: &str, path: &str, want: u16) -> bool {
    for _ in 0..500 {
        if http_status(addr, method, path) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn run_default_posture_boots_the_config_plane_branch_and_is_routable() {
    let admin_addr = format!("127.0.0.1:{}", free_port());
    let upstream = FakeWorkerUpstream::start();

    let worker = Worker(
        Command::new(env!("CARGO_BIN_EXE_awaken-worker"))
            .env("AWAKEN_UPSTREAM_URL", upstream.url())
            .env("AWAKEN_INGRESS", "durable")
            .env("AWAKEN_WORKER_ADMIN_LISTEN", &admin_addr)
            // Explicitly UNSET so run() takes the default (config-plane) branch over
            // in-memory stores (no seal key required).
            .env_remove("AWAKEN_WORKER_GATEWAY_ONLY")
            .env_remove("AWAKEN_MGMT_DIR")
            .env_remove("AWAKEN_ACP_CLI")
            .env_remove("AWAKEN_ACP_ARGV")
            .spawn()
            .expect("spawn the awaken-worker binary"),
    );

    // The default composition — shared stores opened, ConfigExecutorProvider installed,
    // warm config service wired, pool up — becomes routable offline.
    assert!(
        poll_until(&admin_addr, "GET", "/readyz", 200),
        "the default (non-gateway) composition boots over in-memory stores → /readyz 200"
    );
    assert_eq!(http_status(&admin_addr, "GET", "/livez"), 200);

    // Drain still flips readiness in the default posture.
    assert_eq!(http_status(&admin_addr, "POST", "/admin/drain"), 200);
    assert!(
        poll_until(&admin_addr, "GET", "/readyz", 503),
        "drain flips /readyz to 503 in the default posture too"
    );

    drop(worker);
}
