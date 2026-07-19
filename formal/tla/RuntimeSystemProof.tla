-------------------------- MODULE RuntimeSystemProof --------------------------
EXTENDS RuntimeSystem, TLAPS

\* Proof obligations live outside the executable TLC model. This keeps one
\* state-machine definition as the source of truth while allowing TLAPS to
\* check unbounded arguments independently of finite model constants.
LEMMA InitCallStateType == Init => callState \in [Calls -> ToolCallStates]
BY DEF Init, ToolCallStates

LEMMA InitAttemptsType == RuntimeAssumptions /\ Init => attempts \in [Calls -> 0..MaxAttempts]
BY DEF Init, RuntimeAssumptions

LEMMA InitInvokedAttemptType ==
    RuntimeAssumptions /\ Init => invokedAttempt \in [Calls -> 0..MaxAttempts]
BY DEF Init, RuntimeAssumptions

LEMMA InitDecisionType == Init => decision \in [Calls -> ToolDecisionStates]
BY DEF Init, ToolDecisionStates

LEMMA InitEnumTypes == Init =>
    /\ runState \in CoreRunStates
    /\ ticketKind \in TicketKinds
    /\ dispatchState \in DispatchStates
    /\ childState \in DelegatedChildStates
    /\ linkStatus \in DelegationLinkStatuses
BY Z3T(30) DEF Init, CoreRunStates, TicketKinds, DispatchStates,
    DelegatedChildStates, DelegationLinkStatuses

LEMMA InitIdentityTypes == Init =>
    /\ ticketCall \in Calls \cup {NoCall}
    /\ owner \in Owners \cup {NoOwner}
BY Z3T(30) DEF Init

LEMMA InitNumericTypes == RuntimeAssumptions /\ Init =>
    /\ leaseEpoch \in 0..MaxEpoch
    /\ inboxCount \in 0..MaxInbox
    /\ commitVersion \in 0..MaxVersion
BY Z3T(30) DEF Init, RuntimeAssumptions

LEMMA InitBooleanTypes == Init =>
    /\ published \in BOOLEAN
    /\ endedOnce \in BOOLEAN
BY DEF Init

LEMMA InitTypeOK == RuntimeAssumptions /\ Init => TypeOK
BY InitCallStateType, InitAttemptsType, InitInvokedAttemptType,
   InitDecisionType, InitEnumTypes, InitIdentityTypes, InitNumericTypes,
   InitBooleanTypes DEF TypeOK

LEMMA InitRunDispatchCoherence == RuntimeAssumptions /\ Init => RunDispatchCoherence
BY Z3T(30) DEF Init, RunDispatchCoherence, RuntimeAssumptions

LEMMA InitTicketCoherence == Init => TicketCoherence
BY Z3T(30) DEF Init, TicketCoherence

LEMMA InitExecutionWasCommitted == Init => ExecutionWasCommitted
BY Z3T(30) DEF Init, ExecutionWasCommitted

LEMMA InitAttemptsAreCommitted == Init => AttemptsAreCommitted
BY Z3T(30) DEF Init, AttemptsAreCommitted

LEMMA InitApprovalCannotBeBypassed == Init => ApprovalCannotBeBypassed
BY Z3T(30) DEF Init, ApprovalCannotBeBypassed

LEMMA InitDeniedCallsNeverRun == Init => DeniedCallsNeverRun
BY Z3T(30) DEF Init, DeniedCallsNeverRun

LEMMA InitRequestedCallsAreFresh == Init => RequestedCallsAreFresh
BY Z3T(30) DEF Init, RequestedCallsAreFresh

LEMMA InitDeniedCallsAreTerminal == Init => DeniedCallsAreTerminal
BY Z3T(30) DEF Init, DeniedCallsAreTerminal

LEMMA InitPublicationBarrier == Init => PublicationBarrier
BY Z3T(30) DEF Init, PublicationBarrier

LEMMA InitDelegationCompletionIsToolOwned == Init => DelegationCompletionIsToolOwned
BY Z3T(30) DEF Init, DelegationCompletionIsToolOwned

LEMMA InitOpenDelegationHasLiveChild == Init => OpenDelegationHasLiveChild
BY Z3T(30) DEF Init, OpenDelegationHasLiveChild

LEMMA InitCancellationIsDurable == Init => CancellationIsDurable
BY Z3T(30) DEF Init, CancellationIsDurable

