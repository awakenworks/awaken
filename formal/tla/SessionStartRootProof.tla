-------------------- MODULE SessionStartRootProof --------------------
EXTENDS SessionStartProtocol, TLAPS

\* The composition retains its sole Root/Event/Work state owners. This proof
\* only instantiates the existing root theorem under that exact variable map.
RootFacts == INSTANCE SessionRootProof WITH
    Owners <- Actors,
    NoOwner <- NoActor,
    Payloads <- Payloads,
    NoPayload <- NoActor,
    MaxRevision <- MaxRevision,
    MaxRealizationEpoch <- MaxEpoch,
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

\* The two instances are definitionally the same canonical kernel, including
\* every state axis and action. This checked bridge prevents a wrong renaming
\* from borrowing a theorem about another root.
LEMMA RootProofInstanceMatches ==
    /\ RootFacts!KernelAssumptions = Root!KernelAssumptions
    /\ RootFacts!Init = Root!Init
    /\ RootFacts!Safety = Root!Safety
    /\ RootFacts!Next = Root!Next
BY DEF Root!ExistenceStates, Root!BaselineStates, Root!ExecutionStates,
   Root!TerminalExecutionStates, Root!Placements, Root!RealizationStates,
   Root!CreateOutcomes, Root!KernelAssumptions, Root!kVars,
   Root!Init, Root!Create, Root!ExactCreateReplay,
   Root!RejectConflictingCreate, Root!ReplayTombstonedCreate, Root!BeginRealization,
   Root!StageRealization, Root!ActivateRealization, Root!CompleteRealization,
   Root!CloseRunningInterval, Root!RetryableRealizationFailure, Root!PermanentRealizationFailure,
   Root!ProjectWork, Root!LoseWorkProjection, Root!Terminate,
   Root!Tombstone, Root!Next, Root!Spec,
   Root!TypeOK, Root!VisibleRootIsComplete, Root!AbsentIdentityHasNoCreateFact,
   Root!TombstoneNeverReplaysSuccess, Root!RealizationLeaseIsExact, Root!RealizationEffectsAreFenced,
   Root!ReadyRequiresCommittedRealization, Root!WorkProjectionIsDisposableAndScoped, Root!TerminalExecutionIsAbsorbing,
   Root!Safety, RootFacts!ExistenceStates, RootFacts!BaselineStates,
   RootFacts!ExecutionStates, RootFacts!TerminalExecutionStates, RootFacts!Placements,
   RootFacts!RealizationStates, RootFacts!CreateOutcomes, RootFacts!KernelAssumptions,
   RootFacts!kVars, RootFacts!Init, RootFacts!Create,
   RootFacts!ExactCreateReplay, RootFacts!RejectConflictingCreate, RootFacts!ReplayTombstonedCreate,
   RootFacts!BeginRealization, RootFacts!StageRealization, RootFacts!ActivateRealization,
   RootFacts!CompleteRealization, RootFacts!CloseRunningInterval, RootFacts!RetryableRealizationFailure,
   RootFacts!PermanentRealizationFailure, RootFacts!ProjectWork, RootFacts!LoseWorkProjection,
   RootFacts!Terminate, RootFacts!Tombstone, RootFacts!Next,
   RootFacts!Spec, RootFacts!TypeOK, RootFacts!VisibleRootIsComplete,
   RootFacts!AbsentIdentityHasNoCreateFact, RootFacts!TombstoneNeverReplaysSuccess, RootFacts!RealizationLeaseIsExact,
   RootFacts!RealizationEffectsAreFenced, RootFacts!ReadyRequiresCommittedRealization, RootFacts!WorkProjectionIsDisposableAndScoped,
   RootFacts!TerminalExecutionIsAbsorbing, RootFacts!Safety

StartAssumptions == Root!KernelAssumptions

\* Cause/effect proof design: root-writing actions refine the canonical Next;
\* every Event/Work-only action and crash stutters the complete root tuple.
LEMMA StartStepRefinesRoot ==
    StartAssumptions /\ Root!Safety /\ [Next]_vars => [Root!Next]_rootVars
