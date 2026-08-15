------------------------- MODULE RegistrationIntent -------------------------
EXTENDS Naturals, TLC

\* One model instance represents one Environment.  Each lifecycle mutation
\* atomically commits an immutable revision and its registration/withdrawal
\* intent.  Delivery is deliberately split from acknowledgement so TLC also
\* explores the external-effect/acknowledgement crash window.
CONSTANT MaxRevision

Revisions == 1..MaxRevision
Operations == {"register", "withdraw"}
NoOperation == "none"

VARIABLES currentRevision, archived, committedRevisions, intentOperation,
          delivered, activeRevision, activeOperation, effectApplied,
          projectionRevision, projectionOperation, effectFacts, processUp

vars == <<currentRevision, archived, committedRevisions, intentOperation,
          delivered, activeRevision, activeOperation, effectApplied,
          projectionRevision, projectionOperation, effectFacts, processUp>>

Init ==
    /\ currentRevision = 0
    /\ archived = FALSE
    /\ committedRevisions = {}
    /\ intentOperation = [r \in Revisions |-> NoOperation]
    /\ delivered = {}
    /\ activeRevision = 0
    /\ activeOperation = NoOperation
    /\ effectApplied = FALSE
    /\ projectionRevision = 0
    /\ projectionOperation = NoOperation
    /\ effectFacts = {}
    /\ processUp = TRUE

CommitCreate ==
    /\ processUp
    /\ currentRevision = 0
    /\ MaxRevision >= 1
    /\ currentRevision' = 1
    /\ committedRevisions' = {1}
    /\ intentOperation' = [intentOperation EXCEPT ![1] = "register"]
    /\ UNCHANGED <<archived, delivered, activeRevision, activeOperation,
                    effectApplied, projectionRevision, projectionOperation,
                    effectFacts, processUp>>

CommitUpdate ==
    /\ processUp
    /\ currentRevision \in 1..(MaxRevision - 1)
    /\ ~archived
    /\ LET next == currentRevision + 1 IN
       /\ currentRevision' = next
       /\ committedRevisions' = committedRevisions \cup {next}
       /\ intentOperation' =
              [intentOperation EXCEPT ![next] = "register"]
    /\ UNCHANGED <<archived, delivered, activeRevision, activeOperation,
                    effectApplied, projectionRevision, projectionOperation,
                    effectFacts, processUp>>

CommitArchive ==
    /\ processUp
    /\ currentRevision \in 1..(MaxRevision - 1)
    /\ ~archived
    /\ LET next == currentRevision + 1 IN
       /\ currentRevision' = next
       /\ archived' = TRUE
       /\ committedRevisions' = committedRevisions \cup {next}
       /\ intentOperation' =
              [intentOperation EXCEPT ![next] = "withdraw"]
    /\ UNCHANGED <<delivered, activeRevision, activeOperation, effectApplied,
                    projectionRevision, projectionOperation, effectFacts,
                    processUp>>

\* Normal draining selects only pending rows.  Startup recovery may replay any
\* committed row to rebuild an empty/replaced executable projection.
BeginPending(r) ==
    /\ processUp
    /\ activeRevision = 0
    /\ r \in committedRevisions \ delivered
    /\ activeRevision' = r
    /\ activeOperation' = intentOperation[r]
    /\ effectApplied' = FALSE
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, delivered, projectionRevision,
                    projectionOperation, effectFacts, processUp>>

BeginRecovery(r) ==
    /\ processUp
    /\ activeRevision = 0
    /\ r \in committedRevisions
    /\ activeRevision' = r
    /\ activeOperation' = intentOperation[r]
    /\ effectApplied' = FALSE
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, delivered, projectionRevision,
                    projectionOperation, effectFacts, processUp>>

