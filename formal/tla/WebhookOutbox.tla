-------------------------- MODULE WebhookOutbox --------------------------
EXTENDS Naturals, TLC

CONSTANT StableId

VARIABLE committed, queued, delivered, eventId, processUp, crashQueue

vars == <<committed, queued, delivered, eventId, processUp, crashQueue>>

Init == /\ committed = FALSE
        /\ queued = FALSE
        /\ delivered = FALSE
        /\ eventId = StableId
        /\ processUp = TRUE
        /\ crashQueue = FALSE

CommitFact == /\ processUp /\ ~committed
              /\ committed' = TRUE
              /\ queued' = TRUE
              /\ UNCHANGED <<delivered, eventId, processUp, crashQueue>>

DispatchSuccess == /\ processUp /\ queued
                   /\ delivered' = TRUE
                   /\ queued' = FALSE
                   /\ UNCHANGED <<committed, eventId, processUp, crashQueue>>

DispatchFailure == /\ processUp /\ queued
                   /\ UNCHANGED vars

Crash == /\ processUp
         /\ processUp' = FALSE
         /\ crashQueue' = queued
         /\ UNCHANGED <<committed, queued, delivered, eventId>>

Restart == /\ ~processUp
           /\ processUp' = TRUE
           /\ UNCHANGED <<committed, queued, delivered, eventId, crashQueue>>

Next == CommitFact \/ DispatchSuccess \/ DispatchFailure \/ Crash \/ Restart

TypeOK == /\ committed \in BOOLEAN /\ queued \in BOOLEAN
          /\ delivered \in BOOLEAN /\ processUp \in BOOLEAN
          /\ crashQueue \in BOOLEAN /\ eventId = StableId
CommittedFactIsRecoverable == committed => (queued \/ delivered)
DeliveryRequiresCommit == delivered => committed
StableIdentityNeverChanges == eventId = StableId
CrashPreservesPending == (~processUp /\ crashQueue) => (queued \/ delivered)

Spec == Init /\ [][Next]_vars
=============================================================================
