---------------------- MODULE SessionRuntimeProjection ----------------------
EXTENDS Naturals, TLC

CONSTANTS ToolIds, ErrorIds, MaxSourceVersion

VARIABLES phase, pending, resolving, lastError,
          sourceVersion, projectedVersion, refreshSucceeded, history, aggregate

vars == <<phase, pending, resolving, lastError,
          sourceVersion, projectedVersion, refreshSucceeded, history, aggregate>>
NoError == "none"

HistoryValue(awaitingCommitted, terminalCommitted,
             pendingProjected, terminalProjected) ==
    [awaitingCommitted |-> awaitingCommitted,
     terminalCommitted |-> terminalCommitted,
     pendingProjected |-> pendingProjected,
     terminalProjected |-> terminalProjected]

Init ==
    /\ phase = "unknown"
    /\ aggregate = "unknown"
    /\ pending = {}
    /\ resolving = {}
    /\ lastError = NoError
    /\ sourceVersion = 0
    /\ projectedVersion = 0
    /\ refreshSucceeded = FALSE
    /\ history = HistoryValue(FALSE, FALSE, FALSE, FALSE)

projectionVars == <<sourceVersion, projectedVersion, refreshSucceeded>>

Running ==
    /\ phase' = "running"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars, history, aggregate>>

RequiresAction(ids) ==
    /\ ids \subseteq ToolIds
    /\ phase' = "idle"
    /\ pending' = ids
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars, history, aggregate>>

AcceptReply(id) ==
    /\ id \in pending
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \cup {id}
    /\ UNCHANGED <<phase, lastError, projectionVars, history, aggregate>>

ProcessReply(id) ==
    /\ id \in resolving
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, pending, lastError, projectionVars, history, aggregate>>

ObserveToolResult(id) ==
    /\ id \in pending \cup resolving
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, lastError, projectionVars, history, aggregate>>

IdleTerminal ==
    /\ phase' = "idle"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars, history, aggregate>>

Error(errorId) ==
    /\ errorId \in ErrorIds
    /\ phase' = "error"
    /\ pending' = {}
    /\ resolving' = {}
    /\ lastError' = errorId
    /\ UNCHANGED <<projectionVars, history, aggregate>>

\* Any correlation, lifecycle, interval, outcome, or canonicalization failure
\* occurs before the one final swap and is therefore an abstract stuttering
\* step over the externally visible projection.
RefreshFailure == UNCHANGED vars

\* Durable authorities may advance without mutating the disposable result.
CommittedPrefixAdvances ==
    /\ sourceVersion < MaxSourceVersion
    /\ sourceVersion' = sourceVersion + 1
    /\ refreshSucceeded' = FALSE
    /\ UNCHANGED <<phase, pending, resolving, lastError,
                    projectedVersion, history, aggregate>>

\* A successful refresh consumes the stable source suffix and advances the
\* result/checkpoint pair to the exact observed committed prefix.
RefreshSuccess ==
    /\ projectedVersion' = sourceVersion
    /\ refreshSucceeded' = TRUE
    /\ UNCHANGED <<phase, pending, resolving, lastError, sourceVersion, history, aggregate>>

\* The lifecycle feed and audit snapshot may both be read only after a remote
\* wait has already closed. The committed Awaiting target remains the source of
\* the synthetic answerable event; terminal history cannot be projected first.
CommitHistoricalAwaiting ==
    /\ ~history.awaitingCommitted
    /\ history' = HistoryValue(TRUE, history.terminalCommitted,
                               history.pendingProjected, history.terminalProjected)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars, aggregate>>

CommitHistoricalTerminal ==
    /\ history.awaitingCommitted /\ ~history.terminalCommitted
    /\ history' = HistoryValue(TRUE, TRUE,
                               history.pendingProjected, history.terminalProjected)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars, aggregate>>

ProjectHistoricalPending ==
    /\ history.awaitingCommitted /\ ~history.pendingProjected
    /\ history' = HistoryValue(TRUE, history.terminalCommitted,
                               TRUE, history.terminalProjected)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars, aggregate>>

