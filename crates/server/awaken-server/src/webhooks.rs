//! The webhook plane (ADR-0048 / S10). The bridge itself lives in the neutral,
//! `awaken-webhook-managed` crate so the standalone can share it; this
//! module re-exports it under the historical `awaken_server::webhooks` path.
pub use awaken_webhook_managed::*;
