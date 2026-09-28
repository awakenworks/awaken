// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: named trait bundles over the existing DispatchQueue, Inbox, Outbox,
// @formal reason: Control and Session-continuation roles. Dynamic path: composition asks for the narrow
// @formal reason: ordinary or durable bundle before a claim or continuation transaction; these marker
// @formal reason: traits add no methods, state, fallback, or second queue. This requirement classification
// @formal reason: records the split module and makes no direct proof claim about arbitrary
// @formal reason: implementations.
use super::*;

/// Worker/host bundle for the three ordinary durable-ingress aggregates.
pub trait Dispatch: DispatchQueue + Inbox + Outbox {}

impl<T: DispatchQueue + Inbox + Outbox> Dispatch for T {}

pub trait DurableDispatch: Dispatch + SessionContinuationOutbox {}
impl<T: Dispatch + SessionContinuationOutbox> DurableDispatch for T {}

/// Control-side bundle; Worker projections implement [`Dispatch`] only.
pub trait ControlDispatch: ControlDispatchQueue + Inbox + Outbox {}
impl<T: ControlDispatchQueue + Inbox + Outbox> ControlDispatch for T {}

pub trait DurableControlDispatch: ControlDispatch + SessionContinuationOutbox {}
impl<T: ControlDispatch + SessionContinuationOutbox> DurableControlDispatch for T {}
