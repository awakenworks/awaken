//! HTTP server layer for the awaken agent framework.
//!
//! Provides an Axum-based server that exposes agents over HTTP with Server-Sent
//! Events (SSE) streaming. Includes routing, protocol adapters (AI SDK, AG-UI),
//! mailbox polling, metrics, and request/response conversion utilities. Enabled
//! via the `server` feature flag on the `awaken` facade crate.

#![allow(missing_docs)]

pub mod app;
pub mod config_routes;
pub mod event_relay;
pub mod http_run;
pub mod http_sse;
pub mod mailbox;
pub mod message_convert;
pub mod metrics;
pub mod protocols;
pub mod query;
pub mod request;
pub mod routes;
pub mod services;
pub mod transport;
