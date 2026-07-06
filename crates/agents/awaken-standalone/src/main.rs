//! The `awaken-standalone` binary: boot the open single-machine server.
//!
//! Zero configuration — it seeds a singleton tenant + two keys and serves the
//! guarded Managed session surface over the built-in [`HelloModel`]. The admin
//! and api keys are printed once at boot (the single-machine hand-off). Bind
//! address is `AWAKEN_STANDALONE_ADDR` (default `127.0.0.1:8080`).

use std::sync::Arc;

use awaken_standalone::{HelloModel, PROJECT_ID, build};

#[tokio::main]
async fn main() {
    let standalone = build(Arc::new(HelloModel));

    eprintln!("awaken-standalone: single-machine open runtime");
    eprintln!("  admin key: {}", standalone.admin_token);
    eprintln!("  api key:   {}", standalone.api_token);
    eprintln!("  project:   /projects/{PROJECT_ID}/v1/sessions (also bare /v1/sessions)");
    eprintln!("  rotate the printed keys before exposing this beyond localhost.");

    let addr =
        std::env::var("AWAKEN_STANDALONE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|err| panic!("bind {addr}: {err}"));
    eprintln!("  listening on http://{addr}");
    axum::serve(listener, standalone.router)
        .await
        .expect("serve");
}
