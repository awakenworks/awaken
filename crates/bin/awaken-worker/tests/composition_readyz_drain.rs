//! In-crate COMPOSITION test: drive the real [`awaken_worker::run`] entrypoint far
//! enough to prove the pieces it wires actually compose, WITHOUT the full real-binary
//! e2e (those live in `e2e/*.mjs` and stand up a real cell server + model).
//!
//! **Why a subprocess and not an in-process `tokio::spawn(run(...))`?** `run()` is
//! configured only through the environment (`AWAKEN_INGRESS`, `AWAKEN_WORKER_*`, …) and
//! its future is not `Send` (it awaits the non-`Send` sqlx store-open path), so it
//! cannot be `tokio::spawn`ed; and this crate inherits the workspace lint
//! `unsafe_code = "forbid"`, so an in-process `std::env::set_var` (unsafe in edition
//! 2024) will not compile. The faithful realization is therefore to launch this crate's
//! own thin binary — a shell over `run()` — with `Command::env` (safe) and observe the
//! composition through the admin surface it binds. This still stops short of the mjs
//! e2e: the upstream implements only worker lifecycle and an empty claim response, so
//! no real server, durable queue, or model is involved.
//!
//! This posture exercises the `gateway_only` branch (`AWAKEN_WORKER_GATEWAY_ONLY=1`,
//! secretless: no vault/seal key), `ensure_dispatch_pool` over the injected
//! `worker_dispatch_store`, the `/readyz` `/livez` `/metrics` surface, and the drain
//! transition (`POST /admin/drain` flips `/readyz` 200 → 503). Determinism: we POLL
//! `/readyz` until it flips — never a fixed "wait for ready" sleep.

use std::io::{Read, Write};
use std::process::{Child, Command};
use std::time::Duration;

mod support;
use support::FakeWorkerUpstream;

/// Kills the worker subprocess when the test ends (even on an assertion panic), so no
/// child leaks past the test.
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Grab an ephemeral port, then drop the listener so the address is free (and, for the
/// fake upstream, refuses connections promptly). A benign TOCTOU standard for tests.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

/// A minimal blocking HTTP/1.1 probe (no reqwest dep): returns the numeric status, or 0
/// when the admin surface is not yet accepting connections.
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

/// Poll `path` until it returns `want` (or give up after ~10s). Deterministic in
/// outcome: returns as soon as the status flips, never after a fixed wait.
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
fn run_gateway_only_brings_readyz_up_then_drain_flips_it_down() {
    let admin_addr = format!("127.0.0.1:{}", free_port());
    let upstream = FakeWorkerUpstream::start();

    let worker = Worker(
        Command::new(env!("CARGO_BIN_EXE_awaken-worker"))
            .env("AWAKEN_UPSTREAM_URL", upstream.url())
            .env("AWAKEN_INGRESS", "durable") // the pool's enable gate
            .env("AWAKEN_WORKER_GATEWAY_ONLY", "1") // secretless: no vault/seal key
            .env("AWAKEN_WORKER_ADMIN_LISTEN", &admin_addr)
            .env_remove("AWAKEN_MGMT_DIR") // in-memory config plane, no durable path
            .env_remove("AWAKEN_ACP_CLI") // native only, no ACP backend
            .env_remove("AWAKEN_ACP_ARGV")
            .spawn()
            .expect("spawn the awaken-worker binary"),
    );

    // The pool comes up (ensure_dispatch_pool over the injected worker_dispatch_store),
    // so once the admin surface binds, readiness reports ACCEPTING. Polled, not slept.
    assert!(
        poll_until(&admin_addr, "GET", "/readyz", 200),
        "run() brings the dispatch pool up → /readyz 200 (accepting) in the gateway-only posture"
    );

    // Liveness is up and /metrics renders while serving.
    assert_eq!(
        http_status(&admin_addr, "GET", "/livez"),
        200,
        "/livez is 200 while the worker is serving"
    );
    assert_eq!(
        http_status(&admin_addr, "GET", "/metrics"),
        200,
        "/metrics renders the Prometheus scrape (200)"
    );

    // The drain seam: POST /admin/drain (a preStop hook before SIGTERM) stops the pool
    // claiming. It returns 200 synchronously (begin_pool_drain awaited).
    assert_eq!(
        http_status(&admin_addr, "POST", "/admin/drain"),
        200,
        "POST /admin/drain acknowledges the drain"
    );

    // ...and readiness flips to 503, so the orchestrator stops routing new work to a
    // draining worker. This is the composition-level readyz transition (accepting →
    // draining), observed on the live admin surface.
    assert!(
        poll_until(&admin_addr, "GET", "/readyz", 503),
        "after drain, /readyz reports 503 (draining) — the worker is no longer routable"
    );

    drop(worker); // explicit: kill the child now (also happens on panic via Drop)
}
