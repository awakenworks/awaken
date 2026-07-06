//! The `awaken-standalone` binary: boot the open single-machine server.
//!
//! Zero configuration — [`awaken_standalone::run`] seeds a singleton tenant + two
//! keys, assembles the guarded Managed session surface over the built-in
//! `HelloModel`, and serves it. The keys + addressing are printed once at boot
//! (the single-machine hand-off). Bind address is `AWAKEN_STANDALONE_ADDR`
//! (default `127.0.0.1:8080`); it serves until the process is signalled.

#[tokio::main]
async fn main() {
    let addr =
        std::env::var("AWAKEN_STANDALONE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    awaken_standalone::run(
        &addr,
        std::future::pending(),
        awaken_standalone::print_banner,
    )
    .await
    .expect("serve the standalone");
}
