---------------------- MODULE SessionRuntimeProjection ----------------------
EXTENDS Naturals, TLC

CONSTANTS ToolIds, ErrorIds, MaxSourceVersion

VARIABLES phase, pending, resolving, lastError,
          sourceVersion, projectedVersion, refreshSucceeded

vars == <<phase, pending, resolving, lastError,
          sourceVersion, projectedVersion, refreshSucceeded>>
NoError == "none"

Init ==
    /\ phase = "unknown"
    /\ pending = {}
    /\ resolving = {}
    /\ lastError = NoError
    /\ sourceVersion = 0
    /\ projectedVersion = 0
    /\ refreshSucceeded = FALSE

projectionVars == <<sourceVersion, projectedVersion, refreshSucceeded>>

Running ==
    /\ phase' = "running"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars>>

RequiresAction(ids) ==
    /\ ids \subseteq ToolIds
    /\ phase' = "idle"
    /\ pending' = ids
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars>>

AcceptReply(id) ==
    /\ id \in pending
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \cup {id}
    /\ UNCHANGED <<phase, lastError, projectionVars>>

ProcessReply(id) ==
    /\ id \in resolving
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, pending, lastError, projectionVars>>

ObserveToolResult(id) ==
    /\ id \in pending \cup resolving
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, lastError, projectionVars>>

IdleTerminal ==
    /\ phase' = "idle"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED <<lastError, projectionVars>>

Error(errorId) ==
    /\ errorId \in ErrorIds
    /\ phase' = "error"
    /\ pending' = {}
    /\ resolving' = {}
    /\ lastError' = errorId
    /\ UNCHANGED projectionVars

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
                    projectedVersion>>

\* A successful refresh consumes the stable source suffix and advances the
\* result/checkpoint pair to the exact observed committed prefix.
RefreshSuccess ==
    /\ projectedVersion' = sourceVersion
    /\ refreshSucceeded' = TRUE
    /\ UNCHANGED <<phase, pending, resolving, lastError, sourceVersion>>

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

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"unknown", "running", "idle", "error"}
    /\ pending \subseteq ToolIds
    /\ resolving \subseteq ToolIds
    /\ lastError \in ErrorIds \cup {NoError}
    /\ sourceVersion \in 0..MaxSourceVersion
    /\ projectedVersion \in 0..MaxSourceVersion
    /\ refreshSucceeded \in BOOLEAN

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

Safety ==
    /\ TypeOK
    /\ OnlyIdleMayRequireAction
    /\ PendingAndResolvingAreDisjoint
    /\ TerminalFrameClearsPending
    /\ ErrorProjectionHasEvidence
    /\ AcceptedReplyDoesNotBlockFollowup
    /\ ProjectionNeverInventsSourcePrefix
    /\ SuccessfulRefreshReachesObservedSourcePrefix
=============================================================================
