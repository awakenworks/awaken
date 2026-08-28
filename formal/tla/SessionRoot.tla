----------------------------- MODULE SessionRoot -----------------------------
EXTENDS Naturals

CONSTANTS Owners, NoOwner, Payloads, NoPayload, MaxRevision,
          MaxRealizationEpoch

VARIABLES existence, owner, createPayload, createReceipt, baseline, execution,
          placement, revision, hasInitialEvent, realizationState,
          realizationOwner, realizationEpoch, realizationEffects,
          activationCommitted, workProjected, everTerminal, lastCreateOutcome

vars == <<existence, owner, createPayload, createReceipt, baseline, execution,
          placement, revision, hasInitialEvent, realizationState,
          realizationOwner, realizationEpoch, realizationEffects,
          activationCommitted, workProjected, everTerminal, lastCreateOutcome>>

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

Init == Kernel!Init
Next == Kernel!Next
Spec == Init /\ [][Next]_vars
Safety == Kernel!Safety

=============================================================================
