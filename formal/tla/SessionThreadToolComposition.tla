------------------- MODULE SessionThreadToolComposition -------------------
EXTENDS Naturals, FiniteSets

\* A bounded composition scenario over the canonical ThreadCommitProjection
\* transition relation. One Session owns a root Thread and a child Thread; two
\* child Runs share the child Thread, and every Run owns a two-call ToolBatch.
\* The scenario explores all cross-Run interleavings while preserving the
\* production authority boundaries: committed tool state controls recovery and
\* the physical-attempt slot is unique per logical Thread.
CONSTANTS
    RootRun, ChildRunA, ChildRunB,
    RootThread, ChildThread,
    CallA, CallB, NoCall, MaxAttempts, MaxVersion

Runs == {RootRun, ChildRunA, ChildRunB}
Calls == {CallA, CallB}
RunThread(run) == IF run = RootRun THEN RootThread ELSE ChildThread

VARIABLES toolState, physicalRuns, invokedAttempt, crashedRuns

vars == <<toolState, physicalRuns, invokedAttempt, crashedRuns>>

ToolKernel == INSTANCE ThreadCommitProjection WITH
    Calls <- Calls,
    AgentCalls <- {},
    NoCall <- NoCall,
    MaxAttempts <- MaxAttempts,
    MaxVersion <- MaxVersion,
    state <- toolState[RootRun]

ReplaceState(run, next) ==
    toolState' = [toolState EXCEPT ![run] = next]

KeepExecutionHistory ==
    UNCHANGED <<physicalRuns, invokedAttempt, crashedRuns>>

Init ==
    /\ toolState = [run \in Runs |-> ToolKernel!InitialState]
    /\ physicalRuns = {}
    /\ invokedAttempt = [run \in Runs |-> [call \in Calls |-> 0]]
    /\ crashedRuns = {}

StartPhysical(run) ==
    /\ run \in Runs
    /\ run \notin physicalRuns
    /\ toolState[run].runState = "Running"
    /\ \A active \in physicalRuns: RunThread(active) # RunThread(run)
    /\ physicalRuns' = physicalRuns \cup {run}
    /\ UNCHANGED <<toolState, invokedAttempt, crashedRuns>>

PersistBatch(run) ==
    LET s == toolState[run] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall, s.callState, s.attempts,
        "Open", s.linkState, s.version + 1)
    IN  /\ run \in physicalRuns
        /\ ToolKernel!PersistBatch(s, next)
        /\ ReplaceState(run, next)
        /\ KeepExecutionHistory

StartBothCalls(run) ==
    LET s == toolState[run] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall,
        [call \in Calls |-> "Executing"],
        [call \in Calls |-> 1],
        s.batchState, s.linkState, s.version + 1)
    IN  /\ run \in physicalRuns
        /\ ToolKernel!StartParallelCalls(s, next, Calls)
        /\ ReplaceState(run, next)
        /\ KeepExecutionHistory

Invoke(run, call) ==
    /\ run \in physicalRuns
    /\ call \in Calls
    /\ toolState[run].callState[call] = "Executing"
    /\ invokedAttempt[run][call] < toolState[run].attempts[call]
    /\ invokedAttempt' =
          [invokedAttempt EXCEPT ![run][call] = toolState[run].attempts[call]]
    /\ UNCHANGED <<toolState, physicalRuns, crashedRuns>>

CompleteFirstCall(run) ==
    LET s == toolState[run] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall,
        [s.callState EXCEPT ![CallA] = "Completed"],
        s.attempts, s.batchState, s.linkState, s.version + 1)
    IN  /\ run \in physicalRuns
        /\ invokedAttempt[run][CallA] = s.attempts[CallA]
        /\ ToolKernel!CompleteCall(s, next, CallA)
        /\ ReplaceState(run, next)
        /\ KeepExecutionHistory

AwaitRootSecondCall ==
    LET s == toolState[RootRun] IN
    LET next == ToolKernel!StateValue(
        "Awaiting", "External", CallB,
        [s.callState EXCEPT ![CallB] = "Awaiting"],
        s.attempts, s.batchState, s.linkState, s.version + 1)
    IN  /\ RootRun \in physicalRuns
        /\ s.callState[CallA] = "Completed"
        /\ s.callState[CallB] = "Executing"
        /\ s.attempts[CallB] > 0
        /\ invokedAttempt[RootRun][CallB] = s.attempts[CallB]
        /\ ToolKernel!AwaitCall(s, next, CallB, "External")
        /\ ReplaceState(RootRun, next)
        /\ KeepExecutionHistory

CrashChild(run) ==
    /\ run \in {ChildRunA, ChildRunB}
    /\ run \in physicalRuns
    /\ run \notin crashedRuns
    /\ toolState[run].callState[CallA] = "Completed"
    /\ toolState[run].callState[CallB] = "Executing"
    /\ invokedAttempt[run][CallB] = toolState[run].attempts[CallB]
    /\ physicalRuns' = physicalRuns \ {run}
    /\ crashedRuns' = crashedRuns \cup {run}
    /\ UNCHANGED <<toolState, invokedAttempt>>

MarkNeverReplayIndeterminate ==
    LET s == toolState[ChildRunA] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall,
        [s.callState EXCEPT ![CallB] = "Indeterminate"],
        s.attempts, s.batchState, s.linkState, s.version + 1)
    IN  /\ ChildRunA \in physicalRuns
        /\ ChildRunA \in crashedRuns
        /\ ToolKernel!MarkIndeterminate(s, next, CallB)
        /\ ReplaceState(ChildRunA, next)
        /\ KeepExecutionHistory

