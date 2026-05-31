//! HTTP server layer for the awaken agent framework.
//!
//! Provides an Axum-based server that exposes agents over HTTP with Server-Sent
//! Events (SSE) streaming. Includes routing, protocol adapters (AI SDK, AG-UI),
//! mailbox polling, metrics, and request/response conversion utilities. Enabled
//! via the `server` feature flag on the `awaken` facade crate.

#![allow(missing_docs)]

pub(crate) mod admin_assistant;
pub(crate) mod admin_routes;
pub mod app;
pub(crate) mod auth;
pub mod config_routes;
pub mod error;
pub mod eval_limits;
pub mod eval_router;
pub mod event_relay;
pub(crate) mod event_routes;
pub mod http_run;
pub mod http_sse;
pub mod mailbox;
pub mod message_convert;
pub mod metrics;
pub mod outbox_relay;
pub mod protocol_fanout;
pub mod protocol_projector;
pub mod protocol_replay_state;
pub mod protocols;
pub mod query;
pub mod request;
mod route_modules;
pub mod routes;
pub mod scope;
pub mod services;
pub(crate) mod system_routes;
pub mod time;
pub mod transport;
