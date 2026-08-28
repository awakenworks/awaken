-------------------------- MODULE SessionRootProof --------------------------
EXTENDS SessionRootKernel, TLAPS

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY Z3T(60) DEF Init, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

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

LEMMA ReplaySafety ==
    \A owner \in Owners, payload \in Payloads:
      KernelAssumptions /\ Safety /\
      (ExactCreateReplay(owner, payload) \/
       RejectConflictingCreate(owner, payload) \/
       ReplayTombstonedCreate(owner, payload)) => Safety'
BY Z3T(60) DEF ExactCreateReplay, RejectConflictingCreate,
   ReplayTombstonedCreate, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

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

LEMMA ProjectionAndLifecycleSafety ==
    KernelAssumptions /\ Safety /\
      (CloseRunningInterval \/ ProjectWork \/ LoseWorkProjection \/
       Terminate \/ Tombstone) => Safety'
BY Z3T(60) DEF CloseRunningInterval, ProjectWork, LoseWorkProjection,
   Terminate, Tombstone, Safety, TypeOK, VisibleRootIsComplete,
   AbsentIdentityHasNoCreateFact, TombstoneNeverReplaysSuccess,
   RealizationLeaseIsExact, RealizationEffectsAreFenced,
   ReadyRequiresCommittedRealization, WorkProjectionIsDisposableAndScoped,
   TerminalExecutionIsAbsorbing, ExistenceStates, BaselineStates,
   ExecutionStates, TerminalExecutionStates, Placements, RealizationStates,
   CreateOutcomes, KernelAssumptions

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
