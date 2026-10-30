------------------- MODULE ThreadCommitProjectionProof --------------------
EXTENDS ThreadCommitProjection, TLAPS

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

LEMMA StartParallelCallsSafety ==
    \A wave \in SUBSET Calls:
      KernelAssumptions /\ Safety(state) /\
        StartParallelCalls(state, state', wave) => Safety(state')
BY Z3T(30) DEF StartParallelCalls, Bump, StateValue, Safety, TypeOK,
   TicketCoherence, BatchCoherence, AttemptCoherence, DelegationCoherence,
   EndedIsSealed, RunStates, TicketKinds, CallStates, TerminalCallStates,
   BatchStates, LinkStates, KernelAssumptions

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

\* Return decision table: R1 an unlinked immediate answer keeps its link absent;
\* R2 a fresh Agent request staged before preparation refusal opens its link in
\* the raw checkpoint, retaining attempt zero; R3 entered/waiting returns keep
\* their existing links and attempts. R4 returning the ticketed call clears that
\* ticket; every other call, link and attempt remains unchanged. An Open link
\* records a relationship, not evidence that physical execution began.
LEMMA RecordReturnsSafety ==
    \A wave \in SUBSET Calls:
      KernelAssumptions /\ Safety(state) /\
        RecordReturns(state, state', wave) => Safety(state')
BY Z3T(30) DEF RecordReturns, Bump, StateValue, Safety, TypeOK,
   TicketCoherence, BatchCoherence, AttemptCoherence, DelegationCoherence,
   EndedIsSealed, RunStates, TicketKinds, CallStates, TerminalCallStates,
   BatchStates, LinkStates, KernelAssumptions

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

\* Immediate-fold decision table: C1 Requested or Returned with zero attempts;
\* C2 the call has no relationship or its staged relationship is Open. D1 the
\* unlinked answer completes without adding a link; D2 the same completion fold
\* closes Open to Completed, exactly as CompleteCall does. No executor entry
\* or positive-attempt assumption is added to the legitimate refusal path.
LEMMA CompleteImmediateSafety ==
    \A c \in Calls:
      KernelAssumptions /\ Safety(state) /\
        CompleteImmediate(state, state', c) => Safety(state')
BY Z3T(30) DEF CompleteImmediate, Bump, StateValue, Safety, TypeOK, TicketCoherence,
   BatchCoherence, AttemptCoherence, DelegationCoherence, EndedIsSealed,
   RunStates, TicketKinds, CallStates, TerminalCallStates, BatchStates,
   LinkStates, KernelAssumptions

\* C3 all other calls are terminal: the same D1/D2 relationship fold also
\* finalizes this batch; no Open relationship can survive its completed call.
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
BY PersistBatchSafety, CommitNoopSafety, StartOrRetrySafety,
   StartParallelCallsSafety, AwaitCallSafety,
   ResumeExecutingSafety, RecordReturnsSafety,
   CompleteCallSafety, CompleteAndFinalizeSafety,
   CompleteImmediateSafety, CompleteImmediateAndFinalizeSafety,
   MarkIndeterminateSafety, FinalizeBatchSafety, EndRunSafety DEF Next, NextState

InductiveSafety == KernelAssumptions /\ Safety(state)

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextSafety DEF InductiveSafety, KernelAssumptions, vars

THEOREM ThreadCommitProjectionSafety ==
    KernelAssumptions /\ Spec => []Safety(state)
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=============================================================================
