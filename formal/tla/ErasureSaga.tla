--------------------------- MODULE ErasureSaga ---------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Erasers, Replicas
ASSUME Erasers # {} /\ Replicas # {}

VARIABLE durableCompleted, durableRemoved, durableSeen, revision,
         localCompleted, localRemoved, baseRevision, effected,
         stamped, finished, processUp

vars == <<durableCompleted, durableRemoved, durableSeen, revision,
          localCompleted, localRemoved, baseRevision, effected,
          stamped, finished, processUp>>

Init == /\ durableCompleted = {}
        /\ durableRemoved = 0
        /\ durableSeen = {}
        /\ revision = 0
        /\ localCompleted = [r \in Replicas |-> {}]
        /\ localRemoved = [r \in Replicas |-> 0]
        /\ baseRevision = [r \in Replicas |-> 0]
        /\ effected = {}
        /\ stamped = FALSE
        /\ finished = FALSE
        /\ processUp = [r \in Replicas |-> TRUE]

Load(r) == /\ processUp[r]
           /\ localCompleted' = [localCompleted EXCEPT ![r] = durableCompleted]
           /\ localRemoved' = [localRemoved EXCEPT ![r] = durableRemoved]
           /\ baseRevision' = [baseRevision EXCEPT ![r] = revision]
           /\ UNCHANGED <<durableCompleted, durableRemoved, durableSeen, revision,
                          effected, stamped, finished, processUp>>

\* The external effect is idempotent by subject/target. Two replicas may both
\* replay it, but each observes the target's same stable one-record receipt.
Erase(r, e) == /\ processUp[r]
               /\ e \in Erasers \ localCompleted[r]
               /\ effected' = effected \cup {e}
               /\ localCompleted' =
                    [localCompleted EXCEPT ![r] = @ \cup {e}]
               /\ localRemoved' =
                    [localRemoved EXCEPT ![r] = Cardinality(localCompleted'[r])]
               /\ UNCHANGED <<durableCompleted, durableRemoved, durableSeen,
                              revision, baseRevision, stamped, finished, processUp>>

\* compare_and_swap_progress: only a replica that loaded the current revision
\* may publish, and it may only extend the durable target set.
Checkpoint(r) ==
  /\ processUp[r]
  /\ baseRevision[r] = revision
  /\ durableCompleted \subseteq localCompleted[r]
  /\ localCompleted[r] # durableCompleted
  /\ durableCompleted' = localCompleted[r]
  /\ durableRemoved' = localRemoved[r]
  /\ durableSeen' = durableSeen \cup localCompleted[r]
  /\ revision' = revision + 1
  /\ baseRevision' = [baseRevision EXCEPT ![r] = revision + 1]
  /\ UNCHANGED <<localCompleted, localRemoved, effected, stamped, finished,
                 processUp>>

Stamp == /\ durableCompleted = Erasers /\ ~stamped
         /\ stamped' = TRUE
         /\ UNCHANGED <<durableCompleted, durableRemoved, durableSeen, revision,
                        localCompleted, localRemoved, baseRevision, effected,
                        finished, processUp>>

Finish == /\ stamped /\ ~finished
          /\ finished' = TRUE
          /\ UNCHANGED <<durableCompleted, durableRemoved, durableSeen, revision,
                         localCompleted, localRemoved, baseRevision, effected,
                         stamped, processUp>>

Crash(r) == /\ processUp[r]
            /\ processUp' = [processUp EXCEPT ![r] = FALSE]
            /\ UNCHANGED <<durableCompleted, durableRemoved, durableSeen, revision,
                           localCompleted, localRemoved, baseRevision, effected,
                           stamped, finished>>

Restart(r) == /\ ~processUp[r]
              /\ processUp' = [processUp EXCEPT ![r] = TRUE]
              /\ localCompleted' =
                   [localCompleted EXCEPT ![r] = durableCompleted]
              /\ localRemoved' = [localRemoved EXCEPT ![r] = durableRemoved]
              /\ baseRevision' = [baseRevision EXCEPT ![r] = revision]
              /\ UNCHANGED <<durableCompleted, durableRemoved, durableSeen,
                             revision, effected, stamped, finished>>

Next == (\E r \in Replicas: Load(r))
        \/ (\E r \in Replicas, e \in Erasers: Erase(r, e))
        \/ (\E r \in Replicas: Checkpoint(r))
        \/ Stamp \/ Finish
        \/ (\E r \in Replicas: Crash(r) \/ Restart(r))

TypeOK == /\ durableCompleted \subseteq Erasers
          /\ durableSeen \subseteq Erasers
          /\ effected \subseteq Erasers
          /\ durableRemoved \in Nat /\ revision \in Nat
          /\ localCompleted \in [Replicas -> SUBSET Erasers]
          /\ localRemoved \in [Replicas -> Nat]
          /\ baseRevision \in [Replicas -> Nat]
          /\ processUp \in [Replicas -> BOOLEAN]
          /\ stamped \in BOOLEAN /\ finished \in BOOLEAN
CheckpointNeverForgets == durableSeen = durableCompleted
CheckpointRequiresEffect == durableCompleted \subseteq effected
NoEraserIsCountedTwice == durableRemoved = Cardinality(durableCompleted)
StampRequiresEveryEraser == stamped => durableCompleted = Erasers
CompletionRequiresAccountability == finished => stamped

Spec == Init /\ [][Next]_vars
=============================================================================
