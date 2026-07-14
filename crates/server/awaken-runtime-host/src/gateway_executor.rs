//! The cloud-managed-gateway egress port (ADR-0004; open Gap D/E).
//!
//! A run may carry a [`ModelAccessGrant::CloudManagedGateway`] grant: the worker
//! holds a short-lived lease token and a gateway base URL, never a provider key. To
//! honor it the native run path must build a model executor that dials the gateway
//! with the lease — but the runtime host owns no provider stack (that would couple
//! the data-plane host to a concrete client like genai). So the *builder* is a port:
//! the composition root injects a [`GatewayExecutorFactory`] wired to whatever
//! provider it has (the OSS genai adapter, or awaken-cloud's own), and the native
//! path calls it per gateway run.
//!
//! Dependency direction (a hard rule): this crate defines the port; downstream
//! composition roots — including the closed awaken-cloud layer — reverse-depend and
//! implement it. Nothing here reaches out to a hosting layer.
//!
//! Absent a factory, a gateway grant fails closed (the native path rejects it rather
//! than degrade to local credentials — the custody the grant exists to enforce).

use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::model_access::ResolvedModelEndpoint;

/// Builds the per-run model executor that dials a materialized cloud-managed gateway
/// grant. Implemented by a composition root over its provider stack; consumed by the
/// native run path (`execute_activation`) when a run carries a gateway grant.
pub trait GatewayExecutorFactory: Send + Sync {
    /// Build an executor that dials `endpoint` — `base_url` is the gateway, `bearer`
    /// is the lease token (presented on egress; the gateway injects the real
    /// provider credential out of this process), and `dialect` names the wire.
    /// Returns `None` when the dialect is one this factory cannot serve, so the
    /// caller fails closed rather than dialing a wire it cannot speak.
    fn build(&self, endpoint: &ResolvedModelEndpoint) -> Option<Arc<dyn LlmExecutor>>;
}