LEMMA InitMessagesCannotApprove == Init => MessagesCannotApprove
BY Z3T(30) DEF Init, MessagesCannotApprove

LEMMA InitEndedIsAbsorbing == Init => EndedIsAbsorbing
BY Z3T(30) DEF Init, EndedIsAbsorbing

THEOREM InitEstablishesSafety == RuntimeAssumptions /\ Init => Safety
BY InitTypeOK, InitRunDispatchCoherence, InitTicketCoherence,
   InitExecutionWasCommitted, InitAttemptsAreCommitted,
   InitApprovalCannotBeBypassed,
   InitDeniedCallsNeverRun, InitRequestedCallsAreFresh,
   InitDeniedCallsAreTerminal, InitPublicationBarrier,
   InitDelegationCompletionIsToolOwned, InitOpenDelegationHasLiveChild,
   InitCancellationIsDurable,
   InitMessagesCannotApprove, InitEndedIsAbsorbing DEF Safety

LEMMA ClaimPreservesSafety ==
    \A candidate \in Owners:
        RuntimeAssumptions /\ Safety /\ Claim(candidate) => Safety'
BY Z3T(30) DEF Safety, TypeOK, RunDispatchCoherence, TicketCoherence,
    ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed, DeniedCallsNeverRun,
    RequestedCallsAreFresh, DeniedCallsAreTerminal, PublicationBarrier,
    DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Claim, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

\* Expanding the whole Next disjunction creates one enormous SMT obligation.
\* Proving each semantic action separately is faster and, more importantly,
\* makes a failed invariant attributable to one production transition.
ActionSafetyDefs ==
    /\ Safety
    /\ TypeOK
    /\ RunDispatchCoherence
    /\ TicketCoherence
    /\ ExecutionWasCommitted
    /\ AttemptsAreCommitted
    /\ ApprovalCannotBeBypassed
    /\ DeniedCallsNeverRun
    /\ RequestedCallsAreFresh
    /\ DeniedCallsAreTerminal
    /\ PublicationBarrier
    /\ DelegationCompletionIsToolOwned
    /\ OpenDelegationHasLiveChild
    /\ CancellationIsDurable
    /\ MessagesCannotApprove
    /\ EndedIsAbsorbing

