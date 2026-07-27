-------------------- MODULE WorkerCredentialLiveness --------------------
EXTENDS Naturals, TLC

CONSTANTS MaxTime, RequiredRevision, OtherRevision

VARIABLES workerLive, observationState, observationRevision,
          observedAt, validUntil, now, localState,
          claimed, currentEpoch, claimEpoch, probeFailures,
          workerDrainedByProbe, executed, executionExact,
          executionFresh, executionEpochCurrent

vars == <<workerLive, observationState, observationRevision,
          observedAt, validUntil, now, localState,
          claimed, currentEpoch, claimEpoch, probeFailures,
          workerDrainedByProbe, executed, executionExact,
          executionFresh, executionEpochCurrent>>

Fresh == observedAt <= now /\ now < validUntil
Exact == observationRevision = RequiredRevision
Selectable == workerLive /\ observationState = "Available" /\ Exact /\ Fresh

Init == /\ workerLive = TRUE
        /\ observationState = "Unknown"
        /\ observationRevision = OtherRevision
        /\ observedAt = 0 /\ validUntil = 0 /\ now = 0
        /\ localState = "LoginRequired"
        /\ claimed = FALSE /\ currentEpoch = 1 /\ claimEpoch = 0
        /\ probeFailures = 0 /\ workerDrainedByProbe = FALSE
        /\ executed = FALSE /\ executionExact = FALSE
        /\ executionFresh = FALSE /\ executionEpochCurrent = FALSE

Tick == /\ now < MaxTime
        /\ now' = now + 1
        /\ UNCHANGED <<workerLive, observationState, observationRevision,
                        observedAt, validUntil, localState, claimed,
                        currentEpoch, claimEpoch, probeFailures,
                        workerDrainedByProbe, executed, executionExact,
                        executionFresh, executionEpochCurrent>>

ProbeAvailable == /\ observationState' = "Available"
                  /\ observationRevision' \in {RequiredRevision, OtherRevision}
                  /\ observedAt' = now /\ validUntil' = now + 2
                  /\ localState' = "Available"
                  /\ UNCHANGED <<workerLive, now, claimed, currentEpoch,
                                  claimEpoch, probeFailures, workerDrainedByProbe,
                                  executed, executionExact, executionFresh,
                                  executionEpochCurrent>>

ProbeUnavailable == /\ observationState' \in {"LoginRequired", "Expired", "Invalid", "ProbeFailed"}
                    /\ observationRevision' = RequiredRevision
                    /\ observedAt' = now /\ validUntil' = now + 2
                    /\ localState' = observationState'
                    /\ UNCHANGED <<workerLive, now, claimed, currentEpoch,
                                    claimEpoch, probeFailures, workerDrainedByProbe,
                                    executed, executionExact, executionFresh,
                                    executionEpochCurrent>>

ProbeFailure == /\ probeFailures < 2
                /\ probeFailures' = probeFailures + 1
                /\ workerDrainedByProbe' = workerDrainedByProbe
                /\ UNCHANGED <<workerLive, observationState, observationRevision,
                                observedAt, validUntil, now, localState, claimed,
                                currentEpoch, claimEpoch, executed, executionExact,
                                executionFresh, executionEpochCurrent>>

PlaceAndClaim == /\ Selectable /\ ~claimed
                 /\ claimed' = TRUE /\ claimEpoch' = currentEpoch
                 /\ UNCHANGED <<workerLive, observationState, observationRevision,
                                 observedAt, validUntil, now, localState,
                                 currentEpoch, probeFailures, workerDrainedByProbe,
                                 executed, executionExact, executionFresh,
                                 executionEpochCurrent>>

LocalLoss == /\ localState = "Available"
             /\ localState' = "LoginRequired"
             /\ UNCHANGED <<workerLive, observationState, observationRevision,
                             observedAt, validUntil, now, claimed, currentEpoch,
                             claimEpoch, probeFailures, workerDrainedByProbe,
                             executed, executionExact, executionFresh,
                             executionEpochCurrent>>

Reclaim == /\ claimed /\ currentEpoch < 3
           /\ currentEpoch' = currentEpoch + 1 /\ claimed' = FALSE
           /\ UNCHANGED <<workerLive, observationState, observationRevision,
                           observedAt, validUntil, now, localState, claimEpoch,
                           probeFailures, workerDrainedByProbe, executed,
                           executionExact, executionFresh, executionEpochCurrent>>

LoseWorkerLease == /\ workerLive
                   /\ workerLive' = FALSE
                   /\ UNCHANGED <<observationState, observationRevision, observedAt,
                                   validUntil, now, localState, claimed, currentEpoch,
                                   claimEpoch, probeFailures, workerDrainedByProbe,
                                   executed, executionExact, executionFresh,
                                   executionEpochCurrent>>

RevalidateAndExecute == /\ claimed /\ claimEpoch = currentEpoch
                        /\ Selectable /\ localState = "Available"
                        /\ executed' = TRUE
                        /\ executionExact' = Exact
                        /\ executionFresh' = Fresh
                        /\ executionEpochCurrent' = (claimEpoch = currentEpoch)
                        /\ UNCHANGED <<workerLive, observationState,
                                        observationRevision, observedAt, validUntil,
                                        now, localState, claimed, currentEpoch,
                                        claimEpoch, probeFailures,
                                        workerDrainedByProbe>>

Next == Tick \/ ProbeAvailable \/ ProbeUnavailable \/ ProbeFailure
        \/ PlaceAndClaim \/ LocalLoss \/ Reclaim \/ LoseWorkerLease
        \/ RevalidateAndExecute

TypeOK == /\ workerLive \in BOOLEAN
          /\ observationState \in {"Unknown", "Available", "LoginRequired", "Expired", "Invalid", "ProbeFailed"}
          /\ observationRevision \in {RequiredRevision, OtherRevision}
          /\ observedAt \in 0..(MaxTime + 2) /\ validUntil \in 0..(MaxTime + 2)
          /\ now \in 0..MaxTime /\ localState \in {"Available", "LoginRequired", "Expired", "Invalid", "ProbeFailed"}
          /\ claimed \in BOOLEAN /\ currentEpoch \in 1..3 /\ claimEpoch \in 0..3
          /\ probeFailures \in 0..2 /\ workerDrainedByProbe \in BOOLEAN
          /\ executed \in BOOLEAN /\ executionExact \in BOOLEAN
          /\ executionFresh \in BOOLEAN /\ executionEpochCurrent \in BOOLEAN

ExpiredNeverExecutes == executed => executionFresh
ExactRevisionOnly == executed => executionExact
CurrentEpochOnly == executed => executionEpochCurrent
ProbeFailureDoesNotDrainWorker == ~workerDrainedByProbe

Spec == Init /\ [][Next]_vars
=============================================================================
