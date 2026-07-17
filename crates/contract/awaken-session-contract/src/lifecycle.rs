//! The session lifecycle sink port (ADR-0048): a neutral seam the Managed adapter
//! calls after a lifecycle transition commits, handing the session's persisted owner
//! so a consumer (e.g. the webhook bridge) can stamp tenancy. Neutral by
//! construction (all `&str`) — the delivery machinery lives in the assembly layer,
//! so neither this contract nor the wire adapter depends on it.

/// A sink notified of a session's committed lifecycle transitions (webhooks).
/// Implemented in the assembly layer over a webhook dispatcher.
#[async_trait::async_trait]
pub trait SessionLifecycleSink: Send + Sync {
    /// `event_type` is the wire name of the transition (e.g. `session.status_idle`);
    /// `workspace_id` is the session's owning workspace (absent on the bare pre-owner
    /// surface). The **org** is a deployment-level attribution the sink itself carries
    /// (from its assembly config / `AWAKEN_ORG_ID`), not a per-session axis — org is
    /// cloud-only (ADR-0048 D4), so the core never resolves it. Must not block the
    /// caller for long — deliver out-of-band.
    async fn emit(&self, session_id: &str, workspace_id: Option<&str>, event_type: &str);
}
