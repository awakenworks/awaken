//! The dispatch / run-ingress contract (ADR-0039 slice 2.1).
//!
//! This is the durable-dispatch bounded context's port surface, factored out of
//! the `awaken-run-ingress` host so store backends and adapters can depend on the
//! contract without the host crate (G2). It carries the neutral dispatch ports
//! (`DispatchQueue` / `Inbox` / `Outbox`, bundled as `Dispatch`) and the
//! serializable [`RunExecutionRequest`] a durable queue persists and replays — no
//! live handles (G3). Live per-attempt wiring stays in the host.

pub mod dispatch;
pub mod request;

pub use dispatch::{
    CasOutcome, Claimed, Dispatch, DispatchError, DispatchOutcome, DispatchQueue, DispatchStatus,
    DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, SettleOutcome,
    SubmitOptions,
};
pub use request::RunExecutionRequest;
