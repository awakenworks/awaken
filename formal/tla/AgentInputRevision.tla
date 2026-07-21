------------------------ MODULE AgentInputRevision ------------------------
EXTENDS Naturals

CONSTANT MaxRevision

VARIABLE current, attempted, sameContent, before, result

vars == <<current, attempted, sameContent, before, result>>

Init == /\ current = 0
        /\ attempted = 0
        /\ sameContent = FALSE
        /\ before = 0
        /\ result = "idle"

ChooseAttempt ==
    /\ result # "pending"
    /\ attempted' \in 0..MaxRevision
    /\ sameContent' \in BOOLEAN
    /\ before' = current
    /\ result' = "pending"
    /\ UNCHANGED current

ApplySuccessor ==
    /\ result = "pending"
    /\ attempted = current + 1
    /\ attempted <= MaxRevision
    /\ current' = attempted
    /\ result' = "applied"
    /\ UNCHANGED <<attempted, sameContent, before>>

ReplayCurrent ==
    /\ result = "pending"
    /\ attempted = current
    /\ sameContent
    /\ result' = "replayed"
    /\ UNCHANGED <<current, attempted, sameContent, before>>

RejectInvalid ==
    /\ result = "pending"
    /\ ~(attempted = current + 1 /\ attempted <= MaxRevision)
    /\ ~(attempted = current /\ sameContent)
    /\ result' = "rejected"
    /\ UNCHANGED <<current, attempted, sameContent, before>>

Next == ChooseAttempt \/ ApplySuccessor \/ ReplayCurrent \/ RejectInvalid

TypeOK == /\ current \in 0..MaxRevision
          /\ attempted \in 0..MaxRevision
          /\ sameContent \in BOOLEAN
          /\ before \in 0..MaxRevision
          /\ result \in {"idle", "pending", "applied", "replayed", "rejected"}

AppliedIsExactSuccessor == result = "applied" => current = before + 1
ReplayDoesNotAdvance == result = "replayed" => current = before /\ attempted = current
RejectedDoesNotMutate == result = "rejected" => current = before

Spec == Init /\ [][Next]_vars
=============================================================================