BY Z3T(60) DEF StartAssumptions, Next, OtherNext,
   RootOtherNext, WorkOtherNext, EventOtherNext, Root!Next,
   Root!Safety, Root!TypeOK, Root!RealizationLeaseIsExact,
   CreateWithInitialEvent, ReplayCreate, ProjectWork, LoseQueuedWorkProjection,
   ClaimWork, ReclaimWork, HeartbeatWork, AdvanceStoreTime, BeginRealization,
   StageRealization, ActivateRealization, CompleteRealization, FailRealization,
   FenceStaleRealization, ReserveRun, RegisterRunActivity, ActivateRun, ClaimRun,
   ObserveCommittedInput, AnchorCommittedInput, ObserveCommittedRun,
   SettleRunActivity, SettleDispatch, MarkEventProcessed, SettleEventAndClose,
   ReleaseWork, TerminateSession, Actors, Payloads, rootVars, vars

LEMMA StartInitHasRoot == Init => Root!Init
BY DEF Init

StartRootInductive == StartAssumptions /\ RootFacts!Safety

LEMMA StartRootInit == StartAssumptions /\ Init => StartRootInductive
BY RootFacts!InitSafety, StartInitHasRoot, RootProofInstanceMatches
   DEF StartAssumptions, StartRootInductive, RootFacts!InitSafety

LEMMA StartRootStep ==
    StartRootInductive /\ [Next]_vars => StartRootInductive'
BY Z3T(30), RootFacts!StepInductiveSafety, StartStepRefinesRoot,
   RootProofInstanceMatches
   DEF StartAssumptions, StartRootInductive, RootFacts!InductiveSafety,
       RootFacts!StepInductiveSafety,
       RootFacts!kVars, rootVars, Root!KernelAssumptions

THEOREM StartPreservesCanonicalRoot ==
    StartAssumptions /\ Spec => []RootFacts!Safety
BY StartRootInit, StartRootStep, PTL DEF Spec, StartRootInductive

\* Acceptance and disposable projections are observations of that same root.
\* No tombstone action is in this start composition. Terminal failure retains
\* the live queryable aggregate; it must not be confused with deletion.
StartAuthority ==
    /\ existence \in {"Absent", "Live"}
    /\ acceptedResponse = (existence = "Live")
    /\ workRowPresent = workProjected
    /\ (dispatchExists => existence = "Live")
    /\ (execution = "ActivationFailed" => everTerminal)

LEMMA StartAuthorityInit == Init => StartAuthority
BY DEF Init, Root!Init, Event!Init, StartAuthority

\* Causes: root publication, realization, Event settlement and termination
\* change the same canonical root tuple while the accepted response stutters.
\* Effect: each preserves the acceptance, Work and terminal observations.
LEMMA RootOtherPreservesAuthority ==
    StartRootInductive /\ StartAuthority /\
    UNCHANGED acceptedResponse /\ RootOtherNext => StartAuthority'
BY Z3T(60), RootProofInstanceMatches
   DEF StartRootInductive, StartAssumptions, RootFacts!Safety,
   Root!Safety, Root!TypeOK, Root!VisibleRootIsComplete,
   Root!RealizationLeaseIsExact, Root!WorkProjectionIsDisposableAndScoped,
   Root!TerminalExecutionIsAbsorbing,
   StartAuthority, RootOtherNext, rootVars, eventVars, workVars,
   ProjectWork, LoseQueuedWorkProjection, BeginRealization,
   StageRealization, ActivateRealization, CompleteRealization,
   FailRealization, FenceStaleRealization, SettleEventAndClose,
   TerminateSession, SessionExecutable, CurrentWork,
   CurrentRealizationWork,
   Root!ProjectWork, Root!LoseWorkProjection, Root!BeginRealization,
   Root!StageRealization, Root!ActivateRealization,
   Root!CompleteRealization, Root!PermanentRealizationFailure,
   Root!RetryableRealizationFailure, Root!CloseRunningInterval,
   Root!Terminate, Event!SettleInitialEventWake

