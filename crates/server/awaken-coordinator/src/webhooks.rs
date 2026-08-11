//! The webhook plane (ADR-0048 / S10). The bridge itself lives in the neutral,
//! `awaken-webhook-managed` crate so the standalone can share it; this
//! module exposes the Coordinator-owned lifecycle surface at
//! `awaken_coordinator::webhooks`.
pub use awaken_webhook_managed::*;
