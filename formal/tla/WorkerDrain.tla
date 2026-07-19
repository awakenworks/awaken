--------------------------- MODULE WorkerDrain ---------------------------
EXTENDS Naturals, TLC

CONSTANT MaxClaims

VARIABLE gate, claimInProgress, claims, drainAcknowledged, claimsAtDrain

vars == <<gate, claimInProgress, claims, drainAcknowledged, claimsAtDrain>>

Init == /\ gate = "Open" /\ claimInProgress = FALSE /\ claims = 0
        /\ drainAcknowledged = FALSE /\ claimsAtDrain = 0

StartClaim == /\ gate = "Open" /\ ~claimInProgress /\ claims < MaxClaims
              /\ claimInProgress' = TRUE
              /\ UNCHANGED <<gate, claims, drainAcknowledged, claimsAtDrain>>

FinishClaim == /\ claimInProgress
               /\ claimInProgress' = FALSE /\ claims' = claims + 1
               /\ UNCHANGED <<gate, drainAcknowledged, claimsAtDrain>>

BeginDrain == /\ gate = "Open" /\ ~claimInProgress
              /\ gate' = "Closed" /\ drainAcknowledged' = TRUE
              /\ claimsAtDrain' = claims
              /\ UNCHANGED <<claimInProgress, claims>>

RepeatDrain == /\ gate = "Closed"
               /\ UNCHANGED vars

Next == StartClaim \/ FinishClaim \/ BeginDrain \/ RepeatDrain

TypeOK == /\ gate \in {"Open", "Closed"} /\ claimInProgress \in BOOLEAN
          /\ claims \in 0..MaxClaims /\ drainAcknowledged \in BOOLEAN
          /\ claimsAtDrain \in 0..MaxClaims
DrainIsAbsorbing == drainAcknowledged => gate = "Closed"
NoClaimAfterDrainAcknowledgement == drainAcknowledged => claims = claimsAtDrain

Spec == Init /\ [][Next]_vars
=============================================================================