\* Causes: Work claim, reclaim, heartbeat, clock and release change only the
\* canonical Work tuple. Effect: accepted/root/Event observations stutter.
LEMMA WorkOtherPreservesAuthority ==
    StartAuthority /\ UNCHANGED acceptedResponse /\
    WorkOtherNext => StartAuthority'
BY Z3T(60) DEF StartAuthority, WorkOtherNext,
   ClaimWork, ReclaimWork, HeartbeatWork, AdvanceStoreTime,
   ReleaseWork, rootVars, eventVars, workVars

\* Causes: Run reservation, input observation and Event settlement mutate
\* the Event owner while the canonical root and Work projection stutter.
\* Effect: any newly visible Dispatch still belongs to the live Session.
LEMMA EventOtherPreservesAuthority ==
    StartRootInductive /\ StartAuthority /\
    UNCHANGED acceptedResponse /\ EventOtherNext => StartAuthority'
BY Z3T(60), RootProofInstanceMatches
   DEF StartRootInductive, StartAssumptions, RootFacts!Safety,
   Root!Safety, Root!TypeOK, StartAuthority, EventOtherNext,
   ReserveRun, RegisterRunActivity, ActivateRun, ClaimRun,
   ObserveCommittedInput, AnchorCommittedInput, ObserveCommittedRun,
   SettleRunActivity, SettleDispatch, MarkEventProcessed,
   SessionExecutable, CurrentWork, CurrentRealizationWork,
   Event!ReserveRun, Event!RegisterRunActivity, Event!ActivateRun,
   Event!ClaimRun, Event!ObserveCommittedInput,
   Event!AnchorCommittedInput, Event!ObserveCommittedRun,
   Event!SettleRunActivity, Event!SettleCommittedRun,
   Event!MarkProcessed, rootVars, workVars

\* Cause: a reachable Start state has both the canonical Root invariant and
\* the observable acceptance/work equations. Effect: every composed action
\* preserves those equations. The Root invariant comes from the existing
\* StartRootInductive; the three branch proofs reuse canonical actions.
LEMMA StartAuthorityStep ==
    StartRootInductive /\ StartAuthority /\ [Next]_vars => StartAuthority'
BY Z3T(60), RootOtherPreservesAuthority, WorkOtherPreservesAuthority,
   EventOtherPreservesAuthority
   DEF StartAuthority, Next, OtherNext, vars, rootVars, eventVars,
   CreateWithInitialEvent, ReplayCreate, Root!Create,
   Root!ExactCreateReplay, Event!AcceptInitialEvent

THEOREM StartObservationsRetainOneAuthority ==
    StartAssumptions /\ Spec => []StartAuthority
BY StartAuthorityInit, StartAuthorityStep, StartRootInit, StartRootStep,
   PTL DEF Spec, StartRootInductive

StartRootGuarantees ==
    /\ AcceptedResponseIsDurableRoot
    /\ AcceptedFailureRemainsQueryable
    /\ CompleteRootPrecedesEveryProjection
    /\ WorkProjectionHasOneAuthority

LEMMA RootAndAuthorityImplyGuarantees ==
    RootFacts!Safety /\ StartAuthority => StartRootGuarantees
BY Z3T(30), RootProofInstanceMatches DEF Root!Safety, Root!VisibleRootIsComplete,
   Root!WorkProjectionIsDisposableAndScoped, Root!TerminalExecutionIsAbsorbing,
   Root!TerminalExecutionStates, StartAuthority, StartRootGuarantees,
   AcceptedResponseIsDurableRoot, AcceptedFailureRemainsQueryable,
   CompleteRootPrecedesEveryProjection, WorkProjectionHasOneAuthority

THEOREM SessionStartRootGuarantees ==
    StartAssumptions /\ Spec => []StartRootGuarantees
BY StartPreservesCanonicalRoot, StartObservationsRetainOneAuthority,
   RootAndAuthorityImplyGuarantees, PTL
=============================================================================
