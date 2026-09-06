------------------ MODULE SessionResourceActivation ------------------
EXTENDS Naturals

CONSTANT MaxRevision

VARIABLES issuedRevision, activeRevision, pendingRevision,
          activeState, pendingState, retiredState, terminal, preparedRevision

vars == <<issuedRevision, activeRevision, pendingRevision,
          activeState, pendingState, retiredState, terminal, preparedRevision>>

Init == /\ issuedRevision = 0
        /\ activeRevision = 0
        /\ pendingRevision = 0
        /\ activeState = "none"
        /\ pendingState = "none"
        /\ retiredState = "none"
        /\ terminal = FALSE
        /\ preparedRevision = 0

Prepare == /\ ~terminal
           /\ pendingRevision = 0
           /\ issuedRevision < MaxRevision
           /\ issuedRevision' = issuedRevision + 1
           /\ pendingRevision' = issuedRevision + 1
           /\ pendingState' = "prepared"
           /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "releasing"
           /\ UNCHANGED <<activeRevision, retiredState, terminal, preparedRevision>>

\* Stage carries an exact retained generation. The root may change between
\* Stage and Activate (including rollback/replacement/termination). Worker
\* identity, lease time, content equality, and IO are separate proof boundaries.
Stage(revision) == /\ ~terminal
                   /\ revision > 0
                   /\ revision \in {activeRevision, pendingRevision}
                   /\ preparedRevision' = revision
                   /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                  activeState, pendingState, retiredState, terminal>>

RebuildActive == /\ ~terminal
                 /\ preparedRevision > 0
                 /\ preparedRevision = activeRevision
                 /\ preparedRevision' = 0
                 /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                activeState, pendingState, retiredState, terminal>>

RejectStaleStage == /\ preparedRevision > 0
                    /\ terminal \/ preparedRevision \notin {activeRevision, pendingRevision}
                    /\ preparedRevision' = 0
                    /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                   activeState, pendingState, retiredState, terminal>>

Commit == /\ ~terminal
          /\ pendingState = "prepared"
          /\ preparedRevision = pendingRevision
          /\ preparedRevision' = 0
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
            /\ UNCHANGED <<issuedRevision, activeRevision, terminal, preparedRevision>>

Terminate == /\ ~terminal
             /\ terminal' = TRUE
             /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "releasing"
             /\ pendingRevision' = 0
             /\ pendingState' = "none"
             /\ retiredState' = IF pendingState = "prepared" THEN "failed"
                                 ELSE retiredState
             /\ UNCHANGED <<issuedRevision, activeRevision, preparedRevision>>

Release == /\ terminal
           /\ activeState = "releasing"
           /\ activeState' = "released"
           /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                          pendingState, retiredState, terminal, preparedRevision>>

StayTerminal == /\ terminal
                /\ activeState \in {"none", "released"}
                /\ UNCHANGED vars

Next == Prepare \/ (\E revision \in 1..MaxRevision: Stage(revision)) \/
        RebuildActive \/ RejectStaleStage \/ Commit \/ Rollback \/ Terminate \/
        Release \/ StayTerminal

TypeOK == /\ issuedRevision \in 0..MaxRevision
          /\ activeRevision \in 0..MaxRevision
          /\ pendingRevision \in 0..MaxRevision
          /\ activeState \in {"none", "active", "releasing", "released"}
          /\ pendingState \in {"none", "prepared"}
          /\ retiredState \in {"none", "released", "failed"}
          /\ terminal \in BOOLEAN
          /\ preparedRevision \in 0..MaxRevision

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

\* These action properties distinguish retained-A reconstruction from a B
\* commit, rather than merely proving one authoritative projection at rest.
RetainedStagePreservesPending ==
    [][RebuildActive => /\ activeRevision' = activeRevision
                        /\ pendingRevision' = pendingRevision]_vars
ActivationUsesPreparedGeneration == [][Commit => activeRevision' = preparedRevision]_vars
RejectedStageHasNoRootEffect ==
    [][RejectStaleStage => UNCHANGED <<issuedRevision, activeRevision,
                                      pendingRevision, terminal>>]_vars
======================================================================