\* The executable registrar is revision-fenced.  A replay has a stable fact
\* identity, and an older revision cannot roll the projection back.
ApplyExternalEffect ==
    /\ processUp
    /\ activeRevision \in committedRevisions
    /\ ~effectApplied
    /\ effectApplied' = TRUE
    /\ effectFacts' =
           effectFacts \cup {<<activeRevision, activeOperation>>}
    /\ IF activeRevision >= projectionRevision
          THEN /\ projectionRevision' = activeRevision
               /\ projectionOperation' = activeOperation
          ELSE /\ UNCHANGED <<projectionRevision, projectionOperation>>
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, delivered, activeRevision,
                    activeOperation, processUp>>

Acknowledge ==
    /\ processUp
    /\ activeRevision \in committedRevisions
    /\ effectApplied
    /\ delivered' = delivered \cup {activeRevision}
    /\ activeRevision' = 0
    /\ activeOperation' = NoOperation
    /\ effectApplied' = FALSE
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, projectionRevision, projectionOperation,
                    effectFacts, processUp>>

DispatchFailure ==
    /\ processUp
    /\ activeRevision \in committedRevisions
    /\ ~effectApplied
    /\ activeRevision' = 0
    /\ activeOperation' = NoOperation
    /\ effectApplied' = FALSE
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, delivered, projectionRevision,
                    projectionOperation, effectFacts, processUp>>

Crash ==
    /\ processUp
    /\ processUp' = FALSE
    /\ activeRevision' = 0
    /\ activeOperation' = NoOperation
    /\ effectApplied' = FALSE
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, delivered, projectionRevision,
                    projectionOperation, effectFacts>>

Restart ==
    /\ ~processUp
    /\ processUp' = TRUE
    /\ UNCHANGED <<currentRevision, archived, committedRevisions,
                    intentOperation, delivered, activeRevision,
                    activeOperation, effectApplied, projectionRevision,
                    projectionOperation, effectFacts>>

Next ==
    \/ CommitCreate
    \/ CommitUpdate
    \/ CommitArchive
    \/ \E r \in Revisions: BeginPending(r)
    \/ \E r \in Revisions: BeginRecovery(r)
    \/ ApplyExternalEffect
    \/ Acknowledge
    \/ DispatchFailure
    \/ Crash
    \/ Restart

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ currentRevision \in 0..MaxRevision
    /\ archived \in BOOLEAN
    /\ committedRevisions \subseteq Revisions
    /\ intentOperation \in [Revisions -> Operations \cup {NoOperation}]
    /\ delivered \subseteq Revisions
    /\ activeRevision \in 0..MaxRevision
    /\ activeOperation \in Operations \cup {NoOperation}
    /\ effectApplied \in BOOLEAN
    /\ projectionRevision \in 0..MaxRevision
    /\ projectionOperation \in Operations \cup {NoOperation}
    /\ effectFacts \subseteq (Revisions \X Operations)
    /\ processUp \in BOOLEAN

RevisionAndIntentCommitAtomically ==
    \A r \in Revisions:
        (r \in committedRevisions) <=> (intentOperation[r] \in Operations)

CurrentRevisionIsCommitted ==
    (currentRevision = 0) <=> (committedRevisions = {})
    /\ (currentRevision # 0 => currentRevision \in committedRevisions)

ArchiveCommitsExactWithdrawal ==
    archived => intentOperation[currentRevision] = "withdraw"

DeliveredRowsWereCommitted == delivered \subseteq committedRevisions

AttemptUsesExactDurableIdentity ==
    /\ (activeRevision = 0) <=> (activeOperation = NoOperation)
    /\ (activeRevision # 0 =>
           /\ activeRevision \in committedRevisions
           /\ activeOperation = intentOperation[activeRevision])
    /\ (effectApplied => activeRevision # 0)

ProjectionNeverRollsBackOrInventsAnIntent ==
    /\ (projectionRevision = 0) <=> (projectionOperation = NoOperation)
    /\ (projectionRevision # 0 =>
           /\ projectionRevision \in committedRevisions
           /\ projectionOperation = intentOperation[projectionRevision])

EveryEffectHasExactStableIntent ==
    \A fact \in effectFacts:
        /\ fact[1] \in committedRevisions
        /\ fact[2] = intentOperation[fact[1]]

=============================================================================
