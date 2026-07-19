----------------------------- MODULE Delegation -----------------------------
EXTENDS Naturals, FiniteSets, RuntimeVocabulary

\* Run-scoped relationship registry. Tool execution/result delivery is modeled
\* once by ToolBatch; this module owns only identity, budgets, lineage, child
\* lifecycle observation, and durable cancellation.
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
    childOwner,
    childEpoch,
    started

vars == <<parentEnded, childState, linkStatus, childOwner, childEpoch, started>>

Init ==
    /\ parentEnded = FALSE
    /\ childState = [c \in Children |-> "Absent"]
    /\ linkStatus = [c \in Children |-> "Absent"]
    /\ childOwner = [c \in Children |-> NoOwner]
    /\ childEpoch = [c \in Children |-> 0]
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
    /\ UNCHANGED <<parentEnded, childState, childOwner, childEpoch>>

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
    /\ UNCHANGED <<parentEnded, linkStatus, started>>

CrashChild(child, owner) ==
    /\ childOwner[child] = owner
    /\ childState[child] \in {"Running", "Awaiting"}
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ UNCHANGED <<parentEnded, childState, linkStatus, childEpoch, started>>

ReclaimChild(child, owner) ==
    /\ childState[child] \in {"Running", "Awaiting"}
    /\ childOwner[child] = NoOwner
    /\ childEpoch[child] < MaxEpoch
    /\ childOwner' = [childOwner EXCEPT ![child] = owner]
    /\ childEpoch' = [childEpoch EXCEPT ![child] = @ + 1]
    /\ UNCHANGED <<parentEnded, childState, linkStatus, started>>

AwaitChild(child, owner) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Open"
    /\ childState[child] = "Running"
    /\ childOwner[child] = owner
    /\ childState' = [childState EXCEPT ![child] = "Awaiting"]
    /\ UNCHANGED <<parentEnded, linkStatus, childOwner, childEpoch, started>>

ResumeChild(child, owner) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Open"
    /\ childState[child] = "Awaiting"
    /\ childOwner[child] = owner
    /\ childState' = [childState EXCEPT ![child] = "Running"]
    /\ UNCHANGED <<parentEnded, linkStatus, childOwner, childEpoch, started>>

FinishChild(child, owner) ==
    /\ ~parentEnded
    /\ linkStatus[child] = "Open"
    /\ childState[child] \in {"Running", "Awaiting"}
    /\ childOwner[child] = owner
    /\ childState' = [childState EXCEPT ![child] = "Ended"]
    /\ childOwner' = [childOwner EXCEPT ![child] = NoOwner]
    /\ linkStatus' = [linkStatus EXCEPT ![child] = "Completed"]
    /\ UNCHANGED <<parentEnded, childEpoch, started>>

EndParent ==
    /\ ~parentEnded
    /\ parentEnded' = TRUE
    /\ linkStatus' = [c \in Children |->
         IF linkStatus[c] = "Open" THEN "CancelRequested" ELSE linkStatus[c]]
    /\ UNCHANGED <<childState, childOwner, childEpoch, started>>

Next ==
    \/ \E child \in Children: Request(child)
    \/ \E child \in Children: DuplicateRequest(child)
    \/ \E child \in Children, owner \in Owners: StartChild(child, owner)
    \/ \E child \in Children, owner \in Owners: CrashChild(child, owner)
    \/ \E child \in Children, owner \in Owners: ReclaimChild(child, owner)
    \/ \E child \in Children, owner \in Owners: AwaitChild(child, owner)
    \/ \E child \in Children, owner \in Owners: ResumeChild(child, owner)
    \/ \E child \in Children, owner \in Owners: FinishChild(child, owner)
    \/ EndParent

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ parentEnded \in BOOLEAN
    /\ childState \in [Children -> DelegatedChildStates]
    /\ linkStatus \in [Children -> DelegationLinkStatuses]
    /\ childOwner \in [Children -> Owners \cup {NoOwner}]
    /\ childEpoch \in [Children -> 0..MaxEpoch]
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
        linkStatus[c] = "Completed" => childState[c] = "Ended"

EndedParentIsClosed == parentEnded => \A c \in Children: linkStatus[c] # "Open"

CancellationIsDurable ==
    \A c \in Children:
        linkStatus[c] = "CancelRequested" => parentEnded

EndedChildrenHaveNoOwner ==
    \A c \in Children:
        childState[c] \in {"Absent", "Ended"} => childOwner[c] = NoOwner

=============================================================================
