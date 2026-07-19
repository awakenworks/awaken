----------------------------- MODULE RemoteTool -----------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Operations, MaxRequests

ASSUME /\ Operations # {}
       /\ MaxRequests \in Nat \ {0}

LedgerStates == {"Absent", "Executing", "Completed"}
BrainStates == {"Unknown", "Completed"}

VARIABLES ledger, brain, invokes, requests, handUp

vars == <<ledger, brain, invokes, requests, handUp>>

Init ==
    /\ ledger = [op \in Operations |-> "Absent"]
    /\ brain = [op \in Operations |-> "Unknown"]
    /\ invokes = [op \in Operations |-> 0]
    /\ requests = [op \in Operations |-> 0]
    /\ handUp = TRUE

Dispatch(op) ==
    /\ handUp
    /\ brain[op] = "Unknown"
    /\ ledger[op] = "Absent"
    /\ requests[op] < MaxRequests
    /\ ledger' = [ledger EXCEPT ![op] = "Executing"]
    /\ invokes' = [invokes EXCEPT ![op] = @ + 1]
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ UNCHANGED <<brain, handUp>>

Complete(op) ==
    /\ handUp
    /\ ledger[op] = "Executing"
    /\ ledger' = [ledger EXCEPT ![op] = "Completed"]
    /\ UNCHANGED <<brain, invokes, requests, handUp>>

Deliver(op) ==
    /\ handUp
    /\ ledger[op] = "Completed"
    /\ brain[op] = "Unknown"
    /\ brain' = [brain EXCEPT ![op] = "Completed"]
    /\ UNCHANGED <<ledger, invokes, requests, handUp>>

RetryCompleted(op) ==
    /\ handUp
    /\ ledger[op] = "Completed"
    /\ brain[op] = "Unknown"
    /\ requests[op] < MaxRequests
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ brain' = [brain EXCEPT ![op] = "Completed"]
    /\ UNCHANGED <<ledger, invokes, handUp>>

RetryExecuting(op) ==
    /\ handUp
    /\ ledger[op] = "Executing"
    /\ brain[op] = "Unknown"
    /\ requests[op] < MaxRequests
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ UNCHANGED <<ledger, brain, invokes, handUp>>

Crash ==
    /\ handUp
    /\ handUp' = FALSE
    /\ UNCHANGED <<ledger, brain, invokes, requests>>

Restart ==
    /\ ~handUp
    /\ handUp' = TRUE
    /\ UNCHANGED <<ledger, brain, invokes, requests>>

Next ==
    \/ \E op \in Operations: Dispatch(op)
    \/ \E op \in Operations: Complete(op)
    \/ \E op \in Operations: Deliver(op)
    \/ \E op \in Operations: RetryCompleted(op)
    \/ \E op \in Operations: RetryExecuting(op)
    \/ Crash
    \/ Restart

TypeOK ==
    /\ ledger \in [Operations -> LedgerStates]
    /\ brain \in [Operations -> BrainStates]
    /\ invokes \in [Operations -> Nat]
    /\ requests \in [Operations -> 0..MaxRequests]
    /\ handUp \in BOOLEAN

AtMostOnceInvoke == \A op \in Operations: invokes[op] <= 1

CompletionHasOneInvocation ==
    \A op \in Operations: ledger[op] = "Completed" => invokes[op] = 1

KnownResultIsDurable ==
    \A op \in Operations: brain[op] = "Completed" => ledger[op] = "Completed"

RetryNeverChangesInvocationCount ==
    \A op \in Operations: requests[op] >= invokes[op]

Safety ==
    /\ TypeOK
    /\ AtMostOnceInvoke
    /\ CompletionHasOneInvocation
    /\ KnownResultIsDurable
    /\ RetryNeverChangesInvocationCount

Spec == Init /\ [][Next]_vars

=============================================================================
