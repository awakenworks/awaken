------------------ MODULE SessionResourceActivation ------------------
EXTENDS Naturals

CONSTANT MaxRevision

VARIABLES issuedRevision, activeRevision, pendingRevision,
          activeState, pendingState, retiredState, terminal

vars == <<issuedRevision, activeRevision, pendingRevision,
          activeState, pendingState, retiredState, terminal>>

Init == /\ issuedRevision = 0
        /\ activeRevision = 0
        /\ pendingRevision = 0
        /\ activeState = "none"
        /\ pendingState = "none"
        /\ retiredState = "none"
        /\ terminal = FALSE

Prepare == /\ ~terminal
           /\ pendingRevision = 0
           /\ issuedRevision < MaxRevision
           /\ issuedRevision' = issuedRevision + 1
           /\ pendingRevision' = issuedRevision + 1
           /\ pendingState' = "prepared"
           /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "releasing"
           /\ UNCHANGED <<activeRevision, retiredState, terminal>>

Commit == /\ ~terminal
          /\ pendingState = "prepared"
          /\ activeRevision' = pendingRevision
          /\ activeState' = "active"
          /\ pendingRevision' = 0
          /\ pendingState' = "none"
          /\ retiredState' = IF activeState = "releasing" THEN "released"
                              ELSE retiredState
          /\ UNCHANGED <<issuedRevision, terminal>>

Rollback == /\ ~terminal
            /\ pendingState = "prepared"
            /\ pendingRevision' = 0
            /\ pendingState' = "none"
            /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "active"
            /\ retiredState' = "failed"
            /\ UNCHANGED <<issuedRevision, activeRevision, terminal>>

Terminate == /\ ~terminal
             /\ terminal' = TRUE
             /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "releasing"
             /\ pendingRevision' = 0
             /\ pendingState' = "none"
             /\ retiredState' = IF pendingState = "prepared" THEN "failed"
                                 ELSE retiredState
             /\ UNCHANGED <<issuedRevision, activeRevision>>

Release == /\ terminal
           /\ activeState = "releasing"
           /\ activeState' = "released"
           /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                          pendingState, retiredState, terminal>>

StayTerminal == /\ terminal
                /\ activeState \in {"none", "released"}
                /\ UNCHANGED vars

Next == Prepare \/ Commit \/ Rollback \/ Terminate \/ Release \/ StayTerminal

TypeOK == /\ issuedRevision \in 0..MaxRevision
          /\ activeRevision \in 0..MaxRevision
          /\ pendingRevision \in 0..MaxRevision
          /\ activeState \in {"none", "active", "releasing", "released"}
          /\ pendingState \in {"none", "prepared"}
          /\ retiredState \in {"none", "released", "failed"}
          /\ terminal \in BOOLEAN

PendingHasDurableIntent ==
    (pendingState = "prepared") <=>
        (pendingRevision > activeRevision /\ pendingRevision = issuedRevision)

ActiveIsCommitted == activeState = "active" => pendingRevision = 0

RevisionNeverInvented ==
    /\ activeRevision <= issuedRevision
    /\ pendingRevision <= issuedRevision

TerminalNeverReactivates ==
    terminal => /\ activeState # "active"
                /\ pendingState = "none"
                /\ pendingRevision = 0

Spec == Init /\ [][Next]_vars
======================================================================
