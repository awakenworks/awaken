-------------------------- MODULE WebhookOutbox --------------------------
EXTENDS Naturals, TLC

CONSTANT StableId

VARIABLE lifecycleCommitted, queued, delivered, eventId, processUp

vars == <<lifecycleCommitted, queued, delivered, eventId, processUp>>

Init == /\ lifecycleCommitted = FALSE
        /\ queued = FALSE
        /\ delivered = FALSE
        /\ eventId = StableId
        /\ processUp = TRUE

\* One repository transaction commits the lifecycle fact and its outbox row.
CommitLifecycleAndOutbox ==
    /\ processUp /\ ~lifecycleCommitted
    /\ lifecycleCommitted' = TRUE
    /\ queued' = TRUE
    /\ UNCHANGED <<delivered, eventId, processUp>>

DispatchSuccess == /\ processUp /\ queued
                   /\ delivered' = TRUE
                   /\ queued' = FALSE
                   /\ UNCHANGED <<lifecycleCommitted, eventId, processUp>>

DispatchFailure == /\ processUp /\ queued
                   /\ UNCHANGED vars

Crash == /\ processUp
         /\ processUp' = FALSE
         /\ UNCHANGED <<lifecycleCommitted, queued, delivered, eventId>>

Restart == /\ ~processUp
           /\ processUp' = TRUE
           /\ UNCHANGED <<lifecycleCommitted, queued, delivered, eventId>>

Next == CommitLifecycleAndOutbox \/ DispatchSuccess \/ DispatchFailure \/ Crash \/ Restart

TypeOK == /\ lifecycleCommitted \in BOOLEAN /\ queued \in BOOLEAN
          /\ delivered \in BOOLEAN /\ processUp \in BOOLEAN
          /\ eventId = StableId
NoCommittedLifecycleWithoutOutbox == lifecycleCommitted => (queued \/ delivered)
DeliveryRequiresCommittedLifecycle == delivered => lifecycleCommitted
StableIdentityNeverChanges == eventId = StableId
CrashCannotErasePending == (~processUp /\ lifecycleCommitted /\ ~delivered) => queued

Spec == Init /\ [][Next]_vars
=============================================================================
