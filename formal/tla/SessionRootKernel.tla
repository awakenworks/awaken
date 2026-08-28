-------------------------- MODULE SessionRootKernel --------------------------
EXTENDS Naturals

\* Orthogonal durable projection of canonical Session creation and realization.
\* Authoring compilation is represented by the Create precondition: no partial
\* Baseline can cross the atomic root insert. Work projection is disposable and
\* repairable; it is never another Session lifecycle authority.
CONSTANTS Owners, NoOwner, Payloads, NoPayload, MaxRevision,
          MaxRealizationEpoch

ExistenceStates == {"Absent", "Live", "Tombstoned"}
BaselineStates == {"None", "Frozen"}
ExecutionStates == {
    "Preparing", "Activating", "ActivationFailed", "Running",
    "Rescheduling", "Idle", "Terminated"
}
TerminalExecutionStates == {"ActivationFailed", "Terminated"}
Placements == {"None", "Local", "Worker"}
RealizationStates == {"None", "Leased", "Staged", "Activated", "Complete", "Failed"}
CreateOutcomes == {"None", "Applied", "Replayed", "Conflict", "Tombstoned"}

KernelAssumptions ==
    /\ NoOwner \notin Owners
    /\ NoPayload \notin Payloads
    /\ MaxRevision \in Nat \ {0}
    /\ MaxRealizationEpoch \in Nat

VARIABLES
    kExistence,
    kOwner,
    kCreatePayload,
    kCreateReceipt,
    kBaseline,
    kExecution,
    kPlacement,
    kRevision,
    kHasInitialEvent,
    kRealizationState,
    kRealizationOwner,
    kRealizationEpoch,
    kRealizationEffects,
    kActivationCommitted,
    kWorkProjected,
    kEverTerminal,
    kLastCreateOutcome

kVars == <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
           kBaseline, kExecution, kPlacement, kRevision,
           kHasInitialEvent, kRealizationState, kRealizationOwner,
           kRealizationEpoch, kRealizationEffects, kActivationCommitted,
           kWorkProjected, kEverTerminal, kLastCreateOutcome>>

Init ==
    /\ kExistence = "Absent"
    /\ kOwner = NoOwner
    /\ kCreatePayload = NoPayload
    /\ kCreateReceipt = FALSE
    /\ kBaseline = "None"
    /\ kExecution = "Preparing"
    /\ kPlacement = "None"
    /\ kRevision = 0
    /\ kHasInitialEvent = FALSE
    /\ kRealizationState = "None"
    /\ kRealizationOwner = NoOwner
    /\ kRealizationEpoch = 0
    /\ kRealizationEffects = {}
    /\ kActivationCommitted = FALSE
    /\ kWorkProjected = FALSE
    /\ kEverTerminal = FALSE
    /\ kLastCreateOutcome = "None"

\* Owner, receipt, complete Frozen root and optional initial Event plan become
\* visible in one repository transaction.
Create(owner, payload, placement, hasInitialEvent) ==
    /\ owner \in Owners
    /\ payload \in Payloads
    /\ placement \in {"Local", "Worker"}
    /\ hasInitialEvent \in BOOLEAN
    /\ kExistence = "Absent"
    /\ ~kEverTerminal
    /\ MaxRevision > 0
    /\ kExistence' = "Live"
    /\ kOwner' = owner
    /\ kCreatePayload' = payload
    /\ kCreateReceipt' = TRUE
    /\ kBaseline' = "Frozen"
    /\ kExecution' = "Preparing"
    /\ kPlacement' = placement
    /\ kRevision' = 1
    /\ kHasInitialEvent' = hasInitialEvent
    /\ kLastCreateOutcome' = "Applied"
    /\ UNCHANGED <<kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected, kEverTerminal>>

ExactCreateReplay(owner, payload) ==
    /\ kExistence = "Live"
    /\ kOwner = owner
    /\ kCreatePayload = payload
    /\ kCreateReceipt
    /\ kLastCreateOutcome' = "Replayed"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kRevision,
                   kHasInitialEvent, kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected, kEverTerminal>>

RejectConflictingCreate(owner, payload) ==
    /\ owner \in Owners
    /\ payload \in Payloads
    /\ kExistence = "Live"
    /\ \/ owner # kOwner
       \/ payload # kCreatePayload
    /\ kLastCreateOutcome' = "Conflict"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kRevision,
                   kHasInitialEvent, kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected, kEverTerminal>>

ReplayTombstonedCreate(owner, payload) ==
    /\ kExistence = "Tombstoned"
    /\ owner \in Owners
    /\ payload \in Payloads
    /\ kLastCreateOutcome' = "Tombstoned"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kRevision,
                   kHasInitialEvent, kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected, kEverTerminal>>

BeginRealization(driver) ==
    /\ driver \in Owners
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kRealizationState \in {"None", "Failed"}
    /\ kRealizationEpoch < MaxRealizationEpoch
    /\ kRealizationState' = "Leased"
    /\ kRealizationOwner' = driver
    /\ kRealizationEpoch' = kRealizationEpoch + 1
    /\ kExecution' = "Activating"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kPlacement, kRevision, kHasInitialEvent,
                   kRealizationEffects, kActivationCommitted,
                   kWorkProjected, kEverTerminal, kLastCreateOutcome>>

