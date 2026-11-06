// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Required roles are statically bound from one owner; composition adds no second queue or lifecycle law.
//! Owned dispatch role bundles. Leaf ports remain in the contract.
use awaken_dispatch_contract::{
    ClaimedCheckpointTransport, ClaimedCredentialDeliveryTransport, DispatchAdministration,
    DispatchAttemptControl, DispatchClaimOwnership, DispatchClaims, DispatchCommitAuthority,
    DispatchControlClaims, DispatchControlReads, DispatchEnqueue, DispatchExecution,
    DispatchMaintenance, DispatchOperationalFeed, DispatchReads, DispatchSettlement, Inbox,
    InboxRead, Outbox, SessionContinuationOutbox, SessionRunReservations,
};
pub trait Dispatch:
    DispatchEnqueue
    + DispatchClaims
    + DispatchExecution
    + DispatchClaimOwnership
    + DispatchSettlement
    + InboxRead
{
}
impl<
    T: DispatchEnqueue
        + DispatchClaims
        + DispatchExecution
        + DispatchClaimOwnership
        + DispatchSettlement
        + InboxRead,
> Dispatch for T
{
}
pub trait DurableDispatch:
    Dispatch
    + DispatchAdministration
    + DispatchCommitAuthority
    + DispatchReads
    + DispatchMaintenance
    + SessionRunReservations
    + DispatchAttemptControl
    + DispatchControlClaims
    + DispatchControlReads
    + Inbox
    + Outbox
    + SessionContinuationOutbox
    + DispatchOperationalFeed
{
}
impl<
    T: Dispatch
        + DispatchAdministration
        + DispatchCommitAuthority
        + DispatchReads
        + DispatchMaintenance
        + SessionRunReservations
        + DispatchAttemptControl
        + DispatchControlClaims
        + DispatchControlReads
        + Inbox
        + Outbox
        + SessionContinuationOutbox
        + DispatchOperationalFeed,
> DurableDispatch for T
{
}
pub trait RemoteDispatch:
    Dispatch + ClaimedCheckpointTransport + ClaimedCredentialDeliveryTransport
{
}
impl<T: Dispatch + ClaimedCheckpointTransport + ClaimedCredentialDeliveryTransport> RemoteDispatch
    for T
{
}
