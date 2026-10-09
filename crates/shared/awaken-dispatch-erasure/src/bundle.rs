// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: named trait bundles over the existing DispatchQueue, Inbox, Outbox
// @formal reason: and Session-continuation roles, owned by the one erasure that composes them.
// @formal reason: These marker traits add no methods, state, fallback, or second queue; this
// @formal reason: requirement classification makes no direct proof claim about implementations.
//! The role bundles this erasure composes.
//!
//! The run-ingress contract keeps the roles (`DispatchQueue`, `Inbox`, `Outbox`,
//! `SessionContinuationOutbox`); the bundles live here because
//! [`crate::AnyDispatchStore`] is the one place that holds them as trait objects.

use awaken_dispatch_contract::{DispatchQueue, Inbox, Outbox, SessionContinuationOutbox};

/// Worker/host bundle for the three ordinary durable-ingress aggregates.
pub trait Dispatch: DispatchQueue + Inbox + Outbox {}

impl<T: DispatchQueue + Inbox + Outbox> Dispatch for T {}

/// A [`Dispatch`] that also owns the atomic Session continuation write.
pub trait DurableDispatch: Dispatch + SessionContinuationOutbox {}

impl<T: Dispatch + SessionContinuationOutbox> DurableDispatch for T {}
