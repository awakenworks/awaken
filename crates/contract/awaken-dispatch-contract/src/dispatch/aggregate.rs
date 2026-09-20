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