ProjectHistoricalTerminal ==
    /\ history.terminalCommitted /\ history.pendingProjected
    /\ ~history.terminalProjected
    /\ history' = HistoryValue(TRUE, TRUE, TRUE, TRUE)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars, aggregate>>

AggregateStatuses == {"unknown", "idle", "running", "rescheduling", "terminated"}
ObserveAggregate(status) ==
    /\ status \in AggregateStatuses
    /\ aggregate' = status
    /\ UNCHANGED <<phase, pending, resolving, lastError,
                    sourceVersion, projectedVersion, refreshSucceeded, history>>

Next ==
    \/ \E status \in AggregateStatuses: ObserveAggregate(status)
    \/ Running
    \/ \E ids \in SUBSET ToolIds: RequiresAction(ids)
    \/ \E id \in ToolIds: AcceptReply(id)
    \/ \E id \in ToolIds: ProcessReply(id)
    \/ \E id \in ToolIds: ObserveToolResult(id)
    \/ IdleTerminal
    \/ \E errorId \in ErrorIds: Error(errorId)
    \/ RefreshFailure
    \/ CommittedPrefixAdvances
    \/ RefreshSuccess
    \/ CommitHistoricalAwaiting
    \/ CommitHistoricalTerminal
    \/ ProjectHistoricalPending
    \/ ProjectHistoricalTerminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ aggregate \in AggregateStatuses
    /\ phase \in {"unknown", "running", "idle", "error"}
    /\ pending \subseteq ToolIds
    /\ resolving \subseteq ToolIds
    /\ lastError \in ErrorIds \cup {NoError}
    /\ sourceVersion \in 0..MaxSourceVersion
    /\ projectedVersion \in 0..MaxSourceVersion
    /\ refreshSucceeded \in BOOLEAN
    /\ history \in [awaitingCommitted: BOOLEAN,
                     terminalCommitted: BOOLEAN,
                     pendingProjected: BOOLEAN,
                     terminalProjected: BOOLEAN]

OnlyIdleMayRequireAction == pending # {} => phase = "idle"
PendingAndResolvingAreDisjoint == pending \intersect resolving = {}
TerminalFrameClearsPending ==
    phase \in {"running", "error"} => pending = {} /\ resolving = {}
ErrorProjectionHasEvidence == phase = "error" => lastError # NoError
\* C1 aggregate authority; C2 committed event phase; C3 pending/resolving.
\* This is only the client-presented send gate. Server admission separately
\* retains a follow-up behind an already accepted reply and leaves it queued
\* until the durable selector processes that reply.
ClientPresentedCanSend ==
    aggregate = "idle" /\ phase \in {"unknown", "idle", "error"}
    /\ pending = {} /\ resolving = {}
PresentedPhase == IF aggregate = "terminated" THEN "terminated"
                  ELSE IF phase = "error" THEN "error"
                  ELSE IF phase = "running" THEN "running"
                  ELSE IF aggregate \in {"running", "rescheduling"} THEN aggregate
                  ELSE IF aggregate = "idle" THEN "idle" ELSE "unknown"
AcceptedReplyBlocksClientPresentedSendUntilProcessed ==
    resolving # {} => ~ClientPresentedCanSend
ActiveAggregateNeverPresentsSend == aggregate # "idle" => ~ClientPresentedCanSend
ErrorIsVisibleWithoutRelease ==
    (phase = "error" /\ aggregate \in {"running", "rescheduling"})
        => (PresentedPhase = "error" /\ ~ClientPresentedCanSend)
ProjectionNeverInventsSourcePrefix == projectedVersion <= sourceVersion
SuccessfulRefreshReachesObservedSourcePrefix ==
    refreshSucceeded => projectedVersion = sourceVersion
