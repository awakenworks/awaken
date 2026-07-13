//! `awaken-worker` binary: a thin shell over [`awaken_worker::run`].
//!
//! A database-less worker of a cell server. It reads the server URL from
//! `AWAKEN_UPSTREAM_URL`, then drains runs over HTTP and resolves each run's model
//! from the shared DB-configured catalog + credential vault. It holds no store and
//! serves no HTTP surface of its own.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
    awaken_worker::run(&upstream).await
}
