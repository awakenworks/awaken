--------------------------- MODULE WorkerDrain ---------------------------
EXTENDS Naturals, TLC

CONSTANT MaxClaims

VARIABLE gate, claimInProgress, claimAttempts, claims,
         drainRequested, registryState, drainAcknowledged,
         attemptsAtRegistryDrain, claimsAtDrain

vars == <<gate, claimInProgress, claimAttempts, claims,
          drainRequested, registryState, drainAcknowledged,
          attemptsAtRegistryDrain, claimsAtDrain>>

Init == /\ gate = "Open" /\ claimInProgress = FALSE
        /\ claimAttempts = 0 /\ claims = 0
        /\ drainRequested = FALSE /\ registryState = "Ready"
        /\ drainAcknowledged = FALSE
        /\ attemptsAtRegistryDrain = 0 /\ claimsAtDrain = 0

StartClaim == /\ gate = "Open" /\ ~claimInProgress
              /\ claimAttempts < MaxClaims
              /\ claimInProgress' = TRUE
              /\ claimAttempts' = claimAttempts + 1
              /\ UNCHANGED <<gate, claims, drainRequested, registryState,
                              drainAcknowledged, attemptsAtRegistryDrain,
                              claimsAtDrain>>

FinishClaim == /\ claimInProgress
               /\ claimInProgress' = FALSE /\ claims' = claims + 1
               /\ UNCHANGED <<gate, claimAttempts, drainRequested,
                               registryState, drainAcknowledged,
                               attemptsAtRegistryDrain, claimsAtDrain>>

RequestDrain == /\ ~drainRequested
                /\ drainRequested' = TRUE
                /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims,
                                registryState, drainAcknowledged,
                                attemptsAtRegistryDrain, claimsAtDrain>>

CloseAdmission == /\ drainRequested /\ gate = "Open" /\ ~claimInProgress
                  /\ gate' = "Closed"
                  /\ UNCHANGED <<claimInProgress, claimAttempts, claims,
                                  drainRequested, registryState,
                                  drainAcknowledged, attemptsAtRegistryDrain,
                                  claimsAtDrain>>

PublishRegistryDrain == /\ drainRequested /\ gate = "Closed"
                        /\ registryState = "Ready"
                        /\ registryState' = "Draining"
                        /\ attemptsAtRegistryDrain' = claimAttempts
                        /\ UNCHANGED <<gate, claimInProgress, claimAttempts,
                                        claims, drainRequested,
                                        drainAcknowledged, claimsAtDrain>>

AcknowledgeDrain == /\ registryState = "Draining"
                    /\ ~drainAcknowledged
                    /\ drainAcknowledged' = TRUE
                    /\ claimsAtDrain' = claims
                    /\ UNCHANGED <<gate, claimInProgress, claimAttempts,
                                    claims, drainRequested, registryState,
                                    attemptsAtRegistryDrain>>

RepeatDrain == /\ drainRequested /\ gate = "Closed"
               /\ UNCHANGED vars

Next == StartClaim \/ FinishClaim \/ RequestDrain \/ CloseAdmission
        \/ PublishRegistryDrain \/ AcknowledgeDrain \/ RepeatDrain

TypeOK == /\ gate \in {"Open", "Closed"} /\ claimInProgress \in BOOLEAN
          /\ claimAttempts \in 0..MaxClaims /\ claims \in 0..MaxClaims
          /\ claims <= claimAttempts /\ drainRequested \in BOOLEAN
          /\ registryState \in {"Ready", "Draining"}
          /\ drainAcknowledged \in BOOLEAN
          /\ attemptsAtRegistryDrain \in 0..MaxClaims
          /\ claimsAtDrain \in 0..MaxClaims
DrainIsAbsorbing == drainAcknowledged => gate = "Closed"
NoClaimAfterDrainAcknowledgement == drainAcknowledged => claims = claimsAtDrain
RegistryDrainFollowsLocalFence == registryState = "Draining" => gate = "Closed"
NoClaimAttemptAfterRegistryDrain ==
    registryState = "Draining" => claimAttempts = attemptsAtRegistryDrain

Spec == Init /\ [][Next]_vars
=============================================================================
