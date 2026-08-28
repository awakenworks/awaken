-------------------------- MODULE SessionRootProof --------------------------
EXTENDS SessionRootKernel, TLAPS

\* Proof design mirrors the aggregate's causal graph: initialization,
\* idempotent create outcomes, realization, projection, and lifecycle actions
\* are independent proof partitions.  Keep them separate so a new state axis
\* produces one local obligation instead of a timeout-prone disjunction.
LEMMA InitIdentityTypes ==
    KernelAssumptions /\ Init =>
      /\ kExistence \in ExistenceStates
      /\ kOwner \in Owners \cup {NoOwner}
      /\ kCreatePayload \in Payloads \cup {NoPayload}
      /\ kCreateReceipt \in BOOLEAN
BY DEF Init, ExistenceStates, KernelAssumptions

LEMMA InitLifecycleValueTypes ==
    Init =>
      /\ kBaseline \in BaselineStates
      /\ kExecution \in ExecutionStates
      /\ kPlacement \in Placements
      /\ kHasInitialEvent \in BOOLEAN
BY DEF Init, BaselineStates, ExecutionStates, Placements

LEMMA InitRevisionType ==
    KernelAssumptions /\ Init => kRevision \in 0..MaxRevision
BY Z3T(10) DEF Init, KernelAssumptions

LEMMA InitLifecycleTypes ==
    KernelAssumptions /\ Init =>
      /\ kBaseline \in BaselineStates
      /\ kExecution \in ExecutionStates
      /\ kPlacement \in Placements
      /\ kRevision \in 0..MaxRevision
      /\ kHasInitialEvent \in BOOLEAN
BY InitLifecycleValueTypes, InitRevisionType

LEMMA InitRealizationValueTypes ==
    Init =>
      /\ kRealizationState \in RealizationStates
      /\ kRealizationOwner \in Owners \cup {NoOwner}
      /\ kRealizationEffects \subseteq 1..MaxRealizationEpoch
      /\ kActivationCommitted \in BOOLEAN
BY DEF Init, RealizationStates

LEMMA InitRealizationEpochType ==
    KernelAssumptions /\ Init =>
      kRealizationEpoch \in 0..MaxRealizationEpoch
BY Z3T(10) DEF Init, KernelAssumptions

LEMMA InitRealizationTypes ==
    KernelAssumptions /\ Init =>
      /\ kRealizationState \in RealizationStates
      /\ kRealizationOwner \in Owners \cup {NoOwner}
      /\ kRealizationEpoch \in 0..MaxRealizationEpoch
      /\ kRealizationEffects \subseteq 1..MaxRealizationEpoch
      /\ kActivationCommitted \in BOOLEAN
BY InitRealizationValueTypes, InitRealizationEpochType

LEMMA InitProjectionTypes ==
    KernelAssumptions /\ Init =>
      /\ kWorkProjected \in BOOLEAN
      /\ kEverTerminal \in BOOLEAN
      /\ kLastCreateOutcome \in CreateOutcomes
BY DEF Init, CreateOutcomes, KernelAssumptions

LEMMA InitTypeOK == KernelAssumptions /\ Init => TypeOK
BY InitIdentityTypes, InitLifecycleTypes, InitRealizationTypes,
   InitProjectionTypes DEF TypeOK

LEMMA InitStructuralSafety ==
    KernelAssumptions /\ Init =>
      /\ VisibleRootIsComplete
      /\ AbsentIdentityHasNoCreateFact
      /\ TombstoneNeverReplaysSuccess
      /\ RealizationLeaseIsExact
      /\ RealizationEffectsAreFenced
      /\ ReadyRequiresCommittedRealization
      /\ WorkProjectionIsDisposableAndScoped
      /\ TerminalExecutionIsAbsorbing
BY Z3T(30) DEF Init, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, TerminalExecutionStates, KernelAssumptions

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY InitTypeOK, InitStructuralSafety DEF Safety

LEMMA CreateSafety ==
    \A owner \in Owners, payload \in Payloads,
       placement \in {"Local", "Worker"}, initial \in BOOLEAN:
      KernelAssumptions /\ Safety /\
      Create(owner, payload, placement, initial) => Safety'
BY Z3T(60) DEF Create, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA ExactCreateReplaySafety ==
    \A owner \in Owners, payload \in Payloads:
      KernelAssumptions /\ Safety /\ ExactCreateReplay(owner, payload) => Safety'
BY Z3T(30) DEF ExactCreateReplay, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA RejectConflictingCreateSafety ==
    \A owner \in Owners, payload \in Payloads:
      KernelAssumptions /\ Safety /\
      RejectConflictingCreate(owner, payload) => Safety'
BY Z3T(30) DEF RejectConflictingCreate, Safety, TypeOK,
   VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA ReplayTombstonedCreateSafety ==
    \A owner \in Owners, payload \in Payloads:
      KernelAssumptions /\ Safety /\
      ReplayTombstonedCreate(owner, payload) => Safety'
BY Z3T(30) DEF ReplayTombstonedCreate, Safety, TypeOK,
   VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA ReplaySafety ==
    \A owner \in Owners, payload \in Payloads:
      KernelAssumptions /\ Safety /\
      (ExactCreateReplay(owner, payload) \/
       RejectConflictingCreate(owner, payload) \/
       ReplayTombstonedCreate(owner, payload)) => Safety'
BY ExactCreateReplaySafety, RejectConflictingCreateSafety,
   ReplayTombstonedCreateSafety

