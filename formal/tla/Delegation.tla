----------------------------- MODULE Delegation -----------------------------
EXTENDS Naturals, FiniteSets, RuntimeVocabulary

\* Run-scoped relationship registry. Tool execution/result delivery is modeled
\* once by ToolBatch; this module owns only identity, budgets, lineage, child
\* lifecycle observation, reliable result delivery, and durable cancellation.
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

VARIABLES
    parentEnded,
    childState,
    linkStatus,
    resultState,
    childOwner,
    childEpoch,
    cancelDelivered,
    started

vars == <<parentEnded, childState, linkStatus, resultState, childOwner, childEpoch,
          cancelDelivered, started>>

Init ==
    /\ parentEnded = FALSE
    /\ childState = [c \in Children |-> "Absent"]
    /\ linkStatus = [c \in Children |-> "Absent"]
    /\ resultState = [c \in Children |-> "None"]
    /\ childOwner = [c \in Children |-> NoOwner]
    /\ childEpoch = [c \in Children |-> 0]
    /\ cancelDelivered = [c \in Children |-> FALSE]
    /\ started = 0

Created == {c \in Children: linkStatus[c] # "Absent"}
Active == {c \in Children: linkStatus[c] \in {"Open", "CancelRequested"}}

Request(child) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Absent"
    /\ child \in ShallowChildren
    /\ child \notin ParentLineage
    /\ ChildDepth + 1 <= MaxDepth
    /\ Cardinality(Active) < MaxParallel
    /\ started < MaxTotal
    /\ linkStatus' = [linkStatus EXCEPT ![child] = "Open"]
    /\ started' = started + 1
    /\ UNCHANGED <<parentEnded, childState, resultState, childOwner, childEpoch,
                    cancelDelivered>>

DuplicateRequest(child) ==
    /\ linkStatus[child] # "Absent"
    /\ UNCHANGED vars

StartChild(child, owner) ==
    /\ linkStatus[child] = "Open"
    /\ childState[child] = "Absent"
    /\ childEpoch[child] < MaxEpoch
    /\ childState' = [childState EXCEPT ![child] = "Running"]
    /\ childOwner' = [childOwner EXCEPT ![child] = owner]
    /\ childEpoch' = [childEpoch EXCEPT ![child] = @ + 1]
    /\ UNCHANGED <<parentEnded, linkStatus, resultState, cancelDelivered, started>>

CrashChild(child, owner) ==
    /\ childOwner[child] = owner
    /\ childState[child] = "Running"
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ UNCHANGED <<parentEnded, childState, linkStatus, resultState, childEpoch,
                    cancelDelivered, started>>

ReclaimChild(child, owner) ==
    /\ childState[child] \in {"Running", "Awaiting"}
    /\ childOwner[child] = NoOwner
    /\ childEpoch[child] < MaxEpoch
    /\ childOwner' = [childOwner EXCEPT ![child] = owner]
    /\ childEpoch' = [childEpoch EXCEPT ![child] = @ + 1]
    /\ UNCHANGED <<parentEnded, childState, linkStatus, resultState,
                    cancelDelivered, started>>

AwaitChild(child, owner) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Open"
    /\ childState[child] = "Running"
    /\ childOwner[child] = owner
    /\ childState' = [childState EXCEPT ![child] = "Awaiting"]
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ UNCHANGED <<parentEnded, linkStatus, resultState, childEpoch,
                    cancelDelivered, started>>

\* Input delivery and resume claim are one durable dispatch transaction: an
\* awaiting child cannot become generally runnable between the two operations.
ResumeChild(child, owner) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Open"
    /\ childState[child] = "Awaiting"
    /\ childOwner[child] = NoOwner
    /\ childEpoch[child] < MaxEpoch
    /\ childState' = [childState EXCEPT ![child] = "Running"]
    /\ childOwner' = [childOwner EXCEPT ![child] = owner]
    /\ childEpoch' = [childEpoch EXCEPT ![child] = @ + 1]
    /\ UNCHANGED <<parentEnded, linkStatus, resultState,
                    cancelDelivered, started>>

FinishChild(child, owner) ==
    /\ linkStatus[child] \in {"Open", "CancelRequested"}
    /\ \/ /\ childState[child] = "Running"
           /\ childOwner[child] = owner
       \/ /\ childState[child] = "Awaiting"
           /\ childOwner[child] = NoOwner
    /\ childState' = [childState EXCEPT ![child] = "Ended"]
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ resultState' = [resultState EXCEPT ![child] =
         IF parentEnded THEN "Discarded" ELSE "Ready"]
    /\ UNCHANGED <<parentEnded, linkStatus, childEpoch, cancelDelivered, started>>