HistoricalProjectionPreservesCausalOrder ==
    /\ history.terminalCommitted => history.awaitingCommitted
    /\ history.pendingProjected => history.awaitingCommitted
    /\ history.terminalProjected =>
         history.terminalCommitted /\ history.pendingProjected

Safety ==
    /\ TypeOK
    /\ OnlyIdleMayRequireAction
    /\ PendingAndResolvingAreDisjoint
    /\ TerminalFrameClearsPending
    /\ ErrorProjectionHasEvidence
    /\ AcceptedReplyBlocksClientPresentedSendUntilProcessed
    /\ ActiveAggregateNeverPresentsSend
    /\ ErrorIsVisibleWithoutRelease
    /\ ProjectionNeverInventsSourcePrefix
    /\ SuccessfulRefreshReachesObservedSourcePrefix
    /\ HistoricalProjectionPreservesCausalOrder
\* Opening-identity submodel: one retained opening, one optional later interval,
\* and up to MaxSourceVersion source commits. The first interval either has
\* Running evidence or contains only its terminal commit.
\* The same selector is used before/after terminal observation and after cache
\* loss. Runtime/Session source agreement and the selected Run/Thread boundary
\* are assumptions here; Rust reducer and real restart tests check that binding.
\* Existing variables are reused only by the separate OpeningInit/OpeningNext
\* configuration, so this submodel does not multiply the UI admission state space.
OpeningChoice == IF aggregate = "terminal-only" THEN 1 ELSE
                 CHOOSE cursor \in 1..sourceVersion :
                    \A other \in 1..sourceVersion : cursor <= other
OpeningInit == Init
OpeningAppend ==
    /\ phase # "closed" /\ sourceVersion < MaxSourceVersion
    /\ sourceVersion' = sourceVersion + 1
    /\ UNCHANGED <<phase, pending, resolving, lastError,
                    projectedVersion, refreshSucceeded, history, aggregate>>
OpeningWarmRead ==
    /\ sourceVersion > 0
    /\ pending' = pending \cup {OpeningChoice}
    /\ UNCHANGED <<phase, resolving, lastError, sourceVersion,
                    projectedVersion, refreshSucceeded, history, aggregate>>
OpeningClose ==
    /\ sourceVersion > 0 /\ phase # "closed"
    /\ phase' = "closed"
    /\ UNCHANGED <<pending, resolving, lastError, sourceVersion,
                    projectedVersion, refreshSucceeded, history, aggregate>>
OpeningColdRead ==
    /\ sourceVersion > 0
    /\ projectedVersion' = OpeningChoice
    /\ refreshSucceeded' = TRUE
    /\ UNCHANGED <<phase, pending, resolving, lastError, sourceVersion,
                    history, aggregate>>
\* A fast ACP first interval may contain only its terminal commit. After a
\* second interval opens, its Running commits cannot replace the first bracket.
OpeningTerminalOnly ==
    /\ sourceVersion = 0
    /\ sourceVersion' = 1 /\ phase' = "closed" /\ aggregate' = "terminal-only"
    /\ UNCHANGED <<pending, resolving, lastError, projectedVersion,
                    refreshSucceeded, history>>
OpeningReopen ==
    /\ phase = "closed" /\ ~history.terminalCommitted
    /\ sourceVersion < MaxSourceVersion
    /\ sourceVersion' = sourceVersion + 1 /\ phase' = "running"
    /\ history' = HistoryValue(FALSE, TRUE, FALSE, FALSE)
    /\ UNCHANGED <<pending, resolving, lastError, projectedVersion,
                    refreshSucceeded, aggregate>>
OpeningNext == OpeningAppend \/ OpeningWarmRead \/ OpeningClose \/ OpeningColdRead
               \/ OpeningTerminalOnly \/ OpeningReopen
OpeningIdentityStable ==
    /\ (refreshSucceeded => projectedVersion = 1)
    /\ pending \subseteq {1}
    /\ projectedVersion \in {0, 1}
    /\ (pending # {} /\ projectedVersion # 0 => projectedVersion \in pending)
=============================================================================