StageRealization(driver, epoch) ==
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kRealizationState = "Leased"
    /\ kRealizationOwner = driver
    /\ kRealizationEpoch = epoch
    /\ epoch \notin kRealizationEffects
    /\ kRealizationState' = "Staged"
    /\ kRealizationEffects' = kRealizationEffects \cup {epoch}
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kRevision,
                   kHasInitialEvent, kRealizationOwner, kRealizationEpoch,
                   kActivationCommitted, kWorkProjected, kEverTerminal,
                   kLastCreateOutcome>>

ActivateRealization(driver, epoch) ==
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kRealizationState = "Staged"
    /\ kRealizationOwner = driver
    /\ kRealizationEpoch = epoch
    /\ kRealizationState' = "Activated"
    /\ kActivationCommitted' = TRUE
    /\ kRevision < MaxRevision
    /\ kRevision' = kRevision + 1
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kHasInitialEvent,
                   kRealizationOwner, kRealizationEpoch,
                   kRealizationEffects, kWorkProjected, kEverTerminal,
                   kLastCreateOutcome>>

CompleteRealization(driver, epoch, hasActiveActivity) ==
    /\ hasActiveActivity \in BOOLEAN
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kRealizationState = "Activated"
    /\ kRealizationOwner = driver
    /\ kRealizationEpoch = epoch
    /\ kActivationCommitted
    /\ kRealizationState' = "Complete"
    /\ kExecution' = IF hasActiveActivity THEN "Running" ELSE "Idle"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kPlacement, kRevision, kHasInitialEvent,
                   kRealizationOwner, kRealizationEpoch,
                   kRealizationEffects, kActivationCommitted,
                   kWorkProjected, kEverTerminal, kLastCreateOutcome>>

\* Activity membership is owned by SessionActivityKernel. Its observer invokes
\* this root transition only when the final exact activity receipt is removed.
CloseRunningInterval ==
    /\ kExistence = "Live"
    /\ kExecution = "Running"
    /\ kExecution' = "Idle"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kPlacement, kRevision, kHasInitialEvent,
                   kRealizationState, kRealizationOwner, kRealizationEpoch,
                   kRealizationEffects, kActivationCommitted,
                   kWorkProjected, kEverTerminal, kLastCreateOutcome>>

RetryableRealizationFailure(driver, epoch) ==
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kRealizationState \in {"Leased", "Staged", "Activated", "Complete"}
    /\ kRealizationOwner = driver
    /\ kRealizationEpoch = epoch
    /\ kRealizationState' = "Failed"
    /\ kRealizationOwner' = NoOwner
    /\ kExecution' = "Preparing"
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kPlacement, kRevision, kHasInitialEvent,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected, kEverTerminal,
                   kLastCreateOutcome>>

PermanentRealizationFailure(driver, epoch) ==
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kRealizationState \in {"Leased", "Staged", "Activated"}
    /\ kRealizationOwner = driver
    /\ kRealizationEpoch = epoch
    /\ kRealizationState' = "Failed"
    /\ kRealizationOwner' = NoOwner
    /\ kExecution' = "ActivationFailed"
    /\ kEverTerminal' = TRUE
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kPlacement, kRevision, kHasInitialEvent,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected,
                   kLastCreateOutcome>>

ProjectWork ==
    /\ kExistence = "Live"
    /\ kPlacement = "Worker"
    /\ kExecution \notin TerminalExecutionStates
    /\ ~kWorkProjected
    /\ kWorkProjected' = TRUE
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kRevision,
                   kHasInitialEvent, kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kEverTerminal,
                   kLastCreateOutcome>>

LoseWorkProjection ==
    /\ kWorkProjected
    /\ kWorkProjected' = FALSE
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kExecution, kPlacement, kRevision,
                   kHasInitialEvent, kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kEverTerminal,
                   kLastCreateOutcome>>

Terminate ==
    /\ kExistence = "Live"
    /\ kExecution \notin TerminalExecutionStates
    /\ kExecution' = "Terminated"
    /\ kEverTerminal' = TRUE
    /\ UNCHANGED <<kExistence, kOwner, kCreatePayload, kCreateReceipt,
                   kBaseline, kPlacement, kRevision, kHasInitialEvent,
                   kRealizationState, kRealizationOwner,
                   kRealizationEpoch, kRealizationEffects,
                   kActivationCommitted, kWorkProjected,
                   kLastCreateOutcome>>

