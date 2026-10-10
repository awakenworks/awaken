----------------------------- MODULE RemoteTool -----------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Operations, MaxRequests

ASSUME /\ Operations # {}
       /\ MaxRequests \in Nat \ {0}

LedgerStates == {"Absent", "Executing", "Completed"}
BrainStates == {"Unknown", "Completed"}

VARIABLES ledger, brain, invokes, requests, handUp, cache, cancelRequested, cancelledCompletion

vars == <<ledger, brain, invokes, requests, handUp, cache, cancelRequested, cancelledCompletion>>

Init ==
    /\ ledger = [op \in Operations |-> "Absent"]
    /\ brain = [op \in Operations |-> "Unknown"]
    /\ invokes = [op \in Operations |-> 0]
    /\ requests = [op \in Operations |-> 0]
    /\ handUp = TRUE
    /\ cache = [op \in Operations |-> "Missing"]
    /\ cancelRequested = [op \in Operations |-> FALSE]
    /\ cancelledCompletion = [op \in Operations |-> FALSE]

Dispatch(op) ==
    /\ handUp
    /\ brain[op] = "Unknown"
    /\ ledger[op] = "Absent"
    /\ requests[op] < MaxRequests
    /\ ledger' = [ledger EXCEPT ![op] = "Executing"]
    /\ cache' = [cache EXCEPT ![op] = "Executing"]
    /\ invokes' = [invokes EXCEPT ![op] = @ + 1]
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ UNCHANGED <<brain, handUp, cancelRequested, cancelledCompletion>>

Complete(op) ==
    /\ handUp
    /\ ledger[op] = "Executing"
    /\ cache[op] = "Executing"
    /\ ~cancelRequested[op]
    /\ cache' = [cache EXCEPT ![op] = "Known"]
    /\ ledger' = [ledger EXCEPT ![op] = "Completed"]
    /\ UNCHANGED <<brain, invokes, requests, handUp, cancelRequested, cancelledCompletion>>

Deliver(op) ==
    /\ handUp
    /\ ledger[op] = "Completed"
    /\ cache[op] = "Known"
    /\ brain[op] = "Unknown"
    /\ brain' = [brain EXCEPT ![op] = "Completed"]
    /\ UNCHANGED <<ledger, invokes, requests, handUp, cache, cancelRequested, cancelledCompletion>>

RetryCompleted(op) ==
    /\ handUp
    /\ ledger[op] = "Completed"
    /\ cache[op] = "Known"
    /\ brain[op] = "Unknown"
    /\ requests[op] < MaxRequests
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ brain' = [brain EXCEPT ![op] = "Completed"]
    /\ UNCHANGED <<ledger, invokes, handUp, cache, cancelRequested, cancelledCompletion>>

RetryExecuting(op) ==
    /\ handUp
    /\ ledger[op] = "Executing"
    /\ brain[op] = "Unknown"
    /\ requests[op] < MaxRequests
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ UNCHANGED <<ledger, brain, invokes, handUp, cache, cancelRequested, cancelledCompletion>>

Crash ==
    /\ handUp
    /\ handUp' = FALSE
    /\ cache' = [op \in Operations |-> "Missing"]
    /\ cancelRequested' = [op \in Operations |-> FALSE]
    /\ UNCHANGED <<ledger, brain, invokes, requests, cancelledCompletion>>

Restart ==
    /\ ~handUp
    /\ handUp' = TRUE
    /\ UNCHANGED <<ledger, brain, invokes, requests, cache, cancelRequested, cancelledCompletion>>

Observe(op) ==
    /\ handUp
    /\ requests[op] < MaxRequests
    /\ requests' = [requests EXCEPT ![op] = @ + 1]
    /\ UNCHANGED <<ledger, brain, invokes, handUp, cache, cancelRequested, cancelledCompletion>>

Cancel(op) ==
    /\ handUp
    /\ cache[op] = "Executing"
    /\ ~cancelRequested[op]
    /\ cancelRequested' = [cancelRequested EXCEPT ![op] = TRUE]
    /\ UNCHANGED <<ledger, brain, invokes, requests, handUp, cache, cancelledCompletion>>

FinishCancelled(op) ==
    /\ handUp
    /\ cache[op] = "Executing"
    /\ cancelRequested[op]
    /\ cache' = [cache EXCEPT ![op] = "Unknown"]
    /\ ledger' = [ledger EXCEPT ![op] = "Completed"]
    /\ cancelledCompletion' = [cancelledCompletion EXCEPT ![op] = TRUE]
    /\ UNCHANGED <<brain, invokes, requests, handUp, cancelRequested>>

Next ==
    \/ \E op \in Operations: Dispatch(op)
    \/ \E op \in Operations: Complete(op)
    \/ \E op \in Operations: Deliver(op)
    \/ \E op \in Operations: RetryCompleted(op)
    \/ \E op \in Operations: RetryExecuting(op)
    \/ \E op \in Operations: Observe(op)
    \/ \E op \in Operations: Cancel(op)
    \/ \E op \in Operations: FinishCancelled(op)
    \/ Crash
    \/ Restart

TypeOK ==
    /\ ledger \in [Operations -> LedgerStates]
    /\ brain \in [Operations -> BrainStates]
    /\ invokes \in [Operations -> Nat]
    /\ requests \in [Operations -> 0..MaxRequests]
    /\ handUp \in BOOLEAN
    /\ cache \in [Operations -> {"Missing", "Executing", "Known", "Unknown"}]
    /\ cancelRequested \in [Operations -> BOOLEAN]
    /\ cancelledCompletion \in [Operations -> BOOLEAN]

AtMostOnceInvoke == \A op \in Operations: invokes[op] <= 1

CompletionHasOneInvocation ==
    \A op \in Operations: ledger[op] = "Completed" => invokes[op] = 1

KnownResultIsDurable ==
    \A op \in Operations: brain[op] = "Completed" => ledger[op] = "Completed"

RetryNeverChangesInvocationCount ==
    \A op \in Operations: requests[op] >= invokes[op]

CancelledCompletionNeverKnown ==
    \A op \in Operations: cancelledCompletion[op] => brain[op] = "Unknown"

CachedResultHasCompletion ==
    \A op \in Operations: cache[op] = "Known" => ledger[op] = "Completed"

Safety ==
    /\ TypeOK
    /\ AtMostOnceInvoke
    /\ CompletionHasOneInvocation
    /\ KnownResultIsDurable
    /\ RetryNeverChangesInvocationCount
    /\ CancelledCompletionNeverKnown
    /\ CachedResultHasCompletion

Spec == Init /\ [][Next]_vars

=============================================================================
