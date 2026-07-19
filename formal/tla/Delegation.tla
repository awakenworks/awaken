------------------------------ MODULE Delegation ------------------------------
EXTENDS Naturals, FiniteSets

\* Durable parent/child Run coordination. Routing kind is deliberately absent
\* from every transition: LocalChildren and RemoteChildren therefore execute the
\* same formal lifecycle and differ only at an adapter outside this state machine.
CONSTANTS
    Children,
    LocalChildren,
    RemoteChildren,
    ShallowChildren,
    ParentLineage,
    Owners,
    NoOwner,
    MaxDepth,
    ChildDepth,
    MaxParallel,
    MaxTotal,
    MaxEpoch

ParentStates == {"Running", "Ended"}
ChildStates == {"Absent", "Running", "Awaiting", "Ended", "Cancelled"}
ResultStates == {"None", "Pending", "Delivered", "Discarded"}
CancelStates == {"None", "Requested", "Acknowledged"}

VARIABLES
    parentState,
    parentOwner,
    parentEpoch,
    childState,
    childOwner,
    childEpoch,
    resultState,
    deliveryCount,
    cancelState,
    started

vars == <<
    parentState,
    parentOwner,
    parentEpoch,
    childState,
    childOwner,
    childEpoch,
    resultState,
    deliveryCount,
    cancelState,
    started
>>

Init ==
    /\ parentState = "Running"
    /\ parentOwner = NoOwner
    /\ parentEpoch = 0
    /\ childState = [c \in Children |-> "Absent"]
    /\ childOwner = [c \in Children |-> NoOwner]
    /\ childEpoch = [c \in Children |-> 0]
    /\ resultState = [c \in Children |-> "None"]
    /\ deliveryCount = [c \in Children |-> 0]
    /\ cancelState = [c \in Children |-> "None"]
    /\ started = {}

ActiveChildren == {c \in started : childState[c] \in {"Running", "Awaiting"}}

ClaimParent(owner) ==
    /\ owner \in Owners
    /\ parentState = "Running"
    /\ parentOwner = NoOwner
    /\ parentEpoch < MaxEpoch
    /\ parentOwner' = owner
    /\ parentEpoch' = parentEpoch + 1
    /\ UNCHANGED <<parentState, childState, childOwner, childEpoch,
                    resultState, deliveryCount, cancelState, started>>

CrashParent(owner) ==
    /\ owner = parentOwner
    /\ owner \in Owners
    /\ parentState = "Running"
    /\ parentOwner' = NoOwner
    /\ UNCHANGED <<parentState, parentEpoch, childState, childOwner, childEpoch,
                    resultState, deliveryCount, cancelState, started>>

StartChild(child) ==
    /\ child \in Children
    /\ parentState = "Running"
    /\ childState[child] = "Absent"
    /\ child \in ShallowChildren
    /\ child \notin ParentLineage
    /\ ChildDepth <= MaxDepth
    /\ Cardinality(ActiveChildren) < MaxParallel
    /\ Cardinality(started) < MaxTotal
    /\ childState' = [childState EXCEPT ![child] = "Running"]
    /\ started' = started \cup {child}
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childOwner, childEpoch,
                    resultState, deliveryCount, cancelState>>

ClaimChild(child, owner) ==
    /\ child \in started
    /\ owner \in Owners
    /\ childState[child] = "Running"
    /\ childOwner[child] = NoOwner
    /\ childEpoch[child] < MaxEpoch
    /\ childOwner' = [childOwner EXCEPT ![child] = owner]
    /\ childEpoch' = [childEpoch EXCEPT ![child] = @ + 1]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childState, resultState,
                    deliveryCount, cancelState, started>>

CrashChild(child, owner) ==
    /\ child \in started
    /\ owner = childOwner[child]
    /\ owner \in Owners
    /\ childState[child] = "Running"
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childState, childEpoch,
                    resultState, deliveryCount, cancelState, started>>

AwaitChild(child, owner) ==
    /\ child \in started
    /\ childState[child] = "Running"
    /\ childOwner[child] = owner
    /\ owner \in Owners
    /\ cancelState[child] = "None"
    /\ childState' = [childState EXCEPT ![child] = "Awaiting"]
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childEpoch, resultState,
                    deliveryCount, cancelState, started>>

ResumeChild(child, owner) ==
    /\ child \in started
    /\ childState[child] = "Awaiting"
    /\ owner \in Owners
    /\ childEpoch[child] < MaxEpoch
    /\ cancelState[child] = "None"
    /\ childState' = [childState EXCEPT ![child] = "Running"]
    /\ childOwner' = [childOwner EXCEPT ![child] = owner]
    /\ childEpoch' = [childEpoch EXCEPT ![child] = @ + 1]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, resultState,
                    deliveryCount, cancelState, started>>