LEMMA BeginRealizationSafety ==
    \A driver \in Owners:
      KernelAssumptions /\ Safety /\ BeginRealization(driver) => Safety'
BY Z3T(60) DEF BeginRealization, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA StageRealizationSafety ==
    \A driver \in Owners, epoch \in 0..MaxRealizationEpoch:
      KernelAssumptions /\ Safety /\ StageRealization(driver, epoch) => Safety'
BY Z3T(60) DEF StageRealization,
   Safety, TypeOK, VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA ActivateRealizationSafety ==
    \A driver \in Owners, epoch \in 0..MaxRealizationEpoch:
      KernelAssumptions /\ Safety /\ ActivateRealization(driver, epoch) => Safety'
BY Z3T(60) DEF ActivateRealization,
   Safety, TypeOK, VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA RetryableFailureSafety ==
    \A driver \in Owners, epoch \in 0..MaxRealizationEpoch:
      KernelAssumptions /\ Safety /\
      RetryableRealizationFailure(driver, epoch) => Safety'
BY Z3T(60) DEF RetryableRealizationFailure,
   Safety, TypeOK, VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA PermanentFailureSafety ==
    \A driver \in Owners, epoch \in 0..MaxRealizationEpoch:
      KernelAssumptions /\ Safety /\
      PermanentRealizationFailure(driver, epoch) => Safety'
BY Z3T(60) DEF PermanentRealizationFailure,
   Safety, TypeOK, VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA CompleteRealizationSafety ==
    \A driver \in Owners, epoch \in 0..MaxRealizationEpoch,
       active \in BOOLEAN:
      KernelAssumptions /\ Safety /\
      CompleteRealization(driver, epoch, active) => Safety'
BY Z3T(60) DEF CompleteRealization, Safety, TypeOK,
   VisibleRootIsComplete, AbsentIdentityHasNoCreateFact,
   TombstoneNeverReplaysSuccess, RealizationLeaseIsExact,
   RealizationEffectsAreFenced, ReadyRequiresCommittedRealization,
   WorkProjectionIsDisposableAndScoped, TerminalExecutionIsAbsorbing,
   ExistenceStates, BaselineStates, ExecutionStates, TerminalExecutionStates,
   Placements, RealizationStates, CreateOutcomes, KernelAssumptions

LEMMA CloseRunningIntervalSafety ==
    KernelAssumptions /\ Safety /\ CloseRunningInterval => Safety'
BY Z3T(30) DEF CloseRunningInterval, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA ProjectWorkSafety ==
    KernelAssumptions /\ Safety /\ ProjectWork => Safety'
BY Z3T(30) DEF ProjectWork, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA LoseWorkProjectionTypeOK ==
    TypeOK /\ LoseWorkProjection => TypeOK'
BY DEF LoseWorkProjection, TypeOK

LEMMA LoseWorkProjectionClearsScope ==
    LoseWorkProjection => WorkProjectionIsDisposableAndScoped'
BY Z3T(15) DEF LoseWorkProjection, WorkProjectionIsDisposableAndScoped

LEMMA LoseWorkProjectionPreservesIndependentSafety ==
    VisibleRootIsComplete /\
    AbsentIdentityHasNoCreateFact /\
    TombstoneNeverReplaysSuccess /\
    RealizationLeaseIsExact /\
    RealizationEffectsAreFenced /\
    ReadyRequiresCommittedRealization /\
    TerminalExecutionIsAbsorbing /\
    LoseWorkProjection =>
      /\ VisibleRootIsComplete'
      /\ AbsentIdentityHasNoCreateFact'
      /\ TombstoneNeverReplaysSuccess'
      /\ RealizationLeaseIsExact'
      /\ RealizationEffectsAreFenced'
      /\ ReadyRequiresCommittedRealization'
      /\ TerminalExecutionIsAbsorbing'
BY Z3T(15) DEF LoseWorkProjection, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, TerminalExecutionIsAbsorbing

LEMMA LoseWorkProjectionSafety ==
    KernelAssumptions /\ Safety /\ LoseWorkProjection => Safety'
BY LoseWorkProjectionTypeOK, LoseWorkProjectionClearsScope,
   LoseWorkProjectionPreservesIndependentSafety DEF Safety

LEMMA TerminateSafety ==
    KernelAssumptions /\ Safety /\ Terminate => Safety'
BY Z3T(30) DEF Terminate, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA TombstoneSafety ==
    KernelAssumptions /\ Safety /\ Tombstone => Safety'
BY Z3T(30) DEF Tombstone, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

LEMMA ProjectionAndLifecycleSafety ==
    KernelAssumptions /\ Safety /\
      (CloseRunningInterval \/ ProjectWork \/ LoseWorkProjection \/
       Terminate \/ Tombstone) => Safety'
BY CloseRunningIntervalSafety, ProjectWorkSafety, LoseWorkProjectionSafety,
   TerminateSafety, TombstoneSafety

LEMMA NextSafety == KernelAssumptions /\ Safety /\ Next => Safety'
BY CreateSafety, ReplaySafety, BeginRealizationSafety, StageRealizationSafety,
   ActivateRealizationSafety, RetryableFailureSafety, PermanentFailureSafety,
   CompleteRealizationSafety, ProjectionAndLifecycleSafety DEF Next

LEMMA SafetyStutter == Safety /\ UNCHANGED kVars => Safety'
BY Z3T(60) DEF Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, kVars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_kVars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, kVars

THEOREM SessionRootSafety == KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=============================================================================