Tombstone ==
    /\ kExistence = "Live"
    /\ kExecution \in TerminalExecutionStates
    /\ ~kWorkProjected
    /\ kRevision < MaxRevision
    /\ kExistence' = "Tombstoned"
    /\ kBaseline' = "None"
    /\ kPlacement' = "None"
    /\ kRevision' = kRevision + 1
    /\ kRealizationState' = "None"
    /\ kRealizationOwner' = NoOwner
    /\ kLastCreateOutcome' = "None"
    /\ UNCHANGED <<kOwner, kCreatePayload, kCreateReceipt, kExecution,
                   kHasInitialEvent, kRealizationEpoch,
                   kRealizationEffects, kActivationCommitted,
                   kWorkProjected, kEverTerminal>>

Next ==
    \/ \E owner \in Owners, payload \in Payloads,
          placement \in {"Local", "Worker"}, hasInitialEvent \in BOOLEAN:
           Create(owner, payload, placement, hasInitialEvent)
    \/ \E owner \in Owners, payload \in Payloads:
           ExactCreateReplay(owner, payload)
    \/ \E owner \in Owners, payload \in Payloads:
           RejectConflictingCreate(owner, payload)
    \/ \E owner \in Owners, payload \in Payloads:
           ReplayTombstonedCreate(owner, payload)
    \/ \E driver \in Owners: BeginRealization(driver)
    \/ \E driver \in Owners, epoch \in 0..MaxRealizationEpoch:
           StageRealization(driver, epoch)
    \/ \E driver \in Owners, epoch \in 0..MaxRealizationEpoch:
           ActivateRealization(driver, epoch)
    \/ \E driver \in Owners, epoch \in 0..MaxRealizationEpoch,
          active \in BOOLEAN: CompleteRealization(driver, epoch, active)
    \/ CloseRunningInterval
    \/ \E driver \in Owners, epoch \in 0..MaxRealizationEpoch:
           RetryableRealizationFailure(driver, epoch)
    \/ \E driver \in Owners, epoch \in 0..MaxRealizationEpoch:
           PermanentRealizationFailure(driver, epoch)
    \/ ProjectWork
    \/ LoseWorkProjection
    \/ Terminate
    \/ Tombstone

Spec == Init /\ [][Next]_kVars

TypeOK ==
    /\ kExistence \in ExistenceStates
    /\ kOwner \in Owners \cup {NoOwner}
    /\ kCreatePayload \in Payloads \cup {NoPayload}
    /\ kCreateReceipt \in BOOLEAN
    /\ kBaseline \in BaselineStates
    /\ kExecution \in ExecutionStates
    /\ kPlacement \in Placements
    /\ kRevision \in 0..MaxRevision
    /\ kHasInitialEvent \in BOOLEAN
    /\ kRealizationState \in RealizationStates
    /\ kRealizationOwner \in Owners \cup {NoOwner}
    /\ kRealizationEpoch \in 0..MaxRealizationEpoch
    /\ kRealizationEffects \subseteq 1..MaxRealizationEpoch
    /\ kActivationCommitted \in BOOLEAN
    /\ kWorkProjected \in BOOLEAN
    /\ kEverTerminal \in BOOLEAN
    /\ kLastCreateOutcome \in CreateOutcomes

VisibleRootIsComplete ==
    kExistence = "Live" =>
        /\ kOwner \in Owners
        /\ kCreatePayload \in Payloads
        /\ kCreateReceipt
        /\ kBaseline = "Frozen"
        /\ kPlacement \in {"Local", "Worker"}
        /\ kRevision > 0

AbsentIdentityHasNoCreateFact ==
    kExistence = "Absent" =>
        /\ kOwner = NoOwner
        /\ kCreatePayload = NoPayload
        /\ ~kCreateReceipt
        /\ kBaseline = "None"
        /\ kRevision = 0

TombstoneNeverReplaysSuccess ==
    kExistence = "Tombstoned" =>
        /\ kBaseline = "None"
        /\ kPlacement = "None"
        /\ kExecution \in TerminalExecutionStates
        /\ kLastCreateOutcome # "Replayed"

RealizationLeaseIsExact ==
    (kRealizationState \in {"Leased", "Staged", "Activated", "Complete"}) =>
        /\ kRealizationOwner \in Owners
        /\ kRealizationEpoch > 0

RealizationEffectsAreFenced ==
    \A epoch \in kRealizationEffects:
        /\ epoch > 0
        /\ epoch <= kRealizationEpoch

ReadyRequiresCommittedRealization ==
    kExecution \in {"Idle", "Running"} =>
        /\ kRealizationState = "Complete"
        /\ kActivationCommitted

WorkProjectionIsDisposableAndScoped ==
    kWorkProjected =>
        /\ kExistence = "Live"
        /\ kPlacement = "Worker"

TerminalExecutionIsAbsorbing ==
    kEverTerminal => kExecution \in TerminalExecutionStates

Safety ==
    /\ TypeOK
    /\ VisibleRootIsComplete
    /\ AbsentIdentityHasNoCreateFact
    /\ TombstoneNeverReplaysSuccess
    /\ RealizationLeaseIsExact
    /\ RealizationEffectsAreFenced
    /\ ReadyRequiresCommittedRealization
    /\ WorkProjectionIsDisposableAndScoped
    /\ TerminalExecutionIsAbsorbing

=============================================================================
