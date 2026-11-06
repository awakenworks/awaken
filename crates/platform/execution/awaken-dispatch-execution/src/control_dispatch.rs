// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: the named Control-side bundle over the required Coordinator leaf ports,
// @formal reason: Inbox and Outbox roles, owned by the Worker/Pool execution path that drives it.
// @formal reason: The marker trait adds no methods, state, fallback, or second queue; this
// @formal reason: requirement classification makes no direct proof claim about implementations.
//! The Control-side role bundle this execution path drives.
//!
//! The run-ingress contract keeps the roles; the bundle lives with
//! [`crate::DispatchWorker`], [`crate::DispatchPool`] and [`crate::DispatchService`],
//! the one place generic over all three at once. Worker projections implement
//! the Worker-side `Dispatch` bundle of `awaken-dispatch-erasure` only.

use awaken_dispatch_contract::{
    DispatchAdministration, DispatchCommitAuthority, DispatchControlClaims, DispatchControlReads,
    DispatchMaintenance, Inbox, Outbox, SessionReservationAuthority, SessionRunReservations,
};
use awaken_dispatch_erasure::Dispatch;
/// Local execution and scheduling authority selected at construction.
pub trait ControlDispatch:
    LocalWorkerDispatch
    + TerminalRecoveryDispatch
    + DispatchMaintenance
    + DispatchAdministration
    + SessionRunReservations
    + Inbox
    + Outbox
{
}
impl<
    T: Dispatch
        + DispatchCommitAuthority
        + DispatchControlReads
        + DispatchControlClaims
        + DispatchMaintenance
        + DispatchAdministration
        + SessionRunReservations
        + Inbox
        + Outbox,
> ControlDispatch for T
{
}

/// The local Worker's two durable fences, in addition to ordinary execution.
pub trait LocalWorkerDispatch:
    Dispatch + DispatchCommitAuthority + SessionReservationAuthority
{
}
impl<T: Dispatch + DispatchCommitAuthority + SessionReservationAuthority> LocalWorkerDispatch
    for T
{
}
/// Terminal discovery has no administration, inbox or execution mutation authority.
pub trait TerminalRecoveryDispatch: DispatchControlReads + DispatchControlClaims {}
impl<T: DispatchControlReads + DispatchControlClaims> TerminalRecoveryDispatch for T {}
