---------------------- MODULE SessionRuntimeProjection ----------------------
EXTENDS Naturals, TLC

CONSTANTS ToolIds, ErrorIds

VARIABLES phase, pending, lastError

vars == <<phase, pending, lastError>>
NoError == "none"

Init ==
    /\ phase = "unknown"
    /\ pending = {}
    /\ lastError = NoError

Running ==
    /\ phase' = "running"
    /\ pending' = {}
    /\ UNCHANGED lastError

RequiresAction(ids) ==
    /\ ids \subseteq ToolIds
    /\ phase' = "idle"
    /\ pending' = ids
    /\ UNCHANGED lastError

Resolve(id) ==
    /\ id \in ToolIds
    /\ pending' = pending \ {id}
    /\ UNCHANGED <<phase, lastError>>

IdleTerminal ==
    /\ phase' = "idle"
    /\ pending' = {}
    /\ UNCHANGED lastError

Error(errorId) ==
    /\ errorId \in ErrorIds
    /\ phase' = "error"
    /\ pending' = {}
    /\ lastError' = errorId

Next ==
    \/ Running
    \/ \E ids \in SUBSET ToolIds: RequiresAction(ids)
    \/ \E id \in ToolIds: Resolve(id)
    \/ IdleTerminal
    \/ \E errorId \in ErrorIds: Error(errorId)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"unknown", "running", "idle", "error"}
    /\ pending \subseteq ToolIds
    /\ lastError \in ErrorIds \cup {NoError}

OnlyIdleMayRequireAction == pending # {} => phase = "idle"
TerminalFrameClearsPending == phase \in {"running", "error"} => pending = {}
ErrorProjectionHasEvidence == phase = "error" => lastError # NoError

Safety ==
    /\ TypeOK
    /\ OnlyIdleMayRequireAction
    /\ TerminalFrameClearsPending
    /\ ErrorProjectionHasEvidence
=============================================================================
