---------------------- MODULE SessionRuntimeProjection ----------------------
EXTENDS Naturals, TLC

CONSTANTS ToolIds, ErrorIds

VARIABLES phase, pending, resolving, lastError

vars == <<phase, pending, resolving, lastError>>
NoError == "none"

Init ==
    /\ phase = "unknown"
    /\ pending = {}
    /\ resolving = {}
    /\ lastError = NoError

Running ==
    /\ phase' = "running"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED lastError

RequiresAction(ids) ==
    /\ ids \subseteq ToolIds
    /\ phase' = "idle"
    /\ pending' = ids
    /\ resolving' = {}
    /\ UNCHANGED lastError

AcceptReply(id) ==
    /\ id \in pending
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \cup {id}
    /\ UNCHANGED <<phase, lastError>>

ProcessReply(id) ==
    /\ id \in resolving
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, pending, lastError>>

ObserveToolResult(id) ==
    /\ id \in pending \cup resolving
    /\ pending' = pending \ {id}
    /\ resolving' = resolving \ {id}
    /\ UNCHANGED <<phase, lastError>>

IdleTerminal ==
    /\ phase' = "idle"
    /\ pending' = {}
    /\ resolving' = {}
    /\ UNCHANGED lastError

Error(errorId) ==
    /\ errorId \in ErrorIds
    /\ phase' = "error"
    /\ pending' = {}
    /\ resolving' = {}
    /\ lastError' = errorId

\* The production projector builds its complete candidate on a cloned warm
\* record. Any correlation, lifecycle, interval, outcome, or canonicalization
\* failure occurs before the one final swap and is therefore an abstract
\* stuttering step over the externally visible projection.
RefreshFailure == UNCHANGED vars

Next ==
    \/ Running
    \/ \E ids \in SUBSET ToolIds: RequiresAction(ids)
    \/ \E id \in ToolIds: AcceptReply(id)
    \/ \E id \in ToolIds: ProcessReply(id)
    \/ \E id \in ToolIds: ObserveToolResult(id)
    \/ IdleTerminal
    \/ \E errorId \in ErrorIds: Error(errorId)
    \/ RefreshFailure

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"unknown", "running", "idle", "error"}
    /\ pending \subseteq ToolIds
    /\ resolving \subseteq ToolIds
    /\ lastError \in ErrorIds \cup {NoError}

OnlyIdleMayRequireAction == pending # {} => phase = "idle"
PendingAndResolvingAreDisjoint == pending \intersect resolving = {}
TerminalFrameClearsPending ==
    phase \in {"running", "error"} => pending = {} /\ resolving = {}
ErrorProjectionHasEvidence == phase = "error" => lastError # NoError
CanSend == phase = "idle" /\ pending = {}
AcceptedReplyDoesNotBlockFollowup ==
    phase = "idle" /\ pending = {} /\ resolving # {} => CanSend

Safety ==
    /\ TypeOK
    /\ OnlyIdleMayRequireAction
    /\ PendingAndResolvingAreDisjoint
    /\ TerminalFrameClearsPending
    /\ ErrorProjectionHasEvidence
    /\ AcceptedReplyDoesNotBlockFollowup
=============================================================================
