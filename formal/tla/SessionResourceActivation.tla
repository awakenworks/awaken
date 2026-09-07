------------------ MODULE SessionResourceActivation ------------------
EXTENDS Naturals

CONSTANT MaxRevision

VARIABLES issuedRevision, activeRevision, pendingRevision,
          activeState, pendingState, retiredState, terminal, preparedRevision,
          cleanupProjected, cleanupRevision, cleanupCompleted, terminalPending

vars == <<issuedRevision, activeRevision, pendingRevision,
          activeState, pendingState, retiredState, terminal, preparedRevision,
          cleanupProjected, cleanupRevision, cleanupCompleted, terminalPending>>

cleanupVars == <<cleanupProjected, cleanupRevision, cleanupCompleted, terminalPending>>
DesiredRevision == IF pendingRevision = 0 THEN activeRevision ELSE pendingRevision

Init == /\ issuedRevision = 0
        /\ activeRevision = 0
        /\ pendingRevision = 0
        /\ activeState = "none"
        /\ pendingState = "none"
        /\ retiredState = "none"
        /\ terminal = FALSE
        /\ preparedRevision = 0
        /\ cleanupProjected = FALSE
        /\ cleanupRevision = 0
        /\ cleanupCompleted = FALSE
        /\ terminalPending = 0

Prepare == /\ ~terminal
           /\ pendingRevision = 0
           /\ issuedRevision < MaxRevision
           /\ issuedRevision' = issuedRevision + 1
           /\ pendingRevision' = issuedRevision + 1
           /\ pendingState' = "prepared"
           /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "releasing"
           /\ UNCHANGED <<activeRevision, retiredState, terminal, preparedRevision, cleanupVars>>

\* Stage carries an exact retained generation. The root may change between
\* Stage and Activate (including rollback/replacement/termination). Worker
\* identity, lease time, content equality, and IO are separate proof boundaries.
Stage(revision) == /\ ~terminal
                   /\ revision > 0
                   /\ revision \in {activeRevision, pendingRevision}
                   /\ preparedRevision' = revision
                   /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                  activeState, pendingState, retiredState, terminal, cleanupVars>>

RebuildActive == /\ ~terminal
                 /\ preparedRevision > 0
                 /\ preparedRevision = activeRevision
                 /\ preparedRevision' = 0
                 /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                activeState, pendingState, retiredState, terminal, cleanupVars>>

RejectStaleStage == /\ preparedRevision > 0
                    /\ terminal \/ preparedRevision \notin {activeRevision, pendingRevision}
                    /\ preparedRevision' = 0
                    /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                   activeState, pendingState, retiredState, terminal, cleanupVars>>

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
          /\ UNCHANGED <<issuedRevision, terminal, cleanupVars>>

Rollback == /\ ~terminal
            /\ pendingState = "prepared"
            /\ pendingRevision' = 0
            /\ pendingState' = "none"
            /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "active"
            /\ retiredState' = "failed"
            /\ UNCHANGED <<issuedRevision, activeRevision, terminal, preparedRevision, cleanupVars>>

Terminate == /\ ~terminal
             /\ terminal' = TRUE
             /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "releasing"
             /\ terminalPending' = pendingRevision
             /\ UNCHANGED <<issuedRevision, activeRevision, preparedRevision,
                            pendingRevision, pendingState, retiredState,
                            cleanupProjected, cleanupRevision, cleanupCompleted>>

\* The Session adapter sends active -> desired, even after a crash between
\* Worker staging and the root activation receipt. A -> A is NOT equivalent.
ProjectCleanup == /\ terminal /\ ~cleanupCompleted
                  /\ cleanupProjected' = TRUE
                  /\ cleanupRevision' = DesiredRevision
                  /\ UNCHANGED <<issuedRevision, activeRevision, pendingRevision,
                                 activeState, pendingState, retiredState, terminal,
                                 preparedRevision, cleanupCompleted, terminalPending>>

Release == /\ terminal
           /\ ~cleanupCompleted
           /\ cleanupProjected /\ cleanupRevision = DesiredRevision
           /\ activeState' = IF activeRevision = 0 THEN "none" ELSE "released"
           /\ pendingRevision' = 0
           /\ pendingState' = "none"
           /\ retiredState' = IF pendingState = "prepared" THEN "failed" ELSE retiredState
           /\ cleanupCompleted' = TRUE
           /\ UNCHANGED <<issuedRevision, activeRevision, terminal, preparedRevision,
                          cleanupProjected, cleanupRevision, terminalPending>>

StayTerminal == /\ terminal
                /\ activeState \in {"none", "released"}
                /\ UNCHANGED vars

Next == Prepare \/ (\E revision \in 1..MaxRevision: Stage(revision)) \/
        RebuildActive \/ RejectStaleStage \/ Commit \/ Rollback \/ Terminate \/
        ProjectCleanup \/ Release \/ StayTerminal

TypeOK == /\ issuedRevision \in 0..MaxRevision
          /\ activeRevision \in 0..MaxRevision
          /\ pendingRevision \in 0..MaxRevision
          /\ activeState \in {"none", "active", "releasing", "released"}
          /\ pendingState \in {"none", "prepared"}
          /\ retiredState \in {"none", "released", "failed"}
          /\ terminal \in BOOLEAN
          /\ preparedRevision \in 0..MaxRevision
          /\ cleanupProjected \in BOOLEAN
          /\ cleanupRevision \in 0..MaxRevision
          /\ cleanupCompleted \in BOOLEAN
          /\ terminalPending \in 0..MaxRevision

PendingHasDurableIntent ==
    (pendingState = "prepared") <=>
        (pendingRevision > activeRevision /\ pendingRevision = issuedRevision)

ActiveIsCommitted == activeState = "active" => pendingRevision = 0

RevisionNeverInvented ==
    /\ activeRevision <= issuedRevision
    /\ pendingRevision <= issuedRevision

TerminalNeverReactivates ==
    terminal => activeState # "active"

TerminalRetainsPendingUntilReceipt ==
    terminal /\ ~cleanupCompleted => pendingRevision = terminalPending

CleanupProjectionCoversDesired ==
    cleanupProjected /\ ~cleanupCompleted => cleanupRevision = DesiredRevision

CleanupRequiresCompleteProjection ==
    cleanupCompleted => /\ terminal /\ cleanupProjected
                        /\ cleanupRevision >= activeRevision
                        /\ cleanupRevision >= terminalPending
                        /\ pendingRevision = 0

\* Fair recovery and an eventual verified physical receipt are assumptions.
\* Release abstracts that receipt; this model does not prove provider absence,
\* late-create fencing, manifest bytes, or API/database implementation refinement.
CleanupConverges == terminal ~> cleanupCompleted

Spec == Init /\ [][Next]_vars /\ WF_vars(ProjectCleanup) /\ WF_vars(Release)

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