FinishChild(child, owner) ==
    /\ child \in started
    /\ childState[child] = "Running"
    /\ childOwner[child] = owner
    /\ owner \in Owners
    /\ childState' = [childState EXCEPT ![child] = "Ended"]
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ resultState' =
        [resultState EXCEPT ![child] =
            IF parentState = "Running" THEN "Pending" ELSE "Discarded"]
    /\ cancelState' = [cancelState EXCEPT ![child] =
        IF @ = "Requested" THEN "Acknowledged" ELSE @]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childEpoch,
                    deliveryCount, started>>

DeliverResult(child, owner) ==
    /\ child \in started
    /\ parentState = "Running"
    /\ parentOwner = owner
    /\ owner \in Owners
    /\ resultState[child] = "Pending"
    /\ deliveryCount[child] = 0
    /\ resultState' = [resultState EXCEPT ![child] = "Delivered"]
    /\ deliveryCount' = [deliveryCount EXCEPT ![child] = 1]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childState, childOwner,
                    childEpoch, cancelState, started>>

\* An at-least-once redelivery after the parent commit is an explicit no-op.
DuplicateDelivery(child) ==
    /\ child \in started
    /\ resultState[child] = "Delivered"
    /\ UNCHANGED vars

EndParent ==
    /\ parentState = "Running"
    /\ parentState' = "Ended"
    /\ parentOwner' = NoOwner
    /\ resultState' =
        [c \in Children |->
            IF resultState[c] = "Pending" THEN "Discarded" ELSE resultState[c]]
    /\ cancelState' =
        [c \in Children |->
            IF c \in started /\ childState[c] \in {"Running", "Awaiting"}
                THEN "Requested"
                ELSE cancelState[c]]
    /\ UNCHANGED <<parentEpoch, childState, childOwner, childEpoch,
                    deliveryCount, started>>

AcknowledgeCancel(child) ==
    /\ child \in started
    /\ cancelState[child] = "Requested"
    /\ childState[child] \in {"Running", "Awaiting"}
    /\ childState' = [childState EXCEPT ![child] = "Cancelled"]
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ cancelState' = [cancelState EXCEPT ![child] = "Acknowledged"]
    /\ UNCHANGED <<parentState, parentOwner, parentEpoch, childEpoch, resultState,
                    deliveryCount, started>>

Next ==
    \/ \E owner \in Owners: ClaimParent(owner)
    \/ \E owner \in Owners: CrashParent(owner)
    \/ \E child \in Children: StartChild(child)
    \/ \E child \in Children, owner \in Owners: ClaimChild(child, owner)
    \/ \E child \in Children, owner \in Owners: CrashChild(child, owner)
    \/ \E child \in Children, owner \in Owners: AwaitChild(child, owner)
    \/ \E child \in Children, owner \in Owners: ResumeChild(child, owner)
    \/ \E child \in Children, owner \in Owners: FinishChild(child, owner)
    \/ \E child \in Children, owner \in Owners: DeliverResult(child, owner)
    \/ \E child \in Children: DuplicateDelivery(child)
    \/ \E child \in Children: AcknowledgeCancel(child)
    \/ EndParent

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ parentState \in ParentStates
    /\ parentOwner \in Owners \cup {NoOwner}
    /\ parentEpoch \in 0..MaxEpoch
    /\ childState \in [Children -> ChildStates]
    /\ childOwner \in [Children -> (Owners \cup {NoOwner})]
    /\ childEpoch \in [Children -> 0..MaxEpoch]
    /\ resultState \in [Children -> ResultStates]
    /\ deliveryCount \in [Children -> 0..1]
    /\ cancelState \in [Children -> CancelStates]
    /\ started \subseteq Children

KindsPartitionChildren ==
    /\ LocalChildren \cap RemoteChildren = {}
    /\ LocalChildren \cup RemoteChildren = Children

FirstClassIdentity ==
    \A c \in Children: (c \in started) \equiv (childState[c] # "Absent")

ConstraintsHold ==
    /\ started \subseteq ShallowChildren
    /\ started \cap ParentLineage = {}
    /\ Cardinality(ActiveChildren) <= MaxParallel
    /\ Cardinality(started) <= MaxTotal

ResultIsExactlyOnce ==
    \A c \in Children:
        /\ deliveryCount[c] <= 1
        /\ (resultState[c] = "Delivered") \equiv (deliveryCount[c] = 1)

PendingResultIsDurableGap ==
    \A c \in Children:
        (resultState[c] = "Pending") =>
            (childState[c] = "Ended" /\ parentState = "Running")

EndedParentIsClosed ==
    (parentState = "Ended") =>
        (\A c \in Children: resultState[c] # "Pending")

CancellationIsDurable ==
    \A c \in Children:
        (cancelState[c] = "Requested") =>
            (parentState = "Ended" /\ childState[c] \in {"Running", "Awaiting"})

EndedChildrenHaveNoOwner ==
    \A c \in Children:
        (childState[c] \in {"Ended", "Cancelled", "Awaiting", "Absent"}) =>
            childOwner[c] = NoOwner

=============================================================================