RetryReplaySafe ==
    LET s == toolState[ChildRunB] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall, s.callState,
        [s.attempts EXCEPT ![CallB] = @ + 1],
        s.batchState, s.linkState, s.version + 1)
    IN  /\ ChildRunB \in physicalRuns
        /\ ChildRunB \in crashedRuns
        /\ s.attempts[CallB] = 1
        /\ ToolKernel!StartOrRetry(s, next, CallB)
        /\ ReplaceState(ChildRunB, next)
        /\ KeepExecutionHistory

CompleteReplaySafeAndFinalize ==
    LET s == toolState[ChildRunB] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall,
        [s.callState EXCEPT ![CallB] = "Completed"],
        s.attempts, "Finalized", s.linkState, s.version + 1)
    IN  /\ ChildRunB \in physicalRuns
        /\ ChildRunB \in crashedRuns
        /\ s.attempts[CallB] = 2
        /\ invokedAttempt[ChildRunB][CallB] = s.attempts[CallB]
        /\ ToolKernel!CompleteAndFinalize(s, next, CallB)
        /\ ReplaceState(ChildRunB, next)
        /\ KeepExecutionHistory

FinalizeNeverReplay ==
    LET s == toolState[ChildRunA] IN
    LET next == ToolKernel!StateValue(
        s.runState, s.ticketKind, s.ticketCall, s.callState, s.attempts,
        "Finalized", s.linkState, s.version + 1)
    IN  /\ ChildRunA \in physicalRuns
        /\ ToolKernel!FinalizeBatch(s, next)
        /\ ReplaceState(ChildRunA, next)
        /\ KeepExecutionHistory

EndChild(run) ==
    LET s == toolState[run] IN
    LET next == ToolKernel!StateValue(
        "Ended", "None", NoCall, s.callState, s.attempts,
        s.batchState, s.linkState, s.version + 1)
    IN  /\ run \in {ChildRunA, ChildRunB}
        /\ run \in physicalRuns
        /\ s.batchState = "Finalized"
        /\ ToolKernel!EndRun(s, next)
        /\ ReplaceState(run, next)
        /\ KeepExecutionHistory

ReleaseCheckpoint(run) ==
    /\ run \in physicalRuns
    /\ toolState[run].runState \in {"Awaiting", "Ended"}
    /\ physicalRuns' = physicalRuns \ {run}
    /\ UNCHANGED <<toolState, invokedAttempt, crashedRuns>>

Next ==
    \/ \E run \in Runs: StartPhysical(run)
    \/ \E run \in Runs: PersistBatch(run)
    \/ \E run \in Runs: StartBothCalls(run)
    \/ \E run \in Runs, call \in Calls: Invoke(run, call)
    \/ \E run \in Runs: CompleteFirstCall(run)
    \/ AwaitRootSecondCall
    \/ \E run \in {ChildRunA, ChildRunB}: CrashChild(run)
    \/ MarkNeverReplayIndeterminate
    \/ RetryReplaySafe
    \/ CompleteReplaySafeAndFinalize
    \/ FinalizeNeverReplay
    \/ \E run \in {ChildRunA, ChildRunB}: EndChild(run)
    \/ \E run \in Runs: ReleaseCheckpoint(run)

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

TypeOK ==
    /\ toolState \in [Runs -> [
          runState: ToolKernel!RunStates,
          ticketKind: ToolKernel!TicketKinds,
          ticketCall: Calls \cup {NoCall},
          callState: [Calls -> ToolKernel!CallStates],
          attempts: [Calls -> 0..MaxAttempts],
          batchState: ToolKernel!BatchStates,
          linkState: [Calls -> ToolKernel!LinkStates],
          version: 0..MaxVersion]]
    /\ physicalRuns \subseteq Runs
    /\ invokedAttempt \in [Runs -> [Calls -> 0..MaxAttempts]]
    /\ crashedRuns \subseteq {ChildRunA, ChildRunB}

EveryToolAggregateIsSafe ==
    \A run \in Runs: ToolKernel!Safety(toolState[run])

OneAwaitingCallPerRun ==
    \A run \in Runs:
        Cardinality({call \in Calls:
            toolState[run].callState[call] = "Awaiting"}) <= 1

OnePhysicalAttemptPerThread ==
    \A left \in physicalRuns, right \in physicalRuns:
        left # right => RunThread(left) # RunThread(right)

InvocationRequiresCommittedAttempt ==
    \A run \in Runs, call \in Calls:
        invokedAttempt[run][call] <= toolState[run].attempts[call]

NeverReplayDoesNotRetry ==
    toolState[ChildRunA].attempts[CallB] <= 1

ReplayRequiresPriorCrash ==
    toolState[ChildRunB].attempts[CallB] > 1 => ChildRunB \in crashedRuns

Safety ==
    /\ TypeOK
    /\ EveryToolAggregateIsSafe
    /\ OneAwaitingCallPerRun
    /\ OnePhysicalAttemptPerThread
    /\ InvocationRequiresCommittedAttempt
    /\ NeverReplayDoesNotRetry
    /\ ReplayRequiresPriorCrash

AllRecoveryCheckpointsReached ==
    /\ toolState[RootRun].runState = "Awaiting"
    /\ toolState[RootRun].ticketCall = CallB
    /\ toolState[ChildRunA].runState = "Ended"
    /\ toolState[ChildRunA].callState[CallB] = "Indeterminate"
    /\ toolState[ChildRunB].runState = "Ended"
    /\ toolState[ChildRunB].callState[CallB] = "Completed"
    /\ toolState[ChildRunB].attempts[CallB] = 2
    /\ physicalRuns = {}

RecoveryConverges == <>AllRecoveryCheckpointsReached

=============================================================================
