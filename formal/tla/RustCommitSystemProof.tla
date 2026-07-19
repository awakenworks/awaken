----------------------- MODULE RustCommitSystemProof -----------------------
EXTENDS RustCommitSystem, TLAPS

LEMMA InitSafety == KernelAssumptions /\ Init => Safety(state)
BY DEF Init, InitialState, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA PersistBatchSafety ==
    KernelAssumptions /\ Safety(state) /\ PersistBatch(state, state') =>
      Safety(state')
BY Z3T(30) DEF PersistBatch, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA CommitNoopSafety ==
    KernelAssumptions /\ Safety(state) /\ CommitNoop(state, state') =>
      Safety(state')
BY Z3T(30) DEF CommitNoop, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA StartOrRetrySafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\ StartOrRetry(state, state', c) =>
        Safety(state')
BY Z3T(30) DEF StartOrRetry, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA AwaitCallSafety ==
    \A c \in Calls, kind \in TicketKinds \ {"None"}:
      KernelAssumptions /\ Safety(state) /\ AwaitCall(state, state', c, kind) =>
        Safety(state')
BY Z3T(30) DEF AwaitCall, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA ResumeExecutingSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\ ResumeExecuting(state, state', c) =>
        Safety(state')
BY Z3T(30) DEF ResumeExecuting, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA CompleteCallSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\ CompleteCall(state, state', c) =>
        Safety(state')
BY Z3T(30) DEF CompleteCall, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA CompleteAndFinalizeSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\
        CompleteAndFinalize(state, state', c) => Safety(state')
BY Z3T(30) DEF CompleteAndFinalize, Bump, StateValue, Safety, TypeOK,
   TicketCoherence, BatchCoherence, AttemptCoherence, DelegationCoherence,
   EndedIsSealed, RunStates, TicketKinds, CallStates, TerminalCallStates,
   BatchStates, LinkStates, KernelAssumptions

LEMMA CompleteImmediateSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\
        CompleteImmediate(state, state', c) => Safety(state')
BY Z3T(30) DEF CompleteImmediate, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA CompleteImmediateAndFinalizeSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\
        CompleteImmediateAndFinalize(state, state', c) =>
        Safety(state')
BY Z3T(30) DEF CompleteImmediateAndFinalize, Bump, StateValue, Safety, TypeOK,
   TicketCoherence, BatchCoherence, AttemptCoherence, DelegationCoherence,
   EndedIsSealed, RunStates, TicketKinds, CallStates, TerminalCallStates,
   BatchStates, LinkStates, KernelAssumptions

LEMMA MarkIndeterminateSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\ MarkIndeterminate(state, state', c) =>
        Safety(state')
BY Z3T(30) DEF MarkIndeterminate, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA FinalizeBatchSafety ==
    KernelAssumptions /\ Safety(state) /\ FinalizeBatch(state, state') =>
      Safety(state')
BY Z3T(30) DEF FinalizeBatch, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA EndRunSafety ==
    KernelAssumptions /\ Safety(state) /\ EndRun(state, state') => Safety(state')
BY Z3T(30) DEF EndRun, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

LEMMA NextSafety == KernelAssumptions /\ Safety(state) /\ Next => Safety(state')
BY PersistBatchSafety, CommitNoopSafety, StartOrRetrySafety, AwaitCallSafety,
   ResumeExecutingSafety, CompleteCallSafety, CompleteAndFinalizeSafety,
   CompleteImmediateSafety, CompleteImmediateAndFinalizeSafety,
   MarkIndeterminateSafety, FinalizeBatchSafety, EndRunSafety DEF Next, NextState

InductiveSafety == KernelAssumptions /\ Safety(state)

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextSafety DEF InductiveSafety, KernelAssumptions, vars

THEOREM RustCommitSafety ==
    KernelAssumptions /\ Spec => []Safety(state)
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=============================================================================
