//! The dispatch / run-ingress contract (ADR-0039 slice 2.1).
//!
//! This is the durable-dispatch bounded context's port surface, factored out of
//! the `awaken-run-ingress` host so store backends and adapters can depend on the
//! contract without the host crate (G2). It carries the neutral dispatch ports
//! (`DispatchQueue` / `Inbox` / `Outbox`, bundled as `Dispatch`) and the
//! serializable [`RunDispatch`] a durable queue persists and replays — no live
//! handles (G3). Worker wiring stays private to the host.

pub mod dispatch;
pub mod run_dispatch;

pub use dispatch::{
    CasOutcome, Claimed, CommitEpochGuard, Dispatch, DispatchError, DispatchOutcome, DispatchQueue,
    DispatchState, DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, RunClaim,
    SettleOutcome, SubmitOptions,
};
pub use run_dispatch::RunDispatch;
