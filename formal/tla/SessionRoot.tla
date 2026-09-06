----------------------------- MODULE SessionRoot -----------------------------
EXTENDS Naturals

CONSTANTS Owners, NoOwner, Payloads, NoPayload, MaxRevision,
          MaxRealizationEpoch, ObservationTimes, UnknownTime

VARIABLES existence, owner, createPayload, createReceipt, baseline, execution,
          placement, revision, hasInitialEvent, realizationState,
          realizationOwner, realizationEpoch, realizationEffects,
          activationCommitted, workProjected, everTerminal, lastCreateOutcome, createdAt, updatedAt

vars == <<existence, owner, createPayload, createReceipt, baseline, execution,
          placement, revision, hasInitialEvent, realizationState,
          realizationOwner, realizationEpoch, realizationEffects,
          activationCommitted, workProjected, everTerminal, lastCreateOutcome, createdAt, updatedAt>>

Kernel == INSTANCE SessionRootKernel WITH
    Owners <- Owners,
    NoOwner <- NoOwner,
    Payloads <- Payloads,
    NoPayload <- NoPayload,
    MaxRevision <- MaxRevision,
    MaxRealizationEpoch <- MaxRealizationEpoch,
    kExistence <- existence,
    kOwner <- owner,
    kCreatePayload <- createPayload,
    kCreateReceipt <- createReceipt,
    kBaseline <- baseline,
    kExecution <- execution,
    kPlacement <- placement,
    kRevision <- revision,
    kHasInitialEvent <- hasInitialEvent,
    kRealizationState <- realizationState,
    kRealizationOwner <- realizationOwner,
    kRealizationEpoch <- realizationEpoch,
    kRealizationEffects <- realizationEffects,
    kActivationCommitted <- activationCommitted,
    kWorkProjected <- workProjected,
    kEverTerminal <- everTerminal,
    kLastCreateOutcome <- lastCreateOutcome

ASSUME /\ ObservationTimes \subseteq Nat
       /\ ObservationTimes /= {}
       /\ UnknownTime \notin ObservationTimes

Init == /\ Kernel!Init
        /\ createdAt = UnknownTime
        /\ updatedAt = UnknownTime

\* Observe once at the existing root transaction; neither payload identity
\* nor transition admission depends on wall time. Equal and backwards clocks
\* are deliberately included. Exact replay/conflict leaves revision unchanged.
CreatedObservation(wasPresent, isPresent, before, now) ==
    IF ~wasPresent /\ isPresent THEN now ELSE before
UpdatedObservation(beforeRevision, afterRevision, before, now) ==
    IF afterRevision > beforeRevision
    THEN IF before = UnknownTime THEN now ELSE IF now > before THEN now ELSE before
    ELSE before
RootObservationStep(before, after, now) ==
    /\ after.created = CreatedObservation(before.exists, after.exists, before.created, now)
    /\ after.updated = UpdatedObservation(before.revision, after.revision, before.updated, now)

Next == /\ Kernel!Next
        /\ \E now \in ObservationTimes :
             /\ createdAt' = CreatedObservation(existence # "Absent", existence' # "Absent", createdAt, now)
             /\ updatedAt' = UpdatedObservation(revision, revision', updatedAt, now)
Spec == Init /\ [][Next]_vars
Safety == /\ Kernel!Safety
          /\ (existence = "Absent" => createdAt = UnknownTime /\ updatedAt = UnknownTime)
          /\ (existence # "Absent" => /\ createdAt \in ObservationTimes
                                       /\ updatedAt \in ObservationTimes
                                       /\ updatedAt >= createdAt)
CreationTimeIsImmutable == [][createdAt # UnknownTime => createdAt' = createdAt]_vars
UpdateTimeNeverRegresses == [][updatedAt # UnknownTime => updatedAt' >= updatedAt]_vars
NoRevisionChangePreservesTime == [][revision' = revision => updatedAt' = updatedAt]_vars

=============================================================================
