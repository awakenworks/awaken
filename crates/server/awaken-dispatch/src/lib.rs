//! Durable run ingress: the dispatch/server host above the runtime core.
//!
//! `RunIngress` has exactly two delivery semantics (G5): direct, shipped by the
//! runtime as `DirectRunIngress`, and durable, shipped here as
//! [`DurableRunIngress`]. This crate owns the durable half — the dispatch queue,
//! pending input, claim/lease/recovery, and the worker that turns a durable
//! dispatch into a runtime attempt. It sits *above* the runtime (it depends on
//! the kernel; the kernel never depends on it) and adds durability over runtime
//! control without owning the loop, agent truth, or a second commit mechanism
//! (G6). Committed facts remain the single authority (G1/G13).
//!
//! The aggregates follow the run-ingress design's DDD split: [`DispatchQueue`] owns
//! delivery opportunity (claim/lease/recovery), [`Inbox`] owns the
//! thread's pending input, and run outcome stays in committed facts, read back
//! through the commit boundary's `ThreadReader`/`RunStore` ports.

mod any;
mod capability;
mod clock;
mod dispatch;
mod dispatch_schema;
mod durable;
mod live_control;
pub mod memory;
mod pool;
mod postgres;
mod request;
mod send_message;
mod service;
mod sqlite;
mod wake;
mod worker;

pub use any::AnyDispatchStore;
pub use capability::RunIngressCapabilities;
pub use clock::{Clock, ManualClock, SystemClock};
pub use dispatch::{
    CasOutcome, Claimed, Dispatch, DispatchError, DispatchOutcome, DispatchQueue, DispatchStatus,
    DispatchSummary, Inbox, Lease, Outbox, PendingInput, PendingRecord, SubmitOptions,
};
pub use dispatch_schema::dispatch_bundle;
pub use durable::DurableRunIngress;
pub use live_control::{Error as LiveRunControlError, LiveRunControlService};
pub use memory::MemoryDispatchStore;
pub use pool::{DispatchPool, WorkerResolver};
pub use postgres::{PostgresDispatchStore, StoreError as PostgresStoreError};
pub use request::{RunExecutionContext, RunExecutionRequest};
pub use send_message::OutboxMessageSender;
pub use service::{DispatchService, DispatchServiceConfig};
pub use sqlite::{SqliteDispatchStore, StoreError as SqliteStoreError};
#[cfg(feature = "nats")]
pub use wake::NatsWakeSignal;
pub use wake::{LocalWakeSignal, WakeSignal};
pub use worker::{DEFAULT_LEASE_MS, DispatchWorker};

/// A durable-ingress failure: either the dispatch store rejected an operation or
/// a runtime attempt failed. Kept as two arms so a queue-storage failure never
/// masquerades as a run execution failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Dispatch(#[from] dispatch::DispatchError),
    #[error(transparent)]
    Execution(#[from] awaken_runtime_contract::execution::Error),
}
