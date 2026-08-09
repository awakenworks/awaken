-------------------- MODULE WorkerCredentialLiveness --------------------
EXTENDS Naturals, TLC

CONSTANTS MaxTime, RequiredRevision, OtherRevision

VARIABLES workerLive, observationState, observationRevision,
          observedAt, validUntil, now, localState, heartbeatSequence,
          maxReceivedSequence, revisionAtMaxSequence,
          claimed, currentEpoch, claimEpoch, probeFailures,
          workerDrainedByProbe, executed, executionExact,
          executionFresh, executionEpochCurrent

vars == <<workerLive, observationState, observationRevision,
          observedAt, validUntil, now, localState, heartbeatSequence,
          maxReceivedSequence, revisionAtMaxSequence,
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
        /\ heartbeatSequence = 0 /\ maxReceivedSequence = 0
        /\ revisionAtMaxSequence = OtherRevision
        /\ claimed = FALSE /\ currentEpoch = 1 /\ claimEpoch = 0
        /\ probeFailures = 0 /\ workerDrainedByProbe = FALSE
        /\ executed = FALSE /\ executionExact = FALSE
        /\ executionFresh = FALSE /\ executionEpochCurrent = FALSE

Tick == /\ now < MaxTime
        /\ now' = now + 1
        /\ UNCHANGED <<workerLive, observationState, observationRevision,
                        observedAt, validUntil, localState, heartbeatSequence,
                        maxReceivedSequence, revisionAtMaxSequence, claimed,
                        currentEpoch, claimEpoch, probeFailures,
                        workerDrainedByProbe, executed, executionExact,
                        executionFresh, executionEpochCurrent>>

ReportedStates == {"Available", "LoginRequired", "Expired", "Invalid", "ProbeFailed"}

\* The registry accepts one complete heartbeat only when its sequence advances.
\* The history variables make out-of-order delivery machine-checkable: the
\* installed observation must always equal the revision carried by the highest
\* sequence received so far.
Heartbeat ==
    \E sequence \in 0..3, revision \in {RequiredRevision, OtherRevision},
       state \in ReportedStates:
      /\ maxReceivedSequence' = IF sequence > maxReceivedSequence
                                   THEN sequence ELSE maxReceivedSequence
      /\ revisionAtMaxSequence' = IF sequence > maxReceivedSequence
                                     THEN revision ELSE revisionAtMaxSequence
      /\ IF sequence > heartbeatSequence
            THEN /\ heartbeatSequence' = sequence
                 /\ observationState' = state
                 /\ observationRevision' = revision
                 /\ observedAt' = now /\ validUntil' = now + 2
                 /\ localState' = state
            ELSE /\ UNCHANGED <<heartbeatSequence, observationState,
                                  observationRevision, observedAt, validUntil,
                                  localState>>
      /\ UNCHANGED <<workerLive, now, claimed, currentEpoch, claimEpoch,
                      probeFailures, workerDrainedByProbe, executed,
                      executionExact, executionFresh, executionEpochCurrent>>

ProbeFailure == /\ probeFailures < 2
                /\ probeFailures' = probeFailures + 1
                /\ workerDrainedByProbe' = workerDrainedByProbe
                /\ UNCHANGED <<workerLive, observationState, observationRevision,
                                observedAt, validUntil, now, localState,
                                heartbeatSequence, maxReceivedSequence,
                                revisionAtMaxSequence, claimed,
                                currentEpoch, claimEpoch, executed, executionExact,
                                executionFresh, executionEpochCurrent>>

PlaceAndClaim == /\ Selectable /\ ~claimed
                 /\ claimed' = TRUE /\ claimEpoch' = currentEpoch
                 /\ UNCHANGED <<workerLive, observationState, observationRevision,
                                 observedAt, validUntil, now, localState,
                                 heartbeatSequence, maxReceivedSequence,
                                 revisionAtMaxSequence, currentEpoch,
                                 probeFailures, workerDrainedByProbe,
                                 executed, executionExact, executionFresh,
                                 executionEpochCurrent>>

LocalLoss == /\ localState = "Available"
             /\ localState' = "LoginRequired"
             /\ UNCHANGED <<workerLive, observationState, observationRevision,
                                 observedAt, validUntil, now, claimed, currentEpoch,
                                 claimEpoch, heartbeatSequence,
                                 maxReceivedSequence, revisionAtMaxSequence,
                                 probeFailures, workerDrainedByProbe,
                             executed, executionExact, executionFresh,
                             executionEpochCurrent>>

Reclaim == /\ claimed /\ currentEpoch < 3
           /\ currentEpoch' = currentEpoch + 1 /\ claimed' = FALSE
           /\ UNCHANGED <<workerLive, observationState, observationRevision,
                           observedAt, validUntil, now, localState, claimEpoch,
                           heartbeatSequence, maxReceivedSequence,
                           revisionAtMaxSequence,
                           probeFailures, workerDrainedByProbe, executed,
                           executionExact, executionFresh, executionEpochCurrent>>

LoseWorkerLease == /\ workerLive
                   /\ workerLive' = FALSE
                   /\ UNCHANGED <<observationState, observationRevision, observedAt,
                                   validUntil, now, localState, claimed, currentEpoch,
                                   claimEpoch, heartbeatSequence,
                                   maxReceivedSequence, revisionAtMaxSequence,
                                   probeFailures, workerDrainedByProbe,
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
                                        now, localState, heartbeatSequence,
                                        maxReceivedSequence, revisionAtMaxSequence,
                                        claimed, currentEpoch,
                                        claimEpoch, probeFailures,
                                        workerDrainedByProbe>>

Next == Tick \/ Heartbeat \/ ProbeFailure
        \/ PlaceAndClaim \/ LocalLoss \/ Reclaim \/ LoseWorkerLease
        \/ RevalidateAndExecute

TypeOK == /\ workerLive \in BOOLEAN
          /\ observationState \in {"Unknown", "Available", "LoginRequired", "Expired", "Invalid", "ProbeFailed"}
          /\ observationRevision \in {RequiredRevision, OtherRevision}
          /\ observedAt \in 0..(MaxTime + 2) /\ validUntil \in 0..(MaxTime + 2)
          /\ now \in 0..MaxTime /\ localState \in {"Available", "LoginRequired", "Expired", "Invalid", "ProbeFailed"}
          /\ heartbeatSequence \in 0..3 /\ maxReceivedSequence \in 0..3
          /\ revisionAtMaxSequence \in {RequiredRevision, OtherRevision}
          /\ claimed \in BOOLEAN /\ currentEpoch \in 1..3 /\ claimEpoch \in 0..3
          /\ probeFailures \in 0..2 /\ workerDrainedByProbe \in BOOLEAN
          /\ executed \in BOOLEAN /\ executionExact \in BOOLEAN
          /\ executionFresh \in BOOLEAN /\ executionEpochCurrent \in BOOLEAN

ExpiredNeverExecutes == executed => executionFresh
ExactRevisionOnly == executed => executionExact
CurrentEpochOnly == executed => executionEpochCurrent
ProbeFailureDoesNotDrainWorker == ~workerDrainedByProbe
HighestHeartbeatSequenceWins ==
    /\ heartbeatSequence = maxReceivedSequence
    /\ observationRevision = revisionAtMaxSequence

Spec == Init /\ [][Next]_vars
=============================================================================
