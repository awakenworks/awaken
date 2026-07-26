//! `awaken-worker` binary: a thin shell over [`awaken_worker::run`].
//!
//! The legacy standalone presentation is retained only to return a stable
//! migration error. Production Workers start through the canonical typed
//! `awaken worker --config <PATH> --server <URL>` composition.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("standalone awaken-worker configuration was removed; run `awaken worker --config <PATH> --server <URL>`".into())
}