LEMMA ReclaimPreservesSafety ==
    \A candidate \in Owners:
        RuntimeAssumptions /\ Safety /\ Reclaim(candidate) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Reclaim, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA RequestApprovalPreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ RequestApproval(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    RequestApproval, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA ApprovePreservesSafety ==
    \A c \in Calls, candidate \in Owners:
        RuntimeAssumptions /\ Safety /\ Approve(c, candidate) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Approve, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA DenyPreservesSafety ==
    \A c \in Calls, candidate \in Owners:
        RuntimeAssumptions /\ Safety /\ Deny(c, candidate) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Deny, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA SupplyResultPreservesSafety ==
    \A c \in Calls, candidate \in Owners:
        RuntimeAssumptions /\ Safety /\ SupplyResult(c, candidate) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted,
    ApprovalCannotBeBypassed, DeniedCallsNeverRun, RequestedCallsAreFresh,
    DeniedCallsAreTerminal, PublicationBarrier,
    DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    SupplyResult, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA DirectStartPreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ DirectStart(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    DirectStart, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA CompleteImmediatePreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ CompleteImmediate(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted,
    ApprovalCannotBeBypassed, DeniedCallsNeverRun, RequestedCallsAreFresh,
    DeniedCallsAreTerminal, PublicationBarrier,
    DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    CompleteImmediate, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA InvokePreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ Invoke(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Invoke, CoreRunStates, TicketKinds, DispatchStates, ToolCallStates,
    TerminalToolCallStates, ToolDecisionStates, DelegatedChildStates,
    DelegationLinkStatuses, RuntimeAssumptions

LEMMA CompletePreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ Complete(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Complete, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA RecoverReplaySafePreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ RecoverReplaySafe(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    RecoverReplaySafe, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA RecoverIndeterminatePreservesSafety ==
    \A c \in Calls:
        RuntimeAssumptions /\ Safety /\ RecoverIndeterminate(c) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    RecoverIndeterminate, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA StartChildPreservesSafety ==
    RuntimeAssumptions /\ Safety /\ StartChild => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    StartChild, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA AwaitChildPreservesSafety ==
    RuntimeAssumptions /\ Safety /\ AwaitChild => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    AwaitChild, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA ResumeChildPreservesSafety ==
    RuntimeAssumptions /\ Safety /\ ResumeChild => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    ResumeChild, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA FinishChildPreservesSafety ==
    \A candidate \in Owners:
        RuntimeAssumptions /\ Safety /\ FinishChild(candidate) => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    FinishChild, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA QueueMessagePreservesSafety ==
    RuntimeAssumptions /\ Safety /\ QueueMessage => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    QueueMessage, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA ConsumeMessagePreservesSafety ==
    RuntimeAssumptions /\ Safety /\ ConsumeMessage => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    ConsumeMessage, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA FinalizeBatchPreservesSafety ==
    RuntimeAssumptions /\ Safety /\ FinalizeBatch => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    FinalizeBatch, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA EndRunPreservesSafety ==
    RuntimeAssumptions /\ Safety /\ EndRun => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    EndRun, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA CancelPreservesSafety ==
    RuntimeAssumptions /\ Safety /\ Cancel => Safety'
BY Z3T(30) DEF ActionSafetyDefs, Safety, TypeOK, RunDispatchCoherence,
    TicketCoherence, ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed,
    DeniedCallsNeverRun, RequestedCallsAreFresh, DeniedCallsAreTerminal,
    PublicationBarrier, DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing,
    Cancel, Committed, CoreRunStates, TicketKinds, DispatchStates,
    ToolCallStates, TerminalToolCallStates, ToolDecisionStates,
    DelegatedChildStates, DelegationLinkStatuses, RuntimeAssumptions

LEMMA UnchangedPreservesSafety == Safety /\ UNCHANGED vars => Safety'
BY DEF Safety, TypeOK, RunDispatchCoherence, TicketCoherence,
    ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed, DeniedCallsNeverRun,
    RequestedCallsAreFresh, DeniedCallsAreTerminal, PublicationBarrier,
    DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing, vars

LEMMA NextPreservesSafety == RuntimeAssumptions /\ Safety /\ Next => Safety'
BY ClaimPreservesSafety, ReclaimPreservesSafety,
   RequestApprovalPreservesSafety, ApprovePreservesSafety,
   DenyPreservesSafety, SupplyResultPreservesSafety,
   DirectStartPreservesSafety, CompleteImmediatePreservesSafety,
   InvokePreservesSafety,
   CompletePreservesSafety, RecoverReplaySafePreservesSafety,
   RecoverIndeterminatePreservesSafety, StartChildPreservesSafety,
   AwaitChildPreservesSafety, ResumeChildPreservesSafety,
   FinishChildPreservesSafety, QueueMessagePreservesSafety,
   ConsumeMessagePreservesSafety, FinalizeBatchPreservesSafety,
   EndRunPreservesSafety, CancelPreservesSafety,
   UnchangedPreservesSafety
   DEF Next, DuplicateChildCompletion, StaleSettle

LEMMA StutterPreservesSafety == Safety /\ UNCHANGED vars => Safety'
BY DEF Safety, TypeOK, RunDispatchCoherence, TicketCoherence,
    ExecutionWasCommitted, AttemptsAreCommitted, ApprovalCannotBeBypassed, DeniedCallsNeverRun,
    RequestedCallsAreFresh, DeniedCallsAreTerminal, PublicationBarrier,
    DelegationCompletionIsToolOwned, OpenDelegationHasLiveChild,
    CancellationIsDurable, MessagesCannotApprove, EndedIsAbsorbing, vars

InductiveSafety == RuntimeAssumptions /\ Safety

LEMMA InitEstablishesInductiveSafety ==
    RuntimeAssumptions /\ Init => InductiveSafety
BY InitEstablishesSafety DEF InductiveSafety

LEMMA StepPreservesInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextPreservesSafety, StutterPreservesSafety
   DEF InductiveSafety, RuntimeAssumptions, vars

THEOREM InductiveSafetyInvariant ==
    RuntimeAssumptions /\ Spec => []InductiveSafety
BY InitEstablishesInductiveSafety, StepPreservesInductiveSafety, PTL DEF Spec

THEOREM SafetyInvariant == RuntimeAssumptions /\ Spec => []Safety
BY InductiveSafetyInvariant, PTL DEF InductiveSafety

=============================================================================
