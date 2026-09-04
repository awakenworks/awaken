---------------------- MODULE SessionRuntimeProjection ----------------------
EXTENDS Naturals, TLC

CONSTANTS ToolIds, ErrorIds, MaxSourceVersion

VARIABLES phase, pending, resolving, lastError,
          sourceVersion, projectedVersion, refreshSucceeded, history

vars == <<phase, pending, resolving, lastError,
          sourceVersion, projectedVersion, refreshSucceeded, history>>
NoError == "none"

HistoryValue(awaitingCommitted, terminalCommitted,
             pendingProjected, terminalProjected) ==
    [awaitingCommitted |-> awaitingCommitted,
     terminalCommitted |-> terminalCommitted,
     pendingProjected |-> pendingProjected,
     terminalProjected |-> terminalProjected]

Init ==
    /\ phase = "unknown"
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
    /\ UNCHANGED <<lastError, projectionVars, history>>

RequiresAction(ids) ==
    /\ ids \subseteq ToolIds
    /\ phase' = "idle"
    /\ pending' = ids
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars, history>>

AcceptReply(id) ==
    /\ id \in pending
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \cup {id}
    /\ UNCHANGED <<phase, lastError, projectionVars, history>>

ProcessReply(id) ==
    /\ id \in resolving
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, pending, lastError, projectionVars, history>>

ObserveToolResult(id) ==
    /\ id \in pending \cup resolving
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, lastError, projectionVars, history>>

IdleTerminal ==
    /\ phase' = "idle"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars, history>>

Error(errorId) ==
    /\ errorId \in ErrorIds
    /\ phase' = "error"
    /\ pending' = {}
    /\ resolving' = {}
    /\ lastError' = errorId
    /\ UNCHANGED <<projectionVars, history>>

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
                    projectedVersion, history>>

\* A successful refresh consumes the stable source suffix and advances the
\* result/checkpoint pair to the exact observed committed prefix.
RefreshSuccess ==
    /\ projectedVersion' = sourceVersion
    /\ refreshSucceeded' = TRUE
    /\ UNCHANGED <<phase, pending, resolving, lastError, sourceVersion, history>>

\* The lifecycle feed and audit snapshot may both be read only after a remote
\* wait has already closed. The committed Awaiting target remains the source of
\* the synthetic answerable event; terminal history cannot be projected first.
CommitHistoricalAwaiting ==
    /\ ~history.awaitingCommitted
    /\ history' = HistoryValue(TRUE, history.terminalCommitted,
                               history.pendingProjected, history.terminalProjected)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars>>

CommitHistoricalTerminal ==
    /\ history.awaitingCommitted /\ ~history.terminalCommitted
    /\ history' = HistoryValue(TRUE, TRUE,
                               history.pendingProjected, history.terminalProjected)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars>>

ProjectHistoricalPending ==
    /\ history.awaitingCommitted /\ ~history.pendingProjected
    /\ history' = HistoryValue(TRUE, history.terminalCommitted,
                               TRUE, history.terminalProjected)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars>>

ProjectHistoricalTerminal ==
    /\ history.terminalCommitted /\ history.pendingProjected
    /\ ~history.terminalProjected
    /\ history' = HistoryValue(TRUE, TRUE, TRUE, TRUE)
    /\ UNCHANGED <<phase, pending, resolving, lastError, projectionVars>>

Next ==
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
CanSend == phase = "idle" /\ pending = {}
AcceptedReplyDoesNotBlockFollowup ==
    phase = "idle" /\ pending = {} /\ resolving # {} => CanSend
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
    /\ AcceptedReplyDoesNotBlockFollowup
    /\ ProjectionNeverInventsSourcePrefix
    /\ SuccessfulRefreshReachesObservedSourcePrefix
    /\ HistoricalProjectionPreservesCausalOrder
=============================================================================