\* Parent consumption is a distinct durable boundary from child completion.
\* The implementation removes the delivery envelope in the same ThreadCommit
\* that installs the ToolBatch result and completes the relationship.
ConsumeResult(child) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Open"
    /\ childState[child] = "Ended"
    /\ resultState[child] = "Ready"
    /\ linkStatus' = [linkStatus EXCEPT ![child] = "Completed"]
    /\ resultState' = [resultState EXCEPT ![child] = "Consumed"]
    /\ UNCHANGED <<parentEnded, childState, childOwner, childEpoch,
                    cancelDelivered, started>>

EndParent ==
    /\ ~parentEnded
    /\ parentEnded' = TRUE
    /\ linkStatus' = [c \in Children |->
         IF linkStatus[c] = "Open" THEN "CancelRequested" ELSE linkStatus[c]]
    /\ resultState' = [c \in Children |->
         IF resultState[c] = "Ready" THEN "Discarded" ELSE resultState[c]]
    /\ UNCHANGED <<childState, childOwner, childEpoch, cancelDelivered, started>>

\* Delivery follows the durable parent commit. Re-execution is harmless because
\* adapters address the same child/task identity; eventual network success is an
\* environmental liveness assumption, not claimed by this safety model.
DeliverCancellation(child) ==
    /\ parentEnded
    /\ linkStatus[child] = "CancelRequested"
    /\ ~cancelDelivered[child]
    /\ cancelDelivered' = [cancelDelivered EXCEPT ![child] = TRUE]
    /\ UNCHANGED <<parentEnded, childState, linkStatus, resultState, childOwner,
                    childEpoch, started>>

Next ==
    \/ \E child \in Children: Request(child)
    \/ \E child \in Children: DuplicateRequest(child)
    \/ \E child \in Children, owner \in Owners: StartChild(child, owner)
    \/ \E child \in Children, owner \in Owners: CrashChild(child, owner)
    \/ \E child \in Children, owner \in Owners: ReclaimChild(child, owner)
    \/ \E child \in Children, owner \in Owners: AwaitChild(child, owner)
    \/ \E child \in Children, owner \in Owners: ResumeChild(child, owner)
    \/ \E child \in Children, owner \in Owners: FinishChild(child, owner)
    \/ \E child \in Children: ConsumeResult(child)
    \/ EndParent
    \/ \E child \in Children: DeliverCancellation(child)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ parentEnded \in BOOLEAN
    /\ childState \in [Children -> DelegatedChildStates]
    /\ linkStatus \in [Children -> DelegationLinkStatuses]
    /\ resultState \in [Children -> {"None", "Ready", "Consumed", "Discarded"}]
    /\ childOwner \in [Children -> Owners \cup {NoOwner}]
    /\ childEpoch \in [Children -> 0..MaxEpoch]
    /\ cancelDelivered \in [Children -> BOOLEAN]
    /\ started \in 0..MaxTotal

KindsPartitionChildren ==
    /\ LocalChildren \cup RemoteChildren = Children
    /\ LocalChildren \cap RemoteChildren = {}

FirstClassIdentity ==
    \A c \in Children:
        /\ linkStatus[c] = "Absent" => childState[c] = "Absent"
        /\ childState[c] # "Absent" => linkStatus[c] # "Absent"

ConstraintsHold ==
    /\ Cardinality(Created) = started
    /\ Cardinality(Active) <= MaxParallel
    /\ \A c \in Created:
         /\ c \in ShallowChildren
         /\ c \notin ParentLineage
         /\ ChildDepth + 1 <= MaxDepth

CompletedRelationshipHasEndedChild ==
    \A c \in Children:
        linkStatus[c] = "Completed" =>
            /\ childState[c] = "Ended"
            /\ resultState[c] = "Consumed"

ReadyResultIsDeliverable ==
    \A c \in Children:
        resultState[c] = "Ready" =>
            /\ childState[c] = "Ended"
            /\ linkStatus[c] = "Open"
            /\ ~parentEnded

EndedParentHasNoDeliverableResult ==
    parentEnded => \A c \in Children: resultState[c] # "Ready"

EndedParentIsClosed == parentEnded => \A c \in Children: linkStatus[c] # "Open"

CancellationIsDurable ==
    \A c \in Children:
        linkStatus[c] = "CancelRequested" => parentEnded

CancellationDeliveryFollowsIntent ==
    \A c \in Children:
        cancelDelivered[c] =>
            /\ parentEnded
            /\ linkStatus[c] = "CancelRequested"

EndedChildrenHaveNoOwner ==
    \A c \in Children:
        childState[c] \in {"Absent", "Ended"} => childOwner[c] = NoOwner

=============================================================================
