// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: the named Control-side bundle over the existing ControlDispatchQueue,
// @formal reason: Inbox and Outbox roles, owned by the Worker/Pool execution path that drives it.
// @formal reason: The marker trait adds no methods, state, fallback, or second queue; this
// @formal reason: requirement classification makes no direct proof claim about implementations.
//! The Control-side role bundle this execution path drives.
//!
//! The run-ingress contract keeps the roles; the bundle lives with
//! [`crate::DispatchWorker`], [`crate::DispatchPool`] and [`crate::DispatchService`],
//! the one place generic over all three at once. Worker projections implement
//! the Worker-side `Dispatch` bundle of `awaken-dispatch-erasure` only.

use awaken_dispatch_contract::{ControlDispatchQueue, Inbox, Outbox};

/// Control-side bundle of the queue, inbox and outbox roles.
pub trait ControlDispatch: ControlDispatchQueue + Inbox + Outbox {}

impl<T: ControlDispatchQueue + Inbox + Outbox> ControlDispatch for T {}
